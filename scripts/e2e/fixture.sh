# shellcheck shell=bash
# Building the repo a suite runs against, and the prompts its pipeline runs.
#
# One shape, not two. There were four checked-in projects under
# scripts/e2e-fixtures/ once, with their own toolchains, tests and canned
# patches; they bought a realism no suite ever asserted on and charged
# `python3`, `node` and a set of recorded patches that rotted for it. What is
# left is `new_repo`: a seed of a few files, written inline, which is enough for
# every question this harness asks — what the dispatcher decides, which counter
# fires, where a task ends up.
#
# It leaves you standing on a plan branch in a configured checkout, because a
# task may not be based on a protected branch and every suite would otherwise
# open with the same four lines.

# Resolved from this file rather than the caller's cwd: a suite runs from its
# own scratch directory.
SCRIPTS=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# new_repo <dir> <plan-branch>
#
# The trivial seed: one source file and a domain doc, which between them are
# enough for `queue add --touches`, the dependency graph and a closeout.
new_repo() {
  local dir=$1 branch=${2:-plan/demo}

  # `spoolway init` claims this checkout's own basename under `~/.spoolway/`
  # now, and every suite's checkout is named `proj` — so two suites are two
  # directories on disk sharing one real name, and would collide on the very
  # first `init` under a shared real `$HOME`, real or a CI runner's. A scratch
  # one, a sibling of `dir` and shared by every project a suite builds under
  # it, settles that the same way every suite already relies on one on
  # purpose — this just makes it everyone's default rather than an opt-in.
  # `SPOOLWAY_E2E_REAL_HOME=1` opts back out, for `suites/warmth.sh`, which
  # needs the genuine article: real credentials real lanes read.
  mkdir -p "$dir"

  if [ -z "${SPOOLWAY_E2E_REAL_HOME:-}" ]; then
    export HOME
    HOME=$(cd "$(dirname "$dir")" && pwd)/home
    mkdir -p "$HOME"
  fi

  cd "$dir" || return 1

  # Where this project's own runtime state lives now — see
  # `crate::repo::Repo::home` — and `lib.sh`'s own helpers read this rather
  # than `.spoolway/` for the queue, the lanes and the headless backend's
  # records: `basename "$dir"` is what `spoolway init` claims the project as,
  # and it moves with whichever project a suite most recently built, `homed`
  # after `proj` in `suites/flow.sh` among them.
  export SPOOLWAY_PROJECT_HOME="$HOME/.spoolway/$(basename "$dir")"

  must "git init"     git init -q -b main .
  must "git identity" git config user.email spoolway@example.invalid
  must "git identity" git config user.name  "spoolway tests"
  mkdir -p src docs
  echo "fn main() {}" > src/main.rs
  cat > docs/cli.md <<'SEED'
<script type="application/json" id="spoolway-domain">
{ "domain": "cli", "covers": ["src/**"] }
</script>
# CLI
SEED
  # The shipped pipelines' own `handover` used to run `./target/release/spoolway
  # stack` — a path good only inside this repository's own worktree — so a
  # suite driving a real dispatch pass to `handover` needed a binary planted
  # there to find. That is fixed now: the shipped `handover` runs
  # `spoolway stack`, resolved through PATH the same way `run.sh` already
  # puts $SPOOLWAY on PATH for every suite. Nothing plants a binary at a
  # repo-local path any more.
  must "the seed commit" git add -A
  must "the seed commit" git commit -qm "seed"
}

