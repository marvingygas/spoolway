#!/usr/bin/env bash
# `models.<glob>.prompt_cache_ttl`, against a real cloud model. The one
# suite that spends money.
#
# Everything else in this harness runs stand-ins, and everything else should:
# whether the dispatcher decides correctly is decidable from files and exit
# codes. This one cannot be. What it is about is the last link in a chain that
# nothing else can reach:
#
#     a real transcript → usage::session_age → carried_session → resume
#
# `dispatch::tests::carried_session_reports_which_of_the_four_misses_it_was`
# proves the same bound with a transcript the test writes to order — and a
# transcript a test writes is a transcript that agrees with spoolway's parser
# by construction. What it cannot say is whether a transcript *Claude Code
# actually writes today* still leaves spoolway anything to read at all: the age
# of a session is the timestamp of its last reply, read out of the transcript,
# and `read_turn`'s claude arm splits `usage.cache_creation` by lifetime, which
# is what the billed cost of a cache write is priced off. If either shape ever
# moved, nothing anywhere would go red — and a missing reply timestamp is
# worse, since age then falls back to the modified time, which with real waits
# is nearly the same and would keep every check here green. `check_shape` below
# is what stands in the gap for both.
#
# And it has to be a *cloud* model for the same reason it always did: `claude`
# is the one kind this project ever spends real money running headlessly, and
# the shape worth watching is its transcript's, not pi's.
#
# **Real waits.** Age is never faked. The suite never edits a transcript at
# all: every byte stays as Claude Code wrote it, and a session is old because
# the suite waited. That is why a run takes about ten minutes:
#
#     0:00  a real lane runs one turn and holds at the first gate
#     2:00  resume → the same session, and its first turn reads the cache
#           the lane settles at a second gate
#     8:00  resume → a fresh session, because the default five minutes have
#           passed since the last reply
#
# The suite checks spoolway's call at eight minutes and does not assert the
# cache is cold then: Claude Code often writes to the one-hour cache, which is
# still warm. What the two-minute check adds is Anthropic's own
# `cache_read_input_tokens`, so a pass shows a warm resume really hit the
# cache. A failure prints the turn's usage, which tells an early eviction on
# Anthropic's side from a bug here.
#
# **Cost.** Five turns of `claude-haiku-4-5` — three through the pipeline
# scenario, two more through `agent verify --live` at the end — each a few
# thousand tokens against a fixture repo of two files. Fractions of a cent, and
# it is opt-in either way: nothing here runs unless `SPOOLWAY_E2E_CLOUD=1` is
# set. It is in no local tier.
#
# **It uses your real `$HOME`.** It has to: the real `claude` needs your
# credentials, and its transcripts land where it puts them. What it writes there
# is its own new sessions, in a project directory named after this suite's
# scratch tree — the same thing running `claude` in a new directory does. It
# edits nothing of yours, and nothing of its own either.
#
# covers: models.<glob>.prompt_cache_ttl — against a real claude session, aged by waiting: unset, a resume inside the 5m default continues the session and reads the cache, and one past it is refused
# covers: agent.verify.live — every transcript reading, off a real turn and a resumed one, named one by one
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

MODEL=${SPOOLWAY_E2E_CLOUD_MODEL:-claude-haiku-4-5-20251001}

# warmth_cleanup
#
# This is the one suite that runs `SPOOLWAY_E2E_REAL_HOME=1`: every other
# suite's project home is a scratch `$HOME` that `run.sh`'s own trap wipes
# whole, but this one's is `$HOME/.spoolway/proj-<id>`, under the real home,
# and nothing else ever removes it — the `rm -rf "$HOME"/.spoolway/proj-*`
# below, before `configure_project`, only clears a *previous* run's
# leftovers, on the way in.
#
# Replaces `lib.sh`'s own `trap dispatcher_stop EXIT` (the same override
# `board-pause.sh` makes for its own board process) rather than adding a
# second EXIT trap: `dispatcher_stop` still runs, first, exactly as it did.
# Installed before the opt-out `finish` calls below fire, since a failed
# `must` reaches the trap the same way a clean finish does and `$HOME` is
# untouched either way until `new_repo` runs — `${SPOOLWAY_PROJECT_HOME:-}`
# is read at the trap's own fire time, not this one, so it sees whatever
# `project_home_after_init` corrected it to once `configure_project` ran.
warmth_cleanup() {
  dispatcher_stop
  [ -n "${SPOOLWAY_PROJECT_HOME:-}" ] && rm -rf "$SPOOLWAY_PROJECT_HOME"
}
trap warmth_cleanup EXIT

