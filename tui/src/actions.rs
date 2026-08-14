//! The side effects.
//!
//! # The no-network rule
//!
//! This binary never opens a socket. The **only** processes it may spawn:
//!
//! | command | why |
//! |---|---|
//! | `collect.sh` | refresh, detached |
//! | `copy-link.sh` | clipboard (OSC 52 + pbcopy) |
//! | `/usr/bin/open` | browser; absolute path, it is not a Homebrew binary |
//! | `herdr` | `agent list`, `pane send-text`, `notification show`, `api snapshot` |
//!
//! Nothing else. An HTTP client in the dependency tree is an automatic defect.
//!
//! # PATH
//!
//! herdr runs its children with a minimal PATH and the popup inherits it, so
//! `herdr` itself may not be resolvable -- and `ui.sh`'s `export PATH` only
//! reaches us when we were launched through the dispatcher, not when the binary
//! is run directly. Every `Command` therefore sets PATH explicitly to the same
//! list ui.sh exports. It is passed per-command rather than through
//! `std::env::set_var`, which is `unsafe` in edition 2024 for a good reason: the
//! process environment is global and the tick loop is not the only reader.
//!
//! # Sibling scripts
//!
//! `collect.sh` and `copy-link.sh` sit next to the **plugin root**, not next to
//! the binary (`bin/work-inbox` is one level down). They are resolved from
//! `current_exe()`'s parent's parent, with `$HERDR_PLUGIN_DIR` taking precedence
//! when herdr set it. Never hardcode `~/.config/...`: the whole tree is meant to
//! be relocatable, and the staging copy this was built in is proof that it moves.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::model::{Item, Kind};

/// The PATH ui.sh exports, verbatim. Built per call rather than cached in a
/// static: it is three allocations on a key press, and a `OnceLock` here would
/// only hide the `$HOME` read.
fn path_env() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut p = String::from("/opt/homebrew/bin:/usr/local/bin:");
    if !home.is_empty() {
        p.push_str(&home);
        p.push_str("/.local/bin:");
    }
    p.push_str("/usr/bin:/bin:/usr/sbin:/sbin");
    // An empty PATH element means the current directory, so the inherited value
    // is appended element by element with the empty ones dropped -- ui.sh guards
    // the same hazard with its `${PATH:+:${PATH}}`, and an inherited `a::b` gets
    // past that one.
    if let Ok(inherited) = std::env::var("PATH") {
        for el in inherited.split(':').filter(|e| !e.is_empty()) {
            p.push(':');
            p.push_str(el);
        }
    }
    p
}

/// The plugin root: where `collect.sh`, `copy-link.sh` and `ui.sh` live.
pub fn plugin_root() -> Option<PathBuf> {
    if let Some(d) = std::env::var("HERDR_PLUGIN_DIR").ok().filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d));
    }
    // bin/work-inbox -> bin -> <plugin root>
    let exe = std::env::current_exe().ok()?;
    exe.parent()?.parent().map(PathBuf::from)
}

fn sibling(name: &str) -> Option<PathBuf> {
    let p = plugin_root()?.join(name);
    if p.is_file() { Some(p) } else { None }
}

fn base(program: &str) -> Command {
    let mut c = Command::new(program);
    c.env("PATH", path_env());
    c
}

/// Spawn `collect.sh` **detached** and return the handle.
///
/// This is the fix for phase 1's worst key: fzf's `r` ran the collector
/// synchronously inside a `reload(...)` and froze the popup for the length of a
/// GitHub round trip. Here `r` returns within a frame; the tick loop notices the
/// new mtime and reloads.
///
/// Detached means two separate things, and BOTH are needed for the collector to
/// outlive the popup -- which is the entire promise of "collect on open so the
/// next open is fresh". A popup lives a few seconds; `collect.sh` runs 2-60s.
///
/// 1. **All three fds are `/dev/null`.** The popup's pty disappears when the app
///    exits, and a child still holding it would keep the popup on screen (and
///    could scribble into the drawn frame while it is up).
/// 2. **Its own process group** (`process_group(0)`). `ui.sh` `exec`s this
///    binary, so in production the TUI *is* the popup session's leader, and when
///    a session leader dies the kernel SIGHUPs the **foreground process group**
///    -- which is exactly where an inherited-group child sits. Rust not killing
///    children on drop is true and beside the point: the kernel does the killing.
///    A new group is not in the foreground, so no SIGHUP is delivered, and the
///    collector runs to completion after the popup is gone. This is precisely the
///    `nohup ... & disown` semantic `ui.sh:339-345` relies on, and it was
///    measured: without `process_group(0)` a collect spawned at startup dies the
///    instant the user presses `o`/`a`/`q`, silently, mid-fetch.
///
/// The child stays *this* process's child, so `try_wait` still reaps it (waitpid
/// is by pid, not by group) and the header spinner stays truthful rather than
/// being a guess with a timeout. On exit it is simply orphaned, not waited on.
pub fn spawn_collect() -> io::Result<std::process::Child> {
    let script = sibling("collect.sh")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "collect.sh not found"))?;
    base("/bin/bash")
        .arg(script)
        // std-only, no `unsafe`, stable since 1.64; the toolchain here is 1.97.
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// `/usr/bin/open <url>`. The caller exits afterwards.
///
/// Absolute path on purpose: this is macOS's `open`, not a Homebrew binary that
/// happens to share the name, and the herdr LaunchAgent runs in `gui/501` so the
/// LaunchServices round trip works from the service.
pub fn open_in_browser(url: &str) -> io::Result<()> {
    let st = base("/usr/bin/open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if st.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("open exited {st}")))
    }
}

