//! Markdown, drawn.
//!
//! PR descriptions are GitHub-flavoured markdown, and the ADF-to-text converter
//! in `lib/common.sh` already emits the same shapes for Jira (`#` headings,
//! `  - ` bullets, ``` fences), so one renderer covers both bodies.
//!
//! # Why not `Paragraph`'s own wrapping, or a markdown widget crate
//!
//! The preview is scrollable, and `g`/`G`/`ctrl-d` need an exact line count to
//! clamp against. That means the wrapping has to happen here, in display
//! columns, before the lines reach the frame -- which is most of what a markdown
//! widget would have done. What is left is a `pulldown-cmark` event loop, and
//! doing it here keeps the palette the same `C_*` constants everything else uses.
//!
//! # What it renders, and what it deliberately does not
//!
//! Headings, bold, italic, strikethrough, inline code, fenced and indented code
//! blocks, bullet and ordered lists (nested, with real indentation), task list
//! checkboxes, block quotes, thematic breaks, tables, and links.
//!
//! **No syntax highlighting.** It would mean a second parser and a theme to keep
//! in sync with the terminal's own, for text that is read in a 40-column pane.
//! Code blocks get one colour and a gutter instead.
//!
//! **No OSC 8 hyperlinks.** ratatui's buffer is a grid of styled cells and has
//! nowhere to carry the escape, so a link renders as its text plus the URL in
//! `C_REF` -- which is also what a reader on the far end of `herdr --remote`
//! can act on, since `y` copies the item URL anyway.

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::view::{C_DIM, C_GOOD, C_HEADER, C_REF, C_WARN, char_width, width};

/// Gutter drawn down the left of a code block.
const CODE_GUTTER: &str = "│ ";
/// Bullets by nesting depth. Cycles rather than running out.
const BULLETS: [&str; 3] = ["•", "◦", "▪"];

/// Render `src` into lines that are at most `cols` display columns wide.
///
/// Never panics and never returns nothing for non-empty input: anything the
/// parser does not recognise falls through as its own text, which is the same
/// thing the old plain-text preview would have shown.
pub fn render(src: &str, cols: usize) -> Vec<Line<'static>> {
    if cols == 0 {
        return Vec::new();
    }
    let mut opts = Options::empty();
    // GFM. Task lists are not hypothetical here -- the PR bodies in this cache
    // are full of `- [x] 仕様書更新`.
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_FOOTNOTES);

    let mut r = Renderer::new(cols);
    for ev in Parser::new_ext(src, opts) {
        r.event(ev);
    }
    r.finish()
}

/// One inline run: text plus the style it inherited from the tags around it.
#[derive(Clone)]
struct Run {
    text: String,
    style: Style,
}

struct Renderer {
    cols: usize,
    out: Vec<Line<'static>>,
    /// The paragraph being accumulated, as styled runs, before it is wrapped.
    runs: Vec<Run>,
    /// Inline style stack: bold/italic/code nest.
    style: Style,
    /// Indent applied to every line of the current block, in columns.
    indent: usize,
    /// The prefix for the FIRST line of the current block (a bullet, a number),
    /// drawn once and replaced by spaces on continuation lines.
    marker: Option<String>,
    /// Ordered-list counters, one per nesting level; `None` for a bullet list.
    lists: Vec<Option<u64>>,
    in_code_block: bool,
    /// Quote depth, for the `▎` gutter.
    quote: usize,
    /// Cells of the table row being built, and whether it is the header row.
    table: Option<Table>,
    /// The destination of the link currently open, appended when it closes.
    link: Option<String>,
}

struct Table {
    rows: Vec<Vec<String>>,
    head: bool,
    cell: String,
}

