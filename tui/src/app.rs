//! Application state and the event loop.
//!
//! The single mutable `App` the whole TUI reads: which view, which mode, where
//! the cursor is, the search query, the transient message, and the loaded cache.
//! `input` turns a key into a mutation of this struct, `view/*` renders it, and
//! `actions` runs the side effects.
//!
//! # The loop
//!
//! ```text
//! load cache (or an error state -- never a panic)
//! spawn one detached collect, as ui.sh does today
//! loop:
//!   draw
//!   poll for a key event with a 250ms timeout      <- the tick
//!   on timeout (or once per 250ms anyway):
//!     if the file on disk moved -> reload, clamp the cursors
//!     reap the collect child, spin the spinner, expire the transient message
//!   on key: input::handle -> maybe an Action -> App::act
//! ```
//!
//! The 250ms poll timeout IS the tick; there is no separate timer thread and no
//! filesystem-event crate. `crossterm::event::poll` is the only wait.
//!
//! # Startup budget
//!
//! First frame must beat fzf's 0.063s. Nothing blocks before the first draw
//! except reading and parsing cache.json: the startup collect is spawned
//! **after** the frame, detached, and never waited on.

use std::io;
use std::path::PathBuf;
use std::process::Child;
use std::time::{Duration, Instant, SystemTime};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::Rect;

use crate::actions::{self, Agent};
use crate::cache::{self, LoadError, Loaded};
use crate::config::Config;
use crate::input::{self, Action};
use crate::model::{BOARDS, Board, Cache, Column, Item, SECTIONS, Section};
use crate::view;

/// The poll timeout, and therefore the tick period. 250ms is the contract's
/// suggestion and is well under the threshold where a user notices that a
/// refresh landed late.
pub const TICK: Duration = Duration::from_millis(250);

/// How long a `y` message stays in the header.
const FLASH_FOR: Duration = Duration::from_millis(2500);

const SPINNER: [char; 4] = ['|', '/', '-', '\\'];

/// List or kanban. `v` toggles. Defined in `model` because it is persisted in
/// `config.json`; re-exported here because this is where it is used.
pub use crate::model::View;

/// Which keymap is live. See `input` for the bindings of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Moving around. `enter`/`space` goes to `Preview`, `/` to `Search`.
    Nav,
    /// The preview pane has focus: j/k scroll it, esc returns to `Nav`.
    Preview,
    /// Typing a filter. esc clears the query and returns to `Nav`.
    Search,
    /// The agent picker, shown only when the hand-off cannot resolve a single
    /// target. Drawn by this crate -- phase 2 never shells out to fzf.
    AgentPicker,
    /// The config screen: which repositories and projects this panel shows.
    Config,
}

/// One row of the config screen: a group header, or a togglable source.
///
/// The count beside each name is how many items it contributes **before** the
/// filter, so a hidden source still shows what hiding it costs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigRow {
    Header(&'static str),
    Repo { name: String, shown: bool, n: usize },
    Project { name: String, shown: bool, n: usize },
}

/// A short-lived line in the header, e.g. `copied acme/api#12`. Only `y` uses
/// it; `o` and `a` exit the app instead.
#[derive(Debug, Clone)]
pub struct Flash {
    pub text: String,
    /// Monotonic deadline; the tick clears it when passed.
    pub until: Instant,
}

#[derive(Debug)]
pub struct App {
    /// Where the cache is. Held outside `loaded` on purpose: the **error** arm
    /// has to keep polling too, or a first-ever run (no cache -> error frame ->
    /// `r` -> collector writes one) would never recover. See [`App::poll_cache`].
    pub path: PathBuf,
    pub loaded: Result<Loaded, LoadError>,
    /// mtime last seen while `loaded` is `Err`. Unused on the `Ok` arm, which
    /// carries its own inside `Loaded`.
    err_mtime: Option<SystemTime>,

    /// The user's own preferences: which repos/projects to show, and the view to
    /// come back to. Saved whenever one of them changes, never only at exit --
    /// `o` and `a` exit too.
    pub cfg: Config,
    /// The config screen's cursor, over `config_rows()`.
    pub cfg_cursor: usize,

    pub view: View,
    pub mode: Mode,
    /// Focused section in list view.
    pub section: Section,
    /// Focused board in kanban view; `Tab` cycles.
    pub board: Board,
    /// Focused column within the board; `h`/`l` move.
    pub column: usize,
    /// Cursor within the focused section/column, over the **filtered** items.
    pub cursor: usize,
    /// Scroll offset of the list, in rows. Persisted across frames so the view
    /// does not jump when the cursor has not moved.
    pub list_offset: usize,
    /// Scroll offset of the preview pane, in wrapped lines.
    pub preview_scroll: usize,
    /// Current `/` query, already lowercased for `Item::matches`.
    pub query: String,
    pub flash: Option<Flash>,
    /// True while a spawned `collect.sh` is believed to still be running; drives
    /// the header spinner.
    pub refreshing: bool,
    /// The spawned collector. Detached (all three fds are /dev/null, and it is
    /// never waited on at exit) but still *reaped* here, which is what makes the
    /// spinner honest instead of a guess with a timeout.
    collect: Option<Child>,
    spinner: usize,
    /// Wall clock, refreshed on the tick rather than per draw so the header does
    /// not call `gettimeofday` on every frame.
    pub now: i64,
    last_tick: Instant,

