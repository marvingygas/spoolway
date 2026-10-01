#!/usr/bin/env bash
# The queue tab's `t` picker, driven end to end — the one path no unit test
# can drive, since `run_screen` is exercised headlessly in Rust already, but
# never as the whole binary reading real keystrokes off a real pipe, under
# the pty `on_screen` gives it. `t` forks a whole group once per pipeline:
# pipelines ticked on the first popup, one skip set per ticked pipeline on the
# second, and one full copy of the group per tick, each copy in a group of its
# own named `<group>-<pipeline>`, every arm under one freshly minted trial id.
#
# The first half of this suite drives no dispatcher: a trial's whole setup job
# is landing arms in the queue directory with the right `pipeline:`, `skip:`,
# `group:`, `trial:` and `trial_group:` on them, and that is what the
# queue-screen block below
# asserts — not that any of them ever runs. The second half — everything past
# "dispatch and cleanup" — is the dependent half: a real dispatcher, a real
# forge and a real issue-tracking hook, proving what a trial's runtime
# boundary actually does with arms once they run, and what it disposes of once
# they settle.
#
# No `covers:` tag of its own — see `commands.sh`'s own queue-screen block:
# the map `coverage.sh` builds only enumerates `config.toml` keys and
# pipeline step keys, and a queue-screen gesture is neither.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

LIVE=${WORK:-$(mktemp -d)}

new_repo "$LIVE/proj"
configure_project plan/live

# Two pending documents sharing one group, `beta` depending on `alpha` — the
# unit `t` forks whole. `spoolway init` writes exactly the two built-in
# pipelines, `default` and `bugfix`, under `.spoolway/pipelines/`, and
# `trial_pipeline_names` lists them in the same alphabetical order
# (`bugfix`, `default`) the pick screen draws its rows in.
BODY="$LIVE/body.md"
task_body "$BODY"
# A bare `pipeline:` line leaves each document genuinely unassigned — see
# `task_doc`'s own doc comment — so the pick screen opens with nothing
# ticked, and every tick below is one this suite made.
pending_doc alpha "$BODY" "group: audits" "pipeline:"
pending_doc beta "$BODY" "group: audits" "pipeline:" \
  "depends_on: [alpha]"

# `Tab` focuses the tasks pane on a task inside the group (`alpha`, first in
# reading order since it is the dependency), proving `t` reaches the whole
# group from a selected task too, not only from the groups pane. `t` opens
# the pick-pipelines popup with nothing ticked and the cursor on `bugfix`,
# the first row: `space` ticks it, `j` moves onto `default` and `space`
# ticks that too. `enter` then advances to the skips screen, one block per
# ticked pipeline, `bugfix`'s first. The flattened cursor opens on its first
# checkbox: one `j` reaches its second, `fix`, and `space` ticks it; eight
# more `j`s walk past the rest of `bugfix`'s own checkboxes onto `default`'s
# third one, `document`, and `space` ticks that too. Both runs are counted
# off the flattened checkbox list, so they have to be recounted whenever
# either built-in pipeline gains or loses a step. `bugfix` contributes seven
# checkboxes, not the six steps it declares, because every pipeline is
# loaded with a `blocked` step appended to it; `default` contributes five
# the same way. `enter` mints and writes all four arms and goes back to
# browsing; the trailing `n` is noise the screen ignores, and the pipe
# running dry ends it the same way `esc` would.
on_screen '\tt j \rj jjjjjjjj \rn' /dev/null

# Two copies of the two-task chain, ids numbered in pipeline order: the
# `bugfix` copy is `alpha-1`, `beta-1`, the `default` copy `alpha-2`,
# `beta-2`.
ARMS="alpha-1 beta-1 alpha-2 beta-2"
for arm in $ARMS; do
  works "the $arm arm reaches the queue" \
    test -f "$SPOOLWAY_PROJECT_HOME/queue/$arm.md"
done
works "four arms and no more" \
  bash -c '[ "$(ls "$1"/queue/*.md | wc -l)" -eq 4 ]' _ "$SPOOLWAY_PROJECT_HOME"
works "never either task's own bare id" \
  bash -c '[ ! -e "$1/queue/alpha.md" ] && [ ! -e "$1/queue/beta.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

