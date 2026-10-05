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
# `init --setup home` writes a workspace itself now, and the last section runs
# it for real: in a fresh git repository, and again from a second clone that
# joins the same workspace, checking that neither checkout nor its `.git` is
# touched. Then a third clone takes over the queue of the second once its
# folder is gone, and the first moves into a new workspace and back, the
# emptied workspace removed. Which questions it asks and what each flag
# answers is unit-tested in src/commands/init.rs. The lane pass above it still builds its workspace
# by hand, by moving an ordinary setup out of the checkout, because the
# harness's fixture helpers rewrite a checkout's own `.spoolway/` in place.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"
command -v jq >/dev/null || { echo "home-mode.sh needs jq" >&2; exit 2; }

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

# ------------------------------------------------------ config path
says "config path names home mode" '"mode": "home"' "$SPOOLWAY" config path --json
# The workspace's name is also in `setup` and `workspace`, so a plain grep
# for it would pass with `workspaces` empty; read the entry itself.
CONFIG_PATH="$LIVE/config-path.json"
"$SPOOLWAY" config path --json >"$CONFIG_PATH" 2>&1
works "config path lists the workspace this checkout joined, its config/ and its one clone" \
  jq -e --arg name "$(basename "$WS")" --arg config "$WS/config" \
  'any(.workspaces[]; .name == $name and .config == $config and .clones == 1 and (has("error") | not))' \
  "$CONFIG_PATH"

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

# --------------------------------------------------- sync keeps the promise
# `init` is not the only command that promises a home-mode checkout nothing
# is written into it or its `.git` — `sync` does too, and until it carried
# its own `home_mode` check it broke that promise on exactly the two files a
# project migrating into home mode is most likely to still be carrying: a
# tracked `.gitignore` with spoolway's old marked block, and an installed
# skill folder from before the checkout moved its setup into the workspace.
cat >> .gitignore <<GITIGNORE

# >>> spoolway >>>
/.spoolway/
# <<< spoolway <<<
GITIGNORE
mkdir -p .claude/skills/spoolway-config
echo "stale, from before this checkout moved into the workspace" \
  > .claude/skills/spoolway-config/SKILL.md
must "the stale .gitignore block and skill folder are committed" git add .gitignore .claude
must "the stale .gitignore block and skill folder are committed" \
  git commit -qm "e2e: a stale spoolway .gitignore block and skill folder"
before_sync=$(git rev-parse HEAD)

works "a dry-run sync runs cleanly in home mode" "$SPOOLWAY" sync --dry-run
works "a real sync runs cleanly in home mode" "$SPOOLWAY" sync
if [ "$(git rev-parse HEAD)" = "$before_sync" ] && [ -z "$(git status --porcelain --ignored)" ]; then
  ok "sync leaves the checkout unchanged"
else
  bad "sync leaves the checkout unchanged"
  git status --porcelain --ignored | sed 's/^/        /'
fi
has "the .gitignore still carries spoolway's old block" "# >>> spoolway >>>" .gitignore
has "the stale skill file is unchanged" "stale, from before this checkout moved into the workspace" \
  .claude/skills/spoolway-config/SKILL.md

# ------------------------------------------- init writes the workspace
# A git repository spoolway has never seen, set up in home mode by `init`
# itself with every question answered by a flag and no terminal attached.
# The promise is the same as above, made by the writer this time: the
# workspace and the user-level skills are all it leaves behind.
FRESH="$LIVE/api"
mkdir -p "$FRESH"
cd "$FRESH" || exit 2
must "the fresh repo" git init -q -b main .
must "the fresh repo" git -c user.email=spoolway@example.invalid -c user.name="spoolway tests" \
  commit -q --allow-empty -m seed
ls -A .git > "$LIVE/api.git-before"
before_ws=$(ls "$HOME/.spoolway")

must "a home-mode init runs without a terminal" "$SPOOLWAY" init --setup home --workspace new \
  --provider claude --examples --tracker none --yes </dev/null

WS2=
for dir in "$HOME"/.spoolway/api-*/; do
  name=$(basename "$dir")
  grep -qxF "$name" <<<"$before_ws" || WS2="$HOME/.spoolway/$name"
