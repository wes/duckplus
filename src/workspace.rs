//! The workspace window: one per connected server.
//!
//! ┌ title bar ─ connection · server version · latency ───────────────┐
//! │ schema tree  │ SQL editor                                        │
//! │  (filter)    ├───────────────────────────────────────────────────┤
//! │  server      │ result grid (virtualized, lazily formatted)       │
//! └──────────────┴───────────────────────────────────────────────────┘

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Root, Sizable as _, StyledExt as _, TitleBar, WindowExt as _, h_flex,
    resizable_panel, v_flex, v_resizable,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

mod export_tables;
mod import;

use crate::complete::{SchemaIndex, SqlCompletions};
use crate::quack::{
    Catalog, ExportFormat, ExportRows, KeyKind, QuackClient, Relation, ServerInfo,
    is_read_only_query, quote_ident, sql_literal,
};
use crate::query_log::{LogStatus, QueryLog};
use crate::results::{CellEditor, ResultsDelegate};
use crate::store::Profile;
use crate::theme::tag_color;
use crate::{
    AppState, CancelQuery, CopyCsv, DiscardChanges, EditCell, FocusEditor, FocusFilter, FormatSql,
    ExportCsv, ExportSql, ExportTables, InspectCell, NewQuery, OpenConnections, OpenSettings, RefreshSchema,
    RunAll, RunQuery,
    SaveChanges, ToggleComment, ToggleQueryLog,
};

const WELCOME_SQL: &str = "-- ⌘↵ runs the statement (or your selection)\n\
SELECT database_name, schema_name, table_name, estimated_size\n\
FROM duckdb_tables()\n\
ORDER BY estimated_size DESC;\n";

/// Built-in admin views; each is just SQL, so it lands in the editor ready to tweak.
const ADMIN: &[(&str, IconName, &str)] = &[
    (
        "Databases",
        IconName::Database,
        "SELECT database_name, type, path, readonly, internal, comment\nFROM duckdb_databases()\nORDER BY internal, database_name;",
    ),
    (
        "Storage",
        IconName::HardDrive,
        "SELECT *\nFROM pragma_database_size();",
    ),
    (
        "Extensions",
        IconName::Puzzle,
        "SELECT extension_name, loaded, installed, extension_version, install_mode, description\nFROM duckdb_extensions()\nORDER BY loaded DESC, installed DESC, extension_name;",
    ),
    (
        "Settings",
        IconName::Gauge,
        "SELECT name, value, input_type, scope, description\nFROM duckdb_settings()\nORDER BY name;",
    ),
    (
        "Memory",
        IconName::Activity,
        "SELECT tag, memory_usage_bytes, temporary_storage_bytes\nFROM duckdb_memory()\nORDER BY memory_usage_bytes DESC;",
    ),
    (
        "Secrets",
        IconName::KeyRound,
        "SELECT name, type, provider, persistent, storage, scope\nFROM duckdb_secrets();",
    ),
    (
        "Quack servers",
        IconName::Server,
        "FROM quack_server_list();",
    ),
];

pub fn logo(size: f32) -> impl IntoElement {
    div()
        .size(px(size))
        .rounded(px(size * 0.3))
        .bg(rgb(0xffd43b))
        .flex()
        .items_center()
        .justify_center()
        .child(div().size(px(size * 0.34)).rounded_full().bg(rgb(0x17140a)))
}

enum RunState {
    Idle,
    Running(Instant),
    Done {
        rows: usize,
        elapsed: Duration,
        truncated: bool,
    },
    Failed(String),
    Cancelled,
    /// Destructive statement waiting for a second ⌘↵.
    Confirm(String),
}

/// Where the grid's rows came from. A table can be re-sorted on the server
/// and, once its primary key is known, edited.
#[derive(Clone)]
enum ResultSource {
    Query,
    Table {
        rel: Relation,
        /// Server-side ORDER BY: column name and descending.
        sort: Option<(String, bool)>,
        /// Columns identifying a row for UPDATEs, and where they came from.
        key: Vec<String>,
        key_kind: Option<KeyKind>,
        /// The editor query that produced these rows, if it wasn't a plain
        /// table browse; reloads re-run it and sorting stays client-side.
        sql: Option<String>,
    },
}

/// What's selected in the result grid, mirrored from its events.
#[derive(Clone, Copy)]
enum GridSelection {
    Row(usize),
    Column(usize),
    Cell(usize, usize),
}

pub struct Workspace {
    focus: FocusHandle,
    profile: Profile,
    client: QuackClient,
    info: ServerInfo,
    catalog: Catalog,
    catalog_error: Option<String>,
    loading_catalog: bool,
    collapsed: HashSet<String>,
    selected_relation: Option<String>,
    /// Tables picked in the sidebar with ⌘- or ⇧-click (qualified names).
    /// Empty means just the open one, `selected_relation`.
    marked: HashSet<String>,
    /// Where a ⇧-click range starts.
    mark_anchor: Option<String>,
    filter: Entity<InputState>,
    editor: Entity<EditorState>,
    completions: Rc<SqlCompletions>,
    /// Buffer length at the last change, to tell deletes from inserts.
    editor_len: usize,
    table: Entity<TableState<ResultsDelegate>>,
    grid_selection: Option<GridSelection>,
    run: RunState,
    query_task: Option<Task<()>>,
    /// What the grid shows, and what's running for it.
    source: ResultSource,
    pending_source: ResultSource,
    /// Label for runs that didn't come from the editor (a table, an admin view).
    run_label: Option<String>,
    /// Database every query is scoped to (`USE`), if any.
    focus_db: Option<String>,
    /// One-line message in the results bar: (is_error, text).
    notice: Option<(bool, String)>,
    saving: bool,
    edit_sub: Option<Subscription>,
    /// Key columns picked by hand, per table (qualified name), for tables
    /// without a declared primary key.
    chosen_keys: HashMap<String, String>,
    /// Every query this window ran, and the one in flight.
    log: QueryLog,
    log_scroll: ScrollHandle,
    running_log: Option<u64>,
    /// CSV imports in flight (and just finished), shown in the sidebar.
    imports: Vec<import::ImportJob>,
    next_import: u64,
    import_ticking: bool,
    /// A schema refresh was asked for while one was already loading.
    catalog_dirty: bool,
    /// Files being dragged over the window from outside.
    dropping: Option<Vec<std::path::PathBuf>>,
    /// The SQL (and database) behind the rows in the grid, for exports.
    shown_query: Option<(String, Option<String>)>,
    exporting: bool,
    _subs: Vec<Subscription>,
}

