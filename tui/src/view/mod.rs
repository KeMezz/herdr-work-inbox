//! Rendering.
//!
//! Split one file per surface so the two views never grow into each other:
//!
//! * [`list`] -- the default, three sections, 50% preview on the right.
//! * [`kanban`] -- `v` toggles to it, `Tab` cycles the three boards.
//! * [`preview`] -- the pane the list shares, and the focused `Mode::Preview`.
//!
//! The header is here because both views draw the same one, and so are the
//! display-width helpers, which everything that composes fixed-width text needs.
//!
//! # Display width, not characters
//!
//! Titles in this cache are Japanese and Korean. `str::chars().count()` counts
//! `進` as 1 and the terminal draws it as 2, so a kanban card truncated by
//! character count overflows its column and shoves every card to its right out
//! of alignment. Every width decision in the views therefore goes through
//! [`width`] / [`fit`] / [`wrap_lines`], which measure the way the terminal
//! draws.
//!
//! The measurement itself comes from ratatui (`Span::width`, which is
//! `unicode_width::UnicodeWidthStr::width` underneath) rather than from a direct
//! `unicode-width` dependency: the contract pins the dependency set at four
//! crates and there is no reason to widen it for a function ratatui already
//! exports.
//!
//! `model::pad` / `model::truncate` stay character-based on purpose -- they are
//! the phase 1 `jq` renderer ported verbatim and `--dump` prints them, so
//! changing them would silently change the list row format. The views clip those
//! rows at the pane edge instead, which ratatui does grapheme-correctly.

pub mod kanban;
pub mod list;
pub mod md;
pub mod preview;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::app::{App, Mode, View};

// ------------------------------------------------------------------- palette
//
// Deliberately restricted to the 16 ANSI colours: the popup inherits whatever
// theme the user's terminal has, and a hardcoded RGB would fight it.

/// Section and board headers. Phase 1 drew these in bold cyan (`\e[1;36m`).
pub const C_HEADER: Color = Color::Cyan;
/// Staleness banners and collector notes. Phase 1's `\e[33m`.
pub const C_WARN: Color = Color::Yellow;
/// Kind markers (`PR` / `JIRA`) and other structural chrome.
pub const C_DIM: Color = Color::DarkGray;
/// Kept for the palette's sake; the word tags it used to colour are now status
/// glyphs, which bring their own colour.
#[allow(dead_code)]
pub const C_TAG: Color = Color::Magenta;
/// The `[CI failed]` / `[changes requested]` tags, which mean "act on me".
pub const C_BAD: Color = Color::Red;
pub const C_GOOD: Color = Color::Green;
/// Refs, and the focused chrome.
pub const C_REF: Color = Color::Blue;

pub fn header_style() -> Style {
    Style::default().fg(C_HEADER).add_modifier(Modifier::BOLD)
}

/// Colour for a Jira status badge, keyed on the category rather than the status
/// name: this tenant runs an English `To Do` next to a Japanese `進行中`, so
/// matching on the name would colour one board and not the other.
pub fn category_colour(cat: Option<&crate::model::StatusCategory>) -> Color {
    use crate::model::StatusCategory::*;
    match cat {
        Some(ToDo) => C_REF,
        Some(InProgress) => C_WARN,
        Some(Done) => C_GOOD,
        _ => C_DIM,
    }
}

// -------------------------------------------------------------------- padding
//
// The whole frame sits inside a margin. Without it every row started hard against
// the pane border, which is what made the list feel cramped even when it was not
// dense. It is dropped on a small area rather than scaled: below these sizes the
// columns the margin costs are worth more than the breathing room.

/// Columns of margin on each side, and one blank row under the key hints.
const PAD_H: u16 = 2;
/// Minimum area that still gets the margin.
const PAD_MIN_W: u16 = 48;
const PAD_MIN_H: u16 = 12;

fn padded(area: Rect) -> Rect {
    if area.width < PAD_MIN_W || area.height < PAD_MIN_H {
        return area;
    }
    area.inner(Margin {
        horizontal: PAD_H,
        vertical: 0,
    })
}

pub fn selected_style() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

// ------------------------------------------------------------ width helpers

/// Display width of a string, as the terminal will draw it.
pub fn width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Display width of one character, without allocating.
pub(crate) fn char_width(c: char) -> usize {
    let mut buf = [0u8; 4];
    Span::raw(&*c.encode_utf8(&mut buf)).width()
}

