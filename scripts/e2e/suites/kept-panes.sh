#!/usr/bin/env bash
# A finished step's lane stays open until its task is done, or until
# `dispatch.visible_lanes` pushes it out.
#
# A two-step task is driven through `herdr-stub.sh`. While the second step
# runs, `spoolway lane` must still list the first step's lane, and its pane
# must still stand. Once the task is done, both are gone. A second task, run
# with a limit of 1, shows the first step's lane gone while the second runs.
# A four-step task then runs with a limit of 3, which closes the first lane
# once the fourth starts and leaves the other three, and with `all`, which
# closes none.
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
cat > .spoolway/pipelines/fourstep.yml <<YML
description: Four agent steps, so a task can run past a limit of three.

steps:
  - id: first
    description: The first of four.
    agent: pi
    prompt: builder
    model: $MODEL
    on_pass: second

  - id: second
    description: The second of four.
    agent: pi
    prompt: judge
    model: $MODEL
    on_pass: third

  - id: third
    description: The third of four.
    agent: pi
    prompt: builder
    model: $MODEL
    on_pass: fourth

  - id: fourth
    description: The fourth of four.
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

# ------------------------------------------------------ with a limit of 1
# The same two-step task, with a limit of 1: the first step's lane is closed
# behind the second, so only the second is left to list.
must "a limit of 1" "$SPOOLWAY" config set dispatch.visible_lanes 1
task_doc "$LIVE/off.md" off "$BODY" "group: off" "pipeline: twostep"
must "a task queued with a limit of 1" "$SPOOLWAY" queue add --from "$LIVE/off.md"

if drive off first 60 && poll_until 60 lane_listed "off · first"; then
  ok "with a limit of 1, the first step's lane starts"
else bad "with a limit of 1, the first step's lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi

must "the first step reports" report_as off first
if drive off second 60 && poll_until 60 lane_listed "off · second"; then
  ok "with a limit of 1, the second step's lane starts"
else bad "with a limit of 1, the second step's lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi

if poll_while 60 lane_listed "off · first"; then
  ok "the first step's lane is gone once the second is running"
else
  bad "the first step's lane is gone once the second is running"
  "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'
fi
says "and the second step's lane is the one left" "off · second" "$SPOOLWAY" lane

must "the second step reports" report_as off second
if drive off gone 60; then ok "the task with a limit of 1 is done and archived"
else bad "the task with a limit of 1 is done and archived"; fi

# Run a task through `steps`, leaving the last one's lane running: each lane
# starts, and every step but the last reports.
start_steps() { # task step...
  local task=$1 step n i=0; shift; n=$#
  for step in "$@"; do
    i=$((i + 1))
    if drive "$task" "$step" 60 && poll_until 60 lane_listed "$task · $step"; then
      ok "$task: the $step step's lane starts"
    else bad "$task: the $step step's lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi
    if [ "$i" -lt "$n" ]; then must "$task: $step reports" report_as "$task" "$step"; fi
  done
}

# ------------------------------------------------------ with a limit of 3
# Four steps against a limit of 3: the fourth lane's start closes the oldest,
# the first, and leaves the three newest.
must "a limit of 3" "$SPOOLWAY" config set dispatch.visible_lanes 3
task_doc "$LIVE/three.md" three "$BODY" "group: three" "pipeline: fourstep"
must "a task queued with a limit of 3" "$SPOOLWAY" queue add --from "$LIVE/three.md"
start_steps three first second third fourth

if poll_while 60 lane_listed "three · first"; then
  ok "with a limit of 3, the oldest lane is gone once the fourth runs"
else
  bad "with a limit of 3, the oldest lane is gone once the fourth runs"
  "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'
fi
for step in second third fourth; do
  says "and the $step step's lane is still listed" "three · $step" "$SPOOLWAY" lane
done
if [ "$(panes_standing)" -eq 3 ]; then
  ok "three panes stand"
else
  bad "three panes stand: $(cat "$HSTATE/panes")"
fi
must "the fourth step reports" report_as three fourth
if drive three gone 60; then ok "the task with a limit of 3 is done and archived"
else bad "the task with a limit of 3 is done and archived"; fi

