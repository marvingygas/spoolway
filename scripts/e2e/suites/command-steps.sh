#!/usr/bin/env bash
# Command steps: a pipeline running something that is not an agent.
#
# A `run:` puts a plain command line in the graph — a build, a test run, a
# deploy script — with no model, no prompt and no worker slot. The key is the
# discriminator: there is no `kind:` restating it. Two shapes, and the whole
# feature is the difference between them:
#
#   blocking    the task waits, and the exit code picks on_pass or on_fail
#   background  the task moves on at once, and the command runs on behind it
#
# Everything here is about the dispatcher actually running processes, which is
# why it is an e2e suite rather than a unit test: the pipeline file is edited
# the way a project would edit it, and what is asserted afterwards is what the
# command left on disk.
#
# Split out of `commands.sh` (gh-253): that file grew to 282 checks across
# every domain a command step touches, and this is the one domain of the
# three it became — the mechanics of a `run:` step itself. `issue-tracking.sh`
# holds the `[issue_tracking]` cases, and `commands.sh` keeps what is left:
# scaffolding, the queue screen, the archive, config and the overrides layer.
# Every `# covers:` claim that was here stayed here; nothing moved to either
# sibling.
#
# The pane and headless-environment cases below need a real tmux server or
# `herdr-stub.sh`'s real long-lived shell, because a pane is the one thing
# only a real multiplexer can be asked whether it opened — see their own
# comments for why a stand-in cannot answer this instead.
#
# covers: step.run — a command step runs in the task`s worktree and routes on its exit code
# covers: step.background — the task moves on the same pass, and cleanup stops the command
# covers: step.timeout — a hung command is stopped at the step`s own bound, not the dispatcher`s
# covers: step.loop — a command step's own `loop:` bounds how many times the agent step behind it may send a task back in, the shape a mechanical CI gate is built on
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

BODY="$LIVE/body.md"
task_body "$BODY"

# Writes a command step into a pipeline file the way a project would, and
# points `implement` at it so a task actually walks through it.
add_command_step() {
  local pipeline=$1 id=$2 run=$3 on_pass=$4 extra=${5:-}
  local file=".spoolway/pipelines/$pipeline.yml"

  {
    printf '\n  - id: %s\n' "$id"
    printf '    description: A command step, doing whatever this project needs done here.\n'
    printf '    run: %s\n' "$run"
    case "$extra" in
      --background) printf '    background: true\n' ;;
      "")           ;;
      *)            printf '    on_fail: %s\n' "$extra" ;;
      # A failure routed back to the step ahead of this one is a cycle of
      # its own — `implement` → this step → `implement` — but it needs no
      # `loop:` of this step's own: a limit now counts arrivals at the step
      # it names, and `implement` already carries one, unconditionally, in
      # the shipped file this fixture starts from. Inserting a step ahead
      # of it changes nothing about that count.
    esac
    printf '    on_pass: %s\n' "$on_pass"
  } >> "$file"

  # `implement` goes through the new step now, so its `on_pass` names the new
  # step rather than `review`.
  sed -i "0,/^    on_pass: review$/s//    on_pass: $id/" "$file"
}

# Drops a task a case left blocked on a step its next case's pipeline no
# longer has. `spoolway pipeline check` names a live task its pipeline would
# strand, so the checks of every case after it would fail on a leftover rather
# than on their own pipeline.
#
# Taken out the way the case at the herdr stub does it: `dispatcher_stop`
# first, because `unqueue` refuses while a dispatcher is up that could be
# mid-turn on the task and a running one could write a deleted file back, then
# `--force` because a task that got as far as `blocked` has a worktree of its
# own. Deleting the queue file instead left that worktree and any live lane
# behind it. The next `drive` starts a dispatcher again.
forget() {
  local id
  dispatcher_stop
  for id in "$@"; do
    [ -e "$SPOOLWAY_PROJECT_HOME/queue/$id.md" ] || continue
    must "$id is taken out of the queue" "$SPOOLWAY" queue unqueue "$id" --force
  done
}

# The pristine pipeline, kept so each case below can put back whatever it
# edited. Every case works by writing a step into the shipped file the way a
# project would, then restoring this.
cp .spoolway/pipelines/default.yml "$LIVE/default.yml.bak"

# ------------------------------------------------------------- blocking, passing
# The ordinary case: a build between implement and review. The task waits for
# it, and a clean exit carries on down the pipeline.
add_command_step default build \
  "echo \"built \$SPOOLWAY_TASK\" | tee built.txt" review implement
works "a pipeline with a command step checks out" "$SPOOLWAY" pipeline check
says "and show reports it as a step that waits" "build      command   waits" \
  "$SPOOLWAY" pipeline show
says "with the line it will run" 'run: echo "built $SPOOLWAY_TASK"' \
  "$SPOOLWAY" pipeline show

# Every other task in this suite has a group of its own, named after itself: a
# group is one chain, and these are unrelated tasks, several left `blocked`
# while the next is queued — two roots of one group, which `queue add` refuses.
task_doc "$LIVE/land.md" land "$BODY" "group: live"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/land.md"

# `group list` reads `group:` verbatim off the queue — the same field `land`
# just declared, and the same task id it named.
says "group list names the group a queued task declared" "live" \
  "$SPOOLWAY" group list
says "and the task itself" "land" "$SPOOLWAY" group list

# Read while the task is still moving: the archive step reclaims this log.
records "the command's own record says it ran" "built land" \
  "$SPOOLWAY_PROJECT_HOME/commands/land · build.log" land

if drive land gone 180; then ok "a task runs straight through its command step"
else bad "a task runs straight through its command step (stuck at \`$(stage_of land)\`)"; fi
# What it wrote landed in the task's worktree and was committed with the work,
# which is the whole claim about where a command step runs. Read off the branch
# on the forge rather than out of the checkout: nothing merges back here any
# more, and the handed-over branch is where the change actually is.
if git -C "$FORGE/origin.git" show "task/land:built.txt" >/dev/null 2>&1; then
  ok "it ran in the task's worktree, so its output went over with the change"
else
  bad "it ran in the task's worktree, so its output went over with the change"
  git -C "$FORGE/origin.git" ls-tree --name-only task/land | sed 's/^/        /'
fi

# --------------------------------------------------------- a command step first
# A pipeline may *open* on one. A queued task is promoted to whatever its entry
# is, and nothing about a `run:` step needs a lane to have gone first — the
# command cuts the task's worktree itself on its way to running.
#
# This was the one position a command step could not hold. The entry went
# through the path that starts lanes, which has nothing to start for a step that
# takes no slot and passed over it in silence, so the task sat in `queued` and
# the board said `nothing to do` for as long as the dispatcher ran. A suite
# case, because what is asserted is a task moving rather than a file parsing.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
# Spliced in at the top rather than appended: first in the file is what makes a
# step the entry.
must "a pipeline that opens on a command step" \
  sed -i '0,/^steps:$/s##steps:\n\n  - id: prime\n    description: A command step at the entry, before any lane exists.\n    run: echo "primed $SPOOLWAY_TASK" | tee primed.txt\n    on_pass: implement\n    on_fail: implement\n#' \
  .spoolway/pipelines/default.yml
works "a pipeline whose entry is a command step checks out" "$SPOOLWAY" pipeline check

# `--dry-run` used to prove this position works with a throwaway pass that
# said what it would start and wrote nothing — see `run.sh`'s own note on its
# removal. The `records` call below already starts a real dispatcher (or
# restarts the stale one this suite's earlier pipeline edit left running,
# via its own config-stamp check) and catches the command in flight, which is
# the stronger, real-pass version of the same claim: nothing here was fabricated
# by a dry run.
task_doc "$LIVE/opener.md" opener "$BODY" "group: opener"
must "a task queued on it" "$SPOOLWAY" queue add --from "$LIVE/opener.md"

# Caught in flight, ahead of the archive step that reclaims this log.
records "the entry command ran, before any lane existed" "primed opener" \
  "$SPOOLWAY_PROJECT_HOME/commands/opener · prime.log" opener

if drive opener gone 180; then ok "a task whose entry is a command step does not sit in \`queued\`"
else bad "a task whose entry is a command step does not sit in \`queued\` (at \`$(stage_of opener)\`)"; fi
has "and the task went on to the step behind it" "→ \`implement\`" \
  $SPOOLWAY_PROJECT_HOME/archive/opener.md
# The worktree it wrote into is one it cut itself: no lane had run for this task
# when the entry step started, and a command step runs in the task's checkout or
# nowhere.
if git -C "$FORGE/origin.git" show "task/opener:primed.txt" >/dev/null 2>&1; then
  ok "and the checkout it wrote into was one it cut itself"
else
  bad "and the checkout it wrote into was one it cut itself"
fi

# --------------------------------------------------------------- routeless task
# There is no project default to fall back to any more, so a legacy or
# hand-edited document that reached the live queue with no resolvable
# `pipeline:` has to be caught before any lane starts, not discovered by the
# first lane unlucky enough to be picked for it. Written straight into the
# queue directory — the one shape `queue add --from`'s own refusal can never
# produce, since it never lets such a document reach the queue at all.
#
# Stopped first, and the stop is load-bearing: `drive opener` above started a
# dispatcher and left it running, and a `dispatch` that finds the lock held
# does not refuse — it prints the two already-running lines and exits 4 at
# once. That branch sits *above* the routing guard in `run`, so with the lock
# held this call never reaches `check_task_routes` at all, and "and names the
# task" below fails against the wrong message rather than proving the
# refusal it names. No `--dry-run` needed to keep this call to one pass that
# writes nothing: `check_task_routes` bails out ahead of the lock and every
# write, real pass or not.
dispatcher_stop
task_doc "$SPOOLWAY_PROJECT_HOME/queue/routeless.md" routeless "$BODY" \
  "stage: queued" "group: routeless" "pipeline:"
OUT=$("$SPOOLWAY" dispatch 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ]; then ok "dispatch refuses the whole start over a routeless task"
else bad "dispatch refuses the whole start over a routeless task"; fi
if grep -qF "refusing to start: task \`routeless\` has no \`pipeline:\`" <<<"$OUT"; then
  ok "and names the task"
else bad "and names the task"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF "set \`pipeline:\` to one of:" <<<"$OUT" && grep -qF "default" <<<"$OUT"; then
  ok "and lists the pipelines it could choose"
else bad "and lists the pipelines it could choose"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF "nothing was dispatched." <<<"$OUT"; then
  ok "and says nothing was dispatched"
else bad "and says nothing was dispatched"; sed 's/^/        /' <<<"$OUT"; fi
if [ "$(stage_of routeless)" = queued ]; then
  ok "and the routeless task never left queued"
else bad "and the routeless task never left queued (at \`$(stage_of routeless)\`)"; fi
rm -f "$SPOOLWAY_PROJECT_HOME/queue/routeless.md"

# ------------------------------------------------------------- blocking, failing
# A non-zero exit is a failure, and it takes the step's own `on_fail` — the
# whole point of putting a build in the graph is that a broken one routes to the
# step that fixes it rather than to a person.
#
# Where the task *went* is read off the archived file rather than caught in
# flight: a mock lane's turn is over in milliseconds, so a stage this suite
# polls for is a stage it can miss between two passes. The status log is the
# record of the route taken, and it cannot be raced.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
# Red on its first run and green on its second, so the detour this case is
# about is the whole of it: one failure back to `implement`, then a pass on
# down the pipeline. A command that stayed red would spend the bound above and
# park the task on `blocked`, which is the case `gated` below is for. The stamp
# lives outside the worktree so the retry is not itself a change the task
# carries.
#
# The `records` check below reads this log while the task is still moving,
# and both runs write to the same path — the second, passing run's silence
# overwrites the first run's line the moment the retry starts. A mock lane's
# retry is fast enough to win that race outright, so the first run holds a
# beat after it writes before it exits, giving the poll below room to see it.
add_command_step default build \
  "test -f '$LIVE/build.stamp' || { echo 'the build is broken' >&2; touch '$LIVE/build.stamp'; sleep 1; exit 2; }" \
  review implement

task_doc "$LIVE/broken.md" broken "$BODY" "group: broken"
must "a task whose build fails" "$SPOOLWAY" queue add --from "$LIVE/broken.md"

# Its stderr, read while the task is still looping — the archive step reclaims
# this log, so a `has` after `drive broken gone` would find nothing.
records "what the command printed is on the record" "the build is broken" \
  "$SPOOLWAY_PROJECT_HOME/commands/broken · build.log" broken

# The lane the failing build sent the task back to is briefed on it: the
# second `implement` lane's composed prompt names the command step and its
# log. The file is rewritten per launch, so this only matches the second one.
records "the lane sent back is told which command failed" \
  "The command step \`build\` failed" \
  "$SPOOLWAY_PROJECT_HOME/system-prompts/broken · implement.md" broken

if drive broken gone 180; then ok "a task whose command fails still reaches the end"
else bad "a task whose command fails still reaches the end (at \`$(stage_of broken)\`)"; fi
# Counted rather than matched: every task arrives at `implement` once on its
# way in, so the presence of that line says nothing. What the failing exit
# bought is a *second* arrival there, which is the detour itself.
arrivals() { grep -c "→ \`$2\`" "$1" 2>/dev/null || echo 0; }
if [ "$(arrivals $SPOOLWAY_PROJECT_HOME/archive/broken.md implement)" -eq 2 ]; then
  ok "and the failing exit routed it back to the step's on_fail"
else
  bad "and the failing exit routed it back to the step's on_fail (arrived at \`implement\` \
$(arrivals $SPOOLWAY_PROJECT_HOME/archive/broken.md implement) time(s), wanted 2)"
fi
# The one that passed took no such detour, which is what makes the count above
# evidence of the exit code rather than of the graph.
if [ "$(arrivals $SPOOLWAY_PROJECT_HOME/archive/land.md implement)" -eq 1 ]; then
  ok "a task whose command passed never went back there"
