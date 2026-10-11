#!/usr/bin/env bash
# Faults injected at the moments the 2026-10-10 stress test found bugs by hand.
#
# The other suites kill or starve the dispatcher only at moments a test can
# reach from outside: a lane mid-turn, a pass between two tasks. The failures
# found by the stress test sit inside steps that a small fixture finishes in
# milliseconds — the middle of a `git worktree add`, the middle of a
# `git worktree remove` — or need a condition no suite had: a terminal on the
# dispatcher's stdin, an ssh transport that stalls, eight `config set` calls at
# once, a `resume` landing while an unblocker is mid-turn.
#
# `SPOOLWAY_TEST_KILL_AT=<point>` (src/fault.rs) makes `spoolway dispatch`
# SIGKILL itself at a named point, once. The kill cases run a one-shot
# dispatcher with it set, look at what the kill left behind, and only then
# start the ordinary supervised one that has to cope with it. The worktree
# cases use a repository of a few thousand tracked files, so the checkout is
# still being written, or deleted, when the kill lands.
#
# Each case asserts the behaviour its fix promises. A fix that has not reached
# `main` leaves its case marked with `expect_fail` (lib.sh), which keeps the
# suite green and prints a line once the case starts passing. The check that
# the fault was really injected is never inside a mark: a kill that landed too
# late must fail the suite, not pass it.
#
# Measured at 212 seconds on a laptop with a debug build (2026-10-10, every
# case still marked expected-fail, so each waits out its own bound). That is
# over the two minutes a `pr` suite may take, so it is in `nightly` only.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=../lib.sh
source "$HERE/../lib.sh"
# shellcheck source=../fixture.sh
source "$HERE/../fixture.sh"
# shellcheck source=../agents.sh
source "$HERE/../agents.sh"

LIVE=${WORK:-$(mktemp -d)}
CTL="$LIVE/ctl"

new_forge "$LIVE/forge"
install_agents "$LIVE/bin" "$CTL" "" "" "" "$FORGE"

new_repo "$LIVE/proj"
configure_project plan/faults

# Ten thousand tracked files spread over many directories. `git worktree
# add` writes them one at a time and `git worktree remove` deletes them one
# directory at a time, so a kill that lands as the first of either has happened
# leaves a checkout with most of its files missing, or most still there.
for d in $(seq 1 50); do
  mkdir -p "bulk/d$d"
  touch "bulk/d$d"/f{1..200}.txt
done
must "the bulk files" git add -A
must "the bulk commit" git commit -qm "bulk"
publish plan/faults
ORIGIN="$FORGE/origin.git"
TRACKED=$(git ls-tree -r --name-only plan/faults | wc -l)

BODY="$LIVE/body.md"
task_body "$BODY"

E2E_DISPATCH_DIR="$LIVE"
E2E_DISPATCH_LOG="$LIVE/dispatch.log"
: > "$E2E_DISPATCH_LOG"

# shoot <point>
#
# One `spoolway dispatch` with the kill hook set to <point>, on its own and
# unsupervised, so nothing restarts it before the case has looked at what the
# kill left. Returns 0 once it is gone and the hook's marker says it fired.
shoot() {
  local point=$1 pidfile="$LIVE/shot.pid" pid
  rm -f "$pidfile"
  printf -- '--- one-shot dispatcher, killed at %s ---\n' "$point" >> "$E2E_DISPATCH_LOG"
  SPOOLWAY_TEST_KILL_AT=$point setsid bash -c 'echo $$ >"$1"; shift; exec "$@"' \
    _ "$pidfile" "$SPOOLWAY" dispatch >> "$E2E_DISPATCH_LOG" 2>&1 &
  disown
  poll_until 10 test -s "$pidfile" || return 1
  pid=$(cat "$pidfile")
  if ! poll_while 120 kill -0 "$pid"; then
    kill -KILL "$pid" 2>/dev/null
    return 1
  fi
  [ -e "$LIVE/proj/.git/spoolway-test-kill-$point" ]
}

