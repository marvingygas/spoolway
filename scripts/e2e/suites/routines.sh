#!/usr/bin/env bash
# Repeatable tasks under `.spoolway/routines/`, driven end to end
# through bare `spoolway`'s routines tab and the queue tab's `s` panel — the one
# path no unit test can drive, since `run_screen` is exercised headlessly in
# Rust already, but never as the whole binary reading real keystrokes off a
# real pipe, under the pty `on_screen` gives it, against a real, tracked
# `.spoolway/routines/` tree.
#
# Nothing here drives a dispatcher: what is asserted is that `enter` lands
# the right task files in the queue directory, under minted ids, with their
# bodies untouched, and that `s` copies a pending group's documents back
# into the checkout. No `new_forge`/`install_agents` needed for the same
# reason `trials.sh` needs none.
#
# No `covers:` tag of its own — see `commands.sh`'s own queue-screen block:
# the map `coverage.sh` builds only enumerates `config.toml` keys and
# pipeline step keys, and a queue-screen gesture is neither.
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

# A nested `.spoolway/routines/` tree, tracked in the checkout rather than
# under the project's own runtime home: `nightly/` holds two documents
# straight in it, one depending on the other, so queueing the whole folder
# has to remap that `depends_on` onto the ids it actually mints —
# `maintenance/weekly/` is one folder deep besides. A routine is only ever a
# folder directly under `.spoolway/routines/`, so `weekly` is no row of its
# own: its task counts toward `maintenance`, shows under it and queues with
# it.
BODY="$LIVE/body.md"
task_body "$BODY"
mkdir -p .spoolway/routines/nightly .spoolway/routines/maintenance/weekly
task_doc .spoolway/routines/nightly/audit-deps.md audit-deps "$BODY" "group: nightly"
task_doc .spoolway/routines/nightly/audit-docs.md audit-docs "$BODY" \
  "group: nightly" "depends_on: [audit-deps]"
task_doc .spoolway/routines/maintenance/weekly/prune.md prune "$BODY" "group: maintenance"

# `→` moves from the queue tab, where the screen opens, to the routines tab;
# `j` moves the cursor off `maintenance` (alphabetically first) onto
# `nightly`; `space` ticks it; `enter` queues both its documents as one batch
# — starting a dispatcher is the dispatch tab's `enter`, not this one's; the
# trailing `n` is noise the popup saying what was queued ignores, and the
# pipe running dry ends the screen, the same way `trials.sh` does.
on_screen '\x1b[Cj \rn' /dev/null

works "the first task reaches the queue under a minted id" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/audit-deps-1.md"
works "and the second beside it" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"
works "never the tasks' own bare ids" \
  bash -c '[ ! -e "$1/queue/audit-deps.md" ] && [ ! -e "$1/queue/audit-docs.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

has "the minted copy keeps the task's own group" "group: nightly" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-deps-1.md"
has "and its body, untouched" "Add \`notes/<id>.md\`" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-deps-1.md"
has "a depends_on naming a sibling in the same batch is remapped to its own minted id" \
  "depends_on:" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"
has "— onto audit-deps's own minted id, not its bare one" "audit-deps-1" \
  "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"

works "the routines tree is left exactly where it was" \
  test -f .spoolway/routines/nightly/audit-deps.md
has "unminted, unmodified" "id: audit-deps" \
  .spoolway/routines/nightly/audit-deps.md

# The whole of `maintenance`, with the cursor already on it (it sorts
# first) once `→` reaches the routines tab: `space` ticks it and `enter`
# queues it, which takes in `prune` from its nested `weekly/` too — there is
# no `weekly` row to tick on its own. The trailing `n` is noise it ignores,
# and the pipe running dry ends the screen.
on_screen '\x1b[C \rn' /dev/null

works "a nested task queues with its top-level routine" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/prune-1.md"
has "still carrying that routine's group" "group: maintenance" \
  "$SPOOLWAY_PROJECT_HOME/queue/prune-1.md"

