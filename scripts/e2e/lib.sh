# shellcheck shell=bash
# The assertion vocabulary every suite shares, plus the scratch tree it runs in.
#
# Sourced by scripts/e2e/run.sh before a suite, never run on its own. A suite
# is a plain bash file that calls the helpers below; run.sh gives each one a
# scratch directory of its own and sums the counts afterwards, so a suite that
# leaves a mess behind cannot make the next one's assertions a fiction.

# A lane is a shell with a lane's identity in its environment, and a suite run
# from one inherits it: `spoolway report <task> --fail`, the hand a scenario
# plays for the person who never came, then binds to *this* lane's step and is
# refused — "task `crash` is at `implement` now" — because the suite's own
# process claims to be a lane on `blocked` of a different task entirely. The
# suite is the harness, never a lane, so it drops the identity before anything
# reads it. Kept out of run.sh on purpose: a suite invoked directly is the
# ordinary way to debug one, and it needs the same.
unset SPOOLWAY_TASK SPOOLWAY_STEP SPOOLWAY_TASK_FILE SPOOLWAY_WORKTREE \
      SPOOLWAY_HEAD SPOOLWAY_REPO

# Absolute, always: a suite runs from its scratch directory, and `command -v`
# hands back a relative path exactly as it was given.
SPOOLWAY=${SPOOLWAY:-spoolway}
command -v "$SPOOLWAY" >/dev/null 2>&1 || [ -x "$SPOOLWAY" ] || {
  echo "no spoolway: put one on PATH or set SPOOLWAY=path/to/spoolway" >&2; exit 1; }
SPOOLWAY=$(readlink -f "$(command -v "$SPOOLWAY" || echo "$SPOOLWAY")")

pass=0
fail=0

# GitHub renders `::error::` as an annotation on the job. Without it a failure
# is a red X and five hundred lines of log to read; with it the check that
# failed is the first thing on the summary page.
annotate() {
  [ -n "${GITHUB_ACTIONS:-}" ] || return 0
  printf '::error title=%s::%s\n' "${SUITE:-e2e}" "$1"
}

ok()  { printf '  \033[32mok\033[0m    %s\n' "$1"; pass=$((pass+1)); }
bad() { printf '  \033[31mFAIL\033[0m  %s\n' "$1"; fail=$((fail+1)); annotate "$1"; }

# Setup that fails makes every assertion after it meaningless. The suites run
# without `set -e` on purpose — they count failures rather than aborting — so
# anything that has to work says so here instead of failing silently.
#
# On a failure it prints what the command said, for the reason `works` gives
# below: `SETUP the default share` on its own names the line and withholds the
# only thing that identifies the fault. A whole tier's worth of suites once
# stopped at their first `config set` on a retired profile name, and every one
# of them reported a different setup — from CI, that read as flakiness rather
# than as one rename nobody had finished.
#
# Redirected to a file rather than captured in `$(...)`: `must` is handed shell
# functions as often as binaries, and a command substitution would run them in
# a subshell where anything they set or `cd` to is lost.
must() {
  local what=$1; shift
  local log=${TMPDIR:-/tmp}/spoolway-e2e-setup.$$
  if "$@" >"$log" 2>&1; then
    rm -f "$log"
    return 0
  fi
  printf '  \033[31mSETUP\033[0m %s\n' "$what" >&2
  sed 's/^/        /' "$log" >&2
  rm -f "$log"
  annotate "setup failed: $what"
  exit 2
}

# Assert a command succeeds.
# Assert a command succeeds. On a failure it prints what the command said,
# which `refuses` and `says` have always done and this quietly did not: a green
# run is unchanged, and a red one on a machine you cannot log into is the only
# time the output was ever wanted.
works() {
  local what=$1; shift
  local out
  if out=$("$@" 2>&1); then
    ok "$what"
  else
    bad "$what"
    [ -n "$out" ] && sed 's/^/        /' <<<"$out"
  fi
}

