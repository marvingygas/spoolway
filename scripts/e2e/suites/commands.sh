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
# This suite used to also assert the `agent list`/`agent verify` output, the
# transcript an ambient session is read from, and every refusal `pipeline
# check` makes about a `run:` step. None of those start a process, so all
# three were unit tests written in bash — see `src/agent.rs`, `src/usage.rs`
# and `src/pipeline.rs`, which cover them against the code rather than
# against a fixture.
#
# One more thing lives here now that needs a real linked worktree rather than
# a process: `spoolway config`'s asymmetry between reading (the checkout in
# front of the command) and writing (the project, always — refused elsewhere).
# No `covers:` tag of its own — the map `coverage.sh` builds only enumerates
# `config.toml` keys and pipeline step keys, and this is neither; it is CLI
# behaviour the map has no row for.
#
# covers: step.run — a command step runs in the task`s worktree and routes on its exit code
# covers: step.background — the task moves on the same pass, and cleanup stops the command
# covers: step.timeout — a hung command is stopped at the step`s own bound, not the dispatcher`s
# covers: step.loop — a command step's own failure feeds the loop bound on the agent step behind it, the shape a mechanical CI gate is built on
# covers: stack.summary.agent — a config predating the table gets it back from `update`, blank
# covers: stack.summary.model — same run: blank, and unchanged by a second `update`
# covers: stack.summary.effort — same run: blank, and unchanged by a second `update`
# covers: stack.summary.prompt — same run: defaulted to `summariser`, and unchanged by a second `update`
# covers: issue_tracking.hook — a bare filename, resolved inside .spoolway/hooks/, fires once per task per event with the full environment set
# covers: issue_tracking.project_key — opaque, handed to the hook verbatim as SPOOLWAY_PROJECT_KEY
# covers: issue_tracking.on_fail — a non-zero exit under "pause" holds the task on `queued` and `done`, and only records the failure on `blocked` and `paused`
# covers: issue_tracking.key_in_names — with it on and the hook answering slug=, `queue add` writes `group: <slug>-<group>` and `branch: task/<slug>-<id>` and stores the hook's url=
# covers: retention.days — an entry past the age is swept from a byproduct directory, and never from queue/, however old
# covers: prices.max_age_days — doctor notes only a table older than the configured limit; 0 is covered by the unit boundary test
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

# A command step's output has to travel with the change to be worth anything,
# and where the change goes is the forge. So this suite needs one too, even
# though nothing here is about handing over.
new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$CTL" "" "" "" "$FORGE"

new_repo "$LIVE/proj"
configure_project plan/live "$LIVE/worktrees"
publish plan/live

# ---------------------------------------------------------- model price refresh
# A file URL still travels through curl, but never leaves the fixture. Keeping
# the raw litellm shape here (rather than copying spoolway's output shape) also
# proves the command's distilling half is on the path the real binary runs.
PRICE_FIXTURE="$LIVE/model-prices-raw.json"
cat >"$PRICE_FIXTURE" <<'JSON'
{
  "fake-local": {
    "mode": "chat",
    "max_input_tokens": 12345,
    "input_cost_per_token": 0.000001,
    "output_cost_per_token": 0.000002
  },
  "not-chat": {
    "mode": "embedding",
    "input_cost_per_token": 0.000001,
    "output_cost_per_token": 0.000002
  }
}
JSON
must "models refresh fetches and distils a local fixture through curl" \
  env SPOOLWAY_MODEL_PRICES_URL="file://$PRICE_FIXTURE" "$SPOOLWAY" models refresh
works "the refreshed machine-wide table is valid JSON" \
  jq -e '.source and .license == "MIT" and .generated and .models["fake-local"].input == 1' \
  "$HOME/.spoolway/model-prices.json"
MODELS_OUT="$LIVE/models-refreshed.out"
"$SPOOLWAY" models >"$MODELS_OUT"
if grep -qE '^fake-local[[:space:]].*[[:space:]]refreshed[[:space:]]' "$MODELS_OUT"; then
  ok "models reads the new row back with SOURCE refreshed"
else
  bad "models reads the new row back with SOURCE refreshed"
  sed 's/^/        /' "$MODELS_OUT"
fi
REFRESHED_GENERATED=$(jq -r '.generated' "$HOME/.spoolway/model-prices.json")
MODELS_FOOTER=$(tail -n 1 "$MODELS_OUT")
MODELS_FOOTER_PATTERN="^Prices generated ${REFRESHED_GENERATED}, [0-9]+ days ago\\. Refresh with "
MODELS_FOOTER_PATTERN+='`spoolway models refresh`\.$'
if grep -qE "$MODELS_FOOTER_PATTERN" <<<"$MODELS_FOOTER"; then
  ok "models closes with the active table's date, whole-day age, and refresh command"
else
  bad "models closes with the active table's date, whole-day age, and refresh command"
  printf '        %s\n' "$MODELS_FOOTER"
fi

# Rewrite only the fixture header: the same valid refreshed table continues
# answering model lookup, while doctor sees each side of the configured age
# boundary without a network call or a clock-dependent sleep.
PRICE_TABLE="$HOME/.spoolway/model-prices.json"
jq '.generated = "2000-01-01"' "$PRICE_TABLE" >"$PRICE_TABLE.tmp"
mv "$PRICE_TABLE.tmp" "$PRICE_TABLE"
must "price age limit is set for the doctor boundary" \
  "$SPOOLWAY" config set prices.max_age_days 30
says "doctor notes a refreshed table past the age limit" \
  'past the 30 in `prices.max_age_days`' "$SPOOLWAY" doctor

jq --arg today "$(date -u +%F)" '.generated = $today' "$PRICE_TABLE" >"$PRICE_TABLE.tmp"
mv "$PRICE_TABLE.tmp" "$PRICE_TABLE"
silent_about "doctor stays quiet for a refreshed table inside the age limit" \
  'past the 30 in `prices.max_age_days`' "$SPOOLWAY" doctor

BODY="$LIVE/body.md"
task_body "$BODY"

# --------------------------------------------------------------- scaffolding
# `init` asks five questions at a terminal and none anywhere else, which is
# exactly the distinction a shell suite is the right place to hold: everything
# below runs with no tty, so an `init` that ever read stdin here would hang the
# suite rather than fail it. In its own directory — this is a project being
# created, and the suite's own is already one.
INITDIR="$LIVE/init"
mkdir -p "$INITDIR/asked" && (cd "$INITDIR/asked" && git init -q -b main .)
works "init scaffolds a project with the answers given as flags" \
  env -C "$INITDIR/asked" "$SPOOLWAY" init \
  --provider pi --agent codex --model e2e-local-model

has "the kind asked for is what the local profile runs" 'kind = "codex"' \
  "$INITDIR/asked/.spoolway/config.toml"
has "the model asked for is what a local step names" "model: e2e-local-model" \
  "$INITDIR/asked/.spoolway/pipelines/default.yml"
works "and the provider's skills are installed, not suggested" \
  test -f "$INITDIR/asked/.pi/skills/spoolway-plan/SKILL.md"
works "including the task-cutting skill spoolway-plan's step 7 invokes" \
  test -f "$INITDIR/asked/.pi/skills/spoolway-tasks/SKILL.md"
