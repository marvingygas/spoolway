#!/usr/bin/env bash
# CLI behaviour that needs a real process, a real project or a real forge,
# and belongs to no domain of its own: scaffolding (`init`, `sync`), the
# contracts (`task contract`, `pipeline contract`, `prompt contract`), the
# queue tab of bare `spoolway` driven over a real pty, the archive's own rows,
# `config`'s checkout/project asymmetry, the overrides layer resolved through
# a linked worktree, housekeeping's retention sweep, and the silence of a
# command run in a behind checkout, driven over a real pty.
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
configure_project plan/live
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
  "fake-tiered": {
    "mode": "chat",
    "input_cost_per_token": 0.0000001,
    "output_cost_per_token": 0.0000005,
    "cache_read_input_token_cost": 0.00000002,
    "cache_creation_input_token_cost": 0.000000125,
    "input_cost_per_token_above_100k_tokens": 0.0000005,
    "output_cost_per_token_above_100k_tokens": 0.0000025,
    "cache_read_input_token_cost_above_100k_tokens": 0.0000001,
    "cache_creation_input_token_cost_above_100k_tokens": 0.000000625,
    "cache_creation_input_token_cost_above_1hr_above_100k_tokens": 0.000001,
    "input_cost_per_token_above_200k_tokens": 0.000009
  },
  "not-chat": {
    "mode": "embedding",
    "input_cost_per_token": 0.000001,
    "output_cost_per_token": 0.000002
  }
}
JSON
# A refresh that keeps fewer than half the rows of the table it replaces writes
# nothing, and the first refresh here replaces the built-in table's thousands.
# Filler chat rows, as many as the built-in table holds, lift the fixture past
# half of it, so the checks below are about the rows and not about a fixture
# too small to be a price map. The count follows the shipped table because
# every release refreshes it, and a fixed count would one day fall below half.
BUILTIN_ROWS=$(jq '.models | length' "$HERE/../../../assets/model-prices.json")
jq --argjson n "$BUILTIN_ROWS" '. + ([range(0; $n) | {key: "filler-\(.)", value: {mode: "chat",
  input_cost_per_token: 0.000001, output_cost_per_token: 0.000002}}] | from_entries)' \
  "$PRICE_FIXTURE" >"$PRICE_FIXTURE.full"
mv "$PRICE_FIXTURE.full" "$PRICE_FIXTURE"
must "models refresh fetches and distils a local fixture through curl" \
  env SPOOLWAY_MODEL_PRICES_URL="file://$PRICE_FIXTURE" "$SPOOLWAY" models refresh
works "the refreshed machine-wide table is valid JSON" \
  jq -e '.source and .license == "MIT" and .generated and .models["fake-local"].input == 1' \
  "$HOME/.spoolway/model-prices.json"
# The lowest of the row's two thresholds is the one kept, converted to USD
# per million like the base rates, the one-hour cache write included.
works "a tiered row reaches the refreshed table as its lowest tier" \
  jq -e '.models["fake-tiered"] | .input == 0.1
    and .above_100k_tokens == {"input": 0.5, "output": 2.5, "cache_read": 0.1,
      "cache_write_5m": 0.625, "cache_write_1h": 1}
    and (has("above_200k_tokens") | not)' \
  "$HOME/.spoolway/model-prices.json"
# One row priced past $100,000 per million is dropped and named, and the rest
# of the table is still written.
jq '. + {"fake-pricey": {mode: "chat", input_cost_per_token: 1,
  output_cost_per_token: 0.000002}}' "$PRICE_FIXTURE" >"$LIVE/model-prices-pricey.json"
says "a refresh names a row whose rate is out of range" \
  "refused  1 rows out of range: fake-pricey" \
  env SPOOLWAY_MODEL_PRICES_URL="file://$LIVE/model-prices-pricey.json" "$SPOOLWAY" models refresh
works "the out-of-range row is absent and the rest of the table is written" \
  jq -e '(.models | has("fake-pricey") | not) and .models["fake-local"].input == 1' \
  "$HOME/.spoolway/model-prices.json"
# A fixture with too few rows is refused whole, naming both counts and the
# source, and the table written above stays exactly as it was.
TABLE_BEFORE=$(cat "$HOME/.spoolway/model-prices.json")
jq '{"fake-local": .["fake-local"]}' "$PRICE_FIXTURE" >"$LIVE/model-prices-few.json"
refuses "a refresh keeping fewer than half the replaced rows writes nothing" \
  "model-prices-few.json kept 1 priced rows, fewer than half of the $((BUILTIN_ROWS + 2))" \
  env SPOOLWAY_MODEL_PRICES_URL="file://$LIVE/model-prices-few.json" "$SPOOLWAY" models refresh
if [ "$(cat "$HOME/.spoolway/model-prices.json")" = "$TABLE_BEFORE" ]; then
  ok "the refused refresh left the last table untouched"
else
  bad "the refused refresh left the last table untouched"
fi
MODELS_OUT="$LIVE/models-refreshed.out"
"$SPOOLWAY" models >"$MODELS_OUT"
if grep -qE '^fake-local[[:space:]].*[[:space:]]refreshed[[:space:]]' "$MODELS_OUT"; then
  ok "models reads the new row back with SOURCE refreshed"
else
  bad "models reads the new row back with SOURCE refreshed"
  sed 's/^/        /' "$MODELS_OUT"
fi
# The tier is drawn on an indented row under its model, so a step has to name
# the tiered model for the table to show it. The pipeline is put back after.
cp .spoolway/pipelines/default.yml "$LIVE/default.yml.keep"
set_step_of default implement model fake-tiered
"$SPOOLWAY" models >"$LIVE/models-tiered.out"
cp "$LIVE/default.yml.keep" .spoolway/pipelines/default.yml
if grep -qE '^ +above 100k tokens +\$0\.50 +\$2\.50 ' "$LIVE/models-tiered.out"; then
  ok "models draws a tiered model's higher rates on an indented row under it"
