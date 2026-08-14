//! `work-inbox` -- the phase 2 front end for the `jin.work-inbox` herdr plugin.
//!
//! Replaces the fzf front end (`ui.sh`) and nothing else: the collector, the
//! cache and the `prefix+i` popup binding are unchanged. This process reads
//! `cache.json`, draws it, and shells out for the four side effects it is
//! allowed (see `actions`). **It never touches the network.**
//!
//! Two entry points:
//!
//! * `work-inbox` -- the TUI.
//! * `work-inbox --dump` -- parse the cache, print the sections and boards with
//!   their counts and the first few rows as plain text, exit. No terminal setup
//!   at all, so it works without a pty: it is how reviewers and CI check a build,
//!   and it is the startup-timing probe (`time work-inbox --dump` is dominated by
//!   exactly the work that precedes the TUI's first frame).

mod actions;
mod app;
mod cache;
mod config;
mod input;
mod model;
mod view;

use std::io::{self, Write};

use model::{BOARDS, Cache, SECTIONS, Section};

/// How many rows of each section/column `--dump` prints before eliding.
const DUMP_ROWS: usize = 3;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dump = false;

    for a in &args {
        match a.as_str() {
            "--dump" => dump = true,
            "-h" | "--help" => {
                print_usage(&mut io::stdout());
                return;
            }
            "-V" | "--version" => {
                println!("work-inbox {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            other => {
                eprintln!("work-inbox: unknown argument {other:?}");
                print_usage(&mut io::stderr());
                std::process::exit(2);
            }
        }
    }

    if dump {
        std::process::exit(run_dump());
    }

    std::process::exit(run_tui());
}

/// The interactive front end.
///
/// The order here is load-bearing:
///
///  1. **load the cache first**, before the terminal is touched at all. A
///     missing or unparseable one is a DRAWN state (`view::draw_error`), not an
///     exit -- and doing it before raw mode means a bug in the parser cannot
///     wreck the pane, because there is nothing to restore yet.
///  2. `ratatui::try_init()`, which installs a panic hook that leaves the
///     alternate screen and disables raw mode **before** it enables either. From
///     this line on, a panic anywhere still hands the user a working terminal.
///  3. draw the first frame. Nothing blocks between 1 and 3: the budget is fzf's
///     0.063s and the only work is a ~180KB `serde_json` parse.
///  4. spawn one detached `collect.sh`, like ui.sh does today -- AFTER the first
///     frame, never before it.
///  5. run the loop, then restore on **every** exit path. `let r = run(); restore();
///     r` rather than `run()?; restore();` -- an `Err` out of the loop that
///     skipped the restore would leave the pane in raw mode inside the alternate
///     screen, which is precisely the failure the contract forbids.
fn run_tui() -> i32 {
    let path = cache::cache_path();
    let mut app = app::App::new(path);

    let mut terminal = match ratatui::try_init() {
        Ok(t) => t,
        Err(e) => {
            // No pty (a scripted run, a herdr [[actions]] command). Say so on
            // stderr and leave the terminal alone; --dump is the headless path.
            eprintln!("work-inbox: cannot start the TUI: {e}");
            eprintln!("work-inbox: run with --dump for a headless view of the cache");
            return 70;
        }
    };

    // The first frame, before anything is spawned. `--dump`'s timing is a proxy
    // for this moment and this is the moment the budget is about.
    // `.map(|_| ())` drops the CompletedFrame, which borrows the terminal: the
    // loop below needs it back.
    let first = terminal.draw(|f| view::draw(f, &mut app)).map(|_| ());

    if first.is_ok() {
        // Refresh for next time, exactly as ui.sh does on every open. Detached:
        // the tick reaps it and the header shows a spinner while it runs.
        app.start_collect();
    }

    let result = first.and_then(|_| app.run(&mut terminal));
    ratatui::restore();

    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("work-inbox: {e}");
            1
        }
    }
}

fn print_usage<W: Write>(w: &mut W) {
    let _ = writeln!(
        w,
        "usage: work-inbox [--dump]\n\
         \n\
         (no arguments)  open the inbox TUI\n\
         --dump          print the parsed cache as plain text and exit\n\
         -h, --help      this message\n\
         -V, --version   version\n\
         \n\
         The cache is $HERDR_PLUGIN_STATE_DIR/cache.json, falling back to\n\
         ${{XDG_STATE_HOME:-$HOME/.local/state}}/herdr/plugins/jin.work-inbox/cache.json."
    );
}

