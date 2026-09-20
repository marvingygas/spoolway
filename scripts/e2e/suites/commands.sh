#!/usr/bin/env bash
# CLI behaviour that needs a real process, a real project or a real forge,
# and belongs to no domain of its own: scaffolding (`init`, `sync`), the
# contracts (`task contract`, `pipeline contract`, `prompt contract`), the
# queue screen read off a real pipe, the archive's own rows, `config`'s
# checkout/project asymmetry, the overrides layer resolved through a linked
# worktree, housekeeping's retention sweep, and the confirm-dialog gate driven
# over a real pty.
#
# This suite used to also assert the `agent list`/`agent verify` output, the
# transcript an ambient session is read from, and every refusal `pipeline
# check` makes about a `run:` step. None of those start a process, so all
# three were unit tests written in bash — see `src/agent.rs`, `src/usage.rs`
# and `src/pipeline.rs`, which cover them against the code rather than
# against a fixture.
#
# `spoolway config`'s asymmetry between reading (the checkout in front of the
# command) and writing (the project, always — refused elsewhere) has no
# `covers:` tag of its own — the map `coverage.sh` builds only enumerates
# `config.toml` keys and pipeline step keys, and this is neither; it is CLI
# behaviour the map has no row for.
#
# Two of the three domains this suite used to hold, both grown into suites of
# their own (gh-253, once this file passed 2500 lines and 159 seconds): a
# `run:` step's own mechanics are `command-steps.sh`, and `[issue_tracking]`'s
# hook is `issue-tracking.sh`. Every `# covers:` claim either of them proves
# moved there whole; the two left below were never theirs.
#
# covers: housekeeping.retention_days — an entry past the age is swept from a byproduct directory, and never from queue/, however old
# covers: housekeeping.price_max_age_days — doctor notes only a table older than the configured limit; 0 is covered by the unit boundary test
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

# -------------------------------------------------------- embedded releases
# This is intentionally before any fixture creates a repository: the history
# belongs to the installed binary, and requiring project discovery here would
# make the recovery command least useful immediately after installation.
# The bare command prints the release this binary is, so the expectation is
# read off the binary rather than written down: a version bump used to fail
# these two checks until somebody noticed the suite still said 0.1.0.
# Matched on each release's own URL rather than its heading: a heading is now
# the bare version (see `parse_release` in `src/release_notes.rs`), and a bare
# version is not unique to its own section — 0.2.0's migration notes mention
# 0.1.0 by name, so matching that alone would pass even if the range below
# selected nothing.
CURRENT=$("$SPOOLWAY" --version | awk '{print $2}')
says "whats-new works outside a project" "releases/tag/v$CURRENT" \
  env -C "$LIVE" "$SPOOLWAY" whats-new
says "a release range selects the embedded release" "releases/tag/v0.1.0" \
  env -C "$LIVE" "$SPOOLWAY" whats-new --since 0.0.0
says "an empty release range is explicit" "No releases follow $CURRENT." \
  env -C "$LIVE" "$SPOOLWAY" whats-new --since "$CURRENT"
refuses "a malformed release range is refused" "canonical numeric components" \
  env -C "$LIVE" "$SPOOLWAY" whats-new --since yesterday

# `spoolway update` installs the binary and nothing else, so it is the other
# command — beside `whats-new` above — that must work with no project ever
# discovered: `$LIVE` here is not a git repository yet, let alone one carrying
# `.spoolway/`. `--help` rather than a real run, since a real run would shell
# out to npm.
works "update --help works outside a project" \
  env -C "$LIVE" "$SPOOLWAY" update --help

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
  "$SPOOLWAY" config set housekeeping.price_max_age_days 30
says "doctor notes a refreshed table past the age limit" \
  'past the 30 in `housekeeping.price_max_age_days`' "$SPOOLWAY" doctor

jq --arg today "$(date -u +%F)" '.generated = $today' "$PRICE_TABLE" >"$PRICE_TABLE.tmp"
mv "$PRICE_TABLE.tmp" "$PRICE_TABLE"
silent_about "doctor stays quiet for a refreshed table inside the age limit" \
  'past the 30 in `housekeeping.price_max_age_days`' "$SPOOLWAY" doctor

BODY="$LIVE/body.md"
task_body "$BODY"

# --------------------------------------------------------------- scaffolding
# `init` asks three questions at a terminal and none anywhere else, which is
# exactly the distinction a shell suite is the right place to hold: everything
# below runs with no tty, so an `init` that ever read stdin here would hang the
# suite rather than fail it. In its own directory — this is a project being
# created, and the suite's own is already one.
INITDIR="$LIVE/init"
mkdir -p "$INITDIR/asked" && (cd "$INITDIR/asked" && git init -q -b main .)
works "init scaffolds a project with the answers given as flags" \
  env -C "$INITDIR/asked" "$SPOOLWAY" init \
  --provider codex --tracker none

has "the selected provider names the fresh profile" '[agents.codex]' \
  "$INITDIR/asked/.spoolway/config.toml"
has "every model choice is left explicit and blank" 'model: ""' \
  "$INITDIR/asked/.spoolway/pipelines/default.yml"
works "and the provider's skills are installed, not suggested" \
  test -f "$INITDIR/asked/.agents/skills/spoolway-plan/SKILL.md"
works "including the task-cutting skill spoolway-plan's step 7 invokes" \
  test -f "$INITDIR/asked/.agents/skills/spoolway-tasks/SKILL.md"