else
  bad "models draws a tiered model's higher rates on an indented row under it"
  sed 's/^/        /' "$LIVE/models-tiered.out"
fi
# `eval` re-prices the four class columns from the ledger line's own split:
# `tier_tokens` at the tier's rates, the rest at the base rates, both from the
# refreshed table above. The ledger is put back after.
LEDGER="$SPOOLWAY_PROJECT_HOME/usage.jsonl"
LEDGER_HAD=0
if [ -e "$LEDGER" ]; then LEDGER_HAD=1; cp "$LEDGER" "$LIVE/usage.jsonl.keep"; fi
printf '%s\n' '{"ts":"2026-10-09T09:12:40+00:00","task":"tiered-eval","step":"implement","pipeline":"tiered","agent":"pi","kind":"pi","model":"fake-tiered","session":"tiered-eval-s1","turns":2,"tokens":{"input":1000000,"output":1000000,"cache_read":2000000,"cache_write_5m":1000000},"tier_tokens":{"input":400000,"output":200000,"cache_read":1000000,"cache_write_5m":500000},"cost_usd":1.5}' >>"$LEDGER"
# in: 0.6M x $0.10 + 0.4M x $0.50; out: 0.8M x $0.50 + 0.2M x $2.50;
# cache read: 1M x $0.02 + 1M x $0.10; cache write: 0.5M x $0.125 + 0.5M x $0.625.
works "eval prices a line's tier_tokens at the tier's rates in the four class columns" \
  bash -c '"$0" eval --by pipeline --pipeline tiered --json | jq -e "
    def near(a; b): ((a - b) | fabs) < 1e-9;
    .total | near(.in_usd; 0.26) and near(.out_usd; 0.9)
      and near(.cache_read_usd; 0.12) and near(.cache_write_usd; 0.375)
      and .cost_usd == 1.5"' "$SPOOLWAY"
if [ "$LEDGER_HAD" = 1 ]; then cp "$LIVE/usage.jsonl.keep" "$LEDGER"; else rm -f "$LEDGER"; fi
# A claude session banks Claude Code's own `cost-state` total. The claude
# stand-in's transcript writer appends one for a turn whose model nothing
# prices, so a settled lane caught up by `eval` can only get its cost from that
# record. The ledger is put back after.
LEDGER_HAD=0
if [ -e "$LEDGER" ]; then LEDGER_HAD=1; cp "$LEDGER" "$LIVE/usage.jsonl.keep"; fi
printf '%s\n' '{"ts":"2020-01-01T00:00:00+00:00","task":"cost-state-e2e","step":"implement","pipeline":"costcheck","agent":"claude","kind":"claude","model":"fake-cloud","session":"costcheck-s1","tokens":{}}' >>"$LEDGER"
mkdir -p "$LIVE/cost-state-ctl"
printf '100\n' >"$LIVE/cost-state-ctl/transcript"
(
  # shellcheck source=../agents/transcript.sh
  . "$HERE/../agents/transcript.sh"
  E2E_CTL="$LIVE/cost-state-ctl" write_transcript claude --session-id costcheck-s1
)
works "eval banks the total Claude Code reported for a claude session" \
  bash -c '"$0" eval --by pipeline --pipeline costcheck --json | jq -e ".total.cost_usd == 0.5"' "$SPOOLWAY"
works "the ledger line carries that reported total beside the cost" \
  bash -c 'jq -es "map(select(.session == \"costcheck-s1\" and .reported_usd == 0.5 and .cost_usd == 0.5)) | length == 1" "$0" >/dev/null' "$LEDGER"
if [ "$LEDGER_HAD" = 1 ]; then cp "$LIVE/usage.jsonl.keep" "$LEDGER"; else rm -f "$LEDGER"; fi
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
# `init` asks its questions at a terminal — the agent, the tracker and the
# project key — and none anywhere else, which is exactly the distinction a
# shell suite is the right place to hold: everything below runs with no tty,
# so an `init` that ever read stdin here would hang the suite rather than
# fail it. Nobody being there means every question takes its own default,
# and a run with no terminal writes nothing unless `--yes` lets it — so every
# run below that wants a project passes `--yes`, and the one that does not is
# the declined case asserted with it.
# In its own directory — this is a project being created, and the suite's
# own is already one.
INITDIR="$LIVE/init"
mkdir -p "$INITDIR/asked" && (cd "$INITDIR/asked" && git init -q -b main .)
works "init scaffolds a project with the answers given as flags" \
  env -C "$INITDIR/asked" "$SPOOLWAY" init --yes \
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
  env -C "$INITDIR/unasked" "$SPOOLWAY" init --yes
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

# The other side of `--yes`: with no terminal and no `--yes`, `init` exits 0
# having written nothing at all. Nothing here is a `.spoolway/` tree this suite
# has to clean up afterwards, which is the point: a declined `init` leaves
# the directory exactly as it found it, and claims no home either.
DECLINED="$INITDIR/declined"
mkdir -p "$DECLINED" && (cd "$DECLINED" && git init -q -b main .)
works "init with no terminal and no --yes declines rather than scaffolding" \
  env -C "$DECLINED" "$SPOOLWAY" init
# Matched on the trailing component rather than on `$DECLINED` whole: the
# path `init` prints is `git rev-parse --show-toplevel`'s own, which on a
# machine whose temporary directory is a symlink is the resolved one and
# not the string this suite built.
says "and it says which directory it was asking about" "/declined" \
  env -C "$DECLINED" "$SPOOLWAY" init
works "no .spoolway/ was created" test ! -e "$DECLINED/.spoolway"
works "no skills were installed" test ! -e "$DECLINED/.claude"
works "and no home was claimed under ~/.spoolway/" \
  test -z "$(find "$HOME/.spoolway" -maxdepth 1 -name 'declined-*' 2>/dev/null)"

refuses "the retired agent answer is no longer accepted" \
  "unexpected argument '--agent'" env -C "$INITDIR/unasked" "$SPOOLWAY" init --agent gemini

