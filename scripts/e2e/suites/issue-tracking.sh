#!/usr/bin/env bash
# Issue tracking: `[issue_tracking]`'s hook, fired on the stages a pipeline
# file can never put a `run:` step on itself.
#
# `queued`, `blocked`, `paused` and `done` are reserved stage names, and a
# fifth event, `open`, fires once per document before it is even queued — the
# two ids it answers with land in `epic:`/`ticket:` on the document itself.
# Every case here reuses the same detached-process machinery a command step
# runs through (`commands::run_hook` shares `commands::spawn` with a `run:`
# step), which is why it belongs in an e2e suite and not a unit test.
#
# Split out of `commands.sh` (gh-253): that file grew to 282 checks across
# every domain a command step touches, and this is the one domain of the
# three it became — everything gated on `[issue_tracking]`. `command-steps.sh`
# holds the plain `run:`/`background:`/`timeout:`/`loop:` mechanics, and
# `commands.sh` keeps what is left: scaffolding, the queue screen, the
# archive, config and the overrides layer. Every `# covers:` claim that was
# here stayed here; nothing moved to either sibling.
#
# The shipped `github.sh` hook is exercised for real here, against
# `gh-stub.sh` rather than a real GitHub — see that double's own header for
# why it is shared with `suites/stack.sh`.
#
# covers: issue_tracking.hook — a bare filename, resolved inside .spoolway/hooks/, fires once per task per event with the full environment set
# covers: issue_tracking.project_key — opaque, handed to the hook verbatim as SPOOLWAY_PROJECT_KEY
# covers: a non-zero hook exit on `queued` or `done` (and on `started`, unit-tested in `src/dispatch.rs`) pauses the task, naming the hook's own log under tracking/; on `blocked` or `paused` it only records the failure; `spoolway resume` forgets the failed run so the hook fires again, sending a `done` pause back to `done` and a `queued` pause back to `queued`
# covers: issue_tracking.key_in_names — with it on and the hook answering slug=, `queue add` writes `group: <slug>-<group>` and `branch: task/<slug>-<id>` and stores the hook's url=
# covers: `started` fires once a task actually leaves `queued` for its entry step, not merely once it is queued — a dependent task's own `started` event only fires once the task it depends on has already reached `done`
# covers: the shipped github.sh's `check` branch passes with gh logged in and the repository visible, and fails on each of the two alone
# covers: `spoolway doctor` runs the hook with SPOOLWAY_EVENT=check, synchronously; a non-zero exit is one FAIL row carrying the hook's own stderr, not the merged stdout+stderr log a detached run leaves under tracking/
# covers: a task's `labels:` reaches every event with a task behind it as SPOOLWAY_LABELS, comma-joined and empty when the task has none; `queue add` refuses a label holding whitespace or a comma, naming the task and the label
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

# `fetch, real` below reads `spoolway issue show`'s JSON with `jq -e`. This
# guard came with `commands.sh` before the split and stayed with the case
# that actually needs it, rather than getting dropped on the way.
command -v jq >/dev/null || { echo "issue-tracking.sh needs jq" >&2; exit 2; }

LIVE=${WORK:-$(mktemp -d)}
CTL="$LIVE/ctl"

new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$CTL" "" "" "" "$FORGE"

new_repo "$LIVE/proj"
configure_project plan/live "$LIVE/worktrees"
publish plan/live

BODY="$LIVE/body.md"
task_body "$BODY"

# --------------------------------------------------------------- labels:
# `queue add` refuses a label holding whitespace before a hook ever sees it
# — checked with no hook configured at all, since the refusal is
# `parse_submission`'s own, not anything the tracker does.
task_doc "$LIVE/spaced-label.md" spaced-label "$BODY" "group: spaced-label-group" \
  'labels: ["has space"]'
refuses "a label holding a space is refused, naming the task" \
  "spaced-label" \
  "$SPOOLWAY" queue add --from "$LIVE/spaced-label.md"
refuses "a label holding a space is refused, naming the label" \
  "has space" \
  "$SPOOLWAY" queue add --from "$LIVE/spaced-label.md"
works "nothing was queued for the refused label" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/spaced-label.md"

# ------------------------------------------------------- issue_tracking hook
# `[issue_tracking]` fires a project's own script once per task on each of the
# four states nothing inside a pipeline file can already put a `run:` step on
# — `queued`, `blocked`, `paused` and `done` are reserved stage names, never
# steps a pipeline may declare. It reuses the same detached-process machinery
# a command step runs through (`commands::run_hook` shares `commands::spawn`
# with a `run:` step, over in `command-steps.sh`), which is why it belongs in
# an e2e suite and not a unit test.
mkdir -p .spoolway/hooks

# Records its own environment beside the task file it ran for, one file per
# event — every event, `fetch` included, though nothing here calls `issue
# show` to exercise it — and always passes: this half is about what reaches
# the hook, not about a failure.
cat > .spoolway/hooks/record.sh <<'EOF'
#!/bin/sh
# Handles every event the same way, queued/started/blocked/paused/done/
# open/fetch alike — there is no branch here to miss. `check` gets one
# real, no-op `case` arm below: `has_check_branch` matches the actual
# shapes a branch is spelled in (`check)`, `= check`, ...), not the bare
# word, so a comment naming the event is not enough any more — and this
# hook is meant to prove `spoolway doctor` really runs `check` against a
# script that has grown the branch. `check` carries no task and so no
# $SPOOLWAY_TASK_FILE — guarded rather than left to write a stray
# "$PWD/.env.check" on every doctor run in this suite's own checkout.
case "$SPOOLWAY_EVENT" in
  check) : ;;
