//! The preview pane.
//!
//! Shows the **cached** body of the focused item. There is no network call and
//! no `gh pr view` here: `collect.sh` pre-fetches every PR description and Jira
//! description into `item.body` precisely so that moving the cursor costs
//! nothing. Adding a fetch here would undo the entire point of the plugin.
//!
//! Two roles:
//!
//! * as a pane, 50% of the width, next to the list;
//! * as a focus, `Mode::Preview`, entered with enter/space, where j/k scroll,
//!   ctrl-d/ctrl-u half-page, g/G jump to top/bottom, and esc returns to nav.
//!
//! The field block above the body is phase 1's, ported as-is.
//!
//! # Wrapping
//!
//! The text is pre-wrapped by [`crate::view::wrap_lines`] rather than handed to
//! `Paragraph::wrap`, for two reasons. The scroll keys need the exact wrapped
//! line count to clamp `G` and ctrl-d, and ratatui's `Paragraph::line_count` is
//! behind the unstable `rendered-line-info` feature; and a body is routinely
//! 8000 characters, so slicing the visible window out of a pre-wrapped vector is
//! strictly less work per frame than re-wrapping all of it and scrolling.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Borders};

use crate::app::{App, Mode};
use crate::model::Item;
use crate::view::{C_DIM, C_REF, C_WARN, fit, render_rows, wrap_lines};

/// The rule phase 1 draws between the field block and the body.
const RULE: &str = "────────────────────────────────────────────────";

pub fn draw(f: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 || area.width == 0 {
        app.preview_height = 0;
        app.preview_lines = 0;
        return;
    }
    let focused = app.mode == Mode::Preview;
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(if focused {
            " preview (esc to go back) "
        } else {
            " preview "
        })
        .border_style(if focused {
            Style::default().fg(C_REF).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(C_DIM)
        });
    // horizontal: 2, not 1. A margin of 1 lands the text on the column the
    // border occupies the other side of, so the body read as if it were glued to
    // the frame; the second column is the gap.
    let inner = area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        app.preview_height = 0;
        app.preview_lines = 0;
        return;
    }

    let cols = inner.width as usize;
    let rows = match app.selected() {
        Some(it) => lines(it, cols),
        None => vec![Line::styled(
            fit("nothing selected", cols),
            Style::default().fg(C_DIM),
        )],
    };

    app.preview_height = inner.height as usize;
    app.preview_lines = rows.len();
    // The item may have changed under a scrolled preview (j in nav, a reload on
    // the tick), so the offset is clamped here, where the true line count is
    // known, rather than guessed at in the keymap.
    let max = rows.len().saturating_sub(inner.height as usize);
    if app.preview_scroll > max {
        app.preview_scroll = max;
    }
    render_rows(f, inner, &rows, app.preview_scroll);
}

