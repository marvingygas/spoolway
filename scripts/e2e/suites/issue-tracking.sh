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
# covers: issue_tracking.on_fail — a non-zero exit under "pause" holds the task on `queued` and `done`, and only records the failure on `blocked` and `paused`
# covers: issue_tracking.key_in_names — with it on and the hook answering slug=, `queue add` writes `group: <slug>-<group>` and `branch: task/<slug>-<id>` and stores the hook's url=
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
# Handles every event the same way, queued/blocked/paused/done/open/fetch
# alike — there is no branch here to miss.
env | sort > "$SPOOLWAY_TASK_FILE.env.$SPOOLWAY_EVENT"
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
# `handover` (`spoolway stack`) reads $SPOOLWAY_GH for its own `gh` calls, but
# the `checks` step right behind it is a plain `run: gh pr checks` — no
# override of its own, so it resolves `gh` off PATH like any other command.
# Left at `new_forge`'s own double there, it would ask a `gh` that never
# heard of the pull request `spoolway stack` just opened through this one —
# two different pull-request stores, and a wrong answer only because the
# other store still happened to hold something left over from an earlier
# task. So this double goes on PATH too, ahead of `new_forge`'s, for exactly
# as long as $SPOOLWAY_GH points at it.
TRACKED_GH_BIN="$LIVE/tracked-gh-bin"
mkdir -p "$TRACKED_GH_BIN"
install -m 755 "$HERE/../gh-stub.sh" "$TRACKED_GH_BIN/gh"
PATH_BEFORE_TRACKED_GH="$PATH"
PATH="$TRACKED_GH_BIN:$PATH"; export PATH
# The dispatcher inherited its environment when it started, before any of
# that existed — restarted, so the `handover` it runs sees all three.
dispatcher_restart

# A dependent pair rather than two independent tasks: `tracked-b` cannot even
# start until `tracked-a` has archived, which is what keeps `SPOOLWAY_GROUP_LAST`
# deterministic below — the two can never reach `done` in the same pass, so
# `tracked-a`'s own hook always sees `tracked-b` still open.
task_doc "$LIVE/tracked-a.md" tracked-a "$BODY" "group: tracked-pair" \
  "touches: [notes/tracked-a.md]" "group_description: a dependent pair, tracked end to end"
task_doc "$LIVE/tracked-b.md" tracked-b "$BODY" "group: tracked-pair" \
  "touches: [notes/tracked-b.md]" "depends_on: [tracked-a]"
must "the first of a dependent pair queues" "$SPOOLWAY" queue add --from "$LIVE/tracked-a.md"
must "the second, depending on it, queues too" "$SPOOLWAY" queue add --from "$LIVE/tracked-b.md"

if drive tracked-a gone 60 && drive tracked-b gone 60; then
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

ENV_A_DONE="$SPOOLWAY_PROJECT_HOME/queue/tracked-a.md.env.done"
ENV_B_DONE="$SPOOLWAY_PROJECT_HOME/queue/tracked-b.md.env.done"
silent_about "the first of the pair's done event is not the group's last" \
  "SPOOLWAY_GROUP_LAST" cat "$ENV_A_DONE"
has "the second's done event is — it is the group's last open task" \
  "SPOOLWAY_GROUP_LAST=1" "$ENV_B_DONE"

# Undone: everything past here is meant to see the same not-github forge the
# rest of this suite was written against.
must "origin is the bare forge again" \
  git -C "$LIVE/proj" remote set-url origin "$ORIGIN"
must "and the redirect is dropped with it" \
  git -C "$LIVE/proj" config --unset "url.$ORIGIN.insteadOf"
unset SPOOLWAY_GH
PATH="$PATH_BEFORE_TRACKED_GH"; export PATH
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
task_doc "$LIVE/undescribed.md" undescribed "$BODY" "group: undescribed-group" \
  "touches: [notes/undescribed.md]"
refuses "a group with no group_description is refused once a hook is configured" \
  "group \`undescribed-group\` sets no \`group_description:\`" \
  "$SPOOLWAY" queue add --from "$LIVE/undescribed.md"
works "nothing was queued for the refused group" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/undescribed.md"

task_doc "$LIVE/opened-a.md" opened-a "$BODY" "group: opened-pair" \
  "touches: [notes/opened-a.md]" "group_description: a mirrored pair of tasks"
task_doc "$LIVE/opened-b.md" opened-b "$BODY" "group: opened-pair" \
  "touches: [notes/opened-b.md]" "depends_on: [opened-a]"
must "a dependent pair queues in one call, opening a ticket for each" \
  "$SPOOLWAY" queue add --from "$LIVE/opened-a.md" --from "$LIVE/opened-b.md"

OPENED_A="$SPOOLWAY_PROJECT_HOME/queue/opened-a.md"
OPENED_B="$SPOOLWAY_PROJECT_HOME/queue/opened-b.md"
has "the group's epic landed in the first document" "epic: acme/app#" "$OPENED_A"
has "the first document's own ticket" "ticket: acme/app#" "$OPENED_A"
has "the same epic landed in the dependent's document" \
  "$(grep '^epic:' "$OPENED_A")" "$OPENED_B"
has "the dependent's own, different ticket" "ticket: acme/app#" "$OPENED_B"
has "the dependent's call carried its parent's ticket id" \
  "$(grep '^ticket:' "$OPENED_A" | awk '{print $2}')" \
  "$SPOOLWAY_PROJECT_HOME/tracking/depends.opened-b"
has "the open hook read the real, live contents of the document" \
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
  "touches: [notes/opened-ok.md]" "group_description: a batch that fails partway through"
