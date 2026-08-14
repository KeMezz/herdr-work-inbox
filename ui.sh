#!/bin/bash
#
# jin.work-inbox -- interactive front end. Launched by the herdr popup keybinding
# (type = "popup", 90% x 80%), so it HAS a tty but is NOT a plugin command: none
# of the HERDR_PLUGIN_* variables are set and the state directory has to be
# resolved from the same defaults herdr itself uses.
#
# Since phase 2 this file is TWO things: a dispatcher to the Rust TUI at
# bin/work-inbox (see the `dispatch` block below), and -- when that binary is
# not built -- the phase 1 fzf front end it replaced, kept verbatim as the
# fallback. Everything after the dispatch block is that fallback.
#
# The whole point of this file is that it does NOT touch the network on the fast
# path. It reads $STATE_DIR/cache.json, which collect.sh (headless, run from a
# [[actions]] command, a [[events]] hook or from here in the background) keeps
# fresh, and renders it. The old work-inbox.sh spent ~1.3s in GitHub's GraphQL
# API before drawing a single row; this draws from a warm cache in tens of ms.
#
# Two consequences follow from being cache-only and are deliberate:
#   * the preview pane reads the body out of field 5 of the row -- there is no
#     --preview self-invocation and no per-item network call any more;
#   * what you see is as fresh as the last collect. The age is in the header, and
#     `r` forces a synchronous refresh.
#
# No credential is read here. ui.sh never sources the env file and never learns
# the Jira token; only collect.sh does.

set -u

# herdr runs its children with a minimal PATH, and the popup inherits it. The
# `:+` form rather than `:-`: an exported-but-empty PATH would otherwise leave a
# trailing colon, and an empty PATH element means the current directory.
export PATH="/opt/homebrew/bin:/usr/local/bin:${HOME}/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:${PATH}}"

# Absolute self-path, resolved before any cd: fzf bindings re-invoke this script
# and the popup's cwd is the caller's repo, not the plugin root.
SELF=$(cd "$(dirname "$0")" >/dev/null 2>&1 && printf '%s/%s' "$(pwd)" "$(basename "$0")")
PLUGIN_ROOT=$(dirname "$SELF")
COLLECT="$PLUGIN_ROOT/collect.sh"
COPY_LINK="$PLUGIN_ROOT/copy-link.sh"

# -------------------------------------------------------------------- dispatch
#
# Phase 2 moved the front end into a Rust TUI at bin/work-inbox. This script is
# now a dispatcher: if the binary is there, it IS the front end and we hand the
# process over to it; if it is not, everything below still runs.
#
# The fallback is the point. `herdr plugin link` skips [[build]] steps, so the
# binary only exists after someone ran build.sh by hand -- a fresh clone, a
# failed build or an interrupted install all leave it missing. The popup must
# never die because a build broke, and the fzf implementation below is the one
# that shipped in phase 1 and is known to work. Both paths accept --dump.
#
# The fallback is NOT key-compatible with the TUI: it keeps the PHASE 1 keys,
# where `enter` opens in the browser and `ctrl-y` copies, whereas the TUI opens
# with `o`, copies with `y` and uses enter/space for its preview mode. That is
# deliberate -- reworking fzf's keymap to imitate the TUI would mean maintaining
# a second keymap in the path that only runs when something is already wrong.
#
# exec, not a call: the popup's pty and exit status belong to whatever draws the
# list, and there is no reason to keep a bash parent alive for the whole session.
# The PATH export above survives into the binary, which is how the processes IT
# spawns (collect.sh, copy-link.sh, herdr) find gh/jq/curl under herdr's minimal
# environment.
#
# --render and --nav are excluded on purpose: they are fzf implementation
# details, re-invocations of THIS script from a binding created by the fallback
# path below. If a build lands while a fallback popup is open, `r` would
# otherwise exec the TUI inside fzf's reload and feed a terminal program's
# output into the list.
case "${1:-}" in
  --render|--nav) : ;;
  *)
    if [ -x "$PLUGIN_ROOT/bin/work-inbox" ]; then
      exec "$PLUGIN_ROOT/bin/work-inbox" "$@"
    fi
    printf 'work inbox: TUI not built, using the fzf fallback -- build it with %s/build.sh\n' \
      "$PLUGIN_ROOT" >&2
    ;;