done
if [ -n "$WS2" ] && [ -d "$WS2/config" ] && [ -d "$WS2/dispatchers/api" ]; then
  ok "init made a workspace ~/.spoolway/api-<id>/ with config/ and dispatchers/api/"
else
  bad "init made a workspace ~/.spoolway/api-<id>/ with config/ and dispatchers/api/"
  ls -A "$HOME/.spoolway" | sed 's/^/        /'
fi
has "its project.toml lists this clone" "root = \"$(pwd -P)\"" "$WS2/project.toml"
if [ -f "$WS2/config/config.toml" ] && [ -f "$WS2/config/pipelines/default.yml" ]; then
  ok "the setup and the example pipelines were written into config/"
else bad "the setup and the example pipelines were written into config/"; ls -R "$WS2/config" | sed 's/^/        /'; fi
if [ -f "$HOME/.claude/skills/spoolway-plan/SKILL.md" ]; then ok "the skills went into the user's ~/.claude/skills/"
else bad "the skills went into the user's ~/.claude/skills/"; fi
if [ -z "$(git status --porcelain --ignored)" ]; then ok "git status shows nothing from a home-mode init"
else bad "git status shows nothing from a home-mode init"; git status --porcelain --ignored | sed 's/^/        /'; fi
if ls -A .git | diff -q "$LIVE/api.git-before" - >/dev/null && [ -z "$(find .git -iname '*spoolway*')" ]; then
  ok ".git holds nothing from a home-mode init"
else
  bad ".git holds nothing from a home-mode init"
  ls -A .git | diff "$LIVE/api.git-before" - | sed 's/^/        /'
  find .git -iname '*spoolway*' | sed 's/^/        /'
fi

# ------------------------------------------------- a second clone joins
# A clone made by `git clone`, answering the workspace question by name. It
# gets a dispatcher folder of its own beside the first clone's and reads the
# one shared config/, which joining must leave exactly as it was.
SECOND="$LIVE/api-review"
must "the second clone" git clone -q "$FRESH" "$SECOND"
cd "$SECOND" || exit 2
ls -A .git > "$LIVE/review.git-before"
config_before=$(cd "$WS2/config" && find . -type f -exec cksum {} + | sort)

says "joining says which workspace it joined" "Joined workspace $(basename "$WS2")." "$SPOOLWAY" init \
  --setup home --workspace "$(basename "$WS2")" --provider claude --yes </dev/null
if [ "$(cd "$WS2/config" && find . -type f -exec cksum {} + | sort)" = "$config_before" ]; then
  ok "joining leaves the shared config/ unchanged"
else bad "joining leaves the shared config/ unchanged"; fi
has "project.toml now lists the second clone" "root = \"$(pwd -P)\"" "$WS2/project.toml"
has "project.toml still lists the first clone" "root = \"$(cd "$FRESH" && pwd -P)\"" "$WS2/project.toml"
if [ -d "$WS2/dispatchers/api-review" ]; then ok "the second clone has its own dispatchers/api-review/"
else bad "the second clone has its own dispatchers/api-review/"; ls -A "$WS2/dispatchers" | sed 's/^/        /'; fi
says "the second clone finds the shared config/" "$WS2/config" "$SPOOLWAY" doctor -v
if [ -z "$(git status --porcelain --ignored)" ]; then ok "git status shows nothing in the second clone"
else bad "git status shows nothing in the second clone"; git status --porcelain --ignored | sed 's/^/        /'; fi
if ls -A .git | diff -q "$LIVE/review.git-before" - >/dev/null && [ -z "$(find .git -iname '*spoolway*')" ]; then
  ok ".git holds nothing in the second clone"
else bad ".git holds nothing in the second clone"; fi

