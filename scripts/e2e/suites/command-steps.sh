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
# covers: step.loop — a command step's own failure feeds the loop bound on the agent step behind it, the shape a mechanical CI gate is built on
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
      *)            printf '    on_fail: %s\n' "$extra"
                    # A failure routed back to the step ahead of this one is a
                    # cycle of its own — `implement` → this step → `implement`.
                    # It needs a bound whose exit leaves the cycle, and this
                    # step`s own `on_pass` is the one that does.
                    printf '    loop:\n      %s: 1\n' "$extra"
                    printf '    on_loop_max: %s\n' "$on_pass" ;;
    esac
    printf '    on_pass: %s\n' "$on_pass"
  } >> "$file"

  # `implement` goes through the new step now, so its `on_pass` names the new
  # step rather than `review`.
  #
  # `review`'s own budget is left alone. A `loop:` map names the steps a step
  # *sends a task to*, so `review`'s entry is `implement` — the step its
  # `on_fail` routes back to — and inserting a step ahead of `review` does not
  # change that. Rewriting it to name the new step would name a route `review`
  # does not have, and is refused at load.
  sed -i "0,/^    on_pass: review$/s//    on_pass: $id/" "$file"
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

task_doc "$LIVE/land.md" land "$BODY" "group: live" "touches: [notes/land.md]"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/land.md"

# `group list` reads `group:` verbatim off the queue — the same field `land`
# just declared, and the same task id it named.
says "group list names the group a queued task declared" "live" \
  "$SPOOLWAY" group list
says "and the task itself" "land" "$SPOOLWAY" group list

# Read while the task is still moving: the archive step reclaims this log.
records "the command's own record says it ran" "built land" \
  "$SPOOLWAY_PROJECT_HOME/commands/land · build.log" land

if drive land gone 60; then ok "a task runs straight through its command step"
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

# Said with nothing running, so the only thing that could move the task is the
# dry run itself — which may not.
dispatcher_stop
task_doc "$LIVE/opener.md" opener "$BODY" "group: live" "touches: [notes/opener.md]"
must "a task queued on it" "$SPOOLWAY" queue add --from "$LIVE/opener.md"
says "a dry run says it would start the entry command step" "would start \`prime\`" \
  "$SPOOLWAY" dispatch --dry-run
if [ "$(stage_of opener)" = queued ]; then
  ok "and wrote nothing: the task is where it was"
else
  bad "and wrote nothing: the task is where it was (at \`$(stage_of opener)\`)"
fi

# Caught in flight, ahead of the archive step that reclaims this log.
records "the entry command ran, before any lane existed" "primed opener" \
  "$SPOOLWAY_PROJECT_HOME/commands/opener · prime.log" opener

if drive opener gone 60; then ok "a task whose entry is a command step does not sit in \`queued\`"
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
# does not refuse — it draws the read-only board instead, polling until
# somebody stops it. That branch sits *above* the routing guard in `run`, so
# with the lock held this dry run never reaches the refusal under test and
# never returns either; the suite hangs until the job's own timeout kills it
# rather than failing on the line that is wrong. The dry run in the scenario
# above only worked because it had its own `dispatcher_stop` in front of it.
dispatcher_stop
task_doc "$SPOOLWAY_PROJECT_HOME/queue/routeless.md" routeless "$BODY" \
  "stage: queued" "group: live" "touches: [notes/routeless.md]" "pipeline:"
OUT=$("$SPOOLWAY" dispatch --dry-run 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ]; then ok "dispatch refuses the whole start over a routeless task"
else bad "dispatch refuses the whole start over a routeless task"; fi
if grep -qF "refusing to start: task \`routeless\` has no \`pipeline:\`" <<<"$OUT"; then
  ok "and names the task"
else bad "and names the task"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF "Set \`pipeline:\` to one of:" <<<"$OUT" && grep -qF "default" <<<"$OUT"; then
  ok "and lists the pipelines it could choose"
else bad "and lists the pipelines it could choose"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF "Nothing was dispatched." <<<"$OUT"; then
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
add_command_step default build "echo 'the build is broken' >&2; exit 2" review implement

