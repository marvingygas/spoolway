#!/usr/bin/env bash
# The codex row, re-verified against the binary installed here.
#
# Every stand-in's launch contract is proved by construction, not by a suite:
# `agent-mock.sh` writes each one straight from `agent::ADAPTERS`, so a stand-in
# agrees with its row because there is only the one source for both. What
# nothing headless can say is
# whether the *binary* still does: the row was settled by hand against one
# version of the CLI (codex-cli 0.147.0 — `docs/agents.md` has the findings),
# and a version that changes its flag grammar shows up first as a person's plan
# run failing an hour in. This suite is the same shape as `warmth.sh` for the
# same reason, aimed at a kind that costs nothing to check against a local
# endpoint: one command — `spoolway agent verify codex --live` — which runs a
# real turn, reads every accounting clause back, then resumes the session once
# and asserts the resume spelling still means "continue".
#
# **And one commit, from a linked worktree.** `verify --live` runs in a plain
# repo and never commits, so it cannot see what a lane meets: spoolway cuts
# every lane a *linked* worktree, whose index and branch ref live in the main
# checkout's shared `.git`, outside the worktree codex's sandbox confines it
# to. The row grants that directory with `--add-dir {git_dir}` (see
# `agent::ADAPTERS` and `repo::git_dir`), and codex-cli 0.157.1 still refused
# the commit — its sandbox made `.git/worktrees/<name>` read-only — with every
# check here green. `commit_in_worktree` is that lane, minus the dispatcher:
# codex launched with the row's own flags, in a `git worktree add` checkout,
# asked to commit, and failed by name when the commit is refused.
#
# **Opt-in, by naming a model.** spoolway names no model of its own, and this
# suite follows: with no model named the check is skipped, with the variable
# to set. That is also the answer to whether nightly machines must carry this
# CLI — they must not; the suite verifies wherever a person has set it up, and
# requires nothing anywhere else. A model named while the binary is missing IS
# a failure: that machine claimed this coverage and lost it.
#
# **Cost.** Nothing, pointed at a local endpoint — which is what the model name
# is expected to resolve to through the CLI's own provider config (codex wants
# `wire_api = "responses"`). The endpoint is not probed here because only that
# config knows where it is; a model that resolves to a cloud provider spends
# two turns of it, which is why naming the model is the opt-in.
#
# **It uses your real `$HOME`.** It has to: the real binary needs its own
# credentials and provider config. What the check writes is a scratch tree of
# its own making and a per-session home under `~/.spoolway`, same as any lane.
#
# covers: agent.verify.live — codex against the real binary: a turn, a resume, every reading named
# covers: agent.live.worktree_commit — codex, launched with the row's sandbox and grants, commits from a linked worktree
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"

LIVE=${WORK:-$(mktemp -d)}
mkdir -p "$LIVE"

opted_in=

# One kind: run the live verify, then assert the readings by name — so the day
# one goes red, the line says which clause of the row stopped being true.
verify_live() {
  local kind=$1 model=$2 var=$3
  local out="$LIVE/verify-$kind.txt"
  if [ -z "$model" ]; then
    echo "  skipped — set $var to a model \`$kind\`'s own config resolves (a local one spends nothing)"
    return 0
  fi
  opted_in=1
  if ! command -v "$kind" >/dev/null 2>&1; then
    bad "$kind: $var is set but no \`$kind\` is on PATH — this machine claims the coverage and cannot run it"
    return 0
  fi
  if "$SPOOLWAY" agent verify "$kind" --live --model "$model" > "$out" 2>&1; then
    ok "$kind: a real turn and a resumed one settle the whole row"
  else
    bad "$kind: a real turn and a resumed one settle the whole row"
    sed 's/^/        /' "$out"
    return 0
  fi
  has "$kind: the tokens come back off what the binary wrote" \
    "tokens read back" "$out"
  has "$kind: so does the mtime the reminder loop reads" \
    "mtime moved while the turn ran" "$out"
  has "$kind: and the running totals the ledger banks" \
    "running totals read back" "$out"
  has "$kind: and the resume spelling still continues the session" \
    "the resumed turn continued the same session" "$out"
}

verify_live codex "${SPOOLWAY_E2E_CODEX_MODEL:-}" SPOOLWAY_E2E_CODEX_MODEL

# The worktree case's scratch tree, outside the system temp directory on
# purpose: codex's `workspace-write` sandbox leaves `/tmp` and `$TMPDIR`
# writable by default, so a repo under `$LIVE` would have its `.git` writable
# whatever the `--add-dir` grant said, and the case would pass on exactly the
# binary it exists to catch. A real lane's worktree and main checkout both sit
# under `$HOME`, so this does too. Taken back by the EXIT trap, which keeps
# everything `lib.sh`'s own trap did.
WT_ROOT=
live_cleanup() {
  [ -n "$WT_ROOT" ] && rm -rf "$WT_ROOT"
  screen_stop
  dispatcher_stop
  record_results
}
trap live_cleanup EXIT

# A main checkout with one commit, and a linked worktree of it on its own
# branch — the shape `cut_worktree` gives every lane. The identity and the
# signing switch are the repo's own, so the person's global git config can
# neither refuse the commit for a reason of its own nor sign it through an
# agent socket the sandbox cannot reach.
worktree_fixture() {
  local main=$1 lane=$2
  git init -q "$main" &&
    git -C "$main" config user.name "spoolway e2e" &&
    git -C "$main" config user.email "e2e@spoolway.invalid" &&
    git -C "$main" config commit.gpgsign false &&
    git -C "$main" commit -q --allow-empty -m "base" &&
    git -C "$main" worktree add -q -b lane "$lane"
}

