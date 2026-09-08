---
domain: dispatcher
covers: ["src/dispatch.rs", "src/mux.rs", "src/tmux.rs", "src/headless.rs", "src/lock.rs", "src/status/**", "src/problem_log.rs", "src/prompt.rs", "src/teardown.rs", "src/runfiles.rs"]
---

# The dispatcher

The dispatcher is the loop that moves tasks through their pipelines. It is the only part of
spoolway that decides anything at runtime — and it decides everything by lookup, with no
model involved.

## Running it

```
spoolway dispatch                 # runs until the queue is empty
spoolway dispatch --dry-run       # report what one pass would do, and do none of it
spoolway dispatch --interval 5m   # override the configured interval between passes
spoolway dispatch --force         # start anyway, past the restart guard
```

Run it in its own pane so it outlives the session that started it.

**One dispatcher serves the project**, whichever worktree it was started in. Starting a
second one is not the way to get newly queued work moving: each pass re-reads the queue, so
a task queued from another group's worktree while it runs is picked up on the next pass
without a restart. Running `spoolway dispatch` while one already holds the lock draws the
same read-only board described below, headed `watching dispatcher` instead of `dispatcher
running`, with that other process's pid rather than its own. `ctrl-c` ends the watcher and
nothing else — it never takes the lock, starts a lane, or touches a task file. `spoolway
dispatch --plain` in that situation prints the plain table once and exits, for scripts.

### When it stops

It stops when the queue is empty — which is to say when every task has reached a terminal
step and been archived, including anything queued while it was running.

A blocked or paused task stays in the queue, and so does the dispatcher: those are waiting on
a person, not finished. With nothing queued at all it says so rather than looping over an
empty queue.

An enabled job is the one exception. While any job is enabled, the run stays resident on an
empty queue — on the pass that drains the last task, and on a cold start with nothing
queued, where it no longer exits 3. It prints why it is staying and when the next job fires,
then loops on into the wait so it is alive when that window comes round. `ctrl-c` still stops
it, and `dispatch.tear_lanes_on_stop` still applies. See [Jobs](jobs.md).

### Restarting into a repo that cannot run

A supervisor, a shell loop, or a person holding a key can restart `spoolway dispatch` forever
against a repo it cannot run in. Four starts in a row that could not run at all, inside a
30-second window, get the fifth refused, naming the count, the last reason and the way past
it:

```
$ spoolway dispatch
spoolway: refusing to start: 4 starts in a row could not run at all, the last
because a dispatcher is already running for this repo (pid 2688669). Fix the
reason above, or run `spoolway dispatch --force` to start anyway.
```

The count lives in `dispatch.restarts`, beside `dispatch.pid` under the project's own home
directory. Only a start that could not run at all counts towards it — today, that is a start
finding another dispatcher already holding the lock. An empty queue is never counted: a repo
with nothing to do is not a storm, and restarting into one forever is a caller's own choice to
make. A start that actually runs clears the count, and so does `--force`, which starts the run
anyway and lets the next restart storm on this repo begin its own count from zero. The window
is judged from the most recent refusal rather than the first one in the run, so a caller
restarting every few seconds, forever, cannot outrun it by aging the window out from under
itself. A counter file that is missing, short or otherwise corrupt reads as no storm standing,
rather than refusing every start that follows it.

The guard rate-limits a storm rather than stopping it outright: once four refusals stand, the
count is never added to again until a fifth start actually arrives, so the refusal itself lapses
roughly 30 seconds after the fourth and a caller that keeps retrying gets another four starts
before the next refusal.