task_doc "$LIVE/broken.md" broken "$BODY" "group: live" "touches: [notes/broken.md]"
must "a task whose build fails" "$SPOOLWAY" queue add --from "$LIVE/broken.md"

# Its stderr, read while the task is still looping — the archive step reclaims
# this log, so a `has` after `drive broken gone` would find nothing.
records "what the command printed is on the record" "the build is broken" \
  "$SPOOLWAY_PROJECT_HOME/commands/broken · build.log" broken

if drive broken gone 60; then ok "a task whose command fails still reaches the end"
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
# returns to the agent step behind it, that step bounded so a change which
# cannot be made green stops rather than circling forever. Nothing here is
# specific to `cargo` or to end-to-end suites — the graph is the whole of what
# is under test, so a stand-in agent and a command that is always red are
# enough to exercise it.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
{
  printf '\n  - id: e2e\n'
  printf "    description: A stand-in for a mechanical gate's own agent step.\n"
  printf '    agent: pi\n    prompt: implementer\n    model: fake-local\n'
  printf '    loop:\n      gate: 1\n'
  printf '    on_loop_max: blocked\n    on_pass: gate\n    on_fail: blocked\n'
  printf '\n  - id: gate\n'
  printf '    description: Always red, so the loop it bounds is what this case is about.\n'
  printf "    run: echo 'CI would fail here' >&2; exit 1\n"
  printf '    on_pass: document\n    on_fail: e2e\n'
} >> .spoolway/pipelines/default.yml
# `review` fell through to `document` directly; put `e2e` and its gate between
# them. The `0,/…/` range keeps this to the first match — `review`'s, not the
# `gate` step's own `on_pass: document` appended just above.
sed -i "0,/^    on_pass: document\$/s//    on_pass: e2e/" .spoolway/pipelines/default.yml
works "a pipeline whose gate loops back to the agent step before it checks out" \
  "$SPOOLWAY" pipeline check

task_doc "$LIVE/gated.md" gated "$BODY" "group: live" "touches: [notes/gated.md]"
must "a task whose gate never turns green" "$SPOOLWAY" queue add --from "$LIVE/gated.md"
if drive gated blocked 90; then
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

# The same shape with `loop: gate: 2` rather than `1` — every shipped pipeline
# now carries 2, not 1, so a route bounded at 2 is what a reviewer's fix
# actually gets: seen once before the budget is spent, not zero times. The
# exit is taken on the third arrival rather than the second — two allowed
# laps banked, and only the third attempt diverted.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
{
  printf '\n  - id: e2e\n'
  printf "    description: A stand-in for a mechanical gate's own agent step.\n"
  printf '    agent: pi\n    prompt: implementer\n    model: fake-local\n'
  printf '    loop:\n      gate: 2\n'
  printf '    on_loop_max: blocked\n    on_pass: gate\n    on_fail: blocked\n'
  printf '\n  - id: gate\n'
  printf '    description: Always red, so the loop it bounds is what this case is about.\n'
  printf "    run: echo 'CI would fail here' >&2; exit 1\n"
  printf '    on_pass: document\n    on_fail: e2e\n'
} >> .spoolway/pipelines/default.yml
sed -i "0,/^    on_pass: document\$/s//    on_pass: e2e/" .spoolway/pipelines/default.yml
works "a pipeline whose gate loops back to the agent step, bounded at 2" \
  "$SPOOLWAY" pipeline check

task_doc "$LIVE/gated-twice.md" gated-twice "$BODY" "group: live" "touches: [notes/gated-twice.md]"
must "a task whose gate never turns green, on a loop of 2" \
  "$SPOOLWAY" queue add --from "$LIVE/gated-twice.md"
if drive gated-twice blocked 90; then
  ok "a loop of 2 still stops the task rather than circling forever"
else
  bad "a loop of 2 still stops the task rather than circling forever \
(at \`$(stage_of gated-twice)\`)"
fi
# `arrivals` counts every line naming `→ \`e2e\``, and this log carries three
# of them: the entry from `review`, and the two laps `gate` sent back. The
# note `apply_loop_budget` writes on the third attempt is *not* among them —
# it reads "`gate` may not send this back to `e2e` a 3rd time", naming the
# move it is refusing rather than one it made, so it no longer answers a grep
# for arrivals. The two real laps are what the `rounds` counter below proves;
# this only checks that the third attempt left no further arrival behind it.
if [ "$(arrivals $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md e2e)" -eq 3 ]; then
  ok "the third attempt spent the budget rather than arriving at e2e again"
