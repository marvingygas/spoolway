#!/usr/bin/env bash
# `models.<glob>.session_reuse_idle`, against a real cloud model. The one
# suite that spends money.
#
# Everything else in this harness runs stand-ins, and everything else should:
# whether the dispatcher decides correctly is decidable from files and exit
# codes. This one cannot be. What it is about is the last link in a chain that
# nothing else can reach:
#
#     a real transcript → usage::touched_at → carried_session → resume
#
# `dispatch::tests::carried_session_reports_which_of_the_four_misses_it_was`
# proves the same bound with a transcript the test writes to order — and a
# transcript a test writes is a transcript that agrees with spoolway's parser
# by construction, mtime included. What it cannot say is whether a transcript
# *Claude Code actually writes today* still
# leaves spoolway anything to read at all: `read_turn`'s claude arm still
# splits `usage.cache_creation` by lifetime, which is what the billed cost of
# a cache write is priced off, quite apart from the horizon this suite is
# named for. If that shape ever moved, every claude session would still be
# aged correctly off its file's own mtime — the horizon does not depend on
# it — but its cost would silently be wrong, with nothing anywhere going red.
# `check_shape` below is what stands in the gap.
#
# And it has to be a *cloud* model for the same reason it always did: `claude`
# is the one kind this project ever spends real money running headlessly, and
# the shape worth watching is its transcript's, not pi's.
#
# **Cost.** Eight turns of `claude-haiku-4-5` — six through the pipeline
# scenarios, two more through `agent verify --live` at the end — each a few
# thousand tokens against a fixture repo of two files. Fractions of a cent, and
# it is opt-in either way: nothing here runs unless `SPOOLWAY_E2E_CLOUD=1` is
# set. It is in no local tier.
#
# **It uses your real `$HOME`.** It has to: the real `claude` needs your
# credentials, and its transcripts land where it puts them. What it writes there
# is its own new sessions, in a project directory named after this suite's
# scratch tree — the same thing running `claude` in a new directory does. It
# edits nothing of yours. The one transcript it *does* edit is one of its own,
# and only its mtime: a store sitting past `session_reuse_idle` is not a wait a
# test can perform, so the stale scenarios move the file's clock back instead.
# Every field inside it stays exactly as Claude Code wrote it, which is the
# whole point.
#
# covers: models.<glob>.session_reuse_idle — against a real claude session: unset, a store of any age still resumes; set, one whose store has sat past it is refused
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
# with the previous run's own archive still sitting behind the registration.
# `--take-over` is `configure_project`'s way of claiming it anyway rather
# than refusing: a `proj` whose root is still a live directory is somebody
# else's and stays a loud collision either way.
new_repo "$LIVE/proj"
# `--take-over` reclaims the *registration*; the previous run's project home
# — its archive above all — is still there behind it, and `queue add` refuses
# an id the archive already holds. Every task this suite mints is named below
# (`warm`, `thaw`, `chill`, `stuck`), so the stale home is this suite's own
# leavings and nothing else's: sweep it before init writes the fresh one.
rm -rf "$SPOOLWAY_PROJECT_HOME"
configure_project plan/live "$LIVE/worktrees" --take-over
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

# Two visits by one prompt, with a gate between them.
#
# The gate is the sequencing, and it is the only way to get one: the decision
# under test is taken when the *second* lane starts, and it is taken in the same
# pass that banks the first lane's ledger line — so there is no gap between them
# for a suite to reach into and doctor a transcript. `gate: true` opens one that
# spoolway itself holds. The task parks on `paused` with the first lane's spend
# banked, the suite does its work, and `spoolway resume` starts the second.
cat > .spoolway/pipelines/warmth.yml <<YAML
# Written by scripts/e2e/suites/warmth.sh. Two steps, one prompt, one gate.
steps:
  - id: first
    description: Write the note, and stop for a person.
    agent: claude
    prompt: builder
    model: $MODEL
    gate: true
    on_pass: second

  - id: second
    description: Come back to the same conversation, or not — which is the subject.
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