works "and spoolway-calibrate, which hands its own kept findings to it" \
  test -f "$INITDIR/asked/.pi/skills/spoolway-calibrate/SKILL.md"
works "in that provider's directory alone" \
  test ! -e "$INITDIR/asked/.claude"

# The path every script and CI runner takes. Nothing is asked, so the shipped
# defaults stand — including the model placeholder, which has to survive for
# `doctor` to have anything to report.
mkdir -p "$INITDIR/unasked" && (cd "$INITDIR/unasked" && git init -q -b main .)
works "init with no terminal asks nothing and takes the defaults" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" init
has "so the model placeholder is still standing" "model: your-local-model" \
  "$INITDIR/unasked/.spoolway/pipelines/default.yml"
works "and claude's skills are what a run with nobody to ask installs" \
  test -f "$INITDIR/unasked/.claude/skills/spoolway-plan/SKILL.md"
works "spoolway-tasks lands beside it" \
  test -f "$INITDIR/unasked/.claude/skills/spoolway-tasks/SKILL.md"
works "and spoolway-calibrate lands too" \
  test -f "$INITDIR/unasked/.claude/skills/spoolway-calibrate/SKILL.md"

refuses "a kind spoolway cannot launch is refused at init, not at the first lane" \
  "cannot launch" env -C "$INITDIR/unasked" "$SPOOLWAY" init --agent gemini

# `[stack.summary]` is a table every config now carries, blank `agent` and
# `model` included — a project whose config predates it never wrote the
# section at all, so `update` is what has to pick it up, the same way it picks
# up any other setting a config has yet to see.
STACK_CONFIG="$INITDIR/unasked/.spoolway/config.toml"
sed -i '/^\[stack\.summary\]$/,/^prompt = "summariser"$/d' "$STACK_CONFIG"
# `task contract` is what `queue check` became — it is the command in this
# suite that loads the project's config and its pipelines and exits without
# writing anything, which is all "does this config still parse" needs.
works "a config predating [stack.summary] still parses" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" task contract
says "update reports the config as changed" "config.toml" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" update

# Anchored to the table itself — `pipeline_model = ""`, `pipeline_effort = ""`
# and `blocked_effort = ""` already sit elsewhere in this file with the same
# text a bare substring search for `model = ""` or `effort = ""` would catch,
# which would pass even with the whole `[stack.summary]` table missing.
WANT_STACK_TABLE=$(printf '[stack.summary]\nagent = ""\nmodel = ""\neffort = ""\nprompt = "summariser"\n')
GOT_STACK_TABLE=$(sed -n '/^\[stack\.summary\]$/,+4p' "$STACK_CONFIG")
works "and writes the table back with every value blank but prompt" \
  test "$GOT_STACK_TABLE" = "$WANT_STACK_TABLE"

silent_about "a second run finds nothing left to add" "config.toml" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" update
works "so the table's blank values are exactly what they were" \
  test "$(sed -n '/^\[stack\.summary\]$/,+4p' "$STACK_CONFIG")" = "$WANT_STACK_TABLE"

# Run again for a second provider: the skills land, and the config the project
# has been running on is not rewritten around it.
works "a second init installs another provider's skills" \
  env -C "$INITDIR/asked" "$SPOOLWAY" init --provider pi
works "without disturbing the first" \
  test -f "$INITDIR/asked/.pi/skills/spoolway-plan/SKILL.md"
works "and spoolway-tasks is among the second provider's skills too" \
  test -f "$INITDIR/asked/.pi/skills/spoolway-tasks/SKILL.md"
works "spoolway-calibrate as well" \
  test -f "$INITDIR/asked/.pi/skills/spoolway-calibrate/SKILL.md"
has "and without rewriting the config" 'kind = "codex"' \
  "$INITDIR/asked/.spoolway/config.toml"
says "pi's skills come with the one thing the file list cannot say" \
  "only once the project is trusted" \
  env -C "$INITDIR/asked" "$SPOOLWAY" install pi

# ------------------------------------------------------- tracker scaffolding
# `--tracker` and `--project-key` answer the same two questions
# `init`'s menu asks interactively, and land in `[issue_tracking]` — but
# every hook script is written whichever tracker was named, not only the
# chosen one, so switching later is a config edit rather than a second
# `init`.
mkdir -p "$INITDIR/github" && (cd "$INITDIR/github" && git init -q -b main .)
works "init --tracker github --project-key answers both questions with no prompt" \
  env -C "$INITDIR/github" "$SPOOLWAY" init --tracker github --project-key acme/app
has "the hook it names" 'hook = "github.sh"' "$INITDIR/github/.spoolway/config.toml"
has "and the project it files into" 'project_key = "acme/app"' \
  "$INITDIR/github/.spoolway/config.toml"
works "the github hook is written" test -f "$INITDIR/github/.spoolway/hooks/github.sh"
works "and so is jira's, unchosen or not" test -f "$INITDIR/github/.spoolway/hooks/jira.sh"
works "a hook is written executable" test -x "$INITDIR/github/.spoolway/hooks/github.sh"

# `none` — `unasked`'s own `init` above already took every default with
# nobody there to ask, which includes the tracker question defaulting to
# `none`: the table stays empty even though both scripts are on disk.
has "unasked left the hook empty" 'hook = ""' "$INITDIR/unasked/.spoolway/config.toml"
has "and the project key empty" 'project_key = ""' "$INITDIR/unasked/.spoolway/config.toml"
works "both hooks are written all the same" \
  test -f "$INITDIR/unasked/.spoolway/hooks/github.sh"
works "the jira one too" test -f "$INITDIR/unasked/.spoolway/hooks/jira.sh"

# `spoolway update` never touches a hook `init` has already written — the
# same rule a prompt or a task skeleton already follows once a project has
# made a file its own.
HOOK="$INITDIR/github/.spoolway/hooks/github.sh"
printf '#!/bin/sh\necho mine\n' > "$HOOK"
must "update runs over the tracker project" env -C "$INITDIR/github" "$SPOOLWAY" update
has "the hook this project edited is exactly as it left it" "echo mine" "$HOOK"

# ---------------------------------------------------------------- task-log.md
# What belongs under each heading spoolway appends to a task file — seeded by
# `init`, left alone by `update` once a project has made it its own, and
# restorable one file at a time.
TASK_LOG="$INITDIR/unasked/.spoolway/templates/task-log.md"
works "init seeded task-log.md" test -f "$TASK_LOG"
has "with all three of spoolway's own headings" "## Status Log" "$TASK_LOG"
has "and Handoff" "## Handoff" "$TASK_LOG"
has "and Blocker" "## Blocker" "$TASK_LOG"

printf '## Status Log\n\nours, not spoolway'"'"'s.\n' > "$TASK_LOG"
must "update runs over the unasked project" env -C "$INITDIR/unasked" "$SPOOLWAY" update
has "task-log.md this project edited is exactly as it left it" \
  "ours, not spoolway's" "$TASK_LOG"

works "--replace restores the shipped file" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" update --replace .spoolway/templates/task-log.md
has "back to spoolway's own Status Log wording" \
  "One line per transition" "$TASK_LOG"

# --------------------------------------------------------------- task contract
# `task contract` never touches `.spoolway/` in either mode — bare, it only
# ever reads pipelines already loaded in memory; `--from`, it runs the very
# validation `queue add --from` runs and stops short of the save. Both halves
# of that claim are worth a real process: a unit test cannot see whether a
# directory gained a file, only this can.
command -v jq >/dev/null || { echo "commands.sh needs jq" >&2; exit 2; }

