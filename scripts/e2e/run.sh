#!/usr/bin/env bash
# The end-to-end suites, and the one way to run them.
#
#   scripts/e2e/run.sh                the pr tier, what `impl`'s `suite` step
#                                     and every pull request run
#   scripts/e2e/run.sh --tier smoke   the fast signal — for a person running it
#                                     by hand, and what `impl_lite`'s `suite`
#                                     step buys through `scripts/e2e-smoke.sh`
#   scripts/e2e/run.sh --suite flow   one suite
#   scripts/e2e/run.sh --jobs 1       one suite at a time, the old behaviour —
#                                     for a postmortem that needs a suite's own
#                                     output not interleaved with another's
#   scripts/e2e/run.sh --list         what there is, and which setting each case is about
#
# One instrument, one question: *does spoolway still work when it is really
# running processes?* Every lane runs a stand-in, every project is built from a
# seed written inline, and the whole thing needs `git`, `bash`, `curl`,
# `setsid` and `flock` and nothing else — so it runs on every push, on every
# pull request, and on a laptop with no model server and no multiplexer.
#
# **Only what a unit test cannot reach.** Anything decidable from files and
# exit codes belongs in `src/*.rs`, where about seven hundred tests already
# decide it against the code rather than against a fixture. What is left here
# needs a real git repository, a real detached process, or a real forge: a task
# walking the pipeline as processes (`flow`), a rebase onto a base that moved
# (`stacking`, `conflicts`), a command step's pid, log and exit file
# (`command-steps`), and a hand-off that hands nothing over (`forge`).
#
# Fifteen suites were deleted to arrive at that, and they were not lost
# coverage: `queue`, `config`, `settings`, `pipelines`, `personas`, `plan`,
# `plans`, `gates`, `sessions`, `eval`, `bugfix`, `kinds`, `parallel`,
# `escalation` and `large` all asserted things a unit test decides, through a
# slower path, in seven thousand lines of shell. The `pr` tier went from
# nineteen suites and about eleven minutes to five.
#
# The other question — *what does a real run actually do?* — is not this
# harness's, and never was. It is answered by the plans under
# scripts/e2e/plans/, queued by hand into a project scripts/e2e/scaffold.sh
# builds, driven by a real agent against a real model with a person watching.
# That is the end-to-end test of record. `--list` says which settings are
# covered there rather than here, and which are covered nowhere.
#
# A *suite* is a domain of spoolway — the queue, the escalations, the forge. A
# *tier* is a named set of suites, so a pipeline's gate has something short to
# name.
#
# Uses whatever `spoolway` is on PATH; set SPOOLWAY to point at a build:
#   SPOOLWAY=target/release/spoolway scripts/e2e/run.sh
#
# `--dry-run` is gone from `spoolway dispatch` — a run either draws where a
# person can see it or it is refused outright, with no exemption left to
# preview against. Nine cases across three suites used to prove something
# with a throwaway pass, and each one is re-expressed or removed:
#
#   stacking.sh   "nothing starts `top` while `base` is unfinished" is
#                 re-expressed against the real, resident dispatcher `drive`
#                 already starts, read once `base` is demonstrably in flight.
#   commands.sh   "a dry run says it would start the entry command step" is
#                 removed outright — the real pass `records` already drives
#                 right after it proves the same claim more strongly, and
#                 the routeless-task refusal beside it needed no `--dry-run`
#                 in the first place, since `check_task_routes` bails ahead
#                 of the lock and every write whether or not the pass is
#                 real.
#   overrides.sh  the seven piped passes that asked the overrides gate and
#                 the warnings screen what they do with no tty are
#                 re-expressed as three real, resident runs — started in
#                 their own session, watched until their log shows them past
#                 all three gates, then stopped. See `gate_run` there: with
#                 no `--dry-run` there is no early exit anywhere between the
#                 gates and `Lock::acquire`, so a piped run that reaches a
#                 gate is a run that goes on to hold the lock, and the case
#                 has to stop it rather than wait for it to end.
#
# The cursor case in `overrides.sh` is the one worth knowing about: it
# asserts a piped dispatch emits no hide/show-cursor escape, and clap's
# error text for a flag that no longer exists contains none either — so left
# calling `--dry-run` it would have gone on reporting `ok` while asking
# nothing at all.
#
# KEEP=1 leaves every scratch tree behind for a postmortem.
set -uo pipefail