works "and spoolway-calibrate lands beside it" \
  test -f "$INITDIR/asked/.agents/skills/spoolway-calibrate/SKILL.md"
works "in that provider's directory alone" \
  test ! -e "$INITDIR/asked/.claude"

# The path every script and CI runner takes. Nothing is asked, so the shipped
# defaults stand, including the explicit model and effort blanks a person must
# fill before dispatching.
mkdir -p "$INITDIR/unasked" && (cd "$INITDIR/unasked" && git init -q -b main .)
works "init with no terminal asks nothing and takes the defaults" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" init
has "so the model choice is visibly blank" 'model: ""' \
  "$INITDIR/unasked/.spoolway/pipelines/default.yml"

# Even an authentic-looking handover value must not leak the digest into a
# captured stdout stream. The new binary appends it only when stdout is a TTY;
# unit coverage holds the positive side without making this suite depend on a
# platform-specific pseudo-terminal utility.
HANDOVER_OUT=$(env SPOOLWAY_UPGRADED=0.0.0 \
  "$SPOOLWAY" -C "$INITDIR/unasked" update 2>&1)
HANDOVER_STATUS=$?
works "the captured post-handover update still succeeds" \
  test "$HANDOVER_STATUS" -eq 0
silent_about "post-handover notes do not cross the non-terminal output boundary" \
  "Updated spoolway" printf '%s\n' "$HANDOVER_OUT"
works "and claude's skills are what a run with nobody to ask installs" \
  test -f "$INITDIR/unasked/.claude/skills/spoolway-plan/SKILL.md"
works "spoolway-tasks lands beside it" \
  test -f "$INITDIR/unasked/.claude/skills/spoolway-tasks/SKILL.md"
works "and spoolway-calibrate lands too" \
  test -f "$INITDIR/unasked/.claude/skills/spoolway-calibrate/SKILL.md"

refuses "the retired agent answer is no longer accepted" \
  "unexpected argument '--agent'" env -C "$INITDIR/unasked" "$SPOOLWAY" init --agent gemini

# Run again for a second provider: the skills land, and the config the project
# has been running on is not rewritten around it.
works "a second init installs another provider's skills" \
  env -C "$INITDIR/asked" "$SPOOLWAY" init --provider claude
works "without disturbing the first" \
  test -f "$INITDIR/asked/.agents/skills/spoolway-plan/SKILL.md"
works "and spoolway-tasks is among the second provider's skills too" \
  test -f "$INITDIR/asked/.claude/skills/spoolway-tasks/SKILL.md"
works "spoolway-calibrate as well" \
  test -f "$INITDIR/asked/.claude/skills/spoolway-calibrate/SKILL.md"
has "and without rewriting the Codex config" '[agents.codex]' \
  "$INITDIR/asked/.spoolway/config.toml"
says "Pi remains available through standalone install for established projects" \
  "only once the project is trusted" \
  env -C "$INITDIR/asked" "$SPOOLWAY" install pi

# ------------------------------------------------- restoring missing pipelines
# What `spoolway-tasks` does now that it no longer offers to generate a
# pipeline: a project whose `.spoolway/pipelines/` has gone missing is
# repaired by a plain re-run of `init`, and the skill runs it without
# asking. Only a real project on disk shows the whole of that claim — that
# the re-run writes back the pipelines it finds absent, reports every file
# the project already had as kept rather than rewriting it, and that
# `task contract` answers again on the very next call.
RESTORE="$INITDIR/restore"
mkdir -p "$RESTORE" && (cd "$RESTORE" && git init -q -b main .)
must "the project to repair scaffolds" env -C "$RESTORE" "$SPOOLWAY" init
printf '\n# a line this project wrote itself\n' >> "$RESTORE/.spoolway/config.toml"

rm -rf "$RESTORE/.spoolway/pipelines"
refuses "a project whose pipelines went missing says so rather than borrowing" \
  "no pipelines defined" env -C "$RESTORE" "$SPOOLWAY" task contract

RESTORE_OUT=$(env -C "$RESTORE" "$SPOOLWAY" init 2>&1)
if grep -qE '^[[:space:]]*wrote[[:space:]]+\.spoolway/pipelines/default\.yml' <<<"$RESTORE_OUT"; then
  ok "the re-run writes the shipped pipelines back"
else
  bad "the re-run writes the shipped pipelines back"
  sed 's/^/        /' <<<"$RESTORE_OUT"
fi
if grep -qE '^[[:space:]]*kept[[:space:]]+\.spoolway/config\.toml' <<<"$RESTORE_OUT"; then
  ok "and reports the config it found as kept, not written"
else
  bad "and reports the config it found as kept, not written"
  sed 's/^/        /' <<<"$RESTORE_OUT"
fi
works "both shipped pipelines are back" \
  test -f "$RESTORE/.spoolway/pipelines/default.yml" -a \
         -f "$RESTORE/.spoolway/pipelines/bugfix.yml"
has "the line this project wrote itself is still there, byte for byte" \
  "# a line this project wrote itself" "$RESTORE/.spoolway/config.toml"
works "and task contract answers again on the next call" \
  env -C "$RESTORE" "$SPOOLWAY" task contract

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
works "answering github also writes the workflow that closes a mirrored issue" \
  test -f "$INITDIR/github/.github/workflows/spoolway-issues.yml"
works "none never gets one" \
  test ! -e "$INITDIR/unasked/.github/workflows/spoolway-issues.yml"

