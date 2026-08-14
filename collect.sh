#!/bin/bash
#
# jin.work-inbox -- headless collector.
#
# Fetches both sources CONCURRENTLY and writes a single structured cache that
# the front end renders without touching the network. This is what herdr's
# `[[actions]] refresh` runs, and it is also what the popup spawns in the
# background after it has drawn from the existing cache.
#
# It is HEADLESS: stdin/stdout/stderr are not ttys and TERM=dumb. Nothing here
# may be interactive. Exactly one line goes to stdout, which herdr captures into
# `plugin log list`.
#
# Exit 0 if at least one attempted source succeeded, 1 if every attempted source
# failed. Partial failure is never fatal -- a broken Atlassian token must never
# hide your PR reviews.
#
# A FAILED leg keeps the items it fetched last time, together with the timestamp
# of that last SUCCESSFUL fetch, and records ok:false plus the reason. This
# tenant produces intermittent `curl exit 56`, and blanking the section on a
# transient blip is worse than showing yesterday's list next to a "stale" note:
# every source therefore carries its own fetched_unix so the front end can say
# HOW stale. The top-level fetched_unix is the newest of the per-leg values.
#
#   collect.sh                  refresh both legs
#   collect.sh --only github    refresh GitHub, keep Jira's half of the cache
#   collect.sh --only jira      refresh Jira, keep GitHub's half of the cache
#
# Targets bash 3.2.57 (macOS /bin/bash).

set -u

WI_LIB_DIR=$(cd "$(dirname "$0")" >/dev/null 2>&1 && pwd)
# shellcheck source=lib/common.sh
. "${WI_LIB_DIR}/lib/common.sh"

# Everything this script writes -- scratch files, the state dir, the cache --
# holds PR bodies and Jira descriptions, i.e. work-confidential text. 077 before
# the first file is created, so no window exists in which a 0644 cache is
# readable by other local accounts.
umask 077

started=$SECONDS

# ------------------------------------------------------------------- arguments

only=''
while [[ $# -gt 0 ]]; do
  case "$1" in
    --only)
      only="${2:-}"
      case "$only" in
        github|jira) shift 2 ;;
        *) printf 'work-inbox collect: --only takes "github" or "jira"\n' >&2; exit 2 ;;
      esac ;;
    --only=github) only='github'; shift ;;
    --only=jira)   only='jira';   shift ;;
    -h|--help)
      printf 'usage: collect.sh [--only github|jira]\n'
      exit 0 ;;
    *)
      printf 'work-inbox collect: unknown argument: %s\n' "$1" >&2
      exit 2 ;;
  esac
done

do_gh=1 do_jira=1
case "$only" in
  github) do_jira=0 ;;
  jira)   do_gh=0 ;;
esac

command -v jq >/dev/null 2>&1 || {
  printf 'work-inbox collect: jq is not on PATH - cannot build the cache\n' >&2
  exit 1
}

wi_ensure_state_dir || {
  printf 'work-inbox collect: could not create %s\n' "$WI_STATE_DIR" >&2
  exit 1
}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/work-inbox-collect.XXXXXX") || {
  printf 'work-inbox collect: could not create a temp dir\n' >&2
  exit 1
}
trap 'rm -rf "$tmp"' EXIT

# --slurpfile refuses a file that is not valid JSON, so both item files are
# pre-seeded with an empty array. A leg that dies before it produces anything
# therefore contributes zero items instead of aborting the assembly.
printf '[]' > "$tmp/gh.items.json"
printf '[]' > "$tmp/jira.items.json"
: > "$tmp/gh.note"
: > "$tmp/jira.note"
: > "$tmp/jira.warn"

# ------------------------------------------------------------------ github leg
#
# ONE round trip carrying BOTH searches as aliased `search` fields. sort:updated-desc
# is load-bearing: without it the search API orders by its own relevance ranking,
# so the first: 50 cap would drop arbitrary PRs instead of the stalest ones.
# Adding a sort qualifier does not narrow the result set, so team-derived review
# requests are still included.
GH_REVIEW_SEARCH='is:pr is:open review-requested:@me archived:false sort:updated-desc'
GH_MINE_SEARCH='is:pr is:open author:@me archived:false sort:updated-desc'