# ------------------------------------------------------ with `all`
must "every lane kept" "$SPOOLWAY" config set dispatch.visible_lanes all
task_doc "$LIVE/every.md" every "$BODY" "group: every" "pipeline: fourstep"
must "a task queued with every lane kept" "$SPOOLWAY" queue add --from "$LIVE/every.md"
start_steps every first second third fourth
for step in first second third fourth; do
  says "with all, the $step step's lane is still listed" "every · $step" "$SPOOLWAY" lane
done
if [ "$(panes_standing)" -eq 4 ]; then
  ok "four panes stand"
else
  bad "four panes stand: $(cat "$HSTATE/panes")"
fi
must "the fourth step reports" report_as every fourth
if drive every gone 60; then ok "the task with every lane kept is done and archived"
else bad "the task with every lane kept is done and archived"; fi
must "the limit back at its default" "$SPOOLWAY" config set dispatch.visible_lanes 5

# ------------------------------------------------------ command steps
# A command step opens no pane by default, and opens one with
# `dispatch.hide_commands` set to `false`. Every `pane run` the double is asked
# for is one line in `typed.index`, and a command pane is typed its script, so
# the number of lines before and after a task is whether a pane was opened.
# The agent steps above type into their panes too, but none runs here.
cat > .spoolway/pipelines/gated.yml <<YML
description: One command step, for whether it opens a pane.

steps:
  - id: gate
    description: Waits for the suite to read its log, then passes.
    run: 'echo gate-marker; while [ ! -e "\$GATE_RELEASE" ]; do sleep 0.1; done'
    on_pass: done
    on_fail: blocked
YML
works "the command pipeline checks out" "$SPOOLWAY" pipeline check
typed_lines() { wc -l < "$HSTATE/typed.index" | tr -d ' '; }

export GATE_RELEASE="$LIVE/gate.release"
rm -f "$GATE_RELEASE"
BEFORE=$(typed_lines)
task_doc "$LIVE/hidden.md" hidden "$BODY" "group: hidden" "pipeline: gated"
must "a task queued with commands hidden" "$SPOOLWAY" queue add --from "$LIVE/hidden.md"
records "a hidden command's output is in the same log" "gate-marker" \
  "$SPOOLWAY_PROJECT_HOME/commands/hidden · gate.log" hidden
touch "$GATE_RELEASE"
if drive hidden gone 60; then ok "a hidden command still routes on its exit code"
else bad "a hidden command still routes on its exit code (at \`$(stage_of hidden)\`)"; fi
if [ "$(typed_lines)" -eq "$BEFORE" ]; then
  ok "by default a command step opens no pane"
else
  bad "by default a command step opens no pane: $(cat "$HSTATE/typed.index")"
fi

must "commands shown" "$SPOOLWAY" config set dispatch.hide_commands false
rm -f "$GATE_RELEASE"
BEFORE=$(typed_lines)
task_doc "$LIVE/shown.md" shown "$BODY" "group: shown" "pipeline: gated"
must "a task queued with commands shown" "$SPOOLWAY" queue add --from "$LIVE/shown.md"
records "a shown command's output is in the same log" "gate-marker" \
  "$SPOOLWAY_PROJECT_HOME/commands/shown · gate.log" shown
if [ "$(typed_lines)" -gt "$BEFORE" ]; then
  ok "with hide_commands = false a command step opens a pane"
else
  bad "with hide_commands = false a command step opens a pane"
fi
touch "$GATE_RELEASE"
if drive shown gone 60; then ok "a shown command still routes on its exit code"
else bad "a shown command still routes on its exit code (at \`$(stage_of shown)\`)"; fi
# The workspace's own first pane is not the command's, and stands: a command
# step leaves it empty, which is not what this checks.
if ! awk -F'\t' '$4 == "gate" {found = 1} END {exit !found}' "$HSTATE/panes"; then
  ok "and its pane is closed once the task is done"
else
  bad "and its pane is closed once the task is done: $(cat "$HSTATE/panes")"
fi
must "commands hidden again" "$SPOOLWAY" config set dispatch.hide_commands true
unset GATE_RELEASE

"$HERDRBIN/herdr" shutdown state >/dev/null 2>&1 || true
unset HERDR_STUB_STATE
PATH="$PATH_BEFORE_HERDR_STUB"; export PATH
rm -f .spoolway/pipelines/twostep.yml .spoolway/pipelines/fourstep.yml \
  .spoolway/pipelines/gated.yml

finish