# The path git has registered for a task's worktree, whatever state its
# directory is in.
registered_worktree() {
  git worktree list --porcelain | sed -n 's/^worktree //p' | grep -F "/task-$1" | head -1
}

# How many regular files a checkout holds, not counting git's own.
files_in() {
  find "$1" -type f -not -path "$1/.git" -not -path "$1/.git/*" 2>/dev/null | wc -l
}

# Files a branch deleted relative to another ref, read from the forge and from
# the local repository. A restart's commit lands on the local branch, and only
# a later push puts it on the forge, so reading one side alone misses it.
deleted_between() {
  git -C "$ORIGIN" diff --name-only --diff-filter=D "$1" "$2" 2>/dev/null
  git diff --name-only --diff-filter=D "$1" "$2" 2>/dev/null
}

# Subjects of the commits on a local branch that a ref does not have.
commits_beyond() {
  git log --format=%s "$1..$2" 2>/dev/null
}

# await <task> <want> <secs>
#
# Wait for a task to reach a stage (`gone` = archived), with no dispatcher
# started here. Gives up early on a task that settled somewhere it will not
# leave on its own when `gone` was wanted.
await() {
  local task=$1 want=$2 secs=${3:-120} i stage
  for ((i = 0; i < secs * 5; i++)); do
    stage=$(stage_of "$task")
    case "$want" in
      gone) [ -z "$stage" ] && [ -f "$SPOOLWAY_PROJECT_HOME/archive/$task.md" ] && return 0 ;;
      *)    [ "$stage" = "$want" ] && return 0 ;;
    esac
    if [ "$want" = gone ]; then
      case "$stage" in blocked | paused) break ;; esac
    fi
    sleep 0.2
  done
  printf '  \033[31mgave up\033[0m waiting for %s to reach %s (at `%s`)\n' \
    "$task" "$want" "$(stage_of "$task")" >&2
  return 1
}

# settle <task> <want> <secs> — `await` with the supervised dispatcher running.
settle() {
  dispatcher_start
  await "$@"
}

