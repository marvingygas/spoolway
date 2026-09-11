#!/usr/bin/env bash
# The end-to-end suites, and the one way to run them.
#
#   scripts/e2e/run.sh                the pr tier, what every push and every
#                                     pull request runs
#   scripts/e2e/run.sh --tier smoke   the fast signal, for a person running it
#                                     by hand
#   scripts/e2e/run.sh --suite flow   one suite
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
# (`commands`), and a hand-off that hands nothing over (`forge`).
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
#               actually does is scripts/e2e/plans/.
#   status      the --watch board is rendering, not routing: unit tests cover
#               it through a real pass. An e2e version would re-assert the same
#               branches through a slower path.
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
pr_suites=(flow commands stacking stack conflicts forge disaster lock trials routines jobs jobs-screen restart)
nightly_suites=(flow commands stacking stack conflicts forge disaster lock trials routines jobs jobs-screen restart)
cloud_suites=(warmth)
live_suites=(live)

TIER=pr
SUITES=()

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
    printf '  %-12s %s\n' "$name" \
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
    --keep)   KEEP=1; shift ;;
    --list)   list; exit 0 ;;
    -h|--help) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

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

ROOT=$(mktemp -d)
RESULTS="$ROOT/results"
: > "$RESULTS"
# Half the live scenarios end with detached lanes on purpose, so a teardown is
# the default and KEEP=1 is the postmortem. Set it whenever you are going to
# want to know why, because a failure nobody can reproduce is a failure nobody
# fixes.
trap 'if [ -n "${KEEP:-}" ]; then echo "kept: $ROOT"; else rm -rf "$ROOT"; fi' EXIT

echo "spoolway e2e — $("$SPOOLWAY" --version) — tier ${TIER}"

pending=()
failed=()
total_pass=0
total_fail=0

for suite in "${SUITES[@]}"; do
  file="$E2E_DIR/suites/$suite.sh"
  if [ ! -f "$file" ]; then
    pending+=("$suite")
    continue
  fi

  echo
  printf '\033[1m%s\033[0m\n' "$suite"

  work="$ROOT/$suite"
  mkdir -p "$work"
  # Its own tree, its own process. A suite that leaves a mess behind — and the
  # ones that end in `blocked` on purpose all do — cannot reach the next.
  SUITE="$suite" WORK="$work" E2E_RESULTS="$RESULTS" \
    bash "$file" || failed+=("$suite")
done

while read -r name p f; do
  total_pass=$((total_pass + p))
  total_fail=$((total_fail + f))
done < "$RESULTS"

echo
if [ ${#pending[@]} -gt 0 ]; then
  printf '\033[33mpending\033[0m  %s (no suite file yet)\n' "${pending[*]}"
fi
if [ ${#failed[@]} -eq 0 ] && [ "$total_fail" -eq 0 ]; then
  printf '\033[32m%d checks passed across %d suites\033[0m\n' \
    "$total_pass" "$(( ${#SUITES[@]} - ${#pending[@]} ))"
  exit 0
fi
printf '\033[31m%d of %d checks failed — %s\033[0m\n' \
  "$total_fail" "$((total_pass + total_fail))" "${failed[*]:-see above}"
exit 1