# Run again for a second provider: the skills land, and the config the project
# has been running on is not rewritten around it.
works "a second init installs another provider's skills" \
  env -C "$INITDIR/asked" "$SPOOLWAY" init --yes --provider claude
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
must "the project to repair scaffolds" env -C "$RESTORE" "$SPOOLWAY" init --yes
printf '\n# a line this project wrote itself\n' >> "$RESTORE/.spoolway/config.toml"

rm -rf "$RESTORE/.spoolway/pipelines"
refuses "a project whose pipelines went missing says so rather than borrowing" \
  "no pipelines defined" env -C "$RESTORE" "$SPOOLWAY" task contract

RESTORE_OUT=$(env -C "$RESTORE" "$SPOOLWAY" init --yes 2>&1)
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
# `init`'s menu asks interactively, and land in `[issue_tracking]`. Naming
# a tracker writes every hook script, not only the chosen one, so switching
# between trackers later is a config edit rather than a second `init`.
# `none` writes no `hooks/` folder at all.
mkdir -p "$INITDIR/github" && (cd "$INITDIR/github" && git init -q -b main .)
GITHUB_OUT=$(env -C "$INITDIR/github" "$SPOOLWAY" init --yes --examples --tracker github \
  --project-key acme/app 2>&1)
has "init --examples --tracker github answers every question with no prompt" \
  "Project initialized successfully." <(printf '%s\n' "$GITHUB_OUT")
has "and closes on the model-and-effort line" \
  "Set model and effort on every agent step" <(printf '%s\n' "$GITHUB_OUT")
works "the examples are written" \
  test -f "$INITDIR/github/.spoolway/pipelines/default.yml" -a \
       -f "$INITDIR/github/.spoolway/prompts/implementer/PROMPT.md" -a \
       -f "$INITDIR/github/.spoolway/templates/tasks/default.md"
works "the tracking templates are gone — the open hook builds the issue body itself" \
  test ! -e "$INITDIR/github/.spoolway/templates/tracking/ticket.md"
has "the hook it names" 'hook = "github.sh"' "$INITDIR/github/.spoolway/config.toml"
has "and the project it files into" 'project_key = "acme/app"' \
  "$INITDIR/github/.spoolway/config.toml"
works "the github hook is written" test -f "$INITDIR/github/.spoolway/hooks/github.sh"
works "and so is jira's, unchosen or not" test -f "$INITDIR/github/.spoolway/hooks/jira.sh"
works "a hook is written executable" test -x "$INITDIR/github/.spoolway/hooks/github.sh"
works "no close-on-merge workflow ships with github any more" \
  test ! -e "$INITDIR/github/.github"

# `none` — `unasked`'s own `init` above already took every default with
# nobody there to ask, which includes the tracker question defaulting to
# `none`: the table stays empty and there is no `hooks/` folder.
has "unasked left the hook empty" 'hook = ""' "$INITDIR/unasked/.spoolway/config.toml"
has "and the project key empty" 'project_key = ""' "$INITDIR/unasked/.spoolway/config.toml"
works "and no hooks folder is written" test ! -e "$INITDIR/unasked/.spoolway/hooks"

# `--no-examples` writes the config and three empty folders for the
# spoolway-config skill to fill, and closes on the line that names it.
mkdir -p "$INITDIR/bare" && (cd "$INITDIR/bare" && git init -q -b main .)
BARE_OUT=$(env -C "$INITDIR/bare" "$SPOOLWAY" init --yes --no-examples --tracker none 2>&1)
has "init --no-examples closes on the spoolway-config line" \
  "Use the spoolway-config skill to create pipelines." <(printf '%s\n' "$BARE_OUT")
has "and reports the folders it made" "made     .spoolway/pipelines/" \
  <(printf '%s\n' "$BARE_OUT")
works "config.toml is written" test -f "$INITDIR/bare/.spoolway/config.toml"
works "pipelines, prompts and templates are empty folders" \
  test -d "$INITDIR/bare/.spoolway/pipelines" -a -z "$(ls -A "$INITDIR/bare/.spoolway/pipelines")" \
    -a -d "$INITDIR/bare/.spoolway/prompts" -a -z "$(ls -A "$INITDIR/bare/.spoolway/prompts")" \
    -a -d "$INITDIR/bare/.spoolway/templates" -a -z "$(ls -A "$INITDIR/bare/.spoolway/templates")"
works "none writes no hooks folder" test ! -e "$INITDIR/bare/.spoolway/hooks"
works "and no workflow" test ! -e "$INITDIR/bare/.github"

# `spoolway sync` never touches a hook `init` has already written — the
# same rule a prompt or a task skeleton already follows once a project has
# made a file its own.
HOOK="$INITDIR/github/.spoolway/hooks/github.sh"
printf '#!/bin/sh\necho mine\n' > "$HOOK"
must "sync runs over the tracker project" env -C "$INITDIR/github" "$SPOOLWAY" sync
has "the hook this project edited is exactly as it left it" "echo mine" "$HOOK"

# A second sync right after has nothing left to do — the project's own hook
# is untouched by the first sync, so this scan writes and removes nothing.
# Claiming files were overwritten here is a lie the moment anyone runs
# `git status`.
says "an empty sync says so honestly" \
  "Nothing updating." \
  env -C "$INITDIR/github" "$SPOOLWAY" sync
silent_about "and does not claim files were overwritten" \
  "Files were overwritten" \
  env -C "$INITDIR/github" "$SPOOLWAY" sync

# `--replace` takes the whole shipped hook back — and must leave it
# executable, the same as `init` did, even though `write_atomic` itself has
# no opinion about permissions (issue #331).
chmod -x "$HOOK"
must "--replace takes the shipped hook back" \
  env -C "$INITDIR/github" "$SPOOLWAY" sync --replace .spoolway/hooks/github.sh
works "the replaced hook is executable again" test -x "$HOOK"