else
  bad "the third attempt spent the budget rather than arriving at e2e again \
(arrived at \`e2e\` $(arrivals $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md e2e) \
time(s), wanted 3)"
fi
counter "and the counter agrees: two laps banked, not one" \
  rounds "gate->e2e" 2 $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md

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

task_doc "$LIVE/cant-launch.md" cant-launch "$BODY" "group: live" \
  "touches: [notes/cant-launch.md]"
must "a task whose command step can never even start" \
  "$SPOOLWAY" queue add --from "$LIVE/cant-launch.md"

if drive cant-launch blocked 60; then
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

task_doc "$LIVE/stage-flag.md" stage-flag "$BODY" "group: live" \
  "touches: [notes/stage-flag.md]"
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
  "sleep 20; echo 'never finishes in time' > \"\$SPOOLWAY_REPO/bench-done.txt\"" \
  review --background
works "a background command step checks out" "$SPOOLWAY" pipeline check
says "and show says it does not wait" "bench      command   background" \
  "$SPOOLWAY" pipeline show

dispatcher_restart   # the pipeline it is holding has no `bench` step in it
task_doc "$LIVE/quick.md" quick "$BODY" "group: live" "touches: [notes/quick.md]"
must "a task with a background step" "$SPOOLWAY" queue add --from "$LIVE/quick.md"

# Watched by hand rather than with `drive`, for one reason: the pid has to be
# read while the run is still going, and the whole claim under test is that the
# task does not stop there long enough to be caught waiting.
BENCH_PID=""
for _ in $(seq 1 300); do
  if [ -z "$BENCH_PID" ] && [ -s "$SPOOLWAY_PROJECT_HOME/commands/quick · bench.pid" ]; then
    BENCH_PID=$(cat "$SPOOLWAY_PROJECT_HOME/commands/quick · bench.pid")
  fi
  [ -z "$(stage_of quick)" ] && break
  sleep 0.2
done

if [ -n "$BENCH_PID" ]; then ok "the background command really was started"
else bad "the background command really was started"; fi
if [ -z "$(stage_of quick)" ]; then
  ok "the task ran the whole pipeline without waiting for it"
else
  bad "the task ran the whole pipeline without waiting for it (at \`$(stage_of quick)\`)"
fi
# The command sleeps for twenty seconds and the pipeline is done in under one.
# If this file exists, something waited.
if [ -f "$LIVE/proj/bench-done.txt" ]; then
  bad "nothing waited for it — that is what background means"
else
  ok "nothing waited for it — that is what background means"
fi

# A background run outlives the step that started it and must not outlive the
# worktree it is running in: cleanup takes it down with the task.
if [ -n "$BENCH_PID" ] && poll_while 10 test -d "/proc/$BENCH_PID"; then
  ok "cleanup stops a background command rather than orphaning it"
else
  bad "cleanup stops a background command rather than orphaning it (pid $BENCH_PID)"
fi

