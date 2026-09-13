#!/usr/bin/env bash
# The queue screen's `p` picker, driven end to end — the one path no unit
# test can drive, since `run_screen` is exercised headlessly in Rust
# already, but never as the whole binary reading real keystrokes off a real
# pipe. `p` forks a whole group: one pipeline assigned per task on the first
# popup, one skip set per task on the second, one new arm per source task
# under one freshly minted trial id.
#
# Nothing here drives a dispatcher: a trial's whole job is landing arms in
# the queue directory with the right `pipeline:`, `skip:`, `group:` and
# `trial:` on them, and that is what is asserted — not that any of them ever
# runs. No `new_forge`/`install_agents` needed for the same reason. The
# dispatch-and-cleanup half of a trial's own life — what a dispatcher does
# with these arms, and what tears a trial down once it is compared — is the
# dependent `trial-cleanup` task's own proof.
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

# Two pending documents sharing one group, `beta` depending on `alpha` — the
# unit `p` now forks whole. `spoolway init` writes exactly the two built-in
# pipelines, `default` and `bugfix`, under `.spoolway/pipelines/`, and
# `trial_pipeline_names` walks them in the same alphabetical order
# (`bugfix`, `default`) the assign screen's `←`/`→` cycles through.
BODY="$LIVE/body.md"
task_body "$BODY"
pending_doc alpha "$BODY" "group: audits" "touches: [src/main.rs]"
pending_doc beta "$BODY" "group: audits" "touches: [src/main.rs]" \
  "depends_on: [alpha]"

# `Tab` focuses the tasks pane on a task inside the group (`alpha`, first in
# reading order since it is the dependency), proving `p` reaches the whole
# group from a selected task too, not only from the groups pane. `p` opens
# the assign-pipelines popup with every task already defaulted to this
# project's own default pipeline; `←` cycles `alpha` onto `bugfix`, `j`
# moves onto `beta` (left on `default`), `enter` advances to the skips
# screen. There the flattened cursor opens on `alpha`'s first checkbox: one
# `j` reaches its second, `fix`, and `space` ticks it; nine more `j`s walk
# past the rest of `alpha`'s own checkboxes onto `beta`'s third one,
# `document`, and `space` ticks that too. `enter` mints and writes both
# arms; `n` declines the dispatcher offer.
printf '\tp\x1b[Dj\rj jjjjjjjjj \rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the alpha arm reaches the queue" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
works "and the beta arm beside it" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"
works "two distinct minted ids — never either document's own bare one" \
  bash -c '[ ! -e "$1/queue/alpha.md" ] && [ ! -e "$1/queue/beta.md" ]' \
  _ "$SPOOLWAY_PROJECT_HOME"

has "alpha's arm carries the pipeline cycled onto it" "pipeline: bugfix" \
  "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
has "beta's arm keeps the default it was never cycled off of" \
  "pipeline: default" "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"
works "alpha's own ticked skip, and none of beta's" \
  bash -c 'grep -A1 "^skip:" "$1" | tail -1 | grep -qxF -- "- fix"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
works "beta's own ticked skip, and none of alpha's" \
  bash -c 'grep -A1 "^skip:" "$1" | tail -1 | grep -qxF -- "- document"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"
has "both arms keep the document's own group" "group: audits" \
  "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
has "on both arms" "group: audits" \
  "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"

works "the shared trial id is freshly minted, t plus sixteen hex" \
  bash -c 'grep -qE "^trial: t[0-9a-f]{16}$" "$1"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/alpha-1.md"
works "and the same id lands on both arms of the one launch" \
  bash -c '
    a=$(grep "^trial:" "$1/queue/alpha-1.md")
    b=$(grep "^trial:" "$1/queue/beta-1.md")
    [ -n "$a" ] && [ "$a" = "$b" ]
  ' _ "$SPOOLWAY_PROJECT_HOME"

has "beta's depends_on is remapped onto alpha's own minted sibling id" \
  "alpha-1" "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"
works "not left naming the bare id nothing in this batch is queued under" \
  bash -c '! grep -qxF -- "- alpha" "$1"' \
  _ "$SPOOLWAY_PROJECT_HOME/queue/beta-1.md"

# A trial forks the documents; it does not submit them. The pending copies
# are templates the picker read from, not a batch the screen queued and
# cleared.
works "both source documents are left exactly where they were" \
  bash -c 'test -f "$1/pending/alpha.md" && test -f "$1/pending/beta.md"' \
  _ "$SPOOLWAY_PROJECT_HOME"

# A second template, this one already run through a pipeline once: its sole
# document lives in the queue directory, carrying every key spoolway stamped
# on that run — `stage:` chief among them, which `parse_submission` refuses
# outright. `list_groups` reads such a group's task straight out of `queue/`,
# verbatim, so forking it is `p` over exactly the shape a task queued or
# archived earlier has. Named clear of `p` and `s` on purpose: both are
# reserved actions the instant a filter is open, so either letter inside the
# query itself would fire the action early rather than narrow the list —
# `f` narrows to it by name, `p` reaches it straight from the groups pane,
# with no `Tab` needed. `enter` advances past the assign screen without
# touching the pipeline it already defaulted to, and `enter` again launches
# with nothing ticked to skip.
task_doc "$SPOOLWAY_PROJECT_HOME/queue/old-run.md" old-run "$BODY" \
  "group: old-run" \
  "touches: [src/main.rs]" \
  "stage: done" \
  "run: r00000000000000af" \
  "branch: task/old-run" \
  "base: master" \
  "cut_from: master" \
  "base_commit: 0000000000000000000000000000000000000000" \
  "attempts: 2"

printf 'fold-run\rp\r\rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the reset lets a stamped document reach \`finish_trial\` at all" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"
has "under the pipeline it defaulted to" "pipeline: default" \
  "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"
lacks "the reset held: no stamped stage on the forked arm" "stage: done" \
  "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"
lacks "nor the run id the earlier run minted" "run: r00000000000000af" \
  "$SPOOLWAY_PROJECT_HOME/queue/old-run-1.md"

finish