/// Truncate to `cols` **display columns**, appending `…` when something was cut.
///
/// The ellipsis is itself one column wide, so the result is always `<= cols`.
/// A double-width character that would straddle the boundary is dropped whole --
/// half a glyph cannot be drawn and the terminal would pad it anyway.
pub fn fit(s: &str, cols: usize) -> String {
    if cols == 0 {
        return String::new();
    }
    if width(s) <= cols {
        return s.to_string();
    }
    let budget = cols.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// Right-pad to `cols` display columns. Truncates first if it does not fit, so
/// the result is exactly `cols` wide unless a double-width glyph lands on the
/// last column (in which case it is `cols - 1`, never `cols + 1`).
pub fn pad_fit(s: &str, cols: usize) -> String {
    let mut out = fit(s, cols);
    let w = width(&out);
    if w < cols {
        out.extend(std::iter::repeat_n(' ', cols - w));
    }
    out
}

/// Wrap `text` to `cols` display columns, returning the physical lines.
///
/// Written here rather than handed to `Paragraph::wrap` because the preview
/// needs the resulting **line count** to clamp `G` and ctrl-d, and ratatui's
/// `Paragraph::line_count` is gated behind the unstable `rendered-line-info`
/// feature. Pre-wrapping gives an exact count and exact scrolling for free.
///
/// Breaks at whitespace when there is one; hard-breaks a run that is longer than
/// the width (a URL, or CJK text, which has no spaces to break at). An empty
/// input line stays an empty output line -- the preview's blank separators are
/// part of its shape.
pub fn wrap_lines(text: &str, cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    if cols == 0 {
        return out;
    }
    for logical in text.split('\n') {
        let logical = logical.trim_end_matches('\r');
        if logical.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_w = 0usize;
        // Byte offset in `line` of the last space, and its width, so a break can
        // be rewound to it without re-scanning.
        let mut last_space: Option<(usize, usize)> = None;
        for c in logical.chars() {
            let cw = char_width(c);
            if line_w + cw > cols && !line.is_empty() {
                match last_space {
                    Some((at, _)) if at > 0 => {
                        let rest: String = line[at + 1..].to_string();
                        line.truncate(at);
                        out.push(std::mem::take(&mut line));
                        line = rest;
                        line_w = width(&line);
                    }
                    _ => {
                        out.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                }
                last_space = None;
            }
            if c == ' ' {
                last_space = Some((line.len(), line_w));
            }
            line.push(c);
            line_w += cw;
        }
        out.push(line);
    }
    out
}

// ------------------------------------------------------------------- surfaces

/// The whole frame: header, hint line, the active view, and the agent picker on
/// top of it when one is open.
pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width == 0 || area.height == 0 {
        return;
    }

    // Length(1) rows collapse to 0 on a tiny area rather than panicking, which
    // is exactly the degradation the contract asks for. The blank row after the
    // hints is part of the same idea as the side margin: chrome and content read
    // as two things only if there is a gap between them.
    let inner = padded(area);
    let searching = app.mode == Mode::Search || !app.query.is_empty();
    let roomy = inner.height >= PAD_MIN_H;
    let rows: [Rect; 5] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(if searching { 1 } else { 0 }),
        Constraint::Length(if roomy { 1 } else { 0 }),
        Constraint::Min(0),
    ])
    .areas(inner);

    draw_header(f, rows[0], app);
    draw_hints(f, rows[1], app);
    if searching {
        draw_search(f, rows[2], app);
    }

    match &app.loaded {
        Err(e) => {
            let msg = e.to_string();
            draw_error(f, rows[4], &msg);
        }
        Ok(_) => match app.view {
            View::List => list::draw(f, rows[4], app),
            View::Kanban => kanban::draw(f, rows[4], app),
        },
    }

    if app.mode == Mode::AgentPicker {
        draw_agent_picker(f, area, app);
    }
    if app.mode == Mode::Config {
        draw_config(f, area, app);
    }
}