impl Workspace {
    pub fn new(
        profile: Profile,
        client: QuackClient,
        info: ServerInfo,
        initial_sql: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        window.set_window_title(&format!("{} — DuckPlus", profile.name));
        let profile_db = profile.database.clone();
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter tables  ⌘P"));
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .line_number(true)
                .default_value(WELCOME_SQL)
        });
        let completions = Rc::new(SqlCompletions::default());
        editor.update(cx, |s, _| {
            let lsp = s.lsp_mut();
            lsp.completion_provider = Some(completions.clone());
            lsp.completion_menu.max_width = px(420.);
        });
        let this_view = cx.entity().downgrade();
        let table = cx.new(|cx| {
            let font_size = AppState::store(cx).settings.ui_font_size;
            let mut delegate = ResultsDelegate::new(font_size);
            let view = this_view.clone();
            delegate.on_sort = Some(Rc::new(move |col, window, cx| {
                view.update(cx, |this, cx| this.sort_by(col, window, cx))
                    .ok();
            }));
            let view = this_view.clone();
            delegate.on_inspect = Some(Rc::new(move |row, col, window, cx| {
                view.update(cx, |this, cx| this.inspect(row, col, window, cx))
                    .ok();
            }));
            TableState::new(delegate, window, cx)
                .cell_selectable(true)
                .col_selectable(false)
                .col_movable(false)
                .sortable(false)
        });
        let subs = vec![
            cx.observe(&filter, |_, _, cx| cx.notify()),
            // The editor only asks for completions on typed input; keep an
            // open menu in step with deletes too. (Growing edits are typing,
            // already handled, or an accepted suggestion, which closes it.)
            cx.subscribe(&editor, |this, editor, ev: &InputEvent, cx| {
                if !matches!(ev, InputEvent::Change) {
                    return;
                }
                let s = editor.read(cx);
                let len = s.text().len();
                let shrank = len < std::mem::replace(&mut this.editor_len, len);
                if shrank && s.completion_menu_state().open {
                    let items = this.completions.items(s.text(), s.cursor());
                    editor.update(cx, |s, cx| s.present_completion_items(0, "", items, cx));
                }
            }),
            // The editor remembers where a completion session started and
            // ignores keystrokes before it, even once the menu is gone.
            // Pin it to the start so completions work anywhere.
            cx.observe(&editor, |_, editor, cx| {
                let menu = editor.read(cx).completion_menu_state();
                if !menu.open && menu.trigger_start_offset != Some(0) {
                    editor.update(cx, |s, cx| s.present_completion_items(0, "", vec![], cx));
                }
            }),
            cx.observe_global::<AppState>(|this, cx| {
                // Column widths are sized for the grid font; re-fit on change.
                let size = AppState::store(cx).settings.ui_font_size;
                this.table.update(cx, |t, cx| {
                    if t.delegate_mut().set_font_size(size) {
                        t.refresh(cx);
                    }
                });
                cx.notify()
            }),
            cx.subscribe_in(&table, window, |this, _, event: &TableEvent, window, cx| {
                this.grid_selection = match *event {
                    TableEvent::SelectRow(r) => Some(GridSelection::Row(r)),
                    TableEvent::SelectColumn(c) => Some(GridSelection::Column(c)),
                    TableEvent::SelectCell(r, c) => Some(GridSelection::Cell(r, c)),
                    TableEvent::ClearSelection => None,
                    TableEvent::DoubleClickedCell(r, c) => {
                        if this.table.read(cx).delegate().editable {
                            this.start_edit(r, c, window, cx);
                        } else {
                            this.inspect(r, c, window, cx);
                        }
                        return;
                    }
                    _ => return,
                };
            }),
        ];
        editor.update(cx, |s, cx| s.focus(window, cx));

        let mut this = Self {
            focus: cx.focus_handle(),
            profile,
            client,
            info,
            catalog: Catalog::default(),
            catalog_error: None,
            loading_catalog: false,
            collapsed: HashSet::new(),
            selected_relation: None,
            marked: HashSet::new(),
            mark_anchor: None,
            filter,
            editor,
            completions,
            editor_len: WELCOME_SQL.len(),
            table,
            grid_selection: None,
            run: RunState::Idle,
            query_task: None,
            source: ResultSource::Query,
            pending_source: ResultSource::Query,
            run_label: None,
            focus_db: profile_db,
            notice: None,
            saving: false,
            edit_sub: None,
            chosen_keys: HashMap::new(),
            log: QueryLog::default(),
            log_scroll: ScrollHandle::new(),
            running_log: None,
            imports: Vec::new(),
            next_import: 0,
            import_ticking: false,
            catalog_dirty: false,
            dropping: None,
            shown_query: None,
            exporting: false,
            _subs: subs,
        };
        this.refresh_catalog(window, cx);
        if let Some(sql) = initial_sql {
            this.set_sql(&sql, window, cx);
            this.run_sql(sql, false, ResultSource::Query, None, window, cx);
        }
        this
    }

    fn refresh_catalog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading_catalog {
            self.catalog_dirty = true;
            return;
        }
        self.loading_catalog = true;
        cx.notify();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let (catalog, ping) = cx
                .background_executor()
                .spawn(async move { (client.catalog(), client.ping()) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.loading_catalog = false;
                match catalog {
                    Ok(c) => {
                        if let Some(db) = &this.focus_db {
                            if !c.databases.is_empty() && !c.databases.contains(db) {
                                this.focus_db = None;
                            }
                        }
                        this.catalog = c;
                        this.catalog_error = None;
                        this.update_completions();
                    }
                    Err(e) => this.catalog_error = Some(format!("{e:#}")),
                }
                if let Ok(info) = ping {
                    this.info = info;
                }
                if std::mem::take(&mut this.catalog_dirty) {
                    this.refresh_catalog(window, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn update_completions(&self) {
        self.completions
            .set_index(SchemaIndex::new(&self.catalog, self.focus_db.as_deref()));
    }

    fn set_sql(&mut self, sql: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |s, cx| s.set_value(sql.to_string(), window, cx));
    }

    fn has_edits(&self, cx: &App) -> bool {
        let d = self.table.read(cx).delegate();
        !d.edits.is_empty() || d.editing.is_some()
    }

    /// Run SQL into the grid. `label` names runs that didn't come from the
    /// editor (the editor's text is never touched by them).
    fn run_sql(
        &mut self,
        sql: String,
        confirmed: bool,
        source: ResultSource,
        label: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sql = sql.trim().to_string();
        if sql.is_empty() {
            return;
        }
        if self.has_edits(cx) {
            self.notice = Some((true, "Save (⌘S) or discard (Esc) your changes first".into()));
            cx.notify();
            return;
        }
        self.notice = None;
        self.run_label = label;
        // A plain `SELECT … FROM one_table …` from the editor is editable
        // like a browsed table.
        self.pending_source = match source {
            ResultSource::Query => crate::simple_query::single_table(&sql)
                .and_then(|parts| self.resolve_table(&parts))
                .map_or(ResultSource::Query, |rel| ResultSource::Table {
                    rel,
                    sort: None,
                    key: Vec::new(),
                    key_kind: None,
                    sql: Some(sql.clone()),
                }),
            other => other,
        };
        let settings = AppState::store(cx).settings.clone();
        if settings.confirm_destructive && !confirmed && is_destructive(&sql) {
            self.run = RunState::Confirm(sql);
            cx.notify();
            return;
        }

        self.client.cancel();
        let started = Instant::now();
        let origin = self.run_label.clone().unwrap_or_else(|| "Editor".into());
        let log_id = self.log.start(origin, &sql);
        self.running_log = Some(log_id);
        self.log_scroll.scroll_to_bottom();
        self.run = RunState::Running(started);
        cx.notify();

        let client = self.client.clone();
        let limit = settings.row_limit;
        let database = self.focus_db.clone();
        let ran = (sql.clone(), database.clone());
        let query = cx
            .background_executor()
            .spawn(async move { client.run(&sql, limit, database.as_deref()) });

        self.query_task = Some(cx.spawn_in(window, async move |this, cx| {
            // Tick the elapsed timer while the query is in flight.
            let ticker = {
                let this = this.clone();
                let executor = cx.background_executor().clone();
                let mut cx = cx.clone();
                cx.clone().spawn(async move |_| {
                    loop {
                        executor.timer(Duration::from_millis(100)).await;
                        let alive = this
                            .update(&mut cx, |this, cx| {
                                let running = matches!(this.run, RunState::Running(_));
                                if running {
                                    cx.notify();
                                }
                                running
                            })
                            .unwrap_or(false);
                        if !alive {
                            break;
                        }
                    }
                })
            };

            let result = query.await;
            drop(ticker);
            this.update(cx, |this, cx| {
                match result {
                    Ok(r) => {
                        this.log.finish(
                            log_id,
                            r.elapsed,
                            LogStatus::Rows {
                                rows: r.rows,
                                truncated: r.truncated,
                            },
                        );
                        this.run = RunState::Done {
                            rows: r.rows,
                            elapsed: r.elapsed,
                            truncated: r.truncated,
                        };
                        let r = Arc::new(r);
                        this.source = this.pending_source.clone();
                        this.shown_query = Some(ran);
                        // A server-sorted table keeps its header arrow.
                        let sort = match &this.source {
                            ResultSource::Table {
                                sort: Some((name, desc)),
                                ..
                            } => r
                                .columns
                                .iter()
                                .position(|c| &c.name == name)
                                .map(|ix| (ix, *desc)),
                            _ => None,
                        };
                        this.table.update(cx, |t, cx| {
                            t.clear_selection(cx);
                            t.delegate_mut().set_result(Some(r));
                            t.delegate_mut().sort = sort;
                            t.refresh(cx);
                            t.scroll_to_row(0, cx);
                        });
                        this.load_row_key(cx);
                    }
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let cancelled = msg.contains("INTERRUPT") || msg.contains("nterrupted");
                        this.log.finish(
                            log_id,
                            started.elapsed(),
                            if cancelled {
                                LogStatus::Cancelled
                            } else {
                                LogStatus::Failed(msg.clone())
                            },
                        );
                        this.run = if cancelled {
                            RunState::Cancelled
                        } else {
                            RunState::Failed(msg)
                        };
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn on_run(&mut self, _: &RunQuery, window: &mut Window, cx: &mut Context<Self>) {
        let (selected, all) = {
            let e = self.editor.read(cx);
            (e.selected_value().to_string(), e.value().to_string())
        };
        let sql = if selected.trim().is_empty() {
            all
        } else {
            selected
        };
        let confirmed = matches!(&self.run, RunState::Confirm(pending) if *pending == sql.trim());
        self.run_sql(sql, confirmed, ResultSource::Query, None, window, cx);
    }

    fn on_run_all(&mut self, _: &RunAll, window: &mut Window, cx: &mut Context<Self>) {
        let sql = self.editor.read(cx).value().to_string();
        let confirmed = matches!(&self.run, RunState::Confirm(pending) if *pending == sql.trim());
        self.run_sql(sql, confirmed, ResultSource::Query, None, window, cx);
    }

    /// Another query window on the same server, with its own connections.
    fn on_new_query(&mut self, _: &NewQuery, _: &mut Window, cx: &mut Context<Self>) {
        match self.client.fork() {
            Ok(client) => {
                crate::open_workspace(self.profile.clone(), client, self.info.clone(), None, cx)
            }
            Err(e) => {
                self.run = RunState::Failed(format!("Couldn't open a new query window: {e:#}"));
                cx.notify();
            }
        }
    }

    /// Toggle `-- ` on every line touched by the selection (or the cursor's line).
    fn on_toggle_comment(
        &mut self,
        _: &ToggleComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.editor.focus_handle(cx).is_focused(window) {
            return;
        }
        self.editor.update(cx, |e, cx| {
            let text = e.value().to_string();
            let sel = e.selected_range();
            let start = text[..sel.start].rfind('\n').map_or(0, |i| i + 1);
            // A selection ending at the start of a line doesn't include that line.
            let last = if sel.end > sel.start && text[..sel.end].ends_with('\n') {
                sel.end - 1
            } else {
                sel.end
            };
            let end = text[last..].find('\n').map_or(text.len(), |i| last + i);
            let block = &text[start..end];

            let (new_block, delta) = toggle_line_comments(block);
            if new_block == block {
                return;
            }
            e.set_selected_range(start..end, cx);
            e.replace(new_block.clone(), window, cx);
            if sel.is_empty() {
                let cursor = (sel.start as isize + delta).max(start as isize) as usize;
                e.set_selected_range(cursor..cursor, cx);
            } else {
                e.set_selected_range(start..start + new_block.len(), cx);
            }
        });
    }

    /// Format the selection, or the whole editor if nothing is selected.
    /// Goes through the editor's edit path, so ⌘Z undoes it.
    fn on_toggle_log(&mut self, _: &ToggleQueryLog, _: &mut Window, cx: &mut Context<Self>) {
        AppState::update_store(cx, |s| {
            s.settings.show_query_log = !s.settings.show_query_log;
            s.save_settings();
        });
        self.log_scroll.scroll_to_bottom();
        cx.notify();
    }

    fn on_format(&mut self, _: &FormatSql, window: &mut Window, cx: &mut Context<Self>) {
        let (text, sel) = {
            let e = self.editor.read(cx);
            (e.value().to_string(), e.selected_range())
        };
        let range = if sel.is_empty() {
            0..text.len()
        } else {
            sel.clone()
        };
        let source = &text[range.clone()];
        if source.trim().is_empty() {
            return;
        }
        let Some(formatted) = crate::format::format_sql(source) else {
            self.notice = Some((true, "Couldn't format this SQL without changing it".into()));
            cx.notify();
            return;
        };
        if formatted == source {
            return;
        }
        self.editor.update(cx, |e, cx| {
            e.set_selected_range(range.clone(), cx);
            e.replace(formatted.clone(), window, cx);
            if sel.is_empty() {
                e.set_selected_range(0..0, cx);
            } else {
                e.set_selected_range(range.start..range.start + formatted.len(), cx);
            }
        });
        self.editor.update(cx, |e, cx| e.focus(window, cx));
    }

    /// Copy the selected cell's value, or the selected row/column (or the
    /// whole result) as CSV with a header line.
    fn on_copy_csv(&mut self, _: &CopyCsv, _: &mut Window, cx: &mut Context<Self>) {
        let d = self.table.read(cx).delegate();
        let Some(r) = d.result.clone() else {
            return;
        };
        let all_cols: Vec<usize> = (0..r.columns.len()).collect();
        // Grid rows are display rows; map them through any in-place sort.
        let rows = (0..r.rows).map(|i| d.data_row(i));
        let text = match self.grid_selection {
            Some(GridSelection::Cell(row, col)) => {
                d.value(d.data_row(row), col).unwrap_or_default()
            }
            Some(GridSelection::Row(row)) => r.to_csv([d.data_row(row)], &all_cols),
            Some(GridSelection::Column(col)) if col < r.columns.len() => r.to_csv(rows, &[col]),
            _ => r.to_csv(rows, &all_cols),
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn on_cancel(&mut self, _: &CancelQuery, _: &mut Window, cx: &mut Context<Self>) {
        match self.run {
            RunState::Running(started) => {
                if let Some(id) = self.running_log.take() {
                    self.log.finish(id, started.elapsed(), LogStatus::Cancelled);
                }
                // Drop the pending task so a late result is ignored; the UI is
                // free immediately even if the server keeps working.
                self.client.cancel();
                self.query_task = None;
                self.run = RunState::Cancelled;
                cx.notify();
            }
            RunState::Confirm(_) => {
                self.run = RunState::Idle;
                cx.notify();
            }
            _ => {}
        }
    }

    /// Find the catalog table a written name refers to, the way DuckDB
    /// resolves it: `name` and `schema.name` in the focused (else current)
    /// database, `db.name` in its main schema, or fully qualified.
    fn resolve_table(&self, parts: &[String]) -> Option<Relation> {
        let db = self
            .focus_db
            .clone()
            .or_else(|| self.catalog.current_database.clone())?;
        let schema = self
            .catalog
            .current_schema
            .clone()
            .unwrap_or_else(|| "main".into());
        let candidates: Vec<(String, String, String)> = match parts {
            [name] => vec![(db, schema, name.clone())],
            [a, name] => vec![
                (db, a.clone(), name.clone()),
                (a.clone(), "main".into(), name.clone()),
            ],
            [d, s, name] => vec![(d.clone(), s.clone(), name.clone())],
            _ => return None,
        };
        let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        candidates.iter().find_map(|(d, s, n)| {
            self.catalog
                .relations
                .iter()
                .find(|r| eq(&r.database, d) && eq(&r.schema, s) && eq(&r.name, n))
                .cloned()
        })
    }

    /// How this window's queries can name `rel`: without its database when
    /// that's the one in use, and without its schema when that's the default.
    fn short_name(&self, rel: &Relation) -> String {
        let (database, schema) = match &self.focus_db {
            // `USE db` also resets the schema to `main`.
            Some(db) => (Some(db.as_str()), "main"),
            None => (
                self.catalog.current_database.as_deref(),
                self.catalog.current_schema.as_deref().unwrap_or("main"),
            ),
        };
        if !database.is_some_and(|db| db.eq_ignore_ascii_case(&rel.database)) {
            return rel.qualified();
        }
        let ident = |name: &str| {
            let mut chars = name.chars();
            let plain = chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
            if plain && !self.catalog.reserved.contains(&name.to_ascii_lowercase()) {
                name.to_string()
            } else {
                quote_ident(name)
            }
        };
        if rel.schema.eq_ignore_ascii_case(schema) {
            ident(&rel.name)
        } else {
            format!("{}.{}", ident(&rel.schema), ident(&rel.name))
        }
    }

    /// Browse a table in the grid, leaving the editor alone.
    fn open_relation(&mut self, rel: &Relation, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_relation = Some(rel.qualified());
        self.load_table(rel.clone(), None, window, cx);
    }

    fn load_table(
        &mut self,
        rel: Relation,
        sort: Option<(String, bool)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let limit = AppState::store(cx).settings.preview_limit;
        let order = match &sort {
            Some((col, desc)) => format!(
                "\nORDER BY {} {} NULLS LAST",
                quote_ident(col),
                if *desc { "DESC" } else { "ASC" }
            ),
            None => String::new(),
        };
        let sql = format!("FROM {}{order}\nLIMIT {limit};", self.short_name(&rel));
        let label = rel.name.clone();
        let source = ResultSource::Table {
            rel,
            sort,
            key: Vec::new(),
            key_kind: None,
            sql: None,
        };
        self.run_sql(sql, true, source, Some(label), window, cx);
    }

    fn describe_relation(&mut self, rel: &Relation, window: &mut Window, cx: &mut Context<Self>) {
        let sql = format!("DESCRIBE {};", self.short_name(rel));
        self.selected_relation = Some(rel.qualified());
        let label = format!("{} structure", rel.name);
        self.run_sql(sql, true, ResultSource::Query, Some(label), window, cx);
    }

    /// Work out how rows of the shown table are identified for editing: its
    /// primary key, else a UNIQUE constraint, else a column picked by hand,
    /// else a column named `id`. Cells become editable once the result
    /// includes every key column.
    fn load_row_key(&mut self, cx: &mut Context<Self>) {
        let ResultSource::Table { rel, .. } = &self.source else {
            return;
        };
        if rel.is_view {
            return;
        }
        let (client, rel) = (self.client.clone(), rel.clone());
        cx.spawn(async move |this, cx| {
            let declared = cx
                .background_executor()
                .spawn({
                    let rel = rel.clone();
                    async move { client.declared_key(&rel) }
                })
                .await;
            this.update(cx, |this, cx| {
                let declared = match declared {
                    Ok(declared) => declared,
                    Err(e) => {
                        this.notice =
                            Some((true, format!("Couldn't look up the table's key: {e:#}")));
                        None
                    }
                };
                let columns: Vec<String> = this
                    .table
                    .read(cx)
                    .delegate()
                    .result
                    .as_ref()
                    .map(|r| r.columns.iter().map(|c| c.name.clone()).collect())
                    .unwrap_or_default();
                let chosen = this.chosen_keys.get(&rel.qualified()).cloned();
                let guess = columns
                    .iter()
                    .find(|c| c.eq_ignore_ascii_case("id"))
                    .cloned();
                let key = match (declared, chosen, guess) {
                    (Some((KeyKind::Primary, cols)), ..) => Some((KeyKind::Primary, cols)),
                    (_, Some(col), _) => Some((KeyKind::Column, vec![col])),
                    (Some(unique), ..) => Some(unique),
                    (None, None, Some(col)) => Some((KeyKind::Column, vec![col])),
                    (None, None, None) => None,
                };
                this.apply_row_key(&rel, key, cx);
            })
            .ok();
        })
        .detach();
    }

    fn apply_row_key(
        &mut self,
        rel: &Relation,
        key: Option<(KeyKind, Vec<String>)>,
        cx: &mut Context<Self>,
    ) {
        let ResultSource::Table {
            rel: shown,
            key: shown_key,
            key_kind,
            ..
        } = &mut self.source
        else {
            return;
        };
        if shown.qualified() != rel.qualified() {
            return;
        }
        let (kind, cols) = key.map_or((None, Vec::new()), |(k, c)| (Some(k), c));
        *shown_key = cols.clone();
        *key_kind = kind;
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            let has_key_columns = d.result.as_ref().is_some_and(|r| {
                cols.iter()
                    .all(|k| r.columns.iter().any(|c| c.name.eq_ignore_ascii_case(k)))
            });
            d.editable = !cols.is_empty() && has_key_columns;
            cx.notify();
        });
        cx.notify();
    }

    /// Identify rows by a hand-picked column (tables without a primary key).
    fn set_key_column(&mut self, column: String, cx: &mut Context<Self>) {
        let ResultSource::Table { rel, .. } = &self.source else {
            return;
        };
        let rel = rel.clone();
        self.chosen_keys.insert(rel.qualified(), column.clone());
        self.apply_row_key(&rel, Some((KeyKind::Column, vec![column])), cx);
    }

    /// Header click: ascending → descending → unsorted. Tables whose rows
    /// didn't all fit are re-queried with ORDER BY; everything else sorts
    /// the loaded rows in place.
    fn sort_by(&mut self, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (current, truncated, name) = {
            let d = self.table.read(cx).delegate();
            let Some(r) = d.result.as_ref() else { return };
            let Some(c) = r.columns.get(col) else { return };
            (d.sort, r.truncated, c.name.clone())
        };
        let next = match current {
            Some((c, false)) if c == col => Some((col, true)),
            Some((c, true)) if c == col => None,
            _ => Some((col, false)),
        };
        if let (true, ResultSource::Table { rel, sql: None, .. }) = (truncated, &self.source) {
            let rel = rel.clone();
            let sort = next.map(|(_, desc)| (name, desc));
            self.load_table(rel, sort, window, cx);
            return;
        }
        self.table.update(cx, |t, cx| {
            t.delegate_mut().sort_in_place(next);
            t.refresh(cx);
        });
        if truncated && next.is_some() {
            self.notice = Some((false, "Sorted the loaded rows only".into()));
        }
        cx.notify();
    }

    /// Show a cell's full value in a dialog, pretty-printing JSON.
    fn inspect(
        &mut self,
        display_row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (value, column) = {
            let d = self.table.read(cx).delegate();
            let Some(r) = d.result.as_ref() else { return };
            let Some(c) = r.columns.get(col) else { return };
            (d.value(d.data_row(display_row), col), c.clone())
        };
        let raw = value.clone().unwrap_or_else(|| "NULL".into());
        let pretty = value
            .as_deref()
            .filter(|v| v.trim_start().starts_with(['{', '[']))
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .and_then(|j| serde_json::to_string_pretty(&j).ok());
        let is_json = pretty.is_some();
        let text = pretty.unwrap_or_else(|| raw.clone());
        let viewer = cx.new(|cx| {
            let mut s = EditorState::new(window, cx)
                .language(if is_json { "json" } else { "text" })
                .line_number(is_json)
                .soft_wrap(true)
                .default_value(text.clone());
            s.set_readonly(true, cx);
            s
        });
        let title = format!("{} · {}", column.name, column.type_name.to_lowercase());
        window.open_dialog(cx, move |dialog, _, cx| {
            let theme = cx.theme();
            let copy = text.clone();
            dialog
                .title(
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .child(title.clone()),
                )
                .w(px(760.))
                .child(
                    div()
                        .h(px(440.))
                        .border_1()
                        .border_color(theme.border)
                        .rounded(theme.radius)
                        .overflow_hidden()
                        .text_size(px(13.))
                        .child(Editor::new(&viewer).bordered(false).h_full()),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "{} characters{}",
                                    group_digits(raw.chars().count()),
                                    if is_json { " · JSON" } else { "" }
                                )),
                        )
                        .child(
                            Button::new("copy-value")
                                .small()
                                .icon(IconName::Copy)
                                .label("Copy")
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                                }),
                        ),
                )
        });
    }

    /// ↵ on a selected cell starts editing it (when the table is editable).
    fn on_edit_cell(&mut self, _: &EditCell, window: &mut Window, cx: &mut Context<Self>) {
        let d = self.table.read(cx).delegate();
        // Enter inside the cell editor commits it; don't reopen it.
        if !d.editable || d.editing.is_some() {
            cx.propagate();
            return;
        }
        match self.grid_selection {
            Some(GridSelection::Cell(row, col)) => self.start_edit(row, col, window, cx),
            _ => cx.propagate(),
        }
    }

    fn on_inspect_cell(&mut self, _: &InspectCell, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(GridSelection::Cell(row, col)) = self.grid_selection {
            self.inspect(row, col, window, cx);
        }
    }

    // ── editing ──────────────────────────────────────────────────────────

    fn start_edit(
        &mut self,
        display_row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_edit(window, cx);
        let (row, current) = {
            let d = self.table.read(cx).delegate();
            let row = d.data_row(display_row);
            (row, d.value(row, col))
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx).default_value(current.clone().unwrap_or_default())
        });
        self.edit_sub =
            Some(
                cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        this.commit_edit(window, cx);
                    }
                }),
            );
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
        self.table.update(cx, |t, cx| {
            t.delegate_mut().editing = Some(CellEditor { row, col, input });
            cx.notify();
        });
        cx.notify();
    }

    /// Stage the in-place editor's value (if any) and close it.
    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self
            .table
            .update(cx, |t, _| t.delegate_mut().editing.take())
        else {
            return;
        };
        self.edit_sub = None;
        let text = editor.input.read(cx).value().to_string();
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            let was_null = d.value(editor.row, editor.col).is_none();
            // Leaving a NULL cell empty keeps it NULL.
            if !(was_null && text.is_empty()) {
                d.stage(editor.row, editor.col, Some(text));
            }
            cx.notify();
        });
        let focus = self.table.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Esc: first cancels the cell being typed in, then discards staged edits.
    fn on_discard(&mut self, _: &DiscardChanges, window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_edits(cx) {
            cx.propagate();
            return;
        }
        // Unsubscribe first so the editor's blur doesn't stage its text.
        self.edit_sub = None;
        let was_editing = self.table.update(cx, |t, cx| {
            let editing = t.delegate_mut().editing.take().is_some();
            cx.notify();
            editing
        });
        if was_editing {
            let focus = self.table.read(cx).focus_handle(cx);
            window.focus(&focus, cx);
            cx.notify();
            return;
        }
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            d.editing = None;
            d.edits.clear();
            cx.notify();
        });
        self.notice = None;
        cx.notify();
    }

    /// Apply staged edits as UPDATEs in one transaction, then reload.
    fn on_save(&mut self, _: &SaveChanges, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(window, cx);
        if self.saving {
            return;
        }
        let source = self.source.clone();
        let ResultSource::Table {
            rel,
            sort,
            key,
            sql,
            ..
        } = source.clone()
        else {
            return;
        };
        let statements = {
            let d = self.table.read(cx).delegate();
            let Some(r) = d.result.as_ref() else { return };
            if d.edits.is_empty() {
                return;
            }
            let key_cols: Option<Vec<usize>> = key
                .iter()
                .map(|k| {
                    r.columns
                        .iter()
                        .position(|c| c.name.eq_ignore_ascii_case(k))
                })
                .collect();
            let Some(key_cols) = key_cols.filter(|k| !k.is_empty()) else {
                self.notice = Some((true, "Pick a key column to update rows by".into()));
                cx.notify();
                return;
            };
            let mut rows: BTreeMap<usize, Vec<(usize, Option<String>)>> = BTreeMap::new();
            for ((row, col), value) in &d.edits {
                rows.entry(*row).or_default().push((*col, value.clone()));
            }
            let literal = |v: &Option<String>| v.as_deref().map_or("NULL".into(), sql_literal);
            rows.into_iter()
                .flat_map(|(row, sets)| {
                    let set = sets
                        .iter()
                        .map(|(col, v)| format!("{} = {}", quote_ident(&r.columns[*col].name), literal(v)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let filter = key_cols
                        .iter()
                        .map(|&k| {
                            let name = quote_ident(&r.columns[k].name);
                            match r.cell(row, k) {
                                Some(v) => format!("{name} = {}", sql_literal(&v)),
                                None => format!("{name} IS NULL"),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    // Inside the transaction, fail (and roll everything back)
                    // unless the key matches exactly one row: a non-unique
                    // key column would otherwise change several rows, and a
                    // row deleted meanwhile would silently not be saved.
                    let check = format!(
                        "SELECT CASE WHEN count(*) <> 1 THEN error({} || count(*) || ' rows, nothing was saved') END FROM {} WHERE {filter}",
                        sql_literal(&format!("Expected 1 row where {filter}, found ")),
                        rel.qualified(),
                    );
                    let update = format!("UPDATE {} SET {set} WHERE {filter}", rel.qualified());
                    [check, update]
                })
                .collect::<Vec<_>>()
        };
        let changes = self.table.read(cx).delegate().edits.len();
        self.saving = true;
        self.notice = Some((false, "Saving…".into()));
        cx.notify();
        let client = self.client.clone();
        // Log what's actually sent: the whole transaction.
        let script = format!("BEGIN TRANSACTION;\n{};\nCOMMIT;", statements.join(";\n"));
        let log_id = self.log.start("Save", &script);
        self.log_scroll.scroll_to_bottom();
        let started = Instant::now();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.execute_transaction(&statements) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                let status = match &result {
                    Ok(()) => LogStatus::Ok,
                    Err(e) => LogStatus::Failed(format!("{e:#}")),
                };
                this.log.finish(log_id, started.elapsed(), status);
                match result {
                    Ok(()) => {
                        this.table.update(cx, |t, _| t.delegate_mut().edits.clear());
                        match sql {
                            // Re-run the editor query so its filter/order/limit stay.
                            Some(sql) => this.run_sql(sql, true, source, None, window, cx),
                            None => this.load_table(rel, sort, window, cx),
                        }
                        this.notice = Some((
                            false,
                            format!(
                                "Saved {changes} {}",
                                if changes == 1 { "change" } else { "changes" }
                            ),
                        ));
                    }
                    Err(e) => {
                        this.notice =
                            Some((true, format!("Nothing was saved (rolled back): {e:#}")));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ── database focus ───────────────────────────────────────────────────

    fn set_focus_db(&mut self, db: Option<String>, cx: &mut Context<Self>) {
        self.focus_db = db.clone();
        self.profile.database = db.clone();
        self.update_completions();
        let id = self.profile.id.clone();
        AppState::update_store(cx, |s| s.set_database(&id, db));
        cx.notify();
    }

    /// Save the results to a file the user picks, as CSV or as SQL.
    fn export_results(&mut self, as_sql: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(result) = self.table.read(cx).delegate().result.clone() else {
            return;
        };
        if self.exporting || matches!(self.run, RunState::Running(_)) {
            return;
        }
        // All rows are in hand unless the row limit cut them off; then run a
        // read-only query again for the rest (never one that changes data).
        let rerun = match &self.source {
            _ if !result.truncated => None,
            ResultSource::Table { rel, sort, sql: None, .. } => {
                let order = sort.as_ref().map_or(String::new(), |(col, desc)| {
                    format!(" ORDER BY {} {} NULLS LAST", quote_ident(col), if *desc { "DESC" } else { "ASC" })
                });
                Some((format!("FROM {}{order}", self.short_name(rel)), self.focus_db.clone()))
            }
            _ => self.shown_query.clone().filter(|(sql, _)| is_read_only_query(sql)),
        };
        let partial = result.truncated && rerun.is_none();
        let name = match &self.source {
            ResultSource::Table { rel, .. } => rel.name.clone(),
            ResultSource::Query => self.run_label.clone().unwrap_or_else(|| "query_results".into()),
        };
        let stem: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let format = if as_sql {
            ExportFormat::Sql { table: stem.clone() }
        } else {
            ExportFormat::Csv
        };
        let file = format!("{stem}.{}", if as_sql { "sql" } else { "csv" });
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| std::path::PathBuf::from("/"));
        let path = cx.prompt_for_new_path(&dir, Some(&file));
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.exporting = true;
                this.notice = Some((false, "Exporting…".into()));
                cx.notify();
            })
            .ok();
            let written = cx
                .background_executor()
                .spawn({
                    let path = path.clone();
                    async move {
                        let rows = match &rerun {
                            Some((sql, db)) => ExportRows::Query { sql, database: db.as_deref() },
                            None => ExportRows::Loaded(&result),
                        };
                        client.export(rows, &format, &path)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.exporting = false;
                let file = path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                this.notice = Some(match written {
                    Ok(n) => (
                        false,
                        format!(
                            "Exported {} {} to {file}{}",
                            group_digits(n as usize),
                            if n == 1 { "row" } else { "rows" },
                            if partial { " (only the loaded rows: this query can't be re-run safely)" } else { "" }
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

    fn copy_results(&mut self, cx: &mut Context<Self>) {
        if let Some(r) = self.table.read(cx).delegate().result.clone() {
            cx.write_to_clipboard(ClipboardItem::new_string(r.to_tsv()));
            if let RunState::Done { .. } = self.run {
                cx.notify();
            }
        }
    }

    // ── rendering ────────────────────────────────────────────────────────

    /// Title-bar dropdown that scopes every query to one database (`USE`).
    fn render_db_picker(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let databases = self.catalog.databases.clone();
        let focus = self.focus_db.clone();
        let view = cx.entity().downgrade();
        Button::new("focus-db")
            .ghost()
            .xsmall()
            .icon(IconName::Database)
            .label(focus.clone().unwrap_or_else(|| "All databases".into()))
            .tooltip("Focus a database: queries run with USE, the tree shows only it")
            .dropdown_menu(move |menu, _, _| {
                let pick = |db: Option<String>| {
                    let view = view.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        view.update(cx, |this, cx| this.set_focus_db(db.clone(), cx))
                            .ok();
                    }
                };
                let mut menu = menu
                    .item(
                        PopupMenuItem::new("All databases")
                            .checked(focus.is_none())
                            .on_click(pick(None)),
                    )
                    .separator();
                for db in &databases {
                    menu = menu.item(
                        PopupMenuItem::new(db.clone())
                            .checked(focus.as_deref() == Some(db.as_str()))
                            .on_click(pick(Some(db.clone()))),
                    );
                }
                menu
            })
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let db_picker = (!self.catalog.databases.is_empty()).then(|| self.render_db_picker(cx));
        let theme = cx.theme();
        let endpoint = self.profile.location();
        let local = self.client.is_local();
        TitleBar::new().child(
            h_flex()
                .w_full()
                .pr_2()
                .justify_between()
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .size(px(8.))
                                .rounded_full()
                                .bg(tag_color(self.profile.color)),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_semibold()
                                .child(self.profile.name.clone()),
                        )
                        .when(endpoint != self.profile.name, |el| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(endpoint),
                            )
                        })
                        .when_some(db_picker, |el, picker| el.child(picker))
                        .when(self.profile.read_only, |el| {
                            el.child(
                                h_flex()
                                    .gap_1()
                                    .px_1p5()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.border)
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(Icon::new(IconName::Lock).size(px(10.)))
                                    .child("read only"),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .px_2()
                                .py_0p5()
                                .mr_1()
                                .rounded_full()
                                .bg(theme.muted)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(div().size(px(6.)).rounded_full().bg(theme.success))
                                .child(if local {
                                    format!("DuckDB {} · local", self.info.version)
                                } else {
                                    format!(
                                        "DuckDB {} · {} ms",
                                        self.info.version,
                                        self.info.latency.as_millis()
                                    )
                                }),
                        )
                        .child(
                            Button::new("refresh")
                                .ghost()
                                .xsmall()
                                .icon(IconName::RefreshCw)
                                .loading(self.loading_catalog)
                                .tooltip("Refresh schema  ⌘R")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.refresh_catalog(window, cx)
                                })),
                        )
                        .child(
                            Button::new("connections")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plug)
                                .tooltip("Connections  ⌘⇧O")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(OpenConnections), cx)
                                }),
                        )
                        .child(
                            Button::new("settings")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Settings)
                                .tooltip("Settings  ⌘,")
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(OpenSettings), cx)
                                }),
                        ),
                ),
        )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let local = self.client.is_local();
        let needle = self.filter.read(cx).value().to_lowercase();
        let filtering = !needle.is_empty();

        let mut rows: Vec<AnyElement> = Vec::new();
        let mut last_db: Option<&str> = None;
        let mut last_schema: Option<(&str, &str)> = None;
        let relations = self.sidebar_relations(&needle);
        // With a focused database its level is implied, so schemas become roots.
        let base = if self.focus_db.is_some() { 0 } else { 1 };

        for (ix, rel) in relations {
            if base == 1 && last_db != Some(rel.database.as_str()) {
                last_db = Some(&rel.database);
                last_schema = None;
                let key = rel.database.clone();
                let open = filtering || !self.collapsed.contains(&key);
                rows.push(
                    self.tree_header(
                        ("db", ix),
                        IconName::Database,
                        &rel.database,
                        open,
                        0,
                        key,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            if base == 1 && !filtering && self.collapsed.contains(&rel.database) {
                continue;
            }
            if last_schema != Some((&rel.database, &rel.schema)) {
                last_schema = Some((&rel.database, &rel.schema));
                let key = format!("{}.{}", rel.database, rel.schema);
                let open = filtering || !self.collapsed.contains(&key);
                rows.push(
                    self.tree_header(
                        ("schema", ix),
                        IconName::Layers,
                        &rel.schema,
                        open,
                        base,
                        key,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            if !filtering
                && self
                    .collapsed
                    .contains(&format!("{}.{}", rel.database, rel.schema))
            {
                continue;
            }
            rows.push(self.relation_row(ix, rel, base + 1, cx).into_any_element());
        }

        let empty_msg = if self.loading_catalog && self.catalog.relations.is_empty() {
            Some("Loading schema…".to_string())
        } else if let Some(e) = &self.catalog_error {
            Some(e.clone())
        } else if self.catalog.relations.is_empty() {
            Some("No tables yet".into())
        } else if rows.is_empty() {
            Some("No matches".into())
        } else {
            None
        };

        v_flex()
            .size_full()
            .bg(theme.sidebar)
            .child(
                div().px_2().pt_2().pb_2().child(
                    Input::new(&self.filter)
                        .small()
                        .prefix(
                            Icon::new(IconName::Search)
                                .xsmall()
                                .text_color(theme.muted_foreground),
                        )
                        .cleanable(true),
                ),
            )
            .child(
                v_flex()
                    .id("tree")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_1p5()
                    .pb_2()
                    .children(rows)
                    .when_some(empty_msg, |el, msg| {
                        el.child(
                            div()
                                .px_2()
                                .py_4()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(msg),
                        )
                    }),
            )
            .children(self.render_imports(cx))
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .children(
                        ADMIN
                            .iter()
                            .enumerate()
                            .filter(|(_, (label, ..))| {
                                // quack_server_list() needs the quack extension, which
                                // local databases don't load.
                                !(local && *label == "Quack servers")
                            })
                            .map(|(ix, (label, icon, sql))| {
                                let sql = *sql;
                                Button::new(("admin", ix))
                                    .ghost()
                                    .xsmall()
                                    .icon(icon.clone())
                                    .tooltip(*label)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.selected_relation = None;
                                        this.marked.clear();
                                        this.run_sql(
                                            sql.to_string(),
                                            true,
                                            ResultSource::Query,
                                            Some(label.to_string()),
                                            window,
                                            cx,
                                        );
                                    }))
                            }),
                    )
                    .child(
                        Button::new("export-tables")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Download)
                            .tooltip("Export tables…")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_tables_export(window, cx)
                            })),
                    )
                    .child(
                        Button::new("import-csv")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Upload)
                            .tooltip("Import CSV…")
                            .disabled(self.profile.read_only)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pick_import_files(window, cx)
                            })),
                    ),
            )
    }

    /// The relations the sidebar lists for a filter, in order, with their
    /// index in the catalog.
    fn sidebar_relations(&self, needle: &str) -> Vec<(usize, &Relation)> {
        self.catalog
            .relations
            .iter()
            .enumerate()
            .filter(|(_, r)| needle.is_empty() || r.name.to_lowercase().contains(needle))
            .filter(|(_, r)| self.focus_db.as_ref().is_none_or(|db| &r.database == db))
            .collect()
    }

    /// The tables highlighted in the sidebar: the marked ones, else the open one.
    pub(super) fn picked_relations(&self) -> Vec<String> {
        if self.marked.is_empty() {
            self.selected_relation.iter().cloned().collect()
        } else {
            self.marked.iter().cloned().collect()
        }
    }

    /// What a right-click on `qualified` acts on: the whole pick when it's
    /// part of it, else just that one.
    fn context_relations(&self, qualified: &str) -> Vec<String> {
        let picked = self.picked_relations();
        if picked.iter().any(|q| q == qualified) {
            picked
        } else {
            vec![qualified.to_string()]
        }
    }

    /// ⌘-click: add a table to the pick, or take it out.
    fn toggle_mark(&mut self, qualified: String, cx: &mut Context<Self>) {
        if self.marked.is_empty() {
            self.marked.extend(self.selected_relation.clone());
        }
        if !self.marked.remove(&qualified) {
            self.marked.insert(qualified.clone());
        }
        self.mark_anchor = Some(qualified);
        cx.notify();
    }

    /// ⇧-click: pick every listed table between the anchor and this one.
    fn mark_range(&mut self, qualified: String, cx: &mut Context<Self>) {
        let Some(anchor) = self.mark_anchor.clone().or_else(|| self.selected_relation.clone()) else {
            return self.toggle_mark(qualified, cx);
        };
        let needle = self.filter.read(cx).value().to_lowercase();
        let filtering = !needle.is_empty();
        // Only what's on screen: rows inside collapsed groups are skipped.
        let listed: Vec<String> = self
            .sidebar_relations(&needle)
            .into_iter()
            .filter(|(_, r)| {
                filtering
                    || !(self.collapsed.contains(&r.database)
                        || self.collapsed.contains(&format!("{}.{}", r.database, r.schema)))
            })
            .map(|(_, r)| r.qualified())
            .collect();
        let (Some(a), Some(b)) = (
            listed.iter().position(|q| *q == anchor),
            listed.iter().position(|q| *q == qualified),
        ) else {
            return self.toggle_mark(qualified, cx);
        };
        self.marked = listed[a.min(b)..=a.max(b)].iter().cloned().collect();
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn tree_header(
        &self,
        id: (&'static str, usize),
        icon: IconName,
        label: &str,
        open: bool,
        depth: usize,
        key: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        tree_row(id, depth, Some(open), icon, theme.muted_foreground, cx)
            .hover(|el| el.bg(theme.sidebar_accent))
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .font_medium()
                    .child(label.to_string()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(&key) {
                    this.collapsed.insert(key.clone());
                }
                cx.notify();
            }))
    }

    fn relation_row(
        &self,
        ix: usize,
        rel: &Relation,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let qualified = rel.qualified();
        let active = if self.marked.is_empty() {
            self.selected_relation.as_deref() == Some(qualified.as_str())
        } else {
            self.marked.contains(&qualified)
        };
        let rel_open = rel.clone();
        let rel_describe = rel.clone();
        let view = cx.entity().downgrade();
        let menu_rel = rel.clone();
        let (icon, color) = if rel.is_view {
            (IconName::Eye, theme.magenta)
        } else {
            (IconName::Table, theme.muted_foreground)
        };
        tree_row(("rel", ix), depth, None, icon, color, cx)
            .group("rel")
            .when(active, |el| {
                el.bg(theme.list_active).text_color(theme.foreground)
            })
            .when(!active, |el| el.hover(|el| el.bg(theme.sidebar_accent)))
            .child(div().flex_1().truncate().child(rel.name.clone()))
            .child(
                div()
                    .group_hover("rel", |el| el.invisible())
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .when_some(rel.estimated_rows, |el, n| el.child(compact(n))),
            )
            .child(
                div()
                    .absolute()
                    .right_1()
                    .invisible()
                    .group_hover("rel", |el| el.visible())
                    .child(
                        Button::new(("describe", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Columns3)
                            .tooltip("Structure")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.describe_relation(&rel_describe, window, cx)
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                let modifiers = event.modifiers();
                if modifiers.shift {
                    this.mark_range(rel_open.qualified(), cx);
                } else if modifiers.secondary() {
                    this.toggle_mark(rel_open.qualified(), cx);
                } else {
                    this.marked.clear();
                    this.mark_anchor = Some(rel_open.qualified());
                    this.open_relation(&rel_open, window, cx);
                }
            }))
            .context_menu(move |menu, _, cx| {
                let Some(workspace) = view.upgrade() else {
                    return menu;
                };
                let targets = workspace.read(cx).context_relations(&menu_rel.qualified());
                let tables = workspace
                    .read(cx)
                    .catalog
                    .relations
                    .iter()
                    .filter(|r| !r.is_view && targets.contains(&r.qualified()))
                    .count();
                let (open, describe, export) = (view.clone(), view.clone(), view.clone());
                let (open_rel, describe_rel) = (menu_rel.clone(), menu_rel.clone());
                let one = targets.len() == 1;
                menu.when(one, |menu| {
                    menu.item(PopupMenuItem::new("Open").on_click(move |_, window, cx| {
                        open.update(cx, |this, cx| {
                            this.marked.clear();
                            this.open_relation(&open_rel, window, cx)
                        })
                        .ok();
                    }))
                    .item(PopupMenuItem::new("Structure").on_click(move |_, window, cx| {
                        describe
                            .update(cx, |this, cx| this.describe_relation(&describe_rel, window, cx))
                            .ok();
                    }))
                    .separator()
                })
                .item(
                    PopupMenuItem::new(if tables > 1 {
                        format!("Export {tables} Tables…")
                    } else {
                        "Export Table…".to_string()
                    })
                    .icon(IconName::Download)
                    .disabled(tables == 0)
                    .on_click(move |_, window, cx| {
                        let targets = targets.clone();
                        export
                            .update(cx, |this, cx| this.open_tables_export_with(targets, window, cx))
                            .ok();
                    }),
                )
            })
    }

    /// For tables without a primary key: which column rows are updated by,
    /// with a menu to change it.
    fn render_key_picker(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let ResultSource::Table {
            rel, key, key_kind, ..
        } = &self.source
        else {
            return None;
        };
        if rel.is_view || *key_kind == Some(KeyKind::Primary) {
            return None;
        }
        if !matches!(self.run, RunState::Done { .. }) {
            return None;
        }
        let columns: Vec<String> = self
            .table
            .read(cx)
            .delegate()
            .result
            .as_ref()?
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();
        let label = match key_kind {
            Some(KeyKind::Unique) => format!("Key: {} (unique)", key.join(", ")),
            Some(_) => format!("Key: {} (not declared)", key.join(", ")),
            None => "Choose a key column…".into(),
        };
        let current = key.clone();
        let view = cx.entity().downgrade();
        Some(
            Button::new("key-picker")
                .ghost()
                .xsmall()
                .icon(IconName::Key)
                .label(label)
                .tooltip(
                    "Rows are updated by this column; each save checks it matches exactly one row",
                )
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu.item(PopupMenuItem::label("Update rows by"));
                    for col in &columns {
                        let view = view.clone();
                        let pick = col.clone();
                        menu = menu.item(
                            PopupMenuItem::new(col.clone())
                                .checked(current.len() == 1 && current[0] == *col)
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.set_key_column(pick.clone(), cx)
                                    })
                                    .ok();
                                }),
                        );
                    }
                    menu
                }),
        )
    }

    fn render_results_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let status: AnyElement = match &self.run {
            RunState::Idle => div()
                .text_color(theme.muted_foreground)
                .child("Ready")
                .into_any_element(),
            RunState::Running(started) => h_flex()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(div().size(px(6.)).rounded_full().bg(theme.warning))
                .child(format!("Running… {:.1}s", started.elapsed().as_secs_f32()))
                .into_any_element(),
            RunState::Done {
                rows,
                elapsed,
                truncated,
            } => h_flex()
                .gap_2()
                .child(div().size(px(6.)).rounded_full().bg(theme.success))
                .when_some(self.run_label.clone(), |el, label| {
                    el.child(
                        div()
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child(label),
                    )
                    .child(div().text_color(theme.muted_foreground).child("·"))
                })
                .child(div().text_color(theme.foreground).child(format!(
                    "{} {}",
                    group_digits(*rows),
                    if *rows == 1 { "row" } else { "rows" }
                )))
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(fmt_duration(*elapsed)),
                )
                .when(*truncated, |el| {
                    el.child(div().text_color(theme.warning).child("· limit reached"))
                })
                .into_any_element(),
            RunState::Cancelled => div()
                .text_color(theme.muted_foreground)
                .child("Cancelled")
                .into_any_element(),
            RunState::Failed(_) => h_flex()
                .gap_2()
                .text_color(theme.danger)
                .child(div().size(px(6.)).rounded_full().bg(theme.danger))
                .child("Error")
                .into_any_element(),
            RunState::Confirm(_) => h_flex()
                .gap_2()
                .text_color(theme.warning)
                .child(Icon::new(IconName::TriangleAlert).xsmall())
                .child("This statement changes data. Press ⌘↵ again to run, ⌘. to cancel.")
                .into_any_element(),
        };

        let running = matches!(self.run, RunState::Running(_));
        let has_result = self.table.read(cx).delegate().result.is_some();
        let (edits, editable) = {
            let d = self.table.read(cx).delegate();
            (d.edits.len(), d.editable)
        };
        let read_only_table = matches!(&self.source, ResultSource::Table { rel, .. } if !rel.is_view)
            && !editable
            && matches!(self.run, RunState::Done { .. });
        let key_picker = self.render_key_picker(cx);
        let status = h_flex()
            .gap_3()
            .overflow_hidden()
            .child(status)
            .when_some(self.notice.clone(), |el, (is_error, msg)| {
                el.child(
                    div()
                        .truncate()
                        .text_color(if is_error {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        })
                        .child(msg),
                )
            })
            .when(editable && edits == 0 && self.notice.is_none(), |el| {
                el.child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("Double-click or ↵ to edit a cell"),
                )
            })
            .when(read_only_table && self.notice.is_none(), |el| {
                let reason = match &self.source {
                    ResultSource::Table { key, .. } if !key.is_empty() => {
                        format!("Read-only: include {} in the query to edit", key.join(", "))
                    }
                    _ => "Read-only: no primary key".into(),
                };
                el.child(div().text_color(theme.muted_foreground).child(reason))
            })
            .children(key_picker);

        h_flex()
            .h(px(32.))
            .px_3()
            .gap_2()
            .justify_between()
            .border_t_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .text_xs()
            .child(status)
            .child(
                h_flex()
                    .gap_1()
                    .flex_shrink_0()
                    .when(edits > 0, |el| {
                        el.child(div().mr_1().text_color(theme.warning).child(format!(
                            "{edits} unsaved {}",
                            if edits == 1 { "change" } else { "changes" }
                        )))
                        .child(
                            Button::new("discard")
                                .xsmall()
                                .ghost()
                                .label("Discard")
                                .tooltip("Revert staged edits  Esc")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.on_discard(&DiscardChanges, window, cx)
                                })),
                        )
                        .child(
                            Button::new("save")
                                .xsmall()
                                .warning()
                                .label("Save")
                                .tooltip("Apply all edits in one transaction  ⌘S")
                                .loading(self.saving)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.on_save(&SaveChanges, window, cx)
                                })),
                        )
                    })
                    .when(running, |el| {
                        el.child(
                            Button::new("cancel")
                                .xsmall()
                                .danger()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.on_cancel(&CancelQuery, window, cx)
                                })),
                        )
                    })
                    .when(has_result && !running, |el| {
                        let view = cx.entity().downgrade();
                        el.child(
                            Button::new("export")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Download)
                                .label("Export")
                                .loading(self.exporting)
                                .dropdown_menu(move |menu, _, _| {
                                    let (csv, sql) = (view.clone(), view.clone());
                                    menu.item(PopupMenuItem::new("Export as CSV…").on_click(
                                        move |_, window, cx| {
                                            csv.update(cx, |this, cx| {
                                                this.export_results(false, window, cx)
                                            })
                                            .ok();
                                        },
                                    ))
                                    .item(PopupMenuItem::new("Export as SQL…").on_click(
                                        move |_, window, cx| {
                                            sql.update(cx, |this, cx| {
                                                this.export_results(true, window, cx)
                                            })
                                            .ok();
                                        },
                                    ))
                                }),
                        )
                        .child(
                            Button::new("copy")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Copy)
                                .tooltip("Copy results as TSV")
                                .on_click(cx.listener(|this, _, _, cx| this.copy_results(cx))),
                        )
                    })
                    .child(
                        Button::new("run")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Play)
                            .label("Run")
                            .tooltip("Run  ⌘↵")
                            .disabled(running)
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(RunQuery), cx)
                            }),
                    ),
            )
    }

    fn render_results(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        if let RunState::Failed(msg) = &self.run {
            return v_flex()
                .id("error")
                .size_full()
                .overflow_y_scroll()
                .p_4()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .font_semibold()
                        .text_color(theme.danger)
                        .child(Icon::new(IconName::CircleX).small())
                        .child("Query failed"),
                )
                .child(
                    div()
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .text_color(theme.foreground)
                        .whitespace_normal()
                        .child(msg.clone()),
                )
                .into_any_element();
        }
        let settings = &AppState::store(cx).settings;
        let (zebra, row_h) = (settings.zebra_rows, (settings.ui_font_size * 2.).round());
        div()
            .size_full()
            .child(
                DataTable::new(&self.table)
                    .stripe(zebra)
                    .bordered(false)
                    .with_size(gpui_kit::component::Size::Size(px(row_h))),
            )
            .into_any_element()
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let font_size = AppState::store(cx).settings.editor_font_size;
        // A drag that left the window (or ended elsewhere) clears the drop state.
        if !cx.has_active_drag() {
            self.dropping = None;
        }
        let view = cx.entity().downgrade();

        v_flex()
            .size_full()
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_run))
            .on_action(cx.listener(Self::on_run_all))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_new_query))
            .on_action(cx.listener(Self::on_toggle_comment))
            .on_action(cx.listener(Self::on_copy_csv))
            .on_action(cx.listener(|this, _: &ExportCsv, window, cx| {
                this.export_results(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ExportSql, window, cx| {
                this.export_results(true, window, cx)
            }))
            .on_action(cx.listener(|this, _: &ExportTables, window, cx| {
                this.open_tables_export(window, cx)
            }))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_format))
            .on_action(cx.listener(Self::on_toggle_log))
            .on_action(cx.listener(Self::on_discard))
            .on_action(cx.listener(Self::on_inspect_cell))
            .on_action(cx.listener(Self::on_edit_cell))
            .on_action(
                cx.listener(|this, _: &RefreshSchema, window, cx| this.refresh_catalog(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FocusFilter, window, cx| {
                this.filter.update(cx, |s, cx| s.focus(window, cx))
            }))
            .on_action(cx.listener(|this, _: &FocusEditor, window, cx| {
                this.editor.update(cx, |s, cx| s.focus(window, cx))
            }))
            // Files dropped from Finder. GPUI's `on_drop` only fires on a
            // hovered element, and it misses drops right after typing (hover
            // is off in keyboard mode) or over the results grid (which
            // occludes), so track the drag and catch the release ourselves.
            .on_drag_move(cx.listener(|this, ev: &DragMoveEvent<ExternalPaths>, _, cx| {
                if this.dropping.is_none() {
                    this.dropping = Some(ev.drag(cx).paths().to_vec());
                    cx.notify();
                }
            }))
            .child(
                canvas(
                    |_, _, _| {},
                    move |_, _, window, _| {
                        window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
                            if phase != DispatchPhase::Capture || !cx.has_active_drag() {
                                return;
                            }
                            let Ok(Some(paths)) = view.update(cx, |this, _| this.dropping.take())
                            else {
                                return;
                            };
                            let view = view.clone();
                            window.defer(cx, move |window, cx| {
                                view.update(cx, |this, cx| this.prompt_import(paths, window, cx))
                                    .ok();
                            });
                        });
                    },
                )
                .absolute()
                .size_0(),
            )
            .when(self.dropping.is_some(), |el| el.opacity(0.75))
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_title_bar(cx))
            .child(
                h_flex().flex_1().overflow_hidden().child(
                    gpui_kit::component::h_resizable("workspace")
                        .child(
                            resizable_panel()
                                .size(px(260.))
                                .size_range(px(180.)..px(480.))
                                .child(self.render_sidebar(cx)),
                        )
                        .child(
                            resizable_panel().child(
                                v_resizable("main")
                                    .child(
                                        resizable_panel()
                                            .size(px(240.))
                                            .size_range(px(80.)..px(2000.))
                                            .child(
                                                div()
                                                    .size_full()
                                                    .pt_1()
                                                    .text_size(px(font_size))
                                                    .child(
                                                        Editor::new(&self.editor)
                                                            .bordered(false)
                                                            .h_full(),
                                                    ),
                                            ),
                                    )
                                    .child(
                                        resizable_panel().child(
                                            v_flex()
                                                .size_full()
                                                .child(self.render_results_bar(cx))
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .overflow_hidden()
                                                        .child(self.render_results(cx)),
                                                )
                                                .child(crate::query_log::render(
                                                    &self.log,
                                                    AppState::store(cx).settings.show_query_log,
                                                    &self.log_scroll,
                                                    Box::new(cx.listener(|this, _, window, cx| {
                                                        this.on_toggle_log(
                                                            &ToggleQueryLog,
                                                            window,
                                                            cx,
                                                        )
                                                    })),
                                                    Box::new(cx.listener(|this, _, _, cx| {
                                                        this.log.clear();
                                                        cx.notify();
                                                    })),
                                                    cx,
                                                )),
                                        ),
                                    ),
                            ),
                        ),
                ),
            )
            // Dialogs and notifications only draw where a window asks for them.
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

