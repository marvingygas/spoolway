#!/usr/bin/env bash
# With `dispatch.lane_layout` set to `tabs`, each step runs in a tab of its own.
#
# A four-step task is driven through `herdr-stub.sh` with a limit of 3. Each
# step's start adds a tab labelled with the step, the first step takes the
# workspace's own first tab, and the pane in it is not renamed. The fourth
# step's start closes the first step's tab, the oldest, and leaves the three
# newest. A second task fails its second step back to the first: the first
# step runs again in its own tab, so no tab is added. A third task runs past
# the close of its first tab and then fails its last step back to the one
# before: both keep working in tabs of their own, in the workspace they began
# in. A fourth task loses its lane's agent from herdr's list and is found out.
# A task with a limit of 1 keeps one tab, and its workspace survives every
# swap, including the ones after its first tab is gone.
#
# Against the double rather than a real server — its header says why there is
# no isolated herdr to run this on. An agent there is a registration, not a
# process, so the suite stands in for each lane's agent and reports for it.
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
configure_project plan/tabs
publish plan/tabs

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
# Nothing here reminds anybody: the suite reports for each lane in its own time.
must "the lane patience" "$SPOOLWAY" config set dispatch.lane_quiet 1h
must "a tab to a step" "$SPOOLWAY" config set dispatch.lane_layout tabs
says "the setting reads back" "tabs" "$SPOOLWAY" config get dispatch.lane_layout

MODEL=$(awk '/id: implement/ {found=1} found && /model:/ {print $2; exit}' \
  .spoolway/pipelines/default.yml)
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
cat > .spoolway/pipelines/loop.yml <<YML
description: Two agent steps, the second of which can send the task back to the first.

steps:
  - id: first
    description: Runs again when the second fails.
    agent: pi
    prompt: builder
    model: $MODEL
    on_pass: second

  - id: second
    description: Sends the task back to the first.
    agent: pi
    prompt: judge
    model: $MODEL
    on_pass: done
    on_fail: first
    loop: 3
YML
cat > .spoolway/pipelines/rework.yml <<YML
description: Four agent steps, the last of which can send the task back to the third.

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
    description: Runs again when the fourth fails.
    agent: pi
    prompt: builder
    model: $MODEL
    on_pass: fourth

  - id: fourth
    description: Sends the task back to the third.
    agent: pi
    prompt: judge
    model: $MODEL
    on_pass: done
    on_fail: third
    loop: 3
YML
works "the pipelines check out" "$SPOOLWAY" pipeline check

report_as() { # task step [--pass|--fail]
  SPOOLWAY_TASK=$1 SPOOLWAY_STEP=$2 "$SPOOLWAY" report "${3:---pass}" -m "done with $2"
}

lane_listed() { "$SPOOLWAY" lane 2>&1 | grep -qF "$1"; }

# The labels of a task's tabs, in the order they opened.
tabs_of() { # task
  awk -F'\t' -v ws="$(workspace_of "$1")" '$2 == ws {print $3}' "$HSTATE/tabs" | paste -sd' ' -
}
tabs_are() { [ "$(tabs_of "$1")" = "$2" ]; }
tab_count() { tabs_of "$1" | wc -w | tr -d ' '; }

# The label of the pane the task's workspace opened with.
first_pane_label() { # task
  awk -F'\t' -v ws="$(workspace_of "$1")" '$3 == ws {print $4; exit}' "$HSTATE/panes"
}

# Run a task through `steps`, leaving the last one's lane running.
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
must "a limit of 3" "$SPOOLWAY" config set dispatch.visible_lanes 3
task_doc "$LIVE/tabs.md" tabs "$BODY" "group: tabs" "pipeline: fourstep"
must "a task queued on the four-step pipeline" "$SPOOLWAY" queue add --from "$LIVE/tabs.md"

dispatcher_restart

if drive tabs first 60 && poll_until 60 lane_listed "tabs · first"; then
  ok "the first step's lane starts"
else bad "the first step's lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi
if poll_until 30 tabs_are tabs "first"; then
  ok "the workspace's own tab takes the first step's name"
else bad "the workspace's own tab takes the first step's name: $(tabs_of tabs)"; fi
if [ "$(first_pane_label tabs)" = "spoolway/tabs" ]; then
  ok "the pane in it is not renamed"
else bad "the pane in it is not renamed: $(first_pane_label tabs)"; fi

must "the first step reports" report_as tabs first
if drive tabs second 60 && poll_until 60 lane_listed "tabs · second"; then
  ok "the second step's lane starts"
else bad "the second step's lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi
if poll_until 30 tabs_are tabs "first second"; then
  ok "the second step opens a tab of its own, after the first"
else bad "the second step opens a tab of its own: $(tabs_of tabs)"; fi

must "the second step reports" report_as tabs second
if drive tabs third 60 && poll_until 60 lane_listed "tabs · third"; then
  ok "the third step's lane starts"
else bad "the third step's lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi
if poll_until 30 tabs_are tabs "first second third"; then
  ok "three steps, three tabs"
else bad "three steps, three tabs: $(tabs_of tabs)"; fi

