#!/usr/bin/env bash
# The override command surface, end to end: forking a knob out of the
# tracked checkout, seeing it listed, and promoting it back in. Also carries
# the four new `contract` commands (`config`, `override`, `template`, `hook`)
# and `spoolway sync`'s removal of the skill directory `spoolway-pipeline`
# was renamed from — CLI-level checks with nowhere better-fitting to live,
# for the same reason the rest of this file is a suite rather than a unit
# test: what matters is the process actually wired up, not the render.
#
# Everything a unit test can decide about this already is — `src/overrides.rs`
# asserts the promoted file is byte-identical apart from the named values,
# `KEY_BLOCK` still findable, every `description:` intact, comments on an
# edited line preserved, empty layer directories swept — all against a
# fixture string, never a checkout. What none of that reaches is the one
# thing the Goal actually promises: that forking a knob touches nothing `git`
# can see, because the layer lives outside the checkout entirely, and that
# promoting one turns into exactly the diff a person would commit. That needs
# a real git repository with a real working tree to ask `git status` and
# `git diff` of, so it is a suite rather than another case in
# `src/overrides.rs`.
#
# No `covers:` tag of its own — see `commands.sh`'s own note: the map
# `coverage.sh` builds only enumerates `config.toml` keys and pipeline step
# keys, and this is CLI behaviour, not a setting.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

LIVE=${WORK:-$(mktemp -d)}

new_repo "$LIVE/proj"
configure_project plan/live

TRACKED=.spoolway/pipelines/default.yml
LAYER="$SPOOLWAY_PROJECT_HOME/overrides/pipelines/default.yml"

# `configure_project` points every Claude step's blank model at the cloud
# stand-in `set_models` installs — the `default` pipeline's `implement` step
# among them — so this is the value already sitting in the tracked file
# before anything here forks it.
OLD_MODEL=$(local_model default)

git_clean() { test -z "$(git status --porcelain)"; }
git_dirty() { ! git_clean; }

works "the checkout starts clean" git_clean

says "pipeline override writes the patch and shows the change" \
  "implement.model   $OLD_MODEL -> fake-opus" \
  "$SPOOLWAY" pipeline override default --set implement.model=fake-opus

works "the patch landed in the layer, not the checkout" \
  test -f "$LAYER"
works "forking a knob leaves the tracked checkout untouched" \
  git_clean
says "the tracked file itself is unmoved" "model: $OLD_MODEL" \
  cat "$TRACKED"

says "override list names the pipeline patch and its key" \
  "implement.model" \
  "$SPOOLWAY" override list
says "and prints the artifact count in its footer" \
  "1 artifact" \
  "$SPOOLWAY" override list
says "naming the layer's own fingerprint, not the tracked files' combined one" \
  "layer version" \
  "$SPOOLWAY" override list

says "override promote writes the tracked file and reports the new value" \
  "implement.model   fake-opus" \
  "$SPOOLWAY" override promote default

says "the tracked file now carries the promoted value" \
  "model: fake-opus" \
  cat "$TRACKED"
says "the fenced key block survives the edit" \
  ">>> spoolway >>>" \
  cat "$TRACKED"
says "so does the step description beside the promoted key" \
  "Write the code to satisfy the task's acceptance criteria." \
  cat "$TRACKED"

works "promoting cleared the layer entry" \
  test ! -e "$LAYER"
says "override list says the layer is empty again" \
  "no overrides" \
  "$SPOOLWAY" override list

works "the promote is a real, uncommitted change to the tracked file" \
  git_dirty
says "and it is exactly the one line the patch named — nothing else moved" \
  " 1 file changed, 1 insertion(+), 1 deletion(-)" \
  git diff --stat -- "$TRACKED"

# --------------------------------------------------- dispatch's gate, no tty
# Every command above ran through `$(...)`, which never hands the child a
# terminal — so this is already the case the acceptance criteria asks for:
# with a layer present and stdout not a terminal, `dispatch` must print the
# notice and proceed without ever waiting on a key nobody can press.
# `src/commands/dispatch.rs`'s own unit tests cover the same shape directly
# against `overrides_gate_with`, with a scripted reader standing in for the
# terminal; what only a real process can show is that a real `spoolway
# dispatch` invocation, piped the way every script pipes it, never blocks on
# this at all.
BODY="$LIVE/body.md"
task_body "$BODY"
task_doc "$LIVE/gate.md" gate "$BODY" "group: live" "touches: [notes/gate.md]"
must "a task to dispatch against" "$SPOOLWAY" queue add --from "$LIVE/gate.md"

# The regression this guards: `overrides_gate`'s own `TermGuard` used to be
# taken unconditionally, so a piped dispatch printed a hide/show-cursor
# escape as its first bytes even with no layer at all. With nothing
# overridden yet, this run must be byte-for-byte silent about the cursor.
silent_about "a piped dispatch with no layer never touches the cursor" \
  $'\x1b' \
  "$SPOOLWAY" dispatch --dry-run

must "forking a knob again, for the gate" \
  "$SPOOLWAY" pipeline override default --set implement.model=fake-opus

says "dispatch prints the layer's notice with no tty to ask" \
  "overrides are active for this project" \
  "$SPOOLWAY" dispatch --dry-run
says "naming the pipeline it touches" \
  "pipelines/default.yml" \
  "$SPOOLWAY" dispatch --dry-run
works "and proceeds without anybody there to answer" \
  timeout 10 "$SPOOLWAY" dispatch --dry-run

# ------------------------------------------------- the four new contracts
# Each one is a unit-tested render in src/commands/{config,override,template,
# hook}.rs already; what a unit test cannot see is the command actually
# wired up end to end — clap routing the subcommand, `main.rs` calling the
# right function, and the process exiting zero with something on stdout.
works "config contract exits zero" "$SPOOLWAY" config contract
says "and prints the register config.toml's own header renders from" \
  "dispatch.backend" \
  "$SPOOLWAY" config contract

works "override contract exits zero" "$SPOOLWAY" override contract
says "and names the merge rule" \
  "already exist on the tracked pipeline" \
  "$SPOOLWAY" override contract

works "template contract exits zero" "$SPOOLWAY" template contract
says "and names the one shape it still has" \
  "TASK —" \
  "$SPOOLWAY" template contract

works "hook contract exits zero" "$SPOOLWAY" hook contract
says "and names the events a hook script runs on" \
  "SPOOLWAY_EVENT" \
  "$SPOOLWAY" hook contract

# ----------------------------------------------- sync removes the old skill
# `spoolway-pipeline` was renamed to `spoolway-config`; a project that ran
# `install` before the rename has the old directory on disk, and `sync`
# is the one chance to clean it up without touching anything else there.
STALE=.claude/skills/spoolway-pipeline
UNRELATED=.claude/skills/a-projects-own-skill
mkdir -p "$STALE" "$UNRELATED"
echo "the old skill" > "$STALE/SKILL.md"
echo "not spoolway's" > "$UNRELATED/SKILL.md"

must "sync, to clean up the rename" "$SPOOLWAY" sync

works "the stale renamed skill is gone" test ! -e "$STALE"
works "a directory not on the retired list is untouched" \
  test -f "$UNRELATED/SKILL.md"
works "and the renamed skill itself is installed" \
  test -f .claude/skills/spoolway-config/SKILL.md

finish