E2E_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$E2E_DIR/../.." && pwd)

# What each tier runs. A suite named here that has no file yet is reported as
# pending rather than skipped quietly — this list is the plan of record, and a
# typo in it must not look like a suite that simply passed.
#
# Three domains are deliberately *not* suites here, because an end-to-end run
# is the wrong instrument for them and a hollow suite is worse than none:
#
#   guardrails  each agent kind confines its own lane to the worktree, and a
#               mock lane runs none of that, so an e2e suite would assert
#               nothing. The argv that sets it up is built and unit-tested in
#               src/agent.rs.
#   backends    parity between `herdr` and `headless` needs a multiplexer, and
#               these suites must run without one. The backend's own behaviour
#               is unit-tested in src/headless.rs, and what a multiplexer
#               actually does is scripts/e2e/plans/. `command-steps.sh`'s
#               herdr-stub cases are the exception, because they can be asked
#               of nothing but a real pane: the paned-command-step case,
#               which needs to watch a real pane open, carry an environment,
#               and close, and the environment-handover case, which needs to
#               watch an 8KB value actually reach one. Both run against
#               `scripts/e2e/herdr-stub.sh` — herdr, the one backend left,
#               chosen over a real server because it has no isolated
#               instance a suite can spin up of its own; the double's own
#               header says why. `disaster.sh` used to carry a third case
#               here, killing a real tmux server under a live agent lane to
#               prove the heal path that follows; it had no herdr equivalent
#               — the double answers no `agent start` at all, so it can host
#               a pane's own lifecycle but never a real lane — and is gone
#               with the backend it needed.
#   status      the board's *rendering* — every column, every row state:
#               unit tests cover it through a real pass, and an e2e version
#               would re-assert the same branches through a slower path.
#               There is one board per run now, focused rather than drawn a
#               second time — see `commands::dispatch::already_running`. The
#               board's keys are a different question and do have a suite:
#               `board-pause` drives
#               `p`, `P` and `U` as real keystrokes into a real dispatcher,
#               because those interrupt a live lane, write or move a task
#               file, and answer only to `enter`/`esc` once a panel is open —
#               none of which a frame comparison can see.
#
# And two tiers that are in none of the others, because each needs something a
# task's own gate cannot assume:
#
#   cloud       `warmth` runs real `claude-haiku-4-5` lanes, because the thing
#               it is about — whether a transcript Claude Code writes *today*
#               still carries the cache split warmth is read from — cannot be
#               asked of a stand-in that agrees with the parser by
#               construction. It refuses to run at all unless
#               `SPOOLWAY_E2E_CLOUD=1` is set.
#   live        `live` re-verifies the codex row against the real binary,
#               through `agent verify codex --live` — one turn, one resumed —
#               because the stand-ins were written from the rows and so cannot
#               notice a CLI changing its flag grammar. Opt-in by naming a
#               model (SPOOLWAY_E2E_CODEX_MODEL); pointed at a local endpoint
#               it spends nothing.
smoke_suites=(flow)
pr_suites=(flow commands command-steps issue-tracking stacking stack conflicts forge disaster lock trials routines jobs jobs-screen board-pause queue-unqueue restart overrides herdr-bind)
nightly_suites=(flow commands command-steps issue-tracking stacking stack conflicts forge disaster lock trials routines jobs jobs-screen board-pause queue-unqueue restart overrides upgrade herdr-bind)
cloud_suites=(warmth)
live_suites=(live)

TIER=pr
SUITES=()
# How many suites run at once. Each has its own scratch tree, its own $HOME,
# its own tmux socket where it needs one and its own dispatch lock (see the
# module comment above), so nothing here needs coordinating beyond the CPU
# and disk they share — the same contention `SPOOLWAY_E2E_PR_LOCK` already
# serialises a whole second `--tier pr` invocation against. Four, not the
# core count: the suites this buys the most from block on a real process
# more than they burn CPU, and a person's own laptop runs this too.
JOBS=${JOBS:-4}

# ------------------------------------------------------------ what is covered
#
# The settings surface itself is enumerated in `coverage.sh`. `--list` below
# reads it twice: once to print a row per setting, once to check a plan
# page's `covers:` claims against the same rows.
# shellcheck source=coverage.sh
source "$E2E_DIR/coverage.sh"

