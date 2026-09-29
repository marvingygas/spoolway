#!/usr/bin/env bash
# The override command surface, end to end: forking a knob out of the
# tracked checkout, seeing it listed, and promoting it back in. Also carries
# the four new `contract` commands (`config`, `override`, `template`, `hook`)
# and `spoolway sync`'s removal of the skill directory `spoolway-pipeline`
# was renamed from — CLI-level checks with nowhere better-fitting to live,
# for the same reason the rest of this file is a suite rather than a unit
# test: what matters is the process actually wired up, not the render.
#
# Everything a unit test can decide about this already is — `src/overrides.rs`
# asserts the promoted file is byte-identical apart from the named values,
# `KEY_BLOCK` still findable, every `description:` intact, comments on an
# edited line preserved, empty layer directories swept — all against a
# fixture string, never a checkout. What none of that reaches is the one
# thing the Goal actually promises: that forking a knob touches nothing `git`
# can see, because the layer lives outside the checkout entirely, and that
# promoting one turns into exactly the diff a person would commit. That needs
# a real git repository with a real working tree to ask `git status` and
# `git diff` of, so it is a suite rather than another case in
# `src/overrides.rs`.
#
# No `covers:` tag of its own — see `commands.sh`'s own note: the map
# `coverage.sh` builds only enumerates `config.toml` keys and pipeline step
# keys, and this is CLI behaviour, not a setting.
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

TRACKED=.spoolway/pipelines/default.yml
LAYER="$SPOOLWAY_PROJECT_HOME/overrides/pipelines/default.yml"

# `configure_project` points every Claude step's blank model at the cloud
# stand-in `set_models` installs — the `default` pipeline's `implement` step
# among them — so this is the value already sitting in the tracked file
# before anything here forks it.
OLD_MODEL=$(local_model default)

git_clean() { test -z "$(git status --porcelain)"; }
git_dirty() { ! git_clean; }

works "the checkout starts clean" git_clean

says "pipeline override writes the patch and shows the change" \
  "implement.model   $OLD_MODEL -> fake-opus" \
  "$SPOOLWAY" pipeline override default --set implement.model=fake-opus

works "the patch landed in the layer, not the checkout" \
  test -f "$LAYER"
works "forking a knob leaves the tracked checkout untouched" \
  git_clean
says "the tracked file itself is unmoved" "model: $OLD_MODEL" \
  cat "$TRACKED"

says "override list names the pipeline patch and its key" \
  "implement.model" \
  "$SPOOLWAY" override list
says "and prints the artifact count in its footer" \
  "1 artifact" \
  "$SPOOLWAY" override list
says "naming the layer's own fingerprint, not the tracked files' combined one" \
  "layer version" \
  "$SPOOLWAY" override list

says "override promote writes the tracked file and reports the new value" \
  "implement.model   fake-opus" \
  "$SPOOLWAY" override promote default

says "the tracked file now carries the promoted value" \
  "model: fake-opus" \
  cat "$TRACKED"
says "the fenced key block survives the edit" \
  ">>> spoolway >>>" \
  cat "$TRACKED"
says "so does the step description beside the promoted key" \
  "Write the code to satisfy the task's acceptance criteria." \
  cat "$TRACKED"

works "promoting cleared the layer entry" \
  test ! -e "$LAYER"
says "override list says the layer is empty again" \
  "no overrides" \
  "$SPOOLWAY" override list

works "the promote is a real, uncommitted change to the tracked file" \
  git_dirty
says "and it is exactly the one line the patch named — nothing else moved" \
  " 1 file changed, 1 insertion(+), 1 deletion(-)" \
  git diff --stat -- "$TRACKED"

# --------------------------------------------------- dispatch's gate, no tty
# Every command above ran through `$(...)`, which never hands the child a
# terminal — so this is already the case the acceptance criteria asks for:
# with a layer present and stdout not a terminal, `dispatch` must print the
# notice and proceed without ever waiting on a key nobody can press.
# `src/commands/dispatch.rs`'s own unit tests cover the same shape directly
# against `overrides_gate_with`, with a scripted reader standing in for the
# terminal; what only a real process can show is that a real `spoolway
# dispatch` invocation, piped the way every script pipes it, never blocks on
# this at all.
BODY="$LIVE/body.md"
task_body "$BODY"
task_doc "$LIVE/gate.md" gate "$BODY" "group: live"
must "a task to dispatch against" "$SPOOLWAY" queue add --from "$LIVE/gate.md"

