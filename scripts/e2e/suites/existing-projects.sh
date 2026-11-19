#!/usr/bin/env bash
# What a project that already exists reads, prints and refuses, pinned.
#
# Spoolway finds a project's setup in more than one place: the main checkout's
# `.spoolway/`, a linked worktree's own, or a home-mode workspace's `config/`.
# Which one a command reads is easy to change by accident and invisible in an
# exit code, so every case below gives the places it could read different
# answers — a config value, a pipeline name, a prompt's text — and checks which
# answer a command printed. A later change to where the setup is found must
# leave every line here passing; one that moves an answer has changed what an
# existing project does.
#
# Eight shapes, one section each:
#
#   1  a repo-mode main checkout with `.spoolway/`
#   2  a home-mode clone a workspace lists, and a linked worktree of it
#   3  a linked worktree with its own `.spoolway/`
#   4  a linked worktree without one
#   5  lanes under a running dispatcher
#   6  a `--separate-git-dir` clone and its linked worktree
#   7  a `project.toml` with no `source` line
#   8  herdr refusing a dispatcher started in a linked worktree
#
# The last section takes `.spoolway/` off the main checkout's branch and pins
# the refusal each of three shapes then gives: the main checkout, a worktree
# with a setup of its own, and a worktree without one.
#
# Matching is by the lines that name a folder, a value or a refusal, never a
# whole screen, so rewording a command's help or adding a row does not break
# this suite. A refusal's sentence is matched whole, because its wording is
# what a person is told to do.
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

# ---------------------------------------------------------------- helpers

# Run a command from `dir`, as a person standing there would. The harness's
# SPOOLWAY_PROJECT_HOME is only its note of where a project home is; spoolway
# never reads it, so the checkout alone decides which project a command is in.
in_dir() {
  local dir=$1; shift
  (cd "$dir" && "$@")
}

# Outside the main checkout a command names the checkout it read on a line of its
# own, `checkout: <path> (<branch>)`, ahead of its answer. That line is pinned
# where it is the point; everywhere else it is dropped, since a path could
# contain any value a check looks for.
answer() { grep -v '^checkout: '; }

# `config get dispatch.lane_quiet` from `dir`, the value alone.
lane_quiet_in() { in_dir "$1" "$SPOOLWAY" config get dispatch.lane_quiet 2>/dev/null | answer; }

# Every pipeline name `pipeline list` prints from `dir`, one per line: the name
# lines only, not the indented sentence under each.
pipelines_in() { in_dir "$1" "$SPOOLWAY" pipeline list 2>/dev/null | answer | grep -v '^[[:space:]]'; }

# The `prompt show zed` text from `dir`.
zed_in() { in_dir "$1" "$SPOOLWAY" prompt show zed 2>/dev/null | answer; }

# `no_source_line <what> <file>`: no `source = ...` key. Only a key at the start
# of a line counts, since a folder path in the file may contain the word.
no_source_line() {
  if grep -qE '^[[:space:]]*source[[:space:]]*=' "$2"; then
    bad "$1 (a source key is in $2)"; sed 's/^/        /' "$2"
  else ok "$1"; fi
}

# `prompt_used_by <what> <dir> <pipeline>` and `prompt_not_used_by`: whether
# `prompt list` shows a step of that pipeline running a prompt. The list names
# every pipeline it read, so this says which folder it read where the prompt's
# own row, the same from everywhere, cannot.
prompt_used_by() {
  if in_dir "$2" "$SPOOLWAY" prompt list 2>/dev/null | answer | grep -qF -- "$3/implement"; then ok "$1"
  else bad "$1 (no \`$3/implement\` in prompt list from $2)"
    in_dir "$2" "$SPOOLWAY" prompt list 2>&1 | sed 's/^/        /'; fi
}
prompt_not_used_by() {
  if in_dir "$2" "$SPOOLWAY" prompt list 2>/dev/null | answer | grep -qF -- "$3/implement"; then
    bad "$1 (\`$3/implement\` is in prompt list from $2)"
  else ok "$1"; fi
}

# `is <what> <want> <got>`: equal, or the case fails saying what was printed.
is() {
  if [ "$3" = "$2" ]; then ok "$1"
  else bad "$1 (wanted \"$2\", it printed \"$3\")"; fi
}

