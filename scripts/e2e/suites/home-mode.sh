#!/usr/bin/env bash
# A project with no `.spoolway/` anywhere in its checkout, run for real.
#
# Home mode keeps a checkout's setup in a workspace under `~/.spoolway/` —
# `config/` for the setup, `dispatchers/<clone>/` for the queue, archive and
# worktrees — and finds it by the checkout's own path in the workspace's
# `project.toml`, with no id stamped into `.git`. Which files that resolves to
# is unit-tested in src/repo.rs. What a unit test cannot reach is the rest of
# the run: a detached lane in a linked worktree, and the `spoolway report` it
# runs from there, finding the one workspace from a checkout that is neither
# the clone nor carries a `.spoolway/` of its own.
#
# `init --setup home` writes a workspace itself now, and the last section runs
# it for real: in a fresh git repository, and again from a second clone that
# joins the same workspace, checking that neither checkout nor its `.git` is
# touched. A plan page in the workspace is listed by both clones until the
# first has a task naming it. Then a third clone takes over the queue of the
# second once its folder is gone, and the first moves into a new workspace and
# back, the emptied workspace removed. Which questions it asks and what each flag
# answers is unit-tested in src/commands/init.rs. The lane pass above it still builds its workspace
# by hand, by moving an ordinary setup out of the checkout, because the
# harness's fixture helpers rewrite a checkout's own `.spoolway/` in place.
#
# The last sections are about the clone's id rather than home mode as such, and
# run in repo mode: a superproject moved on disk, an `init` cancelled at its
# first question, an `init` that picks workspace only and writes nothing but
# the binding, and several first commands started at once in a fresh clone.
# After them come two about where the project's root sits: a `.spoolway/` below
# the top of the repo, and a linked worktree whose branch has none.
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

# The plans folder is the workspace's own, whichever clone asks, so the
# planning skills can read it from here instead of building a path.
says "config path names the workspace's plans folder" "plans:     $WS/plans" \
  "$SPOOLWAY" config path

# ------------------------------------------------------- one real pass
BODY="$LIVE/body.md"
task_body "$BODY"
task_doc "$LIVE/homed.md" homed "$BODY" "group: live"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/homed.md"
has "it is queued in the clone's dispatcher folder" "id: homed" "$WS/dispatchers/proj/queue/homed.md"

if drive homed gone; then ok "a home-mode task runs queued -> ... -> done and is archived"
else bad "a home-mode task runs queued -> ... -> done and is archived (stuck at \`$(stage_of homed)\`)"; fi

has "the archive is in the clone's dispatcher folder" "→ \`create-pr\`" \
  "$WS/dispatchers/proj/archive/homed.md"
has "its worktree was cut under the dispatcher folder" \
  "worktree_path: $WS/dispatchers/proj/worktrees/" "$WS/dispatchers/proj/archive/homed.md"
if handed_over homed; then ok "the lane's own create-pr handed the change over from its worktree"
else bad "the lane's own create-pr handed the change over from its worktree"; ls "$FORGE/prs" | sed 's/^/        /'; fi

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

# The checkout staying unchanged does not show that sync did its work: one that
# skipped the workspace entirely would pass that too. So the workspace's own
# config.toml is made stale first, cut down to a file written before most
# settings existed, and checked again once sync has run.
printf '[dispatch]\nlane_quiet = "45m"\n' > "$WS/config/config.toml"
lacks "the workspace's config.toml is stale before the sync" "auto_commit" "$WS/config/config.toml"

works "a dry-run sync runs cleanly in home mode" "$SPOOLWAY" sync --dry-run
works "a real sync runs cleanly in home mode" "$SPOOLWAY" sync
if [ "$(git rev-parse HEAD)" = "$before_sync" ] && [ -z "$(git status --porcelain --ignored)" ]; then
  ok "sync leaves the checkout unchanged"
else
  bad "sync leaves the checkout unchanged"
  git status --porcelain --ignored | sed 's/^/        /'
fi
has "sync brought the workspace's config.toml current" "auto_commit" "$WS/config/config.toml"
has "sync kept the value the workspace's config.toml already had" 'lane_quiet = "45m"' "$WS/config/config.toml"
has "the .gitignore still carries spoolway's old block" "# >>> spoolway >>>" .gitignore
has "the stale skill file is unchanged" "stale, from before this checkout moved into the workspace" \
  .claude/skills/spoolway-config/SKILL.md

