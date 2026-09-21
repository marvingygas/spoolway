#!/usr/bin/env bash
# The one window `checks` used to have no answer for: `handover` opens a
# pull request and the very next pass calls `gh pr checks --watch
# --fail-fast` against it, sometimes before GitHub has registered a single
# check run. Real `gh` answers that with "no checks reported" and a
# non-zero exit, and the shipped `checks` step now routes that failure back
# to itself, bounded at 3 — see `src/dispatch.rs`'s `skip_wait` and
# `Report::self_route`, and `gh-stub.sh`'s own `.no_checks` marker.
#
# The claim under test is not just "it eventually reaches `done`" — a task
# that retried every attempt inside the same second would already do that,
# against a `gh` double that clears the marker between two calls close
# enough together to look simultaneous. What actually closes gh-311 is the
# gap: the second arrival at `checks` has to be a whole poll interval after
# the first, not the same second, so something outside the run — GitHub
# registering the check — has a real chance to change in between.
#
# covers: Report::self_route — a bounded self-route waits out a full poll interval before its next try
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

LIVE=${WORK:-$(mktemp -d)}
CTL="$LIVE/ctl"

new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$CTL" "" "" "" "$FORGE"

new_repo "$LIVE/proj"
configure_project plan/live "$LIVE/worktrees"
publish plan/live

# `new_forge` installs the fuller `e2e-fake-gh.sh` double, which never
# answers "no checks reported" at all — that answer is `gh-stub.sh`'s own,
# and this is the one suite that needs it in front of a real dispatch pass
# rather than only a hand-run `spoolway stack`. Same `$FORGE/prs` directory
# either double reads and writes, so nothing about the forge itself changes.
must "the checks-retry double, in place of the fuller one" \
  install -m 755 "$HERE/../gh-stub.sh" "$FORGE/bin/gh"
export GH_STUB_PRS="$FORGE/prs"

BODY="$LIVE/body.md"
task_body "$BODY"

# `gh-stub.sh` numbers pull requests from 1, and this is the first one this
# suite ever opens — so the pull request `handover` is about to open is
# knowable before it exists, and the marker can be in place before `checks`
# ever asks about it, with no race against `handover` finishing first.
touch "$GH_STUB_PRS/1.no_checks"

task_doc "$LIVE/gate.md" gate "$BODY" "group: live" "touches: [notes/gate.md]"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/gate.md"
dispatcher_start

# The line `run_command` pushes for every exit of this step, pass or fail —
# see `src/dispatch.rs`. Counted rather than matched once, the same
# discipline `command-steps.sh`'s own `arrivals` helper uses, so the second
# arrival is read off the log itself and not guessed at from a poll's own
# timing.
EXIT_LINE="\`checks\` exited"
exits() { grep -c "$EXIT_LINE" "$E2E_DISPATCH_LOG" 2>/dev/null || echo 0; }
exited_at_least() { [ "$(exits)" -ge "$1" ]; }

# `queued -> implement -> review -> document -> handover -> checks` is six
# real `dispatch::PROBE_INTERVAL` waits' worth of ground to cover before
# `checks` even runs once — the same journey `flow.sh`'s own `land` task
# gives `drive`'s 150s default, not the 60s this used to budget with no
# slack in it at all. `poll_until` does not scale for `E2E_AGENTS=real` the
# way `drive` does, below, so the budget scales here by hand instead — a
# real agent's own turn ahead of `handover` is the part that grows, not the
# fixed poll rate `checks` itself waits on.
JOURNEY_BUDGET=150
[ "${E2E_AGENTS:-mock}" = real ] && JOURNEY_BUDGET=$((JOURNEY_BUDGET * 10))

if poll_until "$JOURNEY_BUDGET" exited_at_least 1; then
  ok "the first \`checks\` attempt runs and fails on the empty rollup"
else
  bad "the first \`checks\` attempt runs and fails on the empty rollup"
  tail -20 "$E2E_DISPATCH_LOG" | sed 's/^/        /'
fi
has "naming the wording real \`gh\` uses for it" \
  "no checks reported on the 'task/gate' branch" \
  "$SPOOLWAY_PROJECT_HOME/commands/gate · checks.log"
if [ "$(stage_of gate)" = checks ]; then
  ok "the failure routed the task back to \`checks\` itself, not to \`blocked\`"
else
  bad "the failure routed the task back to \`checks\` itself, not to \`blocked\` \
(at \`$(stage_of gate)\`)"
fi
FIRST_AT=$(date +%s)

# GitHub, a few seconds later, has registered the branch's first check run —
# stood in for here by the marker simply going away, so the next attempt
# answers the way real `gh` would once there is something to report.
rm -f "$GH_STUB_PRS/1.no_checks"

if poll_until 60 exited_at_least 2; then
  SECOND_AT=$(date +%s)
  ok "a second \`checks\` attempt follows, once the rollup is no longer empty"
else
  bad "a second \`checks\` attempt follows, once the rollup is no longer empty"
  tail -20 "$E2E_DISPATCH_LOG" | sed 's/^/        /'
  SECOND_AT=$FIRST_AT
fi

GAP=$((SECOND_AT - FIRST_AT))
# `PROBE_INTERVAL` is ten real seconds and nothing in this run shortens it —
# see `src/commands/dispatch.rs`. A couple of seconds of slack for the poll
# loop's own granularity and the mock steps ahead of `checks` still leaves
# this nowhere near the sub-second gap the bug actually produced.
if [ "$GAP" -ge 9 ]; then
  ok "the second arrival at \`checks\` is a poll interval after the first (${GAP}s), not the same second"
else
  bad "the second arrival at \`checks\` is a poll interval after the first (${GAP}s), not the same second"
fi

if drive gate gone 60; then
  ok "the task reaches \`done\` and is archived, rather than parking on \`blocked\`"
else
  bad "the task reaches \`done\` and is archived, rather than parking on \`blocked\` \
(at \`$(stage_of gate)\`)"
fi
counter "the round the self-route spent is on the record" \
  rounds "checks->checks" 1 "$SPOOLWAY_PROJECT_HOME/archive/gate.md"

finish
