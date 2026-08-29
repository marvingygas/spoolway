#!/usr/bin/env bash
# The observer: one process that reads every lane's pane while a plan run
# happens, and writes down what it saw.
#
# Started by the `observe` step of scripts/e2e/runtime/end-to-end.yml, which is
# a backgrounded `run:` step every task passes through on its way in. Nothing
# in spoolway knows this file exists and nothing needs to: `spoolway lane` with
# no argument lists the lanes, and with one reads that lane's pane. That is the
# whole mechanism, and it is deliberately the same one a person uses — an
# observer reaching into the multiplexer directly would be watching something
# other than what the CLI shows.
#
# Singleton, by a lock rather than by arithmetic. Every task starts one, so a
# three-task plan starts three; the second and third find the lock held and
# exit without writing a line. A predecessor whose task is archived takes its
# observer down with it (the dispatcher stops a task's background runs before
# removing its worktree), and the next task's observer picks the lock up and
# appends to the same file — so the record is continuous and there is never
# more than one writer.
#
# Reads the project it is standing in: a command step runs in the task's
# worktree, and every lane, every log and the observations file itself belong to
# the project rather than to any one task.
#
#   observe.sh                  poll until the queue is empty
#   OBSERVE_EVERY=5 observe.sh  ...looking that often, in seconds
#
# The step's `timeout:` is the real bound. This exits on its own when the queue
# empties, and the timeout is what covers the run that never gets there.
set -uo pipefail

REPO=${SPOOLWAY_REPO:-$PWD}
cd "$REPO" || exit 0

SPOOLWAY=${SPOOLWAY:-spoolway}
EVERY=${OBSERVE_EVERY:-10}
OUT="$REPO/observations.md"
LOCK="$REPO/.spoolway/observe.lock"
# How much of a pane to read each time. Enough to see a report, a question or a
# stack trace; not so much that a slow lane rewrites the file every pass.
LINES=${OBSERVE_LINES:-40}

# One observer, whatever the task count.
#
# `flock` where there is one, which is every Linux that has util-linux — and a
# directory otherwise, because `mkdir` is atomic everywhere and this must not
# become a reason the observer is the thing that fails. The directory lock is
# released on exit; if the process is killed the next task's observer clears it
# after finding no process behind the pid inside.
mkdir -p "$(dirname "$LOCK")"
if command -v flock >/dev/null 2>&1; then
  exec 9>"$LOCK" || exit 0
  flock -n 9 || exit 0
else
  if ! mkdir "$LOCK.d" 2>/dev/null; then
    held=$(cat "$LOCK.d/pid" 2>/dev/null || true)
    if [ -n "$held" ] && kill -0 "$held" 2>/dev/null; then exit 0; fi
    rm -rf "$LOCK.d"
    mkdir "$LOCK.d" 2>/dev/null || exit 0
  fi
  echo $$ > "$LOCK.d/pid"
  trap 'rm -rf "$LOCK.d"' EXIT
fi

now() { date '+%H:%M:%S'; }

note() { printf '%s\n' "$*" >> "$OUT"; }

if [ ! -s "$OUT" ]; then
  note "# What the panes did"
  note ""
  note "Written by scripts/e2e/observe.sh, one line per lane per change. Times are"
  note "this machine's. A lane that says nothing new is not written down again."
  note ""
fi
note "## observer up — $(date '+%Y-%m-%d %H:%M:%S')"
note ""

# What each lane last showed, so an unchanged pane is not written down twice.
# Keyed by lane name; the value is a checksum of what was read, because the
# whole point of the file is the moments something changed.
declare -A seen=()

# One line of evidence out of a pane's last $LINES lines.
#
# The last non-empty line is what a person glances at, and it is right far more
# often than any parse of an agent's output would be. A report, a question and a
# crash all land there.
gist() {
  awk 'NF { last = $0 } END { print last }' <<<"$1" | cut -c1-160
}

idle=0
while :; do
  # A lane's name is `<task> · <step>` — two words around the dot — and the
  # state column follows. `$1` alone was the grammar before steps joined the
  # name, and handing that half to `spoolway lane` below got only the "not a
  # lane name" usage error back, so observations.md recorded errors and
  # nothing else.
  lanes=$("$SPOOLWAY" lane 2>/dev/null | awk '$2 == "·" { print $1 " · " $3 }')

  if [ -z "$lanes" ]; then
    # No lanes and no queue is a run that is over. One empty pass is not: the
    # gap between a lane ending and the next one starting is routinely a whole
    # dispatch interval wide.
    if "$SPOOLWAY" queue list 2>/dev/null | grep -q "No tasks queued"; then
      idle=$((idle + 1))
      if [ "$idle" -ge 3 ]; then
        note ""
        note "_queue empty — observer down at $(date '+%Y-%m-%d %H:%M:%S')_"
        exit 0
      fi
    fi
    sleep "$EVERY"
    continue
  fi
  idle=0

  while read -r lane; do
    [ -n "$lane" ] || continue
    pane=$("$SPOOLWAY" lane "$lane" -n "$LINES" 2>&1)
    stamp=$(cksum <<<"$pane" | awk '{print $1}')
    [ "${seen[$lane]:-}" = "$stamp" ] && continue
    seen[$lane]=$stamp
    note "- \`$(now)\` **$lane** — $(gist "$pane")"
  done <<<"$lanes"

  sleep "$EVERY"
done
