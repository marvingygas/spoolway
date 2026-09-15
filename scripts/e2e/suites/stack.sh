#!/usr/bin/env bash
# `spoolway stack`, against a bare repository and no real forge.
#
# Every other suite that reaches `handover` drives it through a full dispatch
# pass, with a mock agent standing in for whatever prompt the step names.
# `handover` names none any more — it runs `spoolway stack` — so this suite
# calls the command directly, in a worktree it cuts by hand, and checks the
# git-and-body mechanics `spoolway stack` owns: the squash to one commit, a
# lease-refused push classified as such, the pull request body taken verbatim
# from the task file, the trailer's contents, and the empty-diff refusal.
#
# `gh` is the one thing no local suite can reach for real, so it runs behind
# `SPOOLWAY_GH` here — `scripts/e2e/gh-stub.sh`, a double built for exactly the
# calls `spoolway stack` makes (`pr view`, `pr create --body-file`, `api …
# stacks`), not the fuller `e2e-fake-gh.sh` every dispatch-driven suite shares.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"

LIVE=${WORK:-$(mktemp -d)}
new_repo "$LIVE/proj" main
must "spoolway init" "$SPOOLWAY" init
project_home_after_init
must "the spoolway commit" git add -A
must "the spoolway commit" git commit -qm "spoolway"

# ------------------------------------------------------------- the bare origin
# Addressed as a github.com URL and redirected to the local bare repo with
# `insteadOf` — `owner_repo()` reads the literal remote URL to name the stack
# API's `{owner}/{repo}`, and every actual git operation still goes to disk.
ORIGIN="$LIVE/origin.git"
must "the bare origin" git init -q --bare -b main "$ORIGIN"
must "the remote, addressed as github" \
  git remote add origin "https://github.com/e2e/spoolway.git"
must "redirected to the local bare repo" \
  git config "url.$ORIGIN.insteadOf" "https://github.com/e2e/spoolway.git"
must "the first push" git push -q -u origin main

export GH_STUB_PRS="$LIVE/prs"
export GH_STUB_URL="file://$LIVE/forge"
export SPOOLWAY_GH="$HERE/../gh-stub.sh"

WORKTREES="$LIVE/worktrees"
mkdir -p "$WORKTREES"

# queue_task <id> [extra frontmatter lines…]
#
# A task file with exactly the fields a cut worktree carries — written by
# hand, since this suite never runs a dispatch pass to cut one for real.
queue_task() {
  local id=$1; shift
  mkdir -p "$SPOOLWAY_PROJECT_HOME/queue"
  {
    echo "---"
    echo "id: $id"
    echo "title: $id, a change of its own"
    echo "stage: handover"
    for line in "$@"; do echo "$line"; done
    echo "---"
    printf '## Goal\n\nAdd `notes/%s.md`.\n\n## Non-goals\n\nOut of scope.\n\n## Acceptance criteria\n\n- `notes/%s.md` exists.\n' \
      "$id" "$id"
  } > "$SPOOLWAY_PROJECT_HOME/queue/$id.md"
}

# --------------------------------------------------------- the bottom: `base`
must "the branch, off main" git branch task/base main
must "the worktree" git worktree add -q "$WORKTREES/base" task/base
(
  cd "$WORKTREES/base" || exit 1
  mkdir -p notes
  echo "# base" > notes/base.md
  git add -A
  git commit -qm "wip(base): implement"
)
queue_task base "touches: [notes/base.md]" "base: main" "branch: task/base"

out_base=$(cd "$WORKTREES/base" && "$SPOOLWAY" stack base 2>&1)
if [ $? -eq 0 ]; then ok "\`spoolway stack\` exits 0 for the foot of the stack"
else bad "\`spoolway stack\` exits 0 for the foot of the stack"; sed 's/^/        /' <<<"$out_base"; fi