for arm in alpha-1 beta-1; do
  has "$arm runs the first ticked pipeline" "pipeline: bugfix" \
    "$SPOOLWAY_PROJECT_HOME/queue/$arm.md"
  has "$arm sits in the bugfix copy's own group" "group: audits-bugfix" \
    "$SPOOLWAY_PROJECT_HOME/queue/$arm.md"
  works "$arm carries bugfix's ticked skip, and none of default's" \
    bash -c 'grep -A1 "^skip:" "$1" | tail -1 | grep -qxF -- "- fix"' \
    _ "$SPOOLWAY_PROJECT_HOME/queue/$arm.md"
done
for arm in alpha-2 beta-2; do
  has "$arm runs the second ticked pipeline" "pipeline: default" \
    "$SPOOLWAY_PROJECT_HOME/queue/$arm.md"
  has "$arm sits in the default copy's own group" "group: audits-default" \
    "$SPOOLWAY_PROJECT_HOME/queue/$arm.md"
  works "$arm carries default's ticked skip, and none of bugfix's" \
    bash -c 'grep -A1 "^skip:" "$1" | tail -1 | grep -qxF -- "- document"' \
    _ "$SPOOLWAY_PROJECT_HOME/queue/$arm.md"
done
for arm in $ARMS; do
  has "$arm names the group the trial forked" "trial_group: audits" \
    "$SPOOLWAY_PROJECT_HOME/queue/$arm.md"
done

QUEUE_LIST=$("$SPOOLWAY" queue list 2>&1)
says "the queue lists the bugfix copy as a group of its own" "audits-bugfix" \
  bash -c 'printf "%s" "$1"' _ "$QUEUE_LIST"
says "and the default copy as another" "audits-default" \
  bash -c 'printf "%s" "$1"' _ "$QUEUE_LIST"

works "the shared trial id is freshly minted, t plus sixteen hex" \
  bash -c 'grep -qE "^trial: t[0-9a-f]{16}$" "$1"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
works "and the same id lands on all four arms of the one launch" \
  bash -c '
    a=$(grep "^trial:" "$1/queue/alpha-1.md")
    [ -n "$a" ] || exit 1
    for arm in beta-1 alpha-2 beta-2; do
      [ "$a" = "$(grep "^trial:" "$1/queue/$arm.md")" ] || exit 1
    done
  ' _ "$SPOOLWAY_PROJECT_HOME"

# Each copy is the whole chain: `beta` waits on `alpha` inside its own copy,
# never on the other pipeline's arm, and never on the bare id nothing in
# this batch is queued under.
works "beta-1 waits on alpha-1, its own copy's minted sibling" \
  bash -c 'grep -A1 "^depends_on:" "$1" | tail -1 | grep -qxF -- "- alpha-1"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"
works "beta-2 waits on alpha-2, its own copy's minted sibling" \
  bash -c 'grep -A1 "^depends_on:" "$1" | tail -1 | grep -qxF -- "- alpha-2"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/beta-2.md"
works "neither is left naming the bare id" \
  bash -c '! grep -qxF -- "- alpha" "$1/queue/beta-1.md" && ! grep -qxF -- "- alpha" "$1/queue/beta-2.md"' \
  _ "$SPOOLWAY_PROJECT_HOME"

# A trial forks the documents; it does not submit them. The pending copies
# are templates the picker read from, not a batch the screen queued and
# cleared.
works "both source tasks are left exactly where they were" \
  bash -c 'test -f "$1/pending/alpha.md" && test -f "$1/pending/beta.md"' \
  _ "$SPOOLWAY_PROJECT_HOME"

# A second template, this one already run through a pipeline once: its sole
# document lives in the archive, carrying every key spoolway stamped on that
# run — `stage:` chief among them, which `parse_submission` refuses outright.
# `list_groups` reads such a group's task straight out of `archive/`,
# verbatim, so forking it is `t` over exactly the shape a task archived
# earlier has. Not `queue/`: the queue tab never lists a queued group, and
# its filter never reaches one. `f` narrows to it by name, `enter` leaves the search
# box keeping the query, and `t` then reaches it straight from the groups
# pane, with no `Tab` needed. Nothing is ticked, since the task names no
# pipeline, and `enter` will not advance until something is: `j` moves onto
# `default` and `space` ticks it, `enter` advances past the pick screen, and
# `enter` again launches with nothing ticked to skip.
mkdir -p "$SPOOLWAY_PROJECT_HOME/archive"
task_doc "$SPOOLWAY_PROJECT_HOME/archive/old-run.md" old-run "$BODY" \
  "group: old-run" \
  "stage: done" \
  "run: r00000000000000af" \
  "branch: task/old-run" \
  "base: master" \
  "cut_from: master" \
  "base_commit: 0000000000000000000000000000000000000000" \
  "attempts: 2" \
  "pipeline:"

