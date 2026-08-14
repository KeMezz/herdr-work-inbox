# work-inbox (`jin.work-inbox`)

One list of everything waiting on you at work: GitHub pull requests where your
review is requested (directly or through a team), pull requests you opened, and
your unresolved Jira issues. Bound to `prefix+i`.

It exists as a plugin because the popup it replaced did all its network work
while you waited. Fetching now happens ahead of time, into a cache; the popup
only reads that cache. Opening went from ~1.3s to the cost of reading one file,
and the detail pane — which used to fire a `gh pr view` or a Jira REST call
on every cursor move — costs nothing at all: the bodies are already cached.

The front end is a Rust TUI (phase 2). fzf is still here as the fallback for
when the binary is not built.

## The parts

**Collector → cache → front end.** The collector talks to the network and
nothing else; `cache.json` is the only contract between the halves; the front
end draws and never fetches — the binary has no HTTP client in its dependency
tree at all, it reads the cache and shells out. Everything else here is a
helper for one of those three.

| Part | What it is | When it runs |
|---|---|---|
| `collect.sh` | The collector. One GitHub GraphQL query and one Jira REST call, both including item bodies, written atomically to `cache.json`. | The `refresh` action, the `worktree.created` event, and a background kick from the front end. |
| `tui/` → `bin/work-inbox` | The front end. A ratatui binary: reads `cache.json`, draws the list and the kanban boards, polls the cache's mtime and reloads when a background collect lands. Spawns only `collect.sh`, `copy-link.sh`, `/usr/bin/open` and `herdr`. | Via `ui.sh`, which execs it. |
| `ui.sh` | Dispatcher, and the fallback front end. Execs `bin/work-inbox` if it is there; otherwise prints one line to stderr and runs the phase 1 fzf implementation, which is kept verbatim below the dispatch block. A broken or unbuilt binary must never leave `prefix+i` dead. | `prefix+i`, as a `type = "popup"` keybinding in `herdr/config.toml`. |
| `build.sh` | Builds the crate and installs `bin/work-inbox`. Run by hand — see below. | Manually, after a checkout or a change to `tui/`. |
| `copy-link.sh` | Puts a link on the **local** machine's clipboard: OSC 52 to `/dev/tty` first, `pbcopy` as well. | `y` in the TUI (`ctrl-y` in the fzf fallback). |
| `lib/common.sh` | What the collector needs: paths, credential loading, the ADF-to-text converter. Defines only — sourcing it creates nothing and reads nothing. | Sourced by `collect.sh` alone. `ui.sh`, `copy-link.sh` and the binary source nothing and carry their own PATH/state-dir lines, so the front end can never reach the credential loader. |

The front end is still a user-config popup rather than a `[[panes]]`
entrypoint because `herdr plugin pane open` (0.8.0) has no size options and
this list needs the popup's 90%/80%. The manifest therefore declares a build
step, an action and an event, and no panes.

Plugin action commands run **headless**: no tty, `TERM=dumb`. `collect.sh` must
never start fzf or anything else interactive. herdr also gives plugin commands
a minimal `PATH`, so every script here exports its own.

## Install

```bash
herdr plugin install KeMezz/herdr-work-inbox
```

`[[build]]` compiles the TUI on install, so the first one takes about twenty
seconds. Then bind the popup — the front end needs a real tty, which plugin
**actions** do not get, so it is a `type = "popup"` keybinding rather than a
`[[panes]]` entrypoint (and `herdr plugin pane open` has no size options, which
this three-section list with a preview needs):

```toml
[[keys.command]]
key = "prefix+i"
type = "popup"
command = "$HOME/.config/herdr/scripts/work-inbox.sh"
description = "work inbox (PR reviews + Jira)"
width = "90%"
height = "80%"
```

where that script resolves the installed plugin and execs its front end — the
managed install path carries a hash, so it cannot be written down:

```bash
#!/bin/bash
root=$(herdr plugin list --plugin jin.work-inbox --json \
       | jq -r '.result.plugins[0].plugin_root')
[ -n "$root" ] && [ "$root" != "null" ] || { echo "work-inbox: not installed" >&2; exit 1; }
exec "$root/ui.sh" "$@"
```

Then set up the Jira credential (see **Where things live**) and fetch once:

```bash
herdr plugin action invoke refresh --plugin jin.work-inbox
```

### Requirements

| | |
|---|---|
| herdr | 0.8.0+ |
| macOS | BSD `stat -f` in the collector, `/usr/bin/open` for links. Not tried on Linux. |
| `gh` | authenticated; GitHub auth is entirely its business |
| `jq` `curl` | the collector |
| `cargo` | to build the front end |
| `fzf` | optional — the fallback front end, see below |

### Rebuilding, and the fallback

