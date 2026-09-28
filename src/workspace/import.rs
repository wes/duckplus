//! CSV import: drop files anywhere on the window (or pick them from the
//! server pane), name their tables, and watch them load side by side.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

use gpui_kit::component::progress::Progress;
use gpui_kit::component::tooltip::Tooltip;

use super::*;
use crate::quack::{ImportProgress, ImportTarget};

/// One file being (or just) imported, shown in the sidebar.
pub(super) struct ImportJob {
    id: u64,
    file: String,
    table: String,
    progress: Arc<ImportProgress>,
    state: JobState,
}

enum JobState {
    Running,
    Done(u64),
    Failed(String),
    Cancelled,
}

/// Files DuckDB's CSV reader takes, compressed ones included.
fn is_csv(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let name = name
        .strip_suffix(".gz")
        .or_else(|| name.strip_suffix(".zst"))
        .unwrap_or(&name);
    [".csv", ".tsv", ".txt"].iter().any(|ext| name.ends_with(ext))
}

/// A tidy table name from a file name: `Sales Q1 (2024).csv` → `sales_q1_2024`.
fn table_name_for(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut stem = name.as_str();
    for ext in [".gz", ".zst", ".csv", ".tsv", ".txt"] {
        stem = stem.strip_suffix(ext).unwrap_or(stem);
    }
    let mut out = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    match out.chars().next() {
        None => "import".into(),
        Some(c) if c.is_ascii_digit() => format!("t_{out}"),
        Some(_) => out.to_string(),
    }
}

/// `table`, `schema.table` or `database.schema.table`, with an unqualified
/// name landing in the window's database and default schema.
fn parse_target(input: &str, database: Option<&str>, schema: &str) -> Option<ImportTarget> {
    let parts: Vec<String> = input
        .split('.')
        .map(|p| p.trim().trim_matches('"').to_string())
        .collect();
    if parts.iter().any(|p| p.is_empty()) {
        return None;
    }
    let (database, schema, table) = match parts.as_slice() {
        [t] => (database.map(str::to_string), schema.to_string(), t.clone()),
        [s, t] => (database.map(str::to_string), s.clone(), t.clone()),
        [d, s, t] => (Some(d.clone()), s.clone(), t.clone()),
        _ => return None,
    };
    Some(ImportTarget { database, schema, table })
}