# body and statusCheckRollup are pulled here, in the background, precisely
# because they are what made the old synchronous popup slow: the body kills the
# per-item preview fetch and the rollup adds the CI signal the old list lacked.
GH_QUERY='query($rq: String!, $mq: String!) {
  reviewing: search(query: $rq, type: ISSUE, first: 50) {
    nodes { ... on PullRequest {
      number title url updatedAt isDraft reviewDecision body
      repository { name } author { login }
      commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
    } }
  }
  mine: search(query: $mq, type: ISSUE, first: 50) {
    nodes { ... on PullRequest {
      number title url updatedAt isDraft reviewDecision body
      repository { name } author { login }
      commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
    } }
  }
}'

# select(.url != null) drops the {} nodes that search(type: ISSUE) yields for
# anything that is not a PullRequest.
#
# Deduplication is done HERE rather than in bash: bash 3.2 has no associative
# arrays, and a PR you authored can legitimately also have review requested of
# you. "review" wins, because that is the section that says someone is waiting
# on you.
#
# commits.nodes is empty for a PR whose head commit was force-pushed away, so
# the rollup path is indexed defensively; a repo with no CI at all yields null,
# which the contract allows.
GH_JQ="$WI_JQ_PRELUDE"'
def mk($section):
  { kind: "pr",
    section: $section,
    ref: ((((.repository.name // "?") | clean)) + "#" + ((.number // 0) | tostring)),
    url: ((.url // "") | clean),
    title: (.title | clean),
    repo: ((.repository.name // "?") | clean),
    number: (.number // 0),
    author: ((.author.login // "ghost") | clean),
    updated: ((.updatedAt // "") | clean),
    draft: (.isDraft // false),
    review_decision: (.reviewDecision // null),
    checks: (((.commits.nodes // [])[0].commit.statusCheckRollup.state) // null),
    body: (.body | trunc) };
((.data.reviewing.nodes // []) | map(select(.url != null))) as $rev
| ((.data.mine.nodes // []) | map(select(.url != null))) as $own
| ($rev | map(.url)) as $seen
| ($rev | map(mk("review")))
  + ($own | map(select((.url as $u | $seen | index($u)) | not)) | map(mk("mine")))
'

fetch_github() {
  local out rc err="$tmp/gh.stderr"

  if ! command -v gh >/dev/null 2>&1; then
    printf 'github: gh is not on PATH - install it with: brew install gh\n' > "$tmp/gh.note"
    return 1
  fi

  # Capture, THEN check rc: on a 401 `gh api graphql` writes the error body to
  # STDOUT, so piping straight into jq surfaces a confusing jq-level failure
  # instead of a clean auth message.
  #
  # -f, not -F: -F applies type coercion and treats a leading @ as a filename,
  # neither of which has any business near a search string.
  out=$(gh api graphql \
          -f query="$GH_QUERY" \
          -f rq="$GH_REVIEW_SEARCH" \
          -f mq="$GH_MINE_SEARCH" 2>"$err")
  rc=$?

  if [[ $rc -ne 0 ]]; then
    # 4 is gh's dedicated "not authenticated" code; every API-level failure is
    # exit 1 and has to be told apart by its stderr.
    if [[ $rc -eq 4 ]]; then
      printf 'github: not authenticated - run: gh auth login\n' > "$tmp/gh.note"
    elif grep -q 'Bad credentials\|HTTP 401' "$err" 2>/dev/null; then
      printf 'github: token rejected (401) - run: gh auth login\n' > "$tmp/gh.note"
    elif grep -q 'error connecting to' "$err" 2>/dev/null; then
      printf 'github: unreachable - check your network connection\n' > "$tmp/gh.note"
    else
      printf 'github: query failed (gh exit %s)\n' "$rc" > "$tmp/gh.note"
    fi
    return 1
  fi

  # A genuinely empty inbox is exit 0 with empty node lists, so success is
  # decided by rc here and the item count is judged later.
  #
  # Built into a scratch file and moved into place only on jq exit 0: a
  # per-node error aborts jq with partial output already flushed, and a
  # half-written array would either break the assembly or land as real data
  # behind a "could not parse" note.
  if ! printf '%s' "$out" | jq "$GH_JQ" > "$tmp/gh.items.part" 2>"$tmp/gh.jqerr"; then
    printf 'github: could not parse the API response\n' > "$tmp/gh.note"
    return 1
  fi
  mv -f "$tmp/gh.items.part" "$tmp/gh.items.json"
  return 0
}

# -------------------------------------------------------------------- jira leg

# /rest/api/3/search/jql requires a BOUNDED query. Sorting server-side means the
# renderer never has to parse a timestamp to order the list.
JIRA_JQL='assignee = currentUser() AND resolution = Unresolved ORDER BY updated DESC'

# `description` arrives as ADF, so it is walked into plain text HERE, at collect
# time -- that is what removes the per-keystroke Jira REST call the old preview
# made. statusCategory rides inside `status`, which is already requested, so the
# board view's stable column key costs no extra field.
#
# status_category is derived from statusCategory.KEY, not .name: the name is
# localised by the tenant (this one returns 進行中, i.e. a byte-for-byte copy of
# .status.name and therefore zero information). The key is the locale-stable
# new / indeterminate / done, so it is mapped to the contract's English values
# here. `status` stays verbatim-from-the-API by the LABEL LANGUAGE rule;
# status_category is a stable key and must not be.
#
# .fields.priority is legitimately null when a project disables priorities.
JIRA_JQ="$WI_JQ_PRELUDE"'
(.issues // [])
| map(
    . as $i
    | ($i.fields // {}) as $f
    | (($i.key // "?") | clean) as $key
    | { kind: "jira",
        section: "jira",
        ref: $key,
        url: (($base | clean) + "/browse/" + $key),
        title: ($f.summary | clean),
        key: $key,
        status: (($f.status.name // "?") | clean),
        status_category: (($f.status.statusCategory.key // "")
                          | if   . == "new"           then "To Do"
                            elif . == "indeterminate" then "In Progress"
                            elif . == "done"          then "Done"
                            else "" end),
        type: (($f.issuetype.name // "?") | clean),
        priority: (($f.priority.name // "-") | clean),
        project: (($f.project.key // "?") | clean),
        updated: (($f.updated // "") | clean),
        body: (($f.description // null)
               | if . == null then "" else ('"$ADF_TO_TEXT"') end
               | trunc) })
'

fetch_jira() {
  if ! command -v curl >/dev/null 2>&1; then
    printf 'jira: curl is not on PATH\n' > "$tmp/jira.note"
    return 1
  fi

  # A fatal error while sourcing (a stray `exit` in the credential file) kills
  # this whole subshell, so the note that explains it has to exist BEFORE the
  # load runs -- otherwise the cache records a failure with no reason attached.
  printf 'jira: failed while reading %s - check its contents (one KEY=value per line)\n' \
    "$ENV_FILE" > "$tmp/jira.note"

  if ! jira_env_load; then
    printf '%s\n' "$JIRA_ERR" > "$tmp/jira.note"
    return 1
  fi
  : > "$tmp/jira.note"
  [[ -n "$JIRA_WARN" ]] && printf '%s\n' "$JIRA_WARN" >> "$tmp/jira.warn"

  local base="$JIRA_BASE" email="$JIRA_MAIL" token="$JIRA_TOK"
  local req="$tmp/jira.req" body="$tmp/jira.body" hdr="$tmp/jira.hdr" err="$tmp/jira.stderr"

  # `fields` is an ARRAY of strings here, and its documented default is just
  # ["id"] -- always send it explicitly or rows come back with no summary.
  if ! jq -n --arg jql "$JIRA_JQL" \
      '{jql: $jql,
        fields: ["summary","status","issuetype","priority","project","updated","description"],
        maxResults: 50}' > "$req"; then
    printf 'jira: could not build the request body\n' > "$tmp/jira.note"
    return 1
  fi

  # `curl --config -` reads the credential from STDIN, so the token stays out of
  # argv (hence out of `ps`) and out of every error string. It produces a
  # byte-identical `Authorization: Basic ...` header. No --fail /
  # --fail-with-body: those turn an HTTP 401 into curl exit 22 and collapse
  # "expired token" into "the network died".
  #
  # xtrace is suspended around the printf as well: `bash -x` on this script (or
  # an inherited SHELLOPTS=xtrace) would otherwise echo the expanded credential
  # to stderr, and herdr copies stderr into the plugin log.
  jira_call() {
    local xt=0 rc
    case "$-" in *x*) xt=1; { set +x; } 2>/dev/null ;; esac
    printf 'user = "%s:%s"\n' "$email" "$token" \
    | curl --silent --show-error --config - \
        --request POST \
        --url "${base}/rest/api/3/search/jql" \
        --header 'Accept: application/json' \
        --header 'Content-Type: application/json' \
        --data @"$req" \
        --connect-timeout 5 --max-time 20 \
        --dump-header "$hdr" \
        --output "$body" \
        --write-out '%{http_code}' 2>"$err"
    rc=$?
    [[ $xt -eq 1 ]] && { set -x; } 2>/dev/null
    return $rc
  }

  # Retry TRANSPORT failures only (curl_rc != 0: 56 connection reset, 6, 7,
  # 28 ...). This tenant has produced exit 56 intermittently, and curl's own
  # --retry does NOT class CURLE_RECV_ERROR as transient. An HTTP 4xx/5xx
  # leaves curl_rc == 0, so it can never be mislabelled as a network problem.
  local code='' curl_rc=0 attempt
  for attempt in 1 2 3; do
    code=$(jira_call); curl_rc=$?
    [[ $curl_rc -eq 0 ]] && break
    [[ $attempt -lt 3 ]] && sleep "$attempt"
  done

  if [[ $curl_rc -ne 0 ]]; then
    # 26 is "could not read the config", i.e. a malformed credential rather than
    # anything to do with the network -- the guard in jira_env_load catches the
    # usual shapes, so this is the belt-and-braces path.
    if [[ $curl_rc -eq 26 ]]; then
      printf 'jira: malformed credential in %s - re-paste JIRA_EMAIL and JIRA_API_TOKEN on one line each\n' \
        "$ENV_FILE" > "$tmp/jira.note"
      return 1
    fi
    printf 'jira: unreachable after 3 tries (curl exit %s) - network, not auth\n' "$curl_rc" \
      > "$tmp/jira.note"
    return 1
  fi

  case "$code" in
    200) : ;;
    401)
      printf 'jira: credential rejected (401) - re-create your Atlassian API token at %s, then update %s\n' \
        "$TOKEN_URL" "$ENV_FILE" > "$tmp/jira.note"
      return 1 ;;
    403)
      if grep -qi 'X-Authentication-Denied-Reason' "$hdr" 2>/dev/null; then
        printf 'jira: browser login required (CAPTCHA) - sign in at %s once, then retry\n' \
          "$base" > "$tmp/jira.note"
      else
        printf 'jira: denied (403) - the token lacks access to this tenant\n' > "$tmp/jira.note"
      fi
      return 1 ;;
    400)
      # Server-controlled text. The control characters are stripped inside jq so
      # the note stays a single line in the cache and in the plugin log.
      printf 'jira: JQL rejected (400): %s\n' \
        "$(jq -r '(.errorMessages // []) | join("; ") | gsub("[\t\r\n]"; " ")' "$body" 2>/dev/null)" \
        > "$tmp/jira.note"
      return 1 ;;
    404|410)
      printf 'jira: HTTP %s from %s/rest/api/3/search/jql - check JIRA_BASE_URL (no /wiki suffix)\n' \
        "$code" "$base" > "$tmp/jira.note"
      return 1 ;;
    000)
      printf 'jira: no HTTP response - check the network and JIRA_BASE_URL (%s)\n' "$base" \
        > "$tmp/jira.note"
      return 1 ;;
    *)
      printf 'jira: unexpected HTTP %s from the API\n' "$code" > "$tmp/jira.note"
      return 1 ;;
  esac

  # Results live in .issues; there is no `total` and no `startAt` on this
  # endpoint. Atomic, like the github leg.
  if ! jq --arg base "$base" "$JIRA_JQ" "$body" > "$tmp/jira.items.part" 2>"$tmp/jira.jqerr"; then
    printf 'jira: could not parse the API response\n' > "$tmp/jira.note"
    return 1
  fi
  mv -f "$tmp/jira.items.part" "$tmp/jira.items.json"
  return 0
}

# ------------------------------------------------------------ concurrent fetch
#
# Both legs run at once, so the wall clock is the slower of the two rather than
# their sum. Each leg reports through files, not variables: they are subshells,
# so nothing they assign survives.

gh_ok=0 jira_ok=0
gh_pid='' jira_pid=''

if [[ $do_gh -eq 1 ]]; then
  fetch_github >/dev/null 2>&1 &
  gh_pid=$!
fi
if [[ $do_jira -eq 1 ]]; then
  fetch_jira >/dev/null 2>&1 &
  jira_pid=$!
fi
[[ -n "$gh_pid" ]]   && { wait "$gh_pid";   gh_ok=$?; }
[[ -n "$jira_pid" ]] && { wait "$jira_pid"; jira_ok=$?; }

note_of() {
  local msg
  msg=$(head -n 1 "$1" 2>/dev/null)
  printf '%s' "${msg:-$2}"
}

gh_note='' jira_note=''
if [[ $gh_ok -ne 0 ]]; then
  gh_note=$(note_of "$tmp/gh.note" 'github: failed for an unknown reason')
fi
if [[ $jira_ok -ne 0 ]]; then
  jira_note=$(note_of "$tmp/jira.note" "jira: failed while reading ${ENV_FILE} - check its contents")
elif [[ -s "$tmp/jira.warn" ]]; then
  # A succeeded-but-complaining leg still carries its warning forward: ok stays
  # true, and the front end can surface the permissions problem.
  jira_note=$(head -n 1 "$tmp/jira.warn")
fi

# A leg's item file is only ever populated by a leg that ran to completion (each
# fetch_* moves it into place as its last act), so nothing has to be blanked
# here: the assembly below simply does not read $ghi/$jri unless that leg
# reported success, and takes the previous cache's items in every other case.

# ------------------------------------------------------------------- assembly
#
# A leg that failed and a leg that was not attempted both inherit their items
# from the previous cache; only a leg that just succeeded replaces them. A
# missing or corrupt cache degrades to "never collected" rather than aborting:
# the whole point of this script is that it always leaves a readable cache
# behind.
#
# The check validates the SHAPE the assembly below actually indexes into, not
# just the top-level type. `$o.sources.github` on a `sources` that is an array,
# and `.kind` on a non-object item, both abort jq -- which would turn an
# externally corrupted cache into exit 1 with NO cache written, throwing away a
# leg that had just succeeded. Anything that does not match degrades to `{}`
# here, at one single point, so the assembly program needs no guards of its own.
#
# `sources` is tested WITHOUT a `// {}` default: jq's `//` treats `false` as
# absent, so `sources: false` would pass the guard and then abort the assembly
# on `.sources.jira`. null must still pass -- a cache with no `sources` key at
# all is the ordinary "never collected" case the `//` defaults below handle.
#
# The per-leg objects and the three timestamps are checked for the same reason,
# now that retention INDEXES INTO a leg and feeds its timestamp to `max`:
# `.fetched_unix` on a `sources.jira` that is an array aborts jq outright, and a
# string timestamp would survive `max` -- jq orders across types instead of
# failing -- and land in the cache as a string, where the front end's age
# arithmetic breaks instead. `and` short-circuits in jq, so the `// {}` in the
# timestamp clauses is only ever reached once the leg is known object-or-null.
if jq -e 'type == "object"
          and ((.items // []) | type == "array" and all(type == "object"))
          and (.fetched_unix | type == "number" or type == "null")
          and (.sources | type == "object" or type == "null")
          and ((.sources // {})
               | (.github | type == "object" or type == "null")
                 and (.jira | type == "object" or type == "null")
                 and ((.github // {}).fetched_unix | type == "number" or type == "null")
                 and ((.jira   // {}).fetched_unix | type == "number" or type == "null"))' \
     "$WI_CACHE" >/dev/null 2>&1; then
  cp "$WI_CACHE" "$tmp/old.json" 2>/dev/null || printf '{}' > "$tmp/old.json"
else
  printf '{}' > "$tmp/old.json"
fi

gh_ok_json=false;   [[ $gh_ok -eq 0 ]]   && gh_ok_json=true
jira_ok_json=false; [[ $jira_ok -eq 0 ]] && jira_ok_json=true

# Items go in via --slurpfile, never via --arg or command substitution: ~80
# items carrying up-to-8000-character bodies is well past a comfortable argv.
if ! jq -n \
      --argjson fetched "$(date +%s)" \
      --argjson do_gh   "$([[ $do_gh -eq 1 ]] && printf true || printf false)" \
      --argjson do_jira "$([[ $do_jira -eq 1 ]] && printf true || printf false)" \
      --argjson gh_ok   "$gh_ok_json" \
      --argjson jira_ok "$jira_ok_json" \
      --arg gh_note   "$gh_note" \
      --arg jira_note "$jira_note" \
      --slurpfile old   "$tmp/old.json" \
      --slurpfile ghi   "$tmp/gh.items.json" \
      --slurpfile jri   "$tmp/jira.items.json" \
      '($old[0] // {}) as $o
       | (($o.items // []) | if type == "array" then . else [] end) as $oldi
       # A phase-1 cache has no per-leg timestamp, so the top-level one stands in
       # for both legs -- it was written by a run in which each `ok` leg had just
       # succeeded, which is exactly what the per-leg value means. Without this
       # fallback the first run after the upgrade would report every retained
       # section as stale since the epoch.
       | ($o.fetched_unix // 0) as $oldt
       | (($o.sources.github // {}).fetched_unix // $oldt) as $gh_prev
       | (($o.sources.jira   // {}).fetched_unix // $oldt) as $jira_prev
       # A leg that has never once succeeded has no previous timestamp to keep,
       # and reports 0 rather than omitting the field: the front end then reads a
       # number unconditionally, and 0 is unmistakably "never", where a "now"
       # placeholder would render as a fresh but empty section.
       | (if $do_gh   and $gh_ok   then $fetched else $gh_prev   end) as $gh_t
       | (if $do_jira and $jira_ok then $fetched else $jira_prev end) as $jira_t
       | { version: 1,
           fetched_unix: ([$gh_t, $jira_t] | max),
           sources: {
             github: (if $do_gh then { ok: $gh_ok, note: $gh_note, fetched_unix: $gh_t }
                      else (($o.sources.github // { ok: false, note: "github: not collected yet" })
                            + { fetched_unix: $gh_t }) end),
             jira:   (if $do_jira then { ok: $jira_ok, note: $jira_note, fetched_unix: $jira_t }
                      else (($o.sources.jira // { ok: false, note: "jira: not collected yet" })
                            + { fetched_unix: $jira_t }) end)
           },
           items: ((if $do_gh   and $gh_ok   then ($ghi[0] // []) else ($oldi | map(select(.kind == "pr")))   end)
                 + (if $do_jira and $jira_ok then ($jri[0] // []) else ($oldi | map(select(.kind == "jira"))) end)) }' \
      > "$tmp/cache.build"; then
  printf 'work-inbox collect: could not assemble the cache\n' >&2
  exit 1
fi

# Atomic: a partial jq run must never land as cache.json. Same filesystem as the
# target, so the rename cannot fall back to a copy.
out_tmp="${WI_CACHE}.tmp.$$"
if ! cp "$tmp/cache.build" "$out_tmp"; then
  printf 'work-inbox collect: could not write %s\n' "$out_tmp" >&2
  exit 1
fi
chmod 600 "$out_tmp" 2>/dev/null || true
if ! mv -f "$out_tmp" "$WI_CACHE"; then
  rm -f "$out_tmp"
  printf 'work-inbox collect: could not replace %s\n' "$WI_CACHE" >&2
  exit 1
fi

# ---------------------------------------------------------------- one-line log

n_pr=$(jq   '[.items[] | select(.kind == "pr")]   | length' "$WI_CACHE" 2>/dev/null || printf 0)
n_jira=$(jq '[.items[] | select(.kind == "jira")] | length' "$WI_CACHE" 2>/dev/null || printf 0)
elapsed=$(( SECONDS - started ))

# The counts above come from the cache that was just written, so a failed leg's
# figure is now its RETAINED items rather than what this run fetched. The word
# has to say so, or the log reads as though a failing source still returned data.
gh_word='skipped'; jira_word='skipped'
[[ $do_gh -eq 1 ]] && { gh_word='ok'; [[ $gh_ok -ne 0 ]] && {
  gh_word="FAILED (${gh_note})"
  [[ $n_pr -gt 0 ]] && gh_word="FAILED, kept ${n_pr} stale (${gh_note})"; }; }
[[ $do_jira -eq 1 ]] && { jira_word='ok'; [[ $jira_ok -ne 0 ]] && {
  jira_word="FAILED (${jira_note})"
  [[ $n_jira -gt 0 ]] && jira_word="FAILED, kept ${n_jira} stale (${jira_note})"; }; }

printf 'work-inbox: github %s, jira %s - %s pr / %s jira items in %ss -> %s\n' \
  "$gh_word" "$jira_word" "$n_pr" "$n_jira" "$elapsed" "$WI_CACHE"

# Exit 1 only when every ATTEMPTED source failed. The cache has already been
# written either way, so a caller that ignores the status still finds a valid
# file with both notes in it.
if [[ $do_gh -eq 1 && $gh_ok -eq 0 ]]; then exit 0; fi
if [[ $do_jira -eq 1 && $jira_ok -eq 0 ]]; then exit 0; fi
exit 1