`build.sh` runs `cargo build --release` in `tui/` and installs the binary as
`bin/work-inbox`, staged as `work-inbox.new` and renamed, so rebuilding while
the popup is open is safe.

`ui.sh` is a dispatcher: it execs `bin/work-inbox` when it is there, and
otherwise runs the fzf front end this project started as. So a failed or skipped
build degrades the UI instead of breaking the keybinding — and **removing
`bin/work-inbox` is the instant rollback** of the whole TUI, with no rebuild and
nothing to restart.

## Keys

Nav mode:

| Key | |
|---|---|
| `j` `k` | move — through the whole list, across section boundaries; within one kanban column |
| `h` `l` | previous / next section, or column |
| `enter` `space` | preview mode |
| `o` | open in the browser, then close |
| `y` | copy the link, stay open |
| `a` | put it in the agent's input — does **not** submit — then close |
| `v` | list ↔ kanban |
| `Tab` `shift+Tab` | next / previous board (kanban only) |
| `c` | which repositories and projects this panel shows |
| `r` | refresh — spawns a detached `collect.sh`, never blocks |
| `/` | filter by ref + title |
| `q` `esc` | close |

In preview mode `j`/`k` scroll, `ctrl-d`/`ctrl-u` half-page, `g`/`G` jump, and
`esc` returns to nav. `ctrl-o` (agent) and `ctrl-y` (copy) still work as phase 1
aliases. Only `y` keeps the popup open; `o` and `a` are terminal actions.

`a` writes `Review this GitHub PR: <ref> <url>` into the agent's composer with
`herdr pane send-text` and stops there. It used to submit, via `herdr agent
prompt`, and that was wrong: the hand-off is a starting point, not a finished
instruction, and a prompt that arrives already running gives you nothing to add
and no way to stop it. Pressing enter is yours. (`send-text` appends to whatever
the composer holds rather than replacing it, so a half-typed message survives.)

Typing in `/` filters as you go and carries the cursor to the match. `enter`
applies the filter and returns to nav with it still in force; `esc` **while
still in `/`** clears it. In nav, `esc` closes the popup — so clearing an
applied filter is `/` then `esc`, which is what the filter line says.

`work-inbox --dump` prints the sections, the boards and the first rows as plain
text and exits — no tty needed. It is the way to check the front end from a
script, a review, or an agent.

## Choosing what the panel shows

`c` opens a list of every GitHub repository and Jira project in the cache, each
with a checkbox and its item count. `space` toggles one, `A` shows everything
again, `esc` closes. There is no save key — a toggle is on disk before the
keypress is over.

It is a **deny** list: what gets stored is what to hide, so a repository nobody
has touched is shown. A review request from a repo that did not exist when the
config was written can therefore never be silently missing, which is the one
failure this panel must not have. Entries survive a repo dropping out of the
cache, so a repo with no open PRs today does not quietly un-hide itself tomorrow.

The header keeps saying how many items are hidden, and `work-inbox --dump`
prints a `hidden` line, so a filtered inbox never reads as an empty one.

The view you were last in — list or kanban, and which board — comes back on the
next open. Same file.

## Markdown in the preview

