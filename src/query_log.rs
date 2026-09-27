//! Query log: every query a workspace runs (editor, sidebar, admin views,
//! sorting, saves), shown under the results — a one-line summary bar when
//! collapsed, a live console when expanded (⌘J).

use std::collections::VecDeque;
use std::time::Duration;

use chrono::{DateTime, Local};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// Oldest entries are dropped past this.
const MAX_ENTRIES: usize = 500;
const EXPANDED_HEIGHT: f32 = 220.;

#[derive(Clone)]
pub enum LogStatus {
    Running,
    Rows {
        rows: usize,
        truncated: bool,
    },
    /// A statement without a result grid (e.g. a save).
    Ok,
    Failed(String),
    Cancelled,
}

#[derive(Clone)]
pub struct LogEntry {
    pub id: u64,
    pub at: DateTime<Local>,
    /// Where it came from: "Editor", a table name, an admin view, "Save".
    pub origin: String,
    pub sql: String,
    pub elapsed: Option<Duration>,
    pub status: LogStatus,
}

#[derive(Default)]
pub struct QueryLog {
    entries: VecDeque<LogEntry>,
    next_id: u64,
}

impl QueryLog {
    /// Record a query as it starts; returns its id for [`Self::finish`].
    pub fn start(&mut self, origin: impl Into<String>, sql: &str) -> u64 {
        self.next_id += 1;
        if self.entries.len() == MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(LogEntry {
            id: self.next_id,
            at: Local::now(),
            origin: origin.into(),
            sql: sql.trim().to_string(),
            elapsed: None,
            status: LogStatus::Running,
        });
        self.next_id
    }