# A plan page written before plans moved to the workspace sits in the clone's
# own dispatcher folder, and a pending task names it by that absolute path.
# `sync` moves the page into the workspace's plans/ and points the task there.
OLD_PAGE="$WS/dispatchers/proj/plans/2026-10-12-moved.html"
NEW_PAGE="$WS/plans/2026-10-12-moved.html"
mkdir -p "$WS/dispatchers/proj/plans" "$WS/dispatchers/proj/pending"
echo "<html>a page the clone already had</html>" > "$OLD_PAGE"
printf -- '---\nid: moved\ngroup: moved\ntitle: a task naming the old page\nplan: %s\n---\n\nbody\n' \
  "$OLD_PAGE" > "$WS/dispatchers/proj/pending/moved.md"
says "a dry-run sync would move the page" "would move" "$SPOOLWAY" sync --dry-run
says "a dry-run sync keeps a space before the rewritten task" "would rewrite ~/" \
  "$SPOOLWAY" sync --dry-run
if [ -e "$OLD_PAGE" ] && [ ! -e "$NEW_PAGE" ]; then ok "a dry-run sync moves nothing"
else bad "a dry-run sync moves nothing"; fi
says "sync reports the move and the rewrite" "rewrote" "$SPOOLWAY" sync
if [ -e "$NEW_PAGE" ] && [ ! -e "$OLD_PAGE" ]; then ok "sync moved the page into the workspace's plans/"
else bad "sync moved the page into the workspace's plans/"; fi
has "the pending task now names the page in the workspace" "plan: $NEW_PAGE" \
  "$WS/dispatchers/proj/pending/moved.md"
rm -f "$WS/dispatchers/proj/pending/moved.md"

# A second page whose name the workspace already has is refused, with the
# task that names it and the remedy, and the workspace's page is untouched.
CLASH_OLD="$WS/dispatchers/proj/plans/2026-10-12-clash.html"
echo "<html>the clone's copy</html>" > "$CLASH_OLD"
echo "<html>the workspace's copy</html>" > "$WS/plans/2026-10-12-clash.html"
printf -- '---\nid: clash\ngroup: clash\ntitle: a task naming the clashing page\nplan: %s\n---\n\nbody\n' \
  "$CLASH_OLD" > "$WS/dispatchers/proj/pending/clash.md"
says "sync refuses a page whose name the workspace has" "refused  ~/" "$SPOOLWAY" sync
says "the refusal names the task and the remedy" \
  "rename the page and that task's plan: line to a free name, then run spoolway sync again" \
  "$SPOOLWAY" sync
has "the workspace's page is kept" "the workspace's copy" "$WS/plans/2026-10-12-clash.html"
has "the clone's page stays where it was" "the clone's copy" "$CLASH_OLD"
rm -f "$WS/dispatchers/proj/pending/clash.md" "$CLASH_OLD" "$WS/plans/2026-10-12-clash.html"

# A sync killed between rewriting a task and moving its page leaves the task
# naming the workspace path while the page is still in the clone's folder.
# The next sync must finish the move and leave the task alone.
HALF_OLD="$WS/dispatchers/proj/plans/2026-10-12-half.html"
HALF_NEW="$WS/plans/2026-10-12-half.html"
echo "<html>a page whose move was cut short</html>" > "$HALF_OLD"
printf -- '---\nid: half\ngroup: half\ntitle: a task already relinked\nplan: %s\n---\n\nbody\n' \
  "$HALF_NEW" > "$WS/dispatchers/proj/pending/half.md"
"$SPOOLWAY" sync >/dev/null 2>&1
if [ -e "$HALF_NEW" ] && [ ! -e "$HALF_OLD" ]; then ok "a sync finishes a move whose task was already relinked"
else bad "a sync finishes a move whose task was already relinked"; fi
has "the relinked task still names the workspace page" "plan: $HALF_NEW" \
  "$WS/dispatchers/proj/pending/half.md"
rm -f "$WS/dispatchers/proj/pending/half.md" "$HALF_NEW"