esac

# HERDR_PLUGIN_STATE_DIR is only set for plugin commands. A popup front end has
# to reproduce the default itself -- and honouring the variable when it IS set
# is what makes this script testable without writing into the real state dir.
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/jin.work-inbox}"
CACHE="$STATE_DIR/cache.json"

dump=0

fail() {
  herdr notification show "work inbox" --body "$1" --sound request >/dev/null 2>&1 || true
  printf 'work inbox: %s\n' "$1" >&2
  # Keep the popup up briefly so the message is readable before it closes. Not
  # in --dump, which is scripted and must not stall.
  [ "$dump" -eq 0 ] && sleep 1.5
  exit 1
}

command -v jq >/dev/null 2>&1 || fail "jq is not available on PATH"

# --------------------------------------------------------------------- renderer
#
# Every row is 5 tab-separated fields; fzf shows and searches only field 1
# (--with-nth=1), so the hidden fields stay reachable from the bindings:
#
#   1  display string (may contain SGR escapes; --ansi is on)
#   2  kind tag: pr | jira | sep | note
#   3  URL          (what enter opens, what ctrl-y copies)
#   4  short ref    (what the agent hand-off quotes)
#   5  preview body, with U+001F standing in for newlines
#
# Rows are emitted with join("\t") rather than @tsv: @tsv also escapes
# backslashes, so a title like `C:\path` would be DISPLAYED as `C:\\path`. The
# tab/newline guarantee @tsv provided is kept explicitly by `clean`/`bodyclean`
# instead, and those go further than @tsv would -- a PR body can plausibly carry
# a stray control character, and a literal U+001F inside one would inject fake
# line breaks into the preview.
#
# "sep" rows are the group headers and "note" rows are inline diagnostics;
# neither carries a URL and the dispatcher treats selecting them as a no-op.
RENDER_JQ='
def RULE: "────────────────────────────────────────────────";

