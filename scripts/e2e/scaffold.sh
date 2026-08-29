#!/usr/bin/env bash
# Build a throwaway project for one plan to run in, and tear it down again.
#
# This is scripts/e2e-sandbox.sh rewritten around a plan. The sandbox scaffolded
# a project and left you to think of something to do with it; a scaffold takes
# the plan first — `--plan escalation` builds a project configured for
# scripts/e2e/plans/escalation.html and nothing else, because a plan that
# watches the reminder loop and a plan that watches a gate want different
# installations and the difference is the plan's to state.
#
#   scripts/e2e/scaffold.sh --plan escalation      build it
#   scripts/e2e/scaffold.sh --plan escalation --headless
#                                                  no multiplexer: lanes are processes
#   scripts/e2e/scaffold.sh --plan escalation --reset
#                                                  tear down and build again
#   scripts/e2e/scaffold.sh --plan escalation --clean
#                                                  tear down and stop: leave nothing behind
#   scripts/e2e/scaffold.sh --list                 the plans there are
#
# What it builds: a seed repo of a few files, a local forge (a bare repo as
# `origin` plus scripts/e2e-fake-gh.sh as `gh`), the `end-to-end` pipeline from
# scripts/e2e/runtime/ with every step pointed at this machine's local model,
# the observer beside it, and whatever `config:` lines the plan itself asks for.
# Then it prints the two commands that queue the plan and start the dispatcher.
#
# The model preflight is here rather than sourced. It used to live in
# scripts/e2e/agent-real.sh, which the suites shared — and the suites do not
# spend a model any more, so there is nobody left to share it with. What
# survived is exactly what a plan run needs: which model, where it is served,
# and an honest refusal when it is not.
#
# A run leaves things on disk and in the multiplexer — a checkout, a forge,
# worktrees and panes — so tearing it down is a real step, not an `rm -rf`.
# Finish a plan run with `--clean`.
#
# Env:
#   SPOOLWAY_E2E_DIR         where the project goes  (default ~/dev/project/spoolway-e2e-<plan>)
#   SPOOLWAY_E2E_MODEL       the local model         (default Qwen3.6-35B-A3B)
#   SPOOLWAY_E2E_MODEL_URL   where it is served      (default: read from pi's registry)
#   SPOOLWAY_E2E_WORKERS     lanes at once           (default 3)
#   SPOOLWAY_E2E_CTX         context window, tokens  (default 100000)
#   SPOOLWAY_E2E_BRANCH      plan branch             (default plan/<plan>)
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPTS=$(cd "$HERE/.." && pwd)
PLANS="$HERE/plans"
RUNTIME="$HERE/runtime"

MODEL=${SPOOLWAY_E2E_MODEL:-Qwen3.6-35B-A3B}
WORKERS=${SPOOLWAY_E2E_WORKERS:-3}
CTX=${SPOOLWAY_E2E_CTX:-100000}

PLAN=""
RESET=0
CLEAN=0
HEADLESS=0

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die() { echo "$*" >&2; exit 1; }

list_plans() {
  printf 'plans:\n'
  for file in "$PLANS"/*.html; do
    [ -e "$file" ] || { printf '  (none yet)\n'; return 0; }
    # The first line of the plan's own coverage block, which says what the plan
    # is for. Read out of the file rather than kept in a table here: a second
    # list of what each plan covers is a list that goes wrong.
    printf '  %-14s %s\n' "$(basename "$file" .html)" \
      "$(sed -n 's/^ *covers: *//p' "$file" | head -1)"
  done
}

while [ $# -gt 0 ]; do
  case "$1" in
    --plan)     PLAN=$2; shift 2 ;;
    --reset)    RESET=1; shift ;;
    --clean)    CLEAN=1; shift ;;
    --headless) HEADLESS=1; shift ;;
    --list)     list_plans; exit 0 ;;
    -h|--help)  sed -n '2,45p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) die "unknown flag: $1" ;;
  esac
done

[ -n "$PLAN" ] || die "which plan? \`--plan <name>\`; \`--list\` says what there is"
PLAN_FILE="$PLANS/$PLAN.html"
[ -f "$PLAN_FILE" ] || die "no such plan: $PLAN_FILE (\`--list\` says what there is)"