on_screen 'fold-run\rtj \r\rn' /dev/null

works "the reset lets a stamped task reach \`finish_trial\` at all" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"
has "under the pipeline ticked for it" "pipeline: default" \
  "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"
lacks "the reset held: no stamped stage on the forked arm" "stage: done" \
  "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"
lacks "nor the run id the earlier run minted" "run: r00000000000000af" \
  "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"

# The source document goes now that the arm is forked off it: `t` leaves a
# source exactly where it found it, and this one was written straight into
# `archive/` with no `pipeline:` on purpose, a record no real run left. The
# runtime half below asserts on what the archive holds after a real
# dispatcher, so a hand-written entry is cleared out of its way first.
rm -f "$SPOOLWAY_PROJECT_HOME/archive/old-run.md"

# ------------------------------------------------------- dispatch and cleanup
# Everything above only ever wrote queue files. From here a real dispatcher
# runs the four trial arms — two copies of a two-task chain, one per
# pipeline — and one ordinary task, against a real (if local) forge and a real `[issue_tracking]`
# hook — proving the runtime boundary the plan's own "trial runtime owns
# safety and disposal" decision describes: no queued/done hook, no publishing,
# and full disposal once every arm of a trial has settled, while an ordinary
# task beside it gets all three exactly as before.
new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$LIVE/ctl" "" "" "" "$FORGE"
publish plan/live

# A stand-in writes no transcript of its own unless told to (see
# `agents/transcript.sh`), and with none there is nothing for `harvest` to
# bank into `usage.jsonl` — so nothing here is written unless something asks.
# The trial-vs-control comparison below needs the ledger, so every step
# writes one turn.
mkdir -p "$LIVE/ctl"
printf '400\n' > "$LIVE/ctl/transcript"

# Records its own environment beside the task file it ran for, one file per
# event — the same double `issue-tracking.sh`'s own `[issue_tracking]` block
# uses, so a hook that never fired leaves no file at all rather than an empty
# one.
mkdir -p .spoolway/hooks
cat > .spoolway/hooks/record.sh <<'EOF'
#!/bin/sh
env | sort > "$SPOOLWAY_TASK_FILE.env.$SPOOLWAY_EVENT"
exit 0
EOF
chmod +x .spoolway/hooks/record.sh
must "the hook is named" "$SPOOLWAY" config set issue_tracking.hook record.sh
must "and a project key" "$SPOOLWAY" config set issue_tracking.project_key acme/app

# `alpha-1`, `beta-1`, `alpha-2` and `beta-2` are exactly the four-arm trial
# the queue-screen block above already minted — a real trial id, shared
# across both copies — so there is no need to hand-write one: `trial:` is a key
# `queue_add::parse_submission` refuses on any document, precisely because it
# is spoolway's own to mint, never a person's or a script's to set. `old-run-1`
# is a trial of one and is left queued rather than driven, so it dispatches in
# the background the same as a person's own queue would, without this suite
# waiting on or asserting over a third arm.
#
# One ordinary task beside them, queued the everyday way and never touched by
# `t` — same pipeline, same shape of work, so the only thing that can explain
# a difference in what happens to it and to the arms is trial mode itself.
TRIAL_ID=$(grep '^trial:' "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md" | awk '{print $2}')
task_doc "$LIVE/control.md" control "$BODY" "group: control-live" \
  "pipeline: default" \
  "group_description: an ordinary control task beside the trial arms"
must "control queues" "$SPOOLWAY" queue add --from "$LIVE/control.md"

if drive control gone 180; then ok "the ordinary control task runs the pipeline to done"
else bad "the ordinary control task runs the pipeline to done (stuck at \`$(stage_of control)\`)"; fi
for arm in $ARMS; do
  if drive "$arm" gone 180; then ok "$arm runs its copy's pipeline to done"
  else bad "$arm runs its copy's pipeline to done (stuck at \`$(stage_of "$arm")\`)"; fi
done

# Acceptance criterion: trial dispatch suppresses the queued/started/done
# issue hooks — the control's own env files below are the proof the hook mechanism
# itself works in this run at all, so a trial arm's missing files are absence
# of the event, not absence of the hook.
has "the control's queued event reached the hook" "SPOOLWAY_EVENT=queued" \
  "$SPOOLWAY_PROJECT_HOME/queue/control.md.env.queued"
