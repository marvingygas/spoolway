#!/usr/bin/env bash
# The `spoolway jobs` screen, driven end to end as the whole binary reading
# real keystrokes off a real pipe against a real, tracked
# `.spoolway/routines/` tree — the one path no unit test covers, since
# `run_jobs_screen` is exercised headlessly in Rust already but never as the
# installed binary.
#
# What is asserted is the TOML the screen writes: a completed walk lands a
# `[jobs.<name>]` table in the user store, `space` pauses it, `x`+`y` removes
# it. Nothing here drives a dispatcher — `install_agents`/`new_forge` are not
# needed, the same reason `routines.sh` needs neither.
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

# n opens the routines browser; space ticks the first folder (`nightly`);
# enter uses it; the expression is typed; enter accepts it; enter chooses the
# default pipeline. The screen then ends as the pipe drains.
printf 'n \r0 3 * * 1-5\r\r' | "$SPOOLWAY" jobs >/dev/null 2>&1

works "the walk writes a job store" test -f "$STORE"
has "under a table named for the routine" "[jobs.nightly]" "$STORE"
has "carrying the typed expression" 'schedule = "0 3 * * 1-5"' "$STORE"
has "pointed at the picked routine" 'routine = "nightly"' "$STORE"
has "on the project default pipeline" 'pipeline = "default"' "$STORE"
lacks "enabled by default, so no key for it" "enabled" "$STORE"

says "and jobs list now shows it" "nightly" "$SPOOLWAY" jobs list

# space over the highlighted job pauses it; q quits.
printf ' q' | "$SPOOLWAY" jobs >/dev/null 2>&1
has "space pauses the job" "enabled = false" "$STORE"

# space again resumes it — the key is dropped, not written back as true.
printf ' q' | "$SPOOLWAY" jobs >/dev/null 2>&1
lacks "space again resumes it" "enabled" "$STORE"

# x asks, y confirms.
printf 'xy' | "$SPOOLWAY" jobs >/dev/null 2>&1
lacks "x then y deletes the job" "[jobs.nightly]" "$STORE"

finish
