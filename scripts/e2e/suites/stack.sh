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
must "spoolway init" "$SPOOLWAY" init --yes
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
# `pipeline:` is required now, so this fills in the built-in `default`
# pipeline unless a caller already named its own.
queue_task() {
  local id=$1; shift
  mkdir -p "$SPOOLWAY_PROJECT_HOME/queue"
  local has_pipeline=0
  for line in "$@"; do
    case "$line" in
      pipeline:*) has_pipeline=1 ;;
    esac
  done
  {
    echo "---"
    echo "id: $id"
    echo "title: $id, a change of its own"
    echo "stage: handover"
    for line in "$@"; do echo "$line"; done
    if [ "$has_pipeline" -eq 0 ]; then
      echo "pipeline: default"
    fi
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
queue_task base "base: main" "branch: task/base"

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
queue_task top "depends_on: [base]" \
  "base: main" "starts_from: task/base" "branch: task/top"

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

# --------------------------------- the title, off `ticket:`'s own shape
# `spoolway stack` starts the pull request title with the ticket key only
# when `ticket:` is a bare key such as a Jira `KAN-11` — never for a GitHub
# `ticket:`, always a URL. Two tasks, same shape, off `main` so neither
# touches the stack `top` and `base` already proved.
must "jira-ticket's branch, off main" git branch task/jira-ticket main
must "its worktree" git worktree add -q "$WORKTREES/jira-ticket" task/jira-ticket
(
  cd "$WORKTREES/jira-ticket" || exit 1
  mkdir -p notes
  echo "# jira-ticket" > notes/jira-ticket.md
  git add -A
  git commit -qm "wip(jira-ticket): implement"
)
queue_task jira-ticket "base: main" "branch: task/jira-ticket" "ticket: KAN-11"

out_jira_ticket=$(cd "$WORKTREES/jira-ticket" && "$SPOOLWAY" stack jira-ticket 2>&1)
if [ $? -eq 0 ]; then ok "and for a task naming a bare Jira ticket key"
else bad "and for a task naming a bare Jira ticket key"; sed 's/^/        /' <<<"$out_jira_ticket"; fi

jira_ticket_pr=$(grep -l '^head=task/jira-ticket$' "$LIVE/prs"/[0-9]* 2>/dev/null | head -1)
if [ -n "$jira_ticket_pr" ] && \
   [ "$(sed -n 's/^title=//p' "$jira_ticket_pr")" = "KAN-11 jira-ticket, a change of its own" ]; then
  ok "its pull request title starts with the bare ticket key"
else
  bad "its pull request title starts with the bare ticket key"
  [ -n "$jira_ticket_pr" ] && sed 's/^/        /' "$jira_ticket_pr"
fi

must "github-ticket's branch, off main" git branch task/github-ticket main
must "its worktree" git worktree add -q "$WORKTREES/github-ticket" task/github-ticket
(
  cd "$WORKTREES/github-ticket" || exit 1
  mkdir -p notes
  echo "# github-ticket" > notes/github-ticket.md
  git add -A
  git commit -qm "wip(github-ticket): implement"
)
queue_task github-ticket "base: main" "branch: task/github-ticket" \
  "ticket: https://github.com/e2e/spoolway/issues/9"

out_github_ticket=$(cd "$WORKTREES/github-ticket" && "$SPOOLWAY" stack github-ticket 2>&1)
if [ $? -eq 0 ]; then ok "and for a task naming a GitHub ticket URL"
else bad "and for a task naming a GitHub ticket URL"; sed 's/^/        /' <<<"$out_github_ticket"; fi

github_ticket_pr=$(grep -l '^head=task/github-ticket$' "$LIVE/prs"/[0-9]* 2>/dev/null | head -1)
if [ -n "$github_ticket_pr" ] && \
   [ "$(sed -n 's/^title=//p' "$github_ticket_pr")" = "github-ticket, a change of its own" ]; then
  ok "and its title is left untouched, since a GitHub ticket is a URL"
else
  bad "and its title is left untouched, since a GitHub ticket is a URL"
  [ -n "$github_ticket_pr" ] && sed 's/^/        /' "$github_ticket_pr"
fi

# ------------------------------------------- the fact stack reports itself
# `rival` — an open task of another group, which nothing here ever runs
# `stack` for — sits on a branch that changes the same file `edge` does,
# differently, off the same base. That is not a reason to refuse the pull
# request, so what is checked is that the predicted conflict is *reported*
# rather than that anything stops.
#
# `twin` changes that same file a third way, but sits in `edge`'s own group,
# one step further up its chain. A group is one chain, and a later task of it
# is cut from the earlier one's branch, so the two are one stack and never
# conflict in the sense `stack` checks. The `conflicts` line walks only other
# groups: it has to name `rival` and leave `twin` out, and only a real
# `git merge-tree` over two real conflicting branches can show the second half.
#
# Reported on the console, and nowhere else. e761a22 cut the pull request
# trailer down to the co-author tag: the predicted conflict is for whoever is
# watching the lane run, and repeating it under the task only crowded the body
# a reviewer opens. So this asserts both halves — that `stack` says it on its
# own output, and that the body stays clear of it — because a check that only
# watched the console would let the line drift back into the pull request
# unnoticed.
must "rival's branch, off main" git branch task/rival main
must "its worktree" git worktree add -q "$WORKTREES/rival" task/rival
(
  cd "$WORKTREES/rival" || exit 1
  mkdir -p notes
  echo "# edge, rival's own idea" > notes/edge.md
  git add -A
  git commit -qm "wip(rival): implement"
)
queue_task rival "group: rival-group" "base: main" "branch: task/rival"

must "twin's branch, off main" git branch task/twin main
must "its worktree" git worktree add -q "$WORKTREES/twin" task/twin
(
  cd "$WORKTREES/twin" || exit 1
  mkdir -p notes
  echo "# edge, twin's own idea" > notes/edge.md
  git add -A
  git commit -qm "wip(twin): implement"
)
queue_task twin "group: edge-group" "depends_on: [edge]" \
  "base: main" "branch: task/twin"

must "edge's branch, off main" git branch task/edge main
must "its worktree" git worktree add -q "$WORKTREES/edge" task/edge
(
  cd "$WORKTREES/edge" || exit 1
  mkdir -p notes
  echo "# edge" > notes/edge.md
  git add -A
  git commit -qm "wip(edge): implement"
)
queue_task edge "group: edge-group" "base: main" "branch: task/edge"

edge_out=$(cd "$WORKTREES/edge" && "$SPOOLWAY" stack edge 2>&1)
if [ $? -eq 0 ]; then ok "and for a task another group's open branch conflicts with"
else bad "and for a task another group's open branch conflicts with"; sed 's/^/        /' <<<"$edge_out"; fi

edge_pr=$(grep -l '^head=task/edge$' "$LIVE/prs"/[0-9]* 2>/dev/null | head -1)
conflicts_line=$(grep -E '^[[:space:]]*conflicts[[:space:]]' <<<"$edge_out")
if grep -qF "rival (group rival-group)" <<<"$conflicts_line"; then
  ok "and its \`conflicts\` line names the other group's task, and that group"
else
  bad "and its \`conflicts\` line names the other group's task, and that group"
  sed 's/^/        /' <<<"$edge_out"
fi
if [ -n "$conflicts_line" ] && ! grep -qF "twin" <<<"$conflicts_line"; then
  ok "but leaves out a conflicting task of its own group"
else
  bad "but leaves out a conflicting task of its own group"
  sed 's/^/        /' <<<"$edge_out"
fi
if grep -qE '^[[:space:]]*siblings[[:space:]]' <<<"$edge_out"; then
  bad "and no \`siblings\` line is printed any more"
  sed 's/^/        /' <<<"$edge_out"
else
  ok "and no \`siblings\` line is printed any more"
fi
if [ -n "$edge_pr" ] && ! grep -qF "rival" "${edge_pr}.body" 2>/dev/null; then
  ok "and the prediction is not repeated in the pull request body"
else
  bad "and the prediction is not repeated in the pull request body"
  [ -n "$edge_pr" ] && sed 's/^/        /' "${edge_pr}.body"
fi

# --------------------------------------------------- empty diff, off `top`
must "a third branch, sitting exactly on top's tip" \
  git branch task/same task/top
must "its worktree" git worktree add -q "$WORKTREES/same" task/same
queue_task same "depends_on: [top]" \
  "base: main" "starts_from: task/top" "branch: task/same"

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
queue_task hooked \
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

# --------------------------------- a base that exists locally and nowhere else
# `task/untracked` is a branch this worktree can see but the remote has never
# heard of — the shape recorded as `starts_from` when a task is cut from a branch
# that was never itself published, such as a worktree-cut branch. `remote_ref`
# hands back the bare local name for it, and nothing publishes that name
# before `gh pr create` is asked to open a pull request against it — for real
# `gh`, that fails outright (`Base sha can't be blank … Base ref must be a
# branch`); this suite's stub is lenient about it, so what is checked directly
# is whether the base branch actually reached the remote, not whether `gh`
# objected.
must "the untracked base's branch, off main" git branch task/untracked main
must "its worktree" git worktree add -q "$WORKTREES/untracked" task/untracked
(
  cd "$WORKTREES/untracked" || exit 1
  mkdir -p notes
  echo "# untracked" > notes/untracked.md
  git add -A
  git commit -qm "wip(untracked): implement, never pushed"
)

must "leaf's branch, off task/untracked" git branch task/leaf task/untracked
must "its worktree" git worktree add -q "$WORKTREES/leaf" task/leaf
(
  cd "$WORKTREES/leaf" || exit 1
  mkdir -p notes
  echo "# leaf" > notes/leaf.md
  git add -A
  git commit -qm "wip(leaf): implement"
)
# `untracked` is a bare branch, never queued as a task of its own, so it has
# no pull request for `register_stack` to stack onto — `leaf` names no
# `depends_on` here, since this case is about `starts_from` resolving a base to
# publish, not about the stacking that a real dependency chain exercises
# elsewhere in this suite.
queue_task leaf \
  "base: main" "starts_from: task/untracked" "branch: task/leaf"

leaf_out=$(cd "$WORKTREES/leaf" && "$SPOOLWAY" stack leaf 2>&1)
leaf_status=$?

if [ "$leaf_status" -eq 0 ] \
   && git -C "$ORIGIN" show-ref --verify --quiet refs/heads/task/untracked; then
  ok "a base that exists only locally is published to the remote before the pull request is opened"
else
  bad "a base that exists only locally is published to the remote before the pull request is opened"
  printf '        exit %s: %s\n' "$leaf_status" "$leaf_out"
fi

leaf_pr=$(grep -l '^head=task/leaf$' "$LIVE/prs"/[0-9]* 2>/dev/null | head -1)
if [ -n "$leaf_pr" ] && [ "$(sed -n 's/^base=//p' "$leaf_pr")" = task/untracked ]; then
  ok "its pull request targets that base"
else
  bad "its pull request targets that base"
  [ -n "$leaf_pr" ] && sed 's/^/        /' "$leaf_pr"
fi

# ------------------------------------------- a task may not name its own branch
# `branch:` is spoolway's field outright. A task that sets it to
# anything spoolway would not have stamped itself — `task/<id>`, or the
# `task/<slug>-<id>` form `issue_tracking.key_in_names` prefixes — is refused
# when the task file is loaded, well before `spoolway stack` could force-push
# a squashed commit onto it or hand a `-`-led value to `gh pr view`
# (findings 11, 12). `branch: main` here is neither shape.
main_local_before=$(git rev-parse main)
main_remote_before=$(git rev-parse origin/main)
queue_task claimed "base: main" "branch: main"
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

# ----------------------------------------- follow a merged base's pull request
# `scripts/e2e-fake-gh.sh`, not this suite's own `gh-stub.sh`: only the fuller
# double answers `gh pr list --head <branch> --state all`, which the fallback
# below depends on to find a pull request whose branch no longer resolves
# anywhere. Its own bare repo is symlinked onto this suite's real `$ORIGIN` so
# `pr create`'s branch-existence check still sees the branches this suite
# actually pushes.
FAKEGH="$LIVE/fakegh"
mkdir -p "$FAKEGH/bin" "$FAKEGH/prs"
must "the fuller gh double" install -m 755 "$SCRIPTS/e2e-fake-gh.sh" "$FAKEGH/bin/gh"
must "its forge shares this suite's real origin" ln -s "$ORIGIN" "$FAKEGH/origin.git"

run_with_fake_gh() {
  SPOOLWAY_GH="$FAKEGH/bin/gh" SPOOLWAY_E2E_FORGE="$FAKEGH" "$SPOOLWAY" "$@"
}

# A pull request recorded for a branch this suite never creates at all — a
# deleted `starts_from` never resolving is exactly what `gh pr list --head`
# still has to answer about, so nothing here needs the branch to exist.
# Numbered well past anything `pr create` will assign on its own below, so a
# real pull request opened during this section can never collide with one of
# these.
record_pr() {
  local n=$1 base=$2 head=$3 state=$4
  {
    echo "number=$n"
    echo "base=$base"
    echo "head=$head"
    echo "title=$head"
    echo "state=$state"
  } > "$FAKEGH/prs/$n"
  echo "a pull request recorded for the sandbox forge" > "$FAKEGH/prs/$n.body"
}
record_pr 9001 main task/gh412-checkout MERGED
record_pr 9002 main task/gh413-checkout CLOSED

# ---- merged: `stack` opens the dependent's own pull request against `main`
must "the branch, off main" git branch task/landed main
must "its worktree" git worktree add -q "$WORKTREES/landed" task/landed
(
  cd "$WORKTREES/landed" || exit 1
  mkdir -p notes
  echo "# landed" > notes/landed.md
  git add -A
  git commit -qm "wip(landed): implement"
)
# `base:` equal to `starts_from` — the chain's first task, which has nowhere to
# fall back to on its own once `starts_from` is gone (acceptance criterion 2).
queue_task landed \
  "base: task/gh412-checkout" "starts_from: task/gh412-checkout" "branch: task/landed"

landed_out=$(cd "$WORKTREES/landed" && run_with_fake_gh stack landed 2>&1)
landed_status=$?
if [ "$landed_status" -eq 0 ] \
   && grep -qF '`task/gh412-checkout` has landed — against `main`' <<<"$landed_out"; then
  ok "a merged pull request for a deleted \`starts_from\` is followed to its own base"
else
  bad "a merged pull request for a deleted \`starts_from\` is followed to its own base"
  printf '        exit %s: %s\n' "$landed_status" "$landed_out"
fi

landed_pr=$(grep -l '^head=task/landed$' "$FAKEGH/prs"/[0-9]* 2>/dev/null | head -1)
if [ -n "$landed_pr" ] && [ "$(sed -n 's/^base=//p' "$landed_pr")" = main ]; then
  ok "and its own pull request is opened with \`--base main\`"
else
  bad "and its own pull request is opened with \`--base main\`"
  [ -n "$landed_pr" ] && sed 's/^/        /' "$landed_pr"
fi

# ---- closed without merging: named rather than followed
must "the branch, off main" git branch task/orphan main
must "its worktree" git worktree add -q "$WORKTREES/orphan" task/orphan
(
  cd "$WORKTREES/orphan" || exit 1
  mkdir -p notes
  echo "# orphan" > notes/orphan.md
  git add -A
  git commit -qm "wip(orphan): implement"
)
queue_task orphan \
  "base: task/gh413-checkout" "starts_from: task/gh413-checkout" "branch: task/orphan"

orphan_out=$(cd "$WORKTREES/orphan" && run_with_fake_gh stack orphan 2>&1)
orphan_status=$?
if [ "$orphan_status" -ne 0 ] \
   && grep -qF "its pull request #9002 was closed without merging" <<<"$orphan_out"; then
  ok "a pull request closed without merging stops the run and names it"
else
  bad "a pull request closed without merging stops the run and names it"
  printf '        exit %s: %s\n' "$orphan_status" "$orphan_out"
fi

# ---- no pull request at all: today's message, unchanged
must "the branch, off main" git branch task/nowhere main
must "its worktree" git worktree add -q "$WORKTREES/nowhere" task/nowhere
(
  cd "$WORKTREES/nowhere" || exit 1
  mkdir -p notes
  echo "# nowhere" > notes/nowhere.md
  git add -A
  git commit -qm "wip(nowhere): implement"
)
queue_task nowhere \
  "base: task/gh999-nothing" "starts_from: task/gh999-nothing" "branch: task/nowhere"

nowhere_out=$(cd "$WORKTREES/nowhere" && run_with_fake_gh stack nowhere 2>&1)
nowhere_status=$?
if [ "$nowhere_status" -ne 0 ] \
   && grep -qF "resolves to nothing" <<<"$nowhere_out" \
   && grep -qF "does not resolve either" <<<"$nowhere_out"; then
  ok "no pull request found at all keeps today's message"
else
  bad "no pull request found at all keeps today's message"
  printf '        exit %s: %s\n' "$nowhere_status" "$nowhere_out"
fi

# ---- a distinct `base:` that still resolves: the older fallback, unchanged.
# No pull request is recorded for this `starts_from`, so landing on `main` here
# can only have come from `base:` itself, never from the forge lookup above.
must "the branch, off main" git branch task/dependent main
must "its worktree" git worktree add -q "$WORKTREES/dependent" task/dependent
(
  cd "$WORKTREES/dependent" || exit 1
  mkdir -p notes
  echo "# dependent" > notes/dependent.md
  git add -A
  git commit -qm "wip(dependent): implement"
)
queue_task dependent \
  "base: main" "starts_from: task/gh414-deleted" "branch: task/dependent"

dependent_out=$(cd "$WORKTREES/dependent" && run_with_fake_gh stack dependent 2>&1)
dependent_status=$?
dependent_pr=$(grep -l '^head=task/dependent$' "$FAKEGH/prs"/[0-9]* 2>/dev/null | head -1)
if [ "$dependent_status" -eq 0 ] \
   && grep -qF '`task/gh414-deleted` has landed — against `main`' <<<"$dependent_out" \
   && [ -n "$dependent_pr" ] && [ "$(sed -n 's/^base=//p' "$dependent_pr")" = main ]; then
  ok "a deleted \`starts_from\` with its own \`base:\` still lands on that base"
else
  bad "a deleted \`starts_from\` with its own \`base:\` still lands on that base"
  printf '        exit %s: %s\n' "$dependent_status" "$dependent_out"
fi

finish
