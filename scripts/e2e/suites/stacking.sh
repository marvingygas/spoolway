#!/usr/bin/env bash
# A stack, actually built.
#
# `handover` (a command step, `run: spoolway stack`) is never mocked — it is
# the real command, run for real against a real bare-repo forge. For a task
# with no dependency that is enough: `spoolway stack` commits, squashes,
# pushes and opens the pull request, and `base` below hands its own change
# over this way, with nothing standing in for it. `top` depends on `base`,
# and a dependent has one more thing to do — register itself in the pull
# request's stack — which needs `gh api` against a real github.com remote.
# This forge is a bare repo on disk, not github.com, so that one call always
# refuses, after everything else `spoolway stack` does has already
# succeeded. Its `on_fail` is `blocked` now, with no second, LLM-run step
# behind it to reconcile what refused: `spoolway stack` already did every
# bit of real work it could, and what is left is a person's call. Two
# chained tasks: `base` depends on nothing, `top` depends on `base`. What has
# to be true at the end is one fact with two halves, and only both together
# mean anything:
#
#   1. `top`'s pull request *targets* `task/base`, not `base`'s own `base:`
#   2. `top`'s branch *contains* `base`'s commits
#
# Both halves are facts of the cut, not something built afterwards. A
# dependent's worktree is cut straight from its dependency's branch — see
# `src/dispatch.rs`'s `ensure_workspace` — so `top` sits on top of `base` the
# moment it exists, and `base`'s branch is kept alive as long as anything
# queued still names it, exactly so `top` has it to be cut from. `spoolway
# stack` finds nothing left to rebase because of that, not because anything
# reconciled it after the fact; this suite checks both halves are already
# true well before `top`'s own `handover` runs, and again once it has.
#
# Nothing in spoolway's git plumbing knows about pull requests or targets.
# The chain is `depends_on`, written by the plan skill; the target is
# `spoolway stack`'s own, read out of the task file's `cut_from` — the fact
# of the cut `depends_on` produced, not `depends_on` itself. Swapping the
# mock for one that opens flat pull requests should fail exactly the two
# `stacked_on`/ancestry checks below and nothing else in the suite.
#
# The chain is also what `last:` is about, so the second half of this suite adds
# a step carrying it and asks which of the two tasks actually ran the command.
# covers: step.last — only the top of a chain runs it; the task below walks past without starting the command
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

# ------------------------------------------------------- a step the stack runs once
# A command step carrying `last:`, between `review` and `document`. It appends a
# line to a file in the control plane rather than in a worktree, so what it
# writes outlives the worktree cleanup that archives each task — and a count of
# lines is then the whole question: one run for a two-task chain, not two.
#
# The `0,/…/` range keeps the rewiring to the first match, which is `review`'s
# `on_pass: document`, not the `on_pass: document` in the step appended just
# above it.
RUNS="$LIVE/proj/suite-runs.txt"
{
  printf '\n  - id: suite\n'
  printf '    description: A stand-in for a check the whole stack needs once.\n'
  printf '    run: echo ran >> "$SPOOLWAY_REPO/suite-runs.txt"\n'
  printf '    last: true\n'
  printf '    on_pass: document\n    on_fail: blocked\n'
} >> .spoolway/pipelines/default.yml
sed -i "0,/^    on_pass: document\$/s//    on_pass: suite/" .spoolway/pipelines/default.yml
works "a pipeline whose stack-wide step declares \`last:\` checks out" \
  "$SPOOLWAY" pipeline check

BODY="$LIVE/body.md"
task_body "$BODY"

ORIGIN="$FORGE/origin.git"

# ------------------------------------------------------------------ the chain
# A chain, which is what the plan skill emits now: ordered always, every task
# depending on the one before it. Two links is the smallest thing that can be a
# stack at all.
task_doc "$LIVE/base.md" base "$BODY" "group: live" "touches: [notes/base.md]"
must "the bottom of the stack" "$SPOOLWAY" queue add --from "$LIVE/base.md"
task_doc "$LIVE/top.md" top "$BODY" "group: live" \
  "touches: [notes/top.md]" "depends_on: [base]"
must "the task above it" "$SPOOLWAY" queue add --from "$LIVE/top.md"

says "the task above says what it is waiting on" "waiting on: base" \
  "$SPOOLWAY" queue list

