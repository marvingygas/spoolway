#!/usr/bin/env bash
# Cron jobs, fired by the dispatcher's own pass.
#
# `routines.sh` proves a person queueing a routine folder by hand lands the
# right task files under minted ids. This proves the engine does the same on
# a schedule: a job in a store, a dispatcher pass against a minute the job's
# expression matches, and the routine's documents in the queue under freshly
# minted ids — never the bare ids the routine's own documents carry.
#
# `* * * * *` matches every minute, so the first pass fires. The chain
# (audit-docs depends on audit-deps) keeps audit-docs sitting in the queue
# while audit-deps runs, so it is there to assert on whichever order the
# dispatcher gets to them.
#
# No `covers:` tag — a job is neither a config.toml key nor a pipeline step
# key, the same reasoning `routines.sh` gives.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

LIVE=${WORK:-$(mktemp -d)}

install_agents "$LIVE/bin" "$LIVE/ctl"
new_repo "$LIVE/proj"
configure_project plan/live

BODY="$LIVE/body.md"
task_body "$BODY"

# The routine a job points at: two documents straight in `nightly/`, the
# second depending on the first, so firing the job has to remap that
# `depends_on` onto the ids it mints.
mkdir -p .spoolway/routines/nightly
task_doc .spoolway/routines/nightly/audit-deps.md audit-deps "$BODY" "group: nightly"
task_doc .spoolway/routines/nightly/audit-docs.md audit-docs "$BODY" \
  "group: nightly" "depends_on: [audit-deps]"

# The job, written straight into the user-scoped store — the screen that
# writes one is a later task, so the suite writes the TOML itself.
cat > "$SPOOLWAY_PROJECT_HOME/jobs.toml" <<TOML
[jobs.nightly-audit]
schedule = "* * * * *"
pipeline = "default"
routine = "nightly"
TOML

works "jobs list shows the job" \
  bash -c '"$1" jobs list | grep -q nightly-audit' _ "$SPOOLWAY"
says "jobs list --json carries the schedule" '"schedule": "* * * * *"' \
  "$SPOOLWAY" jobs list --json

# A dispatcher pass against a matching minute fires the job.
dispatcher_start

poll_until 20 test -f "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md" \
  && ok "the job fired: the routine's second document reached the queue under a minted id" \
  || bad "the job never queued audit-docs-1 (dispatch log follows)"

works "and the first document too, in the queue or already archived" \
  bash -c '[ -f "$1/queue/audit-deps-1.md" ] || [ -f "$1/archive/audit-deps-1.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

works "never the routine documents' own bare ids" \
  bash -c '[ ! -e "$1/queue/audit-deps.md" ] && [ ! -e "$1/queue/audit-docs.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

has "the minted copy keeps the document's own group" "group: nightly" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"
has "and is put on the job's pipeline" "pipeline: default" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"
has "its depends_on is remapped onto the sibling's minted id" "audit-deps-1" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"

works "the routine tree is left exactly where it was" \
  test -f .spoolway/routines/nightly/audit-deps.md
has "unminted, unmodified" "id: audit-deps" \
  .spoolway/routines/nightly/audit-deps.md

dispatcher_stop

# `spoolway doctor` names a job whose expression will not parse, one whose
# expression parses but never comes round, one whose routine is gone, and one
# whose pipeline is not defined.
cat >> "$SPOOLWAY_PROJECT_HOME/jobs.toml" <<TOML

[jobs.broken]
schedule = "99 * * * *"
pipeline = "no-such-pipeline"
routine = "ghost"
enabled = false

[jobs.impossible]
schedule = "0 0 30 2 *"
pipeline = "default"
routine = "nightly"
enabled = false
TOML

refuses "doctor fails when a job is broken" "problem" "$SPOOLWAY" doctor
says "— naming the unparseable expression" "job \`broken\` schedule fires" \
  "$SPOOLWAY" doctor
says "— naming a schedule that never comes round" "\`0 0 30 2 *\` parses but never comes round" \
  "$SPOOLWAY" doctor
says "— naming the routine that is gone" "job \`broken\` routine exists" \
  "$SPOOLWAY" doctor
says "— naming the pipeline that is not defined" "job \`broken\` pipeline is defined" \
  "$SPOOLWAY" doctor

finish
