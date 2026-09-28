mod complete;
mod connections;
mod format;
mod quack;
mod query_log;
mod results;
mod settings;
mod simple_query;
mod store;
mod theme;
mod update;
mod workspace;

use gpui_kit::component::{Root, TitleBar};
use gpui_kit::*;

use std::path::{Path, PathBuf};

use futures::StreamExt as _;
use store::{Profile, ProfileKind, Store};

gpui_kit::actions!(
    duckplus,
    [
        Quit,
        About,
        CheckForUpdates,
        OpenSettings,
        OpenConnections,
        CloseWindow,
        RunQuery,
        RunAll,
        CancelQuery,
        NewQuery,
        ToggleComment,
        CopyCsv,
        ExportCsv,
        ExportSql,
        ExportTables,
        RefreshSchema,
        FocusFilter,
        FocusEditor,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        SaveChanges,
        DiscardChanges,
        InspectCell,
        FormatSql,
        EditCell,
        ToggleQueryLog,
    ]
);

gpui_kit::assets::icon_assets!(
    AppIcons,
    [
        Database,
        Table,
        Table2,
        Eye,
        Key,
        KeyRound,
        Plug,
        PlugZap,
        Unplug,
        Zap,
        Server,
        Columns3,
        Layers,
        Package,
        Puzzle,
        Gauge,
        Activity,
        RefreshCw,
        Terminal,
        Lock,
        ShieldCheck,
        HardDrive,
        ListTree,
        Trash,
        Pencil,
        Clipboard,
        Square,
        Sparkles,
        FolderOpen,
        FolderPlus,
        Maximize2,
        Info,
        Monitor,
        Download,
        ExternalLink,
        Upload,
        FileSpreadsheet,
        X
    ]
);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = AppIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(AppIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// Process-wide state: saved connections, settings, and the singleton windows.
pub struct AppState {
    pub store: Store,
    connections_window: Option<AnyWindowHandle>,
    settings_window: Option<AnyWindowHandle>,
    /// A connection that failed to open outside the launcher (e.g. a file
    /// opened from Finder); the launcher picks it up and shows the error.
    pub launch_error: Option<(Profile, String)>,
}

impl Global for AppState {}

impl AppState {
    pub fn get(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    pub fn store(cx: &App) -> &Store {
        &cx.global::<Self>().store
    }

    /// Mutate the store; observers (open windows) are notified automatically.
    pub fn update_store<R>(cx: &mut App, f: impl FnOnce(&mut Store) -> R) -> R {
        cx.update_global::<Self, R>(|state, _| f(&mut state.store))
    }
}

/// Shared window chrome: frameless with the macOS traffic lights inset.
pub fn window_options(size: Size<Pixels>, min: Size<Pixels>, cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::centered(size, cx)),
        window_min_size: Some(min),
        app_id: Some("app.duckplus.DuckPlus".into()),
        ..TitleBar::window_options()
    }
}

/// Focus an existing window if the handle is still alive.
fn activate(handle: Option<AnyWindowHandle>, cx: &mut App) -> bool {
    let Some(handle) = handle else { return false };
    handle
        .update(cx, |_, window, _| window.activate_window())
        .is_ok()
}

pub fn open_connections(cx: &mut App) {
    if activate(AppState::get(cx).connections_window, cx) {
        return;
    }
    let options = window_options(size(px(760.), px(520.)), size(px(640.), px(440.)), cx);
    if let Ok(handle) = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| connections::ConnectionsView::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    }) {
        cx.global_mut::<AppState>().connections_window = Some(handle.into());
    }
}

pub fn close_connections(cx: &mut App) {
    if let Some(handle) = cx.global_mut::<AppState>().connections_window.take() {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
}

pub fn open_settings(cx: &mut App) {
    if activate(AppState::get(cx).settings_window, cx) {
        return;
    }
    let mut options = window_options(size(px(520.), px(480.)), size(px(520.), px(480.)), cx);
    options.is_resizable = false;
    options.is_minimizable = false;
    if let Ok(handle) = cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| settings::SettingsView::new(window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    }) {
        cx.global_mut::<AppState>().settings_window = Some(handle.into());
    }
}

pub fn open_workspace(
    profile: store::Profile,
    client: quack::QuackClient,
    info: quack::ServerInfo,
    initial_sql: Option<String>,
    cx: &mut App,
) {
    let options = window_options(size(px(1280.), px(820.)), size(px(760.), px(480.)), cx);
    let _ = cx.open_window(options, |window, cx| {
        let view =
            cx.new(|cx| workspace::Workspace::new(profile, client, info, initial_sql, window, cx));
        cx.new(|cx| Root::new(view, window, cx))
    });
}