# One kind, one turn, in a linked worktree: launched with the argv spoolway
# composes for a headless lane — the codex row's `args`, its permission flag,
# then `exec` and the prompt — and a per-session `CODEX_HOME` seeded the way
# `agent::prepare_session_home` seeds one. The flags are restated here
# because no command prints them; the row in `src/agent.rs` is the source,
# and a flag it gains or drops belongs here too.
commit_in_worktree() {
  local kind=$1 model=$2
  # `verify_live` above already said why: skipped with no model, and a model
  # with no binary is its failure, counted once.
  [ -n "$model" ] || return 0
  command -v "$kind" >/dev/null 2>&1 || return 0

  local cache=${XDG_CACHE_HOME:-$HOME/.cache}
  mkdir -p "$cache"
  WT_ROOT=$(mktemp -d "$cache/spoolway-e2e-live.XXXXXX")
  # `state` and `project` stand in for `{state_dir}` and `{project_home}`,
  # the two read grants a lane is given beside the one this case is about.
  local main="$WT_ROOT/main" lane="$WT_ROOT/lane"
  local state="$WT_ROOT/state" project="$WT_ROOT/project"
  local home="$WT_ROOT/$kind-home" out="$LIVE/worktree-$kind.txt"
  local real_home=${CODEX_HOME:-$HOME/.codex}
  must "$kind: a main checkout and a linked worktree of it" \
    worktree_fixture "$main" "$lane"
  mkdir -p "$state" "$project" "$home"

  # `{git_dir}` exactly as `repo::git_dir` resolves it: the common dir, which
  # holds the objects, the branch ref and `worktrees/<name>` with its index.
  local git_dir
  git_dir=$(git -C "$lane" rev-parse --path-format=absolute --git-common-dir)

  # The seed: credentials linked back, the config copied with this worktree
  # trusted in it. The directory is a fresh `mktemp` name, so the table
  # appended cannot already be in the person's file.
  [ -e "$real_home/auth.json" ] && ln -s "$real_home/auth.json" "$home/auth.json"
  { [ -e "$real_home/config.toml" ] && cat "$real_home/config.toml"
    printf '\n[projects."%s"]\ntrust_level = "trusted"\n' "$lane"
  } > "$home/config.toml"

  printf '%s\n' \
    "You work in one git worktree. Do what the message asks with your shell" \
    "tool, exactly as written, then reply in one short line." > "$state/prompt.md"

  local before after status
  before=$(git -C "$lane" rev-parse HEAD)
  ( cd "$lane" && CODEX_HOME="$home" timeout 600 "$kind" \
      --model "$model" \
      -c "model_instructions_file=$state/prompt.md" \
      --sandbox workspace-write \
      --add-dir "$state" \
      --add-dir "$project" \
      --add-dir "$git_dir" \
      -c check_for_update_on_startup=false \
      --ask-for-approval never \
      exec "Run these three commands in the current directory, in order, and nothing else:
printf 'committed from a linked worktree\n' > proof.txt
git add proof.txt
git commit -m 'live: a commit from a linked worktree'
If one of them fails, do not work around it: reply with the command and its error, verbatim." \
      < /dev/null ) > "$out" 2>&1
  status=$?
  after=$(git -C "$lane" rev-parse HEAD)

  if [ "$after" != "$before" ] && git -C "$lane" cat-file -e HEAD:proof.txt 2>/dev/null; then
    ok "$kind: a lane in a linked worktree commits through the row's own sandbox grants"
    return 0
  fi
  # Which of the three commands stopped, read off the worktree rather than
  # off what the model said about it: no file is a turn that never reached
  # git, a file not in the index is `git add` refused (the objects or the
  # index), and a staged file with HEAD unmoved is `git commit` refused (the
  # branch ref or the worktree's own HEAD).
  if [ "$status" -eq 124 ]; then
    bad "$kind: a lane in a linked worktree commits — the turn was killed after 600s, before it finished"
  elif [ ! -e "$lane/proof.txt" ]; then
    bad "$kind: a lane in a linked worktree commits — the turn (exit $status) never wrote proof.txt, so it never reached git"
  elif git -C "$lane" diff --cached --quiet -- proof.txt; then
    bad "$kind: a lane in a linked worktree commits — \`git add\` was refused, though --add-dir $git_dir grants the shared git dir"
  else
    bad "$kind: a lane in a linked worktree commits — \`git commit\` was refused, though --add-dir $git_dir grants the shared git dir"
  fi
  # The refusal itself first, when git or the sandbox named one, so the line
  # that says why is not buried in the turn's whole transcript.
  grep -iE "read-only|not permitted|permission denied|cannot lock|unable to (create|write)|index\.lock" "$out" \
    | head -10 | sed 's/^/        refused: /'
  tail -40 "$out" | sed 's/^/        /'
}

commit_in_worktree codex "${SPOOLWAY_E2E_CODEX_MODEL:-}"

if [ -z "$opted_in" ]; then
  echo "  (every kind skipped — this suite runs only where a person has named the models)"
fi

finish