    // -- geometry, written by the view each frame so the paging keys know how
    //    big a page is. The keymap needs it and only the renderer knows it.
    pub list_height: usize,
    pub preview_height: usize,
    pub preview_lines: usize,

    // -- agent hand-off
    pub agents: Vec<Agent>,
    pub agent_cursor: usize,

    /// Set after a child may have written to the tty (copy-link.sh's OSC 52), to
    /// force a full repaint on the next frame.
    pub needs_clear: bool,
    /// Set by `q`, `esc` in nav, and by the actions that exit (`o`, `a`).
    pub quit: bool,
}

impl App {
    /// Load the cache and build the initial state. Must not block on anything
    /// but the file read.
    pub fn new(path: PathBuf) -> Self {
        Self::with_config(path, Config::load())
    }

    /// The real constructor. Split out so tests can pass a `Config::default()`,
    /// which has no backing path and therefore cannot read -- or WRITE -- the
    /// user's own `config.json`. `cargo test` toggling a view must not clobber
    /// the view the user left the popup in.
    /// Test-only: no config file, so nothing on disk is read or written.
    #[cfg(test)]
    pub fn with_config_default(path: PathBuf) -> Self {
        Self::with_config(path, Config::default())
    }

    pub fn with_config(path: PathBuf, cfg: Config) -> Self {
        let loaded = cache::load_from(&path);
        let err_mtime = if loaded.is_err() {
            cache::mtime_of(&path)
        } else {
            None
        };
        let (view, board) = (cfg.view, cfg.board);
        let mut app = Self {
            path,
            loaded,
            err_mtime,
            cfg,
            cfg_cursor: 0,
            view,
            mode: Mode::Nav,
            section: Section::Review,
            board,
            column: 0,
            cursor: 0,
            list_offset: 0,
            preview_scroll: 0,
            query: String::new(),
            flash: None,
            refreshing: false,
            collect: None,
            spinner: 0,
            now: cache::now_unix(),
            last_tick: Instant::now(),
            list_height: 0,
            preview_height: 0,
            preview_lines: 0,
            agents: Vec::new(),
            agent_cursor: 0,
            needs_clear: false,
            quit: false,
        };
        app.focus_first_nonempty();
        app
    }

    /// Start on the first section that actually has rows. Opening on an empty
    /// REVIEW REQUESTED with 21 PRs one section down is the wrong first frame.
    fn focus_first_nonempty(&mut self) {
        for s in &SECTIONS {
            if !self.section_items(s).is_empty() {
                self.section = s.clone();
                return;
            }
        }
        self.section = Section::Review;
    }

    pub fn cache(&self) -> Option<&Cache> {
        self.loaded.as_ref().ok().map(|l| &l.cache)
    }

    // ------------------------------------------------------------- selections

    /// The items of one section, filtered by the current query, in cache order.
    ///
    /// "An item" is [`crate::model::kind_is_known`]'s definition -- the same one
    /// `Cache::count_in` uses, so the header count, the section header count,
    /// `--dump` and the drawn rows can never disagree.
    /// The single chokepoint for BOTH filters -- the `/` query and the config's
    /// hidden repos/projects. Everything downstream (the kanban buckets, the
    /// section counts, the cursor, the scroll anchors, the empty-section
    /// message) reads through here, so neither filter can make a count disagree
    /// with the rows under it.
    pub fn section_items(&self, section: &Section) -> Vec<&Item> {
        match self.cache() {
            None => Vec::new(),
            Some(c) => c
                .known()
                .filter(|i| &i.section == section)
                .filter(|i| self.is_visible(i))
                .filter(|i| i.matches(&self.query))
                .collect(),
        }
    }

    /// Is this item's repo/project allowed by the config? A deny list, so an
    /// item from somewhere nobody has hidden is always shown.
    pub fn is_visible(&self, it: &Item) -> bool {
        self.cfg.allows(it)
    }

    /// How many known items the config is hiding right now, ignoring the `/`
    /// query. Drawn in the header so a filtered inbox never looks like an empty
    /// one.
    pub fn hidden_count(&self) -> usize {
        match self.cache() {
            None => 0,
            Some(c) => c.known().filter(|i| !self.is_visible(i)).count(),
        }
    }

    /// The focused board's columns with their **filtered** items.
    pub fn board_columns(&self) -> Vec<(Column, Vec<&Item>)> {
        let board = self.board;
        let section = board.section();
        let mut out: Vec<(Column, Vec<&Item>)> =
            board.columns().iter().map(|c| (*c, Vec::new())).collect();
        for it in self.section_items(&section) {
            let col = board.column_of(it);
            if let Some(slot) = out.iter_mut().find(|(c, _)| *c == col) {
                slot.1.push(it);
            }
        }
        out
    }

    /// The items the cursor moves through: one section in list view, one column
    /// in kanban view.
    pub fn focus_items(&self) -> Vec<&Item> {
        match self.view {
            View::List => self.section_items(&self.section),
            View::Kanban => {
                let cols = self.board_columns();
                cols.get(self.column).map(|(_, v)| v.clone()).unwrap_or_default()
            }
        }
    }

    /// The item the cursor is on, if any. `None` on an empty section or column,
    /// and while the cache failed to load.
    pub fn selected(&self) -> Option<&Item> {
        self.focus_items().get(self.cursor).copied()
    }

