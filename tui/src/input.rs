//! The keymap.
//!
//! The contract's keymap, reproduced so it does not have to be looked up. It is
//! confirmed by the user and is not open for reinterpretation:
//!
//! ```text
//! nav mode
//!   j / k            move in the list / within a kanban column
//!   h / l            previous / next section (list) or column (kanban)
//!   ctrl-d / ctrl-u  half page in the LIST
//!   enter / space    -> preview mode
//!   o                open in browser            THEN EXIT
//!   y                copy link                  stay open, transient message
//!   a                put it in the agent input  THEN EXIT (does not submit)
//!   v                toggle list <-> kanban
//!   Tab              cycle boards, kanban only
//!   r                refresh
//!   /                search / filter
//!   q / esc          quit
//!
//! preview mode (entered with enter or space)
//!   j / k            scroll the preview
//!   ctrl-d / ctrl-u  half page
//!   g / G            top / bottom
//!   esc              back to nav
//!
//! aliases, kept because they are in the user's fingers from phase 1
//!   ctrl-o           = a  (agent)
//!   ctrl-y           = y  (copy)
//! ```
//!
//! Only `y` stays open. `o` and `a` exit, matching phase 1 and the user's
//! explicit choice.
//!
//! **The modes are strict.** `o`, `y`, `a`, `v`, `Tab`, `r`, `/` and `q` are nav
//! keys and fire in nav mode only; in preview mode the live keys are j/k,
//! ctrl-d/u, g/G and esc, and nothing else -- `q` does not quit from preview, it
//! is simply unbound there. Extending a confirmed keymap "helpfully" is how a
//! popup ends up with two ways to do everything and no way to predict either.
//!
//! In `Mode::Search` every printable key is text: the nav keys must NOT fire, or
//! typing "review" would trigger a refresh on the first keystroke -- the exact
//! trap phase 1 documents around its `NAV_KEYS` unbind list.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Mode, View};

/// What a key asked for that `app` cannot do by itself. Everything that spawns a
/// process or exits the app comes back as one of these; pure cursor movement
/// mutates `App` in place and returns `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `o` -- `/usr/bin/open <url>`, then exit.
    OpenInBrowser,
    /// `y` / `ctrl-y` -- `copy-link.sh <url>`, stay open, flash the result.
    CopyLink,
    /// `a` / `ctrl-o` -- resolve a target agent and `herdr pane send-text`, then
    /// exit. The text is parked in the composer, NOT submitted. May first enter
    /// `Mode::AgentPicker`.
    HandOffToAgent,
    /// enter in `Mode::AgentPicker` -- hand off to the highlighted agent.
    SubmitAgent,
    /// `r` -- spawn `collect.sh` detached; the tick picks up the new file.
    Refresh,
    /// `q` / `esc` in nav.
    Quit,
}

/// Translate one key press in the app's current mode. Mutates cursors and modes
/// directly; returns an `Action` only for the side-effecting keys.
pub fn handle(app: &mut App, key: KeyEvent) -> Option<Action> {
    match app.mode {
        Mode::Nav => nav(app, key),
        Mode::Preview => preview(app, key),
        Mode::Search => search(app, key),
        Mode::AgentPicker => picker(app, key),
        Mode::Config => config(app, key),
    }
}

fn ctrl(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
}

