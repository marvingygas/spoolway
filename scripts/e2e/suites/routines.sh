#!/usr/bin/env bash
# Repeatable task documents under `.spoolway/routines/`, driven end to end
# through `spoolway queue`'s own `r` pane and `s` panel — the one path no
# unit test can drive, since `run_screen` is exercised headlessly in Rust
# already, but never as the whole binary reading real keystrokes off a real
# pipe against a real, tracked `.spoolway/routines/` tree.
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
# `maintenance/weekly/` is one folder deep besides, so the left pane's own
# "N tasks" tail has something recursive to count.
BODY="$LIVE/body.md"
task_body "$BODY"
mkdir -p .spoolway/routines/nightly .spoolway/routines/maintenance/weekly
task_doc .spoolway/routines/nightly/audit-deps.md audit-deps "$BODY" "group: nightly"
task_doc .spoolway/routines/nightly/audit-docs.md audit-docs "$BODY" \
  "group: nightly" "depends_on: [audit-deps]"
task_doc .spoolway/routines/maintenance/weekly/prune.md prune "$BODY" "group: maintenance"

# `r` swaps the pending screen for the folder tree; `j` moves the cursor off
# `maintenance` (alphabetically first) onto `nightly`; `space` ticks it;
# `enter` queues both its documents as one batch; `n` declines the
# dispatcher offer, the same way `trials.sh` does.
printf 'rj \rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the first document reaches the queue under a minted id" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/audit-deps-1.md"
works "and the second beside it" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/audit-docs-1.md"
works "never the documents' own bare ids" \
  bash -c '[ ! -e "$1/queue/audit-deps.md" ] && [ ! -e "$1/queue/audit-docs.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

has "the minted copy keeps the document's own group" "group: nightly" \
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

# A second, solo pick: `→` opens `maintenance` (it holds a subfolder, so this
# descends rather than focusing its own tasks pane, which it has none of
# directly); `→` again opens `weekly`, a leaf, which focuses its one task;
# `space` queues it alone; `n` declines the dispatcher offer again.
printf 'r\x1b[C\x1b[C n' | "$SPOOLWAY" queue >/dev/null 2>&1

works "a solo pick under a nested folder reaches the queue too" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/prune-1.md"

# A pending group, saved as a routine: `s` opens the panel over `release`,
# already named after the group; `enter` accepts that name and copies its
# one document into `.spoolway/routines/release/`, unchanged; `q` quits.
pending_doc release-notes "$BODY" "group: release"
printf 's\rq' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the saved routine landed in the checkout" \
  test -f .spoolway/routines/release/release-notes.md
has "under its own bare id, untouched" "id: release-notes" \
  .spoolway/routines/release/release-notes.md
has "and its own group" "group: release" \
  .spoolway/routines/release/release-notes.md

# A group whose one document lives only in the queue directory — the shape a
# task already run once through a pipeline has, carrying every key spoolway
# stamped on that run. `list_groups` reads it straight out of `queue/`,
# verbatim, so this is `s` over exactly what a finished or archived task
# looks like, not a fresh producer's document. `f` narrows the picker to it
# by name, since `h` alone would still need this to be told apart from
# `nightly` and `maintenance`, already queued too.
task_doc "$SPOOLWAY_PROJECT_HOME/queue/archived-reuse.md" archived-reuse "$BODY" \
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

printf 'farchived-reuse\rs\rq' | "$SPOOLWAY" queue >/dev/null 2>&1

works "a document an earlier run already stamped still saves as a routine" \
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

# `r` opens with the folder tree's first entry already under the cursor —
# `archived-reuse` sorts before every routine folder this suite wrote earlier
# — so `space` ticks it and `enter` queues it straight from there, the same
# `n`-declines-the-dispatcher shape every other queueing keystroke here uses.
printf 'r \rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the saved routine queues back, past the refusal a stamped copy would hit" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/archived-reuse-1.md"
has "under a freshly minted id" "id: archived-reuse-1" \
  "$SPOOLWAY_PROJECT_HOME/queue/archived-reuse-1.md"
works "never the earlier run's own bare id" \
  bash -c '! grep -qxF "id: archived-reuse" "$1"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/archived-reuse-1.md"

finish
