#!/usr/bin/env bash
# One `spoolway` per project: a second `spoolway dispatch` against a repo
# another dispatcher already holds refuses every time with the same line,
# `Dispatcher already running`, and its own exit code — never a count that
# eventually gives up and never one that is let through.
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

works "init scaffolds .spoolway" "$SPOOLWAY" init --yes
project_home_after_init
agent_models
# Pin the backend. Every start below is meant to be answered by the dispatch
# lock, the empty queue or the missing git identity — and `dispatch` checks
# its backend is reachable before it ever reaches the identity check. The
# shipped default is herdr, so on a machine with no herdr running the last
# scenario here is refused for the wrong reason. Headless needs nothing to
# be running and changes none of the answers this suite asserts.
#
# The marker with it: `spoolway dispatch` refuses `backend = headless`
# outright unless this is set, since the backend draws nowhere a person can
# see and only this harness may run it. Every other suite gets it from
# `fixture.sh`'s `configure_project`, which this one does not call — it
# builds its project by hand. Nothing here asserts on that refusal, and
# nothing here reaches it either: every start below is answered by a check
# that sits ahead of the pane gate. Exported anyway, so the day one of these
# scenarios does reach it, it is answered by one of the checks this suite is
# about rather than by a backend it only ever picked for being quiet.
export SPOOLWAY_TEST_BACKEND=1
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

# Five in a row, each an ordinary "deferred", each its own exit code — not
# the generic 1 an error gets — and each the exact same line, with no count
# behind it that eventually gives up and no count that lets a later one
# through either.
# A start that finds the lock held returns at once with exit 4 and one
# line either way — `dispatch` prints a line per pass now, not a board, so
# every call here is exactly the plain output and the exit code the suite
# is actually asserting on.
for n in 1 2 3 4 5; do
  exit_code "start $n could not run and says so, not an error" 4 "$SPOOLWAY" dispatch
  says "start $n prints the same line every time" \
    "Dispatcher already running" "$SPOOLWAY" dispatch
done

rm -f "$SPOOLWAY_PROJECT_HOME/dispatch.pid"

# ---------------------------------------------------------- an ordinary ending
# An empty queue is not a dispatcher already running — it is a fact about
# the project, and restarting into one forever is a caller's own choice.
# Exit 3 every time, never the lock's own exit 4.
for n in 1 2 3 4 5 6; do
  exit_code "an empty queue is an ordinary ending, not a refusal (start $n)" 3 \
    "$SPOOLWAY" dispatch
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
  "pipeline: selfsweep"
must "a task queues behind the planted workspace" "$SPOOLWAY" queue add --from selfsweep.md

exit_code "the run settles once its one step passes, with a workspace on the checkout already present" 0 \
  "$SPOOLWAY" dispatch

# The planted rows by name, not the table's line count: the task's own tab —
# renamed to its bare slug, `selfsweep`, the moment it is cut, rather than
# left on the `spoolway/selfsweep` its workspace itself is labelled — lands
# in the same table once the run starts, so a count alone would still read 2
# if the sweep took exactly one of the two planted tabs and left the task's
# own behind.
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
# A group of its own: `selfsweep` may still sit in the queue, and a group is
# one chain, so a second unrelated task under `demo` would be refused as a
# second root before the identity check this case is about ever ran.
task_doc identity.md identity identity-body.md "group: identity"
must "a task queues" "$SPOOLWAY" queue add --from identity.md

exit_code "no git identity refuses outright, not an empty queue" 1 \
  "$SPOOLWAY" dispatch
says "refusing to start" "refusing to start" "$SPOOLWAY" dispatch
says "naming the missing key" "user.email" "$SPOOLWAY" dispatch
says "and a command that sets it" "git config --global user.email" \
  "$SPOOLWAY" dispatch

finish