has "and its done event too" "SPOOLWAY_EVENT=done" \
  "$SPOOLWAY_PROJECT_HOME/queue/control.md.env.done"
has "and its started event, as it left queued" "SPOOLWAY_EVENT=started" \
  "$SPOOLWAY_PROJECT_HOME/queue/control.md.env.started"
for arm in $ARMS; do
  works "$arm's queued event never reached the hook" \
    bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/queue/$arm.md.env.queued"
  works "$arm's started event never reached the hook" \
    bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/queue/$arm.md.env.started"
  works "$arm's done event never reached the hook either" \
    bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/queue/$arm.md.env.done"
done

# Acceptance criterion: trial dispatch makes spoolway-owned publishing a
# no-op — the control opens a real pull request on the same forge, so a
# trial arm's missing one is the boundary working, not a forge that never
# came up.
if handed_over control; then ok "the control is handed over as a pushed branch and a pull request"
else bad "the control is handed over as a pushed branch and a pull request"; fi
for arm in $ARMS; do
  works "$arm's branch was never pushed to the forge" \
    bash -c '! git ls-remote --exit-code --heads origin "task/$1" >/dev/null 2>&1' _ "$arm"
  works "$arm never opened a pull request" \
    bash -c '! grep -lx "head=task/$1" "$FORGE"/prs/[0-9]* >/dev/null 2>&1' _ "$arm"
done

# Acceptance criterion: once every task in a trial settles, its documents,
# worktrees and local branches are removed — all four arms, across both
# copies' groups — the control's own archive
# document, worktree and branch survive exactly as before, so the trial
# arms' disappearance is the trial boundary and not ordinary teardown acting
# on everyone alike.
has "the control keeps its archive task, the durable record cleanup means" \
  "id: control" "$SPOOLWAY_PROJECT_HOME/archive/control.md"
for arm in $ARMS; do
  works "$arm's archive task is gone, not merely queued for retention" \
    bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/archive/$arm.md"
  works "$arm's local branch is gone" \
    bash -c '! git rev-parse --verify -q "task/$1" >/dev/null' _ "$arm"
  works "$arm's worktree is gone" \
    bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/worktrees/task-$arm"
done

# Acceptance criterion: usage rows remain correlated by trial id — the one
# thing a settled trial leaves standing, which is what makes `spoolway eval
# --by task --trial <id>` still answerable after every disposable copy is gone.
TRIAL_ROWS=$(grep -c "\"trial\":\"$TRIAL_ID\"" "$SPOOLWAY_PROJECT_HOME/usage.jsonl" 2>/dev/null || true)
works "all four arms' usage rows are still in the ledger, correlated by trial id" \
  bash -c '[ "$1" -ge 4 ]' _ "${TRIAL_ROWS:-0}"
# Every one of those rows names the group the trial forked, which no arm's
# own task does any more: each sat in its copy's `<group>-<pipeline>`, and
# all four were deleted when the trial settled.
works "and every one of them names the source group beside the trial" \
  bash -c '
    rows=$(grep "\"trial\":\"$1\"" "$2")
    [ "$(grep -c "\"trial_group\":\"audits\"" <<<"$rows")" -eq "$(wc -l <<<"$rows")" ]
  ' _ "$TRIAL_ID" "$SPOOLWAY_PROJECT_HOME/usage.jsonl"

# And that question, asked: `--by task` is one row per arm, closed by the
# table's `Total` line, and `--trial` adds one delta line per arm against
# the first. Which arm started first is the mock's scheduling to decide, so
# the delta lines are counted rather than matched by name.
COMPARE_OUT=$("$SPOOLWAY" eval --by task --trial "$TRIAL_ID" 2>&1)
for arm in $ARMS; do
  says "eval --by task --trial has a row for $arm" "$arm" \
    bash -c 'printf "%s" "$1"' _ "$COMPARE_OUT"
done
works "the arms' table closes on its Total line" \
  bash -c 'grep -Eq "^Total +4 " <<<"$1"' _ "$COMPARE_OUT"
works "and compares every other arm against the first, one delta line each" \
  bash -c '[ "$(grep -Ec "^(alpha|beta)-[12] vs (alpha|beta)-[12]: pass .*, cost .*, time " <<<"$1")" -eq 3 ]' \
  _ "$COMPARE_OUT"

