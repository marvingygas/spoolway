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
configure_project plan/live "$LIVE/worktrees"
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

task_doc "$LIVE/stacked.md" stacked "$BODY" "group: live" \
  "base: $REMOTE_ONLY"
must "a task based on a branch only origin has queues" \
  "$SPOOLWAY" queue add --from "$LIVE/stacked.md"

if drive stacked implement 60; then ok "its worktree is cut and the lane starts"
else bad "its worktree is cut and the lane starts (at \`$(stage_of stacked)\`)"; fi

STACKED_WORKTREE=$(worktree_of stacked)
works "the worktree really exists" test -d "$STACKED_WORKTREE"
says "the task file records the remote-only branch as cut_from" \
  "cut_from: $REMOTE_ONLY" cat "$SPOOLWAY_PROJECT_HOME/queue/stacked.md"

works "no local branch was made for the remote-only base by the cut" no_local_copy

finish