    /// The banner (or plain note) a section should show above its rows, if any.
    ///
    /// `ok:false` gives `SourceState::banner`. `ok:true` **with** a note still
    /// shows the note: that is how the collector reports a group-readable
    /// credential file, and phase 1 documents it as the one diagnostic that must
    /// never be silently dropped.
    pub fn section_banner(&self, section: &Section) -> Option<String> {
        let c = self.cache()?;
        let st = c.source_state(section);
        if let Some(b) = st.banner(self.now) {
            return Some(b);
        }
        if st.note.trim().is_empty() {
            None
        } else {
            Some(st.note)
        }
    }

    // -------------------------------------------------------------- movement

    pub fn clamp(&mut self) {
        if self.view == View::Kanban {
            let n = self.board.columns().len();
            if self.column >= n {
                self.column = n.saturating_sub(1);
            }
        }
        let len = self.focus_items().len();
        if self.cursor >= len {
            self.cursor = len.saturating_sub(1);
        }
        if len == 0 {
            self.cursor = 0;
        }
        self.clamp_preview();
    }

    pub fn clamp_preview(&mut self) {
        let max = self.preview_lines.saturating_sub(self.preview_height.max(1));
        if self.preview_scroll > max {
            self.preview_scroll = max;
        }
    }

    /// `j`/`k` and `ctrl-d`/`ctrl-u`.
    ///
    /// In **list** view the cursor moves through the whole list, crossing section
    /// boundaries, because the list IS flat: all three sections are drawn in one
    /// scrolling vector and phase 1's fzf let `j` walk straight through the
    /// separators (`ui.sh:406`, `j:down`). Confining the cursor to one section
    /// made `j` dead-end with the next section's rows visible on the very next
    /// line. `h`/`l` remain whole-section jumps, which is what the keymap says
    /// they are.
    ///
    /// In **kanban** view it stays confined to the focused column -- the keymap is
    /// explicit there ("within a kanban column"), and columns are side by side
    /// rather than end to end, so there is no "next" one to spill into.
    pub fn move_cursor(&mut self, delta: isize) {
        match self.view {
            View::List => self.move_in_list(delta),
            View::Kanban => {
                let len = self.focus_items().len() as isize;
                if len == 0 {
                    self.cursor = 0;
                    return;
                }
                let c = (self.cursor as isize + delta).clamp(0, len - 1);
                if c as usize != self.cursor {
                    self.cursor = c as usize;
                    self.preview_scroll = 0;
                }
            }
        }
    }

    /// Move `delta` rows through the flattened three-section list.
    ///
    /// Empty sections simply contribute nothing, so they are stepped over rather
    /// than landed on. Both ends clamp: the list is a road, not a ring -- `h`/`l`
    /// is the key that wraps.
    fn move_in_list(&mut self, delta: isize) {
        let counts: Vec<usize> = SECTIONS.iter().map(|s| self.section_items(s).len()).collect();
        let total: usize = counts.iter().sum();
        if total == 0 {
            self.cursor = 0;
            return;
        }
        let si = SECTIONS
            .iter()
            .position(|s| *s == self.section)
            .unwrap_or(0);
        let before: isize = counts[..si].iter().sum::<usize>() as isize;

        // A focused section with no rows has no flat position of its own, so the
        // move is measured from the boundary: forward lands on the first row
        // after it, backward on the last row before it.
        let target = if counts[si] == 0 {
            if delta >= 0 { before } else { before - 1 }
        } else {
            before + self.cursor.min(counts[si] - 1) as isize + delta
        };
        let target = target.clamp(0, total as isize - 1) as usize;

        let mut acc = 0usize;
        for (i, n) in counts.iter().enumerate() {
            if target < acc + n {
                let moved = SECTIONS[i] != self.section || target - acc != self.cursor;
                self.section = SECTIONS[i].clone();
                self.cursor = target - acc;
                if moved {
                    self.preview_scroll = 0;
                }
                return;
            }
            acc += n;
        }
    }

    /// After the `/` filter changed: if the focused section has no matches, move
    /// the focus to the first one that does.
    ///
    /// Without this the cursor is stranded -- the only match is drawn on screen
    /// under a different section header, the preview says `nothing selected`, and
    /// `j`/`k` cannot reach it because there is nothing in the focused section to
    /// move from. fzf put the cursor on the match; so does this.
    pub fn refocus_after_filter(&mut self) {
        if self.view != View::List || !self.focus_items().is_empty() {
            return;
        }
        for s in &SECTIONS {
            if !self.section_items(s).is_empty() {
                self.section = s.clone();
                self.cursor = 0;
                self.list_offset = 0;
                self.preview_scroll = 0;
                return;
            }
        }
    }

    /// `h`/`l` in list view. Wraps, because three sections in a popup is a ring,
    /// not a road.
    pub fn move_section(&mut self, delta: isize) {
        let i = SECTIONS
            .iter()
            .position(|s| *s == self.section)
            .unwrap_or(0) as isize;
        let n = SECTIONS.len() as isize;
        let j = ((i + delta) % n + n) % n;
        self.section = SECTIONS[j as usize].clone();
        self.cursor = 0;
        self.list_offset = 0;
        self.preview_scroll = 0;
    }

