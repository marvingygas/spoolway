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

if [ -z "$opted_in" ]; then
  echo "  (every kind skipped — this suite runs only where a person has named the models)"
fi

finish
