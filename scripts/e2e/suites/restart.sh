#!/usr/bin/env bash
# The restart guard: a caller in a tight loop against a repo that cannot run
# is eventually refused, rather than restarted forever.
#
# `pipeline check` already refuses a cycle no `loop:` bounds, inside one
# pipeline's own graph. A caller restarting `spoolway dispatch` against a repo
# whose pipeline is broken — or that just keeps another dispatcher held —
# is the same shape one level up: nothing inside a single pass loops, but the
# process outside it does, and the engine has just as little business
# trusting that it will stop on its own. This is the guard that answers it,
# and the exit codes a script needs to tell an ordinary ending from a refusal.
#
# `dispatch.lane_child_ceiling` has no case here: it needs a lane actually
# holding a process open, which this suite's empty-queue and held-lock
# scenarios never do. See `src/dispatch.rs`'s own unit tests for that one.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

WORK=${WORK:-$(mktemp -d)}
new_repo "$WORK/proj"

works "init scaffolds .spoolway" "$SPOOLWAY" init
project_home_after_init
agent_models
# Pin the backend. Every start below is meant to be answered by the restart
# guard, the dispatch lock, the empty queue or the missing git identity — and
# `dispatch` checks its backend is reachable before it ever reaches the
# identity check. The shipped default is herdr, so on a machine with no herdr
# running the last scenario here is refused for the wrong reason. Headless
# needs nothing to be running and changes none of the answers this suite
# asserts.
must "the headless backend" "$SPOOLWAY" config set dispatch.backend headless
must "the spoolway commit" git add -A
must "the spoolway commit" git commit -qm "spoolway"
must "the plan branch" git checkout -q -b plan/demo

# ------------------------------------------------- starts that cannot run at all
# A live pid stands in for another dispatcher already serving this repo —
# `holder` only asks whether the pid is running, not what it is running.
# `Repo::lock_file` is `dispatch.pid` beside the rest of a project's runtime
# state under `Repo::home`, not the checkout's own `.spoolway/` — the two used
# to be the same directory, and a suite planting the pid in the checkout would
# silently miss the lock check and read straight through to the empty queue.
echo $$ > "$SPOOLWAY_PROJECT_HOME/dispatch.pid"

# Four in a row, each an ordinary "deferred", each its own exit code — not the
# generic 1 an error gets, because a script restarting this in a loop needs to
# tell "deferred" from "the guard has had enough" before the fifth call ever
# happens.
# `--plain`, on every call below: without it a start that finds the lock held
# does not return at all — it becomes a one-shot status watcher and sits
# printing the board until interrupted, which is the right thing for a person
# and a hang for a suite asserting an exit code.
for n in 1 2 3 4; do
  exit_code "start $n could not run and says so, not an error" 4 "$SPOOLWAY" dispatch --plain
done

# The fifth is refused outright: the guard, not the lock check, answers first.
says "a fifth start in the same window is refused" \
  "4 starts in a row could not run" "$SPOOLWAY" dispatch --plain
says "naming the last reason" "already running" "$SPOOLWAY" dispatch --plain
says "and the way out" "spoolway dispatch --force" "$SPOOLWAY" dispatch --plain
exit_code "with its own exit code" 5 "$SPOOLWAY" dispatch --plain

# --force starts one anyway — still deferred, because the lock is still held,
# but no longer refused — and clears the count behind it.
exit_code "--force starts one anyway" 4 "$SPOOLWAY" dispatch --plain --force
exit_code "and the guard is not still tripped on the very next start" 4 \
  "$SPOOLWAY" dispatch --plain

rm -f "$SPOOLWAY_PROJECT_HOME/dispatch.pid"

# ---------------------------------------------------------- an ordinary ending
# An empty queue is not a start that could not run — it is a fact about the
# project, and restarting into one forever is a caller's own choice, never the
# guard's business. Run past the threshold on empty queues alone and nothing
# trips: exit 3 every time, never 5.
for n in 1 2 3 4 5 6; do
  exit_code "an empty queue never counts against the guard (start $n)" 3 \
    "$SPOOLWAY" dispatch --plain
done

