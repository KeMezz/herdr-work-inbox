//! Finding, reading and re-reading `cache.json`.
//!
//! This is the only file in the crate that touches the filesystem for data. It
//! never fetches: the collector (`collect.sh`) owns the network, this owns the
//! file. See `actions::spawn_collect` for the other half.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::model::Cache;

/// Where the cache lives, resolved exactly as `ui.sh` resolves it:
///
/// ```sh
/// STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/jin.work-inbox}"
/// ```
///
/// The `:-` form treats an **empty** variable as unset, which is why both reads
/// filter on `is_empty` -- an exported-but-empty `XDG_STATE_HOME` would
/// otherwise resolve the cache to `/herdr/plugins/...`.
///
/// `HERDR_PLUGIN_STATE_DIR` is set only for plugin commands; the popup is a user
/// keybinding, not a plugin command, so the fallback is the normal path. Test
/// runs set the variable to point at a scratch copy, which is the only reason
/// this binary can be exercised without touching the live cache.
pub fn state_dir() -> PathBuf {
    if let Some(d) = env_nonempty("HERDR_PLUGIN_STATE_DIR") {
        return PathBuf::from(d);
    }
    let base = env_nonempty("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env_nonempty("HOME").unwrap_or_else(|| "/".to_string());
            Path::new(&home).join(".local/state")
        });
    base.join("herdr/plugins/jin.work-inbox")
}

pub fn cache_path() -> PathBuf {
    state_dir().join("cache.json")
}

fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

/// Why a load failed. Both arms are drawable states, never a panic: the contract
/// requires a missing or corrupt cache to render a frame that says so and offers
/// `r`.
#[derive(Debug)]
pub enum LoadError {
    /// No file, or it could not be read.
    Missing(PathBuf, io::Error),
    /// The file exists but is not a cache we understand.
    Unparseable(PathBuf, serde_json::Error),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Missing(p, e) => write!(f, "cannot read {}: {}", p.display(), e),
            LoadError::Unparseable(p, e) => write!(f, "cannot parse {}: {}", p.display(), e),
        }
    }
}

/// A loaded cache plus the mtime it was loaded at.
///
/// The mtime is the whole reload mechanism. `collect.sh` writes with `mv -f`, so
/// the file is replaced atomically and a read that follows an mtime change can
/// never see a half-written cache. The app polls [`Loaded::changed_on_disk`] on
/// its tick (250ms is plenty -- no filesystem-event crate, no extra dependency)
/// and reloads when it fires.
#[derive(Debug)]
pub struct Loaded {
    pub path: PathBuf,
    pub cache: Cache,
    pub mtime: Option<SystemTime>,
}

/// Read and parse the cache at `path`.
///
/// **The mtime is stamped BEFORE the bytes are read, never after.** `collect.sh`
/// replaces the file with `mv -f`, so a rename can land in the window between the
/// two syscalls. Stamping afterwards would pair the *new* mtime with the *old*
/// contents, `changed_on_disk()` would compare equal forever, and the popup would
/// show a stale list until some later collect happened to write again -- a
/// silently frozen UI. Stamping first can only pair an old mtime with new
/// contents, which costs one redundant reload and nothing else.
pub fn load_from(path: &Path) -> Result<Loaded, LoadError> {
    let mtime = mtime_of(path);
    let bytes = fs::read(path).map_err(|e| LoadError::Missing(path.to_path_buf(), e))?;
    let cache: Cache =
        serde_json::from_slice(&bytes).map_err(|e| LoadError::Unparseable(path.to_path_buf(), e))?;
    Ok(Loaded {
        path: path.to_path_buf(),
        cache,
        mtime,
    })
}

pub fn mtime_of(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Loaded {
    /// Has the collector replaced the file since we read it? A file that
    /// disappeared reads as "unchanged" on purpose: a reload would only replace
    /// a good list with an error frame, and the next successful write will trip
    /// this anyway.
    pub fn changed_on_disk(&self) -> bool {
        match (mtime_of(&self.path), self.mtime) {
            (Some(now), Some(then)) => now != then,
            (Some(_), None) => true,
            _ => false,
        }
    }

    /// Re-read in place. On failure the previous contents are kept -- the same
    /// rule ui.sh's `--render` follows, because showing a stale list beats
    /// blanking the popup.
    pub fn reload(&mut self) -> Result<(), LoadError> {
        let fresh = load_from(&self.path)?;
        self.cache = fresh.cache;
        self.mtime = fresh.mtime;
        Ok(())
    }
}

/// Wall-clock seconds, for the age helpers in `model`. Kept here so `model`
/// stays pure and testable.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// `std::env::set_var` is unsafe in edition 2024 and the process env is
    /// global, so the env-dependent assertions live in ONE test that runs
    /// serially within itself rather than racing three parallel ones.
    #[test]
    fn state_dir_matches_ui_sh_resolution() {
        unsafe {
            std::env::set_var("HOME", "/home/tester");
            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            std::env::remove_var("XDG_STATE_HOME");
            assert_eq!(
                state_dir(),
                PathBuf::from("/home/tester/.local/state/herdr/plugins/jin.work-inbox")
            );

            // an empty variable is UNSET, per the `:-` form in ui.sh
            std::env::set_var("XDG_STATE_HOME", "");
            assert_eq!(
                state_dir(),
                PathBuf::from("/home/tester/.local/state/herdr/plugins/jin.work-inbox")
            );

            std::env::set_var("XDG_STATE_HOME", "/xdg");
            assert_eq!(
                state_dir(),
                PathBuf::from("/xdg/herdr/plugins/jin.work-inbox")
            );

            // the plugin state dir, when set, wins outright and is used as-is
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", "/scratch/state");
            assert_eq!(state_dir(), PathBuf::from("/scratch/state"));
            assert_eq!(cache_path(), PathBuf::from("/scratch/state/cache.json"));

            std::env::remove_var("HERDR_PLUGIN_STATE_DIR");
            std::env::remove_var("XDG_STATE_HOME");
        }
    }

    #[test]
    fn missing_and_corrupt_caches_are_errors_not_panics() {
        let dir = std::env::temp_dir().join(format!("wi-cache-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("cache.json");
        let _ = fs::remove_file(&p);

        assert!(matches!(load_from(&p), Err(LoadError::Missing(..))));

        fs::write(&p, b"{ not json").unwrap();
        assert!(matches!(load_from(&p), Err(LoadError::Unparseable(..))));

        fs::write(&p, br#"{"version":1,"fetched_unix":1,"items":[]}"#).unwrap();
        let l = load_from(&p).unwrap();
        assert_eq!(l.cache.version, 1);
        assert!(!l.changed_on_disk());

        // an mtime move is what the tick loop watches for
        let mut f = fs::OpenOptions::new().write(true).open(&p).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        f.write_all(br#"{"version":1,"fetched_unix":2,"items":[]}"#).unwrap();
        f.flush().unwrap();
        drop(f);
        filetime_bump(&p);
        assert!(l.changed_on_disk());

        let _ = fs::remove_dir_all(&dir);
    }

    /// Rewriting a file within the same filesystem timestamp granularity can
    /// leave the mtime untouched; touching it through a second write with a
    /// sleep in between is enough on APFS (1ns granularity) but this keeps the
    /// test honest on coarser filesystems.
    fn filetime_bump(p: &Path) {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
        let _ = f.write_all(b"\n");
    }
}