# `base` and `top` share a group and `top` depends on `base`, so the group's
# block reads as run order: `base` — what a pass would start next — above
# `top`, whatever the alphabet says about the pair.
QUEUE_LIST=$("$SPOOLWAY" queue list 2>&1)
base_line=$(grep -n -m1 -w base <<<"$QUEUE_LIST" | cut -d: -f1)
top_line=$(grep -n -m1 -w top <<<"$QUEUE_LIST" | cut -d: -f1)
if [ -n "$base_line" ] && [ -n "$top_line" ] && [ "$base_line" -lt "$top_line" ]; then
  ok "and the chain reads top to bottom in run order, base above top"
else
  bad "and the chain reads top to bottom in run order, base above top"
  sed 's/^/        /' <<<"$QUEUE_LIST"
fi

silent_about "and nothing starts it while the one below is unfinished" \
  "would start \`implement\` for top" "$SPOOLWAY" dispatch --dry-run

# --------------------------------------------------------- the bottom lands first
if drive base gone 90; then ok "the bottom of the chain runs and is archived"
else bad "the bottom of the chain runs and is archived (at \`$(stage_of base)\`)"; fi

if handed_over base; then ok "and hands its change over as a branch and a pull request"
else bad "and hands its change over as a branch and a pull request"; ls "$FORGE/prs" | sed 's/^/        /'; fi
# With nothing under it, the bottom stacks on its own `base:` — the branch it
# was queued from.
if stacked_on base plan/live; then ok "targeting its own \`base:\`"
else bad "targeting its own \`base:\`"; cat "$FORGE"/prs/[0-9]* | sed 's/^/        /'; fi

has "it documented its own change on the way" "→ \`document\`" $SPOOLWAY_PROJECT_HOME/archive/base.md
# The pane label a lane's transcript is headed by is its own name now
# (`<task> · <step>`, acceptance criterion 6), not the prompt — so which
# prompt actually ran is read from the composed prompt file it was handed.
has "with a real documenting lane" \
  "You bring this project's documents back in line with what a plan built." \
  "$SPOOLWAY_PROJECT_HOME/system-prompts/base · document.md"

# The bottom of the stack is not the last task of it, so the step was walked
# past rather than run. Two readings of the one fact: nothing was written, and
# no run was ever started to write it — a command that had started and failed
# would leave a log behind either way.
if [ ! -e "$RUNS" ]; then
  ok "the bottom of the chain never ran the \`last:\` step"
else
  bad "the bottom of the chain never ran the \`last:\` step"
  cat "$RUNS" | sed 's/^/        /'
fi
if [ ! -e "$SPOOLWAY_PROJECT_HOME/commands/base · suite.log" ]; then
  ok "and no run was started for it at all"
else
  bad "and no run was started for it at all"
  sed 's/^/        /' "$SPOOLWAY_PROJECT_HOME/commands/base · suite.log"
fi
# The dispatcher's own account, not the task file's: walking past a step is a
# fact about a pass, and the task file records where a task went rather than
# what was decided about it on the way.
has "and the pass says why it walked past" \
  "\`suite\` does not run for this task (not last in its chain)" \
  "$E2E_DISPATCH_LOG"

# ------------------------------------------------ cut on top, not beside it
# Held at `handover`, well after the cut and well before it blocks — `top`'s
# worktree and `base`'s own branch both exist for `top`'s whole run, from the
# cut through to the moment `handover` runs, so any stage in between shows
# the same thing the instant of the cut itself did: the ancestry is a fact
# of the cut, not something built afterwards.
if drive_and_hold top handover 90; then ok "the task above it is cut once the one below is in"
else bad "the task above it is cut once the one below is in (at \`$(stage_of top)\`)"; fi

if git rev-parse --verify -q task/base >/dev/null; then
  ok "the branch below it still exists at the moment it is cut from"
else
  bad "the branch below it still exists at the moment it is cut from"
fi

if [ -e "$LIVE/worktrees/task-top/work-base.txt" ]; then
  ok "and its worktree already carries the work that branch holds"
else
  bad "and its worktree already carries the work that branch holds"
  ls "$LIVE/worktrees/task-top" 2>/dev/null | sed 's/^/        /'
fi

# `cut_from` is a fact about the cut, `base:` a fact about where the plan
# lands, and they read differently apart the moment there is a dependency —
# `queue show` prints both, so nobody has to infer the first from the second.
says "\`queue show\` prints what it was actually cut from" \
  "cut_from: task/base" "$SPOOLWAY" queue show top
