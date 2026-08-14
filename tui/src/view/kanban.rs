//! The kanban view.
//!
//! `v` toggles into it, `Tab` cycles the three boards. The columns are FIXED --
//! [`crate::model::Board::columns`] and [`crate::model::Board::column_of`]
//! already implement the whole spec, including the precedence rules, and they
//! are unit-tested. The buckets come from `App::board_columns()` (the same
//! derivation, with the `/` filter applied); nothing here re-derives them.
//!
//! ```text
//! Board 1  REVIEW REQUESTED   NEEDS REVIEW | CHANGES REQUESTED | APPROVED
//! Board 2  MY PULL REQUESTS   DRAFT | IN REVIEW | APPROVED | CI FAILED
//! Board 3  MY JIRA ISSUES     TO DO | IN PROGRESS | DONE
//! ```
//!
//! Column headers are English and uppercase on every board -- including the Jira
//! one, whose underlying `status` strings are Japanese. The card still shows the
//! verbatim `status`.
//!
//! # Cards
//!
//! Three lines: the ref, the truncated title, and the tags (or, for Jira, the
//! verbatim status). Truncation is by **display width** -- a 20-character
//! Japanese title is 40 columns and would run straight through the next column's
//! border.
//!
//! # The preview
//!
//! Unlike the list, kanban does not carry a permanent 50% pane: four columns in
//! half of a popup is ten columns each, which is not a board. enter/space swaps
//! the board out for the preview (same `Mode::Preview`, same keys, esc back),
//! which is the same trade the contract makes for the list -- full width for
//! whichever surface has focus.
//!
//! # Small terminals
//!
//! Columns get `Fill(1)` each, so a board narrower than its columns shrinks them
//! evenly rather than overflowing; at zero width a column draws nothing. Nothing
//! here subtracts without saturating.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::app::{App, Mode};
use crate::model::{Item, Kind};
use crate::view::{
    C_DIM, C_REF, banner_line, category_colour, fit, header_line, preview, render_rows, scroll_to,
    selected_style,
};

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    // Board header (+ banner) on top, columns underneath.
    let banner = app.section_banner(&app.board.section());
    let rows: [Rect; 3] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if banner.is_some() { 1 } else { 0 }),
        Constraint::Min(0),
    ])
    .areas(area);

    let count = app.section_items(&app.board.section()).len();
    f.render_widget(
        Paragraph::new(header_line(
            app.board.header(),
            count,
            rows[0].width as usize,
            true,
        )),
        rows[0],
    );
    if let Some(b) = banner.as_ref().filter(|_| rows[1].height > 0) {
        f.render_widget(Paragraph::new(banner_line(b, rows[1].width as usize)), rows[1]);
    }

    // Preview mode takes the board's area outright.
    if app.mode == Mode::Preview {
        preview::draw(f, rows[2], app);
        return;
    }
    app.preview_height = rows[2].height as usize;

    // A column of gutter between the boxes, so two adjacent borders do not read
    // as one double rule. Dropped when the board is too narrow to spare it.
    let n = app.board.columns().len().max(1);
    let gap = if rows[2].width as usize >= n * 18 { 1 } else { 0 };
    let areas = Layout::horizontal(vec![Constraint::Fill(1); n])
        .spacing(gap)
        .split(rows[2]);

    // Every column's cards are rendered to owned `Line`s in one pass, which ends
    // the borrow of `app` that `board_columns()` holds. Drawing straight out of
    // that borrow would mean no `&mut app` for the scroll bookkeeping below.
    let mut built: Vec<Built> = Vec::new();
    {
        let cols = app.board_columns();
        for (i, (col, items)) in cols.iter().enumerate() {
            let area = areas.get(i).copied().unwrap_or(Rect::ZERO);
            // Must match the inner margin applied when the box is drawn below,
            // or the cards are built for a width the column does not have.
            let inset = if area.width >= 24 { 4 } else { 2 };
            let width = area.width.saturating_sub(inset) as usize;
            let focused = i == app.column;
            let mut lines: Vec<Line<'static>> = Vec::new();
            let mut sel_row = 0usize;
            for (j, it) in items.iter().enumerate() {
                // A blank row between cards. Three unbroken lines per card ran
                // into the next card's ref, so a column read as one paragraph
                // instead of a stack of things.
                if j > 0 {
                    lines.push(Line::raw(""));
                }
                let is_sel = focused && j == app.cursor;
                if is_sel {
                    sel_row = lines.len();
                }
                lines.extend(card(it, width, is_sel));
            }
            if items.is_empty() {
                lines.push(Line::styled(fit("—", width), Style::default().fg(C_DIM)));
            }
            built.push(Built {
                title: format!(" {} ({}) ", col.header(), items.len()),
                area,
                focused,
                lines,
                sel_row,
            });
        }
    }

    for b in &built {
        // 2 when there is room, so the card text is not glued to the border;
        // 1 on a narrow column, where a second blank column costs more than it
        // buys. See the same reasoning in preview.rs.
        let pad = if b.area.width >= 24 { 2 } else { 1 };
        let inner = b.area.inner(Margin {
            horizontal: pad,
            vertical: 1,
        });
        if b.area.width == 0 || b.area.height == 0 {
            continue;
        }
        f.render_widget(
            Block::new()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(fit(&b.title, b.area.width.saturating_sub(2) as usize))
                .border_style(if b.focused {
                    Style::default().fg(C_REF).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(C_DIM)
                }),
            b.area,
        );
        if inner.width == 0 || inner.height == 0 {
            continue;
        }
        // Each column scrolls independently, but only the focused one has a
        // cursor to follow; the others pin to the top.
        let offset = if b.focused {
            let o = scroll_to(app.list_offset, b.sel_row, inner.height as usize, b.lines.len());
            app.list_offset = o;
            app.list_height = inner.height as usize;
            o
        } else {
            0
        };
        render_rows(f, inner, &b.lines, offset);
    }
}