    /// `h`/`l` in kanban view. Does **not** wrap: a board has an order and
    /// running off the right edge back to DRAFT reads as a glitch.
    pub fn move_column(&mut self, delta: isize) {
        let n = self.board.columns().len() as isize;
        if n == 0 {
            return;
        }
        let j = (self.column as isize + delta).clamp(0, n - 1);
        if j as usize != self.column {
            self.column = j as usize;
            self.cursor = 0;
            self.list_offset = 0;
            self.preview_scroll = 0;
        }
    }

    /// `Tab`. Kanban only; a no-op in list view, per the keymap.
    pub fn cycle_board(&mut self) {
        self.step_board(1);
    }

    /// `shift+Tab`. The way back: three boards means Tab-Tab to reach the one
    /// before this, which is exactly the annoyance a reverse key exists for.
    pub fn cycle_board_back(&mut self) {
        self.step_board(-1);
    }

    fn step_board(&mut self, delta: isize) {
        if self.view != View::Kanban {
            return;
        }
        let n = BOARDS.len() as isize;
        let i = BOARDS.iter().position(|b| *b == self.board).unwrap_or(0) as isize;
        // rem_euclid, not `%`: -1 % 3 is -1 in Rust and would panic on index.
        self.board = BOARDS[(i + delta).rem_euclid(n) as usize];
        self.column = 0;
        self.cursor = 0;
        self.preview_scroll = 0;
        self.save_prefs();
    }

    /// `v`. The two views share a focus: toggling carries the section/board over
    /// so the user does not land somewhere unrelated.
    pub fn toggle_view(&mut self) {
        match self.view {
            View::List => {
                self.board = BOARDS
                    .iter()
                    .copied()
                    .find(|b| b.section() == self.section)
                    .unwrap_or(Board::Review);
                self.view = View::Kanban;
                self.column = 0;
            }
            View::Kanban => {
                self.section = self.board.section();
                self.view = View::List;
                self.list_offset = 0;
            }
        }
        self.cursor = 0;
        self.preview_scroll = 0;
        self.save_prefs();
    }

    // ---------------------------------------------------------------- config

    /// Copy the current view/board into the config and write it.
    ///
    /// Called from every mutation that changes something persisted, rather than
    /// once at exit: `o` and `a` also exit, so an exit-time save would have to be
    /// correct on three paths instead of none.
    pub fn save_prefs(&mut self) {
        if self.cfg.view == self.view && self.cfg.board == self.board {
            return;
        }
        self.cfg.view = self.view;
        self.cfg.board = self.board;
        self.cfg.save();
    }

    /// Every repo and project the screen offers, in a stable order.
    ///
    /// The union of what the cache mentions and what the config already hides.
    /// The second half is load-bearing: a repo with no open PRs today is absent
    /// from the cache, and listing only the cache would silently drop its hidden
    /// flag the moment someone opened one.
    pub fn config_rows(&self) -> Vec<ConfigRow> {
        let mut repos: Vec<String> = Vec::new();
        let mut projects: Vec<String> = Vec::new();
        if let Some(c) = self.cache() {
            for it in c.known() {
                let (bucket, name) = match it.kind {
                    crate::model::Kind::Jira => (&mut projects, &it.project),
                    _ => (&mut repos, &it.repo),
                };
                if !name.is_empty() && !bucket.iter().any(|x| x == name) {
                    bucket.push(name.clone());
                }
            }
        }
        for r in &self.cfg.hidden_repos {
            if !repos.iter().any(|x| x == r) {
                repos.push(r.clone());
            }
        }
        for p in &self.cfg.hidden_projects {
            if !projects.iter().any(|x| x == p) {
                projects.push(p.clone());
            }
        }
        repos.sort();
        projects.sort();

        let mut out = Vec::new();
        if !repos.is_empty() {
            out.push(ConfigRow::Header("GITHUB REPOSITORIES"));
        }
        for r in repos {
            let shown = !self.cfg.is_hidden_repo(&r);
            let n = self.count_repo(&r);
            out.push(ConfigRow::Repo { name: r, shown, n });
        }
        if !projects.is_empty() {
            out.push(ConfigRow::Header("JIRA PROJECTS"));
        }
        for p in projects {
            let shown = !self.cfg.is_hidden_project(&p);
            let n = self.count_project(&p);
            out.push(ConfigRow::Project { name: p, shown, n });
        }
        out
    }

    fn count_repo(&self, repo: &str) -> usize {
        self.cache().map_or(0, |c| {
            c.known()
                .filter(|i| i.kind != crate::model::Kind::Jira && i.repo == repo)
                .count()
        })
    }

    fn count_project(&self, project: &str) -> usize {
        self.cache().map_or(0, |c| {
            c.known()
                .filter(|i| i.kind == crate::model::Kind::Jira && i.project == project)
                .count()
        })
    }

    /// Toggle whatever the config cursor is on. Headers are inert.
    pub fn config_toggle(&mut self) {
        let rows = self.config_rows();
        match rows.get(self.cfg_cursor) {
            Some(ConfigRow::Repo { name, .. }) => {
                let name = name.clone();
                self.cfg.toggle_repo(&name);
            }
            Some(ConfigRow::Project { name, .. }) => {
                let name = name.clone();
                self.cfg.toggle_project(&name);
            }
            _ => return,
        }
        self.cfg.save();
        self.clamp_after_filter();
    }