// Tree geometry: every row is [indent][chevron slot][icon][label]. Each level
// indents by half a slot + gap, which keeps deep trees (database > schema >
// table) from eating the sidebar's width; guide lines still run through the
// parent's icon center.
const TREE_PAD: f32 = 6.;
const TREE_SLOT: f32 = 14.;
const TREE_GAP: f32 = 4.;
const TREE_INDENT: f32 = (TREE_SLOT + TREE_GAP) / 2.;

/// Sidebar row height for a font size: 24px at the default 13px.
fn row_height(font_size: f32) -> f32 {
    (font_size * 1.85).round().max(24.)
}

fn tree_row(
    id: impl Into<ElementId>,
    depth: usize,
    chevron: Option<bool>,
    icon: IconName,
    icon_color: Hsla,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    let font_size = AppState::store(cx).settings.ui_font_size;
    let slot = || {
        div()
            .w(px(TREE_SLOT))
            .h_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
    };
    // Leaf rows get one faint guide under their parent's icon, so each group
    // reads as a single continuous line (headers own that column's chevron).
    let guide = (chevron.is_none() && depth > 0).then(|| depth - 1);
    let guides = guide.into_iter().map(|level| {
        div()
            .absolute()
            .top_0()
            .bottom_0()
            // The parent's icon center: its indent, its chevron slot and gap,
            // then half the icon slot.
            .left(px(TREE_PAD
                + level as f32 * TREE_INDENT
                + TREE_SLOT
                + TREE_GAP
                + TREE_SLOT / 2.))
            .w(px(1.))
            .bg(theme.sidebar_border)
    });
    h_flex()
        .id(id)
        .relative()
        .h(px(row_height(font_size)))
        .pl(px(TREE_PAD + depth as f32 * TREE_INDENT))
        .pr_1()
        .gap(px(TREE_GAP))
        .rounded(theme.radius)
        .cursor_pointer()
        .text_size(px(font_size))
        .font_family(theme.mono_font_family.clone())
        .text_color(theme.sidebar_foreground)
        .children(guides)
        .child(slot().when_some(chevron, |el, open| {
            el.child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(12.))
                .text_color(theme.muted_foreground),
            )
        }))
        .child(slot().child(Icon::new(icon).xsmall().text_color(icon_color)))
}