# `--replace` takes a path spelled under `.spoolway/`, which in home mode is
# the workspace's `config/` and not a folder of the checkout. The checkout has
# no such file, so before this resolved through the setup folder the command
# refused with "not a file spoolway ships".
REPLACED="$WS/config/prompts/implementer/PROMPT.md"
mkdir -p "$(dirname "$REPLACED")"
echo "my own implementer prompt" > "$REPLACED"
before_replace=$(git rev-parse HEAD)
if "$SPOOLWAY" sync --replace .spoolway/prompts/implementer/PROMPT.md >"$LIVE/replace.out" 2>&1; then
  ok "a home-mode replace of a path under .spoolway/ runs cleanly"
else
  bad "a home-mode replace of a path under .spoolway/ runs cleanly"
  sed 's/^/        /' "$LIVE/replace.out" | head -30
fi
has "a home-mode replace names the workspace file it wrote, in its ~ form" \
  "wrote   ~/${REPLACED#"$HOME"/}" "$LIVE/replace.out"
has "a home-mode replace puts the note on its own line" \
  "          (whole file, discarding your changes)" "$LIVE/replace.out"
lacks "a home-mode replace does not point at git diff" "git diff" "$LIVE/replace.out"
lacks "the workspace prompt no longer holds the old text" "my own implementer prompt" "$REPLACED"
has "the old text is saved beside it" "my own implementer prompt" "$REPLACED.bak"
if [ "$(git rev-parse HEAD)" = "$before_replace" ] && [ ! -e .spoolway ] && [ -z "$(git status --porcelain --ignored)" ]; then
  ok "a home-mode replace leaves the checkout clean"
else
  bad "a home-mode replace leaves the checkout clean"
  ls -d .spoolway 2>/dev/null | sed 's/^/        /'
  git status --porcelain --ignored | sed 's/^/        /'
fi

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

# ------------------------------------------- a plan is the workspace's
# A plan page in the workspace's plans/ is listed by both clones until a task
# in either one names it. `group list` lists plans by the rule the queue
# screen lists its plan rows by, and which folders that rule reads is
# unit-tested in src/commands/pending.rs. What a unit test cannot reach is two
# checkouts that `init` registered finding each other through project.toml.
# The first listing runs before the page exists, so the `plans-since` stamp it
# writes cannot hide the page.
(cd "$FRESH" && must "a first listing of the plans" "$SPOOLWAY" group list)
PAGE="$WS2/plans/2026-10-20-search.html"
echo '<html></html>' >"$PAGE"
(cd "$FRESH" && says "the first clone lists the plan no task names" "0 open — plan $PAGE" \
  "$SPOOLWAY" group list)
says "the second clone lists the same plan" "0 open — plan $PAGE" "$SPOOLWAY" group list
task_doc "$WS2/dispatchers/api/pending/search-index.md" search-index "$BODY" \
  "group: search" "plan: $PAGE"
silent_about "the second clone drops the plan once the first clone has a task naming it" \
  "plan $PAGE" "$SPOOLWAY" group list
rm -f "$WS2/dispatchers/api/pending/search-index.md" "$PAGE"

# ------------------------------------- a project job fires once per workspace
# Both clones run a real dispatcher over the one shared `config/jobs.toml`. A
# project job there has one fired record, in the workspace folder, so the two
# dispatchers fire its window once between them rather than once each. How the
# record is read, locked and seeded is unit-tested in src/jobs.rs. What a unit
# test cannot reach is two live processes taking the same window at once.
cat > "$WS2/config/jobs.toml" <<TOML
[jobs.shared-audit]
schedule = "* * * * *"
pipeline = "default"
routine = "shared"
base = "main"
TOML
mkdir -p "$WS2/config/routines/shared"
task_doc "$WS2/config/routines/shared/shared-audit.md" shared-audit "$BODY" "group: shared"
# Dispatch refuses to start without a git identity, and this suite's HOME is
# its own, so the identity is set there and not in either clone.
must "a git identity for the dispatchers" git config --global user.name "spoolway tests"
must "a git identity for the dispatchers" git config --global user.email spoolway@example.invalid
# Each in its own session, so stopping it is one signal to one process group.
for clone in "$FRESH" "$SECOND"; do
  name=$(basename "$clone")
  (cd "$clone" && env -u SPOOLWAY_PROJECT_HOME SPOOLWAY_SKIP_VERSION_CHECK=1 \
    E2E_BIN="$SPOOLWAY" E2E_PIDFILE="$LIVE/shared-job-$name.pid" \
    setsid bash -c 'echo $$ >"$E2E_PIDFILE"; exec "$E2E_BIN" dispatch' \
    >>"$LIVE/shared-job-$name.out" 2>&1 </dev/null &)