# Where a setting is covered, if anywhere — every claim, one per line.
#
# A suite declares its cases with `# covers: <setting> — <what the case is>`,
# beside the case rather than in a table here; a plan under scripts/e2e/plans/
# declares the same way inside its own coverage block, and a unit test in
# src/*.rs declares it with a `// covers:` line beside the test. So the map is
# assembled from the files that do the covering, and a case deleted takes its
# claim with it.
#
# Three sources, in the order a reader would want them: the suite that fails
# when the setting stops working, then the plan a person watches, then the unit
# test that is the only instrument able to reach it at all.
#
# **Every claim, and sorted.** This used to take `head -1` off an unsorted
# directory walk, so when two suites claimed one setting *which* claim was
# displayed depended on readdir order — and it bit: `sessions.sh` and
# `settings.sh` both claimed `session_reuse_ctx`, and the weaker "a bad value is
# refused" won on some runs and not others. A setting covered twice is
# information, not a conflict, so all of it is printed and the order is the
# filesystem's no longer.
coverage_for() {
  local setting=$1 hit
  hit=$(grep -rhE "^# covers: ${setting//./\\.}( |—|$)" "$E2E_DIR/suites" 2>/dev/null \
        | sed 's/^# covers: [^ ]* *—* *//' | sort -u)
  if [ -n "$hit" ]; then printf '%s\n' "$hit"; return 0; fi

  local plan found=
  for plan in "$E2E_DIR"/plans/*.html; do
    [ -e "$plan" ] || continue
    # A plan's coverage block is a `<pre>` in a page, so the first line of it
    # carries the tag ahead of the claim.
    if grep -qE "^( *|<pre>)covers: ${setting//./\\.}( |—|$)" "$plan"; then
      printf 'no case — plans/%s\n' "$(basename "$plan" .html)"
      found=1
    fi
  done
  [ -n "$found" ] && return 0

  # A setting no headless *suite* can reach, because what reads it is behind a
  # door an end-to-end run never opens. The claim is a comment beside the test,
  # indented like the code around it, and what is printed is the file to look
  # in — a unit test is not an end-to-end case, so the row still counts as one
  # of the gaps and only stops saying nothing.
  local unit
  unit=$(grep -rlE "^[[:space:]]*// covers: ${setting//./\\.}( |—|$)" \
         "$REPO/src" 2>/dev/null | head -1)
  if [ -n "$unit" ]; then
    printf 'no case — unit %s\n' "${unit#"$REPO/"}"
    return 0
  fi
  return 1
}