fn set_menus(cx: &mut App) {
    cx.set_menus(vec![
        Menu {
            name: "DuckPlus".into(),
            items: vec![
                MenuItem::action("About DuckPlus", About),
                MenuItem::action("Check for Updates…", CheckForUpdates),
                MenuItem::separator(),
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::action("Quit DuckPlus", Quit),
            ],
            disabled: false,
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Connections…", OpenConnections),
                MenuItem::separator(),
                MenuItem::action("Export Tables…", ExportTables),
                MenuItem::separator(),
                MenuItem::action("Close Window", CloseWindow),
            ],
            disabled: false,
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Undo", gpui_kit::component::input::Undo, OsAction::Undo),
                MenuItem::os_action("Redo", gpui_kit::component::input::Redo, OsAction::Redo),
                MenuItem::separator(),
                MenuItem::os_action("Cut", gpui_kit::component::input::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", gpui_kit::component::input::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", gpui_kit::component::input::Paste, OsAction::Paste),
                MenuItem::os_action(
                    "Select All",
                    gpui_kit::component::input::SelectAll,
                    OsAction::SelectAll,
                ),
            ],
            disabled: false,
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Increase Font Size", ZoomIn),
                MenuItem::action("Decrease Font Size", ZoomOut),
                MenuItem::action("Reset Font Size", ZoomReset),
                MenuItem::separator(),
                MenuItem::action("Toggle Query Log", ToggleQueryLog),
            ],
            disabled: false,
        },
        Menu {
            name: "Query".into(),
            items: vec![
                MenuItem::action("New Query Window", NewQuery),
                MenuItem::separator(),
                MenuItem::action("Run", RunQuery),
                MenuItem::action("Run All", RunAll),
                MenuItem::action("Cancel", CancelQuery),
                MenuItem::separator(),
                MenuItem::action("Toggle Comment", ToggleComment),
                MenuItem::action("Format SQL", FormatSql),
                MenuItem::separator(),
                MenuItem::action("Save Changes", SaveChanges),
                MenuItem::action("Discard Changes", DiscardChanges),
                MenuItem::action("Inspect Cell", InspectCell),
                MenuItem::action("Copy Results as CSV", CopyCsv),
                MenuItem::action("Export Results as CSV…", ExportCsv),
                MenuItem::action("Export Results as SQL…", ExportSql),
                MenuItem::separator(),
                MenuItem::action("Refresh Schema", RefreshSchema),
                MenuItem::action("Filter Tables", FocusFilter),
                MenuItem::action("Focus Editor", FocusEditor),
            ],
            disabled: false,
        },
    ]);
}

fn zoom(delta: Option<f32>, cx: &mut App) {
    AppState::update_store(cx, |s| {
        s.settings.zoom(delta);
        s.save_settings();
    });
}

/// Connect to whatever a profile points at. Blocking; run it off the main thread.
pub fn connect_profile(
    profile: &Profile,
    token: &str,
) -> anyhow::Result<(quack::QuackClient, quack::ServerInfo)> {
    match profile.kind {
        ProfileKind::Quack => quack::QuackClient::connect(&profile.endpoint, token, profile.tls),
        ProfileKind::Local => quack::QuackClient::connect_local(
            &store::expand_tilde(&profile.endpoint),
            profile.read_only,
        ),
    }
}

/// Open a DuckDB file in a workspace, reusing its saved connection (name,
/// color, read-only) if there is one and saving a new one otherwise. Falls
/// back to read-only when another process holds the write lock.
pub fn open_local_file(path: PathBuf, sql: Option<String>, cx: &mut App) {
    let path = path.canonicalize().unwrap_or(path);
    let same_file = |p: &&Profile| {
        p.is_local()
            && store::expand_tilde(&p.endpoint)
                .canonicalize()
                .ok()
                .as_ref()
                == Some(&path)
    };
    let profile = AppState::store(cx)
        .profiles
        .iter()
        .find(same_file)
        .cloned()
        .unwrap_or_else(|| Profile::local(&path, false));

    cx.spawn(async move |cx| {
        let attempt = profile.clone();
        let result = cx
            .background_executor()
            .spawn(async move {
                match connect_profile(&attempt, "") {
                    Err(e) if !attempt.read_only && format!("{e:#}").contains("lock") => {
                        let read_only = Profile {
                            read_only: true,
                            ..attempt
                        };
                        connect_profile(&read_only, "").map(|c| (c, read_only))
                    }
                    other => other.map(|c| (c, attempt)),
                }
            })
            .await;
        cx.update(|cx| match result {
            Ok(((client, info), opened)) => {
                AppState::update_store(cx, |s| {
                    s.upsert(profile.clone());
                    s.touch(&profile.id);
                });
                open_workspace(opened, client, info, sql, cx);
                close_connections(cx);
            }
            Err(e) => show_launch_error(profile, format!("{e:#}"), cx),
        });
    })
    .detach();
}