# What a person abandoning a task by hand does, for a task a case leaves
# parked: its lane's process ended, its worktree and branch removed with git,
# its file out of the queue. Same as `disaster.sh`'s `forget`.
forget() {
  local id=$1 pid wt
  pid=$(cat "$SPOOLWAY_PROJECT_HOME/headless/$id · $(stage_of "$id").pid" 2>/dev/null)
  [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null
  wt=$(worktree_of "$id")
  [ -n "$wt" ] && git worktree remove --force "$wt" 2>/dev/null
  git branch -D "task/$id" 2>/dev/null
  rm -f "$SPOOLWAY_PROJECT_HOME/queue/$id.md"
  true
}

# -------------------------------------------- D-1: a kill inside worktree add
task_doc "$LIVE/cut.md" cut "$BODY" "group: cut"
must "the task to cut" "$SPOOLWAY" queue add --from "$LIVE/cut.md"
if shoot cut-during-add; then ok "the dispatcher died inside git worktree add"
else bad "the dispatcher died inside git worktree add"; fi
CUT_WT=$(registered_worktree cut)
if [ -n "$CUT_WT" ] && [ -d "$CUT_WT" ] && [ "$(files_in "$CUT_WT")" -lt "$TRACKED" ]; then
  ok "the kill left a registered worktree missing tracked files ($(files_in "$CUT_WT") of $TRACKED)"
else
  bad "the kill left a registered worktree missing tracked files (${CUT_WT:-none registered}: $([ -d "${CUT_WT:-/nonexistent}" ] && files_in "$CUT_WT" || echo 0) of $TRACKED)"
fi
expect_fail D-1 worktree-crash-safety
# Hold the implement turn open so the checkout the task runs on can be counted
# while it is in use, not after cleanup has removed it.
echo "linger:15" > "$CTL/cut.implement"
dispatcher_start
CUT_RAN_ON=-1
if lane_pid "cut · implement" 90 > /dev/null; then
  CUT_RAN_ON=$(files_in "$(worktree_of cut)")
fi
if [ "$CUT_RAN_ON" -eq "$TRACKED" ]; then
  ok "the task runs on a complete checkout ($CUT_RAN_ON of $TRACKED files)"
else bad "the task runs on a complete checkout ($CUT_RAN_ON of $TRACKED files)"; fi
if await cut gone 120; then ok "the restarted dispatcher runs the task to the end"
else bad "the restarted dispatcher runs the task to the end"; fi
lacks "the half checkout was never marked borrowed" "borrowed: true" \
  "$SPOOLWAY_PROJECT_HOME/archive/cut.md"
if [ -z "$(deleted_between plan/faults task/cut)" ]; then
  ok "nothing was committed as a deletion"
else
  bad "nothing was committed as a deletion"
  deleted_between plan/faults task/cut | head -3 | sed 's/^/        /'
fi
if [ -z "$(registered_worktree cut)" ] && [ ! -d "$CUT_WT" ]; then
  ok "and the worktree is removed at the end"
else bad "and the worktree is removed at the end (${CUT_WT:-?} is still there)"; fi
expect_fail_end
dispatcher_stop
forget cut

# ------------------------------------------ D-6: a kill inside worktree remove
task_doc "$LIVE/rma.md" rma "$BODY" "group: rm"
task_doc "$LIVE/rmb.md" rmb "$BODY" "group: rm" "depends_on: [rma]"
must "the task cleanup will remove" "$SPOOLWAY" queue add --from "$LIVE/rma.md"
must "and its dependent" "$SPOOLWAY" queue add --from "$LIVE/rmb.md"
if shoot teardown-during-remove; then ok "the dispatcher died inside git worktree remove"
else bad "the dispatcher died inside git worktree remove"; fi
RMA_WT=$(registered_worktree rma)
RMA_TIP=$(git rev-parse --verify --quiet task/rma || true)
if [ -n "$RMA_WT" ] && [ -d "$RMA_WT" ] && [ -n "$RMA_TIP" ] \
   && [ "$(files_in "$RMA_WT")" -lt "$TRACKED" ]; then
  ok "the kill left a registered, half-deleted worktree ($(files_in "$RMA_WT") of $TRACKED files)"
else
  bad "the kill left a registered, half-deleted worktree (${RMA_WT:-none registered})"
fi
# git deletes a directory's entries in the order the filesystem lists them,
# and some filesystems list the `.git` link first. A tree with its link gone
# is not the half-deleted state D-6 was found in — there, the restart still
# reached the repository from inside the checkout and committed what it found
# missing — so where this filesystem took the link first, put it back. The
# registration under `.git/worktrees/` is untouched by a kill in the middle of
# the deletion and names where the link pointed.
if [ -d "$RMA_WT" ] && [ ! -e "$RMA_WT/.git" ]; then
  printf 'gitdir: %s\n' "$LIVE/proj/.git/worktrees/$(basename "$RMA_WT")" > "$RMA_WT/.git"
fi
expect_fail D-6 worktree-crash-safety
if settle rma gone 120 && settle rmb blocked 120; then
  ok "the restart finishes both tasks"
else bad "the restart finishes both tasks"; fi
# The restart's commit goes on the local branch, which stays because rmb is
# cut from it and reaches the forge only on a later push, so these checks read
# the local branches.
RMA_WIP=$({ commits_beyond "$RMA_TIP" task/rma; commits_beyond "$RMA_TIP" task/rmb; } | grep '^wip(rma)')
if [ -n "$RMA_WIP" ]; then
  bad "the restart commits nothing from the half-removed tree"
else ok "the restart commits nothing from the half-removed tree"; fi
if [ -n "$RMA_TIP" ] && [ "$(git rev-parse --verify --quiet task/rma)" = "$RMA_TIP" ]; then
  ok "the branch tip is still the task's last real commit"
else
  bad "the branch tip is still the task's last real commit (was ${RMA_TIP:0:7}, now $(git rev-parse --short --verify --quiet task/rma || echo missing))"
  git log --format='        %h %s' "$RMA_TIP..task/rma" 2>/dev/null | head -3
fi
if git rev-parse --verify --quiet task/rmb > /dev/null \
   && [ -z "$(deleted_between "$RMA_TIP" task/rmb)" ]; then
  ok "the dependent was cut from the full tree"
else bad "the dependent was cut from the full tree"; fi
if [ -z "$(registered_worktree rma)" ]; then ok "and no worktree is left registered for the task"
else bad "and no worktree is left registered for the task"; fi
expect_fail_end
dispatcher_stop
forget rmb

# ------------------------------------- D-4: a command step's wrapper is killed
# A command that records when it starts and whether another copy of itself is
# still running, so two runs at once show up as `others=1` rather than needing
# a count of processes.
RUNS="$LIVE/runs"
mkdir -p "$RUNS"
cat > "$LIVE/slow.sh" <<SLOW
#!/bin/sh
others=0
for f in "$RUNS"/alive.*; do
  [ -e "\$f" ] || continue
  kill -0 "\${f##*.}" 2>/dev/null && others=\$((others + 1))
done
echo \$\$ > "$RUNS/alive.\$\$"
echo "\$SPOOLWAY_TASK start \$\$ others=\$others" >> "$RUNS/log"
sleep \$(cat "$CTL/slow-secs" 2>/dev/null || echo 8)
echo "\$SPOOLWAY_TASK end \$\$" >> "$RUNS/log"
rm -f "$RUNS/alive.\$\$"
SLOW
chmod +x "$LIVE/slow.sh"
cp .spoolway/pipelines/default.yml "$LIVE/default.yml.bak"
{
  printf '\n  - id: slow\n'
  printf '    description: A command that outlives a signal sent to its wrapper.\n'
  printf '    run: %s\n' "$LIVE/slow.sh"
  printf '    on_pass: review\n    on_fail: blocked\n'
} >> .spoolway/pipelines/default.yml
sed -i "0,/^    on_pass: review\$/s//    on_pass: slow/" .spoolway/pipelines/default.yml
must "the pipeline with the slow step checks out" "$SPOOLWAY" pipeline check

command_started() { grep -q "^$1 start" "$RUNS/log" 2>/dev/null; }
wrapper_of() { head -n1 "$SPOOLWAY_PROJECT_HOME/commands/$1 · slow.pid" 2>/dev/null; }

# Long enough that the dispatcher's next look at the dead wrapper (its probe
# runs every ten seconds) lands while the first run is still going.
echo 25 > "$CTL/slow-secs"
task_doc "$LIVE/cmdk.md" cmdk "$BODY" "group: cmdk"
task_doc "$LIVE/cmdt.md" cmdt "$BODY" "group: cmdt"
must "the task whose wrapper is killed" "$SPOOLWAY" queue add --from "$LIVE/cmdk.md"
dispatcher_restart
if poll_until 60 command_started cmdk; then ok "the command is running"
else bad "the command is running"; fi
WRAPPER=$(wrapper_of cmdk)
COMMAND=$(awk '$1 == "cmdk" && $2 == "start" {print $3; exit}' "$RUNS/log" 2>/dev/null)
kill -KILL "$WRAPPER" 2>/dev/null
if [ -n "$WRAPPER" ] && poll_while 5 kill -0 "$WRAPPER" && kill -0 "$COMMAND" 2>/dev/null; then
  ok "SIGKILL took the wrapper alone and the command is still running"
else bad "SIGKILL took the wrapper alone and the command is still running"; fi
expect_fail D-4 command-step-lifecycle
if await cmdk gone 90; then ok "the task runs to the end"
else bad "the task runs to the end"; fi
if grep '^cmdk start' "$RUNS/log" | grep -qv 'others=0$'; then
  bad "the command never runs twice at once after its wrapper is killed"
  sed 's/^/        /' "$RUNS/log"
else ok "the command never runs twice at once after its wrapper is killed"; fi
expect_fail_end

echo 6 > "$CTL/slow-secs"
must "the task whose wrapper takes SIGTERM" "$SPOOLWAY" queue add --from "$LIVE/cmdt.md"
if poll_until 60 command_started cmdt; then ok "the second command is running"
else bad "the second command is running"; fi
WRAPPER=$(wrapper_of cmdt)
if [ -n "$WRAPPER" ] && kill -TERM "$WRAPPER" 2>/dev/null; then ok "SIGTERM reached the wrapper"
else bad "SIGTERM reached the wrapper"; fi
expect_fail D-4 command-step-lifecycle
if await cmdt gone 90; then ok "the task runs to the end after its wrapper took SIGTERM"
else bad "the task runs to the end after its wrapper took SIGTERM"; fi
STARTS=$(grep -c '^cmdt start' "$RUNS/log")
if [ "$STARTS" -eq 1 ]; then ok "a command that exited 0 under a SIGTERMed wrapper is not run again"
else bad "a command that exited 0 under a SIGTERMed wrapper is not run again (started $STARTS times)"; fi
expect_fail_end
dispatcher_stop
cp "$LIVE/default.yml.bak" .spoolway/pipelines/default.yml

# ------------------------------------------ C-3: eight config set calls at once
cp .spoolway/config.toml "$LIVE/config.toml.bak"
for i in 1 2 3 4 5 6 7 8; do
  ( "$SPOOLWAY" config set "models.m$i.input" "$i.5" >/dev/null 2>&1; echo $? > "$LIVE/cs.$i" ) &
done
wait
SILENT=0
for i in 1 2 3 4 5 6 7 8; do
  got=$("$SPOOLWAY" config get "models.m$i.input" 2>/dev/null)
  if [ "$got" != "$i.5" ] && [ "$(cat "$LIVE/cs.$i" 2>/dev/null)" = 0 ]; then
    SILENT=$((SILENT + 1))
  fi
done
expect_fail C-3 config-set-lock
if [ "$SILENT" -eq 0 ]; then ok "every value is saved, or its call exited non-zero"
else bad "every value is saved, or its call exited non-zero ($SILENT calls succeeded and lost their value)"; fi
expect_fail_end
cp "$LIVE/config.toml.bak" .spoolway/config.toml

# --------------------------------------------- D-9: a slow git fetch in a cut
# Reached over ssh:// because git calls `core.sshCommand` only for an ssh
# transport. The command sleeps, then runs the requested git command locally,
# as a slow network would. The ids sort the slow task ahead of the free one, so
# the pass reaches the fetch first.
SLOW_BASE=task/slow-base
must "a base only origin has" git -C "$ORIGIN" branch "$SLOW_BASE" plan/faults
echo hang > "$CTL/aa-slow"
echo hang > "$CTL/zz-free"
task_doc "$LIVE/aa-slow.md" aa-slow "$BODY" "group: aa-slow" "base: $SLOW_BASE"
task_doc "$LIVE/zz-free.md" zz-free "$BODY" "group: zz-free"
must "the task on the remote-only base" "$SPOOLWAY" queue add --from "$LIVE/aa-slow.md"
must "and an unrelated one" "$SPOOLWAY" queue add --from "$LIVE/zz-free.md"
cat > "$LIVE/slow-ssh" <<SSH
#!/bin/sh
echo \$PPID >> "$LIVE/ssh.pids"
echo "ssh \$*" >> "$LIVE/ssh.log"
sleep 120
while [ \$# -gt 0 ]; do
  case "\$1" in
    -o | -p | -l | -i | -F | -J | -S) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
shift
exec sh -c "\$1"
SSH
chmod +x "$LIVE/slow-ssh"
must "origin over ssh" git remote set-url origin "ssh://localhost$ORIGIN"
must "the slow transport" git config core.sshCommand "$LIVE/slow-ssh"
# Without this git first runs the command with `-G` to learn which ssh it is,
# and that probe would sleep too.
must "and no probe of it" git config ssh.variant ssh
: > "$LIVE/ssh.log"
: > "$LIVE/ssh.pids"
dispatcher_start
if poll_until 30 test -s "$LIVE/ssh.log"; then ok "the cut reached the sleeping ssh command"
else bad "the cut reached the sleeping ssh command"; fi
expect_fail D-9 fetch-timeout
if await zz-free implement 45; then ok "the unrelated task started"
else bad "the unrelated task started"; fi
# Starting the other task is not enough: a fix that moved the fetch aside
# would pass that and still leave it hanging. The stub records its parent, which
# is the `git fetch` the cut started, and that is the process a bound has to
# stop. The sleeping stub itself is no sign: killing git leaves it behind as an
# orphan for the rest of its sleep, however well the bound works.
FETCH_PID=$(head -n1 "$LIVE/ssh.pids" 2>/dev/null)
if [ -n "$FETCH_PID" ] && ! kill -0 "$FETCH_PID" 2>/dev/null; then
  ok "the fetch gave up within its bound (its git process is gone)"
else bad "the fetch gave up within its bound (git fetch ${FETCH_PID:-?} is still waiting on the transport)"; fi
expect_fail_end
dispatcher_stop
git remote set-url origin "$ORIGIN"
git config --unset core.sshCommand
git config --unset ssh.variant
forget aa-slow
forget zz-free

# ------------------------------- R-11: resume under a live unblocker
must "unattended, so \`blocked\` is staffed" "$SPOOLWAY" config set unattended.enabled true
must "by an agent that honours the ctl files" "$SPOOLWAY" config set unattended.blocked_agent pi
echo "linger:20" > "$CTL/stuck.blocked"
task_doc "$SPOOLWAY_PROJECT_HOME/queue/stuck.md" stuck "$BODY" \
  "stage: blocked" "blocked_from: implement" "group: 0-blocked"
dispatcher_restart
STUCK_PID=$(lane_pid "stuck · blocked" 30)
if [ -n "$STUCK_PID" ] && kill -0 "$STUCK_PID" 2>/dev/null; then ok "the unblocker is mid-turn on the blocked row"
else bad "the unblocker is mid-turn on the blocked row"; fi
poll_until 15 lane_on_record "stuck · blocked"
BEFORE=$(cksum < "$SPOOLWAY_PROJECT_HOME/queue/stuck.md")
RESUME_OUT=$("$SPOOLWAY" resume stuck 2>&1); RESUME_STATUS=$?
QUEUE_OUT=$("$SPOOLWAY" queue resume stuck 2>&1); QUEUE_STATUS=$?
AFTER=$(cksum < "$SPOOLWAY_PROJECT_HOME/queue/stuck.md")
expect_fail R-11 resume-refusals
if [ "$RESUME_STATUS" -ne 0 ]; then ok "spoolway resume is refused while the unblocker works"
else bad "spoolway resume is refused while the unblocker works (it said: $RESUME_OUT)"; fi
if [ "$QUEUE_STATUS" -ne 0 ]; then ok "spoolway queue resume is refused too"
else bad "spoolway queue resume is refused too (it said: $QUEUE_OUT)"; fi
if [ "$BEFORE" = "$AFTER" ]; then ok "and the task file is unchanged"
else bad "and the task file is unchanged (now at \`$(stage_of stuck)\`)"; fi
# The unblocker reports once its turn ends. Refused, its lane log says the
# task has already left the step.
UNBLOCKER_LOG="$SPOOLWAY_PROJECT_HOME/headless/logs/stuck · blocked.log"
if await stuck gone 90 && [ -s "$UNBLOCKER_LOG" ] && ! grep -q 'already left' "$UNBLOCKER_LOG"; then
  ok "the unblocker's own report still lands"
else bad "the unblocker's own report still lands"; fi
expect_fail_end
dispatcher_stop
must "attended again" "$SPOOLWAY" config set unattended.enabled false
forget stuck

# ------------------- D-2: a terminal on the dispatcher, a job, a missing tool
task_doc "$LIVE/one.md" one "$BODY" "group: one"
must "an unrelated task" "$SPOOLWAY" queue add --from "$LIVE/one.md"
mkdir -p .spoolway/hooks .spoolway/routines/tick
# Configured fully, with a `fetch` branch and no `slug=`, so the only thing
# wrong with the hook is the tool it needs and cannot find.
cat > .spoolway/hooks/needs.sh <<'HOOK'
#!/bin/sh
# spoolway-requires: nosuchtool >= 1.0
case "$SPOOLWAY_EVENT" in
  fetch) exit 0 ;;
