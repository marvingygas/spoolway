#!/usr/bin/env bash
# Bare `spoolway`'s jobs tab, two `→`s from the queue tab it opens on, past
# the routines tab between them, driven end to end as the whole binary reading
# real keystrokes off a real pipe against a real, tracked
# `.spoolway/routines/` tree — the one path no unit test covers, since
# `run_jobs_screen` is exercised headlessly in Rust already but never as the
# installed binary.
#
# What is asserted is the TOML the screen writes: a completed walk lands a
# `[jobs.<name>]` table in the user store, the `o` menu's pause and resume
# flip it, and its delete item removes it. Nothing here drives a dispatcher — `install_agents`/`new_forge`
# are not needed, the same reason `routines.sh` needs neither.
#
# No `covers:` tag — the coverage map only enumerates `config.toml` keys and
# pipeline step keys, and a screen gesture is neither.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

LIVE=${WORK:-$(mktemp -d)}

new_repo "$LIVE/proj"
configure_project plan/live

# One routine folder, tracked in the checkout, with a document in it — the
# target the browser's first row picks.
BODY="$LIVE/body.md"
task_body "$BODY"
mkdir -p .spoolway/routines/nightly
task_doc .spoolway/routines/nightly/audit-deps.md audit-deps "$BODY" "group: nightly"

STORE="$SPOOLWAY_PROJECT_HOME/jobs.toml"

# Two `→`s open the jobs tab, the first stopping on the routines tab; n opens
# the routines browser; space ticks the first folder (`nightly`); enter uses
# it; the expression is typed; enter accepts it; enter chooses whichever
# pipeline the picker opens on — there is no project default any more, so it
# opens on the alphabetically first of the shipped set (`bugfix`, `default`),
# which is `bugfix`. The screen then ends as the pipe drains.
on_screen '\033[C\033[Cn \r0 3 * * 1-5\r\r' /dev/null

works "the walk writes a job store" test -f "$STORE"
has "under a table named for the routine" "[jobs.nightly]" "$STORE"
has "carrying the typed expression" 'schedule = "0 3 * * 1-5"' "$STORE"
has "pointed at the picked routine" 'routine = "nightly"' "$STORE"
has "on the pipeline the picker opened on" 'pipeline = "bugfix"' "$STORE"
lacks "enabled by default, so no key for it" "enabled" "$STORE"

says "and jobs list now shows it" "nightly" "$SPOOLWAY" jobs list

# o opens the options menu on the highlighted job; down moves to pause and
# enter picks it. The screen then ends as the pipe drains, the same as the
# walk above.
on_screen '\033[C\033[Co\033[B\r' /dev/null
has "the menu's pause pauses the job" "enabled = false" "$STORE"

# The same two keys now pick resume — the key is dropped, not written back
# as true.
on_screen '\033[C\033[Co\033[B\r' /dev/null
lacks "the menu's resume resumes it" "enabled" "$STORE"

# The old row keys are gone: space and x change nothing.
on_screen '\033[C\033[C x\r' /dev/null
has "space and x no longer reach the job" "[jobs.nightly]" "$STORE"
lacks "and space did not pause it" "enabled" "$STORE"

# The menu's delete asks, esc keeps.
on_screen '\033[C\033[Co\033[B\033[B\r\033' /dev/null
has "the menu's delete then esc keeps the job" "[jobs.nightly]" "$STORE"

# The menu's delete asks, enter confirms.
on_screen '\033[C\033[Co\033[B\033[B\r\r' /dev/null
lacks "the menu's delete then enter deletes the job" "[jobs.nightly]" "$STORE"

# A job runs a whole routine, so the tasks pane no longer picks one task: two
# `→`s open the jobs tab; n opens the routines browser; `tab` moves the cursor
# onto `nightly`'s tasks pane, on `audit-deps`; space is pressed there and the
# expression typed. The store already holds a job on `nightly` under another
# name, so a save would be refused anyway; what is checked is that no
# single-task target is written.
cat >"$STORE" <<'EOF'
[jobs.first-job]
schedule = "0 3 * * *"
pipeline = "bugfix"
routine = "nightly"
EOF
on_screen '\033[C\033[Cn\t 0 4 * * *\r\r' /dev/null
lacks "space on the tasks pane picks no single task" 'audit-deps.md' "$STORE"
lacks "and writes no job for one" "[jobs.audit-deps]" "$STORE"

# A save naming a routine that already has a job is refused with a notice
# naming that job, and writes nothing: n; space ticks `nightly`; enter uses
# it; the expression; the pipeline.
REFUSED="$LIVE/jobs-refused.txt"
on_screen '\033[C\033[Cn \r0 4 * * *\r\r' "$REFUSED"
lacks "a second job on the routine is refused" "[jobs.nightly]" "$STORE"
has "naming the job that already has it" "first-job" "$REFUSED"

finish
