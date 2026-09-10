---
domain: cli
covers: ["src/cli.rs", "src/commands/**", "src/main.rs"]
---

# CLI reference

Every command, subcommand and flag. Where a command has a page of its own, it is linked.
The sections below are the groups `spoolway --help` prints, in the same order.

## Global flags

Available on every command.

| Flag | Meaning |
|---|---|
| `-C`, `--repo <DIR>` | Operate on this repository instead of the current directory's project |
| `--json` | Machine-readable output, where the command supports it |
| `--help` | Full help. Several commands carry worked examples in their long help |
| `--version` | The version of spoolway you are running |

## The `checkout:` line

The commands that read the tracked control plane answer for the checkout they were run in, not
always for the project. Where those two differ, each of them prints one line first, naming the
checkout and the branch it has out:

```
checkout: ~/.spoolway/worktrees/checkout-line (task/checkout-line)
```

It is suppressed whenever the checkout **is** the project, which is where almost every command
runs — including when `-C` names the project explicitly. In the main checkout these commands
print exactly what they always did.

The commands that print it are `pipeline show`, `pipeline check`, `pipeline list`, `prompt
list`, `prompt check`, `config show`, `config list`, `config get` and `config path`. `doctor` reports on both
sides at once and prints the line once. Commands reading only the queue, the lane state, the
archive or the usage ledger never print it: they answer for the project and always did.

Under `--json` the same two facts arrive as one line of JSON instead of prose, so a script reads
them without parsing English:

```
{"path":"/home/you/.spoolway/worktrees/checkout-line","branch":"task/checkout-line"}
```

The path is left absolute there. A script reading it has no `$HOME` to resolve `~` against.

## Your work

### `spoolway queue`

Bare, with no subcommand, opens the queue screen: a terminal interface listing one row per
distinct `group:` across the task documents under `~/.spoolway/<project>/pending/`,
`~/.spoolway/<project>/queue/`, and `~/.spoolway/<project>/archive/` on the left, and the
highlighted group's tasks on the right. Groups that can still be queued are listed first, groups
the queue already holds every task of next, and groups that have finished every task last, each
third ordered newest-written first — the moment a group's newest document was written, read off
the file's own birth time, not its filename or when it was last touched. A blank row separates
each pair of thirds that is on screen at once — up to two blank rows in all; a blank row is never
a group, and moving the cursor across one lands on the group beyond it in a single key press. The
screen opens showing only the still-queueable groups — the left pane's title reads
`groups <shown> of <total>`, and hiding every group leaves the pane reading
`nothing to queue — <n> groups hidden` rather than an empty list. `h` widens what is shown one
step at a time and wraps: the footer names the next widening, reading `h show queued` in the
opening state, `h show done` once queued groups are also shown, and `h hide all` once every group
is on screen, at which point pressing `h` again returns to the opening state.

A group is queued whole, so queueing it removes exactly its own documents from the pending
directory and leaves every other group's where they are — and its row survives that, built
straight from its documents in the queue directory instead, marked `queued` with no checkbox.
Once every one of a group's tasks has run to the end and moved into the archive directory, its
row is marked `done` instead, still with no checkbox — a finished group stays on screen behind
the third `h` press rather than disappearing, so `s` and `p` can still reach it. A group with
some tasks archived and others still queued shows its whole chain in the tasks pane, and each
task's own row there carries its own `queued` or `done` tail next to its id, since a group's own
row can only speak for the group as a whole.

`f` opens a filter over the left pane: the query is drawn on the pane's own first row, reading
`find: <query>`, with a
cursor after it, typing appends and backspace deletes, `enter` keeps the filter and returns to
browsing, `esc` clears it and returns. A group is searched on its name, each task's id and each
task's title, scored by a fuzzy subsequence match — a query below three characters only searches
the name and the task ids, so a short query stays on the name-shaped fields rather than matching
half the sentences in the project. The list is ranked best match first, and the pane title
counts what survived out of the total. A filter reaches every group whether or not `h` is hiding
it — a queued group it matches is listed with its `queued` tail and no checkbox, exactly as `h`
would show it. While the filter has focus, `q` is an ordinary character rather than the key that
quits the screen — which it otherwise does from browsing, the gate picker and a report on
screen — and the footer reads
`type to narrow   ↑↓ move   enter keep filter   esc clear   q is a letter here` in its place.

The two panes are sized to the terminal on every frame, and a pane with more rows than fit
scrolls to whatever is highlighted. `↑`/`↓` (or `j`/`k`) move the cursor by one group or task.
`space` selects a group — it is queued whole, because its tasks are one chain. `o`, live only
while the tasks pane has focus and a task is highlighted under it, opens that task's document in
an editor — `$VISUAL`, then `$EDITOR`, then a platform default, the same resolution the
dispatcher board's own `o` uses — in a pane of its own, without waiting for the editor to exit; a
queued group's row opens the copy in the queue directory. The tasks pane draws each task's id as
a header row,
then a labelled row for each fact it knows about the task — `Pipeline:`, `Depends on:`, `Gate:`
when one is set, and `Description:` — every value starting at the same column, in dependency
order: the task that waits on nothing first, then whatever waits on it. `Depends on:` reads `-`
when the list is empty. A right pane narrower than forty columns keeps every label and drops
its value to an indented line underneath, rather than dropping the label. A task's `touches`
globs are not drawn — they were the widest thing on the pane, and nobody picks what to queue by
glob.

The screen keeps itself current on its own: it polls between keystrokes, so a terminal resize
or a pending document edited on disk redraws without a keystroke, and a reload keeps the cursor
on the same group where it still exists.

`g` opens the highlighted task's pipeline and sets or clears a `gate_at` on the step picked, in
a panel drawn over the layout — in memory only, never written back to the document. `enter`
validates the selection and writes it, with nothing drawn in between. A validation failure — a
reserved key, an unknown `depends_on`, a cycle among the selection — is shown and leaves the
pending directory untouched. The screen no longer checks the selection for overlapping `touches`
globs itself: an overlap across two groups merges on two separate branches and is never one a
`depends_on` here could fix, so it is left for `spoolway queue conflicts` to report on demand,
ordered by nothing.

