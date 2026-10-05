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
PIPELINE    RUNS  PASS  BLOCKS  CTX PEAK AVG  CTX PEAK       IN      OUT  CACHE R  CACHE W       USD      TIME
impl          41   82%       3           32%       52%    26.3k    6.39M    1.72B   32.78M    692.90   49h 12m
impl_ui       15   79%       2           31%       50%    12.2k    2.95M   793.5M   15.15M    319.95   31h 00m
Total         56             5                            38.5k    9.34M    2.51B   47.93M   1012.85   80h 12m
```

The token, cost and time columns show each row's totals. `spoolway eval --per-run` prints each of
them divided by the row's runs. See [Totals and per run](#totals-and-per-run).

## The screen

`spoolway eval` always prints the lanes table. Bare `spoolway`'s eval tab draws an interactive
screen over the same rows.

<img src="screenshots/eval.png" alt="the eval screen">

The screen holds three tables. The lanes table groups dispatched lanes. The directory table
groups sessions run by hand in a watched directory. The trials table lists every trial. `tab`
cycles through them, lanes to directories to trials and back.

Opening the tab, pressing `[r]`, or applying the filter panel shows a small `Loading…` popup
while spoolway rereads the ledger. It reads a session's transcript again only if the
transcript's size or modification time changed since the last read. On first opening the tab,
the popup sits over an empty frame. On `[r]` or the filter panel, it sits over the table
already on screen. It closes by itself when the read finishes, and only `[←]`, `[→]` and `[q]`
work while it shows.

| Key | What it does |
|---|---|
| `[↑↓]` | Move the cursor |
| `[a]` | Open the sort popup, ascending |
| `[d]` | Open the sort popup, descending |
| `[t]` | Switch the lanes and directory tables between totals and per run |
| `[tab]` | Cycle to the next table |
| `[f]` | Open the filter panel |
| `[e]` | Export the rows on screen to a CSV file |
| `[r]` | Re-read the ledger |
| `[q]` | Quit |

### Totals and per run

The lanes and directory tables open on totals. `[t]` switches both tables to per-run figures,
and pressing it again switches back. The frame title names the view, and the key line names the
view `[t]` switches to.

| View | Frame title | Key line |
|---|---|---|
| Totals | `eval · by pipeline · totals` | `[d] descending   [t] per run   [tab] dirs` |
| Per run | `eval · by pipeline · per run` | `[d] descending   [t] totals   [tab] dirs` |

The view survives `[tab]`, `[r]`, a change of `by` and the filter panel. Every new visit to the
eval tab opens on totals. The trials table has no `[t]`.

The top border's right side names the scope only for `--project` and `--all`. It lists the
filters in force. With nothing to show, it has no label.

### The sort popup

`[a]` or `[d]` opens a popup listing `default order`, then every column the table draws in its
current view.
`[↑↓]` moves, `[enter]` sorts by the chosen column in that direction, `[esc]` closes the popup
unchanged. The sorted header shows `▲` (ascending) or `▼` (descending) in the space before its
name, so no column moves.

The sort reads each row's raw figure, not its drawn cell, so `1h 20m` sorts correctly against
`54m` and `54.2k` against `9.1M`. Ties keep the table's default order. A blank figure, such as
`—` or an unpriced `USD`, sorts last in both directions, and the `Total` or `Average` line always
stays last.

The sort survives a change of `by`, the filter panel, `[tab]` and `[r]`. Each table keeps its
own sort. Pressing `[t]` moves a sort on a figure column to the matching column of the other
view, such as `IN` to `IN/RUN` and `TIME` to `TIME/RUN`. A sort on `USD/RUN` moves to `USD`.

The cursor moves to the first row after a sort. A view that does not draw the sorted
column falls back to the default order until that column comes back.

A sort carries into every export of the rows it orders: `[e]` writes whatever order the
screen shows, and `--sort` orders `--csv` and `--json` the same way.

## The lanes table

The lanes table opens on `by pipeline`, one row per pipeline. `by` also takes `group`, `task`,
`step` and `version`, changing only the columns that name a row. The figure columns stay the
same under every `by`, so switching how the rows are grouped never moves a figure.

| `by` | One row is |
|---|---|
| `group` | A task `group:`, every run its tasks made |
| `task` | A run of one task, with its pipeline, version and date |
| `pipeline` | A pipeline |
| `step` | A pipeline's step, in the pipeline's own walk order |
| `version` | A pipeline and one `pipeline_version:` it ran under, newest first |

| Column | What it is |
|---|---|
| `RUNS` | Runs that touched this row |
| `PASS` | Share of lanes that reported `pass`. Lanes that never reported are left out. |
| `BLOCKS` | Lanes that reported `--block`, and lanes whose pass or fail left the task on `blocked`. A pass or fail does that when the next step's `loop:` is spent, when a failing step has no `on_fail`, or when a passing step's worktree cannot be committed. A lane counts once, and a pass still counts in `PASS`. Per run, `BLOCKS/RUN` is that count divided by `RUNS`, with two decimals. |
| `CTX PEAK AVG` | The mean of the row's lanes' own peak context reading, each as a share of its model's window |
| `CTX PEAK` | The largest context reading any lane on the row banked, as a share of the model's window. A raw token count when the model has no `context_window`. `—` when no lane banked one. |
| `IN`, `OUT`, `CACHE R`, `CACHE W` | Each token class, summed over the row. Per run: `IN/RUN`, `OUT/RUN`, `CACHE R/RUN`, `CACHE W/RUN`, each divided by `RUNS`. |
| `USD` | Total cost. Drawn in both views. |
| `USD/RUN` | Cost per run. Drawn per run only. |
| `TIME` | Wall time, summed over the row. Per run: `TIME/RUN`, divided by `RUNS`. |

A summary line closes the table. It is drawn under the scroll indicator and stays at the bottom
of the screen however far the rows have scrolled. The column header stays at the top. When every
row fits, the line sits right under the last row. A frame with fewer than 3 body rows scrolls the
line with the rows, and a frame with fewer than 4 scrolls the header too.

The line is called `Total` in the totals view and `Average` in the per-run view. Both are
computed from the ledger entries on screen.

| Column | `Total` | `Average` |
|---|---|---|
| `RUNS` | Distinct runs in the table | The same |
| `BLOCKS` | The `BLOCKS` lanes counted above | `BLOCKS/RUN`: that count divided by distinct runs, with two decimals |
| `IN`, `OUT`, `CACHE R`, `CACHE W` | The sum of each | `IN/RUN`, `OUT/RUN`, `CACHE R/RUN`, `CACHE W/RUN`: each sum divided by distinct runs |
| `USD` | The sum | Blank |
| `USD/RUN` | Not drawn | Total cost divided by distinct runs |
| `TIME` | Lane time added up | `TIME/RUN`: lane time added up, divided by distinct runs |
| `PASS`, `CTX PEAK AVG`, `CTX PEAK` | Blank | Blank |

`TIME` on `Total` is the wall time of every lane added together. Lanes that ran at the same time
each count in full, so it is not the calendar time the work took.

Runs are counted once per table. Under `by step`, one run spans several rows, so `RUNS` on the
line is less than the sum of the rows' `RUNS`.

With `--all`, a `PROJECT` column appears when the rows span more than one project.

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
| `--step <STEP>` | One step's lanes only |
| `--pipeline-version <X.Y>` | Lanes that ran under this pipeline version only |
| `--since <WHEN>`, `--until <WHEN>` | The window |
| `--all` | Every project spoolway knows about |
| `--project <NAME>` | One named project |
| `--trial <ID>` | One trial's arms. With `--by task`, one row per arm plus a delta line per arm against the first. |
| `--discard <ID>` | Throw a whole trial away now: every arm's task, worktree, branch, pane and run files. The ledger rows and the source group stay. |
| `--force` | `--discard` only: stop live lanes and discard anyway |
| `--csv` | Print the rows as CSV |
| `--json` | Print the rows as JSON |
| `--per-run` | Print the per-run columns. Without it, the totals columns print. |
| `--sort <column>[:asc\|:desc]` | Sort the rows by one column, descending when the direction is left off |

`--sort` takes a column name the way `--csv`'s header spells it, below. It accepts the totals
names and the per-run names in both views. A name the printed view does not draw still sorts
the rows and marks no header. An unknown name is refused, naming every column `--by`'s current
value accepts. It cannot be combined with `--discard`.

`--per-run` changes only the printed table. It is refused beside `--csv`, `--json` and
`--discard`, because the exports already carry both column sets.

A trial forks one group into one full copy per pipeline it was ticked under, all under one
trial id. See [Trials](planning.md#trials).

```
spoolway eval --by task --trial t-8c21e0
```
```
TASK                PIPELINE   VER  WHEN        RUNS  PASS  BLOCKS  CTX PEAK AVG  CTX PEAK       IN      OUT  CACHE R  CACHE W       USD      TIME
cart-totals-1       impl       1.0  2026-09-26     1   83%       0           19%       31%       433   105.1k   28.27M   539.2k     11.40   38m 20s
cart-totals-2       impl_fast  1.0  2026-09-26     1  100%       0           14%       22%       276    67.1k   18.05M   344.3k      7.28   28m 40s
Total                                              2             0                              709   172.2k   46.32M   883.5k     18.68     1h 07m

