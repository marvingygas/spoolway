---
domain: eval
covers: ["src/eval.rs", "src/screen.rs"]
---

# Comparing pipelines

`spoolway eval` shows what each pipeline costs to run, by pass rate, cost and time. It reads
the ledger described in [Cost accounting](cost.md). There is no second store.

```
spoolway eval
```
```
PIPELINE  TASKS  PASS  BLOCKS  TURNS  CTX AVG       IN  IN USD      OUT  OUT USD  CACHE R  CACHE R USD  CACHE W  CACHE W USD       USD      TIME
impl         28   79%       1   1640      13%    11.6k    0.01    2.95M    37.19  952.76M       190.55   12.15M        66.00    293.86   16h 18m
impl_ui      11   84%       1    612      13%     4.7k    0.01    1.34M    20.10  267.48M        53.50    5.74M        40.26    113.87    5h 55m
total        39             2   2252            16.3k    0.02    4.29M    57.29    1.22B       244.05   17.89M       106.26    407.73   22h 13m
```

The turn, token, cost and time columns show each row's totals. `spoolway eval --per-task` prints
each of them divided by the row's tasks. See [Totals and per task](#totals-and-per-task).

## The screen

`spoolway eval` always prints the lanes table. Bare `spoolway`'s eval tab draws an interactive
screen over the same rows.

<img src="screenshots/readme/eval.png" alt="the eval screen">

The screen holds three tables. The lanes table groups dispatched lanes. The directory table
groups sessions run by hand in a watched directory. The trials table lists every trial. `tab`
cycles through them, lanes to directories to trials and back.

A table with no rows shows the single line `no data.`, whatever left it empty. The directory
table shows it under both `by dir` and `by session`.

Opening the tab, pressing `[r]`, or applying the filter panel shows a small `loading…` popup
while spoolway rereads the ledger. It reads a session's transcript again only if the
transcript's size or modification time changed since the last read. The same read prices every
model once from the price table. The screen keeps those prices until the next read, so a price
table refreshed meanwhile shows after `[r]`. On first opening the tab,
the popup sits over an empty frame. On `[r]` or the filter panel, it sits over the table
already on screen. It closes by itself when the read finishes, and only `[←]`, `[→]` and `[q]`
work while it shows.

| Key | What it does |
|---|---|
| `[↑↓]` | Move the cursor |
| `[a]` | Open the sort popup, ascending |
| `[d]` | Open the sort popup, descending |
| `[t]` | Switch the lanes table between totals and per task, and the directory table between totals and per session |
| `[tab]` | Move to the next section |
| `[f]` | Open the filter panel |
| `[e]` | Export the table on screen, in the view drawn, to a CSV file |
| `[r]` | Re-read the ledger |
| `[q]` | Quit |

The cursor stops at the last row of the table, so one `[↑]` always moves it up. When a read, a
filter or `[t]` leaves fewer rows than the cursor's position, the cursor moves to the new last
row.

While you browse, keys that arrive faster than the screen draws are all applied in the order
typed, and the screen draws one frame for them. Holding `[↓]` and then pressing `[←]` switches
tab at once. Opening a popup, the filter panel, the calendar or a notice ends the burst, and
that frame is drawn before the next key is read.

### Totals and per task

The lanes and directory tables open on totals. `[t]` switches the lanes table to per-task
figures and the directory table to per-session figures, and pressing it again switches back.
The frame title names the three sections, `[eval]   directories   trials`, with the open one
bracketed. The top border's right side starts with the grouping and the view, and the key line
names the view `[t]` switches to.

| View | Border's right side | Key line |
|---|---|---|
| Totals | `by pipeline · totals` | `[d] descending   [t] per task   [tab] section` |
| Per task | `by pipeline · per task` | `[d] descending   [t] totals   [tab] section` |

On the directory table the same view is named `per session`, as in `by dir · per session`.

The view survives `[tab]`, `[r]`, a change of `by` and the filter panel. Every new visit to the
eval tab opens on totals. The trials table has no `[t]`.