CHECK_JSON=$("$SPOOLWAY" task contract 2>&1)
if jq -e '.pipelines.default.id_budget' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "bare task contract prints the contract as parseable JSON"
else
  bad "bare task contract prints the contract as parseable JSON"
  echo "$CHECK_JSON" | sed 's/^/        /'
fi

# The three things this task added to the bare call: which pipeline is the
# default, a `fields` sentence for every key the JSON itself says is
# required or optional, and a `body` string shipped alongside every
# pipeline's other own facts — not a second command, not a second read.
if jq -e '.default | type == "string"' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "the contract names this project's default pipeline"
else
  bad "the contract names this project's default pipeline"
  echo "$CHECK_JSON" | sed 's/^/        /'
fi

if jq -e '
    ( .keys.required + .keys.optional ) as $settable
    | ($settable - (.fields | keys)) == []
  ' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "every required or optional key has a fields entry"
else
  bad "every required or optional key has a fields entry"
  echo "$CHECK_JSON" | sed 's/^/        /'
fi

if jq -e '[.pipelines[] | (.body | type == "string")] | all' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "every pipeline carries its body skeleton as a string"
else
  bad "every pipeline carries its body skeleton as a string"
  echo "$CHECK_JSON" | sed 's/^/        /'
fi

RESERVED="$LIVE/reserved.md"
{
  echo "---"
  echo "id: reserved-key"
  echo "title: reserved-key, done"
  echo "group: live"
  echo "run: r00001"
  echo "---"
  cat "$BODY"
} > "$RESERVED"

BEFORE_CHECK=$(ls "$SPOOLWAY_PROJECT_HOME/queue" 2>/dev/null | sort)
"$SPOOLWAY" task contract --from "$RESERVED" >/dev/null 2>&1
CHECK_STATUS=$?
AFTER_CHECK=$(ls "$SPOOLWAY_PROJECT_HOME/queue" 2>/dev/null | sort)

if [ "$CHECK_STATUS" -ne 0 ]; then
  ok "task contract --from a document setting run: exits non-zero"
else
  bad "task contract --from a document setting run: exits non-zero"
fi
if [ "$BEFORE_CHECK" = "$AFTER_CHECK" ]; then
  ok "and leaves the queue directory exactly as it was"
else
  bad "and leaves the queue directory exactly as it was"
  diff <(echo "$BEFORE_CHECK") <(echo "$AFTER_CHECK") | sed 's/^/        /'
fi

# --------------------------------------------------------- pipeline contract
# `pipeline default` is gone — the format is now printed from the binary
# itself, in `pipeline contract`, rather than dumped from the shipped files.
refuses "pipeline default is no longer a subcommand" \
  "unrecognized subcommand" "$SPOOLWAY" pipeline default

PIPELINE_JSON=$("$SPOOLWAY" pipeline contract 2>&1)
if jq -e '(.keys.step | index("agent")) and (.keys.step | index("loop"))' \
    <<<"$PIPELINE_JSON" >/dev/null 2>&1; then
  ok "bare pipeline contract prints the step keys as parseable JSON"
else
  bad "bare pipeline contract prints the step keys as parseable JSON"
  echo "$PIPELINE_JSON" | sed 's/^/        /'
fi

if jq -e '
    ( .keys.pipeline + .keys.step ) as $settable
    | ($settable - (.fields | keys)) == []
  ' <<<"$PIPELINE_JSON" >/dev/null 2>&1; then
  ok "every pipeline or step key has a fields entry"
else
  bad "every pipeline or step key has a fields entry"
  echo "$PIPELINE_JSON" | sed 's/^/        /'
fi

says "and names this project's own agent profiles" '"pi"' \
  "$SPOOLWAY" pipeline contract

# ---------------------------------------------------------- prompt contract
# `prompt contract` gains a seventh section, on every call, whatever
# `--step` names — the prompt skeleton, not a paraphrase of it.
says "prompt contract prints the shape-to-write section" \
  "THE SHAPE TO WRITE" "$SPOOLWAY" prompt contract

# ------------------------------------------------------------- the queue screen
# The one thing no unit test can reach: `spoolway queue` reading real keystrokes
# off a pipe, submitting a real group, and clearing that group's documents off
# a real disk. `run_screen` is driven headlessly in Rust already — what is only
# provable here is that the whole binary, invoked as a person invokes it, does
# the same thing end to end.
#
# Two documents of one group, and a third of another, so "removes exactly that
# group's documents" has something to be wrong about.
# `screen-other` is written first on purpose, so `screen-batch` is both the
# newer group and the earlier name: the cursor opens on it under either
# tie-break, and this case never depends on how fine-grained a birth time this
# filesystem keeps.
pending_doc screen-other "$BODY" "group: screen-other" "touches: [notes/other.md]"
pending_doc screen-one "$BODY" "group: screen-batch" "touches: [notes/screen-one.md]"
pending_doc screen-two "$BODY" "group: screen-batch" \
  "depends_on: [screen-one]" "touches: [notes/screen-two.md]"

# space selects the highlighted group, enter submits it, `n` declines the
# dispatcher the screen then offers. The screen ends on its own the moment the
# pipe runs dry — see `queue_screen`'s own doc comment on why a pipe is read
# exactly as a terminal would be.
printf ' \rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the screen queues the first task of the group it submitted" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-one.md"
works "and the second one with it — a group goes whole or not at all" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-two.md"
works "the submitted group's documents are gone from pending" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/pending/screen-one.md"
works "both of them" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/pending/screen-two.md"
works "and the group nobody selected is left exactly where it was" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/screen-other.md"
works "which is still not in the queue" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/screen-other.md"

# The other half of the same directory: `--from` pointed at it queues every
# `.md` document there, with no screen involved at all.
must "the group left behind, queued by naming the directory" \
  "$SPOOLWAY" queue add --from "$SPOOLWAY_PROJECT_HOME/pending"
works "reaches the queue the same way" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-other.md"
# `queue add --from` is not the screen: it queues a document, it does not own
# the directory, so nothing is deleted.
works "and leaves the document where it found it" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/screen-other.md"
rm -f "$SPOOLWAY_PROJECT_HOME/pending/screen-other.md"

# --------------------------------------------- a group with no pending documents
# Queued straight through `queue add --from`, never through the screen, so its
# only trace anywhere is the queue directory — the same shape a group is left
# in the moment the screen itself submits it. The row this is checking for is
# not one `list_groups`' own unit tests can reach on their own: those call it
# directly against a fixture, never through the real binary reading a real
# keystroke off a real pipe.
SCREEN_QUEUE_ONLY="$LIVE/screen-shipped.md"
task_doc "$SCREEN_QUEUE_ONLY" screen-shipped "$BODY" \
  "group: screen-shipped-group" "touches: [notes/screen-shipped.md]"
must "a group queued directly, never through the screen" \
  "$SPOOLWAY" queue add --from "$SCREEN_QUEUE_ONLY"
works "it never touched the pending directory" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/pending/screen-shipped.md"

