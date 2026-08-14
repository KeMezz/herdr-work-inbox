//! The list view -- the default, and the shape phase 1 already proved.
//!
//! Three sections, in this order, headers verbatim:
//!
//! ```text
//! REVIEW REQUESTED     Section::Review
//! MY PULL REQUESTS     Section::Mine
//! MY JIRA ISSUES       Section::Jira
//! ```
//!
//! Row text comes from [`crate::model::Item::list_row`], which is the phase 1
//! `jq` renderer ported verbatim (ref padded to 30, fixed tag order, then
//! `(@author, MM-DD)` or `(type, priority, MM-DD)`).
//!
//! What this module adds is colour, so the row is built as a `Line` of `Span`s
//! rather than printed as one string. The column positions stay identical, and
//! `row_spans_are_list_row` at the bottom of this file asserts it: the spans
//! concatenated must equal `list_row()` character for character. That test is
//! the only thing keeping the TUI and `--dump` from silently drifting apart.
//!
//! Layout: the list on the left, a 50% preview pane on the right showing the
//! cached body -- no network, the body is already in the cache. Below ~56
//! columns the preview is dropped entirely and the list takes the full width: a
//! 20-column preview shows nothing useful and costs the list the room it needs.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::{App, Mode};
use crate::model::{Item, Kind, SECTIONS, pad_cols};
use crate::view::{
    C_DIM, C_REF, banner_line, category_colour, preview, render_rows, scroll_to, selected_style,
};

/// Below this many columns the preview pane is not drawn at all.
const MIN_SPLIT: u16 = 56;
/// Columns of gutter between the list and the preview pane.
const SPLIT_GAP: u16 = 2;

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // Too narrow for a split, and the user asked for the preview: give it the
    // whole area, exactly as kanban does. The alternative -- forcing the mode
    // back to Nav -- made `enter` look dead below 56 columns AND turned the `esc`
    // the user then pressed into a quit, because nav's esc quits.
    if area.width < MIN_SPLIT && app.mode == Mode::Preview {
        preview::draw(f, area, app);
        return;
    }

    // A gutter between the two panes. Without it the longest title in the list
    // ran straight into the preview's border and the two panes read as one
    // block of text; the divider is doing more work than the border is.
    let (left, right) = if area.width >= MIN_SPLIT {
        let cols: [Rect; 2] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .spacing(SPLIT_GAP)
                .areas(area);
        (cols[0], Some(cols[1]))
    } else {
        (area, None)
    };

    let (rows, anchor, header_anchor) = build(app, left.width as usize);
    app.list_height = left.height as usize;
    let h = left.height as usize;
    // Two passes, and the order is the priority order: the anchor (the selected
    // row, or the focused section's last row when nothing is selected) is brought
    // into view first, then the focused header, which therefore wins if the
    // viewport is too short for both. With a selection the two are the same row
    // and the second pass is a no-op.
    let o = scroll_to(app.list_offset, anchor, h, rows.len());
    app.list_offset = scroll_to(o, header_anchor, h, rows.len());
    render_rows(f, left, &rows, app.list_offset);

    if let Some(right) = right {
        preview::draw(f, right, app);
    } else {
        // Without a pane, preview mode still has to scroll something; give it
        // the list's own height so ctrl-d is not a no-op.
        app.preview_height = left.height as usize;
    }
}

/// Build every row of the list, plus the row the viewport must keep visible.
///
/// One flat vector for all three sections, exactly like phase 1's fzf list: the
/// sections scroll together, and `h`/`l` moves the *focus* between them rather
/// than switching to a different list.
///
/// Returns `(rows, anchor, header_anchor)`. `anchor` is the selected item's row
/// when there is one, and otherwise the focused section's LAST row -- its empty
/// message, which is the only thing it has to say. `header_anchor` is always the
/// focused section's header.
///
/// The fallback is load-bearing: a focused section with no rows (an empty review
/// queue, an empty Jira day, a `/` filter that missed) has nothing to select, and
/// anchoring the scroll at row 0 instead pinned the viewport to the top of the
/// list -- so `h`/`l` onto that section looked like a key that does nothing,
/// because its header was below the fold and the frame did not change.
fn build(app: &App, cols: usize) -> (Vec<Line<'static>>, usize, usize) {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut selected = None;
    let mut focused_header = 0usize;
    let mut focused_last = 0usize;

    for section in &SECTIONS {
        let focused = *section == app.section;
        let items = app.section_items(section);
        if focused {
            focused_header = rows.len();
        }
        rows.push(crate::view::header_line(
            section.header(),
            items.len(),
            cols,
            focused,
        ));

        if let Some(b) = app.section_banner(section) {
            rows.push(banner_line(&b, cols));
        }

        if items.is_empty() {
            let msg = if app.query.is_empty() {
                section.empty_message().to_string()
            } else {
                format!("no match for \"{}\" here", app.query)
            };
            rows.push(Line::styled(
                format!("     {msg}"),
                Style::default().fg(C_DIM),
            ));
        }

        for (i, it) in items.iter().enumerate() {
            let is_sel = focused && i == app.cursor;
            if is_sel {
                selected = Some(rows.len());
            }
            rows.push(row_line(it, cols, is_sel));
        }
        if focused {
            focused_last = rows.len().saturating_sub(1);
        }
        rows.push(Line::raw(""));
    }
    (rows, selected.unwrap_or(focused_last), focused_header)
}