After the grouping and the view, the top border's right side names the scope only for
`--project` and `--all`, then lists the filters in force. The trials table has no grouping, so
with nothing else to show its right side has no label.

When the right side is wider than the room the terminal leaves, it drops whole parts from its end
and a final `…` stands in for them. The section names on the left are always drawn whole. The
grouping is the first part, so it is the last to drop. For example, in an 80-column terminal:

```
╭─ [eval]   directories   trials ───── by pipeline · totals · step review · … ─╮
```

### The sort popup

`[a]` or `[d]` opens a popup listing `default order`, then every column the table draws in its
current view.
`[↑↓]` moves, `[enter]` sorts by the chosen column in that direction, `[esc]` closes the popup
unchanged. The sorted header shows `▲` (ascending) or `▼` (descending) in the space before its
name, so no column moves.

The sort reads each row's raw figure, not its drawn cell, so `1h 20m` sorts correctly against
`54m` and `54.2k` against `9.1M`. Ties keep the table's default order. A blank figure, such as
`—` or an unpriced cost, sorts last in both directions, and the `total` or `average` line always
stays last.

The sort survives a change of `by`, the filter panel, `[tab]` and `[r]`. Each table keeps its
own sort. Pressing `[t]` moves a sort on a figure column to the matching column of the other
view, such as `IN` to `IN/TASK` and `TIME` to `TIME/TASK`. `IN USD` moves to `IN USD/TASK`, and the other three class costs move the same way. A sort on `USD/TASK` moves to `USD`.

The cursor moves to the first row after a sort. A view that does not draw the sorted
column falls back to the default order until that column comes back.

A sort carries into every export of the rows it orders: `[e]` writes whatever order the
screen shows, and `--sort` orders `--csv` and `--json` the same way.

## The lanes table

The lanes table opens on `by pipeline`, one row per pipeline. `by` also takes `group`, `task`,
`step` and `version`, changing the columns that name a row. The figure columns are the same
under every `by`, and each figure keeps its value. `by task` adds one column, `CTX PEAK`,
right after `CTX AVG`, and names each row with a `COMPLEXITY` column.

| `by` | One row is |
|---|---|
| `group` | A task `group:`, every run its tasks made |
| `task` | A run of one task, with its pipeline, version and start time. A copy of a routine is named by its routine. |
| `pipeline` | A pipeline |
| `step` | A pipeline's step, in the pipeline's own walk order |
| `version` | A pipeline and one `pipeline_version:` it ran under, newest first |

Under `by task`, each row is one run. Two runs of the same task or routine stay two rows.