done
poll_until 10 test -s "$LIVE/shared-job-api.pid" -a -s "$LIVE/shared-job-api-review.pid" \
  || bad "the two dispatchers never started"
shared_copies() {
  find "$WS2/dispatchers" \( -path '*/queue/*' -o -path '*/archive/*' \) \
    -name 'shared-audit-*.md' 2>/dev/null | wc -l
}
shared_fired_minute() {
  jq -r '."shared-audit".fired_minute // empty' "$WS2/jobs.state.json" 2>/dev/null
}
# The job runs every minute, so the first window is at most a minute off. The
# count is taken inside the poll: written as `test "$(shared_copies)" -ge 1`,
# it was counted once, when the poll began, and a dispatcher whose first pass
# had not fired yet failed the whole wait however soon the copy appeared.
shared_fired() { [ "$(shared_copies)" -ge 1 ]; }
poll_until 90 shared_fired \
  && ok "the shared project job fired" || bad "the shared project job never fired"
# Let both dispatchers pass over the same minute again before counting: each
# prints "next pass in" when a pass ends idle, so two of those from each is
# proof that a later pass found the window already fired.
passes_in() { grep -ac "next pass in" "$1" 2>/dev/null; }
two_passes_each() {
  [ "$(passes_in "$LIVE/shared-job-api.out")" -ge 2 ] \
    && [ "$(passes_in "$LIVE/shared-job-api-review.out")" -ge 2 ]
}
poll_until 60 two_passes_each || bad "the two dispatchers did not both pass again"
MINUTE=$(shared_fired_minute)
if [ -n "$MINUTE" ]; then ok "the fired minute is in the workspace's own record"
else bad "the fired minute is in the workspace's own record"; ls -A "$WS2" | sed 's/^/        /'; fi
if [ "$(shared_fired_minute)" = "$MINUTE" ]; then
  works "one copy of the task across both clones for the minute" test "$(shared_copies)" -eq 1
fi
lacks "no clone's own record holds the project job" "shared-audit" "$WS2/dispatchers/api/jobs.state.json"
lacks "and neither does the other clone's" "shared-audit" "$WS2/dispatchers/api-review/jobs.state.json"
for f in "$LIVE"/shared-job-*.pid; do
  pid=$(cat "$f")
  kill -TERM -- "-$pid" 2>/dev/null
  poll_while 5 kill -0 -- "-$pid"
  kill -KILL -- "-$pid" 2>/dev/null
done
rm -f "$WS2"/dispatchers/*/queue/shared-audit-*.md "$WS2"/dispatchers/*/archive/shared-audit-*.md
rm -f "$WS2/config/jobs.toml"

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

# ------------------------------------------- a moved superproject
# A project that is a submodule keeps its id in the superproject's
# `.git/modules/`, and records where its checkout is beside it. Moving the
# superproject leaves that record naming a folder that is gone. The clone is
# still the same one, so the command run from its new place must find the home
# it had, queue and all, and rewrite the record, rather than look in a folder
# named after the checkout and advise deleting a stamp that was fine.
GIT_ID=(-c user.email=spoolway@example.invalid -c user.name="spoolway tests")
SUB_ORIGIN="$LIVE/sub-origin"
mkdir -p "$SUB_ORIGIN"
must "the submodule's origin" git -C "$SUB_ORIGIN" init -q -b main
must "the submodule's origin" git -C "$SUB_ORIGIN" "${GIT_ID[@]}" commit -q --allow-empty -m seed
SUPER="$LIVE/super"
mkdir -p "$SUPER"
must "the superproject" git -C "$SUPER" init -q -b main
must "the superproject holds the project as a submodule" \
  git -C "$SUPER" -c protocol.file.allow=always submodule add -q "$SUB_ORIGIN" sub