# `none` — `unasked`'s own `init` above already took every default with
# nobody there to ask, which includes the tracker question defaulting to
# `none`: the table stays empty even though both scripts are on disk.
has "unasked left the hook empty" 'hook = ""' "$INITDIR/unasked/.spoolway/config.toml"
has "and the project key empty" 'project_key = ""' "$INITDIR/unasked/.spoolway/config.toml"
works "both hooks are written all the same" \
  test -f "$INITDIR/unasked/.spoolway/hooks/github.sh"
works "the jira one too" test -f "$INITDIR/unasked/.spoolway/hooks/jira.sh"

# `spoolway sync` never touches a hook `init` has already written — the
# same rule a prompt or a task skeleton already follows once a project has
# made a file its own.
HOOK="$INITDIR/github/.spoolway/hooks/github.sh"
printf '#!/bin/sh\necho mine\n' > "$HOOK"
must "sync runs over the tracker project" env -C "$INITDIR/github" "$SPOOLWAY" sync
has "the hook this project edited is exactly as it left it" "echo mine" "$HOOK"

# ------------------------------------------------------------ lane-prompts.md
# The seven typed pane messages are spoolway's own now, with no project
# override left to resolve against them — `init` no longer seeds one, and a
# checkout carrying one from an older release is swept by `sync` rather
# than preserved. The sweep itself, against a real released tree, is
# `upgrade.sh`'s: what only a fresh `init` can show is that a project started
# today never gets the file in the first place.
LANE_PROMPTS="$INITDIR/unasked/.spoolway/templates/lane-prompts.md"
works "init no longer seeds lane-prompts.md" test ! -e "$LANE_PROMPTS"

# A project carrying the file from before this change — `--replace` no
# longer has anything to hand it back, since the file is not one spoolway
# ships any more.
printf 'a project'"'"'s own leftover.\n' > "$LANE_PROMPTS"
must "sync sweeps a leftover lane-prompts.md" env -C "$INITDIR/unasked" "$SPOOLWAY" sync
works "and it is gone" test ! -e "$LANE_PROMPTS"
refuses "so --replace has nothing left to hand back" \
  "not a file spoolway ships" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" sync --replace .spoolway/templates/lane-prompts.md

# ------------------------------------------------------- dispatch.interval
# `dispatch.interval` is retired hard: `DispatchConfig` denies unknown
# fields, so a config that still names it is a parse error rather than a
# quietly-dropped setting, everywhere except `sync`, which is the one place
# meant to bring such a file forward.
CONFIG="$INITDIR/unasked/.spoolway/config.toml"
sed -i '/^\[dispatch\]$/a interval = "10s"' "$CONFIG"
refuses "a config still naming dispatch.interval is refused" \
  "unknown field" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" config get dispatch.lane_quiet
must "sync runs over the project anyway" env -C "$INITDIR/unasked" "$SPOOLWAY" sync
lacks "dispatch.interval is gone from the rewritten config" "interval" "$CONFIG"
works "and the config is accepted again" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" config get dispatch.lane_quiet

# --------------------------------------------------------------- task contract
# `task contract` never touches `.spoolway/` in either mode — bare, it only
# ever reads pipelines already loaded in memory; `--from`, it runs the very
# validation `queue add --from` runs and stops short of the save. Both halves
# of that claim are worth a real process: a unit test cannot see whether a
# directory gained a file, only this can.
command -v jq >/dev/null || { echo "commands.sh needs jq" >&2; exit 2; }

# Stdout only, not `2>&1`: the confirm-dialog gate's own notice, when this
# checkout is behind, is a line on stderr ahead of the JSON — merging the
# two would hand `jq` that line as its first byte and fail every parse on a
# behind checkout, which is not what either check below is about.
CHECK_JSON=$("$SPOOLWAY" task contract 2>"$LIVE/task-contract.err")
if jq -e '.pipelines.default.id_budget' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "bare task contract prints the contract as parseable JSON"
else
  bad "bare task contract prints the contract as parseable JSON"
  echo "$CHECK_JSON" | sed 's/^/        /'
  sed 's/^/        /' "$LIVE/task-contract.err"
fi

# There is no project default any more — `dispatch.default_pipeline` is
# retired — so the contract advertises no `default` field at all, and
# `pipeline` is one of the required keys, not a courtesy the document may
# skip.
if jq -e '.default == null' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "the contract advertises no project default pipeline"
else
  bad "the contract advertises no project default pipeline"
  echo "$CHECK_JSON" | sed 's/^/        /'
fi
if jq -e '.keys.required | index("pipeline") != null' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "the contract requires pipeline:"
else
  bad "the contract requires pipeline:"
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

# ------------------------------------------------------ no pipelines/ at all
# A missing `.spoolway/pipelines/` used to be answered from the pipelines
# compiled into the binary; an empty one was already a hard `no pipelines
# defined` error. Both now reach that same error, naming the directory, so
# the broken state is visible where it happens rather than surfacing three
# commands later at dispatch on a `PROMPT.md` that was never written.
mv .spoolway/pipelines "$LIVE/pipelines.bak"
refuses "task contract with no pipelines/ reports the shared error" \
  ".spoolway/pipelines: no pipelines defined" "$SPOOLWAY" task contract

DOCTOR_OUT=$("$SPOOLWAY" doctor 2>&1)
if grep -qF "FAIL  pipelines load: in " <<<"$DOCTOR_OUT" \
  && grep -qF ".spoolway/pipelines: no pipelines defined" <<<"$DOCTOR_OUT"; then
  ok "doctor's own pipelines check fails the same way, naming the same directory"