list() {
  printf 'suites:\n'
  local file name cases
  for file in "$E2E_DIR"/suites/*.sh; do
    [ -e "$file" ] || continue
    name=$(basename "$file" .sh)
    cases=$(grep -c '^# covers:' "$file")
    printf '  %-15s %s\n' "$name" \
      "$([ "$cases" -gt 0 ] && echo "$cases setting(s)" || echo "-")"
  done

  printf '\nsettings:\n'
  local setting where first claim uncovered=0 total=0
  while read -r setting; do
    [ -n "$setting" ] || continue
    total=$((total + 1))
    if where=$(coverage_for "$setting"); then
      # The first claim on the setting's own row, the rest under it. A setting
      # two suites cover reads as two lines rather than as whichever one the
      # directory walk happened to reach first.
      first=1
      while IFS= read -r claim; do
        case "$first" in
          1) printf '  %-28s %s\n' "$setting" "$claim"; first= ;;
          *) printf '  %-28s %s\n' "" "$claim" ;;
        esac
      done <<<"$where"
      case "$where" in "no case"*) uncovered=$((uncovered + 1)) ;; esac
    else
      printf '  %-28s \033[33mno case\033[0m\n' "$setting"
      uncovered=$((uncovered + 1))
    fi
  done < <(config_settings; pipeline_settings)

  printf '\n  %d of %d settings have no case here.\n' "$uncovered" "$total"
  printf '  A `no case — plans/x` is covered by a plan run instead: only a person\n'
  printf '  with a multiplexer can see it. A `no case — unit <path>` is covered by a\n'
  printf '  unit test there. A bare `no case` is a gap.\n'

  printf '\ntiers:\n  smoke    %s\n  pr       %s\n  nightly  %s\n  cloud    %s\n  live     %s\n' \
    "${smoke_suites[*]}" "${pr_suites[*]}" "${nightly_suites[*]}" "${cloud_suites[*]}" \
    "${live_suites[*]}"
  printf '\n  The cloud tier spends real tokens and runs only with SPOOLWAY_E2E_CLOUD=1.\n'
  printf '  The live tier runs the real codex binary only with\n'
  printf '  SPOOLWAY_E2E_CODEX_MODEL set.\n'
}

while [ $# -gt 0 ]; do
  case "$1" in
    --tier)   TIER=$2; shift 2 ;;
    --suite)  SUITES+=("$2"); shift 2 ;;
    --jobs)   JOBS=$2; shift 2 ;;
    --keep)   KEEP=1; shift ;;
    --list)   list; exit 0 ;;
    -h|--help) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

case "$JOBS" in
  *[!0-9]* | '') echo "invalid --jobs: $JOBS (expected a positive integer)" >&2; exit 2 ;;
esac
[ "$JOBS" -ge 1 ] || { echo "invalid --jobs: $JOBS (expected a positive integer)" >&2; exit 2; }

if [ ${#SUITES[@]} -eq 0 ]; then
  case "$TIER" in
    smoke)   SUITES=("${smoke_suites[@]}") ;;
    pr)      SUITES=("${pr_suites[@]}") ;;
    nightly) SUITES=("${nightly_suites[@]}") ;;
    cloud)   SUITES=("${cloud_suites[@]}") ;;
    live)    SUITES=("${live_suites[@]}") ;;
    *) echo "unknown tier: $TIER (expected smoke, pr, nightly, cloud or live)" >&2; exit 2 ;;
  esac
fi

SPOOLWAY=${SPOOLWAY:-spoolway}
command -v "$SPOOLWAY" >/dev/null 2>&1 || [ -x "$SPOOLWAY" ] || {
  echo "no spoolway: put one on PATH or set SPOOLWAY=path/to/spoolway" >&2; exit 1; }
SPOOLWAY=$(readlink -f "$(command -v "$SPOOLWAY" || echo "$SPOOLWAY")")
export SPOOLWAY
# A command step (`run: spoolway stack`, `run: spoolway pipeline check`, …)
# shells out to the bare name, resolved from the dispatcher's own PATH — it
# never reads $SPOOLWAY, which only this harness's own CLI calls honour. Put
# the build's directory first so those subprocesses see the same binary this
# run is testing, not whatever the machine already had installed.
PATH="$(dirname "$SPOOLWAY"):$PATH"
export PATH

# --------------------------------------------------------- one pr tier at a time
#
# Two `pr`-tier runs on the same machine at once contend for the CPU and disk
# the suites below assume they have to themselves. A real regression was
# bisected out of exactly that: two `--tier pr` runs in the same wall-clock
# minute left thirteen assertions failed on bounded polling, and the lane
# that read them called it flakiness rather than the regression it was.
#
# `flock` on a fixed file, held for the whole run: a second `pr`-tier
# invocation blocks here until the first releases it — at process exit,
# whether that is a clean finish or this script being killed — rather than
# refusing outright, so nothing here needs a retry loop of its own. `smoke`
# and `nightly` are unaffected; they are rare and light enough that this
# contention was never observed against them.
#
# `SPOOLWAY_E2E_PR_LOCK` overrides the path — `scripts/e2e/suites/lock.sh`
# uses this to test the blocking itself against a lock of its own, rather
# than nesting a second `--tier pr` run against the very lock this run is
# already holding, which would deadlock the whole thing instead of testing
# anything.
if [ "$TIER" = pr ]; then
  LOCK="${SPOOLWAY_E2E_PR_LOCK:-${TMPDIR:-/tmp}/spoolway-e2e-pr.lock}"
  exec {E2E_PR_LOCK_FD}>"$LOCK"
  flock "$E2E_PR_LOCK_FD"
  # Test-only, for `lock.sh`: holds the lock this many seconds past
  # acquiring it, so a suite driving two overlapping invocations has
  # something to actually measure the second one waiting on.
  [ -n "${E2E_LOCK_HOLD:-}" ] && sleep "$E2E_LOCK_HOLD"
fi

# --------------------------------------------------------- stale scratch roots
#
# The common kill path is SIGTERM, a full second before SIGKILL escalates
# (`crate::headless::kill_group`, src/headless.rs) — this whole process
# group, this script included. Bash already runs the EXIT trap on SIGTERM,
# so TERM and INT are trapped explicitly below only to exit with the
# conventional 143/130 rather than however bash's own re-raise would end
# it, and a person's own Ctrl-C gets the same clean stop. SIGKILL cannot be
# trapped at all, and is the one case nothing in *this* run can survive
# being the target of: 54 roots holding 165 MB piled up in /tmp this way,
# and every run now sweeps whatever an earlier, SIGKILLed one left behind
# before it starts, as the backstop that case still needs.
#
# The pid is embedded in the root's own name — right after the fixed
# prefix, before `mktemp`'s own random suffix — so staleness is decidable
# without a lock: a directory whose pid no longer exists belonged to a run
# that is over, one way or another, and is safe to remove. A `.keep` marker
# — written below, by a run that finished and was asked to keep its tree —
# is the one root a sweep leaves alone even though its process has since
# exited normally. The random suffix is what keeps that promise on a reused
# pid: without it, a later run drawing the same pid would `rm -rf` an
# earlier one's kept tree outright on the very next line.
sweep_stale_roots() {
  local dir base pid
  for dir in "${TMPDIR:-/tmp}"/spoolway-e2e-run.*; do
    [ -d "$dir" ] || continue
    [ -e "$dir/.keep" ] && continue
    base=${dir##*/}
    pid=${base#spoolway-e2e-run.}
    pid=${pid%%.*}
    case "$pid" in '' | *[!0-9]*) continue ;; esac
    kill -0 "$pid" 2>/dev/null && continue
    rm -rf "$dir"
  done
}
sweep_stale_roots