says "distinctly from \`base:\`, which still names the plan branch" \
  "base: plan/live" "$SPOOLWAY" queue show top

# ------------------------------------------------------------- and then the top
if drive top blocked 90; then ok "the task above it runs once the one below is in"
else bad "the task above it runs once the one below is in (at \`$(stage_of top)\`)"; fi

if handed_over top; then ok "and it hands its change over too, before the one call that fails"
else bad "and it hands its change over too, before the one call that fails"; ls "$FORGE/prs" | sed 's/^/        /'; fi

# ------------------------------------------------------------ half one: the base
if stacked_on top task/base; then
  ok "its pull request targets the branch below it, not its own \`base:\`"
else
  bad "its pull request targets the branch below it, not its own \`base:\`"
  cat "$FORGE"/prs/[0-9]* | sed 's/^/        /'
fi

# ------------------------------------------------------ half two: the ancestry
# Read off the forge, because the local branches are gone: cleanup deletes
# them, and what the pull request is actually about is what is on the remote.
if git -C "$ORIGIN" merge-base --is-ancestor "task/base" "task/top"; then
  ok "and its branch really sits on top of that one"
else
  bad "and its branch really sits on top of that one"
  git -C "$ORIGIN" log --oneline --graph task/base task/top | sed 's/^/        /' | head -20
fi

# Said the other way round, as the thing anybody reviewing that pull request would
# see: the diff between the two branches is `top`'s work and nothing else. A
# branch that merely names `task/base` without sitting on it shows `base`'s
# files being deleted, which is the exact failure the rebase exists to prevent.
if git -C "$ORIGIN" show "task/top:notes/base.md" >/dev/null 2>&1 \
   || git -C "$ORIGIN" show "task/top:work-base.txt" >/dev/null 2>&1; then
  ok "so the work below it is present in the branch above, not deleted by it"
else
  bad "so the work below it is present in the branch above, not deleted by it"
  git -C "$ORIGIN" diff --stat "task/base" "task/top" | sed 's/^/        /'
fi

# `handover` is a command step (`run: spoolway stack`, no role prompt, no
# system prompt file) — see .spoolway/pipelines/default.yml — and its own log shows both
# halves of what happened: the commit, squash, push and pull request all
# went through for real, and only the stack registration call refused,
# because this forge is a bare repo rather than github.com. `on_fail` is
# `blocked` now, with no second, LLM-run step behind it to reconcile
# anything — there is nothing left for one to do, and no lane is staffed to
# say so on its own: this run is not unattended, so `top` simply waits at
# `blocked` for a person, exactly as it does at any other step's failure.
has "spoolway stack committed, pushed and opened the pull request for real" \
  "pull req" "$SPOOLWAY_PROJECT_HOME/commands/top · handover.log"
has "before the one call that needs a real github.com remote refused it" \
  "is not a github.com remote" "$SPOOLWAY_PROJECT_HOME/commands/top · handover.log"
if [ -e "$SPOOLWAY_PROJECT_HOME/system-prompts/top · blocked.md" ]; then
  bad "and nothing is staffed to paper over the block on its own"
  cat "$SPOOLWAY_PROJECT_HOME/system-prompts/top · blocked.md" | sed 's/^/        /'
else
  ok "and nothing is staffed to paper over the block on its own"
fi

# ---------------------------------------------------- the top runs it, once
# The task at the top carries every change beneath it, so its one run is the
# run the whole stack gets. A line per run, and a chain of two that answered
# "last" twice would show two.
if [ -e "$RUNS" ] && [ "$(wc -l < "$RUNS")" -eq 1 ]; then
  ok "the top of the chain ran the \`last:\` step, once for the stack"
else
  bad "the top of the chain ran the \`last:\` step, once for the stack"
  printf '        %s\n' "$([ -e "$RUNS" ] && wc -l < "$RUNS" || echo 'no file at all')"
fi
if [ -e "$SPOOLWAY_PROJECT_HOME/commands/top · suite.log" ]; then
  ok "and it was the top's own run that did it"
else
  bad "and it was the top's own run that did it"
  ls $SPOOLWAY_PROJECT_HOME/commands/ 2>/dev/null | sed 's/^/        /'
fi