# The session the first lane actually opened, out of the ledger — which is the
# same correlation `carried_session` makes, so reading it here is reading what
# the decision under test will read.
session_of() {
  local task=$1 line
  line=$(grep "\"task\":\"$task\"" "$SPOOLWAY_PROJECT_HOME/usage.jsonl" 2>/dev/null \
         | grep '"step":"first"' | tail -1)
  [ -n "$line" ] || return 1
  sed -n 's/.*"session":"\([^"]*\)".*/\1/p' <<<"$line"
}

# Whether the first lane's spend has reached the ledger yet.
#
# Two different writers, and the suite has to wait for the second: the *lane*
# writes the stage when it reports, and the *dispatcher* banks the spend on the
# pass that parks it. Stopping the dispatcher the instant the stage says `paused`
# stops it before the ledger line exists — and then waiting for that line is
# waiting for something nothing is left running to do.
banked() {
  poll_until 90 grep -q "\"task\":\"$1\".*\"step\":\"first\"" "$SPOOLWAY_PROJECT_HOME/usage.jsonl"
}

# Where the real claude put it. Never guessed at: both agents shard their
# sessions by an escaping of the lane's working directory, and the id spoolway
# minted is unambiguous wherever it landed — which is exactly what
# `usage::session_file` does too.
transcript_of() {
  find "$HOME/.claude/projects" -name "$1.jsonl" -print -quit 2>/dev/null
}

# Move a real transcript's own clock back, and nothing else about it.
#
# `touched_at` reads the store's mtime, not any record inside it, so ageing a
# session for this suite means moving the file's clock rather than rewriting
# its content — every byte Claude Code wrote stays exactly as it wrote it.
backdate() {
  touch -d "-$2 seconds" "$1"
}

# Whether the second lane carried the first one's conversation, read off the
# task's own record.
#
# Not off the lane's log: a real `claude --print` writes its *answer* there and
# nothing about how it was launched, so the "this is your own session,
# continued" a stand-in echoes back has no equivalent here. The `new_sessions:`
# counter this used to read is retired — nothing writes it any more — and what
# replaced it is better anyway: a `session:` step that opens fresh writes one
# Status Log line naming the step and why (`- \`second\`: … — opened fresh`),
# and a carried session writes no such line at all. See `session_miss` in
# `dispatch::start_one`.
#
# Read out of the archive, since a landed task has left the queue.
carried() {
  local task=$1 fresh=$2 what=$3
  if [ "$fresh" -eq 1 ]; then
    has   "$what" 'opened fresh' "$SPOOLWAY_PROJECT_HOME/archive/$task.md"
  else
    lacks "$what" 'opened fresh' "$SPOOLWAY_PROJECT_HOME/archive/$task.md"
  fi
}

# One scenario: run the first lane for real, hold at the gate, do `$3`, resume.
#
# `$1` is the task, `$2` how long a real lane may take, `$3` a function run
# while the queue is held still.
at_the_gate() {
  local task=$1 secs=$2 doctor=$3
  task_doc "$LIVE/$task.md" "$task" "$BODY" \
    "group: live" "pipeline: warmth" "touches: [notes/$task.md]"
  must "a task for $task" "$SPOOLWAY" queue add --from "$LIVE/$task.md"
  if ! drive "$task" paused "$secs"; then
    bad "$task: the first real lane reached the gate (at \`$(stage_of "$task")\`)"
    tail -20 "$SPOOLWAY_PROJECT_HOME/headless/$task · first.log" 2>/dev/null | sed 's/^/        /'
    return 1
  fi
  ok "$task: a real lane ran, reported a pass, and the gate held it on \`paused\`"
  if ! banked "$task"; then
    bad "$task: and the pass that parked it banked what it spent"
    dispatcher_stop
    return 1
  fi
  dispatcher_stop
  "$doctor" "$task" || return 1
  must "resume $task" "$SPOOLWAY" resume "$task"
  return 0
}