# ------------------------------------------------------------ lane-prompts.md
# The eight typed pane messages are spoolway's own now, with no project
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
# `dispatch.interval` is retired, and a config that still names it loads past
# the key with a note rather than refusing — `DispatchConfig` still denies
# unknown fields, but the key is stripped before the parse sees it. `sync` is
# the one place that drops it from the file for good.
CONFIG="$INITDIR/unasked/.spoolway/config.toml"
sed -i '/^\[dispatch\]$/a interval = "10s"' "$CONFIG"
works "a config still naming dispatch.interval loads" \
  env -C "$INITDIR/unasked" "$SPOOLWAY" config get dispatch.lane_quiet
says "and says the key is retired, pointing at sync" \
  "dispatch.interval in" \
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

# Stdout only, not `2>&1`: stderr is where spoolway's notices go ahead of a
# command's own output — merging the two would hand `jq` such a line as its
# first byte and fail every parse, which is not what either check below is
# about.
CHECK_JSON=$("$SPOOLWAY" task contract 2>"$LIVE/task-contract.err")
if jq -e '.pipelines.default.longest_agent_step' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "bare task contract prints the contract as parseable JSON"
else
  bad "bare task contract prints the contract as parseable JSON"
  echo "$CHECK_JSON" | sed 's/^/        /'
  sed 's/^/        /' "$LIVE/task-contract.err"
fi

# gh-359: a lane too long for the multiplexer's own name limit gets a short
# internal alias instead of a refusal, so the contract advertises no
# pipeline-dependent task-id budget any more.
if jq -e '.pipelines.default | has("id_budget") | not' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "the contract advertises no pipeline-dependent task-id budget"
else
  bad "the contract advertises no pipeline-dependent task-id budget"
  echo "$CHECK_JSON" | sed 's/^/        /'
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

# A producer sizing a chain has to know which end of it each marked command
# step lands on, so both ends are advertised per pipeline. Neither is ever
# absent from the object: a pipeline that marks no such step carries `null`,
# not a missing key, so a caller reading the field never has to tell "this
# pipeline has no root step" apart from "this version does not report one".
if jq -e '
    [.pipelines[] | has("first_of_chain") and has("last_of_chain")] | all
  ' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "every pipeline reports both first_of_chain and last_of_chain"
else
  bad "every pipeline reports both first_of_chain and last_of_chain"
  echo "$CHECK_JSON" | sed 's/^/        /'
fi

# Whatever a pipeline reports as its root step has to be one of that
# pipeline's own steps — the same list `gate_at` may name — rather than a
# label from somewhere else.
# `. as $p` before the index: jq evaluates `index`'s argument against the
# array on its left, so the shorter `(.gate_at | index(.first_of_chain))`
# looks `first_of_chain` up on the step list and errors out instead of
# answering. Binding the pipeline first keeps both halves reading off it.
if jq -e '
    [ .pipelines[]
      | select(.first_of_chain != null)
      | . as $p
      | ($p.gate_at | index($p.first_of_chain)) != null
    ] | all
  ' <<<"$CHECK_JSON" >/dev/null 2>&1; then
  ok "a reported first_of_chain names one of that pipeline's own steps"
else
  bad "a reported first_of_chain names one of that pipeline's own steps"
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
  ok "task contract --from a task setting run: exits non-zero"
else
  bad "task contract --from a task setting run: exits non-zero"
fi
if [ "$BEFORE_CHECK" = "$AFTER_CHECK" ]; then
  ok "and leaves the queue directory exactly as it was"
else
  bad "and leaves the queue directory exactly as it was"
  diff <(echo "$BEFORE_CHECK") <(echo "$AFTER_CHECK") | sed 's/^/        /'
fi

# ------------------------------------------------------------- gh-359: long ids
# `release-spoolway-2` on the release pipeline's `merge-released-repair` step
# used to be refused before it ever queued: the lane it would build there ran
# past the multiplexer's own 32-character name limit, and the queue measured
# a task id against that full, unaliased lane name. A lane too long for the
# wire now gets a short internal alias instead, so nothing about a task id's
# length is the queue's business any more — checked here against an id well
# past what the old 34-byte lane budget ever allowed.
LONG_ID="a-task-id-much-longer-than-herdrs-own-agent-name-limit-of-32-characters"
LONG_ID_DOC="$LIVE/long-id.md"
{
  echo "---"
  echo "id: $LONG_ID"
  echo "title: a very long task id must still queue"
  echo "group: live"
  echo "base: main"
  echo "pipeline: default"
  echo "---"
  cat "$BODY"
} > "$LONG_ID_DOC"

works "gh-359: task contract --from checks out a task id longer than any lane budget" \
  "$SPOOLWAY" task contract --from "$LONG_ID_DOC"
silent_about "gh-359: the check report no longer measures an id against a lane name" \
  "fits a lane name" "$SPOOLWAY" task contract --from "$LONG_ID_DOC"

works "gh-359: queue add --from actually queues the long task id" \
  "$SPOOLWAY" queue add --from "$LONG_ID_DOC"
says "and the long id landed in the queue rather than being refused" "base: main" \
  "$SPOOLWAY" queue show "$LONG_ID"
rm -f "$SPOOLWAY_PROJECT_HOME/queue/$LONG_ID.md"

# ------------------------------------------------------ no pipelines/ at all
# A missing `.spoolway/pipelines/` used to be answered from the pipelines
# compiled into the binary; an empty one was already a hard `no pipelines
# defined` error. Both now reach that same error, naming the directory, so
# the broken state is visible where it happens rather than surfacing three
# commands later at dispatch on a `PROMPT.md` that was never written. A
# repo-mode project also reads private pipelines from its home's `local/`,
# so the error names both places a pipeline could have come from.
mv .spoolway/pipelines "$LIVE/pipelines.bak"
refuses "task contract with no pipelines/ reports the shared error" \
  "\.spoolway/pipelines or .*/local/pipelines: no pipelines defined" \
  "$SPOOLWAY" task contract