# -------------------------------------------------- a third link, joining both
# A task naming both `base` and `top` in its own `depends_on` — the case
# `chains-not-fans` adds a rule for: the list has to start with whichever
# parent's own history already reaches the other, because that is the only
# one its worktree can be cut from and still carry both. `top` already
# reaches `base` (it depends on it), so the wrong order given below —
# `base` before `top` — is exactly what `check_dependencies_set` has to
# reorder for this to mean anything.
task_doc "$LIVE/apex.md" apex "$BODY" "group: live" \
  "touches: [notes/apex.md]" "depends_on: [base, top]"
must "a third task naming both parents, in either order" \
  "$SPOOLWAY" queue add --from "$LIVE/apex.md"
says "check_dependencies_set puts the parent that reaches the other one first" \
  $'depends_on:\n- top\n- base' "$SPOOLWAY" queue show apex

# `top` never reaches `done` in this suite — its own handover always blocks
# on the one call this forge cannot answer, which is the whole point of the
# section above. That is real for `top` itself, but it also means nothing
# queued behind it would ever be released to prove the cut this section is
# about. So this simulates the one thing that actually frees it: a person
# reading its still-real pull request and landing it by hand. `stage: done`
# is the same file `add_task_with` writes throughout the unit tests for an
# already-finished task, and `branch_still_needed` (src/dispatch.rs) is what
# then keeps `task/top` alive through the archiving this triggers, because
# `apex` is still in the queue naming it.
sed -i 's/^stage: blocked$/stage: done/' "$SPOOLWAY_PROJECT_HOME/queue/top.md"

if drive_and_hold apex handover 90; then ok "apex is cut once both its parents are in"
else bad "apex is cut once both its parents are in (at \`$(stage_of apex)\`)"; fi

says "and it was cut from the deeper parent" "cut_from: task/top" \
  "$SPOOLWAY" queue show apex

if [ -e "$LIVE/worktrees/task-apex/work-base.txt" ] && [ -e "$LIVE/worktrees/task-apex/work-top.txt" ]; then
  ok "its worktree already carries both parents' work"
else
  bad "its worktree already carries both parents' work"
  ls "$LIVE/worktrees/task-apex" 2>/dev/null | sed 's/^/        /'
fi

if drive apex blocked 90; then ok "apex hands its own change over too"
else bad "apex hands its own change over too (at \`$(stage_of apex)\`)"; fi

if stacked_on apex task/top; then
  ok "and its pull request targets the deeper parent, not the shallower one"
else
  bad "and its pull request targets the deeper parent, not the shallower one"
  cat "$FORGE"/prs/[0-9]* | sed 's/^/        /'
fi

if git -C "$ORIGIN" merge-base --is-ancestor "task/top" "task/apex" \
   && git -C "$ORIGIN" merge-base --is-ancestor "task/base" "task/apex"; then
  ok "and both parents' commits are really in its history"
else
  bad "and both parents' commits are really in its history"
  git -C "$ORIGIN" log --oneline --graph task/base task/top task/apex \
    | sed 's/^/        /' | head -20
fi

# ---------------------------------------------------------------- never merged
# Handing over stops at a green pull request. Nothing in this suite, or in the
# mock it drives, ever merges a branch or opens a pull request for the plan
# itself — landing is a person's, bottom-up, and out of scope for a dispatch run.
open_count=$(grep -lx 'state=OPEN' "$FORGE"/prs/[0-9]* 2>/dev/null \
  | xargs -r grep -lxE 'head=task/(base|top|apex)' | wc -l)
if [ "$open_count" -eq 3 ]; then
  ok "all three pull requests are still open — nothing merged them"
else
  bad "expected all three pull requests still open, found $open_count"
  cat "$FORGE"/prs/[0-9]* | sed 's/^/        /'
fi

if grep -lx "head=plan/live" "$FORGE"/prs/[0-9]* >/dev/null 2>&1; then
  bad "no plan pull request should exist, but one was opened against plan/live"
  cat "$FORGE"/prs/[0-9]* | sed 's/^/        /'
else
  ok "and no plan pull request was ever opened"
fi

