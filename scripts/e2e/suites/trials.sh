#!/usr/bin/env bash
# The queue screen's `p` picker, driven end to end — the one path no unit
# test can drive, since `run_screen` is exercised headlessly in Rust
# already, but never as the whole binary reading real keystrokes off a real
# pipe. `p` forks a whole group: one pipeline assigned per task on the first
# popup, one skip set per task on the second, one new arm per source task
# under one freshly minted trial id.
#
# The first half of this suite drives no dispatcher: a trial's whole setup job
# is landing arms in the queue directory with the right `pipeline:`, `skip:`,
# `group:` and `trial:` on them, and that is what the queue-screen block below
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
# unit `p` now forks whole. `spoolway init` writes exactly the two built-in
# pipelines, `default` and `bugfix`, under `.spoolway/pipelines/`, and
# `trial_pipeline_names` walks them in the same alphabetical order
# (`bugfix`, `default`) the assign screen's `←`/`→` cycles through.
BODY="$LIVE/body.md"
task_body "$BODY"
# A bare `pipeline:` line leaves each document genuinely unassigned — see
# `task_doc`'s own doc comment — which is the whole point of this suite's
# picker cases below.
pending_doc alpha "$BODY" "group: audits" "touches: [src/main.rs]" "pipeline:"
pending_doc beta "$BODY" "group: audits" "touches: [src/main.rs]" "pipeline:" \
  "depends_on: [alpha]"

# `Tab` focuses the tasks pane on a task inside the group (`alpha`, first in
# reading order since it is the dependency), proving `p` reaches the whole
# group from a selected task too, not only from the groups pane. `p` opens
# the assign-pipelines popup with every task unassigned — there is no
# project default to seed it with any more, so `enter` on this screen
# refuses to advance until every task has one. `←` on `alpha` lands on
# `bugfix`, the pipeline that sorts first, since there is no current
# position to cycle away from yet; two `→`s on `beta` land it on `default`
# instead — one press to the same first-sorting `bugfix`, a second to cycle
# past it. `enter` then advances to the skips screen. There the flattened
# cursor opens on `alpha`'s first checkbox: one `j` reaches its second,
# `fix`, and `space` ticks it; nine more `j`s walk past the rest of
# `alpha`'s own checkboxes onto `beta`'s third one, `document`, and `space`
# ticks that too. `enter` mints and writes both arms; `n` declines the
# dispatcher offer.
printf '\tp\x1b[Dj\x1b[C\x1b[C\rj jjjjjjjjj \rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the alpha arm reaches the queue" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
works "and the beta arm beside it" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"
works "two distinct minted ids — never either document's own bare one" \
  bash -c '[ ! -e "$1/queue/alpha.md" ] && [ ! -e "$1/queue/beta.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

has "alpha's arm carries the pipeline cycled onto it" "pipeline: bugfix" \
  "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
has "beta's arm carries the pipeline explicitly cycled onto it too" \
  "pipeline: default" "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"
works "alpha's own ticked skip, and none of beta's" \
  bash -c 'grep -A1 "^skip:" "$1" | tail -1 | grep -qxF -- "- fix"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
works "beta's own ticked skip, and none of alpha's" \
  bash -c 'grep -A1 "^skip:" "$1" | tail -1 | grep -qxF -- "- document"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"
has "both arms keep the document's own group" "group: audits" \
  "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
has "on both arms" "group: audits" \
  "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"

works "the shared trial id is freshly minted, t plus sixteen hex" \
  bash -c 'grep -qE "^trial: t[0-9a-f]{16}$" "$1"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
works "and the same id lands on both arms of the one launch" \
  bash -c '
    a=$(grep "^trial:" "$1/queue/alpha-1.md")
    b=$(grep "^trial:" "$1/queue/beta-1.md")
    [ -n "$a" ] && [ "$a" = "$b" ]
  ' _ "$SPOOLWAY_PROJECT_HOME"

has "beta's depends_on is remapped onto alpha's own minted sibling id" \
  "alpha-1" "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"
works "not left naming the bare id nothing in this batch is queued under" \
  bash -c '! grep -qxF -- "- alpha" "$1"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"

# A trial forks the documents; it does not submit them. The pending copies
# are templates the picker read from, not a batch the screen queued and
# cleared.
works "both source documents are left exactly where they were" \
  bash -c 'test -f "$1/pending/alpha.md" && test -f "$1/pending/beta.md"' \
  _ "$SPOOLWAY_PROJECT_HOME"