/// The full preview text for one item, already wrapped to `cols`.
///
/// Field block, url, rule, body -- the same order and the same labels phase 1's
/// `jq` renderer emits, so the two front ends show the same thing.
pub fn lines(it: &Item, cols: usize) -> Vec<Line<'static>> {
    let label = Style::default().fg(C_DIM);
    let mut out: Vec<Line<'static>> = Vec::new();

    let field = |k: &str, v: &str, out: &mut Vec<Line<'static>>| {
        // The label column is 10 wide in phase 1 (`ref       `), and the value
        // wraps under it rather than being cut.
        let head = format!("{k:<10}");
        let body_cols = cols.saturating_sub(10).max(1);
        let wrapped = wrap_lines(v, body_cols);
        for (i, w) in wrapped.iter().enumerate() {
            if i == 0 {
                out.push(Line::from(vec![
                    ratatui::text::Span::styled(head.clone(), label),
                    ratatui::text::Span::raw(w.clone()),
                ]));
            } else {
                out.push(Line::raw(format!("{}{}", " ".repeat(10), w)));
            }
        }
        if wrapped.is_empty() {
            out.push(Line::styled(head.clone(), label));
        }
    };

    match it.kind {
        crate::model::Kind::Jira => {
            out.push(Line::styled(
                "Jira issue",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            out.push(Line::raw(""));
            let key = if it.r#ref.is_empty() { &it.key } else { &it.r#ref };
            field("key", key, &mut out);
            field("summary", &it.title, &mut out);
            field(
                "status",
                &format!(
                    "{} ({})",
                    or(&it.status, "?"),
                    it.status_category.as_ref().map_or("?", |c| c.as_str())
                ),
                &mut out,
            );
            field("type", or(&it.r#type, "?"), &mut out);
            field("priority", or(&it.priority, "-"), &mut out);
            field("project", or(&it.project, "?"), &mut out);
            field("updated", or(&it.updated, "?"), &mut out);
        }
        _ => {
            out.push(Line::styled(
                "GitHub pull request",
                Style::default().add_modifier(Modifier::BOLD),
            ));
            out.push(Line::raw(""));
            field("ref", &it.r#ref, &mut out);
            field("title", &it.title, &mut out);
            field("repo", or(&it.repo, "?"), &mut out);
            field("author", &format!("@{}", or(&it.author, "ghost")), &mut out);
            field("updated", or(&it.updated, "?"), &mut out);
            field("draft", if it.draft { "yes" } else { "no" }, &mut out);
            field(
                "review",
                it.review_decision
                    .as_ref()
                    .map_or("(no decision yet)", |d| d.as_str()),
                &mut out,
            );
            field(
                "checks",
                it.checks.as_ref().map_or("(none reported)", |c| c.as_str()),
                &mut out,
            );
        }
    }

    out.push(Line::raw(""));
    for w in wrap_lines(&it.url, cols) {
        out.push(Line::styled(w, Style::default().fg(C_REF)));
    }
    out.push(Line::raw(""));
    out.push(Line::styled(
        fit(RULE, cols),
        Style::default().fg(C_DIM),
    ));
    out.push(Line::raw(""));

    if it.body.trim().is_empty() {
        out.push(Line::styled("(no description)", Style::default().fg(C_WARN)));
    } else {
        // Control characters would move the cursor inside the drawn frame; the
        // jq renderer strips the same class before it ever reaches fzf.
        let body: String = it
            .body
            .chars()
            // A newline is the one control character that survives -- it is
            // the body's own structure and the renderer splits on it. Every
            // other one, tab included, becomes a space.
            .map(|c| if c == '\n' || !c.is_control() { c } else { ' ' })
            .collect();
        // Markdown, not raw text. PR bodies are GFM and the ADF walker in
        // lib/common.sh emits the same shapes for Jira, so one renderer covers
        // both. It does its own column-accurate wrapping, which is why
        // `wrap_lines` is not involved here: the preview scroll needs an exact
        // line count and only the renderer knows where it broke.
        out.extend(crate::view::md::render(&body, cols));
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

    fn item(json: &str) -> Item {
        let c: Cache =
            serde_json::from_str(&format!(r#"{{"version":1,"items":[{json}]}}"#)).unwrap();
        c.items.into_iter().next().unwrap()
    }

    #[test]
    fn a_pr_preview_has_the_phase1_field_block() {
        let it = item(
            r#"{"kind":"pr","section":"mine","ref":"acme/api#12","url":"https://x/1",
                "title":"t","repo":"acme/api","author":"me","updated":"2026-08-11T00:00:00Z",
                "draft":true,"checks":"FAILURE","body":"hello\nworld"}"#,
        );
        let text: Vec<String> = lines(&it, 60).iter().map(|l| l.to_string()).collect();
        let joined = text.join("\n");
        assert!(joined.starts_with("GitHub pull request"));
        for k in ["ref", "title", "repo", "author", "updated", "draft", "review", "checks"] {
            assert!(joined.contains(&format!("{k:<10}")), "missing field {k}");
        }
        assert!(joined.contains("(no decision yet)"));
        assert!(joined.contains("https://x/1"));
        assert!(joined.contains("hello"));
        assert!(joined.contains("world"));
    }

    #[test]
    fn a_jira_preview_shows_verbatim_status_and_its_category() {
        let it = item(
            r#"{"kind":"jira","section":"jira","ref":"G-1","key":"G-1","url":"https://x/j",
                "title":"進行中のタスク","status":"進行中","status_category":"In Progress",
                "type":"Task","priority":"Medium","project":"ACME",
                "updated":"2026-08-11T00:00:00Z","body":""}"#,
        );
        let joined = lines(&it, 60)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.starts_with("Jira issue"));
        assert!(joined.contains("進行中 (In Progress)"));
        assert!(joined.contains("(no description)"));
    }

    /// A body that is 8000 characters must wrap, not blow the layout: every
    /// physical line has to fit the pane.
    #[test]
    fn a_huge_body_wraps_within_the_pane() {
        let big = "lorem ipsum dolor sit amet ".repeat(300);
        let it = item(&format!(
            r#"{{"kind":"pr","section":"mine","ref":"a#1","url":"u","title":"t","body":{}}}"#,
            serde_json::to_string(&big).unwrap()
        ));
        let rows = lines(&it, 40);
        assert!(rows.len() > 100);
        for l in &rows {
            assert!(l.width() <= 40, "{:?} is {} cols", l.to_string(), l.width());
        }
    }

    /// A body can carry a stray control character (a BEL, an ESC from a CI log
    /// pasted into a PR description). Inside the alternate screen an ESC would
    /// be interpreted as a cursor command and repaint the wrong cells, so it
    /// never reaches the frame -- the same class the jq renderer strips before
    /// it hands a row to fzf.
    #[test]
    fn control_characters_in_a_body_never_reach_the_frame() {
        let raw = "a\u{7}b\u{1b}[31mc\td";
        let it = item(&format!(
            r#"{{"kind":"pr","section":"mine","ref":"a#1","url":"u","title":"t","body":{}}}"#,
            serde_json::to_string(raw).unwrap()
        ));
        let joined = lines(&it, 60)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.contains('\u{7}'), "a BEL reached the frame");
        assert!(!joined.contains('\u{1b}'), "an ESC reached the frame");
        assert!(!joined.contains('\t'), "a tab reached the frame");
        assert!(joined.contains("a b [31mc d"), "{joined}");
    }

    /// A one-column pane is a degenerate area, not a panic.
    #[test]
    fn a_sliver_of_a_pane_still_produces_lines() {
        let it = item(r#"{"kind":"pr","section":"mine","ref":"a#1","url":"u","title":"t","body":"x"}"#);
        for cols in [1usize, 2, 3, 11] {
            let rows = lines(&it, cols);
            assert!(!rows.is_empty());
        }
    }
}