# new_forge <dir>
#
# A local forge: a bare repo on disk as `origin`, plus the `gh` test double.
# No repo created and deleted around every run, no token, no network.
#
# Every suite that drives a task to `done` needs one now. `spoolway stack`
# pushes the branch and opens a stacked pull request against its dependency's
# — there is no route that lands a change without a remote any more, which is
# the trade `merge: git` used to buy and no longer exists to.
#
# Exports FORGE and SPOOLWAY_E2E_FORGE, and leaves the caller to `git remote
# add` once its repo exists.
new_forge() {
  local dir=$1
  FORGE="$dir"
  mkdir -p "$FORGE/bin" "$FORGE/prs"
  must "the bare origin" git init -q --bare -b main "$FORGE/origin.git"
  must "the gh double" install -m 755 "$SCRIPTS/e2e-fake-gh.sh" "$FORGE/bin/gh"
  export FORGE SPOOLWAY_E2E_FORGE="$FORGE"

  # In front of the real one, and this is what makes a *real* lane reach the
  # double at all. A stand-in calls it by absolute path and never needed this;
  # a real lane runs the prompt's own words, and the prompt says `gh`. Without
  # this line that resolves to /usr/bin/gh, which talks to GitHub about a
  # repository that does not exist — and the lane reports a pass having pushed
  # nothing, which is exactly what it looks like when the suite is wrong.
  export PATH="$FORGE/bin:$PATH"
}

# publish <plan-branch>
#
# Point the repo just created at the forge and push what a lane will need to
# find there: `main`, and the plan branch every task is based on.
publish() {
  local branch=${1:-plan/demo}
  must "the remote" git remote add origin "$FORGE/origin.git"
  must "the first push" git push -q -u origin main
  must "the plan branch is on the forge" git push -q -u origin "$branch"
}