# Assert a command fails — and, when a third argument is given, that its message
# says why. A refusal with an unhelpful message is half a bug.
refuses() {
  local what=$1 why=${2:-} ; shift 2 2>/dev/null || shift
  local out
  if out=$("$@" 2>&1); then
    bad "$what (it was accepted)"
  elif [ -n "$why" ] && ! grep -qi -- "$why" <<<"$out"; then
    bad "$what (refused, but did not say \"$why\"): $out"
  else
    ok "$what"
  fi
}

# Assert a command's exit code is exactly `want` — for the handful of commands
# spoolway hands more than one non-error ending, where `works` (only 0) and
# `refuses` (anything nonzero) are both too coarse to tell them apart.
exit_code() {
  local what=$1 want=$2; shift 2
  local out status
  out=$("$@" 2>&1); status=$?
  if [ "$status" -eq "$want" ]; then ok "$what"
  else bad "$what (exit $status, wanted $want)"; sed 's/^/        /' <<<"$out"; fi
}

# Assert a command's output does (or does not) contain a string. The output is
# captured rather than piped: `grep -q` exits on its first match, and under
# `pipefail` the SIGPIPE that gives the producer would report a match as a
# failure.
says() {
  local what=$1 want=$2; shift 2
  local out; out=$("$@" 2>&1)
  if grep -qF -- "$want" <<<"$out"; then ok "$what"; else
    bad "$what (missing \"$want\")"; sed 's/^/        /' <<<"$out"; fi
}
silent_about() {
  local what=$1 unwanted=$2; shift 2
  local out; out=$("$@" 2>&1)
  if grep -qF -- "$unwanted" <<<"$out"; then
    bad "$what (said \"$unwanted\")"; sed 's/^/        /' <<<"$out"
  else ok "$what"; fi
}

has() {
  local what=$1 want=$2 file=$3
  if grep -qF -- "$want" "$file" 2>/dev/null; then ok "$what"
  else bad "$what (no \"$want\" in $file)"; sed 's/^/        /' "$file" 2>/dev/null | head -30; fi
}

