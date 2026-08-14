#!/bin/bash
#
# jin.work-inbox -- build the phase 2 TUI and install it as bin/work-inbox.
#
# This is the REAL install path for the front end. `herdr plugin link` skips
# [[build]] steps, so the manifest's build entry only ever runs for a future
# `herdr plugin install`; on this machine the binary exists because someone ran
# this script. Nothing else may depend on [[build]] having run -- ui.sh falls
# back to fzf precisely because this step is manual and can be skipped.
#
# Safe to run from any cwd: everything is resolved from the script's own path,
# never from $PWD. Safe to run while the popup is open: the binary is written
# beside the old one and moved into place, so a running work-inbox keeps its
# own inode instead of being overwritten under itself (macOS refuses that with
# ETXTBSY, and a half-written binary would be worse).

set -u

# Same minimal-PATH defence as the other scripts here, plus rustup's default
# location: this may be invoked from herdr (which gives its children a minimal
# PATH) or from a login shell.
export PATH="/opt/homebrew/bin:/usr/local/bin:${HOME}/.cargo/bin:${HOME}/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin${PATH:+:${PATH}}"

ROOT=$(cd "$(dirname "$0")" >/dev/null 2>&1 && pwd) || {
  printf 'build: could not resolve the plugin root from %s\n' "$0" >&2
  exit 1
}

CRATE="$ROOT/tui"
# Pinned with --target-dir rather than left to cargo: a CARGO_TARGET_DIR in the
# environment would otherwise move the output somewhere this script cannot find
# and somewhere .gitignore does not cover.
TARGET="$CRATE/target"
BUILT="$TARGET/release/work-inbox"
DEST="$ROOT/bin/work-inbox"

[ -f "$CRATE/Cargo.toml" ] || {
  printf 'build: no crate at %s (expected Cargo.toml there)\n' "$CRATE" >&2
  exit 1
}

command -v cargo >/dev/null 2>&1 || {
  printf 'build: cargo is not on PATH.\n' >&2
  printf '       Install the Rust toolchain (brew install rust, or rustup) and re-run.\n' >&2
  printf '       PATH=%s\n' "$PATH" >&2
  exit 1
}

printf 'build: %s (%s)\n' "$CRATE" "$(cargo --version 2>/dev/null)"

# No output redirection: a failing compile has to be readable. This is a manual
# step, run by a human at a terminal.
cargo build --release --manifest-path "$CRATE/Cargo.toml" --target-dir "$TARGET" || {
  printf 'build: cargo build failed -- bin/work-inbox left untouched\n' >&2
  exit 1
}

# Exact name, no guessing. Picking "the only executable in target/release" would
# quietly install the wrong file the day the crate grows a second binary or an
# example, and the name is fixed by the layout the dispatcher and the manifest
# both hardcode.
[ -x "$BUILT" ] || {
  printf 'build: cargo succeeded but %s is missing.\n' "$BUILT" >&2
  printf '       The crate must produce a binary named work-inbox.\n' >&2
  printf '       %s contains:\n' "$TARGET/release" >&2
  ls -1 "$TARGET/release" 2>/dev/null | sed 's/^/         /' >&2
  exit 1
}

mkdir -p "$ROOT/bin" || { printf 'build: could not create %s/bin\n' "$ROOT" >&2; exit 1; }

# copy-then-rename, never a copy straight onto $DEST: mv within one filesystem
# is a rename, so a work-inbox that is running right now keeps the old inode and
# nothing ever observes a partially written binary.
cp -f "$BUILT" "$DEST.new" || { printf 'build: could not stage %s.new\n' "$DEST" >&2; exit 1; }
chmod 755 "$DEST.new" || { rm -f "$DEST.new"; printf 'build: chmod failed\n' >&2; exit 1; }
mv -f "$DEST.new" "$DEST" || { rm -f "$DEST.new"; printf 'build: could not install %s\n' "$DEST" >&2; exit 1; }

printf 'build: installed %s\n' "$DEST"
ls -l "$DEST"

# Smoke test, gated on there being a cache to read. --dump is the headless mode
# every reviewer and the install checklist exercise first, so a failure here is
# worth catching now -- but on a machine that has never run collect.sh there is
# nothing to dump, and "no cache" must not be reported as a broken build. Same
# state-dir default as ui.sh, for the same reason: this is not a plugin command,
# so HERDR_PLUGIN_STATE_DIR is normally unset.
STATE_DIR="${HERDR_PLUGIN_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/herdr/plugins/jin.work-inbox}"
if [ -s "$STATE_DIR/cache.json" ]; then
  if "$DEST" --dump >/dev/null 2>&1; then
    printf 'build: %s --dump exits 0\n' "$DEST"
  else
    printf 'build: WARNING -- %s --dump did not exit 0 against %s.\n' "$DEST" "$STATE_DIR/cache.json" >&2
    printf '       The binary is installed and ui.sh will exec it. Run it by hand to see why;\n' >&2
    printf '       remove %s to fall back to fzf.\n' "$DEST" >&2
    exit 1
  fi
else
  printf 'build: no cache at %s yet, skipping the --dump smoke test\n' "$STATE_DIR/cache.json"
fi