# configure_project <plan-branch> [<worktrees>] [init-flag ...]
#
# What every suite needs before it can queue anything: an initialised project,
# models whichever agent mode is in force answers to, and a plan branch to be
# based on.
#
# Any argument past the worktree root is passed straight to `spoolway init` —
# `suites/warmth.sh` is the one caller that needs one, `--take-over`, since it
# is also the one suite that reuses the real `~/.spoolway/proj` registration
# run to run rather than a scratch `$HOME` of its own; see there.
#
# `dispatch.lane_quiet` is turned right down here for the same reason the
# harness runs its dispatcher at a one-second interval: these suites run on a
# compressed clock. The shipped value is fifteen minutes, which is how long a
# real lane may reasonably be quiet before it is reminded to report — and a
# suite that waited that out for one reminder would never finish. Two seconds
# is the same behaviour at the harness's own scale. Only a *settled* lane is
# ever measured against it, so a mock still running is untouched by this.
configure_project() {
  local branch=${1:-plan/demo} worktrees=${2:-}
  shift $(( $# > 2 ? 2 : $# ))
  must "spoolway init" "$SPOOLWAY" init "$@"
  # Fresh init deliberately writes only the selected profile and points every
  # scaffolded step at it. The dispatcher suites also exercise the older
  # mixed-profile shape, because existing projects keep supporting Pi: restore
  # that fixture-specific shape explicitly rather than relying on init.
  cat >> .spoolway/config.toml <<'PROFILE'

[agents.pi]
kind = "pi"
PROFILE
  for file in .spoolway/pipelines/*.yml; do
    [ -f "$file" ] || continue
    sed -i '/^[[:space:]]*agent: claude[[:space:]]*$/{N;/prompt: \(implementer\|reproducer\|archivist\)[[:space:]]*$/s/agent: claude/agent: pi/;P;D;}' "$file"
  done
  agent_models
  own_prompts
  must "the headless backend" "$SPOOLWAY" config set dispatch.backend headless
  # How long a lane may be quiet before the watchdog reminds it to report.
  # It ships at fifteen minutes, which is patience for a real lane waiting on
  # a real test run — and far longer than any suite here is willing to sit.
  # Cut to the harness's own poll rate, so a stand-in that exits without
  # reporting is nudged on the very next pass. This used to need no line at
  # all: the watchdog read its patience off `dispatch.interval`, which the
  # harness already sets to a second. `dispatch.lane_quiet` split the two
  # apart, and the suites that force the reminder loop need the short one.
  must "the lane patience" "$SPOOLWAY" config set dispatch.lane_quiet "${E2E_INTERVAL:-1s}"
  [ -n "$worktrees" ] && must "the worktree root" \
    "$SPOOLWAY" config set dispatch.worktree_root "$worktrees"
  agent_sandbox
  must "the spoolway commit" git add -A
  must "the spoolway commit" git commit -qm "spoolway"
  must "the plan branch" git checkout -q -b "$branch"
}

# own_prompts
#
# Rewrite every shipped prompt name out of this project's pipelines, and write
# the prompts that replace them.
#
# A suite asserting that an `(archivist)` lane ran is asserting about routing —
# which step started which role — and that is a fine thing to assert. What made
# it wrong was the name: it coupled every suite to the shipped prompt set, so
# rewording `assets/prompts/archivist/PROMPT.md` broke five suites that were
# about something else entirely. The assertion is unchanged in what it proves;
# it now proves it about a prompt the harness owns.
#
# The shipped set is not left untested by this. `src/prompt.rs`'s own unit
# tests are about exactly that set — every shipped prompt names only real
# commands, knows nothing of the pipeline graph, and never names spoolway
# itself — and no suite names one any more.
#
# The prompts are short on purpose. A stand-in never reads them; what they are
# for is `spoolway prompt check`, which reads a prompt against the step that
# runs it, and a pipeline whose prompts were missing would not dispatch at all.
#
# `blocked`'s prompt is not in any pipeline file to `sed` over any more — it
# is `unattended.blocked_prompt` in config.toml, materialised onto every
# pipeline's `blocked` step by `Pipelines::assemble` — so it is rewritten with
# `spoolway config set` instead, the same as `set_models` rewrites its model.
own_prompts() {
  local file shipped own
  for file in .spoolway/pipelines/*.yml; do
    [ -f "$file" ] || continue
    while read -r shipped own; do
      sed -i "s|^\([[:space:]]\+\)prompt:[[:space:]]*$shipped[[:space:]]*$|\1prompt: $own|" "$file"
    done <<'MAP'
implementer builder
reviewer    judge
archivist   closer
reproducer  repro
MAP
  done
  must "the unblocker's prompt" "$SPOOLWAY" config set unattended.blocked_prompt clearer

  # Written once, whatever the pipelines turned out to name. A prompt nobody
  # names is harmless — `prompt check` reads the ones a step runs.
  suite_prompt builder "You write the change a task asks for, and nothing beside it."
  suite_prompt judge   "You read a diff against the task's acceptance criteria and say pass or fail."
  suite_prompt closer  "You bring this project's documents back in line with what a plan built."
  suite_prompt repro   "You turn a bug report into a check that fails, and later into one that passes."
  suite_prompt clearer "You read why the task stopped and either clear it or leave a note for another look."

  # The one prompt whose first line is load-bearing: the stand-in `pi` decides
  # it is standing in for the hand-off role by reading the composed prompt's
  # opening line, because a step id has been `pr`, then `land`, then `merge`,
  # now `handover`, and a stand-in keyed on the name fell through every time.
  # Keep this line and the match in agent-mock.sh together.
  suite_prompt stacker "You own git. Rebase this task onto the branch below it, push, and open its pull request against that same branch."
}

# suite_prompt <name> <first line>
#
# One prompt file, in the shape spoolway expects: a directory with a
# PROMPT.md in it.
suite_prompt() {
  local name=$1 opening=$2
  mkdir -p ".spoolway/prompts/$name"
  cat > ".spoolway/prompts/$name/PROMPT.md" <<PROMPT
$opening

Written by the end-to-end harness, for a pipeline the harness wrote. Nothing
here is the product's: the shipped prompts are \`src/prompt.rs\`'s own unit
tests' subject, and no suite names one.

## Never

- Never work outside the task's worktree.
- Never report an outcome for a step that is not yours.
PROMPT
}

# set_pipeline <pipeline> <key> <value>
#
# Rewrite one top-level key in a pipeline's own file. A pipeline is a file you
# edit, which is the whole point of it being data, and there is no
# `spoolway config set` for anything in it. Top-level keys are unindented, so an
# anchored line rewrite is enough and a yaml parser would be a dependency for
# nothing.
#
# No shipped pipeline carries a top-level key a suite wants to change any more —
# `merge:`, `gate:` and `unblock:` were the three, and all are gone — so nothing
# calls this today. It stays because `task_template:` is still top-level and a
# suite for it would want exactly this.
#
# The key must already be in the file. A suite that sets one absent from the
# shipped pipeline is a suite whose assertion would silently be about the
# default, so this fails loudly instead.
set_pipeline() {
  local pipeline=$1 key=$2 value=$3
  local file=".spoolway/pipelines/$pipeline.yml"
  [ -f "$file" ] || { echo "no pipeline file: $file" >&2; return 1; }
  grep -q "^$key:" "$file" || { echo "no \`$key:\` in $file" >&2; return 1; }
  sed -i "s|^$key:.*|$key: $value|" "$file"
}

# set_step <pipeline> <key> <value>
#
# The same, for a key on a step rather than on the pipeline. Steps are indented
# under `steps:`, and nothing here addresses one by id — so this refuses unless
# the key appears exactly once in the file, which keeps a suite from silently
# rewriting a second step it never meant to touch. `effort:` is the one key
# these suites change, and the shipped default pipeline declares it once.
set_step() {
  local pipeline=$1 key=$2 value=$3
  local file=".spoolway/pipelines/$pipeline.yml" n
  [ -f "$file" ] || { echo "no pipeline file: $file" >&2; return 1; }
  n=$(grep -c "^[[:space:]]\+$key:" "$file")
  [ "$n" = 1 ] || { echo "\`$key:\` appears $n time(s) in $file, expected 1" >&2; return 1; }
  sed -i "s|^\([[:space:]]\+\)$key:.*|\1$key: $value|" "$file"
}

# set_step_of <pipeline> <step-id> <key> <value>
#
# The same again, for a key on *one named* step. `set_step` addresses a key
# that appears exactly once in the whole file, which `model:` never does — it
# is on every agent step now — so a suite changing one of them has to say
# which.
#
# Sets the key whether or not the step already carries it: adding `effort:` to
# a step that has none is a scenario in its own right, since `pipeline check`
# refuses one on a kind with no effort flag. An unknown step id is an error,
# because a suite that silently edited nothing would assert against the
# shipped default and pass for the wrong reason.
#
# A key whose old value was a nested block loses that block too, not just its
# own line. `loop:` is the one that matters: a suite pinning it writes the
# flow form, `set_step_of default review loop "{fix: 3}"`, and the routes the
# step listed underneath before must go with the line that headed them or the
# file is left with orphaned keys the loader refuses.
set_step_of() {
  local pipeline=$1 step=$2 key=$3 value=$4
  local file=".spoolway/pipelines/$pipeline.yml"
  [ -f "$file" ] || { echo "no pipeline file: $file" >&2; return 1; }
  awk -v step="$step" -v key="$key" -v value="$value" '
    /^[[:space:]]*-[[:space:]]*id:/ {
      id = $0
      sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/, "", id)
      sub(/[[:space:]]*$/, "", id)
      here = (id == step)
      dropping = 0
      print
      if (here) {
        # A step key is indented to where `id:` sits, which is two past the
        # `- ` that opens the item.
        match($0, /^[[:space:]]*-[[:space:]]*/)
        print substr($0, 1, RLENGTH - 2) "  " key ": " value
        found = 1
      }
      next
    }
    # Drop whatever the step said before, so setting is not appending.
    here && $0 ~ "^[[:space:]]+" key ":" {
      match($0, /^[[:space:]]*/)
      dropping = 1
      drop_indent = RLENGTH
      next
    }
    # And drop the block that key headed, if it had one: every following line
    # indented past it belongs to the value being replaced.
    dropping {
      match($0, /^[[:space:]]*/)
      if (RLENGTH > drop_indent && $0 !~ /^[[:space:]]*$/) next
      dropping = 0
    }
    { print }
    END { if (!found) exit 3 }
  ' "$file" > "$file.new" || {
    echo "no step \`$step\` in $file" >&2; rm -f "$file.new"; return 1
  }
  mv "$file.new" "$file"
}