/// Headless dump. Returns the process exit code: 0 on a readable cache, 1 when
/// it is missing or unparseable (which is a real failure for a checker, even
/// though the TUI draws it as a frame rather than exiting).
fn run_dump() -> i32 {
    let path = cache::cache_path();
    let loaded = match cache::load_from(&path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("work-inbox: {e}");
            return 1;
        }
    };
    // The dump shows what the TUI SHOWS, so the user's display filter applies
    // here too -- a dump that quietly ignored it would cross-foot against the
    // cache and disagree with the screen. What it must not do is apply it
    // silently: the `hidden` line below is what keeps the two readings
    // reconcilable.
    let cfg = config::Config::load();
    let mut c = loaded.cache.clone();
    let before = c.known().count();
    c.items.retain(|i| cfg.allows(i));
    let hidden = before - c.known().count();

    let out = io::stdout();
    let mut w = io::BufWriter::new(out.lock());
    write_dump(&mut w, &c, &path.display().to_string(), cache::now_unix());
    if hidden > 0 {
        let _ = writeln!(
            w,
            "\nhidden       {} items ({} repos, {} projects) -- press c in the TUI",
            hidden,
            cfg.hidden_repos.len(),
            cfg.hidden_projects.len()
        );
    }
    let _ = w.flush();
    0
}

