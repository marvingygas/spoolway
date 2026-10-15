#!/usr/bin/env bash
# A lane starts in herdr as `<step>-<tag>`, and herdr's rules on a name hold.
#
# Two tasks whose ids share the first three hex characters of their hash are
# driven through the same step on `herdr-stub.sh`, which refuses a name over 32
# characters and a name a live agent holds, as herdr does. The first task's
# lane takes `first-<tag>`. The second task's lane would have the same name,
# so it takes a four-character tag. When the first task's second step fails it
# back to the first, the first step runs again and keeps its three-character
# tag. If another task's agent holds the name, that agent keeps it and the
# step's tag grows to five characters. When the first task is done and
# archived, its records leave `lane_aliases.json` and the second task's stay.
#
# Against the double rather than a real server — its header says why there is
# no isolated herdr to run this on. An agent there is a registration, not a
# process, so the suite stands in for each lane's agent and reports for it.
#
# The two task ids were found by hashing: FNV-1a over the id's bytes, as
# `lane_alias::task_hash` does, gives `c9e99…` for both.
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
configure_project plan/names
publish plan/names

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

MODEL=$(awk '/id: implement/ {found=1} found && /model:/ {print $2; exit}' \
  .spoolway/pipelines/default.yml)
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
works "the pipeline checks out" "$SPOOLWAY" pipeline check

report_as() { # task step [--pass|--fail]
  SPOOLWAY_TASK=$1 SPOOLWAY_STEP=$2 "$SPOOLWAY" report "${3:---pass}" -m "done with $2"
}

lane_listed() { "$SPOOLWAY" lane 2>&1 | grep -qF "$1"; }

# The names of the agents live in the double, in the order they started, with
# the one a cleared name leaves behind left out.
agent_names() { awk -F'\t' '$1 != "-" {print $1}' "$HSTATE/agents" | paste -sd' ' -; }
agent_named() { agent_names | tr ' ' '\n' | grep -qxF "$1"; }
# How many agents the double lists, named or not.
agent_count() { wc -l < "$HSTATE/agents" | tr -d ' '; }
# The alias a lane has on record, if any.
recorded_alias() { # lane
  grep -F -B1 "\"lane\": \"$1\"" "$SPOOLWAY_PROJECT_HOME/lane_aliases.json" 2>/dev/null \
    | sed -n 's/.*"alias": "\([^"]*\)".*/\1/p'
}

# `names-a0` and `names-a1` share the tag `c9e`, and the four characters `c9e9`.
FIRST=names-a0
SECOND=names-a1
TAG=c9e

task_doc "$LIVE/first.md" "$FIRST" "$BODY" "group: names-a0" "pipeline: loop"
task_doc "$LIVE/second.md" "$SECOND" "$BODY" "group: names-a1" "pipeline: loop"
must "the first task is queued" "$SPOOLWAY" queue add --from "$LIVE/first.md"

dispatcher_restart

# ------------------------------------------------------ the first task
if drive "$FIRST" first 60 && poll_until 60 lane_listed "$FIRST · first"; then
  ok "the first task's first lane starts"
else bad "the first task's first lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi
if poll_until 30 agent_named "first-$TAG"; then
  ok "its agent is named for the step and the task's three-character tag"
else bad "its agent is named first-$TAG: $(agent_names)"; fi
if [ "$(recorded_alias "$FIRST · first")" = "first-$TAG" ]; then
  ok "and the name is on record for the lane"
else bad "and the name is on record for the lane: $(recorded_alias "$FIRST · first")"; fi

# ------------------------------------------------------ a clash
must "the second task is queued" "$SPOOLWAY" queue add --from "$LIVE/second.md"
if drive "$SECOND" first 60 && poll_until 60 lane_listed "$SECOND · first"; then
  ok "the second task's first lane starts, though the first task holds its name"
else bad "the second task's first lane starts"; "$SPOOLWAY" lane 2>&1 | sed 's/^/        /'; fi
if poll_until 30 agent_named "first-${TAG}9"; then
  ok "its agent takes a four-character tag"
else bad "its agent takes a four-character tag: $(agent_names)"; fi
if agent_named "first-$TAG"; then
  ok "and the first task's agent keeps its three-character one"
else bad "and the first task's agent keeps its name: $(agent_names)"; fi

# ------------------------------------------------------ a step that runs again
must "the first task's first step reports" report_as "$FIRST" first
if drive "$FIRST" second 60 && poll_until 60 lane_listed "$FIRST · second"; then
  ok "the first task's second lane starts"
else bad "the first task's second lane starts"; fi
if poll_until 30 agent_named "second-$TAG"; then
  ok "named for its own step and the same tag"
