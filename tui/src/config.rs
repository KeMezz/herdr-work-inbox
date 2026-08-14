//! The user's own preferences: which repos and projects this panel shows, and
//! where it was left.
//!
//! Separate from `cache.rs` on purpose. The cache is derived data that the
//! collector overwrites wholesale and that anyone may delete to force a refetch;
//! this file is the only thing here the user authored, so it lives in the plugin
//! **config** directory and survives a cache wipe.
//!
//! ```json
//! { "version": 1,
//!   "view": "kanban", "board": "mine",
//!   "hidden_repos": ["acme/scratch"], "hidden_projects": ["OPS"] }
//! ```
//!
//! # Why a deny list and not an allow list
//!
//! The filter stores what to HIDE. A repository nobody has hidden is shown, so a
//! review request from a repo that did not exist when the config was written can
//! never be silently absent -- which is the one failure this panel must not
//! have. An allow list has the opposite default and would need maintaining every
//! time a new repo appears.
//!
//! Entries are kept even when nothing in the cache mentions them any more: a
//! repo with no open PRs today would otherwise quietly un-hide itself the moment
//! someone opened one.
//!
//! # Why it saves on change rather than on exit
//!
//! `o` and `a` both exit the app after doing their work, so "save on quit" would
//! mean auditing every exit path and would lose a preference to the first one
//! that was missed. The file is a few hundred bytes; writing it whenever
//! something changes costs nothing and removes the question.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Board, View};

/// Where the config lives. `HERDR_PLUGIN_CONFIG_DIR` is set for plugin commands
/// only -- the popup front end is a keybinding, not a plugin command, so it has
/// to reproduce herdr's own default. Same shape as `cache::state_dir`.
pub fn config_dir() -> PathBuf {
    if let Some(d) = non_empty("HERDR_PLUGIN_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let base = non_empty("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home()).join(".config"));
    base.join("herdr/plugins/config/jin.work-inbox")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Where this was read from, and where `save` writes.
    ///
    /// `None` means "not backed by a file", and `save` is then a no-op. That is
    /// what `Config::default()` gives, which is what the unit tests use -- a
    /// test that toggles a view must not reach into the real
    /// `~/.config/herdr/plugins/config/` and overwrite what the user left there.
    /// `load()` is the only thing that sets it.
    #[serde(skip)]
    pub path: Option<PathBuf>,

    #[serde(default = "one")]
    pub version: u32,
    /// The view the app was last in. Restored on the next open.
    #[serde(default)]
    pub view: View,
    /// The kanban board it was last on. Restored with the view, because landing
    /// on board 1 every time is the same annoyance as landing in list view.
    #[serde(default)]
    pub board: Board,
    /// GitHub repositories to hide, by the bare `repo` field the collector
    /// writes (`acme_api`, not `owner/acme_api`).
    #[serde(default)]
    pub hidden_repos: Vec<String>,
    /// Jira project keys to hide (`ACME`).
    #[serde(default)]
    pub hidden_projects: Vec<String>,
}

fn one() -> u32 {
    1
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: None,
            version: 1,
            view: View::default(),
            board: Board::default(),
            hidden_repos: Vec::new(),
            hidden_projects: Vec::new(),
        }
    }
}

impl Config {
    /// Read it, or fall back to the default.
    ///
    /// **Every failure is silent and non-fatal.** A missing file is the ordinary
    /// first run; a corrupt one is someone's half-finished hand edit. Neither is
    /// worth an error frame in front of the inbox, and neither can lose data
    /// that is not already on screen -- the next save rewrites the file whole.
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Self {
        let mut c: Self = fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        c.path = Some(path.to_path_buf());
        c
    }

    /// Write, if this config came from a file. Errors are swallowed: a read-only
    /// config directory must not take the inbox away from the user, and the
    /// preference is still correct for this session.
    pub fn save(&self) {
        if let Some(p) = self.path.clone() {
            let _ = self.save_to(&p);
        }
    }