# A second template, this one already run through a pipeline once: its sole
# document lives in the queue directory, carrying every key spoolway stamped
# on that run — `stage:` chief among them, which `parse_submission` refuses
# outright. `list_groups` reads such a group's task straight out of `queue/`,
# verbatim, so forking it is `p` over exactly the shape a task queued or
# archived earlier has. Named clear of `p` and `s` on purpose: both are
# reserved actions the instant a filter is open, so either letter inside the
# query itself would fire the action early rather than narrow the list —
# `f` narrows to it by name, `p` reaches it straight from the groups pane,
# with no `Tab` needed. Two `→`s land it on `default` — one press to
# `bugfix`, the pipeline that sorts first with nothing assigned yet, a
# second past it — since `enter` refuses to advance with it still
# unassigned; `enter` then advances past the assign screen, and `enter`
# again launches with nothing ticked to skip.
task_doc "$SPOOLWAY_PROJECT_HOME/queue/old-run.md" old-run "$BODY" \
  "group: old-run" \
  "touches: [src/main.rs]" \
  "stage: done" \
  "run: r00000000000000af" \
  "branch: task/old-run" \
  "base: master" \
  "cut_from: master" \
  "base_commit: 0000000000000000000000000000000000000000" \
  "attempts: 2" \
  "pipeline:"

printf 'fold-run\rp\x1b[C\x1b[C\r\rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the reset lets a stamped document reach \`finish_trial\` at all" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"
has "under the pipeline cycled onto it" "pipeline: default" \
  "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"
lacks "the reset held: no stamped stage on the forked arm" "stage: done" \
  "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"
lacks "nor the run id the earlier run minted" "run: r00000000000000af" \
  "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"

# The source document goes now that the arm is forked off it, and it has to:
# `p` leaves a source exactly where it found it, and this one was written
# straight into `queue/` with no `pipeline:` on purpose. A routeless document
# in the live queue is precisely what `check_task_routes` refuses a whole
# start over — correctly — so leaving it here would refuse every dispatcher
# the rest of this suite starts, and the runtime half below would assert on
# tasks nothing ever moved. The same disposal `commands.sh` does for its own
# routeless fixture, for the same reason.
rm -f "$SPOOLWAY_PROJECT_HOME/queue/old-run.md"

# ------------------------------------------------------- dispatch and cleanup
# Everything above only ever wrote queue files. From here a real dispatcher
# runs two trial arms and one ordinary task through the same `default`
# pipeline, against a real (if local) forge and a real `[issue_tracking]`
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
# event — the same double `commands.sh`'s own `[issue_tracking]` block uses,
# so a hook that never fired leaves no file at all rather than an empty one.
mkdir -p .spoolway/hooks
cat > .spoolway/hooks/record.sh <<'EOF'
#!/bin/sh
env | sort > "$SPOOLWAY_TASK_FILE.env.$SPOOLWAY_EVENT"
exit 0
EOF
chmod +x .spoolway/hooks/record.sh
must "the hook is named" "$SPOOLWAY" config set issue_tracking.hook record.sh
must "and a project key" "$SPOOLWAY" config set issue_tracking.project_key acme/app

# `alpha-1` and `beta-1` are exactly the two-arm trial the queue-screen block
# above already minted — a real trial id, shared, on the same `group:
# audits` — so there is no need to hand-write one: `trial:` is a key
# `queue_add::parse_submission` refuses on any document, precisely because it
# is spoolway's own to mint, never a person's or a script's to set. `old-run-1`
# is a trial of one and is left queued rather than driven, so it dispatches in
# the background the same as a person's own queue would, without this suite
# waiting on or asserting over a third arm.
#
# One ordinary task beside them, queued the everyday way and never touched by
# `p` — same pipeline, same shape of work, so the only thing that can explain
# a difference in what happens to it and to `alpha-1`/`beta-1` is trial mode
# itself.
TRIAL_ID=$(grep '^trial:' "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md" | awk '{print $2}')
task_doc "$LIVE/control.md" control "$BODY" "group: control-live" \
  "touches: [notes/control.md]" "pipeline: default" \
  "group_description: an ordinary control task beside the trial arms"
must "control queues" "$SPOOLWAY" queue add --from "$LIVE/control.md"

if drive control gone 60; then ok "the ordinary control task runs the pipeline to done"
else bad "the ordinary control task runs the pipeline to done (stuck at \`$(stage_of control)\`)"; fi
if drive alpha-1 gone 60 && drive beta-1 gone 60; then
  ok "both trial arms run their assigned pipelines to done"
else
  bad "both trial arms run their assigned pipelines to done (at \`$(stage_of alpha-1)\`/\`$(stage_of beta-1)\`)"
fi

# Acceptance criterion: trial dispatch suppresses the queued/done issue
# hooks — the control's own env files below are the proof the hook mechanism
# itself works in this run at all, so a trial arm's missing files are absence
# of the event, not absence of the hook.
has "the control's queued event reached the hook" "SPOOLWAY_EVENT=queued" \
  "$SPOOLWAY_PROJECT_HOME/queue/control.md.env.queued"
has "and its done event too" "SPOOLWAY_EVENT=done" \
  "$SPOOLWAY_PROJECT_HOME/queue/control.md.env.done"
