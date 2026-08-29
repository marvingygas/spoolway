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

# ------------------------------------------------ [stack.summary]'s two modes
# Every branch above ran with `agent` and `model` blank — task-file mode, the
# default `spoolway init` writes — so nothing above this line ever ran a
# model turn at all. What follows is `run_summary` itself: a model filling in
# the template, a half-set table refused outright, and a missing template
# refused before anything reaches the remote.
#
# The stand-in below speaks the one turn `run_summary` actually runs — a
# plain process, its stdout read back directly — not the lane protocol
# `agent-mock.sh`'s stand-ins speak (`spoolway report`, ctl-files, a
# transcript); that machinery answers a different call entirely. It logs its
# prompt so a case can check the template's own text — not just a path to
# it — actually reached the model.
STUBBIN="$LIVE/stub-bin"
mkdir -p "$STUBBIN"
SUMMARY_PROMPT_LOG="$LIVE/summary-prompt.txt"
cat > "$STUBBIN/pi" <<'STUB'
#!/usr/bin/env bash
set -u
echo "${@: -1}" > "$SUMMARY_PROMPT_LOG"
printf '%s\n' "spool-turns: turn the spool while the queue moves"
printf '\n## Why\n\nThe masthead was a still logo.\n'
STUB
chmod +x "$STUBBIN/pi"
export SUMMARY_PROMPT_LOG
PATH="$STUBBIN:$PATH"