must "the superproject holds the project as a submodule" git -C "$SUPER" "${GIT_ID[@]}" commit -qm sub
cd "$SUPER/sub" || exit 2
must "init sets the submodule up" "$SPOOLWAY" init --yes --provider claude --tracker none </dev/null
SUB_HOME=$(ls -d "$HOME"/.spoolway/sub-*/ | head -1)
SUB_HOME=${SUB_HOME%/}
mkdir -p "$SUB_HOME/queue"
task_doc "$SUB_HOME/queue/m1.md" m1 "$BODY" "stage: queued"

cd "$LIVE" || exit 2
must "the superproject moves" mv "$SUPER" "$LIVE/super-moved"
cd "$LIVE/super-moved/sub" || exit 2
MOVED=$(pwd -P)
out=$("$SPOOLWAY" queue list 2>&1)
if grep -qF "$SUB_HOME/project.toml now records $MOVED" <<<"$out"; then
  ok "a moved superproject records the move"
else bad "a moved superproject records the move"; sed 's/^/        /' <<<"$out"; fi
if grep -qF "m1" <<<"$out"; then ok "the moved project finds the queue it had"
else bad "the moved project finds the queue it had"; sed 's/^/        /' <<<"$out"; fi
if grep -qF "no home holds the id" <<<"$out"; then bad "the moved project is not refused for want of a home"
else ok "the moved project is not refused for want of a home"; fi
has "the record names the checkout where it is now" "root = \"$MOVED\"" "$SUB_HOME/project.toml"
if [ "$(ls -d "$HOME"/.spoolway/sub-*/ | wc -l)" -eq 1 ]; then ok "no second home is made for the moved checkout"
else bad "no second home is made for the moved checkout"; ls -A "$HOME/.spoolway" | sed 's/^/        /'; fi

# --------------------------------------------------- a cancelled init
# `init` asks every question before it writes anything. So a run stopped at
# its first question leaves `.git` and `~/.spoolway` exactly as they were. Run under a pseudo-terminal, since a
# silent run never reaches a question at all.
if script -qec true /dev/null >/dev/null 2>&1; then
  CANCEL="$LIVE/cancelled"
  mkdir -p "$CANCEL"
  must "the repo init is cancelled in" git -C "$CANCEL" init -q -b main
  must "the repo init is cancelled in" git -C "$CANCEL" "${GIT_ID[@]}" commit -q --allow-empty -m seed
  cd "$CANCEL" || exit 2
  ls -A .git > "$LIVE/cancelled.git-before"
  ls -A "$HOME/.spoolway" > "$LIVE/cancelled.home-before"
  # Stopped once its first question is on screen, however long that takes,
  # with a cap so a hung run cannot hold the suite. Killing the wrapper takes
  # the question's session down with it, which is what an interrupt at the
  # prompt would end in.
  script -qec "$SPOOLWAY init" /dev/null </dev/null >"$LIVE/cancelled.out" 2>&1 &
  init_pid=$!
  for _ in $(seq 1 300); do
    grep -qF "Where should this project's setup live" "$LIVE/cancelled.out" 2>/dev/null && break
    sleep 0.1
  done
  kill -INT "$init_pid" 2>/dev/null
  sleep 0.5
  kill -TERM "$init_pid" 2>/dev/null
  wait "$init_pid" 2>/dev/null
  has "the cancelled init got as far as its first question" "Where should this project's setup live" \
    "$LIVE/cancelled.out"
  if ls -A .git | diff -q "$LIVE/cancelled.git-before" - >/dev/null && [ -z "$(find .git -iname '*spoolway*')" ]; then
    ok "a cancelled init leaves .git as it was"
  else bad "a cancelled init leaves .git as it was"; find .git -iname '*spoolway*' | sed 's/^/        /'; fi
  if ls -A "$HOME/.spoolway" | diff -q "$LIVE/cancelled.home-before" - >/dev/null; then
    ok "a cancelled init leaves ~/.spoolway as it was"
  else bad "a cancelled init leaves ~/.spoolway as it was"; ls -A "$HOME/.spoolway" | sed 's/^/        /'; fi
else
  echo "  skip  a cancelled init (no pseudo-terminal wrapper on this machine)"
fi

