#!/usr/bin/env bash
# `scripts/e2e/run.sh`'s own `pr`-tier lock — a second `--tier pr` run waits
# for the first rather than running beside it.
#
# Two `pr`-tier runs on the same machine at once contend for the CPU and disk
# every other suite here assumes it has to itself — a real regression was
# bisected out of exactly that: two `--tier pr` runs in the same wall-clock
# minute left thirteen assertions failed on bounded polling, and the lane
# that read them called it flakiness rather than the regression it was.
#
# Both nested runs below point `SPOOLWAY_E2E_PR_LOCK` at a lock file of their
# own, never at the real one — this suite is itself very likely running
# inside a `pr`-tier run that already holds that lock, and nesting a second
# `--tier pr` invocation against it would deadlock the whole thing rather
# than test anything. `--suite ghost` names no real suite, so each nested
# run costs nothing beyond the lock itself: `run.sh` marks an unknown suite
# `pending` and moves on.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"

RUN="$HERE/../run.sh"
LIVE=${WORK:-$(mktemp -d)}
LOCK="$LIVE/pr.lock"

# The first run holds the lock a few seconds past acquiring it, purely so the
# second below has something to measure itself waiting on. Longer than it used
# to be: the tier this suite now runs inside of runs four suites at once (see
# `run.sh`'s own `--jobs`), and a nested `run.sh` invocation below shares that
# same contention. A short hold left too little margin between "blocked" and
# "did not block" once a loaded machine's own baseline overhead could eat a
# third of it — this suite once asserted a sub-second upper bound outright,
# which was the one assertion that could not survive the machine actually
# being busy.
HOLD=4
# The head start the second run gives the first, below. It comes off the
# wait the second can possibly measure: the lock is released `HOLD` seconds
# after the *first* run took it, so the second only ever waits
# `HOLD - SETTLE` for it.
SETTLE=0.3
# Everything here is timed in milliseconds. Whole-second `date +%s`
# arithmetic cannot measure this wait: 1.7s of real waiting truncates to
# either 1 or 2 depending on where the subsecond boundaries happen to fall,
# which failed roughly one run in three.
#
# Built from `%s` and `%N` rather than `%s%3N`, because the width on `%3N`
# is a GNU extension: uutils coreutils, which is `date` on some developer
# machines, ignores it and prints all nine digits of nanoseconds instead.
# Concatenated onto the seconds that is not a number at all, and subtracting
# two of them across a second boundary gives billions of milliseconds. One
# `date` call, so the two fields cannot straddle a second between them, and
# `10#` so a nanosecond field with leading zeros is not read as octal.
now_ms() {
  local t
  t=$(date +%s.%N)
  echo $(( ${t%.*} * 1000 + 10#${t#*.} / 1000000 ))
}
# A baseline run of the exact same shape, uncontended, timed right before the
# contended runs below: it is what "no waiting at all" costs *right now*, on
# whatever else this machine is doing. This suite now runs inside a tier that
# can itself be running three other suites at once (see `run.sh`'s own
# `--jobs`), so a fixed millisecond figure is not a bound this suite's own
# assertions can rely on any more — a busy machine's overhead is not the thing
# under test, and it grows exactly when telling "was blocked" from "was not"
# apart matters most. Comparing against this instead keeps the margin real
# whatever else is on the machine right now.
baseline_lock="$LIVE/baseline.lock"
baseline_start=$(now_ms)
SPOOLWAY_E2E_PR_LOCK="$baseline_lock" SPOOLWAY="$SPOOLWAY" \
  bash "$RUN" --tier pr --suite ghost >"$LIVE/baseline.log" 2>&1
BASELINE_MS=$(( $(now_ms) - baseline_start ))

first_log="$LIVE/first.log"
(
  SPOOLWAY_E2E_PR_LOCK="$LOCK" E2E_LOCK_HOLD="$HOLD" SPOOLWAY="$SPOOLWAY" \
    bash "$RUN" --tier pr --suite ghost >"$first_log" 2>&1
) &
first_pid=$!

# A moment for the first to actually take the lock before the second asks
# for it — otherwise the two could race for it in either order, and a second
# that happened to win would look exactly like a lock that never blocked.
sleep "$SETTLE"

start=$(now_ms)
SPOOLWAY_E2E_PR_LOCK="$LOCK" SPOOLWAY="$SPOOLWAY" \
  bash "$RUN" --tier pr --suite ghost >"$LIVE/second.log" 2>&1
second_status=$?
elapsed=$(( $(now_ms) - start ))

wait "$first_pid"
first_status=$?

if [ "$first_status" -eq 0 ] && [ "$second_status" -eq 0 ]; then
  ok "both runs still exit 0 — waiting on the lock is not a failure"
else
  bad "both runs still exit 0 — waiting on the lock is not a failure"
  printf '        first exit %s, second exit %s\n' "$first_status" "$second_status"
  sed 's/^/        /' "$first_log" "$LIVE/second.log"
fi

# The second's whole invocation — flock included — spent most of the first's
# hold waiting: it blocked rather than running straight through.
#
# The baseline plus half the hold, rather than the `HOLD - SETTLE` it should
# really take, so a loaded machine that drifts by a few hundred milliseconds
# still passes. The two outcomes are nowhere near each other anyway: a run
# that blocks waits the baseline plus about `HOLD` seconds, and one that does
# not returns in about the baseline itself.
WANT=$(( BASELINE_MS + HOLD * 1000 / 2 ))
if [ "$elapsed" -ge "$WANT" ]; then
  ok "a second \`--tier pr\` run blocks until the first releases the lock"
else
  bad "a second \`--tier pr\` run blocks until the first releases the lock"
  printf '        second run took %sms, wanted at least %sms\n' "$elapsed" "$WANT"
fi

# ------------------------------------------------------------ a lower tier is unaffected
# The same two-at-once shape, `--tier smoke` this time: nothing here should
# wait on anything, because the lock is `pr`-tier's alone.
smoke_lock="$LIVE/smoke.lock"
(
  SPOOLWAY_E2E_PR_LOCK="$smoke_lock" E2E_LOCK_HOLD="$HOLD" SPOOLWAY="$SPOOLWAY" \
    bash "$RUN" --tier smoke --suite ghost >"$LIVE/smoke-first.log" 2>&1
) &
smoke_first_pid=$!
sleep "$SETTLE"

start=$(now_ms)
SPOOLWAY_E2E_PR_LOCK="$smoke_lock" SPOOLWAY="$SPOOLWAY" \
  bash "$RUN" --tier smoke --suite ghost >"$LIVE/smoke-second.log" 2>&1
smoke_elapsed=$(( $(now_ms) - start ))
wait "$smoke_first_pid"

if [ "$smoke_elapsed" -lt "$WANT" ]; then
  ok "a lower tier never takes the lock, so two \`--tier smoke\` runs do not serialise"
else
  bad "a lower tier never takes the lock, so two \`--tier smoke\` runs do not serialise"
  printf '        second smoke run took %sms, wanted under %sms\n' "$smoke_elapsed" "$WANT"
fi

# ------------------------------------------------- how many run at once
# `--jobs` is the other half of this file's subject: the `pr` lock says how
# many `run.sh` invocations may run at once, and `--jobs` says how many suites
# one invocation runs at once. It is asserted here rather than in a suite of
# its own because this is already the one suite that runs `run.sh` itself.
#
# What needs saying is the case the flag now *refuses*. `--jobs` narrowed what
# `run.sh` accepts the day it was added: before it, there was no count to get
# wrong; after it, a zero or a word reaches an arithmetic comparison and a
# `while [ "$running" -ge "$JOBS" ]` that would either spin forever or launch
# every suite at once. The reading that proves the guard is the bad value, not
# the good one — a run with `--jobs 4` behaves identically whether the guard
# is there or not.
#
# These cost nothing and take no lock: `run.sh` validates `--jobs` before it
# opens the lock file at all. `SPOOLWAY_E2E_PR_LOCK` is pointed at a throwaway
# path anyway, so that ordering is this suite's assumption rather than its
# hostage.
jobs_lock="$LIVE/jobs.lock"
refuses "\`--jobs 0\` is refused rather than run with no lanes at all" \
  "invalid --jobs" \
  env SPOOLWAY_E2E_PR_LOCK="$jobs_lock" SPOOLWAY="$SPOOLWAY" \
  bash "$RUN" --tier pr --suite ghost --jobs 0
refuses "\`--jobs\` refuses a value that is not a number" \
  "invalid --jobs" \
  env SPOOLWAY_E2E_PR_LOCK="$jobs_lock" SPOOLWAY="$SPOOLWAY" \
  bash "$RUN" --tier pr --suite ghost --jobs four
refuses "\`--jobs\` refuses a negative count" \
  "invalid --jobs" \
  env SPOOLWAY_E2E_PR_LOCK="$jobs_lock" SPOOLWAY="$SPOOLWAY" \
  bash "$RUN" --tier pr --suite ghost --jobs -1

# And the count a person reaches for when a concurrent run's output is too
# interleaved to read: one at a time is still a run that finishes, not a
# degenerate case of the reap loop. `--tier smoke` so this takes no lock.
exit_code "\`--jobs 1\` still completes a run" 0 \
  env SPOOLWAY_E2E_PR_LOCK="$jobs_lock" SPOOLWAY="$SPOOLWAY" \
  bash "$RUN" --tier smoke --suite ghost --jobs 1

finish
