#!/usr/bin/env bash
# A task's whole life, live.
#
# `dispatch.backend = headless` runs lanes as real detached processes, so this
# is the pipeline actually running: queued -> implement -> review -> handover
# -> done, with a lane per step, a real worktree, a real pull request, and
# the archive at the end.
#
# It runs both ways. The repo is a seed rather than a project, so what a real
# model is asked for here is one small file — enough that a lane has real work
# to report on, and not so much that the suite is a benchmark of the model.
#
# covers: agents.<profile>.concurrency — one slot admits one lane and the pass says what waits
# covers: dispatch.worktree_root — the task`s worktree is cut there, and is gone after cleanup
# covers: step.on_pass — the route a task took is on the record, step by step
# covers: dispatch.priority — an open group with no ready work of its own no longer holds slots from never-run groups
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

# Handing a change over means pushing it and opening a pull request, so every
# suite that drives a task to `done` needs a forge. There is no route that lands
# a change without a remote any more.
new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$CTL" "" "" "" "$FORGE"

new_repo "$LIVE/proj"
configure_project plan/live "$LIVE/worktrees"
publish plan/live

BODY="$LIVE/body.md"
task_body "$BODY"

# ------------------------------------------------------- queued to archived
# `--touches` is what the task writes, and the body asks for a file of the
# task's own: the two parallel tasks below run at once, and this suite is about
# the pipeline rather than about what happens when two lanes want one file.
task_doc "$LIVE/land.md" land "$BODY" "group: live" "touches: [notes/land.md]"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/land.md"

if drive land gone; then ok "a task runs queued -> ... -> done and is archived"
else bad "a task runs queued -> ... -> done and is archived (stuck at \`$(stage_of land)\`)"; fi

has "its journey is in the status log" "→ \`handover\`" $SPOOLWAY_PROJECT_HOME/archive/land.md
if handed_over land; then ok "the change is handed over as a pushed branch and a pull request"
else bad "the change is handed over as a pushed branch and a pull request"; ls "$FORGE/prs" | sed 's/^/        /'; fi
# With no dependency, a task stacks on the plan branch it was cut from.
if stacked_on land plan/live; then ok "and its pull request targets the branch it sits on"
else bad "and its pull request targets the branch it sits on"; cat "$FORGE"/prs/[0-9]* | sed 's/^/        /'; fi

if [ -e "$LIVE/worktrees/task-land" ]; then bad "the task's worktree is gone after cleanup"
else ok "the task's worktree is gone after cleanup"; fi
if git rev-parse --verify -q task/land >/dev/null; then bad "the task's branch is gone after cleanup"
else ok "the task's branch is gone after cleanup"; fi

# ---------------------------------------------------------- the checkout, not root
# `Repo::checkout` is where the tracked control plane is read from now — the
# worktree a lane actually runs in, not the main checkout `Repo::root` still
# names. A branch's own pipeline set proves it: give one to a branch nobody
# has queued anything on, and only that branch's own checkout sees it.
WT="$LIVE/worktrees/manual-wt"
must "cutting a worktree for the pipeline set" \
  git worktree add -q -b task/pipeline-set "$WT" plan/live
printf 'description: A pipeline only this branch has.\nsteps:\n  - id: solo\n    end: true\n' \
  > "$WT/.spoolway/pipelines/extra.yml"
must "committing the branch's own pipeline" \
  git -C "$WT" add .spoolway/pipelines/extra.yml
must "committing the branch's own pipeline" \
  git -C "$WT" commit -qm "e2e: a pipeline only this branch has"

says "pipeline list in the worktree names its own branch's pipeline" "extra" \
  "$SPOOLWAY" -C "$WT" pipeline list
silent_about "pipeline list in the main checkout does not see it" "extra" \
  "$SPOOLWAY" pipeline list
# The description under `extra`'s name is what a person actually chooses a
# pipeline by, so `list` prints it — in prose and, as a real field, in JSON.
says "pipeline list prints extra's description under its name" \
  "A pipeline only this branch has." \
  "$SPOOLWAY" -C "$WT" pipeline list
says "--json pipeline list carries extra's description as a field" \
  '"description": "A pipeline only this branch has."' \
  "$SPOOLWAY" -C "$WT" --json pipeline list

