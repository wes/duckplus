//! The launcher window: saved connections on the left, a quick-connect form on
//! the right. Paste an endpoint and token (or pick a DuckDB file), hit Enter —
//! you're in.

use gpui_kit::assets::IconName;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, StyledExt as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::quack::TlsMode;
use crate::store::{Folder, Profile, ProfileKind, expand_tilde, tildify};
use crate::theme::{TAG_COLORS, tag_color};
use crate::update::{self, UpdateState, Updater};
use crate::{AppState, close_connections, connect_profile, open_workspace};

const ENDPOINT_PLACEHOLDER: &str = "quack:localhost  ·  db.example.com:9494";
const FILE_PLACEHOLDER: &str = "~/data/analytics.duckdb";

enum Status {
    Idle,
    Busy(&'static str),
    Ok(String),
    Err(String),
}

pub struct ConnectionsView {
    focus: FocusHandle,
    selected: Option<String>,
    name: Entity<InputState>,
    endpoint: Entity<InputState>,
    token: Entity<InputState>,
    kind: ProfileKind,
    /// Folder the form's connection is filed under.
    folder: Option<String>,
    /// Folder whose name is being edited inline, and its input.
    renaming: Option<String>,
    folder_name: Entity<InputState>,
    plain_http: bool,
    read_only: bool,
    color: usize,
    status: Status,
    _subs: Vec<Subscription>,
}

impl ConnectionsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name = cx.new(|cx| InputState::new(window, cx).placeholder("Production analytics"));
        let endpoint = cx.new(|cx| InputState::new(window, cx).placeholder(ENDPOINT_PLACEHOLDER));
        let token = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("Token")
        });

        let mut subs = Vec::new();
        for input in [&name, &endpoint, &token] {
            subs.push(
                cx.subscribe_in(input, window, |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.connect(window, cx);
                    }
                }),
            );
        }
        let folder_name = cx.new(|cx| InputState::new(window, cx));
        subs.push(
            cx.subscribe_in(&folder_name, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.commit_rename(cx);
                }
            }),
        );
        subs.push(
            cx.observe_global_in::<AppState>(window, |this, window, cx| {
                this.take_launch_error(window, cx);
                cx.notify();
            }),
        );
        subs.push(cx.observe_global::<Updater>(|_, cx| cx.notify()));

        endpoint.update(cx, |s, cx| s.focus(window, cx));

        let mut this = Self {
            focus: cx.focus_handle(),
            selected: None,
            name,
            endpoint,
            token,
            kind: ProfileKind::Quack,
            folder: None,
            renaming: None,
            folder_name,
            plain_http: false,
            read_only: false,
            color: 0,
            status: Status::Idle,
            _subs: subs,
        };
        if let Some(first) = AppState::store(cx).sorted_profiles().first().cloned() {
            this.select(&first, window, cx);
        }
        this.take_launch_error(window, cx);
        this
    }

    /// Show a connection that failed to open elsewhere (e.g. from Finder).
    fn take_launch_error(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Check before `global_mut`: it notifies global observers (including
        // the one calling this), so taking unconditionally loops forever.
        if AppState::get(cx).launch_error.is_none() {
            return;
        }
        let Some((profile, message)) = cx.global_mut::<AppState>().launch_error.take() else {
            return;
        };
        self.select(&profile, window, cx);
        self.status = Status::Err(message);
    }

    fn set_kind(&mut self, kind: ProfileKind, window: &mut Window, cx: &mut Context<Self>) {
        if self.kind == kind {
            return;
        }
        self.kind = kind;
        self.status = Status::Idle;
        let placeholder = match kind {
            ProfileKind::Quack => ENDPOINT_PLACEHOLDER,
            ProfileKind::Local => FILE_PLACEHOLDER,
        };
        self.endpoint.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder(placeholder, window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    fn browse(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.endpoint.update(cx, |s, cx| {
                    s.set_value(tildify(&path.display().to_string()), window, cx)
                });
                if this.name.read(cx).value().trim().is_empty() {
                    let stem = Profile::local(&path, false).name;
                    this.name.update(cx, |s, cx| s.set_value(stem, window, cx));
                }
                this.status = Status::Idle;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = AppState::update_store(cx, |s| s.add_folder("New Folder"));
        self.start_rename(id, window, cx);
    }

    fn start_rename(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let name = AppState::store(cx)
            .folders
            .iter()
            .find(|f| f.id == id)
            .map(|f| f.name.clone())
            .unwrap_or_default();
        self.renaming = Some(id);
        self.folder_name.update(cx, |s, cx| {
            s.set_value(name, window, cx);
            s.focus(window, cx);
            s.select_all(window, cx);
        });
        cx.notify();
    }

    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.renaming.take() else {
            return;
        };
        let name = self.folder_name.read(cx).value().trim().to_string();
        if !name.is_empty() {
            AppState::update_store(cx, |s| s.rename_folder(&id, &name));
        }
        cx.notify();
    }

    fn move_to_folder(&mut self, profile_id: &str, folder: Option<String>, cx: &mut Context<Self>) {
        // Keep the form in step so saving it doesn't undo the move.
        if self.selected.as_deref() == Some(profile_id) {
            self.folder = folder.clone();
        }
        AppState::update_store(cx, |s| s.move_to_folder(profile_id, folder));
        cx.notify();
    }

    fn select(&mut self, p: &Profile, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(p.id.clone());
        self.folder = p.folder.clone();
        self.set_kind(p.kind, window, cx);
        self.plain_http = p.tls == TlsMode::Disabled;
        self.read_only = p.read_only;
        self.color = p.color;
        self.status = Status::Idle;
        self.name
            .update(cx, |s, cx| s.set_value(p.name.clone(), window, cx));
        let endpoint = if p.is_local() {
            tildify(&p.endpoint)
        } else {
            p.endpoint.clone()
        };
        self.endpoint
            .update(cx, |s, cx| s.set_value(endpoint, window, cx));
        self.token.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder("Stored in Keychain", window, cx);
        });
        cx.notify();
    }

    fn new_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = None;
        self.folder = None;
        self.plain_http = false;
        self.read_only = false;
        self.color = 0;
        self.status = Status::Idle;
        for input in [&self.name, &self.endpoint, &self.token] {
            input.update(cx, |s, cx| s.set_value("", window, cx));
        }
        self.token
            .update(cx, |s, cx| s.set_placeholder("Token", window, cx));
        self.endpoint.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    /// Build a profile from the form, reusing the selected profile's id.
    fn form_profile(&self, cx: &App) -> Option<Profile> {
        let mut endpoint = self.endpoint.read(cx).value().trim().to_string();
        if endpoint.is_empty() {
            return None;
        }
        // Store absolute paths so a file opened from Finder matches its profile.
        let draft = match self.kind {
            ProfileKind::Quack => Profile::new(String::new(), endpoint.clone(), TlsMode::Auto, 0),
            ProfileKind::Local => {
                let path = expand_tilde(&endpoint);
                endpoint = path.display().to_string();
                Profile::local(&path, false)
            }
        };
        let mut name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            name = match self.kind {
                ProfileKind::Quack => draft.location(),
                ProfileKind::Local => draft.name.clone(),
            };
        }
        let tls = if self.plain_http {
            TlsMode::Disabled
        } else {
            TlsMode::Auto
        };
        let existing = self.selected.as_ref().and_then(|id| {
            AppState::store(cx)
                .profiles
                .iter()
                .find(|p| &p.id == id)
                .cloned()
        });
        Some(match existing {
            Some(mut p) => {
                p.name = name;
                p.kind = self.kind;
                p.endpoint = endpoint;
                p.tls = tls;
                p.read_only = self.read_only;
                p.color = self.color;
                p.folder = self.folder.clone();
                p
            }
            None => Profile {
                name,
                endpoint,
                tls,
                read_only: self.read_only,
                color: self.color,
                folder: self.folder.clone(),
                ..draft
            },
        })
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Profile> {
        let Some(profile) = self.form_profile(cx) else {
            self.status = Status::Err(self.missing_target().into());
            cx.notify();
            return None;
        };
        let token = if profile.is_local() {
            String::new()
        } else {
            self.token.read(cx).value().to_string()
        };
        if !token.is_empty() {
            if let Err(e) = profile.set_token(&token) {
                self.status = Status::Err(format!("{e:#}"));
                cx.notify();
                return None;
            }
        }
        AppState::update_store(cx, |s| s.upsert(profile.clone()));
        self.selected = Some(profile.id.clone());
        if !token.is_empty() {
            self.token.update(cx, |s, cx| {
                s.set_value("", window, cx);
                s.set_placeholder("Stored in Keychain", window, cx);
            });
        }
        Some(profile)
    }

    fn delete(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        AppState::update_store(cx, |s| s.remove(&id));
        if self.selected.as_deref() == Some(id.as_str()) {
            self.new_connection(window, cx);
        }
        cx.notify();
    }

    fn missing_target(&self) -> &'static str {
        match self.kind {
            ProfileKind::Quack => "Enter an endpoint first",
            ProfileKind::Local => "Choose a database file first",
        }
    }

    /// Resolve the token: the field wins, otherwise the keychain.
    fn resolve_token(&self, profile: &Profile, cx: &App) -> Result<String, String> {
        if profile.is_local() {
            return Ok(String::new());
        }
        let typed = self.token.read(cx).value().to_string();
        if !typed.is_empty() {
            return Ok(typed);
        }
        if AppState::store(cx)
            .profiles
            .iter()
            .any(|p| p.id == profile.id)
        {
            return profile.token().map_err(|e| format!("{e:#}"));
        }
        Err("Enter a token".into())
    }

    fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run(false, window, cx);
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.run(true, window, cx);
    }

    fn run(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.status, Status::Busy(_)) {
            return;
        }
        let Some(profile) = self.form_profile(cx) else {
            self.status = Status::Err(self.missing_target().into());
            cx.notify();
            return;
        };
        let token = match self.resolve_token(&profile, cx) {
            Ok(t) => t,
            Err(e) => {
                self.status = Status::Err(e);
                cx.notify();
                return;
            }
        };
        self.status = Status::Busy(match (open, profile.is_local()) {
            (true, true) => "Opening…",
            (true, false) => "Connecting…",
            (false, _) => "Testing…",
        });
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { connect_profile(&profile, &token) })
                .await;
            this.update_in(cx, |this, window, cx| match result {
                Ok((client, info)) => {
                    let summary = if client.is_local() {
                        format!("DuckDB {}", info.version)
                    } else {
                        format!("DuckDB {} · {} ms", info.version, info.latency.as_millis())
                    };
                    if open {
                        let Some(saved) = this.save(window, cx) else {
                            return;
                        };
                        AppState::update_store(cx, |s| s.touch(&saved.id));
                        this.status = Status::Idle;
                        open_workspace(saved, client, info, None, cx);
                        close_connections(cx);
                    } else {
                        this.status = Status::Ok(summary);
                        cx.notify();
                    }
                }
                Err(e) => {
                    this.status = Status::Err(format!("{e:#}"));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let store = AppState::store(cx);
        let folders = store.sorted_folders();
        let profiles = store.sorted_profiles();
        let in_folder = |p: &Profile, id: Option<&str>| {
            // Connections whose folder was deleted elsewhere count as unfiled.
            let filed = p
                .folder
                .as_deref()
                .filter(|f| folders.iter().any(|folder| folder.id == *f));
            filed == id
        };

        let mut list = v_flex().gap_0p5();
        let mut row_ix = 0;
        for (fx, folder) in folders.iter().enumerate() {
            let members: Vec<&Profile> = profiles
                .iter()
                .filter(|p| in_folder(p, Some(&folder.id)))
                .collect();
            let mut block = v_flex()
                .id(("folder-block", fx))
                .gap_0p5()
                .rounded(theme.radius)
                .drag_over::<DraggedProfile>(|style, _, _, cx| style.bg(cx.theme().sidebar_accent))
                .on_drop(cx.listener({
                    let id = folder.id.clone();
                    move |this, dragged: &DraggedProfile, _, cx| {
                        cx.stop_propagation();
                        this.move_to_folder(&dragged.id, Some(id.clone()), cx);
                    }
                }))
                .child(self.folder_row(fx, folder, members.len(), cx));
            if !folder.collapsed {
                for p in members {
                    block = block.child(self.profile_row(row_ix, p, true, cx));
                    row_ix += 1;
                }
            }
            list = list.child(block);
        }
        for p in profiles.iter().filter(|p| in_folder(p, None)) {
            list = list.child(self.profile_row(row_ix, p, false, cx));
            row_ix += 1;
        }

        v_flex()
            .w(px(250.))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .px_3()
                    .pt_1()
                    .pb_2()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(theme.muted_foreground)
                            .child("CONNECTIONS"),
                    )
                    .child(
                        h_flex()
                            .child(
                                Button::new("new-folder")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::FolderPlus)
                                    .tooltip("New folder")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_folder(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("new")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .tooltip("New connection")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_connection(window, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("profiles")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .pb_2()
                    // Dropping outside any folder moves a connection to the top level.
                    .on_drop(cx.listener(|this, dragged: &DraggedProfile, _, cx| {
                        this.move_to_folder(&dragged.id, None, cx);
                    }))
                    .when(profiles.is_empty() && folders.is_empty(), |el| {
                        el.child(
                            div()
                                .px_2()
                                .py_6()
                                .text_xs()
                                .text_center()
                                .text_color(theme.muted_foreground)
                                .child("No saved connections yet"),
                        )
                    })
                    .child(list),
            )
            .children(self.render_update(cx))
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(IconName::Lock).xsmall())
                    .child("Tokens are kept in your system keychain"),
            )
    }

    /// "A new version is available" and the update's progress, above the footer.
    fn render_update(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let state = cx.global::<Updater>().state.clone();
        let current = update::current();
        let card = |icon: IconName, color: Hsla, title: String| {
            v_flex()
                .mx_2()
                .mb_2()
                .p_2p5()
                .gap_2()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.secondary)
                .text_xs()
                .child(
                    h_flex()
                        .gap_1p5()
                        .font_semibold()
                        .text_color(theme.foreground)
                        .child(Icon::new(icon).xsmall().text_color(color))
                        .child(title),
                )
        };
        let muted = |text: String| div().text_color(theme.muted_foreground).child(text);
        let notes = |version: &semver::Version| {
            let url = format!("{}/tag/v{version}", update::RELEASES);
            Button::new("update-notes")
                .ghost()
                .xsmall()
                .icon(IconName::ExternalLink)
                .label("What's new")
                .on_click(move |_, _, cx| cx.open_url(&url))
        };
        let el = match state {
            UpdateState::Idle => return None,
            UpdateState::Checking => card(
                IconName::RefreshCw,
                theme.muted_foreground,
                "Checking for updates…".into(),
            ),
            UpdateState::UpToDate => card(
                IconName::CircleCheck,
                theme.success,
                format!("DuckPlus {current} is up to date"),
            ),
            UpdateState::Available(version) => {
                let install = version.clone();
                card(
                    IconName::Download,
                    theme.primary,
                    format!("DuckPlus {version} is available"),
                )
                .child(muted(format!("You have {current}. Updating restarts DuckPlus.")))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("update-install")
                                .small()
                                .icon(IconName::Download)
                                .label("Update")
                                .on_click(move |_, _, cx| update::install(install.clone(), cx)),
                        )
                        .child(notes(&version)),
                )
            }
            UpdateState::Installing { version, .. } => {
                let progress = state_progress(cx);
                card(
                    IconName::Download,
                    theme.primary,
                    format!("Updating to {version}…"),
                )
                .child(
                    Progress::new("update-progress")
                        .xsmall()
                        .loading(progress.is_none_or(|p| p <= 0.))
                        .value(progress.unwrap_or(0.) * 100.),
                )
                .child(muted(match progress {
                    Some(p) if p >= 1. => "Verifying and installing…".into(),
                    Some(p) if p > 0. => format!("Downloading… {:.0}%", p * 100.),
                    _ => "Starting download…".into(),
                }))
            }
            UpdateState::Message(msg) => card(IconName::Info, theme.muted_foreground, "Almost done".into())
                .child(muted(msg)),
            UpdateState::Failed(msg) => card(IconName::CircleX, theme.danger, "Update problem".into())
                .child(muted(msg))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("update-retry")
                                .small()
                                .label("Try again")
                                .on_click(|_, _, cx| update::check_now(cx)),
                        )
                        .child(
                            Button::new("update-download")
                                .ghost()
                                .xsmall()
                                .icon(IconName::ExternalLink)
                                .label("Download")
                                .on_click(|_, _, cx| cx.open_url(&format!("{}/latest", update::RELEASES))),
                        ),
                ),
        };
        Some(el.into_any_element())
    }

    fn folder_row(
        &self,
        fx: usize,
        folder: &Folder,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let renaming = self.renaming.as_deref() == Some(folder.id.as_str());
        let (toggle_id, add_id, rename_id, delete_id) = (
            folder.id.clone(),
            folder.id.clone(),
            folder.id.clone(),
            folder.id.clone(),
        );
        let menu_view = cx.entity().downgrade();
        h_flex()
            .id(("folder", fx))
            .group("folder")
            .h(px(28.))
            .px_1()
            .gap_1p5()
            .rounded(theme.radius)
            .cursor_pointer()
            .text_sm()
            .text_color(theme.sidebar_foreground)
            .hover(|el| el.bg(theme.sidebar_accent))
            .child(
                Icon::new(if folder.collapsed {
                    IconName::ChevronRight
                } else {
                    IconName::ChevronDown
                })
                .size(px(12.))
                .text_color(theme.muted_foreground),
            )
            .child(
                Icon::new(IconName::Folder)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .map(|el| {
                if renaming {
                    el.child(div().flex_1().child(Input::new(&self.folder_name).xsmall()))
                } else {
                    el.child(
                        div()
                            .flex_1()
                            .truncate()
                            .font_medium()
                            .child(folder.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .group_hover("folder", |el| el.invisible())
                            .child(count.to_string()),
                    )
                }
            })
            .when(!renaming, |el| {
                el.child(
                    h_flex()
                        .absolute()
                        .right_1()
                        .invisible()
                        .group_hover("folder", |el| el.visible())
                        .child(
                            Button::new(("folder-add", fx))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .tooltip("New connection in folder")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.new_connection(window, cx);
                                    this.folder = Some(add_id.clone());
                                })),
                        )
                        .child(
                            Button::new(("folder-menu", fx))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Ellipsis)
                                .on_click(|_, _, cx| cx.stop_propagation())
                                .dropdown_menu(move |menu, _, _| {
                                    let (rename_view, delete_view) =
                                        (menu_view.clone(), menu_view.clone());
                                    let (rename_id, delete_id) =
                                        (rename_id.clone(), delete_id.clone());
                                    menu.item(PopupMenuItem::new("Rename").on_click(
                                        move |_, window, cx| {
                                            let id = rename_id.clone();
                                            rename_view
                                                .update(cx, |this, cx| {
                                                    this.start_rename(id, window, cx)
                                                })
                                                .ok();
                                        },
                                    ))
                                    .item(
                                        PopupMenuItem::new("Delete Folder").on_click(
                                            move |_, _, cx| {
                                                let id = delete_id.clone();
                                                AppState::update_store(cx, |s| {
                                                    s.delete_folder(&id)
                                                });
                                                delete_view
                                                    .update(cx, |this, cx| {
                                                        if this.folder.as_deref() == Some(&id) {
                                                            this.folder = None;
                                                        }
                                                        cx.notify()
                                                    })
                                                    .ok();
                                            },
                                        ),
                                    )
                                }),
                        ),
                )
            })
            .relative()
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                if this.renaming.is_some() {
                    return;
                }
                if ev.click_count() >= 2 {
                    this.start_rename(toggle_id.clone(), window, cx);
                } else {
                    AppState::update_store(cx, |s| s.toggle_folder(&toggle_id));
                }
            }))
    }

    fn profile_row(
        &self,
        ix: usize,
        p: &Profile,
        nested: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let active = self.selected.as_deref() == Some(p.id.as_str());
        let location = p.location();
        let kind_icon = if p.is_local() {
            IconName::HardDrive
        } else {
            IconName::Server
        };
        let id_for_delete = p.id.clone();
        let p_click = p.clone();
        let dragged = DraggedProfile {
            id: p.id.clone(),
            name: p.name.clone(),
            color: p.color,
        };
        h_flex()
            .id(("profile", ix))
            .group("profile")
            .px_2()
            .when(nested, |el| el.pl(px(26.)))
            .py_1p5()
            .gap_2p5()
            .rounded(theme.radius)
            .cursor_pointer()
            .when(active, |el| el.bg(theme.sidebar_accent))
            .hover(|el| el.bg(theme.sidebar_accent))
            .on_drag(dragged, |d, _, _, cx| cx.new(|_| d.clone()))
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .flex_shrink_0()
                    .bg(tag_color(p.color)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .text_color(theme.sidebar_foreground)
                            .truncate()
                            .child(p.name.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(kind_icon).size(px(11.)))
                            .child(div().truncate().child(location)),
                    ),
            )
            .child(
                div()
                    .invisible()
                    .group_hover("profile", |el| el.visible())
                    .child(
                        Button::new(("del", ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Trash)
                            .tooltip("Delete connection")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.delete(id_for_delete.clone(), window, cx)
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                this.select(&p_click, window, cx);
                if ev.click_count() >= 2 {
                    this.connect(window, cx);
                }
            }))
    }

    fn render_folder_picker(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let folders = AppState::store(cx).sorted_folders();
        let current = self
            .folder
            .as_ref()
            .and_then(|id| folders.iter().find(|f| &f.id == id))
            .map(|f| f.name.clone());
        let view = cx.entity().downgrade();
        Button::new("folder-picker")
            .outline()
            .small()
            .w_full()
            .icon(IconName::Folder)
            .label(current.clone().unwrap_or_else(|| "No folder".into()))
            .dropdown_menu(move |menu, _, _| {
                let pick = |id: Option<String>| {
                    let view = view.clone();
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        view.update(cx, |this, cx| {
                            this.folder = id.clone();
                            cx.notify()
                        })
                        .ok();
                    }
                };
                let none_checked = current.is_none();
                let mut menu = menu.item(
                    PopupMenuItem::new("No folder")
                        .checked(none_checked)
                        .on_click(pick(None)),
                );
                if !folders.is_empty() {
                    menu = menu.separator();
                }
                for f in &folders {
                    menu = menu.item(
                        PopupMenuItem::new(f.name.clone())
                            .checked(current.as_deref() == Some(f.name.as_str()))
                            .on_click(pick(Some(f.id.clone()))),
                    );
                }
                menu
            })
    }

    fn field(label: &'static str, input: impl IntoElement, cx: &App) -> impl IntoElement {
        v_flex()
            .gap_1p5()
            .child(
                div()
                    .text_xs()
                    .font_medium()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(input)
    }

    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let busy = matches!(self.status, Status::Busy(_));
        let local = self.kind == ProfileKind::Local;
        let folder_picker = self.render_folder_picker(cx);
        let heading = match (self.selected.is_some(), local) {
            (true, _) => "Edit connection",
            (false, false) => "Connect to Quack",
            (false, true) => "Open a DuckDB file",
        };
        let subtitle = if local {
            "Pick a database file on this Mac. Press ↵ to open."
        } else {
            "Paste a Quack endpoint and token. Press ↵ to connect."
        };

        let status = match &self.status {
            Status::Idle => div().child(""),
            Status::Busy(msg) => div().text_color(theme.muted_foreground).child(*msg),
            Status::Ok(msg) => h_flex()
                .gap_1p5()
                .text_color(theme.success)
                .child(Icon::new(IconName::CircleCheck).xsmall())
                .child(msg.clone()),
            Status::Err(msg) => h_flex()
                .gap_1p5()
                .items_start()
                .text_color(theme.danger)
                .child(Icon::new(IconName::TriangleAlert).xsmall().mt_0p5())
                .child(div().flex_1().child(msg.clone())),
        };

        v_flex()
            .flex_1()
            .h_full()
            .px_8()
            .pt_2()
            .pb_6()
            .gap_5()
            .child(
                h_flex()
                    .justify_between()
                    .items_start()
                    .child(
                        v_flex()
                            .gap_1()
                            .child(div().text_xl().font_semibold().child(heading))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        ButtonGroup::new("kind")
                            .xsmall()
                            .outline()
                            .child(
                                Button::new("quack")
                                    .icon(IconName::Server)
                                    .label("Quack")
                                    .selected(!local),
                            )
                            .child(
                                Button::new("local")
                                    .icon(IconName::HardDrive)
                                    .label("File")
                                    .selected(local),
                            )
                            .on_click(cx.listener(|this, selected: &Vec<usize>, window, cx| {
                                let kind = match selected.first() {
                                    Some(1) => ProfileKind::Local,
                                    _ => ProfileKind::Quack,
                                };
                                this.set_kind(kind, window, cx);
                            })),
                    ),
            )
            .when(!local, |el| {
                el.child(Self::field("Endpoint", Input::new(&self.endpoint), cx))
                    .child(Self::field(
                        "Token",
                        Input::new(&self.token).mask_toggle(),
                        cx,
                    ))
            })
            .when(local, |el| {
                el.child(Self::field(
                    "Database file",
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.endpoint)))
                        .child(
                            Button::new("browse")
                                .small()
                                .icon(IconName::FolderOpen)
                                .label("Browse…")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.browse(window, cx)),
                                ),
                        ),
                    cx,
                ))
            })
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .child(Self::field("Name", Input::new(&self.name), cx)),
                    )
                    .child(
                        div()
                            .w(px(170.))
                            .child(Self::field("Folder", folder_picker, cx)),
                    ),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child(h_flex().gap_2().children(TAG_COLORS.iter().enumerate().map(
                        |(ix, (_, label))| {
                            let active = self.color == ix;
                            div()
                                .id(("tag", ix))
                                .size(px(18.))
                                .rounded_full()
                                .cursor_pointer()
                                .flex()
                                .items_center()
                                .justify_center()
                                .border_2()
                                .border_color(if active {
                                    tag_color(ix)
                                } else {
                                    transparent_black()
                                })
                                .child(div().size(px(10.)).rounded_full().bg(tag_color(ix)))
                                .tooltip({
                                    let label = *label;
                                    move |window, cx| {
                                        gpui_kit::component::tooltip::Tooltip::new(label)
                                            .build(window, cx)
                                    }
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.color = ix;
                                    cx.notify();
                                }))
                        },
                    )))
                    .when(!local, |el| {
                        el.child(
                            Switch::new("plain-http")
                                .checked(self.plain_http)
                                .small()
                                .label("Plain HTTP")
                                .tooltip("Skip TLS (localhost never uses it)")
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.plain_http = *checked;
                                    cx.notify();
                                })),
                        )
                    })
                    .when(local, |el| {
                        el.child(
                            Switch::new("read-only")
                                .checked(self.read_only)
                                .small()
                                .label("Read only")
                                .tooltip("Open without taking the write lock")
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.read_only = *checked;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(div().flex_1())
            .child(div().text_xs().min_h(px(18.)).child(status))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("test")
                            .label("Test")
                            .small()
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.test(window, cx))),
                    )
                    .child(
                        Button::new("save")
                            .label("Save")
                            .small()
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if this.save(window, cx).is_some() {
                                    this.status = Status::Ok("Saved".into());
                                }
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("connect")
                            .primary()
                            .small()
                            .icon(IconName::Zap)
                            .label(if local { "Open" } else { "Connect" })
                            .loading(busy)
                            .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
                    ),
            )
    }
}