    pub fn config_show_all(&mut self) {
        self.cfg.show_all();
        self.cfg.save();
        self.clamp_after_filter();
    }

    pub fn config_move(&mut self, delta: isize) {
        let rows = self.config_rows();
        if rows.is_empty() {
            self.cfg_cursor = 0;
            return;
        }
        let mut i = self.cfg_cursor as isize;
        // Skip headers: they cannot be toggled, so stopping on one would be a
        // keypress that appears to do nothing.
        for _ in 0..rows.len() {
            i = (i + delta).rem_euclid(rows.len() as isize);
            if !matches!(rows[i as usize], ConfigRow::Header(_)) {
                break;
            }
        }
        self.cfg_cursor = i as usize;
    }

    /// After the filter changes, the list underneath may be shorter than the
    /// cursor that was pointing into it -- or the focused section may be empty
    /// now, which strands the selection somewhere the movement keys cannot
    /// reach. Both are already solved for the `/` query; reuse that.
    fn clamp_after_filter(&mut self) {
        self.clamp();
        self.refocus_after_filter();
        self.list_offset = 0;
        self.preview_scroll = 0;
    }

    // ----------------------------------------------------------------- tick

    pub fn spinner_char(&self) -> char {
        SPINNER[self.spinner % SPINNER.len()]
    }

    pub fn flash_msg(&mut self, text: impl Into<String>) {
        self.flash = Some(Flash {
            text: text.into(),
            until: Instant::now() + FLASH_FOR,
        });
    }

    /// Called on the tick. Reloads when the collector has replaced the file,
    /// clamps the cursors to the new item counts, reaps the child and expires
    /// the flash.
    pub fn tick(&mut self) {
        self.now = cache::now_unix();
        self.last_tick = Instant::now();
        if self.refreshing {
            self.spinner = self.spinner.wrapping_add(1);
        }
        if self.flash.as_ref().is_some_and(|f| Instant::now() >= f.until) {
            self.flash = None;
        }
        self.reap_collect();
        self.poll_cache();
    }

