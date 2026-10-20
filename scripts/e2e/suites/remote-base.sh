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
# The last two cases are the opposite: a start branch that exists nowhere. A
# dependency's pull request merges, GitHub deletes its branch, and the
# dependent — queued while the branch was still there — has nothing to be cut
# from. It pauses with one line saying so, and starts from `main` once the
# person writes `starts_from: main` into its front matter and resumes it. A
# dependent queued after the branch is gone is refused by `queue add` instead,
# with the same advice, and queues once `starts_from: main` is written.
#
# The last case is a fetch that hangs. The cut of a remote-only base runs
# `git fetch origin <base>` inside the dispatcher's pass, so a transport stuck
# on a prompt used to hold every other task back with nothing printed. The
# fetch is now given up on after a bound, and an unrelated task still starts.
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

# A pull request recorded in the sandbox forge for a dependency's branch. A
# finished dependency that GitHub knows no pull request for was closed with
# nothing to hand over, and its dependents start from its own base instead
# (`Repo::closed_dependency_base`). These two dependencies did open one, so
# the forge has to say so, or the dependent never looks for the deleted
# branch at all. Numbered well past anything `pr create` assigns here.
record_pr() {
  local n=$1 head=$2 state=$3
  printf 'number=%s\nbase=plan/live\nhead=%s\ntitle=%s\nstate=%s\n' \
    "$n" "$head" "$head" "$state" > "$FORGE/prs/$n"
  echo "a pull request recorded for the sandbox forge" > "$FORGE/prs/$n.body"
}

# A finished dependency whose pull request is still open: its task file is
# archived and names `task/gh-413-merged`, which `origin` still has. The
# dispatcher is stopped first, so nothing cuts the dependent's worktree
# between queueing it and the branch going.
dispatcher_stop
must "the dependency's branch, on origin while its pull request is open" \
  git -C "$FORGE/origin.git" branch task/gh-413-merged main
record_pr 9413 task/gh-413-merged OPEN
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

# The pull request merges and GitHub deletes the branch, before the
# dependent starts. The dispatcher would have failed the cut three times
# over it.
must "the dependency's branch is deleted on origin" \
  git -C "$FORGE/origin.git" branch -D task/gh-413-merged
sed -i 's/^state=OPEN$/state=MERGED/' "$FORGE/prs/9413"

if drive late paused 60; then
  ok "a queued task whose start branch was deleted pauses instead of failing to start"
else
  bad "a queued task whose start branch was deleted pauses instead of failing to start (at \`$(stage_of late)\`)"
fi
says "and the task file names the branch that is missing" \
  "missing_start_branch: task/gh-413-merged" cat "$SPOOLWAY_PROJECT_HOME/queue/late.md"
works "and no worktree was cut for it" test -z "$(worktree_of late)"
# The pause is saved before the dispatcher prints its line, so the stage can
# read `paused` a moment before the log has the sentence. Wait for it.
poll_until 10 grep -qF 'task/gh-413-merged`, which doesn'"'"'t exist' "$E2E_DISPATCH_LOG"
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

# A dependent sent after the branch is gone: `shipped` finished, its pull
# request merged, and its branch `task/gh-414-shipped` was deleted on
# `origin` — this checkout never had a copy. `queue add` refuses `after` with
# one sentence, and nothing is queued. The dispatcher is stopped so the
# accepted task below is not carried off before the suite ends.
dispatcher_stop
must "the finished dependency's branch, on origin while its pull request is open" \
  git -C "$FORGE/origin.git" branch task/gh-414-shipped main
must "its pull request merges, and the branch is deleted on origin" \
  git -C "$FORGE/origin.git" branch -D task/gh-414-shipped
record_pr 9414 task/gh-414-shipped MERGED
works "this checkout has no copy of it either" \
  test -z "$(git branch --list task/gh-414-shipped)"
cat > "$SPOOLWAY_PROJECT_HOME/archive/shipped.md" <<'DOC'
---
id: shipped
title: shipped, its branch deleted
stage: done
group: shipped
base: plan/live
branch: task/gh-414-shipped
---
DOC
task_doc "$LIVE/after.md" after "$BODY" "group: after" "depends_on: [shipped]" \
  "base: plan/live"