# --------------------------------------------------------- background, on_fail
# The refusal `pipeline check` used to make against `background: true` plus
# `on_fail:` is gone — a background command that fails now routes the task
# down its `on_fail`, whichever step the task has since reached. Proven with
# two file gates rather than a sleep, so the assertion does not race the mock
# pipeline's own speed (the whole thing above ran in under a second): `scratch`
# only fails once told to, and `hold`, spliced in right after it, only passes
# once told to — so the task is provably sitting well past `scratch`, still
# going, when the failure lands.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
SCRATCH_FAIL="$LIVE/scratch-fail-now"
SCRATCH_HOLD="$LIVE/scratch-hold-release"
rm -f "$SCRATCH_FAIL" "$SCRATCH_HOLD"
{
  printf '\n  - id: scratch\n'
  printf '    description: A background step whose command fails once told to.\n'
  printf '    run: while [ ! -f %q ]; do sleep 0.2; done; echo scratch-failed >&2; exit 1\n' \
    "$SCRATCH_FAIL"
  printf '    background: true\n'
  printf '    on_pass: hold\n'
  printf '    on_fail: blocked\n'
  printf '\n  - id: hold\n'
  printf '    description: Holds the task here so the test can prove it moved past scratch.\n'
  printf '    run: while [ ! -f %q ]; do sleep 0.2; done\n' "$SCRATCH_HOLD"
  printf '    on_pass: review\n'
} >> .spoolway/pipelines/default.yml
# The same rewrite `add_command_step` makes, done by hand for this splice:
# `implement`'s own `on_pass: review` becomes the entry into `scratch`.
# `review`'s `loop: implement: 2` is left alone — a budget names the step its
# owner sends a task *to*, and `review` still sends back to `implement`.
sed -i "0,/^    on_pass: review\$/s//    on_pass: scratch/" .spoolway/pipelines/default.yml
works "a background step that also declares on_fail checks out" "$SPOOLWAY" pipeline check

dispatcher_restart   # the pipeline it is holding has neither new step in it
task_doc "$LIVE/scratch-fail.md" scratch-fail "$BODY" "group: live" \
  "touches: [notes/scratch-fail.md]"
must "a task behind a background step that will later fail" \
  "$SPOOLWAY" queue add --from "$LIVE/scratch-fail.md"

if drive scratch-fail hold 60; then
  ok "the task moved on past the background step while its command was still running"
else
  bad "the task moved on past the background step while its command was still running \
(at \`$(stage_of scratch-fail)\`)"
fi

touch "$SCRATCH_FAIL"
if drive scratch-fail blocked 60; then
  ok "the background command's failure still reached the task, at the step it moved on to"
else
  bad "the background command's failure still reached the task, at the step it moved on to \
(at \`$(stage_of scratch-fail)\`)"
fi
# The route is narrated on the dispatcher's own record, same as the mockup —
# not the task's own Status Log, which `set_stage` writes with no message here,
# the same as every other route a fall-through or a command step's ordinary
# `on_fail` takes.
if wait_for_text 20 "$E2E_DISPATCH_LOG" \
  '`scratch` (background) exited 1 — moving to `blocked`'; then
  ok "and the route taken is on the record"
else
  bad "and the route taken is on the record"
fi
has "blocked_from names the step it was actually pulled out of, not \`scratch\` itself" \
  "blocked_from: hold" $SPOOLWAY_PROJECT_HOME/queue/scratch-fail.md

# Let the stranded `hold` command finish rather than leave it running for the
# rest of the suite — its own task has already moved on to `blocked`, so
# nothing is waiting on it any more.
touch "$SCRATCH_HOLD"

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
  "sleep 20; echo 'no-setsid done' > \"\$SPOOLWAY_REPO/no-setsid-done.txt\"" \
  review --background
must "marking it headless: true" \
  sed -i 's|^    background: true$|    background: true\n    headless: true|' \
  .spoolway/pipelines/default.yml
works "a headless background command step checks out" "$SPOOLWAY" pipeline check

REAL_SPOOLWAY="$SPOOLWAY"
SPOOLWAY="$NO_SETSID_SPOOLWAY"
dispatcher_restart   # both the new step and the setsid-less binary are new
task_doc "$LIVE/nosetsid.md" nosetsid "$BODY" "group: live" \
  "touches: [notes/nosetsid.md]"
must "a task with a headless step, dispatched with no setsid on PATH" \
  "$SPOOLWAY" queue add --from "$LIVE/nosetsid.md"

# Watched by hand for the same reason `bench` above is: the pid has to be
# read while the run is still going. Stopped the moment the pid file
# appears, rather than waiting for the task to finish — a background step's
# whole task can archive inside the same pass that started it, and cleanup
# at archival is what takes the process down (see the `bench` case above),
# so waiting past that point would be checking the wrong thing.
NOSETSID_PID=""
for _ in $(seq 1 300); do
  if [ -s "$SPOOLWAY_PROJECT_HOME/commands/nosetsid · nosetsid.pid" ]; then
    NOSETSID_PID=$(cat "$SPOOLWAY_PROJECT_HOME/commands/nosetsid · nosetsid.pid")
    break
  fi
  sleep 0.2