# The pending directory is empty at this point — both cases above already
# cleared every document out of it — so this is also the one place proving
# the screen opens on an empty pending directory rather than reporting "No
# task documents", so long as the queue itself still holds a group.
printf 'h' | "$SPOOLWAY" queue >"$LIVE/queue-screen-only.out" 2>&1
if grep -q "No task documents" "$LIVE/queue-screen-only.out"; then
  bad "a group with nothing left in pending still opens the screen"
  sed 's/^/        /' "$LIVE/queue-screen-only.out"
else
  ok "a group with nothing left in pending still opens the screen"
fi
has "\`h\` still lists a group whose documents are only in the queue now" \
  "screen-shipped-group" "$LIVE/queue-screen-only.out"

# --------------------------------------------------- the archive's own rows
# `list_groups` now reads `archive/` as a third source, and `h` widens the
# left pane one state at a time: pending only, then plus queued, then plus
# done, then wraps. Only the real binary, run against a task a real
# dispatcher actually archived, proves the wiring — `list_groups`'s own unit
# tests read a synthetic fixture directory, never `Repo::archive_dir()`
# after a real run.
task_doc "$LIVE/archived-row.md" archived-row "$BODY" \
  "group: arch-row" "touches: [notes/archived-row.md]"
must "a task queued for the archive-cycling case" \
  "$SPOOLWAY" queue add --from "$LIVE/archived-row.md"
if drive archived-row gone 60; then
  ok "it ran to completion and left the queue"
else
  bad "it ran to completion and left the queue (at \`$(stage_of archived-row)\`)"
fi
works "and landed in the archive" \
  test -f "$SPOOLWAY_PROJECT_HOME/archive/archived-row.md"

# Three key presses of `h`, captured as one session: `draw` writes a fresh
# `\x1b[2J\x1b[H` before every frame that differs from the last, so the
# python snippet below splits the raw output back into the four frames this
# draws — opening, then one per press — rather than grepping the whole file,
# which could never tell "shown once, then hidden again" from "never shown".
printf 'hhh' | "$SPOOLWAY" queue >"$LIVE/queue-h-cycle.out" 2>&1
if python3 - "$LIVE/queue-h-cycle.out" arch-row <<'PY'
import sys

data = open(sys.argv[1], "rb").read()
name = sys.argv[2].encode()
# `draw` writes `\x1b[?25l` (hide the cursor) once, before the first frame,
# so the piece ahead of the first real `\x1b[2J\x1b[H` is that preamble, not
# a frame — dropped with `[1:]` rather than filtered for being non-empty,
# since the preamble is itself a few bytes long and would otherwise pass.
frames = data.split(b"\x1b[2J\x1b[H")[1:]
# frames[0..3] are the opening frame and the three `h` presses, in order —
# `done` is the third widening, so the group first appears in frames[2] and
# the fourth frame (the wrap) must not carry it any more.
ok = len(frames) >= 4 and name in frames[2] and name not in frames[-1]
sys.exit(0 if ok else 1)
PY
then
  ok "\`h\` shows the archived group under \`done\` on the third press and hides it again on the wrap"
else
  bad "\`h\` shows the archived group under \`done\` on the third press and hides it again on the wrap"
  sed 's/^/        /' "$LIVE/queue-h-cycle.out"
fi

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

  # `implement` goes through the new step now, which means `review` is entered
  # from it rather than from `implement` — so the budget `review` keeps has to
  # name the new step. One that names a route the graph no longer has is
  # refused at load, correctly: it would bound nothing, and the cycle it was
  # written for would be free to run forever.
  sed -i "0,/^    on_pass: review$/s//    on_pass: $id/" "$file"
  sed -i "0,/^      implement: /s//      $id: /" "$file"
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
  printf '    agent: pi\n    prompt: implementer\n    model: your-local-model\n'
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
  printf '    agent: pi\n    prompt: implementer\n    model: your-local-model\n'
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
# `arrivals` counts every line naming `→ \`e2e\``, and this log carries four
# of them: the entry from `review`, the two laps `gate` sent back, and
# `apply_loop_budget`'s own note on the third — "`gate` → `e2e` spent its 2
# rounds…" — which matches the same grep. The two real laps are what the
# `rounds` counter below actually proves; this only checks that the third
# attempt left no further arrival at `e2e` behind it.
if [ "$(arrivals $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md e2e)" -eq 4 ]; then
  ok "the third attempt spent the budget rather than arriving at e2e again"
else
  bad "the third attempt spent the budget rather than arriving at e2e again \
(arrived at \`e2e\` $(arrivals $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md e2e) \
time(s), wanted 4)"
fi
counter "and the counter agrees: two laps banked, not one" \
  rounds "gate->e2e" 2 $SPOOLWAY_PROJECT_HOME/queue/gated-twice.md

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

  # A failing, paned command leaves its pane standing rather than closing it —
  # the one thing a person needs the pane to look at.
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
    ok "and its pane stands rather than closing behind a failure"
  else
    bad "and its pane stands rather than closing behind a failure"
  fi

  tmux -S "$SOCK" kill-server 2>/dev/null || true
  unset SPOOLWAY_TMUX_SOCKET
  must "back to headless" "$SPOOLWAY" config set dispatch.backend headless
fi

# ------------------------------------------------------ config follows the checkout
# `config get`/`show`/`path` read `repo.checkout` now — the worktree actually
# in front of a command — while `config set` still writes only `repo.root`,
# the project's own copy, and refuses rather than land somewhere the
# dispatcher will never read from. A worktree whose `config.toml` differs
# from the project's is what proves reads really did follow the checkout.
PROJECT=$(pwd -P)
WT="$LIVE/worktrees/config-wt"
must "cutting a worktree for a config of its own" \
  git worktree add -q -b task/config-diff "$WT" plan/live
must "giving the worktree's own config a value the project's does not have" \
  sed -i 's|^default_pipeline = .*|default_pipeline = "from-the-worktree"|' \
  "$WT/.spoolway/config.toml"
must "committing the worktree's own config" \
  git -C "$WT" add .spoolway/config.toml
must "committing the worktree's own config" \
  git -C "$WT" commit -qm "e2e: a config value only this worktree has"

says "config get in the worktree reads its own value" "from-the-worktree" \
  "$SPOOLWAY" -C "$WT" config get dispatch.default_pipeline
silent_about "config get in the main checkout does not see it" "from-the-worktree" \
  "$SPOOLWAY" config get dispatch.default_pipeline
says "config path in the worktree names its own file, not the project's" \
  "$WT/.spoolway/config.toml" "$SPOOLWAY" -C "$WT" config path

BEFORE_ROOT=$(cat .spoolway/config.toml)
BEFORE_WT=$(cat "$WT/.spoolway/config.toml")
OUT=$("$SPOOLWAY" -C "$WT" config set dispatch.default_pipeline "should-not-land" 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ]; then ok "config set in the worktree exits non-zero"
else bad "config set in the worktree exits non-zero"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF "the dispatcher reads the project's config, not this worktree's." <<<"$OUT"; then
  ok "and says why"
else bad "and says why"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF -- "-C $PROJECT config set dispatch.default_pipeline should-not-land" <<<"$OUT"; then
  ok "and names the exact -C invocation that would write to the project"
else bad "and names the exact -C invocation that would write to the project"; sed 's/^/        /' <<<"$OUT"; fi
if [ "$(cat .spoolway/config.toml)" = "$BEFORE_ROOT" ]; then
  ok "and the project's own config is left untouched"