/// `copy-link.sh <url>`. Returns whether a clipboard path actually ran.
///
/// **Terminal hazard.** copy-link.sh writes an OSC 52 escape directly to
/// `/dev/tty`, which this process owns in raw mode inside the alternate screen.
/// That is deliberate and must keep working -- OSC 52 is the only clipboard path
/// that reaches the client when herdr is driven with `--remote`. Two rules make
/// it safe:
///
/// * stdout and stderr are `/dev/null`. The script's own diagnostics would
///   otherwise land in the middle of the drawn frame; `/dev/tty` is opened by
///   the script itself and is unaffected by the redirection.
/// * the caller sets `App::needs_clear`, so the next frame repaints from
///   scratch. A terminal that does not understand OSC 52 may echo the payload,
///   and a repaint is cheaper than reasoning about which ones do.
pub fn copy_link(url: &str) -> io::Result<bool> {
    let script = sibling("copy-link.sh")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "copy-link.sh not found"))?;
    let st = base("/bin/bash")
        .arg(script)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(st.success())
}

/// `herdr notification show "work inbox" --body <text> --sound done|request`.
/// Best effort: a missing `herdr` is never an error here.
pub fn notify(body: &str, ok: bool) {
    let _ = base("herdr")
        .args(["notification", "show", "work inbox", "--body", body])
        .args(["--sound", if ok { "done" } else { "request" }])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// One row of `herdr agent list`, as the picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub pane_id: String,
    pub agent: String,
    pub agent_status: String,
    pub workspace_id: String,
    pub terminal_title: String,
    pub focused: bool,
}

/// Parse `herdr agent list`'s JSON (`.result.agents[]`).
pub fn list_agents() -> io::Result<Vec<Agent>> {
    let out = base("herdr")
        .args(["agent", "list"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "herdr agent list exited {}",
            out.status
        )));
    }
    Ok(parse_agents(&out.stdout))
}

/// Split out of [`list_agents`] so the shape can be tested without a herdr.
///
/// Unknown/missing fields degrade to empty strings rather than failing the whole
/// list: a hand-off that cannot happen because one agent grew a new field would
/// be a spectacularly annoying bug.
pub fn parse_agents(bytes: &[u8]) -> Vec<Agent> {
    let v: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.pointer("/result/agents").and_then(|a| a.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let s = |o: &serde_json::Value, k: &str| -> String {
        o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
    };
    arr.iter()
        .map(|o| Agent {
            pane_id: s(o, "pane_id"),
            agent: s(o, "agent"),
            agent_status: s(o, "agent_status"),
            workspace_id: s(o, "workspace_id"),
            terminal_title: {
                let t = s(o, "terminal_title_stripped");
                if t.is_empty() { s(o, "terminal_title") } else { t }
            },
            focused: o.get("focused").and_then(|x| x.as_bool()).unwrap_or(false),
        })
        .collect()
}

/// Resolve the hand-off target, porting phase 1's ranking exactly:
///
/// 1. the **caller pane** -- `HERDR_ACTIVE_PANE_ID`, else `HERDR_PANE_ID`.
///    Inside a popup `HERDR_PANE_ID` is the popup's own pane, so only the
///    `HERDR_ACTIVE_*` set identifies the caller. The pane_id match doubles as
///    caller validation.
/// 2. `.focused == true` -- ranked BELOW the caller because focus can move
///    between the keypress that opened the popup and the selection.
/// 3. the sole agent in the workspace (`HERDR_ACTIVE_WORKSPACE_ID`, else
///    `HERDR_WORKSPACE_ID`, else `herdr api snapshot`'s
///    `.result.snapshot.focused_workspace_id`) -- **only if there is exactly
///    one**.
///
/// Zero or several candidates returns `None`. Never inject a prompt into the
/// wrong agent's live turn.
pub fn resolve_agent(agents: &[Agent]) -> Option<String> {
    let caller = env_first(&["HERDR_ACTIVE_PANE_ID", "HERDR_PANE_ID"]);
    let ws = env_first(&["HERDR_ACTIVE_WORKSPACE_ID", "HERDR_WORKSPACE_ID"])
        .or_else(focused_workspace_id);
    rank(agents, caller.as_deref(), ws.as_deref())
}

/// The pure half of [`resolve_agent`], so the ranking is testable without a
/// herdr and without touching the process environment.
pub fn rank(agents: &[Agent], caller: Option<&str>, ws: Option<&str>) -> Option<String> {
    if let Some(c) = caller.filter(|c| !c.is_empty())
        && let Some(a) = agents.iter().find(|a| a.pane_id == c)
    {
        return Some(a.pane_id.clone());
    }
    if let Some(a) = agents.iter().find(|a| a.focused) {
        return Some(a.pane_id.clone());
    }
    if let Some(w) = ws.filter(|w| !w.is_empty()) {
        let mut in_ws = agents.iter().filter(|a| a.workspace_id == w);
        if let (Some(a), None) = (in_ws.next(), in_ws.next()) {
            return Some(a.pane_id.clone());
        }
    }
    None
}

fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
}