/// The header line:
///
/// ```text
/// updated 4m ago   11 jira / 5 review / 21 mine        refreshing -
/// ```
///
/// Age comes from `Cache::age` (`just now` under 60s), counts from
/// `Cache::header_counts` in the phase 1 order (jira / review / mine). A spawned
/// collect that is still running puts a spinner on the right; the tick reaps the
/// child, so the marker disappears on its own.
pub fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let mut left = match &app.loaded {
        Ok(l) => {
            let (j, r, m) = l.cache.header_counts();
            format!(
                "updated {}   {} jira / {} review / {} mine",
                l.cache.age(app.now),
                j,
                r,
                m
            )
        }
        Err(_) => "no cache".to_string(),
    };
    if let Some(flash) = &app.flash {
        left.push_str("   ");
        left.push_str(&flash.text);
    }

    let right = if app.refreshing {
        format!("refreshing {}", app.spinner_char())
    } else {
        String::new()
    };

    let cols = area.width as usize;
    let rw = width(&right);
    let lw = cols.saturating_sub(rw + 1).max(1);
    let line = Line::from(vec![
        Span::styled(pad_fit(&left, lw), Style::default().fg(C_HEADER)),
        Span::raw(" "),
        Span::styled(right, Style::default().fg(C_WARN)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// Key hints, per mode. Only the keys that are live in the current mode are
/// listed -- the keymap is modal and advertising `o` while in preview would be a
/// lie.
fn draw_hints(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let hints = match app.mode {
        Mode::Preview => "j/k scroll  ctrl-d/u half page  g/G top/bottom  esc back",
        Mode::Search => "type to filter  enter apply  esc clear",
        Mode::AgentPicker => "j/k choose  enter put in its input  esc cancel",
        Mode::Config => "j/k move  space show/hide  A show all  esc done",
        Mode::Nav => match app.view {
            View::List => {
                "j/k move  h/l section  enter preview  o open  y copy  a agent  \
                 v kanban  r refresh  / search  q quit"
            }
            View::Kanban => {
                "j/k card  h/l column  Tab board  enter preview  o open  y copy  \
                 a agent  v list  r refresh  / search  q quit"
            }
        },
    };
    f.render_widget(
        Paragraph::new(Line::styled(
            fit(hints, area.width as usize),
            Style::default().fg(C_DIM),
        )),
        area,
    );
}

fn draw_search(f: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    // In nav the hint must describe the key that actually clears the filter.
    // `esc` in nav QUITS -- that is the user-confirmed keymap -- so the old
    // `(esc clears)` invited the user to close the popup while trying to widen
    // the list. `/` re-enters search mode, where esc really does clear.
    let prompt = if app.mode == Mode::Search {
        format!("/{}_", app.query)
    } else {
        format!("/{}   (/ then esc clears)", app.query)
    };
    f.render_widget(
        Paragraph::new(Line::styled(
            fit(&prompt, area.width as usize),
            Style::default()
                .fg(C_REF)
                .add_modifier(Modifier::BOLD),
        )),
        area,
    );
}

/// One line of a section/board header: `── LABEL (n) ──────────`.
pub fn header_line(label: &str, count: usize, cols: usize, focused: bool) -> Line<'static> {
    let text = format!("── {label} ({count}) ");
    let mut s = fit(&text, cols);
    let w = width(&s);
    if w < cols {
        s.extend(std::iter::repeat_n('─', cols - w));
    }
    let style = if focused {
        header_style().add_modifier(Modifier::REVERSED)
    } else {
        header_style()
    };
    Line::styled(s, style)
}

/// The warning banner for a failed source, from `SourceState::banner`.
///
/// * with retained items: `jira: stale, 41m ago — <note>`
/// * with no items:       `<note>`
///
/// A leg that is `ok:true` **with** a note still shows the note (see
/// [`crate::app::App::section_banner`]) -- it is how the collector reports a
/// group-readable credential file, and it is the one diagnostic that must never
/// be silently dropped.
pub fn banner_line(text: &str, cols: usize) -> Line<'static> {
    Line::styled(
        fit(&format!("  ⚠ {text}"), cols),
        Style::default().fg(C_WARN).add_modifier(Modifier::BOLD),
    )
}

/// The frame drawn when the cache is missing or unparseable: which file, why,
/// and how to fix it. Never a panic, and never an empty screen.
pub fn draw_error(f: &mut Frame, area: Rect, err: &str) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let cols = area.width.saturating_sub(2) as usize;
    let mut lines = vec![
        Line::styled(
            "the cache could not be read",
            Style::default().fg(C_BAD).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
    ];
    for l in wrap_lines(err, cols) {
        lines.push(Line::raw(l));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "press r to run the collector, q to close",
        Style::default().fg(C_DIM),
    ));
    lines.push(Line::styled(
        "the list appears by itself once the collector has written a cache",
        Style::default().fg(C_DIM),
    ));
    f.render_widget(
        Paragraph::new(lines).block(Block::new().borders(Borders::ALL).border_type(BorderType::Rounded)),
        area,
    );
}

