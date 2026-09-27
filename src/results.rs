//! Result grid: a virtualized table over the Arrow batches of a [`QueryResult`].
//!
//! Rows can be re-ordered in place (sorting) and cells edited: edits are
//! staged here, keyed by the underlying data row, until the workspace saves
//! or discards them.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::quack::QueryResult;

/// Values longer than this (or JSON-looking ones) get an expand button.
const INSPECT_LEN: usize = 40;

type ColumnCallback = Rc<dyn Fn(usize, &mut Window, &mut App)>;
type CellCallback = Rc<dyn Fn(usize, usize, &mut Window, &mut App)>;

/// A cell being edited in place.
pub struct CellEditor {
    pub row: usize,
    pub col: usize,
    pub input: Entity<InputState>,
}

pub struct ResultsDelegate {
    pub result: Option<Arc<QueryResult>>,
    columns: Vec<Column>,
    font_size: f32,
    /// The grid's cell padding, so edit highlights can cover whole cells.
    pub cell_padding: Edges<Pixels>,
    /// Display row → data row, when sorted in place.
    order: Option<Vec<usize>>,
    /// Column and direction (`true` = descending) shown in the header.
    pub sort: Option<(usize, bool)>,
    /// Whether cells can be edited (a table with a primary key).
    pub editable: bool,
    /// Staged values by (data row, column); `None` is SQL NULL.
    pub edits: BTreeMap<(usize, usize), Option<String>>,
    pub editing: Option<CellEditor>,
    /// Header clicked (sort).
    pub on_sort: Option<ColumnCallback>,
    /// Expand button clicked, with the display row.
    pub on_inspect: Option<CellCallback>,
}

impl ResultsDelegate {
    pub fn new(font_size: f32) -> Self {
        Self {
            result: None,
            columns: Vec::new(),
            font_size,
            // The grid is sized with `Size::Size(row_height)`; its padding
            // doesn't depend on the height.
            cell_padding: gpui_kit::component::Size::Size(px(0.)).table_cell_padding(),
            order: None,
            sort: None,
            editable: false,
            edits: BTreeMap::new(),
            editing: None,
            on_sort: None,
            on_inspect: None,
        }
    }

    /// Returns whether the size changed (and the columns were re-fit).
    pub fn set_font_size(&mut self, font_size: f32) -> bool {
        if font_size == self.font_size {
            return false;
        }
        self.font_size = font_size;
        self.fit_columns();
        true
    }

    /// Show a new result. Sorting, edits and editability start fresh.
    pub fn set_result(&mut self, result: Option<Arc<QueryResult>>) {
        self.result = result;
        self.order = None;
        self.sort = None;
        self.editable = false;
        self.edits.clear();
        self.editing = None;
        self.fit_columns();
    }

    fn fit_columns(&mut self) {
        let font_size = self.font_size;
        self.columns = self
            .result
            .as_ref()
            .map(|r| {
                r.columns
                    .iter()
                    .enumerate()
                    .map(|(ix, c)| {
                        let col = Column::new(SharedString::from(format!("c{ix}")), c.name.clone())
                            .width(px(estimate_width(r, ix, font_size)))
                            .resizable(true)
                            .movable(false);
                        if c.numeric { col.text_right() } else { col }
                    })
                    .collect()
            })
            .unwrap_or_default();
    }

    /// Sort in place by a column, or restore the original order with `None`.
    pub fn sort_in_place(&mut self, sort: Option<(usize, bool)>) {
        self.sort = sort;
        self.order = match (sort, &self.result) {
            (Some((col, desc)), Some(r)) => Some(r.sorted_order(col, desc)),
            _ => None,
        };
    }

    pub fn data_row(&self, display_row: usize) -> usize {
        self.order
            .as_ref()
            .and_then(|o| o.get(display_row).copied())
            .unwrap_or(display_row)
    }

    pub fn original(&self, row: usize, col: usize) -> Option<String> {
        self.result.as_ref().and_then(|r| r.cell(row, col))
    }

    /// The value shown for a data cell: the staged edit, else the original.
    pub fn value(&self, row: usize, col: usize) -> Option<String> {
        match self.edits.get(&(row, col)) {
            Some(staged) => staged.clone(),
            None => self.original(row, col),
        }
    }

    /// Stage a value, or drop the edit if it matches the original.
    pub fn stage(&mut self, row: usize, col: usize, value: Option<String>) {
        if value == self.original(row, col) {
            self.edits.remove(&(row, col));
        } else {
            self.edits.insert((row, col), value);
        }
    }
}