esac
exit 0
HOOK
chmod +x .spoolway/hooks/needs.sh
must "the issue hook" "$SPOOLWAY" config set issue_tracking.hook needs.sh
must "its project key" "$SPOOLWAY" config set issue_tracking.project_key FAULT
must "and no key in names" "$SPOOLWAY" config set issue_tracking.key_in_names false
task_doc .spoolway/routines/tick/tick.md tick "$BODY" "group: tick"
echo hang > "$CTL/tick-1"
cat > "$SPOOLWAY_PROJECT_HOME/jobs.toml" <<TOML
[jobs.tick]
schedule = "* * * * *"
pipeline = "default"
routine = "tick"
TOML
# A dispatcher on a terminal stops before it starts to list what `doctor` has
# found, and waits for a key; this fixture's stand-in models and unused prompts
# are on that list. The key is typed into `script`'s own stdin, which it
# forwards to the dispatcher's terminal — the dispatcher's own stdin is never
# redirected, so it has a terminal on both ends, as it does in a herdr pane.
TERM_LOG="$LIVE/term.log"
TERM_KEYS="$LIVE/term.keys"
rm -f "$LIVE/term.pid" "$TERM_KEYS"
mkfifo "$TERM_KEYS" || { echo "no fifo" >&2; exit 2; }
exec 8<>"$TERM_KEYS"
env SPOOLWAY_SKIP_VERSION_CHECK=1 E2E_BIN="$SPOOLWAY" E2E_PIDFILE="$LIVE/term.pid" \
  setsid script -qfaec 'stty cols 160 rows 50; echo $$ >"$E2E_PIDFILE"; exec "$E2E_BIN" dispatch' \
  "$TERM_LOG" <"$TERM_KEYS" >/dev/null 2>&1 &
