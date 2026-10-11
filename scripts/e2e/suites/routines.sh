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
# bodies untouched, that `s` copies a pending group's documents back
# into the checkout, that `x` removes a routine with the jobs that point
# into it, and that `n` saves a job for a routine to the user store. No
# `new_forge`/`install_agents` needed for the same reason `trials.sh` needs
# none.
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
#
# `prune-1` lands first, the way a finished run leaves it: a group is one
# chain, and `maintenance` still holding `prune-1` in the queue would make a
# second run of the same routine a second root of it, which `queue add`
# refuses. Archived, the routine's name is free for the next run, and minting
# still skips `prune-1`, since it reads the archive as well as the queue.
sed -i 's/^stage: .*/stage: done/' "$SPOOLWAY_PROJECT_HOME/queue/prune-1.md"
mkdir -p "$SPOOLWAY_PROJECT_HOME/archive"
must "prune-1 lands, as a finished run would" \
  mv "$SPOOLWAY_PROJECT_HOME/queue/prune-1.md" "$SPOOLWAY_PROJECT_HOME/archive/prune-1.md"

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
# stamped on that run. `list_groups` lists it from `archive/index.jsonl`,
# rebuilt here because the file was dropped in by hand, and `s` then opens
# the one `archive/<id>.md` by name and saves it verbatim, so this is `s`
# over exactly what a finished task looks like, not a fresh producer's
# document. Not `queue/`: the queue tab never lists a
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
  "starts_from: master" \
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

# `x` deletes a routine and every job that points into it: a user job on the
# whole `nightly` folder, a project job on one task inside it, and a user job
# on `maintenance` that has to survive. The folders sort `archived-reuse`,
# `maintenance`, `nightly`, `release`, so `jj` puts the cursor on `nightly`.
USER_STORE="$SPOOLWAY_PROJECT_HOME/jobs.toml"
PROJECT_STORE=.spoolway/jobs.toml
cat >"$USER_STORE" <<'EOF'
[jobs.nightly-audit]
schedule = "0 3 * * 1-5"
pipeline = "bugfix"
routine = "nightly"

[jobs.weekly-prune]
schedule = "0 4 * * 1"
pipeline = "bugfix"
routine = "maintenance"
EOF
cat >"$PROJECT_STORE" <<'EOF'
[jobs.audit-docs]
schedule = "0 5 * * *"
pipeline = "bugfix"
routine = "nightly/audit-docs.md"
EOF

# x asks, esc keeps everything.
DELETE="$LIVE/routine-delete.txt"
LAST="$LIVE/routine-delete-popup.txt"
on_screen '\x1b[Cjjx\x1b' "$DELETE"
works "x then esc keeps the routine" test -d .spoolway/routines/nightly
has "and its whole-folder job" "[jobs.nightly-audit]" "$USER_STORE"
has "and its single-task job" "[jobs.audit-docs]" "$PROJECT_STORE"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } /delete this routine/ { last = $0 } END { print last }' \
  "$DELETE" | sed 's/\x1b\[[0-9;]*m//g' >"$LAST"
has "x opens the delete popup" "delete this routine" "$LAST"
has "naming the routine and its task count" "nightly · 2 tasks" "$LAST"
has "the whole-folder job beside its store" "nightly-audit   user" "$LAST"
has "the single-task job beside its store" "audit-docs      project" "$LAST"
lacks "never a job on another routine" "weekly-prune" "$LAST"
has "under its own keys" "[enter] delete   [esc] keep" "$LAST"

# x asks, enter deletes the jobs and then the folder.
on_screen '\x1b[Cjjx\r' /dev/null
works "x then enter removes the routine folder" \
  test ! -e .spoolway/routines/nightly
lacks "and its whole-folder job from the user store" "[jobs.nightly-audit]" "$USER_STORE"
lacks "and its single-task job from the project store" "[jobs.audit-docs]" "$PROJECT_STORE"
has "a job on another routine is left alone" "[jobs.weekly-prune]" "$USER_STORE"
works "the other routines stay" test -d .spoolway/routines/maintenance
works "nothing is staged" \
  bash -c '[ -z "$(git diff --cached --name-only -- .spoolway/routines .spoolway/jobs.toml)" ]'

# `n` makes a job of the highlighted routine: a schedule, a pipeline found by
# typing, `enter` — and the job is in the user store under the routine's own
# name. The folders now sort `archived-reuse`, `maintenance`, `release`, so
# `jj` puts the cursor on `release`.
NEW_JOB="$LIVE/routine-new-job.txt"
LAST="$LIVE/routine-new-job-saved.txt"
on_screen '\x1b[Cjjn0 3 * * 1-5\rbug\r' "$NEW_JOB"
has "n saves the job to the user store" "[jobs.release]" "$USER_STORE"
has "with the routine it was made on" 'routine = "release"' "$USER_STORE"
has "with the schedule typed" 'schedule = "0 3 * * 1-5"' "$USER_STORE"
has "with the pipeline picked" 'pipeline = "bugfix"' "$USER_STORE"
lacks "never in the project store" "[jobs.release]" "$PROJECT_STORE"
awk 'BEGIN { RS = "\033\\[\\?2026h\033\\[H" } /job saved/ { last = $0 } END { print last }' \
  "$NEW_JOB" | sed 's/\x1b\[[0-9;]*m//g' >"$LAST"
has "and says so in a popup" "release runs at 03:00, Monday to Friday, on bugfix." "$LAST"

# A second `n` on the same routine is refused: the name is taken.
REFUSED="$LIVE/routine-new-job-refused.txt"
on_screen '\x1b[Cjjn0 4 * * *\r\r' "$REFUSED"
has "n on a routine whose job exists is refused" "already exists" "$REFUSED"
works "and the store keeps the one job" \
  bash -c '[ "$(grep -c "^\[jobs.release\]" "$1")" = 1 ] && grep -qF "schedule = \"0 3 * * 1-5\"" "$1"' \
  _ "$USER_STORE"

# A routine folder holding a file that will not parse is refused whole, and
# the refusal names the file: queueing it any other way would leave the
# routine a task short without a word. `zz-broken` sorts after every other
# folder here, so three `j` put the cursor on it.
mkdir -p .spoolway/routines/zz-broken
task_doc .spoolway/routines/zz-broken/fine.md zz-fine "$BODY" "group: zz-broken"
printf 'no frontmatter fence here\n' >.spoolway/routines/zz-broken/torn.md
BROKEN="$LIVE/routine-unparseable.txt"
on_screen '\x1b[Cjjj \r' "$BROKEN"
has "queueing a routine folder with an unparseable file is refused" "queue refused" "$BROKEN"
has "naming the file" "torn.md" "$BROKEN"
works "and none of the folder's other tasks was queued" \
  bash -c '! ls "$1"/queue/zz-fine-* >/dev/null 2>&1' _ "$SPOOLWAY_PROJECT_HOME"

finish