fn nav(app: &mut App, key: KeyEvent) -> Option<Action> {
    // The half-page is measured against what the renderer last drew, so it is a
    // real page rather than a guessed constant. Before the first frame it is 0,
    // and `max(1)` keeps the key from becoming a no-op.
    let half = (app.list_height / 2).max(1) as isize;
    match key.code {
        KeyCode::Char('d') if ctrl(&key) => app.move_cursor(half),
        KeyCode::Char('u') if ctrl(&key) => app.move_cursor(-half),
        // ctrl-o and ctrl-y are the phase 1 aliases; they must be checked before
        // the bare-letter arms or ctrl-y would fall through to `y`'s own arm
        // (harmlessly here, since they mean the same thing -- but ctrl-o would
        // NOT, and would read as `o`).
        KeyCode::Char('o') if ctrl(&key) => return Some(Action::HandOffToAgent),
        KeyCode::Char('y') if ctrl(&key) => return Some(Action::CopyLink),
        // A bare ctrl-<anything else> is deliberately inert.
        _ if ctrl(&key) => {}

        KeyCode::Char('j') | KeyCode::Down => app.move_cursor(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_cursor(-1),
        KeyCode::Char('h') | KeyCode::Left => match app.view {
            View::List => app.move_section(-1),
            View::Kanban => app.move_column(-1),
        },
        KeyCode::Char('l') | KeyCode::Right => match app.view {
            View::List => app.move_section(1),
            View::Kanban => app.move_column(1),
        },
        KeyCode::Enter | KeyCode::Char(' ') => {
            // Entering preview on nothing would give a mode with no content and
            // no obvious way back other than esc.
            if app.selected().is_some() {
                app.mode = Mode::Preview;
                app.preview_scroll = 0;
            }
        }
        KeyCode::Char('o') => return Some(Action::OpenInBrowser),
        KeyCode::Char('y') => return Some(Action::CopyLink),
        KeyCode::Char('a') => return Some(Action::HandOffToAgent),
        KeyCode::Char('v') => app.toggle_view(),
        // Match on BackTab itself and ignore the modifier: crossterm may or may
        // not attach SHIFT to it depending on the terminal's encoding.
        KeyCode::Tab => app.cycle_board(),
        KeyCode::BackTab => app.cycle_board_back(),
        KeyCode::Char('r') => return Some(Action::Refresh),
        KeyCode::Char('c') => {
            app.mode = Mode::Config;
            app.cfg_cursor = 0;
            // Land on a togglable row rather than the group header at index 0.
            app.config_move(1);
        }
        KeyCode::Char('/') => {
            app.mode = Mode::Search;
            app.query.clear();
        }
        KeyCode::Char('q') | KeyCode::Esc => return Some(Action::Quit),
        _ => {}
    }
    None
}

fn preview(app: &mut App, key: KeyEvent) -> Option<Action> {
    let page = (app.preview_height / 2).max(1);
    let max = app.preview_lines.saturating_sub(app.preview_height.max(1));
    match key.code {
        KeyCode::Char('d') if ctrl(&key) => app.preview_scroll = (app.preview_scroll + page).min(max),
        KeyCode::Char('u') if ctrl(&key) => app.preview_scroll = app.preview_scroll.saturating_sub(page),
        _ if ctrl(&key) => {}
        KeyCode::Char('j') | KeyCode::Down => {
            app.preview_scroll = (app.preview_scroll + 1).min(max)
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.preview_scroll = app.preview_scroll.saturating_sub(1)
        }
        KeyCode::Char('g') => app.preview_scroll = 0,
        KeyCode::Char('G') => app.preview_scroll = max,
        KeyCode::Esc => app.mode = Mode::Nav,
        _ => {}
    }
    None
}

/// Every arm that changes the query ends with `clamp()` **and**
/// `refocus_after_filter()`: clamping alone leaves the cursor in a section the
/// filter just emptied, which strands the selection somewhere the movement keys
/// cannot reach (see [`App::refocus_after_filter`]).
fn search(app: &mut App, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Esc => {
            // "esc clears and returns to nav" -- the filter goes with it.
            app.query.clear();
            app.mode = Mode::Nav;
            app.clamp();
            app.refocus_after_filter();
        }
        KeyCode::Enter => {
            // Applying the filter and going back to nav, where the movement keys
            // work again. The query stays visible in its own line so it is never
            // a mystery why the list is short.
            app.mode = Mode::Nav;
            app.clamp();
            app.refocus_after_filter();
        }
        KeyCode::Backspace => {
            app.query.pop();
            app.cursor = 0;
            app.clamp();
            app.refocus_after_filter();
        }
        // ctrl-u clears the line, as it does in every readline on the machine.
        KeyCode::Char('u') if ctrl(&key) => {
            app.query.clear();
            app.cursor = 0;
            app.clamp();
            app.refocus_after_filter();
        }
        _ if ctrl(&key) => {}
        KeyCode::Char(c) => {
            // `Item::matches` takes an already-lowercased needle, so the query is
            // stored lowercased and the fold happens once per keystroke rather
            // than once per item per frame.
            app.query.extend(c.to_lowercase());
            app.cursor = 0;
            app.clamp();
            app.refocus_after_filter();
        }
        _ => {}
    }
    None
}

fn picker(app: &mut App, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if app.agent_cursor + 1 < app.agents.len() {
                app.agent_cursor += 1;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => app.agent_cursor = app.agent_cursor.saturating_sub(1),
        KeyCode::Enter => return Some(Action::SubmitAgent),
        KeyCode::Esc | KeyCode::Char('q') => {
            app.mode = Mode::Nav;
            app.agents.clear();
        }
        _ => {}
    }
    None
}