else
  bad "a task whose command passed never went back there (arrived at \`implement\` \
$(arrivals $SPOOLWAY_PROJECT_HOME/archive/land.md implement) time(s), wanted 1)"
fi

# ------------------------------------------------------------- a gate that never turns green
# The shape a mechanical CI gate is built on: a command step whose failure
# returns to the agent step behind it, and which carries its own `loop:` so a
# change that cannot be made green stops rather than circling forever. Nothing
# here is specific to `cargo` or to end-to-end suites — the graph is the whole
# of what is under test, so a stand-in agent and a command that is always red
# are enough to exercise it.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
{
  printf '\n  - id: e2e\n'
  printf "    description: A stand-in for a mechanical gate's own agent step.\n"
  printf '    agent: pi\n    prompt: implementer\n    model: fake-local\n'
  printf '    on_pass: gate\n    on_fail: blocked\n'
  printf '\n  - id: gate\n'
  printf '    description: Always red, so the loop it bounds is what this case is about.\n'
  printf "    run: echo 'CI would fail here' >&2; exit 1\n"
  printf '    loop: 1\n'
  printf '    on_pass: document\n    on_fail: e2e\n'
} >> .spoolway/pipelines/default.yml
# `review` fell through to `document` directly; put `e2e` and its gate between
# them. The `0,/…/` range keeps this to the first match — `review`'s, not the
# `gate` step's own `on_pass: document` appended just above.
sed -i "0,/^    on_pass: document\$/s//    on_pass: e2e/" .spoolway/pipelines/default.yml
works "a pipeline whose gate loops back to the agent step before it checks out" \
  "$SPOOLWAY" pipeline check

task_doc "$LIVE/gated.md" gated "$BODY" "group: gated"
must "a task whose gate never turns green" "$SPOOLWAY" queue add --from "$LIVE/gated.md"
if drive gated blocked 150; then
  ok "a gate that never passes stops the task rather than circling forever"
else
  bad "a gate that never passes stops the task rather than circling forever (at \`$(stage_of gated)\`)"
fi
has "it routed back to the step before the gate, not past it" "→ \`e2e\`" \
  $SPOOLWAY_PROJECT_HOME/queue/gated.md
lacks "and never reached the step the gate guards" "→ \`document\`" \
  $SPOOLWAY_PROJECT_HOME/queue/gated.md
counter "the round the loop bounds is what actually stopped it, not a guess" \
  rounds "gate->e2e" 1 $SPOOLWAY_PROJECT_HOME/queue/gated.md
# And the refusal is `gate`'s own limit, read off its arrivals: `e2e`'s pass is
# what would have carried the task there a second time, and a pass into a
# spent step is refused the same as a fail.
has "and it was gate's own loop: 1 that refused the second arrival" \
  "\`e2e\` may not send this to \`gate\` a 2nd time — \`gate\` has \`loop: 1\`" \
  $SPOOLWAY_PROJECT_HOME/queue/gated.md

# The same shape with `gate`'s own `loop: 2` rather than `1` — every shipped
# pipeline now carries 2, not 1, so a route bounded at 2 is what a reviewer's
# fix actually gets: seen once before the budget is spent, not zero times. The
# exit is taken on the third arrival rather than the second — two allowed
# laps banked, and only the third attempt diverted.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
{
  printf '\n  - id: e2e\n'
  printf "    description: A stand-in for a mechanical gate's own agent step.\n"
  printf '    agent: pi\n    prompt: implementer\n    model: fake-local\n'
  printf '    on_pass: gate\n    on_fail: blocked\n'
  printf '\n  - id: gate\n'
  printf '    description: Always red, so the loop it bounds is what this case is about.\n'
  printf "    run: echo 'CI would fail here' >&2; exit 1\n"
  printf '    loop: 2\n'
  printf '    on_pass: document\n    on_fail: e2e\n'
} >> .spoolway/pipelines/default.yml
sed -i "0,/^    on_pass: document\$/s//    on_pass: e2e/" .spoolway/pipelines/default.yml
works "a pipeline whose gate loops back to the agent step, bounded at 2" \
  "$SPOOLWAY" pipeline check

task_doc "$LIVE/gated-twice.md" gated-twice "$BODY" "group: gated-twice"
must "a task whose gate never turns green, on a loop of 2" \
  "$SPOOLWAY" queue add --from "$LIVE/gated-twice.md"
if drive gated-twice blocked 150; then
  ok "a loop of 2 still stops the task rather than circling forever"
else
  bad "a loop of 2 still stops the task rather than circling forever \
(at \`$(stage_of gated-twice)\`)"
fi
# `arrivals` counts every line naming `→ \`e2e\``, and this log carries three
# of them: the entry from `review`, and the two laps `gate` sent back. The
# note `apply_loop_budget` writes on the third attempt is *not* among them —
# it reads "`e2e` may not send this to `gate` a 3rd time", naming the move it
# is refusing rather than one it made, so it no longer answers a grep for
# arrivals. The two real laps are what the `rounds` counter below proves; this
# only checks that the third attempt left no further arrival behind it.
if [ "$(arrivals $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md e2e)" -eq 3 ]; then
  ok "the third attempt spent the budget rather than arriving at e2e again"
else
  bad "the third attempt spent the budget rather than arriving at e2e again \
(arrived at \`e2e\` $(arrivals $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md e2e) \
time(s), wanted 3)"
fi
counter "and the counter agrees: two laps banked, not one" \
  rounds "gate->e2e" 2 $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md
has "and it was gate's own loop: 2 that refused the third arrival" \
  "\`e2e\` may not send this to \`gate\` a 3rd time — \`gate\` has \`loop: 2\`" \
  $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md

forget gated gated-twice

# --------------------------------------------------------- a launch that never succeeds
# The other half of a command step's own trouble: not one that ran and
# failed, like `build` and `gate` above, but one that never got to run at
# all. Before `nothing-starts-silently`, a `Fresh` arrival that could not
# even start stayed `Fresh` forever — the same arrival, retried every pass
# for the life of the run, with the same line landing in the problem log
# every single time. Now it is bounded the same way a loop is: three
# attempts, then routed to the step's own `on_fail` exactly as a failing
# exit code would be.
#
# The obstruction is a directory sitting where the run's own log file needs
# to go, with its `.prev.log` slot pre-occupied too so `Runs::prepare`'s own
# roll-aside cannot clear it out of the way — a spawn that never starts
# because its bookkeeping cannot be written, which needs no multiplexer and
# no missing binary to reproduce.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default launchfail "echo should-never-run" review ""
works "a pipeline with a step whose launch can fail checks out" \
  "$SPOOLWAY" pipeline check

KEY="cant-launch · launchfail"
mkdir -p "$SPOOLWAY_PROJECT_HOME/commands"
mkdir -p "$SPOOLWAY_PROJECT_HOME/commands/$KEY.log"
mkdir -p "$SPOOLWAY_PROJECT_HOME/commands/$KEY.prev.log"
touch "$SPOOLWAY_PROJECT_HOME/commands/$KEY.prev.log/keep-this-slot-occupied"

task_doc "$LIVE/cant-launch.md" cant-launch "$BODY" "group: cant-launch"
must "a task whose command step can never even start" \
  "$SPOOLWAY" queue add --from "$LIVE/cant-launch.md"

if drive cant-launch blocked 120; then
  ok "three failed launches in a row park the task rather than retrying forever"
else
  bad "three failed launches in a row park the task rather than retrying forever \
(at \`$(stage_of cant-launch)\`)"
fi
has "routed to the step's own on_fail, same as a failing exit code would be" \
  "→ \`blocked\`" $SPOOLWAY_PROJECT_HOME/queue/cant-launch.md
has "blocked_from names the step that could not start, for a resume to reach" \
  "blocked_from: launchfail" $SPOOLWAY_PROJECT_HOME/queue/cant-launch.md
counter "and the launch-failure count agrees: three, not one per pass forever" \
  launch_failures launchfail 3 $SPOOLWAY_PROJECT_HOME/queue/cant-launch.md

REASON="could not be started after 3 attempts"
STATUS_HITS=$(grep -c "$REASON" "$SPOOLWAY_PROJECT_HOME/queue/cant-launch.md" 2>/dev/null || echo 0)
if [ "$STATUS_HITS" -eq 1 ]; then
  ok "the reason lands on the task's own Status Log exactly once"
else
  bad "the reason lands on the task's own Status Log exactly once (found $STATUS_HITS)"
fi

# `problem_log::path` keys this off `$SPOOLWAY_PROJECT_HOME`'s own
# basename (`<label>-<id>`) now, not the checkout's plain basename — see
# `src/problem_log.rs` and the `binding-record` task.
PROBLEM_LOG="$HOME/.spoolway/logs/$(basename "$SPOOLWAY_PROJECT_HOME").log"
PROBLEM_HITS=$(grep -c "$REASON" "$PROBLEM_LOG" 2>/dev/null || echo 0)
if [ "$PROBLEM_HITS" -eq 1 ]; then
  ok "and once in the project's problem log — not once per one of the three attempts"
else
  bad "and once in the project's problem log — not once per one of the three attempts \
(found $PROBLEM_HITS in $PROBLEM_LOG)"
fi

# Parked, so the dispatcher has nothing left to do here for the rest of the
# run — this is what "the run ends" means for a step that can never launch:
# not the process exiting (a person still has to clear a block), but the
# retry loop itself stopping rather than spending another pass on the same
# dead end. A few more passes with the obstruction still in place, and
# neither the count nor the reason moves again.
sleep 3
counter "further passes spend nothing more on it — the count does not move" \
  launch_failures launchfail 3 $SPOOLWAY_PROJECT_HOME/queue/cant-launch.md
PROBLEM_HITS_AFTER=$(grep -c "$REASON" "$PROBLEM_LOG" 2>/dev/null || echo 0)
if [ "$PROBLEM_HITS_AFTER" -eq 1 ]; then
  ok "nor does the problem log grow while it sits blocked"
else
  bad "nor does the problem log grow while it sits blocked (found $PROBLEM_HITS_AFTER)"
fi

forget cant-launch

# ------------------------------------------- a launch that fails for another reason
# The other kind of launch that never succeeds: the step is fine, and what
# stops it is around it. Here `task/branch-taken` appears after the task queues and the task
# has no checkout on record, so the worktree cannot be cut. `implement` is
# given an `on_fail` for this case, which is what tells the two roads apart:
# sent down it, the task would be routed as though `implement` had run and
# failed. It belongs in front of a person on `blocked`, with `blocked_from`
# naming the step that could not start.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
sed -i '0,/^    loop: 2$/s//    loop: 2\n    on_fail: review/' .spoolway/pipelines/default.yml
works "a pipeline whose first step names an on_fail checks out" \
  "$SPOOLWAY" pipeline check

task_doc "$LIVE/branch-taken.md" branch-taken "$BODY" "group: branch-taken"
must "a task whose branch name is free when it queues" \
  "$SPOOLWAY" queue add --from "$LIVE/branch-taken.md"
# `queue add` refuses an id whose branch is already there, so the branch is
# taken after the task is queued and before the dispatcher cuts it.
git branch task/branch-taken HEAD
if drive branch-taken blocked 120; then
  ok "three failed launches that are not the step's fault park the task on blocked"
else
  bad "three failed launches that are not the step's fault park the task on blocked \
(at \`$(stage_of branch-taken)\`)"
fi
has "blocked_from names the step that could not start" \
  "blocked_from: implement" $SPOOLWAY_PROJECT_HOME/queue/branch-taken.md
lacks "and the task never went down the step's on_fail" \
  "→ \`review\`" $SPOOLWAY_PROJECT_HOME/queue/branch-taken.md
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
forget branch-taken

# --------------------------------------------------- report --pass --stage, off blocked
# `spoolway report --pass --stage <step>` names where a cleared block lands,
# bounded by the steps this task has actually run — its own `steps:`, the
# launch record a command step now banks into exactly the way an agent lane
# does (see `add_command_step` above). A real mock-agent `implement` lane
# and a real failing command step give this task two entries in that record
# before it ever reaches `blocked`, so both halves of the flag below are
# proved against a run that actually happened, not a hand-placed fixture.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default failstep "exit 1" review ""
works "a pipeline with a command step that blocks on failure checks out" \
  "$SPOOLWAY" pipeline check

task_doc "$LIVE/stage-flag.md" stage-flag "$BODY" "group: stage-flag"
must "a task that will run implement, then block on a failing command step" \
  "$SPOOLWAY" queue add --from "$LIVE/stage-flag.md"
if drive stage-flag blocked 60; then
  ok "the task ran implement, then blocked on the failing command step"
else
  bad "the task ran implement, then blocked on the failing command step \
(at \`$(stage_of stage-flag)\`)"
fi
has "the mock agent's lane at implement landed in the launch record" \
  "queued->implement: 1" $SPOOLWAY_PROJECT_HOME/queue/stage-flag.md
has "and so did the command step's own run, now that it banks one too" \
  "implement->failstep: 1" $SPOOLWAY_PROJECT_HOME/queue/stage-flag.md

OUT=$("$SPOOLWAY" report stage-flag --pass --stage review -m "done" 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ] && grep -qF "has never been at \`review\`" <<<"$OUT" \
  && grep -qF "\`implement\`, \`failstep\`" <<<"$OUT"; then
  ok "naming a step this task has never been at is refused, naming the steps it has run"
else
  bad "naming a step this task has never been at is refused, naming the steps it has run \
(exit $STATUS)"
  sed 's/^/        /' <<<"$OUT"
fi
if [ "$(stage_of stage-flag)" = blocked ]; then
  ok "and the refused report left the task exactly where it was"