/// Show a failed connection in the launcher (opening it if needed), which
/// takes the error from [`AppState::launch_error`].
fn show_launch_error(profile: Profile, message: String, cx: &mut App) {
    cx.update_global::<AppState, _>(|state, _| state.launch_error = Some((profile, message)));
    open_connections(cx);
}

/// Handle `file://` URLs from Finder / the Dock. Returns whether any were files.
fn open_urls(urls: Vec<String>, cx: &mut App) -> bool {
    let mut opened = false;
    for url in urls {
        if let Some(path) = url.strip_prefix("file://") {
            open_local_file(PathBuf::from(percent_decode(path)), None, cx);
            opened = true;
        }
    }
    opened
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a CLI argument names a database file rather than a Quack endpoint.
fn is_database_file(arg: &str) -> bool {
    let path = Path::new(arg);
    path.is_file()
        || matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("duckdb" | "ddb" | "db")
        )
}

struct CliTarget {
    endpoint: String,
    token: String,
    sql: Option<String>,
}

/// `duckplus quack:host[:port] [--token T] [-c SQL]` (or `DUCKPLUS_TOKEN=T`)
/// connects straight into a workspace without saving anything.
/// `duckplus path/to/file.duckdb [-c SQL]` opens a local database.
fn cli_target() -> Option<CliTarget> {
    let mut args = std::env::args().skip(1);
    let mut endpoint = None;
    let mut token = std::env::var("DUCKPLUS_TOKEN").ok();
    let mut sql = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--token" | "-t" => token = args.next(),
            "--command" | "-c" => sql = args.next(),
            a if a.starts_with("--token=") => token = Some(a["--token=".len()..].to_string()),
            a if !a.starts_with('-') && endpoint.is_none() => endpoint = Some(a.to_string()),
            _ => {}
        }
    }
    Some(CliTarget {
        endpoint: endpoint?,
        token: token.unwrap_or_default(),
        sql,
    })
}

fn quick_connect(
    CliTarget {
        endpoint,
        token,
        sql,
    }: CliTarget,
    cx: &mut App,
) {
    cx.spawn(async move |cx| {
        let target = endpoint.clone();
        let result = cx
            .background_executor()
            .spawn(
                async move { quack::QuackClient::connect(&target, &token, quack::TlsMode::Auto) },
            )
            .await;
        cx.update(|cx| match result {
            Ok((client, info)) => {
                let name = quack::normalize_endpoint(&endpoint)
                    .trim_start_matches("quack:")
                    .to_string();
                let profile = store::Profile::new(name, endpoint, quack::TlsMode::Auto, 0);
                open_workspace(profile, client, info, sql, cx);
            }
            Err(e) => {
                eprintln!("duckplus: {e:#}");
                open_connections(cx);
            }
        });
    })
    .detach();
}