# A second, solo pick: `→` reaches the routines tab, and `tab` moves the
# cursor onto `maintenance`'s tasks pane, already on `prune`, its one task,
# nested or not; `space` queues it alone. `prune-1` is taken by the batch
# above, so this one mints the next number.
on_screen '\x1b[C\t n' /dev/null

works "tab reaches a nested task under its routine, and a solo pick queues it" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/prune-2.md"

# A pending group, saved as a routine: `s` opens the panel over `release`,
# already named after the group; `enter` accepts that name and copies its
# one document into `.spoolway/routines/release/`, unchanged; the screen
# then ends as the pipe drains.
pending_doc release-notes "$BODY" "group: release"
on_screen 's\r' /dev/null

works "the saved routine landed in the checkout" \
  test -f .spoolway/routines/release/release-notes.md
has "under its own bare id, untouched" "id: release-notes" \
  .spoolway/routines/release/release-notes.md
has "and its own group" "group: release" \
  .spoolway/routines/release/release-notes.md

# A group whose one document lives only in the archive — the shape a task
# already run once through a pipeline has, carrying every key spoolway
# stamped on that run. `list_groups` reads it straight out of `archive/`,
# verbatim, so this is `s` over exactly what a finished task looks like, not
# a fresh producer's document. Not `queue/`: the queue tab never lists a
# queued group, and its filter never reaches one. `f` narrows the picker to
# it by name, which reaches a done group without `h`.
#
# The query is `archived-` rather than the whole group name — narrower than
# it needs to be, but it still matches only this group, and every letter of
# it reaches the filter as ordinary text: `s` no longer fires the save panel
# mid-query the way it once did, so nothing here has to dodge a letter to
# keep from triggering an action early.
mkdir -p "$SPOOLWAY_PROJECT_HOME/archive"
task_doc "$SPOOLWAY_PROJECT_HOME/archive/archived-reuse.md" archived-reuse "$BODY" \
  "group: archived-reuse" \
  "stage: done" \
  "run: r00000000000000ar" \
  "branch: task/archived-reuse" \
  "base: master" \
  "cut_from: master" \
  "base_commit: 0000000000000000000000000000000000000000" \
  "worktree_path: /nonexistent/archived-reuse" \
  "workspace_id: wZZ" \
  "pane_id: wZZ:p1" \
  "tab_id: wZZ:t1" \
  "attempts: 3" \
  "epic: https://example.invalid/epic/9" \
  "ticket: https://example.invalid/ticket/9"

on_screen 'farchived-\rs\r' /dev/null

works "a task an earlier run already stamped still saves as a routine" \
  test -f .spoolway/routines/archived-reuse/archived-reuse.md
has "the author's own group survives the reset" "group: archived-reuse" \
  .spoolway/routines/archived-reuse/archived-reuse.md
lacks "stage: is dropped, not copied verbatim" "stage:" \
  .spoolway/routines/archived-reuse/archived-reuse.md
lacks "so is the run id the earlier run minted" "run:" \
  .spoolway/routines/archived-reuse/archived-reuse.md
lacks "and the ticket that earlier run's own hook opened" "ticket:" \
  .spoolway/routines/archived-reuse/archived-reuse.md
lacks "and the epic a fresh run must not inherit" "epic:" \
  .spoolway/routines/archived-reuse/archived-reuse.md

# The routines tab reads its folders fresh on the `→` that reaches it, so
# the routine `s` just saved is listed, and it opens with the list's first
# entry under the cursor. `archived-reuse` sorts before every routine folder
# this suite wrote earlier, so `space` ticks it and `enter` queues it
# straight from there, the same `n`-declines-the-dispatcher shape every
# other queueing keystroke here uses.
on_screen '\x1b[C \rn' /dev/null

works "the saved routine queues back, past the refusal a stamped copy would hit" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/archived-reuse-1.md"
has "under a freshly minted id" "id: archived-reuse-1" \
  "$SPOOLWAY_PROJECT_HOME/queue/archived-reuse-1.md"
works "never the earlier run's own bare id" \
  bash -c '! grep -qxF "id: archived-reuse" "$1"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/archived-reuse-1.md"

finish
