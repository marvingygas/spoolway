#!/usr/bin/env bash
# One task, forked across two pipelines from the queue screen's own `p`
# picker — the one path no unit test can drive end to end, since
# `run_screen` is exercised headlessly in Rust already, but never as the
# whole binary reading real keystrokes off a real pipe.
#
# Nothing here drives a dispatcher: a trial's whole job is landing two task
# files in the queue directory with the right `pipeline:`, `skip:` and
# `group:` on them, and that is what is asserted — not that either arm ever
# runs. No `new_forge`/`install_agents` needed for the same reason.
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

# One pending document, the template every arm is forked from. `spoolway
# init` writes exactly the two built-in pipelines, `default` and `bugfix`,
# under `.spoolway/pipelines/` — the same pair the picker lists, in the same
# alphabetical order the picker's cursor walks.
BODY="$LIVE/body.md"
task_body "$BODY"
pending_doc solo "$BODY" "group: audits" "touches: [src/main.rs]"

# Tab onto the tasks pane, `p` opens the picker on its first row (`bugfix`,
# alphabetically first); space ticks it, `j` walks down onto `default`,
# space ticks that too. Seven more `j`s walk onto `checks`, one short of the
# last of the eight steps the two pipelines' union offers as candidates to
# skip — space ticks it, `enter` queues both arms, `n` declines the
# dispatcher offer.
printf '\tp j jjjjjjj \rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the bugfix arm reaches the queue" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/solo-1.md"
works "and the default arm beside it" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/solo-2.md"
works "two distinct minted ids — never the document's own bare one" \
  test ! -e "$SPOOLWAY_PROJECT_HOME/queue/solo.md"

says "the first arm carries its own pipeline" "pipeline: bugfix" \
  cat "$SPOOLWAY_PROJECT_HOME/queue/solo-1.md"
says "the second arm carries its own pipeline" "pipeline: default" \
  cat "$SPOOLWAY_PROJECT_HOME/queue/solo-2.md"
says "the first arm's ticked skip list" "checks" \
  grep -A1 '^skip:' "$SPOOLWAY_PROJECT_HOME/queue/solo-1.md"
says "the second arm's ticked skip list" "checks" \
  grep -A1 '^skip:' "$SPOOLWAY_PROJECT_HOME/queue/solo-2.md"
says "both arms keep the document's own group" "group: audits" \
  cat "$SPOOLWAY_PROJECT_HOME/queue/solo-1.md"
says "on both arms" "group: audits" \
  cat "$SPOOLWAY_PROJECT_HOME/queue/solo-2.md"

# A trial forks the document; it does not submit it. The pending copy is a
# template the picker read from, not a batch the screen queued and cleared.
works "the source document is left exactly where it was" \
  test -f "$SPOOLWAY_PROJECT_HOME/pending/solo.md"

# A second template, this one already run through a pipeline once: its sole
# document lives in the queue directory, carrying every key spoolway stamped
# on that run — `stage:` chief among them, which `parse_submission` refuses
# outright. `list_groups` reads such a group's task straight out of `queue/`,
# verbatim, so forking it is `p` over exactly the shape a task queued or
# archived earlier has. `f` narrows the picker to it by name; `tab` then
# moves focus onto the tasks pane, which `p` is gated on.
task_doc "$SPOOLWAY_PROJECT_HOME/queue/was-queued.md" was-queued "$BODY" \
  "group: was-queued" \
  "touches: [src/main.rs]" \
  "stage: done" \
  "run: r00000000000000af" \
  "branch: task/was-queued" \
  "base: master" \
  "cut_from: master" \
  "base_commit: 0000000000000000000000000000000000000000" \
  "attempts: 2"

printf 'fwas-queued\r\tp \rn' | "$SPOOLWAY" queue >/dev/null 2>&1

works "the reset lets a stamped document reach \`finish_trial\` at all" \
  test -f "$SPOOLWAY_PROJECT_HOME/queue/was-queued-1.md"
says "under its own picked pipeline" "pipeline: bugfix" \
  cat "$SPOOLWAY_PROJECT_HOME/queue/was-queued-1.md"
lacks "the reset held: no stamped stage on the forked arm" "stage: done" \
  "$SPOOLWAY_PROJECT_HOME/queue/was-queued-1.md"
lacks "nor the run id the earlier run minted" "run: r00000000000000af" \
  "$SPOOLWAY_PROJECT_HOME/queue/was-queued-1.md"

finish