# `lists <what> <dir> <name>` and `unlisted <what> <dir> <name>`: whether
# `pipeline list` names a pipeline, as a whole line.
lists() {
  if pipelines_in "$2" | grep -qx -- "$3"; then ok "$1"
  else bad "$1 (no pipeline \`$3\` from $2)"; pipelines_in "$2" | sed 's/^/        /'; fi
}
unlisted() {
  if pipelines_in "$2" | grep -qx -- "$3"; then bad "$1 (\`$3\` is listed from $2)"
  else ok "$1"; fi
}

# Which values a setup carries, written into `dir/.spoolway` by `stamp_setup`:
# the config value, a pipeline of its own and the text of the prompt `zed`. Each
# setup in this suite gets a set of its own, so a command's output names the
# folder it read.
stamp_setup() {
  local setup=$1 quiet=$2 pipeline=$3 text=$4
  sed -i "s/^lane_quiet = .*/lane_quiet = \"$quiet\"/" "$setup/config.toml"
  rm -f "$setup"/pipelines/*pipe.yml
  cp "$setup/pipelines/default.yml" "$setup/pipelines/$pipeline.yml"
  mkdir -p "$setup/prompts/zed"
  printf '%s\n' "$text" > "$setup/prompts/zed/PROMPT.md"
}

# ----------------------------------------------- 1: a repo-mode main checkout
new_repo "$LIVE/proj"
configure_project plan/live
MAIN=$(pwd -P)
stamp_setup "$MAIN/.spoolway" 11m mainpipe "MAIN CHECKOUT PROMPT"
must "the main setup is committed" git add -A
must "the main setup is committed" git commit -qm "e2e: the main checkout's setup"

is "the main checkout prints its own config value" "11m" "$(lane_quiet_in "$MAIN")"
lists "it lists its own pipeline" "$MAIN" mainpipe
is "it prints its own prompt text" "MAIN CHECKOUT PROMPT" "$(zed_in "$MAIN")"
prompt_used_by "prompt list shows its own pipeline's steps" "$MAIN" mainpipe
says "config path names its own .spoolway/" "setup:     $MAIN/.spoolway" \
  in_dir "$MAIN" "$SPOOLWAY" config path
says "config path says repo mode" "mode:      repo" in_dir "$MAIN" "$SPOOLWAY" config path
silent_about "it prints no checkout line, being the checkout it reads" "checkout: " \
  in_dir "$MAIN" "$SPOOLWAY" config path
# The column is padded to the longest prompt name, so the row is matched by
# pattern rather than by its spaces.
if in_dir "$MAIN" "$SPOOLWAY" prompt list | grep -qE '^zed[[:space:]]+— nothing runs it$'; then
  ok "prompt list names the prompt and that nothing runs it"
else bad "prompt list names the prompt and that nothing runs it"; fi

# ------------------------------------ 3: a linked worktree with its own setup
OWN="$LIVE/own"
must "a worktree with its own setup" git worktree add -q -b task/own "$OWN"
stamp_setup "$OWN/.spoolway" 22m ownpipe "OWN WORKTREE PROMPT"
must "the worktree's setup is committed" git -C "$OWN" add -A
must "the worktree's setup is committed" git -C "$OWN" commit -qm "e2e: the worktree's own setup"
OWN=$(cd "$OWN" && pwd -P)

is "a worktree with its own setup prints its own config value" "22m" "$(lane_quiet_in "$OWN")"
lists "it lists its own pipeline" "$OWN" ownpipe
unlisted "and not the main checkout's" "$OWN" mainpipe
is "it prints its own prompt text" "OWN WORKTREE PROMPT" "$(zed_in "$OWN")"
prompt_used_by "prompt list shows its own pipeline's steps" "$OWN" ownpipe
prompt_not_used_by "and not the main checkout's" "$OWN" mainpipe
says "config path names the worktree's .spoolway/" "setup:     $OWN/.spoolway" \
  in_dir "$OWN" "$SPOOLWAY" config path
says "and the checkout line names the worktree and its branch" \
  "checkout: $OWN (task/own)" in_dir "$OWN" "$SPOOLWAY" config path
# The project's runtime state is the main checkout's, whichever worktree asks.
HOME_OF_MAIN=$(in_dir "$MAIN" "$SPOOLWAY" config path | sed -n 's/^plans: *//p' | sed 's|/plans$||')
is "the plans folder is the main checkout's project home, not a worktree's" \
  "$HOME_OF_MAIN/plans" "$(in_dir "$OWN" "$SPOOLWAY" config path | sed -n 's/^plans: *//p')"

# -------------------------------- 4: a linked worktree without a setup of its own
PLAIN="$LIVE/plain"
must "a worktree without a setup" git worktree add -q -b task/plain "$PLAIN"
must "its branch drops the setup" git -C "$PLAIN" rm -rq .spoolway
must "its branch drops the setup" git -C "$PLAIN" commit -qm "e2e: no setup on this branch"
PLAIN=$(cd "$PLAIN" && pwd -P)

is "a worktree without a setup prints the main checkout's config value" "11m" \
  "$(lane_quiet_in "$PLAIN")"
lists "it lists the main checkout's pipeline" "$PLAIN" mainpipe
unlisted "and not the other worktree's" "$PLAIN" ownpipe
is "it prints the main checkout's prompt text" "MAIN CHECKOUT PROMPT" "$(zed_in "$PLAIN")"
prompt_used_by "prompt list shows the main checkout's pipeline's steps" "$PLAIN" mainpipe
prompt_not_used_by "and not the other worktree's" "$PLAIN" ownpipe
says "its checkout line names the main checkout, not the worktree" \
  "checkout: $MAIN (plan/live)" in_dir "$PLAIN" "$SPOOLWAY" config path
says "and config path names the main checkout's .spoolway/" "setup:     $MAIN/.spoolway" \
  in_dir "$PLAIN" "$SPOOLWAY" config path

# ------------------------------------------------ 7: a project.toml, no source
# Every project's home carries one. Today it names an id and a root and nothing
# says where the setup is, so a project has one place to read it from.
BINDING="$SPOOLWAY_PROJECT_HOME/project.toml"
has "the project's binding names its id" "id = " "$BINDING"
has "and its root" "root = \"$MAIN\"" "$BINDING"
no_source_line "and has no source line" "$BINDING"
says "so config path lists no workspace" "workspace: -" in_dir "$MAIN" "$SPOOLWAY" config path

# --------------------------------------------- 5: lanes under a running dispatcher
# A lane's worktree is cut from the branch, so it carries the setup that branch
# committed. The main checkout then changes its own copy without committing:
# a command a lane runs prints whichever of the two it reads.
PROBE="$LIVE/lane-probe.txt"
cat > "$LIVE/lane-probe.sh" <<PROBE_SH
#!/usr/bin/env bash
{
  echo "config: \$("$SPOOLWAY" config get dispatch.lane_quiet 2>/dev/null | grep -v '^checkout: ')"
  echo "pipelines:"; "$SPOOLWAY" pipeline list 2>/dev/null | grep -v '^[[:space:]]'
  echo "prompt: \$("$SPOOLWAY" prompt show zed 2>/dev/null | grep -v '^checkout: ')"
  echo "prompts:"; "$SPOOLWAY" prompt list 2>/dev/null | grep -v '^checkout: '
  echo "path:"; "$SPOOLWAY" config path 2>/dev/null
} > "$PROBE"
PROBE_SH
chmod +x "$LIVE/lane-probe.sh"

BODY="$LIVE/body.md"
task_body "$BODY"
sed -i "0,/^    on_pass: review\$/s//    on_pass: probe/" .spoolway/pipelines/default.yml
cat >> .spoolway/pipelines/default.yml <<YML

  - id: probe
    description: Prints what a lane's own commands read, for the suite to check.
    run: $LIVE/lane-probe.sh
    on_pass: review
YML
must "the probe step is committed" git add -A
must "the probe step is committed" git commit -qm "e2e: a step that prints what a lane reads"
# What the main checkout now holds is not what the branch committed, so a lane
# that read it would print the values below instead of the committed ones.
stamp_setup "$MAIN/.spoolway" 33m dirtypipe "MAIN CHECKOUT, UNCOMMITTED"
publish plan/live
task_doc "$LIVE/lane.md" lane "$BODY" "group: live"
must "the task queues" "$SPOOLWAY" queue add --from "$LIVE/lane.md"

if drive lane gone; then ok "a task runs through the probe step and is archived"
else bad "a task runs through the probe step and is archived (stuck at \`$(stage_of lane)\`)"; fi
# `drive` leaves the dispatcher running, and it would start the task queued for
# the herdr section below, then keep running while the last section switches the
# main checkout's branch under it.
dispatcher_stop
LANE_TREE=$(sed -n 's/^worktree_path: *//p' "$SPOOLWAY_PROJECT_HOME/archive/lane.md")
has "the lane ran in a worktree of its own" "/worktrees/" "$SPOOLWAY_PROJECT_HOME/archive/lane.md"
has "a lane prints the setup its worktree carries" "config: 11m" "$PROBE"
# Only the block between `pipelines:` and `prompt:` is the pipeline list; the
# prompt list further down names pipelines too.
LANE_PIPELINES=$(sed -n '/^pipelines:$/,/^prompt:/p' "$PROBE")
if grep -qx mainpipe <<<"$LANE_PIPELINES"; then ok "its pipelines are the worktree's"
else bad "its pipelines are the worktree's"; sed 's/^/        /' "$PROBE"; fi
if grep -qx dirtypipe <<<"$LANE_PIPELINES"; then bad "and not the main checkout's uncommitted one"
else ok "and not the main checkout's uncommitted one"; fi
has "its prompt is the worktree's" "prompt: MAIN CHECKOUT PROMPT" "$PROBE"
has "and prompt list shows the worktree's pipeline's steps" "mainpipe/implement" "$PROBE"
lacks "and not the main checkout's uncommitted one" "dirtypipe/implement" "$PROBE"
has "and config path names the lane's own .spoolway/" "setup:     $LANE_TREE/.spoolway" "$PROBE"

# --------------------------------------------- 8: herdr and a linked worktree
# `check_backend_checkout` refuses a dispatcher started in a linked worktree
# when the backend is herdr. The double stands in for herdr: it answers the
# agent list that checks herdr is reachable, and the refusal comes before any
# workspace or pane is opened.
HSTATE="$LIVE/herdr-stub"
HERDRBIN="$LIVE/herdr-bin"
mkdir -p "$HSTATE" "$HERDRBIN"
install -m 755 "$HERE/../herdr-stub.sh" "$HERDRBIN/herdr"
export HERDR_STUB_STATE="$HSTATE"
task_doc "$LIVE/held.md" held "$BODY" "group: held"
must "a second task queues, so a dispatch has work" "$SPOOLWAY" queue add --from "$LIVE/held.md"
# Any value but `headless` runs herdr, here the double above.
with_herdr() { ( export SPOOLWAY_TEST_BACKEND=1 PATH="$HERDRBIN:$PATH"; "$@" ); }

REFUSAL=$(with_herdr in_dir "$OWN" "$SPOOLWAY" dispatch 2>&1)
status=$?
if [ "$status" -ne 0 ]; then ok "a dispatcher started in a linked worktree is refused"
else bad "a dispatcher started in a linked worktree is refused (exit $status)"; fi
for line in \
  "spoolway: refusing to start: herdr cannot open a workspace on this checkout" \
  "  dispatching from  $OWN" \
  "  main checkout     $MAIN — herdr refuses a linked worktree as a workspace root, so every lane would be filed under $MAIN instead." \
  "  run the dispatcher from $MAIN"; do
  if grep -qxF -- "$line" <<<"$REFUSAL"; then ok "it says: ${line# }"
  else bad "it says: ${line# }"; sed 's/^/        /' <<<"$REFUSAL"; fi
done
says "doctor fails the same check from the worktree" \
  "FAIL  backend and checkout: herdr cannot open a workspace on this checkout" \
  with_herdr in_dir "$OWN" "$SPOOLWAY" doctor
silent_about "and from the main checkout does not" "FAIL  backend and checkout" \
  with_herdr in_dir "$MAIN" "$SPOOLWAY" doctor
silent_about "nor from a worktree with no setup, which resolves to the main checkout" \
  "FAIL  backend and checkout" with_herdr in_dir "$PLAIN" "$SPOOLWAY" doctor
must "the second task leaves the queue" "$SPOOLWAY" queue unqueue held

# ----------------------------- 6: a --separate-git-dir clone and its worktree
# The git directory is nowhere near the checkout, so the main checkout cannot
# be found by looking one folder up from `.git`. The id `init` stamps is what
# names it, and a linked worktree of it must still find the same project.
SEP="$LIVE/sep"
mkdir -p "$LIVE/gitdirs"
must "a clone whose git directory lives elsewhere" \
  git init -q -b main "--separate-git-dir=$LIVE/gitdirs/sep" "$SEP"
cd "$SEP" || exit 2
must "git identity" git config user.email spoolway@example.invalid
must "git identity" git config user.name "spoolway tests"
echo "fn main() {}" > main.rs
must "the seed commit" git add -A
must "the seed commit" git commit -qm "seed"
must "spoolway init in the clone" "$SPOOLWAY" init --yes
SEP=$(pwd -P)
stamp_setup "$SEP/.spoolway" 44m seppipe "SEPARATE GIT DIR PROMPT"
must "the clone's setup is committed" git add -A
must "the clone's setup is committed" git commit -qm "e2e: the clone's setup"
SEPWT="$LIVE/sepwt"
must "a linked worktree of it" git worktree add -q -b task/sep "$SEPWT"
stamp_setup "$SEPWT/.spoolway" 55m sepwtpipe "SEPARATE WORKTREE PROMPT"
must "its own setup is committed" git -C "$SEPWT" add -A
must "its own setup is committed" git -C "$SEPWT" commit -qm "e2e: the worktree's own setup"
SEPWT=$(cd "$SEPWT" && pwd -P)

is "the clone prints its own config value" "44m" "$(lane_quiet_in "$SEP")"
lists "it lists its own pipeline" "$SEP" seppipe
is "it prints its own prompt text" "SEPARATE GIT DIR PROMPT" "$(zed_in "$SEP")"
prompt_used_by "prompt list shows its own pipeline's steps" "$SEP" seppipe
says "config path names the clone's .spoolway/, not the git directory's folder" \
  "setup:     $SEP/.spoolway" in_dir "$SEP" "$SPOOLWAY" config path
is "its linked worktree prints its own config value" "55m" "$(lane_quiet_in "$SEPWT")"
lists "and lists its own pipeline" "$SEPWT" sepwtpipe
unlisted "and not the clone's" "$SEPWT" seppipe
is "and its own prompt text" "SEPARATE WORKTREE PROMPT" "$(zed_in "$SEPWT")"
prompt_used_by "prompt list shows its own pipeline's steps" "$SEPWT" sepwtpipe
prompt_not_used_by "and not the clone's" "$SEPWT" seppipe
says "and config path names the worktree's .spoolway/" "setup:     $SEPWT/.spoolway" \
  in_dir "$SEPWT" "$SPOOLWAY" config path
# The id `init` stamped names the project home; it sits in the git directory,
# not in the checkout.
SEP_PLANS="$HOME/.spoolway/sep-$(cat "$LIVE/gitdirs/sep/spoolway-id")/plans"
is "the clone's plans folder is in its project home" "$SEP_PLANS" \
  "$(in_dir "$SEP" "$SPOOLWAY" config path | sed -n 's/^plans: *//p')"
is "and its worktree shares that project home" "$SEP_PLANS" \
  "$(in_dir "$SEPWT" "$SPOOLWAY" config path | sed -n 's/^plans: *//p')"
cd "$MAIN" || exit 2

# ------------------------------- 2: a home-mode clone a workspace lists
# The setup lives in the workspace's `config/`; the clone's tree has none. A
# linked worktree whose branch carries a `.spoolway/` of its own still reads the
# workspace's, because home mode keeps nothing branch-local to read.
HOMEC="$LIVE/homeclone"
mkdir -p "$HOMEC"
cd "$HOMEC" || exit 2
must "git init" git init -q -b main .
must "git identity" git config user.email spoolway@example.invalid
must "git identity" git config user.name "spoolway tests"
echo "fn main() {}" > main.rs
must "the seed commit" git add -A
must "the seed commit" git commit -qm "seed"
must "spoolway init in the clone" "$SPOOLWAY" init --yes
HOMEC=$(pwd -P)
must "the clone's setup is committed" git add -A
must "the clone's setup is committed" git commit -qm "e2e: the clone's setup"
HOMEWT="$LIVE/homewt"
must "a linked worktree of it" git worktree add -q -b task/home "$HOMEWT"
stamp_setup "$HOMEWT/.spoolway" 17m homewtpipe "HOME WORKTREE OWN PROMPT"
must "the worktree's own setup is committed" git -C "$HOMEWT" add -A
must "the worktree's own setup is committed" git -C "$HOMEWT" commit -qm "e2e: its own setup"
HOMEWT=$(cd "$HOMEWT" && pwd -P)

WS="$HOME/.spoolway/homeclone-ws"
mkdir -p "$WS/config" "$WS/dispatchers"
must "the setup moves into the workspace" cp -a .spoolway/. "$WS/config/"
must "the clone drops its setup" git rm -rq .spoolway
must "the clone drops its setup" git commit -qm "e2e: setup lives in the workspace"
# `init` also made a repo-mode home for the clone. A home-mode clone has none, so
# it goes with the id, leaving the workspace's `clones` list to answer for it.
rm -rf "$HOME/.spoolway/homeclone-$(cat .git/spoolway-id)"
rm -rf .spoolway .git/spoolway-id
stamp_setup "$WS/config" 13m wspipe "WORKSPACE PROMPT"
cat > "$WS/project.toml" <<TOML
id = "homeclone-ws"
clones = [{ root = "$HOMEC", dispatcher = "homeclone" }]
TOML

is "a home-mode clone prints the workspace's config value" "13m" "$(lane_quiet_in "$HOMEC")"
lists "it lists the workspace's pipeline" "$HOMEC" wspipe
is "it prints the workspace's prompt text" "WORKSPACE PROMPT" "$(zed_in "$HOMEC")"
prompt_used_by "prompt list shows the workspace's pipeline's steps" "$HOMEC" wspipe
says "config path says home mode" "mode:      home" in_dir "$HOMEC" "$SPOOLWAY" config path
says "and names the workspace's config/" "setup:     $WS/config" in_dir "$HOMEC" "$SPOOLWAY" config path
no_source_line "the workspace's project.toml has no source line either" "$WS/project.toml"
is "a linked worktree carrying its own setup prints the workspace's config value" "13m" \
  "$(lane_quiet_in "$HOMEWT")"
lists "and lists the workspace's pipeline" "$HOMEWT" wspipe
unlisted "and not the one its branch carries" "$HOMEWT" homewtpipe
is "and the workspace's prompt text" "WORKSPACE PROMPT" "$(zed_in "$HOMEWT")"
prompt_used_by "prompt list shows the workspace's pipeline's steps" "$HOMEWT" wspipe
prompt_not_used_by "and not the one its branch carries" "$HOMEWT" homewtpipe
says "and config path names the workspace's config/" "setup:     $WS/config" \
  in_dir "$HOMEWT" "$SPOOLWAY" config path
cd "$MAIN" || exit 2

# ------------------------------- the refusals when the main checkout has no setup
# The main checkout moves to a branch from before `init`. Two shapes then
# fail two different ways, and a person is told a different thing to do
# each time, so each sentence is pinned whole. A linked worktree carrying its
# own setup is no longer refused: with nothing in the main checkout to read,
# it reads that setup whole.
must "the main checkout leaves the branch that carries .spoolway/" git checkout -q -b before-init
must "and its branch has no setup" git rm -rqf .spoolway
must "and its branch has no setup" git commit -qm "e2e: a branch from before init"
# What the lane section left uncommitted stays on disk after `git rm`, and a
# `.spoolway/` holding even one file counts as a setup, so the folder goes too.
rm -rf .spoolway

MAIN_REFUSAL="spoolway: $MAIN is a spoolway project, but \`.spoolway/\` is not on branch \`before-init\` — check out a branch that carries it, or run \`spoolway init\` here"
PLAIN_REFUSAL="spoolway: $PLAIN is a spoolway project, but \`.spoolway/\` is not on branch \`task/plain\` — check out a branch that carries it, or run \`spoolway init\` here"

for command in "config get dispatch.lane_quiet" "config path" "pipeline list" "prompt list"; do
  # shellcheck disable=SC2086
  for shape in "main:$MAIN:$MAIN_REFUSAL" "plain:$PLAIN:$PLAIN_REFUSAL"; do
    name=${shape%%:*}; rest=${shape#*:}; dir=${rest%%:*}; want=${rest#*:}
    out=$(in_dir "$dir" "$SPOOLWAY" $command 2>&1); status=$?
    if [ "$status" -eq 1 ] && [ "$out" = "$want" ]; then
      ok "\`$command\` from the $name checkout is refused, and says what to do"
    else
      bad "\`$command\` from the $name checkout is refused, and says what to do (exit $status)"
      sed 's/^/        /' <<<"$out"
    fi
  done
done

is "the own worktree prints its own config value" "22m" "$(lane_quiet_in "$OWN")"
lists "it lists its own pipeline" "$OWN" ownpipe
prompt_used_by "prompt list shows its own pipeline's steps" "$OWN" ownpipe
says "config path names the worktree's .spoolway/" "setup:     $OWN/.spoolway" \
  in_dir "$OWN" "$SPOOLWAY" config path

finish
