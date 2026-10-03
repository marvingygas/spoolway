#!/usr/bin/env bash
# A base branch that only `origin` has.
#
# Cleanup deletes a finished task's local branch once it is pushed, so a
# pending pull request's branch — the ordinary shape of a base for the next
# task in a stack — is usually on `origin` only. `queue add --from` used to
# refuse it outright: `check_task_base` looked at `refs/heads/*` alone. It now
# also asks `git ls-remote`, so a batch naming one queues, and the dispatcher
# cuts the new task's worktree straight from `origin/<base>` with no local
# branch made for it — see `resolve_cut_base` in `src/mux.rs`.
#
# The forge every suite already builds is enough to prove this without any
# scaffold of its own: `new_forge` gives a bare repo on disk as `origin`, and
# a branch made directly in it, never fetched by this project's own checkout,
# is exactly a base only `origin` has.
#
# The last case is the opposite: a start branch that exists nowhere. A
# dependency's pull request merges, GitHub deletes its branch, and the
# dependent — queued while the branch was still there — has nothing to be cut
# from. It pauses with one line saying so, and starts from `main` once the
# person writes `starts_from: main` into its front matter and resumes it.
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
configure_project plan/live
publish plan/live

# A branch made straight in the bare origin, on top of what `publish` already
# pushed there — never fetched into this checkout, so `refs/heads/…` and
# `refs/remotes/origin/…` both come back empty for it here. That is the whole
# point: `git ls-remote` has to answer this honestly with no local copy at all.
REMOTE_ONLY=task/gh-412-checkout
must "a branch made only in the bare origin, never fetched here" \
  git -C "$FORGE/origin.git" branch "$REMOTE_ONLY" main

no_local_copy() {
  ! git rev-parse --verify --quiet "refs/heads/$REMOTE_ONLY" >/dev/null 2>&1
}
no_remote_tracking_ref() {
  ! git rev-parse --verify --quiet "refs/remotes/origin/$REMOTE_ONLY" >/dev/null 2>&1
}

works "this checkout really has no local copy of it" no_local_copy
works "and no remote-tracking ref for it either — nothing here has fetched it" \
  no_remote_tracking_ref

BODY="$LIVE/body.md"
task_body "$BODY"

# A stand-in's turn is over in milliseconds, so a lane left to run carries the
# task through every step and into the archive between two of `drive`'s polls
# — it never sees `implement`, and the queue file the checks below read is
# gone. Hung mid-turn, the task sits on `implement` with its worktree cut.
echo hang > "$CTL/stacked"

task_doc "$LIVE/stacked.md" stacked "$BODY" "group: live" \
  "base: $REMOTE_ONLY"
must "a task based on a branch only origin has queues" \
  "$SPOOLWAY" queue add --from "$LIVE/stacked.md"

if drive stacked implement 60; then ok "its worktree is cut and the lane starts"
else bad "its worktree is cut and the lane starts (at \`$(stage_of stacked)\`)"; fi

STACKED_WORKTREE=$(worktree_of stacked)
works "the worktree really exists" test -d "$STACKED_WORKTREE"
says "the task file records the remote-only branch as starts_from" \
  "starts_from: $REMOTE_ONLY" cat "$SPOOLWAY_PROJECT_HOME/queue/stacked.md"

works "no local branch was made for the remote-only base by the cut" no_local_copy

# A finished dependency whose branch is gone from here and from `origin`: its
# task file is archived and still names `task/gh-413-merged`, which nothing
# has. The dispatcher would have failed the cut three times over it.
mkdir -p "$SPOOLWAY_PROJECT_HOME/archive"
cat > "$SPOOLWAY_PROJECT_HOME/archive/merged.md" <<'DOC'
---
id: merged
title: merged, its branch deleted
stage: done
group: late
base: plan/live
branch: task/gh-413-merged
---
DOC
echo hang > "$CTL/late"
task_doc "$LIVE/late.md" late "$BODY" "group: late" "depends_on: [merged]" \
  "base: plan/live"
must "a dependent of a finished task queues" \
  "$SPOOLWAY" queue add --from "$LIVE/late.md"

if drive late paused 60; then
  ok "a queued task whose start branch was deleted pauses instead of failing to start"
else
  bad "a queued task whose start branch was deleted pauses instead of failing to start (at \`$(stage_of late)\`)"
fi
says "and the task file names the branch that is missing" \
  "missing_start_branch: task/gh-413-merged" cat "$SPOOLWAY_PROJECT_HOME/queue/late.md"
works "and no worktree was cut for it" test -z "$(worktree_of late)"
says "and the dispatch log says what to set" \
  'it starts from `task/gh-413-merged`, which doesn'"'"'t exist. Set `starts_from:` in the task front matter and resume.' \
  cat "$E2E_DISPATCH_LOG"

# The person's fix: name a branch that exists, then resume.
sed -i 's/^base: plan\/live$/&\nstarts_from: main/' "$SPOOLWAY_PROJECT_HOME/queue/late.md"
says "resume puts it back on queued" "late: -> queued" "$SPOOLWAY" resume late

if drive late implement 60; then ok "and it starts, from the branch the person named"
else bad "and it starts, from the branch the person named (at \`$(stage_of late)\`)"; fi
says "the task file records \`main\` as starts_from" \
  "starts_from: main" cat "$SPOOLWAY_PROJECT_HOME/queue/late.md"
lacks "and no longer carries the missing branch" \
  "missing_start_branch" "$SPOOLWAY_PROJECT_HOME/queue/late.md"

finish
