#!/bin/bash
#
# jin.work-inbox -- shared definitions.
#
# Sourced by collect.sh only. ui.sh and copy-link.sh source nothing: they carry
# their own copies of the PATH export and (in ui.sh) of the state-dir resolution,
# because the front end must not pay a source on the fast path and must never be
# able to reach the credential loader below. Those copies are duplicated on
# purpose and are kept identical by hand; if that stops being true, move the
# shared halves out rather than making the front end source this file.
#
# This file DEFINES ONLY: sourcing it must not create a directory, touch the
# network, or read the credential. The one side effect it does have is the PATH
# export, which every consumer needs before it can run anything at all.
#
# Safe to source under `set -u`: every parameter expansion carries a default.
#
# Targets bash 3.2.57 (macOS /bin/bash). No associative arrays, no mapfile, no
# ${var,,}, no negative array indices.

# herdr runs plugin commands with launchd's minimal PATH. gh and fzf are
# Homebrew; jq, curl and open are /usr/bin. ~/.local/bin carries herdr itself.
# The inherited PATH is appended with ${PATH:+:...} rather than :${PATH:-}: an
# exported-but-empty PATH would otherwise leave a trailing colon, and an empty
# PATH element means the CURRENT DIRECTORY -- a hostile cwd could then shadow a
# binary that is missing from the seven dirs above.
export PATH="/opt/homebrew/bin:/usr/local/bin:${HOME:-}/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:${PATH}}"

# The credential lives OUTSIDE this git repo, in ~/.local/state/herdr-work-inbox/env
# (dir 0700, file 0600), because ~/.config IS the dotfiles repo root. Unchanged
# from the pre-plugin script -- there is no migration.
WI_ENV_FILE="${HOME:-}/.local/state/herdr-work-inbox/env"
ENV_FILE="$WI_ENV_FILE"
TOKEN_URL="https://id.atlassian.com/manage-profile/security/api-tokens"

# State dir. Plugin COMMANDS get HERDR_PLUGIN_STATE_DIR from herdr; the popup
# front end is a user keybinding, not a plugin command, so it will not have it
# and must reproduce the same literal path.
WI_STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-${XDG_STATE_HOME:-${HOME:-}/.local/state}/herdr/plugins/jin.work-inbox}"
WI_STATE_DIR="${WI_STATE_DIR%/}"
WI_CACHE="${WI_STATE_DIR}/cache.json"

# Optional, may not exist.
WI_CONFIG_DIR="${HERDR_PLUGIN_CONFIG_DIR:-${HOME:-}/.config/herdr/plugins/config/jin.work-inbox}"
WI_CONFIG_DIR="${WI_CONFIG_DIR%/}"
WI_CONFIG_FILE="${WI_CONFIG_DIR}/config.toml"

# ------------------------------------------------------------------ state dir
#
# 0700, and re-chmod'ed even when it already exists: the cache now holds PR
# bodies and Jira descriptions, so a directory left at 0755 by an earlier
# version would expose work-confidential text to every local account.
wi_ensure_state_dir() {
  [[ -d "$WI_STATE_DIR" ]] || mkdir -p "$WI_STATE_DIR" || return 1
  chmod 700 "$WI_STATE_DIR" 2>/dev/null || true
  return 0
}

# --------------------------------------------------------------- jira credential
#
# Validate and read the credential file. Only the collector ever calls this --
# the popup front end has no reason to learn the token and never sources this
# file, so the blast radius of the token stays one process wide.
#
# Sets JIRA_BASE / JIRA_MAIL / JIRA_TOK on success, JIRA_ERR on failure, and
# JIRA_WARN for a non-fatal complaint. Returns via globals rather than stdout
# precisely because `$(jira_env_load)` would run it in a subshell, where the
# assignments would be discarded.
JIRA_BASE='' JIRA_MAIL='' JIRA_TOK='' JIRA_ERR='' JIRA_WARN=''