# set_loop_of <pipeline> <step-id> <arriving-step> <n>
#
# The round budget one step allows arrivals from another, which `set_step_of`
# cannot reach: `loop:` is a nested map, and setting it to a scalar would leave
# the `<arriving>: <n>` line under it orphaned at its own indent.
#
# A suite that wants several rounds says so here rather than inheriting
# whatever the shipped pipeline happens to carry. Budgets are tuning — this
# project dropped every one of its own to a single lap once — and a suite about
# the loop mechanism must not go red because somebody turned a number down.
#
# The step must already carry a `loop:` block naming that arriving step; a
# suite that silently edited nothing would assert against the shipped default
# and pass for the wrong reason.
set_loop_of() {
  local pipeline=$1 step=$2 from=$3 n=$4
  local file=".spoolway/pipelines/$pipeline.yml"
  [ -f "$file" ] || { echo "no pipeline file: $file" >&2; return 1; }
  awk -v step="$step" -v from="$from" -v n="$n" '
    /^[[:space:]]*-[[:space:]]*id:/ {
      id = $0
      sub(/^[[:space:]]*-[[:space:]]*id:[[:space:]]*/, "", id)
      sub(/[[:space:]]*$/, "", id)
      here = (id == step)
      inloop = 0
      print
      next
    }
    here && $0 ~ /^[[:space:]]+loop:[[:space:]]*$/ { inloop = 1; print; next }
    # Any other key at the step level closes the nested block.
    here && inloop && $0 ~ /^[[:space:]]+[a-z_]+:/ && $0 !~ "^[[:space:]]+" from ":" { inloop = 0 }
    here && inloop && $0 ~ "^[[:space:]]+" from ":" {
      match($0, /^[[:space:]]*/)
      print substr($0, 1, RLENGTH) from ": " n
      found = 1
      next
    }
    { print }
    END { if (!found) exit 3 }
  ' "$file" > "$file.new" || {
    echo "no \`loop: {$from: ...}\` on step \`$step\` in $file" >&2
    rm -f "$file.new"; return 1
  }
  mv "$file.new" "$file"
}