# How each case below runs its dispatch, and why it is no longer a one-liner.
#
# `spoolway dispatch --dry-run` used to be what made each of these a single
# piped pass that printed the gate and stopped. The flag is gone, and there
# is no early exit left anywhere between the three gates and
# `Lock::acquire` — anything that reaches those lines goes on to hold the
# lock and stay resident. So the only way left to ask a gate what it does
# with no tty is to ask a real run: started in its own session with neither
# stream a terminal, watched until its own log shows it past all three gates
# and into the pass loop, then stopped along with every lane it launched.
#
# One run answers every question in its phase, which is why each phase here
# captures once and asserts against that capture several times — three real
# dispatches rather than the seven throwaway ones this file used to do.
#
# `dispatch` prints a line per pass rather than drawing a board — the gates
# and the pane check both sit ahead of that, and the plain log is the only
# form a captured file can be read back from.
GATE_LOG="$LIVE/gate-run.log"
GATE_PID="$LIVE/gate-run.pid"
# Printed by the pass loop, so it is only ever reached past the lock — which
# makes it both the signal to stop watching and the proof that nothing on
# the way there stopped to ask.
PAST_THE_GATES="next pass in"

gate_run() {
  : > "$GATE_LOG"
  rm -f "$GATE_PID"
  # Its own session, so one signal takes the dispatcher and its lanes
  # together — the same shape as `dispatcher_start`, and for the same
  # reason. `setsid` may or may not fork, so the group leader `exec`s the
  # binary over itself after writing its own pid.
  setsid bash -c 'echo $$ > "$2"; exec "$1" dispatch' \
    _ "$SPOOLWAY" "$GATE_PID" >"$GATE_LOG" 2>&1 &
  disown
  poll_until 10 test -s "$GATE_PID"
  wait_for_text 60 "$GATE_LOG" "$PAST_THE_GATES"
  local pid; pid=$(cat "$GATE_PID" 2>/dev/null)
  [ -n "$pid" ] || return 0
  kill -TERM -- "-$pid" 2>/dev/null
  poll_while 5 kill -0 -- "-$pid"
  kill -KILL -- "-$pid" 2>/dev/null
  return 0
}

# The regression this guards: `overrides_gate`'s own `TermGuard` used to be
# taken unconditionally, so a piped dispatch printed a hide/show-cursor
# escape as its first bytes even with no layer at all. With nothing
# overridden yet, this run must be byte-for-byte silent about the cursor.
#
# It has to be a run that actually reaches `overrides_gate` to prove that,
# which is the whole reason for the shape above: clap's own error text for a
# flag that no longer exists contains no escape either, so a refused
# invocation would report `ok` here having asked nothing.
gate_run
lacks "a piped dispatch with no layer never touches the cursor" \
  $'\x1b' "$GATE_LOG"
has "and that run really did get past the gate, rather than never reaching it" \
  "$PAST_THE_GATES" "$GATE_LOG"

must "forking a knob again, for the gate" \
  "$SPOOLWAY" pipeline override default --set implement.model=fake-opus

gate_run
has "dispatch prints the layer's notice with no tty to ask" \
  "overrides are active" "$GATE_LOG"
has "naming the pipeline it touches" \
  "pipelines/default.yml" "$GATE_LOG"
has "and proceeds without anybody there to answer" \
  "$PAST_THE_GATES" "$GATE_LOG"

# ------------------------------------------------- the warnings screen, no tty
# The gate task `warnings-screen` adds sits right beside `overrides_gate`
# above and is proved the same way: `src/commands/dispatch.rs`'s own unit
# tests cover `warnings_gate_with`'s render and its print-and-proceed path
# against a scripted reader, but only a real process piped the way every
# script here pipes it can show that doctor's cheap findings, once they are
# real data rather than a fixture, still never block a `spoolway dispatch`
# with nothing there to answer. Turning on `unattended.enabled` and leaving
# both ceilings at their default of 0 is also the mockup's own example line
# — doctor's own `unattended.enabled` note, which the gate now carries
# under `settings` with nothing of its own added — so this is the one
# config shape in this file guaranteed to give the new screen something to
# say.
must "unattended, with neither ceiling set — the mockup's own case" \
  "$SPOOLWAY" config set unattended.enabled true

gate_run
has "dispatch prints the warnings screen's own heading with no tty to ask" \
  "before this run starts" "$GATE_LOG"
has "and doctor's own unattended note the mockup draws" \
  "unattended.enabled is on with no unattended.max_output_tokens" "$GATE_LOG"
has "and proceeds without anybody there to answer, same as the overrides gate beside it" \
  "$PAST_THE_GATES" "$GATE_LOG"

must "unattended off again, so nothing later in this file inherits it" \
  "$SPOOLWAY" config set unattended.enabled false