if [ "$(cd "$WORKTREES/base" && git rev-list --count main..HEAD)" = 1 ]; then
  ok "the branch is squashed to one commit"
else
  bad "the branch is squashed to one commit"
  (cd "$WORKTREES/base" && git log --oneline main..HEAD) | sed 's/^/        /'
fi
says "the squashed commit's subject is the task's title, verbatim" \
  "base, a change of its own" \
  git -C "$WORKTREES/base" log -1 --format=%s

base_pr=$(grep -l '^head=task/base$' "$LIVE/prs"/[0-9]* 2>/dev/null | head -1)
if [ -n "$base_pr" ]; then
  ok "a pull request was opened for it"
  base_body="${base_pr}.body"
  if grep -qF '`notes/base.md` exists.' "$base_body" 2>/dev/null; then
    ok "its body carries the task file's own acceptance criteria, verbatim"
  else
    bad "its body carries the task file's own acceptance criteria, verbatim"
    sed 's/^/        /' "$base_body" 2>/dev/null
  fi
  if grep -qF 'Co-Authored-By: Claude Code' "$base_body" 2>/dev/null; then
    ok "the trailer closes with the co-author tag"
  else
    bad "the trailer closes with the co-author tag"
    sed 's/^/        /' "$base_body" 2>/dev/null
  fi
  if grep -q '@' "$base_body" 2>/dev/null; then
    bad "and names no email address anywhere"
    sed 's/^/        /' "$base_body" 2>/dev/null
  else
    ok "and names no email address anywhere"
  fi
else
  bad "a pull request was opened for it"
fi

# ------------------------------------------------------------ the top: `top`
must "the branch, off task/base" git branch task/top task/base
must "the worktree" git worktree add -q "$WORKTREES/top" task/top
(
  cd "$WORKTREES/top" || exit 1
  mkdir -p notes
  echo "# top" > notes/top.md
  git add -A
  git commit -qm "wip(top): implement"
)
queue_task top "touches: [notes/top.md]" "depends_on: [base]" \
  "base: main" "cut_from: task/base" "branch: task/top"

out_top=$(cd "$WORKTREES/top" && "$SPOOLWAY" stack top 2>&1)
if [ $? -eq 0 ]; then ok "and for the task stacked on top of it"
else bad "and for the task stacked on top of it"; sed 's/^/        /' <<<"$out_top"; fi

top_pr=$(grep -l '^head=task/top$' "$LIVE/prs"/[0-9]* 2>/dev/null | head -1)
if [ -n "$top_pr" ] && [ "$(sed -n 's/^base=//p' "$top_pr")" = task/base ]; then
  ok "its pull request targets the branch it was cut from, not \`main\`"
else
  bad "its pull request targets the branch it was cut from, not \`main\`"
  [ -n "$top_pr" ] && sed 's/^/        /' "$top_pr"
fi

# --------------------------------------- the trailer's own two extra lines
# `edge` changes a file its `touches` never names, and `rival` — a sibling
# `parallel: true` task nothing here ever runs `stack` for — sits on a branch
# that touches the very same file differently, off the same base. Neither is
# a reason to refuse the pull request; both are the trailer's business, so
# what is checked is that they reach the body rather than that anything stops.
must "rival's branch, off main" git branch task/rival main
must "its worktree" git worktree add -q "$WORKTREES/rival" task/rival
(
  cd "$WORKTREES/rival" || exit 1
  mkdir -p notes
  echo "# edge, rival's own idea" > notes/edge.md
  git add -A
  git commit -qm "wip(rival): implement"
)
queue_task rival "touches: [notes/edge.md]" "parallel: true" \
  "base: main" "branch: task/rival"

must "edge's branch, off main" git branch task/edge main
must "its worktree" git worktree add -q "$WORKTREES/edge" task/edge
(
  cd "$WORKTREES/edge" || exit 1
  mkdir -p notes
  echo "# edge" > notes/edge.md
  echo "not in touches" > notes/undeclared.md
  git add -A
  git commit -qm "wip(edge): implement"
)
queue_task edge "touches: [notes/edge.md]" "base: main" "branch: task/edge"