else
  bad "doctor's own pipelines check fails the same way, naming the same directory"
  sed 's/^/        /' <<<"$DOCTOR_OUT"
fi
mv "$LIVE/pipelines.bak" .spoolway/pipelines

# ------------------------------------------------------- explicit pipeline
# `pipeline:` is required now — there is no `dispatch.default_pipeline` to
# route an omission through any more — so a document naming none is refused
# the same way one naming no `group:` already is, by `task contract --from`
# and by `queue add --from` alike.
NOPIPELINE="$LIVE/nopipeline.md"
{
  echo "---"
  echo "id: no-pipeline"
  echo "title: no-pipeline, done"
  echo "group: live"
  echo "---"
  cat "$BODY"
} > "$NOPIPELINE"

OUT=$("$SPOOLWAY" task contract --from "$NOPIPELINE" 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ]; then ok "task contract --from a document with no pipeline: exits non-zero"
else bad "task contract --from a document with no pipeline: exits non-zero"; fi
if grep -qF "pipeline:" <<<"$OUT"; then
  ok "and names the missing key"
else bad "and names the missing key"; sed 's/^/        /' <<<"$OUT"; fi

OUT=$("$SPOOLWAY" queue add --from "$NOPIPELINE" 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ]; then ok "queue add --from the same document exits non-zero"
else bad "queue add --from the same document exits non-zero"; fi
if [ ! -e "$SPOOLWAY_PROJECT_HOME/queue/no-pipeline.md" ]; then
  ok "and nothing was queued"
else bad "and nothing was queued"; fi

# An unknown pipeline is refused the same way, naming the document and the
# defined choices rather than the bare "no pipeline `x`" a `pipeline get`
# lookup would give.
NOSUCHPIPE="$LIVE/nosuchpipe.md"
{
  echo "---"
  echo "id: no-such-pipeline"
  echo "title: no-such-pipeline, done"
  echo "group: live"
  echo "pipeline: not-a-real-pipeline"
  echo "base: plan/live"
  echo "---"
  cat "$BODY"
} > "$NOSUCHPIPE"
OUT=$("$SPOOLWAY" task contract --from "$NOSUCHPIPE" 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ]; then ok "task contract --from a document naming an unknown pipeline exits non-zero"
else bad "task contract --from a document naming an unknown pipeline exits non-zero"; fi
if grep -qF "not-a-real-pipeline" <<<"$OUT"; then
  ok "and names the pipeline it could not find"
else bad "and names the pipeline it could not find"; sed 's/^/        /' <<<"$OUT"; fi

# ------------------------------------------------------------ explicit base
# A task's base is a value somebody chose — a document's own `base:` or a
# `queue add --base` covering the whole submission — never the branch this
# checkout happens to have out. A submission naming neither is refused by
# name, and writes nothing. A project of its own, never dispatched: these
# tasks would otherwise sit in the shared project's queue for the rest of
# this suite, competing with everything timing-sensitive that follows.
BASECHECK="$LIVE/basecheck"
mkdir -p "$BASECHECK" && (cd "$BASECHECK" && git init -q -b plan/x .)
must "a git identity for the explicit-base case" \
  env -C "$BASECHECK" git config user.email t@example.com
must "a git identity for the explicit-base case" \
  env -C "$BASECHECK" git config user.name t
must "a project scaffolded fresh for the explicit-base case" \
  env -C "$BASECHECK" "$SPOOLWAY" init --provider claude --tracker none
must "a seed commit, so a second branch has something to point at" \
  env -C "$BASECHECK" git commit -q --allow-empty -m seed
must "a second local branch --base could point at instead" \
  env -C "$BASECHECK" git branch other/base

NOBASE="$BASECHECK/nobase.md"
{
  echo "---"
  echo "id: nobase"
  echo "title: nobase, done"
  echo "group: live"
  echo "pipeline: default"
  echo "---"
  cat "$BODY"
} > "$NOBASE"

refuses "a submission with no base anywhere is refused" \
  'sets no `base:` and no --base' \
  env -C "$BASECHECK" "$SPOOLWAY" queue add --from "$NOBASE"
refuses "and nothing was queued for it" "" \
  env -C "$BASECHECK" "$SPOOLWAY" queue show nobase

works "--base covers a submission with no base of its own" \
  env -C "$BASECHECK" "$SPOOLWAY" queue add --from "$NOBASE" --base plan/x
says "and the task is cut from the flag's branch" "base: plan/x" \
  env -C "$BASECHECK" "$SPOOLWAY" queue show nobase

OWNBASE="$BASECHECK/ownbase.md"
{
  echo "---"
  echo "id: ownbase"
  echo "title: ownbase, done"
  echo "group: live"
  echo "base: plan/x"
  echo "pipeline: default"
  echo "---"
  cat "$BODY"
} > "$OWNBASE"
works "a document's own base wins over --base" \
  env -C "$BASECHECK" "$SPOOLWAY" queue add --from "$OWNBASE" --base other/base
says "and the task is cut from the document's own branch, not the flag's" \
  "base: plan/x" env -C "$BASECHECK" "$SPOOLWAY" queue show ownbase

# --------------------------------------------------------- pipeline contract
# `pipeline default` is gone — the format is now printed from the binary
# itself, in `pipeline contract`, rather than dumped from the shipped files.
refuses "pipeline default is no longer a subcommand" \
  "unrecognized subcommand" "$SPOOLWAY" pipeline default