/// One column, already rendered to owned lines.
struct Built {
    title: String,
    area: Rect,
    focused: bool,
    lines: Vec<Line<'static>>,
    sel_row: usize,
}

/// One card: ref, truncated title, tags. Three lines, so a card is visually a
/// block even without a border of its own (borders per card would eat two of the
/// ~20 columns a board column gets).
fn card(it: &Item, cols: usize, selected: bool) -> Vec<Line<'static>> {
    let sel = selected_style();
    let mut out = Vec::new();

    // The status slots lead the card the same way they lead a list row, so the
    // two views teach one vocabulary rather than two. They also make the old `▸`
    // selection marker redundant -- the whole card is already reversed when it is
    // the cursor -- which buys back the column the glyphs cost.
    let (g1, g2) = it.status_glyphs();
    let r#ref = if it.r#ref.is_empty() { &it.key } else { &it.r#ref };
    let ref_style = Style::default().fg(C_REF).add_modifier(Modifier::BOLD);
    // The glyph prefix is a fixed 5 columns and cannot be truncated -- half an
    // emoji is not drawable -- so a column too narrow to hold it plus something
    // of the ref drops it entirely rather than letting the head line overrun the
    // column. That path also covers `cols == 0`, where every line must come out
    // zero-width instead of 5 wide.
    let mut head = if cols >= 8 {
        Line::from(vec![
            ratatui::text::Span::raw(format!("{g1}{g2} ")),
            ratatui::text::Span::styled(pad(r#ref, cols - 5), ref_style),
        ])
    } else {
        Line::styled(pad(r#ref, cols), ref_style)
    };
    let mut title = Line::styled(
        pad(&format!("  {}", fit(&it.title, cols.saturating_sub(2))), cols),
        Style::default(),
    );

    // The third line carries what the column does NOT already say. On a PR board
    // the column IS the review state, so repeating `[approved]` there was noise;
    // who opened it and when it last moved is the thing you cannot see anywhere
    // else on the card.
    let (text, colour) = match it.kind {
        Kind::Jira => (
            format!("  {}", it.status.trim()),
            category_colour(it.status_category.as_ref()),
        ),
        _ => (
            format!(
                "  @{} · {}",
                if it.author.is_empty() { "ghost" } else { &it.author },
                it.day()
            ),
            C_DIM,
        ),
    };
    let mut tail = Line::styled(pad(&text, cols), Style::default().fg(colour));

    if selected {
        head = head.style(sel);
        title = title.style(sel);
        tail = tail.style(sel);
    }
    out.push(head);
    out.push(title);
    out.push(tail);
    out
}

/// Display-width pad + truncate in one, so a card line is exactly the column
/// width and the selection highlight is a solid block.
fn pad(s: &str, cols: usize) -> String {
    crate::view::pad_fit(s, cols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Cache;

    fn items() -> Vec<Item> {
        let c: Cache = serde_json::from_str(
            r#"{"version":1,"items":[
              {"kind":"pr","section":"mine","ref":"acme/api#13",
               "title":"日本語のとても長いタイトルでカラムをはみ出すはずのもの",
               "draft":true,"checks":"FAILURE"},
              {"kind":"jira","section":"jira","ref":"ACME-123","title":"進行中のタスク",
               "status":"進行中","status_category":"In Progress"},
              {"kind":"pr","section":"mine","ref":"acme/api#1","title":"short"}
            ]}"#,
        )
        .unwrap();
        c.items
    }

    /// The bug this whole module has to avoid: a CJK title measured in
    /// characters overflows the column and shoves the board sideways.
    #[test]
    fn every_card_line_is_exactly_the_column_width() {
        for it in items() {
            for cols in [8usize, 12, 18, 24, 40] {
                for l in card(&it, cols, false) {
                    assert!(
                        l.width() <= cols,
                        "{:?} at {} cols is {} wide: {:?}",
                        it.r#ref,
                        cols,
                        l.width(),
                        l.to_string()
                    );
                }
            }
        }
    }

    /// The card leads with the two status glyphs, then the ref, the title, and a
    /// trailer of what the column does not already say.
    #[test]
    fn a_card_shows_the_glyphs_the_ref_the_title_and_the_author() {
        use crate::model::{G_CI_FAILED, G_DRAFT};
        let it = &items()[0];
        let text: Vec<String> = card(it, 40, false).iter().map(|l| l.to_string()).collect();
        // draft with no decision yet, and a red CI: one glyph per slot.
        assert!(text[0].starts_with(G_DRAFT), "{:?}", text[0]);
        assert!(text[0].contains(G_CI_FAILED), "{:?}", text[0]);
        assert!(text[0].contains("acme/api#13"));
        assert!(text[1].starts_with("  日本語"));
        assert!(text[2].contains('@'), "{:?}", text[2]);
    }

    /// A Jira card shows the verbatim, locale-specific status -- the column it
    /// sits in is derived from `status_category`, but the card never claims a
    /// translated status.
    #[test]
    fn a_jira_card_shows_the_verbatim_status() {
        let it = &items()[1];
        let text: Vec<String> = card(it, 40, false).iter().map(|l| l.to_string()).collect();
        assert!(text[2].contains("進行中"), "{:?}", text[2]);
        assert!(
            text[0].starts_with(crate::model::G_IN_PROGRESS),
            "{:?}",
            text[0]
        );
    }

    #[test]
    fn a_zero_width_column_produces_empty_lines_not_a_panic() {
        for it in items() {
            for l in card(&it, 0, true) {
                assert_eq!(l.width(), 0);
            }
        }
    }
}
