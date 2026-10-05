---
domain: dispatcher
covers: ["src/dispatch.rs", "src/mux.rs", "src/lane_alias.rs", "src/headless.rs", "src/lock.rs", "src/status/**", "src/problem_log.rs", "src/prompt.rs", "src/teardown.rs", "src/archive_index.rs", "src/runfiles.rs", "src/claim.rs"]
---

# The dispatcher

The dispatcher moves tasks through their pipelines. It uses no model. Every decision is a
lookup in a task file or in the list of live lanes.

## Running it

```
spoolway dispatch                 # runs until the queue is empty, printing a line per pass
```

`spoolway dispatch` asks herdr which pane it is running in and refuses to start outside one,
whatever flags are given:

```
$ spoolway dispatch
spoolway: Open herdr and start spoolway there:

  herdr
  spoolway
```

An internal headless backend exists for automated tests only. It is not a supported runtime
or an alternative to installing herdr. See [Testing](testing.md).

One `spoolway` serves the whole project at a time: a dispatcher started with `spoolway
dispatch`, or a screen opened with bare `spoolway`. Every pass re-reads the queue, so a task
queued while a dispatcher runs is picked up on the next pass. A second `spoolway dispatch`
while either is up prints the same line and exits without drawing a board:

```
$ spoolway dispatch
Dispatcher already running
```