jira_env_load() {
  JIRA_BASE='' JIRA_MAIL='' JIRA_TOK='' JIRA_ERR='' JIRA_WARN=''

  # Defensive sourcing: the path must exist, be a regular file, be readable,
  # be OWNED BY US and not be writable by anyone else. Sourcing is the strongest
  # primitive in this script -- whoever can write the file gets code execution
  # here -- so a bad owner or a writable bit is a hard refusal, not a warning.
  if [[ ! -e "$ENV_FILE" ]]; then
    JIRA_ERR="jira: not configured - create ${ENV_FILE} (see the comments inside it), then re-open"
    return 1
  fi
  if [[ ! -f "$ENV_FILE" ]]; then
    JIRA_ERR="jira: ${ENV_FILE} is not a regular file - replace it"
    return 1
  fi
  if [[ ! -r "$ENV_FILE" ]]; then
    JIRA_ERR="jira: ${ENV_FILE} is unreadable - run: chmod 600 ${ENV_FILE}"
    return 1
  fi

  # The mode is tested BITWISE, not against a literal 600: 400 is stricter than
  # 600 and must not be reported as a problem.
  local mode owner
  mode=$(stat -f '%Lp' "$ENV_FILE" 2>/dev/null || printf '')
  owner=$(stat -f '%u' "$ENV_FILE" 2>/dev/null || printf '')
  if [[ -n "$owner" && "$owner" != "$(id -u)" ]]; then
    JIRA_ERR="jira: ${ENV_FILE} is not owned by you - delete it and re-create it"
    return 1
  fi
  if [[ -n "$mode" ]] && (( 8#$mode & 8#022 )); then
    JIRA_ERR="jira: ${ENV_FILE} is group/world-writable (mode ${mode}) - run: chmod 600 ${ENV_FILE}"
    return 1
  fi
  if [[ -n "$mode" ]] && (( 8#$mode & 8#044 )); then
    JIRA_WARN="jira: ${ENV_FILE} is readable by others (mode ${mode}) - run: chmod 600 ${ENV_FILE}"
  fi

  # Sourced plainly, WITHOUT `set -a`: the three values are copied into the
  # JIRA_* globals below, so exporting them would only hand the token to the
  # environment of every child process (curl, jq, grep, sleep) for no benefit.
  # nounset is relaxed across the source so that a value referring to an unset
  # variable degrades to the "missing JIRA_*" message instead of killing the
  # caller. A stray `exit` in the file still kills us, which is why the caller
  # pre-seeds a note before calling this.
  local src_rc=0
  set +u
  # shellcheck disable=SC1090
  . "$ENV_FILE" || src_rc=$?
  set -u
  if [[ $src_rc -ne 0 ]]; then
    JIRA_ERR="jira: could not source ${ENV_FILE} - check its shell syntax"
    return 1
  fi

  JIRA_BASE="${JIRA_BASE_URL:-}"; JIRA_BASE="${JIRA_BASE%/}"
  JIRA_MAIL="${JIRA_EMAIL:-}"
  JIRA_TOK="${JIRA_API_TOKEN:-}"

  if [[ -z "$JIRA_BASE" || -z "$JIRA_MAIL" || -z "$JIRA_TOK" ]]; then
    JIRA_ERR="jira: ${ENV_FILE} is missing JIRA_BASE_URL, JIRA_EMAIL or JIRA_API_TOKEN"
    return 1
  fi

  # Never send the shipped placeholders at the tenant -- report them instead.
  case "$JIRA_TOK" in
    REPLACE_ME_WITH_YOUR_ATLASSIAN_API_TOKEN|REPLACE_ME*|PASTE_*|CHANGE_ME*|xxx*)
      JIRA_ERR="jira: not configured yet - put a real API token in ${ENV_FILE} (create one at ${TOKEN_URL})"
      return 1 ;;
  esac
  case "$JIRA_MAIL" in
    you@example.com|REPLACE_ME*)
      JIRA_ERR="jira: set JIRA_EMAIL to your Atlassian account email in ${ENV_FILE}"
      return 1 ;;
  esac

  # A quote, backslash or line break in either half makes `curl --config -`
  # refuse to parse its config and exit 26, which the transport retry would
  # otherwise report as a network problem. Diagnose it here instead: the
  # realistic cause is a token pasted with a stray newline or quote.
  if [[ "$JIRA_TOK$JIRA_MAIL" == *[$'\n\r\t"']* || "$JIRA_TOK$JIRA_MAIL" == *'\'* ]]; then
    JIRA_ERR="jira: JIRA_EMAIL or JIRA_API_TOKEN in ${ENV_FILE} contains a quote, backslash or line break - re-paste it on one line"
    return 1
  fi

  return 0
}

# ----------------------------------------------------------------- ADF walker
#
# Atlassian returns the description as ADF (Atlassian Document Format), a
# nested JSON tree rather than text or markdown, so it has to be walked. This
# keeps the structures that carry meaning in a ticket -- paragraphs, list items,
# headings, code blocks, links -- and drops the styling marks.
#
# It is a complete jq expression, so it is used by INTERPOLATING it inside
# parentheses in a larger program: `... | (' "$ADF_TO_TEXT" ')`. Interpolate it
# from a single-quoted shell string only -- the program contains $t and $href,
# which a double-quoted shell context would eat.
ADF_TO_TEXT='
def walk:
  if type == "array" then map(walk) | join("")
  elif type != "object" then ""
  elif .type == "text" then
    (.text // "") as $t
    | ( [ (.marks // [])[] | select(.type == "link") | .attrs.href ] | first ) as $href
    | if $href then ($t + " <" + $href + ">") else $t end
  elif .type == "hardBreak" then "\n"
  elif .type == "paragraph" then ((.content | walk) + "\n\n")
  elif .type == "heading" then
    ("#" * ((.attrs.level // 1)) + " " + (.content | walk) + "\n\n")
  elif .type == "listItem" then ("  - " + ((.content | walk) | rtrimstr("\n\n")) + "\n")
  elif .type == "bulletList" or .type == "orderedList" then ((.content | walk) + "\n")
  elif .type == "codeBlock" then ("```\n" + (.content | walk) + "\n```\n\n")
  elif .type == "blockquote" then ("> " + (.content | walk))
  elif .type == "rule" then "\n---\n\n"
  elif .type == "mediaSingle" or .type == "mediaGroup" then "[attachment]\n\n"
  elif .type == "inlineCard" then ((.attrs.url // "") + " ")
  elif .type == "mention" then ("@" + (.attrs.text // "" | ltrimstr("@")) + " ")
  elif .type == "emoji" then ((.attrs.text // .attrs.shortName // "") + "")
  else (.content | walk)
  end;
walk | gsub("\n{3,}"; "\n\n") | sub("\n+$"; "")'

# Shared jq preamble for the item builders. `clean` protects one-line fields
# (title, ref, status) from tabs and line breaks the renderer would trip on;
# `trunc` enforces the cache contract's 8000-character body cap.
WI_JQ_PRELUDE='
def clean: (. // "") | gsub("[\t\r\n]"; " ");
def trunc: (. // "")
  | if (length > 8000) then (.[0:8000] + "\n\n[truncated]") else . end;
'