# Stdout only — see the same note above `CHECK_JSON`.
PIPELINE_JSON=$("$SPOOLWAY" pipeline contract 2>"$LIVE/pipeline-contract.err")
if jq -e '(.keys.step | index("agent")) and (.keys.step | index("loop"))' \
    <<<"$PIPELINE_JSON" >/dev/null 2>&1; then
  ok "bare pipeline contract prints the step keys as parseable JSON"
else
  bad "bare pipeline contract prints the step keys as parseable JSON"
  echo "$PIPELINE_JSON" | sed 's/^/        /'
  sed 's/^/        /' "$LIVE/pipeline-contract.err"
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

# ------------------------------------------------------- pipeline check: project-owned only
# `pipeline check` used to also validate the two pipelines shipped inside the
# binary — `assets/pipelines/default.yml` and `bugfix.yml` — against this
# project's config, even for a project that dropped its own copy of one of
# them. A project that keeps only its own override, `default.yml`, and never
# wrote a `bugfix.yml` of its own, with the prompt only that embedded sample
# ever called for gone too, is what proves the leak is closed: only a real
# process, run against the built binary and its real embedded assets, can
# prove that.
PICHECK="$LIVE/picheck"
mkdir -p "$PICHECK" && (cd "$PICHECK" && git init -q -b main .)
must "a project scaffolded fresh for this case" \
  env -C "$PICHECK" "$SPOOLWAY" init --provider claude --tracker none
rm -f "$PICHECK/.spoolway/pipelines/bugfix.yml"
rm -rf "$PICHECK/.spoolway/prompts/reproducer"
must "filling in the three models its own pipeline needs" \
  sed -i 's/model: ""/model: fake-local/' "$PICHECK/.spoolway/pipelines/default.yml"

PICHECK_OUT="$LIVE/picheck.out"
if env -C "$PICHECK" "$SPOOLWAY" pipeline check >"$PICHECK_OUT" 2>&1; then
  ok "pipeline check passes on the project's own override alone, the shipped bugfix.yml and its reproducer prompt gone"
else
  bad "pipeline check passes on the project's own override alone, the shipped bugfix.yml and its reproducer prompt gone"
  sed 's/^/        /' "$PICHECK_OUT"
fi
has "and reports only the pipeline this project actually loaded" '["default"]' "$PICHECK_OUT"
lacks "with no mention of the bundled sample it dropped" "bugfix" "$PICHECK_OUT"

# ---------------------------------------------------------- prompt contract
# `prompt contract` gains a seventh section, on every call, whatever
# `--step` names — the prompt skeleton, not a paraphrase of it. There is no
# project default pipeline to fall back to any more, so the call now has to
# name one explicitly.
says "prompt contract prints the shape-to-write section" \
  "THE SHAPE TO WRITE" "$SPOOLWAY" prompt contract --pipeline default

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

# space selects the highlighted group, enter submits it and reaches the
# overview, `esc` declines it. The screen ends on its own the moment the
# pipe runs dry — see `queue_screen`'s own doc comment on why a pipe is read
# exactly as a terminal would be.
printf ' \r\x1b' | "$SPOOLWAY" queue >/dev/null 2>&1

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

# -------------------------------------------------- taking one back and sending it again
# `screen-two` carried back out of the queue by hand (it is the dependent of
# the pair, so nothing still queued waits on it), then the group sent through
# the screen a second time: `screen-one`, still queued, must be left exactly
# where it is rather than tripping the `stage:` refusal its own document
# carries, and the report has to name it rather than silently dropping it.
must "screen-two carried back out of the queue by hand" \
  "$SPOOLWAY" queue unqueue screen-two
works "its document is back in the pending directory" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/screen-two.md"
works "and gone from the queue" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/screen-two.md"

printf ' \r\x1b' | "$SPOOLWAY" queue >"$LIVE/screen-requeue.out" 2>&1

works "screen-two reaches the queue again" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-two.md"
works "and is gone from pending once more" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/pending/screen-two.md"
works "screen-one, already queued, is left exactly where it was" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-one.md"
# The report `enter` used to leave on screen, naming the sibling left alone,
# is gone — replaced by the overview, which lists the whole queue instead of
# one submission's own report. See the mockup on task `overview-and-gates`.
# Anchored on the table header rather than "screen-batch": the browsing
# pane's own group row already prints that name on every frame, long before
# `enter` ever reaches the overview, so a match on it alone would pass
# whether or not the overview drew at all.
has "the overview it reaches draws its own table header" \
  "TASK                PIPELINE    STEP      BASE" "$LIVE/screen-requeue.out"
lacks "and never a 'left alone' line for the sibling still sitting there" \
  "left alone" "$LIVE/screen-requeue.out"

# The other half of the same directory: `--from` pointed at it queues every
# `.md` document there, with no screen involved at all.
must "the group left behind, queued by naming the directory" \
  "$SPOOLWAY" queue add --from "$SPOOLWAY_PROJECT_HOME/pending"
works "reaches the queue the same way" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-other.md"
# The source lived in this project's own pending directory, so `queue add
# --from` clears it once the batch is written — the same "one inbox, whichever
# door" rule the screen already keeps, so a task queued from the command line
# is not left sitting in pending as well as in the queue.
works "and clears the document out of the pending directory" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/pending/screen-other.md"

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
if drive archived-row gone 180; then
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
  sed -i 's|^worktree_root = .*|worktree_root = "from-the-worktree"|' \
  "$WT/.spoolway/config.toml"