# --------------------------------------------- a gone clone is taken over
# The second clone has a task queued, then its folder is deleted. A fresh clone
# of the same repository joining the workspace is the one gone entry with
# this root commit, so it takes over that entry's queue without asking, says
# only that it joined, and leaves the shared config/ exactly as it was.
NAME2=$(basename "$WS2")
# Written straight into the dispatcher folder's queue: what is being checked
# is whose queue it is after the takeover, not how it got there.
mkdir -p "$WS2/dispatchers/api-review/queue"
task_doc "$WS2/dispatchers/api-review/queue/d1.md" d1 "$BODY" "stage: queued"
cd "$LIVE" || exit 2
rm -rf "$SECOND"
THIRD="$LIVE/moved/api-review"
must "a fresh clone of the same repository" git clone -q "$FRESH" "$THIRD"
cd "$THIRD" || exit 2
config_before=$(cd "$WS2/config" && find . -type f -exec cksum {} + | sort)
out=$("$SPOOLWAY" init --workspace "$NAME2" --provider claude --yes </dev/null 2>&1)
if grep -qF "Joined workspace $NAME2, taking over the entry for" <<<"$out"; then ok "the takeover says it took over an entry"
else bad "the takeover says it took over an entry"; sed 's/^/        /' <<<"$out"; fi
has "project.toml now names the fresh clone" "root = \"$(pwd -P)\"" "$WS2/project.toml"
if [ "$(ls "$WS2/dispatchers" | wc -l)" -eq 2 ]; then ok "no third dispatcher folder is made"
else bad "no third dispatcher folder is made"; ls -A "$WS2/dispatchers" | sed 's/^/        /'; fi
says "the gone clone's task is this checkout's now" "d1" "$SPOOLWAY" queue list
if [ "$(cd "$WS2/config" && find . -type f -exec cksum {} + | sort)" = "$config_before" ]; then
  ok "taking over leaves the shared config/ unchanged"
else bad "taking over leaves the shared config/ unchanged"; fi

# ----------------------------------------------------- moving a checkout
# The first clone moves into a new workspace, then back. The way back is
# refused while one of its tasks holds a worktree, and once it is clear, the
# move names the workspace it emptied and keeps its folder.
cd "$FRESH" || exit 2
before_ws=$(ls "$HOME/.spoolway")
says "a move into a new workspace says where it went" "Moved to workspace api-" \
  "$SPOOLWAY" init --workspace new --provider claude --examples --tracker none --yes </dev/null
WS3=
for dir in "$HOME"/.spoolway/api-*/; do
  name=$(basename "$dir")
  grep -qxF "$name" <<<"$before_ws" || WS3="$HOME/.spoolway/$name"
done
if [ -n "$WS3" ] && [ -f "$WS3/config/config.toml" ] && [ -d "$WS3/dispatchers/api" ]; then
  ok "the new workspace has a setup and the moved dispatcher folder"
else bad "the new workspace has a setup and the moved dispatcher folder"; ls -A "$HOME/.spoolway" | sed 's/^/        /'; fi
lacks "the workspace it left no longer lists it" "root = \"$(pwd -P)\"" "$WS2/project.toml"
if [ -d "$WS2" ]; then ok "a workspace still holding a checkout is kept"
else bad "a workspace still holding a checkout is kept"; fi

mkdir -p "$WS3/dispatchers/api/queue"
task_doc "$WS3/dispatchers/api/queue/c1.md" c1 "$BODY" "stage: implement" \
  "worktree_path: $WS3/dispatchers/api/worktrees/task-c1"
ws3_before=$(cat "$WS3/project.toml")
refuses "a move waits while a task holds a worktree" "c1 holds a worktree" \
  "$SPOOLWAY" init --workspace "$NAME2" --provider claude --yes </dev/null
if [ "$(cat "$WS3/project.toml")" = "$ws3_before" ]; then ok "the refused move wrote nothing"
else bad "the refused move wrote nothing"; fi
rm -f "$WS3/dispatchers/api/queue/c1.md"

out=$("$SPOOLWAY" init --workspace "$NAME2" --provider claude --yes </dev/null 2>&1)
if grep -qxF "Moved to workspace $NAME2." <<<"$out" \
  && grep -qxF "Workspace $(basename "$WS3") lists no checkout now. Its folder is kept:" <<<"$out"; then
  ok "the move back says where it went and which workspace it left empty"
else bad "the move back says where it went and which workspace it left empty"; sed 's/^/        /' <<<"$out"; fi
if [ -d "$WS3/config" ]; then ok "the emptied workspace is kept"; else bad "the emptied workspace is kept"; fi
has "the first clone is listed back in its workspace" "root = \"$(pwd -P)\"" "$WS2/project.toml"

finish