# ------------------------------------------------------------------ opt in
if [ "${SPOOLWAY_E2E_CLOUD:-}" != 1 ]; then
  echo "  skipped — set SPOOLWAY_E2E_CLOUD=1 to spend $MODEL tokens on this one"
  finish
fi
REAL_CLAUDE=$(command -v claude 2>/dev/null)
if [ -z "$REAL_CLAUDE" ]; then
  echo "  skipped — no \`claude\` on PATH, and this suite is about the real one"
  finish
fi

LIVE=${WORK:-$(mktemp -d)}
CTL="$LIVE/ctl"
mkdir -p "$LIVE"

# `new_repo` defaults to a scratch `$HOME` for every other suite — see
# `fixture.sh` — but this one needs the genuine article: the real `claude`
# reads real credentials, and its transcripts have to land where it actually
# puts them.
SPOOLWAY_E2E_REAL_HOME=1

new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$CTL"
# The stand-in `pi` stays — no local step runs here, and leaving it in place
# keeps the fixture identical to every other suite's. The stand-in `claude` does
# not: this suite is about what the real binary writes, and the headless backend
# runs the binary its profile's `kind` names, so replacing the file on PATH is
# the whole of the swap.
ln -sf "$REAL_CLAUDE" "$LIVE/bin/claude"

# And the build under test, on the lane's own PATH.
#
# Every other suite runs stand-ins, and a stand-in calls `"$SPOOLWAY" report` by
# absolute path. A *real* lane runs the prompt's own words, and the prompt
# says `spoolway report` — which resolves through PATH, to whatever is installed
# on this machine. That is correct in production and wrong here: the first run
# of this suite reported to `~/.local/bin/spoolway`, a build that predated
# `paused`, so the gate this suite sequences itself with silently did not hold
# and every scenario measured a conversation that had never stopped.
ln -sf "$SPOOLWAY" "$LIVE/bin/spoolway"

# This is the one suite whose `spoolway init` claims `proj` under the *real*
# `~/.spoolway` — every other suite gets a scratch `$HOME` from `new_repo` —
# and the fixture it registers is a mktemp tree that is gone by the next run,
# with the previous run's own home still sitting behind wherever it landed.
new_repo "$LIVE/proj"
# What actually keeps this run clean is the sweep below. Every task this
# suite mints is named below (`waits`, `stuck`), so a stale
# `proj-*` home is this suite's own leavings and nothing else's: remove it
# before init writes the fresh one.
#
# A home is `<label>-<id>` now, and every run's `$LIVE` is a fresh `mktemp`
# checkout, so it mints a fresh id every time — there is no one path left to
# name here, only every home a `proj` label has ever answered to.
# `SPOOLWAY_PROJECT_HOME` (still the pre-init, id-less guess at this point)
# is no help either; glob the real family instead.
rm -rf "$HOME"/.spoolway/proj-*
configure_project plan/live
publish plan/live

# A real lane, so it needs to be able to run its tools without a person to
# approve them. `auto` stops at the first Bash call in a `--print` turn, and the
# one Bash call every lane has to make is `spoolway report`.
must "a lane that can run its own tools" \
  "$SPOOLWAY" config set agents.claude.permission_mode bypassPermissions
must "one cloud lane at a time"  "$SPOOLWAY" config set agents.claude.concurrency 1
# The window a share is a share of. Without it `carried_session` never sizes the
# session at all and every scenario here would fall out at `WindowUnset`,
# looking like an age result and being nothing of the kind.
must "the model's window"        "$SPOOLWAY" config set "models.$MODEL.context_window" 200000
must "a share nothing crosses"   "$SPOOLWAY" config set agents.claude.session_reuse_ctx 90

# Three visits by one prompt, with a gate after each of the first two.
#
# The gate is the sequencing, and it is the only way to get one: the decision
# under test is taken when the next lane starts, in the same pass that banks the
# previous lane's ledger line — so there is no gap between them for a suite to
# wait in. `gate: true` opens one that spoolway itself holds. The task parks on
# `paused` with the lane's spend banked, the suite waits a real while, and
# `spoolway resume` starts the next lane.
cat > .spoolway/pipelines/warmth.yml <<YAML
# Written by scripts/e2e/suites/warmth.sh. Three steps, one prompt, two gates.
steps:
  - id: first
    description: Write the note, and stop for a person.
    agent: claude
    prompt: builder
    model: $MODEL
    gate: true
    on_pass: second

  - id: second
    description: Come back to the same conversation about two minutes on, and stop again.
    agent: claude
    prompt: builder
    model: $MODEL
    session: true
    gate: true
    on_pass: third

  - id: third
    description: Come back after the five minutes have passed — and open fresh.
    agent: claude
    prompt: builder
    model: $MODEL
    session: true
    on_pass: done