Queueing a batch from the queue screen while another dispatcher holds the lock works the same
way: the batch is written, and the running dispatcher picks it up on its next pass. See
[`spoolway queue`](cli-reference.md#spoolway-queue).

Before the first pass, an [overrides layer](configuration.md#the-overrides-layer) screen, then
a warnings screen holding `spoolway doctor`'s cheap findings, each holds for a key, whenever
either has something to say. See [`spoolway dispatch`](cli-reference.md#spoolway-dispatch). Once
the run has taken the lock, a failure to find or open its own workspace gets a notice of its
own.

| Exit code | Meaning |
|---|---|
| 0 | The run dispatched and stopped on its own. |
| 3 | The queue was empty and no job is enabled. |
| 4 | Another dispatcher or screen already holds the project. |
| 1 | Any other error. |

### When it stops

The run stops when the queue is empty. A `paused` or `blocked` task keeps the run alive,
because it is waiting on a person. While any job is enabled, the run also stays up on an empty
queue and prints when the next job fires. See [Jobs](jobs.md).

`ctrl-c` stops the run the same way. A stop tears nothing down. Every worktree, pane, tab and
lane stays where it is, and the next run picks it back up. Before it exits, the run banks each
still-running lane's spend in the usage ledger and forgives that lane's launch counter, so the
next run does not treat a lane that survived the stop as a failed launch. A second `ctrl-c`
kills the process at once.

## What a pass does

A pass reads two things: each task file's stage, and the list of live lanes. Before it reads the
queue, it deletes any half-written task file in `queue/` whose writer has stopped running.

```mermaid
flowchart TD
  A[Fire any cron job due this minute] --> B[Reconcile every task]
  B --> B1[A lane settled: read its report, move the stage]
  B --> B2[A queued task whose dependencies are done: mark it ready]
  B --> B3[A silent lane: remind or escalate]
  B1 & B2 & B3 --> C[Sort the ready tasks]
  C --> D[Start lanes while slots are free]
  D --> E[Fire issue-tracking hooks for tasks that arrived at queued, blocked, paused or done, and started for a queued task about to launch]
  E --> F{Draw the board. Did this pass move a task?}
  F -->|yes| A
  F -->|no| G[Wait for the next pass]
  G --> A
```

A key also answers inside a pass, not only during the wait between passes: the dispatcher checks
stdin between each task, during and between each lane start, the sweep and the archive, so a busy
run reads the keyboard at the same rate as an idle one.

Between two passes the wait is not empty. The dispatcher slices the wait into one-second
stretches and draws a frame at the top of each one, so the board redraws every second on every
target. On Linux, the dispatcher also watches the commands directory for the whole wait,
alongside the keystroke it already listens for. A file landing there, such as a background
command step finishing, ends the wait immediately and starts the next pass, which is what routes
that step. A pass also draws on the same once-a-second cadence between the checkpoints in its
own work, so the board keeps redrawing through a slow pass too.

A model's `slots` caps lanes on that model, and a profile's `concurrency` caps lanes on that
profile. A model marked `exclusive` never runs beside a different exclusive model. See
[`[models."<glob>"]`](configuration.md#modelsglob--what-a-model-costs-and-how-big-its-window-is).

A task with a `depends_on` is cut from its first dependency's branch. A task without one is cut
from `base:`. A `starts_from:` set in the task file wins over both. When the branch to cut from
exists nowhere, the task pauses instead, as
[A start branch that does not exist](tasks.md#a-start-branch-that-does-not-exist) describes. A task file that does not parse is skipped and named under the board.

## What runs next

When more tasks are ready than there are free slots, the pass sorts them:

1. Fewest steps left on its pipeline first.
2. Then the group with the least work left.
3. Then the task the most other tasks wait on.

With `dispatch.priority = "group"`, the default, a task from a group that has not started yet
ranks behind every task from a group that is already running. It still takes a slot when no
running group has ready work. With `dispatch.priority = "any"` only the three rules apply.

A task that reaches a `serial: true` command step while another task's run of the same step and
pipeline is going waits there unstarted, out of the sort above. Its run starts on the first pass
after that run exits, alongside whichever other ready task the sort picks first.

## What a lane is sent

A lane starts with a system prompt and one typed message. The system prompt holds the step's
prompt file and the report contract: the exact `spoolway report` calls and what they do. It is
written to `~/.spoolway/<project>/system-prompts/<lane>.md`. The typed message is the task
file's path. `spoolway prompt contract` prints the system prompt for a sample task.

## Reading the state

The dispatcher runs a pass every ten seconds. That rate is not configurable, and stays the floor
under how long a quiet run can go without a pass: a change in the queue or commands directory
wakes the next pass sooner, as described above. A pass that moves a task to a new stage, frees a
lane or archives a task runs the next pass at once instead of waiting for the next one. A long
run of such passes in a row eventually waits anyway.

<img src="screenshots/dispatch.png" alt="the dispatcher board">

The header above the task rows names the running dispatcher's version, next to its pid. If a
`spoolway` executable on `PATH` reports a newer version, the header adds `(restart to use latest
installed version)`.

Bare `spoolway`'s dispatch tab draws the same board, under the tab strip, from a `spoolway
dispatch` child the tab starts on `enter` and stops behind a popup the next `enter` opens — the
tab runs no pass itself. Its header names the pid of that child, or reads `dispatcher stopped`
with no pid once it has stopped, next to a count of the steps still working: `dispatcher stopped
· 3 steps finishing` (`1 step finishing` for one), left out once none are. The wordmark's spool
turns while that count is above zero. On `enter`, before the child's first pass has claimed
anything, the tab covers the board with a keyless `Starting dispatcher` popup. See
[`spoolway`](cli-reference.md#spoolway).

`enter` over a running dispatcher always opens the stop popup, even with nothing running:

```
┌─ stop dispatching ──────────────────────────────────────────────────┐
│                                                                     │
│  No new steps will be started.                                      │
│  Interrupting stops agents and commands. When resumed, agents pick  │
│  up where they left off and commands restart.                       │
│                                                                     │
│  [enter] let running steps finish                                   │
│  [i] interrupt them now   [esc] back                                │
└─────────────────────────────────────────────────────────────────────┘
```

`enter` there stops the child the way `ctrl-c` does: nothing is interrupted, and every lane
keeps running. `i` interrupts every live agent turn and kills every running command step first,
parking each of those tasks on `paused` with a mark saying the stop parked it, then stops the
child. `esc` leaves the dispatcher running. A second `enter` while a stop is already going does
nothing: there is nothing left to ask.

Starting dispatching again, from the tab or with `spoolway dispatch`, resumes every task the
stop parked, back onto the step it was on, before its first pass.

Rows are grouped by `group:`. A `▌<group>` line opens each block, and a total line closes it.
The total is the group's banked spend: every step that has settled, across every task in the
group, added up as soon as each one banks. It does not include a running step's unbanked
figures, so it can read lower than the row above it. A group with nothing banked yet closes
with no figures at all.
When the group has an issue behind it, the group name is a link to that issue. A group whose
first task stacks onto another group's own last task reads `▌<group>  after <group>`. See
[Stacking one group on another](tasks.md#stacking-one-group-on-another).

### Columns

| Column | What it shows |
|---|---|
| TASK | The task id. |
| PIPELINE | The pipeline the task runs on. |
| STEP | The current step. For a starting task, the step the dispatcher is booting a lane for, ahead of the task file's own stage. `↻ <n>` is how many times the task has arrived at that step, whichever route carried it there. It appears from the second arrival on and always draws dim. |
| STATE | One of the states below. |
| CTX | How full the lane's context window is, as a percentage of the model's `context_window`. |
| OUT | Output tokens this step has produced. |
| COST | What this step has cost. |
| TIME | How long the lane's pane has been busy on this step. A paused or blocked row's TIME does not grow. |
| NEXT | For a running or starting task, the step it goes to on pass. For one with a scheduled pause, `→ paused after <step>`. For a queued task, what it waits on. For a paused task, the outcome the pause caught and where a resume sends it, key first: `[r] review failed → e2e — \`spoolway resume <task>\``; a caught pass reads `[r] → e2e — \`spoolway resume <task>\``. A task parked before it ever started reads `→ queued — [r] resumes it`. A task the dispatch tab's stop popup parked reads `→ <step> — resumes when dispatching starts`. A blocked task waiting for a person reads `[r] → <step> — \`spoolway resume <task>\``, naming the step `spoolway resume` sends it back to. While a dependency or the task's own lane is busy, the row reads `→ <step>` with no key. For a lane holding a permission prompt, `press a key in pane \`<task> · <step>\``. |

`spoolway eval --by task` gives the task's whole bill.

### States

```mermaid
stateDiagram-v2
  [*] --> queued
  [*] --> unknown: stage: names a step the pipeline does not have
  queued --> starting: dependencies done, slot free
  starting --> running: the lane comes up
  starting --> queued: the boot fails
  running --> starting: step passes or fails, the next lane starts
  running --> prompt: a permission prompt in its pane
  prompt --> running: the prompt is answered
  running --> paused: gate, p on the board, or the stop popup's i
  running --> blocked: a step reports a block or a budget runs out
  paused --> running: spoolway resume, or a p/Escape park's lane working again
  blocked --> running: spoolway resume
  running --> done: last step passes
  done --> [*]
```

| State | Meaning |
|---|---|
| `queued` | Waiting for its dependencies and a free slot. |
| `starting` | The dispatcher has claimed a slot and is booting the lane. herdr does not list it until the boot is well along. The logo turns the same as it does for `running`. |
| `running` | A lane is working the current step. |
| `prompt` | A live lane's pane is holding a permission prompt. Read fresh off the lane list every redraw, and gone the instant the prompt is answered. Not resumable: the task has not stopped. |
| `paused` | The task's own stage is `paused`: a gate, or a park from `p` or the dispatch tab's stop. |
| `blocked` | A step reported a block, a launch failed, or a loop budget ran out. Read `## Blocker` in the task file. |
| `unknown` | The task file's `stage:` names a step the task's pipeline does not have. Nothing on the row can be resumed. Correct `stage:` in the task file. |
| `done` | Finished and archived. The row stays, dimmed, until the whole group is done. |
| `finished` | Only on a stopped dispatch tab: a step whose lane has settled, or whose command run has exited, with nothing up to move it on. TIME stops where the board first saw it settle. NEXT reads `moves on when dispatching starts`. The same step reads `running` while a dispatcher is up, since it moves on within the pass that settles it. |

`RECENT` lists the last task moves. Errors from a pass go to `~/.spoolway/logs/<project>.log`.

### Keys

Lowercase acts on the row under the `▸` cursor. Uppercase acts on the whole run.

| Key | What it does |
|---|---|
| `↑` `↓` | Move the cursor. It starts on the first row of the first group and walks every row the board draws, done ones included. If its row leaves the board, such as its group finishing, the cursor falls back to the first row with no key pressed. |
| `o` | Open the task file in `$VISUAL`, else `$EDITOR`, in a new pane. Works on a `done` row too. |
| `r` | Resume a paused or blocked row whose dependencies are done. A row parked by `p` or Escape is live even while its own lane reads `Working` or `Blocked`: it puts the task back on its step and leaves that lane running. A row parked before it ever started resumes straight back to `queued`, whatever its dependencies read. Also restarts a row on a step with nothing running behind it. Otherwise the same as `spoolway resume <task>`. |
| `R` | Resume every paused task. Asks first if any of them is at a real gate. |
| `p` | Pause the row, including a `blocked` one. Asks first if it would interrupt a running agent turn or command. |
| `s` | On an open pause panel, schedule the pause instead of carrying it out. |
| `u` | Take a `queued` task, and every unstarted task that depends on it, out of the queue and write their tasks back to `~/.spoolway/<project>/pending/`. Asks first. |
| `U` | Do the same for every task that has not started. Asks first. |
| `ctrl-c` | Stop the run. |

Pausing an agent turn sends Escape to the pane, so a resume picks the session back up. Pausing
a command step kills the run, and the command runs again in full on resume.

`s` on a pause panel interrupts nothing. It writes a `gate_at` for the step the panel named, so
the named task pauses itself once that step reports, whatever it reports. Press `s` again on a
row that already has a scheduled pause to clear it.

### Footer

One line per agent profile: `<profile>   slots <live>/<cap>`. A model with its own `slots` gets
its own figure appended after the profile's, model name then `<live>/<cap>`, so a profile
running a pooled model reads `pi   slots 2/3   Ornith-1.5-35B-A3B   1/2`. A line
`issue_tracking: N hook failures — see tracking/` appears while any hook
has failed. Then the job ledger lists every enabled job with its next firing:

```
jobs    2 active
        ○ nightly-audit       Sat 12 Sep 03:00   (in 6h 48m)
        ○ release-readiness   Mon 14 Sep 08:00   (in 2d 12h)
```

`spoolway queue list` prints the same table from another terminal, and `spoolway lane <lane>`
shows what one lane is doing.

## Gates: when the pipeline waits for you

A step with `gate: true` stops the task on `paused` after the step reports. The pane stays open
for you to read.

```
spoolway resume deploy-login                            # let it past
spoolway resume deploy-login --stage implement -m "not tonight"  # send it back round
```

A paused task holds no slot. You can type into its pane. When that turn ends, the dispatcher
commits the worktree with a `## Status Log` line saying a person drove the round.

Other things put a task on `paused`:

| Cause | How the task file records it |
|---|---|
| `p` on the board | `parked_from: <step>` |
| The dispatch tab's stop popup, `i` | `parked_from: <step>` and `parked_by_stop: true` |
| Escape typed by hand into a lane's pane | `parked_from: <step>`, written on the next pass |
| A staffed `blocked` lane reports `--pause`, `--fail` or `--block` | `paused_at: <the step it blocked on>` |
| A failing `[issue_tracking]` hook on `queued`, `started` or `done` | `hook_paused: queued`, `hook_paused: started` or `hook_paused: done` |
| A queued task whose start branch exists nowhere | `missing_start_branch: <branch>` |

`spoolway resume` on any of these puts the task back on its step, and so does `r` on the board.
A task the stop popup parked also resumes on its own, back onto the step it was on, the next
time dispatching starts — from the tab or from `spoolway dispatch` — and its NEXT column reads
`→ <step> — resumes when dispatching starts` until then. A paused task's pane survives a stop of
the dispatcher.

A `p` or Escape park also goes back onto its step without a key press, once its own lane is seen
`Working` again — a person typed a follow-up straight into the pane instead of pressing `r`. The
next pass restores it straight away; nothing is relaunched, since the lane never left. A report
that lane sends while the task is still parked is applied the same way, as if the task were
already back on its step. The stop popup's own park does not auto-restore like this: it waits for
dispatching to start, as above.

A hook pause is the one exception: nothing inside the pipeline failed, so there is no step to
go back to. `spoolway resume` forgets the hook's failed run, so it fires again. A task paused on
`queued` or `started` resumes to `queued`. A task paused on `started` never actually left
`queued`. A task paused on `done` resumes straight back to `done`.

## A lane that settles without reporting

A lane can end its turn without calling `spoolway report`. Then:

1. The next pass sends the report contract into the pane again.
2. Each later pass where the transcript has grown sends another reminder, up to three.
3. A lane whose transcript has not grown since the last reminder is blocked, with the last of
   what the pane said in the task's `## Blocker`. `spoolway resume` restarts the step on the
   same session.
4. A fourth due reminder blocks the task instead.

A lane that still holds a process it started, such as a long build, is excused from reminders
until `dispatch.lane_child_ceiling` (one hour by default), then escalated.

A lane whose multiplexer reports it `blocked` is on a permission prompt. The board reads this
live off the lane list every redraw: the row draws `● prompt` with the pane named in NEXT, and
nothing is sent to it. The task's own stage does not move. Answer the prompt and the row reads
`running` again on the next redraw.

## A step that carries its own session

```yaml
- id: implement
  session: true       # reuse the newest session this prompt held on this task
- id: document
  session: false      # the default: every visit opens fresh
```

The step carries the session on if two bounds hold:

| Bound | Setting | Checked against |
|---|---|---|
| Size | `agents.<profile>.session_reuse_ctx`, a percentage | The input, cache-read and cache-write tokens of the session's last turn, over the model's `context_window`. |
| Age | `models.<glob>.session_reuse_idle` | The session store's modification time. |

Otherwise the step opens a fresh session, and `RECENT` says why. The lookup goes by prompt,
so two steps running the same prompt share one conversation. A blocked task's resume is
separate from this. See [When a task needs a person](tasks.md#when-a-task-needs-a-person).

## Restarts, laps and escalation

| Limit | What it bounds | Reset by |
|---|---|---|
| Launch guard | A lane that dies at launch and leaves no session blocks the task. In an unattended run it is retried on a doubling delay, capped at one hour. | A pass that sees the lane; every stage transition; a dispatcher stop. |
| Launch-failure ceiling | A launch that cannot start at all, such as a refused tab or an unconfigured model, is retried twice. The third failure in a row routes the task to the step's `on_fail`, or `blocked`. | A launch that starts; arriving at the step again; re-queueing the task. |
| Pane-busy wait | A pane that has not reached its shell prompt refuses `agent start`. The task waits. After ten minutes it routes the way the launch-failure ceiling does. | A launch that starts; arriving at the step again; re-queueing the task. |
| A step's `loop:` | How many times a task may arrive at the step, by any route. | Never. A person's resume counts too, and refunds nothing. |
| Reminder loop | Three reminders to a silent lane. | Anything the lane writes to its transcript. |
| Live-child ceiling | How long a lane may hold a child process before it is escalated. | The process exiting. |

When a loop budget runs out, the task parks on `blocked`.

## Escalation

An escalation puts the task on `blocked`. In an attended run the board marks the row amber and
names the pane in NEXT, and the task waits for `spoolway resume`. In an
[unattended run](pipelines.md#unattended-runs) a lane starts on the `blocked` step instead, using
the `[unattended]` `blocked_*` settings.

A staffed `blocked` lane answers with `--pass` when it cleared the way, or `--pause` when it
cannot. A `--fail` or `--block` from it is read as `--pause`. A `--pass` carries the task past
the blocked step to that step's `on_pass`, or back to itself for a command step. `--pass --stage
<step>` sends it to `<step>` instead, bounded by the steps this task has already run. A
`--pause`, `--fail` or `--block` puts the task on `paused`, and `spoolway resume` then hands it
back to the step it blocked on.

A blocked task keeps its pane open until it is resumed.

## Where a lane lives

```toml
[dispatch]
herdr_mode = "split"  # or "grouped"
```

| Mode | Where a lane runs |
|---|---|
| `grouped` | One tab per project in the shared `spoolway-dispatcher` workspace. One pane per running task. |
| `split` | One herdr workspace per task, nested under the project's row as `spoolway/<task>`. |

Under a multiplexer every lane is a real pane you can watch and type into. A task holds one
pane for its whole life. See [Vacating a pane](#vacating-a-pane). A lane is named
`<task> · <step>`. Sessions you start by hand are never touched.

### One home for every run, in every project

`spoolway-dispatcher` is one herdr workspace shared by every project on the
machine. It opens on `~/.spoolway/.dispatcher/`, which is not a repository. Every project's home
is named `<label>-<id>`, so no project can take the name `.dispatcher`. The board itself stays
in the pane you ran `spoolway dispatch` in.

### herdr

Under `grouped`, the project's tab closes when its last task is archived. The shared workspace
is only ever closed by hand. Under `split`, each task's workspace is bound to its worktree with
`herdr worktree open`.

A task gets one tab, whichever step it is on. Every pane after the first splits inside that same
tab. Each split halves the smallest pane in the tab along its longer side, so a tab grows as a
spiral. A tab left holding nothing is closed on the next pass, unless it is its workspace's only
tab.

Under `split`, a task's tab is renamed to the task's slug, and stays that way across a
dispatcher restart. Its panes then show only the step. Under `grouped`, several tasks share one
tab, so its panes show `<task> · <step>`, the same as the lane's own name.

### Vacating a pane

When a step finishes, the dispatcher asks its session to leave the pane, so the next step can
start in the same pane. Only `claude` has a quit gesture. See [Leaving a pane without closing
it](agents.md#leaving-a-pane-without-closing-it).

The next step waits up to two minutes for the earlier lane to leave. After that it splits its
own pane and the old one is closed. The pane goes blank during the transition.

## Looking into a lane

```
spoolway lane                       # the lanes there are
spoolway lane "login · implement"   # that lane's output
spoolway lane "login · implement" -n 500
```

Quote the lane name. It holds a space.

## Where work happens on disk

Each task gets a git worktree under the project's own home, at
`~/.spoolway/<project>/worktrees/task-<id>`, or, for a home-mode checkout, its
workspace's `dispatchers/<dispatcher>/worktrees/task-<id>`. See [Home
mode](concepts.md#home-mode). If somebody already has the task's branch checked out, the lane
borrows that checkout and cleanup leaves it alone. See
[Whose worktree](pipelines.md#whose-worktree).

Each worktree builds into its own `target/` directory. Lanes never share a build directory.

When a task reaches `done`:

1. Leftover work is committed as `wip(<task>): <step>`. If that fails, the task is held on
   `blocked`.
2. The worktree is removed, unless it was borrowed.
3. The branch is deleted, unless a queued task still depends on it or it has commits no remote
   has.
4. The task file moves to `archive/`, and its run files and session homes are deleted.
5. One line for the task is appended to `archive/index.jsonl`.

### The archive index

`archive/index.jsonl` holds one JSON line for each archived task. The `<id>.md` files in
`archive/` stay as they are, and the index is a copy that spoolway can always rebuild from them.

| Field | Holds |
|---|---|
| `id` | The task id. |
| `group` | The task's group. |
| `title` | The task's title. |
| `pipeline` | The pipeline the task ran. |
| `branch` | The task's branch. |
| `depends_on` | The ids the task depended on. |
| `worktree_path` | The task's worktree. Left out when the task had none. |
| `base` | The branch the task's group lands on. Left out when the task has none. |
| `trial` | `true` when the task was a trial arm. Left out otherwise. |
| `slug` | The task's issue slug. Left out when the task has none. |
| `url` | The task's issue URL. Left out when the task has none. |
| `created_ns` | When the task file was created, in nanoseconds. It is the file's modification time on a filesystem that keeps no creation time. |
| `archived_at` | When the task was archived, in Unix seconds. |

Four readers list archived tasks from the index:

| Reader | Uses it for |
|---|---|
| The board | The done rows and the issue link on a group band. |
| The queue tab | The archived groups, ordered by `created_ns`. |
| `spoolway queue add` and `spoolway task contract` | The check that a `depends_on` names a queued or archived task. |
| `spoolway eval` | The worktree path of each archived task. |

None of them opens a task file while the index matches the folder. The queue tab opens one
archived file by name when a key needs that task's text. The index matches when its modification
time equals the modification time of `archive/`. Each write to the index sets its time to the
folder's.

Archiving a task adds its line to a screen's cache without reading any other archived file.

The index is rebuilt from the `<id>.md` files when it is missing, when a line does not parse, or
when the folder has changed since the index was written. A file that does not parse is left out.
A rebuilt line uses the file's modification time as `archived_at` and the file's creation time as `created_ns`.

The retention sweep removes the line of every archive file it deletes. It never deletes
`index.jsonl` itself. It only deletes archive files when `archive_retention_days` is set. See
[Configuration](configuration.md).

Archiving a task, the retention sweep, a trial settling and `spoolway eval --discard` each take
the lock at `<home>/archive-index.lock` before they change `archive/`. They wait up to ten minutes
for it. A reader of the index waits three seconds, then reads the task files without writing the
index.

### Trial arms

A trial forks a group into one full copy per ticked pipeline, each copy in a group of its own,
every task in every copy an arm under one shared `trial:` id. See [Trials](planning.md#trials).
An arm is a disposable copy:

- Its `queued`, `started` and `done` events never fire the `[issue_tracking]` hook.
- `spoolway stack` is a no-op for it, so it opens no pull request.
- Its branch is deleted at cleanup whether or not a remote has it.
- It carries `trial_group:`, the source group a person tried. Every ledger line it banks stores
  that source group beside `trial:`. The field is absent on an ordinary task's lines.

When the last arm reaches `done`, every arm's copy is removed. The source group and the usage
rows stay:

```
trial t9f3a settled

  kept      source group board-step-grace-window
  kept      usage rows for 6 trial tasks
  removed   6 tasks, worktrees and local branches
  removed   6 panes, scratch dirs, sessions and run-file sets

  read      spoolway eval --by task --trial t9f3a
```

`spoolway eval --discard <id>` removes a trial before it settles. It refuses while an arm is
mid-turn unless you pass `--force`.