# `resume` is one verb over both roads out of a stop, and the scenarios below
# only ever exercise the `paused` half of it, through a real gated lane. The
# `blocked` half needs no real lane to prove — clearing a block is a file edit
# and a routing decision, nothing `session_reuse_idle` touches — so this task
# is written onto `blocked` by hand rather than spending a turn to reach it.
{
  echo "---"
  echo "id: stuck"
  echo "title: stuck, blocked by hand"
  echo "stage: blocked"
  echo "blocked_from: first"
  echo "pipeline: warmth"
  echo "touches: [notes/stuck.md]"
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
  then ok "a real turn still carries the cache split billing is read from"
  else bad "a real turn still carries the cache split billing is read from"; fi

  printf '%s\n' "$file" > "$LIVE/transcript.$task"
  return 0
}

# ------------------------------------------------------------- warm, resumed
#
# No horizon is set on this model yet — every model ships that way — and the
# store is untouched, so the conversation is carried regardless. On its own
# this proves little — an unset horizon resumes anything, however old — and
# that is exactly why it is here: it is the control the two scenarios below
# are measured against. Same lane, same transcript, one thing different each
# time.
if at_the_gate warm 600 check_shape; then
  if drive warm gone 600; then
    ok "the second real lane runs and the task lands"
    carried warm 0 "a fresh real session is resumed"
  else
    bad "the second real lane runs and the task lands (at \`$(stage_of warm)\`)"
    tail -10 "$SPOOLWAY_PROJECT_HOME/headless/warm · second.log" 2>/dev/null | sed 's/^/        /'
  fi
fi

# ---------------------------------------------- old, and no horizon set yet
#
# Still no `session_reuse_idle` on this model — the store is just old now, two
# hours backdated. Unset already says what `session_reuse_uncached = true`
# used to: carry it regardless of age. This is the acceptance criterion in as
# many words — the store's age refuses nothing until a horizon exists to
# refuse against.
stale() {
  local task=$1 sid file
  sid=$(session_of "$task") || return 1
  file=$(transcript_of "$sid")
  [ -n "$file" ] || { bad "no transcript to age"; return 1; }
  backdate "$file" 7200
  return 0
}

if at_the_gate thaw 600 stale; then
  if drive thaw gone 600; then
    ok "the second real lane runs and that task lands too"
    carried thaw 0 "with no session_reuse_idle set, a two-hour-old real store still resumes"
  else
    bad "the second real lane runs and that task lands too (at \`$(stage_of thaw)\`)"
    tail -10 "$SPOOLWAY_PROJECT_HOME/headless/thaw · second.log" 2>/dev/null | sed 's/^/        /'
  fi
fi

# --------------------------------------------------------- old, past a horizon
#
# The discriminating case: the same two hours of age, and now a horizon on the
# model that it is well past. A store that has sat longer than
# `session_reuse_idle` is refused rather than resumed, whatever it is made of.
must "an idle horizon" "$SPOOLWAY" config set "models.$MODEL.session_reuse_idle" 5m
if at_the_gate chill 600 stale; then
  if drive chill gone 600; then
    ok "the second real lane runs and that task lands too"
    carried chill 1 "a real session past the horizon is refused and a fresh one opened"
    has "and the task file says which bound refused it" \
      "session_reuse_idle — opened fresh" "$SPOOLWAY_PROJECT_HOME/archive/chill.md"
  else
    bad "the second real lane runs and that task lands too (at \`$(stage_of chill)\`)"
    tail -10 "$SPOOLWAY_PROJECT_HOME/headless/chill · second.log" 2>/dev/null | sed 's/^/        /'
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
has "and so does the mtime this whole suite's horizon is read against" \
  "mtime moved while the turn ran" "$LIVE_OUT"
has "and the running totals the ledger banks" \
  "running totals read back" "$LIVE_OUT"
has "and the resume spelling still continues a session" \
  "the resumed turn continued the same session" "$LIVE_OUT"

finish