refuses "queue add refuses a dependent whose start branch was deleted" \
  "after starts from task/gh-414-shipped, which doesn't exist. Set starts_from: in the task front matter and requeue." \
  "$SPOOLWAY" queue add --from "$LIVE/after.md"
works "and queues nothing" test ! -e "$SPOOLWAY_PROJECT_HOME/queue/after.md"

# The person's fix: name a branch that exists, then queue it again.
task_doc "$LIVE/after.md" after "$BODY" "group: after" "depends_on: [shipped]" \
  "base: plan/live" "starts_from: main"
says "and queues it once starts_from: main is written" "queued after" \
  "$SPOOLWAY" queue add --from "$LIVE/after.md"
says "the queued task starts from main" \
  "starts_from: main" cat "$SPOOLWAY_PROJECT_HOME/queue/after.md"

# A fetch that hangs. `origin` is pointed at an ssh url whose transport is the
# person's own `core.sshCommand`, a script that serves every call from the
# real bare repo except a fetch, which it holds. Pushes keep going to the real
# bare repo through `pushurl`. Both tasks are queued first, because `queue add`
# itself asks `origin` whether the base exists and is not what this case is
# about. The stuck task's base is an `origin` branch nothing here has fetched,
# so its cut fetches. The other task's base is local, but a task that follows
# its own base is cut from `origin/<base>`, so its cut fetches too. Each fetch
# is bounded, so it must still start once the stuck task's fetches and its own
# have stopped, before the transport would answer.
dispatcher_stop
HUNG_BASE=task/gh-415-hung
must "a branch made only in the bare origin for the stuck fetch" \
  git -C "$FORGE/origin.git" branch "$HUNG_BASE" main
echo hang > "$CTL/stuck"
echo hang > "$CTL/free"
task_doc "$LIVE/stuck.md" stuck "$BODY" "group: stuck" "base: $HUNG_BASE"
task_doc "$LIVE/free.md" free "$BODY" "group: free" "base: plan/live"
must "a task on the remote-only base queues" \
  "$SPOOLWAY" queue add --from "$LIVE/stuck.md"
must "an unrelated task on a local base queues" \
  "$SPOOLWAY" queue add --from "$LIVE/free.md"

ORIGIN_URL=$(git remote get-url origin)
# The transport answers `git ls-remote`, which the dispatcher runs to check a
# start branch exists, by serving the real origin itself. It holds `git fetch`,
# which is the call this case is about, for longer than the fetch's bound, and
# first writes down the process group it is held in, so the cleanup below can
# end that group and nothing else. `ps` rather than /proc, which macOS does
# not have.
cat > "$LIVE/hung-ssh.sh" <<SH
#!/bin/sh
if ps -o args= -p \$PPID | grep -q ' fetch '; then
  ps -o pgid= -p \$\$ | tr -d ' ' > '$LIVE/hung-group'
  sleep 40
fi
exec git-upload-pack '$FORGE/origin.git'
SH
chmod +x "$LIVE/hung-ssh.sh"
must "fetches go to an ssh url" \
  git remote set-url origin ssh://spoolway-e2e.invalid/nowhere.git
must "pushes still go to the real origin" \
  git remote set-url --push origin "$ORIGIN_URL"
must "the person's own ssh command is one that hangs on a fetch" \
  git config core.sshCommand "$LIVE/hung-ssh.sh"

if drive free implement 60; then
  ok "an unrelated task starts while another task's fetch hangs"
else
  bad "an unrelated task starts while another task's fetch hangs (at \`$(stage_of free)\`)"
fi
dispatcher_stop
# The fetch runs in a process group of its own, so stopping the dispatcher
# does not reach one still in flight. That group holds git, the script and its
# `sleep`, and ending it takes them together.
if [ -s "$LIVE/hung-group" ]; then
  kill -TERM -- "-$(cat "$LIVE/hung-group")" 2>/dev/null
fi
git config --unset core.sshCommand
git remote set-url origin "$ORIGIN_URL"

finish