else bad "named second-$TAG: $(agent_names)"; fi

pane_of_agent() { awk -F'\t' -v n="$1" '$1 == n {print $2; exit}' "$HSTATE/agents"; }
rows_on() { awk -F'\t' -v p="$1" -v n="$2" '$2 == p && $1 == n' "$HSTATE/agents" | wc -l | tr -d ' '; }
named_once() { [ "$(agent_names | tr ' ' '\n' | grep -cxF "first-$TAG")" -eq 1 ]; }
HOLDER=$(pane_of_agent "first-${TAG}9")

# The dispatcher closes the old pane, and with it the old agent, before the step
# runs again, so the name is free and the step takes it back with its
# three-character tag.
must "its second step fails" report_as "$FIRST" second --fail
if drive "$FIRST" first 60; then ok "the task goes back to the first step"
else bad "the task goes back to the first step"; fi
if poll_until 60 named_once; then
  ok "the step that runs again keeps its three-character name, held by one agent"
else bad "the step that runs again keeps its name: $(cat "$HSTATE/agents")"; fi
if agent_named "first-${TAG}9"; then
  ok "and the other task's four-character name is untouched"
else bad "and the other task's name is untouched: $(agent_names)"; fi
if [ "$(recorded_alias "$FIRST · first")" = "first-$TAG" ]; then
  ok "its record names the new agent's name"
else bad "its record names the new agent's name: $(recorded_alias "$FIRST · first")"; fi

# Another task's agent holds the name, on a pane the record does not name. Herdr
# does not promise that the old pane is closed first, so the double is set up
# as if a layout kept it: the first task's own agent is made nameless, and an
# agent on the other task's pane holds the name instead. That agent is not the
# step's own, so it is not stripped: the name is taken, and the tag grows to
# five characters.
put_holder() { # pane: the name moves from the first task's agent to a new one on `pane`
  awk -F'\t' -v OFS='\t' -v n="first-$TAG" '$1 == n {$1 = "-"} {print}' "$HSTATE/agents" \
    >"$HSTATE/agents.tmp"
  mv "$HSTATE/agents.tmp" "$HSTATE/agents"
  printf '%s\t%s\t%s\n' "first-$TAG" "$1" pi >>"$HSTATE/agents"
}
must "the first step reports again" report_as "$FIRST" first
if drive "$FIRST" second 60 && poll_until 60 lane_listed "$FIRST · second"; then
  ok "the first task reaches the second step again"
else bad "the first task reaches the second step again"; fi
put_holder "$HOLDER"
must "its second step fails again" report_as "$FIRST" second --fail
if drive "$FIRST" first 60; then ok "the task goes back to the first step again"
else bad "the task goes back to the first step again"; fi
if poll_until 60 agent_named "first-${TAG}99"; then
  ok "the name is taken by another task's agent, so the tag grows to five characters"
else bad "the tag grows to five characters: $(cat "$HSTATE/agents")"; fi
if [ "$(rows_on "$HOLDER" "first-$TAG")" -eq 1 ]; then
  ok "and the other task's agent keeps the name it held"
else bad "and the other task's agent keeps its name: $(cat "$HSTATE/agents")"; fi
if [ "$(recorded_alias "$FIRST · first")" = "first-${TAG}99" ]; then
  ok "its record names the five-character name"
else bad "its record names the five-character name: $(recorded_alias "$FIRST · first")"; fi

# ------------------------------------------------------ archiving
must "the first step reports a third time" report_as "$FIRST" first
if drive "$FIRST" second 60; then ok "the task reaches the second step a third time"
else bad "the task reaches the second step a third time"; fi
must "the second step passes" report_as "$FIRST" second
if drive "$FIRST" gone 60; then ok "the first task is done and archived"
else bad "the first task is done and archived"; fi
if [ -z "$(recorded_alias "$FIRST · first")" ] && [ -z "$(recorded_alias "$FIRST · second")" ]; then
  ok "its records have left lane_aliases.json"
else bad "its records have left lane_aliases.json: $(cat "$SPOOLWAY_PROJECT_HOME/lane_aliases.json")"; fi
if [ -n "$(recorded_alias "$SECOND · first")" ]; then
  ok "and the other task's record stays"
else bad "and the other task's record stays"; fi

must "the second task's first step reports" report_as "$SECOND" first
must "the second task's second step passes" report_as "$SECOND" second
drive "$SECOND" gone 60 || true

PATH="$PATH_BEFORE_HERDR_STUB"; export PATH
rm -f .spoolway/pipelines/loop.yml

finish