# set_models <local-model> <cloud-model>
#
# Point every step at the model this run's agents answer to.
#
# A model is a pipeline fact now — each step names its own — so this rewrites
# the pipeline files rather than setting a config key. Fresh pipelines carry
# explicit blank models. This fixture restored the old mixed profile shape
# above, so Pi steps take the local stand-in and Claude steps the cloud one.
#
# `blocked` is the exception: it is not in any pipeline file for an `awk` to
# find, so its blank `unattended.blocked_model` is rewritten with `spoolway
# config set` instead.
#
# The check at the end is the point of doing it this way rather than with a
# blanket substitution: if a shipped pipeline ever renames its placeholder,
# a lane would otherwise start against a model no agent here answers to and
# the suite would fail somewhere a long way from the cause.
set_models() {
  local local_model=$1 cloud_model=$2 file found=
  for file in .spoolway/pipelines/*.yml; do
    [ -f "$file" ] || continue
    found=1
    awk -v local_model="$local_model" -v cloud_model="$cloud_model" '
      /^[[:space:]]+agent:[[:space:]]*/ { agent = $2 }
      /^[[:space:]]+model:[[:space:]]*""[[:space:]]*$/ {
        match($0, /^[[:space:]]*/)
        model = agent == "pi" ? local_model : cloud_model
        print substr($0, 1, RLENGTH) "model: " model
        next
      }
      { print }
    ' "$file" > "$file.tmp" && mv "$file.tmp" "$file"
  done
  [ -n "$found" ] || { echo "no pipelines to point at a model" >&2; return 1; }
  must "the unblocker's model" "$SPOOLWAY" config set unattended.blocked_model "$cloud_model"
  if grep -rlE '^[[:space:]]+model:[[:space:]]*""[[:space:]]*$' \
       .spoolway/pipelines/ >&2; then
    echo "a scaffold model blank survived set_models (files above)" >&2
    return 1
  fi
  return 0
}