impl Renderer {
    fn new(cols: usize) -> Self {
        Self {
            cols,
            out: Vec::new(),
            runs: Vec::new(),
            style: Style::default(),
            indent: 0,
            marker: None,
            lists: Vec::new(),
            in_code_block: false,
            quote: 0,
            table: None,
            link: None,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        // A trailing blank line is an artefact of block separation, not content.
        while self
            .out
            .last()
            .is_some_and(|l| l.width() == 0 || l.to_string().trim().is_empty())
        {
            self.out.pop();
        }
        self.out
    }

    fn blank(&mut self) {
        if !self.out.is_empty() && self.out.last().is_some_and(|l| l.width() > 0) {
            self.out.push(Line::raw(""));
        }
    }

    fn push_text(&mut self, s: &str) {
        if let Some(t) = self.table.as_mut() {
            t.cell.push_str(s);
            return;
        }
        self.runs.push(Run {
            text: s.to_string(),
            style: self.style,
        });
    }

    /// Wrap the accumulated runs and emit them, preserving each run's style
    /// across the wrap points.
    ///
    /// This is the only place a line is built, so the indent, the marker and the
    /// quote gutter are applied in exactly one place for every block type.
    fn flush(&mut self) {
        if self.runs.is_empty() {
            return;
        }
        let runs = std::mem::take(&mut self.runs);
        let quote_w = self.quote * 2;
        let total_indent = self.indent + quote_w;
        let avail = self.cols.saturating_sub(total_indent).max(1);

        let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut w = 0usize;
        for run in runs {
            // Split on whitespace but keep it: a break happens AT a space, and
            // the space is dropped, while a run with no spaces (a URL, CJK) is
            // hard-broken at the column.
            for token in split_keep(&run.text) {
                if token == "\n" {
                    lines.push(Vec::new());
                    w = 0;
                    continue;
                }
                let tw = width(token);
                if tw > avail && token.trim().is_empty() {
                    continue;
                }
                if w + tw > avail && w > 0 {
                    lines.push(Vec::new());
                    w = 0;
                    if token.trim().is_empty() {
                        continue;
                    }
                }
                if tw > avail {
                    // Longer than the whole width: hard-break it.
                    for chunk in hard_break(token, avail) {
                        let cw = width(&chunk);
                        if w + cw > avail && w > 0 {
                            lines.push(Vec::new());
                            w = 0;
                        }
                        lines.last_mut().unwrap().push(Span::styled(chunk, run.style));
                        w += cw;
                    }
                    continue;
                }
                lines
                    .last_mut()
                    .unwrap()
                    .push(Span::styled(token.to_string(), run.style));
                w += tw;
            }
        }

        let marker = self.marker.take();
        for (i, mut spans) in lines.into_iter().enumerate() {
            if spans.is_empty() && i > 0 {
                continue;
            }
            let mut head: Vec<Span<'static>> = Vec::new();
            for _ in 0..self.quote {
                head.push(Span::styled("▎ ", Style::default().fg(C_DIM)));
            }
            // The marker sits at the END of the indent, not the start of it:
            // a nested bullet has to begin further right than its parent, and
            // padding after the marker instead would put every level's bullet
            // in column 0.
            let prefix = match (&marker, i) {
                (Some(m), 0) => {
                    let pad = self.indent.saturating_sub(width(m));
                    format!("{}{m}", " ".repeat(pad))
                }
                _ => " ".repeat(self.indent),
            };
            if !prefix.is_empty() {
                head.push(Span::styled(prefix, Style::default().fg(C_DIM)));
            }
            head.append(&mut spans);
            self.out.push(Line::from(head));
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if self.in_code_block {
                    self.code_lines(&t);
                } else {
                    self.push_text(&t);
                }
            }
            Event::Code(t) => {
                let saved = self.style;
                self.style = Style::default().fg(C_WARN);
                self.push_text(&format!("`{t}`"));
                self.style = saved;
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.push_text("\n"),
            Event::Rule => {
                self.flush();
                self.blank();
                self.out.push(Line::styled(
                    "─".repeat(self.cols),
                    Style::default().fg(C_DIM),
                ));
                self.blank();
            }
            Event::TaskListMarker(done) => {
                let (mark, colour) = if done {
                    ("[x] ", C_GOOD)
                } else {
                    ("[ ] ", C_DIM)
                };
                let saved = self.style;
                self.style = Style::default().fg(colour);
                self.push_text(mark);
                self.style = saved;
            }
            // Raw HTML in a PR body is usually a <details> wrapper or a comment;
            // showing the tag is noise, so only its inner text survives (which
            // arrives as ordinary Text events).
            Event::Html(_) | Event::InlineHtml(_) => {}
            Event::FootnoteReference(name) => {
                let saved = self.style;
                self.style = Style::default().fg(C_DIM);
                self.push_text(&format!("[^{name}]"));
                self.style = saved;
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => self.push_text(&t),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush();
                self.blank();
            }
            Tag::Heading { level, .. } => {
                self.flush();
                self.blank();
                let n = heading_depth(level);
                self.style = Style::default()
                    .fg(C_HEADER)
                    .add_modifier(Modifier::BOLD);
                // The `#`s are kept: they are how a reader tells h2 from h3 when
                // the only other signal would be a font size the terminal cannot
                // give.
                self.push_text(&format!("{} ", "#".repeat(n)));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.blank();
                self.quote += 1;
            }
            Tag::CodeBlock(_) => {
                self.flush();
                self.blank();
                self.in_code_block = true;
            }
            Tag::List(start) => {
                self.flush();
                if self.lists.is_empty() {
                    self.blank();
                }
                self.lists.push(start);
                self.indent += 2;
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => format!("{} ", BULLETS[depth % BULLETS.len()]),
                };
                self.marker = Some(marker);
            }
            Tag::Emphasis => self.style = self.style.add_modifier(Modifier::ITALIC),
            Tag::Strong => self.style = self.style.add_modifier(Modifier::BOLD),
            Tag::Strikethrough => self.style = self.style.add_modifier(Modifier::CROSSED_OUT),
            Tag::Link { dest_url, .. } => {
                self.style = self.style.fg(C_REF);
                self.link = Some(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                self.push_text(&format!("[image: {dest_url}]"));
            }
            Tag::Table(_) => {
                self.flush();
                self.blank();
                self.table = Some(Table {
                    rows: Vec::new(),
                    head: false,
                    cell: String::new(),
                });
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.head = true;
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.cell.clear();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::Heading(_) => {
                self.flush();
                self.style = Style::default();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.blank();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.indent = self.indent.saturating_sub(2);
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => self.flush(),
            TagEnd::Emphasis => self.style = self.style.remove_modifier(Modifier::ITALIC),
            TagEnd::Strong => self.style = self.style.remove_modifier(Modifier::BOLD),
            TagEnd::Strikethrough => {
                self.style = self.style.remove_modifier(Modifier::CROSSED_OUT)
            }
            TagEnd::Link => {
                // The URL after the text, dimmed. A terminal cell cannot carry an
                // OSC 8 hyperlink through ratatui's buffer, so the address has to
                // be visible to be usable -- and it is skipped when it is the
                // same as the text, which is what a bare autolink produces.
                if let Some(url) = self.link.take() {
                    let shown: String = self.runs.iter().map(|r| r.text.as_str()).collect();
                    if !shown.trim_end().ends_with(url.as_str()) {
                        let saved = self.style;
                        self.style = Style::default().fg(C_DIM);
                        self.push_text(&format!(" <{url}>"));
                        self.style = saved;
                    }
                }
                self.style = Style::default();
            }
            TagEnd::Table => self.draw_table(),
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    let cell = std::mem::take(&mut t.cell);
                    if let Some(row) = t.rows.last_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.head = false;
                }
            }
            _ => {}
        }
    }

    /// A code block, one line at a time, with a gutter and no wrapping: code
    /// that is wrapped is code that cannot be read, so a long line is truncated
    /// and the pane scrolls instead.
    fn code_lines(&mut self, text: &str) {
        let avail = self
            .cols
            .saturating_sub(self.indent + width(CODE_GUTTER))
            .max(1);
        for raw in text.split('\n') {
            if raw.is_empty() && text.ends_with('\n') && raw.as_ptr() == text.as_ptr() {
                continue;
            }
            let body = crate::view::fit(raw, avail);
            self.out.push(Line::from(vec![
                Span::raw(" ".repeat(self.indent)),
                Span::styled(CODE_GUTTER, Style::default().fg(C_DIM)),
                Span::styled(body, Style::default().fg(C_WARN)),
            ]));
        }
        // A fenced block ends with a newline, which split leaves as a trailing
        // empty piece; drop it so the block does not gain a blank last row.
        if text.ends_with('\n') {
            self.out.pop();
        }
    }

    /// Tables, laid out to the real column widths and truncated to fit the pane.
    fn draw_table(&mut self) {
        let Some(t) = self.table.take() else { return };
        let rows: Vec<Vec<String>> = t.rows.into_iter().filter(|r| !r.is_empty()).collect();
        if rows.is_empty() {
            return;
        }
        let n = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut w = vec![0usize; n];
        for r in &rows {
            for (i, c) in r.iter().enumerate() {
                w[i] = w[i].max(width(c.trim()));
            }
        }
        // Shrink proportionally if the natural widths overflow the pane.
        let sep = 3 * n.saturating_sub(1);
        let total: usize = w.iter().sum::<usize>() + sep;
        if total > self.cols && n > 0 {
            let budget = self.cols.saturating_sub(sep).max(n);
            let scale = budget as f64 / w.iter().sum::<usize>().max(1) as f64;
            for x in w.iter_mut() {
                *x = ((*x as f64) * scale).floor().max(3.0) as usize;
            }
        }
        for (ri, r) in rows.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for i in 0..n {
                if i > 0 {
                    spans.push(Span::styled(" │ ", Style::default().fg(C_DIM)));
                }
                let cell = r.get(i).map(|s| s.trim()).unwrap_or("");
                let style = if ri == 0 {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                spans.push(Span::styled(crate::view::pad_fit(cell, w[i]), style));
            }
            self.out.push(Line::from(spans));
            if ri == 0 {
                let rule: String = w
                    .iter()
                    .map(|x| "─".repeat(*x))
                    .collect::<Vec<_>>()
                    .join("─┼─");
                self.out
                    .push(Line::styled(rule, Style::default().fg(C_DIM)));
            }
        }
        self.blank();
    }
}

fn heading_depth(l: HeadingLevel) -> usize {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Split into wrappable tokens, keeping the spaces as their own tokens so a
/// break can drop one. Newlines come through as their own `"\n"` token.
fn split_keep(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_ws = None::<bool>;
    for (i, c) in s.char_indices() {
        if c == '\n' {
            if i > start {
                out.push(&s[start..i]);
            }
            out.push("\n");
            start = i + c.len_utf8();
            in_ws = None;
            continue;
        }
        let ws = c.is_whitespace();
        match in_ws {
            None => in_ws = Some(ws),
            Some(prev) if prev != ws => {
                out.push(&s[start..i]);
                start = i;
                in_ws = Some(ws);
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Break a token longer than the whole line into column-sized chunks, never
/// splitting a character and never letting a double-width glyph straddle.
fn hard_break(s: &str, cols: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut w = 0usize;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > cols && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            w = 0;
        }
        cur.push(c);
        w += cw;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(src: &str, cols: usize) -> Vec<String> {
        render(src, cols).iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn nothing_is_ever_wider_than_the_pane() {
        let src = "# A very long heading that will certainly not fit in a narrow pane\n\n\
                   Some prose with a https://example.com/a/very/long/url/that/cannot/be/broken/at/a/space in it.\n\n\
                   - 日本語の箇条書きで、折り返しが必要になるくらい長い行を書いてみます\n\
                     - ネストした項目\n\n\
                   ```\nlet x = a_very_long_line_of_code_that_should_be_truncated_not_wrapped();\n```\n\n\
                   | col a | col b |\n|---|---|\n| 1 | 2 |\n";
        for cols in [20usize, 30, 40, 60, 80] {
            for l in render(src, cols) {
                assert!(
                    l.width() <= cols,
                    "{} cols: {:?} is {}",
                    cols,
                    l.to_string(),
                    l.width()
                );
            }
        }
    }

    #[test]
    fn headings_keep_their_level() {
        let out = plain("# one\n\n## two\n", 40);
        assert!(out.iter().any(|l| l.starts_with("# one")), "{out:?}");
        assert!(out.iter().any(|l| l.starts_with("## two")), "{out:?}");
    }

    #[test]
    fn task_lists_render_their_boxes() {
        let out = plain("- [x] done\n- [ ] not done\n", 40);
        assert!(out.iter().any(|l| l.contains("[x] done")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("[ ] not done")), "{out:?}");
    }

    #[test]
    fn lists_are_bulleted_and_nest() {
        let out = plain("- a\n  - b\n- c\n", 40);
        assert!(out.iter().any(|l| l.contains("• a")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("◦ b")), "{out:?}");
        // the nested item is indented further than its parent
        let a = out.iter().position(|l| l.contains("• a")).unwrap();
        let b = out.iter().position(|l| l.contains("◦ b")).unwrap();
        let ind = |s: &str| s.len() - s.trim_start().len();
        assert!(ind(&out[b]) > ind(&out[a]), "{out:?}");
    }

    #[test]
    fn ordered_lists_count() {
        let out = plain("1. one\n2. two\n3. three\n", 40);
        assert!(out.iter().any(|l| l.contains("1. one")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("3. three")), "{out:?}");
    }

    #[test]
    fn code_blocks_get_a_gutter_and_are_not_wrapped() {
        let out = plain("```rust\nfn main() {}\n```\n", 40);
        assert!(out.iter().any(|l| l.contains("│ fn main() {}")), "{out:?}");
    }

    #[test]
    fn a_link_shows_its_text_and_its_url() {
        let out = plain("see [the issue](https://x.test/1) please", 60);
        let joined = out.join(" ");
        assert!(joined.contains("the issue"), "{out:?}");
        assert!(joined.contains("https://x.test/1"), "{out:?}");
    }

    #[test]
    fn block_quotes_get_a_gutter() {
        let out = plain("> quoted\n", 40);
        assert!(out.iter().any(|l| l.contains('▎')), "{out:?}");
    }

    /// The bodies in this cache are Japanese and Korean; a wrap measured in
    /// characters would overflow every one of them.
    #[test]
    fn wide_text_wraps_by_columns() {
        let src = "日本語のテキストはスペースがないので、折り返しは文字幅で測る必要があります。";
        for l in render(src, 20) {
            assert!(l.width() <= 20, "{:?} is {}", l.to_string(), l.width());
        }
    }

    #[test]
    fn empty_and_degenerate_input_is_safe() {
        assert!(render("", 40).is_empty());
        assert!(render("text", 0).is_empty());
        // no panic on a bare fence, an unclosed emphasis, a lone pipe
        for src in ["```", "*unclosed", "|", "- ", "#", ">"] {
            let _ = render(src, 10);
        }
    }

    /// Jira bodies arrive from the ADF walker in `lib/common.sh`, which emits
    /// `#` headings, `  - ` bullets and fenced code -- the same renderer has to
    /// cover them.
    #[test]
    fn it_renders_the_adf_walkers_output_shape() {
        let src = "# 概要\n\nテキスト <https://x.test>\n\n  - 項目1\n  - 項目2\n\n```\ncode\n```\n";
        let out = plain(src, 40);
        assert!(out.iter().any(|l| l.starts_with("# 概要")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("項目1")), "{out:?}");
        assert!(out.iter().any(|l| l.contains("│ code")), "{out:?}");
    }
}