disown
poll_until 10 test -s "$LIVE/term.pid" || {
  printf '  \033[31mSETUP\033[0m the dispatcher on a terminal never started\n' >&2
  exit 2
}
if poll_until 20 grep -aqF 'start the run' "$TERM_LOG"; then printf '\r' >&8; fi
TERM_PID=$(cat "$LIVE/term.pid")
# `ps` prints `?` (or `??`) for a process with no controlling terminal.
TERM_TTY=$(ps -o tty= -p "$TERM_PID" 2>/dev/null | tr -d ' ')
case "$TERM_TTY" in
  '' | '?' | '??' | '-') bad "the dispatcher's stdin is a terminal" ;;
  *) ok "the dispatcher's stdin is a terminal" ;;
esac
expect_fail D-2 cron-tool-prompt
if await one gone 60; then ok "an unrelated task moves on while the job fires"
else bad "an unrelated task moves on while the job fires"; fi
lacks "the pass never waits on a key" "queue anyway" "$TERM_LOG"
if poll_until 20 test -f "$SPOOLWAY_PROJECT_HOME/queue/tick-1.md"; then
  has "and the job's task is queued with tracking off" "tracking: off" \
    "$SPOOLWAY_PROJECT_HOME/queue/tick-1.md"
else bad "and the job's task is queued with tracking off (it was never queued)"; fi
expect_fail_end
kill -KILL "$TERM_PID" 2>/dev/null
exec 8>&-
forget tick-1

finish