/// The agent picker, drawn by this crate. Phase 1 shelled out to a second fzf
/// here; phase 2 owns the terminal and must not hand it to another full-screen
/// program.
///
/// **The rows scroll.** There are 18 agents on this machine and the popup is 80%
/// of the window height, so the candidate list routinely outgrows the box. Drawn
/// as a plain `Paragraph` it silently showed only the first `h-2` of them while
/// `j` walked the cursor past the bottom edge -- invisible highlight, no marker,
/// and `enter` then writing into an agent that was never on
/// screen. That is exactly the mis-delivered hand-off `resolve_agent` refuses to
/// make; it must not come back one layer up. The offset is recomputed from
/// `agent_cursor` every frame, which is exact because a candidate is one row.
/// The config screen: which repositories and projects this panel shows.
///
/// A modal over the list rather than a separate screen, so the effect of a
/// toggle is visible in the counts the moment it is made.
///
/// Deliberately a deny list -- a checked row means "shown", and a source nobody
/// has touched is checked. A new repository appearing in your review queue can
/// never be hidden by a default.
fn draw_config(f: &mut Frame, area: Rect, app: &App) {
    let rows = app.config_rows();
    let w = area.width.saturating_sub(4).clamp(1, 64);
    let h = (rows.len() as u16 + 4)
        .min(area.height.saturating_sub(2))
        .max(3);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);

    f.render_widget(Clear, rect);
    let hidden = app.hidden_count();
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(if hidden > 0 {
            format!(" show in this panel  ({hidden} items hidden) ")
        } else {
            " show in this panel ".to_string()
        })
        .border_style(Style::default().fg(C_REF));
    let inner = block.inner(rect).inner(Margin {
        horizontal: 1,
        vertical: 0,
    });
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let cols = inner.width as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        match row {
            crate::app::ConfigRow::Header(h) => {
                // A blank line above every group but the first.
                if !lines.is_empty() {
                    lines.push(Line::raw(""));
                }
                lines.push(Line::styled(pad_fit(h, cols), header_style()));
            }
            crate::app::ConfigRow::Repo { name, shown, n }
            | crate::app::ConfigRow::Project { name, shown, n } => {
                let mark = if *shown { "[x]" } else { "[ ]" };
                let text = format!("{} {}", mark, pad_fit(name, cols.saturating_sub(12)));
                let count = format!("{n:>4}");
                let mut line = Line::from(vec![
                    Span::styled(
                        text,
                        if *shown {
                            Style::default()
                        } else {
                            Style::default().fg(C_DIM)
                        },
                    ),
                    Span::styled(count, Style::default().fg(C_DIM)),
                ]);
                let lw = line.width();
                if lw < cols {
                    line.push_span(Span::raw(" ".repeat(cols - lw)));
                }
                if i == app.cfg_cursor {
                    line = line.style(selected_style());
                }
                lines.push(line);
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled(
            "nothing in the cache to filter yet",
            Style::default().fg(C_WARN),
        ));
    }
    // Keep the cursor visible when the list is taller than the modal.
    let sel = rows
        .iter()
        .take(app.cfg_cursor)
        .filter(|r| matches!(r, crate::app::ConfigRow::Header(_)))
        .count()
        + app.cfg_cursor;
    let offset = scroll_to(0, sel, inner.height as usize, lines.len());
    render_rows(f, inner, &lines, offset);
}

fn draw_agent_picker(f: &mut Frame, area: Rect, app: &App) {
    let w = area.width.saturating_sub(4).clamp(1, 90);
    let h = (app.agents.len() as u16 + 4)
        .min(area.height.saturating_sub(2))
        .max(3);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect::new(x, y, w, h);

    f.render_widget(Clear, rect);
    let n = app.agents.len();
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(if n > 1 {
            format!(" put it in which agent's input? ({}/{}) ", app.agent_cursor + 1, n)
        } else {
            " put it in which agent's input? ".to_string()
        })
        .border_style(Style::default().fg(C_REF));
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let inner_w = inner.width as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, a) in app.agents.iter().enumerate() {
        let text = format!(
            "{}  {}  {}  {}",
            pad_fit(&a.agent, 12),
            pad_fit(&a.agent_status, 10),
            pad_fit(&a.workspace_id, 10),
            a.terminal_title
        );
        let style = if i == app.agent_cursor {
            selected_style()
        } else {
            Style::default()
        };
        lines.push(Line::styled(pad_fit(&text, inner_w), style));
    }
    if lines.is_empty() {
        lines.push(Line::styled("no agent is running", Style::default().fg(C_WARN)));
    }
    let offset = scroll_to(0, app.agent_cursor, inner.height as usize, lines.len());
    render_rows(f, inner, &lines, offset);
}

