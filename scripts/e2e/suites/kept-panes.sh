#!/usr/bin/env bash
# A finished step's lane stays open until its task is done.
#
# A two-step task is driven through `herdr-stub.sh`. While the second step
# runs, `spoolway lane` must still list the first step's lane, and its pane
# must still stand. Once the task is done, both are gone.
#
# Against the double rather than a real server — its header says why there is
# no isolated herdr to run this on. An agent there is a registration, not a
# process, so the suite stands in for each lane's agent and reports for it:
# the thing under test is whether a pane outlives its step, and for that only
# the dispatcher's side of the report matters.
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
configure_project plan/kept
publish plan/kept

BODY="$LIVE/body.md"
task_body "$BODY"

# ---------------------------------------------------- the double, and a pipeline
HSTATE="$LIVE/herdr-stub"
HERDRBIN="$LIVE/herdr-bin"
mkdir -p "$HSTATE" "$HERDRBIN"
install -m 755 "$HERE/../herdr-stub.sh" "$HERDRBIN/herdr"
export HERDR_STUB_STATE="$HSTATE"
PATH_BEFORE_HERDR_STUB="$PATH"
PATH="$HERDRBIN:$PATH"; export PATH

must "the herdr backend" "$SPOOLWAY" config set dispatch.backend herdr
# An agent that has not reported is reminded after `lane_quiet`, and the
# fixture turns that down to a second for the suites about reminders. Nothing
# here reminds anybody: the suite reports for each lane in its own time.
must "the lane patience" "$SPOOLWAY" config set dispatch.lane_quiet 1h

MODEL=$(awk '/id: implement/ {found=1} found && /model:/ {print $2; exit}' \
  .spoolway/pipelines/default.yml)
cat > .spoolway/pipelines/twostep.yml <<YML
description: Two agent steps, so a task has a first step's lane to keep while the second runs.

steps:
  - id: first
    description: Stands in for a step that finishes and moves the task on.
    agent: pi
    prompt: builder
    model: $MODEL
    on_pass: second

  - id: second
    description: Stands in for the step that follows it.
    agent: pi
    prompt: judge
    model: $MODEL
    on_pass: done
YML
works "the two-step pipeline checks out" "$SPOOLWAY" pipeline check

# The lane a task's agent would be, reporting for itself: a lane reports on the
# step it was started for, from the task it belongs to.
report_as() { # task step
  SPOOLWAY_TASK=$1 SPOOLWAY_STEP=$2 "$SPOOLWAY" report --pass -m "done with $2"
}

lane_listed() { "$SPOOLWAY" lane 2>&1 | grep -qF "$1"; }
panes_standing() { wc -l < "$HSTATE/panes" | tr -d ' '; }

# ------------------------------------------------------------------ the task
task_doc "$LIVE/kept.md" kept "$BODY" "group: kept" "pipeline: twostep"
must "a task queued on the two-step pipeline" "$SPOOLWAY" queue add --from "$LIVE/kept.md"

dispatcher_restart

# The lane is listed the moment its agent starts, a beat before the task is
# moved onto its step, and a report before that move is refused as one about a
# step the task has not reached.
if drive kept first 60 && poll_until 60 lane_listed "kept · first"; then
  ok "the first step's lane starts"
else bad "the first step's lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi

must "the first step reports" report_as kept first
if drive kept second 60; then ok "the task moves on to the second step"
else bad "the task moves on to the second step"; fi

if poll_until 60 lane_listed "kept · second"; then ok "the second step's lane starts"
else bad "the second step's lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi

# The point of the suite: the step the task left is still there, in its own
# pane, beside the one it is on.
says "the first step's lane is still listed while the second runs" "kept · first" \
  "$SPOOLWAY" lane
says "and so is the second's" "kept · second" "$SPOOLWAY" lane
if [ "$(panes_standing)" -ge 2 ]; then
  ok "both lanes have a pane standing"
else
  bad "both lanes have a pane standing: $(cat "$HSTATE/panes")"
fi

# ----------------------------------------------------------------- the end
must "the second step reports" report_as kept second
if drive kept gone 60; then ok "the task is done and archived"
else bad "the task is done and archived"; fi

says "no lane is listed once the task is done" "no lanes are running" "$SPOOLWAY" lane
if [ "$(panes_standing)" -eq 0 ]; then
  ok "and every pane it had is closed"
else
  bad "and every pane it had is closed: $(cat "$HSTATE/panes")"
fi

"$HERDRBIN/herdr" shutdown state >/dev/null 2>&1 || true
unset HERDR_STUB_STATE
PATH="$PATH_BEFORE_HERDR_STUB"; export PATH
rm -f .spoolway/pipelines/twostep.yml

finish
