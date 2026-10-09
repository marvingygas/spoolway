# shellcheck shell=bash
# Puts stand-ins for `setsid` and `flock` on PATH when the machine has none.
#
# Sourced by run.sh and lib.sh, never run on its own. The harness calls both
# commands by name — `lib.sh` detaches the dispatcher with `setsid`, and
# `herdr-stub.sh` numbers its panes under `flock` — and they are util-linux
# programs that macOS does not ship (Homebrew's `util-linux` is keg-only, so
# installing it does not put them on PATH either). A Mac runner is the only
# Mac this repo is tested on, so the harness brings its own rather than asking
# every runner to install a package.
#
# The stand-ins live in `portable/` and are perl, which macOS does ship. They
# are appended to PATH, never prepended: a machine with the real commands keeps
# using them, and `--tier pr` on Linux is unchanged.
_portable_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/portable
for _portable_cmd in setsid flock; do
  if ! command -v "$_portable_cmd" >/dev/null 2>&1; then
    PATH="$PATH:$_portable_dir"
    export PATH
    break
  fi
done
unset _portable_dir _portable_cmd