/// `herdr api snapshot | jq .result.snapshot.focused_workspace_id`, the last
/// fallback phase 1 uses. Still `herdr`, so still inside the allowed set.
fn focused_workspace_id() -> Option<String> {
    let out = base("herdr")
        .args(["api", "snapshot"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.pointer("/result/snapshot/focused_workspace_id")?
        .as_str()
        .map(str::to_string)
}

/// The hand-off text, verbatim from phase 1. It must not start with a dash:
/// neither `herdr` subcommand honours a `--` separator.
///
/// * pr   -> `Review this GitHub PR: <ref> <url>`
/// * jira -> `Work on Jira issue <ref>: <url>`
pub fn prompt_text(item: &Item) -> String {
    match item.kind {
        Kind::Pr => format!("Review this GitHub PR: {} {}", item.r#ref, item.url),
        Kind::Jira => format!("Work on Jira issue {}: {}", item.r#ref, item.url),
        _ => format!("Take a look at {}", item.url),
    }
}

/// `herdr pane send-text <pane> <text>` -- writes the text into the agent's
/// composer and stops there.
///
/// **Deliberately not `herdr agent prompt`**, which is the command that actually
/// submits. Phase 1 submitted, and that is the behaviour being replaced: the
/// hand-off is a starting point, not a finished instruction, and a prompt that
/// arrives already running gives you nothing to add to and no way to stop it. The
/// text lands in the input, the composer keeps the cursor, and pressing enter is
/// the user's decision.
///
/// One consequence worth knowing: `send-text` writes into whatever the pane's
/// composer currently holds rather than replacing it, so a half-typed message is
/// appended to, not lost.
///
/// Never pass `--wait`/`--until`/`--timeout` from a popup either way: it would
/// block on a settled state and freeze the UI.
pub fn send_to_agent(pane_id: &str, text: &str) -> io::Result<()> {
    let st = base("herdr")
        .args(["pane", "send-text", pane_id, text])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if st.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("herdr pane send-text exited {st}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(pane: &str, focused: bool, ws: &str) -> Agent {
        Agent {
            pane_id: pane.into(),
            agent: "claude".into(),
            agent_status: "idle".into(),
            workspace_id: ws.into(),
            terminal_title: String::new(),
            focused,
        }
    }

    #[test]
    fn the_caller_pane_outranks_focus() {
        let a = vec![agent("p1", true, "w1"), agent("p2", false, "w1")];
        // focus is on p1, but the popup was opened from p2
        assert_eq!(rank(&a, Some("p2"), Some("w1")).as_deref(), Some("p2"));
        // no caller -> focus wins
        assert_eq!(rank(&a, None, Some("w1")).as_deref(), Some("p1"));
        // a caller that is not an agent falls through rather than failing
        assert_eq!(rank(&a, Some("p9"), Some("w1")).as_deref(), Some("p1"));
    }

    #[test]
    fn a_sole_workspace_agent_is_the_last_resort_and_only_when_sole() {
        let one = vec![agent("p1", false, "w1")];
        assert_eq!(rank(&one, None, Some("w1")).as_deref(), Some("p1"));
        let two = vec![agent("p1", false, "w1"), agent("p2", false, "w1")];
        assert_eq!(rank(&two, None, Some("w1")), None, "two candidates -> picker");
        // an agent in a different workspace is not a candidate
        assert_eq!(rank(&one, None, Some("w2")), None);
        assert_eq!(rank(&[], Some("p1"), Some("w1")), None);
        // an empty caller/workspace is unset, not a match against ""
        assert_eq!(rank(&one, Some(""), Some("")), None);
    }

    #[test]
    fn agent_list_json_survives_missing_fields() {
        let js = r#"{"result":{"agents":[
          {"pane_id":"p1","agent":"claude","agent_status":"idle","workspace_id":"w",
           "terminal_title_stripped":"repo — claude","focused":true},
          {"pane_id":"p2"}
        ]}}"#
        .as_bytes();
        let a = parse_agents(js);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].terminal_title, "repo — claude");
        assert!(a[0].focused);
        assert_eq!(a[1].pane_id, "p2");
        assert_eq!(a[1].agent, "");
        assert!(!a[1].focused);

        // a herdr that returns nothing useful is an empty list, not a panic
        assert!(parse_agents(b"").is_empty());
        assert!(parse_agents(b"{}").is_empty());
        assert!(parse_agents(br#"{"result":{"agents":null}}"#).is_empty());
    }

    /// The real `herdr agent list` payload carries fourteen keys per agent, of
    /// which this parser reads six. The shape below is the real one -- key set,
    /// nesting and types checked against a live `herdr agent list` on this
    /// machine -- with the values replaced, because the real titles are work
    /// content and this file is meant to be committable.
    #[test]
    fn the_real_agent_list_shape_parses() {
        let js = r#"{"id":"cli:agent:list","result":{"agents":[
          {"agent":"claude",
           "agent_session":{"agent":"claude","kind":"id","source":"herdr:claude",
                            "value":"61e8484f-11e3-4a1d-af93-af1d67ba64da"},
           "agent_status":"idle","cwd":"/repo","focused":false,
           "foreground_cwd":"/repo","pane_id":"w1:pA","revision":14,
           "state_change_seq":274,"tab_id":"w1:t7","terminal_id":"term_658bd",
           "terminal_title":"\u2733 a title","terminal_title_stripped":"a title",
           "workspace_id":"w1"},
          {"agent":"claude",
           "agent_session":{"agent":"claude","kind":"id","source":"herdr:claude",
                            "value":"205d63ef-f336-4fb7-be52-43fbf2438201"},
           "agent_status":"busy","cwd":"/repo2","focused":true,
           "foreground_cwd":"/repo2","pane_id":"w2:pC","revision":4,
           "state_change_seq":170,"tab_id":"w2:t8","terminal_id":"term_658bf",
           "terminal_title":"\u2733 another","terminal_title_stripped":"another",
           "workspace_id":"w2"}
        ]}}"#
        .as_bytes();
        let a = parse_agents(js);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].pane_id, "w1:pA");
        assert_eq!(a[0].workspace_id, "w1");
        // the stripped title is preferred: the raw one carries the ✳ status glyph
        assert_eq!(a[0].terminal_title, "a title");
        assert!(!a[0].focused && a[1].focused);
        // no caller pane -> the single focused agent wins, which is exactly the
        // resolution that must NOT be exercised against a live herdr in a test
        assert_eq!(rank(&a, None, None).as_deref(), Some("w2:pC"));
        // a caller pane still outranks it
        assert_eq!(rank(&a, Some("w1:pA"), None).as_deref(), Some("w1:pA"));
    }

    #[test]
    fn prompt_text_matches_phase1_and_never_starts_with_a_dash() {
        let mut pr = Item {
            kind: Kind::Pr,
            r#ref: "acme/api#12".into(),
            url: "https://example/pr".into(),
            ..Default::default()
        };
        assert_eq!(
            prompt_text(&pr),
            "Review this GitHub PR: acme/api#12 https://example/pr"
        );
        pr.kind = Kind::Jira;
        pr.r#ref = "G-1".into();
        pr.url = "https://example/jira".into();
        assert_eq!(prompt_text(&pr), "Work on Jira issue G-1: https://example/jira");
        assert!(!prompt_text(&pr).starts_with('-'));
    }

    #[test]
    fn path_env_starts_with_the_list_ui_sh_exports() {
        let p = path_env();
        assert!(p.starts_with("/opt/homebrew/bin:/usr/local/bin:"));
        assert!(p.contains(":/usr/bin:/bin:/usr/sbin:/sbin"));
        // never an empty element, which would mean the current directory
        assert!(!p.split(':').any(str::is_empty), "{p}");
    }
}