DIR=${SPOOLWAY_E2E_DIR:-$HOME/dev/project/spoolway-e2e-$PLAN}
BRANCH=${SPOOLWAY_E2E_BRANCH:-plan/$PLAN}
# Beside the project, never inside it: a bare repo under the checkout would be
# swept into the very commits under test.
FORGE="$DIR-forge"

# Where this project's own runtime state lives — see
# `crate::repo::Repo::home` — and where a lane's worktree is cut. Both
# backends' worktree locations, always: a teardown has to work without being
# told which one made the mess, and `--clean` is routinely run in a shell
# that never saw the `--headless` that built it.
PROJECT_HOME="$HOME/.spoolway/$(basename "$DIR")"
WORKTREES="$HOME/.herdr/worktrees/$(basename "$DIR")"
WORKTREES_HEADLESS="$PROJECT_HOME/worktrees"

# `clean` is one `rm -rf` away from being the worst line in this repo, and $DIR
# comes from the environment. Refuse anything that is not a directory of its
# own, well below home.
case "$DIR" in
  "$HOME" | "$HOME/" | / | "" | */..*)
    die "refusing to treat '$DIR' as a project directory" ;;
  /*/*/*) ;;
  *) die "SPOOLWAY_E2E_DIR must be an absolute path at least three deep: '$DIR'" ;;
esac

# ------------------------------------------------------------------- teardown

