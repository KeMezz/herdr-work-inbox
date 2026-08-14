#!/bin/bash
#
# Copy $1 to the LOCAL machine's clipboard.
#
# Two paths, both attempted, deliberately: OSC 52 written to /dev/tty is the
# only one that can reach the attached terminal when herdr is driven with
# --remote (the escape rides the pty back to the client), but it is UNVERIFIED
# over --remote and silently no-ops on terminals that refuse clipboard writes.
# pbcopy is verified but only ever touches the machine the script runs on. Doing
# both means the local case always works and the remote case works if the
# terminal cooperates. Exit 0 if either path ran.

set -u
export PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:${PATH}}"

url="${1:-}"
[ -n "$url" ] || { printf 'copy-link: no URL given\n' >&2; exit 2; }

ran=1

# OSC 52: ESC ] 52 ; c ; <base64> BEL. tr -d guards GNU coreutils, which wraps.
# /dev/tty rather than stdout: the caller's stdout may be a pipe someone parses.
#
# Redirect ORDER IS LOAD-BEARING: 2>/dev/null must come BEFORE >/dev/tty. bash
# applies redirects left to right and reports a failed open() on whatever stderr
# is installed at that moment. Headless (herdr [[actions]]: no controlling
# terminal) the open() of /dev/tty fails ENXIO, and with the tty redirect first
# the "Device not configured" diagnostic escapes to the real stderr. `[ -w
# /dev/tty ]` does NOT prevent this: it is an access(2) on a crw-rw-rw- device
# node, so it passes even with no controlling terminal.
b64=$(printf '%s' "$url" | base64 2>/dev/null | tr -d '\n')
if [ -n "$b64" ] && [ -w /dev/tty ]; then
  printf '\033]52;c;%s\a' "$b64" 2>/dev/null >/dev/tty && ran=0
fi

if command -v pbcopy >/dev/null 2>&1; then
  if printf '%s' "$url" | pbcopy 2>/dev/null; then
    ran=0
  fi
fi

[ "$ran" -eq 0 ] || printf 'copy-link: no clipboard path available\n' >&2
exit "$ran"