else
  bad "and the refused report left the task exactly where it was \
(at \`$(stage_of stage-flag)\`)"
fi

works "naming a step this task has run moves it there" \
  "$SPOOLWAY" report stage-flag --pass --stage implement -m "the fix belongs to the implementer"
if [ "$(stage_of stage-flag)" = implement ]; then
  ok "and it lands exactly on the named step, not carried past it"
else
  bad "and it lands exactly on the named step, not carried past it \
(at \`$(stage_of stage-flag)\`)"
fi

# ------------------------------------------------------------------- background
# The other half: the task does not wait, and the command is still going after
# it has moved on.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default bench \
  "sleep 240; echo 'never finishes in time' > \"\$SPOOLWAY_REPO/bench-done.txt\"" \
  review --background
# An ordinary waiting step immediately behind it, held on a release file the
# suite touches once it has read what spoolway wrote about the background
# run. Everything asserted below — the pid under `commands/`, and the
# process that pid names — lives only as long as the task does, and
# `teardown.rs` reclaims `commands/` on the archive.
#
# `quick` used to be watched by hand for that, on the reasoning that its walk
# from `bench` to `done` cost a real interval per transition and so could be
# sampled on the way. It cannot any more: the dispatcher wakes on a change
# rather than waiting out the interval it announces, and this suite's own
# dispatch log shows `bench` starting and the task reaching `create-pr` inside
# a single pass — the whole window closed between two 0.2s reads, and the
# pid was read as empty.
#
# It holds the task, never the background command: `bench` is already gone
# about its own business by the time this step starts, which is the claim
# below about `bench-done.txt`.
BENCH_RELEASE="$LIVE/bench.release"
rm -f "$BENCH_RELEASE"
add_command_step default hold \
  "for _ in \$(seq 1 1200); do [ -e \"$BENCH_RELEASE\" ] && break; sleep 0.1; done" \
  review
works "a background command step checks out" "$SPOOLWAY" pipeline check
says "and show says it does not wait" "bench      command   background" \
  "$SPOOLWAY" pipeline show

dispatcher_restart   # the pipeline it is holding has no `bench` step in it
task_doc "$LIVE/quick.md" quick "$BODY" "group: quick"
must "a task with a background step" "$SPOOLWAY" queue add --from "$LIVE/quick.md"

# Read while `hold` keeps the task in the queue, so `commands/` is still
# there to read from. What spoolway wrote, not what the command wrote about
# itself: that a pid was recorded at all is half of what this case is about.
BENCH_PID=""
if poll_until 300 test -s "$SPOOLWAY_PROJECT_HOME/commands/quick · bench.pid"; then
  BENCH_PID=$(head -n1 "$SPOOLWAY_PROJECT_HOME/commands/quick · bench.pid")
fi

if [ -n "$BENCH_PID" ]; then ok "the background command really was started"
else bad "the background command really was started"; fi

# Read, so `hold` may let go and the task go on to `done`. `bench` itself is
# four minutes from finishing and nothing below waits for it — which is the
# next thing asserted.
touch "$BENCH_RELEASE"
if drive quick gone 300; then
  ok "the task ran the whole pipeline without waiting for it"
else
  bad "the task ran the whole pipeline without waiting for it (at \`$(stage_of quick)\`)"
fi
# The command sleeps for four minutes — comfortably longer than the rest of
# the pipeline takes to reach `done` even at one PROBE_INTERVAL per
# transition — so if this file exists, something waited.
if [ -f "$LIVE/proj/bench-done.txt" ]; then
  bad "nothing waited for it — that is what background means"
else
  ok "nothing waited for it — that is what background means"
fi

# `kill -0` rather than a `/proc/<pid>` directory, which macOS has none of.
# A background run outlives the step that started it and must not outlive the
# worktree it is running in: cleanup takes it down with the task.
if [ -n "$BENCH_PID" ] && poll_while 10 kill -0 "$BENCH_PID"; then
  ok "cleanup stops a background command rather than orphaning it"
else
  bad "cleanup stops a background command rather than orphaning it (pid $BENCH_PID)"
fi

# ------------------------------------------------------- background, its end
# A background run never moves its task, so its failure would reach nobody:
# the dispatcher's own report scrolls away. The line in the task's
# `## Status Log` is what a person or a later agent can still read.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default sidecar "exit 3" review --background
SIDECAR_RELEASE="$LIVE/sidecar.release"
rm -f "$SIDECAR_RELEASE"
add_command_step default hold \
  "for _ in \$(seq 1 1200); do [ -e \"$SIDECAR_RELEASE\" ] && break; sleep 0.1; done" \
  review
dispatcher_restart
task_doc "$LIVE/sidecar.md" sidecar "$BODY" "group: sidecar"
must "a task with a failing background step" "$SPOOLWAY" queue add --from "$LIVE/sidecar.md"

# Read while `hold` keeps the task in the queue, so its file is still there.
if poll_until 300 grep -q "(background) exited 3" "$SPOOLWAY_PROJECT_HOME/queue/sidecar.md"; then
  ok "a background command that exits non-zero leaves its line in the Status Log"
else
  bad "a background command that exits non-zero leaves its line in the Status Log"
fi
if [ "$(stage_of sidecar)" = "hold" ]; then
  ok "and the task is still on the step it was on"
else
  bad "and the task is still on the step it was on (at \`$(stage_of sidecar)\`)"
fi
touch "$SIDECAR_RELEASE"
drive sidecar gone 300 || true
forget sidecar
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
dispatcher_restart

# --------------------------------------------------------- background, on_fail
# A background run is a sidecar, so a route on its exit is refused at load:
# `pipeline check` says so and names both ways out.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
{
  printf '\n  - id: scratch\n'
  printf '    description: A background step that also declares on_fail.\n'
  printf '    run: exit 1\n'
  printf '    background: true\n'
  printf '    on_pass: review\n'
  printf '    on_fail: blocked\n'
} >> .spoolway/pipelines/default.yml
sed -i "0,/^    on_pass: review\$/s//    on_pass: scratch/" .spoolway/pipelines/default.yml
refuses "a background step that declares on_fail is refused" \
  "step \`scratch\` runs in the background but declares \`on_fail\`" \
  "$SPOOLWAY" pipeline check
refuses "and the refusal offers to keep it a sidecar" \
  "keep it a sidecar:  delete \`on_fail:\`" "$SPOOLWAY" pipeline check
refuses "and to make it a check" \
  "make it a check:    delete \`background:\`, and the step blocks" \
  "$SPOOLWAY" pipeline check
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml

# ----------------------------------------------------------- headless, no setsid
# The reason this task exists: detaching a headless command step used to run
# `setsid sh -c`, and macOS ships no `setsid` binary. It now calls
# `libc::setsid()` itself, in the child between fork and exec, so it owes
# nothing to PATH — proven here by building a PATH with every real command
# symlinked in except that one, and running the dispatcher through it.
NO_SETSID_BIN="$LIVE/no-setsid-bin"
mkdir -p "$NO_SETSID_BIN"
IFS=: read -ra _real_path_dirs <<<"$PATH"
for _dir in "${_real_path_dirs[@]}"; do
  [ -d "$_dir" ] || continue
  for _bin in "$_dir"/*; do
    [ -e "$_bin" ] || continue
    _name=$(basename "$_bin")
    [ "$_name" = setsid ] && continue
    [ -e "$NO_SETSID_BIN/$_name" ] && continue
    ln -s "$_bin" "$NO_SETSID_BIN/$_name" 2>/dev/null
  done
done
unset _dir _bin _name _real_path_dirs
if PATH="$NO_SETSID_BIN" command -v setsid >/dev/null 2>&1; then
  bad "the stand-in PATH really has no setsid on it"
else
  ok "the stand-in PATH really has no setsid on it"
fi

# A wrapper binary rather than exporting PATH for the whole suite: what has to
# lose `setsid` is the dispatcher and everything it forks, not `lib.sh`'s own
# `dispatcher_start`, which detaches its supervisor with a literal `setsid`
# of its own — a harness concern, not the thing this task changed.
NO_SETSID_SPOOLWAY="$LIVE/spoolway-no-setsid"
cat >"$NO_SETSID_SPOOLWAY" <<SHIM
#!/bin/sh
PATH="$NO_SETSID_BIN"
export PATH
exec "$SPOOLWAY" "\$@"
SHIM
chmod +x "$NO_SETSID_SPOOLWAY"

cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default nosetsid \
  "sleep 240; echo 'no-setsid done' > \"\$SPOOLWAY_REPO/no-setsid-done.txt\"" \
  review --background
# The same hold the `bench` case above takes, for the same reason and with
# the same reasoning behind it: the pid this case is entirely about is
# written under `commands/`, and `commands/` goes when the task does.
NO_SETSID_RELEASE="$LIVE/no-setsid.release"
rm -f "$NO_SETSID_RELEASE"
add_command_step default hold \
  "for _ in \$(seq 1 1200); do [ -e \"$NO_SETSID_RELEASE\" ] && break; sleep 0.1; done" \
  review
must "marking it hide: true" \
  sed -i 's|^    background: true$|    background: true\n    hide: true|' \
  .spoolway/pipelines/default.yml
works "a headless background command step checks out" "$SPOOLWAY" pipeline check

REAL_SPOOLWAY="$SPOOLWAY"
SPOOLWAY="$NO_SETSID_SPOOLWAY"
dispatcher_restart   # both the new step and the setsid-less binary are new
task_doc "$LIVE/nosetsid.md" nosetsid "$BODY" "group: nosetsid"
must "a task with a headless step, dispatched with no setsid on PATH" \
  "$SPOOLWAY" queue add --from "$LIVE/nosetsid.md"

# Read while `hold` keeps the task in the queue — the same window the
# `bench` case above makes, and made for the same reason: a background
# step's whole task can archive inside the same pass that started it,
# taking `commands/` and this pid file with it.
NOSETSID_PID=""
if poll_until 60 test -s "$SPOOLWAY_PROJECT_HOME/commands/nosetsid · nosetsid.pid"; then
  NOSETSID_PID=$(head -n1 "$SPOOLWAY_PROJECT_HOME/commands/nosetsid · nosetsid.pid")
fi

if [ -n "$NOSETSID_PID" ]; then
  ok "a headless command step still starts and writes its pid with no setsid on PATH"
else
  bad "a headless command step still starts and writes its pid with no setsid on PATH"
fi
# Caught right as the pid file appears: the process is up on its own, with
# nobody waiting on it, which is what outliving the pass that spawned it
# means.
if [ -n "$NOSETSID_PID" ] && kill -0 "$NOSETSID_PID" 2>/dev/null; then
  ok "and it outlives the pass that started it"
else
  bad "and it outlives the pass that started it (pid $NOSETSID_PID)"
fi
# Cleaned up the same way `bench` is, rather than left running into whatever
# this suite does next — so `hold` is let go first, and the task still has
# to walk review, document and create-pr to reach `done` after that.
touch "$NO_SETSID_RELEASE"
if [ -n "$NOSETSID_PID" ] && poll_while 300 kill -0 "$NOSETSID_PID"; then
  ok "and cleanup stops it once the task is done, same as any other background run"
else
  bad "and cleanup stops it once the task is done, same as any other background run (pid $NOSETSID_PID)"
fi

# Back to the real binary before anything later in this suite reads $SPOOLWAY.
SPOOLWAY="$REAL_SPOOLWAY"
dispatcher_restart

# -------------------------------------------------------------------- timeout
# The hang. A blocking command that never ends would park its task for as long
# as the dispatcher runs — no other clock in a pass has an opinion about a
# process that is simply still going — so the step's own timeout is the only
# thing that ends it.
#
# Hangs once and returns at once the second time, same as `broken` above and
# for the same reason: a command that kept hanging would spend the loop bound
# and park the task on `blocked` rather than let it reach `gone`. The stamp is
# written before the sleep so the timeout still kills the first run.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default build \
  "test -f '$LIVE/hang.stamp' && exit 0 || { touch '$LIVE/hang.stamp'; sleep 300; }" \
  review implement
must "a timeout short enough for a suite to reach" \
  sed -i "\\|^    run: test -f '$LIVE/hang.stamp'|a\\    timeout: 3s" \
  .spoolway/pipelines/default.yml
works "a step may name its own timeout" "$SPOOLWAY" pipeline check
says "and show resolves it" "timeout=3s" "$SPOOLWAY" pipeline show

task_doc "$LIVE/hung.md" hung "$BODY" "group: hung"
must "a task whose command hangs" "$SPOOLWAY" queue add --from "$LIVE/hung.md"
if drive hung gone 200; then ok "a hung command does not park its task forever"
else bad "a hung command does not park its task forever (at \`$(stage_of hung)\`)"; fi
has "and the timeout routed it like any other failure" "→ \`implement\`" \
  $SPOOLWAY_PROJECT_HOME/archive/hung.md

# Every command step is bounded, including the ones that never say so — which
# is what makes the guarantee worth anything.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default build "make" review implement
says "a step that names no timeout still has one" "timeout=30m" \
  "$SPOOLWAY" pipeline show

# `timeout: 0s` reads as "no limit" and would mean the opposite.
must "a zero timeout" \
  sed -i 's|^    run: make$|    run: make\n    timeout: 0s|' \
  .spoolway/pipelines/default.yml
refuses "a zero timeout is refused rather than read as no limit" \
  "as soon as it started" "$SPOOLWAY" pipeline check

# ----------------------------------------------------------------- no sandbox
# Stated as a test because it is a decision, not an accident: a command step is
# the operator's own command from a file only people write, and it runs
# unconfined. A lane doing this is refused by the kernel; this is not a lane.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
OUTSIDE="$LIVE/outside-every-worktree.txt"
add_command_step default build "echo reached > '$OUTSIDE'" review implement

task_doc "$LIVE/unconfined.md" unconfined "$BODY" "group: unconfined"
must "a task whose command writes outside its worktree" \
  "$SPOOLWAY" queue add --from "$LIVE/unconfined.md"
if drive unconfined gone 180; then ok "a command step is not confined to its worktree"
else bad "a command step is not confined to its worktree (at \`$(stage_of unconfined)\`)"; fi
works "and it really did write where no lane could" test -f "$OUTSIDE"

# ------------------------------------------------------------------ a pane, or none
# By default, and with `dispatch.hide_commands` set to `false`, a command step
# with no `hide:` runs in a pane of its own, under a real multiplexer. With
# `hide_commands = true`, and with `hide: true`, it runs silent and detached.
# With `hide_commands = true`, a step with `hide: false` still gets its pane.
# None of the three is provable against the headless backend the rest of this suite runs on — a
# pane is the one thing only a real pane can be asked whether it opened — so
# this switches to `herdr`, against `scripts/e2e/herdr-stub.sh`, for as long
# as the case needs. Against the
# double rather than a real server: its header says why there is no isolated
# herdr to run this on.
HSTATE="$LIVE/herdr-stub-pane"
HERDRBIN="$LIVE/herdr-bin-pane"
mkdir -p "$HSTATE" "$HERDRBIN"
install -m 755 "$HERE/../herdr-stub.sh" "$HERDRBIN/herdr"
export HERDR_STUB_STATE="$HSTATE"
PATH_BEFORE_HERDR_STUB="$PATH"
PATH="$HERDRBIN:$PATH"; export PATH

# Any value but `headless` runs herdr, here the double above; `1` also keeps
# the test-only gates open, as the harness's own export does.
export SPOOLWAY_TEST_BACKEND=1
# Commands are shown by default, which opens a pane; the cases below are about
# that pane, so they read the default and set nothing.

# The pane gate itself, proven end to end rather than only by the unit tests
# in src/commands/dispatch.rs — those already know the answer they are
# asking `Mux::in_own_pane` for; this asks the double for real. Stopped
# first: a resident dispatcher left running from the headless section above
# would find its own lock held and exit 4 without ever reaching
# `check_dispatcher_visible`, so "and names why" below would fail against
# the wrong message rather than proving the refusal it names — the same
# hazard the routeless-task case earlier in this file guards against.
# `HERDR_STUB_NO_PANE` is this one call's own — every other `dispatch` in
# this suite runs without it, and the double answers "there is a pane" by
# default for exactly that reason.
dispatcher_stop
task_doc "$LIVE/paneless.md" paneless "$BODY" "group: paneless"
must "a task queued ahead of the pane gate" "$SPOOLWAY" queue add --from "$LIVE/paneless.md"
# The rest of what the refusal promises — no repo lock taken, nothing written
# to the task file — read as "these two files are byte for byte what they
# were", which is stronger than the stage line alone and survives a lock file
# left behind by the `dispatcher_stop` above. `Lock::acquire` rewrites
# `dispatch.pid` with the run's own pid, so a gate that let the run through
# could not leave it untouched.
PANE_LOCK_BEFORE="$LIVE/pane-lock.before"
PANE_DOC_BEFORE="$LIVE/pane-doc.before"
cp "$SPOOLWAY_PROJECT_HOME/dispatch.pid" "$PANE_LOCK_BEFORE" 2>/dev/null || : > "$PANE_LOCK_BEFORE"
cp "$SPOOLWAY_PROJECT_HOME/queue/paneless.md" "$PANE_DOC_BEFORE"
OUT=$(HERDR_STUB_NO_PANE=1 "$SPOOLWAY" dispatch 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ]; then ok "dispatch refuses outside a herdr pane, herdr backend included"
else bad "dispatch refuses outside a herdr pane, herdr backend included"; fi
if grep -qF "open herdr and start spoolway there:" <<<"$OUT"; then
  ok "and names why"
else bad "and names why"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qx "  herdr" <<<"$OUT" && grep -qx "  spoolway" <<<"$OUT"; then
  ok "and names both commands as the way in"
else bad "and names both commands as the way in"; sed 's/^/        /' <<<"$OUT"; fi
if [ "$(stage_of paneless)" = queued ]; then
  ok "and the queued task never left queued"
else
  bad "and the queued task never left queued (at \`$(stage_of paneless)\`)"
fi
cp "$SPOOLWAY_PROJECT_HOME/dispatch.pid" "$LIVE/pane-lock.after" 2>/dev/null \
  || : > "$LIVE/pane-lock.after"
if cmp -s "$PANE_LOCK_BEFORE" "$LIVE/pane-lock.after"; then
  ok "and took no repo lock on the way out"
else bad "and took no repo lock on the way out"; fi
if cmp -s "$PANE_DOC_BEFORE" "$SPOOLWAY_PROJECT_HOME/queue/paneless.md"; then
  ok "and wrote nothing at all to the task file"
else
  bad "and wrote nothing at all to the task file"
  diff "$PANE_DOC_BEFORE" "$SPOOLWAY_PROJECT_HOME/queue/paneless.md" | sed 's/^/        /'
fi

# An ordinary `dispatch` outside a pane, with no flag to be an exemption:
# there is no board to draw any more, only the one plain run, so the gate
# is checked before any argv reaches it.
OUT=$(HERDR_STUB_NO_PANE=1 "$SPOOLWAY" dispatch 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ] \
   && grep -qF "open herdr and start spoolway there:" <<<"$OUT"; then
  ok "and an ordinary dispatch is refused the same way, not exempted"
else bad "and an ordinary dispatch is refused the same way, not exempted"; sed 's/^/        /' <<<"$OUT"; fi

# Taken back out rather than left to be picked up for real: its pipeline is
# an ordinary agent-starting one, and this suite has nothing to report for
# its lane, which `herdr-stub.sh` only registers — the resident dispatcher the
# next scenario starts would just find it stuck.
must "the pane-gate task is taken back out" "$SPOOLWAY" queue unqueue paneless

# A pane's shell starts life with the *stub server's* environment, never the
# dispatcher's — unlike a headless run's child process, which inherits by
# ordinary fork/exec. `Mux::run_in_pane` is handed the dispatcher's own
# environment as an inherited layer for exactly this reason, and this is the
# one variable set here specifically so nothing on the harness's own PATH
# could already carry it: a false pass would mean nothing.
export SPOOLWAY_E2E_PANE_ENV_MARKER="from-the-dispatchers-own-environment"

# The visible half: no `hide:` key, so the command gets a pane of its
# own, and it stands until this suite lets it go.
#
# A made window, not a caught one — the same release-file shape the herdr
# pane case below uses, and for the same reason: `queued → visible → resume →
# archived` is over inside a moment otherwise, and `teardown.rs` reclaims
# `commands/` on the archive. Everything read below — the pane file, the log,
# and the `.kept` copy the env-marker check greps — is written while the step
# is running, and a `sleep 2` was only ever a guess at how long that would
# take. It stopped being long enough once a pass got fast: the log was
# reclaimed out from under `records`, whose `cp` then left no `.kept` at all.
# `timeout:` is the backstop, so a suite that dies before releasing either
# step does not leave a pane waiting forever.
#
# A second step, `resume`, follows `visible` before either routes to `done` —
# not the `default` pipeline's `implement` → this step chain the tmux-backed
# version of this case used, because that starts on an agent lane, and
# `herdr-stub.sh` only registers one: it is here for the handover, not for a
# full agent lifecycle (see its own header), and `resume`
# is the cheap stand-in for one: a later pass finding this task already
# sitting on its own tab, the same as a resumed dispatcher would.
VISIBLE_RELEASE="$LIVE/visible.release"
RESUME_RELEASE="$LIVE/resume.release"
rm -f "$VISIBLE_RELEASE" "$RESUME_RELEASE"
sed -e "s|@RELEASE@|$VISIBLE_RELEASE|" -e "s|@RESUME_RELEASE@|$RESUME_RELEASE|" \
  > .spoolway/pipelines/panevisible.yml <<'YML'
description: Two paned command steps, for whether a later one on the same task's own tab still gets a pane.

steps:
  - id: visible
    description: Stand in a pane until the suite has read everything it writes.
    run: 'echo visible-pane-marker; echo "env:$SPOOLWAY_E2E_PANE_ENV_MARKER"; while [ ! -e "@RELEASE@" ]; do sleep 0.1; done'
    timeout: 120s
    on_pass: resume
    on_fail: blocked
  - id: resume
    description: A second step on the same tab, standing in for a resumed dispatcher meeting it again.
    run: 'echo resume-pane-marker; while [ ! -e "@RESUME_RELEASE@" ]; do sleep 0.1; done'
    timeout: 120s
    on_pass: done
    on_fail: blocked
YML
works "a pipeline with a paned command step checks out" "$SPOOLWAY" pipeline check

# Restarted after the export above, so the dispatcher this starts is the
# one that inherited it — see `dispatcher_restart` a few lines up for why
# that ordering matters.
dispatcher_restart
task_doc "$LIVE/paned.md" paned "$BODY" "group: paned" \
  "pipeline: panevisible"
must "a task through a paned command step" \
  "$SPOOLWAY" queue add --from "$LIVE/paned.md"

# The pane id spoolway recorded for this step, checked against the double's
# own `panes` file — the split's own reply is not enough to trust, and
# `split_pane` refuses to record an id the multiplexer has never heard of.
PANE_FILE="$SPOOLWAY_PROJECT_HOME/commands/paned · visible.pane"
# Ten seconds was enough when E2E_INTERVAL drove the dispatcher at one
# second: the task reached `visible` almost as fast as it was queued. The
# rate is fixed now, so the walk from `queued` to this step costs a real
# PROBE_INTERVAL, and the pane itself only stands for the two seconds its
# command sleeps — the budget has to cover the walk, while the
# tenth-of-a-second pace is what catches the pane inside it.
RECORDED=""
for _ in $(seq 1 1200); do
  RECORDED=$(cat "$PANE_FILE" 2>/dev/null || true)
  if [ -n "$RECORDED" ] && grep -q "^$RECORDED	" "$HSTATE/panes"; then
    break
  fi
  RECORDED=""
  sleep 0.1
done
if [ -n "$RECORDED" ]; then
  ok "a command step with no hide: key runs in a pane of its own"
else
  bad "a command step with no hide: key runs in a pane of its own"
fi

# The task's own tab already names it, so the pane inside it
# carries only the step — not `paned · visible`, the identity `PANE_FILE`
# above is keyed on. Read from the double's own tables, by the ids `RECORDED`
# already proved live, rather than from anything spoolway itself reported.
PANE_LABEL=$(awk -F'\t' -v p="$RECORDED" '$1==p {print $4}' "$HSTATE/panes")
if [ "$PANE_LABEL" = visible ]; then
  ok "a split command pane's visible label is just the step"
else
  bad "a split command pane's visible label is just the step (was \`$PANE_LABEL\`)"
fi

# An *agent* lane's pane takes the same label by the same branch in
# `prepare_boot`, and hands it to the same `rename_pane` this case just proved
# carries a label through to herdr — but it is never asserted here, and that
# is deliberate rather than forgotten. `Herdr::start_lane` only reaches
# `rename_pane` once `agent start` has succeeded, and this double's `agent
# start` only registers a session: it takes no turn and sends no label back.
# See `herdr-stub.sh`'s own header, and `run.sh`'s, which records the same
# boundary. The agent half of this branch is decided instead where a unit test
# can reach it, in
# `src/dispatch.rs::tests::a_split_agent_panes_label_is_only_its_step_not_the_task`.
PANED_TAB=$(awk -F'\t' -v p="$RECORDED" '$1==p {print $2}' "$HSTATE/panes")
TAB_LABEL=$(awk -F'\t' -v t="$PANED_TAB" '$1==t {print $3}' "$HSTATE/tabs")
if [ "$TAB_LABEL" = paned ]; then
  ok "and the task's own split tab shows its slug"
else
  bad "and the task's own split tab shows its slug (was \`$TAB_LABEL\`)"
fi

# Read while the task is still moving — the archive step reclaims this log.
# `records` keeps a `.kept` copy so the env-marker check below still has a
# file to read after `drive paned gone` has deleted the original.
records "the pane's own output is on the record just the same" "visible-pane-marker" \
  "$SPOOLWAY_PROJECT_HOME/commands/paned · visible.log" paned
has "a variable only the dispatcher's own environment carried reached the herdr pane" \
  "env:from-the-dispatchers-own-environment" \
  "$SPOOLWAY_PROJECT_HOME/commands/paned · visible.log.kept"

# Everything about `visible` has been read, so it may finish and hand the
# task to `resume` — a second step, on the same tab, standing in for a
# dispatcher meeting an already-placed task again. Corrupted here, right
# before release, the same way a restarted Herdr would have forgotten it:
# a rename that only ever lived in that process's own memory.
awk -F'\t' -v OFS='\t' -v t="$PANED_TAB" '$1==t {$3="w-corrupted-by-e2e"} {print}' \
  "$HSTATE/tabs" > "$HSTATE/tabs.tmp"
mv "$HSTATE/tabs.tmp" "$HSTATE/tabs"
touch "$VISIBLE_RELEASE"

RESUME_PANE_FILE="$SPOOLWAY_PROJECT_HOME/commands/paned · resume.pane"
RESUME_RECORDED=""
for _ in $(seq 1 1200); do
  RESUME_RECORDED=$(cat "$RESUME_PANE_FILE" 2>/dev/null || true)
  if [ -n "$RESUME_RECORDED" ] && grep -q "^$RESUME_RECORDED	" "$HSTATE/panes"; then
    break
  fi
  RESUME_RECORDED=""
  sleep 0.1
done
if [ -n "$RESUME_RECORDED" ]; then
  ok "the task's second step runs in a pane of its own too"
else
  bad "the task's second step runs in a pane of its own too"
fi

# The proof this whole detour exists for: a later pass finding the task
# already sitting on its tab puts the slug back, exactly as it did the
# moment the tab was cut — not only once, at creation.
RESUMED_TAB_LABEL=$(awk -F'\t' -v t="$PANED_TAB" '$1==t {print $3}' "$HSTATE/tabs")
if [ "$RESUMED_TAB_LABEL" = paned ]; then
  ok "a later pass puts the task's own tab label back, as a resumed dispatcher must"
else
  bad "a later pass puts the task's own tab label back (was \`$RESUMED_TAB_LABEL\`)"
fi

# Everything above has been read, so the command may finish and the task may
# go on to be archived — which is the next thing asserted.
touch "$RESUME_RELEASE"
if drive paned gone 180; then ok "the task carries on once the command has passed"
else bad "the task carries on once the command has passed (at \`$(stage_of paned)\`)"; fi
unset SPOOLWAY_E2E_PANE_ENV_MARKER
if [ -n "$RESUME_RECORDED" ] && grep -q "^$RESUME_RECORDED	" "$HSTATE/panes"; then
  bad "a shown command's pane is closed once its task is done"
else
  ok "a shown command's pane is closed once its task is done"
fi

# The hidden half: the same shape, with `hide: true` added — a
# silent, detached run, and no pane ever asked for. Held open by a release
# file of its own for the same reason the visible half is: its log is read
# while it runs, and the archive reclaims that log the moment the one step
# it has routes to `done`.
#
# The wait bounds itself rather than taking a `timeout:` key the way the
# visible half and `herdrpane.yml` do — `show marks it` just below reads
# this step's *resolved* timeout, and the whole point of that check is that
# it says the headless default, 30m, with nothing written here to say it.
HIDDEN_RELEASE="$LIVE/hidden.release"
rm -f "$HIDDEN_RELEASE"
sed "s|@RELEASE@|$HIDDEN_RELEASE|" > .spoolway/pipelines/panehidden.yml <<'YML'
description: One hidden command step, for whether it ever opens a pane.

steps:
  - id: hidden
    description: Run detached, with no pane, until the suite has read its log.
    run: 'echo hidden-command-marker; for _ in $(seq 1 1200); do [ -e "@RELEASE@" ] && break; sleep 0.1; done'
    hide: true
    on_pass: done
    on_fail: blocked
YML
works "a pipeline naming hide: true checks out" "$SPOOLWAY" pipeline check
says "and show marks it" "hidden     command   waits hide=true timeout=30m" \
  "$SPOOLWAY" pipeline show

dispatcher_restart
task_doc "$LIVE/hiddenc.md" hiddenc "$BODY" "group: hiddenc" \
  "pipeline: panehidden"
must "a task through a hide: true command step" \
  "$SPOOLWAY" queue add --from "$LIVE/hiddenc.md"
# Read while the step is still held, ahead of the archive that reclaims
# this log.
records "and its output is on the record just the same" "hidden-command-marker" \
  "$SPOOLWAY_PROJECT_HOME/commands/hiddenc · hidden.log" hiddenc
# A hidden step has no pane, so its row is the only sign it is working, and
# `queue list` draws how long it has run after the state: `47s`, `9m 12s`,
# `1h 04m`. Polled, as every check on a live row here is: the row is redrawn
# from disk on each call.
hidden_row_timed() {
  "$SPOOLWAY" queue list | grep -E 'hiddenc.*● running [0-9]+(s|m [0-9]{2}s|h [0-9]{2}m)'
}
if poll_until 30 hidden_row_timed; then
  ok "queue list shows how long a running hidden step has run"
else
  bad "queue list shows how long a running hidden step has run"
  "$SPOOLWAY" queue list 2>&1 | sed 's/^/        /'
fi

touch "$HIDDEN_RELEASE"
if drive hiddenc gone 180; then ok "a hide: true command step still routes on its exit code"
else bad "a hide: true command step still routes on its exit code (at \`$(stage_of hiddenc)\`)"; fi
if [ -f "$SPOOLWAY_PROJECT_HOME/commands/hiddenc · hidden.pane" ]; then
  bad "hide: true never opened a pane at all"
else
  ok "hide: true never opened a pane at all"
fi

# The workspace half: a task opens its herdr workspace at the first step that
# shows a pane, not before. A hidden command first, held by a release file,
# leaves the double with no workspace and no pane while it runs. The agent step
# after it then opens the workspace and runs in its first pane, with no second
# pane split beside it.
LAZY_RELEASE="$LIVE/lazy.release"
rm -f "$LAZY_RELEASE"
LAZY_MODEL=$(awk '/id: implement/ {found=1} found && /model:/ {print $2; exit}' \
  .spoolway/pipelines/default.yml)
sed -e "s|@RELEASE@|$LAZY_RELEASE|" -e "s|@MODEL@|$LAZY_MODEL|" \
  > .spoolway/pipelines/panelazy.yml <<'YML'
description: A hidden command, then an agent step, for when the workspace opens.

steps:
  - id: quiet
    description: Run detached until the suite has looked at the double.
    run: 'for _ in $(seq 1 1200); do [ -e "@RELEASE@" ] && break; sleep 0.1; done'
    timeout: 120s
    hide: true
    on_pass: watched
    on_fail: blocked
  - id: watched
    description: The first step that shows a pane.
    agent: pi
    prompt: builder
    model: @MODEL@
    on_pass: done
YML
works "a hidden command followed by an agent step checks out" "$SPOOLWAY" pipeline check

dispatcher_restart
task_doc "$LIVE/lazy.md" lazy "$BODY" "group: lazy" "pipeline: panelazy"
must "a task whose first step is hidden" "$SPOOLWAY" queue add --from "$LIVE/lazy.md"
lazy_worktree_cut() { [ -n "$(worktree_of lazy)" ]; }
if poll_until 60 lazy_worktree_cut; then ok "a hidden first step still has a worktree to run in"
else bad "a hidden first step still has a worktree to run in"; fi
if [ -z "$(workspace_of lazy)" ] && [ -z "$(tab_of lazy)" ] \
  && ! grep -q '^spare_pane:' "$SPOOLWAY_PROJECT_HOME/queue/lazy.md"; then
  ok "and the task records no workspace, tab or spare pane"
else
  bad "and the task records no workspace, tab or spare pane"
  sed 's/^/        /' "$SPOOLWAY_PROJECT_HOME/queue/lazy.md"
fi
LAZY_WORKTREE=$(worktree_of lazy)
if ! awk -F'\t' -v c="$LAZY_WORKTREE" '$3 == c' "$HSTATE/workspaces" | grep -q .; then
  ok "herdr holds no workspace on the task's worktree while the hidden step runs"
else
  bad "herdr holds no workspace on the task's worktree while the hidden step runs"
  sed 's/^/        /' "$HSTATE/workspaces"
fi

touch "$LAZY_RELEASE"
lazy_agent_started() { [ "$(stage_of lazy)" = watched ] && [ -n "$(workspace_of lazy)" ] \
  && [ -s "$HSTATE/agents" ]; }
if poll_until 90 lazy_agent_started; then ok "the workspace opens when the agent step starts"
else bad "the workspace opens when the agent step starts (at \`$(stage_of lazy)\`)"; fi
LAZY_WORKSPACE=$(workspace_of lazy)
if [ "$(awk -F'\t' -v w="$LAZY_WORKSPACE" '$3 == w' "$HSTATE/panes" | wc -l | tr -d ' ')" -eq 1 ]; then
  ok "and the agent runs in the workspace's first pane, with none split beside it"
else
  bad "and the agent runs in the workspace's first pane, with none split beside it"
  sed 's/^/        /' "$HSTATE/panes" "$HSTATE/agents"
fi
must "the agent step reports" env SPOOLWAY_TASK=lazy SPOOLWAY_STEP=watched \
  "$SPOOLWAY" report --pass -m "done with watched" --handoff "done with watched"
if drive lazy gone 120; then ok "the task finishes and is archived"
else bad "the task finishes and is archived (at \`$(stage_of lazy)\`)"; fi

# The override half: commands are hidden for the project, and one step says
# `hide: false`. That step gets a pane of its own, and the step after it, with
# no `hide:` key, follows the project and opens none. Each is held by a release
# file for the reason the visible half is: the pane file and the log are read
# while the step runs, and the archive reclaims them.
must "commands hidden" "$SPOOLWAY" config set dispatch.hide_commands true
OVERRIDE_RELEASE="$LIVE/override.release"
FOLLOW_RELEASE="$LIVE/follow.release"
rm -f "$OVERRIDE_RELEASE" "$FOLLOW_RELEASE"
sed -e "s|@RELEASE@|$OVERRIDE_RELEASE|" -e "s|@FOLLOW_RELEASE@|$FOLLOW_RELEASE|" \
  > .spoolway/pipelines/paneoverride.yml <<'YML'
description: One step that shows its command and one that follows the project, with commands hidden.

steps:
  - id: shown
    description: Stand in a pane of its own although the project hides commands.
    run: 'echo shown-override-marker; while [ ! -e "@RELEASE@" ]; do sleep 0.1; done'
    timeout: 120s
    hide: false
    on_pass: follows
    on_fail: blocked
  - id: follows
    description: Say nothing about hide, so the project's setting hides it.
    run: 'echo follows-project-marker; while [ ! -e "@FOLLOW_RELEASE@" ]; do sleep 0.1; done'
    timeout: 120s
    on_pass: done
    on_fail: blocked
YML
works "a pipeline naming hide: false checks out" "$SPOOLWAY" pipeline check
says "and show marks it" "waits hide=false" \
  "$SPOOLWAY" pipeline show

dispatcher_restart
task_doc "$LIVE/override.md" override "$BODY" "group: override" \
  "pipeline: paneoverride"
must "a task through a hide: false step under hidden commands" \
  "$SPOOLWAY" queue add --from "$LIVE/override.md"
OVERRIDE_PANE_FILE="$SPOOLWAY_PROJECT_HOME/commands/override · shown.pane"
OVERRIDE_RECORDED=""
for _ in $(seq 1 1200); do
  OVERRIDE_RECORDED=$(cat "$OVERRIDE_PANE_FILE" 2>/dev/null || true)
  if [ -n "$OVERRIDE_RECORDED" ] && grep -q "^$OVERRIDE_RECORDED	" "$HSTATE/panes"; then
    break
  fi
  OVERRIDE_RECORDED=""
  sleep 0.1
done
if [ -n "$OVERRIDE_RECORDED" ]; then
  ok "hide: false runs in a pane of its own while the project hides commands"
else
  bad "hide: false runs in a pane of its own while the project hides commands"
fi
# A step in a pane keeps the bare `● running`: the pane is where to look.
shown_row_untimed() {
  local line
  line=$("$SPOOLWAY" queue list | grep 'override.*● running') || return 1
  ! grep -qE '● running [0-9]' <<<"$line"
}
if poll_until 30 shown_row_untimed; then
  ok "and its queue list row shows no running time"
else
  bad "and its queue list row shows no running time"
  "$SPOOLWAY" queue list 2>&1 | sed 's/^/        /'
fi
# The pane file is written only after the pane exists, so a pane the next step
# wrongly opened would show in the stub's own list first. Count it before and after.
PANES_BEFORE=$(wc -l < "$HSTATE/panes")
touch "$OVERRIDE_RELEASE"
records "and the step after it follows the project, on the record" "follows-project-marker" \
  "$SPOOLWAY_PROJECT_HOME/commands/override · follows.log" override
if [ -f "$SPOOLWAY_PROJECT_HOME/commands/override · follows.pane" ] \
  || [ "$(wc -l < "$HSTATE/panes")" -gt "$PANES_BEFORE" ]; then
  bad "a step with no hide: key opens no pane while the project hides commands"
else
  ok "a step with no hide: key opens no pane while the project hides commands"
fi
touch "$FOLLOW_RELEASE"
if drive override gone 180; then ok "the task carries on through both steps"
else bad "the task carries on through both steps (at \`$(stage_of override)\`)"; fi
must "commands shown again" "$SPOOLWAY" config set dispatch.hide_commands false

# A failing, shown command keeps its pane when the task leaves the step for
# `blocked`, so the output of the failure is still on screen.
cat > .spoolway/pipelines/paneflaky.yml <<'YML'
description: One failing paned command step, for whether its pane stays.

steps:
  - id: flaky
    description: Fail every time, so the task lands on blocked.
    run: echo flaky-pane-marker; exit 3
    on_pass: done
    on_fail: blocked
YML
works "a pipeline with a failing paned step checks out" "$SPOOLWAY" pipeline check

dispatcher_restart
task_doc "$LIVE/panedfail.md" panedfail "$BODY" "group: panedfail" \
  "pipeline: paneflaky"
must "a task whose paned step fails" \
  "$SPOOLWAY" queue add --from "$LIVE/panedfail.md"
if drive panedfail blocked 120; then ok "a failing paned command still routes on its exit code"
else bad "a failing paned command still routes on its exit code (at \`$(stage_of panedfail)\`)"; fi
FLAKY_RECORDED=$(cat "$SPOOLWAY_PROJECT_HOME/commands/panedfail · flaky.pane" 2>/dev/null || true)
if [ -n "$FLAKY_RECORDED" ] && grep -q "^$FLAKY_RECORDED	" "$HSTATE/panes"; then
  ok "and its pane stays after the failure, with the output on screen"
else
  bad "and its pane stays after the failure, with the output on screen"
fi

# `panedfail` landed on `blocked` and that was the point of it — but
# `blocked` is a live stage, so it is still a task in this queue, and
# `paneflaky` is about to be deleted out from under it. `check_task_routes`
# reads every live task's `pipeline:` whole, ahead of the lock, and refuses
# the whole run when one of them names a pipeline that is not there: left
# queued, this one task refuses every `spoolway dispatch` for the rest of
# the file. Taken out here rather than at the end of the suite, and with
# `--force` because it has a worktree of its own by now, and
# `dispatcher_stop` ahead of it because `unqueue` refuses outright while a
# dispatcher is up that could be mid-turn on the task. The next scenario
# starts its own with `dispatcher_restart`, so nothing is left without one.
dispatcher_stop
must "the failing-pane task is taken back out before its pipeline goes" \
  "$SPOOLWAY" queue unqueue panedfail --force

"$HERDRBIN/herdr" shutdown state >/dev/null 2>&1 || true
unset HERDR_STUB_STATE
PATH="$PATH_BEFORE_HERDR_STUB"; export PATH
export SPOOLWAY_TEST_BACKEND=headless
rm -f .spoolway/pipelines/panevisible.yml .spoolway/pipelines/panehidden.yml \
  .spoolway/pipelines/paneoverride.yml .spoolway/pipelines/paneflaky.yml

# ---------------------------------------------- a big environment, handed over
# A pane's shell does not inherit the dispatcher's environment: it belongs to
# the multiplexer's server, so whatever a command step needs has to be carried
# across deliberately. herdr's way in is typing at the pane's prompt, and a
# whole inherited environment typed as one `export` line is longer than herdr
# will carry — it used to cut mid-value, leaving the pane's shell waiting
# forever on an unterminated quote, with no pid ever written and no log to
# read. It is written to a file and sourced now, and this is that, end to end.
#
# Against `scripts/e2e/herdr-stub.sh` rather than a real server — its header
# says why there is no isolated herdr to run this on, and which half of the
# behaviour a double can still be honest about. The short version: each pane
# there is a real long-lived shell started under `env -i`, so a variable that
# reaches the command got there because spoolway carried it.
HSTATE="$LIVE/herdr-stub"
HERDRBIN="$LIVE/herdr-bin"
mkdir -p "$HSTATE" "$HERDRBIN"
install -m 755 "$HERE/../herdr-stub.sh" "$HERDRBIN/herdr"
export HERDR_STUB_STATE="$HSTATE"
# Saved so the teardown below can put it back — a `herdr` left first on PATH
# refuses to run at all once HERDR_STUB_STATE is unset, which would break the
# first later case that so much as checks the backend is available.
PATH_BEFORE_HERDR_STUB="$PATH"
PATH="$HERDRBIN:$PATH"; export PATH

# Any value but `headless` runs herdr, here the double above; `1` also keeps
# the test-only gates open, as the harness's own export does.
export SPOOLWAY_TEST_BACKEND=1

# A real project home is `~/.spoolway/<label>-<id>/`, a good deal longer than
# the short paths this suite lives under, and every path in a paned step's
# wrapper grows with it. The binary builds that home from `$HOME`, so `$HOME`
# is pointed at a symlink to the real one with a long name for the length of
# this case, and the dispatcher restarted under it: a line that only fits a
# short home does not pass here. Put back after the case, before the
# dispatcher is restarted for the cases that follow.
REAL_HOME="$HOME"
REAL_PROJECT_HOME="$SPOOLWAY_PROJECT_HOME"
PADDED_HOME="$LIVE/a-home-as-long-as-a-real-one-with-its-user-name/padding"
mkdir -p "$(dirname "$PADDED_HOME")"
ln -s "$REAL_HOME" "$PADDED_HOME"
export HOME="$PADDED_HOME"
export SPOOLWAY_PROJECT_HOME="$PADDED_HOME/.spoolway/${REAL_PROJECT_HOME#"$REAL_HOME"/.spoolway/}"
if [ "${#SPOOLWAY_PROJECT_HOME}" -ge 100 ]; then
  ok "the project home for this case is ${#SPOOLWAY_PROJECT_HOME} bytes, as long as a real one"
else
  bad "the project home for this case is padded to at least 100 bytes (${#SPOOLWAY_PROJECT_HOME})"
fi

# The two variables this case is about, both set before the dispatcher starts
# so they are really part of the environment it inherited.
#
# The bulk one is the point: eight kilobytes in a single value, comfortably
# past the length a pane's prompt used to cut an `export` line at. The marker
# is how a false pass is ruled out — nothing on the harness's own PATH carries
# it, and the pane's shell is started with no environment at all.
export SPOOLWAY_E2E_PANE_ENV_MARKER="from-the-dispatchers-own-environment"
SPOOLWAY_E2E_PANE_BULK=$(head -c 8000 /dev/zero | tr '\0' 'x'); export SPOOLWAY_E2E_PANE_BULK

# A pipeline of two paned command steps, so this case needs no agent lane and
# the double needs no agent to start. The first reads both variables back out;
# the second has a `run:` line of about four kilobytes, to show a long line is
# run from the wrapper file and never typed.
#
# And then it waits on a file, which is not decoration. Seven of the
# assertions below read something spoolway wrote *while this step was
# running* — `carry.log`, `carry.pane` and `handover.env`, all under
# `commands/`, and the pane itself, which `run_command`'s own exit arm closes
# the moment the command is over. A one-step pipeline routing straight to
# `done` goes queued -> carry -> archived inside a second, and `teardown.rs`
# reclaims the whole `commands/` directory on the archive; every one of those
# reads was then a race, and on this machine it lost — `records` left a `.kept`
# copy holding half a line, and `handover.env` and `carry.pane` were simply
# gone. It loses on the unsplit `commands.sh` at this task's base commit too,
# the same seven checks, so this is the case's own race and not the split's.
#
# A made window rather than a caught one, the same way `forge.sh` holds its
# hand-off open: the command writes what it was asked to, then blocks until
# the suite has read it. `timeout:` is the backstop, so a suite that dies
# before releasing it does not leave a pane waiting forever.
CARRY_RELEASE="$LIVE/carry.release"
rm -f "$CARRY_RELEASE"

LONG_PAD=$(head -c 4000 /dev/zero | tr '\0' 'x')
sed -e "s|@RELEASE@|$CARRY_RELEASE|" -e "s|@PAD@|$LONG_PAD|" > .spoolway/pipelines/herdrpane.yml <<'YML'
description: Two paned command steps, for the environment a pane is handed and a long run line.

steps:
  - id: carry
    description: Read back the environment the dispatcher was started with.
    run: 'echo "env:$SPOOLWAY_E2E_PANE_ENV_MARKER"; echo "bulk:${#SPOOLWAY_E2E_PANE_BULK}"; while [ ! -e "@RELEASE@" ]; do sleep 0.1; done'
    timeout: 120s
    on_pass: long
    on_fail: blocked
  - id: long
    description: A run line of about four kilobytes, which is never typed into the pane.
    run: ': @PAD@; echo long-run-line-ran-whole'
    timeout: 120s
    on_pass: done
    on_fail: blocked
YML
works "a two-step paned pipeline checks out" "$SPOOLWAY" pipeline check

dispatcher_restart
task_doc "$LIVE/carried.md" carried "$BODY" "group: carried" \
  "pipeline: herdrpane"
must "a task through a paned command step on herdr" \
  "$SPOOLWAY" queue add --from "$LIVE/carried.md"

records "the pane's own output is on the record" "env:" \
  "$SPOOLWAY_PROJECT_HOME/commands/carried · carry.log" carried
has "a variable only the dispatcher's own environment carried reached the herdr pane" \
  "env:from-the-dispatchers-own-environment" \
  "$SPOOLWAY_PROJECT_HOME/commands/carried · carry.log.kept"
has "and the eight-kilobyte value arrived whole, not cut mid-quote" \
  "bulk:8000" \
  "$SPOOLWAY_PROJECT_HOME/commands/carried · carry.log.kept"

# The other half: it arrived that way *because nothing long was typed*. The
# environment is a file beside the run's own bookkeeping, and what went to the
# pane is the one `.` command that sources it.
HANDOVER="$SPOOLWAY_PROJECT_HOME/commands/carried · handover.env"
works "the environment was written down beside the run" test -f "$HANDOVER"
has "with the dispatcher's own value in it" \
  "from-the-dispatchers-own-environment" "$HANDOVER"
if [ "$(wc -c <"$HANDOVER")" -gt 8000 ]; then
  ok "and it is the big one — past what a prompt would have carried"
else
  bad "and it is the big one — past what a prompt would have carried"
fi

# The pane spoolway recorded is the one herdr's own `pane list` agrees with —
# the split's reply alone is not enough to record, and `split_pane` refuses
# rather than write an id the multiplexer has never heard of.
RECORDED=$(cat "$SPOOLWAY_PROJECT_HOME/commands/carried · carry.pane" 2>/dev/null || true)
if [ -n "$RECORDED" ] && grep -q "^$RECORDED	" "$HSTATE/panes"; then
  ok "the recorded pane id is one the multiplexer itself lists"
else
  bad "the recorded pane id is one the multiplexer itself lists (recorded \"$RECORDED\")"
fi

# Everything the window was held open for has been read. Let the command
# finish, and the task carries on from a pass like any other. The second step
# is a `run:` line of about four kilobytes under the padded home: it runs whole
# only if the pane was handed a file to run, not the line itself.
touch "$CARRY_RELEASE"
records "a four-kilobyte run: line under a full-length home ran whole" \
  "long-run-line-ran-whole" \
  "$SPOOLWAY_PROJECT_HOME/commands/carried · long.log" carried

# `herdr-stub.sh` keeps every string spoolway typed into a pane, with its
# length, and refuses one past 512 bytes. The environment is 8KB and the
# run: line 4KB; nothing typed may be near either. Read here, after both steps
# have been typed, so the second one's line is counted too.
TYPED_MAX=$(awk -F'\t' 'BEGIN{m=0} $2>m {m=$2} END{print m+0}' "$HSTATE/typed.index")
if [ "$(wc -l <"$HSTATE/typed.index")" -ge 3 ] && [ "$TYPED_MAX" -le 512 ]; then
  ok "and no single line typed into a pane was longer than $TYPED_MAX bytes"
else
  bad "nothing over 512 bytes is typed into a pane (longest was $TYPED_MAX bytes)"
  cut -f1,2 "$HSTATE/typed.index"
fi
# What the binary typed carries the padded home, so the length above was
# earned under it and not under the short one.
if grep -rqF -- "sh '$SPOOLWAY_PROJECT_HOME/commands/carried · long.sh'" "$HSTATE/typed"; then
  ok "and the line that ran it names the wrapper file under the full-length home"
else
  bad "and the line that ran it names the wrapper file under the full-length home"
fi
if grep -rqF -- ". '$HANDOVER'" "$HSTATE/typed"; then
  ok "and one of them is the dot command that sources the file"
else
  bad "and one of them is the dot command that sources the file"
fi
if drive carried gone 180; then ok "the task carries on once the paned command has passed"
else bad "the task carries on once the paned command has passed (at \`$(stage_of carried)\`)"; fi
rm -f "$CARRY_RELEASE"

unset SPOOLWAY_E2E_PANE_ENV_MARKER SPOOLWAY_E2E_PANE_BULK
export HOME="$REAL_HOME"
export SPOOLWAY_PROJECT_HOME="$REAL_PROJECT_HOME"
rm -f .spoolway/pipelines/herdrpane.yml
export SPOOLWAY_TEST_BACKEND=headless
# Every pane of the double is a real shell with a real process holding its
# fifo open. A suite that walked away would leave both behind. All of this
# teardown runs *before* `dispatcher_restart` below: that call starts the
# long-lived dispatcher every later case in this suite shares, and it must
# inherit the real PATH, not the stub's — a dispatcher started one line too
# early here is exactly the half of the PATH fix that used to not land.
"$HERDRBIN/herdr" shutdown state >/dev/null 2>&1 || true
unset HERDR_STUB_STATE
PATH="$PATH_BEFORE_HERDR_STUB"; export PATH
dispatcher_restart
# Only now: the dispatcher stopped by that restart was started under it.
rm -f "$PADDED_HOME"

# ------------------------------------------------------------- a run line that execs
# `exec` replaces the shell it runs in. The line used to run in the wrapper
# itself, so an `exec` took the wrapper's exit trap with it: no exit code was
# ever written, the step read as interrupted, and the dispatcher ran it again
# on every pass without the task ever leaving it. The line now runs in a child
# shell, so what is asserted is the task moving on and the line running once.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
EXEC_RAN="$LIVE/exec-ran.txt"
rm -f "$EXEC_RAN"
add_command_step default execed \
  "exec sh -c 'echo ran >> $EXEC_RAN; exit 0'" review implement
must "marking it hide: true" \
  sed -i 's|^    run: exec sh .*$|&\n    hide: true|' .spoolway/pipelines/default.yml
works "a command step whose line starts with exec checks out" "$SPOOLWAY" pipeline check

task_doc "$LIVE/execed.md" execed "$BODY" "group: execed"
must "a task behind the exec step queues" "$SPOOLWAY" queue add --from "$LIVE/execed.md"
if drive execed gone 180; then ok "a command line that starts with exec lets its task move on"
else bad "a command line that starts with exec lets its task move on (stuck at \`$(stage_of execed)\`)"; fi
if [ "$(wc -l < "$EXEC_RAN" 2>/dev/null || echo 0)" -eq 1 ]; then
  ok "and the line ran once"
else
  bad "and the line ran once ($(wc -l < "$EXEC_RAN" 2>/dev/null || echo 0) line(s) in $EXEC_RAN)"
fi
lacks "and the dispatcher never took it for an interrupted run" \
  "was interrupted without an exit code" "$E2E_DISPATCH_LOG"

# ------------------------------------------------------------- paused after the run exited
# Pausing a command step kills a run that is still going, and the command runs
# again in full on resume. A run that had already exited has nothing to kill,
# so only its exit code is left on disk, and a resume that kept it would route
# on that code at once without running the command. The dispatcher is stopped
# while the command runs, and the command waits for a file that is only
# touched once the dispatcher is gone, so the exit lands while nothing is
# reading it. The first run fails and the second passes. The step sends a
# failure to `blocked`, so a resume that kept the old code would stop there
# with one run; the file counts how often the command ran.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
HELD_GATE="$LIVE/held.gate"
HELD_GO="$LIVE/held.go"
HELD_RAN="$LIVE/held-c.txt"
rm -f "$HELD_GATE" "$HELD_GO" "$HELD_RAN"
{
  printf '\n  - id: held-c\n'
  printf '    description: Fails while its gate file is missing, and passes once it is there.\n'
  printf '    run: echo ran >> %s; while [ ! -f %s ]; do sleep 0.2; done; test -f %s\n' \
    "$HELD_RAN" "$HELD_GO" "$HELD_GATE"
  printf '    hide: true\n'
  printf '    on_pass: review\n    on_fail: blocked\n'
} >> .spoolway/pipelines/default.yml
sed -i "0,/^    on_pass: review\$/s//    on_pass: held-c/" .spoolway/pipelines/default.yml
works "a command step to pause after its run exited checks out" "$SPOOLWAY" pipeline check

task_doc "$LIVE/held.md" held "$BODY" "group: held"
must "a task for the paused-after-exit case queues" "$SPOOLWAY" queue add --from "$LIVE/held.md"
drive held held-c 120 || bad "the task reaches the command step (at \`$(stage_of held)\`)"
poll_while 30 test ! -s "$HELD_RAN"
dispatcher_stop
touch "$HELD_GO"
if poll_while 30 sh -c '! ls "$1"/*held-c*.exit >/dev/null 2>&1' _ "$SPOOLWAY_PROJECT_HOME/commands"; then
  ok "the command's run exited while the dispatcher was stopped"
else
  bad "the command's run exited while the dispatcher was stopped"
fi
must "pausing the task after its run exited" "$SPOOLWAY" queue pause held
must "resuming it" "$SPOOLWAY" queue resume held
touch "$HELD_GATE"
if drive held gone 240; then ok "a task resumed after its run exited still reaches the end"
else bad "a task resumed after its run exited still reaches the end (at \`$(stage_of held)\`)"; fi
if [ "$(wc -l < "$HELD_RAN" 2>/dev/null || echo 0)" -eq 2 ]; then
  ok "the command ran again on resume rather than routing on the old run's code"
else
  bad "the command ran again on resume rather than routing on the old run's code \
($(wc -l < "$HELD_RAN" 2>/dev/null || echo 0) run(s) in $HELD_RAN, wanted 2)"
fi
# The pipeline's own count, not the status log: the resume writes a
# `→ \`held-c\`` line of its own, which is not an arrival.
HELD_ARRIVALS=$(awk '/^arrivals:/{f=1;next} f&&/^  held-c:/{print $2;exit} f&&!/^  /{exit}' \
  "$SPOOLWAY_PROJECT_HOME/archive/held.md")
if [ "${HELD_ARRIVALS:-0}" -eq 1 ]; then
  ok "and the resume did not count as an arrival"
else
  bad "and the resume did not count as an arrival (${HELD_ARRIVALS:-0} arrival(s), wanted 1)"
fi

# ------------------------------------------------------------- a second visit runs again
# Arriving at a command step starts it from nothing, whichever road the task
# took in. Here the road is the ordinary one: `visit-d` fails once and sends the
# task back through the agent step to `visit-c`, which must run its command a
# second time rather than route on the code the first visit left. The file the
# command appends to counts how often it really ran.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
VISIT_MARK="$LIVE/visit-d.mark"
VISIT_RAN="$LIVE/visit-c.txt"
rm -f "$VISIT_MARK" "$VISIT_RAN"
{
  printf '\n  - id: visit-c\n'
  printf '    description: Counts its own runs, once per visit.\n'
  printf '    run: echo ran >> %s\n' "$VISIT_RAN"
  printf '    hide: true\n'
  printf '    on_pass: visit-d\n    on_fail: implement\n'
  printf '\n  - id: visit-d\n'
  printf '    description: Fails the first time and passes after that.\n'
  printf '    run: if [ -f %s ]; then exit 0; fi; touch %s; exit 1\n' "$VISIT_MARK" "$VISIT_MARK"
  printf '    hide: true\n'
  printf '    on_pass: review\n    on_fail: implement\n'
} >> .spoolway/pipelines/default.yml
sed -i "0,/^    on_pass: review\$/s//    on_pass: visit-c/" .spoolway/pipelines/default.yml
works "a command step revisited through the agent step checks out" "$SPOOLWAY" pipeline check

task_doc "$LIVE/visited.md" visited "$BODY" "group: visited"
must "a task for the second-visit case queues" "$SPOOLWAY" queue add --from "$LIVE/visited.md"
if drive visited gone 240; then ok "a task sent back to a command step still reaches the end"
else bad "a task sent back to a command step still reaches the end (at \`$(stage_of visited)\`)"; fi
if [ "$(wc -l < "$VISIT_RAN" 2>/dev/null || echo 0)" -eq 2 ]; then
  ok "the command ran again on the second visit rather than routing on the first visit's code"
else
  bad "the command ran again on the second visit rather than routing on the first visit's code \
($(wc -l < "$VISIT_RAN" 2>/dev/null || echo 0) run(s) in $VISIT_RAN, wanted 2)"
fi
if [ "$(arrivals $SPOOLWAY_PROJECT_HOME/archive/visited.md visit-c)" -eq 2 ]; then
  ok "and the task arrived at it twice"
else
  bad "and the task arrived at it twice ($(arrivals $SPOOLWAY_PROJECT_HOME/archive/visited.md visit-c) time(s))"
fi

# ------------------------------------------------------------- a walk-past stops at the limit
# `loop:` limits arrivals "by any route", and a step a task's `skip:` names is
# walked past without a lane. Two steps that both are, passing to each other,
# are the shape that once ran 56 arrivals at a step with `loop: 2` in half a
# minute: the walk-past never asked the step it landed on whether it was spent.
cat > .spoolway/pipelines/default.yml <<'YAML'
steps:
  - id: walk-a
    description: Bounded, and reached again by every lap of the walk-past below.
    run: "true"
    loop: 2
    on_pass: walk-b
  - id: walk-b
    description: Hands the task straight back to walk-a.
    run: "true"
    on_pass: walk-a
    on_fail: done
YAML
works "a pipeline whose only loop is a walk-past checks out" "$SPOOLWAY" pipeline check

# The dispatcher the last case left up would take `walked` the moment it is
# queued, and run `walk-a` as a command before the `skip:` below lands on it.
# `drive` starts a fresh one once the file is ready.
dispatcher_stop
task_doc "$LIVE/walked.md" walked "$BODY" "group: walked"
must "a task for the walk-past case queues" "$SPOOLWAY" queue add --from "$LIVE/walked.md"
# `queue add` empties `skip:` on every task it queues, so it is written onto the
# queued file afterwards, before the dispatcher is up to read it.
sed -i '0,/^group: walked$/s//group: walked\nskip: [walk-a, walk-b]/' \
  "$SPOOLWAY_PROJECT_HOME/queue/walked.md"
has "the queued task carries the skip: list" "skip: [walk-a, walk-b]" \
  "$SPOOLWAY_PROJECT_HOME/queue/walked.md"
if drive walked blocked 120; then
  ok "a walk-past that would circle forever stops the task at the limit"
else
  bad "a walk-past that would circle forever stops the task at the limit (at \`$(stage_of walked)\`)"
fi
has "the steps were walked past rather than run" "walked past \`walk-a\` (skip)" \
  "$SPOOLWAY_PROJECT_HOME/queue/walked.md"
if [ -e "$SPOOLWAY_PROJECT_HOME/commands/walked · walk-a.log" ]; then
  bad "no command ran for a walked-past step (found walked · walk-a.log)"
else
  ok "no command ran for a walked-past step"
fi
has "and the block names the step and the arrival it refused" \
  "\`walk-b\` may not send this to \`walk-a\` a 3rd time — \`walk-a\` has \`loop: 2\`" \
  $SPOOLWAY_PROJECT_HOME/queue/walked.md
if [ "$(arrivals $SPOOLWAY_PROJECT_HOME/queue/walked.md walk-a)" -eq 2 ]; then
  ok "walk-a was arrived at exactly its limit of times"
else
  bad "walk-a was arrived at exactly its limit of times (arrived \
$(arrivals $SPOOLWAY_PROJECT_HOME/queue/walked.md walk-a) time(s), wanted 2)"
fi

# ------------------------------------------------------------- the usage ledger
# A command step's run is banked to the usage ledger when it ends, as a line of
# its own: no tokens and no session, so it is not a lane, and `spoolway eval`
# reads it into a command steps table under `commands`. One step that exits 0
# and one that exits 1 are the two shapes of a gate's verdict, and the second
# one sends the task to `blocked`, which the lanes table cannot show.
cat > .spoolway/pipelines/default.yml <<'YAML'
steps:
  - id: okgate
    description: Exits 0, so the task goes on to the next gate.
    run: "true"
    on_pass: redgate
  - id: redgate
    description: Exits 1, and a failing step with no on_fail blocks the task.
    run: "exit 1"
    on_pass: done
YAML
# The walk-past case's task is still queued and names steps this pipeline
# lacks, which `pipeline check` would report.
rm -f "$SPOOLWAY_PROJECT_HOME/queue/walked.md"
works "a pipeline of two gates checks out" "$SPOOLWAY" pipeline check

dispatcher_stop
task_doc "$LIVE/ledgered.md" ledgered "$BODY" "group: ledgered"
must "a task for the ledger case queues" "$SPOOLWAY" queue add --from "$LIVE/ledgered.md"
if drive ledgered blocked 120; then
  ok "a task whose second gate exits 1 stops on blocked"
else
  bad "a task whose second gate exits 1 stops on blocked (at \`$(stage_of ledgered)\`)"
fi

LEDGER="$SPOOLWAY_PROJECT_HOME/usage.jsonl"
# One line per run: the dispatcher reads an exit code on every pass until the
# move it chose has landed, and a line per read would bank each run many times.
for gate in okgate redgate; do
  COUNT=$(jq -s --arg step "$gate" \
    '[.[] | select(.task == "ledgered" and .step == $step and .command)] | length' \
    "$LEDGER" 2>/dev/null)
  if [ "$COUNT" = 1 ]; then ok "the ledger holds one command line for \`$gate\`"
  else bad "the ledger holds one command line for \`$gate\` (found ${COUNT:-none})"; fi
done
if jq -se '[.[] | select(.task == "ledgered" and .step == "okgate")][0]
    | .command.ended == "exited" and .command.exit == 0 and .outcome == "pass"
      and (.blocked | not) and (.session // "") == "" and (.cost_usd == null)' \
    "$LEDGER" >/dev/null 2>&1; then
  ok "the passing gate's line records exit 0, a pass, no block and no cost"
else
  bad "the passing gate's line records exit 0, a pass, no block and no cost"
  grep '"task":"ledgered"' "$LEDGER" | sed 's/^/        /'
fi
if jq -se '[.[] | select(.task == "ledgered" and .step == "redgate")][0]
    | .command.ended == "exited" and .command.exit == 1 and .outcome == "fail"
      and .blocked == true and .wall_s >= 0' \
    "$LEDGER" >/dev/null 2>&1; then
  ok "the failing gate's line records exit 1, a fail and the move to blocked"
else
  bad "the failing gate's line records exit 1, a fail and the move to blocked"
  grep '"task":"ledgered"' "$LEDGER" | sed 's/^/        /'
fi

EVAL=$("$SPOOLWAY" eval --by step --json 2>&1)
if jq -e '.commands.by == "step"
    and ([.commands.rows[] | select(.step == "okgate")][0] | .runs == 1 and .pass == 1 and .blocks == 0)
    and ([.commands.rows[] | select(.step == "redgate")][0] | .runs == 1 and .pass == 0 and .blocks == 1)' \
    <<<"$EVAL" >/dev/null 2>&1; then
  ok "eval --by step --json shows both gates under commands"
else
  bad "eval --by step --json shows both gates under commands"; sed 's/^/        /' <<<"$EVAL" | head -40
fi
# The lanes table is for lanes: a command line adds none.
if jq -e '[.rows[] | select(.step == "okgate" or .step == "redgate")] | length == 0' \
    <<<"$EVAL" >/dev/null 2>&1; then
  ok "and neither gate is a row of the lanes table"
else
  bad "and neither gate is a row of the lanes table"
fi
says "eval --step shows a command step's rows instead of refusing" "redgate" \
  "$SPOOLWAY" eval --by step --step redgate
# Still queued on `blocked`, and the pipelines below have no `redgate` step,
# which `pipeline check` reports for a live task.
forget ledgered

cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml

# ---------------------------------------------------- a process left behind
# `pipeline check` is not run on these two pipelines: the `walked` task above
# is still queued, blocked, and names steps these pipelines do not have.
#
# A blocking step whose line backgrounds a process and returns: the step passes
# at once, its exit is routed and its run files are forgotten, and the process
# is still running in the task's worktree. Cleanup has to find it without those
# files, or it outlives the task and keeps a deleted directory as its cwd.
#
# Held on a release file for the same reason the `bench` case is: the pid this
# case reads is written under `commands/`, which goes when the task does.
LEFT_CHILD="$LIVE/left.child"
LEFT_RELEASE="$LIVE/left.release"
rm -f "$LEFT_CHILD" "$LEFT_RELEASE"
add_command_step default left \
  "(sleep 98 & echo \$! > \"$LEFT_CHILD\") ; true" review
add_command_step default hold \
  "for _ in \$(seq 1 1200); do [ -e \"$LEFT_RELEASE\" ] && break; sleep 0.1; done" \
  review

dispatcher_restart
task_doc "$LIVE/left.md" left "$BODY" "group: left"
must "a task whose command step leaves a process running" \
  "$SPOOLWAY" queue add --from "$LIVE/left.md"

if poll_until 120 test -s "$LEFT_CHILD"; then
  ok "the command left a process running behind its own exit"
else
  bad "the command left a process running behind its own exit"
fi
LEFT_PID=$(cat "$LEFT_CHILD" 2>/dev/null)
touch "$LEFT_RELEASE"
if drive left gone 300; then
  ok "the task reached done with a process still running behind its step"
else
  bad "the task reached done with a process still running behind its step (at \`$(stage_of left)\`)"
fi
if [ -n "$LEFT_PID" ] && poll_while 20 kill -0 "$LEFT_PID"; then
  ok "cleanup stops a process a finished command step left behind"
else
  bad "cleanup stops a process a finished command step left behind (pid $LEFT_PID)"
  [ -n "$LEFT_PID" ] && kill "$LEFT_PID" 2>/dev/null
fi

# ------------------------------------------------------- a killed wrapper
# Only the wrapper is killed, with SIGKILL, so the command under it carries on.
# The run reads as interrupted and the command starts again, and the first run
# must be gone by then: two runs of one command side by side share a worktree.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
KILLED_RUNS="$LIVE/killed.runs"
KILLED_RELEASE="$LIVE/killed.release"
rm -f "$KILLED_RUNS" "$KILLED_RELEASE"
add_command_step default killed \
  "sleep 15 & echo \$! >> \"$KILLED_RUNS\"; wait" review
add_command_step default hold \
  "for _ in \$(seq 1 1200); do [ -e \"$KILLED_RELEASE\" ] && break; sleep 0.1; done" \
  review

dispatcher_restart
task_doc "$LIVE/killed.md" killed "$BODY" "group: killed"
must "a task whose command step will lose its wrapper" \
  "$SPOOLWAY" queue add --from "$LIVE/killed.md"

KILLED_PIDFILE="$SPOOLWAY_PROJECT_HOME/commands/killed · killed.pid"
poll_until 120 test -s "$KILLED_RUNS"
FIRST_CHILD=$(head -n1 "$KILLED_RUNS" 2>/dev/null)
WRAPPER=$(head -n1 "$KILLED_PIDFILE" 2>/dev/null)
if [ -n "$WRAPPER" ] && kill -9 "$WRAPPER" 2>/dev/null; then
  ok "the wrapper alone was killed"
else
  bad "the wrapper alone was killed (pid $WRAPPER)"
fi
# A function, because `poll_until` runs a command and the count has to be read
# again on every try.
runs_started() { [ "$(wc -l < "$KILLED_RUNS")" -ge 2 ]; }
if poll_until 60 runs_started; then
  ok "the command started again"
else
  bad "the command started again"
fi
if [ -n "$FIRST_CHILD" ] && ! kill -0 "$FIRST_CHILD" 2>/dev/null; then
  ok "and the first run was gone by the time the second started"
else
  bad "and the first run was gone by the time the second started (pid $FIRST_CHILD)"
fi
touch "$KILLED_RELEASE"
drive killed gone 300 || true

cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml

# ------------------------------------------------------ restarting a command
# `spoolway restart` on a task whose command is running stops that run and
# starts the step again from the top. The command appends its child's pid to a
# file each time it starts, so a second line is a second run, and the first
# child must be gone by then: two runs of one command share a worktree.
RESTART_RUNS="$LIVE/restart.runs"
RESTART_RELEASE="$LIVE/restart.release"
rm -f "$RESTART_RUNS" "$RESTART_RELEASE"
add_command_step default rerun \
  "sleep 60 & echo \$! >> \"$RESTART_RUNS\"; wait" review
add_command_step default hold \
  "for _ in \$(seq 1 1200); do [ -e \"$RESTART_RELEASE\" ] && break; sleep 0.1; done" \
  review

dispatcher_restart
task_doc "$LIVE/rerun.md" rerun "$BODY" "group: rerun"
must "a task whose command step will be restarted" \
  "$SPOOLWAY" queue add --from "$LIVE/rerun.md"

poll_until 120 test -s "$RESTART_RUNS"
RERUN_FIRST=$(head -n1 "$RESTART_RUNS" 2>/dev/null)
must "a restart of the task while its command runs" "$SPOOLWAY" restart rerun
restart_runs_started() { [ "$(wc -l < "$RESTART_RUNS")" -ge 2 ]; }
if poll_until 120 restart_runs_started; then
  ok "the command ran again from the start"
else
  bad "the command ran again from the start"
fi
if [ -n "$RERUN_FIRST" ] && ! kill -0 "$RERUN_FIRST" 2>/dev/null; then
  ok "and the run that was going is gone"
else
  bad "and the run that was going is gone (pid $RERUN_FIRST)"
fi
lacks "and no conversation is named for a command" "restart:" \
  "$SPOOLWAY_PROJECT_HOME/queue/rerun.md"
touch "$RESTART_RELEASE"
drive rerun gone 300 || true

cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml



forget walked

# ------------------------------------------- an unblocker that keeps passing a red gate
# Last in the file, because an unattended run also resumes every task an
# earlier case left blocked, which would answer for them instead of for this.
# A command step that fails goes to `blocked`; an unattended run staffs
# `blocked` with an unblocker, whose pass hands the command back to itself,
# and leaving `blocked` wipes every `loop:` count. Nothing in the graph can
# stop that cycle, so spoolway counts the laps itself. The pass after the
# third lap parks the task on `paused`. The mock unblocker always passes, so
# the command being always red is the whole case.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default redgate "echo 'CI would fail here' >&2; exit 1" review ""
works "a pipeline whose red gate has no loop to bound it checks out" \
  "$SPOOLWAY" pipeline check
must "an unattended run" "$SPOOLWAY" config set unattended.enabled true

task_doc "$LIVE/redgate.md" redgate "$BODY" "group: redgate"
must "a task whose gate is always red" "$SPOOLWAY" queue add --from "$LIVE/redgate.md"
if drive redgate paused 150; then
  ok "an unblocker that keeps passing a red gate ends on paused, not another lap"
else
  bad "an unblocker that keeps passing a red gate ends on paused \
(at \`$(stage_of redgate)\`)"
fi
has "the Status Log names the step and the laps" \
  "\`redgate\` still fails after 3 unblocker laps — parked on \`paused\`" \
  $SPOOLWAY_PROJECT_HOME/queue/redgate.md
has "and the task keeps the step to hand back to" "paused_at: redgate" \
  $SPOOLWAY_PROJECT_HOME/queue/redgate.md
counter "three laps were handed back to the gate, not a fourth" \
  steps "blocked->redgate" 3 $SPOOLWAY_PROJECT_HOME/queue/redgate.md

# ---------------------------- an unblocker that keeps passing a gate that never turns green
# The shape of "a gate that never turns green" above, with nobody at the board:
# `gate` fails back to the agent step `e2e` (`on_fail`), and its `loop: 1`
# sends the task to `blocked` from `e2e`, not from `gate`. The unblocker's
# pass is carried past `e2e` onto `gate` again, so `gate` is never the step the
# task stopped at. The bound has to count the landing, not the step stopped
# at, and keep the count across `e2e`.
forget redgate
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
{
  printf '\n  - id: e2e\n'
  printf "    description: A stand-in for a mechanical gate's own agent step.\n"
  printf '    agent: pi\n    prompt: implementer\n    model: fake-local\n'
  printf '    on_pass: gate\n    on_fail: blocked\n'
  printf '\n  - id: gate\n'
  printf '    description: Always red, so the loop it bounds is what this case is about.\n'
  printf "    run: echo 'CI would fail here' >&2; exit 1\n"
  printf '    loop: 1\n'
  printf '    on_pass: document\n    on_fail: e2e\n'
} >> .spoolway/pipelines/default.yml
sed -i "0,/^    on_pass: document\$/s//    on_pass: e2e/" .spoolway/pipelines/default.yml
works "a pipeline whose gate loops back to the agent step checks out unattended" \
  "$SPOOLWAY" pipeline check

task_doc "$LIVE/gated-unattended.md" gated-unattended "$BODY" "group: gated-unattended"
must "a task whose gate never turns green, unattended" \
  "$SPOOLWAY" queue add --from "$LIVE/gated-unattended.md"
if drive gated-unattended paused 150; then
  ok "an unblocker that keeps passing a gate that never turns green ends on paused"
else
  bad "an unblocker that keeps passing a gate that never turns green ends on paused \
(at \`$(stage_of gated-unattended)\`)"
fi
has "the Status Log names the gate and the laps" \
  "\`gate\` still fails after 3 unblocker laps — parked on \`paused\`" \
  $SPOOLWAY_PROJECT_HOME/queue/gated-unattended.md
has "and the task keeps the agent step it stopped at to hand back to" "paused_at: e2e" \
  $SPOOLWAY_PROJECT_HOME/queue/gated-unattended.md
counter "three laps were handed back to the gate, not a fourth" \
  steps "blocked->gate" 3 $SPOOLWAY_PROJECT_HOME/queue/gated-unattended.md
lacks "and it never reached the step the gate guards" "→ \`document\`" \
  $SPOOLWAY_PROJECT_HOME/queue/gated-unattended.md

# --------------------- an unblocker that answers `--stage` instead of a plain pass
# The count watches the command, not the road the unblocker takes back to it.
# The `stage:<step>` stand-in answers `--pass --stage <step>` on `blocked`, a
# route the plain-pass cases above never take. The gate pipeline is still in
# place, so the first case sends the task back to the agent step behind the gate.
forget gated-unattended
echo "stage:e2e" > "$CTL/gated-stage"
task_doc "$LIVE/gated-stage.md" gated-stage "$BODY" "group: gated-stage"
must "a task whose gate never turns green, with an unblocker that answers --stage" \
  "$SPOOLWAY" queue add --from "$LIVE/gated-stage.md"
if drive gated-stage paused 150; then
  ok "an unblocker that answers --stage to the agent behind a red gate ends on paused"
else
  bad "an unblocker that answers --stage to the agent behind a red gate ends on paused \
(at \`$(stage_of gated-stage)\`)"
fi
has "the Status Log names the gate and the laps" \
  "\`gate\` still fails after 3 unblocker laps — parked on \`paused\`" \
  $SPOOLWAY_PROJECT_HOME/queue/gated-stage.md
counter "three laps were handed back towards the gate, not a fourth" \
  steps "blocked->gate" 3 $SPOOLWAY_PROJECT_HOME/queue/gated-stage.md

# The direct shape: the command is the step the unblocker names.
forget gated-stage
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default redgate "echo 'CI would fail here' >&2; exit 1" review ""
echo "stage:redgate" > "$CTL/redstage"
task_doc "$LIVE/redstage.md" redstage "$BODY" "group: redstage"
must "a task whose gate is always red, with an unblocker that answers --stage" \
  "$SPOOLWAY" queue add --from "$LIVE/redstage.md"
if drive redstage paused 150; then
  ok "an unblocker that answers --stage to the failing command ends on paused"
else
  bad "an unblocker that answers --stage to the failing command ends on paused \
(at \`$(stage_of redstage)\`)"
fi
has "the Status Log names the command and the laps" \
  "\`redgate\` still fails after 3 unblocker laps — parked on \`paused\`" \
  $SPOOLWAY_PROJECT_HOME/queue/redstage.md
counter "three laps were handed back to the command, not a fourth" \
  steps "blocked->redgate" 3 $SPOOLWAY_PROJECT_HOME/queue/redstage.md

must "unattended off again, so nothing later in this file inherits it" \
  "$SPOOLWAY" config set unattended.enabled false

# ------------------------------------------------- the dispatcher's own binary
# A command step runs the `spoolway` its dispatcher is, even when an older one
# comes first on the person's PATH. A lane reporting with an older binary could
# not read the dispatcher's pipeline snapshot, and its task stayed `running`.
# A suite case, because it needs a real dispatcher process starting a real
# command, with a real PATH in front of it.
forget gated-stage redstage
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
OLDBIN="$LIVE/oldbin"
mkdir -p "$OLDBIN"
printf '#!/bin/sh\necho "spoolway 0.0.1"\n' > "$OLDBIN/spoolway"
chmod +x "$OLDBIN/spoolway"
add_command_step default whichbin \
  'command -v spoolway > which.txt' review implement
task_doc "$LIVE/whichbin.md" whichbin "$BODY" "group: whichbin"
must "a task queued behind a command step that asks which spoolway it has" \
  "$SPOOLWAY" queue add --from "$LIVE/whichbin.md"
dispatcher_stop
saved_path=$PATH
PATH="$OLDBIN:$PATH"; export PATH
dispatcher_start
PATH=$saved_path; export PATH
if drive whichbin gone 180; then ok "the task runs through, with an older spoolway first on PATH"
else bad "the task runs through, with an older spoolway first on PATH (at \`$(stage_of whichbin)\`)"; fi
got=$(git -C "$FORGE/origin.git" show "task/whichbin:which.txt" 2>/dev/null)
if [ "$got" = "$SPOOLWAY" ]; then
  ok "the command's spoolway is the dispatcher's own binary, not the older one"
else
  bad "the command's spoolway is the dispatcher's own binary, not the older one (got \`$got\`, want \`$SPOOLWAY\`)"
fi
dispatcher_stop

finish