YAML
must "the pipeline loads" "$SPOOLWAY" pipeline check

BODY="$LIVE/body.md"
cat > "$BODY" <<'TASKBODY'
## Goal

Append one line to `notes/<id>.md`, where `<id>` is this task's own id: today's
date and one short sentence about what this repository is.

## Non-goals

- changing any file outside `notes/`
- running any command other than the one that ends your turn

## Acceptance criteria

- `notes/<id>.md` exists and has at least one line in it

## References

- `docs/cli.md` — what this repository is
TASKBODY

# ------------------------------------------------------------------- helpers

# The session a step's lane actually ran in, out of the ledger — which is the
# same correlation `carried_session` makes, so reading it here is reading what
# the decision under test will read.
session_of() {
  local task=$1 step=${2:-first} line
  line=$(grep "\"task\":\"$task\"" "$SPOOLWAY_PROJECT_HOME/usage.jsonl" 2>/dev/null \
         | grep "\"step\":\"$step\"" | tail -1)
  [ -n "$line" ] || return 1
  sed -n 's/.*"session":"\([^"]*\)".*/\1/p' <<<"$line"
}

# Whether a step's spend has reached the ledger yet.
#
# Two different writers, and the suite has to wait for the second: the *lane*
# writes the stage when it reports, and the *dispatcher* banks the spend on the
# pass that parks it. Stopping the dispatcher the instant the stage says `paused`
# stops it before the ledger line exists — and then waiting for that line is
# waiting for something nothing is left running to do.
banked() {
  poll_until 90 grep -q "\"task\":\"$1\".*\"step\":\"$2\"" "$SPOOLWAY_PROJECT_HOME/usage.jsonl"
}

# Where the real claude put it. Never guessed at: both agents shard their
# sessions by an escaping of the lane's working directory, and the id spoolway
# minted is unambiguous wherever it landed — which is exactly what
# `usage::session_file` does too.
transcript_of() {
  find "$HOME/.claude/projects" -name "$1.jsonl" -print -quit 2>/dev/null
}

# Whether a step opened a fresh session, read off the task's own record.
#
# Not off the lane's log: a real `claude --print` writes its *answer* there and
# nothing about how it was launched, so the "this is your own session,
# continued" a stand-in echoes back has no equivalent here. The `new_sessions:`
# counter this used to read is retired — nothing writes it any more — and what
# replaced it is better anyway: a `session:` step that opens fresh writes one
# Status Log line naming the step and why (`- \`second\`: … — opened fresh`),
# and a carried session writes no such line at all. See `session_miss` in
# `dispatch::prepare_boot`.
#
# Read out of the archive, since a landed task has left the queue.
opened_fresh() {
  local task=$1 step=$2 what=$3 file="$SPOOLWAY_PROJECT_HOME/archive/$1.md"
  if grep -qE "\`$step\`.*opened fresh" "$file" 2>/dev/null; then
    ok "$what"
  else
    bad "$what (no \`$step\` ... opened fresh in $file)"
  fi
}

# When the last lane was banked, in `$SECONDS`. A session's age is measured from
# its last reply, which is written before the pass that banks the spend, so
# waiting this long from here is never less than the age asked for.
BANKED_AT=0

# Wait, for real, until the last banked lane is at least `$1` seconds old.
age() {
  while [ $((SECONDS - BANKED_AT)) -lt "$1" ]; do sleep 5; done
}

# Run a step's lane for real, to the gate after it, with the dispatcher stopped
# and nothing done to the transcript. `$1` is the task and `$2` the step.
reach_gate() {
  local task=$1 step=$2
  if ! drive "$task" paused 600; then
    bad "$task: the \`$step\` real lane reached its gate (at \`$(stage_of "$task")\`)"
    tail -20 "$SPOOLWAY_PROJECT_HOME/headless/logs/$task · $step.log" 2>/dev/null | sed 's/^/        /'
    return 1
  fi
  if ! banked "$task" "$step"; then
    bad "$task: the pass that parked \`$step\` banked what it spent"
    dispatcher_stop
    return 1
  fi
  BANKED_AT=$SECONDS
  dispatcher_stop
  return 0
}

