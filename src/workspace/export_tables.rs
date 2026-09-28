//! Export tables: pick tables and what to write for each (structure, a
//! DROP first, data), as one SQL script or a CSV file per table.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use gpui_kit::component::button::ButtonGroup;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{Selectable as _, WindowExt as _};

use super::*;
use crate::quack::{TableExport, TablesFormat};

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Sql,
    Csv,
}

#[derive(Clone, Copy, PartialEq)]
enum Part {
    Structure,
    Drop,
    Data,
}

#[derive(Clone, Copy, Default)]
struct Pick {
    structure: bool,
    drop: bool,
    data: bool,
}

impl Pick {
    fn get(&self, part: Part) -> bool {
        match part {
            Part::Structure => self.structure,
            Part::Drop => self.drop,
            Part::Data => self.data,
        }
    }

    fn set(&mut self, part: Part, on: bool) {
        match part {
            Part::Structure => self.structure = on,
            Part::Drop => self.drop = on,
            Part::Data => self.data = on,
        }
    }
}

const DELIMITERS: [(char, &str); 4] = [(',', "Comma"), (';', "Semicolon"), ('\t', "Tab"), ('|', "Pipe")];

/// The dialog's state: which tables, which parts, which format.
pub(super) struct TablesExport {
    tables: Vec<Relation>,
    picks: Vec<Pick>,
    /// Group headers are shown with their database too.
    multi_db: bool,
    collapsed: HashSet<String>,
    format: Format,
    gzip: bool,
    header: bool,
    null_as_empty: bool,
    delimiter: char,
}

fn group_key(rel: &Relation) -> String {
    format!("{}.{}", rel.database, rel.schema)
}

impl TablesExport {
    fn new(mut tables: Vec<Relation>, selected: &[String]) -> Self {
        tables.sort_by(|a, b| {
            (&a.database, &a.schema, a.name.to_lowercase()).cmp(&(&b.database, &b.schema, b.name.to_lowercase()))
        });
        let picks = tables
            .iter()
            .map(|t| {
                let on = selected.contains(&t.qualified());
                Pick { structure: on, drop: false, data: on }
            })
            .collect();
        let multi_db = tables.windows(2).any(|w| w[0].database != w[1].database);
        Self {
            tables,
            picks,
            multi_db,
            collapsed: HashSet::new(),
            format: Format::Sql,
            gzip: false,
            header: true,
            null_as_empty: true,
            delimiter: ',',
        }
    }