/// The dump itself, factored out of `run_dump` so it can be exercised against a
/// fixture without a filesystem or a clock.
fn write_dump<W: Write>(w: &mut W, c: &Cache, path: &str, now: i64) {
    let (n_jira, n_review, n_mine) = c.header_counts();
    let _ = writeln!(w, "cache        {path}");
    let _ = writeln!(w, "version      {}", c.version);
    // The item total counts the items this build would DRAW, not the raw array:
    // `--dump` is the verifier's window into what the TUI shows, and a total that
    // included a kind the views drop would make the two disagree.
    let _ = writeln!(
        w,
        "updated      {} ({} jira / {} review / {} mine, {} items)",
        c.age(now),
        n_jira,
        n_review,
        n_mine,
        c.known().count()
    );
    for name in ["github", "jira"] {
        let s = c.source_status(name);
        let _ = writeln!(
            w,
            "source       {:<7} ok={:<5} fetched={} note={:?}",
            name,
            s.ok,
            s.fetched_unix
                .map_or_else(|| "(inherited)".to_string(), |v| v.to_string()),
            s.note
        );
    }

    let _ = writeln!(w, "\nLIST");
    for section in &SECTIONS {
        let items = c.items_in(section);
        let _ = writeln!(w, "== {} ({})", section.header(), items.len());
        let state = c.source_state(section);
        if let Some(b) = state.banner(now) {
            let _ = writeln!(w, "   !! {b}");
        } else if !state.note.is_empty() {
            let _ = writeln!(w, "   !  {}", state.note);
        }
        if items.is_empty() {
            let _ = writeln!(w, "   ({})", section.empty_message());
        }
        for it in items.iter().take(DUMP_ROWS) {
            let _ = writeln!(w, "   {}", it.list_row());
        }
        if items.len() > DUMP_ROWS {
            let _ = writeln!(w, "   ... {} more", items.len() - DUMP_ROWS);
        }
    }

    let _ = writeln!(w, "\nKANBAN");
    for board in BOARDS {
        let section: Section = board.section();
        let _ = writeln!(
            w,
            "== {} ({})",
            board.header(),
            c.count_in(&section)
        );
        if let Some(b) = c.source_state(&section).banner(now) {
            let _ = writeln!(w, "   !! {b}");
        }
        for (col, items) in c.board_columns(board) {
            let _ = writeln!(w, "   -- {} ({})", col.header(), items.len());
            for it in items.iter().take(DUMP_ROWS) {
                // Same two status slots the TUI card leads with, so a dump and a
                // screenshot describe the same card.
                let (g1, g2) = it.status_glyphs();
                let _ = writeln!(
                    w,
                    "      {}{} {} {}",
                    g1,
                    g2,
                    model::pad_cols(&it.r#ref, 24),
                    it.card_title(48),
                );
            }
            if items.len() > DUMP_ROWS {
                let _ = writeln!(w, "      ... {} more", items.len() - DUMP_ROWS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dump is the reviewer's and the verifier's only window into the data
    /// layer, so its shape is pinned: section counts, board counts, the stale
    /// banner and the elision line all have to appear.
    #[test]
    fn dump_reports_sections_boards_and_staleness() {
        let json = r#"{
          "version": 1, "fetched_unix": 1000,
          "sources": {
            "github": {"ok": true, "note": "", "fetched_unix": 1000},
            "jira": {"ok": false, "note": "jira: unreachable after 3 tries (curl exit 56)",
                     "fetched_unix": 400}
          },
          "items": [
            {"kind":"pr","section":"review","ref":"a/b#1","title":"one","updated":"2026-08-11T00:00:00Z","author":"x","review_decision":"APPROVED"},
            {"kind":"pr","section":"review","ref":"a/b#2","title":"two","updated":"2026-08-11T00:00:00Z","author":"x"},
            {"kind":"pr","section":"mine","ref":"a/b#3","title":"three","updated":"2026-08-11T00:00:00Z","author":"me","draft":true,"checks":"FAILURE"},
            {"kind":"jira","section":"jira","ref":"G-1","key":"G-1","title":"j1","status":"進行中","status_category":"In Progress","type":"Task","priority":"Medium","updated":"2026-08-11T00:00:00Z"},
            {"kind":"jira","section":"jira","ref":"G-2","key":"G-2","title":"j2","status":"To Do","status_category":"To Do","type":"Bug","priority":"High","updated":"2026-08-11T00:00:00Z"},
            {"kind":"jira","section":"jira","ref":"G-3","key":"G-3","title":"j3","status":"To Do","status_category":"To Do","type":"Bug","priority":"High","updated":"2026-08-11T00:00:00Z"},
            {"kind":"jira","section":"jira","ref":"G-4","key":"G-4","title":"j4","status":"完了","status_category":"Done","type":"Bug","priority":"Low","updated":"2026-08-11T00:00:00Z"}
          ]
        }"#;
        let c: Cache = serde_json::from_str(json).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_dump(&mut buf, &c, "/tmp/cache.json", 1000 + 600);
        let s = String::from_utf8(buf).unwrap();

        assert!(s.contains("updated      10m ago (4 jira / 2 review / 1 mine, 7 items)"), "{s}");
        assert!(s.contains("== REVIEW REQUESTED (2)"));
        assert!(s.contains("== MY PULL REQUESTS (1)"));
        assert!(s.contains("== MY JIRA ISSUES (4)"));
        // the empty-state line for a healthy but empty section is absent here,
        // but the stale banner for the failed jira leg is not -- and it carries
        // the LEG's age (1600-400), not the cache's.
        assert!(s.contains("!! jira: stale, 20m ago — jira: unreachable"), "{s}");
        // kanban buckets
        assert!(s.contains("-- NEEDS REVIEW (1)"));
        assert!(s.contains("-- CHANGES REQUESTED (0)"));
        assert!(s.contains("-- APPROVED (1)"));
        assert!(s.contains("-- DRAFT (0)")); // the draft PR has a failed run
        assert!(s.contains("-- CI FAILED (1)"));
        assert!(s.contains("-- TO DO (2)"));
        assert!(s.contains("-- IN PROGRESS (1)"));
        assert!(s.contains("-- DONE (1)"));
        // elision
        assert!(s.contains("... 1 more"), "{s}");
    }

    /// `--dump` is the verifier's window into what the TUI draws, so its counts
    /// have to be the TUI's counts. An item whose `kind` the views drop must not
    /// be counted in the header, the section header, the board header or the
    /// item total -- otherwise the dump says 2 where one row is drawn.
    #[test]
    fn dump_counts_only_the_items_the_views_can_draw() {
        let c: Cache = serde_json::from_str(
            r#"{"version":1,"fetched_unix":1000,
                "sources":{"github":{"ok":true,"note":""},"jira":{"ok":true,"note":""}},
                "items":[
                  {"kind":"pr","section":"review","ref":"a/b#1","title":"one","author":"x"},
                  {"kind":"discussion","section":"review","ref":"a/b#2","title":"future kind"}
                ]}"#,
        )
        .unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_dump(&mut buf, &c, "/tmp/cache.json", 1000);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("0 jira / 1 review / 0 mine, 1 items"), "{s}");
        assert!(s.contains("== REVIEW REQUESTED (1)"), "{s}");
        assert!(s.contains("-- NEEDS REVIEW (1)"), "{s}");
        assert!(!s.contains("a/b#2"), "an undrawable kind must not appear\n{s}");
    }

    #[test]
    fn dump_of_an_empty_cache_still_names_every_section() {
        let c = Cache::default();
        let mut buf: Vec<u8> = Vec::new();
        write_dump(&mut buf, &c, "/tmp/none.json", 0);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("== REVIEW REQUESTED (0)"));
        assert!(s.contains("no PRs are waiting on your review"));
        assert!(s.contains("no unresolved issues are assigned to you"));
        // a default cache has ok:false with no items and no note on both legs:
        // the banner falls back rather than printing an empty warning
        assert!(s.contains("github: failed for an unknown reason"));
        assert!(s.contains("jira: failed for an unknown reason"));
    }
}