# Close every lane and workspace this project opened in the multiplexer.
#
# Two ownership tests, and both are needed. A *workspace* is this project's when
# its checkout is under it — matched on the path and never on a label, since a
# label is a name anyone may have reused. A *pane* is this project's only when
# it hosts a named agent session: `herdr agent list` lists every pane, and
# `name` is null for every session spoolway did not start, which is exactly what
# a person's own shell in the same directory looks like.
clean_mux() {
  command -v herdr >/dev/null 2>&1 || return 0
  herdr agent list >/dev/null 2>&1 || return 0
  command -v jq >/dev/null 2>&1 || {
    echo "no jq — leaving the multiplexer's workspaces alone" >&2; return 0; }

  # `$root` is bound by name before the inner pipe: `$p | startswith(. + "/")`
  # re-binds `.` to `$p` inside the pipe, so the first version of this helper
  # compared every path against itself and never matched anything — every
  # sweep below was a silent no-op, which is how a whole dispatch workspace
  # of dead panes outlived `--clean`.
  local under='def under: . as $p
                 | any($ARGS.positional[];
                       . as $root
                       | $p == $root or ($p | startswith($root + "/")));'

  herdr agent list \
    | jq -r --args "$under"'
        .result.agents[]
        | select(.name != null and .agent != null)
        | select(.cwd != null and (.cwd | under))
        | .pane_id
      ' "$DIR" "$WORKTREES" "$WORKTREES_HEADLESS" 2>/dev/null \
    | while read -r pane; do
        say "closing lane pane $pane"
        herdr pane close "$pane" >/dev/null 2>&1 || true
      done

  herdr workspace list \
    | jq -r --args "$under"'
        .result.workspaces[]
        | select(.worktree != null and (.worktree.checkout_path | under))
        | .workspace_id
      ' "$DIR" "$WORKTREES" 2>/dev/null \
    | while read -r ws; do
        say "closing workspace $ws"
        herdr workspace close "$ws" >/dev/null 2>&1 || true
      done

  # The dispatcher's own workspace — `spoolway-dispatcher`, see
  # `mux::DISPATCH_WORKSPACE_LABEL` — holds no herdr worktree, so the match
  # above never finds it, and its base panes hold no agent, so the lane sweep
  # misses them too: a whole workspace of shells sitting in deleted
  # directories used to survive every `--clean`. Its panes' directories are
  # what tie it to *this* plan rather than to another project dispatching at
  # the same time; a directory already gone reads back with ` (deleted)`
  # appended, which is stripped before the match.
  herdr workspace list \
    | jq -r '.result.workspaces[]
             | select(.label == "spoolway-dispatcher")
             | .workspace_id' 2>/dev/null \
    | while read -r ws; do
        inside=$(herdr pane list --workspace "$ws" \
          | jq -r --args "$under"'
              [.result.panes[]
               | select(.cwd != null
                        and ((.cwd | sub(" \\(deleted\\)$"; "")) | under))]
              | length
            ' "$DIR" "$WORKTREES" "$WORKTREES_HEADLESS" "$PROJECT_HOME" 2>/dev/null)
        [ "${inside:-0}" -gt 0 ] || continue
        say "closing dispatch workspace $ws"
        herdr workspace close "$ws" >/dev/null 2>&1 || true
      done
}

# Kill anything this project left running with no multiplexer to ask: headless
# lanes, and the observer, which is a detached process by construction and the
# one thing here that nothing else would ever stop.
clean_processes() {
  local pidfile pid
  for pidfile in "$PROJECT_HOME/headless"/*.pid "$PROJECT_HOME/commands"/*.pid; do
    [ -f "$pidfile" ] || continue
    pid=$(cat "$pidfile" 2>/dev/null) || continue
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null || continue
    say "killing $(basename "$pidfile" .pid) (pid $pid)"
    kill -TERM "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
  done
  sleep 1
  for pidfile in "$PROJECT_HOME/headless"/*.pid "$PROJECT_HOME/commands"/*.pid; do
    [ -f "$pidfile" ] || continue
    pid=$(cat "$pidfile" 2>/dev/null) || continue
    [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null || continue
    kill -KILL "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null || true
  done
}

# The one thing a plan run makes that is worth more than the fixture: the
# observer's minute-by-minute record of what the panes did.
#
# It is written inside the checkout, so `clean` used to take it — and it is the
# only artefact of a plan run that a person would ever want to read again, since
# the whole point of a plan is to see what a real model does and the tasks,
# branches and worktrees are all disposable. Kept under the harness's own
# directory, stamped, so several runs of one plan accumulate rather than
# overwrite.
KEPT="${SPOOLWAY_E2E_KEEP:-$HOME/.spoolway/e2e-observations}"

keep_observations() {
  local record="$DIR/observations.md" destination
  [ -s "$record" ] || return 0
  mkdir -p "$KEPT" || return 0
  destination="$KEPT/$PLAN-$(date -u +%Y%m%dT%H%M%SZ).md"
  cp "$record" "$destination" 2>/dev/null || return 0
  say "kept the observer's record: $destination"
}

clean() {
  clean_mux
  clean_processes
  keep_observations
  if [ -e "$DIR" ] || [ -e "$WORKTREES" ] || [ -e "$WORKTREES_HEADLESS" ] || [ -e "$FORGE" ]; then
    say "removing $DIR, its worktrees and $FORGE"
    rm -rf "$DIR" "$WORKTREES" "$WORKTREES_HEADLESS" "$FORGE"
  fi
  # `$PROJECT_HOME` is the queue, the archive and every other piece of this
  # plan's runtime state — none of it under `$DIR`, by design, so removing
  # the checkout above never touched it. Left standing, it is a dead `init`
  # registration for the next run to reclaim rather than a clean slate.
  if [ -e "$PROJECT_HOME" ]; then
    say "removing $PROJECT_HOME"
    rm -rf "$PROJECT_HOME"
  fi
}

if [ "$CLEAN" = 1 ]; then
  clean
  say "nothing left of $PLAN"
  exit 0
fi

# ------------------------------------------------------------------- preflight
#
# What a plan run needs before it is worth building anything: git, a spoolway on
# PATH, the agent binary a local lane runs, and a model server that has actually
# heard of the model the lanes will name. A run that discovers the last one an
# hour in is an hour of nothing.

command -v git      >/dev/null || die "git not found"
command -v spoolway >/dev/null || die "spoolway not on PATH — cargo build --release && cp target/release/spoolway ~/.local/bin/"
command -v pi       >/dev/null || die "no \`pi\` on PATH — a local lane runs the binary its agent profile names"
[ -f "$SCRIPTS/e2e-fake-gh.sh" ] || die "missing $SCRIPTS/e2e-fake-gh.sh — the forge double"

# Where the local model is actually served, read rather than guessed. pi
# resolves a model id through its own provider registry, so that file is the
# only thing that knows which port `$MODEL` means — and a lane pointed at a
# server that is not there fails as a wall of retries with no port in it.
model_url() {
  if [ -n "${SPOOLWAY_E2E_MODEL_URL:-}" ]; then echo "$SPOOLWAY_E2E_MODEL_URL"; return 0; fi
  local registry=$HOME/.pi/agent/models.json url
  [ -f "$registry" ] || die "no provider registry at $registry — set SPOOLWAY_E2E_MODEL_URL"
  command -v jq >/dev/null 2>&1 || die "no jq to read $registry — set SPOOLWAY_E2E_MODEL_URL"
  url=$(jq -r --arg m "$MODEL" '
          .providers | to_entries[] | .value
          | select(any(.models[]?; .id == $m)) | .baseUrl' "$registry" 2>/dev/null | head -1)
  [ -n "$url" ] && [ "$url" != null ] \
    || die "$registry serves no \`$MODEL\` — set SPOOLWAY_E2E_MODEL_URL, or SPOOLWAY_E2E_MODEL to one it has"
  echo "$url"
}

URL=$(model_url)
listed=$(curl -fsS -m 5 "$URL/models" 2>/dev/null) \
  || die "nothing is answering at $URL — start the model server first"
# A reachable endpoint proves only that the router is up: behind it every local
# model is fronted at one port. Ask whether it knows this one. Whether it is
# loaded right now is deliberately not asked — the router loads on demand.
if command -v jq >/dev/null 2>&1; then
  echo "$listed" | jq -e --arg m "$MODEL" \
    'any(.data[]?; .id == $m or (.aliases // [] | index($m)))' >/dev/null 2>&1 \
    || die "$URL serves no \`$MODEL\` — check models.ini, or set SPOOLWAY_E2E_MODEL to one it lists"
fi

[ "$RESET" = 1 ] && clean
[ -e "$DIR" ]   && die "$DIR already exists — pass --reset to rebuild it"
[ -e "$FORGE" ] && die "$FORGE already exists — pass --reset to rebuild it"

# ------------------------------------------------------------------ the project

say "seeding $DIR"
mkdir -p "$DIR/src" "$DIR/docs"
cd "$DIR"

# A seed rather than a fixture. What a plan run is watching is the multiplexer,
# so the project underneath only has to be a real repository with somewhere
# obvious to put a change — a toolchain here would be an installation cost paid
# by every plan and read by none of them.
cat > src/main.rs <<'SEED'
fn main() {
    println!("tinytool");
}
SEED
cat > docs/cli.md <<'SEED'
<script type="application/json" id="spoolway-domain">
{ "domain": "cli", "covers": ["src/**"] }
</script>
# CLI

`tinytool` prints its own name. Everything a task adds goes under `src/`, and
this document is what the closeout brings back into line with it.
SEED
cat > notes.md <<'SEED'
# Notes

A file any task may append a line to. Two tasks appending to it are in each
other's way on purpose — that is what a conflict looks like from the outside.
SEED
# The observer writes here, in the main checkout rather than in any worktree, so
# no lane ever sees it and `auto_commit` never sweeps it into a change.
cat > .gitignore <<'SEED'
observations.md
SEED

git init -q -b main .
git config user.email spoolway@example.invalid
git config user.name  "spoolway end-to-end"
git add -A
git commit -qm "tinytool: the seed this plan runs against"

say "building the local forge at $FORGE"
mkdir -p "$FORGE/bin" "$FORGE/prs"
git init -q --bare -b main "$FORGE/origin.git"
install -m 755 "$SCRIPTS/e2e-fake-gh.sh" "$FORGE/bin/gh"
git remote add origin "$FORGE/origin.git"
git push -q -u origin main

say "spoolway init"
# The skills come with it: `init` installs them for the provider it was given,
# and with no terminal to ask it takes claude — which is what this scaffold
# wanted anyway and used to ask for on its own line.
spoolway init >/dev/null

# ------------------------------------------------------------------- the runtime

say "installing the end-to-end pipeline"
# The only pipeline this project has. The shipped ones go: `default` names a
# cloud model on its review step, and a plan run that quietly spent one would be
# a plan nobody queues twice.
rm -f .spoolway/pipelines/default.yml .spoolway/pipelines/bugfix.yml
cp "$RUNTIME/end-to-end.yml" .spoolway/pipelines/end-to-end.yml
cp -R "$RUNTIME/prompts/." .spoolway/prompts/
install -m 755 "$HERE/observe.sh" .spoolway/observe.sh

# Every step's model, by name. The pipeline ships the placeholder every shipped
# pipeline does, and this is where it stops being a placeholder — checked
# afterwards, because a lane started against a model no server serves fails a
# long way from the cause.
sed -i "s|^\([[:space:]]\+\)model:[[:space:]]*your-local-model[[:space:]]*$|\1model: $MODEL|" \
  .spoolway/pipelines/end-to-end.yml
if grep -q "your-local-model" .spoolway/pipelines/end-to-end.yml; then
  die "a model placeholder survived in the pipeline — has runtime/end-to-end.yml changed?"
fi

# `handover` is `spoolway stack` now, not a prompt with `gh` spelled into its
# prose — so pointing it at the local forge is the same seam the headless
# suites use: `SPOOLWAY_GH`, an env var `spoolway stack` reads at run time
# rather than a file this script edits. The printed instructions below export
# it before `spoolway dispatch`, so every lane's `handover` step inherits it.

# --------------------------------------------------------------------- config

say "configuring $MODEL, $WORKERS lanes, ${CTX} tokens"
spoolway config set dispatch.default_pipeline end-to-end
spoolway config set agents.pi.concurrency "$WORKERS"
# A window belongs to the model rather than to the profile that launches it, so
# it is keyed by model name. Unquoted even though a real name has dots in it:
# the key is split on its *last* dot, so everything between `models.` and
# `.context_window` is the name.
spoolway config set "models.$MODEL.context_window" "$CTX"
# Short, because somebody is watching this happen.
spoolway config set dispatch.interval 10s

# `blocked` is no longer a step end-to-end.yml declares; `Pipelines::assemble`
# materialises it from these four keys instead. Kept on `pi` and this
# project's own local model, same as every other step here — the shipped
# `claude`/`claude-opus-5` default would spend a real cloud model on a plan
# run whose whole point is watching the multiplexer for free.
spoolway config set unattended.blocked_agent pi
spoolway config set unattended.blocked_prompt unblocker
spoolway config set unattended.blocked_model "$MODEL"
spoolway config set unattended.blocked_session true

if [ "$HEADLESS" = 1 ]; then
  spoolway config set dispatch.backend headless
  spoolway config set dispatch.worktree_root "$WORKTREES_HEADLESS"
fi

# What the plan itself asks for. Every `config: <key> <value>` line in the plan
# file is applied here, which is how one plan runs with a dispatch interval of
# ten seconds and another with a session share of 5% without either of them
# needing a flag on this script. The plan says which settings it is reaching and
# this is the reaching.
while read -r key value; do
  [ -n "$key" ] || continue
  say "the plan asks for: $key = $value"
  spoolway config set "$key" -- "$value"
# The coverage block is a `<pre>` in a page, so the last line of it carries the
# closing tag. Taken off here rather than left to whoever reads the value: a
# `dispatch.interval` of `10s</pre>` is refused, and a value that takes any
# string at all would swallow one silently, which is worse.
done < <(sed -n 's/^ *config: *//p' "$PLAN_FILE" | sed 's#</pre>##')

git add -A
git commit -qm "Set up spoolway for the $PLAN plan"
git push -q origin main

say "branching $BRANCH"
git checkout -q -b "$BRANCH"
git push -q -u origin "$BRANCH"

say "checking what was installed"
spoolway pipeline check
spoolway prompt check
spoolway doctor || true

cat <<EOF

Ready: $DIR (on $BRANCH)
  plan:   $PLAN_FILE
  model:  $MODEL at $URL, $WORKERS lane(s), ${CTX} tokens each
  forge:  $FORGE (a gap in the forge never stops a run — read $FORGE/warnings.log after)
  panes:  $([ "$HEADLESS" = 1 ] && echo "none — headless, lanes are processes and logs" || echo "herdr")

  1. cd $DIR
  2. $HERE/queue-plan.sh $PLAN_FILE       (or: spoolway queue, to queue it from the screen)
  3. export SPOOLWAY_GH="$FORGE/bin/gh"  (so \`handover\`'s \`spoolway stack\` finds the local forge)
  4. spoolway dispatch
  5. watch the panes; read observations.md as it fills
  6. $HERE/scaffold.sh --plan $PLAN --clean

  --clean keeps observations.md under $KEPT before it removes anything.
EOF