done

if [ -n "$NOSETSID_PID" ]; then
  ok "a headless command step still starts and writes its pid with no setsid on PATH"
else
  bad "a headless command step still starts and writes its pid with no setsid on PATH"
fi
# Caught right as the pid file appears: the process is up on its own, with
# nobody waiting on it, which is what outliving the pass that spawned it
# means.
if [ -n "$NOSETSID_PID" ] && [ -d "/proc/$NOSETSID_PID" ]; then
  ok "and it outlives the pass that started it"
else
  bad "and it outlives the pass that started it (pid $NOSETSID_PID)"
fi
# Cleaned up the same way `bench` is, rather than left running into whatever
# this suite does next.
if [ -n "$NOSETSID_PID" ] && poll_while 10 test -d "/proc/$NOSETSID_PID"; then
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
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
add_command_step default build "sleep 300" review implement
must "a timeout short enough for a suite to reach" \
  sed -i 's|^    run: sleep 300$|    run: sleep 300\n    timeout: 3s|' \
  .spoolway/pipelines/default.yml
works "a step may name its own timeout" "$SPOOLWAY" pipeline check
says "and show resolves it" "timeout=3s" "$SPOOLWAY" pipeline show

task_doc "$LIVE/hung.md" hung "$BODY" "group: live" "touches: [notes/hung.md]"
must "a task whose command hangs" "$SPOOLWAY" queue add --from "$LIVE/hung.md"
if drive hung gone 90; then ok "a hung command does not park its task forever"
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

task_doc "$LIVE/unconfined.md" unconfined "$BODY" "group: live" \
  "touches: [notes/unconfined.md]"
must "a task whose command writes outside its worktree" \
  "$SPOOLWAY" queue add --from "$LIVE/unconfined.md"
if drive unconfined gone 60; then ok "a command step is not confined to its worktree"
else bad "a command step is not confined to its worktree (at \`$(stage_of unconfined)\`)"; fi
works "and it really did write where no lane could" test -f "$OUTSIDE"

# ------------------------------------------------------------------ a pane, or none
# The default now: a command step with no `headless:` runs in a pane of its
# own, under a real multiplexer. `headless: true` is the escape hatch back to
# today's silent, detached run. Neither half is provable against the headless
# backend the rest of this suite runs on — a pane is the one thing only a real
# tmux server can be asked whether it opened — so this switches to it for as
# long as the case needs, the same way `disaster.sh`'s own tmux case does.
if ! command -v tmux >/dev/null 2>&1; then
  echo "  skipped — no \`tmux\` on PATH, and this case needs a real server"