must "committing the worktree's own config" \
  git -C "$WT" add .spoolway/config.toml
must "committing the worktree's own config" \
  git -C "$WT" commit -qm "e2e: a config value only this worktree has"

says "config get in the worktree reads its own value" "from-the-worktree" \
  "$SPOOLWAY" -C "$WT" config get dispatch.worktree_root
silent_about "config get in the main checkout does not see it" "from-the-worktree" \
  "$SPOOLWAY" config get dispatch.worktree_root
says "config path in the worktree names its own file, not the project's" \
  "$WT/.spoolway/config.toml" "$SPOOLWAY" -C "$WT" config path

BEFORE_ROOT=$(cat .spoolway/config.toml)
BEFORE_WT=$(cat "$WT/.spoolway/config.toml")
OUT=$("$SPOOLWAY" -C "$WT" config set dispatch.worktree_root "should-not-land" 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ]; then ok "config set in the worktree exits non-zero"
else bad "config set in the worktree exits non-zero"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF "the dispatcher reads the project's config, not this worktree's." <<<"$OUT"; then
  ok "and says why"
else bad "and says why"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF -- "-C $PROJECT config set dispatch.worktree_root should-not-land" <<<"$OUT"; then
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

# --------------------------------------------------------- overrides layer
# `~/.spoolway/<project>/overrides/` is read by `Pipelines::load`,
# `Config::load` and `prompt::path_for` through the shared
# `overrides::dir_for`, which resolves whatever checkout it is handed back
# to the *main* checkout with a real `git` subprocess before keying it off
# `crate::mux::project_home` (see `src/overrides.rs`'s own doc). Every unit
# test below that call runs against a bare fixture directory with no git
# repository behind it, so all of them exercise only `dir_for`'s fallback
# branch ("no repo here, treat root as the main checkout already"). Nothing
# anywhere proves the git-subprocess branch itself resolves a *linked
# worktree* back to the same directory a command run from the main checkout
# reads — which is exactly what a lane merging overrides from inside its own
# worktree depends on. That takes a real worktree, so it is this suite's
# job and no unit test's; everything else about the merge (an unknown step
# id refused by name, `id:` refused, a broken graph still caught by
# `validate()`, the directory absent leaving load unchanged) is already
# proved in `src/pipeline.rs`'s own `with_override_fixture` tests.
BEFORE_OUT=$("$SPOOLWAY" pipeline show)
REVIEW_LINE_BEFORE=$(grep -E '^  review ' <<<"$BEFORE_OUT")
# The untouched key the patch must leave alone, read off the tracked file
# rather than named literally: `fixture.sh`'s `own_prompts` renames every
# shipped prompt to a suite-local one (`implementer` becomes `builder`), so a
# hard-coded name here asserts the fixture's naming, not the merge.
IMPLEMENT_PROMPT_BEFORE=$(grep -oE 'prompt=[^ ]+' <<<"$(grep -E '^  implement ' <<<"$BEFORE_OUT")")

OVERRIDE_MODEL="overridden-by-the-layer"
mkdir -p "$SPOOLWAY_PROJECT_HOME/overrides/pipelines"
cat > "$SPOOLWAY_PROJECT_HOME/overrides/pipelines/default.yml" <<YML
steps:
  implement:
    model: $OVERRIDE_MODEL
YML

OUT=$("$SPOOLWAY" pipeline show)
IMPLEMENT_LINE=$(grep -E '^  implement ' <<<"$OUT")
REVIEW_LINE=$(grep -E '^  review ' <<<"$OUT")
# The `-n` guard is load-bearing: an empty `IMPLEMENT_PROMPT_BEFORE` would
# make `grep -qF ""` match anything at all, and the second half of this
# assertion would pass while proving nothing.
if grep -qF "model=$OVERRIDE_MODEL" <<<"$IMPLEMENT_LINE" \
  && [ -n "$IMPLEMENT_PROMPT_BEFORE" ] \
  && grep -qF "$IMPLEMENT_PROMPT_BEFORE" <<<"$IMPLEMENT_LINE"; then
  ok "pipeline show applies a patch from the overrides layer, from the main checkout"
else
  bad "pipeline show applies a patch from the overrides layer, from the main checkout"
  sed 's/^/        /' <<<"$IMPLEMENT_LINE"
fi
if [ "$REVIEW_LINE" = "$REVIEW_LINE_BEFORE" ]; then
  ok "and every other step still comes from the tracked file"
else
  bad "and every other step still comes from the tracked file"
  diff <(echo "$REVIEW_LINE_BEFORE") <(echo "$REVIEW_LINE") | sed 's/^/        /'
fi

# The same lane, seen from inside a linked worktree it never copied anything
# into: `pipeline show` there calls `Pipelines::load(&repo.checkout, ...)`
# with the worktree's own checkout, so this is the git-subprocess branch of
# `dir_for`, not the fallback every unit test above takes.
WT2="$LIVE/worktrees/overrides-wt"
must "cutting a worktree to read the layer from" \
  git worktree add -q -b task/overrides-wt "$WT2" plan/live
WT_IMPLEMENT_LINE=$(grep -E '^  implement ' <<<"$("$SPOOLWAY" -C "$WT2" pipeline show)")
if grep -qF "model=$OVERRIDE_MODEL" <<<"$WT_IMPLEMENT_LINE"; then
  ok "and the same patch is found from inside a linked worktree, with nothing copied into it"