# Control characters that must never reach a display field. \t \r \n are handled
# separately by each caller; this class is everything else that could corrupt the
# row or the pane, U+001F included.
def ctl: gsub("[\u0001-\u0008\u000b\u000c\u000e-\u001f\u007f]"; " ");
def clean: (. // "") | tostring | gsub("[\t\r\n]"; " ") | ctl;
# split/join, not gsub, for the three characters that occur on every line
# of a body: each gsub step is a fresh Oniguruma pass over the whole string
# and the bodies are the bulk of the cache. Measured on a 207KB / 42-item
# cache: 110ms of render time with gsub, 10ms with split/join, byte-identical
# output. The control-class pass stays a regex -- it matches rarely and there
# is no literal equivalent.
def bodyclean:
  (. // "") | tostring
  | split("\r") | join("")
  | split("\t") | join("    ")
  | gsub("[\u0001-\u0008\u000b\u000c\u000e-\u001f\u007f]"; "")
  | split("\n") | join("\u001f");
def pad($n): . as $s | ($n - ($s|length)) as $d
  | $s + (if $d > 0 then (" " * $d) else "" end);
def day: (. // "") | clean | .[5:10];
def yesno: if . then "yes" else "no" end;

def sep($label):
  "\u001b[1;36m── " + $label + " " + RULE + "\u001b[0m\tsep\t\t\t" + $label;
def note($msg; $body):
  ($msg | clean) as $m | ($body | clean) as $b
  | "\u001b[33m     " + $m + "\u001b[0m\tnote\t\t\t" + $b;

# Tag order is fixed by the spec: [draft] [changes requested] [approved]
# [CI failed] [CI pending]. FAILURE and ERROR both read as a failed run; PENDING
# and EXPECTED both mean "not finished yet".
def prtags:
  (if (.draft // false) then "[draft] " else "" end)
  + (if .review_decision == "CHANGES_REQUESTED" then "[changes requested] " else "" end)
  + (if .review_decision == "APPROVED" then "[approved] " else "" end)
  + (if (.checks == "FAILURE" or .checks == "ERROR") then "[CI failed] " else "" end)
  + (if (.checks == "PENDING" or .checks == "EXPECTED") then "[CI pending] " else "" end);

def prrow:
  . as $p
  | ($p.ref | clean) as $ref
  | ($p.title | clean) as $title
  | (($p.author // "ghost") | clean) as $who
  | ($p.url | clean) as $link
  | (($p.updated // "?") | clean) as $upd
  | ($p.body | bodyclean) as $body
  | [ "PR   " + ($ref | pad(30)) + " " + prtags + $title
        + "  (@" + $who + ", " + ($p.updated | day) + ")",
      "pr",
      $link,
      $ref,
      ([ "GitHub pull request",
         "",
         "ref       " + $ref,
         "title     " + $title,
         "repo      " + (($p.repo // "?") | clean),
         "author    @" + $who,
         "updated   " + $upd,
         "draft     " + (($p.draft // false) | yesno),
         "review    " + (($p.review_decision // "(no decision yet)") | clean),
         "checks    " + (($p.checks // "(none reported)") | clean),
         "",
         $link,
         "",
         RULE,
         ""
       ] | join("\u001f"))
      + (if $body == "" then "(no description)" else $body end)
    ] | join("\t");

def jirarow:
  . as $i
  | (($i.ref // $i.key) | clean) as $key
  | ($i.title | clean) as $sum
  | (($i.status // "?") | clean) as $status
  | (($i.type // "?") | clean) as $type
  | (($i.priority // "-") | clean) as $prio
  | ($i.url | clean) as $link
  | ($i.body | bodyclean) as $body
  | [ "JIRA " + ($key | pad(30)) + " [" + $status + "] " + $sum
        + "  (" + $type + ", " + $prio + ", " + ($i.updated | day) + ")",
      "jira",
      $link,
      $key,
      ([ "Jira issue",
         "",
         "key       " + $key,
         "summary   " + $sum,
         "status    " + $status + " (" + (($i.status_category // "?") | clean) + ")",
         "type      " + $type,
         "priority  " + $prio,
         "project   " + (($i.project // "?") | clean),
         "updated   " + (($i.updated // "?") | clean),
         "",
         $link,
         "",
         RULE,
         ""
       ] | join("\u001f"))
      + (if $body == "" then "(no description)" else $body end)
    ] | join("\t");

def row: if .kind == "pr" then prrow elif .kind == "jira" then jirarow else empty end;

# One section: header, then any note the source left, then either the empty-state
# note or the rows.
#
# A note is NOT exclusive to failure. The collector reports a group/world-readable
# credential file as ok:true WITH a note, because the fetch genuinely succeeded --
# branching on note-emptiness instead of on .ok would then hide that warning
# entirely, which is the one diagnostic that must never be silently dropped.
#
# The last parameter is $emptymsg, not $empty, and the name is load-bearing: the
# jq `def f($x)` sugar also binds a FILTER named `x`, so `$empty` would shadow
# the `empty` builtin inside this body and the "else empty end" below would emit
# a bare one-field row above every section.
def sect($id; $src; $label; $emptymsg):
  . as $c
  | ((($c.sources // {})[$src]) // {}) as $s
  | [$c.items[]? | select(.section == $id)] as $rows
  | (($s.note // "") | tostring) as $n
  # A note can be missing entirely if a source died mid-flight, and an empty
  # yellow row would read as a rendering glitch rather than a diagnostic.
  | (if ($n | length) == 0 then ($src + ": failed for an unknown reason") else $n end) as $why
  | sep($label),
    ( if ($s.ok // false) | not then note($why; $why)
      else ( if ($n | length) > 0 then note($n; $n) else empty end),
           ( if ($rows | length) == 0 then note($emptymsg; $emptymsg)
             else ($rows[] | row) end)
      end );

sect("review"; "github"; "REVIEW REQUESTED";  "no PRs are waiting on your review"),
sect("mine";   "github"; "MY PULL REQUESTS";  "you have no open pull requests"),
sect("jira";   "jira";   "MY JIRA ISSUES";    "no unresolved issues are assigned to you")
'

# Cache age, in English, from fetched_unix. Under a minute reads "just now"
# rather than "0m ago", which would look like a bug.
age_str() {
  # Both arguments are pre-validated as digit strings by the caller: `$(( ))` in
  # bash 3.2 dies on a float, and a cache written by a future collect could carry
  # a fractional timestamp.
  local d=$(( $1 - $2 ))
  if [ "$d" -lt 0 ]; then printf 'just now'
  elif [ "$d" -lt 60 ]; then printf 'just now'
  elif [ "$d" -lt 3600 ]; then printf '%dm ago' $(( d / 60 ))
  elif [ "$d" -lt 86400 ]; then printf '%dh ago' $(( d / 3600 ))
  else printf '%dd ago' $(( d / 86400 ))
  fi
}

HDR_KEYS='j/k move  h/l section  / search  enter open  ctrl-o agent  ctrl-y copy  r refresh  q close'

# Rows to stdout. Also writes, when the corresponding variable is set, the rows
# file (esc-out-of-search reloads from it), the separator positions (h/l read
# them) and the header text (the `load` event re-reads it). Writing them HERE
# rather than in the caller is what keeps `r` honest: the reload runs this same
# mode, so the positions and the header follow the new list instead of pinning
# the ones computed at startup.
render() {
  local tmpf own=0 meta fetched n_jira n_review n_mine now age

  meta=$(jq -r '[ ((.fetched_unix // 0) | floor),
                  ([.items[]? | select(.section == "jira")]   | length),
                  ([.items[]? | select(.section == "review")] | length),
                  ([.items[]? | select(.section == "mine")]   | length) ]
                | map(tostring) | join(" ")' "$CACHE" 2>/dev/null) || meta=''
  # A function has its own positionals, so this cannot disturb the caller's "$@".
  set -- ${meta:-0 0 0 0}
  fetched=${1:-0}; n_jira=${2:-0}; n_review=${3:-0}; n_mine=${4:-0}
  # Belt and braces for age_str's arithmetic: anything non-numeric reads as 0.
  case "$fetched" in ''|*[!0-9]*) fetched=0 ;; esac

  # No EXIT trap for the scratch file: the trap body would run after this
  # function returned, where a `local` is out of scope and `set -u` turns the
  # cleanup itself into an error message on stderr.
  tmpf="${WI_ROWS:-}"
  if [ -z "$tmpf" ]; then
    tmpf=$(mktemp "${TMPDIR:-/tmp}/work-inbox-rows.XXXXXX") || return 1
    own=1
  fi

  jq -r "$RENDER_JQ" "$CACHE" > "$tmpf.part" 2>/dev/null || {
    # A per-item error aborts jq mid-stream with earlier rows already flushed;
    # a half-written list must never be shown as if it were the whole inbox.
    printf 'work inbox: could not render %s\n' "$CACHE" >&2
    rm -f "$tmpf.part"
    [ "$own" -eq 1 ] && rm -f "$tmpf"
    return 1
  }
  mv -f "$tmpf.part" "$tmpf"

  if [ -n "${WI_SECPOS:-}" ]; then
    awk -F'\t' '$2 == "sep" { print NR + 1 }' "$tmpf" > "$WI_SECPOS"
  fi
  if [ -n "${WI_HEADER:-}" ]; then
    now=$(date +%s)
    age=$(age_str "$now" "$fetched")
    printf 'updated %s   %s jira / %s review / %s mine   %s\n' \
      "$age" "$n_jira" "$n_review" "$n_mine" "$HDR_KEYS" > "$WI_HEADER"
  fi

  cat "$tmpf"
  [ "$own" -eq 1 ] && rm -f "$tmpf"
  return 0
}

# ------------------------------------------------------------------- collection

cache_ok() {
  [ -s "$CACHE" ] || return 1
  jq -e '.version == 1 and (.items | type) == "array" and (.fetched_unix | type) == "number"' \
    "$CACHE" >/dev/null 2>&1
}

# Detached, so the NEXT open is fresh. All three fds are redirected and nohup is
# used because the popup's pty goes away the moment fzf exits -- a child still
# holding it would either die with SIGHUP or keep the popup on screen.
spawn_collect() {
  [ -x "$COLLECT" ] || return 0
  nohup "$COLLECT" >/dev/null 2>&1 </dev/null &
  disown 2>/dev/null || true
  return 0
}

# ------------------------------------------------------------------------ modes

case "${1:-}" in
  --dump)   dump=1 ;;
  --render)
    # Internal: the `r` binding. Whatever happens, SOMETHING has to reach stdout:
    # fzf's reload replaces the list with this command's output, so exiting
    # silently would empty the popup. On failure the previous rows are re-fed,
    # which leaves the list exactly as the user last saw it.
    if cache_ok && render; then
      exit 0
    fi
    [ -n "${WI_ROWS:-}" ] && [ -s "${WI_ROWS:-}" ] && cat "$WI_ROWS"
    exit 1 ;;
  --nav)
    # Internal: h/l. fzf cannot express "the next separator" on its own, and
    # `transform` bindings are the only way to compute a target at keypress time.
    # The positions are read from a FILE rather than baked into the binding
    # string, so `r` can rewrite them and h/l keep working on the new list.
    #
    #   --nav next|prev <FZF_POS> <positions-file>   ->  "pos(N)" | "ignore"
    dir="${2:-next}"; pos="${3:-1}"; posfile="${4:-}"
    case "$pos" in ''|*[!0-9]*) pos=1 ;; esac
    [ -f "$posfile" ] || { printf 'ignore\n'; exit 0; }
    awk -v p="$pos" -v dir="$dir" '
      /^[0-9]+$/ { a[++n] = $1 + 0 }
      END {
        t = 0
        if (dir == "next") { for (i = n; i >= 1; i--) if (a[i] > p) t = a[i] }
        else               { for (i = 1; i <= n; i++) if (a[i] < p) t = a[i] }
        if (t > 0) printf "pos(%d)\n", t; else print "ignore"
      }' "$posfile"
    exit 0 ;;
esac

# ------------------------------------------------------------------- fast path

collected=0
if ! cache_ok; then
  # First run (or a cache someone truncated). This is the only path that blocks
  # on the network, and it is the only path that prints anything before fzf.
  if [ ! -x "$COLLECT" ]; then
    fail "no cache at ${CACHE} and ${COLLECT} is missing or not executable"
  fi
  [ "$dump" -eq 0 ] && printf 'work inbox: fetching...\n'
  "$COLLECT" >/dev/null 2>&1
  collected=1
  cache_ok || fail "could not build ${CACHE} - run ${COLLECT} by hand to see why"
fi

tmp=$(mktemp -d "${TMPDIR:-/tmp}/work-inbox.XXXXXX") || fail "could not create a temp dir"
trap 'rm -rf "$tmp"' EXIT

export WI_ROWS="$tmp/rows" WI_SECPOS="$tmp/secpos" WI_HEADER="$tmp/header"
render >/dev/null || fail "could not render ${CACHE}"
rows="$WI_ROWS"

if [ "$dump" -eq 1 ]; then
  # Rows on stdout, diagnostics on stderr: the 5-field contract is then checkable
  # with a plain awk over stdout.
  printf '# cache=%s\n# header=%s\n# sep positions=%s\n' \
    "$CACHE" "$(cat "$WI_HEADER")" "$(tr '\n' ' ' < "$WI_SECPOS")" >&2
  cat "$rows"
  exit 0
fi

# Refresh for next time. Skipped when we just collected synchronously.
[ "$collected" -eq 1 ] || spawn_collect

# ----------------------------------------------------------------- interaction

command -v fzf >/dev/null 2>&1 || fail "fzf is not available on PATH"

# The list opens in NAV mode: --disabled turns search off so the vim keys move
# the cursor instead of typing into a hidden query. `/` switches to SEARCH mode
# (the nav keys are unbound so they become literal characters again); esc backs
# out of search into nav, and esc in nav closes the popup. `r` is in NAV_KEYS for
# exactly that reason -- without it, typing "review" in a query would trigger a
# refresh on the first keystroke.
#
# Leaving search mode has to `reload` the rows, not just disable-search: with
# fzf 0.74, disable-search leaves the previous result set in place and
# clear-query alone does not re-expand it either. Re-feeding the file resets the
# match set, the query and the cursor in one action.
#
# --with-shell pins fzf's child processes to bash. Herdr popups inherit $SHELL,
# which is fish here, and the esc binding is bash syntax.
NAV_KEYS='j,k,g,G,h,l,q,r,/'

SELF_Q=$(printf '%q' "$SELF")
COLLECT_Q=$(printf '%q' "$COLLECT")
SECPOS_Q=$(printf '%q' "$WI_SECPOS")
HEADER_Q=$(printf '%q' "$WI_HEADER")

# --expect rather than --bind for the secondary actions: the two are mutually
# exclusive on the same key, and the agent hand-off has to run after fzf exits so
# it can fall back to a picker of its own. ctrl-o is the documented hand-off key;
# ctrl-a is accepted too, but Herdr normally swallows it as the prefix.
sel=$(fzf --prompt="inbox > " --height=100% --reverse --ansi \
          --delimiter=$'\t' --with-nth=1 \
          --with-shell='bash -c' \
          --disabled \
          --bind='j:down,k:up,g:first,G:last,ctrl-d:half-page-down,ctrl-u:half-page-up' \
          --bind='q:abort' \
          --bind='ctrl-f:preview-half-page-down,ctrl-b:preview-half-page-up' \
          --bind="h:transform:$SELF_Q --nav prev \$FZF_POS $SECPOS_Q" \
          --bind="l:transform:$SELF_Q --nav next \$FZF_POS $SECPOS_Q" \
          --bind="r:reload($COLLECT_Q >/dev/null 2>&1; $SELF_Q --render)" \
          --bind="load:transform-header(cat $HEADER_Q)" \
          --bind="/:unbind($NAV_KEYS)+enable-search+change-prompt(search > )" \
          --bind="esc:transform:[[ \$FZF_PROMPT == search* ]] && echo 'clear-query+disable-search+reload(cat \"$rows\")+change-prompt(inbox > )+rebind($NAV_KEYS)' || echo abort" \
          --expect=ctrl-o,ctrl-a,ctrl-y \
          --header="$(cat "$WI_HEADER")" \
          --preview="printf '%s\n' {5} | tr '\037' '\n'" \
          --preview-window='right,50%,wrap' \
          < "$rows")
fzf_rc=$?

# 130 is esc (the documented close path) and 1 is "no match"; both mean the user
# dismissed the popup. Anything else -- 2 is fzf's option/runtime error -- has to
# be surfaced, or a mistyped flag just makes the popup flicker and vanish.
case $fzf_rc in
  0)      : ;;
  1|130)  exit 0 ;;
  *)      fail "fzf failed (exit $fzf_rc)" ;;
esac

key=$(printf '%s\n' "$sel" | sed -n 1p)
row=$(printf '%s\n' "$sel" | sed -n 2p)
[ -n "$row" ] || exit 0

kind=$(printf '%s' "$row" | cut -f2)
url=$(printf '%s' "$row" | cut -f3)
ref=$(printf '%s' "$row" | cut -f4)

# Whitelist, not blacklist: only pr and jira rows are actionable. If a row ever
# ends up with more fields than it should, kind holds a fragment of something
# else and this exits rather than treating field 3 as a URL.
case "$kind" in
  pr|jira) : ;;
  *)       exit 0 ;;
esac
[ -n "$url" ] || exit 0

if [ "$key" = "ctrl-y" ]; then
  # OSC 52 first (it is the only thing that can reach the client's clipboard over
  # --remote), pbcopy as well. copy-link.sh decides; here we only report.
  if [ -x "$COPY_LINK" ] && "$COPY_LINK" "$url"; then
    herdr notification show "work inbox" --body "copied ${ref}" --sound done >/dev/null 2>&1 || true
  else
    herdr notification show "work inbox" --body "could not copy ${ref}" --sound request >/dev/null 2>&1 || true
    printf 'work inbox: %s\n' "$url" >&2
  fi
  exit 0
fi

if [ "$key" = "ctrl-o" ] || [ "$key" = "ctrl-a" ]; then
  # Hand-off. `herdr agent prompt <PANE_ID> <TEXT>` is the only command that
  # actually submits; `pane send-text` would park the text in the composer.
  agents=$(herdr agent list 2>/dev/null) || fail "herdr agent list failed"

  # Inside a popup HERDR_PANE_ID is the POPUP's own pane, so only the
  # HERDR_ACTIVE_* set identifies the caller.
  caller="${HERDR_ACTIVE_PANE_ID:-${HERDR_PANE_ID:-}}"
  ws="${HERDR_ACTIVE_WORKSPACE_ID:-${HERDR_WORKSPACE_ID:-}}"
  if [ -z "$ws" ]; then
    ws=$(herdr api snapshot 2>/dev/null \
      | jq -r '.result.snapshot.focused_workspace_id // empty')
  fi

  # Caller pane outranks `.focused`: the focused agent can move between the
  # keypress that opened the popup and the selection. The pane_id match doubles
  # as caller validation.
  target=$(printf '%s' "$agents" | jq -r --arg caller "$caller" --arg ws "$ws" '
    .result.agents as $a
    | [ ($a[] | select(.pane_id == $caller)),
        ($a[] | select(.focused == true)),
        ([$a[] | select(.workspace_id == $ws)] | if length == 1 then .[] else empty end)
      ]
    | .[0].pane_id // empty')

  # Zero or several candidates leaves the array empty: ask, rather than inject a
  # prompt into the wrong agent's live turn.
  if [ -z "$target" ]; then
    # Materialise the candidates FIRST. `herdr agent list` succeeds with an empty
    # array when nothing is running, and fzf fed zero rows just draws an empty
    # prompt and waits, which reads as a hung popup.
    cands=$(printf '%s' "$agents" | jq -r '.result.agents[]?
        | [.pane_id, .agent, .agent_status, .workspace_id,
           (.terminal_title_stripped // "")] | join("\t")')
    [ -n "$cands" ] || fail "no agent is running to hand ${ref} to"

    target=$(printf '%s\n' "$cands" \
      | fzf --prompt="agent > " --height=100% --reverse \
            --delimiter=$'\t' --with-nth=2,3,4,5)
    pick_rc=$?
    case $pick_rc in
      0)      : ;;
      1|130)  exit 0 ;;   # esc or no match in the picker is a cancel
      *)      fail "fzf failed (exit $pick_rc)" ;;
    esac
    target=$(printf '%s' "$target" | cut -f1)
    [ -n "$target" ] || exit 0
  fi

  # `herdr agent prompt` honours no `--` separator, so the text must not start
  # with a dash.
  case "$kind" in
    pr)   text="Review this GitHub PR: ${ref} ${url}" ;;
    jira) text="Work on Jira issue ${ref}: ${url}" ;;
    *)    text="Take a look at ${url}" ;;
  esac

  # Never --wait/--until/--timeout from a popup: it would block on a settled
  # state, freezing the popup.
  herdr agent prompt "$target" "$text" >/dev/null 2>&1 \
    || fail "could not hand ${ref} to ${target}"
  herdr notification show "work inbox" \
    --body "handed ${ref} to ${target}" --sound done >/dev/null 2>&1 || true
  exit 0
fi

# Default (enter): open in the browser. Absolute path -- this is /usr/bin/open,
# not a Homebrew binary, and the Herdr LaunchAgent runs in gui/501 so the
# LaunchServices round trip works from the service.
/usr/bin/open "$url" >/dev/null 2>&1 || fail "could not open ${url}"