must "the third step reports" report_as tabs third
if drive tabs fourth 60 && poll_until 60 lane_listed "tabs · fourth"; then
  ok "the fourth step's lane starts"
else bad "the fourth step's lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi
if poll_until 30 tabs_are tabs "second third fourth"; then
  ok "the oldest step's tab is closed, and the three newest are left"
else bad "the oldest step's tab is closed: $(tabs_of tabs)"; fi
if poll_while 60 lane_listed "tabs · first"; then
  ok "the closed tab's lane is gone"
else bad "the closed tab's lane is gone"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi

WORKSPACE=$(workspace_of tabs)
must "the fourth step reports" report_as tabs fourth
if drive tabs gone 60; then ok "the task is done and archived"
else bad "the task is done and archived"; fi
if ! awk -F'\t' -v ws="$WORKSPACE" '$3 == ws {found = 1} END {exit !found}' "$HSTATE/panes"; then
  ok "and none of its panes are left"
else bad "and none of its panes are left: $(cat "$HSTATE/panes")"; fi

# The panes `$HSTATE/panes` lists in a tab, one id a line.
panes_in() { awk -F'\t' -v t="$1" '$2 == t {print $1}' "$HSTATE/panes"; }
# Whether `$1` holds exactly the one pane, and it is not `$2`: a step that came
# back has put its new pane in the tab and the old one is gone. Re-reads the
# panes on every call, so `poll_until` sees the change, which a command
# substitution in its arguments would not.
tab_holds_only_a_new_pane() {
  [ "$(panes_in "$1" | wc -l | tr -d ' ')" -eq 1 ] && [ "$(panes_in "$1")" != "$2" ]
}

# ------------------------------------------------------ a step that comes back
# The second step fails the task back to the first. The first runs again in the
# tab it had, so no tab is added and the order does not change.
must "a limit of 3 for the looping task" "$SPOOLWAY" config set dispatch.visible_lanes 3
task_doc "$LIVE/again.md" again "$BODY" "group: again" "pipeline: loop"
must "a task queued on the looping pipeline" "$SPOOLWAY" queue add --from "$LIVE/again.md"

if drive again first 60 && poll_until 60 lane_listed "again · first"; then
  ok "the looping task's first lane starts"
else bad "the looping task's first lane starts"; fi
must "its first step reports" report_as again first
if drive again second 60 && poll_until 60 lane_listed "again · second"; then
  ok "its second lane starts"
else bad "its second lane starts"; fi
poll_until 30 tabs_are again "first second" || true
FIRST_TAB=$(awk -F'\t' -v ws="$(workspace_of again)" '$2 == ws && $3 == "first" {print $1}' "$HSTATE/tabs")
# The first lane's pane stays open once its step is over, until the step
# comes back and its new pane replaces it.
FIRST_OLD_PANE=$(panes_in "$FIRST_TAB")

must "its second step fails" report_as again second --fail
if drive again first 60; then ok "the task goes back to the first step"
else bad "the task goes back to the first step"; fi
# The checks below wait for the new pane first: `report --fail` moves the task
# at once, before the dispatcher has placed the returning lane, and the tabs
# and the one old pane already read as they will afterwards.
if [ -n "$FIRST_TAB" ] && [ -n "$FIRST_OLD_PANE" ] \
  && poll_until 60 tab_holds_only_a_new_pane "$FIRST_TAB" "$FIRST_OLD_PANE"; then
  ok "its new pane replaced the old one inside the same tab"
else
  bad "its new pane replaced the old one inside the same tab ($FIRST_TAB, was $FIRST_OLD_PANE)"
  sed 's/^/        /' "$HSTATE/panes"
fi
if tabs_are again "first second"; then
  ok "the step that came back adds no tab"
else bad "the step that came back adds no tab: $(tabs_of again)"; fi

must "the first step reports again" report_as again first
if drive again second 60; then ok "the task reaches the second step again"
else bad "the task reaches the second step again"; fi
must "the second step passes" report_as again second
if drive again gone 60; then ok "the looping task is done and archived"
else bad "the looping task is done and archived"; fi

# ------------------------------------------------------ past the first tab's close
# The fourth step's start closes the first tab, which the task file still
# names. The task must not read that as a lost placement: the fourth fails
# back to the third, and the third runs again in the tab it had, in the
# workspace the task began in.
must "a limit of 3 for the reworked task" "$SPOOLWAY" config set dispatch.visible_lanes 3
task_doc "$LIVE/rework.md" rework "$BODY" "group: rework" "pipeline: rework"
must "a task queued on the rework pipeline" "$SPOOLWAY" queue add --from "$LIVE/rework.md"
start_steps rework first second third fourth
REWORK_WS=$(workspace_of rework)
if poll_until 30 tabs_are rework "second third fourth"; then
  ok "the fourth step closed the first tab and kept its own"
else bad "the fourth step closed the first tab and kept its own: $(tabs_of rework)"; fi
THIRD_TAB=$(awk -F'\t' -v ws="$REWORK_WS" '$2 == ws && $3 == "third" {print $1}' "$HSTATE/tabs")
THIRD_OLD_PANE=$(panes_in "$THIRD_TAB")