    /// Atomic, and 0600 like everything else this plugin writes. Private repo
    /// and project names are not a secret on the level of the Jira token, but
    /// they are the user's employer's, and the discipline is cheaper to keep
    /// than to reason about each time.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
            set_mode(dir, 0o700);
        }
        let body = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // A distinct temp name per process: two popups open at once must not
        // half-write each other's file.
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        set_mode(&tmp, 0o600);
        fs::rename(&tmp, path)
    }

    /// The filter itself, in one place: an item is shown unless its repo (PR) or
    /// project (Jira) is on the deny list. `App::is_visible` and `--dump` both
    /// call this so the TUI and the headless view can never disagree about what
    /// is on screen.
    pub fn allows(&self, it: &crate::model::Item) -> bool {
        match it.kind {
            crate::model::Kind::Jira => !self.is_hidden_project(&it.project),
            _ => !self.is_hidden_repo(&it.repo),
        }
    }

    pub fn is_hidden_repo(&self, repo: &str) -> bool {
        self.hidden_repos.iter().any(|r| r == repo)
    }

    pub fn is_hidden_project(&self, project: &str) -> bool {
        self.hidden_projects.iter().any(|p| p == project)
    }

    /// Flip one entry. Empty names are ignored rather than stored: an item whose
    /// repo or project the collector could not determine must stay visible, or
    /// it would vanish behind a filter row that says nothing.
    pub fn toggle_repo(&mut self, repo: &str) {
        toggle(&mut self.hidden_repos, repo);
    }

    pub fn toggle_project(&mut self, project: &str) {
        toggle(&mut self.hidden_projects, project);
    }

    pub fn show_all(&mut self) {
        self.hidden_repos.clear();
        self.hidden_projects.clear();
    }
}

fn toggle(list: &mut Vec<String>, name: &str) {
    if name.is_empty() {
        return;
    }
    if let Some(i) = list.iter().position(|x| x == name) {
        list.remove(i);
    } else {
        list.push(name.to_string());
        list.sort();
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wi-cfg-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_missing_or_corrupt_file_is_the_default_not_an_error() {
        let d = tmpdir("missing");
        assert!(Config::load_from(&d.join("nope.json")).hidden_repos.is_empty());

        let p = d.join("bad.json");
        fs::write(&p, "{ not json").unwrap();
        let c = Config::load_from(&p);
        assert_eq!(c.view, View::List);
        assert!(c.hidden_repos.is_empty());
    }

    #[test]
    fn it_round_trips() {
        let d = tmpdir("roundtrip");
        let p = d.join("config.json");
        let mut c = Config::default();
        c.view = View::Kanban;
        c.board = Board::Jira;
        c.toggle_repo("acme_api");
        c.toggle_project("ACME");
        c.save_to(&p).unwrap();

        let back = Config::load_from(&p);
        assert_eq!(back.view, View::Kanban);
        assert_eq!(back.board, Board::Jira);
        assert!(back.is_hidden_repo("acme_api"));
        assert!(back.is_hidden_project("ACME"));
        // and anything not named is visible -- the deny-list property
        assert!(!back.is_hidden_repo("acme_web"));
        assert!(!back.is_hidden_project("OPS"));
    }

    #[test]
    fn toggling_is_symmetric_and_ignores_empty_names() {
        let mut c = Config::default();
        c.toggle_repo("a");
        assert!(c.is_hidden_repo("a"));
        c.toggle_repo("a");
        assert!(!c.is_hidden_repo("a"));

        c.toggle_repo("");
        assert!(c.hidden_repos.is_empty());
        assert!(!c.is_hidden_repo(""));
    }

    /// A hidden entry the cache no longer mentions must survive a save/load
    /// cycle, or a repo with no open PRs today un-hides itself tomorrow.
    #[test]
    fn hidden_entries_outlive_the_cache() {
        let d = tmpdir("outlive");
        let p = d.join("config.json");
        let mut c = Config::default();
        c.toggle_repo("a_repo_with_nothing_open");
        c.save_to(&p).unwrap();
        assert!(Config::load_from(&p).is_hidden_repo("a_repo_with_nothing_open"));
    }

    #[test]
    fn the_file_is_written_atomically_and_0600() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("mode");
        let p = d.join("config.json");
        Config::default().save_to(&p).unwrap();
        let mode = fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config is {mode:o}");
        // no temp file left behind
        let leftovers: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    /// A config written by a future build must not blank the preferences this
    /// build does understand.
    #[test]
    fn unknown_fields_are_ignored() {
        let d = tmpdir("unknown");
        let p = d.join("config.json");
        fs::write(
            &p,
            r#"{"version":1,"view":"kanban","hidden_repos":["x"],"sort_by":"age"}"#,
        )
        .unwrap();
        let c = Config::load_from(&p);
        assert_eq!(c.view, View::Kanban);
        assert!(c.is_hidden_repo("x"));
    }
}