impl Focusable for ConnectionsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ConnectionsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep the download's progress bar moving.
        if state_progress(cx).is_some() {
            window.request_animation_frame();
        }
        let theme = cx.theme();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .font_semibold()
                        .text_color(theme.muted_foreground)
                        .child(crate::workspace::logo(14.))
                        .child("DuckPlus"),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_sidebar(cx))
                    .child(self.render_form(cx)),
            )
    }
}

/// Drag payload (and drag preview) for moving a connection between folders.
#[derive(Clone)]
struct DraggedProfile {
    id: String,
    name: String,
    color: usize,
}

impl Render for DraggedProfile {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .shadow_md()
            .text_sm()
            .text_color(theme.popover_foreground)
            .child(div().size(px(8.)).rounded_full().bg(tag_color(self.color)))
            .child(self.name.clone())
    }
}

fn state_progress(cx: &App) -> Option<f32> {
    cx.global::<Updater>().state.progress()
}

/// The update card in a headless connections window.
#[cfg(test)]
mod update_ui_tests {
    use std::time::Duration;

    use gpui_kit::component::Root;
    use gpui_kit::test::{TestAppContextExt, TestWindowExt};
    use gpui_kit::{AppContext as _, BorrowAppContext as _, TestAppContext, px, size};

    use super::ConnectionsView;
    use crate::AppState;
    use crate::store::Store;
    use crate::update::{UpdateState, Updater};

    #[gpui_kit::test]
    async fn shows_available_update(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            crate::theme::init(cx);
            cx.set_global(AppState {
                store: Store::default(),
                connections_window: None,
                settings_window: None,
                launch_error: None,
            });
            cx.set_global(Updater::default());
        });
        let window = cx
            .open_window(size(px(760.), px(520.)), |window, cx| {
                let view = cx.new(|cx| ConnectionsView::new(window, cx));
                Root::new(view, window, cx)
            })
            .into();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("update-install").is_none(), "nothing to show yet");
        })
        .unwrap();
        cx.update(|cx| {
            cx.update_global::<Updater, _>(|u, _| {
                u.state = UpdateState::Available(semver::Version::new(9, 9, 9))
            })
        });
        cx.wait_for(window, Duration::from_secs(1), |window, _| {
            window.try_find("update-install").is_some()
        })
        .await;
        cx.update_window(window, |_, window, _| {
            assert!(window.find("update-install").visible());
            assert!(window.find("update-notes").visible());
        })
        .unwrap();
    }
}