/// The config screen. `space` toggles, `A` shows everything again, `esc`/`c`/`q`
/// closes. Every change is already on disk by the time you leave -- there is no
/// save key and no way to lose an edit by closing the wrong way.
fn config(app: &mut App, key: KeyEvent) -> Option<Action> {
    match key.code {
        KeyCode::Char('j') | KeyCode::Down => app.config_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.config_move(-1),
        KeyCode::Char(' ') | KeyCode::Enter => app.config_toggle(),
        KeyCode::Char('A') => app.config_show_all(),
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') => {
            app.mode = Mode::Nav;
            // The filter may have emptied the section the cursor was in.
            app.clamp();
            app.refocus_after_filter();
        }
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::Loaded;
    use crate::model::{Board, Cache, Section};
    use std::path::PathBuf;

    fn app() -> App {
        let cache: Cache = serde_json::from_str(
            r#"{"version":1,"fetched_unix":1000,"items":[
              {"kind":"pr","section":"review","ref":"a/b#1","url":"u1","title":"alpha"},
              {"kind":"pr","section":"review","ref":"a/b#2","url":"u2","title":"beta"},
              {"kind":"pr","section":"mine","ref":"a/b#3","url":"u3","title":"gamma","draft":true}
            ]}"#,
        )
        .unwrap();
        let mut a = App::with_config_default(PathBuf::from("/nonexistent/cache.json"));
        a.loaded = Ok(Loaded {
            path: PathBuf::from("/nonexistent/cache.json"),
            cache,
            mtime: None,
        });
        a.section = Section::Review;
        a.list_height = 10;
        a
    }

    fn k(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn kc(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn nav_movement_and_actions() {
        let mut a = app();
        assert_eq!(handle(&mut a, k('j')), None);
        assert_eq!(a.cursor, 1);
        handle(&mut a, k('k'));
        assert_eq!(a.cursor, 0);
        assert_eq!(handle(&mut a, k('o')), Some(Action::OpenInBrowser));
        assert_eq!(handle(&mut a, k('y')), Some(Action::CopyLink));
        assert_eq!(handle(&mut a, k('a')), Some(Action::HandOffToAgent));
        assert_eq!(handle(&mut a, k('r')), Some(Action::Refresh));
        assert_eq!(handle(&mut a, k('q')), Some(Action::Quit));
        assert_eq!(handle(&mut a, code(KeyCode::Esc)), Some(Action::Quit));
    }

    #[test]
    fn phase1_aliases_still_work_and_do_not_collide() {
        let mut a = app();
        assert_eq!(handle(&mut a, kc('o')), Some(Action::HandOffToAgent));
        assert_eq!(handle(&mut a, kc('y')), Some(Action::CopyLink));
    }

    #[test]
    fn enter_and_space_open_preview_only_when_something_is_selected() {
        let mut a = app();
        handle(&mut a, code(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Preview);
        handle(&mut a, code(KeyCode::Esc));
        assert_eq!(a.mode, Mode::Nav);
        handle(&mut a, k(' '));
        assert_eq!(a.mode, Mode::Preview);
        handle(&mut a, code(KeyCode::Esc));

        a.query = "zzzz".into();
        a.clamp();
        handle(&mut a, code(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Nav, "nothing selected -> no preview mode");
    }

    /// The keymap is modal and strict: in preview, only the scroll keys and esc
    /// do anything. `q` in particular must NOT quit from here.
    #[test]
    fn preview_mode_is_strict() {
        let mut a = app();
        a.mode = Mode::Preview;
        a.preview_height = 10;
        a.preview_lines = 100;

        assert_eq!(handle(&mut a, k('q')), None);
        assert_eq!(a.mode, Mode::Preview);
        assert_eq!(handle(&mut a, k('o')), None);
        assert_eq!(handle(&mut a, k('y')), None);
        assert_eq!(handle(&mut a, k('a')), None);
        assert_eq!(handle(&mut a, k('r')), None);
        assert_eq!(handle(&mut a, k('v')), None);
        assert_eq!(a.view, View::List);

        handle(&mut a, k('j'));
        assert_eq!(a.preview_scroll, 1);
        handle(&mut a, k('k'));
        assert_eq!(a.preview_scroll, 0);
        handle(&mut a, kc('d'));
        assert_eq!(a.preview_scroll, 5);
        handle(&mut a, kc('u'));
        assert_eq!(a.preview_scroll, 0);
        handle(&mut a, k('G'));
        assert_eq!(a.preview_scroll, 90);
        handle(&mut a, k('g'));
        assert_eq!(a.preview_scroll, 0);
        handle(&mut a, code(KeyCode::Esc));
        assert_eq!(a.mode, Mode::Nav);
    }

    /// The phase 1 trap: typing "review" must not fire r/e/v/i/e/w as commands.
    #[test]
    fn search_swallows_the_nav_keys() {
        let mut a = app();
        handle(&mut a, k('/'));
        assert_eq!(a.mode, Mode::Search);
        for c in "review".chars() {
            assert_eq!(handle(&mut a, k(c)), None);
        }
        assert_eq!(a.query, "review");
        assert_eq!(a.mode, Mode::Search);
        // uppercase is folded on the way in
        handle(&mut a, k('A'));
        assert_eq!(a.query, "reviewa");
        handle(&mut a, code(KeyCode::Backspace));
        assert_eq!(a.query, "review");
        // esc clears and returns
        handle(&mut a, code(KeyCode::Esc));
        assert_eq!(a.mode, Mode::Nav);
        assert!(a.query.is_empty());
    }

    #[test]
    fn search_enter_keeps_the_filter_and_returns_to_nav() {
        let mut a = app();
        handle(&mut a, k('/'));
        for c in "beta".chars() {
            handle(&mut a, k(c));
        }
        handle(&mut a, code(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Nav);
        assert_eq!(a.query, "beta");
        assert_eq!(a.section_items(&Section::Review).len(), 1);
    }

    /// The half-page is measured against the height the renderer last drew, and
    /// it pages through the FLAT list: 2 review rows then 1 mine row, so a
    /// half-page of 2 from the top lands on the mine row rather than stopping at
    /// the section boundary.
    #[test]
    fn half_page_uses_the_height_the_renderer_measured() {
        let mut a = app();
        a.list_height = 4;
        handle(&mut a, kc('d'));
        assert_eq!((a.section.clone(), a.cursor), (Section::Mine, 0));
        handle(&mut a, kc('u'));
        assert_eq!((a.section.clone(), a.cursor), (Section::Review, 0));
        // a height of 0 (before the first frame) must not divide to a no-op
        a.list_height = 0;
        handle(&mut a, kc('d'));
        assert_eq!((a.section.clone(), a.cursor), (Section::Review, 1));
        // and the end of the last section clamps rather than wrapping
        handle(&mut a, kc('d'));
        handle(&mut a, kc('d'));
        handle(&mut a, kc('d'));
        assert_eq!((a.section.clone(), a.cursor), (Section::Mine, 0));
    }

    /// Typing a filter must leave the cursor ON the match. Without it the only
    /// row on screen sits under a section the movement keys cannot reach.
    #[test]
    fn a_typed_filter_lands_the_cursor_on_the_match() {
        let mut a = app();
        a.section = Section::Review;
        handle(&mut a, k('/'));
        for c in "gamma".chars() {
            handle(&mut a, k(c));
        }
        assert_eq!(a.section, Section::Mine, "the focus follows the match as you type");
        handle(&mut a, code(KeyCode::Enter));
        assert_eq!(a.section, Section::Mine);
        assert_eq!(a.selected().map(|i| i.r#ref.clone()).as_deref(), Some("a/b#3"));
    }

    #[test]
    fn tab_cycles_boards_in_kanban_only() {
        let mut a = app();
        handle(&mut a, k('v'));
        assert_eq!(a.view, View::Kanban);
        assert_eq!(a.board, Board::Review);
        handle(&mut a, code(KeyCode::Tab));
        assert_eq!(a.board, Board::Mine);
        handle(&mut a, code(KeyCode::Tab));
        assert_eq!(a.board, Board::Jira);
        handle(&mut a, code(KeyCode::Tab));
        assert_eq!(a.board, Board::Review);
        // h/l move columns here, not sections
        handle(&mut a, k('l'));
        assert_eq!(a.column, 1);
        handle(&mut a, k('h'));
        assert_eq!(a.column, 0);
    }

    #[test]
    fn the_picker_is_its_own_mode() {
        let mut a = app();
        a.mode = Mode::AgentPicker;
        a.agents = vec![
            crate::actions::Agent {
                pane_id: "p1".into(),
                agent: "claude".into(),
                agent_status: "idle".into(),
                workspace_id: "w".into(),
                terminal_title: "one".into(),
                focused: false,
            },
            crate::actions::Agent {
                pane_id: "p2".into(),
                agent: "claude".into(),
                agent_status: "busy".into(),
                workspace_id: "w".into(),
                terminal_title: "two".into(),
                focused: false,
            },
        ];
        handle(&mut a, k('j'));
        assert_eq!(a.agent_cursor, 1);
        handle(&mut a, k('j'));
        assert_eq!(a.agent_cursor, 1, "must not run off the end");
        handle(&mut a, k('k'));
        assert_eq!(a.agent_cursor, 0);
        assert_eq!(handle(&mut a, code(KeyCode::Enter)), Some(Action::SubmitAgent));
        handle(&mut a, code(KeyCode::Esc));
        assert_eq!(a.mode, Mode::Nav);
        assert!(a.agents.is_empty());
    }
}