for arm in alpha-1 beta-1; do
  works "$arm's queued event never reached the hook" \
    bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/queue/$arm.md.env.queued"
  works "$arm's done event never reached the hook either" \
    bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/queue/$arm.md.env.done"
done

# Acceptance criterion: trial dispatch makes spoolway-owned publishing a
# no-op — the control opens a real pull request on the same forge, so a
# trial arm's missing one is the boundary working, not a forge that never
# came up.
if handed_over control; then ok "the control is handed over as a pushed branch and a pull request"
else bad "the control is handed over as a pushed branch and a pull request"; fi
for arm in alpha-1 beta-1; do
  works "$arm's branch was never pushed to the forge" \
    bash -c '! git ls-remote --exit-code --heads origin "task/$1" >/dev/null 2>&1' _ "$arm"
  works "$arm never opened a pull request" \
    bash -c '! grep -lx "head=task/$1" "$FORGE"/prs/[0-9]* >/dev/null 2>&1' _ "$arm"
done

# Acceptance criterion: once every task in a trial settles, its documents,
# worktrees and local branches are removed — the control's own archive
# document, worktree and branch survive exactly as before, so the trial
# arms' disappearance is the trial boundary and not ordinary teardown acting
# on everyone alike.
has "the control keeps its archive document, the durable record cleanup means" \
  "id: control" "$SPOOLWAY_PROJECT_HOME/archive/control.md"
for arm in alpha-1 beta-1; do
  works "$arm's archive document is gone, not merely queued for retention" \
    bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/archive/$arm.md"
  works "$arm's local branch is gone" \
    bash -c '! git rev-parse --verify -q "task/$1" >/dev/null' _ "$arm"
  works "$arm's worktree is gone" \
    bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/worktrees/task-$arm"
done

# Acceptance criterion: usage rows remain correlated by trial id — the one
# thing a settled trial leaves standing, which is what makes `spoolway eval
# --runs --trial <id>` still answerable after every disposable copy is gone.
TRIAL_ROWS=$(grep -c "\"trial\":\"$TRIAL_ID\"" "$SPOOLWAY_PROJECT_HOME/usage.jsonl" 2>/dev/null || true)
works "both arms' usage rows are still in the ledger, correlated by trial id" \
  bash -c '[ "$1" -ge 2 ]' _ "${TRIAL_ROWS:-0}"

# ------------------------------------------------------------ explicit discard
# A trial has two cleanup triggers, and everything above only proves the first
# one: settlement, which happens by itself once the last arm reaches `done`.
# This is the other one — a person who has seen enough and wants the arms gone
# now, mid-flight, with nothing waiting on them to finish.
#
# A trial of its own group, so the discard below has nothing in common with
# the two arms already settled. `p`'s minimal form: `f` narrows to the group
# by name, `p` opens the picker, two `enter`s take both screens' defaults,
# `n` declines the dispatcher offer. The group is named clear of `p` and `s`
# for the reason the `old-run` block above gives: either letter inside the
# query fires its own action rather than narrowing.
pending_doc oneoff "$BODY" "group: oneoff" "touches: [notes/oneoff.md]"
printf 'foneoff\rp\r\rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the one-task trial's arm reaches the queue" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/oneoff-1.md"
SOLO_TRIAL=$(grep '^trial:' "$SPOOLWAY_PROJECT_HOME/queue/oneoff-1.md" | awk '{print $2}')

# Driven partway rather than to `done`, and then held: the whole claim of a
# discard is that it disposes of an arm that has *not* settled, so the arm has
# to still be in the queue — with a worktree and a branch of its own already
# cut — at the moment it is discarded. `drive_and_hold` stops the dispatcher,
# so nothing carries `oneoff-1` further between here and the assertions.
if drive_and_hold oneoff-1 review 60; then ok "the arm is mid-flight, with a worktree cut"
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
  "eval --runs --trial $SOLO_TRIAL" \
  bash -c 'printf "%s" "$1"' _ "$DISCARD_OUT"
works "the discarded arm's task document is gone" \
  bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/queue/oneoff-1.md"
works "its worktree is gone" \
  bash -c '[ ! -e "$1" ]' _ "$SPOOLWAY_PROJECT_HOME/worktrees/task-oneoff-1"
works "its local branch is gone, though nothing ever pushed it" \
  bash -c '! git rev-parse --verify -q "task/oneoff-1" >/dev/null'

# Acceptance criterion: the original source group is never modified or removed
# by trial cleanup — a discard reaches further than settlement does, and still
# must not reach this.
works "the source document the trial forked is left where it was" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/oneoff.md"

# A trial id nothing carries is a typo, and removing nothing quietly reads
# exactly like success.
refuses "an unknown trial id is refused rather than silently doing nothing" \
  "no trial" "$SPOOLWAY" eval --discard tdeadbeefdeadbeef

finish