fn is_destructive(sql: &str) -> bool {
    let upper = sql.to_uppercase();
    upper
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|w| matches!(w, "DROP" | "DELETE" | "TRUNCATE"))
}

fn group_digits(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}K", n as f64 / 1e3),
        1_000_000..=999_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{:.1}B", n as f64 / 1e9),
    }
    .replace(".0", "")
}

fn fmt_duration(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.;
    if ms < 1. {
        format!("{:.0} µs", ms * 1000.)
    } else if ms < 1000. {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1000.)
    }
}

/// Comment every non-blank line with `-- ` at the block's shallowest indent,
/// or uncomment if they're all commented already. Returns the new text and
/// how far the first line shifted (for keeping the cursor in place).
fn toggle_line_comments(block: &str) -> (String, isize) {
    let lines: Vec<&str> = block.split('\n').collect();
    let code = || lines.iter().filter(|l| !l.trim().is_empty());
    if code().next().is_none() {
        return (block.to_string(), 0);
    }
    let uncomment = code().all(|l| l.trim_start().starts_with("--"));
    let indent = code()
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    let mut first_delta = None;
    let out: Vec<String> = lines
        .iter()
        .map(|l| {
            let (new, delta) = if l.trim().is_empty() {
                (l.to_string(), 0)
            } else if uncomment {
                let at = l.len() - l.trim_start().len();
                let marker = if l[at..].starts_with("-- ") { 3 } else { 2 };
                (
                    format!("{}{}", &l[..at], &l[at + marker..]),
                    -(marker as isize),
                )
            } else {
                (format!("{}-- {}", &l[..indent], &l[indent..]), 3)
            };
            first_delta.get_or_insert(delta);
            new
        })
        .collect();
    (out.join("\n"), first_delta.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::{compact, group_digits, is_destructive, toggle_line_comments};

    #[test]
    fn helpers() {
        assert!(is_destructive("drop table x"));
        assert!(is_destructive("SELECT 1; DELETE FROM t"));
        assert!(!is_destructive("SELECT dropped FROM t"));
        assert_eq!(group_digits(1234567), "1,234,567");
        assert_eq!(group_digits(12), "12");
        assert_eq!(compact(1500), "1.5K");
        assert_eq!(compact(2_000_000), "2M");
    }

    #[test]
    fn comments_and_uncomments_at_shared_indent() {
        let sql = "SELECT 1\n  FROM t\n\n  WHERE x";
        let (commented, delta) = toggle_line_comments(sql);
        assert_eq!(commented, "-- SELECT 1\n--   FROM t\n\n--   WHERE x");
        assert_eq!(delta, 3);
        assert_eq!(toggle_line_comments(&commented).0, sql);
    }

    #[test]
    fn mixed_block_gets_commented() {
        let (out, _) = toggle_line_comments("-- a\nb");
        assert_eq!(out, "-- -- a\n-- b");
    }

    #[test]
    fn uncomments_without_space() {
        assert_eq!(toggle_line_comments("--a\n  --b").0, "a\n  b");
    }
}