impl Workspace {
    pub(super) fn pick_import_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| this.prompt_import(paths, window, cx))
                .ok();
        })
        .detach();
    }

    /// Ask what to name each file's table, then import them all at once.
    pub(super) fn prompt_import(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.profile.read_only {
            self.notice = Some((true, "This database is open read-only".into()));
            cx.notify();
            return;
        }
        let files: Vec<PathBuf> = paths.into_iter().filter(|p| is_csv(p)).collect();
        if files.is_empty() {
            self.notice = Some((true, "Only .csv and .tsv files can be imported".into()));
            cx.notify();
            return;
        }
        let rows: Rc<Vec<(PathBuf, Entity<InputState>)>> = Rc::new(
            files
                .into_iter()
                .map(|path| {
                    let name = table_name_for(&path);
                    let input = cx.new(|cx| InputState::new(window, cx).default_value(name));
                    (path, input)
                })
                .collect(),
        );
        if let Some((_, first)) = rows.first() {
            first.update(cx, |s, cx| {
                s.focus(window, cx);
                s.select_all(window, cx);
            });
        }
        let database = self
            .focus_db
            .clone()
            .or_else(|| self.catalog.current_database.clone());
        let schema = self
            .catalog
            .current_schema
            .clone()
            .unwrap_or_else(|| "main".into());
        let into = match &database {
            Some(db) => format!("{db}.{schema}"),
            None => schema.clone(),
        };
        let error: Rc<RefCell<Option<String>>> = Rc::default();
        let view = cx.entity().downgrade();
        let focus_db = self.focus_db.clone();

        let submit = {
            let (rows, error) = (rows.clone(), error.clone());
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let mut jobs = Vec::new();
                let mut seen = HashSet::new();
                for (path, input) in rows.iter() {
                    let name = input.read(cx).value().trim().to_string();
                    let Some(target) = parse_target(&name, focus_db.as_deref(), &schema) else {
                        *error.borrow_mut() = Some(format!("“{name}” isn't a valid table name"));
                        window.refresh();
                        return;
                    };
                    let key = format!(
                        "{}.{}.{}",
                        target.database.as_deref().unwrap_or_default(),
                        target.schema,
                        target.table
                    )
                    .to_lowercase();
                    if !seen.insert(key) {
                        *error.borrow_mut() = Some(format!("Two files would both become “{name}”"));
                        window.refresh();
                        return;
                    }
                    jobs.push((path.clone(), target));
                }
                view.update(cx, |this, cx| this.start_imports(jobs, window, cx))
                    .ok();
                window.close_dialog(cx);
            })
        };

        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme();
            let count = rows.len();
            let ok = submit.clone();
            dialog
                .title(if count == 1 {
                    "Import CSV".to_string()
                } else {
                    format!("Import {count} CSV files")
                })
                .w(px(560.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "Each file becomes a new table in {into}. \
                                     Use schema.table to put one somewhere else."
                                )),
                        )
                        .child(v_flex().gap_2().children(rows.iter().enumerate().map(
                            |(ix, (path, input))| {
                                let size = std::fs::metadata(path)
                                    .map(|m| format_bytes(m.len()))
                                    .unwrap_or_default();
                                h_flex()
                                    .id(("import-row", ix))
                                    .gap_2()
                                    .child(
                                        h_flex()
                                            .w(px(210.))
                                            .flex_none()
                                            .gap_1p5()
                                            .overflow_hidden()
                                            .child(
                                                Icon::new(IconName::FileSpreadsheet)
                                                    .small()
                                                    .text_color(theme.muted_foreground),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_sm()
                                                    .child(
                                                        path.file_name()
                                                            .map(|n| n.to_string_lossy().to_string())
                                                            .unwrap_or_default(),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(size),
                                            ),
                                    )
                                    .child(
                                        Icon::new(IconName::ArrowRight)
                                            .xsmall()
                                            .text_color(theme.muted_foreground),
                                    )
                                    .child(div().flex_1().child(Input::new(input).small()))
                            },
                        )))
                        .when_some(error.borrow().clone(), |el, msg| {
                            el.child(div().text_sm().text_color(theme.danger).child(msg))
                        }),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("import-cancel")
                                .small()
                                .ghost()
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("import-ok")
                                .small()
                                .icon(IconName::Upload)
                                .label("Import")
                                .on_click(move |_, window, cx| ok(window, cx)),
                        ),
                )
        });
    }

    fn start_imports(
        &mut self,
        jobs: Vec<(PathBuf, ImportTarget)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (path, target) in jobs {
            let id = self.next_import;
            self.next_import += 1;
            let progress = Arc::new(ImportProgress::default());
            self.imports.push(ImportJob {
                id,
                file: path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                table: target.table.clone(),
                progress: progress.clone(),
                state: JobState::Running,
            });
            let client = self.client.clone();
            cx.spawn_in(window, async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn({
                        let progress = progress.clone();
                        async move { client.import_csv(&path, &target, &progress) }
                    })
                    .await;
                let done = result.is_ok();
                this.update_in(cx, |this, window, cx| {
                    let Some(job) = this.imports.iter_mut().find(|j| j.id == id) else {
                        return;
                    };
                    job.state = match result {
                        Ok(rows) => JobState::Done(rows),
                        Err(_) if progress.cancelled.load(Relaxed) => JobState::Cancelled,
                        Err(e) => JobState::Failed(format!("{e:#}")),
                    };
                    if done {
                        this.refresh_catalog(window, cx);
                    }
                    cx.notify();
                })
                .ok();
                // Finished rows clear themselves; failures wait to be read.
                if done || progress.cancelled.load(Relaxed) {
                    cx.background_executor().timer(Duration::from_secs(5)).await;
                    this.update(cx, |this, cx| {
                        this.imports.retain(|j| j.id != id);
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
        self.tick_imports(window, cx);
        cx.notify();
    }

    /// Redraw while imports run, so their progress bars move.
    fn tick_imports(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.import_ticking {
            return;
        }
        self.import_ticking = true;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        let running = this
                            .imports
                            .iter()
                            .any(|j| matches!(j.state, JobState::Running));
                        this.import_ticking = running;
                        running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    fn close_import(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(job) = self.imports.iter().find(|j| j.id == id) else {
            return;
        };
        if matches!(job.state, JobState::Running) {
            job.progress.cancelled.store(true, Relaxed);
        } else {
            self.imports.retain(|j| j.id != id);
        }
        cx.notify();
    }

    pub(super) fn render_imports(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.imports.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        Some(
            v_flex()
                .px_2()
                .py_1p5()
                .gap_2()
                .border_t_1()
                .border_color(theme.sidebar_border)
                .children(self.imports.iter().map(|job| {
                    let id = job.id;
                    let fraction = job.progress.fraction();
                    let cancelling =
                        job.progress.cancelled.load(Relaxed) && matches!(job.state, JobState::Running);
                    let (icon, color, status) = match &job.state {
                        JobState::Running if cancelling => (
                            IconName::FileSpreadsheet,
                            theme.muted_foreground,
                            "Cancelling…".to_string(),
                        ),
                        JobState::Running => (
                            IconName::FileSpreadsheet,
                            theme.muted_foreground,
                            match fraction {
                                None => "Reading…".to_string(),
                                Some(f) => format!("{:.0}%", f * 100.),
                            },
                        ),
                        JobState::Done(rows) => (
                            IconName::CircleCheck,
                            theme.success,
                            format!("{} rows", group_digits(*rows as usize)),
                        ),
                        JobState::Failed(_) => {
                            (IconName::CircleX, theme.danger, "Failed".to_string())
                        }
                        JobState::Cancelled => {
                            (IconName::CircleX, theme.muted_foreground, "Cancelled".to_string())
                        }
                    };
                    let running = matches!(job.state, JobState::Running);
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .id(("import", id as usize))
                                .gap_1p5()
                                .text_xs()
                                .tooltip({
                                    let tip = format!("{} → {}", job.file, job.table);
                                    move |window, cx| Tooltip::new(tip.clone()).build(window, cx)
                                })
                                .child(Icon::new(icon).xsmall().text_color(color))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(theme.sidebar_foreground)
                                        .child(job.table.clone()),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_color(theme.muted_foreground)
                                        .child(status),
                                )
                                .child(
                                    Button::new(("import-close", id as usize))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::X)
                                        .tooltip(if running { "Cancel import" } else { "Dismiss" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.close_import(id, cx)
                                        })),
                                ),
                        )
                        .when(running, |el| {
                            el.child(
                                Progress::new(("import-progress", id as usize))
                                    .xsmall()
                                    .loading(fraction.is_none())
                                    .value(fraction.unwrap_or(0.) * 100.),
                            )
                        })
                        .when_some(
                            match &job.state {
                                JobState::Failed(msg) => Some(msg.clone()),
                                _ => None,
                            },
                            |el, msg| {
                                el.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.danger)
                                        .whitespace_normal()
                                        .child(msg),
                                )
                            },
                        )
                })),
        )
        .map(IntoElement::into_any_element)
    }
}

fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut size = n as f64 / 1024.;
    let mut unit = 0;
    while size >= 1024. && unit < UNITS.len() - 1 {
        size /= 1024.;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::{is_csv, parse_target, table_name_for};
    use std::path::Path;

    #[test]
    fn names_from_files() {
        assert_eq!(table_name_for(Path::new("/x/Sales Q1 (2024).csv")), "sales_q1_2024");
        assert_eq!(table_name_for(Path::new("events.csv.gz")), "events");
        assert_eq!(table_name_for(Path::new("2024-trips.tsv")), "t_2024_trips");
        assert_eq!(table_name_for(Path::new("---.csv")), "import");
        assert!(is_csv(Path::new("a.CSV")) && is_csv(Path::new("b.tsv.zst")));
        assert!(!is_csv(Path::new("c.parquet")));
    }

    #[test]
    fn targets() {
        let t = parse_target("people", Some("lake"), "main").unwrap();
        assert_eq!((t.database.as_deref(), t.schema.as_str(), t.table.as_str()), (Some("lake"), "main", "people"));
        let t = parse_target("raw.people", None, "main").unwrap();
        assert_eq!((t.database, t.schema.as_str()), (None, "raw"));
        let t = parse_target("db.\"Raw\".people", None, "main").unwrap();
        assert_eq!((t.database.as_deref(), t.schema.as_str()), (Some("db"), "Raw"));
        assert!(parse_target("a..b", None, "main").is_none());
        assert!(parse_target("", None, "main").is_none());
    }
}