# ------------------------------------------------------------ explicit discard
# A trial has two cleanup triggers, and everything above only proves the first
# one: settlement, which happens by itself once the last arm reaches `done`.
# This is the other one — a person who has seen enough and wants the arms gone
# now, mid-flight, with nothing waiting on them to finish.
#
# A trial of its own group, so the discard below has nothing in common with
# the two arms already settled. `t`'s minimal form: `f` narrows to the group
# by name, `enter` leaves the search box keeping the query, `t` opens the
# picker with the task's own `default` already ticked, two `enter`s take
# both screens' defaults and queue the trial as one copy under it;
# the trailing `n` is noise it ignores, and the pipe running dry ends the
# screen.
#
# Mid-flight is a state this makes rather than one it catches. A mock lane's
# step is over in a couple of hundred milliseconds and `drive` looks every two
# hundred, so waiting for one named stage is a bet on the look landing inside
# it — and with four suites sharing a machine the look lands after the arm has
# run all the way to `done` and settled, taking its worktree, its branch and
# its document with it. Every assertion below then fails against an arm that
# no longer exists, and the discard refuses an id nothing carries any more.
# That is exactly how this failed once the tier started running concurrently.
#
# So every pipeline here gates its own entry step: whichever pipeline the
# arm's copy runs, the arm runs that one step — cutting the worktree and the
# branch this scenario is about — and the pass is then held on `paused`, where
# it stays until a person resumes it. A state, not a window.
#
# A gate rather than a command step that parks, which was the other way to
# hold this open and is the wrong one: a parked command is *running*, and
# `eval --discard` refuses a trial with work in flight and says to pass
# `--force`. The claim here is the plain discard, so what it needs is an arm
# standing still with nothing running — which is what a gate leaves.
for pipeline_file in .spoolway/pipelines/*.yml; do
  awk '
    { print }
    /^  - id: / && !gated { print "    gate: true"; gated = 1 }
  ' "$pipeline_file" > "$pipeline_file.gated" && mv "$pipeline_file.gated" "$pipeline_file"
done

pending_doc oneoff "$BODY" "group: oneoff"
on_screen 'foneoff\rt\r\rn' /dev/null

works "the one-task trial's arm reaches the queue" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/oneoff-1.md"
SOLO_TRIAL=$(grep '^trial:' "$SPOOLWAY_PROJECT_HOME/queue/oneoff-1.md" | awk '{print $2}')

# Driven as far as the hold and no further: the whole claim of a discard is
# that it disposes of an arm that has *not* settled, so the arm has to still
# be in the queue — with a worktree and a branch of its own already cut — at
# the moment it is discarded. The gate above holds exactly that, and
# `drive_and_hold` stops the dispatcher besides.
if drive_and_hold oneoff-1 paused 120; then ok "the arm is mid-flight, with a worktree cut"
else bad "the arm is mid-flight, with a worktree cut (at \`$(stage_of oneoff-1)\`)"; fi

# Acceptance criterion: an explicitly discarded trial loses its task
# documents, worktrees and local branches — including a branch nothing ever
# pushed, the same exemption settlement makes — and the report names what was
# kept and what was removed.
DISCARD_OUT=$("$SPOOLWAY" eval --discard "$SOLO_TRIAL" 2>&1)
DISCARD_STATUS=$?
if [ "$DISCARD_STATUS" -eq 0 ]; then ok "the discard exits clean"
else bad "the discard exits clean (exit $DISCARD_STATUS)"; echo "$DISCARD_OUT" | sed 's/^/        /'; fi
says "the discard names the trial it threw away" "discarded" \
  bash -c 'printf "%s" "$1"' _ "$DISCARD_OUT"
says "and points back at the same ledger the settlement path reads" \
  "eval --by task --trial $SOLO_TRIAL" \
  bash -c 'printf "%s" "$1"' _ "$DISCARD_OUT"
works "the discarded arm's task is gone" \
  bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/queue/oneoff-1.md"
works "its worktree is gone" \
  bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/worktrees/task-oneoff-1"
works "its local branch is gone, though nothing ever pushed it" \
  bash -c '! git rev-parse --verify -q "task/oneoff-1" >/dev/null'

# Acceptance criterion: the original source group is never modified or removed
# by trial cleanup — a discard reaches further than settlement does, and still
# must not reach this.
works "the source task the trial forked is left where it was" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/oneoff.md"

# A trial id nothing carries is a typo, and removing nothing quietly reads
# exactly like success.
refuses "an unknown trial id is refused rather than silently doing nothing" \
  "no trial" "$SPOOLWAY" eval --discard tdeadbeefdeadbeef

finish