# `resume` is one verb over both roads out of a stop, and the scenarios below
# only ever exercise the `paused` half of it, through a real gated lane. The
# `blocked` half needs no real lane to prove — clearing a block is a file edit
# and a routing decision, nothing `prompt_cache_ttl` touches — so this task
# is written onto `blocked` by hand rather than spending a turn to reach it.
{
  echo "---"
  echo "id: stuck"
  echo "title: stuck, blocked by hand"
  echo "stage: blocked"
  echo "blocked_from: first"
  echo "pipeline: warmth"
  echo
  echo "---"
  printf '## Goal\n\nAdd `notes/stuck.md`.\n\n## Non-goals\n\nOut of scope.\n\n## Acceptance criteria\n\n- `notes/stuck.md` exists.\n'
} > "$SPOOLWAY_PROJECT_HOME/queue/stuck.md"
must "resume a blocked task" "$SPOOLWAY" resume stuck
if [ "$(stage_of stuck)" = "first" ]; then
  ok "the same verb resumes a blocked task at the step it stopped on"
else
  bad "resume should have put the blocked task back on \`first\` (at \`$(stage_of stuck)\`)"
fi
# Gone the moment the assertion is made, or the dispatcher started below for
# the `paused` scenarios would pick this task up on its own and spend a real
# turn launching a lane on it — the exact cost this block exists to avoid.
rm -f "$SPOOLWAY_PROJECT_HOME/queue/stuck.md"

# ------------------------------------------- the shape billing is read from
#
# The guard. Everything below is an assertion about a decision; this one is an
# assertion about the *record*, and it is the one that goes red the day Claude
# Code changes how it reports a cache write — a fact the idle horizon no
# longer depends on, but the price of a cache write still does (`read_turn`'s
# claude arm, `ModelPrice::apply`).
check_shape() {
  local task=$1 sid file
  sid=$(session_of "$task") || { bad "the first lane banked a session id"; return 1; }
  file=$(transcript_of "$sid")
  if [ -z "$file" ]; then
    bad "the real lane's transcript is where usage::session_file looks"
    return 1
  fi
  ok "the real lane's transcript is where usage::session_file looks"

  if python3 - "$file" <<'PY'
import json, sys
last = None
for line in open(sys.argv[1]):
    try:
        record = json.loads(line)
    except ValueError:
        continue
    if record.get("type") == "assistant":
        last = record
if last is None:
    sys.exit("no assistant record")
if not isinstance(last.get("timestamp"), str):
    sys.exit("no `timestamp` on the last reply — a session's age would fall back to the modified time")
usage = last.get("message", {}).get("usage", {})
detail = usage.get("cache_creation")
if not isinstance(detail, dict):
    sys.exit("no `usage.cache_creation` — a cache write here would be priced as ordinary input")
buckets = ("ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens")
if not any(k in detail for k in buckets):
    sys.exit(f"`cache_creation` carries none of {buckets}: {sorted(detail)}")
if not any(detail.get(k, 0) for k in buckets):
    sys.exit("every cache bucket is zero — this turn wrote to no cache to measure")
PY
  then ok "a real turn still carries the reply timestamp and the cache split billing is read from"
  else bad "a real turn still carries the reply timestamp and the cache split billing is read from"; fi

  printf '%s\n' "$file" > "$LIVE/transcript.$task"
  return 0
}