/// Render a pre-built row vector into `area` with a scroll `offset`, slicing
/// rather than asking `Paragraph` to scroll: the rows are already exactly one
/// physical line each, so a slice is both cheaper and exact.
pub fn render_rows(f: &mut Frame, area: Rect, rows: &[Line<'static>], offset: usize) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let h = area.height as usize;
    let start = offset.min(rows.len());
    let end = (start + h).min(rows.len());
    let slice: Vec<Line> = rows[start..end].to_vec();
    f.render_widget(Paragraph::new(slice), area);
}

/// Keep `offset` such that row `sel` is visible in a viewport `h` rows tall.
pub fn scroll_to(offset: usize, sel: usize, h: usize, len: usize) -> usize {
    if h == 0 {
        return 0;
    }
    let mut o = offset;
    if sel < o {
        o = sel;
    } else if sel >= o + h {
        o = sel + 1 - h;
    }
    o.min(len.saturating_sub(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_counts_columns_not_characters() {
        assert_eq!(width("abc"), 3);
        // the exact case that misaligns a kanban column: 4 chars, 8 columns
        assert_eq!(width("進行中で"), 8);
        assert_eq!(width("한글"), 4);
        assert_eq!(width(""), 0);
    }

    #[test]
    fn fit_never_exceeds_the_column_budget() {
        assert_eq!(fit("abcdef", 10), "abcdef");
        assert_eq!(fit("abcdef", 6), "abcdef");
        assert_eq!(fit("abcdef", 3), "ab…");
        assert_eq!(fit("abc", 0), "");
        // a double-width glyph that would straddle the last column is dropped
        // whole rather than half-drawn
        let s = fit("進行中です", 5);
        assert!(width(&s) <= 5, "{s:?} is {} cols", width(&s));
        assert!(s.ends_with('…'));
        for cols in 1..12 {
            assert!(width(&fit("進行中ですよ", cols)) <= cols);
            assert!(width(&fit("mixed 進行 text", cols)) <= cols);
        }
    }

    #[test]
    fn pad_fit_is_exactly_the_requested_width_for_ascii() {
        assert_eq!(pad_fit("ab", 5), "ab   ");
        assert_eq!(width(&pad_fit("進行中", 8)), 8);
        // never wider than asked, even when a wide glyph lands on the boundary
        for cols in 1..12 {
            assert!(width(&pad_fit("進行中ですよ", cols)) <= cols);
        }
    }

    #[test]
    fn wrap_breaks_at_spaces_and_hard_breaks_cjk() {
        let w = wrap_lines("the quick brown fox", 10);
        assert_eq!(w, vec!["the quick", "brown fox"]);
        // no spaces to break at: hard break, and every line fits
        let w = wrap_lines("進行中ですよろしく", 6);
        for l in &w {
            assert!(width(l) <= 6, "{l:?}");
        }
        assert_eq!(w.concat(), "進行中ですよろしく");
        // blank lines survive -- the preview's separators depend on it
        assert_eq!(wrap_lines("a\n\nb", 10), vec!["a", "", "b"]);
        // a single run longer than the width is hard-broken, not dropped
        let w = wrap_lines("https://example.com/a/very/long/path", 10);
        assert!(w.len() > 1);
        assert_eq!(w.concat(), "https://example.com/a/very/long/path");
        // zero width is a degenerate area, not a panic
        assert!(wrap_lines("anything", 0).is_empty());
    }

    #[test]
    fn scroll_keeps_the_selection_visible() {
        assert_eq!(scroll_to(0, 0, 10, 40), 0);
        assert_eq!(scroll_to(0, 15, 10, 40), 6);
        assert_eq!(scroll_to(20, 3, 10, 40), 3);
        // never scrolls past the end
        assert_eq!(scroll_to(0, 39, 10, 40), 30);
        // a viewport taller than the content pins to the top
        assert_eq!(scroll_to(5, 2, 40, 10), 0);
        // a zero-height viewport is not a division by anything
        assert_eq!(scroll_to(3, 3, 0, 40), 0);
    }
}

/// Whole-frame rendering tests against ratatui's `TestBackend`.
///
/// These are the "never panic on a tiny area" guarantee, executed rather than
/// argued: every mode and both views are drawn at sizes from 1x1 up, and a
/// panic anywhere in the layout maths fails the test. They also cover the two
/// surfaces the pty smoke test cannot reach without side effects -- the agent
/// picker (which would need a live `herdr agent list`) and the error frame.
#[cfg(test)]
mod frame_tests {
    use super::*;
    use crate::app::App;
    use crate::cache::Loaded;
    use crate::model::{Board, Cache, Section};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    const FIXTURE: &str = r#"{
      "version": 1, "fetched_unix": 1000,
      "sources": {
        "github": {"ok": true, "note": "", "fetched_unix": 1000},
        "jira": {"ok": false, "note": "jira: unreachable after 3 tries (curl exit 56) - network, not auth",
                 "fetched_unix": 400}
      },
      "items": [
        {"kind":"pr","section":"review","ref":"acme/api#1","url":"https://x/1","title":"alpha",
         "author":"someone","updated":"2026-08-11T00:00:00Z","review_decision":"APPROVED",
         "body":"a body that is long enough to need wrapping in a narrow pane, several times over"},
        {"kind":"pr","section":"mine","ref":"acme/api#2","url":"https://x/2",
         "title":"日本語のとても長いタイトルでカラムをはみ出すはずのもの","author":"me",
         "updated":"2026-08-10T00:00:00Z","draft":true,"checks":"FAILURE","body":""},
        {"kind":"jira","section":"jira","ref":"ACME-1","key":"ACME-1","url":"https://x/j",
         "title":"進行中のタスク","status":"進行中","status_category":"In Progress","type":"PBI",
         "priority":"Medium","project":"ACME","updated":"2026-08-11T00:00:00Z","body":"説明"}
      ]
    }"#;

    fn app() -> App {
        let cache: Cache = serde_json::from_str(FIXTURE).unwrap();
        let mut a = App::new(PathBuf::from("/nonexistent/wi/cache.json"));
        a.loaded = Ok(Loaded {
            path: PathBuf::from("/nonexistent/wi/cache.json"),
            cache,
            mtime: None,
        });
        a.now = 1600;
        a.section = Section::Review;
        a
    }

    fn render(a: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, a)).unwrap();
        let buf = t.backend().buffer().clone();
        // A double-width glyph occupies TWO cells: the first holds the glyph,
        // the second is a reset cell whose symbol is a space. Joining cells
        // naively turns 進行中 into "進 行 中" and every assertion about
        // Japanese text becomes a lie, so the continuation cell is skipped.
        (0..buf.area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0u16;
                while x < buf.area.width {
                    let sym = buf[(x, y)].symbol();
                    row.push_str(sym);
                    x += if width(sym) == 2 { 2 } else { 1 };
                }
                row
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The popup is 90%x80% of the user's window, but nothing stops that window
    /// from being tiny. Every size here must draw a frame, not panic.
    #[test]
    fn no_size_panics_in_any_mode() {
        for (w, h) in [(1u16, 1u16), (2, 2), (5, 3), (20, 5), (40, 10), (56, 12), (80, 24), (200, 45)] {
            for view in [View::List, View::Kanban] {
                for mode in [Mode::Nav, Mode::Preview, Mode::Search, Mode::AgentPicker] {
                    for board in [Board::Review, Board::Mine, Board::Jira] {
                        let mut a = app();
                        a.view = view;
                        a.mode = mode;
                        a.board = board;
                        a.column = board.columns().len() - 1;
                        a.query = if mode == Mode::Search { "a".into() } else { String::new() };
                        a.refreshing = true;
                        a.flash_msg("copied acme/api#1");
                        a.agents = vec![crate::actions::Agent {
                            pane_id: "p1".into(),
                            agent: "claude".into(),
                            agent_status: "idle".into(),
                            workspace_id: "w1".into(),
                            terminal_title: "repo — claude".into(),
                            focused: true,
                        }];
                        a.clamp();
                        let _ = render(&mut a, w, h);
                    }
                }
            }
        }
    }

    #[test]
    fn the_list_frame_has_the_header_the_sections_and_the_stale_banner() {
        let mut a = app();
        let s = render(&mut a, 120, 30);
        assert!(s.contains("updated 10m ago"), "{s}");
        assert!(s.contains("1 jira / 1 review / 1 mine"));
        assert!(s.contains("REVIEW REQUESTED (1)"));
        assert!(s.contains("MY PULL REQUESTS (1)"));
        assert!(s.contains("MY JIRA ISSUES (1)"));
        // the failed jira leg retained its item -> "stale", with the LEG's age
        assert!(s.contains("jira: stale, 20m ago"), "{s}");
        assert!(s.contains("q quit"));
        // the preview pane is drawn next to the list
        assert!(s.contains("GitHub pull request"));
    }

    /// A leg that failed with no retained items shows the bare note, not a
    /// "stale" claim about data it does not have.
    #[test]
    fn an_empty_failed_leg_shows_the_bare_note() {
        let mut a = app();
        if let Ok(l) = &mut a.loaded {
            l.cache.items.retain(|i| i.section != Section::Jira);
        }
        let s = render(&mut a, 120, 30);
        assert!(s.contains("curl exit 56"), "{s}");
        assert!(!s.contains("jira: stale"), "{s}");
    }

    #[test]
    fn the_kanban_frame_has_the_board_header_and_every_column() {
        let mut a = app();
        a.view = View::Kanban;
        for (board, cols) in [
            (Board::Review, vec!["NEEDS REVIEW", "CHANGES REQUESTED", "APPROVED"]),
            (Board::Mine, vec!["DRAFT", "IN REVIEW", "APPROVED", "CI FAILED"]),
            (Board::Jira, vec!["TO DO", "IN PROGRESS", "DONE"]),
        ] {
            a.board = board;
            a.column = 0;
            a.clamp();
            let s = render(&mut a, 160, 30);
            assert!(s.contains(board.header()), "{}: {s}", board.header());
            for c in cols {
                assert!(s.contains(c), "board {} is missing column {c}\n{s}", board.header());
            }
        }
    }

    /// The Jira board buckets on `status_category` but the card shows the
    /// verbatim, locale-specific `status`.
    #[test]
    fn a_jira_card_sits_under_an_english_header_and_shows_a_japanese_status() {
        let mut a = app();
        a.view = View::Kanban;
        a.board = Board::Jira;
        a.column = 1;
        a.clamp();
        let s = render(&mut a, 160, 30);
        assert!(s.contains("IN PROGRESS (1)"), "{s}");
        assert!(s.contains("進行中"), "{s}");
        assert!(s.contains("ACME-1"));
    }

    #[test]
    fn preview_mode_takes_the_board_over_in_kanban() {
        let mut a = app();
        a.view = View::Kanban;
        a.board = Board::Jira;
        a.column = 1; // the only jira item is IN PROGRESS
        a.mode = Mode::Preview;
        a.clamp();
        let s = render(&mut a, 120, 30);
        assert!(s.contains("Jira issue"), "{s}");
        assert!(s.contains("esc to go back"));
        assert!(!s.contains("IN PROGRESS ("), "the board is not drawn under the preview\n{s}");
    }

    #[test]
    fn the_error_frame_names_the_file_and_offers_r() {
        let mut a = App::new(PathBuf::from("/nonexistent/wi/cache.json"));
        let s = render(&mut a, 100, 20);
        assert!(s.contains("the cache could not be read"), "{s}");
        assert!(s.contains("cache.json"), "{s}");
        assert!(s.contains("press r to run the collector"), "{s}");
    }

    #[test]
    fn the_agent_picker_lists_its_candidates() {
        let mut a = app();
        a.mode = Mode::AgentPicker;
        a.agents = vec![
            crate::actions::Agent {
                pane_id: "pane-1".into(),
                agent: "claude".into(),
                agent_status: "idle".into(),
                workspace_id: "ws-1".into(),
                terminal_title: "config — claude".into(),
                focused: false,
            },
            crate::actions::Agent {
                pane_id: "pane-2".into(),
                agent: "codex".into(),
                agent_status: "busy".into(),
                workspace_id: "ws-2".into(),
                terminal_title: "api — codex".into(),
                focused: true,
            },
        ];
        let s = render(&mut a, 120, 30);
        assert!(s.contains("put it in which agent"), "{s}");
        assert!(s.contains("claude"));
        assert!(s.contains("codex"));
    }

    /// The search line is its own row and the hints change with the mode, so the
    /// filter is never a mystery.
    #[test]
    fn search_mode_shows_the_query_and_filters_the_sections() {
        let mut a = app();
        a.mode = Mode::Search;
        a.query = "acme-1".into();
        a.clamp();
        let s = render(&mut a, 120, 30);
        assert!(s.contains("/acme-1_"), "{s}");
        assert!(s.contains("type to filter"));
        assert!(s.contains("REVIEW REQUESTED (0)"), "{s}");
        assert!(s.contains("MY JIRA ISSUES (1)"), "{s}");
    }

    /// In kanban the filter works on the cards in place: the columns and their
    /// headers stay put, and the counts shown are the filtered counts.
    #[test]
    fn search_filters_kanban_cards_in_place() {
        let mut a = app();
        a.view = View::Kanban;
        a.board = Board::Mine;
        a.mode = Mode::Search;
        a.query = "zzzz".into();
        a.clamp();
        let s = render(&mut a, 160, 30);
        for c in ["DRAFT (0)", "IN REVIEW (0)", "APPROVED (0)", "CI FAILED (0)"] {
            assert!(s.contains(c), "missing {c}\n{s}");
        }
        assert!(s.contains("MY PULL REQUESTS (0)"), "{s}");

        a.query = "acme".into();
        a.clamp();
        let s = render(&mut a, 160, 30);
        assert!(s.contains("CI FAILED (1)"), "{s}");
        assert!(s.contains("MY PULL REQUESTS (1)"), "{s}");
    }

    /// 18 agents is what `herdr agent list` returns on this machine and the
    /// picker box is ~10 rows tall inside an 80%-height popup. The highlighted
    /// candidate must be ON SCREEN -- an invisible cursor plus `enter` is a
    /// prompt injected into an agent the user never saw.
    #[test]
    fn the_agent_picker_scrolls_to_keep_the_cursor_visible() {
        let mut a = app();
        a.mode = Mode::AgentPicker;
        a.agents = (0..18)
            .map(|i| crate::actions::Agent {
                pane_id: format!("pane-{i}"),
                agent: "claude".into(),
                agent_status: "idle".into(),
                workspace_id: format!("ws-{i}"),
                terminal_title: format!("AGENT-TITLE-{i}"),
                focused: false,
            })
            .collect();

        a.agent_cursor = 17;
        let s = render(&mut a, 100, 14);
        assert!(s.contains("AGENT-TITLE-17"), "the cursor row must be drawn\n{s}");
        assert!(!s.contains("AGENT-TITLE-0"), "it must have scrolled\n{s}");
        assert!(s.contains("(18/18)"), "the box says where in the list you are\n{s}");

        // and back at the top the first candidate is the one on screen
        a.agent_cursor = 0;
        let s = render(&mut a, 100, 14);
        assert!(s.contains("AGENT-TITLE-0"), "{s}");
        assert!(!s.contains("AGENT-TITLE-17"), "{s}");
    }

    /// Focus on a section with no rows must scroll ITS header into view. With the
    /// list anchored at row 0 instead, `h`/`l` onto an empty section looked like a
    /// dead key: the header was below the fold and the frame did not change.
    #[test]
    fn focusing_an_empty_section_scrolls_its_header_into_view() {
        let mut a = app();
        if let Ok(l) = &mut a.loaded {
            l.cache.items.retain(|i| i.section != Section::Jira);
            let template = l
                .cache
                .items
                .iter()
                .find(|i| i.section == Section::Mine)
                .unwrap()
                .clone();
            for n in 0..30 {
                let mut it = template.clone();
                it.r#ref = format!("acme/api#{}", 100 + n);
                l.cache.items.push(it);
            }
        }
        a.section = Section::Jira;
        a.cursor = 0;
        a.clamp();
        let s = render(&mut a, 120, 12);
        assert!(s.contains("MY JIRA ISSUES (0)"), "{s}");
        assert!(s.contains("no unresolved issues are assigned to you"), "{s}");
    }

    /// Below the split width the preview takes the whole area instead of the
    /// mode being cancelled. The cancellation made `enter` look dead AND turned
    /// the `esc` that followed into a quit.
    #[test]
    fn a_narrow_terminal_gives_preview_mode_the_whole_width() {
        let mut a = app();
        a.mode = Mode::Preview;
        let s = render(&mut a, 50, 20);
        assert_eq!(a.mode, Mode::Preview, "the mode must survive a narrow frame");
        assert!(s.contains("GitHub pull request"), "{s}");
        assert!(s.contains("esc to go back"), "{s}");
        assert!(a.preview_height > 0, "and it must still be scrollable");
    }

    /// The applied-filter line has to name a key that really clears the filter:
    /// `esc` in nav QUITS, so advertising it here cost the user their popup.
    #[test]
    fn the_applied_filter_line_names_a_key_that_clears_it() {
        let mut a = app();
        a.mode = Mode::Nav;
        a.query = "acme".into();
        a.clamp();
        let s = render(&mut a, 120, 20);
        assert!(s.contains("/acme   (/ then esc clears)"), "{s}");
        assert!(!s.contains("(esc clears)"), "{s}");
    }

    /// A pane too narrow to be useful is dropped rather than drawn as a sliver.
    #[test]
    fn a_narrow_terminal_drops_the_preview_pane() {
        let mut a = app();
        let wide = render(&mut a, 120, 20);
        assert!(wide.contains("GitHub pull request"));
        let narrow = render(&mut a, 50, 20);
        // the pane's contents, not the word "preview", which is in the hint line
        assert!(!narrow.contains("GitHub pull request"), "{narrow}");
        assert!(narrow.contains("REVIEW REQUESTED"));
    }
}