# what map route want file
#
# Assert one route's count inside one of a task's two counter maps. `prompts`
# and `rounds` are keyed identically, so a plain `has` for "review->fix: 3"
# cannot say which of them it found — and telling them apart is the entire
# point of there being two. An absent route reads as 0, which is what the task
# file means by leaving it out.
counter() {
  local what=$1 map=$2 route=$3 want=$4 file=$5 got
  got=$(awk -v map="$map:" -v route="  $route:" '
    $0 == map { inmap = 1; next }
    /^[a-z_]+:/ { inmap = 0 }
    inmap && index($0, route) == 1 { print $2; exit }
  ' "$file" 2>/dev/null)
  got=${got:-0}
  if [ "$got" = "$want" ]; then ok "$what"
  else bad "$what ($map/$route is $got, wanted $want)"; sed 's/^/        /' "$file" 2>/dev/null | head -30; fi
}

# The other half of `has`: a file that must not say something. A missing file
# counts as not saying it, which is the honest reading — there is no assertion
# here that the file exists, and the `has` beside it is usually making that one.
lacks() {
  local what=$1 unwanted=$2 file=$3
  if grep -qF -- "$unwanted" "$file" 2>/dev/null; then
    bad "$what (found \"$unwanted\" in $file)"
    grep -nF -- "$unwanted" "$file" | sed 's/^/        /' | head -10
  else ok "$what"; fi
}

# ---------------------------------------------------------------- waiting well
#
# A fixed `sleep` is a bet on how fast the machine is, and a shared CI runner
# loses that bet at the worst possible time. Everything below waits for the
# condition it actually cares about and gives up on a bound, so a slow runner is
# slow rather than red.

# Run a command until it succeeds, up to `secs` seconds.
poll_until() {
  local secs=$1; shift
  local i
  for ((i = 0; i < secs * 10; i++)); do
    "$@" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  return 1
}

# Run a command until it fails — waiting for something to go away.
poll_while() {
  local secs=$1; shift
  local i
  for ((i = 0; i < secs * 10; i++)); do
    "$@" >/dev/null 2>&1 || return 0
    sleep 0.1
  done
  return 1
}

# Wait for a file to exist and hold a string. The two are one wait: a log that
# exists but has not been written to yet is not evidence of anything.
wait_for_text() {
  local secs=$1 file=$2 want=$3
  poll_until "$secs" grep -qF -- "$want" "$file"
}

# `says`, waited for. What the board reports about a lane is a reading taken
# now, not a line appended to a file, so a suite watching for one has nothing
# to grep but the command itself. Output captured rather than piped, for the
# reason `says` gives.
_said() {
  local want=$1 out; shift
  out=$("$@" 2>&1)
  grep -qF -- "$want" <<<"$out"
}
wait_for_said() {
  local secs=$1; shift
  poll_until "$secs" _said "$@"
}

# The pid a headless lane wrote down, once it has written one — and while it
# still says so. A lane's records are the dispatcher's to tidy away when its
# turn is judged, so this answers about a lane that is *running*; there is no
# waiting here for one to finish. What a settled lane did is a fact about the
# task, and the task file is where a suite should be watching for it.
lane_pid() {
  local lane=$1 pidfile="$SPOOLWAY_PROJECT_HOME/headless/$1.pid"
  poll_until "${2:-15}" test -s "$pidfile" || return 1
  cat "$pidfile" 2>/dev/null
}

# Wait until the dispatcher has written a lane down — the session it opened
# included.
#
# `drive <task> blocked` returns on the *task file*, which the lane's own
# `spoolway report` writes, and that is well before the pass which started that
# lane has finished recording it. A stand-in's whole turn fits inside the launch
# handshake, so the window is real and it is the machine's speed that decides
# whether a scenario lands inside it.
#
# It matters for anything that frees a lane's records next. `spoolway resume`
# does, deliberately — a lane left settled from before the block would otherwise
# look to the next pass like one still holding a question — and doing it to a
# launch that has not finished confirming itself takes the pid file out from
# under it: the pass reports a lane that "was spawned but never reported a pid",
# keeps no record of it, and the session that lane opened is lost. What comes
# back is a fresh conversation where the scenario asked for a resumed one.
#
# So a scenario that unblocks and then asserts about the resumed session waits
# for this first. `lanes.json` is the dispatcher's own bookkeeping, written at
# the end of the pass that started the lane, and it is exactly what a resume
# reads to find the session to continue.
lane_on_record() {
  grep -qF "\"$1\"" "$SPOOLWAY_PROJECT_HOME/lanes.json" 2>/dev/null
}

# --------------------------------------------------------- a resident dispatcher
#
# A suite drives the product: `spoolway dispatch`, left running in the
# background exactly as a person leaves it in a pane. There is no step-one-pass
# mode to drive instead, and a suite that had one would be asserting about a
# shape spoolway does not have — the dispatcher is a loop, and what a scenario
# sets up between two of its passes is a race the real thing has too.
#
# So a scenario states the facts it is waiting for rather than counting passes:
# `drive` waits for a stage, `wait_for_text` for a line in a log, `wait_for_said`
# for a reading off the board. Every one of them is a bound on how long the
# suite will wait, never a duration it spends. Where a scenario needs the queue
# to stop moving while it looks, it says so — `drive_and_hold`.
#
# Supervised, because the loop is allowed to end. `dispatch` exits when the
# queue empties and refuses to start on an empty one, while a suite queues its
# scenarios one after another — so the supervisor restarts it, and "there is a
# dispatcher" holds for the whole suite rather than only while there is work.

# One second is the shortest a duration can express, and the interval only
# bounds how long an *idle* pass waits: a pass with work to do is followed by
# the next one as fast as the queue empties and the supervisor restarts it.
E2E_INTERVAL=${E2E_INTERVAL:-1s}

# How many rounds the supervisor below will run before giving up on its own,
# whatever `spoolway dispatch` keeps exiting. Not a guard against the restart
# storm `spoolway`'s own restart guard now refuses on its own account — this
# is the harness's own backstop, for the shape of bug that guard cannot see:
# a `dispatch` that keeps exiting 0 or 3, cleanly, forever, because a suite
# left something re-queuing itself. Sixty rounds at up to 4s apart is minutes,
# comfortably past any suite that is actually making progress.
E2E_DISPATCH_MAX_ROUNDS=${E2E_DISPATCH_MAX_ROUNDS:-60}

# What the dispatcher reads when it starts, and never again while it runs.
#
# A suite rewrites the pipeline between scenarios and turns a budget down for
# one of them, and it does that far more often than a person would — so rather
# than have every one of those edits remember to restart the dispatcher,
# `drive` compares this and restarts when it has moved. By content, not mtime:
# `add_command_step` rewrites the same file several times a second.
_config_stamp() {
  cat .spoolway/config.toml .spoolway/pipelines/*.yml 2>/dev/null | cksum
}

# Start one, if this suite has not already. Idempotent: every `drive` calls it,
# so a scenario only says this out loud when it wants a dispatcher running with
# nothing to drive yet.
dispatcher_start() {
  [ -n "${E2E_DISPATCHER_PID:-}" ] && return 0
  E2E_CONFIG_STAMP=$(_config_stamp)
  E2E_DISPATCH_DIR=${E2E_DISPATCH_DIR:-${LIVE:-${WORK:-$PWD}}}
  local pidfile="$E2E_DISPATCH_DIR/dispatcher.pid"
  rm -f "$pidfile"
  # Emptied once per suite and appended to by every dispatcher after that. A
  # restart that truncated it would leave a postmortem holding only whatever
  # the last scenario did, which is never the one being read about.
  if [ -z "${E2E_DISPATCH_LOG:-}" ]; then
    E2E_DISPATCH_LOG="$E2E_DISPATCH_DIR/dispatch.log"
    : > "$E2E_DISPATCH_LOG"
  fi
  printf -- '--- dispatcher up ---\n' >> "$E2E_DISPATCH_LOG"

  # Its own session, so stopping it is one signal to one process group and the
  # `spoolway dispatch` inside goes with it. `setsid` may or may not fork, so
  # the group leader writes its own pid rather than the shell guessing at `$!`.
  setsid bash -c '
    spoolway=$1 interval=$2 pidfile=$3 log=$4 max_rounds=$5
    echo $$ > "$pidfile"
    wait=0.5
    round=0
    while :; do
      round=$((round + 1))
      if [ "$round" -gt "$max_rounds" ]; then
        # The harness'"'"'s own backstop, not `spoolway`'"'"'s: a `dispatch` that
        # keeps exiting cleanly, forever, is not a caller in a restart storm
        # the engine can refuse — every one of these rounds genuinely ran.
        # `drive` reads the same marker either way, so a suite stuck on this
        # fails exactly as it would on a real refusal, with the reason on the
        # line above it rather than a bare timeout.
        echo "E2E-DISPATCH-REFUSED (round cap $max_rounds reached)" >> "$log"
        exit 0
      fi
      # Every round after the first — the previous one'"'"'s own result, so a
      # postmortem reads what the supervisor was doing between two lines of
      # `spoolway`'"'"'s own log rather than having to infer it from timing.
      if [ "$round" -gt 1 ]; then
        echo "round $round: previous exit $status, waited ${wait}s" >> "$log"
      fi
      "$spoolway" dispatch --plain --interval "$interval" >> "$log" 2>&1
      status=$?
      case $status in
        # A clean end (the queue emptied, an empty queue to begin with, or
        # nothing was queued yet), or the signal that stops the suite. Round
        # again — the empty-queue exit joined this set the day `spoolway`
        # gained one of its own rather than folding it into a clean 0.
        0|3|130|143) ;;
        # A dispatcher that *cannot run* does not recover on the next pass, and
        # restarting it would bury the reason under a hundred copies of itself.
        # A real lane rewriting the pipeline that governs it is how this was
        # found: the suite must fail on the refusal, not time out around it.
        #
        # `*)`, and `status` assigned above. This arm used to read `status)`,
        # which is a literal pattern an exit code can never equal — so every
        # refusal fell through to the restart below and the suite hung until
        # its timeout instead of failing with the reason on the very next line.
        *) echo "E2E-DISPATCH-REFUSED (exit $status)" >> "$log"; exit 0 ;;
      esac
      # A round that did real work resets the wait: more is probably coming.
      # One that found nothing to do doubles it, capped at four seconds, so a
      # suite between scenarios is not spun on a second-a-round poll the
      # whole time nothing is queued.
      if [ "$status" -eq 0 ]; then
        wait=0.5
      else
        wait=$(awk -v w="$wait" '"'"'BEGIN { w = w * 2; if (w > 4) w = 4; printf "%.1f", w }'"'"')
      fi
      sleep "$wait"
    done
  ' _ "$SPOOLWAY" "$E2E_INTERVAL" "$pidfile" "$E2E_DISPATCH_LOG" "$E2E_DISPATCH_MAX_ROUNDS" &
  # Untrack it right away. A case that SIGKILLs this group (the disaster suite
  # does, on purpose) would otherwise get a bash job-status notification that
  # dumps the whole supervisor body above into the suite's own output — five
  # times over a run with several kills — burying the `ok` lines the mockup
  # promises between copies of this function's source.
  disown

  poll_until 10 test -s "$pidfile" || {
    printf '  \033[31mSETUP\033[0m the dispatcher never started\n' >&2
    exit 2
  }
  E2E_DISPATCHER_PID=$(cat "$pidfile")
}

# Stop it, and everything it has running. Called from `finish` and from the
# EXIT trap below, so a suite that dies on a failed `must` does not leave a
# dispatcher writing into a tree run.sh is about to delete.
dispatcher_stop() {
  local pid=${E2E_DISPATCHER_PID:-}
  E2E_DISPATCHER_PID=""
  [ -n "$pid" ] || return 0
  kill -TERM -- "-$pid" 2>/dev/null
  poll_while 5 kill -0 -- "-$pid"
  kill -KILL -- "-$pid" 2>/dev/null
  return 0
}

# The dispatcher reads config once, when it starts. That is fine for a person —
# they restart it — and it is the one thing a suite has to say out loud after a
# `config set` or a pipeline edit whose effect it is about to assert on.
dispatcher_restart() {
  dispatcher_stop
  dispatcher_start
}

# A suite reaching its end is the ordinary exit; a failed `must` is the other
# one. Both have to take the dispatcher down with them.
trap dispatcher_stop EXIT

# ------------------------------------------------------------- the task queue
stage_of() { grep '^stage:' "$SPOOLWAY_PROJECT_HOME/queue/$1.md" 2>/dev/null | awk '{print $2}'; }
front()    { cat "$SPOOLWAY_PROJECT_HOME/queue/$1.md" 2>/dev/null; }

# The front matter's own record of where a task's worktree was cut, its
# workspace id or its tab id — one grep each, for a scenario that needs to
# notice one of the three changing (a heal, a restart) rather than read the
# whole file to find it.
worktree_of()    { grep '^worktree_path:' "$SPOOLWAY_PROJECT_HOME/queue/$1.md" 2>/dev/null | sed 's/^worktree_path: *//'; }
workspace_of()   { grep '^workspace_id:'  "$SPOOLWAY_PROJECT_HOME/queue/$1.md" 2>/dev/null | sed 's/^workspace_id: *//'; }
tab_of()         { grep '^tab_id:'        "$SPOOLWAY_PROJECT_HOME/queue/$1.md" 2>/dev/null | sed 's/^tab_id: *//'; }

# What a give-up has to show for itself, beside the log: everything still
# alive in the supervisor's own process group. A suite that timed out because
# a lane is genuinely still working looks nothing like one where the
# supervisor itself died — the log tail alone cannot tell the two apart, and
# this can.
_still_in_group() {
  local pid=${E2E_DISPATCHER_PID:-}
  [ -n "$pid" ] || return 0
  echo '  still in the group:' >&2
  ps -o pid,ppid,stat,cmd --sort=pid -g "$pid" 2>/dev/null | sed 's/^/        /' >&2
}

# Wait for a task to reach a stage ("gone" = archived out of the queue), with a
# dispatcher running to carry it there. The budget is in seconds and is a bound,
# never a duration spent: this returns the moment the task arrives.
#
# A stand-in's turn is over in milliseconds and a model's is minutes, so the
# same number in both modes would be a suite that gives up on a lane
# mid-sentence and calls it stuck — which is exactly what the first real run did.
drive() {
  local task=$1 want=$2 secs=${3:-40} stage i
  [ "${E2E_AGENTS:-mock}" = real ] && secs=$((secs * 10))
  if [ -n "${E2E_DISPATCHER_PID:-}" ] && [ "$(_config_stamp)" != "$E2E_CONFIG_STAMP" ]; then
    dispatcher_restart
  fi
  dispatcher_start
  for ((i = 0; i < secs * 5; i++)); do
    stage=$(stage_of "$task")
    case "$want" in
      gone) [ -z "$stage" ] && return 0 ;;
      *)    [ "$stage" = "$want" ] && return 0 ;;
    esac
    if grep -q E2E-DISPATCH-REFUSED "$E2E_DISPATCH_LOG" 2>/dev/null; then
      printf '  \033[31mrefused\033[0m the dispatcher would not run:\n' >&2
      tail -20 "$E2E_DISPATCH_LOG" | sed 's/^/        /' >&2
      _still_in_group
      return 1
    fi
    sleep 0.2
  done
  printf '  \033[31mgave up\033[0m waiting for %s to reach %s:\n' "$task" "$want" >&2
  tail -20 "$E2E_DISPATCH_LOG" | sed 's/^/        /' >&2
  _still_in_group
  return 1
}