must "its fourth step fails" report_as rework fourth --fail
if drive rework third 60; then
  ok "the task goes back to the third step, past the first tab's close"
else bad "the task goes back to the third step, past the first tab's close"; fi
# As above, the checks wait for the new pane: they hold of the state before it
# too, and then prove nothing about where the returning step was placed.
if [ -n "$THIRD_TAB" ] && [ -n "$THIRD_OLD_PANE" ] \
  && poll_until 60 tab_holds_only_a_new_pane "$THIRD_TAB" "$THIRD_OLD_PANE"; then
  ok "its new pane replaced the old one inside the third step's tab"
else
  bad "its new pane replaced the old one inside the third step's tab ($THIRD_TAB, was $THIRD_OLD_PANE)"
  sed 's/^/        /' "$HSTATE/panes"
fi
if [ "$(workspace_of rework)" = "$REWORK_WS" ]; then
  ok "and stays in the workspace it began in"
else bad "and stays in the workspace it began in: $(workspace_of rework) not $REWORK_WS"; fi
if tabs_are rework "second third fourth"; then
  ok "the step that came back adds no tab, and none is renamed for the task"
else bad "the step that came back adds no tab: $(tabs_of rework)"; fi
must "the third step reports again" report_as rework third
if drive rework fourth 60 && poll_until 60 lane_listed "rework · fourth"; then
  ok "the fourth step runs again in the same workspace"
else bad "the fourth step runs again in the same workspace"; fi
must "the fourth step passes" report_as rework fourth
if drive rework gone 60; then ok "the reworked task is done and archived"
else bad "the reworked task is done and archived"; fi

# ------------------------------------------------------ a lane that dies
# The dispatcher asks herdr for the agent list on every pass, and for the
# workspace, pane and tab lists only when a lane starts or ends or a minute has
# gone by. A lane whose agent disappears from the list is therefore found on the
# next pass, which looks at everything, and the task is blocked.
must "a limit of 5 for the dying task" "$SPOOLWAY" config set dispatch.visible_lanes 5
task_doc "$LIVE/dies.md" dies "$BODY" "group: dies" "pipeline: fourstep"
must "a task queued to lose its lane" "$SPOOLWAY" queue add --from "$LIVE/dies.md"
if drive dies first 60 && poll_until 60 lane_listed "dies · first"; then
  ok "the dying task's lane starts"
else bad "the dying task's lane starts"; fi
poll_until 30 tabs_are dies "first" || true

# The agent rows of the task's workspace, one pane id a line.
agent_panes_of() { # task
  local ws; ws=$(workspace_of "$1")
  awk -F'\t' -v ws="$ws" 'NR == FNR {if ($3 == ws) mine[$1] = 1; next} $2 in mine {print $2}' \
    "$HSTATE/panes" "$HSTATE/agents"
}
DYING_PANE=$(agent_panes_of dies | head -n 1)
# What a lane that exited looks like to herdr: the pane stands, the agent does not.
awk -F'\t' -v p="$DYING_PANE" '$2 != p' "$HSTATE/agents" > "$HSTATE/agents.tmp" \
  && mv "$HSTATE/agents.tmp" "$HSTATE/agents"
is_blocked() { [ "$(stage_of dies)" = blocked ]; }
if [ -n "$DYING_PANE" ] && poll_until 60 is_blocked; then
  ok "a lane whose agent vanished is found on a later pass, and its task blocked"
else
  bad "a lane whose agent vanished is found on a later pass, and its task blocked (was $DYING_PANE, stage $(stage_of dies))"
  sed 's/^/        /' "$HSTATE/agents"
fi
forget dies

# ------------------------------------------------------ with a limit of 1
# One tab at a time. The new tab opens before the old one closes, so the
# workspace is never left with none, which would close it with the task still
# running.
must "a limit of 1" "$SPOOLWAY" config set dispatch.visible_lanes 1
task_doc "$LIVE/one.md" one "$BODY" "group: one" "pipeline: fourstep"
must "a task queued with a limit of 1" "$SPOOLWAY" queue add --from "$LIVE/one.md"
start_steps one first second third fourth
ONE_WS=$(workspace_of one)
if poll_until 30 tabs_are one "fourth"; then
  ok "with a limit of 1, only the newest step's tab is left, as the first tab is gone"
else bad "with a limit of 1, only the newest step's tab is left: $(tabs_of one)"; fi
if [ -n "$ONE_WS" ] && grep -q "^$ONE_WS	" "$HSTATE/workspaces"; then
  ok "and its workspace is still open"
else bad "and its workspace is still open"; fi
must "the fourth step reports" report_as one fourth
if drive one gone 60; then ok "the task with a limit of 1 is done and archived"
else bad "the task with a limit of 1 is done and archived"; fi
must "the limit back at its default" "$SPOOLWAY" config set dispatch.visible_lanes 5

PATH="$PATH_BEFORE_HERDR_STUB"; export PATH
rm -f .spoolway/pipelines/fourstep.yml .spoolway/pipelines/loop.yml \
  .spoolway/pipelines/rework.yml

finish