| Column | What it shows under `by task` |
|---|---|
| `TASK` | The routine's name when the run's ledger lines carry `routine`. Otherwise the task id. |
| `WHEN` | The run's start in local time, as `YYYY-MM-DD HH:MM`. It is the time of the run's earliest ledger line. |
| `COMPLEXITY` | The task's `complexity:`, the planner's experimental estimate from 1 to 100, right after `WHEN`. `-` when the run's ledger lines carry none. See [The frontmatter is spoolway's](tasks.md#the-frontmatter-is-spoolways). |

Rows read newest start first. `--json` and `--csv` carry the same label in `task`, the same time in `when` and the estimate in `complexity`, a number or blank in CSV and `null` in JSON. `--task` still matches the task id.

| Column | What it is |
|---|---|
| `TASKS` | Tasks that touched this row |
| `PASS` | Share of lanes that reported `pass`. Lanes that never reported are left out. |
| `BLOCKS` | Lanes that reported `--block`, and lanes whose pass or fail left the task on `blocked`. A pass or fail does that when the next step's `loop:` is spent, when a failing step has no `on_fail`, or when a passing step's worktree cannot be committed. A lane counts once, and a pass still counts in `PASS`. Per task, `BLOCKS/TASK` is that count divided by `TASKS`, with two decimals. |
| `TURNS` | The model's replies in the row's lanes, subagents included. Each reply reads the whole conversation again from the cache, so `CACHE R` grows with `TURNS` times context. Read `TURNS` beside `CTX AVG` to see which of the two drove the cache reads. Per task: `TURNS/TASK`, divided by `TASKS` and rounded to a whole number. |
| `CTX AVG` | The mean of the row's lanes' own peak context reading, each as a share of its model's window. A raw token count when no lane's model has a `context_window`. `—` when no lane banked one. |
| `CTX PEAK` | Drawn under `--by task` only, right after `CTX AVG`. The highest share of its own model's window any of the task's lanes reached. A task can mix models with different windows, so this is not always the largest raw reading. A raw token count when no lane's model has a `context_window`. `—` when no lane banked one. |
| `IN`, `OUT`, `CACHE R`, `CACHE W` | Each token class, summed over the row. Per task: `IN/TASK`, `OUT/TASK`, `CACHE R/TASK`, `CACHE W/TASK`, each divided by `TASKS`. |
| `IN USD`, `OUT USD`, `CACHE R USD`, `CACHE W USD` | The cost of the token column before each, priced at today's price table when eval reads. `CACHE W USD` prices the five-minute and one-hour writes each at its own rate and adds them. A line's `tier_tokens`, the part of its tokens that came from over-threshold turns, are priced at the model's tier rates in today's table, and the rest at the base rates. A line without `tier_tokens`, or a model with no tier today, is priced at the base rates. Together they can differ from `USD`, which was banked when the lane settled. Blank when every lane on the row that spent tokens has no price today. Per task: `IN USD/TASK`, `OUT USD/TASK`, `CACHE R USD/TASK`, `CACHE W USD/TASK`, each divided by `TASKS`. |
| `USD` | Total cost. Drawn in both views. |
| `USD/TASK` | Cost per task. Drawn per task only. |
| `TIME` | Wall time, summed over the row. Per task: `TIME/TASK`, divided by `TASKS`. |

A summary line closes the table. It is drawn under the scroll indicator and stays at the bottom
of the screen however far the rows have scrolled. The column header stays at the top. When every
row fits, the line sits right under the last row. A frame with fewer than 3 body rows scrolls the
line with the rows, and a frame with fewer than 4 scrolls the header too.

The line is called `total` in the totals view and `average` in the per-task view. Both are
computed from the ledger entries on screen.

| Column | `total` | `average` |
|---|---|---|
| `TASKS` | Distinct tasks in the table | The same |
| `BLOCKS` | The `BLOCKS` lanes counted above | `BLOCKS/TASK`: that count divided by distinct tasks, with two decimals |
| `TURNS` | The sum of every row's turns | `TURNS/TASK`: that sum divided by distinct tasks |
| `IN`, `OUT`, `CACHE R`, `CACHE W` | The sum of each | `IN/TASK`, `OUT/TASK`, `CACHE R/TASK`, `CACHE W/TASK`: each sum divided by distinct tasks |
| `IN USD`, `OUT USD`, `CACHE R USD`, `CACHE W USD` | The sum of each | `IN USD/TASK`, `OUT USD/TASK`, `CACHE R USD/TASK`, `CACHE W USD/TASK`: each sum divided by distinct tasks |
| `USD` | The sum | Blank |
| `USD/TASK` | Not drawn | Total cost divided by distinct tasks |
| `TIME` | Lane time added up | `TIME/TASK`: lane time added up, divided by distinct tasks |
| `PASS`, `CTX AVG`, `CTX PEAK` | Blank | Blank |

`TIME` on `total` is the wall time of every lane added together. Lanes that ran at the same time
each count in full, so it is not the calendar time the work took.

Tasks are counted once per table. Under `by step`, one task spans several rows, so `TASKS` on
the line is less than the sum of the rows' `TASKS`.

With `--all`, each project gets its own row for a group, pipeline, step or version name. A `PROJECT` column appears when the rows span more than one project.

### The filter panel

`[f]` opens a panel of seven rows: `by`, `pipeline`, `step`, `version`, `trial`, `since` and
`until`. `[↑↓]` moves between rows, `[←→]` cycles a row's value, `[enter]` on `since` or
`until` opens a calendar. On a cycled row, `‹` or `›` is drawn only on the side that still
changes the value, a space in its place otherwise. `by` opens on `pipeline`, its left end, so
only `→` moves it at first. `[enter]` applies the panel, `[esc]` cancels it.

The `trial` row cycles through the trials inside the `since`/`until` window, newest first,
each named `<group> · <date>`. `all` clears it. Opening a trial from the trials table sets
this row for you.

In the calendar, `←`/`→` move a day, `↑`/`↓` a week, `pgup`/`pgdn` a month. `enter` picks the
day, `x` clears the bound, `esc` goes back.

`--since` and `--until` take a duration back from now (`30d`, `4h`), a local date
(`2026-08-21`), or a whole month (`2026-08`). `--until` includes the whole day or month.

### On the command line

```
spoolway eval --by pipeline               every pipeline
spoolway eval --by step --pipeline impl   one pipeline's steps, in walk order
spoolway eval --by version                every pipeline version, newest first
spoolway eval --by task --since 30d       one row per run of a task
spoolway eval --by task --trial <id>      a trial's arms, compared
```

| Flag | What it does |
|---|---|
| `--by <group\|task\|pipeline\|step\|version>` | What one row stands for. Defaults to `pipeline`. |
| `--group <NAME>` | One group's runs only |
| `--task <ID>` | One task's runs only |
| `--pipeline <NAME>` | One pipeline's lanes only |
| `--step <STEP>` | One step's lanes and command step runs only. A command step such as `test` shows its rows in the command steps table. |
| `--pipeline-version <X.Y>` | Lanes that ran under this pipeline version only |
| `--since <WHEN>`, `--until <WHEN>` | The window |
| `--all` | Every project spoolway knows about |
| `--project <NAME>` | One named project |
| `--trial <ID>` | One trial's arms. With `--by task`, one row per arm plus a delta line per arm against the first. |
| `--discard <ID>` | Throw a whole trial away now: every arm's task, worktree, branch, pane and run files. The ledger rows and the source group stay. |
| `--force` | `--discard` only: stop live lanes and discard anyway |
| `--csv` | Print the rows as CSV |
| `--json` | Print the rows as JSON |
| `--per-task` | Print the per-task columns. Without it, the totals columns print. |
| `--sort <column>[:asc\|:desc]` | Sort the rows by one column, descending when the direction is left off |

`--sort` takes a column name as the export's header spells it, below. `--sort turns` orders the
rows by their turns. It also takes `lanes` and `unpriced`. Under `--by task`, `--sort complexity` orders the runs by their estimate, and runs with no estimate come last. It accepts the totals names and the per-task names in both views. A name the printed view does not draw still sorts
the rows and marks no header. An unknown name is refused, naming every column `--by`'s current
value accepts. It cannot be combined with `--discard`.

`--per-task` picks the per-task columns for the printed table, `--csv` and `--json` alike. It
is refused beside `--discard`, which prints no table.

A trial forks one group into one full copy per pipeline it was ticked under, all under one
trial id. See [Trials](planning.md#trials).

```
spoolway eval --by task --trial t-8c21e0
```
```
TASK                PIPELINE   VER  WHEN             COMPLEXITY  TASKS  PASS  BLOCKS  TURNS  CTX AVG  CTX PEAK       IN  IN USD     OUT  OUT USD  CACHE R  CACHE R USD  CACHE W  CACHE W USD       USD      TIME
cart-totals-1       impl       1.0  2026-09-26 14:05         24      1   83%       0     52      19%       31%      433    0.00  105.1k     1.58   28.27M         8.48   539.2k         2.02     11.40   38m 20s
cart-totals-2       impl_fast  1.0  2026-09-26 14:07         24      1  100%       0     41      14%       22%      276    0.00   67.1k     1.01   18.05M         5.42   344.3k         1.29      7.28   28m 40s
total                                                                2             0     93                         709    0.00  172.2k     2.58   46.32M        13.90   883.5k         3.31     18.68    1h 07m

cart-totals-2 vs cart-totals-1: pass +17pp, cost -$4.12, time -9m 40s
```

The delta line reads each later arm against the first, on pass rate, cost and time. The sign
says which way it moved, not which arm is better. A `--sort` can put a later arm on top; the
delta line still reads against the arm that started first.

## The command steps table

A command step, one with a `run:` line, runs no lane and spends no tokens, so it has no row in
the lanes table. Every run of one that ends banks its own line in the usage ledger, and the
printed `spoolway eval` shows those lines in a second table below the lanes table. It is drawn
under every `by`, and its rows are named the way the lanes table names them. Under `--by task` that includes the `COMPLEXITY` column. No lane figure
changes when the ledger holds command steps, and the board's eval tab stays lanes only.

```
spoolway eval --by step --pipeline impl
```
```
Command steps
PIPELINE  STEP    RUNS  PASS  BLOCKS      TIME
impl      suite     14   86%       2  1h 12m
impl      test      31   74%       5    48m 09s
total               45             7  2h 00m
```

| Column | What it is |
|---|---|
| `RUNS` | Command step runs on this row. This counts runs of the command, so it is not the task that the lanes table's `TASKS` counts. |
| `PASS` | Share of the runs that exited 0, out of those that got a verdict. A run stopped at its `timeout:` counts as a failure. A run that vanished without an exit code is left out of both sides, as a lane that never reported is. |
| `BLOCKS` | Runs whose move went to `blocked`: a failing run with no `on_fail`, a spent `loop:`, or the third kill in a row. |
| `TIME` | Wall time, summed. |

The `total` line adds `RUNS`, `BLOCKS` and `TIME` and leaves `PASS` blank. The table has the
same four columns under `--per-task`, which changes only the lanes table. `--sort` does not
reach it, and `--csv` does not carry it. When only command steps match, `--csv` stops with a
message that points at `--json`. The filters `--since`, `--until`, `--group`, `--task`,
`--pipeline`, `--step`, `--pipeline-version` and `--trial` narrow it as they narrow the lanes
table. When only command steps match, the lanes table is left out.

`--json` holds the table under a `commands` key, beside `by`, `rows` and `total`. It has the
same shape: `commands.by`, `commands.rows` and `commands.total`. A row carries the naming
columns of the lanes rows under the same `by`, plus `runs`, `judged` (the runs that got a
verdict), `pass` (a fraction of `judged`, `null` when none did), `blocks` and `time_s`.

Each line in `usage.jsonl` has a `command` object and no `session`, `kind`, `model` or cost.
`command.ended` says how the run ended: `exited` with its `command.exit` code, `timeout`,
`interrupted` or `killed`. A run that vanished without an exit code is `interrupted` and is
started over, which banks its own line. The third such run in a row is `killed`, and blocks the
task. `outcome` is `pass` or `fail` for the first two and absent for the others. `blocked` says
whether the move went to `blocked`. `wall_s` is the time from the run's start to its exit.
A run stopped at its `timeout:` banks the time until it was stopped, and a run that vanished
banks `0`. The line also carries the task, step, pipeline, pipeline version, run, round and
group, as a lane's line does. A step the task walked past, such as a `last:` step that was not
reached, ran nothing and banks nothing.

## The directory table

`[tab]` switches to the directory table. A second `[tab]` opens the trials table, below. The
directory table covers every watched root, this project's own directory included, even one
with no sessions yet. It opens on `by dir`, one row per watched directory.

A session run by hand in one of the project's own worktrees counts here too, on the project
root's own row. A session in a worktree a task owns counts on the lanes table instead. See
[Worktree spend](cost.md#worktree-spend).

| Column | What it is |
|---|---|
| `SESSIONS` | Sessions run in that directory |
| `IN`, `OUT`, `CACHE R`, `CACHE W` | Each token class, summed over the row. Per session: `IN/SESSION`, `OUT/SESSION`, `CACHE R/SESSION`, `CACHE W/SESSION`, each divided by `SESSIONS`. |
| `IN USD`, `OUT USD`, `CACHE R USD`, `CACHE W USD` | The cost of the token column before each, priced at today's price table when eval reads, as on the lanes table. Together they can differ from `USD`. Blank when no session on the row that spent tokens has a price today. Per session: `IN USD/SESSION`, `OUT USD/SESSION`, `CACHE R USD/SESSION`, `CACHE W USD/SESSION`, each divided by `SESSIONS`. |
| `USD` | Total cost. Drawn in both views. |
| `USD/SESSION` | Cost per session. Drawn per session only. |
| `CTX AVG` | The mean of its sessions' own peak context reading, each as a share of its model's window |
| `TIME` | Wall time, summed over the row. Per session: `TIME/SESSION`, divided by `SESSIONS`. |

`by session` lists one row per session instead, newest first. Its columns are the same in both
views.

| Column | What it is |
|---|---|
| `WHEN` | The session's start, to the minute |
| `DIR` | The directory's own name |
| `SKILL` | The first skill the session ran, with `+n` when it ran more than one. A dash when it ran none. |
| `MODEL` | The model the session used |
| `IN`, `OUT`, `CACHE R`, `CACHE W` | That session's own token counts |
| `IN USD`, `OUT USD`, `CACHE R USD`, `CACHE W USD` | The cost of the token column before each, priced at today's price table. Blank when nothing the session spent has a price today. |
| `USD` | That session's own cost |
| `TIME` | That session's own wall time |

A skill counts whether a session typed its command or ran it through the Skill tool; either
way names the same skill. A typed command counts only when it is in the text of a message the
user sent. Text that a tool returned or the assistant wrote never counts.

A skill also counts only when the transcript shows its text loading from a skill folder. Built-in
commands such as `/clear` and `/model` load no skill text, so they never count. A skill bundled
with Claude Code loads without a folder of the user's own, so it never counts either. A skill
from a plugin folder counts.

A subagent's tokens, cost and skills count on the session that ran it, not on a row of its own.

A summary line closes the `by dir` table and is pinned the same way as the lanes table's. In the
totals view it is `total`: the session count and the sums of every token class, the four class
costs, `USD` and `TIME`. In the per-session view it is `average`: the session count, each token class,
each class cost and `TIME` divided by `SESSIONS`, `USD` blank and `USD/SESSION` carrying the cost
divided by `SESSIONS`. `CTX AVG` is blank. A table with no sessions shows a
dash in its token, `TIME` and cost cells.

Under `by session`, the `total` line carries every token class, the four class costs, `USD` and
`TIME` in both views.

Its filter panel has five rows: `by`, `dir`, `skill`, `since` and `until`. The `skill` filter
keeps whole sessions that ran that skill, as counted above. There is no command-line flag for the
directory table; it is read from the screen only.

## The trials table

A second `[tab]` from the directory table opens the trials table, one row per trial that
banked a lane in the window, newest first. A trial groups the arms the `trial` item on a group forked, one
per ticked pipeline, under one trial id. See [Trials](planning.md#trials).

| Column | What it is |
|---|---|
| `GROUP` | The group the trial forked. Falls back to the trial id when that is not known. |
| `WHEN` | The day the trial's earliest arm started |
| `PIPELINES` | Every pipeline an arm ran under, trimmed with `…` when the list is too wide for the column |
| `ARMS` | The trial's own tasks, one per ticked pipeline |
| `STATE` | `running` while a queued task still carries the trial's id, `settled` otherwise |

The table has no `total` line and no `[e]` export.

`enter` opens the highlighted trial. It sets the lanes table's `trial` filter to it, switches
`by` to `pipeline`, clears `pipeline` and `version`, and shows the lanes table, so every one of
the trial's pipelines is a row.

`[a]`/`[d]` sort the table the same way as the other two. Its filter panel has only `since` and
`until`; there is no command-line flag for it, it is read from the screen only.

## Exporting

An export is the table on screen. It carries the columns of the view that is drawn, in the
order they are drawn, with every figure as a raw number a spreadsheet can add up: `195812`
rather than `195.8k`, and `2244` seconds rather than `37m 24s`. Every row also starts with
`project`, and under `--by task` the run id follows it as `run`. Under `--by task`, `complexity` follows `when`.

`[e]` writes the lanes or directory table on screen to
`.spoolway/evals/eval-by-<by>-<view>-<date>-<time>.csv`. `<view>` is `totals`, `per-task` or
`per-session`. The confirmation names the rows, the `by` and the view, such as
`11 rows, by pipeline · per task`. A second export in the same minute gets a `-2` suffix rather
than overwriting the first. The file name follows the view on the top border's right side. Under `by session` the
only view is the session table, and `[t]` names it `per-session`, so the file is
`eval-by-session-per-session-<date>-<time>.csv`.

`--csv` and `--json` print the totals view, and with `--per-task` the per-task view:

```
spoolway eval --by step --pipeline impl --csv
```
```
project,pipeline,step,tasks,pass,blocks,turns,ctx_avg_tokens,ctx_avg_pct,in_tokens,in_usd,out_tokens,out_usd,cache_read_tokens,cache_read_usd,cache_write_tokens,cache_write_usd,cost_usd,time_s
spoolway,impl,implement,48,0.98,2,1310,322000,0.32,17140,0.05,4166300,62.49,1120540000,336.16,21362400,80.11,451.68,81120
```
```
spoolway eval --by step --pipeline impl --per-task --csv
```
```
project,pipeline,step,tasks,pass,blocks_per_task,turns_per_task,ctx_avg_tokens,ctx_avg_pct,in_per_task,in_usd_per_task,out_per_task,out_usd_per_task,cache_read_per_task,cache_read_usd_per_task,cache_write_per_task,cache_write_usd_per_task,cost_usd,cost_per_task,time_per_task_s
spoolway,impl,implement,48,0.98,0.04,27,322000,0.32,357,0.00,86800,1.30,23343000,7.00,445117,1.67,451.68,9.41,1690
```

Each context column is written twice, as a token count and then as a share of the model's
window. The drawn cell shows the share, or the tokens when no window is set. `ctx_peak_tokens`
and `ctx_peak_pct` appear only under `--by task`, where `CTX PEAK` is drawn. `pass` and the
shares are fractions, so `0.98` is 98%.

A class cost such as `in_usd` is blank in CSV, and `null` in `--json`, when every lane on the
row that spent tokens is unpriced. `cost_usd` is blank the same way when every lane on the row
is unpriced. The directory table's export follows the same rule, with `*_per_session` columns
in its per-session view. Under `by session` it has one view, with no per-session columns.

The last CSV line is the screen's own last line, labelled `total` or `average` in the first
column. It fills only the cells the screen fills, so the naming columns, `pass` and the context
columns stay blank, and so does `cost_usd` on an `average` line. `--json` prints an object,
`{"by", "rows", "total", "commands"}`, rather than a bare array, so the last line cannot be
mistaken for a row. Each row carries the CSV's columns as keys. `total` carries only the keys
its line fills, in the same view. `--csv` and `--json` are not allowed together.

An export has no `by`, `lanes` or `unpriced` column, and none of the other view's columns. A
script that reads `in_per_task` needs `--per-task`, and one that reads `in_tokens` needs the
totals view.

When no lane matches the filters, `--json` prints the same object with an empty `rows`. `--csv`
prints the header and the total row. Both exit 0 with nothing else on stdout. The plain table
prints a sentence instead.

## The honest limit

Every task is different work. A pipeline that drew easy tasks looks better than one that drew
hard ones. Read every figure against `TASKS`, the sample size. `spoolway eval` catches drift,
such as a review step that got a third pricier after a prompt edit.

Every figure is a floor, not an invoice. spoolway reads what each agent writes to its
transcript, and an agent does not always write every request it makes there. A request
missing from the transcript is missing from eval, so eval can read low. Compare pipelines,
steps and versions here, and read what you paid on your provider's bill.

## Where the fields come from

`spoolway report` writes a lane's verdict to the task's `last_report:`. The dispatcher reads it
back when it banks the lane. A lane that never reported counts as neither pass nor failure.

A command step's verdict is its exit code, which the dispatcher reads from the run's files in
`commands/` under the project home. It banks the line once, when the result is final: an exit
code, a stop at the step's `timeout:`, a run that vanished, or a background run's exit. An exit
code read again on a later pass is not banked again. The run's start is the time its `.pid` file
was written, and its exit is the time the wrapper wrote `.exit`.