/// One row as a styled `Line`, padded to the pane width so the selection
/// highlight covers it end to end.
fn row_line(it: &Item, cols: usize, selected: bool) -> Line<'static> {
    let spans: Vec<Span<'static>> = row_spans(it)
        .into_iter()
        .map(|(text, style)| Span::styled(text, style))
        .collect();
    let mut line = Line::from(spans);
    let w = line.width();
    if w < cols {
        line.push_span(Span::raw(" ".repeat(cols - w)));
    }
    if selected {
        line = line.style(selected_style());
    }
    line
}

/// The row broken into (text, style) pairs.
///
/// **This must concatenate to [`Item::list_row`] exactly.** Colour is the only
/// difference between the two, and the test at the bottom of the file enforces
/// it against the real cache's shapes.
fn row_spans(it: &Item) -> Vec<(String, Style)> {
    let dim = Style::default().fg(C_DIM);
    let refs = Style::default().fg(C_REF).add_modifier(Modifier::BOLD);
    let plain = Style::default();
    let mut out: Vec<(String, Style)> = Vec::new();

    // The two status slots lead every row, in both kinds, so the column can be
    // scanned straight down. They are drawn unstyled: an emoji carries its own
    // colour and an fg on top of it either does nothing or fights the glyph.
    let (g1, g2) = it.status_glyphs();
    out.push((format!("{g1} {g2}  "), plain));

    match it.kind {
        Kind::Jira => {
            let key = if it.r#ref.is_empty() { &it.key } else { &it.r#ref };
            out.push((pad_cols(key, 30), refs));
            out.push((" ".to_string(), plain));
            out.push((
                it.status_badge(),
                Style::default()
                    .fg(category_colour(it.status_category.as_ref()))
                    .add_modifier(Modifier::BOLD),
            ));
            out.push((" ".to_string(), plain));
            out.push((it.title.clone(), plain));
            out.push((
                format!(
                    "  ({}, {}, {})",
                    or(&it.r#type, "?"),
                    or(&it.priority, "-"),
                    it.day()
                ),
                dim,
            ));
        }
        _ => {
            out.push((pad_cols(&it.r#ref, 30), refs));
            out.push((" ".to_string(), plain));
            out.push((it.title.clone(), plain));
            out.push((
                format!("  (@{}, {})", or(&it.author, "ghost"), it.day()),
                dim,
            ));
        }
    }
    out
}

fn or<'a>(s: &'a str, fallback: &'a str) -> &'a str {
    if s.is_empty() { fallback } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Cache;

    fn items() -> Vec<Item> {
        let c: Cache = serde_json::from_str(
            r#"{"version":1,"fetched_unix":1,"items":[
              {"kind":"pr","section":"review","ref":"acme/api#12","title":"fix the thing",
               "author":"someone","updated":"2026-08-11T04:12:33Z"},
              {"kind":"pr","section":"mine","ref":"acme/api#13","title":"日本語のタイトルです",
               "author":"me","updated":"2026-08-10T04:12:33Z","draft":true,
               "review_decision":"CHANGES_REQUESTED","checks":"FAILURE"},
              {"kind":"pr","section":"mine","ref":"acme/api#14","title":"approved and pending",
               "author":"me","updated":"2026-08-09T04:12:33Z","review_decision":"APPROVED",
               "checks":"PENDING"},
              {"kind":"pr","section":"mine","ref":"acme/api#15","title":"no author no date"},
              {"kind":"jira","section":"jira","ref":"ACME-123","key":"ACME-123",
               "title":"進行中のタスク","status":"進行中","status_category":"In Progress",
               "type":"Task","priority":"Medium","updated":"2026-08-11T04:12:33Z"},
              {"kind":"jira","section":"jira","key":"ACME-1","title":"no ref, no fields"}
            ]}"#,
        )
        .unwrap();
        c.items
    }

    /// The whole point of building the row out of spans instead of printing
    /// `list_row()`: it has to stay the same row. `--dump` prints `list_row`,
    /// the TUI draws these spans, and this is what keeps them honest.
    #[test]
    fn row_spans_are_list_row() {
        for it in items() {
            let joined: String = row_spans(&it).into_iter().map(|(t, _)| t).collect();
            assert_eq!(joined, it.list_row(), "kind={:?} ref={:?}", it.kind, it.r#ref);
        }
    }

    #[test]
    fn a_row_is_padded_to_the_pane_width_for_the_highlight() {
        let it = &items()[0];
        let line = row_line(it, 200, true);
        assert_eq!(line.width(), 200);
        // and a pane narrower than the row is not padded (the buffer clips it)
        let line = row_line(it, 10, false);
        assert!(line.width() > 10);
    }

    /// Wide titles: the row's own width must be measured in display columns, or
    /// the padding under-fills and the highlight stops halfway.
    #[test]
    fn wide_titles_are_measured_in_columns() {
        let it = items().into_iter().find(|i| i.title.contains('日')).unwrap();
        let line = row_line(&it, 200, true);
        assert_eq!(line.width(), 200);
    }
}