else
  bad "and the same patch is found from inside a linked worktree, with nothing copied into it"
  sed 's/^/        /' <<<"$WT_IMPLEMENT_LINE"
fi
must "removing the overrides worktree" git worktree remove --force "$WT2"
must "removing its branch" git branch -D task/overrides-wt
must "the overrides layer" rm -rf "$SPOOLWAY_PROJECT_HOME/overrides"

# --------------------------------------------------------------- retention
# `retain` sweeps byproducts and never live state, whatever its age.
# `system-prompts/` and `queue/` are dated the same way here, so what makes
# the difference is only which directory holds the entry.
must "retention is set to an age touch can force in a moment" \
  "$SPOOLWAY" config set housekeeping.retention_days 1

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
  echo "stage: queued"; echo "group: live"; echo "pipeline: default"
  echo "touches: [notes/aged-queued-task.md]"; echo "---"; cat "$BODY"
} > "$OLD_QUEUED"
touch -d "2 days ago" "$OLD_QUEUED"

# No dedicated command sweeps anything — an ordinary one is what triggers
# the once-per-process pass.
must "an ordinary command runs the retention sweep" "$SPOOLWAY" group list

works "the aged prompt went" test ! -e "$OLD_PROMPT"
works "the aged queue entry did not — queue/ is never swept, whatever its age" \
  test -f "$OLD_QUEUED"

must "retention restored to its default" "$SPOOLWAY" config set housekeeping.retention_days 30
rm -f "$OLD_QUEUED"

# ------------------------------------------------------- confirm-dialog's gate
# `new version installed, apply updates`: the panel `main.rs` draws in front
# of a project command once this checkout's stamp no longer matches what
# this binary would write — `spoolway update` already having installed a
# newer release is the scenario, forced here by hand since only one binary
# is on `PATH` for a suite to run. Driven both ways, per the task: piped,
# where the one-line notice on stderr takes over and the command still
# runs, and keyed, where a real terminal answers Enter and gets the
# command's own output straight after the report `sync` printed for real.
#
# `override list` is the command under test: it is routed through the same
# catch-all in `main.rs` every project command passes through, and its own
# output ("no overrides") is fixed regardless of anything this suite queued
# earlier, unlike `queue list` or `group list`.
STALE_SKILL=.claude/skills/spoolway-config/SKILL.md
PROJECT_STAMP="$SPOOLWAY_PROJECT_HOME/sync-stamp"

# One real file for a scan to find, and a stamp claiming a release that never
# shipped — `stamp_behind` reads true on the version alone, whatever the
# fingerprint says. Both conditions the acceptance criteria name, not either
# alone: `src/gate.rs`'s own unit tests already cover a stale stamp with
# nothing for a scan to do proceeding silently, so this suite only has to
# prove the shape where both fire, on the real binary.
behind_checkout() {
  rm -f "$STALE_SKILL"
  echo "0.0.0-behind-e2e deadbeef $(pwd)" > "$PROJECT_STAMP"
}

behind_checkout
silent_about "a piped command with a behind checkout never touches the cursor" \
  $'\x1b' \
  "$SPOOLWAY" override list
says "and prints the one-line notice" \
  "spoolway wants to update:" \
  "$SPOOLWAY" override list
says "naming a file count, whatever configure_project's own fixture leaves behind" \
  "file(s) in this checkout." \
  "$SPOOLWAY" override list
says "naming the command that clears it" \
  'Run `spoolway sync`.' \
  "$SPOOLWAY" override list
says "the command itself still ran, piped or not" \
  "no overrides" \
  "$SPOOLWAY" override list
works "and the piped path never wrote anything back" \
  test ! -e "$STALE_SKILL"

# The panel only ever draws with both ends a real terminal, which none of the
# above ever were — every suite invocation runs under `$(...)`. A plain pipe
# cannot stand in for one either: `queue`'s own screen reads keys off a pipe
# fine because it never asks whether anyone is watching, but this dialog
# does, on purpose (`ask::interactive()`), so stdin has to be a terminal a
# `read` can block on, not just a descriptor bytes happen to arrive on.
#
# `python3`'s `pty` module opens one without needing a real terminal behind
# this suite's own process — already how `warmth.sh`, `jobs.sh` and
# `disaster.sh` drive a check no shell built-in reaches, and no new tool this
# harness does not already depend on. `pty.fork()` specifically, not a plain
# pty pair handed to `subprocess.Popen`: only `pty.fork()`'s child calls
# `setsid()` and makes the slave its controlling terminal, which is what a
# real ctrl-c needs to turn into a real `SIGINT` at all — a slave fd merely
# `dup2`'d onto a child's stdio carries bytes fine but is nobody's
# controlling terminal, so the kernel never raises anything on it. Byte
# `\x03` (ctrl-c) sent down a pty missing that step is silently swallowed as
# ordinary input instead, which would make the ctrl-c case below pass for
# the wrong reason — proceeding on EOF, not on the interrupt.
PTY_DRIVER="$LIVE/confirm-dialog-pty.py"
cat >"$PTY_DRIVER" <<'PY'
import os, pty, select, sys, time

key = bytes([int(sys.argv[1])])
argv = sys.argv[2:]

pid, master = pty.fork()
if pid == 0:
    os.execvp(argv[0], argv)
    os._exit(127)