else bad "and the project's own config is left untouched"; fi
if [ "$(cat "$WT/.spoolway/config.toml")" = "$BEFORE_WT" ]; then
  ok "and the worktree's own config is left untouched too"
else bad "and the worktree's own config is left untouched too"; fi

must "removing the worktree" git worktree remove --force "$WT"
must "removing its branch" git branch -D task/config-diff

# ------------------------------------------------------- issue_tracking hook
# `[issue_tracking]` fires a project's own script once per task on each of the
# four states nothing inside a pipeline file can already put a `run:` step on
# — `queued`, `blocked`, `paused` and `done` are reserved stage names, never
# steps a pipeline may declare. It reuses the same detached-process machinery
# every command step in this suite already runs through, which is why it
# belongs here rather than in a unit test.
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml
mkdir -p .spoolway/hooks

# Records its own environment beside the task file it ran for, one file per
# event — every event, `fetch` included, though nothing here calls `issue
# show` to exercise it — and always passes: this half is about what reaches
# the hook, not about a failure.
cat > .spoolway/hooks/record.sh <<'EOF'
#!/bin/sh
# Handles every event the same way, queued/blocked/paused/done/open/fetch
# alike — there is no branch here to miss.
env | sort > "$SPOOLWAY_TASK_FILE.env.$SPOOLWAY_EVENT"
exit 0
EOF
chmod +x .spoolway/hooks/record.sh

must "the hook is named" "$SPOOLWAY" config set issue_tracking.hook record.sh
must "and a project key" "$SPOOLWAY" config set issue_tracking.project_key acme/app

# `doctor` reads the same table straight off a real config.toml on a real
# checkout, which `doctor()` itself does not decouple from — it also pulls
# the branch and the task graph — so this is the only place a unit test
# cannot already stand in for it.
silent_about "doctor is quiet about a fully-configured issue_tracking" \
  "issue_tracking" "$SPOOLWAY" doctor

must "project_key is cleared to make the one half-set case" \
  "$SPOOLWAY" config set issue_tracking.project_key ""
says "doctor names both keys and the two ways out" \
  "[issue_tracking] names \`record.sh\` but project_key is empty" \
  "$SPOOLWAY" doctor
must "project_key is restored" \
  "$SPOOLWAY" config set issue_tracking.project_key acme/app

must "hook is set to a path rather than a bare name" \
  "$SPOOLWAY" config set issue_tracking.hook "../record.sh"
says "doctor refuses a hook name that is not a bare filename" \
  "which is not a bare filename" \
  "$SPOOLWAY" doctor
must "hook is restored to the bare name" \
  "$SPOOLWAY" config set issue_tracking.hook record.sh

# `handover` is `spoolway stack`, and the second task of a group is the second
# pull request of a stack — one `gh api` call that needs a remote reading as
# `github.com` and a `gh` that tracks stack membership. This suite's forge is
# a bare repo on disk and its `gh` double tracks no stacks at all, so without
# the three lines below `tracked-b` blocks at `handover` and never reaches
# `done`, taking the `SPOOLWAY_GROUP_LAST` assertions with it. Both facts are
# deliberate elsewhere — `suites/stacking.sh` asserts that exact refusal as
# its own subject — so this is scoped to this block and undone after it.
ORIGIN="$FORGE/origin.git"
TRACKED_URL="https://github.com/e2e/spoolway-tracked.git"
must "origin addressed as github, so the second of the pair can stack" \
  git -C "$LIVE/proj" remote set-url origin "$TRACKED_URL"
must "and redirected straight back to the same bare forge" \
  git -C "$LIVE/proj" config "url.$ORIGIN.insteadOf" "$TRACKED_URL"
export SPOOLWAY_GH="$HERE/../gh-stub.sh"
export GH_STUB_PRS="$LIVE/tracked-prs"
export GH_STUB_URL="file://$ORIGIN"
# The dispatcher inherited its environment when it started, before any of
# that existed — restarted, so the `handover` it runs sees all three.
dispatcher_restart

# A dependent pair rather than two independent tasks: `tracked-b` cannot even
# start until `tracked-a` has archived, which is what keeps `SPOOLWAY_GROUP_LAST`
# deterministic below — the two can never reach `done` in the same pass, so
# `tracked-a`'s own hook always sees `tracked-b` still open.
task_doc "$LIVE/tracked-a.md" tracked-a "$BODY" "group: tracked-pair" \
  "touches: [notes/tracked-a.md]"
task_doc "$LIVE/tracked-b.md" tracked-b "$BODY" "group: tracked-pair" \
  "touches: [notes/tracked-b.md]" "depends_on: [tracked-a]"
must "the first of a dependent pair queues" "$SPOOLWAY" queue add --from "$LIVE/tracked-a.md"
must "the second, depending on it, queues too" "$SPOOLWAY" queue add --from "$LIVE/tracked-b.md"

if drive tracked-a gone 60 && drive tracked-b gone 60; then
  ok "both tasks of the dependent pair reach done"
else
  bad "both tasks of the dependent pair reach done (at \`$(stage_of tracked-a)\`/\`$(stage_of tracked-b)\`)"
fi

ENV_A_QUEUED="$SPOOLWAY_PROJECT_HOME/queue/tracked-a.md.env.queued"
has "the queued hook ran with the task's own identity" "SPOOLWAY_TASK=tracked-a" "$ENV_A_QUEUED"
has "the event it fired for" "SPOOLWAY_EVENT=queued" "$ENV_A_QUEUED"
has "the project key from config" "SPOOLWAY_PROJECT_KEY=acme/app" "$ENV_A_QUEUED"
has "the task's own group" "SPOOLWAY_GROUP=tracked-pair" "$ENV_A_QUEUED"
has "the task file's own absolute path" "SPOOLWAY_TASK_FILE=" "$ENV_A_QUEUED"

ENV_A_DONE="$SPOOLWAY_PROJECT_HOME/queue/tracked-a.md.env.done"
ENV_B_DONE="$SPOOLWAY_PROJECT_HOME/queue/tracked-b.md.env.done"
silent_about "the first of the pair's done event is not the group's last" \
  "SPOOLWAY_GROUP_LAST" cat "$ENV_A_DONE"
has "the second's done event is — it is the group's last open task" \
  "SPOOLWAY_GROUP_LAST=1" "$ENV_B_DONE"

# Undone: everything past here is meant to see the same not-github forge the
# rest of this suite was written against.
must "origin is the bare forge again" \
  git -C "$LIVE/proj" remote set-url origin "$ORIGIN"
must "and the redirect is dropped with it" \
  git -C "$LIVE/proj" config --unset "url.$ORIGIN.insteadOf"
unset SPOOLWAY_GH
dispatcher_restart