# ------------------------------------------------- a workspace-only init
# The setup menu's third entry writes the binding a first command makes and
# nothing else: the stamp, the home's record, no `.spoolway/` and no skills.
# Picked with two Down arrows and Enter under a pseudo-terminal, the keys sent
# only once the menu is on screen. Stdin stays open until the run has said it
# is done, since `script` ends the session when its input closes. The terminal
# is given a size first: `ask::choose` cuts each row at the terminal's width,
# `script` starts with none, and the 80 columns read in its place would cut
# the notes short.
if script -qec true /dev/null >/dev/null 2>&1; then
  ONLY="$LIVE/wsonly"
  mkdir -p "$ONLY"
  must "the workspace-only checkout" git -C "$ONLY" init -q -b main
  must "the workspace-only checkout" git -C "$ONLY" "${GIT_ID[@]}" commit -q --allow-empty -m seed
  cd "$ONLY" || exit 2
  {
    for _ in $(seq 1 300); do
      grep -qF "Where should this project's setup live" "$LIVE/wsonly.out" 2>/dev/null && break
      sleep 0.1
    done
    printf '\033[B\033[B\r'
    for _ in $(seq 1 300); do
      grep -qF "initialized successfully" "$LIVE/wsonly.out" 2>/dev/null && break
      sleep 0.1
    done
  } | script -qec "stty rows 40 cols 160; $SPOOLWAY init" /dev/null >"$LIVE/wsonly.out" 2>&1
  has "the menu offers workspace only" "workspace only  only the workspace, use when .spoolway/ is already in the repo" \
    "$LIVE/wsonly.out"
  has "the pick is shown after the question" "live?: workspace only" "$LIVE/wsonly.out"
  ONLY_ID=$(cat .git/spoolway-id 2>/dev/null)
  has "workspace only stamps the checkout" "  stamped  .git/spoolway-id       $ONLY_ID" "$LIVE/wsonly.out"
  has "workspace only names its home" "  home     ~/.spoolway/wsonly-$ONLY_ID/" "$LIVE/wsonly.out"
  has "workspace only says it is done" "Workspace initialized successfully." "$LIVE/wsonly.out"
  if grep -qF "Select your agent" "$LIVE/wsonly.out"; then bad "workspace only asks nothing after the menu"
  else ok "workspace only asks nothing after the menu"; fi
  has "the home records the checkout" "root = \"$(pwd -P)\"" "$HOME/.spoolway/wsonly-$ONLY_ID/project.toml"
  if [ -z "$(git status --porcelain)" ] && [ ! -e .spoolway ] && [ ! -e .claude ]; then
    ok "workspace only leaves the checkout clean, with no setup and no skills"
  else bad "workspace only leaves the checkout clean, with no setup and no skills"; git status --porcelain | sed 's/^/        /'; fi
  # The setup arriving later, as a pull would bring it, finds the same home.
  mkdir .spoolway
  out=$("$SPOOLWAY" config path 2>&1)
  if grep -qE '^mode: +repo' <<<"$out" && grep -qF "/.spoolway/wsonly-$ONLY_ID/plans" <<<"$out"; then
    ok "a .spoolway/ arriving later is repo mode in the same home"
  else bad "a .spoolway/ arriving later is repo mode in the same home"; sed 's/^/        /' <<<"$out"; fi
else
  echo "  skip  a workspace-only init (no pseudo-terminal wrapper on this machine)"
fi