else
  SOCK="$LIVE/tmux-pane.sock"
  export SPOOLWAY_TMUX_SOCKET="$SOCK"
  # The tasks below walk a whole pipeline of agent steps to get at two claims
  # about a command step, and every one of those lanes is a tmux pane the
  # stand-in closes by returning from it — before spoolway has finished
  # launching it. See `agents/transcript.sh`'s own note: a second of sitting
  # still is what a real agent's prompt gives spoolway for free, and it is
  # read by no lane outside this block.
  export E2E_PANE_LINGER=1
  must "the tmux backend" "$SPOOLWAY" config set dispatch.backend tmux
  must "tmux runs split" "$SPOOLWAY" config set dispatch.tmux_mode split

  # A tmux pane starts life with the *server's* environment, never the
  # dispatcher's — unlike a headless run's child process, which inherits by
  # ordinary fork/exec. `Mux::run_in_pane` is handed the dispatcher's own
  # environment as an inherited layer for exactly this reason, and this is
  # the one variable set here specifically so nothing on the harness's own
  # PATH could already carry it: a false pass would mean nothing.
  export SPOOLWAY_E2E_PANE_ENV_MARKER="from-the-dispatchers-own-environment"

  # The visible half: no `headless:` key, so the command gets a pane of its
  # own — long enough that a poll can catch it standing while the command
  # runs, and short enough that the suite is not built around a sleep.
  cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
  add_command_step default visible \
    "sleep 2; echo visible-pane-marker; echo \"env:\$SPOOLWAY_E2E_PANE_ENV_MARKER\"" \
    review ""
  works "a pipeline with a paned command step checks out" "$SPOOLWAY" pipeline check

  # Restarted after the export above, so the dispatcher this starts is the
  # one that inherited it — see `dispatcher_restart` a few lines up for why
  # that ordering matters.
  dispatcher_restart
  task_doc "$LIVE/paned.md" paned "$BODY" "group: live" "touches: [notes/paned.md]"
  must "a task through a paned command step" \
    "$SPOOLWAY" queue add --from "$LIVE/paned.md"

  FOUND_PANE=""
  for _ in $(seq 1 100); do
    FOUND_PANE=$(tmux -S "$SOCK" list-panes -a -F '#{pane_title}' 2>/dev/null \
      | grep -F "paned · visible" || true)
    [ -n "$FOUND_PANE" ] && break
    sleep 0.1
  done
  if [ -n "$FOUND_PANE" ]; then
    ok "a command step with no headless: key runs in a pane of its own"
  else
    bad "a command step with no headless: key runs in a pane of its own"
  fi

  # Read while the task is still moving — the archive step reclaims this log.
  # `records` keeps a `.kept` copy so the env-marker check below still has a
  # file to read after `drive paned gone` has deleted the original.
  records "the pane's own output is on the record just the same" "visible-pane-marker" \
    "$SPOOLWAY_PROJECT_HOME/commands/paned · visible.log" paned
  has "a variable only the dispatcher's own environment carried reached the tmux pane" \
    "env:from-the-dispatchers-own-environment" \
    "$SPOOLWAY_PROJECT_HOME/commands/paned · visible.log.kept"

  if drive paned gone 60; then ok "the task carries on once the command has passed"
  else bad "the task carries on once the command has passed (at \`$(stage_of paned)\`)"; fi
  unset SPOOLWAY_E2E_PANE_ENV_MARKER
  if tmux -S "$SOCK" list-panes -a -F '#{pane_title}' 2>/dev/null \
      | grep -qF "paned · visible"; then
    bad "a passing command's pane closes behind it"
  else
    ok "a passing command's pane closes behind it"
  fi

  # The hidden half: the same shape, with `headless: true` added — today's
  # silent, detached run, and no pane ever asked for.
  cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
  add_command_step default hidden "sleep 2; echo hidden-command-marker" review ""
  must "adding headless: true to it" \
    sed -i 's|^    run: sleep 2; echo hidden-command-marker$|    run: sleep 2; echo hidden-command-marker\n    headless: true|' \
    .spoolway/pipelines/default.yml
  works "a pipeline naming headless: true checks out" "$SPOOLWAY" pipeline check
  says "and show marks it" "hidden     command   waits headless timeout=30m" \
    "$SPOOLWAY" pipeline show

  dispatcher_restart
  task_doc "$LIVE/hiddenc.md" hiddenc "$BODY" "group: live" "touches: [notes/hiddenc.md]"
  must "a task through a headless command step" \
    "$SPOOLWAY" queue add --from "$LIVE/hiddenc.md"
  # Caught in flight, ahead of the archive step that reclaims this log.
  records "and its output is on the record just the same" "hidden-command-marker" \
    "$SPOOLWAY_PROJECT_HOME/commands/hiddenc · hidden.log" hiddenc

  if drive hiddenc gone 60; then ok "a headless command step still routes on its exit code"
  else bad "a headless command step still routes on its exit code (at \`$(stage_of hiddenc)\`)"; fi
  if tmux -S "$SOCK" list-panes -a -F '#{pane_title}' 2>/dev/null \
      | grep -qF "hiddenc · hidden"; then
    bad "headless: true never opened a pane at all"
  else
    ok "headless: true never opened a pane at all"
  fi

  # A failing, paned command closes its pane the moment the task leaves the
  # step for `blocked` — nothing is left standing on the chance of a retry.
  cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
  add_command_step default flaky "echo flaky-pane-marker; exit 3" review ""
  works "a pipeline with a failing paned step checks out" "$SPOOLWAY" pipeline check

  dispatcher_restart
  task_doc "$LIVE/panedfail.md" panedfail "$BODY" "group: live" \
    "touches: [notes/panedfail.md]"
  must "a task whose paned step fails" \
    "$SPOOLWAY" queue add --from "$LIVE/panedfail.md"
  if drive panedfail blocked 60; then ok "a failing paned command still routes on its exit code"
  else bad "a failing paned command still routes on its exit code (at \`$(stage_of panedfail)\`)"; fi
  if tmux -S "$SOCK" list-panes -a -F '#{pane_title}' 2>/dev/null \
      | grep -qF "panedfail · flaky"; then
    bad "and its pane closes behind the failure rather than standing"
  else
    ok "and its pane closes behind the failure rather than standing"
  fi

  tmux -S "$SOCK" kill-server 2>/dev/null || true
  unset SPOOLWAY_TMUX_SOCKET E2E_PANE_LINGER
  must "back to headless" "$SPOOLWAY" config set dispatch.backend headless