# The panel is drawn before the process ever reads a key, but there is no
# signal back to this driver that says so — the read it is about to make is
# exactly what blocks on the answer. A short, fixed wait is what every other
# scripted-keystroke case in this harness already accepts (`records`' own
# 0.1s poll), and this dialog's panel is a handful of `write` calls, not a
# search.
time.sleep(0.3)
os.write(master, key)

out = b""
status = None
deadline = time.time() + 10
while time.time() < deadline:
    ready, _, _ = select.select([master], [], [], 0.2)
    if master in ready:
        try:
            chunk = os.read(master, 4096)
        except OSError:
            chunk = b""
        if not chunk:
            break
        out += chunk
    wpid, status = os.waitpid(pid, os.WNOHANG)
    if wpid != 0:
        break
    status = None

if status is None:
    # The process has exited (or the pty closed) but a last chunk may still
    # be sitting in the kernel buffer — drained briefly rather than trusted
    # to have already arrived in the loop above.
    end = time.time() + 1
    while time.time() < end:
        ready, _, _ = select.select([master], [], [], 0.1)
        if master not in ready:
            break
        try:
            chunk = os.read(master, 4096)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    # `WNOHANG` on a child that has not exited yet returns `(0, 0)`, not
    # `(0, None)` — trusting `status` straight off that call is exactly the
    # bug review found: a `status` of `0` reads as `WIFEXITED` true and
    # `WEXITSTATUS` 0, so a genuinely hung child reported a clean exit and
    # every assertion the ctrl-c case makes about something NOT happening
    # passed against a process that was still sitting there. `wpid` is what
    # actually says whether the child exited; `status` from the same call is
    # only trustworthy once `wpid` says so.
    #
    # One `WNOHANG` is not enough to ask, though: the loop above leaves here
    # on the pty reaching EOF, and the kernel closes a dying process's fds
    # before it makes the process reapable, so on a loaded machine the child
    # is regularly still running at this instant — measured at 236 false
    # timeouts in 400 runs with every core busy, against 0 on an idle one.
    # Asked once, that reads back as a hang and kills a process that had
    # already finished. So it is asked repeatedly until the same deadline
    # the loop above used, which leaves the timeout branch reachable for a
    # child that really never exits while costing a genuine exit only the
    # sleep below.
    reaped = None
    while True:
        wpid, st = os.waitpid(pid, os.WNOHANG)
        if wpid != 0:
            reaped = st
            break
        if time.time() >= deadline:
            break
        time.sleep(0.02)
    status = reaped

sys.stdout.buffer.write(out)
if status is None:
    print("confirm-dialog-pty.py: timed out waiting for the process", file=sys.stderr)
    os.kill(pid, 9)
    sys.exit(124)
sys.exit(os.WEXITSTATUS(status) if os.WIFEXITED(status) else 128 + os.WTERMSIG(status))
PY

behind_checkout
KEYED_OUT=$(python3 "$PTY_DRIVER" 13 "$SPOOLWAY" override list 2>&1)
KEYED_STATUS=$?
if [ "$KEYED_STATUS" -eq 0 ]; then
  ok "a keyed run confirms and exits zero"
else
  bad "a keyed run confirms and exits zero (exit $KEYED_STATUS)"
  sed 's/^/        /' <<<"$KEYED_OUT"
fi
if grep -qF "new version installed, apply updates" <<<"$KEYED_OUT"; then
  ok "the panel drew"
else
  bad "the panel drew"; sed 's/^/        /' <<<"$KEYED_OUT"
fi
if grep -qF "no overrides" <<<"$KEYED_OUT"; then
  ok "and the command's own output followed, after the panel and its report"
else
  bad "and the command's own output followed, after the panel and its report"
  sed 's/^/        /' <<<"$KEYED_OUT"
fi
works "confirming wrote the missing file back for real" \
  test -f "$STALE_SKILL"

# Ctrl-c, over the same real pty — the acceptance criterion the injectable
# unit tests cannot reach on their own: those inject "was this interrupted"
# directly, so nothing in this repository until now has proven that a real
# ctrl-c over a real terminal actually gets there. Review's own finding: a
# first attempt at this caught the interrupt with `libc::signal`, which
# glibc installs with `SA_RESTART`, so the blocked `read` underneath
# `screen::read_key` silently resumed instead of failing with `EINTR` — the
# process hung at the panel forever, and only a second ctrl-c (through the
# kernel's now-restored default disposition) killed it, leaving the
# terminal in raw mode. `src/gate.rs`'s own `SigintGuard` installs without
# `SA_RESTART` for exactly this reason; this is what proves it against the
# real kernel rather than the docs it was fixed against.
behind_checkout
CTRLC_OUT=$(python3 "$PTY_DRIVER" 3 "$SPOOLWAY" override list 2>&1)
CTRLC_STATUS=$?
if [ "$CTRLC_STATUS" -eq 0 ]; then
  ok "ctrl-c at the panel exits zero rather than hanging or being killed"
else
  bad "ctrl-c at the panel exits zero rather than hanging or being killed (exit $CTRLC_STATUS)"
  sed 's/^/        /' <<<"$CTRLC_OUT"
fi
if grep -qF "no overrides" <<<"$CTRLC_OUT"; then
  bad "ctrl-c must not run the command it interrupted"
  sed 's/^/        /' <<<"$CTRLC_OUT"
else
  ok "and the command it interrupted never ran"
fi
works "ctrl-c wrote nothing back" \
  test ! -e "$STALE_SKILL"

finish
