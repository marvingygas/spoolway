#!/usr/bin/env bash
# A project with no `.spoolway/` anywhere in its checkout, run for real.
#
# Home mode keeps a checkout's setup in a workspace under `~/.spoolway/` —
# `config/` for the setup, `dispatchers/<clone>/` for the queue, archive and
# worktrees — and finds it by the checkout's own path in the workspace's
# `project.toml`, with no id stamped into `.git`. Which files that resolves to
# is unit-tested in src/repo.rs. What a unit test cannot reach is the rest of
# the run: a detached lane in a linked worktree, and the `spoolway report` and
# `spoolway stack` it runs from there, each finding the one workspace from a
# checkout that is neither the clone nor carries a `.spoolway/` of its own.
#
# The workspace is written by hand, as the task that introduced it says it
# must be until `init` learns to write one: the project is initialised the
# ordinary way, and its setup is then moved out of the checkout.
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
# No worktree root of our own: where a lane's worktree is cut by default is
# part of what home mode moves, into the clone's dispatcher folder.
configure_project plan/live

# ------------------------------------------------------ into a workspace
# The setup moves to `config/`, the checkout forgets it ever had one, and the
# repo-mode home and its stamp go with it — so nothing but the workspace's
# `clones` list can answer for this checkout any more.
CLONE=$(pwd -P)
WS="$HOME/.spoolway/proj-ws"
mkdir -p "$WS/config" "$WS/dispatchers"
must "the setup moves into the workspace" cp -a .spoolway/. "$WS/config/"
must "the checkout drops its setup" git rm -rq .spoolway
must "the checkout drops its setup" git commit -qm "e2e: setup lives in the workspace"
rm -rf .spoolway "$SPOOLWAY_PROJECT_HOME" .git/spoolway-id
cat > "$WS/project.toml" <<TOML
id = "proj-ws"
clones = [{ root = "$CLONE", dispatcher = "proj" }]
TOML
export SPOOLWAY_PROJECT_HOME="$WS/dispatchers/proj"
publish plan/live

# ------------------------------------------------------------ doctor
says "doctor names home mode" "home mode" "$SPOOLWAY" doctor -v
says "doctor reads the setup from the workspace's config/" "$WS/config" "$SPOOLWAY" doctor -v
works "pipeline check reads the workspace's pipelines" "$SPOOLWAY" pipeline check

# ------------------------------------------------------- one real pass
BODY="$LIVE/body.md"
task_body "$BODY"
task_doc "$LIVE/homed.md" homed "$BODY" "group: live"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/homed.md"
has "it is queued in the clone's dispatcher folder" "id: homed" "$WS/dispatchers/proj/queue/homed.md"

if drive homed gone; then ok "a home-mode task runs queued -> ... -> done and is archived"
else bad "a home-mode task runs queued -> ... -> done and is archived (stuck at \`$(stage_of homed)\`)"; fi

has "the archive is in the clone's dispatcher folder" "→ \`handover\`" \
  "$WS/dispatchers/proj/archive/homed.md"
has "its worktree was cut under the dispatcher folder" \
  "worktree_path: $WS/dispatchers/proj/worktrees/" "$WS/dispatchers/proj/archive/homed.md"
if handed_over homed; then ok "the lane's own spoolway stack handed the change over from its worktree"
else bad "the lane's own spoolway stack handed the change over from its worktree"; ls "$FORGE/prs" | sed 's/^/        /'; fi

# ---------------------------------------------- nothing written back
# Home mode's promise to the clone: nothing in `.git`, nothing in the tree.
if [ -e .git/spoolway-id ]; then bad "no id is stamped into .git"
else ok "no id is stamped into .git"; fi
if [ -e .spoolway ]; then bad "no .spoolway/ reappears in the checkout"
else ok "no .spoolway/ reappears in the checkout"; fi
if [ -z "$(git status --porcelain)" ]; then ok "the checkout is left clean"
else bad "the checkout is left clean"; git status --porcelain | sed 's/^/        /'; fi

finish