`p`, live only while the tasks pane has focus and a task is highlighted under it, opens a picker
for running that one task on several pipelines at once — the project's pipelines first, `space`
ticking one, and, once at least one is ticked, the union of their own steps below it, each with
its own `space` to tick as a step to skip. `enter` queues one arm per ticked pipeline, each under
a minted id — the task's own id with the lowest free number appended — with the ticked steps in
its own `skip:`; `esc` closes the picker with the selection exactly as it was. See
[Trials](planning.md#trials) for the full flow.

`r` swaps the left pane for the folder tree under `.spoolway/routines/` — the repeatable task
documents a project keeps in its own checkout rather than in the pending directory. It is a
screen of its own rather than an overlay, with a footer of its own:
`↑↓ move  → open  ← up  space select  enter queue now  r pending  q quit`.
`s`, from the pending screen, opens a panel that saves the
highlighted group's documents into `.spoolway/routines/<name>/`, the name typed and
edited the way the filter's query is — `enter` saves, `esc` cancels. Neither ever moves,
rewrites or deletes a file under `.spoolway/routines/`. See [Routines](planning.md#routines).

`enter` submits straight through: every selected group's documents go through the same `--from`
path `queue add` uses. Once every task file is written — and only then — the screen deletes those
documents from the pending directory, so a submission that fails validation removes nothing.
What is left on screen is the same report `queue add --from` prints for the same batch, one pair
of lines per task and then the branch they were based on. Under it the screen offers to start a
dispatcher over what was just queued — unless one is already running, in which case the offer
never appears: the report is shown instead, its last line naming the holder's pid and that it
will pick the new tasks up on its next pass. See [Queueing a plan](planning.md#queueing-a-plan)
for the full flow. With no terminal to drive — a redirected or piped stdin — the screen ends the
moment input runs out instead of blocking, which is what lets it be driven headlessly.

### `spoolway queue add`

Queue whole task documents — the only way a task reaches the queue. See [Queueing a
task](tasks.md#queueing-a-task) for the document's shape and the fields it may set.

| Flag | Meaning |
|---|---|
| `--from <PATH>` | A task document to queue: a file, a directory of `*.md` files (read in filename order — pointing it at `~/.spoolway/<project>/pending/` queues every document waiting there), or `-` for a `---`-separated stream on standard input. Repeatable — every document named across every `--from` is validated together and written all or none. Unlike the screen, this deletes nothing |

With no `--from` at all, this prints the project's default pipeline's skeleton document —
frontmatter plus the pipeline's own body skeleton — for a person to save, fill in, and hand
back through `--from`.

With `[issue_tracking]` configured, this opens a ticket for every document in the batch before
any of them is written — see [`open` — a fifth event, run by `queue
add` itself](configuration.md#open--a-fifth-event-run-by-queue-add-itself). With
`issue_tracking.key_in_names` on and the hook answering a `slug=`, it also prefixes each
task's `group:`, `branch:` and worktree directory with that slug.

### `spoolway queue list`

Whether a dispatcher is running, then a band per group naming it and its done counter, and
under each a row per task: TASK, PIPELINE, STEP, STATE and NEXT — the pipeline it resolves to,
the step it is on, its state, and what happens to it next. The same grouping the dispatch
board draws, once and as plain text, without the board's own spend columns or its per-group
total lines — for another terminal while a run is going, or for no run at all.

`--json` prints the dispatcher's pid, if one holds the lock, and one object per task instead —
the fields a script would actually want, rather than the board's own row shape. A question-held
row and a gate-held paused row both emit `"state": "paused"` and both carry the `[r]` resume
key — the difference is the route, not the key: the question-held row's `next` names the pane
to look at (``look at pane `<lane>``` — [r] resumes it), the gate-held row's names the resume
route (`→ handover — [r] resumes it`). Parked rows
retain the `parked_until` recheck clock for compatibility and add `parked_age`, the formatted
elapsed duration from `parked_at` (`null` for a legacy park without that timestamp).

### `spoolway queue show <task>`

Print one task file.

### `spoolway queue conflicts`

Report tasks whose globs can name the same file with no ordering between them. A pair both
marked `parallel: true` is reported too, as a mistake in the group rather than a missing
`depends_on`.

### `spoolway queue pause <task>`

Interrupt any live agent lane the task owns, then park it on `paused` — the same
interrupt-and-park `p` does on the board, but unconditional rather than skipped when there was
nothing live to interrupt. A running command step is refused rather than acted on, unless
`--force` says to kill it and pause anyway, throwing away its work in progress.

| Flag | Meaning |
|---|---|
| `--force` | Stop a running command step and pause anyway. Without it, a task running one is refused rather than killed on a script's say-so |

### `spoolway queue resume <task>`

What the board's `r` key does to one row, from a script: send it past a gate it finished, or
back onto the step a park or a block pulled it off of. Exactly `spoolway resume <task>` with
no other flags.

### `spoolway group list`

One line per group with tasks still open: its `group:` string, read verbatim — a bare name
and a path to a page are different groups, even when they share a file stem — how many tasks
are open, and which ones.

```
$ spoolway group list
auth                          2 open — auth-api, auth-session
pipeline-handover             1 open — pipeline-and-prompts
```

This reads the queue, not a store — spoolway keeps no notion of a group store at all, so this
is the only window it has left onto a group. `queue add` refuses a document with no
`group:`, so the only line missing here is a replay's, which is attributed to no group on
purpose; a task that reaches `done` is archived, so a group whose tasks have all finished has
no line here either.

**The one-line-per-group format is not to be reflowed.** It is how a person reads what a group
has left before landing its stack by hand — see [Closing a plan
out](planning.md#closing-a-plan-out). There is no `--json`: one grep-able line is the format.

### `spoolway dispatch`

Run the pipeline: a single pass, or a loop until the queue is empty.

A loop holds the terminal it was started in and draws the live board there between passes —
a row per task, grouped under the group it came from, with the step it is on, how full its
session has got, what the step has spent, where it goes next, and whatever waits on you
marked in amber where its run order puts it, each group's block closed by a total line of what
it has spent, over a ticker of recent transitions and a line per agent profile carrying its
worker slots. `↑`/`↓` move a cursor over the board's rows, and the rest of the keys split by case:
lowercase acts on the row the cursor sits on, uppercase acts on the whole run. `r` resumes
whichever paused or blocked row the cursor sits on, through the same code `spoolway resume`
runs, on a row whose every dependency is satisfied and whose own lane is not still mid-turn —
a paused row's NEXT column says so directly, ending `[r] resumes it`; a blocked row's NEXT
reads only the step a pass would carry it to, and never says whether the key does anything
here. `R` resumes every paused row whose own resume key is live. `p` interrupts and parks
the cursor's own task; `P` does the same over every live agent lane the run owns. `u` takes the
cursor's task off the queue and back to pending — behind a confirm panel naming the task and the
path — for a task nothing has run for yet, and does nothing on one that is running, paused,
blocked, or that a still-queued task names in its own `depends_on`; `U` does the same for every
task that has not started, with no `depends_on` refusal of its own, since whatever depends on one
of them has not started either. `ctrl-c` stops the run and leaves the last frame on screen.

| Flag | Meaning |
|---|---|
| `--interval <DURATION>` | Override the configured interval between passes, e.g. `5m` |
| `--dry-run` | Report what one pass would do without spawning anything or writing to task files, then exit. One pass, because a dry run archives nothing: a second would report the same untouched queue |
| `--plain` | Print a line per pass instead of drawing the board — for a pipe, a CI log, or a terminal that mangles the redraw. `--dry-run` prints lines regardless |
| `--unattended` | Stop for nobody: every block starts a lane on `blocked` instead of parking the task for a person. Every pipeline stages `blocked` — `Pipelines::assemble` materialises one from `[unattended]`'s `blocked_*` keys onto any pipeline that does not declare its own — so there is always a step to route to, staffed by whatever prompt `blocked` names, with `loop` applying as usual and nothing bounding how many times it round-trips. When that lane passes, `unattended.skip_blocked_lane` decides where the task lands: on by default, one step past where the block was hit; `false`, back on the step it blocked on. The launch ceiling that parks a task whose lane keeps dying backs off instead. `gate:` holds either way, and still parks a task on `paused` for a person. Overrides `unattended.enabled` for this run — see [Unattended runs](pipelines.md#unattended-runs). With neither `unattended.max_output_tokens` nor `unattended.max_cost_usd` set, this run has no ceiling at all |
| `--attended` | Park blocked tasks in front of a person for this run, whatever `unattended.enabled` says |
| `--force` | Start anyway, past the restart guard — see below |

Run while another dispatcher already holds the lock for this repo, and it draws the same
board read-only instead — headed `watching dispatcher` with that other process's pid, rather
than `dispatcher running` with its own. It never takes the lock, starts a lane, or writes a
task file; `ctrl-c` ends only the watcher. `--plain` in that situation prints the table once
and exits.

A caller restarting `dispatch` in a tight loop against a repo that cannot run at all is
refused rather than let to spin forever: four starts in a row that could not run, inside 30
seconds, get the fifth turned away naming the count, the last reason and the way past it.

```
$ spoolway dispatch
spoolway: refusing to start: 4 starts in a row could not run at all, the last
because a dispatcher is already running for this repo (pid 2688669). Fix the
reason above, or run `spoolway dispatch --force` to start anyway.
```

Only a start that could not run at all counts — today, the lock already held by another
dispatcher. An empty queue is never counted: a repo with nothing to do is not a storm. A start
that actually runs, or one made with `--force`, clears the count. See [Restarting into a repo
that cannot run](dispatcher.md#restarting-into-a-repo-that-cannot-run).

`spoolway dispatch` exits 0 on a run that dispatched and stopped on its own, 3 on an empty
queue with no job enabled, 4 when another dispatcher already holds the lock, 5 when the
restart guard refuses a start, and 1 on any other error. With a job enabled the run stays
resident on an empty queue rather than exiting 3 — see [Jobs](jobs.md).

Where every task is sitting, from another terminal or with no run going: `spoolway queue
list`.

### `spoolway issue show <reference>`

Read one issue out of this project's own tracker, through the `[issue_tracking]` hook's
`fetch` event, and print it as one JSON object on stdout: `ref`, `url`, `title`, `state`,
`labels`, `body` and `comments`, in that order. Writes nothing — the same synchronous run
`queue add` gives the hook's own `open` event, blocking until the hook is done rather than
leaving anything to a later pass.

`reference` is the issue's own reference, in whatever shape the tracker and its hook script
expect — a bare number on GitHub, a key like `PROJ-123` on Jira. Never parsed by spoolway
itself; handed to the hook exactly as typed, as `SPOOLWAY_REF`.

Refused, by name, when no hook is configured at all, or when the configured hook's own script
has no `fetch` branch — `spoolway doctor` reports the second of those too, for every install
whose hook predates this event. See [`fetch` — a sixth event, run by `spoolway issue
show`](configuration.md#fetch--a-sixth-event-run-by-spoolway-issue-show).

### `spoolway jobs`

Open the jobs screen — the only thing that writes a cron job. The left pane lists every job
both stores hold and the right pane shows the highlighted one in full. With no job anywhere,
the screen names both store paths instead.

`n` writes a new job by walking three panels: the routines browser, where `space` ticks a
folder and `enter` over that ticked folder picks it (or `space` picks one document); the
schedule field, which states the expression back in words and shows its next three firings as
it is typed; and the pipeline picker, which narrows over every pipeline the repo defines.
`esc` at any panel leaves nothing written.

Over the list, `e` edits the highlighted job through those same three panels, `space` pauses
and resumes it, `x` deletes it after confirming, `r` fires it now, and `q` quits. A new job is
written to the user store and takes its name from the leaf of its routine; `e` keeps the name
and store the job already had.

See [Jobs](jobs.md) for the stores, the grammar, and how a pass fires one.

### `spoolway jobs list`

Every job across both stores, as a table: `NAME`, `SCOPE`, `SCHEDULE`, `PIPELINE`, `NEXT`,
`LAST`. A count line and a per-store count follow it.

`NEXT` reads `paused` for a disabled job, `bad expr` for one whose expression will not parse,
`never` for one that parses but never comes round, and otherwise a relative time within a
day, a weekday and time within a week, or a full date beyond that. `LAST` is `-` until the
job has fired, then `ok, <time> ago`.

`--json` prints one object per job instead: `name`, `scope`, `schedule`, `pipeline`,
`routine`, `enabled`, `source`, `next` (local RFC 3339, or `null`) and `last_fired` (epoch
seconds, or `null`). A `schedule_error` key is present only on a row whose expression will
not parse, so a script can test for its presence.

### `spoolway jobs run <name>`

Fire one job now, ignoring its schedule — the way to test a job you have just written. The
routine is queued exactly as a scheduled firing would queue it, and the run is recorded, so
`LAST` updates and a still-running previous copy is not stacked on. The job's next scheduled
firing is unaffected. Refused from inside a lane.

### `spoolway eval`

Compare versions of the pipeline and prompts by what they cost to run. A version is a
fingerprint of the tracked `.spoolway/` configuration, recorded on every lane. One block per
pipeline, its versions inside it newest first.

Bare, with no flag at all, no `--json`, and stdout a real terminal, this opens a screen instead:
four views — `pipelines`, `steps`, `runs`, `skills` — cycled with `tab`, a cursor over the rows
with `↑↓`, a filter panel (`f`) over every flag below, `e` to export the rows on screen to
`.spoolway/evals/eval-<view>-YYYY-MM-DD-HHMMSS.csv` — with a `-2`, `-3` and so on appended
when that name is already taken — `r` to refresh, `q` to quit. Reads keys the same way
`spoolway queue`'s own screen does, and ends the moment a piped stdin runs out rather than
blocking. Any flag, `--json` included, or stdout not a terminal, takes the printing path below
unchanged — so redirecting bare `spoolway eval` to a file never writes the screen's own escape
sequences into it. See [The screen](eval.md#the-screen).

`--by` is a deprecated alias for `spoolway spend`, kept so a script or skill written before
the split keeps working: it reads the ledger as a spend table instead of comparing versions,
prints a note to stderr saying where the table moved to, and otherwise behaves exactly like
`spoolway spend` — see below.

| Flag | Meaning |
|---|---|
| `--pipeline <NAME>` | One pipeline's block only |
| `--step <STEP>` | One step's rows only |
| `--since <WHEN>` | Start of the window: a duration ago (`24h`, `7d`, `2d6h`), a local date (`2026-08-01`), or a whole month (`2026-08`) |
| `--until <WHEN>` | End of the window, same forms. A date includes the whole of that day, a month the whole of that month |
| `--month <YYYY-MM>` | One calendar month, local time. Shorthand for the `--since`/`--until` pair that bounds it. Requires `--by`; refused together with `--since`/`--until` |
| `--limit <N>` | How many versions to show per pipeline block. Ten by default |
| `--all` | Every project spoolway knows about, not just this one. Refused together with `--project` |
| `--project <NAME>` | One named project — its directory name, or its path |
| `--runs` | One row per run instead of a grouped summary: task, when, version, pipeline, lanes, pass, blocks, ctx peak, out, cost, time |
| `--task <ID>` | `--runs` only: one task's runs. A trial's arms are separate tasks (`solo-1`, `solo-2`, …), so this is not how to see a trial side by side — that is `--runs --trial <id>` |
| `--group <GROUP>` | `--runs` only: only runs whose task carries this `group:` |
| `--trial <ID>` | `--runs` only: one trial's arms, side by side on pass rate, cost and time, plus a delta line per arm against the first |
| `--csv` | The same rows this would print, as CSV. Refused together with the global `--json` |
| `--by [<task\|group\|step\|model\|project\|month\|skill\|lane>]` | Deprecated: see `spoolway spend --help` |

See [Comparing versions](eval.md) and [Cost accounting](cost.md).

### `spoolway spend [<task\|group\|step\|model\|project\|month\|skill\|lane>]`

Read the lane ledger back out as a spend summary: what the pipeline has spent, grouped by
task, group, step, model, project, month or skill, or one row per lane with `lane`. Bare,
with no cut named, this still picks one: `step`, or `project` when more than one project is
in scope. Prints a skills block below the pipeline's table where the ledger holds skill
spend, and sweeps this project's interactive sessions onto the ledger before it prints — see
[Skill sessions](cost.md#skill-sessions).

| Flag | Meaning |
|---|---|
| `--since <WHEN>` | Start of the window: a duration ago (`24h`, `7d`, `2d6h`), a local date (`2026-08-01`), or a whole month (`2026-08`) |
| `--until <WHEN>` | End of the window, same forms. A date includes the whole of that day, a month the whole of that month |
| `--month <YYYY-MM>` | One calendar month, local time. Shorthand for the `--since`/`--until` pair that bounds it. Refused together with `--since`/`--until` |
| `--all` | Every project spoolway knows about, not just this one. Refused together with `--project` |
| `--project <NAME>` | One named project — its directory name, or its path |
| `--csv` | The same rows this would print, as CSV. Refused together with the global `--json` |

Under the global `--json`, this does not honour the cut at all: it dumps the matching ledger
entries themselves, raw and ungrouped. See [Cost accounting](cost.md).

## When something needs you

### `spoolway lane [<lane>]`

Read what a lane has been doing, answer it, or open its session — one verb, and a flag says
which of the three you want. Omit the lane entirely to list the lanes there are.

Bare, or given only a lane name, this reads it. A live lane is read from its pane; under a
multiplexer, or its log under headless, which outlives the lane. A lane that has already
settled and has no pane left — an unattended run closes one the moment its task blocks,
before a staffed `blocked` step's lane can start on it — falls back to its transcript,
resolved the same way the usage ledger resolves one: by the lane's recorded agent kind and
session id.

`-m` answers a lane that ended its turn on a question. Under a multiplexer you would type
into its pane and never need this; headless there is no pane, so this is how the answer gets
in — the lane's session is resumed with your words and it carries on from where it asked. You
rarely need it for a lane that simply forgot to report: the dispatcher already sends that one
the report contract again on its own, through the same call this uses, as often as it takes —
see [A lane that settles without reporting](dispatcher.md#a-lane-that-settles-without-reporting).
`-m` is for the words only you can supply, a real question a lane is waiting on.

`--attach` opens a lane's session in this terminal, with its whole conversation — the
interactive counterpart of `-m`. Under a multiplexer the lane's pane already holds the
session, and this says where it is.

| Flag | Default | Meaning |
|---|---|---|
| `-n`, `--lines <N>` | `100` | How many lines from the end |
| `-m`, `--message <WORDS>` | | Answer a lane that ended its turn on a question |
| `--attach` | | Open its session in this terminal, ready to type into |

`-n` and `-m`/`--attach` are mutually exclusive with each other, refused at parse time rather
than run and ignored. `--attach` also needs a lane named, since there is no session to open
without one.

`--json` reads a lane's output the same way, printed as a JSON object naming the lane and its
text; with no lane named, it lists the lanes there are instead. Ignored, the same as it is
alongside any other subcommand, next to `-m` or `--attach`.

### `spoolway resume <task>`

Carry a stopped task on — past a gate it is paused at, or back onto the step that blocked it.
Which one applies is read off the task itself, so you never have to say which kind of stop it
is on.

| Flag | Meaning |
|---|---|
| `--stage <STEP>` | Resume at a named step instead, overriding the route |
| `--reject` | Against a paused task, send it back round instead, by the step's `on_fail` route — or onto `blocked` when the step declares none |
| `-m`, `--message <TEXT>` | Note recorded in the task's status log — and, with `--reject`, written into `## Handoff` for the lane that answers it |

On a `blocked` task, this resumes where it stopped: that continues the lane's own session
rather than opening a new one on the same step, and hands back the loop budgets out of that
step. An [unattended run](pipelines.md#unattended-runs) never waits for this — a task that
blocks gets a lane started on `blocked`, and that lane's own pass routes it onward under
`unattended.skip_blocked_lane`. This command is the by-hand path for a run that has you in it.

On a `paused` task — one that finished a step declaring `gate: true`, one held by its own
`gate_at:`, or one a staffed `blocked` step's own `--pause` (or a `--fail` or `--block` read the
same way) parked there — this lets it past. For the first two, a paused task was never blocked,
and resuming it *at* its gated step would run that step's work a second time, so where it goes is
read out of the pipeline as it stands now, off that step's own `on_pass` — a pipeline edited
while the task waited routes it the way the file says today. For the third, the task genuinely
was blocked, and it resumes to the same destination a pass from `blocked` would have reached —
carrying it past the step it blocked on, not back onto it. `--reject` sends the first two back
round by `on_fail` instead — or onto `blocked`, same as any other fail, when the gated step
declares no `on_fail` of its own; `spoolway pipeline check` warns about a gate shaped that way.
Nothing here runs, rebuilds or checks anything — the lane already
did its work and reported, and all that is left is the routing decision the pass was not allowed
to take. See [Gates](pipelines.md#gates).

`--stage` reroutes the task on either kind of stop, and the step it names starts fresh — this
is also how you let a paused task past its gate to somewhere other than the gate's own route,
since naming a step by hand is you overriding the route.

## Shaping the project

### `spoolway pipeline show`

Print your graphs as a readable flow, with every default resolved — and, under each agent
step, the tokens one lane of that profile actually gets.

Each pipeline's own `description:` is printed under its header line, indented. That is the
sentence a person or a skill reads to choose between pipelines, so this command is where the
choice is made from. A step that only runs on the last task of a chain is marked
`last-of-chain`.

```
$ spoolway pipeline show
pipeline `impl`  (default)  entry: implement
    One unit of feature work, start to finish: implement against the acceptance criteria, review the diff, carry the change into the end-to-end suites, then document and hand over.

  implement  agent     agent=claude prompt=implementer model=claude-sonnet-5 session
             Write the code to satisfy the task's acceptance criteria.
             pass -> review   fail -> blocked

  review     agent     agent=claude prompt=reviewer model=claude-opus-5 session loop=implement:2 exit=blocked
             Check the diff against the acceptance criteria and project standards.
             pass -> e2e   fail -> implement

  suite      command   waits timeout=45m last-of-chain
             The end-to-end suites, on the last task of the chain.
             run: SPOOLWAY="$PWD/target/release/spoolway" scripts/e2e/run.sh --tier pr
             pass -> document   fail -> e2e
```

That block is one pipeline, with its middle steps left out. The real command prints every
pipeline in the project, every step of each, in order.

In a linked worktree the [`checkout:` line](#the-checkout-line) comes first, naming the checkout these
definitions were read from. In the main checkout nothing extra is printed.

### `spoolway pipeline check`

Validate the pipeline definitions and their agent references against the config they will run
with. Includes the prompt checks. Also validates the two shipped pipelines,
`assets/pipelines/default.yml` and `assets/pipelines/bugfix.yml`, checking their `run:` commands
against this same config — even when a project's own `.spoolway/pipelines/` overrides them, so a
shipped `run:` line that would only work inside this repository is caught before it ships.

```
$ spoolway pipeline check
checkout: ~/.spoolway/worktrees/checkout-line (task/checkout-line)
3 pipeline(s) valid: ["bugfix", "default", "local"], agents ["claude", "pi"]
```

The [`checkout:` line](#the-checkout-line) is the whole of the addition, and is suppressed in the main
checkout.

A missing or overlong `description:` is a warning, not a problem. The check still passes, and
the warning prints under the verdict beside the gate and prompt findings:

```
  warning: pipeline `local` has no `description:` — nothing reading `pipeline list` can tell what it is for
```

The overlong warning fires past 400 characters, counted in characters rather than bytes. A
description is read to choose between pipelines, so a few sentences is the size it wants.

A pipeline file that will not parse does not abort the command you ran to find it. The load
failure is printed as the first problem and the two shipped pipelines' `run:` commands are
still checked.

### `spoolway pipeline contract`

Print the pipeline format: every key a pipeline and a step may carry, one sentence each on how
to fill it, the rules refused at load, this project's own agent profiles and prompts, and an
annotated blank pipeline to copy from. Nothing here is documentation kept in sync by hand — it
is read off the same structs and the same validation `pipeline check` runs, so it cannot drift
from what the loader actually enforces.

Copy the blank to `.spoolway/pipelines/<name>.yml`, delete what the flow has no use for, and
run `spoolway pipeline check`.

Moving a pipeline between projects needs no command: copy its file from
`.spoolway/pipelines/` together with the prompts its steps name, and `spoolway pipeline
check` on the other side says what is still missing.

### `spoolway pipeline list`

Print every pipeline's name, marking the default, with its `description:` indented underneath:

```
$ spoolway pipeline list
bugfix
    Reproduce the bug first with a failing test, fix it, then run the same reproduction again to prove it is gone. For a defect with a known symptom and a way to trigger it, never for new work.

impl (default)
    One unit of feature work, start to finish: implement against the acceptance criteria, review the diff, carry the change into the end-to-end suites, then document and hand over.
```

A pipeline with no `description:` prints its name alone. `spoolway pipeline check` warns about
that pipeline.

Standing in a linked worktree, the [`checkout:` line](#the-checkout-line) comes first:

```
$ spoolway pipeline list
checkout: ~/.spoolway/worktrees/checkout-line (task/checkout-line)
bugfix
    Reproduce the bug first with a failing test, fix it, then run the same reproduction again to prove it is gone. For a defect with a known symptom and a way to trigger it, never for new work.

impl (default)
    One unit of feature work, start to finish: implement against the acceptance criteria, review the diff, carry the change into the end-to-end suites, then document and hand over.
```

This is the list a breakdown is routed from. A subject is matched to a pipeline by reading
these descriptions, before that subject is sized. `pipeline show` prints the same descriptions
alongside every step and model, and is what a skill reads when it also needs the models.

### `spoolway pipeline list --json`

The same list as a JSON object: the default pipeline's name, then one entry per pipeline
carrying its name, whether it is the default, and its description.

```
$ spoolway pipeline list --json
{
  "default": "impl",
  "pipelines": [
    {
      "name": "bugfix",
      "default": false,
      "description": "Reproduce the bug first with a failing test, fix it, then run the same reproduction again to prove it is gone. For a defect with a known symptom and a way to trigger it, never for new work."
    },
    {
      "name": "impl",
      "default": true,
      "description": "One unit of feature work, start to finish: implement against the acceptance criteria, review the diff, carry the change into the end-to-end suites, then document and hand over."
    }
  ]
}
```

A pipeline with no `description:` carries `"description": null`. In a linked worktree the
[`checkout:` line](#the-checkout-line) arrives first as its own line of JSON, before this
object.

### `spoolway pipeline gen [--plan <path>]`

Open a fresh agent session, in a pane of the current checkout, to write a new pipeline. Nothing
is written by this command itself — it starts the `[pipeline_gen]` profile and prompts the
`spoolway-pipeline` skill, which carries the whole generation procedure from there:

```
$ spoolway pipeline gen --plan ~/.spoolway/myproject/plans/my-plan.html

agent         claude · claude-opus-5 · effort high
plan          ~/.spoolway/myproject/plans/my-plan.html
preferences   auto = false · loop_default = 1 · local_models = true

opened a pane on this checkout
prompted `spoolway-pipeline`

Nothing is written yet. Answer it in that pane.
```

`--plan` is where the pipeline is being generated for, named however its producer names it —
an issue URL, a page path, a ticket. It is never parsed; without it, the session opens with
nothing named. Refused, naming the key and what to do about it, when
`pipeline_gen.pipeline_model` is blank, when `pipeline_gen.pipeline_agent` names no
`[agents.*]` profile, or when `dispatch.backend` is `"headless"` — this command needs a real
pane to open a session in.

See [`[pipeline_gen]`](configuration.md#pipeline_gen--generating-a-pipeline) for the block this
reads.

### `spoolway prompt contract`

Print the contract a prompt is written against, rendered from this project's own pipeline.

| Flag | Meaning |
|---|---|
| `--step <STEP>` | Which step to render for. Defaults to the first agent step of the default pipeline |
| `--pipeline <PIPELINE>` | Which pipeline the step belongs to. Defaults to the file's `default:` |
| `--task <TASK>` | Render both halves against a real queued task instead of a sample one — which is how to see what a lane that misbehaved was handed |

### `spoolway prompt list`

Every prompt, and which steps run it. In a linked worktree the [`checkout:` line](#the-checkout-line)
comes first; in the main checkout it is suppressed.

### `spoolway prompt show <name>`

Print one prompt file.

### `spoolway prompt check [<name>]`

Read prompts against the steps that run them. Also part of `pipeline check`. In a linked
worktree the [`checkout:` line](#the-checkout-line) comes first; in the main checkout it is suppressed.

### `spoolway agent list`

Every kind spoolway knows, with its launch state, its accounting state, and where its binary
resolves on `PATH`. `--json` prints a JSON array instead, one object per kind.

### `spoolway agent verify <kind>`

Check one kind against its row, clause by clause: binary, launch row, args render, env
render, how its session is pinned, resume rewrite, accounting row or its absence, transcript
directory. The exit code follows the launch half alone — a kind that runs unmetered is
reported at length and still exits 0, because an unmetered kind is a legal state rather than
a fault. See [Agents and
models](agents.md#accounting-is-optional-and-its-absence-is-a-cost-not-a-refusal).

| Flag | Meaning |
|---|---|
| `--live` | Run one real turn, then a resumed one, and read back what only turns can settle: that they ran, that the prompt reached the model, that the session landed in the home spoolway made, that the resume spelling still continued the first turn's session, and — on a metered kind — tokens, last-turn size, mtime and running totals. Without it nothing is started and nothing is spent |
| `--model <MODEL>` | The model those turns run. Defaults to one this project's pipelines already name for this kind; spoolway names none of its own |

`--json` prints the launch half's clauses as a JSON object instead of the printed report, one
entry per clause plus a failure count; it does not yet cover `--live`, so the two are refused
together.

An unmetered kind still runs its turn under `--live`: there are no readings to take, and the
launch, the prompt and the session pinning are exactly what nothing else can check.

Each `--live` turn is bounded. A turn still running after 180 seconds has hung, so spoolway
kills it and reports the clause as failed rather than waiting forever — `verify --live` is
often run from CI, where a hang is a stuck job with nobody to interrupt it. The scratch tree
`--live` works in is removed when the command ends, whichever way it ends. So is the
per-session agent home it creates for a kind that pins by home — with one carve-out.

The carve-out is a kind whose quota reading is read out of one of those homes, which today
means `codex`. That home is kept, so the rollout the live turn just wrote stays on disk and a
stale reading can be refreshed by hand with one `spoolway agent verify codex --live`. Only the
newest such home survives: on the way out the command takes back every older home its own
earlier runs left, so what is kept never grows past one per kind. A real lane's home in the
same directory is never touched.

`agent list` and `agent verify` both run against the whole adapter table rather than the
profiles this project configures — that is `doctor`'s scope, and the kind you want to ask
about is usually the one no profile names yet. Neither needs a project, so both answer the
same standing anywhere.

**There is no `spoolway plan` command at all.** Not `plan new`, `plan pages`, `plan path`,
`plan close`, `plan list` or `plan check`. spoolway does not know what a plan is: writing a plan
page is `spoolway-plan`'s, not the binary's — see [Planning](planning.md) — and what the binary
reads is task documents, which `queue add --from` and the queue screen validate the same way
whoever wrote them. A plan closes out through the steps every task already runs — see [Closing a
plan out](planning.md#closing-a-plan-out), and `spoolway group list` answers what `plan list`
used to.

To check a breakdown before queueing it, point `spoolway task contract --from` at the pending
directory: it runs exactly the validation `queue add --from` runs and stops short of the save.

### `spoolway task contract`

Print or validate the task-document contract — the same rules `parse_submission` and
`check_dependencies_set` enforce, reached through the same `validate_batch` `queue add --from`
itself runs, so neither mode can say something the enforcement does not.

Bare, prints the whole contract as JSON on stdout and nothing else: this project's default
pipeline; where a finished document is written, what to name it there, and the commands that
check it and send it, under `output`; the document's required, optional, refused and ignored
keys, plus what happens to a key named in none of them; one sentence per settable key on how
to fill it; one entry per pipeline giving its longest agent step, its id budget, the step ids
`gate_at` accepts and the body skeleton a task on it is written from; and the rules that only
hold across a set. A producer with no access to `docs/tasks.md` can write a queueable document
from this alone.

`output.dir` is this machine's own pending directory, already resolved rather than given as a
pattern to expand, so a producer handed nothing but this JSON still writes where `spoolway
queue` reads.

| Flag | Meaning |
|---|---|
| `--from <PATH>` | A document to check, in any shape `queue add --from` reads. Repeatable, checked together as one set |

`--from` runs the same validation `queue add --from` would, and writes nothing either way —
neither a passing check nor a failing one touches anything under `.spoolway/`. A refusal prints
the same message `queue add --from` gives for the same document, and exits non-zero; nothing is
printed for one that passes beyond a short report of what was checked.

### `spoolway config show` / `list` / `path` / `get <key>` / `set <key> <value>` / `edit`

Read or write single values non-interactively, e.g. `spoolway config set
agents.pi.concurrency 4`. `edit` opens the file in `$EDITOR` and re-validates it on save —
the file's own comments carry every explanation. `list` prints every scalar setting as
`key = value`, one per line, in key order — the same keys `get` and `set` resolve, including
the ones the file omits while they hold their default. A `[models]` glob nobody has named yet
is settable but not listed. `--json` emits `[{"key","value"}, …]`.

`show`, `list`, `path`, `get` and `edit` answer for the checkout a command was run in; standing in a
linked worktree, they read and validate that worktree's own `config.toml`. `show`, `list`, `path` and
`get` say so, printing the [`checkout:` line](#the-checkout-line) before their output whenever the
checkout is not the project. `edit` opens a file rather than printing one, so it prints no such
line. `set` writes only the
project's copy, and refuses inside a linked worktree rather than writing somewhere the dispatcher
never reads — it prints the `-C <project>` invocation that would land in the right file. See
[Configuration](configuration.md).

### `spoolway models`

Every model an agent step of this project's pipelines names, resolved: window, per-1M rates,
its own `SLOTS` and `EXCL`, and which table answered — this project's own `[models]`, the
machine-wide refreshed table at `~/.spoolway/model-prices.json`, the built-in one, or
`unknown` where none has heard of it. `spoolway doctor` reports the same gap in passing; this
is where to see it in full.

`refresh` fetches litellm's current price map through `curl -fsSL --max-time 30`, distils it to
the same six numbers, and atomically replaces the table it writes. By default it replaces the
machine-wide table at `~/.spoolway/model-prices.json` and prints what it fetched, how many rows
it kept and dropped, and how the table changed; `--vendor` instead replaces this checkout's
`assets/model-prices.json` and reports only the path written. The source URL honours the
`SPOOLWAY_MODEL_PRICES_URL` override, the same way `SPOOLWAY_GH` overrides the `gh` binary, so a
suite can point it at a local fixture. A missing curl, a non-zero curl exit, or a response that
does not parse each exits non-zero and leaves any existing table untouched.

See [Pricing](cost.md#pricing).

## Setting up

### `spoolway init`

Scaffold `.spoolway/` in a repository: config, both pipelines, the six prompts, the
skeletons, the four issue-tracker hook scripts — and the skills, in the convention of
whichever coding agent you plan in. It also claims this project's own name under
`~/.spoolway/`, where the queue, the archive and everything else spoolway writes while it
runs will live; see [Runtime state](configuration.md#runtime-state).

At a terminal it asks three things, each of which is otherwise a file to go and edit
afterwards: the coding agent you plan in, whose convention the skills go in and which
becomes the fresh project's one agent profile, the issue tracker to name in
`[issue_tracking]` and the project its tickets file into. Answer any of them with the flag
instead and it is not asked. With no terminal — a script, CI, a pipe — nothing is asked
and nothing blocks: the defaults are taken silently, `claude` for the agent and `none`
among them for the tracker. Fixed choices start on the default and use ↑/↓ to move; Enter or
Space accepts the highlighted answer.

The agent you plan in is the single source of a fresh project's agent identity: a fresh
scaffold keeps exactly that one profile, points `pipeline_gen.pipeline_agent` and
`unattended.blocked_agent` at it, and specializes every bundled pipeline's agent steps to
it. Model and effort are left blank on every step, because spoolway cannot choose either
for you and a fresh scaffold writes the blank down explicitly rather than leaving the
placeholder standing. Add or change profiles with `spoolway config set`.

| Flag | Meaning |
|---|---|
| `--provider <claude\|codex>` | The coding agent you plan in, whose convention the skills are installed under and which becomes the fresh project's sole agent profile. Defaults to `claude` |
| `--tracker <github\|jira\|none>` | The tracker `[issue_tracking]` names. The prompt's menu notes whether the tool each one calls — `gh` or `acli` — is on `PATH`. Defaults to `none` |
| `--project-key <KEY>` | The project the chosen tracker's tickets open into: `owner/repo` on github, a project key on jira. Ignored when `--tracker` is `none` or unset |
| `--force` | Overwrite existing config, pipeline and prompt files |
| `--take-over` | Claim this project's name even though the machine's record of it still holds an archive or queued tasks, when the checkout that name was registered to no longer exists. Refused without this flag — nothing is deleted either way, but a state that old is not yours to walk into by accident |

Run again in a project that already has a config, `init` installs skills and changes
nothing else — `--tracker` and `--project-key` are reported as not applied rather than
silently dropped, since rewriting a config a project has been running on is not what a
second `init` is for. Every hook script is still written whichever tracker a project
answers, or none at all, so switching trackers afterwards is a
`spoolway config set issue_tracking.hook` away rather than a second `init`.

A successful fresh run prints “Skills installed successfully.” and “Project initialized
successfully.”, plus one concise sentence telling you to set model and effort on every agent
step in `.spoolway/pipelines/*.yml` before dispatching, apart from actionable warnings. A
repeat run that only adds skills omits the project line and the sentence. `spoolway install
<provider>` uses the same concise skills message.

See [Installation and setup](installation.md#scaffolding-a-project).

### `spoolway install <provider>`

Write the five pipeline skills for a coding agent. `init` runs this for you; this is how a
project adds a second provider, or takes newer skills without touching anything else.

Every provider converged on the same layout — one directory per skill, holding a
`SKILL.md` — so the only difference is where that directory goes:

| Provider | Skills go in |
|---|---|
| `claude` | `.claude/skills/` |
| `codex` | `.agents/skills/` |
| `pi` | `.pi/skills/` — loaded only once the project is trusted, so answer pi's trust prompt or start it with `--approve` |

`gemini` is not here. It has no adapter row either: nothing about it has been settled
against the binary, and a guessed row is worse than no row.

| Flag | Meaning |
|---|---|
| `--force` | Overwrite files that already exist |

### `spoolway update`

Install the latest published release, then take what it writes without touching what you
wrote. Prints the paths it brought forward and one line saying what it left alone. After a
successful npm self-update at a terminal, the new binary also prints the exact version range,
a compact release digest, any migration instructions, and a link to the full notes. Dry runs,
file-only updates, unmanaged installs, dispatcher-blocked upgrades, and captured output do not
print that digest.

The binary half only happens where npm installed spoolway, because npm is what upgrades it —
anything else is told which release is out and left alone. A running dispatcher stops the
install outright: replacing the executable underneath one kills the run mid-pass. Neither
case stops the files being brought forward.

| Flag | Meaning |
|---|---|
| `--dry-run` | Print what would change and write nothing. Installs nothing either |
| `--replace <PATH>` | Replace one whole file with the shipped version, saving yours beside it. Repeat for each |

### `spoolway whats-new`

Read the release record embedded in the installed binary. With no flag it prints that binary's
full release section: theme, overview, three to five highlights, any migration instructions,
other recorded sections, and the GitHub release URL. It does not discover a project or contact
GitHub, so it works from any directory and remains available offline.

| Flag | Meaning |
|---|---|
| `--since <VERSION>` | Print every embedded release later than `X.Y.Z`, oldest first. An empty range is reported explicitly |

### `spoolway doctor`

Check that everything the configured pipeline needs is actually present. The one command
that still runs when the config file does not parse, or a pipeline file does not: the load
failure becomes one failed check and the rest run. Among its checks: whether `gh` is on
PATH and authenticated — what `spoolway stack` needs for everything past the push.

It prints the [`checkout:` line](#the-checkout-line) once when the checkout is not the
project, even though it reports on both sides.

Its config findings answer for the checkout, the same file `config get` and friends read —
run from a linked worktree, they are checked against that worktree's own copy, not the
project's. When the project's own `config.toml` fails to parse, that is reported as a
finding of its own, naming the project's file, so the dispatcher's copy is never checked
silently against defaults.

By default it reports only what needs your attention: any failing check, any note such as
a file that has fallen behind `spoolway update`, and a closing line such as `28 checks
passed. Everything checks out.` A failing run instead prints each `FAIL` line, then `N of M
checks passed.`, and exits non-zero.

| Flag | Meaning |
|---|---|
| `-v`, `--verbose` | Print every check, in the order it runs, including the ones that passed |

Under `-v`, a prompt referenced by several pipeline steps prints as a single row —
``prompt `archivist``` — rather than once per step; a missing prompt file still fails,
naming every step that referenced it.

`--json` prints the same findings as a JSON object instead, a `kind` tag per row and `-v`
narrowing which rows are included the same way it does in text. In a linked worktree this
prints two separate JSON documents on stdout, the checkout note first and the report second —
the same shape `config show --json` and `config get --json` use.

## Called by lanes, not by you

These are called by prompts. You rarely run them yourself.

### `spoolway report [<task>]`

Report a step's outcome. The task defaults to the one in the lane's environment. Naming a
task other than the one this lane was started for is refused: a lane may only report on its
own task.

| Flag | Meaning |
|---|---|
| `--pass` | The step succeeded; advance along `on_pass` |
| `--fail` | The step failed; route along `on_fail` |
| `--block` | Something outside this step's control is in the way; escalate |
| `--pause` | Nothing short of a person can clear this. Only means anything on `blocked`, refused on every other step; parks the task on `paused` with the same destination a pass would have reached |
| `-m`, `--message <TEXT>` | One line on what happened, recorded in the status log |
| `--handoff <TEXT>` | One thing the next step should know, that does not belong in `-m`. Repeat for each. Written into the task's `## Handoff` as `` - `<step>` — <text> ``, whatever the outcome, in the same save that routes the task |

### `spoolway stack [<task>]`

Hand a task's change over with git and `gh` — no rebase, and a model only when
`[stack.summary]` names one. Run in a task's worktree, usually as a pipeline's `handover` step's
`run:` line — see [`spoolway stack` hands the change over](pipelines.md#spoolway-stack-hands-the-change-over).
The task defaults to the one in the environment, exactly as `report` does. In order: with
`[stack.summary]`'s `agent` and `model` both set, runs that prompt on the task file for one
turn and takes its printed output as the whole pull request body — a blank half of the pair, or
a missing or empty `.spoolway/templates/pull-request.md`, refuses the command here, before
anything below runs; commits what is uncommitted, squashes the branch to one commit named after
the task's `title:` verbatim — written `feat(queue): add a --dry-run flag`, and no task id is
prefixed onto it. The squash is built with `git commit-tree`, so no `commit-msg` or
`pre-commit` hook can reject it and leave the branch half-collapsed; for the same reason a
project that signs its commits gets an unsigned squash here. Then it pushes with
`--force-with-lease`, opens the pull request against the branch
the worktree was cut from (or reuses the one already open), and registers the GitHub stack. A
GitHub stack is a single linear chain, so when this task's dependency's pull request already has
a different pull request stacked above it — two tasks that `depends_on` the same dependency are
siblings, and only the first to reach `handover` can hold that slot — the `stack` line says so
by name (`none — \`<task>\` is a sibling of #<n> on #<m>, outside stack #<k>`) instead of
failing. Exits 0 on success, non-zero on any git or `gh` failure — a push refused because the
remote moved says so distinctly from any other failure, and an empty three-dot diff against the cut
point refuses to open a pull request at all.