# ----------------------------------------- parallel first commands
# A fresh clone of a project that tracks its own `.spoolway/` has no id and no
# home. Several commands started at once in it must settle on one home between
# them: the stamp and the home's record are written together, under one lock,
# and a command that finds the stamp without the record waits for it rather
# than failing with "no home holds the id".
PAR_ORIGIN="$LIVE/par-origin"
mkdir -p "$PAR_ORIGIN"
must "the project to clone" git -C "$PAR_ORIGIN" init -q -b main
cd "$PAR_ORIGIN" || exit 2
must "the project to clone is set up" "$SPOOLWAY" init --yes --provider claude --tracker none </dev/null
must "the project to clone is committed" git add -A
must "the project to clone is committed" git "${GIT_ID[@]}" commit -qm "set up"
PAR="$LIVE/par-clone"
for round in 1 2 3; do
  rm -rf "$PAR"
  before_homes=$(ls "$HOME/.spoolway" | grep -c '^par-clone-' || true)
  must "a fresh clone" git clone -q "$PAR_ORIGIN" "$PAR"
  cd "$PAR" || exit 2
  pids=()
  for n in 1 2 3 4 5 6; do
    ("$SPOOLWAY" queue list >"$LIVE/par.$n.out" 2>&1; echo $? >"$LIVE/par.$n.rc") &
    pids+=($!)
  done
  wait "${pids[@]}"
  failed=$(cat "$LIVE"/par.*.rc | grep -vc '^0$' || true)
  if [ "$failed" -eq 0 ]; then ok "round $round: six parallel first commands all succeed"
  else bad "round $round: six parallel first commands all succeed ($failed failed)"; cat "$LIVE"/par.*.out | sed 's/^/        /'; fi
  homes=$(ls "$HOME/.spoolway" | grep -c '^par-clone-' || true)
  if [ "$((homes - before_homes))" -eq 1 ]; then ok "round $round: they agree on one home"
  else bad "round $round: they agree on one home ($((homes - before_homes)) made)"; ls -A "$HOME/.spoolway" | sed 's/^/        /'; fi
  rm -rf "$HOME"/.spoolway/par-clone-* "$LIVE"/par.*.rc "$LIVE"/par.*.out
  cd "$LIVE" || exit 2
done

# ------------------------------------------- the root is the repo top
# One project per clone, and its `.spoolway/` is at the top of the repo. A
# setup tracked further down is refused by name, from inside it and by `init`
# at the top, rather than taken for a second project of the same clone.
NEST="$LIVE/nested"
mkdir -p "$NEST/vendor/src"
must "a repo with a setup below its top" git -C "$NEST" init -q -b main
mkdir -p "$NEST/vendor/.spoolway"
printf '[dispatch]\n' > "$NEST/vendor/.spoolway/config.toml"
must "the nested setup is tracked" git -C "$NEST" add -A
must "the nested setup is tracked" git -C "$NEST" "${GIT_ID[@]}" commit -qm "vendored setup"
cd "$NEST/vendor/src" || exit 2
refuses "a command inside a nested setup names it" "vendor/.spoolway is not at the top of the repo" \
  "$SPOOLWAY" queue list
cd "$NEST" || exit 2
refuses "init at the top refuses a nested setup too" "vendor/.spoolway is not at the top of the repo" \
  "$SPOOLWAY" init --yes --provider claude --tracker none </dev/null
refuses "the refusal says where the setup belongs" "A project's setup lives at $(pwd -P)/.spoolway" \
  "$SPOOLWAY" init --yes --provider claude --tracker none </dev/null
if [ -e "$NEST/.spoolway" ]; then bad "the refused init wrote nothing"; else ok "the refused init wrote nothing"; fi
cd "$LIVE" || exit 2

# A lane's worktree normally carries the branch's own `.spoolway/`. One cut on
# a branch without it reads the main checkout's setup instead: the `checkout:`
# line names the main checkout, and `sync` refuses to start a setup there.
BARE="$LIVE/bare-branch"
mkdir -p "$BARE"
must "a project to cut a worktree from" git -C "$BARE" init -q -b main
cd "$BARE" || exit 2
must "the project is set up" "$SPOOLWAY" init --yes --provider claude --tracker none </dev/null
must "the setup is committed" git add -A
must "the setup is committed" git "${GIT_ID[@]}" commit -qm "set up"
must "a worktree on a branch" git worktree add -q -b no-setup "$LIVE/bare-wt" HEAD
must "the branch drops its setup" git -C "$LIVE/bare-wt" rm -rq .spoolway
must "the branch drops its setup" git -C "$LIVE/bare-wt" "${GIT_ID[@]}" commit -qm "no setup here"
cd "$LIVE/bare-wt" || exit 2
says "the worktree reads the main checkout's setup" "setup:     $(cd "$BARE" && pwd -P)/.spoolway" \
  "$SPOOLWAY" config path
says "the checkout line names the main checkout" "checkout: $(cd "$BARE" && pwd -P) (main)" \
  "$SPOOLWAY" config path
refuses "sync will not write a setup into the worktree" "no \`.spoolway/\` on its branch" \
  "$SPOOLWAY" sync
if [ -e "$LIVE/bare-wt/.spoolway" ]; then bad "the refused sync wrote nothing"; else ok "the refused sync wrote nothing"; fi
cd "$LIVE" || exit 2

finish