# ------------------------------- state that already exists: a workspace on the checkout
# The anchor sweep runs before anything else on every non-dry pass, so state
# already sitting in the multiplexer when this dispatch starts is exactly
# what it meets first. Against `herdr-stub.sh` rather than headless, because
# headless has no tabs to sweep at all: its double's workspace table is a
# TSV with one line per workspace, so a row bound to the project root — a
# person's own herdr workspace on the checkout, not any task's worktree —
# can stand there before the first pass. Two agent-free tabs on it must both
# survive the whole run, the one the dispatcher itself would be running in
# among them. A one-step command pipeline gives the run something to
# actually dispatch — an empty queue never reaches `pass` at all, let alone
# its sweep — and settles in one pass, so this needs no resident dispatcher.
# See `src/dispatch.rs`'s `sweep_anchor_tabs` and the bug this task tracks.
HSTATE="$WORK/herdr-stub"
HERDRBIN="$WORK/herdr-bin"
mkdir -p "$HSTATE" "$HERDRBIN"
install -m 755 "$HERE/../herdr-stub.sh" "$HERDRBIN/herdr"
export HERDR_STUB_STATE="$HSTATE"
PATH_BEFORE_HERDR_STUB="$PATH"
PATH="$HERDRBIN:$PATH"; export PATH
must "the herdr backend" "$SPOOLWAY" config set dispatch.backend herdr
must "herdr gives each task a workspace" "$SPOOLWAY" config set dispatch.herdr_mode split

cat > .spoolway/pipelines/selfsweep.yml <<'YML'
description: One command step that passes at once, so a run against the herdr double reaches an ordinary end in a single pass.

steps:
  - id: only
    description: Passes immediately; nothing about what it does is the point.
    run: 'true'
    on_pass: done
    on_fail: blocked
YML
works "the one-step pipeline checks out" "$SPOOLWAY" pipeline check

PROJECT_ROOT=$(pwd -P)
printf 'w-self\tself\t%s\n' "$PROJECT_ROOT" >>"$HSTATE/workspaces"
printf 'w-self:t1\tw-self\twork\n' >>"$HSTATE/tabs"
printf 'w-self:t2\tw-self\tdispatch\n' >>"$HSTATE/tabs"

SELFSWEEP_BODY="$WORK/selfsweep-body.md"
task_body "$SELFSWEEP_BODY"
task_doc selfsweep.md selfsweep "$SELFSWEEP_BODY" "group: demo" \
  "pipeline: selfsweep" "touches: [notes/selfsweep.md]"
must "a task queues behind the planted workspace" "$SPOOLWAY" queue add --from selfsweep.md

exit_code "the run settles once its one step passes, with a workspace on the checkout already present" 0 \
  "$SPOOLWAY" dispatch --plain --interval 1

# The planted rows by name, not the table's line count: the dispatcher's own
# tab (`spoolway/selfsweep`) lands in the same table once the run starts, so
# a count alone would still read 2 if the sweep took exactly one of the two
# planted tabs and left the dispatcher's own behind.
if [ "$(grep -c '^w-self:' "$HSTATE/tabs")" -eq 2 ]; then
  ok "neither of the project checkout's own tabs was swept along the way"
else
  bad "neither of the project checkout's own tabs was swept along the way: $(cat "$HSTATE/tabs")"
fi

"$HERDRBIN/herdr" shutdown state >/dev/null 2>&1 || true
unset HERDR_STUB_STATE
PATH="$PATH_BEFORE_HERDR_STUB"; export PATH
rm -f .spoolway/pipelines/selfsweep.yml
must "back to headless again" "$SPOOLWAY" config set dispatch.backend headless

# --------------------------------------------- refused before the lock: git identity
# A start that would actually try to run something and cannot — no git
# identity, and the shipped `handover` step reaches `spoolway stack`, which
# builds its squash commit with `commit-tree` whether or not
# `dispatch.auto_commit` is on — is refused outright, exit 1, and is not to
# be confused with the ordinary "nothing queued" ending above, exit 3. `git
# init` gave this checkout a local identity; unset it rather than reach for
# `--global`, since `new_repo` already pointed `$HOME` at a scratch
# directory with no `~/.gitconfig` of its own for a global unset to fall
# through to.
git config --unset user.email
git config --unset user.name

cat > identity-body.md <<'BODY'
## Goal

Add `src/notes.md`: one sentence saying what this repository is.

## Acceptance criteria

- `src/notes.md` exists

## References

- `docs/cli.md` — what this repository is
BODY
task_doc identity.md identity identity-body.md "group: demo" \
  "touches: [src/notes.md]"
must "a task queues" "$SPOOLWAY" queue add --from identity.md

exit_code "no git identity refuses outright, not an empty queue" 1 \
  "$SPOOLWAY" dispatch --plain
says "refusing to start" "refusing to start" "$SPOOLWAY" dispatch --plain
says "naming the missing key" "user.email" "$SPOOLWAY" dispatch --plain
says "and a command that sets it" "git config --global user.email" \
  "$SPOOLWAY" dispatch --plain

finish
