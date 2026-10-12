#!/usr/bin/env bash
# A suite that ends, or forgets a lane, leaves none of that lane's processes
# running.
#
# A headless lane is `sh -c <script>` in a process group of its own, and the
# pid file under `headless/` holds the shell's pid, which is also the group's
# id. The agent is the shell's child, so signalling the pid alone stops the
# shell and leaves the agent running with nothing above it.
#
# Two things have to stop the whole group: the `forget` helper that the
# disaster and faults suites use to abandon a task, and the exit trap in
# `lib.sh`, which has to take down every lane a suite left behind, including
# the ones it kept alive on purpose.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"

LIVE=${WORK:-$(mktemp -d)}
MARK="lane-orphans-$$-$RANDOM"

# A stand-in lane: a shell leading its own group, with a long-lived agent
# script whose name carries a marker no other process has. The shell writes
# its own pid to the pid file, as spoolway's lane script does, because `setsid`
# may or may not fork and `$!` would then be its pid rather than the group's.
start_lane() {
  local home=$1 name=$2
  mkdir -p "$home/headless"
  local agent="$home/$MARK-${name// /_}"
  printf '#!/bin/sh\nsleep 300 &\nwait\n' > "$agent"
  chmod +x "$agent"
  setsid sh -c "echo \$\$ > \"\$1\"; '$agent' & wait" _ "$home/headless/$name.pid" \
    >/dev/null 2>&1 &
  # Disowned so bash does not print a "Killed" job notice when the lane is
  # stopped; `dispatcher_start` does the same for its own background job.
  disown
  poll_until 5 pgrep -f "$agent$" >/dev/null
}

alive() { pgrep -f "/bin/sh .*$MARK-${1// /_}$" >/dev/null; }
# Stops every lane this suite started by the groups in its pid files, not by
# name: the agent's `sleep 300` child does not carry the marker, and would be
# left running under init for five minutes by exactly the regression this
# suite exists to catch.
reap() {
  local f
  for f in "$LIVE"/*/headless/*.pid; do
    [ -f "$f" ] && kill_lane_group "$(cat "$f" 2>/dev/null)"
  done
  true
}
# `lib.sh`'s own steps stay in the trap, so a suite that exits early still
# hands `run.sh` its tally.
trap 'reap; screen_stop; dispatcher_stop; lanes_stop; record_results' EXIT

# ------------------------------------------------ the suite exit trap
# A child shell sources `lib.sh`, starts two lanes and a stale pid file, then
# exits without any `forget`, the way a suite dying on a failed `must` does.
# The child inherits `E2E_RESULTS` from `run.sh`, and its own exit trap would
# write a "0 0" tally as the first line of this suite's row, which is the only
# line `run.sh` reads; `env -u` keeps the child out of that file.
home="$LIVE/trap-home"
mkdir -p "$home/headless"
echo 999999 > "$home/headless/gone.pid"
env -u E2E_RESULTS bash -c '
  source "$1/../lib.sh"
  SPOOLWAY_PROJECT_HOME=$2
  '"$(declare -f start_lane poll_until poll_while); MARK=$MARK"'
  start_lane "$SPOOLWAY_PROJECT_HOME" one
  start_lane "$SPOOLWAY_PROJECT_HOME" two
  exit 0
' _ "$HERE" "$home" >/dev/null 2>&1
if ! poll_while 5 alive one || ! poll_while 5 alive two; then
  bad "the exit trap leaves no lane process running"
  reap
else
  ok "the exit trap leaves no lane process running"
fi

# ----------------------------------------------- the forget helpers
for suite in disaster faults; do
  home="$LIVE/forget-$suite"
  stage_of() { echo implement; }
  worktree_of() { true; }
  SPOOLWAY_PROJECT_HOME=$home
  eval "$(sed -n '/^forget() {/,/^}/p' "$HERE/$suite.sh")"
  start_lane "$home" "task · implement"
  cp "$home/headless/task · implement.pid" "$home/headless/pidfile.keep"
  # `forget` runs `git branch -D` in the working directory, which must not be
  # the person's own checkout.
  cd "$home" || exit 2
  forget task >/dev/null 2>&1
  if ! poll_while 5 alive "task · implement"; then
    bad "$suite's forget stops the lane's agent, not only its shell"
    reap
  else
    ok "$suite's forget stops the lane's agent, not only its shell"
  fi
done

# ------------------------------------------------------ the pid guards
# A pid file holding 0 would, unguarded, make `kill -- -0` signal the exiting
# shell's own group. The child runs in a session of its own so that going
# wrong costs only the child, and it must still reach `record_results`, which
# writes the row checked below. No `-w`: a foreground child of this
# non-interactive shell is not a group leader, so `setsid` calls `setsid(2)`
# in place without forking, and the stand-in in `portable/` takes no flags.
home="$LIVE/guard-home"
row="$LIVE/guard-row"
mkdir -p "$home/headless"
echo 0 > "$home/headless/zero.pid"
echo abc > "$home/headless/text.pid"
: > "$home/headless/empty.pid"
: > "$row"
E2E_RESULTS=$row SUITE=guard setsid bash -c '
  source "$1/../lib.sh"
  SPOOLWAY_PROJECT_HOME=$2
  exit 0
' _ "$HERE" "$home" >/dev/null 2>&1
if [ -s "$row" ]; then
  ok "the exit trap never signals group 0 or a pid that is not a number"
else
  bad "the exit trap never signals group 0 or a pid that is not a number"
fi

finish