# Taken back out rather than left behind: the three runs above each started
# a real lane on it and were stopped mid-turn, so it is sitting on
# `implement` with a worktree of its own, and nothing below this line is
# about it. `--force` because of exactly that — a plain `unqueue` refuses a
# task that has left `queued`, and tearing the checkout down is the point.
must "the dispatched-against task is taken back out" \
  "$SPOOLWAY" queue unqueue gate --force

# ------------------------------------------------------------ a stale override
# The incident the group's own task describes: a step's own shape changes
# under an override that still names a key the step no longer takes — here,
# `review` turning from an agent step into a command step while the layer
# still sets `review.agent`. Before this task every command refused to load
# at all; now the stale entry is skipped, one stderr line names it, and
# every other override in the file still applies.
must "fork a knob on the step that is about to change shape" \
  "$SPOOLWAY" pipeline override default --set review.agent=fake-agent
# A command step may declare none of `prompt`, `model`, `effort` or
# `session` — those are a lane's — so turning `review` into one has to drop
# all four, not just swap `agent:` for `run:`, or the tracked file is
# invalid on its own before the override ever enters the picture.
must "review turns into a command step, the override none the wiser" \
  sed -i '/- id: review/,/- id: document/ {
    s/^    agent: claude/    run: echo hi/
    /^    prompt:/d
    /^    model:/d
    /^    effort:/d
    /^    session:/d
  }' "$TRACKED"

LAYER_BYTES_BEFORE=$(cat "$LAYER")

works "a plain command still loads — the stale entry is skipped, not refused" \
  "$SPOOLWAY" pipeline check
says "and its stderr names exactly the override left out" \
  "spoolway: override ignored — pipelines/default.yml step \`review\`: names both \`run:\` and \`agent:\` — a step runs a process or a model, not both" \
  "$SPOOLWAY" pipeline check
says "stdout still carries the ordinary report" \
  "pipeline(s) valid" \
  "$SPOOLWAY" pipeline check

# `says` reads both streams merged, so the three checks above cannot tell
# which stream the notice went to, nor how often. One command loads its
# pipelines several times over (`main.rs` ahead of its dispatch, the command
# again after), which only a whole process shows — so split the streams of
# one real invocation and count.
IGNORED_LINE="spoolway: override ignored — pipelines/default.yml step \`review\`"
STALE_ERR="$LIVE/stale-check.err"
STALE_OUT=$("$SPOOLWAY" pipeline check 2>"$STALE_ERR")
if [ "$(grep -cF -- "$IGNORED_LINE" "$STALE_ERR")" = 1 ]; then
  ok "the notice is printed exactly once, however many times the command loads"
else
  bad "the notice is printed exactly once, however many times the command loads"
  sed 's/^/        /' "$STALE_ERR"
fi
if grep -qF -- "override ignored" <<<"$STALE_OUT"; then
  bad "and it goes to stderr only — stdout is left as it was"
  sed 's/^/        /' <<<"$STALE_OUT"
else
  ok "and it goes to stderr only — stdout is left as it was"
fi

# Inside a lane the same load is silent: `SPOOLWAY_TASK` in the environment
# is how a real process knows it is one.
silent_about "a command run inside a lane prints nothing about the ignored override" \
  "override ignored" \
  env SPOOLWAY_TASK=gate "$SPOOLWAY" pipeline check
works "and still loads" \
  env SPOOLWAY_TASK=gate "$SPOOLWAY" pipeline check

works "the override file is never edited or deleted" \
  test -f "$LAYER"
if [ "$(cat "$LAYER")" = "$LAYER_BYTES_BEFORE" ]; then
  ok "and its bytes are exactly what they were before the load that skipped it"
else
  bad "and its bytes are exactly what they were before the load that skipped it"
fi

says "override list moves the stale entry into its own ignored line" \
  "ignored  review.agent" \
  "$SPOOLWAY" override list
says "while the entry beside it in the same file is still listed as applying" \
  "patch  implement.model" \
  "$SPOOLWAY" override list