    fn reap_collect(&mut self) {
        if let Some(child) = &mut self.collect {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => {
                    self.collect = None;
                    self.refreshing = false;
                }
                Ok(None) => {}
            }
        } else {
            self.refreshing = false;
        }
    }

    /// Reload when the collector has replaced the file.
    ///
    /// `collect.sh` writes with `mv -f`, so a read that follows an mtime change
    /// can never see a half-written cache. Both arms of `loaded` are polled: the
    /// `Err` arm is the first-ever-run path, where the file does not exist yet
    /// and the whole recovery depends on noticing when it appears.
    fn poll_cache(&mut self) {
        match &mut self.loaded {
            Ok(l) => {
                if l.changed_on_disk() {
                    // On a failed re-read the previous contents are kept -- the
                    // same rule ui.sh's `--render` follows, because showing a
                    // stale list beats blanking the popup.
                    let _ = l.reload();
                    self.clamp();
                }
            }
            Err(_) => {
                let m = cache::mtime_of(&self.path);
                if m != self.err_mtime {
                    self.err_mtime = m;
                    if let Ok(fresh) = cache::load_from(&self.path) {
                        self.loaded = Ok(fresh);
                        self.focus_first_nonempty();
                        self.cursor = 0;
                        self.clamp();
                    }
                }
            }
        }
    }

    /// `r`, and once at startup. Spawns the collector detached and returns; the
    /// tick notices the new mtime.
    pub fn start_collect(&mut self) {
        if self.refreshing {
            // A second concurrent collect would be harmless (the writes are
            // atomic and the temp file is PID-suffixed) but pointless.
            return;
        }
        match actions::spawn_collect() {
            Ok(child) => {
                self.collect = Some(child);
                self.refreshing = true;
                self.spinner = 0;
            }
            Err(e) => self.flash_msg(format!("could not start the collector: {e}")),
        }
    }

    // -------------------------------------------------------------- the loop

    /// The event loop. Terminal setup and teardown live in `main`, so that a
    /// `?` out of here still passes through the restore.
    pub fn run(&mut self, term: &mut DefaultTerminal) -> io::Result<()> {
        loop {
            if self.needs_clear {
                // A full repaint, WITHOUT `Terminal::clear()`.
                //
                // `clear()` snapshots the cursor with a DSR round trip
                // (`ESC[6n` out, a reply expected back on stdin). In a real
                // terminal that is instant; under a pty with no emulator behind
                // it -- a scripted run, a smoke test -- nothing ever answers and
                // crossterm fails after two seconds with "the cursor position
                // could not be read", which would take the whole app down on a
                // keypress whose only job was to tidy the screen. `resize` to
                // the size we already have clears the viewport and resets the
                // diff buffer with no round trip at all.
                let size = term.size()?;
                term.resize(Rect::new(0, 0, size.width, size.height))?;
                self.needs_clear = false;
            }
            term.draw(|f| view::draw(f, self))?;
            if self.quit {
                return Ok(());
            }

            if event::poll(TICK)? {
                match event::read()? {
                    // Release events only arrive under the kitty keyboard
                    // protocol, which we never enable -- but if a terminal sends
                    // them anyway, acting on both halves would double every key.
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        if let Some(a) = input::handle(self, k) {
                            self.act(a, term)?;
                        }
                    }
                    // A resize needs no state change: the next draw reads the
                    // new area and every offset is clamped at render time.
                    _ => {}
                }
            }
            if self.last_tick.elapsed() >= TICK {
                self.tick();
            }
            if self.quit {
                return Ok(());
            }
        }
    }

    /// Run one side effect. The only place in the crate that spawns a process
    /// other than the startup collect.
    fn act(&mut self, a: Action, term: &mut DefaultTerminal) -> io::Result<()> {
        match a {
            Action::Quit => self.quit = true,
            Action::Refresh => self.start_collect(),
            Action::OpenInBrowser => {
                // Nothing selected -> nothing to open, and therefore nothing to
                // exit for: quitting here would close the popup on a key that did
                // not do its job (easy to hit with a filter that matches nothing).
                // `a` already returns early for the same reason.
                let Some((_, url)) = self.selected_ref_url() else {
                    return Ok(());
                };
                let _ = actions::open_in_browser(&url);
                // `o` exits, matching phase 1 and the user's explicit choice.
                self.quit = true;
            }
            Action::CopyLink => {
                if let Some((r#ref, url)) = self.selected_ref_url() {
                    let ok = actions::copy_link(&url).unwrap_or(false);
                    if ok {
                        self.flash_msg(format!("copied {ref}", ref = r#ref));
                        actions::notify(&format!("copied {}", r#ref), true);
                    } else {
                        self.flash_msg(format!("could not copy {}", r#ref));
                        actions::notify(&format!("could not copy {}", r#ref), false);
                    }
                    // copy-link.sh wrote an OSC 52 escape straight to /dev/tty,
                    // which this process owns in raw mode inside the alternate
                    // screen. The escape itself paints nothing, but a terminal
                    // that does not understand it may echo the payload -- repaint
                    // from scratch rather than trust it.
                    self.needs_clear = true;
                }
            }
            Action::HandOffToAgent => self.begin_handoff(term)?,
            Action::SubmitAgent => {
                let pane = self.agents.get(self.agent_cursor).map(|a| a.pane_id.clone());
                if let Some(pane) = pane {
                    self.send_handoff(&pane);
                }
            }
        }
        Ok(())
    }

    fn selected_ref_url(&self) -> Option<(String, String)> {
        let it = self.selected()?;
        if it.url.is_empty() {
            return None;
        }
        Some((it.r#ref.clone(), it.url.clone()))
    }

    /// `a`: resolve a target agent and hand off, or open the picker.
    ///
    /// Blocking, unlike everything else in the loop -- `herdr agent list` is a
    /// local IPC round trip and only runs on this key. Nothing is drawn between
    /// the call and its result, so a slow herdr shows as a frozen frame for its
    /// duration, which is the same behaviour phase 1 had.
    fn begin_handoff(&mut self, _term: &mut DefaultTerminal) -> io::Result<()> {
        if self.selected_ref_url().is_none() {
            return Ok(());
        }
        let agents = match actions::list_agents() {
            Ok(a) => a,
            Err(e) => {
                self.flash_msg(format!("herdr agent list failed: {e}"));
                return Ok(());
            }
        };
        if let Some(pane) = actions::resolve_agent(&agents) {
            self.send_handoff(&pane);
            return Ok(());
        }
        if agents.is_empty() {
            // A picker over zero candidates is a hang-shaped UI. Phase 1 failed
            // with a notification here and so do we -- except we stay open, so
            // the user can start an agent and press `a` again.
            let r#ref = self.selected().map(|i| i.r#ref.clone()).unwrap_or_default();
            self.flash_msg(format!("no agent is running to hand {} to", r#ref));
            actions::notify(&format!("no agent is running to hand {} to", r#ref), false);
            return Ok(());
        }
        self.agents = agents;
        self.agent_cursor = 0;
        self.mode = Mode::AgentPicker;
        Ok(())
    }

    fn send_handoff(&mut self, pane: &str) {
        let Some(it) = self.selected() else { return };
        let r#ref = it.r#ref.clone();
        let text = actions::prompt_text(it);
        match actions::send_to_agent(pane, &text) {
            Ok(()) => {
                // "put in" rather than "handed to": the text is sitting in the
                // agent's composer waiting for the user to press enter, and a
                // message that read like a completed hand-off would send them
                // off to do something else while nothing had been asked yet.
                actions::notify(&format!("put {} in {}'s input", r#ref, pane), true);
                // `a` exits, like `o` -- the popup is in the way of the composer
                // it just wrote into.
                self.quit = true;
            }
            Err(e) => {
                self.mode = Mode::Nav;
                self.flash_msg(format!("could not put {} in {}: {}", r#ref, pane, e));
                actions::notify(&format!("could not put {} in {}", r#ref, pane), false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
      "version": 1, "fetched_unix": 1000,
      "sources": {
        "github": {"ok": true, "note": "", "fetched_unix": 1000},
        "jira": {"ok": false, "note": "jira: unreachable", "fetched_unix": 400}
      },
      "items": [
        {"kind":"pr","section":"review","ref":"a/b#1","url":"u1","title":"alpha","author":"x","updated":"2026-08-11T00:00:00Z","review_decision":"APPROVED"},
        {"kind":"pr","section":"review","ref":"a/b#2","url":"u2","title":"beta","author":"x","updated":"2026-08-11T00:00:00Z"},
        {"kind":"pr","section":"mine","ref":"a/b#3","url":"u3","title":"gamma","author":"me","updated":"2026-08-11T00:00:00Z","draft":true},
        {"kind":"jira","section":"jira","ref":"G-1","url":"u4","title":"進行中のタスク","status":"進行中","status_category":"In Progress","type":"Task","priority":"Medium","updated":"2026-08-11T00:00:00Z"},
        {"kind":"wat","section":"review","ref":"a/b#9","url":"u9","title":"future kind","updated":"2026-08-11T00:00:00Z"}
      ]
    }"#;

    fn app() -> App {
        let cache: Cache = serde_json::from_str(FIXTURE).unwrap();
        let loaded = Loaded {
            path: PathBuf::from("/nonexistent/cache.json"),
            cache,
            mtime: None,
        };
        let mut a = App::with_config_default(PathBuf::from("/nonexistent/cache.json"));
        a.loaded = Ok(loaded);
        a.now = 1600;
        a.focus_first_nonempty();
        a
    }

    #[test]
    fn unknown_kinds_are_dropped_from_every_view() {
        let a = app();
        // 3 review items in the cache, one of them kind:"wat"
        assert_eq!(a.section_items(&Section::Review).len(), 2);
        assert!(
            a.section_items(&Section::Review)
                .iter()
                .all(|i| i.r#ref != "a/b#9")
        );
        // ...and the COUNTS agree with the rows. A header that says 3 above two
        // drawn rows is the same bug seen from the other side.
        let c = a.cache().unwrap();
        assert_eq!(c.count_in(&Section::Review), 2);
        assert_eq!(c.header_counts(), (1, 2, 1));
        assert_eq!(c.items_in(&Section::Review).len(), 2);
        let total: usize = c
            .board_columns(Board::Review)
            .iter()
            .map(|(_, v)| v.len())
            .sum();
        assert_eq!(total, 2, "a kanban board must not bucket a kind it cannot draw");
        assert_eq!(c.known().count(), 4);
    }

    /// `j`/`k` walk the whole list. The three sections are drawn as one scrolling
    /// vector, so a cursor that stopped at a section boundary dead-ended with the
    /// next section's rows visible one line below it.
    #[test]
    fn the_cursor_crosses_section_boundaries_in_list_view() {
        let mut a = app();
        a.section = Section::Review;
        a.cursor = 0;

        a.move_cursor(1);
        assert_eq!((a.section.clone(), a.cursor), (Section::Review, 1));
        a.move_cursor(1);
        assert_eq!((a.section.clone(), a.cursor), (Section::Mine, 0));
        a.move_cursor(1);
        assert_eq!((a.section.clone(), a.cursor), (Section::Jira, 0));
        // the bottom clamps -- h/l is the key that wraps, not j
        a.move_cursor(1);
        assert_eq!((a.section.clone(), a.cursor), (Section::Jira, 0));

        a.move_cursor(-1);
        assert_eq!((a.section.clone(), a.cursor), (Section::Mine, 0));
        // a half-page from anywhere clamps at the top rather than wrapping
        a.move_cursor(-99);
        assert_eq!((a.section.clone(), a.cursor), (Section::Review, 0));
    }

    /// An empty focused section has no flat position of its own: the move is
    /// measured from the boundary, so the section is stepped over rather than
    /// swallowing the keystroke.
    #[test]
    fn an_empty_focused_section_is_stepped_over() {
        let mut a = app();
        a.query = "a/b#3".into(); // matches the single MINE pr and nothing else
        assert!(a.section_items(&Section::Review).is_empty());
        assert!(a.section_items(&Section::Jira).is_empty());

        a.section = Section::Review;
        a.cursor = 0;
        a.move_cursor(1);
        assert_eq!((a.section.clone(), a.cursor), (Section::Mine, 0));

        a.section = Section::Jira;
        a.cursor = 0;
        a.move_cursor(-1);
        assert_eq!((a.section.clone(), a.cursor), (Section::Mine, 0));

        // nothing matches at all -> a move is a no-op, not a panic
        a.query = "zzzz".into();
        a.section = Section::Review;
        a.move_cursor(1);
        assert_eq!(a.cursor, 0);
        assert!(a.selected().is_none());
    }

    /// Kanban is the other rule: the keymap says "within a kanban column", and
    /// columns sit side by side, so there is nothing to spill into.
    #[test]
    fn kanban_movement_stays_inside_its_column() {
        let mut a = app();
        a.view = View::Kanban;
        a.board = Board::Review;
        a.column = 0; // NEEDS REVIEW holds exactly one card
        a.cursor = 0;
        a.move_cursor(1);
        assert_eq!(a.cursor, 0);
        assert_eq!(a.column, 0);
        a.move_cursor(-1);
        assert_eq!(a.cursor, 0);
    }

    /// A filter that empties the focused section must carry the focus to the
    /// match, or the only row on screen is unreachable and un-actionable.
    #[test]
    fn a_filter_moves_the_focus_to_where_the_matches_are() {
        let mut a = app();
        a.section = Section::Review;
        a.query = "進行中".into();
        a.clamp();
        assert!(a.selected().is_none(), "precondition: the focus went empty");
        a.refocus_after_filter();
        assert_eq!(a.section, Section::Jira);
        assert_eq!(a.selected().map(|i| i.r#ref.clone()).as_deref(), Some("G-1"));

        // a focus that still has matches is left exactly where it is
        a.query = "a/b".into();
        a.section = Section::Mine;
        a.cursor = 0;
        a.refocus_after_filter();
        assert_eq!(a.section, Section::Mine);
    }

    #[test]
    fn search_filters_every_focus_unit() {
        let mut a = app();
        a.query = "beta".into();
        assert_eq!(a.section_items(&Section::Review).len(), 1);
        a.view = View::Kanban;
        a.board = Board::Review;
        let cols = a.board_columns();
        let total: usize = cols.iter().map(|(_, v)| v.len()).sum();
        assert_eq!(total, 1);
        // and the columns themselves survive the filter
        assert_eq!(cols.len(), 3);
    }

    #[test]
    fn cursor_is_clamped_when_the_filter_shrinks_the_list() {
        let mut a = app();
        a.section = Section::Review;
        a.cursor = 1;
        a.query = "beta".into();
        a.clamp();
        assert_eq!(a.cursor, 0);
        // and an empty result leaves a valid (if selectionless) state
        a.query = "zzzz".into();
        a.clamp();
        assert_eq!(a.cursor, 0);
        assert!(a.selected().is_none());
    }

    #[test]
    fn sections_wrap_and_columns_do_not() {
        let mut a = app();
        a.section = Section::Review;
        a.move_section(-1);
        assert_eq!(a.section, Section::Jira);
        a.move_section(1);
        assert_eq!(a.section, Section::Review);

        a.view = View::Kanban;
        a.board = Board::Mine; // 4 columns
        a.column = 0;
        a.move_column(-1);
        assert_eq!(a.column, 0, "left edge must not wrap");
        a.move_column(9);
        assert_eq!(a.column, 3, "right edge must not wrap");
    }

    #[test]
    fn toggling_the_view_carries_the_focus_over() {
        let mut a = app();
        a.section = Section::Jira;
        a.toggle_view();
        assert_eq!(a.view, View::Kanban);
        assert_eq!(a.board, Board::Jira);
        a.cycle_board();
        assert_eq!(a.board, Board::Review);
        a.toggle_view();
        assert_eq!(a.view, View::List);
        assert_eq!(a.section, Section::Review);
    }

    #[test]
    fn tab_is_a_no_op_in_list_view() {
        let mut a = app();
        a.view = View::List;
        a.board = Board::Review;
        a.cycle_board();
        assert_eq!(a.board, Board::Review);
    }

    #[test]
    fn a_failed_leg_with_retained_items_banners_as_stale() {
        let a = app();
        let b = a.section_banner(&Section::Jira).unwrap();
        assert!(b.starts_with("jira: stale, 20m ago — jira: unreachable"), "{b}");
        // a healthy leg with an empty note gets nothing at all
        assert!(a.section_banner(&Section::Review).is_none());
    }

    #[test]
    fn an_ok_leg_with_a_note_still_shows_it() {
        let mut a = app();
        if let Ok(l) = &mut a.loaded {
            l.cache.sources.github.note = "github: credential file is group-readable".into();
        }
        assert_eq!(
            a.section_banner(&Section::Review).as_deref(),
            Some("github: credential file is group-readable")
        );
    }

    #[test]
    fn startup_focus_skips_empty_sections() {
        let cache: Cache = serde_json::from_str(
            r#"{"version":1,"fetched_unix":1,"items":[
                 {"kind":"jira","section":"jira","ref":"G-1","title":"x","url":"u"}]}"#,
        )
        .unwrap();
        let mut a = App::with_config_default(PathBuf::from("/nonexistent/cache.json"));
        a.loaded = Ok(Loaded {
            path: PathBuf::from("/nonexistent/cache.json"),
            cache,
            mtime: None,
        });
        a.focus_first_nonempty();
        assert_eq!(a.section, Section::Jira);
    }

    #[test]
    fn a_missing_cache_is_an_error_state_not_a_panic() {
        let a = App::with_config_default(PathBuf::from("/nonexistent/wi/cache.json"));
        assert!(a.loaded.is_err());
        assert!(a.selected().is_none());
        assert!(a.section_items(&Section::Review).is_empty());
        assert!(a.section_banner(&Section::Jira).is_none());
    }

    /// The first-ever-run recovery path: no cache, `r`, collector writes one,
    /// the tick has to notice. Without `err_mtime` this silently never happens.
    #[test]
    fn the_error_state_recovers_when_a_cache_appears() {
        let dir = std::env::temp_dir().join(format!("wi-app-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("cache.json");
        let _ = std::fs::remove_file(&p);

        let mut a = App::with_config_default(p.clone());
        assert!(a.loaded.is_err());

        std::fs::write(
            &p,
            br#"{"version":1,"fetched_unix":5,"items":[
                 {"kind":"pr","section":"mine","ref":"a/b#1","title":"t","url":"u"}]}"#,
        )
        .unwrap();
        a.tick();
        assert!(a.loaded.is_ok(), "a cache that appears must be picked up");
        assert_eq!(a.section, Section::Mine);
        assert_eq!(a.section_items(&Section::Mine).len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