edge_out=$(cd "$WORKTREES/edge" && "$SPOOLWAY" stack edge 2>&1)
if [ $? -eq 0 ]; then ok "and for a task with an undeclared file and a conflicting sibling"
else bad "and for a task with an undeclared file and a conflicting sibling"; sed 's/^/        /' <<<"$edge_out"; fi

edge_pr=$(grep -l '^head=task/edge$' "$LIVE/prs"/[0-9]* 2>/dev/null | head -1)
if [ -n "$edge_pr" ] \
   && grep -qF "Changed 1 file it did not declare in \`touches\`:" "${edge_pr}.body" 2>/dev/null \
   && grep -qF "  notes/undeclared.md" "${edge_pr}.body" 2>/dev/null; then
  ok "the trailer names the file it changed outside \`touches\`"
else
  bad "the trailer names the file it changed outside \`touches\`"
  [ -n "$edge_pr" ] && sed 's/^/        /' "${edge_pr}.body"
fi
if [ -n "$edge_pr" ] \
   && grep -qF "Will conflict with \`rival\`, which is not ordered against this task." "${edge_pr}.body" 2>/dev/null; then
  ok "and the parallel sibling it will conflict with"
else
  bad "and the parallel sibling it will conflict with"
  [ -n "$edge_pr" ] && sed 's/^/        /' "${edge_pr}.body"
fi

# --------------------------------------------------- empty diff, off `top`
must "a third branch, sitting exactly on top's tip" \
  git branch task/same task/top
must "its worktree" git worktree add -q "$WORKTREES/same" task/same
queue_task same "touches: [notes/top.md]" "depends_on: [top]" \
  "base: main" "cut_from: task/top" "branch: task/same"

same_out=$(cd "$WORKTREES/same" && "$SPOOLWAY" stack same 2>&1)
same_status=$?
if [ "$same_status" -ne 0 ] && grep -qi "three-dot diff is empty" <<<"$same_out"; then
  ok "a branch with nothing beyond its cut point refuses to open a pull request"
else
  bad "a branch with nothing beyond its cut point refuses to open a pull request"
  printf '        exit %s: %s\n' "$same_status" "$same_out"
fi

# ----------------------------------------------- a lease refused by a mover
# Somebody else pushes to `task/top` from a second clone — this worktree never
# fetches that branch itself, only the branch it is cut from, so its
# remote-tracking ref for `task/top` stays exactly where the first push above
# left it.
must "a second clone of the origin" git clone -q "$ORIGIN" "$LIVE/elsewhere"
(
  cd "$LIVE/elsewhere" || exit 1
  git checkout -q task/top
  echo "someone else was here" >> notes/top.md
  git add -A
  git -c user.email=other@example.invalid -c user.name=other commit -qm "a push from elsewhere"
  git push -q origin task/top
)

(
  cd "$WORKTREES/top" || exit 1
  echo "a second local commit" >> notes/top.md
  git add -A
  git commit -qm "wip(top): fix"
)
lease_out=$(cd "$WORKTREES/top" && "$SPOOLWAY" stack top 2>&1)
lease_status=$?
if [ "$lease_status" -ne 0 ] && grep -qi "the remote moved" <<<"$lease_out"; then
  ok "a push the remote moved out from under is classified as such"
else
  bad "a push the remote moved out from under is classified as such"
  printf '        exit %s: %s\n' "$lease_status" "$lease_out"
fi
if ! grep -qi "^spoolway: .git push .force-with-lease. failed" <<<"$lease_out"; then
  ok "and not reported as a bare, unclassified git failure"
else
  bad "and not reported as a bare, unclassified git failure"
fi