/// Size a column to fit its header and a sample of its values.
fn estimate_width(result: &QueryResult, col: usize, font_size: f32) -> f32 {
    // JetBrains Mono advances ~0.6em per character.
    let per_char = font_size * 0.61;
    let c = &result.columns[col];
    let header = c.name.chars().count() as f32 * font_size * 0.63
        + c.type_name.len() as f32 * (font_size - 2.) * 0.61
        + 12.
        + 16.; // sort arrow
    let widest = (0..result.rows.min(64))
        .filter_map(|row| result.cell(row, col))
        .map(|s| s.chars().count().min(60))
        .max()
        .unwrap_or(4) as f32;
    (header.max(widest * per_char) + 24.).clamp(64., font_size * 34.)
}

fn looks_like_json(s: &str) -> bool {
    let s = s.trim_start();
    s.starts_with('{') || s.starts_with('[')
}

impl TableDelegate for ResultsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.result.as_ref().map_or(0, |r| r.rows)
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let (name, ty) = self
            .result
            .as_ref()
            .and_then(|r| r.columns.get(col_ix))
            .map(|c| (c.name.clone(), c.type_name.clone()))
            .unwrap_or_default();
        let arrow = match self.sort {
            Some((c, false)) if c == col_ix => Some(IconName::ArrowUp),
            Some((c, true)) if c == col_ix => Some(IconName::ArrowDown),
            _ => None,
        };
        let on_sort = self.on_sort.clone();
        h_flex()
            .id(("th", col_ix))
            .size_full()
            .gap_1p5()
            .overflow_hidden()
            .cursor_pointer()
            .font_family(theme.mono_font_family.clone())
            .child(
                div()
                    .text_size(px(self.font_size))
                    .font_semibold()
                    .text_color(theme.foreground)
                    .flex_shrink_0()
                    .child(name),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(self.font_size - 2.))
                    .text_color(theme.muted_foreground.opacity(0.7))
                    .truncate()
                    .child(ty.to_lowercase()),
            )
            .when_some(arrow, |el, icon| {
                el.child(
                    Icon::new(icon)
                        .size(px(12.))
                        .flex_shrink_0()
                        .text_color(theme.foreground),
                )
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                if let Some(f) = &on_sort {
                    f(col_ix, window, cx);
                }
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let row = self.data_row(row_ix);
        let numeric = self
            .result
            .as_ref()
            .and_then(|r| r.columns.get(col_ix))
            .is_some_and(|c| c.numeric);
        let el = h_flex()
            .size_full()
            .overflow_hidden()
            .text_size(px(self.font_size))
            .font_family(theme.mono_font_family.clone());

        if let Some(editor) = self
            .editing
            .as_ref()
            .filter(|e| e.row == row && e.col == col_ix)
        {
            return el
                .child(
                    Input::new(&editor.input)
                        .xsmall()
                        .appearance(false)
                        .w_full(),
                )
                .into_any_element();
        }

        let staged = self.edits.contains_key(&(row, col_ix));
        let el = el
            .id(("td", row_ix * 10_000 + col_ix))
            .group("cell")
            .relative()
            .when(numeric, |el| el.justify_end());
        let content = match self.value(row, col_ix) {
            None => el
                .text_color(theme.muted_foreground.opacity(0.6))
                .italic()
                .child("NULL")
                .into_any_element(),
            Some(s) => {
                let inspectable = s.chars().count() > INSPECT_LEN || looks_like_json(&s);
                let mut s = s;
                if s.len() > 512 {
                    s.truncate(s.floor_char_boundary(512));
                    s.push('…');
                }
                // One line per cell: collapse newlines so rows keep a fixed height.
                let s = s.replace(['\n', '\r'], " ");
                let on_inspect = self.on_inspect.clone();
                el.text_color(theme.foreground)
                    .child(div().truncate().child(s))
                    .when(inspectable, |el| {
                        el.child(
                            div()
                                .id("inspect")
                                .absolute()
                                .right_0()
                                .top_0()
                                .bottom_0()
                                .flex()
                                .items_center()
                                .px_1()
                                .invisible()
                                .group_hover("cell", |el| el.visible())
                                .bg(theme.table_hover)
                                .cursor_pointer()
                                .child(
                                    Icon::new(IconName::Maximize2)
                                        .xsmall()
                                        .text_color(theme.muted_foreground),
                                )
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    if let Some(f) = &on_inspect {
                                        f(row_ix, col_ix, window, cx);
                                    }
                                }),
                        )
                    })
                    .into_any_element()
            }
        };
        if !staged {
            return content;
        }
        // Staged (unsaved) edits fill the whole cell, reaching out over its
        // padding (the cell clips at its own edges).
        let pad = self.cell_padding;
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .absolute()
                    .top(-pad.top)
                    .bottom(-pad.bottom)
                    .left(-pad.left)
                    .right(-pad.right)
                    .bg(theme.warning.opacity(0.28)),
            )
            .child(content)
            .into_any_element()
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.value(self.data_row(row_ix), col_ix)
            .unwrap_or_else(|| "NULL".into())
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        h_flex()
            .size_full()
            .justify_center()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(if self.result.is_some() {
                "Query returned no rows"
            } else {
                "Run a query with ⌘↵"
            })
    }
}