fn main() {
    let app = gpui_kit::application().with_assets(AppAssets);
    // Files opened from Finder or dropped on the Dock arrive as file:// URLs.
    // At launch they can land before `run`'s callback, so queue them.
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<String>>();
    app.on_open_urls(move |urls| {
        let _ = open_tx.unbounded_send(urls);
    });
    // Clicking the Dock icon with no windows open brings the launcher back.
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_connections(cx);
        }
    });
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        theme::init(cx);

        let store = Store::load();
        let appearance = store.settings.appearance;
        cx.set_global(AppState {
            store,
            connections_window: None,
            settings_window: None,
            launch_error: None,
        });
        theme::apply(appearance, None, cx);
        update::init(cx);

        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-,", OpenSettings, None),
            KeyBinding::new("cmd-shift-o", OpenConnections, None),
            KeyBinding::new("cmd-n", OpenConnections, None),
            KeyBinding::new("cmd-w", CloseWindow, None),
            KeyBinding::new("cmd-=", ZoomIn, None),
            KeyBinding::new("cmd-+", ZoomIn, None),
            KeyBinding::new("cmd--", ZoomOut, None),
            KeyBinding::new("cmd-0", ZoomReset, None),
            KeyBinding::new("cmd-enter", RunQuery, Some("Workspace")),
            KeyBinding::new("cmd-.", CancelQuery, Some("Workspace")),
            // The editor binds these itself (secondary-enter, code actions) and
            // sits deeper than Workspace, so override them at its depth.
            KeyBinding::new("cmd-enter", RunQuery, Some("Workspace > Input")),
            KeyBinding::new("cmd-.", CancelQuery, Some("Workspace > Input")),
            KeyBinding::new("cmd-shift-enter", RunAll, Some("Workspace")),
            KeyBinding::new("cmd-shift-enter", RunAll, Some("Workspace > Input")),
            KeyBinding::new("cmd-t", NewQuery, Some("Workspace")),
            KeyBinding::new("cmd-/", ToggleComment, Some("Workspace")),
            KeyBinding::new("cmd-shift-c", CopyCsv, Some("Workspace")),
            KeyBinding::new("cmd-j", ToggleQueryLog, Some("Workspace")),
            KeyBinding::new("cmd-j", ToggleQueryLog, Some("Workspace > Input")),
            KeyBinding::new("cmd-l", FormatSql, Some("Workspace")),
            KeyBinding::new("cmd-l", FormatSql, Some("Workspace > Input")),
            KeyBinding::new("cmd-s", SaveChanges, Some("Workspace")),
            KeyBinding::new("cmd-s", SaveChanges, Some("Workspace > Input")),
            // Registered after the grid's own Escape so it wins; with no
            // staged edits the handler passes Escape through.
            KeyBinding::new("escape", DiscardChanges, Some("Workspace > DataTable")),
            KeyBinding::new("space", InspectCell, Some("Workspace > DataTable")),
            KeyBinding::new("enter", EditCell, Some("Workspace > DataTable")),
            KeyBinding::new("cmd-r", RefreshSchema, Some("Workspace")),
            KeyBinding::new("cmd-p", FocusFilter, Some("Workspace")),
            KeyBinding::new("cmd-e", FocusEditor, Some("Workspace")),
        ]);

        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_action(|_: &ZoomIn, cx| zoom(Some(1.), cx));
        cx.on_action(|_: &ZoomOut, cx| zoom(Some(-1.), cx));
        cx.on_action(|_: &ZoomReset, cx| zoom(None, cx));
        cx.on_action(|_: &OpenSettings, cx| open_settings(cx));
        cx.on_action(|_: &OpenConnections, cx| open_connections(cx));
        cx.on_action(|_: &About, cx| open_connections(cx));
        cx.on_action(|_: &CheckForUpdates, cx| {
            update::check_now(cx);
            open_connections(cx);
        });
        cx.on_action(|_: &CloseWindow, cx| {
            if let Some(handle) = cx.active_window() {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            }
        });

        // Forget singleton handles once their windows close, and keep the
        // launcher around when the last window goes away.
        // Forget singleton handles once their windows close. When the last
        // workspace closes, fall back to the launcher (but closing the
        // launcher itself leaves the app idle in the Dock, macOS-style).
        cx.on_window_closed(|cx, closed| {
            let alive: Vec<_> = cx.windows();
            let state = cx.global_mut::<AppState>();
            let launcher_closed = state
                .connections_window
                .is_some_and(|h| h.window_id() == closed);
            let settings_closed = state
                .settings_window
                .is_some_and(|h| h.window_id() == closed);
            for slot in [&mut state.connections_window, &mut state.settings_window] {
                if slot.is_some_and(|h| !alive.contains(&h)) {
                    *slot = None;
                }
            }
            if alive.is_empty() && !launcher_closed && !settings_closed {
                open_connections(cx);
            }
        })
        .detach();

        set_menus(cx);
        let mut opened_file = false;
        while let Ok(urls) = open_rx.try_recv() {
            opened_file |= open_urls(urls, cx);
        }
        match cli_target() {
            Some(target) if is_database_file(&target.endpoint) => {
                open_local_file(PathBuf::from(target.endpoint), target.sql, cx)
            }
            Some(target) => quick_connect(target, cx),
            None if !opened_file => open_connections(cx),
            None => {}
        }
        cx.spawn(async move |cx| {
            while let Some(urls) = open_rx.next().await {
                cx.update(|cx| open_urls(urls, cx));
            }
        })
        .detach();
        cx.activate(true);
    });
}