`spoolway dispatch` exits 0 on a run that dispatched and stopped on its own, 3 on an empty
queue with no job enabled, 4 when another dispatcher already holds the lock, 5 when the
restart guard refuses a start, and 1 on any other error — see [`spoolway
dispatch`](cli-reference.md#spoolway-dispatch). With a job enabled the run stays resident
instead of exiting 3.

## What a pass does

Every pass reconciles the pipeline from two sources of truth and nothing else: **each task
file's stage**, and **the live lane list**. It remembers nothing between passes, which is
what makes it safe to interrupt at any point.

Before either of the steps below, a pass fires any cron job whose expression matches the
current local minute, so the routine it queues is dispatched by this same pass. A dry run
fires nothing. The dispatcher only ever fires jobs; it never writes one. The `spoolway jobs`
screen is the only writer. See [Jobs](jobs.md).

In outline, a pass:

1. Reconciles every task: a lane that has settled, a task whose stage moved, a command step's
   question, a queued task whose dependencies have all finished.
2. Starts lanes for whatever is ready, up to each candidate's cap — a resolved model's own
   `slots` when it has any, its profile's `concurrency` otherwise (see
   [`[models."<glob>"]`](configuration.md#modelsglob--what-a-model-costs-and-how-big-its-window-is))
   — and refuses a candidate whose model is `exclusive` while a live lane runs a different
   `exclusive` model.

A model's `slots` and its `exclusive` are counted for every step naming that model, including
one carrying `slot: false`: that key buys a step out of its *profile's* `concurrency`, which is
a number about a harness, and not out of a number about a machine. Every live lane is counted,
whatever the multiplexer says it is doing this second — a lane spends its first seconds `idle`,
before its agent has taken a turn, and it is holding the model's weights the whole time.

A pass fires a project's `[issue_tracking]` hook, if one is configured, on a task's arrival at
`queued`, `blocked`, `paused` or `done` — the four states reserved in `src/pipeline.rs`, which
is why no pipeline's own `run:` step can put work on any of them. See
[`[issue_tracking]`](configuration.md#issue_tracking--a-hook-fired-on-four-task-events) for the
table, the environment a hook runs with, and what `on_fail = "pause"` holds. That table blank
is what "no issue tracking" means: a blank `hook` runs nothing here, and a pass behaves exactly
as it does with the table absent.

A pass is safe to run beside a lane. A lane's `spoolway report` writes the same task file the
pass is working from, out of another process. Both sides take a short per-task lock around
their own read and write, so the two cannot interleave. If a report lands after the pass has
already read that task, the pass drops its now-stale write of that one file. The next pass
redoes the bookkeeping against what the lane actually wrote.

A queue file that will not parse does not stop a pass. The bad file is skipped, every other
task runs as normal, and the file is named twice: once in `~/.spoolway/logs/<project>.log`,
and again in amber under the board's table, so a person sees which file to fix.

Step one re-examines a task's stage right away whenever it moves it without starting a lane —
a step named in the task's own `skip:` (see [Trials](planning.md#trials), the one thing that
writes it) falls through to `on_pass` on the spot rather than waiting for the next pass. That
can chain:
`skip: [document, handover]` walks both in the one pass that first reaches the tail, bounded by
the pipeline's own step count so a fall-through cycle in a pipeline's wiring cannot spin the
pass forever. Nothing in a pipeline file can make a step fall through: every step a pipeline
lists runs for every task on it.

There used to be a step before those two, fast-forwarding each base branch the queue names so
a task would not be cut from a base missing its own dependency. It is gone with the model that
needed it: a task with a `depends_on` is cut straight from its first dependency's branch, not
from `base:`, so there is no base branch left to fall behind. A task with no dependency is
still cut from `base:` exactly as before. Either way `base:` keeps recording the branch the
group lands in, and there is no rebase left for `handover` to run.

## What runs next

When more work is ready than there are slots, three rules decide, in order:

1. **What is nearly done, first.** Fewer steps left in the pipeline outranks more, so a task
   one step from handover finishes before a fresh one starts — whatever pipeline either is on.
   Steps left rather than the step's raw index, because pipelines in this project are not all
   the same length, and a task's step 7 on a 17-step pipeline is not a task's step 7 on a
   7-step one.
2. **The group with least left to do, first.** A group is only ready to land once every one of
   its tasks has a green pull request, so spreading effort across groups finishes none of them.
3. **Whatever unblocks the most, first.** Within a group, the task other tasks are waiting on.

Rule one comes from the order of steps in the pipeline file, and from nothing else.

**The group gate runs first, ahead of all three rules**, and only under `dispatch.priority =
"group"` — the shipped default. It ranks a candidate behind one from a still-open group,
rather than dropping it from the pass, whenever its own group has never run and some other
group that has already produced work is still able to move: a pull request only lands once
every task of its group has landed, so opening a second one while the first can still finish
spreads effort across two groups instead of landing either. A group counts as already running
the moment any one of its tasks has left `queued` — or, once a chain's earlier tasks have all
been archived and only its last one is still queued, the moment that task's `depends_on`
names one of them.

Because the gate only ranks, a slot never idles for it: a candidate from a never-run group
loses ties against one from a group still landing, but it stays in the pass and takes the
slot the moment no open group has ready work of its own to offer instead. A task with no
`group:` is exempt from the gate altogether, in both directions: it has no siblings holding
up a pull request, so there is nothing for the gate to hold it back for, and it never counts
as an "open group" a candidate elsewhere is ranked behind either.

Under `dispatch.priority = "any"` the gate never runs at all: every ready candidate is sorted
and started exactly as the three rules above decide, whichever groups they belong to.

## What a lane is sent

A lane gets two things when it starts: a system prompt, composed per launch and written to
`<lane>.md` under the project's own home (`~/.spoolway/<project>/system-prompts/`), and one
message typed into its pane.

The system prompt carries everything spoolway knows that a prompt cannot — the framing, the
prompt itself, this pass's policy — and ends with the report contract: the exact `spoolway
report` invocations, what `--handoff` does, and that the pipeline decides what happens next.
It is what a model reads as *the rules* rather than as *a request*, so this is where the
contract has to hold even when a project has replaced every shipped prompt with its own.
`spoolway prompt contract` prints it in full, for a sample task or a real one.

The typed message is two paragraphs and nothing else: the task file's path, with the
instruction to read only that file first, and a one-line pointer back to the system prompt for
how the turn ends. It used to carry the whole report contract itself; that moved into the
system prompt, and what is left is only what a lane must act on before anything else this
turn.

## Reading the state

A dispatch run watches itself. A pass takes seconds and the loop waits its configured
`interval` between them — ten seconds by default — and it spends that time drawing the board
in the terminal it was started in.

```
spoolway dispatch
```

```
dispatcher running · pid 48213 · up 6m 49s

   TASK       PIPELINE   STEP           STATE              CTX   OUT    COST      TIME   NEXT

 ▌auth
 ▸ login      default    deploy         ● waiting on you     —   18k   $2.14    4m 00s   answer it in pane `login · deploy`
   profile    bugfix     queued         ● unreachable        —     —       —         —   unreachable — login is blocked
   signup     default    done           ● done               —     —       —         —
                                                                 49k   $2.14   31m 02s

 ▌dispatcher-ui
   billing    ui         review (2/2)   ● running          48%   31k   $0.86   14m 08s   → handover
   sessions   default    queued         ○ queued             —     —       —         —   waiting on: login
                                                                 31k   $0.86   14m 08s

RECENT
09:41  billing      review    ✓ pass   2/6
09:38  login        deploy    ✓ pass   4/4

──────────────────────────────────────────
cloud   slots 1/5
local   slots 0/2

[↑↓] row   [o] open   [r/R] resume / all   [p/P] pause / all   [u/U] unqueue / all
```

`spoolway dispatch` run while another process already holds the lock draws this same board,
reading fresh from the task files and from `Mux::list_lanes` exactly as the driving board
does, but with the header replaced: `watching dispatcher · pid 48213 · up 6m 49s` names the
lock's actual holder, never the watcher's own pid. If that process dies while the watcher is
still open, the very next redraw drops to `no dispatcher is running` — with whatever lanes
it left running still on the board, which is what makes an abandoned run visible.

A band line opens each group's block, at the row's own left margin and outdented past the
cursor gutter so it reads as a heading: `▌<group>`, the group's name and nothing else. Where
the group has an issue behind it, that name is also a link to it: the board wraps the name — the
`▌` and the dim styling stay outside — in an OSC 8 terminal hyperlink pointing at the issue URL
any one of the group's tasks carries, so clicking it opens the issue in your browser. The link
is written only where the board paints colour, so the colourless renderings — `dispatch
--plain` and `spoolway queue list` — carry the band's old bytes unchanged, as does a group
with no issue behind it. A task
queued outside any group falls into `no group` — sorted last, fixed
rather than alphabetical — whose block gets no band and no closing total line, since there is
no group name for either to carry. Inside a group, rows sort by run order: a done row sorts
last, then the rest order by how deep each task sits in the run, then by how many steps are
left on the row's own pipeline, then by how many other tasks it is holding up (most first),
then by task id — a task's state no longer moves its row. Whatever waits on you keeps its
amber marking, but not a place at the top. The `RECENT` ticker stays one list for the whole
board: it is a record of the run, not of any one group, and it carries task movements only —
nothing a pass went wrong on ever reaches it. Whatever a pass hit instead is appended to
`~/.spoolway/logs/<project>.log`, one line per problem, with the whole error chain intact and
no rewording. Each line is an RFC 3339 local timestamp, two spaces, then the message. Opening
the log at the start of a run trims it to the last thirty days — every line whose own
timestamp is older than that is dropped, and everything else is left byte for byte, including
a line whose timestamp cannot be parsed. A task declared `parallel: true` carries `∥` after
its id in the TASK
column — the same mark `queue conflicts` reasons an overlap from — so a reader sees which
tasks of a group are meant to run beside each other rather than in sequence.

Between TASK and STEP, PIPELINE names the pipeline the task actually resolves to — the
project's configured default for a task with no `pipeline:` of its own, dimmed the same way on
every row rather than only where it disagrees with the default, since the dim is what separates
the column from its plain-text neighbours. `spoolway queue list` carries it too, undimmed, since
this is the one column that is not one of the spend figures `--plain` leaves out.

A driving board — not a watching one, and not a `--plain` run — carries a cursor, marked `▸`
in a two-column gutter of its own between the row's left margin and its TASK cell. That
gutter is drawn on every row, cursor or none, and on the header and each group's total line
too, so no column ever moves when the cursor does. `↑` and `↓` move the cursor row by row,
wrapping at either end, and it tracks a task by id rather than by row position: a row that
moves because the board resorted does not strand the cursor on a different task. The board's
keys split by case: lowercase acts on the row the cursor sits on, uppercase acts on the whole
run. `r` resumes the cursor's row, through the same `spoolway resume` the command line runs,
on a paused or blocked row whose every dependency is already satisfied and whose own lane is
not still mid-turn; on any other row it does nothing. A paused row's NEXT column says so
directly, ending `[r] resumes it` rather than naming the command — and, where the key is not
live, naming `spoolway resume <task>` instead; a blocked one does not carry the hint in its
NEXT text at all, so its resumability is only ever the key-hint line's to say.

`o` opens the cursor's queued task file in an editor — `$VISUAL`, else `$EDITOR`, else the
platform's own default, `vi` everywhere but native Windows, where it is `notepad` — in a pane
the multiplexer opens for it, labelled `<task> · edit`. It never blocks: the editor runs in its
own pane, and the board keeps redrawing and the dispatcher's pass loop keeps running while it
is open. Headless has no pane to open one in and refuses the same way
`open_tab` already does; the refusal is swallowed silently, and nothing about the run or the
board changes. The key-hint line under the footer,
`[↑↓] row   [o] open   [r/R] resume / all   [p/P] pause / all   [u/U] unqueue / all`, draws on
every frame of a driving board, whether or not any row can use any of it this frame.

`p` and `P` both open a panel first wherever what they are about to do is not free to undo —
interrupting a live turn, or killing a command step's run outright.

`p` interrupts the cursor's own task, if this run owns a live agent lane for it, and parks it
on `paused`, `parked_from` naming the step it was on. Under a multiplexer the interrupt is a
keystroke — Escape, sent the way `enter` sends a prompt — which leaves the session itself
intact for a later resume to find exactly as it left it; headless has no keyboard to reach a
running turn with, so there the lane's process is stopped outright, the same as `stop_lane`.
If the cursor's own task is itself on a running command step, `p` also opens a panel naming it,
titled with the task's id:

```
┌─ pause gate-board ─────────────────────────────────┐
│  1 command step is running:                        │
│                                                    │
│  gate-board · test    4m 12s                       │
│                                                    │
│  Killing it stops this task now. A killed step     │
│  runs again in full when you resume.               │
│                                                    │
│  [k] kill it   [l] leave it running   [esc] cancel │
└────────────────────────────────────────────────────┘
```

`P` does the same over the whole run rather than one row: it interrupts every live agent lane
the run owns and parks each of their tasks, then — if any command step anywhere in the queue is
running — opens the same kind of panel, titled `pause all` and worded in the plural:

```
┌─ pause all ─────────────────────────────────────────────┐
│  2 command steps are running:                           │
│                                                         │
│  gate-board · test    4m 12s                            │
│  quiet-pane · suite   1m 03s                            │
│                                                         │
│  Killing them stops the run now. A killed step          │
│  runs again in full when you resume.                    │
│                                                         │
│  [k] kill them   [l] leave them running   [esc] cancel  │
└─────────────────────────────────────────────────────────┘
```

`[k]` stops the command step or steps the panel names and parks each of their tasks the same
way an interrupted agent lane already was; `[l]` and `[esc]` both leave every one of them
running. Whichever agent lane `p` or `P` owns is interrupted and parked whether or not its
panel ends up opening — only a running command step waits on an answer.

`R` resumes every paused task whose own resume key is live, through the same `spoolway resume`
a single row's `r` runs. If any paused task carries `paused_at` — a
genuine gate, rather than a plain park `p` or an interrupt left behind — `R` opens a panel first,
naming those tasks, and only resumes anything once you confirm with `R` again; `esc` leaves every
paused task exactly where it is.

`u` and `U` take a task off the queue and back to pending — `~/.spoolway/<project>/pending/`, the
directory `spoolway queue add --from` reads a document back out of — for a task nothing has run
for yet: sitting on `queued` itself, with no worktree cut and no lane ever started. Neither
reaches a task that is running, paused or blocked, since undoing any of those would mean tearing
down a checkout, which is outside what either key does. Both always open a confirm panel first,
since writing a document back to pending is not free to undo either — the queue has no memory of
where it came from, so a document unqueued in error has to be sent again by hand.

`u` acts on the cursor's own row, naming the task and the path its document lands at:

```
┌─ unqueue chain-refusals ─────────────────────────┐
│  Nothing has run for it yet.                     │
│                                                  │
│  The document goes back to:                      │
│  ~/.spoolway/spoolway/pending/chain-refusals.md  │
│                                                  │
│  `spoolway queue` is what sends it again.        │
│                                                  │
│  [u] unqueue it   [esc] cancel                   │
└──────────────────────────────────────────────────┘
```

It also refuses a task that a still-queued task names in its own `depends_on`: carrying it back
to pending alone would leave that dependent waiting on a dependency the queue no longer shows.

`U` does the same for every task that has not started, naming the count and every id it covers,
one per line the same way `R`'s own panel lists its gated tasks — so the panel's width tracks the
longest id rather than growing with how many there are:

```
┌─ unqueue all ────────────────────────────────────┐
│  2 tasks have not started:                       │
│                                                  │
│  chain-refusals                                  │
│  month-instant                                   │
│                                                  │
│  Each goes back to pending. Running, paused and  │
│  blocked tasks stay where they are.              │
│                                                  │
│  [U] unqueue them   [esc] cancel                 │
└──────────────────────────────────────────────────┘
```

`U` carries no `depends_on` refusal of its own: whatever depends on a task in that set has not
started either, so it is in the set too, and both go back to pending together with nothing left
stranded. Either key drops every field `spoolway` writes on a task's way through the queue —
`stage`, `run`, `attempts`, `base_commit`, `cut_from` — from the document it writes, so `spoolway
queue add --from` accepts it again exactly as it would a document that had never been queued at
all.

Each group's block is closed by a total line: that group's whole OUT, COST and TIME — every
task, every step, every round it has run — aligned under the same columns the rows use. It
carries no label at all, only blank space up to the first figure; the sums under their own
columns are enough to say what the line is. A finished task stays on the board as
a dimmed `● done` row under its group for as long as that group still has something in the
queue; once the last of a group's tasks has archived, the whole block — its band, rows and
total — leaves the board rather than lingering as a stack of nothing but history.

A frame is drawn by clearing the pane and printing over it, so it has to fit the pane it is
drawn in: a frame one line too tall scrolls, and what scrolls off the top stays in the
scrollback, leaving a fresh lockup behind the board on every pass. So the frame is measured
against the pane, and the ticker is what gives — it shows as many of its newest arrival lines as
the rows left over allow, down to none at all on a pane with no room for one, and each line is
cut to the pane's width rather than wrapped. A line for a task arriving somewhere new names
the step that reported, not the one it arrived at: a move that lands on that step's own
`on_pass` route draws `✓ pass` in green, one landing on its `on_fail` route draws `✗ fail` in
red, and a task arriving at `paused` or `blocked` draws `● paused` or `● blocked` — the same
word and colour the table's STATE column uses — naming the step recorded in `paused_at` or
`blocked_from`. A move that matches none of those routes draws dim, with the step named and no
verdict word. After the verdict comes a position, the named step's own place in its pipeline's
walk over how many steps that walk still has left, or `—` where the task's pipeline cannot be
read. Only the verdict token itself carries colour; the rest of the line, including the step
name and the position, stays dim. The id, step and verdict columns are padded to the widest
value among the rows about to be drawn, so a burst of several tasks moving at once lines up
the way the table's own columns do, and the finished line is cut to the pane's width, ending in
`…` rather than wrapping. A task moving twice while its line is still in the window earns one
row rather than two: the earlier arrival is dropped and the later one takes its place at the
bottom of the block. A task entering the queue, and one archiving, earn no line at all — both
already show as a row change on the table above, not as something a lane reported. A pane too
short even for an empty ticker loses
the bottom of the frame instead: what the run is doing outranks what it has spent. None of
this applies to a board that is not drawn to a terminal, which has no bottom to fall off and
keeps every line. One blank row opens every frame, above the lockup, so its top line has a
margin to sit in rather than landing flush against the pane's own edge.

The lockup mark turns while at least one row is running, and holds still the moment none is.
Which of its two hand-drawn frames a draw picks is a function of the wall clock alone — the
elapsed seconds taken modulo two — rather than a count of how many times the
board has redrawn, so the dispatcher's own board and a second `watching` process reading the
same instant always agree on which frame to show. `spoolway init`'s one-shot banner, and every
board not drawn to a terminal, always draw the mark's first frame.

Below the table's natural width something has to give before the terminal wraps a row onto
two, which would scroll the same way a frame too tall does. TASK clips toward a
fourteen-character floor first, ending in `…` where it did; only once it is at that floor does
a column go — NEXT, then PIPELINE, then TIME, COST, OUT, CTX, right to left across the row —
stopping the moment the row fits. Losing PIPELINE is what brings back the wider gap TASK and
STEP carried before the column existed, so a board too narrow to show it reads exactly as one
that never gained it. The header and each group's band and total line shed exactly what the
rows shed, so the block stays aligned. None of this reaches `spoolway queue list`, which
prints the same table unclipped at every width — it is piped into `grep` too often for an
ellipsis to be safe there.

The NEXT column answers what happens to that task next. For one that is moving that is the
step it goes to when this one passes — one step, not the whole chain, which is a fact about
the pipeline rather than about the run and is one `spoolway pipeline show` away. A row on
`blocked` reads the same way, `→` and the step a pass out of it would actually carry the task
to — the same answer `cleared_block_target` gives, and nothing else: the reason it blocked is
not repeated here, since the row's own `● blocked` state already says that much. For one that
is stuck on `queued` it is the dependency it is waiting on, or `waiting for a worker slot to
free up` where no dependency holds it at all, and a lane holding a question in its pane names
the pane.

The STEP column carries its own counter, `<step> (N/M)`, wherever the step the task is on
declares a `loop:` budget for the route it arrived by — from the first arrival, with no
floor. `N` is the laps this task has taken from the step it arrived here from, `M` that
route's own budget, the same pair `apply_loop_budget` compares before it lets another one
through. A step with no declared budget for that route shows a bare step id.

A task holding a `parked_until:` in the future reads `● parked · 14:00`, on the board and in
`spoolway queue list` alike — the one state whose word carries a clock, because the only thing
a reader wants from a parked row is when it stops being parked. A reset within the day shows a
bare time; a longer wait shows the date and how far off it is.

```
TASK       PIPELINE  STEP       STATE                NEXT
wire-up    impl      implement  ● parked · 14:00     review
log-view   impl      implement  ● running            review
```

Re-queueing a document clears `parked_until:` along with the `usage_limit_hold` beside it, the
same way it clears every other field the dispatcher stamped on the earlier run — so a task can
be taken off a park without waiting the clock out.

`deploy` there is a project's own gated step — no step of any shipped pipeline declares
`gate:`, because a pull request is already the checkpoint. A task that has passed one reads
`● paused`, with `→` and the step passing the gate would carry it to in the NEXT column, and
— on a driving board where every dependency is satisfied and no lane of the task's own is
mid-turn — ` — [r] resumes it` after it, naming the key rather than the command.

CTX is how full the conversation a live lane is holding has got, as a percentage of that
step's model context window — the same reading `session_reuse_ctx` forks a fresh session
on, so a row approaching that number is a row about to lose its session. A profile whose
`agents.<profile>.session_blocked_ctx` is set watches this same reading on every pass, and
stops a lane whose row crosses it — see [`[agents.*]`](configuration.md#agents--who-runs-a-step).
OUT is what the
step has produced and COST is what it spent producing it: both read off the live lane's own
transcript while there is one, and off the ledger once there is not, where every round of
that step is summed. TIME is how long the row's own lane has been open at the step it is on:
`now - launched_at` while it is live, and the ledger's summed `wall_s` at that step once it
is not.

All four are about the step the row is on, and none of them about the task. A task's whole
bill is a different question and `spoolway spend task` answers it; on the row it would
mean a fresh session opened at a late step reporting the spend of every session before it,
next to a CTX and an OUT that are only ever about the new one. A step still running is
priced from the transcript it is writing, less whatever the ledger has already banked for
that session — so a lane resumed on its predecessor's session shows what *it* has spent, and
the figure on the board is the one the ledger records when the step settles.

Any of the four reads as `—` wherever there is no honest answer: no live lane, a model
whose window nothing resolves, a model nothing prices — a local worker's whole answer — or a
step nothing has been spent at yet. The transcript behind them is re-read at most every ten
seconds and only when it has changed, so a figure can be that stale — invisible against a
one-second redraw, and much cheaper than reading a megabyte of conversation per frame.

The footer shows one line per capped agent profile, its name and `slots <live>/<cap>`.
Quota-capable profiles also show `quota off`, or `quota ceiling <percent>%` when enabled. Where the model a profile's live lanes are running has `slots` of
its own, the figure counts against the model's own cap instead of the profile's
`concurrency` — a smaller local model swapped in shrinks that line on its own, with nobody
touching `[agents.pi]`. What a run has spent lives on the board itself now, in each group's
own total line, rather than summed once across every group at the foot of the frame.

One more line joins the footer whenever an `[issue_tracking]` hook has failed:
`issue_tracking: N hook failures — see tracking/`, counting every failing task-and-event pair
whose task is still in the queue, whether `on_fail` is `"ignore"` or `"pause"`. A pair whose
task has been archived does not count, so a stale failure stops showing once the task is
gone. A failed `fetch` run is keyed on an issue reference rather than a task and always
counts. The line is absent entirely while nothing has failed.

One more line joins the footer for each `local` model a task in the queue will run. It sits
below the whole slots block, after a blank line, and is never folded into a slots line. The
trigger is any agent step of that task's pipeline that names a model whose `[models]` entry
sets `local = true`. The step the task currently sits on does not matter, and neither does
its state — a queued, paused or blocked task all count.

The line reads `local   <model> — manually started sessions are not considered by the slots
pool`. The word `local` is bold and column-aligned under the slot names above it. The slot
figures count only the lanes spoolway started. A person can load the same server from another
terminal, and this line is the standing reminder of that gap.

The line is absent when nothing in the queue names a `local` model. It is also absent under
`--plain` and in a pipe, where no board is drawn. Every pipeline this project ships names
cloud models, so its own board never carries the line.

The header says whose process this is and how long it has been going, and nothing about
when the next pass is due: `up` moves on every redraw, which is all a board needs to show it
is alive, and a pass is not something a person watching can bring forward. Every frame is
re-read from the task files and the lane list — the board is a renderer, never a
participant, and a `--plain` run takes exactly the same decisions in the same order.

The two rows a frame cannot afford to reparse from scratch — every task ever archived, for a
finished row still on the board, and the whole usage ledger, for what a run has spent — are
cached instead, keyed off the archive directory's mtime and an append offset into the ledger.
A frame that finds neither changed hands back the same shared history rather than rebuilding
it, so a board left open over a large archive and a large ledger pays to parse them again only
when a task finishes or a lane banks a line, not once a second regardless.

The board draws on the main screen rather than the alternate one: a *second* `ctrl-c` kills
the process without unwinding, and the last frame simply stays where it is with the shell
prompt under it. The first `ctrl-c` is caught — it is the board's only control — and unwinds
through the same stop that an empty queue reaches, sweeping what the run was holding before
the process exits.

While the board is up it holds the terminal: the terminal's own cursor is hidden, and on Unix
stdin is put in raw-enough mode — no echo, no line buffering, `ctrl-c` deliberately still able
to raise `SIGINT` — so a keystroke is neither echoed under the footer nor held for a line that
outlives the run in the shell it returns to. On a driving board a keystroke read this way is
what moves the board's own `▸` cursor and fires the resume key; on a watching board or a
`--plain` run stdin is never read at all. Both ways a run ends reach the same restore, because
it hangs off the board going out of scope rather than off either exit path: the terminal is
drained of whatever was typed and handed back, cursor and all. Native Windows gets the cursor
back and nothing else — there is no termios there to take, and no key is read for anything on
that platform, so `ctrl-c` stays its only control.

From another terminal, while a run is going, `spoolway queue list` says where every task is
sitting and `spoolway lane <lane>` says what one is doing.

## Gates: when the pipeline waits for you

A step declares `gate: true` to say a person approves its work before the task goes any
further. Spoolway holds that pass by itself — nothing the lane does or reports changes where
it parks. **The lane is told a person will read its pane**, though: it leaves anything
viewable running instead of tearing it down, and writes a short account for them. The task
lands on `paused`, its pane is kept open for you to read, and it waits.

```
spoolway resume deploy-login                            # let it past
spoolway resume deploy-login --reject -m "not tonight"  # send it back round
```

`paused` is its own state, beside `blocked` rather than inside it, because the two ask you
for opposite things: a block is *I could not finish*, and a pause is *I finished, and you
wanted to see it first*. A paused task's dependents wait quietly rather than being reported
as stranded, and it is not counted against its profile's concurrency.

A staffed `blocked` step borrows this same shape for the one thing it cannot answer itself:
`spoolway report --pause` (or a `--fail` or `--block`, read the same way — see
[Escalation](#escalation)) parks the task on `paused`, `paused_at` naming the step it originally
blocked on rather than `blocked` itself. `spoolway resume` on it reaches the destination a pass
from `blocked` would have — carrying the task past that step, not back onto it.

The board's own `p` and `P` keys (see [Reading the state](#reading-the-state)) park a task on
`paused` too, `parked_from` naming the step it was interrupted at rather than one it gated on —
a plain park, with no `paused_at` and no `blocked_from`, telling it apart from both a genuine
gate and a real block. `R` and a row's own `r` both read that difference: only a task carrying
`paused_at` is confirmed past with a named panel before it resumes. Resuming a park is not a lap
either — the task never left the step, so putting it back banks nothing and repeats none of a
block's own bookkeeping; see `parked_from` in [the frontmatter field table](tasks.md).

A person is not only ever pressing `p` on the board to get this: typing Escape by hand,
straight into a lane's own pane, ends the turn the same way but does not by itself write
anything to the task file. The pass that next sees that lane settled reads its transcript for
the same aborted-turn shape [the agent kind's own marker
tells apart](agents.md#telling-an-interrupted-turn-from-a-finished-one), and if it finds one,
parks the task there and then — `parked_from` naming the step, written the same way the
board's own park is — rather than waiting on the reminder loop below to nudge it awake again.
Caught within the one pass that first sees the lane settled, never after.

Resuming either kind of park sends nothing extra if the lane it left is already busy again —
a person who typed straight into the pane after the Escape is mid-turn on their own, and the
resume only clears `parked_from` and hands the step back rather than typing a prompt on top of
what they just asked for. An idle lane still gets the ordinary resumed-park briefing, telling
it a person stopped its turn by hand and has since put it back, with nothing to clear and
nothing changed while it was stopped.

**A paused task's pane survives a stop, the same way a blocked one's does.** Stopping the
dispatcher never tears down the worktree or the pane of a task parked in front of a person —
`paused` unconditionally, and `blocked` wherever nobody is staffed to answer it — because
that pane is the only record of what a person's own turn in it produced.

During a run, whether the pane a paused task lands on is kept open at all comes down to the
backend: only one that keeps a resident session worth looking at holds it, so under
`headless`, which has no pane, the checkout is freed rather than held for nobody to read.

**Typing into a held pane is itself committed.** Once a lane is held for you to read, the
dispatcher watches its pane go busy and then idle again — that transition is a person's own
round finishing — and commits the worktree at that moment, with a `## Status Log` line saying
a person drove the round. Nothing is ever sent to the pane to prompt this: the dispatcher only
watches and commits, and the task stays on `paused` until you run `spoolway resume`.

**It used to ask the lane instead**, in a paragraph of the opening prompt telling it to print
its question and end its turn. That paragraph was the entire enforcement — spoolway's own
half was passive: it exempted the lane from the reminder loop and let it hold its slot. Over
three plan runs against a small local model, three gated steps, not one held: each lane read
the instruction, decided the work was fine, reported a pass and the task moved on. A
checkpoint a model can talk itself out of is not one. Held outside the session there is
nothing to talk to.

## A lane that settles without reporting

Any lane may end its turn without reporting an outcome. There is no way to look at a settled
session and tell a forgotten `spoolway report` from a lane genuinely stuck on a question — that
judgement stays outside the pipeline — so rather than guess, the dispatcher answers the
ambiguity the same way every time: the pass after the one that first sees the lane settled, it
is sent the report contract again, through the same `Mux::prompt` a person's own words go
through with `spoolway lane -m`. A lane that forgot acts on it; a lane waiting on a person is
told something that costs it nothing.

One case is not ambiguous at all, and is read before any of this runs: a lane whose transcript
ends in a person's own Escape (see [Gates](#gates-when-the-pipeline-waits-for-you)) is parked
on the spot, and never reaches the reminder or the escalation below. A settled lane with no
such record in its transcript takes exactly the road that follows.

It is reminded again on every later pass whose transcript carries something new since the last
reminder, and never on a pass where it does not — one comparison, capped at three reminders. A
lane genuinely mid-answer deserves patience, but not an unbounded amount of it purely because
its replies count as progress: a fourth due reminder is escalated in its place, naming the lane
and the pane you would have to go look at. The count lives on the lane record beside the
timestamp of its last reminder and needs no clearing — it dies with the record.

A lane can also go quiet on its transcript while a process it started is still running — a
long build, a server a turn forgot to stop — and a backend able to say so, through
`Mux::lane_process_alive`, excuses it from the reminder for exactly that reason rather than
guessing from the screen. That excuse has its own ceiling, `dispatch.lane_child_ceiling` (an
hour by default): past it, the lane is escalated for holding the process open rather than
reminded again, and the transcript signal above never even gets asked. A backend with no way
to answer the question keeps today's behaviour exactly, transcript alone.

One exit bounds the loop besides the count, and it is counted too, not timed. A lane that goes
fully quiet *after* a reminder — nothing further in its transcript — is the dead session no
reminder can reach: due for another reminder is one comparison, whether the transcript has
written since the last one, and a lane that has not is blocked on that very pass rather than
given a clock to wait out. There the task carries the reason and the last of what the pane said,
its session is closed and its worker slot goes back, and `spoolway resume` resumes it at the
step it never reported from, as the session it was — see
[When a task needs a person](tasks.md#when-a-task-needs-a-person) for what is continued and what
starts fresh.

None of this runs at all for a lane that *did* report — a report always moves the stage, so the
pass that would otherwise find a settled lane here finds the task already moved on instead.
`blocked` used to be the one exception: a `--fail` or a `--block` from there landed back on
`blocked` itself, so the stage never moved and a lane that reported looked, by that proxy,
exactly like one that never did. It no longer is — see [Escalation](#escalation) — but the
dispatcher still reads the answer rather than inferring it from movement, since a report and a
reminder can race on the same pass regardless: `spoolway report` stamps `last_report` with the
moment it banked, and a lane that reported after its own `started_at` has its round ended there —
its pane freed and its usage banked — without ever reaching the reminder above.

There is no exception for a gated step. There used to be — a gated lane was *expected* to end
its turn on a question, so it was left alone with no clock on it at all — and that exemption
went with the question: a gated lane reports like any other, and a settled one that did not is
the same fault it is anywhere else, reminded and, failing that, escalated the same way.

**This is the only mechanism**, in the sense that matters: nothing wired per agent kind, and
nothing that reads a transcript to guess at intent. The system prompt asks for the report in as
many words — see [What a lane is sent](#what-a-lane-is-sent) — that is prevention, and a good
model needs nothing else. Reminding is what happens once that fails; escalating is what happens
when it keeps failing with nobody left running to answer.

There used to be a second: a per-kind hook asked, as a turn ended, whether the lane had
reported, and pushed back once if it had not. It is gone. A reminder wired per agent kind is a
pipeline that behaves differently depending on who staffed a step, and the pi half of it
never fired for a headless lane at all — it loaded, it passed `spoolway doctor`, and it did
nothing. `Mux::prompt` is what stands in its place instead: implemented once, on every backend,
so a headless lane is reminded by reopening its pinned session and a resident one by typing into
the pane it never left — the same call either way, identical for every agent kind.

Either way you are told once that a pane is worth a look — the board's `waiting on you` — and
the pane is named in the status output.

Once, and not for the rest of the lane's life: the mark comes off the moment the lane is seen
working again. Whether you answered the question or the lane was only quiet long enough between
turns to look settled, a lane mid-turn is holding nothing anybody can reply to, and the row
reads as the running step it is. Settle again on the same step and the mark comes back, because
that is a second question and worth being told about.

A lane that ended its turn *waiting* on something — a backgrounded watch, a scheduled
wakeup — is this same case and not a third one. Nothing under spoolway resumes a settled
session beyond the reminder above, so the pane it left is the pane a lane holding a question
leaves, and the board says `waiting on you` about a pane with no question in it. That is
prevention's job too: the framing every lane is sent says in as many words that nothing brings
it back to a turn, and that nothing watches how long any one call takes any more — long waits
are polled through, awake, inside the turn, because there is no clock left to sit through them
instead.

## A step that carries its own session

A step can declare `session:` in the pipeline file to ask for its prompt's own conversation
back, rather than a fresh one, whenever that prompt has already been on this task:

```yaml
- id: implement
  session: true       # look for an earlier session by this prompt on this task
- id: document
  session: false       # the default — every visit opens fresh
```

`session:` is a plain switch now: the step says only *whether* to look. *How far* a found
session may be carried is split across two facts, neither of which is the step's own:
`agents.<profile>.session_reuse_ctx` bounds how large it may be, a fact about the agent
profile running the step (see [`[agents.*]`](configuration.md#agents--who-runs-a-step)); and
`models.<glob>.session_reuse_idle` bounds how long it may have sat, a fact about the model
itself (see [`[models."<glob>"]`](configuration.md#modelsglob--what-a-model-costs-and-how-big-its-window-is)).
A pipeline shared between two machines never has to name either bound.

This is a different question from the one-shot resume a blocked task gets back into (see
[When a task needs a person](tasks.md#when-a-task-needs-a-person)): that names one exact
session because a person is putting a specific lane back where it stopped, and only ever
applies to the step a task is already blocked on. `session:` is standing — it is checked on
every ordinary start of the step, is never cleared, and looks for whichever session its
prompt most recently held on this task rather than a particular lane. The two never
compete: a task coming back from `blocked` takes the one-shot resume, and `session:` is only
tried when that is not what is happening.

The lookup goes by prompt rather than by step, because that is the case this exists for. A
project that splits one prompt's work across two steps — an `implement` and a `repair`, say —
wants the second to find the conversation the first left behind rather than open a cold one on
the same task. Concretely: the newest entry in the usage ledger for this task whose own step
resolves, in this pipeline, to the same prompt this step runs.

The shipped pipeline no longer needs that split. A failed review goes back to `implement`
itself, which carries `session: true`, so the lookup finds that step's own earlier
conversation — the simplest case of the same rule.

Size is checked on every carried session — there is no unbounded form left, `true` only says
to look, never skips the arithmetic. It is not that ledger entry's own token count, which is
a running sum across every turn the session has ever spent and only ever grows, so it says
what the session has cost rather than how large it now is. What matters is the conversation's
size *right now*: the input, cache-read and cache-write tokens of its last assistant turn,
read straight from the transcript, weighed against the model's `context_window` from
`[models]` (see [Cost accounting](cost.md#pricing)) against `session_reuse_ctx`'s percentage.

Age is checked after size, and only on a session that size has already cleared. It comes from
the session's own store — `touched_at`, the same mtime the reminder loop already reads —
weighed against the model's `session_reuse_idle`, never queried from the provider. See [Cache
warmth is a model's fact](agents.md#cache-warmth-is-a-models-fact). Unset, or a store that
cannot be read, and the age check refuses nothing — there is no per-profile override left, only
the model's own horizon.

Four ways this falls through to a fresh session and the full opening prompt instead, and the
board's `RECENT` ticker says which:

- no earlier session by this prompt on this task can be found, or one was found but its
  transcript could not be read to size it
- one was found and read, but its last turn is already past `session_reuse_ctx`
- the model's `context_window` is not set, so there is nothing to measure the session
  against — this is checked before the transcript is ever read, so it is not that the
  session was measured and found small
- one was found, under size, but its store has sat past the model's `session_reuse_idle`

A lane resumed this way is prompted differently from an unblocked one: nobody has been and
gone, so there is nothing to say about a person — only that this is the prompt's next visit,
that the work its last visit asked for or left has been done, and that what changed since is
in the task file's `## Status Log`, same as it always is for a resumed lane.

A step that sets no `session:`, or sets `session: false`, is unaffected by any of this — it
opens fresh every time, as every step always has. `pipeline check` refuses anything else a
step's `session:` might name — a percentage, in particular, is refused rather than migrated,
naming `agents.<profile>.session_reuse_ctx` as where that bound lives now.

## Restarts, laps and escalation

Three different limits, and they bound three different things:

| Setting | Bounds | Zeroed by |
|---|---|---|
| The launch guard | How many times a lane may be **launched** at the step a task is on | Every transition |
| A step's `loop` | How many times a task may **arrive** at that step **from a given step** before escalating — a lap of the loop | A resume, for the loops the step it resumes at can spend. Binds the same in an [unattended run](pipelines.md#unattended-runs): every pipeline stages `blocked`, so an exit that resolves there spends the budget exactly as an attended run's would. `blocked` itself never spends this: see [Escalation](#escalation) — a report from `blocked` always moves the task off it, so it never arrives there from itself |
| The reminder loop | How many times a settled lane's **transcript** may go unwritten since its last reminder before it is blocked | Anything the lane writes to its transcript |
| The live-child ceiling | How long a lane may be excused the reminder loop for holding open a process it started before it is escalated anyway | The process exiting, or a backend with no way to check it in the first place |

A round is a lap, not a conversation. On a step with `session: true` the loop may be
re-prompted as often as it needs, and that used to matter to the count — only a cold start
spent the budget, so the same three visits could cost wildly different tokens depending on
which ones happened to open fresh, and only the lower reading could be written down in
advance and mean the same thing every run. Now every arrival counts the same: what still
bounds a conversation's size is `session_reuse_ctx`, entirely separate from this.

A task records `prompts` and `rounds`, both per route, keyed `from->to`. `prompts` is every
lane launched, retries included, and is what the usage ledger reads — banked at launch,
because a retried lane is a second prompt and was paid for like one. `rounds` is banked on
arrival instead, once per transition, by the same call that writes the `## Status Log` line
for it — a relaunch after a lane died before saying anything is a second prompt on the same
lap, not a second lap, so it costs `prompts` and leaves `rounds` alone. `rounds` is the only
one a loop budget is spent from, and the one the board's own `(N/M)` counter reads.

When a budget runs out the task carries on to that step's `on_loop_max` if it names one,
or to its own `on_pass` otherwise, and a `## Status Log` line says so, naming the round
count. A loop whose output nobody else will ever read can say `on_loop_max: blocked` instead,
to stop rather than carry on.

The launch guard catches a lane that dies at launch. Such a lane leaves no session behind, so
the task looks unstarted again on the next pass — without a budget it would be re-spawned
forever. It is **not** a retry budget for work that went badly: a lane that ends its turn keeps its
pane. The counter is forgiven the moment a pass can see the lane the launch produced, so a
person stopping the dispatcher mid-task no longer blocks that task on the next run — see
`dispatch.tear_lanes_on_stop`. Never a number
anybody tuned in practice, so it is a constant now — `dispatch::MAX_LAUNCHES`, one launch
before a person — rather than the old `dispatch.max_launches` setting, which still parses in
a config that has it and is dropped on the next save. In an [unattended
run](pipelines.md#unattended-runs) there is no person to hand it to, so it becomes a backoff
instead: the task keeps its place and is retried on a doubling delay, capped at an hour.

A dispatcher killed between launching a lane and finishing that pass gets one extra pass of
grace before the guard above applies. `lanes.json` is written back when a pass returns, its
error paths included, but a process killed outright never reaches that write, so that crash
loses the record the launch itself just wrote, and the restarted dispatcher's first
pass would otherwise read the silence as a genuinely dead launch and hand the task to a person
over a failure that never happened. Instead, the first pass that finds a launch with no
`lanes.json` record of its own — gated on the task actually having a `launched_at`, so this
never fires for a task that reached the guard some other way — marks the lane seen and tries
again rather than escalating. That mark is itself written to `lanes.json`, so it survives past
this one pass: the very next pass, whenever it lands, reads the lane as one this dispatcher has
already watched, and escalates normally if it is still gone. The grace is one pass, not a
window of time — a restart minutes or hours later gets exactly the same one chance a restart a
second later would.

A `lanes.json` that will not parse is not discarded. The pass starts from no lane records, but
it first copies the bad file aside as `lanes.json.bad` and writes a line to the problem log
saying so, rather than silently overwriting a hand edit or a disk error with an empty file.

A lane whose pane carries its own kind's usage-limit message takes a different road entirely,
and it does not go through the backoff above. **The lane is left running.** Its pane, session
and worktree are untouched and no `stop_lane` is called, because the agent picks its own turn
back up once the window resets — killing it would throw away work that is going to finish by
itself. Instead the dispatcher writes `parked_until:` on the task and stops nudging it: no
reminder is sent while the park stands, the task never reaches `blocked`, and `attempts` is
left exactly where the launch that hit the limit set it. A `## Status Log` line names the
limit and the time the park runs to.

This check runs whether the multiplexer reports the lane `Working` or settled. A limit surface
that keeps redrawing never settles, so a check that only looked at settled lanes would miss the
common case entirely and leave the reminder loop to escalate it.

The park's clock comes from an observed exhausted window's reset, including a weekly reset.
Without one, `quota_retries` backs off rechecks from one minute to one hour independently of
launch attempts. The counter survives restarts and clears when the lane resumes or the task
moves on. Repeated holds update the clock without appending duplicate status-log entries.
Whether a tail is a limit is answered by `Adapter::usage_limit` in `src/agent.rs`.

### Parking a task before the limit lands

The check above catches a limit that has already landed. `agents.<profile>.quota_ceiling`
catches one before it does. Ahead of starting a lane, a pass reads the profile's kind's own
cached usage percentage — something the agent wrote to disk, never a network call — and at or
above the ceiling on either window it starts no new lane of that profile at all. Every candidate task of
that profile gets `parked_until:` written from the tripped window's own `resets_at`, and the
first park of a continuous hold also stamps `parked_at:` — the fixed start the age counts from —
while every later re-probe moves only `parked_until`. Tasks
whose step names a different profile are staffed in the same pass. See [Reading a kind's quota
before a lane starts](agents.md#reading-a-kinds-quota-before-a-lane-starts) for where the
reading comes from. An enabled ceiling holds launches when that reading is unavailable,
stale, malformed or expired, rechecking with the same persistent backoff. The board's profile
footer names the quota ceiling, or `quota off`; a task awaiting a reading says
`quota unavailable` beside its recheck time. `spoolway agent verify <kind>` diagnoses the source.
The ceiling checks admission only: running lanes and external sessions can still exhaust
the account during processing.

    pass 41
      wire-up: `implement` parked until 14:00 — claude at 88% of its
               five-hour window, ceiling is 85
      log-view: started `implement` on pi
      lanes still working

**The park lives on the task file, not in the dispatcher.** That is the whole point of writing
a timestamp rather than holding a timer: a seven-day wait outlives any dispatcher process, and
often the machine. A dispatcher started from cold reads `parked_until:` at the top of its
per-task loop, before it resolves a step or looks at a lane, and honours it without taking any
reading of its own. The first pass after the timestamp has passed rechecks quota without touching the park: an expired
deadline is a recheck, not an exit, so the same pass resolves the hold in one write — re-parking
with a fresh deadline against a still-exhausted reading, or admitting another lane once the reading
has dropped. The hold's age, stamped once when it began (`parked_at`), keeps counting from there
rather than restarting, and a pass that reaches no decision at all (a dependency still open, say)
leaves the whole park on disk with its deadline simply in the past — still held, not yet decided. A
forty-minute five-hour wait and a six-day seven-day wait behave
identically.

A park never ends the run. The dispatcher keeps passing and reports the park each time; closing
it is a person's call.


Resuming a task hands it its full attempt budget back, since every transition zeroes the
count.

It hands back loop budgets too, but only the ones it is about to need: the bounded routes
out of the step it resumes at. A task that ran out of review-and-fix laps resumes at `review`
with that loop's `rounds` back at zero — without it, resuming buys one attempt and the next
failure hits the same wall, which is not resuming. Loops elsewhere in the pipeline keep what
they have spent: saying carry on to a stuck review says nothing about the rebase loop at the
other end. `prompts` is never handed back — it is the record of what this task has cost, not
a licence. `resume` prints each budget it returns.

## Escalation

An escalation parks the task in the built-in `blocked` state. Attended, it tells you and waits
for `spoolway resume`. In an [unattended run](pipelines.md#unattended-runs) there is nobody to
tell, so a lane is started on `blocked` instead — every pipeline stages it, declared or
materialised from `[unattended]`'s `blocked_*` keys — and that lane's pass carries the task on
under `unattended.skip_blocked_lane`. That covers every road to `blocked`, not only a lane's
own `--block`: a `fail` with nowhere left to route, a silent lane, a dead launch, a live lane
stopped for reading over its profile's `session_blocked_ctx` ceiling. They are ways of writing
down one fact —
this task is not moving without help — and which of them it was is not something anybody
remembers when the notification arrives.

**None of those roads lead from `blocked` back to `blocked`.** A staffed `blocked` step's own
lane answers with `--pass` when it cleared the way, and `--pause` when it genuinely cannot — and
`--fail` or `--block`, the old habit, are read as `--pause` would be rather than sent round again.
A `--pause` (or a `--fail` or `--block` reported from `blocked`) parks the task on `paused`,
naming the step it originally blocked on, rather than on `blocked` itself — the same is true of
this section's own escalation, when it is a `blocked` lane itself that died, went silent, or
never launched: it too lands on `paused`, not `blocked`. `spoolway resume` on a task paused this
way reaches exactly the destination a pass from `blocked` would have.

A spent `loop` is not one of those roads. It takes the step's exit — `on_loop_max` if it
names one, `on_pass` otherwise — a destination the pipeline named rather than a request for a
person, and an unattended run follows it exactly as an attended one does. An exit that
resolves to `blocked` is no exception: every pipeline stages `blocked`, so the budget binds
there exactly as it would in an attended run, because `blocked` is a lane now, not a person.

**A task that reaches `blocked` keeps its pane — unless a lane is about to start there.** The
lane that stopped is over — its slot is given back and what it spent is booked — but the pane
it ran in is left open and focused, because the session as it stood when it stopped is the
only real account of what went wrong, and it is not in the task file. It is closed on the pass
after the task is unblocked, just before the step it stopped on is started again. An
unattended run keeps no pane for this: a fresh lane is about to start on the staffed `blocked`
step (see [Staffing `blocked`](pipelines.md#staffing-blocked) in pipelines.md), so something
is about to be working on the task, not waiting to be read.

When a task needs a person — an escalation, or a gated step waiting for an answer — the
board says so: the row is marked amber, once rather than once per pass, with the pane to
go and look at named in its NEXT column. The mark does not move the row — a group's rows
still sort by run order, not by whether they need a person.
`spoolway queue list` gives the same reading from another terminal, or with no run going.

## Backends: where a lane lives

```toml
[dispatch]
backend = "herdr"     # or "tmux", or "headless"
```

### Under a multiplexer

Every lane is a real pane you can watch, attach to and take over. Under `grouped`, a task's
lane is a pane split off its **project's** tab in the shared `spoolway-dispatcher` group;
under `split`, each task gets a row of its own — a workspace (herdr) or session (tmux) — and
the lane is a pane split off that. Either way, the pane belongs to the task rather than to
one of its steps: a step that finishes is talked out of its pane and the step after it starts
in the same one, so a task holds one lane pane for its whole life and its `pane_id` does not
change as it moves. A kind with no way to leave a pane keeps the older lifecycle — its pane
closes with it, and the next step splits a fresh one. See [Vacating a pane](#vacating-a-pane).

**A restarted multiplexer heals itself on the next pass.** A worktree can outlive the server
fronting it — a reboot, `tmux kill-server`, herdr restarted — even though every workspace and
tab id it ever handed out does not. Before reusing a task's recorded `workspace_id` and
`tab_id`, the dispatcher checks them against the multiplexer that is actually running; if
either has gone, it clears all four recorded fields and opens a fresh pane on the same
worktree, exactly as it already did when the worktree's own directory had gone missing. The
worktree, its branch and whether it was borrowed are never touched — only the multiplexer ids
move. Headless answers this check `true` unconditionally, since its workspace id is derived
from the checkout path itself and has nothing separate to lose track of.

### One home for every run, in every project

`spoolway-dispatcher` is a single workspace (herdr) or session (tmux), fixed and shared by
every project that dispatches on this machine — not named after any one of them, so two
projects dispatching at once share this one row in the sidebar rather than opening one each.
It holds no checkout of its own, opened instead on `~/.spoolway/.dispatcher/`, which is
not a repository — that absence of a checkout is the other half of how it is found again: a
workspace opened anywhere inside a repository comes back bound to it, and this directory binds
to nothing. Named with a leading dot so it sorts apart from every project's own directory
beside it, and so no checkout can claim the name for itself — `spoolway init` refuses it.

`herdr_mode` and `tmux_mode` both take the same two answers: `grouped` and `split`. (The
herdr-era spellings `workspace` and `worktrees` still parse and mean the same thing; so does
the singular `worktree`.)

### `herdr_mode = "grouped"` — one tab per project

```toml
[dispatch]
herdr_mode = "grouped"    # the default
```

The shared workspace holds one tab per project, labelled with the project's directory name,
holding every lane that project currently has going — whatever group each task belongs to.
Reading down the sidebar is reading which projects are dispatching right now, not which groups:

```
▾ spoolway-dispatcher
    spoolway          every lane of this project, any group
    otherapp          that project's lanes, dispatching at the same time
```

`spoolway dispatch` never moves or relaunches itself to draw this: the board stays in the pane
you typed the command into, under `grouped` exactly as it already did under `split`. The shared
workspace is opened purely to hold lanes — found or opened the first time a project dispatches,
never joined by the dispatcher's own pane.

**A project's tab holds one pane per running task of that project — nothing else.** The first
task through opens the tab itself, on its own worktree rather than the bare project root, and
runs straight in the pane that opens with it: there is no separate placeholder pane sitting idle
in the project root the way there used to be. Every task after that splits its own pane off one
already in the tab, because herdr has no rebalance command and always splitting the newest pane
degenerates into slivers as the tab grows.

Two rules pick the split. The pane cut is the biggest one holding no agent, so a split takes its
space from an idle shell rather than from a running lane; when every pane holds an agent, the
biggest pane overall is cut instead. The direction is `down` while both halves would keep at
least twenty rows, and `right` below that — a terminal cell is about twice as tall as it is
wide, so comparing a rect's columns against its rows directly reads a full-screen tab as wider
than tall and lays every pane out as a narrow column.

tmux needs none of that arithmetic: `select-layout tiled` retiles the whole window after every
split.

The tab is named for the project's own directory, and found again by that name alone — there is
no pane or directory left to check it against. Nor is one needed: a project's directory name is
already unique on this machine, since `spoolway init` refuses a second checkout that claims a
basename another one already has, so two same-named projects sharing this workspace cannot
happen to begin with.

Spoolway cuts a task's worktree itself, with `git worktree add`, under
`~/.spoolway/<project>/worktrees/` by default — because the shared workspace holds no
checkout for herdr to cut against. That is what keeps the sidebar clean:
herdr never hears about the checkout, so there is no row for it, and `git worktree list` is
where you find one an interrupted run left behind.

A step ending closes its own pane, never the tab — the tab is the project's, shared by every
task of it still running, and outlives any one of them. **A project's tab closes with the
project**: when its last task is archived, and on a stop once the sweep has emptied it. What is
left standing is any project with something parked in front of a person in it — paused, or
blocked with nobody staffed to answer it — that pane is being held open for you to read.
Never the shared workspace: another project's lanes may still be live in it, so
only you close that row, by hand.

### `herdr_mode = "split"` — a row per task

```toml
[dispatch]
herdr_mode = "split"
```

No tab in the shared workspace at all, and the dispatcher stays where you started it. Each
task gets its own herdr workspace, cut with git and bound to the result with `herdr worktree
open` — never `workspace create`, which leaves a row with nothing for herdr to nest it under.
Bound, the row sits nested under the project's own row, prefixed `spoolway/` rather than named
after the task alone, so the group already naming the repository and the prefix naming the
row's owner is enough to tell your own checkouts apart from the run's. The label is fixed at
creation and never relabelled as the task moves through its steps; what says which step a lane
is on now is the lane's own pane.

Pick this when a row per task is what you want to look at, and `grouped` when you would
rather the run were one thing.

**Stopping ends the run's live agents and gives its worktrees back**, if
[`tear_lanes_on_stop`](configuration.md) says so — every lane the run holds stops, along with
its background runs, and then the workspace and worktree of every task it cut one for.
`worktree open` having bound the workspace is what makes the second half work: `worktree
remove` can find the checkout to take with it, which it could not when the workspace had
nothing recorded against it. A task that reached `done` already tore its own down on the way,
and took its local branch with it, so what this catches is whatever was still in flight when
you stopped.

Under `grouped` that means each task's checkout is removed with git directly — there is no tab
or workspace of the task's own to close, since it shared its project's; under `split` the
task's whole workspace goes, taking the worktree with it.

Those tasks stay in the queue rather than being archived: they were interrupted, not
finished. **Their branches stay too.** The worktree is the thing the run was holding; the
commits on the branch are what the agent got done before you stopped it, and there is nowhere
else they exist. The next run finds the branch, cuts a fresh worktree on it, and resumes the
step on top of that work rather than starting it again from base.

A task parked in front of a person — paused, or blocked with nobody staffed to answer it —
is never swept. Its pane is being held open for you to read and `spoolway resume` resumes it
against the checkout underneath, so anything spared is still
sitting in its tab or workspace when the run is over. `Ctrl-C` is caught to make this happen; a
second one kills the run outright, and so does anything that stops the process without asking.

Only sessions spoolway named are spoolway's. A lane is named `<task> · <step>` and matched on
that name — the same string the pane shows, `spoolway lane` lists, and `spoolway lane -m`
takes, with no fallback to an older spelling — which is absent for anything started by hand, so
your own agent windows in the same session are never counted, prompted, or torn down.

**Submitting a prompt can land it typed but unsent.** Observed against Claude Code: the text
appears in the pane's input box, no turn begins, and the task would otherwise sit on its step
until somebody looked. herdr tells the two cases apart on its own, so spoolway acts on what it
reports rather than re-deriving it from the lane's status: a stalled submission comes back
with `agent_prompt_stalled` in herdr's own error envelope, and a plain `timeout` at the same
bound means the same thing, because the timeout spoolway passes sits exactly on herdr's own
stall threshold. Only a reported stall gets a single `Enter` sent to the pane — a lane that
answered normally, or failed for any other reason, is left alone — and that Enter is
re-verified with a bounded `herdr agent wait` rather than a single immediate status read. A
stall that survives the Enter is reported as a failure rather than papered over.

### tmux

```toml
[dispatch]
backend = "tmux"
tmux_mode = "grouped"    # or "split"
```

The same layouts, spoken tmux: a tmux **session** stands where a herdr workspace does, a
**window** where a tab does, and a pane is a pane. Every lane is a real pane here too —
attach to watch or take over, detach and the run keeps going. tmux's ids are used
throughout, so renaming or rearranging things by hand confuses nothing.

No tmux knowledge is assumed. The run lives in a background tmux server whether or not you
are looking; you attach to look and detach to walk away. Everything is the prefix key —
press `Ctrl-b`, release, then one key — and spoolway turns the mouse on for the sessions it
creates, so clicking a window in the status bar, clicking into a pane, and wheel-scrolling
all work:

```
tmux attach -t spoolway-dispatcher    attach — the same name for every project
Ctrl-b  d       detach — everything keeps running
Ctrl-b  w       every session and window as a tree: the "sidebar" key
Ctrl-b  n / p   next / previous window;  Ctrl-b 1..9 jumps by number
Ctrl-b  ↑ / ↓   move between panes;  Ctrl-b z zooms one fullscreen
Ctrl-b  [       scroll back (q to leave)
```

**`tmux_mode = "grouped"`** is one session, `spoolway-dispatcher`, shared by every project
dispatching on the machine. Each project gets one window of its own — found or opened the
first time it dispatches, its own invocation moved into it exactly as it always has been under
tmux — carrying one pane per running task, nothing else. `tmux ls` shows the one shared
session; `Ctrl-b w` walks its windows, one per project:

```
$ tmux ls
spoolway-dispatcher:  2 windows  (created Thu Aug 14 09:12)

┌───────────────────────────── [1] spoolway ──────────────────────────────┐
│ ● add-auth · implement                 │ ● port-docs · plan             │
│   editing src/auth.rs…                 │   drafting the task list…      │
│   ✻ Working… (esc to interrupt)        │   ✻ Working… (esc to interrupt)│
├────────────────────────────────────────┼────────────────────────────────┤
│ ● fix-flaky · review                   │                                │
│   waiting on you                       │                                │
└────────────────────────────────────────┴────────────────────────────────┘
```

A step ending closes only its own pane; the window survives it and every task after it, and
closes only when the project's dispatcher stops — never the shared session, which another
project's lanes may still be live in.

**`tmux_mode = "split"`** is a session per task instead, named `spoolway/<task>` and fixed —
never relabelled as the task moves through its steps — with the dispatcher left in the
terminal you started it in:

```
$ tmux ls
spoolway/add-auth:  1 windows  (created Thu Aug 14 09:12)
spoolway/fix-flaky: 1 windows  (created Thu Aug 14 09:14)

┌────────────────────────── spoolway/add-auth ──────────────────────────┐
│ ╭───────────────────────────────────────────────────────────────────╮ │
│ │ ● claude — add-auth · implement                                   │ │
│ │   I'll add the auth middleware next. Editing src/auth.rs…         │ │
│ │   ✻ Working… (esc to interrupt)                                   │ │
│ ╰───────────────────────────────────────────────────────────────────╯ │
├───────────────────────────────────────────────────────────────────────┤
│ [0] add-auth · implement*                                             │
└───────────────────────────────────────────────────────────────────────┘
```

Pick `split` when a row per task is what you want `tmux ls` to show, and `grouped` — the
default — when the run should be one shared session rather than a row per task. Teardown
follows the layout: under `grouped` a task's own checkout is removed with git directly — there
is no window or session of the task's own to close — and under `split` the task's whole
session goes, taking the worktree with it. A task parked in front of a person — paused, or
blocked with nobody staffed to answer it — is spared either way, its pane held and focused
for you to read.

Two things tmux has no opinion on, so spoolway carries them itself:

- **Which panes are spoolway's.** Lanes are stamped onto their panes (tmux "user options"),
  along with the project they belong to — so your own tmux sessions on the same server are
  never counted, prompted, or closed, and two projects dispatching at once stay out of each
  other's lanes. The same stamps are how `spoolway lane` and `spoolway lane -m` in a fresh
  terminal find the lanes the dispatcher started.
- **Whether a lane is mid-turn.** herdr's agent layer answers that directly; tmux is read
  the way you would read it — a pane whose screen is still changing is working, one that
  has sat unchanged for a dozen seconds has settled and is promptable again. The reminder
  loop never depends on this: silence is measured on the agent's own transcript, and tmux
  has no cheaper way to tell whether a lane still holds a process of its own open, so
  `Mux::lane_process_alive` answers `None` here exactly as it does on any other backend
  that cannot check.

Prompts go in as one bracketed paste followed by Enter, so a multi-line prompt cannot
self-submit halfway — and, as under herdr, a submission that visibly fails to start a turn
gets exactly one more Enter before being reported rather than papered over.

### Vacating a pane

`Mux::vacate_lane` is a weaker ending than `Mux::stop_lane`: it asks the session in a pane to
leave, rather than destroying the pane to end it. It is what lets a task's steps share a single
pane instead of each splitting a new one and closing it again, and the dispatcher calls it
wherever it used to close a finished lane's pane.

A step is not started while an earlier step of the same task still has a live lane, because
that lane still holds the task's pane — a lane reports mid-turn and keeps talking, so the pass
that reads the moved stage still sees it working. The wait is bounded by `HANDOVER_WAIT` in
`src/dispatch.rs`, two minutes, and is a constant rather than a config key. Past it the next
step starts anyway: it splits a pane of its own first, and the lane that would not let go is
closed after — never the other way round, since a pane closed with nothing beside it can take
its tab with it.

A step that reports back onto itself — `blocked` reporting `--block` again, most often — hands
its pane forward the same way, but across a pass boundary rather than within one: the task is
not a candidate on the pass that ends the round, so there is nobody in that pass to hand the
pane to. The dispatcher writes the pane down on the lane's own record instead, and the pass
that starts that step's name again picks it up from there.

What a person watching the pane sees during any handover is it going blank. Claude Code runs
in the alternate screen buffer, so leaving a pane loses its transcript the same way closing it
would — nothing is captured to carry the conversation across, only the pane itself.

Which kinds have anything to send is `Adapter.quit` — see [Leaving a pane without
closing it](agents.md#leaving-a-pane-without-closing-it) in agents.md — and only `claude`
carries a gesture today. A backend that can send it types the line at the pane, submits it,
and then watches for the agent to actually go rather than assuming a submitted line worked: up
to ten seconds, polled every quarter second, before giving up. A pane reported empty that is
not is the one failure this is written to avoid, since the next step's `agent start` would type
straight into whatever conversation is still sitting there.

The call reports which of three things happened, because "the pane did not come back to a
shell" covers two situations a caller has to tell apart and handle differently: the session
left and the pane is standing empty at its shell; no gesture was tried, so the session was
ended the only other way there is and the pane went with it — exactly today's behaviour; or the
gesture was sent and the agent was still there when the ten seconds ran out, in which case the
pane is left exactly as found, agent and all, for the caller to close only after splitting
whatever replaces it.

herdr is the only backend that can send a gesture at all, by typing the kind's `quit` line into
the pane and watching `herdr agent list` for the session to drop off it. tmux cannot,
structurally: a tmux lane's pane *is* the agent's own process, so once it respawns the pane
that way there is no shell underneath left to hand back. Headless has no pane in the first
place. Both fall back to closing the pane, exactly as `stop_lane` already does.

### Headless

```toml
[dispatch]
backend = "headless"
```

The same pipeline with no multiplexer, no tmux and no panes. Each turn is its own detached
process, its output goes to a log, and the conversation is carried across turns by the
session id the profile already pins — the same one that makes cost accounting possible.

**Every scheduling decision is identical.** A pass still reconciles from the task file's
stage and the live lane list; the dispatcher still uses no model. What changes is only where
a lane lives, and therefore what a person can do with one.

Three things fall out of a turn being a process rather than a resident session:

- **Status gets simpler, not harder.** Running is `working`, exited is `done`. There is no
  blocked state: a headless run cannot stop and ask, it just ends — which the dispatcher
  already reads as "settled with an unchanged stage" and reminds by reopening the lane's own
  pinned session, exactly as it does under a multiplexer; see [A lane that settles without
  reporting](#a-lane-that-settles-without-reporting).
- **A gate stops costing a worker slot.** There is no session to protect, so an approval
  sitting unread overnight holds nothing.
- **Taking over changes shape.** There is no pane to attach to; see below.

What you lose is *live* observation — watching a lane think. That is the honest price.
The board still narrates the run — it is drawn by the dispatcher itself, so it is there
headless exactly as it is under a multiplexer — and a lane's transcript is `spoolway lane`.

Only kinds spoolway has established headless flags for can run this way — `pi`, `claude`
and `codex` today. A kind without them is refused at lane start rather than
launched with a guessed flag.

Worktrees are cut at the configured worktree root
(`~/.spoolway/<project>/worktrees` by default), deliberately outside the checkout: under
`.spoolway/` every task's checkout would sit beside the prompts every lane already reads,
and a lane building there could rewrite any other task's checkout by name.

## Looking into a lane

```
spoolway lane                       # the lanes there are
spoolway lane "login · implement"   # that lane's output
spoolway lane "login · implement" -n 500
```

A lane name holds a space, so quote it — an unquoted `login · implement` reaches the
shell as three words, not one argument.

This is the model-free answer to "what is it up to", the way looking at a pane costs
nothing. Under a multiplexer it reads the lane's pane. Headless it reads the lane's log —
which **outlives the lane**, so a task that went wrong can still be looked into after it has
finished.

To intervene by hand headless, `spoolway lane <lane> --attach` reopens the lane's own session in
your terminal, whole conversation included — it works over plain SSH, which attaching to
somebody's multiplexer does not.

## Where work happens on disk

Each task gets a git worktree cut from its base branch, or from its first dependency's branch
when it has a `depends_on` — unless somebody already has the task's branch checked out, in
which case the lane borrows that checkout and cleanup leaves it alone; see [Whose
worktree](pipelines.md#whose-worktree).

**One root, every backend, every layout**: `worktree_root`, which is
`~/.spoolway/<project>/worktrees` unless you name another — nested under the project's own
home, beside its queue and archive, so a worktree cut here never registers as a workspace of
its own the way one cut at a repository's root did. The directory inside it is the branch
flattened to one component — `task-<id>`, or `task-<slug>-<id>` when a tracker slug prefixed
the branch — for every backend now. One entry per task is what makes "everything the run is
holding" something you can list, and the branch is what `git worktree list` shows and so what
a person looking for it reads.

Deliberately *not* under `~/.herdr/`, which it used to be. That is herdr's directory, and
these are checkouts herdr never hears about until it is pointed at one. A worktree you cut by
hand still lives where herdr files it, `~/.herdr/worktrees/<repo>/<branch>`.

A task worktree carries a `.spoolway/` of its own, because the config, the pipeline and the
prompts are tracked. The queue is not, deliberately. That is why every command resolves the
project through git's common directory rather than by walking up for a `.spoolway/`
directory: the latter would find the worktree and an empty queue.

Worktrees and branches are removed, and the task file archived, when a task reaches a
terminal step with cleanup enabled — unless the checkout was borrowed, in which case it and
its branch were somebody else's before the task started and are still theirs after it. The
task file records which it was, as `borrowed:`, because afterwards the two look identical to
git.

Cleanup commits whatever the worktree still holds before it removes anything, the same
`wip(<task>): <step>` backstop `spoolway report` runs. If that commit cannot be made — a git
command fails, or `dispatch.auto_commit` is off and the lane left work behind — the task is
held at `blocked` instead of archived, with a `## Status Log` line saying why, so a person
sees the worktree before it is gone. Residue a lane deliberately left is not this: it is on
this machine's mirror and named in the log, and cleanup proceeds.

A finished task's own branch is kept alive past that cleanup while anything still queued names
it in `depends_on` — that branch is what the dependent's worktree gets cut from. It is freed by
the next cleanup to run once nothing queued needs it any more.

Archiving a task also reclaims the run files and session state named for it. Its hook run
files under `tracking/` and its command-step run files under `commands/` are deleted, matched
on the `<task> · ` filename prefix so another task's files are left alone. The per-session
agent home each of its lanes was given is removed too — both the lanes just banked by this
cleanup and any older lane record still held for the task. All of this runs only after the
task file has moved into `archive/`, past the point where cleanup can still turn back and
hold the task at `blocked`, so a held task keeps its scratch tree, its run files and its
session homes for `spoolway resume`.