/// The whole flow in a headless window: drop or pick → dialog → table.
#[cfg(test)]
mod ui_tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use gpui_kit::component::Root;
    use gpui_kit::test::{TestAppContextExt, TestWindowExt};
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Entity, ExternalPaths, FileDropEvent, InputEvent as _,
        Keystroke, TestAppContext, point, px, size,
    };

    use super::Workspace;
    use crate::AppState;
    use crate::quack::QuackClient;
    use crate::store::{Profile, Store};

    struct Setup {
        dir: PathBuf,
        csv: PathBuf,
        check: QuackClient,
        window: AnyWindowHandle,
        workspace: Entity<Workspace>,
    }

    fn setup(cx: &mut TestAppContext) -> Setup {
        let dir = std::env::temp_dir().join(format!("duckplus-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("Sales Q1.csv");
        std::fs::write(&csv, "id,name\n1,ada\n2,alan\n").unwrap();
        let db = dir.join("x.duckdb");
        duckdb::Connection::open(&db).unwrap();
        let (client, info) = QuackClient::connect_local(&db, false).unwrap();
        let check = client.clone();
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            cx.set_global(AppState {
                store: Store::default(),
                connections_window: None,
                settings_window: None,
                launch_error: None,
            });
        });
        let profile = Profile::local(&db, false);
        let mut workspace = None;
        let handle = cx.open_window(size(px(1200.), px(800.)), |window, cx| {
            let view = cx.new(|cx| Workspace::new(profile, client, info, None, window, cx));
            workspace = Some(view.clone());
            Root::new(view, window, cx)
        });
        let window: AnyWindowHandle = handle.into();
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        Setup { dir, csv, check, window, workspace: workspace.unwrap() }
    }

    /// Confirm the open dialog and wait for `sales_q1` to hold both rows.
    async fn import_and_check(cx: &mut TestAppContext, s: Setup) {
        cx.wait_for(s.window, Duration::from_secs(2), |window, _| {
            window.try_find("dialog").is_some()
        })
        .await;
        cx.update_window(s.window, |_, window, cx| {
            window.within("dialog").click("import-ok", cx);
        })
        .unwrap();
        cx.wait_for(s.window, Duration::from_secs(5), |window, _| {
            window.try_find("dialog").is_none()
        })
        .await;
        for _ in 0..100 {
            cx.run_until_parked();
            if let Ok(rows) = s.check.meta_rows("SELECT count(*) FROM main.sales_q1") {
                assert_eq!(rows[0][0].as_deref(), Some("2"));
                std::fs::remove_dir_all(&s.dir).ok();
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("sales_q1 was never created");
    }

    #[gpui_kit::test]
    async fn picked_files_open_the_dialog(cx: &mut TestAppContext) {
        let s = setup(cx);
        let (workspace, csv) = (s.workspace.clone(), s.csv.clone());
        cx.update_window(s.window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.prompt_import(vec![csv], window, cx));
        })
        .unwrap();
        import_and_check(cx, s).await;
    }

    /// Export a result the row limit cut short: the file still has every row.
    #[gpui_kit::test]
    async fn export_writes_all_rows(cx: &mut TestAppContext) {
        let s = setup(cx);
        s.check.run("CREATE TABLE big AS FROM range(20) t(i)", 1, None).unwrap();
        cx.update(|cx| AppState::update_store(cx, |store| store.settings.row_limit = 5));
        let workspace = s.workspace.clone();
        cx.update_window(s.window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.run_sql("FROM big ORDER BY i".into(), true, super::ResultSource::Query, None, window, cx)
            });
        })
        .unwrap();
        cx.wait_for(s.window, Duration::from_secs(5), |window, _| window.try_find("export").is_some())
            .await;
        let out = s.dir.join("big.csv");
        let answer = out.clone();
        cx.update_window(s.window, |_, window, cx| {
            workspace.update(cx, |this, cx| this.export_results(false, window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.simulate_new_path_selection(move |_| Some(answer));
        for _ in 0..100 {
            cx.run_until_parked();
            let notice = cx.update(|cx| workspace.read(cx).notice.clone());
            if let Some((false, msg)) = &notice
                && msg.starts_with("Exported")
            {
                assert_eq!(msg, "Exported 20 rows to big.csv");
                let csv = std::fs::read_to_string(&out).unwrap();
                assert_eq!(csv.lines().count(), 21, "{csv}");
                std::fs::remove_dir_all(&s.dir).ok();
                return;
            }
            assert!(!matches!(notice, Some((true, _))), "{notice:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("export never finished");
    }

    /// Export Tables: the dialog opens with the viewed table picked, SQL by
    /// default, and writes a script; switching to CSV writes that table's file.
    #[gpui_kit::test]
    async fn export_tables_dialog(cx: &mut TestAppContext) {
        let s = setup(cx);
        s.check
            .run("CREATE SCHEMA sales; CREATE TABLE sales.orders (id INTEGER PRIMARY KEY, total DECIMAL(8,2)); \
                  INSERT INTO sales.orders VALUES (1, 9.50), (2, 20); CREATE TABLE other AS SELECT 1 AS x; SELECT 1", 1, None)
            .unwrap();
        let workspace = s.workspace.clone();
        let wait_notice = async |cx: &mut TestAppContext, prefix: &str| -> String {
            for _ in 0..100 {
                cx.run_until_parked();
                if let Some((is_error, msg)) = cx.update(|cx| workspace.read(cx).notice.clone()) {
                    assert!(!is_error, "{msg}");
                    if msg.starts_with(prefix) {
                        return msg;
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            panic!("no {prefix:?} notice");
        };
        cx.update_window(s.window, |_, window, cx| workspace.update(cx, |this, cx| this.refresh_catalog(window, cx)))
            .unwrap();
        cx.run_until_parked();
        // Nothing picked: ticking a table's Data box enables Export.
        cx.update_window(s.window, |_, window, cx| {
            workspace.update(cx, |this, cx| {
                this.selected_relation = None;
                this.open_tables_export(window, cx);
            });
        })
        .unwrap();
        cx.wait_for(s.window, Duration::from_secs(2), |window, _| window.try_find("export-tables-ok").is_some())
            .await;
        cx.update_window(s.window, |_, window, cx| {
            window.within("dialog").click("export-tables-ok", cx); // disabled: nothing happens
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!cx.did_prompt_for_new_path(), "Export must be disabled with nothing picked");
        cx.update_window(s.window, |_, window, cx| {
            // Tables sort main.other, sales.orders; Data is the third box.
            window.within("dialog").click(("pick", 1usize * 3 + 2), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(s.window, |_, window, cx| {
            window.within("dialog").click("export-tables-ok", cx);
        })
        .unwrap();
        cx.run_until_parked();
        let data_only = s.dir.join("data_only.sql");
        let answer = data_only.clone();
        cx.simulate_new_path_selection(move |_| Some(answer));
        assert_eq!(wait_notice(cx, "Exported").await, "Exported 1 table (2 rows) to data_only.sql");
        let text = std::fs::read_to_string(&data_only).unwrap();
        assert!(text.contains("INSERT INTO \"sales\".\"orders\"") && !text.contains("CREATE"), "{text}");
        cx.update(|cx| workspace.update(cx, |this, _| this.notice = None));

        for (format_button, file, expect) in [
            (None, "tables.sql", "Exported 1 table (2 rows) to tables.sql"),
            (Some("format-csv"), "orders.csv", "Exported 1 table (2 rows) to orders.csv"),
        ] {
            cx.update_window(s.window, |_, window, cx| {
                workspace.update(cx, |this, cx| {
                    this.refresh_catalog(window, cx);
                });
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(s.window, |_, window, cx| {
                workspace.update(cx, |this, cx| {
                    let rel = this.catalog.relations.iter().find(|r| r.name == "orders").unwrap().clone();
                    this.selected_relation = Some(rel.qualified());
                    this.open_tables_export(window, cx);
                });
            })
            .unwrap();
            cx.wait_for(s.window, Duration::from_secs(2), |window, _| window.try_find("export-tables-ok").is_some())
                .await;
            cx.update_window(s.window, |_, window, cx| {
                assert!(window.within("dialog").find("format-sql").visible());
                if let Some(button) = format_button {
                    window.within("dialog").click(button, cx);
                }
            })
            .unwrap();
            cx.run_until_parked();
            cx.update_window(s.window, |_, window, cx| {
                window.within("dialog").click("export-tables-ok", cx);
            })
            .unwrap();
            cx.run_until_parked();
            let out = s.dir.join(file);
            let answer = out.clone();
            cx.simulate_new_path_selection(move |_| Some(answer));
            assert_eq!(wait_notice(cx, "Exported").await, expect);
            let text = std::fs::read_to_string(&out).unwrap();
            if file.ends_with(".sql") {
                assert!(text.contains("CREATE SCHEMA IF NOT EXISTS \"sales\";"), "{text}");
                assert!(text.contains("CREATE TABLE sales.orders(id INTEGER PRIMARY KEY"), "{text}");
                assert!(!text.contains("\"other\""), "only the picked table: {text}");
            } else {
                assert_eq!(text, "id,total\n1,9.50\n2,20.00\n");
            }
            cx.update(|cx| workspace.update(cx, |this, _| this.notice = None));
        }
        std::fs::remove_dir_all(&s.dir).ok();
    }

    /// Tables picked in the sidebar with ⌘- and ⇧-click come into the
    /// dialog ticked; a right-click outside the pick acts on that table alone.
    #[gpui_kit::test]
    async fn export_marked_tables(cx: &mut TestAppContext) {
        let s = setup(cx);
        s.check
            .run("CREATE TABLE t1 AS SELECT 1 AS x; CREATE TABLE t2 AS SELECT 2 AS x; \
                  CREATE TABLE t3 AS SELECT 3 AS x; CREATE TABLE t4 AS SELECT 4 AS x; SELECT 1", 1, None)
            .unwrap();
        let workspace = s.workspace.clone();
        cx.update_window(s.window, |_, window, cx| workspace.update(cx, |this, cx| this.refresh_catalog(window, cx)))
            .unwrap();
        cx.run_until_parked();
        let q = |cx: &mut TestAppContext, name: &str| {
            cx.update(|cx| {
                workspace.read(cx).catalog.relations.iter().find(|r| r.name == name).unwrap().qualified()
            })
        };
        let (t1, t2, t3, t4) = (q(cx, "t1"), q(cx, "t2"), q(cx, "t3"), q(cx, "t4"));
        cx.update(|cx| {
            workspace.update(cx, |this, cx| {
                // t1 open, ⇧-click t3 (t1…t3), ⌘-click t2 off.
                this.selected_relation = Some(t1.clone());
                this.mark_anchor = Some(t1.clone());
                this.mark_range(t3.clone(), cx);
                assert_eq!(this.marked.len(), 3);
                this.toggle_mark(t2.clone(), cx);
                let mut picked = this.picked_relations();
                picked.sort();
                assert_eq!(picked, vec![t1.clone(), t3.clone()]);
                assert_eq!(this.context_relations(&t3).len(), 2);
                assert_eq!(this.context_relations(&t4), vec![t4.clone()]);
            })
        });
        cx.update_window(s.window, |_, window, cx| workspace.update(cx, |this, cx| this.open_tables_export(window, cx)))
            .unwrap();
        cx.wait_for(s.window, Duration::from_secs(2), |window, _| window.try_find("export-tables-ok").is_some())
            .await;
        cx.update_window(s.window, |_, window, cx| window.within("dialog").click("export-tables-ok", cx))
            .unwrap();
        cx.run_until_parked();
        let out = s.dir.join("marked.sql");
        let answer = out.clone();
        cx.simulate_new_path_selection(move |_| Some(answer));
        for _ in 0..100 {
            cx.run_until_parked();
            if let Some((is_error, msg)) = cx.update(|cx| workspace.read(cx).notice.clone()) {
                assert!(!is_error, "{msg}");
                if msg.starts_with("Exported") {
                    assert!(msg.starts_with("Exported 2 tables"), "{msg}");
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("t1") && text.contains("t3"), "{text}");
        assert!(!text.contains("t2") && !text.contains("t4"), "{text}");
        std::fs::remove_dir_all(&s.dir).ok();
    }

    /// Dropped right after typing in the editor, which is when GPUI's own
    /// `on_drop` misses it, and over the results grid, which occludes.
    #[gpui_kit::test]
    async fn dropped_files_open_the_dialog(cx: &mut TestAppContext) {
        for (x, y) in [(700., 120.), (700., 520.)] {
            let s = setup(cx);
            let paths = ExternalPaths([s.csv.clone()].into_iter().collect());
            let position = point(px(x), px(y));
            cx.update_window(s.window, |_, window, cx| {
                window.dispatch_keystroke(Keystroke::parse("a").unwrap(), cx);
                for event in [
                    FileDropEvent::Entered { position, paths: paths.clone() },
                    FileDropEvent::Pending { position },
                    FileDropEvent::Submit { position },
                ] {
                    window.dispatch_event(event.to_platform_input(), cx);
                    window.render_frame(cx);
                }
            })
            .unwrap();
            import_and_check(cx, s).await;
        }
    }
}