# --------------------------------------------------------- two siblings, one slot
# `base` and `top` are a chain — every earlier assertion in this suite is
# about that shape. A GitHub stack can only ever be a chain: `dep` here has
# two dependents, `siba` and `sibb`, and only the first can hold the slot
# above it. `register_stack` (src/commands/stack.rs) has to say so rather
# than fail `handover` over a shape the real stack API refuses outright.
#
# `spoolway stack` is called directly, the way scripts/e2e/suites/stack.sh
# does, rather than through a dispatch pass: `owner_repo()` needs a remote
# that reads as `github.com`, which this suite's own forge deliberately is
# not (the assertion above this block depends on that staying true), so
# `origin` is repointed at a github.com-shaped URL — redirected straight
# back to the same bare forge with `insteadOf` — for this block alone, and
# `gh-stub.sh` stands in for `gh` instead of the sandbox's own double, which
# never tracks stack membership at all (see its own header comment).
cd "$LIVE/proj" || exit 1
must "origin addressed as github, for this block" \
  git remote set-url origin "https://github.com/e2e/spoolway-siblings.git"
must "and redirected straight back to the same bare forge" \
  git config "url.$ORIGIN.insteadOf" "https://github.com/e2e/spoolway-siblings.git"

export GH_STUB_PRS="$LIVE/sib-prs"
export GH_STUB_URL="file://$ORIGIN"
export SPOOLWAY_GH="$HERE/../gh-stub.sh"

# sib_task <id> [extra frontmatter lines…] — a task file with exactly the
# fields a cut worktree carries, written by hand: this block never runs a
# dispatch pass to cut one for real.
sib_task() {
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

WORKTREES_SIB="$LIVE/worktrees-sib"
mkdir -p "$WORKTREES_SIB"

must "dep's branch, off main" git branch task/dep main
must "dep's worktree" git worktree add -q "$WORKTREES_SIB/dep" task/dep
(
  cd "$WORKTREES_SIB/dep" || exit 1
  mkdir -p notes
  echo "# dep" > notes/dep.md
  git add -A
  git commit -qm "wip(dep): implement"
)
sib_task dep "touches: [notes/dep.md]" "base: main" "branch: task/dep"
dep_out=$(cd "$WORKTREES_SIB/dep" && "$SPOOLWAY" stack dep 2>&1)
if [ $? -eq 0 ]; then ok "the shared dependency hands over"
else bad "the shared dependency hands over"; sed 's/^/        /' <<<"$dep_out"; fi

must "the first sibling's branch, off task/dep" git branch task/siba task/dep
must "its worktree" git worktree add -q "$WORKTREES_SIB/siba" task/siba
(
  cd "$WORKTREES_SIB/siba" || exit 1
  mkdir -p notes
  echo "# siba" > notes/siba.md
  git add -A
  git commit -qm "wip(siba): implement"
)
sib_task siba "touches: [notes/siba.md]" "depends_on: [dep]" \
  "base: main" "cut_from: task/dep" "branch: task/siba"
siba_out=$(cd "$WORKTREES_SIB/siba" && "$SPOOLWAY" stack siba 2>&1)
if [ $? -eq 0 ]; then ok "the first sibling takes the slot above it, in a stack of its own"
else bad "the first sibling takes the slot above it, in a stack of its own"; sed 's/^/        /' <<<"$siba_out"; fi
if grep -qF "created, bottom #" <<<"$siba_out"; then
  ok "and the report says a stack was created"
else
  bad "and the report says a stack was created"; sed 's/^/        /' <<<"$siba_out"
fi

must "the second sibling's branch, also off task/dep" git branch task/sibb task/dep
must "its worktree" git worktree add -q "$WORKTREES_SIB/sibb" task/sibb
(
  cd "$WORKTREES_SIB/sibb" || exit 1
  mkdir -p notes
  echo "# sibb" > notes/sibb.md
  git add -A
  git commit -qm "wip(sibb): implement"
)
sib_task sibb "touches: [notes/sibb.md]" "depends_on: [dep]" \
  "base: main" "cut_from: task/dep" "branch: task/sibb"
sibb_out=$(cd "$WORKTREES_SIB/sibb" && "$SPOOLWAY" stack sibb 2>&1)
sibb_status=$?
if [ "$sibb_status" -eq 0 ]; then
  ok "the second sibling still exits zero — a full slot is not a failure"
else
  bad "the second sibling still exits zero — a full slot is not a failure"
  printf '        exit %s: %s\n' "$sibb_status" "$sibb_out"
fi
if grep -qE "stack +none — .sibb. is a sibling of #[0-9]+ on #[0-9]+, outside stack #[0-9]+" <<<"$sibb_out"; then
  ok "and its \`stack\` line names the sibling and the stack it is outside of"
else
  bad "and its \`stack\` line names the sibling and the stack it is outside of"
  sed 's/^/        /' <<<"$sibb_out"
fi

finish