# local_model [pipeline]
#
# The model this project's local steps are running.
#
# Every agent step names a model and `pipeline check` refuses one that does
# not, so a suite adding a step has to give it one — and which model that is
# depends on the agent mode. Read out of the file rather than passed in, so a
# suite adding a step never becomes a second place the mode has to be known.
local_model() {
  local file=".spoolway/pipelines/${1:-default}.yml" model
  [ -f "$file" ] || { echo "no pipeline file: $file" >&2; return 1; }
  model=$(awk '/^  - id: implement$/,/^$/ { if ($1 == "model:") print $2 }' "$file")
  [ -n "$model" ] || { echo "no \`model:\` under implement in $file" >&2; return 1; }
  printf '%s\n' "$model"
}

# A task body is markdown a suite writes, not flags spoolway formats: what a
# task file says is the project's, and nothing in spoolway reads it back.
#
# It asks for real work all the same, because a body nobody could act on is a
# body that stops being maintained: it is what a person reads when a suite is
# failing and they are trying to work out what the lane was even asked for. So
# the work is small, needs no toolchain, and lands in a file named
# after the task — which is the same reason the stand-in writes
# `work-<task>.txt` rather than a shared file: two lanes running at once must
# not be in each other's way in a suite that is not about conflicts.
task_body() {
  local path=$1
  cat > "$path" <<'TASKBODY'
## Goal

Add `notes/<id>.md`, where `<id>` is this task's own id: a heading `# <id>`, and
one sentence under it saying what this repository is.

## Non-goals

Out of scope. Doing any of these is a review failure, not a bonus.

- changing any file outside `notes/`
- anything not required by the acceptance criteria below

## Acceptance criteria

- `notes/<id>.md` exists, opens with `# <id>`, and has one sentence under it
- nothing else in the repository changes

## References

Read this before you start. It describes the system as it is; work from it
rather than restating it.

- `docs/cli.md` — what this repository is
TASKBODY
}

# task_doc <path> <id> <body-file> [extra frontmatter lines...]
#
# Writes a whole task document to <path>: `id: <id>`, whatever extra
# frontmatter lines a suite names, and the given body underneath — the shape
# `queue add --from` reads, since the per-task flags it used to take
# (`--body-file`, `--touches`, `--depends-on`, `--plan`, `--pipeline`,
# `--parallel`) are gone.
task_doc() {
  local path=$1 id=$2 body=$3
  shift 3
  {
    echo "---"
    echo "id: $id"
    echo "title: $id, done"
    for line in "$@"; do echo "$line"; done
    echo "---"
    cat "$body"
  } >"$path"
}

# pending_doc <id> <body-file> [extra frontmatter lines...]
#
# One task document in the pending directory the queue screen scans — the
# same document `task_doc` writes, put where a producer leaves it rather than
# where a `--from` argument would name it.
pending_doc() {
  local id=$1 body=$2
  shift 2
  mkdir -p "$SPOOLWAY_PROJECT_HOME/pending"
  task_doc "$SPOOLWAY_PROJECT_HOME/pending/$id.md" "$id" "$body" "$@"
}