# set_stack_summary <config-file> <agent> <model>
#
# Rewrites the two lines `run_summary` actually branches on, in place, and
# leaves `effort` and `prompt` exactly as `init` wrote them. Committed to
# `main` before each branch below is cut from it, so a task's own diff never
# carries this edit — an uncommitted config change would otherwise show up
# as a file `touches` never declared, noise no case here is about.
set_stack_summary() {
  local cfg=$1 agent=$2 model=$3
  awk -v agent="$agent" -v model="$model" '
    /^\[stack\.summary\]$/ { print; in_block=1; next }
    in_block && /^agent = / { print "agent = \"" agent "\""; next }
    in_block && /^model = / { print "model = \"" model "\""; next }
    in_block && /^\[/ { in_block = 0 }
    { print }
  ' "$cfg" > "$cfg.tmp" && mv "$cfg.tmp" "$cfg"
}

# ---------------------------------------------------------- both set: the model runs
set_stack_summary .spoolway/config.toml pi fake-model
must "the model-mode config" git add .spoolway/config.toml
must "the model-mode config" git commit -qm "config: turn on [stack.summary]"

must "the branch, off main" git branch task/spool-turns main
must "the worktree" git worktree add -q "$WORKTREES/spool-turns" task/spool-turns
(
  cd "$WORKTREES/spool-turns" || exit 1
  mkdir -p notes
  echo "# spool-turns" > notes/spool-turns.md
  git add -A
  git commit -qm "wip(spool-turns): implement"
)
queue_task spool-turns "touches: [notes/spool-turns.md]" "base: main" "branch: task/spool-turns"

: > "$SUMMARY_PROMPT_LOG"
model_out=$(cd "$WORKTREES/spool-turns" && "$SPOOLWAY" stack spool-turns 2>&1)
if [ $? -eq 0 ]; then ok "with \`agent\` and \`model\` set, the model runs and the command exits 0"
else bad "with \`agent\` and \`model\` set, the model runs and the command exits 0"; sed 's/^/        /' <<<"$model_out"; fi

if grep -qF "Your first line is the pull request's title" "$SUMMARY_PROMPT_LOG" 2>/dev/null; then
  ok "the template's own text reached the model in the prompt, not just its path"
else
  bad "the template's own text reached the model in the prompt, not just its path"
  sed 's/^/        /' "$SUMMARY_PROMPT_LOG" 2>/dev/null
fi

model_pr=$(grep -l '^head=task/spool-turns$' "$LIVE/prs"/[0-9]* 2>/dev/null | head -1)
if [ -n "$model_pr" ] && [ "$(sed -n 's/^title=//p' "$model_pr")" = "spool-turns: turn the spool while the queue moves" ]; then
  ok "the model's first line becomes the pull request's title"
else
  bad "the model's first line becomes the pull request's title"
  [ -n "$model_pr" ] && sed 's/^/        /' "$model_pr"
fi
model_body="${model_pr}.body"
if [ -n "$model_pr" ] && grep -qF "## Why" "$model_body" 2>/dev/null \
   && ! grep -qF "# spool-turns" "$model_body" 2>/dev/null; then
  ok "the body is the model's own output, with no heading from the task file below it"
else
  bad "the body is the model's own output, with no heading from the task file below it"
  sed 's/^/        /' "$model_body" 2>/dev/null
fi
if [ -n "$model_pr" ] && grep -qF "Co-Authored-By: Claude Code" "$model_body" 2>/dev/null; then
  ok "and the trailer still closes it"
else
  bad "and the trailer still closes it"
fi

# ------------------------------------------------- exactly one of the two set
set_stack_summary .spoolway/config.toml pi ""
must "the half-set config" git add .spoolway/config.toml
must "the half-set config" git commit -qm "config: agent without a model"

must "the branch, off main" git branch task/half-set main
must "the worktree" git worktree add -q "$WORKTREES/half-set" task/half-set
(
  cd "$WORKTREES/half-set" || exit 1
  mkdir -p notes
  echo "# half-set" > notes/half-set.md
  git add -A
  git commit -qm "wip(half-set): implement"
)
queue_task half-set "touches: [notes/half-set.md]" "base: main" "branch: task/half-set"

before_sha=$(git rev-parse task/half-set)
half_out=$(cd "$WORKTREES/half-set" && "$SPOOLWAY" stack half-set 2>&1)
half_status=$?
if [ "$half_status" -ne 0 ] && grep -qF "stack.summary.model" <<<"$half_out" \
   && grep -qi "blank" <<<"$half_out"; then
  ok "exactly one of \`agent\`/\`model\` set names the blank key and refuses"
else
  bad "exactly one of \`agent\`/\`model\` set names the blank key and refuses"
  printf '        exit %s: %s\n' "$half_status" "$half_out"
fi
if [ "$(git rev-parse --verify -q origin/task/half-set 2>/dev/null || true)" = "" ]; then
  ok "and nothing was pushed"
else
  bad "and nothing was pushed"
fi
if [ -z "$(grep -l '^head=task/half-set$' "$LIVE/prs"/[0-9]* 2>/dev/null)" ]; then
  ok "and no pull request was opened"
else
  bad "and no pull request was opened"
fi
if [ "$(git -C "$WORKTREES/half-set" rev-parse HEAD)" = "$before_sha" ]; then
  ok "and the branch was never squashed"
else
  bad "and the branch was never squashed"
  git -C "$WORKTREES/half-set" log --oneline "$before_sha"..HEAD | sed 's/^/        /'
fi

# --------------------------------------------------- both set, no template
must "the template is removed" git rm -q .spoolway/templates/pull-request.md
set_stack_summary .spoolway/config.toml pi fake-model
must "the no-template config" git add .spoolway/config.toml
must "the no-template commit" git commit -qm "config: model summary, no template"

must "the branch, off main" git branch task/no-template main
must "the worktree" git worktree add -q "$WORKTREES/no-template" task/no-template
(
  cd "$WORKTREES/no-template" || exit 1
  mkdir -p notes
  echo "# no-template" > notes/no-template.md
  git add -A
  git commit -qm "wip(no-template): implement"
)
queue_task no-template "touches: [notes/no-template.md]" "base: main" "branch: task/no-template"

before_sha=$(git rev-parse task/no-template)
notpl_out=$(cd "$WORKTREES/no-template" && "$SPOOLWAY" stack no-template 2>&1)
notpl_status=$?
if [ "$notpl_status" -ne 0 ] \
   && grep -qF ".spoolway/templates/pull-request.md" <<<"$notpl_out" \
   && grep -qF "spoolway update --replace" <<<"$notpl_out"; then
  ok "a missing template refuses, naming the path and the fix"
else
  bad "a missing template refuses, naming the path and the fix"
  printf '        exit %s: %s\n' "$notpl_status" "$notpl_out"
fi
if [ "$(git rev-parse --verify -q origin/task/no-template 2>/dev/null || true)" = "" ]; then
  ok "and nothing was pushed"
else
  bad "and nothing was pushed"
fi
if [ -z "$(grep -l '^head=task/no-template$' "$LIVE/prs"/[0-9]* 2>/dev/null)" ]; then
  ok "and no pull request was opened"
else
  bad "and no pull request was opened"
fi
if [ "$(git -C "$WORKTREES/no-template" rev-parse HEAD)" = "$before_sha" ]; then
  ok "and the branch was never squashed"
else
  bad "and the branch was never squashed"
  git -C "$WORKTREES/no-template" log --oneline "$before_sha"..HEAD | sed 's/^/        /'
fi

finish