esac
[ -n "$SPOOLWAY_TASK_FILE" ] && env | sort > "$SPOOLWAY_TASK_FILE.env.$SPOOLWAY_EVENT"
# On `started`, also the stage every task still in the queue stands at, as
# the hook saw it: what a dependency had reached when its dependent started
# is read straight off this, rather than off which of two hooks running at
# once happened to write its file first.
if [ "$SPOOLWAY_EVENT" = started ] && [ -n "$SPOOLWAY_TASK_FILE" ]; then
  for f in "$(dirname "$SPOOLWAY_TASK_FILE")"/*.md; do
    [ -e "$f" ] && echo "$(basename "$f" .md) $(sed -n 's/^stage: //p' "$f")"
  done > "$SPOOLWAY_TASK_FILE.stages.started"
fi
exit 0
EOF
chmod +x .spoolway/hooks/record.sh

must "the hook is named" "$SPOOLWAY" config set issue_tracking.hook record.sh
must "and a project key" "$SPOOLWAY" config set issue_tracking.project_key acme/app

# `doctor` reads the same table straight off a real config.toml on a real
# checkout, which `doctor()` itself does not decouple from — it also pulls
# the branch and the task graph — so this is the only place a unit test
# cannot already stand in for it.
silent_about "doctor is quiet about a fully-configured issue_tracking" \
  "issue_tracking" "$SPOOLWAY" doctor

must "project_key is cleared to make the one half-set case" \
  "$SPOOLWAY" config set issue_tracking.project_key ""
says "doctor names both keys and the two ways out" \
  "[issue_tracking] names \`record.sh\` but project_key is empty" \
  "$SPOOLWAY" doctor
must "project_key is restored" \
  "$SPOOLWAY" config set issue_tracking.project_key acme/app

must "hook is set to a path rather than a bare name" \
  "$SPOOLWAY" config set issue_tracking.hook "../record.sh"
says "doctor refuses a hook name that is not a bare filename" \
  "which is not a bare filename" \
  "$SPOOLWAY" doctor
must "hook is restored to the bare name" \
  "$SPOOLWAY" config set issue_tracking.hook record.sh

# `check` proves the hook works before any task can ever pause on it —
# `doctor` runs it synchronously, once, and a non-zero exit is one FAIL row
# carrying the hook's own stderr, not the merged stdout+stderr log a
# detached run under `tracking/` would leave.
cat > .spoolway/hooks/check-fails.sh <<'EOF'
#!/bin/sh
if [ "$SPOOLWAY_EVENT" = check ]; then
  echo 'status "Review" does not exist in project KAN' >&2
  exit 1
fi
exit 0
EOF
chmod +x .spoolway/hooks/check-fails.sh
must "the hook is switched to one whose check branch fails" \
  "$SPOOLWAY" config set issue_tracking.hook check-fails.sh
says "doctor reports the failing check as one FAIL row carrying its own stderr" \
  'FAIL  check-fails.sh check: status "Review" does not exist in project KAN' \
  "$SPOOLWAY" doctor
must "the hook is restored to the recording one" \
  "$SPOOLWAY" config set issue_tracking.hook record.sh

# `handover` is `spoolway stack`, and the second task of a group is the second
# pull request of a stack — one `gh api` call that needs a remote reading as
# `github.com` and a `gh` that tracks stack membership. This suite's forge is
# a bare repo on disk and its `gh` double tracks no stacks at all, so without
# the three lines below `tracked-b` blocks at `handover` and never reaches
# `done`, taking the `SPOOLWAY_GROUP_LAST` assertions with it. Both facts are
# deliberate elsewhere — `suites/stacking.sh` asserts that exact refusal as
# its own subject — so this is scoped to this block and undone after it.
ORIGIN="$FORGE/origin.git"
TRACKED_URL="https://github.com/e2e/spoolway-tracked.git"
must "origin addressed as github, so the second of the pair can stack" \
  git -C "$LIVE/proj" remote set-url origin "$TRACKED_URL"
must "and redirected straight back to the same bare forge" \
  git -C "$LIVE/proj" config "url.$ORIGIN.insteadOf" "$TRACKED_URL"
export SPOOLWAY_GH="$HERE/../gh-stub.sh"
export GH_STUB_PRS="$LIVE/tracked-prs"
export GH_STUB_URL="file://$ORIGIN"
# `handover` (`spoolway stack`) reads $SPOOLWAY_GH for its own `gh` calls —
# the only ones this pipeline makes any more, `checks` having gone with it —
# so there is no bare `gh` left to resolve off PATH, and no second double to
# put ahead of `new_forge`'s own.
# The dispatcher inherited its environment when it started, before any of
# that existed — restarted, so the `handover` it runs sees all three.
dispatcher_restart

# A dependent pair rather than two independent tasks: `tracked-b` cannot even
# start until `tracked-a` has reached `done`, which is what keeps
# `SPOOLWAY_GROUP_LAST` deterministic below — the two can never reach `done`
# in the same pass, so `tracked-a`'s own hook always sees `tracked-b` still
# open.
task_doc "$LIVE/tracked-a.md" tracked-a "$BODY" "group: tracked-pair" \
  "group_description: a dependent pair, tracked end to end" \
  "labels: [gh, tracked]"
task_doc "$LIVE/tracked-b.md" tracked-b "$BODY" "group: tracked-pair" \
  "depends_on: [tracked-a]"
must "the first of a dependent pair queues" "$SPOOLWAY" queue add --from "$LIVE/tracked-a.md"
must "the second, depending on it, queues too" "$SPOOLWAY" queue add --from "$LIVE/tracked-b.md"

if drive tracked-a gone 180 && drive tracked-b gone 180; then
  ok "both tasks of the dependent pair reach done"
else
  bad "both tasks of the dependent pair reach done (at \`$(stage_of tracked-a)\`/\`$(stage_of tracked-b)\`)"
fi

ENV_A_QUEUED="$SPOOLWAY_PROJECT_HOME/queue/tracked-a.md.env.queued"
has "the queued hook ran with the task's own identity" "SPOOLWAY_TASK=tracked-a" "$ENV_A_QUEUED"
has "the event it fired for" "SPOOLWAY_EVENT=queued" "$ENV_A_QUEUED"
has "the project key from config" "SPOOLWAY_PROJECT_KEY=acme/app" "$ENV_A_QUEUED"
has "the task's own group" "SPOOLWAY_GROUP=tracked-pair" "$ENV_A_QUEUED"
has "the task file's own absolute path" "SPOOLWAY_TASK_FILE=" "$ENV_A_QUEUED"
has "its own labels, comma-joined" "SPOOLWAY_LABELS=gh,tracked" "$ENV_A_QUEUED"

ENV_B_QUEUED="$SPOOLWAY_PROJECT_HOME/queue/tracked-b.md.env.queued"
# The whole line, not a prefix: `has` would pass on any value at all, and the
# promise is that a task with no `labels:` hands the hook an empty one.
works "a task with no labels: carries the variable, empty" \
  grep -qx 'SPOOLWAY_LABELS=' "$ENV_B_QUEUED"

ENV_A_DONE="$SPOOLWAY_PROJECT_HOME/queue/tracked-a.md.env.done"
ENV_B_DONE="$SPOOLWAY_PROJECT_HOME/queue/tracked-b.md.env.done"
silent_about "the first of the pair's done event is not the group's last" \
  "SPOOLWAY_GROUP_LAST" cat "$ENV_A_DONE"
has "the second's done event is — it is the group's last open task" \
  "SPOOLWAY_GROUP_LAST=1" "$ENV_B_DONE"

# `started` fires once a task actually leaves `queued` for its entry step,
# not merely once it is queued — the moment `queued` itself already fires
# on. `tracked-b` depends on `tracked-a`, so it cannot leave `queued` until
# `tracked-a` has reached `done`, which is exactly what a `started` event for
# `tracked-b` seeing `tracked-a` at any other stage would contradict.
ENV_A_STARTED="$SPOOLWAY_PROJECT_HOME/queue/tracked-a.md.env.started"
has "the first of the pair's started event fired with its own identity" \
  "SPOOLWAY_TASK=tracked-a" "$ENV_A_STARTED"
has "the event it fired for" "SPOOLWAY_EVENT=started" "$ENV_A_STARTED"
# Read off the stages `record.sh` snapshots on `started`, not off file times.
# `tracked-a` counts as finished once it stands at `done`, so `tracked-b` may
# start in the very pass `tracked-a`'s own `done` hook is still running — the
# two hooks run at once, and which writes its file first is a race. What the
# gate does promise is that `tracked-a` is at `done`, or already archived out
# of the queue, by the time `tracked-b`'s `started` fires.
STAGES_B_STARTED="$SPOOLWAY_PROJECT_HOME/queue/tracked-b.md.stages.started"
has "the second's started event saw itself still queued" \
  "tracked-b queued" "$STAGES_B_STARTED"
if ! grep -q '^tracked-a ' "$STAGES_B_STARTED" 2>/dev/null \
  || grep -qx 'tracked-a done' "$STAGES_B_STARTED"; then
  ok "tracked-b's started event fired only once tracked-a had already reached done"
else
  bad "tracked-b's started event saw tracked-a at \`$(sed -n 's/^tracked-a //p' "$STAGES_B_STARTED")\` ($STAGES_B_STARTED) — the dependency gate did not hold started back"
fi

# Undone: everything past here is meant to see the same not-github forge the
# rest of this suite was written against.
must "origin is the bare forge again" \
  git -C "$LIVE/proj" remote set-url origin "$ORIGIN"
must "and the redirect is dropped with it" \
  git -C "$LIVE/proj" config --unset "url.$ORIGIN.insteadOf"
unset SPOOLWAY_GH
dispatcher_restart

# --------------------------------------------------------------- open hook
# `queue add` calls a fifth event, `open`, once per document before anything
# is queued at all: the two ids it answers with land in `epic:`/`ticket:`,
# right in the document's own frontmatter, in dependency order so a
# dependent's own call already has its parent's ticket id to hand over in
# `SPOOLWAY_DEPENDS_TICKETS`. A document already naming a `ticket:` is
# skipped outright — reported `kept`, never opened twice — which is what
# lets a batch a hook failed partway through resume on the next `queue add`
# rather than open a second set.
cat > .spoolway/hooks/open.sh <<'EOF'
#!/bin/sh
[ "$SPOOLWAY_EVENT" = open ] || exit 0
dir=$(dirname "$SPOOLWAY_OUT")
counter="$dir/open-counter"
next() {
  n=$(cat "$counter" 2>/dev/null || echo 40)
  n=$((n + 1))
  echo "$n" >"$counter"
  echo "$n"
}
epic=$SPOOLWAY_EPIC
if [ -z "$epic" ] && [ "$SPOOLWAY_GROUP_SIZE" -gt 1 ]; then
  epic="acme/app#$(next)"
fi
if [ "$SPOOLWAY_TASK" = "opened-fails" ]; then
  exit 7
fi
ticket="acme/app#$(next)"
{ echo "epic=$epic"; echo "ticket=$ticket"; } >"$SPOOLWAY_OUT"
echo "$SPOOLWAY_DEPENDS_TICKETS" >"$dir/depends.$SPOOLWAY_TASK"
# Proves `SPOOLWAY_TASK_FILE` names a path the hook can actually open right
# now — the document mid-flight, still in `pending/`, not the `queue/`
# destination `queue add` has not written yet (that used to be the bug: the
# document did not exist there until after every hook in the batch had run).
cat "$SPOOLWAY_TASK_FILE" >"$dir/task-file.$SPOOLWAY_TASK"
echo "$SPOOLWAY_GROUP_DESCRIPTION" >"$dir/group-description.$SPOOLWAY_TASK"
EOF
chmod +x .spoolway/hooks/open.sh
must "the hook is switched to one that opens tickets" \
  "$SPOOLWAY" config set issue_tracking.hook open.sh

# A group with a hook configured and no `group_description:` on any of its
# documents is refused outright, naming the group — before the hook is ever
# run, and before anything is queued.
task_doc "$LIVE/undescribed.md" undescribed "$BODY" "group: undescribed-group"
refuses "a group with no group_description is refused once a hook is configured" \
  "group \`undescribed-group\` sets no \`group_description:\`" \
  "$SPOOLWAY" queue add --from "$LIVE/undescribed.md"
works "nothing was queued for the refused group" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/undescribed.md"

task_doc "$LIVE/opened-a.md" opened-a "$BODY" "group: opened-pair" \
  "group_description: a mirrored pair of tasks"
task_doc "$LIVE/opened-b.md" opened-b "$BODY" "group: opened-pair" \
  "depends_on: [opened-a]"
must "a dependent pair queues in one call, opening a ticket for each" \
  "$SPOOLWAY" queue add --from "$LIVE/opened-a.md" --from "$LIVE/opened-b.md"

OPENED_A="$SPOOLWAY_PROJECT_HOME/queue/opened-a.md"
OPENED_B="$SPOOLWAY_PROJECT_HOME/queue/opened-b.md"
has "the group's epic landed in the first task" "epic: acme/app#" "$OPENED_A"
has "the first task's own ticket" "ticket: acme/app#" "$OPENED_A"
has "the same epic landed in the dependent's task" \
  "$(grep '^epic:' "$OPENED_A")" "$OPENED_B"
has "the dependent's own, different ticket" "ticket: acme/app#" "$OPENED_B"
has "the dependent's call carried its parent's ticket id" \
  "$(grep '^ticket:' "$OPENED_A" | awk '{print $2}')" \
  "$SPOOLWAY_PROJECT_HOME/tracking/depends.opened-b"
has "the open hook read the real, live contents of the task" \
  "id: opened-a" "$SPOOLWAY_PROJECT_HOME/tracking/task-file.opened-a"
has "the group's own words reached the hook on the first task" \
  "a mirrored pair of tasks" "$SPOOLWAY_PROJECT_HOME/tracking/group-description.opened-a"
has "and on the dependent, which set no group_description of its own" \
  "a mirrored pair of tasks" "$SPOOLWAY_PROJECT_HOME/tracking/group-description.opened-b"

# A batch of two, the second of which the hook above refuses by name: nothing
# is queued, but the first document's own ticket was already written back
# into it in place, in `$LIVE` — not the queue — so a second `queue add` over
# the same two documents resumes rather than opening a second set.
task_doc "$LIVE/opened-ok.md" opened-ok "$BODY" "group: opened-fail-batch" \
  "group_description: a batch that fails partway through"
task_doc "$LIVE/opened-fails.md" opened-fails "$BODY" "group: opened-fail-batch"
refuses "a mid-batch hook failure queues nothing" "opened-fails" \
  "$SPOOLWAY" queue add --from "$LIVE/opened-ok.md" --from "$LIVE/opened-fails.md"
if [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/opened-ok.md" ] \
  && [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/opened-fails.md" ]; then
  ok "neither task of the failed batch was queued"
else
  bad "neither task of the failed batch was queued"
fi
has "the one that succeeded had its ticket written back into the pending file" \
  "ticket: acme/app#" "$LIVE/opened-ok.md"

# The hook no longer refuses anything: the second run over the same two
# documents queues both, and the first is `kept` rather than opened again —
# proven by its ticket id being unchanged from what the first run secured.
FIRST_TICKET=$(grep '^ticket:' "$LIVE/opened-ok.md" | awk '{print $2}')
sed -i '/SPOOLWAY_TASK" = "opened-fails"/,+2d' .spoolway/hooks/open.sh
must "the second run over the same batch queues both" \
  "$SPOOLWAY" queue add --from "$LIVE/opened-ok.md" --from "$LIVE/opened-fails.md"
has "the resumed run kept the first ticket rather than opening a new one" \
  "ticket: $FIRST_TICKET" "$SPOOLWAY_PROJECT_HOME/queue/opened-ok.md"

# --------------------------------------------- issue_tracking.key_in_names
# With the flag on and the hook answering a `slug=`, `queue add` writes the
# prefixed `group:` and `branch:` into each queued document — proven here
# through the real binary and a real hook. The worktree directory follows the
# branch slug too, but that is a dispatched checkout this block never cuts;
# `src/mux.rs`'s own tests cover the directory name. The hook also answers a
# `url=`, which is stored on the task and validated but shown nowhere yet.
cat > .spoolway/hooks/open-key.sh <<'EOF'
#!/bin/sh
[ "$SPOOLWAY_EVENT" = open ] || exit 0
dir=$(dirname "$SPOOLWAY_OUT")
counter="$dir/open-key-counter"
next() {
  n=$(cat "$counter" 2>/dev/null || echo 90)
  n=$((n + 1)); echo "$n" >"$counter"; echo "$n"
}
epic=$SPOOLWAY_EPIC
if [ -z "$epic" ] && [ "$SPOOLWAY_GROUP_SIZE" -gt 1 ]; then
  epic="acme/app#$(next)"
fi
ticket="acme/app#$(next)"
key=${epic:-$ticket}
num=${key##*#}
{
  echo "epic=$epic"; echo "ticket=$ticket"
  echo "slug=gh-$num"
  echo "url=https://github.com/acme/app/issues/$num"
} >"$SPOOLWAY_OUT"
EOF
chmod +x .spoolway/hooks/open-key.sh
must "the hook is switched to one that also answers a slug and a url" \
  "$SPOOLWAY" config set issue_tracking.hook open-key.sh
must "key_in_names is turned on" \
  "$SPOOLWAY" config set issue_tracking.key_in_names true

task_doc "$LIVE/keyed-a.md" keyed-a "$BODY" "group: keyed-rework" \
  "group_description: reworking the keyed pair"
task_doc "$LIVE/keyed-b.md" keyed-b "$BODY" "group: keyed-rework" \
  "depends_on: [keyed-a]"
says "queue add names the prefix it applied" \
  "names prefixed" \
  "$SPOOLWAY" queue add --from "$LIVE/keyed-a.md" --from "$LIVE/keyed-b.md"

KEYED_A="$SPOOLWAY_PROJECT_HOME/queue/keyed-a.md"
KEYED_B="$SPOOLWAY_PROJECT_HOME/queue/keyed-b.md"
has "the group carries the slug prefix" "group: gh-" "$KEYED_A"
has "the group keeps its original name after the prefix" "keyed-rework" "$KEYED_A"
has "the branch carries the slug prefix and ends in the task id" \
  "branch: task/gh-" "$KEYED_A"
has "the branch ends in the task id" "-keyed-a" "$KEYED_A"
has "the slug is stored on the task" "slug: gh-" "$KEYED_A"
has "the issue url is stored on the task" \
  "url: https://github.com/acme/app/issues/" "$KEYED_A"
has "the dependent of the same group takes the same prefix" \
  "branch: task/gh-" "$KEYED_B"

must "key_in_names is turned back off" \
  "$SPOOLWAY" config set issue_tracking.key_in_names false
must "the hook is restored to the plain one" \
  "$SPOOLWAY" config set issue_tracking.hook open.sh

# ------------------------------------------ the queue tab asks before `open`
# The queue tab's `enter` no longer runs the hook straight away: with
# tracking on, it asks first — `[enter] create and queue`, `[n] queue only`,
# `[esc] back` — over every task in the batch. What only this suite can say
# is that `n` really does keep the hook's process from ever starting: the
# whole binary reading real keys off a pipe, against the real `open.sh` above,
# which writes `tracking/task-file.<task>` on every `open` it is called for.
# `enter` on the same question is the control — the same hook, the same
# marker, written — so a marker missing after `n` is the question working, not
# a hook that never fires. One group in pending at a time, so the cursor opens
# on it.
pending_doc asked-a "$BODY" "group: asked" \
  "group_description: a pair queued through the question"
pending_doc asked-b "$BODY" "group: asked" \
  "depends_on: [asked-a]"
TRACKING="$SPOOLWAY_PROJECT_HOME/tracking"

ASK_ESC="$LIVE/ask-esc.out"
on_screen ' \r\x1b' "$ASK_ESC"; sed -i 's/\x1b\[[0-9;]*m//g' "$ASK_ESC"
has "enter on the queue tab asks before any ticket is opened" \
  "create 2 issues on open for asked" "$ASK_ESC"
has "in a popup over the queue tab" "┌─ issue tracking " "$ASK_ESC"
has "listing every task in the batch" "asked-b" "$ASK_ESC"
has "whose keys read as drawn" \
  "[enter] create and queue   [n] queue only   [esc] back" "$ASK_ESC"
works "esc queues nothing" test ! -e "$SPOOLWAY_PROJECT_HOME/queue/asked-a.md"
works "and leaves the group in pending" test -f "$SPOOLWAY_PROJECT_HOME/pending/asked-a.md"
works "and the hook was never called" \
  bash -c '[ ! -e "$1/task-file.asked-a" ] && [ ! -e "$1/task-file.asked-b" ]' _ "$TRACKING"

ASK_N="$LIVE/ask-n.out"
on_screen ' \rn' "$ASK_N"; sed -i 's/\x1b\[[0-9;]*m//g' "$ASK_N"
works "n on the question queues the group" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/asked-a.md"
works "the whole of it" test -f "$SPOOLWAY_PROJECT_HOME/queue/asked-b.md"
works "with no hook call for either task" \
  bash -c '[ ! -e "$1/task-file.asked-a" ] && [ ! -e "$1/task-file.asked-b" ]' _ "$TRACKING"
lacks "so no ticket landed on the first" "ticket:" "$SPOOLWAY_PROJECT_HOME/queue/asked-a.md"
lacks "nor on the second" "ticket:" "$SPOOLWAY_PROJECT_HOME/queue/asked-b.md"
lacks "and no epic" "epic:" "$SPOOLWAY_PROJECT_HOME/queue/asked-a.md"
has "the result says what was queued" "queued 2 tasks" "$ASK_N"

pending_doc asked-yes "$BODY" "group: asked-yes" \
  "group_description: one task queued through the question's enter"
ASK_YES="$LIVE/ask-yes.out"
on_screen ' \r\r' "$ASK_YES"; sed -i 's/\x1b\[[0-9;]*m//g' "$ASK_YES"
works "enter on the question queues the group" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/asked-yes.md"
works "after calling the hook for it" test -e "$TRACKING/task-file.asked-yes"
has "whose ticket landed on the task" "ticket: acme/app#" \
  "$SPOOLWAY_PROJECT_HOME/queue/asked-yes.md"
has "and the result popup names what was created" "┌─ issues created " "$ASK_YES"

# -------------------------------------------- tracking off: no hook, ever
# `n` above proved the hook never runs at `open`. This is this task's own
# acceptance criterion: `n` also stamps `tracking: off` onto the queued task
# (`commands::queue::open_and_prefix`), and the dispatcher reads that back —
# the same way it already reads a trial arm's `trial:` — to fire no
# `[issue_tracking]` event for it on `queued`, `started` or `done` either,
# all the way through a real run to the archive. Switched to `record.sh` for
# this one case, which marks every event `queued`/`started`/`blocked`/
# `paused`/`done`/`open`/`fetch` alike beside the task file itself;
# `open.sh`, left in place above, is silent on every event but `open` and so
# cannot prove a `queued`, `started` or `done` hook call never happened.
must "the hook is switched to the one that marks every event, for this case" \
  "$SPOOLWAY" config set issue_tracking.hook record.sh
pending_doc declined-tracking "$BODY" "group: declined-tracking" \
  "group_description: a task queued with tracking declined, driven to done"
DECLINED_OUT="$LIVE/declined-tracking.out"
on_screen ' \rn' "$DECLINED_OUT"; sed -i 's/\x1b\[[0-9;]*m//g' "$DECLINED_OUT"
has "the question, answered n, queues the task" "queued 1 task" "$DECLINED_OUT"
has "with tracking: off stamped onto it" "tracking: off" \
  "$SPOOLWAY_PROJECT_HOME/queue/declined-tracking.md"

if drive declined-tracking gone 180; then
  ok "the declined task reaches done with the dispatcher running the whole way"
else
  bad "the declined task reaches done with the dispatcher running the whole way \
(at \`$(stage_of declined-tracking)\`)"
fi

works "no queued event ever ran the hook for it" \
  bash -c '[ ! -e "$1/queue/declined-tracking.md.env.queued" ]' _ "$SPOOLWAY_PROJECT_HOME"
works "no started event ran the hook for it" \
  bash -c '[ ! -e "$1/queue/declined-tracking.md.env.started" ]' _ "$SPOOLWAY_PROJECT_HOME"
works "no done event ran the hook for it either" \
  bash -c '[ ! -e "$1/queue/declined-tracking.md.env.done" ]' _ "$SPOOLWAY_PROJECT_HOME"
works "and no tracking/ bookkeeping file exists for it at all" \
  bash -c '! ls "$1/declined-tracking · "* >/dev/null 2>&1' _ "$TRACKING"
# The control that keeps the three checks above from passing on a hook that
# never ran for anyone: `asked-yes`, queued above through `enter` with its
# ticket opened, sat at `queued` with the dispatcher stopped until the
# `drive` above started one reading `record.sh`. Same dispatcher, same hook,
# tracking on — so its `queued` marker is what the declined task's missing
# one is measured against. `asked-a`, declined through the same `n`, is the
# screen's two-task batch getting the same treatment.
works "the control: a tracking-on task in the same run did fire the queued hook" \
  test -e "$SPOOLWAY_PROJECT_HOME/queue/asked-yes.md.env.queued"
works "while asked-a, declined through the same n, fired none" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/asked-a.md.env.queued"

must "the hook is switched back to the one that only answers open" \
  "$SPOOLWAY" config set issue_tracking.hook open.sh

# ------------------------------------------------- a failing hook pauses
# A hook that always fails, on each of the four events by hand: a non-zero
# exit on `queued` or `done` pauses the task now, naming the hook's own log
# under tracking/ in the reason, and only records the failure on `blocked`
# and `paused` — both already stopped for a person, so nothing about pausing
# them again would mean anything.
#
# `open` is the one event it lets through, and the exemption is the point:
# a failing `open` refuses the whole `queue add` outright — the block right
# above this one is what covers that — so a hook failing there too would
# leave nothing queued to carry the four events this block is about.
cat > .spoolway/hooks/fail.sh <<'EOF'
#!/bin/sh
[ "$SPOOLWAY_EVENT" = open ] && exit 0
exit 1
EOF
chmod +x .spoolway/hooks/fail.sh
must "the hook now always fails" "$SPOOLWAY" config set issue_tracking.hook fail.sh

task_doc "$LIVE/hook-queued.md" hook-queued "$BODY" "group: live" \
  "group_description: a task under an always-failing hook"
must "a task queues under an always-failing hook" \
  "$SPOOLWAY" queue add --from "$LIVE/hook-queued.md"

if drive hook-queued paused 60; then
  ok "a failing queued hook lands the task on paused"
else
  bad "a failing queued hook lands the task on paused (at \`$(stage_of hook-queued)\`)"
fi
has "the reason names the hook's own log under tracking/" "tracking/hook-queued" \
  "$SPOOLWAY_PROJECT_HOME/queue/hook-queued.md"

# Placed by hand at the other three stages, the same way flow.sh's own
# hand-blocked scenario proves a road through `blocked` without spending a
# real lane on it — `blocked`, `paused` and `done` are reached by a person or
# a pipeline, never by `queue add`.
{
  echo "---"; echo "id: hook-blocked"; echo "title: hook-blocked, done"
  echo "stage: blocked"; echo "blocked_from: implement"; echo "group: live"
  echo "base: plan/live"; echo "pipeline: default"
  echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/hook-blocked.md"
{
  echo "---"; echo "id: hook-paused"; echo "title: hook-paused, done"
  echo "stage: paused"; echo "paused_at: implement"; echo "group: live"
  echo "base: plan/live"; echo "pipeline: default"
  echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/hook-paused.md"
{
  echo "---"; echo "id: hook-done"; echo "title: hook-done, done"
  echo "stage: done"; echo "group: live"
  echo "base: plan/live"; echo "pipeline: default"
  echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/hook-done.md"

dispatcher_start
for _ in $(seq 1 150); do
  [ -f "$TRACKING/hook-blocked · blocked.exit" ] \
    && [ -f "$TRACKING/hook-paused · paused.exit" ] \
    && [ -f "$TRACKING/hook-done · done.exit" ] \
    && break
  sleep 0.2
done

has "the blocked event's hook ran and failed" "1" "$TRACKING/hook-blocked · blocked.exit"
has "the paused event's hook ran and failed" "1" "$TRACKING/hook-paused · paused.exit"
has "the done event's hook ran and failed" "1" "$TRACKING/hook-done · done.exit"

if [ "$(stage_of hook-blocked)" = blocked ]; then
  ok "a failing hook on blocked only records the failure"
else
  bad "a failing hook on blocked only records the failure (at \`$(stage_of hook-blocked)\`)"
fi
if [ "$(stage_of hook-paused)" = paused ]; then
  ok "a failing hook on paused only records the failure"
else
  bad "a failing hook on paused only records the failure (at \`$(stage_of hook-paused)\`)"
fi
if drive hook-done paused 60; then
  ok "a failing done hook pauses the task, out of the archive"
else
  bad "a failing done hook pauses the task, out of the archive (at \`$(stage_of hook-done)\`)"
fi

# --------------------------------------------------- resume re-runs the hook
# The road out of a hook pause: fixing whatever the hook's own log named,
# then `spoolway resume` — it forgets the failed run, so the very next pass's
# `fire` starts it over rather than reading the same stale exit code and
# pausing the task right back. A `queued` pause resumes onto `queued`, where
# it is gated like any other; a `done` pause resumes straight back to `done`,
# not to `queued` or a step.
must "the hook is fixed" "$SPOOLWAY" config set issue_tracking.hook record.sh
dispatcher_restart   # a new hook name only takes effect on the next start —
                      # a pass landing between the config set and the first
                      # resume, still holding fail.sh in memory, would
                      # otherwise re-fire it and pause the task right back.

# Held at `implement` for a while. A stand-in's whole turn fits inside a poll
# interval, so without this the task can run on through every step to the
# archive between two of `drive`'s looks, and the check fails on a task that
# did exactly what it should. Step-scoped, so it governs this one turn.
echo linger:15 > "$CTL/hook-queued.implement"
must "resume clears the queued pause" "$SPOOLWAY" resume hook-queued
if drive hook-queued implement 60; then
  ok "resuming a queued hook pause re-runs the hook and lets the task start"
else
  bad "resuming a queued hook pause re-runs the hook and lets the task start \
(at \`$(stage_of hook-queued)\`)"
fi
# The task moving on is the effect; this is the cause. `record.sh` leaves
# its own mark beside the task file only when it actually runs, so a pass
# that let the task through without firing the fixed hook again leaves none.
has "and the fixed hook really ran for the resumed queued event" \
  "SPOOLWAY_EVENT=queued" "$SPOOLWAY_PROJECT_HOME/queue/hook-queued.md.env.queued"

must "resume clears the done pause" "$SPOOLWAY" resume hook-done
if drive hook-done gone 60; then
  ok "resuming a done hook pause re-runs the hook and lets the task archive"
else
  bad "resuming a done hook pause re-runs the hook and lets the task archive \
(still at \`$(stage_of hook-done)\`)"
fi
has "and the fixed hook really ran for the resumed done event" \
  "SPOOLWAY_EVENT=done" "$SPOOLWAY_PROJECT_HOME/queue/hook-done.md.env.done"

must "the hook is put back so it stops holding tasks" \
  "$SPOOLWAY" config set issue_tracking.hook ""

# ----------------------------------------------------------- github.sh, real
# The shipped script itself, not a hand-written stand-in — `configure_project`
# already ran `spoolway init` with no `--tracker`, which writes
# `.spoolway/hooks/github.sh` all the same (see the tracker scaffolding
# above), so this project already has the real file on disk. Pointed at a
# `gh` double on `PATH` rather than at a real repository — see
# `scripts/e2e/gh-stub.sh`'s own header for why that double is shared with
# `suites/stack.sh`.
GH_STUBBIN="$LIVE/gh-stub-bin"
mkdir -p "$GH_STUBBIN"
install -m 755 "$HERE/../gh-stub.sh" "$GH_STUBBIN/gh"
export GH_STUB_PRS="$LIVE/gh-prs"
export GH_STUB_ISSUES="$LIVE/gh-issues"
export GH_STUB_URL="file://$LIVE/gh-forge"
PATH="$GH_STUBBIN:$PATH"

must "the hook is switched to the real github.sh" \
  "$SPOOLWAY" config set issue_tracking.hook github.sh
must "and a project key" "$SPOOLWAY" config set issue_tracking.project_key acme/app

# The `# spoolway-requires: gh >= 2.97.0` line this task added to the real,
# shipped `github.sh` — not a hand-written stand-in the way every unit test
# of this check is — read off disk and checked against this stub's own
# `--version` answer. The one place both are real text rather than a value a
# unit test chose inline, so drift between the two — the shipped floor
# moving without the double's answer following it, or the reverse — would
# show up here and nowhere else.
silent_about "doctor is quiet about the shipped github.sh's own declared gh version" \
  "requires gh >=" "$SPOOLWAY" doctor

# The shipped script's own `check` branch, run by `doctor` against the same
# double: it passes while `gh` is logged in and can see `acme/app`, and each
# of the two things it tests fails it on its own, with the script's own
# stderr as the row. A branch that only ever said yes would pass the first
# of these three and neither of the others.
says "doctor runs the shipped github.sh's check branch, and it passes" \
  "ok    github.sh check" "$SPOOLWAY" doctor --verbose
says "a logged-out gh fails github.sh's check, naming the login" \
  "FAIL  github.sh check: github.sh check: gh is not logged in" \
  env GH_STUB_LOGGED_OUT=1 "$SPOOLWAY" doctor
says "a repository gh cannot see fails github.sh's check, naming the repository" \
  "FAIL  github.sh check: github.sh check: repository acme/app not found" \
  env GH_STUB_NO_REPO=1 "$SPOOLWAY" doctor

# ------------------------------------------- gh below the floor: the submit gate
# `queue add --from` is the non-interactive route the gate's own mockup
# names — printing the same block a person would see and proceeding without
# waiting for a key, since nothing here is a terminal. A `--version`-only
# double stands in for `gh`, just for this one call: the hook this proves
# never runs is never asked for anything else, and the suite's own PATH is
# left pointed at the full double for every github.sh test after this one.
LOWVER_BIN="$LIVE/gh-lowver-bin"
mkdir -p "$LOWVER_BIN"
cat >"$LOWVER_BIN/gh" <<'GHSTUB'
#!/bin/sh
case "$1" in
  --version) echo "gh version 2.46.0 (2024-01-01)"; exit 0 ;;
esac
exit 1
GHSTUB
chmod +x "$LOWVER_BIN/gh"

task_doc "$LIVE/github-gate.md" github-gate "$BODY" \
  "group: github-gate" \
  "group_description: proving the gh version gate"
GATE_OUT="$LIVE/github-gate.out"
env PATH="$LOWVER_BIN:$PATH" "$SPOOLWAY" queue add --from "$LIVE/github-gate.md" \
  >"$GATE_OUT" 2>&1
GATE_STATUS=$?
if [ "$GATE_STATUS" -eq 0 ]; then
  ok "a submit whose declared gh version is unmet still queues"
else
  bad "a submit whose declared gh version is unmet still queues (exit $GATE_STATUS)"
  sed 's/^/        /' "$GATE_OUT"
fi
has "the gate names the unmet declaration" "gh >= 2.97.0" "$GATE_OUT"
has "and says issue tracking is not supported" \
  "issue tracking is not supported." "$GATE_OUT"
works "the task reached the queue anyway" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/github-gate.md"
lacks "with the hook never invoked — no ticket written" \
  "ticket:" "$SPOOLWAY_PROJECT_HOME/queue/github-gate.md"
lacks "and no epic either" "epic:" "$SPOOLWAY_PROJECT_HOME/queue/github-gate.md"

# After both, not before: the dispatcher reads the config once at startup and
# the block above this one left `fail.sh` in it, so a restart any earlier
# would run every hook here as the always-failing one — silently, since that
# is exactly what `fail.sh` does. The restart also hands it the `PATH` the
# stub was just added to, which it could not have inherited when it started.
dispatcher_restart

# A group of one still gets its own epic now — every group does, whatever
# its size (see `.spoolway/hooks/github.sh`'s own `open` branch doc) — so
# this proves the ticket half *and* the epic half together, both linked by
# `--parent` rather than the old design's separate `sub_issues` REST call.
task_doc "$LIVE/github-open-check.md" github-open-check "$BODY" \
  "group: github-single" \
  "group_description: proving the real open branch"
must "queuing it calls the real hook's open branch" \
  "$SPOOLWAY" queue add --from "$LIVE/github-open-check.md"

TICKET=$(grep '^ticket:' "$SPOOLWAY_PROJECT_HOME/queue/github-open-check.md" | awk '{print $2}')
EPIC=$(grep '^epic:' "$SPOOLWAY_PROJECT_HOME/queue/github-open-check.md" | awk '{print $2}')
if [ -n "$TICKET" ] && [ -n "$EPIC" ]; then
  ok "github.sh answered a ticket and an epic url on open"
else
  bad "github.sh answered a ticket and an epic url on open"
fi
ISSUE_NUM=${TICKET##*/}
EPIC_NUM=${EPIC##*/}
has "the stub's issue was created against the configured project" \
  "repo=acme/app" "$GH_STUB_ISSUES/$ISSUE_NUM"
has "with the rendered ticket body, not the template's raw placeholders" \
  "Mirrors task \`github-open-check\` in group \`github-single\`." \
  "$GH_STUB_ISSUES/$ISSUE_NUM.body"
has "the ticket carries the task label" "spoolway:task" "$GH_STUB_ISSUES/$ISSUE_NUM.labels"
has "the epic carries the group label" "spoolway:group" "$GH_STUB_ISSUES/$EPIC_NUM.labels"
has "and the ticket is parented under the epic, by native --parent" \
  "$EPIC" "$GH_STUB_ISSUES/$ISSUE_NUM.parent"

# A group of two: the same epic is shared, `github-pair-b` also depends on
# `github-pair-a`, and `--blocked-by` is what carries that dependency's own
# ticket across onto the second issue. The description is deliberately more
# than one line: the epic's title must be the group slug itself, and the
# full multiline text — not just its first line — must lead the issue body.
task_doc "$LIVE/github-pair-a.md" github-pair-a "$BODY" \
  "group: github-pair" \
  "group_description: |" \
  "  Proving a shared epic and native parent/blocked-by links." \
  "  A second line the title must never swallow."
task_doc "$LIVE/github-pair-b.md" github-pair-b "$BODY" \
  "group: github-pair" \
  "depends_on: [github-pair-a]"
must "queuing a group of two calls the hook's epic branch too" \
  "$SPOOLWAY" queue add --from "$LIVE/github-pair-a.md" --from "$LIVE/github-pair-b.md"

PAIR_EPIC=$(grep '^epic:' "$SPOOLWAY_PROJECT_HOME/queue/github-pair-a.md" | awk '{print $2}')
PAIR_TICKET_A=$(grep '^ticket:' "$SPOOLWAY_PROJECT_HOME/queue/github-pair-a.md" | awk '{print $2}')
PAIR_TICKET_B=$(grep '^ticket:' "$SPOOLWAY_PROJECT_HOME/queue/github-pair-b.md" | awk '{print $2}')
PAIR_EPIC_B=$(grep '^epic:' "$SPOOLWAY_PROJECT_HOME/queue/github-pair-b.md" | awk '{print $2}')
if [ -n "$PAIR_EPIC" ]; then
  ok "github.sh answered an epic url for a group of two"
else
  bad "github.sh answered an epic url for a group of two"
fi
works "both tasks of the pair share the one epic" \
  test "$PAIR_EPIC" = "$PAIR_EPIC_B"
# `has` is a substring check, so it would also pass for a title spoolway never
# asked for (`title=github-pair-extra`); an exact line match is what actually
# proves `SPOOLWAY_GROUP` was used verbatim.
if grep -qFx "title=github-pair" "$GH_STUB_ISSUES/${PAIR_EPIC##*/}"; then
  ok "the epic's title is the group slug itself, not the description"
else
  bad "the epic's title is the group slug itself, not the description"
  sed 's/^/        /' "$GH_STUB_ISSUES/${PAIR_EPIC##*/}" 2>/dev/null
fi

# Independent substring/line-order checks would still pass with an extra
# blank line, a missing one, or a swallowed template — none of that proves
# the exact ordered body the ## Mockup describes. Build the whole expected
# body and `cmp` the full captured file against it.
#
# No `- Source:` line is expected: neither fixture sets `source:`, so
# `$SPOOLWAY_SOURCE` resolves empty and `task_template::render_tracking`
# drops a template line whose only placeholder is empty (task_template.rs,
# `render_tracking`/`render_line`) rather than leaving a bare `- Source: `.
PAIR_BODY="$GH_STUB_ISSUES/${PAIR_EPIC##*/}.body"
printf '%s\n' \
  "Proving a shared epic and native parent/blocked-by links." \
  "A second line the title must never swallow." \
  "" \
  '- Group: `github-pair`' \
  '- Tasks queued together: `2`' \
  > "$LIVE/expected-epic-body.txt"
if cmp -s "$LIVE/expected-epic-body.txt" "$PAIR_BODY"; then
  ok "the epic's body is exactly the full description, one blank line, then the intact rendered template"
else
  bad "the epic's body is exactly the full description, one blank line, then the intact rendered template"
  diff "$LIVE/expected-epic-body.txt" "$PAIR_BODY" | sed 's/^/        /'
fi
has "the first ticket is parented under the shared epic" \
  "$PAIR_EPIC" "$GH_STUB_ISSUES/${PAIR_TICKET_A##*/}.parent"
has "the second ticket names the first as blocking it" \
  "${PAIR_TICKET_A##*/}" "$GH_STUB_ISSUES/${PAIR_TICKET_B##*/}.blocked_by"

# Placed on `blocked` by hand, the same way `hook-blocked` above stands in for
# a pipeline actually reaching it — naming the ticket `open` already secured.
# Carries its own `## Status Log`, unlike `$BODY`: `comment_snapshot` prints
# that section's own content, or nothing at all when a task file has none —
# so proving it actually reaches the comment needs one here to extract.
{
  echo "---"; echo "id: github-blocked"; echo "title: github-blocked, done"
  echo "stage: blocked"; echo "blocked_from: implement"
  echo "group: github-single"
  echo "base: plan/live"; echo "pipeline: default"
  echo "ticket: $TICKET"
  echo "---"; cat "$BODY"
  echo; echo "## Status Log"
  echo "- blocked on implement, waiting on a dependency"
} > "$SPOOLWAY_PROJECT_HOME/queue/github-blocked.md"

dispatcher_start
# Waits for the comment this task's own hook posts, not merely for a file at
# that name. `github-open-check` holds the same ticket, so its own `started`
# label edit or blocked comment can land first; polling on the content
# asserts against the right one whichever arrives first.
for _ in $(seq 1 150); do
  grep -q "github-blocked" "$GH_STUB_ISSUES/$ISSUE_NUM.comment" 2>/dev/null && break
  sleep 0.2
done
has "the blocked event's comment names the task" \
  "github-blocked" "$GH_STUB_ISSUES/$ISSUE_NUM.comment"
has "and carries the status log section, not the whole task file" \
  "## Status Log" "$GH_STUB_ISSUES/$ISSUE_NUM.comment"

# --------------------------------------------------------------- fetch, real
# A person's own issue, filed on the tracker before spoolway ever touched
# it — seeded straight into the stub's flat files, the shape its own header
# describes, rather than created through `gh issue create` the way every
# issue above this line was. `spoolway issue show` never writes anything, so
# there is nothing to queue here at all.
FETCH_NUM=501
{ echo "repo=acme/app"; echo "title=Rework the session store"; } >"$GH_STUB_ISSUES/$FETCH_NUM"
printf 'Sessions are keyed by a random string with no expiry.' \
  >"$GH_STUB_ISSUES/$FETCH_NUM.body"
echo '[{"name":"area:auth"},{"name":"size:l"}]' >"$GH_STUB_ISSUES/$FETCH_NUM.labels.json"
echo '[{"author":{"login":"rk"},"body":"Only reproduces on mobile Safari."}]' \
  >"$GH_STUB_ISSUES/$FETCH_NUM.comments.json"

FETCH_JSON=$("$SPOOLWAY" issue show "$FETCH_NUM")
if jq -e --arg n "$FETCH_NUM" '
     .ref == $n and .title == "Rework the session store" and .state == "open" and
     (.labels | index("area:auth")) and (.comments[0].author == "rk")
   ' <<<"$FETCH_JSON" >/dev/null 2>&1
then
  ok "spoolway issue show reads the real hook's fetch branch"
else
  bad "spoolway issue show reads the real hook's fetch branch ($FETCH_JSON)"
fi

# ------------------------------------------------------- open hangs under it
# Queuing a task whose own `source:` names that same filed issue: `open`'s
# `same_repo_issue` check has to recognise it and give the group's own new
# epic a native `--parent` naming it — and, separately, leave a plan-page
# `source:` (every other task in this suite) exactly alone, since none of
# those match `same_repo_issue`'s own `.../issues/<n>` pattern.
task_doc "$LIVE/github-hang-under.md" github-hang-under "$BODY" \
  "group: github-hang-single" \
  "group_description: proving same_repo_issue parents the epic natively" \
  "source: $GH_STUB_URL/acme/app/issues/$FETCH_NUM"
must "queuing a task whose source names a filed issue calls the open branch" \
  "$SPOOLWAY" queue add --from "$LIVE/github-hang-under.md"

HANG_EPIC=$(grep '^epic:' "$SPOOLWAY_PROJECT_HOME/queue/github-hang-under.md" | awk '{print $2}')
has "the filed issue is named as the new epic's own parent" \
  "$GH_STUB_URL/acme/app/issues/$FETCH_NUM" "$GH_STUB_ISSUES/${HANG_EPIC##*/}.parent"
works "and github-open-check's own epic, queued with no source: at all, never grew one" \
  test ! -e "$GH_STUB_ISSUES/$EPIC_NUM.parent"


finish
