#!/usr/bin/env bash
# The override command surface, end to end: forking a knob out of the
# tracked checkout, seeing it listed, and promoting it back in.
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

finish