# --------------------------------------------------------------- open hook
# `queue add` calls a fifth event, `open`, once per document before anything
# is queued at all: the two ids it answers with land in `epic:`/`ticket:`,
# right in the document's own frontmatter, in dependency order so a
# dependent's own call already has its parent's ticket id to hand over in
# `SPOOLWAY_DEPENDS_TICKETS`. A document already naming a `ticket:` is
# skipped outright — reported `kept`, never opened twice — which is what
# lets a batch a hook failed partway through resume on the next `queue add`
# rather than open a second set.
cat > .spoolway/hooks/open.sh <<'EOF'
#!/bin/sh
[ "$SPOOLWAY_EVENT" = open ] || exit 0
dir=$(dirname "$SPOOLWAY_OUT")
counter="$dir/open-counter"
next() {
  n=$(cat "$counter" 2>/dev/null || echo 40)
  n=$((n + 1))
  echo "$n" >"$counter"
  echo "$n"
}
epic=$SPOOLWAY_EPIC
if [ -z "$epic" ] && [ "$SPOOLWAY_GROUP_SIZE" -gt 1 ]; then
  epic="acme/app#$(next)"
fi
if [ "$SPOOLWAY_TASK" = "opened-fails" ]; then
  exit 7
fi
ticket="acme/app#$(next)"
{ echo "epic=$epic"; echo "ticket=$ticket"; } >"$SPOOLWAY_OUT"
echo "$SPOOLWAY_DEPENDS_TICKETS" >"$dir/depends.$SPOOLWAY_TASK"
EOF
chmod +x .spoolway/hooks/open.sh
must "the hook is switched to one that opens tickets" \
  "$SPOOLWAY" config set issue_tracking.hook open.sh

task_doc "$LIVE/opened-a.md" opened-a "$BODY" "group: opened-pair" \
  "touches: [notes/opened-a.md]"
task_doc "$LIVE/opened-b.md" opened-b "$BODY" "group: opened-pair" \
  "touches: [notes/opened-b.md]" "depends_on: [opened-a]"
must "a dependent pair queues in one call, opening a ticket for each" \
  "$SPOOLWAY" queue add --from "$LIVE/opened-a.md" --from "$LIVE/opened-b.md"

OPENED_A="$SPOOLWAY_PROJECT_HOME/queue/opened-a.md"
OPENED_B="$SPOOLWAY_PROJECT_HOME/queue/opened-b.md"
has "the group's epic landed in the first document" "epic: acme/app#" "$OPENED_A"
has "the first document's own ticket" "ticket: acme/app#" "$OPENED_A"
has "the same epic landed in the dependent's document" \
  "$(grep '^epic:' "$OPENED_A")" "$OPENED_B"
has "the dependent's own, different ticket" "ticket: acme/app#" "$OPENED_B"
has "the dependent's call carried its parent's ticket id" \
  "$(grep '^ticket:' "$OPENED_A" | awk '{print $2}')" \
  "$SPOOLWAY_PROJECT_HOME/tracking/depends.opened-b"

# A batch of two, the second of which the hook above refuses by name: nothing
# is queued, but the first document's own ticket was already written back
# into it in place, in `$LIVE` — not the queue — so a second `queue add` over
# the same two documents resumes rather than opening a second set.
task_doc "$LIVE/opened-ok.md" opened-ok "$BODY" "group: opened-fail-batch" \
  "touches: [notes/opened-ok.md]"
task_doc "$LIVE/opened-fails.md" opened-fails "$BODY" "group: opened-fail-batch" \
  "touches: [notes/opened-fails.md]"
refuses "a mid-batch hook failure queues nothing" "opened-fails" \
  "$SPOOLWAY" queue add --from "$LIVE/opened-ok.md" --from "$LIVE/opened-fails.md"
if [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/opened-ok.md" ] \
  && [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/opened-fails.md" ]; then
  ok "neither document of the failed batch was queued"
else
  bad "neither document of the failed batch was queued"
fi
has "the one that succeeded had its ticket written back into the pending file" \
  "ticket: acme/app#" "$LIVE/opened-ok.md"

# The hook no longer refuses anything: the second run over the same two
# documents queues both, and the first is `kept` rather than opened again —
# proven by its ticket id being unchanged from what the first run secured.
FIRST_TICKET=$(grep '^ticket:' "$LIVE/opened-ok.md" | awk '{print $2}')
sed -i '/SPOOLWAY_TASK" = "opened-fails"/,+2d' .spoolway/hooks/open.sh
must "the second run over the same batch queues both" \
  "$SPOOLWAY" queue add --from "$LIVE/opened-ok.md" --from "$LIVE/opened-fails.md"
has "the resumed run kept the first ticket rather than opening a new one" \
  "ticket: $FIRST_TICKET" "$SPOOLWAY_PROJECT_HOME/queue/opened-ok.md"

# --------------------------------------------- issue_tracking.key_in_names
# With the flag on and the hook answering a `slug=`, `queue add` writes the
# prefixed `group:` and `branch:` into each queued document — proven here
# through the real binary and a real hook. The worktree directory follows the
# branch slug too, but that is a dispatched checkout this block never cuts;
# `src/mux.rs`'s own tests cover the directory name. The hook also answers a
# `url=`, which is stored on the task and validated but shown nowhere yet.
cat > .spoolway/hooks/open-key.sh <<'EOF'
#!/bin/sh
[ "$SPOOLWAY_EVENT" = open ] || exit 0
dir=$(dirname "$SPOOLWAY_OUT")
counter="$dir/open-key-counter"
next() {
  n=$(cat "$counter" 2>/dev/null || echo 90)
  n=$((n + 1)); echo "$n" >"$counter"; echo "$n"
}
epic=$SPOOLWAY_EPIC
if [ -z "$epic" ] && [ "$SPOOLWAY_GROUP_SIZE" -gt 1 ]; then
  epic="acme/app#$(next)"