# `mktemp -d` still does the two things it always did — created with mode
# 0700, and a name nothing else on the machine could have pre-created to
# race — the pid prefix rides along on top of its own random suffix rather
# than replacing it.
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/spoolway-e2e-run.$$.XXXXXX")
RESULTS="$ROOT/results"
: > "$RESULTS"
# Half the live scenarios end with detached lanes on purpose, so a teardown is
# the default and KEEP=1 is the postmortem. Set it whenever you are going to
# want to know why, because a failure nobody can reproduce is a failure nobody
# fixes. The marker is what tells the next run's sweep above this tree was
# kept on purpose, not left behind by a kill.
cleanup() {
  if [ -n "${KEEP:-}" ]; then
    touch "$ROOT/.keep" 2>/dev/null
    echo "kept: $ROOT"
  else
    rm -rf "$ROOT"
  fi
}
trap cleanup EXIT
# TERM and INT run the same cleanup, then clear the EXIT trap and exit
# explicitly — an untrapped signal terminates on its own, but trapping one
# suppresses that default, so a handler that never calls `exit` would leave
# this script running past the kill it was just sent. Clearing EXIT first
# stops `cleanup` from running a second time on the way out.
trap 'cleanup; trap - EXIT; exit 143' TERM
trap 'cleanup; trap - EXIT; exit 130' INT

echo "spoolway e2e — $("$SPOOLWAY" --version) — tier ${TIER} — ${JOBS} at a time"
echo

pending=()
failed=()
total_pass=0
total_fail=0

# A suite that hangs has to fail, and fail naming itself. Without a bound it is
# indistinguishable from a slow one: the run stops printing, and the only thing
# that ever ends it is the `suite` step's own 45-minute budget or a CI job
# timeout — both of which report a whole tier that took too long rather than the
# suite that stopped. `task/explicit-task-route` spent two of those before
# anybody read the last `ok` and saw where it had got to, and the hang itself
# was a race that passed on a quiet machine and caught on a loaded one, so the
# lane never saw it at all.
#
# TERM first, so each suite's own EXIT trap still takes its dispatcher and its
# lanes down rather than orphaning them into the next suite's tree; KILL after a
# grace period for whatever ignored it. The budget scales with `E2E_AGENTS` the
# same way `drive` scales its own waits — a stand-in's turn is over in
# milliseconds and a real model's takes minutes.
#
# Thirty minutes rather than the ten it was while `E2E_INTERVAL` existed. The
# dispatcher's rate is fixed now, so every transition a suite drives costs a
# real `dispatch::PROBE_INTERVAL` and no knob can buy it back: `commands`, the
# longest of them, measured a little over twenty-one minutes where it used to
# fit inside ten. Ten would report it as hung, which is the one thing this
# bound exists not to say about a suite that is merely slow.
E2E_SUITE_TIMEOUT=${E2E_SUITE_TIMEOUT:-30m}
if [ "${E2E_AGENTS:-mock}" = real ]; then
  E2E_SUITE_TIMEOUT=${E2E_SUITE_TIMEOUT_REAL:-100m}