# `drive`, and then hold the queue still.
#
# For a scenario whose claim is about a *window*: the stack before it merges,
# the branch before its base moves under it. The next pass would carry the task
# out of the state being asserted on, so there must not be a next pass until
# the assertions have run.
#
# It is a stage that can be waited for at all because a lane reports its own
# outcome — the task file says `land` the moment the `handover` lane says so,
# which is well before any pass has looked. Starting whatever comes next is the
# dispatcher's, and only ever happens on a pass, so stopping here holds the
# window open. The next `drive` starts a dispatcher again.
drive_and_hold() {
  local status=0
  drive "$@" || status=1
  dispatcher_stop
  return "$status"
}

# ------------------------------------------------------------------ reporting
#
# Each suite is its own process, so counts cannot come back in a variable.
# `$E2E_RESULTS` is where run.sh collects them; a suite run on its own has no
# such file and simply prints its own summary.
finish() {
  dispatcher_stop
  echo
  if [ "$fail" -eq 0 ]; then
    printf '\033[32m%d checks passed\033[0m\n' "$pass"
  else
    printf '\033[31m%d of %d checks failed\033[0m\n' "$fail" "$((pass+fail))"
  fi
  [ -n "${E2E_RESULTS:-}" ] && printf '%s %d %d\n' "${SUITE:-e2e}" "$pass" "$fail" >> "$E2E_RESULTS"
  exit $(( fail > 0 ))
}