fi
ticket="acme/app#$(next)"
key=${epic:-$ticket}
num=${key##*#}
{
  echo "epic=$epic"; echo "ticket=$ticket"
  echo "slug=gh-$num"
  echo "url=https://github.com/acme/app/issues/$num"
} >"$SPOOLWAY_OUT"
EOF
chmod +x .spoolway/hooks/open-key.sh
must "the hook is switched to one that also answers a slug and a url" \
  "$SPOOLWAY" config set issue_tracking.hook open-key.sh
must "key_in_names is turned on" \
  "$SPOOLWAY" config set issue_tracking.key_in_names true

task_doc "$LIVE/keyed-a.md" keyed-a "$BODY" "group: keyed-rework" \
  "touches: [notes/keyed-a.md]"
task_doc "$LIVE/keyed-b.md" keyed-b "$BODY" "group: keyed-rework" \
  "touches: [notes/keyed-b.md]" "depends_on: [keyed-a]"
says "queue add names the prefix it applied" \
  "names prefixed" \
  "$SPOOLWAY" queue add --from "$LIVE/keyed-a.md" --from "$LIVE/keyed-b.md"

KEYED_A="$SPOOLWAY_PROJECT_HOME/queue/keyed-a.md"
KEYED_B="$SPOOLWAY_PROJECT_HOME/queue/keyed-b.md"
has "the group carries the slug prefix" "group: gh-" "$KEYED_A"
has "the group keeps its original name after the prefix" "keyed-rework" "$KEYED_A"
has "the branch carries the slug prefix and ends in the task id" \
  "branch: task/gh-" "$KEYED_A"
has "the branch ends in the task id" "-keyed-a" "$KEYED_A"
has "the slug is stored on the task" "slug: gh-" "$KEYED_A"
has "the issue url is stored on the task" \
  "url: https://github.com/acme/app/issues/" "$KEYED_A"
has "the dependent of the same group takes the same prefix" \
  "branch: task/gh-" "$KEYED_B"

must "key_in_names is turned back off" \
  "$SPOOLWAY" config set issue_tracking.key_in_names false
must "the hook is restored to the plain one" \
  "$SPOOLWAY" config set issue_tracking.hook open.sh

# ------------------------------------------------- on_fail = "pause"
# A hook that always fails, on each of the four events by hand: `pause` holds
# a task on `queued` and out of the archive on `done`, and only records the
# failure on `blocked` and `paused` — both already stopped for a person, so
# nothing about pausing them again would mean anything.
#
# `open` is the one event it lets through, and the exemption is the point:
# a failing `open` refuses the whole `queue add` outright — the block right
# above this one is what covers that — so a hook failing there too would
# leave nothing queued to carry the four events this block is about.
cat > .spoolway/hooks/fail.sh <<'EOF'
#!/bin/sh
[ "$SPOOLWAY_EVENT" = open ] && exit 0
exit 1
EOF
chmod +x .spoolway/hooks/fail.sh
must "the hook now always fails" "$SPOOLWAY" config set issue_tracking.hook fail.sh
must "and on_fail pauses the task" "$SPOOLWAY" config set issue_tracking.on_fail pause

task_doc "$LIVE/hook-queued.md" hook-queued "$BODY" "group: live" \
  "touches: [notes/hook-queued.md]"
must "a task queues under an always-failing hook" \
  "$SPOOLWAY" queue add --from "$LIVE/hook-queued.md"

if drive hook-queued paused 30; then
  ok "a failing queued hook under on_fail=pause lands the task on paused"
else
  bad "a failing queued hook under on_fail=pause lands the task on paused \
(at \`$(stage_of hook-queued)\`)"
fi

# Placed by hand at the other three stages, the same way flow.sh's own
# hand-blocked scenario proves a road through `blocked` without spending a
# real lane on it — `blocked`, `paused` and `done` are reached by a person or
# a pipeline, never by `queue add`.
{
  echo "---"; echo "id: hook-blocked"; echo "title: hook-blocked, done"
  echo "stage: blocked"; echo "blocked_from: implement"; echo "group: live"
  echo "touches: [notes/hook-blocked.md]"; echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/hook-blocked.md"
{
  echo "---"; echo "id: hook-paused"; echo "title: hook-paused, done"
  echo "stage: paused"; echo "paused_at: implement"; echo "group: live"
  echo "touches: [notes/hook-paused.md]"; echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/hook-paused.md"
{
  echo "---"; echo "id: hook-done"; echo "title: hook-done, done"
  echo "stage: done"; echo "group: live"
  echo "touches: [notes/hook-done.md]"; echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/hook-done.md"

dispatcher_start
TRACKING="$SPOOLWAY_PROJECT_HOME/tracking"
for _ in $(seq 1 150); do
  [ -f "$TRACKING/hook-blocked · blocked.exit" ] \
    && [ -f "$TRACKING/hook-paused · paused.exit" ] \
    && [ -f "$TRACKING/hook-done · done.failed" ] \
    && break
  sleep 0.2
done

has "the blocked event's hook ran and failed" "1" "$TRACKING/hook-blocked · blocked.exit"
has "the paused event's hook ran and failed" "1" "$TRACKING/hook-paused · paused.exit"
# `done` is the one event a failure is retried on, and `retry_if_failed`
# forgets the run — `.exit` file and all — on every pass that finds it still
# failing. The `.failed` marker it leaves first is the evidence that survives;
# see `failure_count` in src/tracking.rs, which reads both for the same reason.
works "the done event's hook ran and failed" \
  test -f "$TRACKING/hook-done · done.failed"

if [ "$(stage_of hook-blocked)" = blocked ]; then
  ok "a failing hook on blocked only records the failure"
else
  bad "a failing hook on blocked only records the failure (at \`$(stage_of hook-blocked)\`)"
fi
if [ "$(stage_of hook-paused)" = paused ]; then
  ok "a failing hook on paused only records the failure"
else
  bad "a failing hook on paused only records the failure (at \`$(stage_of hook-paused)\`)"
fi
if [ -f "$SPOOLWAY_PROJECT_HOME/queue/hook-done.md" ] && [ "$(stage_of hook-done)" = done ]; then
  ok "a failing done hook under on_fail=pause holds the task out of the archive"
else
  bad "a failing done hook under on_fail=pause holds the task out of the archive \
(queue file present: $([ -f "$SPOOLWAY_PROJECT_HOME/queue/hook-done.md" ] && echo yes || echo no), \
stage: $(stage_of hook-done))"
fi

# ----------------------------------------------------------- github.sh, real
# The shipped script itself, not a hand-written stand-in — `configure_project`
# already ran `spoolway init` with no `--tracker`, which writes
# `.spoolway/hooks/github.sh` all the same (see the tracker scaffolding
# above), so this project already has the real file on disk. Pointed at a
# `gh` double on `PATH` rather than at a real repository — see
# `scripts/e2e/gh-stub.sh`'s own header for why that double is shared with
# `suites/stack.sh`.
GH_STUBBIN="$LIVE/gh-stub-bin"
mkdir -p "$GH_STUBBIN"
install -m 755 "$HERE/../gh-stub.sh" "$GH_STUBBIN/gh"
export GH_STUB_PRS="$LIVE/gh-prs"
export GH_STUB_ISSUES="$LIVE/gh-issues"
export GH_STUB_URL="file://$LIVE/gh-forge"
PATH="$GH_STUBBIN:$PATH"

must "the hook is switched to the real github.sh" \
  "$SPOOLWAY" config set issue_tracking.hook github.sh
must "and a project key" "$SPOOLWAY" config set issue_tracking.project_key acme/app

# After both, not before: the dispatcher reads the config once at startup and
# the block above this one left `fail.sh` in it, so a restart any earlier
# would run every hook here as the always-failing one — silently, since that
# is exactly what `fail.sh` does. The restart also hands it the `PATH` the
# stub was just added to, which it could not have inherited when it started.
dispatcher_restart

# A group of one, so the script's own `epic` branch never fires — this is
# about the ticket half, which every group size takes.
task_doc "$LIVE/github-open-check.md" github-open-check "$BODY" \
  "group: github-single" "touches: [notes/github-open-check.md]"
must "queuing it calls the real hook's open branch" \
  "$SPOOLWAY" queue add --from "$LIVE/github-open-check.md"

TICKET=$(grep '^ticket:' "$SPOOLWAY_PROJECT_HOME/queue/github-open-check.md" | awk '{print $2}')
if [ -n "$TICKET" ]; then
  ok "github.sh answered a ticket url on open"
else
  bad "github.sh answered a ticket url on open"
fi
ISSUE_NUM=${TICKET##*/}
has "the stub's issue was created against the configured project" \
  "repo=acme/app" "$GH_STUB_ISSUES/$ISSUE_NUM"
has "with the rendered ticket body, not the template's raw placeholders" \
  "Opened automatically for task \`github-open-check\`." \
  "$GH_STUB_ISSUES/$ISSUE_NUM.body"

# A group of two, so the script's `epic` branch fires as well — and with it the
# `gh api .../sub_issues` call that is the only way `gh` 2.97.0 can make one
# issue the child of another. The endpoint takes the issue's *numeric* id, not
# the node id `gh issue view --json id` hands back, so this is the case that
# catches that pair being swapped.
task_doc "$LIVE/github-pair-a.md" github-pair-a "$BODY" \
  "group: github-pair" "touches: [notes/github-pair-a.md]"
task_doc "$LIVE/github-pair-b.md" github-pair-b "$BODY" \
  "group: github-pair" "touches: [notes/github-pair-b.md]"
must "queuing a group of two calls the hook's epic branch too" \
  "$SPOOLWAY" queue add --from "$LIVE/github-pair-a.md" --from "$LIVE/github-pair-b.md"

PAIR_EPIC=$(grep '^epic:' "$SPOOLWAY_PROJECT_HOME/queue/github-pair-a.md" | awk '{print $2}')
PAIR_TICKET=$(grep '^ticket:' "$SPOOLWAY_PROJECT_HOME/queue/github-pair-a.md" | awk '{print $2}')
if [ -n "$PAIR_EPIC" ]; then
  ok "github.sh answered an epic url for a group of two"
else
  bad "github.sh answered an epic url for a group of two"
fi
has "the epic carries the group's name, not a task's" \
  "title=github-pair" "$GH_STUB_ISSUES/${PAIR_EPIC##*/}"
has "and the ticket was linked under it as a sub-issue, by numeric id" \
  "sub_issue_id=${PAIR_TICKET##*/}" "$GH_STUB_ISSUES/${PAIR_EPIC##*/}.sub_issues"

# Placed on `blocked` by hand, the same way `hook-blocked` above stands in for
# a pipeline actually reaching it — naming the ticket `open` already secured.
{
  echo "---"; echo "id: github-blocked"; echo "title: github-blocked, done"
  echo "stage: blocked"; echo "blocked_from: implement"
  echo "group: github-single"
  echo "ticket: $TICKET"
  echo "touches: [notes/github-blocked.md]"; echo "---"; cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/github-blocked.md"

dispatcher_start
# Waits for the comment this task's own hook posts, not merely for a file at
# that name. `github-open-check` holds the same ticket, so its own blocked or
# paused comment can land on this issue first and be overwritten a moment
# later; polling on the content asserts against the right one whichever
# arrives first.
for _ in $(seq 1 150); do
  grep -q "id: github-blocked" "$GH_STUB_ISSUES/$ISSUE_NUM.comment" 2>/dev/null && break
  sleep 0.2
done
has "the blocked event's comment carries the task file" \
  "id: github-blocked" "$GH_STUB_ISSUES/$ISSUE_NUM.comment"
has "task file heading and all, inside the collapsed block" \
  "<details><summary>Task file</summary>" "$GH_STUB_ISSUES/$ISSUE_NUM.comment"

# --------------------------------------------------------------- fetch, real
# A person's own issue, filed on the tracker before spoolway ever touched
# it — seeded straight into the stub's flat files, the shape its own header
# describes, rather than created through `gh issue create` the way every
# issue above this line was. `spoolway issue show` never writes anything, so
# there is nothing to queue here at all.
FETCH_NUM=501
{ echo "repo=acme/app"; echo "title=Rework the session store"; } >"$GH_STUB_ISSUES/$FETCH_NUM"
printf 'Sessions are keyed by a random string with no expiry.' \
  >"$GH_STUB_ISSUES/$FETCH_NUM.body"
echo '[{"name":"area:auth"},{"name":"size:l"}]' >"$GH_STUB_ISSUES/$FETCH_NUM.labels.json"
echo '[{"author":{"login":"rk"},"body":"Only reproduces on mobile Safari."}]' \
  >"$GH_STUB_ISSUES/$FETCH_NUM.comments.json"

FETCH_JSON=$("$SPOOLWAY" issue show "$FETCH_NUM")
if jq -e --arg n "$FETCH_NUM" '
     .ref == $n and .title == "Rework the session store" and .state == "open" and
     (.labels | index("area:auth")) and (.comments[0].author == "rk")
   ' <<<"$FETCH_JSON" >/dev/null 2>&1
then
  ok "spoolway issue show reads the real hook's fetch branch"
else
  bad "spoolway issue show reads the real hook's fetch branch ($FETCH_JSON)"
fi

# ------------------------------------------------------- open hangs under it
# Queuing a task whose own `source:` names that same filed issue: the
# `open` branch's `hang_under` has to recognise it and link the new ticket
# as a GitHub sub-issue underneath — and, separately, leave a plan-page
# `source:` (every other task in this suite) exactly alone.
task_doc "$LIVE/github-hang-under.md" github-hang-under "$BODY" \
  "group: github-hang-single" "touches: [notes/github-hang-under.md]" \
  "source: $GH_STUB_URL/acme/app/issues/$FETCH_NUM"
must "queuing a task whose source names a filed issue calls the open branch" \
  "$SPOOLWAY" queue add --from "$LIVE/github-hang-under.md"

HANG_TICKET=$(grep '^ticket:' "$SPOOLWAY_PROJECT_HOME/queue/github-hang-under.md" | awk '{print $2}')
has "the filed issue now lists the new ticket as a GitHub sub-issue" \
  "sub_issue_id=${HANG_TICKET##*/}" "$GH_STUB_ISSUES/$FETCH_NUM.sub_issues"
works "and github-open-check's own ticket, queued with no source: at all, never grew one" \
  test ! -e "$GH_STUB_ISSUES/${TICKET##*/}.sub_issues"

# --------------------------------------------------------------- retention
# `retain` sweeps byproducts and never live state, whatever its age.
# `system-prompts/` and `queue/` are dated the same way here, so what makes
# the difference is only which directory holds the entry.
must "retention is set to an age touch can force in a moment" \
  "$SPOOLWAY" config set retention.days 1

# `system-prompts/` is the composed-prompt directory, one file per lane.
# Not `.spoolway/prompts/` in the checkout, which holds the project's tracked
# prompt templates and is never swept.
mkdir -p "$SPOOLWAY_PROJECT_HOME/system-prompts"
OLD_PROMPT="$SPOOLWAY_PROJECT_HOME/system-prompts/aged · step.md"
echo "a composed prompt nobody will read again" > "$OLD_PROMPT"
touch -d "2 days ago" "$OLD_PROMPT"

# A task file placed by hand rather than through `queue add`, the same way
# `hook-blocked` above stands in for a task already sitting in `queue/` —
# the sweep has to leave it alone regardless of how it got there.
OLD_QUEUED="$SPOOLWAY_PROJECT_HOME/queue/aged-queued-task.md"
{
  echo "---"; echo "id: aged-queued-task"; echo "title: aged-queued-task, done"
  echo "stage: queued"; echo "group: live"
  echo "touches: [notes/aged-queued-task.md]"; echo "---"; cat "$BODY"
} > "$OLD_QUEUED"
touch -d "2 days ago" "$OLD_QUEUED"

# No dedicated command sweeps anything — an ordinary one is what triggers
# the once-per-process pass.
must "an ordinary command runs the retention sweep" "$SPOOLWAY" group list

works "the aged prompt went" test ! -e "$OLD_PROMPT"
works "the aged queue entry did not — queue/ is never swept, whatever its age" \
  test -f "$OLD_QUEUED"

must "retention restored to its default" "$SPOOLWAY" config set retention.days 30
rm -f "$OLD_QUEUED"

finish
