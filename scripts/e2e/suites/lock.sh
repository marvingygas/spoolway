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

# The first run holds the lock a couple of seconds past acquiring it, purely
# so the second below has something to measure itself waiting on.
HOLD=2
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
# Half the hold, rather than the `HOLD - SETTLE` it should really take, so a
# loaded machine that drifts by a few hundred milliseconds still passes. The
# two outcomes are nowhere near each other anyway: a run that blocks waits
# about 1700ms, and one that does not returns in the tens of milliseconds it
# takes `run.sh` to mark an unknown suite pending.
WANT=$(( HOLD * 1000 / 2 ))
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

finish