task_doc "$LIVE/opened-fails.md" opened-fails "$BODY" "group: opened-fail-batch" \
  "touches: [notes/opened-fails.md]"
refuses "a mid-batch hook failure queues nothing" "opened-fails" \
  "$SPOOLWAY" queue add --from "$LIVE/opened-ok.md" --from "$LIVE/opened-fails.md"
if [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/opened-ok.md" ] \
  && [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/opened-fails.md" ]; then
  ok "neither document of the failed batch was queued"
else
  bad "neither document of the failed batch was queued"
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
  "touches: [notes/keyed-a.md]" "group_description: reworking the keyed pair"
task_doc "$LIVE/keyed-b.md" keyed-b "$BODY" "group: keyed-rework" \
  "touches: [notes/keyed-b.md]" "depends_on: [keyed-a]"
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

# ------------------------------------------------- on_fail = "pause"
# A hook that always fails, on each of the four events by hand: `pause` holds
# a task on `queued` and out of the archive on `done`, and only records the
# failure on `blocked` and `paused` — both already stopped for a person, so
# nothing about pausing them again would mean anything.
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
must "and on_fail pauses the task" "$SPOOLWAY" config set issue_tracking.on_fail pause

task_doc "$LIVE/hook-queued.md" hook-queued "$BODY" "group: live" \
  "touches: [notes/hook-queued.md]" "group_description: a task under an always-failing hook"
must "a task queues under an always-failing hook" \
  "$SPOOLWAY" queue add --from "$LIVE/hook-queued.md"

if drive hook-queued paused 30; then
  ok "a failing queued hook under on_fail=pause lands the task on paused"
else
  bad "a failing queued hook under on_fail=pause lands the task on paused \
(at \`$(stage_of hook-queued)\`)"
fi

# Placed by hand at the other three stages, the same way flow.sh's own
# hand-blocked scenario proves a road through `blocked` without spending a
# real lane on it — `blocked`, `paused` and `done` are reached by a person or
# a pipeline, never by `queue add`.
{
  echo "---"; echo "id: hook-blocked"; echo "title: hook-blocked, done"
  echo "stage: blocked"; echo "blocked_from: implement"; echo "group: live"
  echo "base: plan/live"; echo "pipeline: default"
  echo "touches: [notes/hook-blocked.md]"; echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/hook-blocked.md"
{
  echo "---"; echo "id: hook-paused"; echo "title: hook-paused, done"
  echo "stage: paused"; echo "paused_at: implement"; echo "group: live"
  echo "base: plan/live"; echo "pipeline: default"
  echo "touches: [notes/hook-paused.md]"; echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/hook-paused.md"
{
  echo "---"; echo "id: hook-done"; echo "title: hook-done, done"
  echo "stage: done"; echo "group: live"
  echo "base: plan/live"; echo "pipeline: default"
  echo "touches: [notes/hook-done.md]"; echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/hook-done.md"

dispatcher_start
TRACKING="$SPOOLWAY_PROJECT_HOME/tracking"
for _ in $(seq 1 150); do
  [ -f "$TRACKING/hook-blocked · blocked.exit" ] \
    && [ -f "$TRACKING/hook-paused · paused.exit" ] \
    && [ -f "$TRACKING/hook-done · done.failed" ] \
    && break
  sleep 0.2
done

has "the blocked event's hook ran and failed" "1" "$TRACKING/hook-blocked · blocked.exit"
has "the paused event's hook ran and failed" "1" "$TRACKING/hook-paused · paused.exit"
# `done` is the one event a failure is retried on, and `retry_if_failed`
# forgets the run — `.exit` file and all — on every pass that finds it still
# failing. The `.failed` marker it leaves first is the evidence that survives;
# see `failure_count` in src/tracking.rs, which reads both for the same reason.
works "the done event's hook ran and failed" \
  test -f "$TRACKING/hook-done · done.failed"

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
if [ -f "$SPOOLWAY_PROJECT_HOME/queue/hook-done.md" ] && [ "$(stage_of hook-done)" = done ]; then
  ok "a failing done hook under on_fail=pause holds the task out of the archive"
else
  bad "a failing done hook under on_fail=pause holds the task out of the archive \
(queue file present: $([ -f "$SPOOLWAY_PROJECT_HOME/queue/hook-done.md" ] && echo yes || echo no), \
stage: $(stage_of hook-done))"
fi

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
  "group: github-gate" "touches: [notes/github-gate.md]" \
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
  "group: github-single" "touches: [notes/github-open-check.md]" \
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
# ticket across onto the second issue.
task_doc "$LIVE/github-pair-a.md" github-pair-a "$BODY" \
  "group: github-pair" "touches: [notes/github-pair-a.md]" \
  "group_description: proving a shared epic and native parent/blocked-by links"
task_doc "$LIVE/github-pair-b.md" github-pair-b "$BODY" \
  "group: github-pair" "touches: [notes/github-pair-b.md]" \
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
has "the epic's title is the group description's own first line, not the group's name" \
  "title=proving a shared epic and native parent/blocked-by links" \
  "$GH_STUB_ISSUES/${PAIR_EPIC##*/}"
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
  echo "touches: [notes/github-blocked.md]"; echo "---"; cat "$BODY"
  echo; echo "## Status Log"
  echo "- blocked on implement, waiting on a dependency"
} > "$SPOOLWAY_PROJECT_HOME/queue/github-blocked.md"

dispatcher_start
# Waits for the comment this task's own hook posts, not merely for a file at
# that name. `github-open-check` holds the same ticket, so its own `queued`
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
  "group: github-hang-single" "touches: [notes/github-hang-under.md]" \
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