PR descriptions are GitHub-flavoured markdown, and the ADF-to-text converter in
`lib/common.sh` emits the same shapes for Jira, so one renderer covers both:
headings, bold/italic/strikethrough, inline code, fenced code with a gutter,
nested bullet and ordered lists, task list checkboxes, block quotes, rules,
tables, and links (text plus the URL — a terminal cell cannot carry an OSC 8
hyperlink through ratatui's buffer).

No syntax highlighting: it needs a second parser and a theme to keep in sync
with the terminal's own, for text read in a 40-column pane. Wrapping is measured
in display columns, in the renderer, because the preview scroll needs an exact
line count and only the renderer knows where it broke.

## Status at a glance

Every row and every card leads with two fixed slots, so the state of the whole
inbox is one column you scan down rather than words you read across.

| Slot 1 — where it stands | | Slot 2 — CI | |
|---|---|---|---|
| 🔴 | changes requested | ❌ | a check failed |
| 📝 | your draft | ⏳ | still running |
| ✅ | approved | | *blank when green — "CI is fine" is not news* |
| 👀 | waiting on a review | | |
| ⚪ 🔵 🏁 | Jira: To Do / In Progress / Done | | |

Slot 1 takes the most actionable state first: changes requested outranks draft,
because somebody is waiting on you; draft outranks approved, because an approved
draft still cannot merge. Slot 2 is CI and only speaks up when CI is a problem.

Jira keys on `status_category`, never on the status name — this tenant runs an
English `To Do` beside a Japanese `進行中` — but the card and the row always print
the tenant's own wording, colour-coded by that category.

Every glyph is two display columns wide, with no variation selector, and a test
asserts it: a narrow one would shear every row beneath it, and only on the
machine whose font disagreed.

## Where things live

- **Cache** — `~/.local/state/herdr/plugins/jin.work-inbox/cache.json`
  (`$HERDR_PLUGIN_STATE_DIR` when herdr sets it; the popup is not a plugin
  command, so it falls back to the literal path). Directory `0700`, file
  `0600`: the cache holds PR descriptions and Jira ticket bodies, which is
  work-confidential text that never used to leave a `mktemp` directory.
- **Jira credential** — `~/.local/state/herdr-work-inbox/env`, unchanged from
  the old script. `JIRA_BASE_URL`, `JIRA_EMAIL`, `JIRA_API_TOKEN`. It sits
  outside the plugin directory so that a dotfiles repo cannot swallow it, and
  because `herdr plugin install` replaces the plugin tree wholesale on update.
  The file is sourced, so it is refused unless it is a regular file owned by you and not
  group- or world-writable, and the token is handed to `curl` on stdin via
  `--config -` so it never appears in `ps`.
- **GitHub credential** — none of our business; `gh` handles its own auth.
- **Config** — `~/.config/herdr/plugins/config/jin.work-inbox/config.json`
  (`$HERDR_PLUGIN_CONFIG_DIR` when herdr sets it). Written by the TUI, not by
  hand: the hidden repositories and projects, and the view it was last in.
  Machine-local: it is written by the TUI and never checked in anywhere.
  Deleting it resets the panel; it is never needed to run.

  The kanban **columns** are still not configurable, and that decision stands:
  they key on `status_category`, not on the tenant's status names, which is the
  only thing a column config would have existed to absorb.
- **Binary** — `bin/work-inbox`, built from `tui/` by `build.sh`. Not tracked,
  not shipped: it is rebuilt on each machine.

## Refresh by hand

```bash
herdr plugin action invoke refresh --plugin jin.work-inbox
```

or, outside herdr entirely:

```bash
bash "$(herdr plugin list --plugin jin.work-inbox --json | jq -r '.result.plugins[0].plugin_root')/collect.sh"
```

`collect.sh --only github` and `--only jira` refresh one leg and leave the
other half of the cache alone — useful when one source is down or when you are
only interested in what one of them returns.

Inside the popup, `r` spawns a detached collect and returns immediately; the
TUI watches `cache.json`'s mtime and redraws when the new file lands, so a
refresh started anywhere — the action, a worktree event, another window —
shows up while the popup is open. (The fzf fallback cannot do that: there `r`
blocks until the collect finishes.) One collect is also kicked off in the
background every time the popup opens, so the *next* open is fresh.

## Staleness

A leg that fails **keeps the items it fetched last time**. This tenant produces
intermittent `curl exit 56`, and blanking the Jira list on a transient network
blip is worse than showing yesterday's issues with a label on them.

Each source therefore carries its own `fetched_unix` in the cache, holding the
time of its last *successful* fetch, and the top-level `fetched_unix` is the
newest of the two. `ok:false` with items retained means "stale, and here is
why"; `ok:false` with no items (a leg that has never succeeded) means an empty
section plus the note. Both draw a banner in the failing section or board:

```
jira: stale, 41m ago — jira: unreachable after 3 tries (curl exit 56) - network, not auth
```

The cache is still `version: 1` — the per-leg timestamps were added
additively, so the fzf fallback reads a phase 2 cache without complaint
(verified against the stale-leg fixtures). It just cannot *show* the retained
items: phase 1's renderer prints the note in place of a failed source's rows.
One more reason the fallback is a fallback.

## Debugging

The action produces no terminal output — it is headless. Read the plugin log:

```bash
herdr plugin log list --plugin jin.work-inbox --limit 5 \
  | jq -r '.result.logs[] | "\(.status) exit=\(.exit_code)\n\(.stderr)"'
```

Then look at what it wrote:

```bash
CACHE=~/.local/state/herdr/plugins/jin.work-inbox/cache.json
stat -f '%Lp %Sm %z %N' "$CACHE"
jq '{fetched_unix, sources, n: (.items | length)}' "$CACHE"
```

`sources.github.note` and `sources.jira.note` carry the human explanation for a
failed leg — the same text the popup shows inline.

To see the front end's own errors, run it from a normal terminal instead of
from the popup — or without a terminal at all. `$ROOT` is wherever herdr
installed the plugin:

```bash
ROOT=$(herdr plugin list --plugin jin.work-inbox --json | jq -r '.result.plugins[0].plugin_root')
bash "$ROOT/ui.sh"                # the front end: TUI, or fzf if unbuilt
"$ROOT/bin/work-inbox" --dump     # headless, no tty needed
```

If the TUI misbehaves, `rm bin/work-inbox` puts the fzf front end back on the
next `prefix+i` — no rebuild, no config change, no reload. Note that the
fallback keeps the **phase 1** keys: `enter` opens, `ctrl-y` copies.