DOCTOR_OUT=$("$SPOOLWAY" doctor 2>&1)
if grep -qF "FAIL  pipelines load: in " <<<"$DOCTOR_OUT" \
  && grep -q "\.spoolway/pipelines or .*/local/pipelines: no pipelines defined" \
    <<<"$DOCTOR_OUT"; then
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
  env -C "$BASECHECK" "$SPOOLWAY" init --yes --provider claude --tracker none
must "a seed commit, so a second branch has something to point at" \
  env -C "$BASECHECK" git commit -q --allow-empty -m seed
must "a second local branch --base could point at instead" \
  env -C "$BASECHECK" git branch other/base

NOBASE="$BASECHECK/nobase.md"
{
  echo "---"
  echo "id: nobase"
  echo "title: nobase, done"
  echo "group: nobase"
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

# A group of its own: `nobase` above is still queued, and a group is one
# chain, so a second unrelated task in its group would be refused as a second
# root before the base rule this case is about was ever asked.
OWNBASE="$BASECHECK/ownbase.md"
{
  echo "---"
  echo "id: ownbase"
  echo "title: ownbase, done"
  echo "group: ownbase"
  echo "base: plan/x"
  echo "pipeline: default"
  echo "---"
  cat "$BODY"
} > "$OWNBASE"
works "a task's own base wins over --base" \
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

# `first:` is a step key a pipeline author can only learn about from here or
# from the annotated starter, so both have to carry it: the key list names
# it, and the starter shows the line to uncomment.
if jq -e '
    (.keys.step | index("first")) != null
    and (.fields.first | test("Command steps only"))
    and (.template | test("# first: true"))
  ' <<<"$PIPELINE_JSON" >/dev/null 2>&1; then
  ok "the step keys carry first, with its effect and a starter line"
else
  bad "the step keys carry first, with its effect and a starter line"
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
  env -C "$PICHECK" "$SPOOLWAY" init --yes --provider claude --tracker none
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

# --------------------------------------------- copy commands refuse escaping names
# `pipeline copy` and `prompt copy` join `<from>`/`<to>` straight onto a
# directory with nothing else standing between the string and the
# filesystem, so a traversal name has to be refused before anything is
# written — proven here against the real project home under
# `$SPOOLWAY_PROJECT_HOME`, which no unit test ever constructs.
refuses "pipeline copy refuses a traversal <to>" "is not a valid pipeline name" \
  "$SPOOLWAY" pipeline copy default ../../../../escaped
# `pipelines_dir.join("<to>.yml")` under `$SPOOLWAY_PROJECT_HOME/local/pipelines/`
# is exactly four levels up from there — `$HOME/escaped.yml` — the same path
# the task's own repro names a bad `<to>` reaching before this fix.
works "and nothing escaped into home" test ! -e "$HOME/escaped.yml"
refuses "prompt copy refuses a traversal <to>" "is not a valid prompt name" \
  "$SPOOLWAY" prompt copy builder ../../../../../evilp
# `prompts_dir.join("<to>").join("PROMPT.md")` under the same `local/prompts/`
# is one level deeper than the pipeline case above, so five levels up from
# there lands one above `$HOME` — `$(dirname "$HOME")/evilp/PROMPT.md` —
# never inside the tracked `.spoolway/prompts/` a looser check might assume.
works "and nothing escaped above home" test ! -e "$(dirname "$HOME")/evilp"

# ---------------------------------------------------------- prompt contract
# `prompt contract` gains a seventh section, on every call, whatever
# `--step` names — the prompt skeleton, not a paraphrase of it. There is no
# project default pipeline to fall back to any more, so the call now has to
# name one explicitly.
says "prompt contract prints the shape-to-write section" \
  "THE SHAPE TO WRITE" "$SPOOLWAY" prompt contract --pipeline default

# Section 4 tells a prompt author these variables are never read by prose.
says "prompt contract titles the environment as never read" \
  "THE ENVIRONMENT EVERY LANE HAS, AND NEVER READS" "$SPOOLWAY" prompt contract --pipeline default

# Section 3 renders the `restart` state from the same list as every other.
says "prompt contract renders the restart state" \
  "This is \`restart\`:" "$SPOOLWAY" prompt contract --pipeline default
says "and its briefing says the worktree is not clean" \
  "its changes are" "$SPOOLWAY" prompt contract --pipeline default

# A gated step's report contract carries no "not available to you" block at
# all: `--stage` is refused by `spoolway report` itself off every step but
# `blocked`, and `compose::report_contract` no longer names it under refusal
# wording to say so a second time — see `src/compose.rs`. `check`'s `on_fail`
# has to land somewhere other than `blocked` — a `retry` step to send it to —
# or its fail and block destinations collide and `--fail` stays withheld,
# same as any other step whose two outcomes go the same way; only with a
# distinct fail route does the block disappear entirely rather than shrink to
# one line, which is the Mockup's own "gated step's whole contract" case.
# Written relative to the suite's cwd, its checkout root — `Pipelines::
# dir_in` reads `.spoolway/pipelines/` under there, never under
# `$SPOOLWAY_PROJECT_HOME` — the same place `restart.sh` and `warmth.sh`
# already write their own throwaway pipelines. `builder` is the harness's
# own prompt, written by `own_prompts`: this suite names no shipped one
# (only `flow.sh` does, to prove the shipped pipeline runs on them), so
# rewording `assets/prompts/` never reaches this check.
cat > .spoolway/pipelines/gate-check.yml <<'YML'
steps:
  - id: check
    agent: pi
    prompt: builder
    model: fake-local
    gate: true
    on_pass: done
    on_fail: retry

  - id: retry
    agent: pi
    prompt: builder
    model: fake-local
    on_pass: done
YML
silent_about "a gated step's contract carries no withheld block" \
  "not available to you" \
  "$SPOOLWAY" prompt contract --pipeline gate-check --step check
says "and still holds the pass for the person who opens the pane" \
  "A pass is held here for a person, who opens this pane." \
  "$SPOOLWAY" prompt contract --pipeline gate-check --step check
rm -f .spoolway/pipelines/gate-check.yml

# ------------------------------------------------- unblocker holds the gate
# gh-448: the gate belongs to the step, whoever does its work. A task
# hand-placed on `blocked` with `blocked_from` naming a `gate: true` step —
# the same hand-placed shape `flow.sh`'s own "blocked has one turn, not a
# loop" scenario proves the ordinary road out of a block with, no real lane
# spent — is cleared with a plain `--pass` and must land on `paused` at that
# step's own gate, not carried past it to `e2e` the way an ungated block
# already is (see `command-steps.sh`'s own `--pass --stage` cases).
cat > .spoolway/pipelines/gate-hold.yml <<'YML'
steps:
  - id: look
    agent: pi
    prompt: builder
    model: fake-local
    gate: true
    on_pass: e2e

  - id: e2e
    agent: pi
    prompt: builder
    model: fake-local
    on_pass: done
YML

GATE_TASK="tab-shell"
{
  echo "---"
  echo "id: $GATE_TASK"
  echo "title: stuck at a gate, blocked by hand"
  echo "stage: blocked"
  echo "blocked_from: look"
  echo "pipeline: gate-hold"
  echo "group: live"
  echo
  echo "---"
  cat "$BODY"
} > "$SPOOLWAY_PROJECT_HOME/queue/$GATE_TASK.md"

GATE_OUT=$("$SPOOLWAY" report "$GATE_TASK" --pass -m "fixed the strip" 2>&1)
GATE_STATUS=$?
if [ "$GATE_STATUS" -eq 0 ] && grep -qF "held here for a person, at \`look\`'s gate" <<<"$GATE_OUT"
then
  ok "an unblocker's pass off a gated step's own block names whose gate is holding it"
else
  bad "an unblocker's pass off a gated step's own block names whose gate is holding it \
(exit $GATE_STATUS)"
  sed 's/^/        /' <<<"$GATE_OUT"
fi
if [ "$(stage_of "$GATE_TASK")" = paused ]; then
  ok "and the task lands on paused rather than carried past the gate to e2e"
else
  bad "and the task lands on paused rather than carried past the gate to e2e \
(at \`$(stage_of "$GATE_TASK")\`)"
fi
has "paused_at names the step whose gate is holding it, not blocked" \
  "paused_at: look" "$SPOOLWAY_PROJECT_HOME/queue/$GATE_TASK.md"
has "paused_by names the gate" "paused_by: gate" "$SPOOLWAY_PROJECT_HOME/queue/$GATE_TASK.md"
lacks "blocked_from does not survive the hold — nothing here is a caught block" \
  "blocked_from:" "$SPOOLWAY_PROJECT_HOME/queue/$GATE_TASK.md"

says "queue route names where resuming the held gate sends it" \
  "Resuming on the board sends it to e2e." \
  "$SPOOLWAY" queue route "$GATE_TASK"
says "and tells a person the --stage line to run in their own shell" \
  "spoolway resume $GATE_TASK --stage" \
  "$SPOOLWAY" queue route "$GATE_TASK"

must "resuming the held gate" "$SPOOLWAY" resume "$GATE_TASK"
if [ "$(stage_of "$GATE_TASK")" = e2e ]; then
  ok "resuming takes look's own on_pass — the unblocker's pass still stands in for finished work"
else
  bad "resuming takes look's own on_pass (at \`$(stage_of "$GATE_TASK")\`)"
fi

rm -f "$SPOOLWAY_PROJECT_HOME/queue/$GATE_TASK.md"
rm -f .spoolway/pipelines/gate-hold.yml

# ------------------------------------------------------------- the queue screen
# The one thing no unit test can reach: bare `spoolway`'s queue tab reading
# real keystrokes off a pipe, submitting a real group, and clearing that
# group's documents off a real disk. `run_screen` is driven headlessly in Rust
# already — what is only provable here is that the whole binary, invoked as a
# person invokes it, does the same thing end to end.
#
# Two documents of one group, and a third of another, so "removes exactly that
# group's documents" has something to be wrong about.
# `screen-other` is written first on purpose, so `screen-batch` is both the
# newer group and the earlier name: the cursor opens on it under either
# tie-break, and this case never depends on how fine-grained a birth time this
# filesystem keeps.
pending_doc screen-other "$BODY" "group: screen-other"
pending_doc screen-one "$BODY" "group: screen-batch"
pending_doc screen-two "$BODY" "group: screen-batch" \
  "depends_on: [screen-one]"

# space selects the highlighted group, enter submits it and draws the
# `queued` popup over the tab, which only `enter` closes — so the `esc` after
# it is taken by the popup. The screen ends on its own the moment the
# pipe runs dry — see `on_screen` in `lib.sh` on why the keys arrive on a
# pipe while the screen draws to a pty.
on_screen ' \r\x1b' /dev/null

works "the screen queues the first task of the group it submitted" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-one.md"
works "and the second one with it — a group goes whole or not at all" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-two.md"
works "the submitted group's tasks are gone from pending" \
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
works "its task is back in the pending directory" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/screen-two.md"
works "and gone from the queue" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/screen-two.md"

on_screen ' \r\x1b' "$LIVE/screen-requeue.out"

works "screen-two reaches the queue again" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-two.md"
works "and is gone from pending once more" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/pending/screen-two.md"
works "screen-one, already queued, is left exactly where it was" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/screen-one.md"
# The report `enter` used to leave on screen, naming the sibling left alone,
# is gone, and so is the overview that replaced it: `enter` only queues, and
# starting a dispatcher is the dispatch tab's own `enter`. Anchored on the
# overview's table header rather than "screen-batch": the browsing pane's own
# group row prints that name on every frame, so only the header can say the
# overview was not drawn.
lacks "enter only queues — no overview is drawn after it" \
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
works "and clears the task out of the pending directory" \
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
  "group: screen-shipped-group"
must "a group queued directly, never through the screen" \
  "$SPOOLWAY" queue add --from "$SCREEN_QUEUE_ONLY"
works "it never touched the pending directory" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/pending/screen-shipped.md"

# The pending directory is empty at this point — both cases above already
# cleared every document out of it — so this is also the one place proving
# the screen opens on an empty pending directory rather than refusing with
# "Nothing to list", so long as the queue itself still holds a group. The
# queue tab never lists a queued group, so not even `h` draws its row.
on_screen 'h' "$LIVE/queue-screen-only.out"
if grep -q "Nothing to list" "$LIVE/queue-screen-only.out"; then
  bad "a group with nothing left in pending still opens the screen"
  sed 's/^/        /' "$LIVE/queue-screen-only.out"
else
  ok "a group with nothing left in pending still opens the screen"
fi
lacks "\`h\` never lists a group whose tasks are only in the queue now" \
  "screen-shipped-group" "$LIVE/queue-screen-only.out"

# --------------------------------------------------- the archive's own rows
# `list_groups` reads the archive as a third source, through the line
# teardown appends to `archive/index.jsonl`, and `h` switches the left pane
# between queueable only and queueable plus done. Only the real binary, run
# against a task a real dispatcher actually archived, proves the wiring —
# `list_groups`'s own unit tests read a synthetic fixture directory, never
# `Repo::archive_dir()` after a real run.
task_doc "$LIVE/archived-row.md" archived-row "$BODY" \
  "group: arch-row"
must "a task queued for the archive-cycling case" \
  "$SPOOLWAY" queue add --from "$LIVE/archived-row.md"
if drive archived-row gone 180; then
  ok "it ran to completion and left the queue"
else
  bad "it ran to completion and left the queue (at \`$(stage_of archived-row)\`)"
fi
works "and landed in the archive" \
  test -f "$SPOOLWAY_PROJECT_HOME/archive/archived-row.md"

# Two key presses of `h`, captured as one session: the shared frame writer
# (`src/screen/frame_writer.rs`) writes a fresh `\x1b[?2026h\x1b[H` before
# every frame that differs from the last, so the python snippet below splits
# the raw output back into the three frames this draws — opening, then one
# per press — rather than grepping the whole file, which could never tell
# "shown once, then hidden again" from "never shown".
on_screen 'hh' "$LIVE/queue-h-cycle.out"
if python3 - "$LIVE/queue-h-cycle.out" arch-row <<'PY'
import sys

data = open(sys.argv[1], "rb").read()
name = sys.argv[2].encode()
# `draw` writes `\x1b[?25l` (hide the cursor) once, before the first frame,
# so the piece ahead of the first real `\x1b[?2026h\x1b[H` is that preamble,
# not a frame — dropped with `[1:]` rather than filtered for being
# non-empty, since the preamble is itself a few bytes long and would
# otherwise pass.
frames = data.split(b"\x1b[?2026h\x1b[H")[1:]
# frames[0..2] are the opening frame and the two `h` presses, in order —
# the first press shows done groups, so the group first appears in frames[1],
# and the second press hides them again, so the last frame must not carry it.
ok = len(frames) >= 3 and name not in frames[0] and name in frames[1] and name not in frames[-1]
sys.exit(0 if ok else 1)
PY
then
  ok "\`h\` shows the archived group under \`done\` on the first press and hides it again on the second"
else
  bad "\`h\` shows the archived group under \`done\` on the first press and hides it again on the second"
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
  sed -i 's|^blocked_agent = .*|blocked_agent = "from-the-worktree"|' \
  "$WT/.spoolway/config.toml"
must "committing the worktree's own config" \
  git -C "$WT" add .spoolway/config.toml
must "committing the worktree's own config" \
  git -C "$WT" commit -qm "e2e: a config value only this worktree has"

says "config get in the worktree reads its own value" "from-the-worktree" \
  "$SPOOLWAY" -C "$WT" config get unattended.blocked_agent
silent_about "config get in the main checkout does not see it" "from-the-worktree" \
  "$SPOOLWAY" config get unattended.blocked_agent
says "config path in the worktree names its own setup folder, not the project's" \
  "setup:     $WT/.spoolway" "$SPOOLWAY" -C "$WT" config path

BEFORE_ROOT=$(cat .spoolway/config.toml)
BEFORE_WT=$(cat "$WT/.spoolway/config.toml")
OUT=$("$SPOOLWAY" -C "$WT" config set unattended.blocked_agent "should-not-land" 2>&1)
STATUS=$?
if [ "$STATUS" -ne 0 ]; then ok "config set in the worktree exits non-zero"
else bad "config set in the worktree exits non-zero"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF "the dispatcher reads the project's config, not this worktree's." <<<"$OUT"; then
  ok "and says why"
else bad "and says why"; sed 's/^/        /' <<<"$OUT"; fi
if grep -qF -- "-C $PROJECT config set unattended.blocked_agent should-not-land" <<<"$OUT"; then
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
  echo "---"; cat "$BODY"
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

# ------------------------------------------------------ config set checks
# `config set` runs the file-based checks `doctor` runs. A value that can
# never be right is refused, and one naming something not set up yet is saved
# with a warning, so a script can set keys in the order it likes.
BEFORE_BACKEND=$("$SPOOLWAY" config get dispatch.backend)
refuses "config set refuses a backend that does not exist, naming the allowed ones" \
  "not a backend — use herdr or headless" "$SPOOLWAY" config set dispatch.backend tmux
works "and the backend is still what it was" \
  test "$("$SPOOLWAY" config get dispatch.backend)" = "$BEFORE_BACKEND"

BEFORE_AGENT=$("$SPOOLWAY" config get unattended.blocked_agent)
says "config set saves an agent not in [agents] yet, with a warning" \
  'warning: no agent `ghost` in [agents] yet — `spoolway doctor` fails until one is added' \
  "$SPOOLWAY" config set unattended.blocked_agent ghost
works "and the value saved is the one typed" \
  test "$("$SPOOLWAY" config get unattended.blocked_agent)" = "ghost"
refuses "and doctor fails on it, as warned" 'no agent profile `ghost`' \
  "$SPOOLWAY" doctor --no-live
must "blocked agent restored" "$SPOOLWAY" config set unattended.blocked_agent "$BEFORE_AGENT"

refuses "config set refuses a hook that can never run, as doctor fails it" \
  "not a bare filename" "$SPOOLWAY" config set issue_tracking.hook ../evil.sh
BEFORE_HOOK=$("$SPOOLWAY" config get issue_tracking.hook)
says "config set warns on a hook script that is not there yet" \
  'fails its check "`issue_tracking.hook` script exists"' \
  "$SPOOLWAY" config set issue_tracking.hook nosuch.sh
must "hook restored" "$SPOOLWAY" config set issue_tracking.hook "$BEFORE_HOOK"

# Calls that run at the same time, as an agent issuing several tool calls at
# once does, each save their own key. A call that lost the race may be
# refused, but none may print success and leave its value out of the file.
PAR_DIR="$LIVE/config-set-parallel"
mkdir -p "$PAR_DIR"
for i in 1 2 3 4 5 6 7 8; do
  ( "$SPOOLWAY" config set "models.par$i.input" "$i.5" >"$PAR_DIR/set-$i.out" 2>&1
    echo $? >"$PAR_DIR/set-$i.status" ) &
done
wait
for i in 1 2 3 4 5 6 7 8; do
  if [ "$(cat "$PAR_DIR/set-$i.status")" != 0 ] \
    || [ "$("$SPOOLWAY" config get "models.par$i.input" 2>/dev/null)" = "$i.5" ]; then
    ok "parallel config set: models.par$i.input is saved, or its call was refused"
  else
    bad "parallel config set: models.par$i.input was reported saved but is not in config.toml"
    sed 's/^/        /' "$PAR_DIR/set-$i.out"
  fi
done

# The same for `pipeline override --set`, three keys of one pipeline's patch.
PAR_STEPS=(implement review document)
for n in 0 1 2; do
  ( "$SPOOLWAY" pipeline override default --set "${PAR_STEPS[$n]}.model=par-model-$n" \
      >"$PAR_DIR/ovr-$n.out" 2>&1
    echo $? >"$PAR_DIR/ovr-$n.status" ) &
done
wait
PAR_PATCH="$SPOOLWAY_PROJECT_HOME/overrides/pipelines/default.yml"
for n in 0 1 2; do
  if [ "$(cat "$PAR_DIR/ovr-$n.status")" != 0 ] \
    || grep -qF "par-model-$n" "$PAR_PATCH" 2>/dev/null; then
    ok "parallel pipeline override --set: ${PAR_STEPS[$n]}.model is saved, or its call was refused"
  else
    bad "parallel pipeline override --set: ${PAR_STEPS[$n]}.model was reported saved but is not in the patch"
    sed 's/^/        /' "$PAR_DIR/ovr-$n.out"
  fi
done
rm -rf "$SPOOLWAY_PROJECT_HOME/overrides"

# ------------------------------------------------------------- sync panel
# A checkout behind the binary is told nothing in front of a command: no line
# on stderr, no stamp file. Only `spoolway sync` and `spoolway doctor` speak
# of it. A deleted skill stands in for "behind" — it is a file `sync` would
# write — and `override list` is the command under test, its own output
# ("no overrides") being fixed whatever this suite queued earlier.
SYNC_LINE="Run spoolway sync to apply the last update."
STALE_SKILL=.claude/skills/spoolway-config/SKILL.md

behind_checkout() {
  rm -f "$STALE_SKILL"
}

# Driven under a real terminal on stdout and stderr, which is the only place
# the old line ever printed. `write_pty_driver` (`lib.sh`) gives the process
# one without needing one behind this suite's own process.
PTY_DRIVER="$LIVE/sync-notice-pty.py"
write_pty_driver "$PTY_DRIVER"

behind_checkout
TTY_OUT=$(python3 "$PTY_DRIVER" "$SPOOLWAY" override list 2>&1)
TTY_STATUS=$?
if [ "$TTY_STATUS" -eq 0 ]; then
  ok "at a terminal the command exits zero without a key pressed"
else
  bad "at a terminal the command exits zero without a key pressed (exit $TTY_STATUS)"
  sed 's/^/        /' <<<"$TTY_OUT"
fi
if grep -qF "$SYNC_LINE" <<<"$TTY_OUT"; then
  bad "a behind checkout prints no sync line"; sed 's/^/        /' <<<"$TTY_OUT"
else
  ok "a behind checkout prints no sync line"
fi
if grep -qF "no overrides" <<<"$TTY_OUT"; then
  ok "and the command's own output is there"
else
  bad "and the command's own output is there"
  sed 's/^/        /' <<<"$TTY_OUT"
fi
works "and nothing was written back" \
  test ! -e "$STALE_SKILL"
works "and no sync-stamp file exists" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/sync-stamp"

# `spoolway sync`'s own confirm panel, driven the same way: 522b247 put it
# behind a blank alternate screen because it draws first and takes the
# guard after, and nothing here ran it on a pty until now (gh-528). The
# driver waits for the panel's title before sending `esc`, so the read is
# of what a person would actually see, not a blind key into an empty
# screen.
behind_checkout
SYNC_OUT="$LIVE/sync-panel.out"
python3 "$PTY_DRIVER" --after "apply updates" --keys $'\033' \
  "$SPOOLWAY" sync >"$SYNC_OUT" 2>&1
has   "sync's panel reaches the terminal" \
      "new version installed, apply updates" "$SYNC_OUT"
lacks "and sync never enters the alternate screen" $'\033[?1049h' "$SYNC_OUT"
has   "esc under it writes nothing" "Nothing was changed." "$SYNC_OUT"

finish