cart-totals-2 vs cart-totals-1: pass +17pp, cost -$4.12, time -9m 40s
```

The delta line reads each later arm against the first, on pass rate, cost and time. The sign
says which way it moved, not which arm is better. A `--sort` can put a later arm on top; the
delta line still reads against the arm that started first.

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
| `IN`, `OUT`, `CACHE R`, `CACHE W` | Each token class, summed over the row. Per run: `IN/SESSION`, `OUT/SESSION`, `CACHE R/SESSION`, `CACHE W/SESSION`, each divided by `SESSIONS`. |
| `USD` | Total cost. Drawn in both views. |
| `USD/SESSION` | Cost per session. Drawn per run only. |
| `CTX PEAK AVG` | The mean of its sessions' own peak context reading, each as a share of its model's window |
| `CTX PEAK` | The largest context reading any session banked |
| `TIME` | Wall time, summed over the row. Per run: `TIME/SESSION`, divided by `SESSIONS`. |

`by session` lists one row per session instead, newest first. Its columns are the same in both
views.

| Column | What it is |
|---|---|
| `WHEN` | The session's start, to the minute |
| `DIR` | The directory's own name |
| `SKILL` | The first skill the session ran, with `+n` when it ran more than one. A dash when it ran none. |
| `MODEL` | The model the session used |
| `IN`, `OUT`, `CACHE R`, `CACHE W` | That session's own token counts |
| `USD` | That session's own cost |
| `TIME` | That session's own wall time |

A skill counts whether a session typed its command or ran it through the Skill tool; either
way names the same skill. A subagent's tokens, cost and skills count on the session that ran
it, not on a row of its own.

A summary line closes the `by dir` table and is pinned the same way as the lanes table's. In the
totals view it is `Total`: the session count and the sums of every token class, `USD` and `TIME`.
In the per-run view it is `Average`: the session count, each token class and `TIME` divided by
`SESSIONS`, `USD` blank and `USD/SESSION` carrying the cost divided by `SESSIONS`. `CTX PEAK AVG`
and `CTX PEAK` are blank. A table with no sessions shows a dash in its token, `TIME` and cost cells.

Under `by session`, the `Total` line carries every token class, `USD` and `TIME` in both views.

Its filter panel has five rows: `by`, `dir`, `skill`, `since` and `until`. The `skill` filter
keeps whole sessions whose transcript names that skill. There is no command-line flag for the
directory table; it is read from the screen only.

## The trials table

A second `[tab]` from the directory table opens the trials table, one row per trial that
banked a lane in the window, newest first. A trial groups the arms a `t` on a group forked, one
per ticked pipeline, under one trial id. See [Trials](planning.md#trials).

| Column | What it is |
|---|---|
| `GROUP` | The group the trial forked. Falls back to the trial id when that is not known. |
| `WHEN` | The day the trial's earliest arm started |
| `PIPELINES` | Every pipeline an arm ran under, trimmed with `…` when the list is too wide for the column |
| `ARMS` | The trial's own tasks, one per ticked pipeline |
| `STATE` | `running` while a queued task still carries the trial's id, `settled` otherwise |

The table has no `Total` line and no `[e]` export.

`enter` opens the highlighted trial. It sets the lanes table's `trial` filter to it, switches
`by` to `pipeline`, clears `pipeline` and `version`, and shows the lanes table, so every one of
the trial's pipelines is a row.

`[a]`/`[d]` sort the table the same way as the other two. Its filter panel has only `since` and
`until`; there is no command-line flag for it, it is read from the screen only.

## Exporting

`[e]` writes the table on screen to `.spoolway/evals/eval-by-<by>-<date>-<time>.csv`, named
after the current `by`. A second export in the same minute gets a `-2` suffix rather than
overwriting the first.

```
spoolway eval --by step --pipeline impl --csv
```
```
project,by,pipeline,step,pipeline_version,runs,lanes,pass,blocks,ctx_peak_tokens,ctx_peak_pct,ctx_peak_avg_tokens,ctx_peak_avg_pct,in_tokens,out_tokens,cache_read_tokens,cache_write_tokens,in_per_run,out_per_run,cache_read_per_run,cache_write_per_run,cost_usd,cost_per_run,unpriced,time_s,time_per_run_s
spoolway,step,impl,implement,,48,106,0.98,2,520000,0.52,322000,0.32,17140,4166300,1120540000,21362400,357,86800,23343000,445117,451.68,9.41,0,81120,1690
```

The CSV and `--json` rows carry the per-run token figures beside the raw totals, the
`ctx_peak_avg` pair, and `pipeline_version`. The `total` row and the `--json` `total` object
carry the sums and the per-run figures the screen's `Total` and `Average` lines show:
`in_tokens`, `out_tokens`, `cache_read_tokens`, `cache_write_tokens`, the four `*_per_run`
columns, `cost_per_run`, `time_s` and `time_per_run_s`. The directory table's export carries the
`*_per_session` columns instead. The pass and context columns stay blank on the CSV total row, and
both views export the same row. `--json` prints an object, `{"by", "rows",
"total"}`, rather than a bare array, so the `Total` line cannot be mistaken for a row. Its own
`by` column reads `total`. `--csv` and `--json` are not allowed together.

`unpriced` counts how many lanes have no price. `cost_usd` is blank in CSV, and `null` in
`--json`, when every lane on the row is unpriced.

## The honest limit

Every task is different work. A pipeline that drew easy tasks looks better than one that drew
hard ones. Read every figure against `RUNS`, the sample size. `spoolway eval` catches drift,
such as a review step that got a third pricier after a prompt edit.

## Where the fields come from

`spoolway report` writes a lane's verdict to the task's `last_report:`. The dispatcher reads it
back when it banks the lane. A lane that never reported counts as neither pass nor failure.