# ----------------------------------- a rejecting commit-msg hook cannot break it
# The squash is built with `git commit-tree`, which runs no hooks, and the
# branch ref moves only once that object exists. So a `commit-msg` hook that
# rejects every message can neither stop the squash nor leave the branch
# collapsed onto its merge base with the work stranded in the index and the
# reflog (finding 55).
must "the branch, off main" git branch task/hooked main
must "the worktree" git worktree add -q "$WORKTREES/hooked" task/hooked
(
  cd "$WORKTREES/hooked" || exit 1
  mkdir -p notes
  echo "# hooked" > notes/hooked.md
  git add -A
  git commit -qm "wip(hooked): implement"
  echo "second commit" > notes/hooked-two.md
  git add -A
  git commit -qm "wip(hooked): fix"
)
queue_task hooked "touches: [notes/hooked.md, notes/hooked-two.md]" \
  "base: main" "branch: task/hooked"

HOOKDIR=$(cd "$WORKTREES/hooked" && git rev-parse --git-path hooks)
mkdir -p "$HOOKDIR"
printf '#!/bin/sh\necho "commit-msg hook says no" >&2\nexit 1\n' > "$HOOKDIR/commit-msg"
chmod +x "$HOOKDIR/commit-msg"

hooked_out=$(cd "$WORKTREES/hooked" && "$SPOOLWAY" stack hooked 2>&1)
hooked_status=$?
rm -f "$HOOKDIR/commit-msg"
if [ "$hooked_status" -eq 0 ] \
   && [ "$(cd "$WORKTREES/hooked" && git rev-list --count main..HEAD)" = 1 ]; then
  ok "a rejecting commit-msg hook neither stops the squash nor leaves the branch collapsed"
else
  bad "a rejecting commit-msg hook neither stops the squash nor leaves the branch collapsed"
  printf '        exit %s: %s\n' "$hooked_status" "$hooked_out"
  git -C "$WORKTREES/hooked" log --oneline main..HEAD | sed 's/^/        /'
fi
if [ -f "$WORKTREES/hooked/notes/hooked.md" ] \
   && [ -f "$WORKTREES/hooked/notes/hooked-two.md" ] \
   && git -C "$WORKTREES/hooked" diff --quiet; then
  ok "the squashed commit carries every changed file and leaves a clean tree"
else
  bad "the squashed commit carries every changed file and leaves a clean tree"
  git -C "$WORKTREES/hooked" status --porcelain | sed 's/^/        /'
fi

# ------------------------------------------- a task may not name its own branch
# `branch:` is spoolway's field outright. A task document that sets it to
# anything spoolway would not have stamped itself — `task/<id>`, or the
# `task/<slug>-<id>` form `issue_tracking.key_in_names` prefixes — is refused
# when the task file is loaded, well before `spoolway stack` could force-push
# a squashed commit onto it or hand a `-`-led value to `gh pr view`
# (findings 11, 12). `branch: main` here is neither shape.
main_local_before=$(git rev-parse main)
main_remote_before=$(git rev-parse origin/main)
queue_task claimed "touches: [notes/claimed.md]" "base: main" "branch: main"
claimed_out=$(cd "$WORKTREES/base" && "$SPOOLWAY" stack claimed 2>&1)
claimed_status=$?
if [ "$claimed_status" -ne 0 ] && grep -qF "spoolway owns that field" <<<"$claimed_out"; then
  ok "a task whose \`branch:\` is a shape spoolway would not stamp is refused at load"
else
  bad "a task whose \`branch:\` is a shape spoolway would not stamp is refused at load"
  printf '        exit %s: %s\n' "$claimed_status" "$claimed_out"
fi
if [ "$(git rev-parse main)" = "$main_local_before" ] \
   && [ "$(git rev-parse origin/main)" = "$main_remote_before" ]; then
  ok "and nothing was force-pushed onto \`main\`"
else
  bad "and nothing was force-pushed onto \`main\`"
fi

finish