    fn parts(&self) -> &'static [Part] {
        match self.format {
            Format::Sql => &[Part::Structure, Part::Drop, Part::Data],
            Format::Csv => &[Part::Data],
        }
    }

    /// The selected tables and what to write for each.
    fn selection(&self) -> Vec<TableExport> {
        self.tables
            .iter()
            .zip(&self.picks)
            .filter_map(|(rel, p)| {
                let t = match self.format {
                    Format::Sql => TableExport { rel: rel.clone(), structure: p.structure, drop: p.drop, data: p.data },
                    Format::Csv => TableExport { rel: rel.clone(), structure: false, drop: false, data: p.data },
                };
                (t.structure || t.drop || t.data).then_some(t)
            })
            .collect()
    }

    fn tables_format(&self) -> TablesFormat {
        match self.format {
            Format::Sql => TablesFormat::Sql { gzip: self.gzip },
            Format::Csv => TablesFormat::Csv {
                header: self.header,
                delimiter: self.delimiter,
                null_as_empty: self.null_as_empty,
                gzip: self.gzip,
            },
        }
    }

    fn group_all(&self, key: &str, part: Part) -> bool {
        self.tables
            .iter()
            .zip(&self.picks)
            .filter(|(r, _)| group_key(r) == key)
            .all(|(_, p)| p.get(part))
    }

    fn set_group(&mut self, key: &str, part: Part, on: bool) {
        for (rel, pick) in self.tables.iter().zip(self.picks.iter_mut()) {
            if group_key(rel) == key {
                pick.set(part, on);
            }
        }
    }

    fn checkbox(id: impl Into<ElementId>, checked: bool, on_toggle: impl Fn(bool, &mut App) + 'static) -> Checkbox {
        Checkbox::new(id)
            .checked(checked)
            .on_click(move |checked: &bool, _, cx| on_toggle(*checked, cx))
    }

    fn render_tree(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let parts = self.parts();
        let cells = |row: Stateful<Div>, boxes: Vec<Checkbox>| {
            row.children(boxes.into_iter().map(|b| h_flex().w(px(64.)).flex_none().justify_center().child(b)))
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut last_group: Option<String> = None;
        for (ix, (rel, pick)) in self.tables.iter().zip(&self.picks).enumerate() {
            let key = group_key(rel);
            if last_group.as_deref() != Some(key.as_str()) {
                last_group = Some(key.clone());
                let collapsed = self.collapsed.contains(&key);
                let label = if self.multi_db { key.clone() } else { rel.schema.clone() };
                let boxes = parts
                    .iter()
                    .map(|&part| {
                        let (view, key) = (view.clone(), key.clone());
                        Self::checkbox(
                            SharedString::from(format!("group-{key}-{}", part as u8)),
                            self.group_all(&key, part),
                            move |on, cx| {
                                view.update(cx, |this, cx| {
                                    this.set_group(&key, part, on);
                                    cx.notify();
                                })
                                .ok();
                            },
                        )
                    })
                    .collect();
                let toggle = key.clone();
                let head = h_flex()
                    .id(SharedString::from(format!("group-{key}")))
                    .h(px(26.))
                    .gap_1p5()
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .cursor_pointer()
                            .text_sm()
                            .child(
                                Icon::new(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown })
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(Icon::new(IconName::Layers).xsmall().text_color(theme.muted_foreground))
                            .child(div().truncate().child(label))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    if !this.collapsed.remove(&toggle) {
                                        this.collapsed.insert(toggle.clone());
                                    }
                                    cx.notify();
                                }),
                            ),
                    );
                rows.push(cells(head, boxes).into_any_element());
            }
            if self.collapsed.contains(&key) {
                continue;
            }
            let boxes = parts
                .iter()
                .map(|&part| {
                    let view = view.clone();
                    Self::checkbox(("pick", ix * 3 + part as usize), pick.get(part), move |on, cx| {
                        view.update(cx, |this, cx| {
                            this.picks[ix].set(part, on);
                            cx.notify();
                        })
                        .ok();
                    })
                })
                .collect();
            let row = h_flex()
                .id(("table", ix))
                .h(px(26.))
                .gap_1p5()
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .pl_6()
                        .gap_1p5()
                        .text_sm()
                        .child(Icon::new(IconName::Table).xsmall().text_color(theme.muted_foreground))
                        .child(div().truncate().child(rel.name.clone())),
                );
            rows.push(cells(row, boxes).into_any_element());
        }
        let labels = parts.iter().map(|part| {
            div()
                .w(px(64.))
                .flex_none()
                .text_center()
                .child(match part {
                    Part::Structure => "Structure",
                    Part::Drop => "Drop",
                    Part::Data => "Data",
                })
        });
        v_flex()
            .w(px(420.))
            .flex_none()
            .border_r_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .h(px(28.))
                    .px_2()
                    .gap_1p5()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().flex_1().child("Tables"))
                    .children(labels),
            )
            .child(
                v_flex()
                    .id("export-tree")
                    .h(px(360.))
                    .overflow_y_scroll()
                    .px_2()
                    .py_1()
                    .children(rows)
                    .when(self.tables.is_empty(), |el| {
                        el.child(
                            div()
                                .py_6()
                                .text_sm()
                                .text_center()
                                .text_color(theme.muted_foreground)
                                .child("No tables to export"),
                        )
                    }),
            )
    }

    fn render_options(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let view = cx.entity().downgrade();
        let option = |id: &'static str, label: &'static str, checked: bool, set: fn(&mut Self, bool)| {
            let view = view.clone();
            Checkbox::new(id).label(label).checked(checked).on_click(move |on: &bool, _, cx| {
                let on = *on;
                view.update(cx, |this, cx| {
                    set(this, on);
                    cx.notify();
                })
                .ok();
            })
        };
        let body = match self.format {
            Format::Sql => v_flex()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("One script with every selected table. Load it into another DuckDB to recreate them."),
                )
                .child(option("gzip", "Compress the file using gzip", self.gzip, |t, on| t.gzip = on)),
            Format::Csv => v_flex()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("A CSV file per table. Several tables go into a folder you choose."),
                )
                .child(option("header", "Put field names in the first row", self.header, |t, on| t.header = on))
                .child(option("null-empty", "Write NULL as an empty value", self.null_as_empty, |t, on| {
                    t.null_as_empty = on
                }))
                .child(option("gzip", "Compress the files using gzip", self.gzip, |t, on| t.gzip = on))
                .child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child("Delimiter")
                        .child(
                            ButtonGroup::new("delimiter")
                                .xsmall()
                                .outline()
                                .children(DELIMITERS.iter().enumerate().map(|(i, (c, label))| {
                                    Button::new(("delim", i)).label(*label).selected(self.delimiter == *c)
                                }))
                                .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                                    if let Some((c, _)) = selected.first().and_then(|i| DELIMITERS.get(*i)) {
                                        this.delimiter = *c;
                                        cx.notify();
                                    }
                                })),
                        ),
                ),
        };
        v_flex()
            .flex_1()
            .p_4()
            .gap_4()
            .child(
                h_flex().justify_center().child(
                    ButtonGroup::new("format")
                        .small()
                        .outline()
                        .child(Button::new("format-sql").label("SQL").selected(self.format == Format::Sql))
                        .child(Button::new("format-csv").label("CSV").selected(self.format == Format::Csv))
                        .on_click(cx.listener(|this, selected: &Vec<usize>, _, cx| {
                            this.format = if selected.first() == Some(&1) { Format::Csv } else { Format::Sql };
                            cx.notify();
                        })),
                ),
            )
            .child(body)
    }
}

