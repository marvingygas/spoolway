#!/usr/bin/env bash
# Queue a plan's tasks into the project scripts/e2e/scaffold.sh just built.
#
# The `spoolway queue` screen is what queues a plan in real life, and it asks a
# person to pick which plan, which cards, and any gates on the way. A plan
# under scripts/e2e/plans/ has already answered all of that: it is named on
# the command line, every task in it carries `"pipeline"`, and starting the
# dispatcher is the next line of scaffold.sh's own closing instructions. So
# this is the screen with the questions taken out, and nothing else — the
# same `spoolway queue add` calls, in the same dependency order, with the
# body written from the same skeleton.
#
# Why it exists at all: a plan run that needs an interactive agent session to
# start cannot be run twice the same way, and the whole value of a plan is
# watching the same run again after a change. This is the half that is
# mechanical.
#
#   cd <scaffolded project>
#   scripts/e2e/queue-plan.sh <path to plan.html>
#   scripts/e2e/queue-plan.sh --dry-run <plan>    print the calls, queue nothing
#   scripts/e2e/queue-plan.sh --pass 2 <plan>     queue a later pass's tasks
#                                                 (a task carrying `"pass": 2`
#                                                 is left out of the default
#                                                 pass — its card says what to
#                                                 set up in between)
#
# Run it from the plan branch of the scaffolded project: a task is based on the
# branch of the worktree it is queued in.
set -euo pipefail

DRY=0
PLAN=""
WANT_PASS=1
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1; shift ;;
    --pass) WANT_PASS=$2; shift 2 ;;
    -h|--help) sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown flag: $1" >&2; exit 2 ;;
    *) PLAN=$1; shift ;;
  esac
done

[ -n "$PLAN" ] || { echo "usage: queue-plan.sh [--dry-run] <plan file>" >&2; exit 2; }
[ -f "$PLAN" ] || { echo "no such plan: $PLAN" >&2; exit 2; }
command -v jq >/dev/null || { echo "queue-plan.sh needs jq" >&2; exit 2; }

SPOOLWAY=${SPOOLWAY:-spoolway}

# The plan *is* the JSON block; the page around it is for the person who already
# read it. Same block the skill reads, found the same way.
json=$(sed -n '/<script type="application\/json" id="spoolway-plan">/,/<\/script>/p' "$PLAN" \
       | sed -e '1d' -e '$d')
[ -n "$json" ] || { echo "$PLAN carries no spoolway-plan JSON block" >&2; exit 1; }

# An open question is the person deciding not to answer something yet, and the
# JSON cannot tell you — so the page is checked, exactly as the skill checks it.
if grep -q 'id="open"' "$PLAN"; then
  echo "$PLAN still has open questions — settle them before queueing" >&2
  exit 1
fi

count=$(jq '.tasks | length' <<<"$json")
group=$(jq -r '.plan // empty' <<<"$json")
selected=$(jq -r --arg p "$WANT_PASS" \
  '[.tasks[] | select(((.pass // 1) | tostring) == $p)] | length' <<<"$json")
echo "queueing $selected of $count task(s) from $(basename "$PLAN") (pass $WANT_PASS)"

# One document per task, composed here and handed to a single `queue add
# --from -` as a `---`-separated stream: `queue add` takes whole documents
# now, not flags, and one call validates the set together — which is what
# lets `observer.html`'s `reads-doc` name `writes-doc` in `depends_on:`
# while both are still in the same breath. The front matter keys are the
# document's to set
# (`id`, `pipeline`, `group`, `source`, `touches`, `depends_on`,
# `parallel`); `group:` is the plan's own name, the key that groups the
# board and drains the chain as one, and `source:` carries the page's path
# for a person to follow back.
stream=""
for i in $(seq 0 $((count - 1))); do
  task=$(jq -c ".tasks[$i]" <<<"$json")
  id=$(jq -r '.id' <<<"$task")
  pipeline=$(jq -r '.pipeline // empty' <<<"$task")
  [ -n "$pipeline" ] || { echo "task $id names no pipeline" >&2; exit 1; }

  # A task marked `"pass": 2` belongs to a later dispatcher pass — queued on
  # its own, usually after a config change the plan's own card spells out
  # (`escalation.html`'s `staffed` flips `unattended.enabled` between
  # passes). The prose always said so; queueing it here anyway put it in
  # front of the first pass's own lanes, where the config it assumes is not
  # set and the scenario it exists for cannot happen.
  pass=$(jq -r '.pass // 1' <<<"$task")
  if [ "$pass" != "$WANT_PASS" ]; then
    case "$pass" in
      1) echo "leaving $id — it belongs to pass 1, and this call asked for pass $WANT_PASS" ;;
      *) echo "leaving $id for pass $pass — \`queue-plan.sh --pass $pass\` queues it when this pass is done (its card says what to set first)" ;;
    esac
    continue
  fi

  # The body, written from .spoolway/templates/tasks/<pipeline>.md's shape. The
  # skeleton's headings rather than the skeleton's file: what a lane needs is
  # the shape its prompts were written to expect, and every slot in the file
  # itself is filled from the plan here.
  body=$(jq -r '
    def lines(xs): (xs // []) | map("- " + .) | join("\n");
    "## Context\n\n"          + lines(.context) + "\n\n" +
    "## Goal\n\n"             + .goal + "\n\n" +
    "## Non-goals\n\n"        + lines(.non_goals) + "\n\n" +
    "## Acceptance criteria\n\n" + lines(.criteria) + "\n\n" +
    "## End-to-end coverage\n\n" + lines(.e2e) + "\n\n" +
    "## References\n\n"       + lines(.references) + "\n"
  ' <<<"$task")

  # The front matter, from the same JSON. yaml strings are quoted through
  # jq's own @json, so an id or a glob with a quote in it survives.
  front=$(jq -r --arg source "$PLAN" --arg group "$group" '
    def list(name; xs): if (xs // []) | length > 0
      then name + ":\n" + ((xs) | map("- " + (. | @json)) | join("\n")) + "\n"
      else "" end;
    "id: " + (.id | @json) + "\n"
    + "title: " + ((.title // (.goal | split(": ")[0] | split(". ")[0])) | @json) + "\n"
    + "pipeline: " + (.pipeline | @json) + "\n"
    + (if $group != "" then "group: " + ($group | @json) + "\n" else "" end)
    + "source: " + ($source | @json) + "\n"
    + list("touches"; .touches)
    + list("depends_on"; .depends_on)
    + (if .parallel == true then "parallel: true\n" else "" end)
  ' <<<"$task")

  # `$( )` stripped `front`'s own trailing newline, so the fence adds one.
  stream+=$(printf -- '---\n%s\n---\n%s\n' "$front" "$body")
  stream+=$'\n'
done

if [ "$DRY" = 1 ]; then
  printf '%s queue add --from - <<DOCS\n%s\nDOCS\n' "$SPOOLWAY" "$stream"
  exit 0
fi
printf '%s' "$stream" | "$SPOOLWAY" queue add --from -

# Step 6 and 7 of the skill: a plan whose tasks are not a chain stacks wrong,
# and a plan under plans/ is a chain by construction — so this is a check that
# the transcription above kept it one, not a judgement about the plan.
echo
"$SPOOLWAY" queue conflicts || true
"$SPOOLWAY" queue list