fi

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

must "the herdr backend" "$SPOOLWAY" config set dispatch.backend herdr
must "herdr gives each task a workspace" "$SPOOLWAY" config set dispatch.herdr_mode split

# The two variables this case is about, both set before the dispatcher starts
# so they are really part of the environment it inherited.
#
# The bulk one is the point: eight kilobytes in a single value, comfortably
# past the length a pane's prompt used to cut an `export` line at. The marker
# is how a false pass is ruled out — nothing on the harness's own PATH carries
# it, and the pane's shell is started with no environment at all.
export SPOOLWAY_E2E_PANE_ENV_MARKER="from-the-dispatchers-own-environment"
SPOOLWAY_E2E_PANE_BULK=$(head -c 8000 /dev/zero | tr '\0' 'x'); export SPOOLWAY_E2E_PANE_BULK

# A pipeline of one paned command step, so this case needs no agent lane and
# the double needs no agent to start. The step reads both variables back out.
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
sed "s|@RELEASE@|$CARRY_RELEASE|" > .spoolway/pipelines/herdrpane.yml <<'YML'
description: One paned command step, for the environment a pane is handed.

steps:
  - id: carry
    description: Read back the environment the dispatcher was started with.
    run: 'echo "env:$SPOOLWAY_E2E_PANE_ENV_MARKER"; echo "bulk:${#SPOOLWAY_E2E_PANE_BULK}"; while [ ! -e "@RELEASE@" ]; do sleep 0.1; done'
    timeout: 120s
    on_pass: done
    on_fail: blocked
YML
works "a one-step paned pipeline checks out" "$SPOOLWAY" pipeline check

dispatcher_restart
task_doc "$LIVE/carried.md" carried "$BODY" "group: live" \
  "pipeline: herdrpane" "touches: [notes/carried.md]"
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

# `herdr-stub.sh` keeps every string spoolway typed into a pane, with its
# length. The environment is 8KB; nothing typed may be anywhere near that.
TYPED_MAX=$(awk -F'\t' 'BEGIN{m=0} $2>m {m=$2} END{print m+0}' "$HSTATE/typed.index")
if [ "$TYPED_MAX" -lt 2000 ]; then
  ok "and no single line typed into a pane was longer than $TYPED_MAX bytes"
else
  bad "nothing long is typed into a pane (longest was $TYPED_MAX bytes)"
  cut -f1,2 "$HSTATE/typed.index"
fi
if grep -rqF -- ". '$HANDOVER'" "$HSTATE/typed"; then
  ok "and one of them is the dot command that sources the file"
else
  bad "and one of them is the dot command that sources the file"
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
# finish, and the task carries on from a pass like any other.
touch "$CARRY_RELEASE"
if drive carried gone 60; then ok "the task carries on once the paned command has passed"
else bad "the task carries on once the paned command has passed (at \`$(stage_of carried)\`)"; fi
rm -f "$CARRY_RELEASE"

unset SPOOLWAY_E2E_PANE_ENV_MARKER SPOOLWAY_E2E_PANE_BULK
rm -f .spoolway/pipelines/herdrpane.yml
must "back to headless again" "$SPOOLWAY" config set dispatch.backend headless
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


finish