# Bare `spoolway` says the same thing as a popup over the tab it opens on:
# the stderr line above is printed ahead of the screen, and its first frame
# clears it before anybody could read it. `frame N` pulls the Nth frame out
# of a screen's typescript — every frame opens on a clear-screen — with the
# colour codes taken out, so a row reads as the plain line the Mockup draws.
frame() {
  awk -v n="$(($2 + 1))" 'BEGIN { RS = "\033\\[2J\033\\[H" } NR == n { print; exit }' "$1" |
    sed 's/\x1b\[[0-9;]*m//g'
}
IGNORED_SCREEN="$LIVE/ignored-screen.txt"
# `on_screen` runs `spoolway sync` first, and sync rewrites the fixture's
# `config.toml` into its own table order (`[agents.pi]` moves up), which
# `git status` would then blame on the restore below. Settle that rewrite
# into the fixture's history before the screen ever opens.
"$SPOOLWAY" sync >/dev/null 2>&1 || true
git commit -qm "settle config.toml the way sync writes it" -- .spoolway/config.toml >/dev/null 2>&1 || true
on_screen '\r' "$IGNORED_SCREEN"
frame "$IGNORED_SCREEN" 1 >"$IGNORED_SCREEN.first"
frame "$IGNORED_SCREEN" 2 >"$IGNORED_SCREEN.second"
has "bare spoolway opens on the override ignored popup" \
  "┌─ override ignored " "$IGNORED_SCREEN.first"
has "over the queue tab it opens on" \
  "dispatch       [queue]       routines        jobs        eval" "$IGNORED_SCREEN.first"
has "naming the file, the step and the keys it set" \
  "pipelines/default.yml   step review   agent" "$IGNORED_SCREEN.first"
# The reason is wrapped to the queue tab's frame, so only its first row is
# read whole: the part after the em dash is what `override list` leaves out.
has "with the whole reason under them" \
  "names both \`run:\` and \`agent:\` — a step runs a process or a model," \
  "$IGNORED_SCREEN.first"
has "and enter to close it" "[enter] close" "$IGNORED_SCREEN.first"
lacks "enter closes it" "override ignored" "$IGNORED_SCREEN.second"
has "and the queue tab is drawn again under the strip" \
  "dispatch       [queue]       routines        jobs        eval" "$IGNORED_SCREEN.second"

# Never acknowledged: the next open asks again while the override is still
# ignored.
on_screen '\r' "$IGNORED_SCREEN.again"
frame "$IGNORED_SCREEN.again" 1 >"$IGNORED_SCREEN.again.first"
has "the next open shows it again" \
  "┌─ override ignored " "$IGNORED_SCREEN.again.first"

must "clean up: drop the layer entry" \
  "$SPOOLWAY" override drop pipelines/default.yml
# `git checkout --`, not a reverse `sed`: the edit above deleted lines
# rather than only substituting them, so there is no single reverse
# substitution that puts `review` back — restoring from the commit
# `configure_project` made is the actual inverse.
must "and restore the tracked step from the committed copy" \
  git checkout -- "$TRACKED"
works "the checkout is clean again" git_clean

# With nothing in the layer ignored any more, the screen opens on no popup.
on_screen '' "$IGNORED_SCREEN.gone"
frame "$IGNORED_SCREEN.gone" 1 >"$IGNORED_SCREEN.gone.first"
lacks "once nothing is ignored the screen opens without the popup" \
  "override ignored" "$IGNORED_SCREEN.gone.first"

# ------------------------------------------------- the four new contracts
# Each one is a unit-tested render in src/commands/{config,override,template,
# hook}.rs already; what a unit test cannot see is the command actually
# wired up end to end — clap routing the subcommand, `main.rs` calling the
# right function, and the process exiting zero with something on stdout.
works "config contract exits zero" "$SPOOLWAY" config contract
says "and prints the register config.toml's own header renders from" \
  "dispatch.backend" \
  "$SPOOLWAY" config contract

works "override contract exits zero" "$SPOOLWAY" override contract
says "and names the merge rule" \
  "already exist on the tracked pipeline" \
  "$SPOOLWAY" override contract

works "template contract exits zero" "$SPOOLWAY" template contract
says "and names the one shape it still has" \
  "TASK —" \
  "$SPOOLWAY" template contract

works "hook contract exits zero" "$SPOOLWAY" hook contract
says "and names the events a hook script runs on" \
  "SPOOLWAY_EVENT" \
  "$SPOOLWAY" hook contract

# ----------------------------------------------- sync removes the old skill
# `spoolway-pipeline` was renamed to `spoolway-config`; a project that ran
# `install` before the rename has the old directory on disk, and `sync`
# is the one chance to clean it up without touching anything else there.
STALE=.claude/skills/spoolway-pipeline
UNRELATED=.claude/skills/a-projects-own-skill
mkdir -p "$STALE" "$UNRELATED"
echo "the old skill" > "$STALE/SKILL.md"
echo "not spoolway's" > "$UNRELATED/SKILL.md"

must "sync, to clean up the rename" "$SPOOLWAY" sync

works "the stale renamed skill is gone" test ! -e "$STALE"
works "a directory not on the retired list is untouched" \
  test -f "$UNRELATED/SKILL.md"
works "and the renamed skill itself is installed" \
  test -f .claude/skills/spoolway-config/SKILL.md

finish