    pub fn finish(&mut self, id: u64, elapsed: Duration, status: LogStatus) {
        if let Some(e) = self.entries.iter_mut().rev().find(|e| e.id == id) {
            e.elapsed = Some(elapsed);
            e.status = status;
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn fmt_elapsed(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.;
    if ms < 1000. {
        format!("{ms:.0} ms")
    } else {
        format!("{:.2} s", ms / 1000.)
    }
}

fn status_color(status: &LogStatus, cx: &App) -> Hsla {
    let theme = cx.theme();
    match status {
        LogStatus::Running => theme.warning,
        LogStatus::Rows { .. } | LogStatus::Ok => theme.success,
        LogStatus::Failed(_) => theme.danger,
        LogStatus::Cancelled => theme.muted_foreground,
    }
}

fn outcome(e: &LogEntry) -> String {
    let rows = match &e.status {
        LogStatus::Running => return "running…".into(),
        LogStatus::Rows { rows, truncated } => {
            format!(
                "{rows} {}{}",
                if *rows == 1 { "row" } else { "rows" },
                if *truncated { "+" } else { "" }
            )
        }
        LogStatus::Ok => "ok".into(),
        LogStatus::Failed(_) => "error".into(),
        LogStatus::Cancelled => "cancelled".into(),
    };
    match e.elapsed {
        Some(d) => format!("{rows} · {}", fmt_elapsed(d)),
        None => rows,
    }
}

type Handler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// The pane: header bar (always) plus, when expanded, the entries.
pub fn render(
    log: &QueryLog,
    expanded: bool,
    scroll: &ScrollHandle,
    on_toggle: Handler,
    on_clear: Handler,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let mono = theme.mono_font_family.clone();
    let last = log.entries.back();

    let header = h_flex()
        .id("query-log-bar")
        .h(px(26.))
        .flex_shrink_0()
        .px_3()
        .gap_2()
        .border_t_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .text_xs()
        .text_color(theme.muted_foreground)
        .cursor_pointer()
        .hover(|el| el.bg(theme.sidebar_accent))
        .on_click(on_toggle)
        .child(
            Icon::new(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronUp
            })
            .size(px(12.)),
        )
        .child(div().font_medium().child("Query log"))
        .child(
            div()
                .px_1p5()
                .rounded_full()
                .bg(theme.muted)
                .child(log.entries.len().to_string()),
        )
        // Collapsed: just the latest query, one line.
        .when_some(last.filter(|_| !expanded), |el, e| {
            el.child(
                div()
                    .size(px(6.))
                    .rounded_full()
                    .flex_shrink_0()
                    .bg(status_color(&e.status, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .font_family(mono.clone())
                    .text_color(theme.foreground.opacity(0.75))
                    .child(one_line(&e.sql)),
            )
            .child(div().flex_shrink_0().child(outcome(e)))
        })
        .when(expanded || last.is_none(), |el| el.child(div().flex_1()))
        .when(expanded && !log.entries.is_empty(), |el| {
            el.child(
                Button::new("clear-log")
                    .ghost()
                    .xsmall()
                    .label("Clear")
                    .on_click(move |ev, window, cx| {
                        cx.stop_propagation();
                        on_clear(ev, window, cx)
                    }),
            )
        })
        .child(div().flex_shrink_0().opacity(0.6).child("⌘J"));

    v_flex().flex_shrink_0().child(header).when(expanded, |el| {
        el.child(
            v_flex()
                .id("query-log")
                .h(px(EXPANDED_HEIGHT))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .bg(theme.background)
                .border_t_1()
                .border_color(theme.border)
                .py_1()
                .font_family(mono.clone())
                .text_xs()
                .when(log.entries.is_empty(), |el| {
                    el.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_color(theme.muted_foreground)
                            .child("Queries you run show up here"),
                    )
                })
                .children(log.entries.iter().map(|e| render_entry(e, cx))),
        )
    })
}

fn render_entry(e: &LogEntry, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let sql = e.sql.clone();
    v_flex()
        .id(("log", e.id))
        .group("log-entry")
        .px_3()
        .py_1()
        .gap_0p5()
        .hover(|el| el.bg(theme.table_hover))
        .child(
            h_flex()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.muted_foreground)
                        .child(e.at.format("%H:%M:%S").to_string()),
                )
                .child(
                    div()
                        .mt(px(5.))
                        .size(px(6.))
                        .rounded_full()
                        .flex_shrink_0()
                        .bg(status_color(&e.status, cx)),
                )
                .child(
                    div()
                        .w(px(96.))
                        .flex_shrink_0()
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(e.origin.clone()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.foreground)
                        .whitespace_normal()
                        .line_clamp(3)
                        .child(one_line(&e.sql)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.muted_foreground)
                        .child(outcome(e)),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .invisible()
                        .group_hover("log-entry", |el| el.visible())
                        .child(
                            Button::new(("copy-log", e.id))
                                .ghost()
                                .xsmall()
                                .icon(IconName::Copy)
                                .tooltip("Copy SQL")
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(sql.clone()))
                                }),
                        ),
                ),
        )
        .when_some(
            match &e.status {
                LogStatus::Failed(msg) => Some(msg.clone()),
                _ => None,
            },
            |el, msg| {
                el.child(
                    div()
                        .pl(px(200.))
                        .text_color(theme.danger)
                        .whitespace_normal()
                        .line_clamp(2)
                        .child(msg),
                )
            },
        )
}

#[cfg(test)]
mod tests {
    use super::{LogStatus, MAX_ENTRIES, QueryLog, outcome};
    use std::time::Duration;

    #[test]
    fn records_finishes_and_caps() {
        let mut log = QueryLog::default();
        let id = log.start("Editor", "  select 1  ");
        assert_eq!(outcome(log.entries.back().unwrap()), "running…");
        log.finish(
            id,
            Duration::from_millis(12),
            LogStatus::Rows {
                rows: 1,
                truncated: false,
            },
        );
        let e = log.entries.back().unwrap();
        assert_eq!(e.sql, "select 1");
        assert_eq!(outcome(e), "1 row · 12 ms");

        for i in 0..MAX_ENTRIES + 5 {
            log.start("Editor", &format!("select {i}"));
        }
        assert_eq!(log.entries.len(), MAX_ENTRIES);
        assert_eq!(
            log.entries.back().unwrap().sql,
            format!("select {}", MAX_ENTRIES + 4)
        );
    }
}