fi

# ------------------------------------------------------------------ the clock
#
# Every change made to these suites claims seconds, and until this was here
# those seconds could only be reconstructed from CI log timestamps after the
# fact — which has already cost something: three timing budgets were last
# re-sized by estimate, and one of the three was a watchdog calling a merely
# slow suite hung. A suite runs in a process of its own, so this loop is the
# only place that knows a suite's whole wall clock, including the part a
# timed-out one spends being killed.
#
# Milliseconds off a single `date` call, so the seconds and the nanoseconds
# cannot straddle a second between them, and `10#` so a nanosecond field with
# leading zeros is not read as octal. `scripts/e2e/suites/lock.sh` builds the
# same clock and its header says why neither is `%s%3N`: the width on `%3N` is
# a GNU extension, and uutils coreutils — which is `date` on some developer
# machines — ignores it and prints all nine digits instead.
now_ms() {
  local t
  t=$(date +%s.%N)
  echo $(( ${t%.*} * 1000 + 10#${t#*.} / 1000000 ))
}

# Milliseconds as the tenth of a second the summary is read in. Integer
# arithmetic throughout: what is stored is exact and sorts with a plain
# `sort -n`, and nothing here has to parse a decimal back out of a field.
secs() {
  local tenths=$(( ($1 + 50) / 100 ))
  printf '%d.%ds' "$((tenths / 10))" "$((tenths % 10))"
}

# --------------------------------------------------------- running a tier at once
#
# Each suite already has its own scratch tree, its own $HOME, its own tmux
# socket where it needs one and its own dispatch lock (see the module comment
# at the top of this file), so nothing here needs coordinating beyond letting
# up to $JOBS of them have a process at once. A suite is launched as a
# background subshell of this script and reaped with `wait -n`, which returns
# the moment *any* of them exits — not the pid-returning `-n -p` form, so this
# still runs on a bash older than 5.1. Which one just finished is read off the
# filesystem afterwards (`kill -0` on each tracked pid) rather than trusted to
# `wait -n`'s own exit status, which is only ever the one job's.
declare -A SUITE_OF=() WORK_OF=() ROW_OF=() LOG_OF=() STARTED_OF=()
running=0

# A suite's own ok/bad lines interleaved with three others' would be illegible,
# so with more than one job at a time its output is captured rather than
# streamed — the summary row below is what a concurrent run reads live, and a
# failure's log is dumped under its row the moment it is reaped. `--jobs 1`
# keeps the old, simpler shape for a postmortem on one suite: streamed live,
# with nothing captured to repeat back afterwards.
launch_suite() {
  local suite=$1 file=$2
  local work="$ROOT/$suite"
  mkdir -p "$work"
  # The suite writes its tally into a file of its own rather than straight into
  # the results file, because the duration beside it is this process's to add
  # and only the row they make together is worth keeping. Folding the two here
  # is also what puts a row in the results file for a suite that was killed
  # before it could write one at all.
  local row="$ROOT/$suite.row"
  : > "$row"
  local log="$work/output.log"
  local started
  started=$(now_ms)
  # Its own tree, its own process. A suite that leaves a mess behind — and the
  # ones that end in `blocked` on purpose all do — cannot reach the next.
  (
    # Nothing a suite starts may inherit the `pr` lock. The fd above is held
    # open for this whole run, so without this every process a suite forks
    # gets a copy of it — including the detached lanes and stand-ins that
    # `disaster` and `board-pause` leave running on purpose. Those outlive
    # the run, get reparented to init, and go on holding the lock from a
    # scratch tree that no longer exists, so the *next* `--tier pr` run on
    # the machine blocks in `flock` forever: before any suite starts, and so
    # before the per-suite watchdog covers anything. Observed exactly that
    # way on a dev box, with `pi` doubles from a finished run still pinning
    # the file. Closing it here costs the suite nothing — it is this
    # script's lock, not the suite's, and `lock.sh`'s nested runs each open
    # their own.
    if [ -n "${E2E_PR_LOCK_FD:-}" ]; then exec {E2E_PR_LOCK_FD}>&-; fi
    status=0
    if [ "$JOBS" -eq 1 ]; then
      SUITE="$suite" WORK="$work" E2E_RESULTS="$row" \
        timeout -k 30s "$E2E_SUITE_TIMEOUT" bash "$file" 2>&1 | tee "$log"
      status=${PIPESTATUS[0]}
    else
      SUITE="$suite" WORK="$work" E2E_RESULTS="$row" \
        timeout -k 30s "$E2E_SUITE_TIMEOUT" bash "$file" >"$log" 2>&1
      status=$?
    fi
    echo "$status" > "$work/exit-status"
  ) &
  local pid=$!
  SUITE_OF[$pid]=$suite
  WORK_OF[$pid]=$work
  ROW_OF[$pid]=$row
  LOG_OF[$pid]=$log
  STARTED_OF[$pid]=$started
  running=$((running + 1))
}

# What a failing suite leaves behind, for a reader who was not watching.
#
# With more than one job at a time a suite's output is captured, never
# streamed, and `$ROOT` goes with the run — so whatever is printed here is the
# *whole* record of what failed. A blind `tail` is not that record: `forge`
# ends its one failing scenario by dumping the task document it drove, which
# is longer than thirty lines, so all three of its failed checks were pushed
# off the end and the run's own output named none of them. The suite had to be
# re-run before the failure could be read at all.
#
# So: the failed checks by name first — they are one line each, straight from
# lib.sh's `bad` — then the tail, which is the context around the last of them
# and, for a suite killed before it could fail anything, the only thing there
# is. The log itself is copied somewhere that outlives `$ROOT`, one file per
# suite so a run overwrites its own rather than piling up, because thirty
# lines is a pointer and the postmortem usually wants the rest.
dump_failure() {
  local log=$1 suite=$2 marker named kept
  marker=$(printf '  \033[31mFAIL\033[0m  ')
  named=$(grep -aF "$marker" "$log" | head -30)
  if [ -n "$named" ]; then
    printf '%s\n' "$named" | sed 's/^/        /' >&2
    # The tail without the failures already listed above, which are otherwise
    # printed twice for any suite whose last check is the one that failed.
    tail -30 "$log" | grep -avF "$marker" | sed 's/^/        /' >&2
  else
    tail -30 "$log" | sed 's/^/        /' >&2
  fi
  kept="${TMPDIR:-/tmp}/spoolway-e2e-fail.$suite.log"
  if cp "$log" "$kept" 2>/dev/null; then
    printf '  \033[31mlog\033[0m     %s\n' "$kept" >&2
  fi
}

# The suite's own line, and it comes after the suite rather than before it
# because the duration does not exist until the suite is over. Watched live
# that costs nothing a reader needs: the last line printed names the suite
# *before* this one, so the tier list above says which one is running, and a
# suite that hangs is named by this line and the TIMEOUT printed under it.
#
# Duration first, right-aligned in a column of its own — a run reads as a
# column of durations rather than as whatever each suite's name left — then
# the suite name, then its own tally. That tally is not decoration: with more
# than one job at a time a suite's stdout is captured rather than streamed
# (see `launch_suite`), so lib.sh's own "N checks passed" line never reaches
# the terminal at all, and this row is the only place a green concurrent run
# says how many checks a suite actually ran.
#
# Rows print in the order suites *finish*, not the order they were launched —
# concurrent, that is the only order there is.
report_suite() {
  local pid=$1
  local suite=${SUITE_OF[$pid]} work=${WORK_OF[$pid]} row=${ROW_OF[$pid]} \
        log=${LOG_OF[$pid]} started=${STARTED_OF[$pid]}
  local elapsed=$(( $(now_ms) - started ))
  local status
  status=$(cat "$work/exit-status" 2>/dev/null || echo 1)

  # `read` on an empty file leaves both fields empty, which is a suite killed
  # before lib.sh's EXIT trap could record anything — one that sat through the
  # TERM and had to be put down. Its counts are unknowable and stay zero; the
  # time it spent is not, and reporting that is the whole point of the row.
  local suite_pass suite_fail
  read -r _ suite_pass suite_fail < "$row"
  printf '%s %d %d %d\n' \
    "$suite" "${suite_pass:-0}" "${suite_fail:-0}" "$elapsed" >> "$RESULTS"

  if [ "${suite_fail:-0}" -eq 0 ]; then
    printf '%6s  \033[1m%-15s\033[0m%5d checks passed\n' \
      "$(secs "$elapsed")" "$suite" "${suite_pass:-0}"
  else
    printf '%6s  \033[1m%-15s\033[0m%5d of %d checks failed\n' \
      "$(secs "$elapsed")" "$suite" "${suite_fail:-0}" "$((${suite_pass:-0} + suite_fail))"
  fi

  # 124 is `timeout`'s own verdict; 137 is a suite that sat through the TERM
  # and had to be killed. Either way it is the budget that ended this, not the
  # suite, so it is reported as the one thing a bare non-zero exit cannot say.
  if [ "$status" -eq 124 ] || [ "$status" -eq 137 ]; then
    printf '  \033[31mTIMEOUT\033[0m  no further in %s — hung, not slow\n' \
      "$E2E_SUITE_TIMEOUT" >&2
    failed+=("$suite (timed out)")
    [ "$JOBS" -eq 1 ] || dump_failure "$log" "$suite"
  elif [ "$status" -ne 0 ]; then
    failed+=("$suite")
    [ "$JOBS" -eq 1 ] || dump_failure "$log" "$suite"
  fi

  unset 'SUITE_OF[$pid]' 'WORK_OF[$pid]' 'ROW_OF[$pid]' 'LOG_OF[$pid]' 'STARTED_OF[$pid]'
  running=$((running - 1))
}

# Every tracked pid still holding its slot: still running, so left for the
# next `wait -n`.
reap_finished() {
  local pid
  for pid in "${!SUITE_OF[@]}"; do
    kill -0 "$pid" 2>/dev/null && continue
    report_suite "$pid"
  done
}

tier_started=$(now_ms)

for suite in "${SUITES[@]}"; do
  file="$E2E_DIR/suites/$suite.sh"
  if [ ! -f "$file" ]; then
    pending+=("$suite")
    continue
  fi

  while [ "$running" -ge "$JOBS" ]; do
    wait -n 2>/dev/null || true
    reap_finished
  done
  launch_suite "$suite" "$file"
done

while [ "$running" -gt 0 ]; do
  wait -n 2>/dev/null || true
  reap_finished
done

tier_ms=$(( $(now_ms) - tier_started ))

while read -r name p f ms; do
  total_pass=$((total_pass + p))
  total_fail=$((total_fail + f))
done < "$RESULTS"

# The one that matters under concurrency: with suites running at once, the
# tier's own wall clock cannot go below its longest suite (see the module
# comment on `JOBS`), so this is the lane a tier is tuned against. The whole
# table is the results file itself, which KEEP=1 leaves behind.
slowest=
read -r name p f ms < <(sort -k4 -nr "$RESULTS" | head -1)
[ -n "${name:-}" ] && slowest="$name $(secs "$ms")"

echo
if [ ${#pending[@]} -gt 0 ]; then
  printf '\033[33mpending\033[0m  %s (no suite file yet)\n' "${pending[*]}"
fi
# The verdict, held rather than exited on, so the longest-lane line below is
# printed whichever way the run went — a red tier is the one most worth
# knowing the shape of.
rc=0
if [ ${#failed[@]} -eq 0 ] && [ "$total_fail" -eq 0 ]; then
  printf '\033[32m%d checks passed across %d suites in %s\033[0m\n' \
    "$total_pass" "$(( ${#SUITES[@]} - ${#pending[@]} ))" "$(secs "$tier_ms")"
else
  printf '\033[31m%d of %d checks failed in %s — %s\033[0m\n' \
    "$total_fail" "$((total_pass + total_fail))" "$(secs "$tier_ms")" \
    "${failed[*]:-see above}"
  rc=1
fi
[ -n "$slowest" ] && printf 'longest lane  %s\n' "$slowest"
exit "$rc"