# Which checkout answered is on the screen, but only where it is not the
# project. The line names the worktree and the branch it has out; in the main
# checkout — the directory almost every command runs in — there is no such
# line at all.
# The line shortens a path under home to `~`, exactly as the command does, so
# this matches wherever the suite's scratch directory happens to sit.
WT_SHOWN=$WT
case "$WT/" in "$HOME"/*) WT_SHOWN="~${WT#"$HOME"}" ;; esac
says "pipeline list in the worktree names the checkout it answered for" \
  "checkout: $WT_SHOWN (task/pipeline-set)" \
  "$SPOOLWAY" -C "$WT" pipeline list
silent_about "pipeline list in the main checkout names no checkout" "checkout:" \
  "$SPOOLWAY" pipeline list
# -C naming the project explicitly is still the main checkout, so still silent.
silent_about "-C naming the project itself names no checkout" "checkout:" \
  "$SPOOLWAY" -C "$LIVE" pipeline list
# --json carries the same two facts as fields rather than prose, so a script
# reads them without parsing English.
says "--json in the worktree carries the checkout as a field" \
  '"branch":"task/pipeline-set"' \
  "$SPOOLWAY" -C "$WT" --json pipeline list
silent_about "--json in the worktree prints no prose line" "checkout: " \
  "$SPOOLWAY" -C "$WT" --json pipeline list
# doctor reports on both sides at once and still says it exactly once.
if [ "$("$SPOOLWAY" -C "$WT" doctor 2>&1 | grep -c '^checkout: ')" = "1" ]; then
  ok "doctor in the worktree names the checkout exactly once"
else
  bad "doctor in the worktree names the checkout exactly once"
fi
# The queue, the lane state and the lock stay one thing across every branch in
# flight, so `-C` naming the worktree still resolves the one project and its
# one shared queue.
if [ "$("$SPOOLWAY" -C "$WT" queue list)" = "$("$SPOOLWAY" queue list)" ]; then
  ok "queue list from the worktree still names the shared queue"
else
  bad "queue list from the worktree still names the shared queue"
fi

# `pipeline check` is the command you run to find out what is wrong with a
# pipeline file, so a project file that no longer parses must not be the thing
# that stops it answering. It reads the checkout in front of it and never the
# project's routing graph — the file below is the shape that actually caused
# this, a key spelled the way it was before `task_template:`. A command that
# does route still refuses on it, as it has to.
BROKEN="$LIVE/proj/.spoolway/pipelines/broken.yml"
printf 'template: default\nsteps:\n  - id: solo\n    end: true\n' > "$BROKEN"

works "pipeline check in the worktree survives a project file that does not parse" \
  "$SPOOLWAY" -C "$WT" pipeline check
refuses "pipeline check in the main checkout names the file that did not parse" "broken.yml" \
  "$SPOOLWAY" pipeline check
refuses "a command that routes off the project's graph still refuses" "broken.yml" \
  "$SPOOLWAY" queue list
# And in the main checkout, where the unreadable file actually is: a command
# that never routes is not stopped by it either. `stack` is the one this was
# found on — it opens a pull request and reads no graph at all — and `group
# list` stands in for it here, being the same shape with nothing to push.
works "a command that does not route survives the same file" \
  "$SPOOLWAY" group list

rm -f "$BROKEN"

must "removing the worktree" git worktree remove --force "$WT"
must "removing its branch" git branch -D task/pipeline-set

# -------------------------------------------------------------- the transcript
# With no pane to attach to, a lane's log is the only account of what it did —
# `spoolway lane` must serve it even after the lane itself is long gone.
says "a lane's transcript outlives it" "stand-in argv" "$SPOOLWAY" lane "land · review"
# The pane a transcript is headed by is labelled with the lane's own name
# (`<task> · <step>`, acceptance criterion 6) rather than the prompt.
says "and is headed by the turn and the lane's own name" "=== turn 1 (land · implement) ===" \
  "$SPOOLWAY" lane "land · implement"

# --------------------------------------------------------------- the slot counter
# One local slot, two runnable tasks: exactly one lane starts and the pass says
# what the other is waiting for.
must "one slot" "$SPOOLWAY" config set agents.pi.concurrency 1
# Both queued against a stopped dispatcher, so the pass that starts the first
# is the same pass that finds the second runnable and has nowhere to put it.
# Queued one at a time under a running one, the first could be started, done
# and out of the way before the second existed — and a slot nothing is
# contending for is not a slot counter under test.
dispatcher_stop
task_doc "$LIVE/par-one.md" par-one "$BODY" "group: live" "touches: [notes/par-one.md]"
must "a first parallel task"  "$SPOOLWAY" queue add --from "$LIVE/par-one.md"
task_doc "$LIVE/par-two.md" par-two "$BODY" "group: live" "touches: [notes/par-two.md]"
must "a second parallel task" "$SPOOLWAY" queue add --from "$LIVE/par-two.md"
dispatcher_start
if wait_for_text 30 "$E2E_DISPATCH_LOG" "waiting for a \`pi\` slot"; then
  ok "one slot admits one lane and says so"
else
  bad "one slot admits one lane and says so"
  tail -20 "$E2E_DISPATCH_LOG" | sed 's/^/        /'
fi
must "two slots again" "$SPOOLWAY" config set agents.pi.concurrency 2
dispatcher_restart   # a running one is still holding the config it started with
# Those two `config set`s edited `.spoolway/config.toml` in the base checkout.
# Nothing merges into it any more, but a task cut from it afterwards would
# inherit an uncommitted change nobody put in its diff — so the suite that made
# the edit commits it.
# Nothing to commit where the slot count came back to what it started as, which
# is the mock configuration exactly — so this asks git whether there is.
must "staging the slot change" git add .spoolway/config.toml
git diff --cached --quiet .spoolway/config.toml \
  || must "committing the slot change" git commit -qm "e2e: back to two slots"

if drive par-one gone 60 && drive par-two gone 60; then
  ok "both parallel tasks land once the slot frees"
else
  bad "both parallel tasks land once the slot frees"
fi

# ------------------------------------------------- the group gate ranks, it does not reserve
# `dispatch.priority = "group"` — the shipped default, unchanged above — used
# to drop a never-run group's candidates outright while some other group was
# already open, whether or not that open group had any ready work of its own.
# Three slots: `gate-chain`'s own task is started and running first, so its
# group is genuinely open — past `queued` — before either never-run group is
# even in the queue; only then are the other two queued, against a dispatcher
# already up. The old code would have left their two slots idle for as long
# as `gate-chain` stayed open; this one fills them the very next pass.
must "three slots" "$SPOOLWAY" config set agents.pi.concurrency 3
dispatcher_stop
task_doc "$LIVE/chain-a.md" chain-a "$BODY" "group: gate-chain" "touches: [notes/chain-a.md]"
task_doc "$LIVE/chain-b.md" chain-b "$BODY" "group: gate-chain" \
  "depends_on: [chain-a]" "touches: [notes/chain-b.md]"
must "the chain's first task" "$SPOOLWAY" queue add --from "$LIVE/chain-a.md"
must "the chain's second task, not yet ready" "$SPOOLWAY" queue add --from "$LIVE/chain-b.md"
dispatcher_start

if lane_pid "chain-a · implement" 30 >/dev/null; then
  ok "the chain's own task starts and its group counts as open"
else
  bad "the chain's own task starts and its group counts as open"
  tail -20 "$E2E_DISPATCH_LOG" | sed 's/^/        /'
fi

# Queued only now, against the dispatcher that already has `gate-chain` open
# and nothing else of it ready — `chain-b` is still waiting on `chain-a`.
task_doc "$LIVE/gate-untouched-a.md" gate-untouched-a "$BODY" \
  "group: gate-untouched-a" "touches: [notes/gate-untouched-a.md]"
must "a never-run group of its own" "$SPOOLWAY" queue add --from "$LIVE/gate-untouched-a.md"
task_doc "$LIVE/gate-untouched-b.md" gate-untouched-b "$BODY" \
  "group: gate-untouched-b" "touches: [notes/gate-untouched-b.md]"
must "a second never-run group" "$SPOOLWAY" queue add --from "$LIVE/gate-untouched-b.md"

if lane_pid "gate-untouched-a · implement" 30 >/dev/null \
  && lane_pid "gate-untouched-b · implement" 30 >/dev/null; then
  ok "both never-run groups start anyway, on the two slots gate-chain left free"
else
  bad "both never-run groups start anyway, on the two slots gate-chain left free"
  tail -20 "$E2E_DISPATCH_LOG" | sed 's/^/        /'
fi

must "back to two slots" "$SPOOLWAY" config set agents.pi.concurrency 2
dispatcher_restart
must "staging the slot change" git add .spoolway/config.toml
git diff --cached --quiet .spoolway/config.toml \
  || must "committing the slot change" git commit -qm "e2e: back to two slots, again"

# `chain-b` is a dependent task, so — exactly as `top` does in `stacking.sh`,
# and for the same reason — its own `handover` always blocks against this
# suite's bare-repo forge, on the one call that needs a real github.com
# remote. `blocked` is as far as this suite drives it; landing a stack all
# the way is `stacking.sh`'s scenario, not this one's.
if drive chain-a gone 60 && drive chain-b blocked 60 \
  && drive gate-untouched-a gone 60 && drive gate-untouched-b gone 60; then
  ok "the chain runs to its real end and both never-run groups land"
else
  bad "the chain runs to its real end and both never-run groups land"
fi

# --------------------------------------------------- blocked has one turn, not a loop
#
# No `covers:` tag of its own — CLI behaviour the map has no row for, same as
# the checkout scenario above. `--pause` (and, for the old habit, `--fail` or
# `--block`) reported from `blocked` has nowhere to go but `paused`: no real
# lane is needed to prove it, the same as `warmth.sh`'s own hand-blocked
# scenario proves the other road out of a block without spending one.
for outcome in pause block; do
  task="stuck-$outcome"
  {
    echo "---"
    echo "id: $task"
    echo "title: stuck, blocked by hand ($outcome)"
    echo "stage: blocked"
    echo "blocked_from: implement"
    echo "touches: [notes/$task.md]"
    echo "---"
    printf '## Goal\n\nAdd `notes/%s.md`.\n\n## Non-goals\n\nOut of scope.\n\n## Acceptance criteria\n\n- `notes/%s.md` exists.\n' \
      "$task" "$task"
  } > "$SPOOLWAY_PROJECT_HOME/queue/$task.md"

  OUT=$("$SPOOLWAY" report "$task" --"$outcome" -m "only the owner can clear this" 2>&1)
  if [ $? -eq 0 ]; then ok "reporting --$outcome from blocked"
  else bad "reporting --$outcome from blocked"; sed 's/^/        /' <<<"$OUT"; fi
  if grep -qF "nothing here could clear it" <<<"$OUT"; then
    ok "--$outcome from blocked says why it went to \`paused\`"
  else
    bad "--$outcome from blocked says why it went to \`paused\`"; sed 's/^/        /' <<<"$OUT"
  fi

  if [ "$(stage_of "$task")" = paused ]; then
    ok "--$outcome from blocked lands on \`paused\`, not back on \`blocked\`"
  else
    bad "--$outcome from blocked lands on \`paused\` (at \`$(stage_of "$task")\`)"
  fi
  has "paused_at names the step it originally blocked on" "paused_at: implement" \
    "$SPOOLWAY_PROJECT_HOME/queue/$task.md"
  has "blocked_from survives the pause, for the resume below to read" \
    "blocked_from: implement" "$SPOOLWAY_PROJECT_HOME/queue/$task.md"
done

# And resuming either one reaches exactly what a pass from `blocked` would
# have — `implement`'s own `on_pass` — not back onto `implement` to redo it.
must "resuming the paused task" "$SPOOLWAY" resume stuck-pause
if [ "$(stage_of stuck-pause)" = review ]; then
  ok "resuming a pause from \`blocked\` carries the task past where it blocked"
else
  bad "resuming a pause from \`blocked\` carries the task past where it blocked (at \`$(stage_of stuck-pause)\`)"
fi
rm -f "$SPOOLWAY_PROJECT_HOME/queue/stuck-pause.md" "$SPOOLWAY_PROJECT_HOME/queue/stuck-block.md"

finish