impl Render for TablesExport {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        h_flex()
            .h(px(390.))
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius)
            .overflow_hidden()
            .child(self.render_tree(cx))
            .child(self.render_options(cx))
    }
}

impl Workspace {
    /// Open the dialog with the tables picked in the sidebar ticked.
    pub(super) fn open_tables_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_tables_export_with(self.picked_relations(), window, cx)
    }

    /// Open the dialog with `selected` (qualified names) ticked.
    pub(super) fn open_tables_export_with(&mut self, selected: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let tables: Vec<Relation> = self
            .catalog
            .relations
            .iter()
            .filter(|r| !r.is_view)
            .filter(|r| self.focus_db.as_ref().is_none_or(|db| db.eq_ignore_ascii_case(&r.database)))
            .cloned()
            .collect();
        let state = cx.new(|_| TablesExport::new(tables, &selected));
        let workspace = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme();
            let (count, format) = {
                let s = state.read(cx);
                (s.selection().len(), s.format)
            };
            let (start, state_for_export) = (workspace.clone(), state.clone());
            dialog
                .title("Export Tables")
                .w(px(860.))
                .child(state.clone())
                .footer(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(div().text_sm().text_color(theme.muted_foreground).child(match count {
                            0 => "Pick the tables to export".to_string(),
                            1 => "1 table".to_string(),
                            n => format!("{n} tables"),
                        }))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("export-cancel")
                                        .small()
                                        .ghost()
                                        .label("Cancel")
                                        .on_click(|_, window, cx| window.close_dialog(cx)),
                                )
                                .child(
                                    Button::new("export-tables-ok")
                                        .small()
                                        .icon(IconName::Download)
                                        .label(if format == Format::Csv { "Export CSV…" } else { "Export SQL…" })
                                        .disabled(count == 0)
                                        .on_click(move |_, window, cx| {
                                            let (tables, format) = {
                                                let s = state_for_export.read(cx);
                                                (s.selection(), s.tables_format())
                                            };
                                            window.close_dialog(cx);
                                            start
                                                .update(cx, |this, cx| this.choose_export_destination(tables, format, window, cx))
                                                .ok();
                                        }),
                                ),
                        ),
                )
        });
    }

    /// Ask where the export goes: a file, or a folder for several CSVs.
    fn choose_export_destination(
        &mut self,
        tables: Vec<TableExport>,
        format: TablesFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dir = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| std::path::PathBuf::from("/"));
        let csv_files = tables.iter().filter(|t| t.data).count();
        let folder = matches!(format, TablesFormat::Csv { .. }) && csv_files > 1;
        let destination: futures::future::LocalBoxFuture<'static, Option<std::path::PathBuf>> = if folder {
            let paths = cx.prompt_for_paths(PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some("Export Here".into()),
            });
            Box::pin(async move { paths.await.ok()?.ok()??.into_iter().next() })
        } else {
            let stem = match (&format, tables.as_slice()) {
                (TablesFormat::Csv { .. }, [one]) => one.rel.name.clone(),
                _ => format!(
                    "{}_{}",
                    self.focus_db
                        .clone()
                        .or_else(|| self.catalog.current_database.clone())
                        .unwrap_or_else(|| self.profile.name.clone()),
                    chrono::Local::now().format("%Y%m%d%H%M")
                ),
            };
            let name = format!("{}.{}", stem.replace(['/', ':'], "_"), format.extension());
            let path = cx.prompt_for_new_path(&dir, Some(&name));
            Box::pin(async move { path.await.ok()?.ok()? })
        };
        cx.spawn_in(window, async move |this, cx| {
            let Some(dest) = destination.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| this.run_tables_export(tables, format, dest, window, cx))
                .ok();
        })
        .detach();
    }

    fn run_tables_export(
        &mut self,
        tables: Vec<TableExport>,
        format: TablesFormat,
        dest: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let total = match format {
            TablesFormat::Sql { .. } => tables.len(),
            TablesFormat::Csv { .. } => tables.iter().filter(|t| t.data).count(),
        };
        let done = Arc::new(AtomicU64::new(0));
        self.exporting = true;
        self.notice = Some((false, format!("Exporting {total} tables…")));
        cx.notify();
        let client = self.client.clone();
        let job = cx.background_executor().spawn({
            let (done, dest) = (done.clone(), dest.clone());
            async move { client.export_tables(&tables, &format, &dest, &done) }
        });
        cx.spawn_in(window, async move |this, cx| {
            // Count finished tables in the results bar while it runs.
            let ticker = {
                let (this, done) = (this.clone(), done.clone());
                let executor = cx.background_executor().clone();
                let mut cx = cx.clone();
                cx.clone().spawn(async move |_| {
                    loop {
                        executor.timer(Duration::from_millis(200)).await;
                        let n = done.load(Relaxed);
                        let alive = this
                            .update(&mut cx, |this, cx| {
                                if this.exporting {
                                    this.notice = Some((false, format!("Exporting tables… {n} of {total}")));
                                    cx.notify();
                                }
                                this.exporting
                            })
                            .unwrap_or(false);
                        if !alive {
                            break;
                        }
                    }
                })
            };
            let result = job.await;
            drop(ticker);
            this.update(cx, |this, cx| {
                this.exporting = false;
                let place = dest.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                this.notice = Some(match result {
                    Ok(rows) => (
                        false,
                        format!(
                            "Exported {total} {} ({} rows) to {place}",
                            if total == 1 { "table" } else { "tables" },
                            group_digits(rows as usize)
                        ),
                    ),
                    Err(e) => (true, format!("Export failed: {e:#}")),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