# --------------------------------------------------- one task, gated twice
#
# No `prompt_cache_ttl` is set on this model — every model ships that way, so
# the five-minute default applies, and nothing in the project names the limit.
task_doc "$LIVE/waits.md" waits "$BODY" "group: waits" "pipeline: warmth"
must "a task for waits" "$SPOOLWAY" queue add --from "$LIVE/waits.md"
if reach_gate waits first; then
  ok "a real lane ran, reported a pass, and the gate held it on \`paused\`"
  check_shape waits
  first_sid=$(session_of waits first)
  # How long the transcript is at the gate, so the check below can find the
  # first turn the resumed lane added.
  FIRST_LINES=$(wc -l < "$(transcript_of "$first_sid")" 2>/dev/null || echo 0)

  # About two minutes on, well inside the default horizon, so the conversation
  # is carried. The resumed lane's first turn must read Anthropic's cache. A
  # failure prints that turn's usage: zero cache read with a healthy shape is
  # an eviction on Anthropic's side, and a missing usage block is a bug here.
  age 120
  must "resume waits" "$SPOOLWAY" resume waits
  if reach_gate waits second; then
    second_sid=$(session_of waits second)
    if [ -n "$first_sid" ] && [ "$first_sid" = "$second_sid" ]; then
      ok "a resume at 2m continued the same session"
    else
      bad "a resume at 2m continued the same session (first \`$first_sid\`, second \`$second_sid\`)"
      grep '"task":"waits"' "$SPOOLWAY_PROJECT_HOME/usage.jsonl" | sed 's/^/        /'
    fi
    if usage=$(python3 - "$(transcript_of "$second_sid")" "$FIRST_LINES" <<'PY'
import json, sys
path, skip = sys.argv[1], int(sys.argv[2])
for n, line in enumerate(open(path)):
    if n < skip:
        continue
    try:
        record = json.loads(line)
    except ValueError:
        continue
    if record.get("type") != "assistant":
        continue
    usage = record.get("message", {}).get("usage", {})
    print(json.dumps(usage))
    sys.exit(0 if usage.get("cache_read_input_tokens", 0) > 0 else 1)
print("{}")
sys.exit(1)
PY
    ); then
      ok "its first turn read the cache (cache_read_input_tokens > 0)"
    else
      bad "its first turn read the cache (cache_read_input_tokens > 0)"
      printf '        first turn usage after the resume: %s\n' "${usage:-none found}"
    fi

    # More than five minutes after the second lane's last reply, with the
    # default still in force: the store is refused and the ledger names a new
    # session. Nothing here says the cache is cold — Claude Code often writes
    # to the one-hour cache. The check is spoolway's call.
    age 360
    must "resume waits again" "$SPOOLWAY" resume waits
    if drive waits gone 600; then
      third_sid=$(session_of waits third)
      if [ -n "$third_sid" ] && [ "$third_sid" != "$second_sid" ]; then
        ok "a resume at 8m opened a fresh session"
      else
        bad "a resume at 8m opened a fresh session (second \`$second_sid\`, third \`$third_sid\`)"
        grep '"task":"waits"' "$SPOOLWAY_PROJECT_HOME/usage.jsonl" | sed 's/^/        /'
      fi
      opened_fresh waits third "and the task file says the third step opened fresh"
      has "and which bound refused it" \
        "prompt_cache_ttl — opened fresh" "$SPOOLWAY_PROJECT_HOME/archive/waits.md"
    else
      bad "a resume at 8m opened a fresh session (the task stopped at \`$(stage_of waits)\`)"
      tail -10 "$SPOOLWAY_PROJECT_HOME/headless/logs/waits · third.log" 2>/dev/null | sed 's/^/        /'
    fi
  fi
fi

# ------------------------------------------------ the same chain, in one command
#
# `spoolway agent verify <kind> --live` is the same last link the whole suite
# above is about — a real transcript, read back through `usage` — asked as one
# question instead of driven through a pipeline. It belongs here and nowhere
# else for the reason everything above does: it spends. Two turns — one fresh,
# one resumed — behind the same `SPOOLWAY_E2E_CLOUD=1` opt-in the suite
# already gated on.
#
# What it adds over the scenarios above is coverage of the readings the
# dispatcher never consults together: age and last-turn size are asserted
# above through their *effect* on a resume decision, and `harvest`'s running
# totals and `last_written`'s mtime are asserted nowhere against a real
# transcript at all. A provider that changed its usage shape would show up
# here as a named reading rather than as a resume decision that went the other
# way for reasons nobody could see.
#
# It runs in a scratch directory of its own, not this project — that is the
# command's own doing, and worth knowing when looking for what it wrote.
# One invocation, two turns — the second resumed, which is the command re-checking
# claude's `--session-id`→`--resume` swap against the binary — and read five
# ways. A second invocation would be two more turns, and the reason this suite
# is opt-in is that its turns cost money.
LIVE_OUT="$LIVE/verify-live.txt"
if "$SPOOLWAY" agent verify claude --live --model "$MODEL" > "$LIVE_OUT" 2>&1; then
  ok "one command settles the accounting half against a real turn"
else
  bad "one command settles the accounting half against a real turn"
  sed 's/^/        /' "$LIVE_OUT"
fi
has "the tokens come back off the transcript claude actually wrote" \
  "tokens read back" "$LIVE_OUT"
has "and so does the transcript mtime the stall watchdog reads" \
  "mtime moved while the turn ran" "$LIVE_OUT"
has "and the running totals the ledger banks" \
  "running totals read back" "$LIVE_OUT"
has "and the resume spelling still continues a session" \
  "the resumed turn continued the same session" "$LIVE_OUT"

finish
