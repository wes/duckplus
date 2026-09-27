//! Preferences window. Every change applies instantly to all windows.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonGroup};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, StyledExt as _, TitleBar, h_flex, v_flex,
};
use gpui_kit::*;

use crate::AppState;
use crate::store::{Appearance, EDITOR_FONT_SIZES, Settings, UI_FONT_SIZES};

pub struct SettingsView {
    row_limit: Entity<InputState>,
    preview_limit: Entity<InputState>,
    font_size: Entity<InputState>,
    ui_font_size: Entity<InputState>,
    _subs: Vec<Subscription>,
}

fn update_settings(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    AppState::update_store(cx, |s| {
        f(&mut s.settings);
        s.save_settings();
    });
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = AppState::store(cx).settings.clone();
        let num = |v: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).default_value(v))
        };
        let row_limit = num(s.row_limit.to_string(), window, cx);
        let preview_limit = num(s.preview_limit.to_string(), window, cx);
        let font_size = num(format!("{}", s.editor_font_size), window, cx);
        let ui_font_size = num(format!("{}", s.ui_font_size), window, cx);

        let subs = vec![
            cx.subscribe_in(&row_limit, window, |_, input, ev: &InputEvent, _, cx| {
                if let (InputEvent::Change, Ok(v)) = (ev, input.read(cx).value().parse::<usize>()) {
                    update_settings(cx, |s| s.row_limit = v.clamp(1, 5_000_000));
                }
            }),
            cx.subscribe_in(
                &preview_limit,
                window,
                |_, input, ev: &InputEvent, _, cx| {
                    if let (InputEvent::Change, Ok(v)) =
                        (ev, input.read(cx).value().parse::<usize>())
                    {
                        update_settings(cx, |s| s.preview_limit = v.clamp(1, 1_000_000));
                    }
                },
            ),
            cx.subscribe_in(&font_size, window, |_, input, ev: &InputEvent, _, cx| {
                if let (InputEvent::Change, Ok(v)) = (ev, input.read(cx).value().parse::<f32>()) {
                    let (min, max) = EDITOR_FONT_SIZES;
                    update_settings(cx, |s| s.editor_font_size = v.clamp(min, max));
                }
            }),
            cx.subscribe_in(&ui_font_size, window, |_, input, ev: &InputEvent, _, cx| {
                if let (InputEvent::Change, Ok(v)) = (ev, input.read(cx).value().parse::<f32>()) {
                    let (min, max) = UI_FONT_SIZES;
                    update_settings(cx, |s| s.ui_font_size = v.clamp(min, max));
                }
            }),
            // ⌘+ / ⌘− change the sizes from anywhere; mirror them here unless
            // the field is being edited.
            cx.observe_global_in::<AppState>(window, |this, window, cx| {
                let s = AppState::store(cx).settings.clone();
                for (input, value) in [
                    (&this.font_size, s.editor_font_size),
                    (&this.ui_font_size, s.ui_font_size),
                ] {
                    let shown = input.read(cx).value().parse::<f32>().ok();
                    if shown != Some(value) && !input.focus_handle(cx).is_focused(window) {
                        input.update(cx, |i, cx| i.set_value(format!("{value}"), window, cx));
                    }
                }
                cx.notify()
            }),
        ];

        Self {
            row_limit,
            preview_limit,
            font_size,
            ui_font_size,
            _subs: subs,
        }
    }

    fn section(title: &'static str, cx: &App) -> Div {
        v_flex().gap_3().child(
            div()
                .text_xs()
                .font_semibold()
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
    }

    fn row(
        label: &'static str,
        hint: &'static str,
        control: impl IntoElement,
        cx: &App,
    ) -> impl IntoElement {
        h_flex()
            .justify_between()
            .gap_4()
            .child(
                v_flex().child(div().text_sm().child(label)).child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(hint),
                ),
            )
            .child(control)
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let s = AppState::store(cx).settings.clone();

        let appearance = ButtonGroup::new("appearance")
            .small()
            .outline()
            .child(
                Button::new("system")
                    .icon(IconName::Monitor)
                    .label("System")
                    .selected(s.appearance == Appearance::System),
            )
            .child(
                Button::new("light")
                    .icon(IconName::Sun)
                    .label("Light")
                    .selected(s.appearance == Appearance::Light),
            )
            .child(
                Button::new("dark")
                    .icon(IconName::Moon)
                    .label("Dark")
                    .selected(s.appearance == Appearance::Dark),
            )
            .on_click(|selected: &Vec<usize>, window, cx| {
                let appearance = match selected.first() {
                    Some(1) => Appearance::Light,
                    Some(2) => Appearance::Dark,
                    _ => Appearance::System,
                };
                update_settings(cx, |s| s.appearance = appearance);
                crate::theme::apply(appearance, Some(window), cx);
                cx.refresh_windows();
            });

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
                        .child(Icon::new(IconName::Settings).xsmall())
                        .child("Settings"),
                ),
            )
            .child(
                v_flex()
                    .id("settings")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_8()
                    .py_5()
                    .gap_7()
                    .child(Self::section("APPEARANCE", cx).child(Self::row(
                        "Theme",
                        "Follow macOS or pick one",
                        appearance,
                        cx,
                    )))
                    .child(
                        Self::section("QUERIES", cx)
                            .child(Self::row(
                                "Row limit",
                                "Max rows fetched per query",
                                div().w(px(110.)).child(Input::new(&self.row_limit).small()),
                                cx,
                            ))
                            .child(Self::row(
                                "Table preview",
                                "Rows loaded when opening a table",
                                div()
                                    .w(px(110.))
                                    .child(Input::new(&self.preview_limit).small()),
                                cx,
                            ))
                            .child(Self::row(
                                "Confirm destructive queries",
                                "DROP, DELETE and TRUNCATE need a second ⌘↵",
                                Switch::new("confirm")
                                    .checked(s.confirm_destructive)
                                    .on_click(|v: &bool, _, cx| {
                                        let v = *v;
                                        update_settings(cx, |s| s.confirm_destructive = v)
                                    }),
                                cx,
                            )),
                    )
                    .child(
                        Self::section("EDITOR & GRID", cx)
                            .child(Self::row(
                                "Editor font size",
                                "Points · ⌘+ / ⌘− adjust both sizes",
                                div().w(px(110.)).child(Input::new(&self.font_size).small()),
                                cx,
                            ))
                            .child(Self::row(
                                "Interface font size",
                                "Schema tree and results grid",
                                div()
                                    .w(px(110.))
                                    .child(Input::new(&self.ui_font_size).small()),
                                cx,
                            ))
                            .child(Self::row(
                                "Striped rows",
                                "Alternate row shading in results",
                                Switch::new("zebra").checked(s.zebra_rows).on_click(
                                    |v: &bool, _, cx| {
                                        let v = *v;
                                        update_settings(cx, |s| s.zebra_rows = v)
                                    },
                                ),
                                cx,
                            )),
                    ),
            )
    }
}
