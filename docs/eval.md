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
PIPELINE    RUNS  PASS  BLOCKS  CTX PEAK AVG  CTX PEAK  IN/RUN  OUT/RUN  CACHE R/RUN  CACHE W/RUN       USD  USD/RUN  TIME/RUN
impl          41   82%       3           32%       52%     642   155.8k       41.91M       799.4k    692.90    16.90    1h 12m
impl_ui       15   79%       2           31%       50%     810   196.7k       52.90M        1.01M    319.95    21.33    2h 04m
Total         56             5                                                              1012.85
```

## The screen

`spoolway eval` always prints the lanes table. Bare `spoolway`'s eval tab draws an interactive
screen over the same rows.

<img src="screenshots/eval.png" alt="the eval screen">

The screen holds three tables. The lanes table groups dispatched lanes. The directory table
groups sessions run by hand in a watched directory. The trials table lists every trial. `tab`
cycles through them, lanes to directories to trials and back.

Opening the tab, pressing `[r]`, or applying the filter panel shows a small `Loading…` popup
while spoolway rereads the ledger and every session's transcript. On first opening the tab, the
popup sits over an empty frame. On `[r]` or the filter panel, it sits over the table already on
screen. It closes by itself when the read finishes, and only `[←]`, `[→]` and `[q]` work while
it shows.

| Key | What it does |
|---|---|
| `[↑↓]` | Move the cursor |
| `[a]` | Open the sort popup, ascending |
| `[d]` | Open the sort popup, descending |
| `[tab]` | Cycle to the next table |
| `[f]` | Open the filter panel |
| `[e]` | Export the rows on screen to a CSV file |
| `[r]` | Re-read the ledger |
| `[q]` | Quit |

### The sort popup

`[a]` or `[d]` opens a popup listing `default order`, then every column the table draws.
`[↑↓]` moves, `[enter]` sorts by the chosen column in that direction, `[esc]` closes the popup
unchanged. The sorted header shows `▲` (ascending) or `▼` (descending) in the space before its
name, so no column moves.

The sort reads each row's raw figure, not its drawn cell, so `1h 20m` sorts correctly against
`54m` and `54.2k` against `9.1M`. Ties keep the table's default order. A blank figure, such as
`—` or an unpriced `USD`, sorts last in both directions, and the `Total` line always stays
last.

The sort survives a change of `by`, the filter panel, `[tab]` and `[r]`. Each table keeps its
own sort. The cursor moves to the first row after a sort. A view that does not draw the sorted
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
| `BLOCKS` | Lanes that ended blocked |
| `CTX PEAK AVG` | The mean of the row's lanes' own peak context reading, each as a share of its model's window |
| `CTX PEAK` | The largest context reading any lane on the row banked, as a share of the model's window. A raw token count when the model has no `context_window`. `—` when no lane banked one. |
| `IN/RUN`, `OUT/RUN`, `CACHE R/RUN`, `CACHE W/RUN` | Each token class, divided by `RUNS` |
| `USD` | Total cost |
| `USD/RUN` | Cost per run |
| `TIME/RUN` | Wall time per run |

A `Total` line closes the table, carrying only what adds up across rows: distinct `RUNS`,
`BLOCKS` and `USD`. A per-run average or a summed peak would not mean anything added together,
so those cells are blank on `Total`.

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
| `--sort <column>[:asc\|:desc]` | Sort the rows by one column, descending when the direction is left off |

`--sort` takes a column name the way `--csv`'s header spells it, below. An unknown name is
refused, naming every column `--by`'s current value accepts. It cannot be combined with
`--discard`.

A trial forks one group into one full copy per pipeline it was ticked under, all under one
trial id. See [Trials](planning.md#trials).

```
spoolway eval --by task --trial t-8c21e0
```
```
TASK                PIPELINE   VER  WHEN        RUNS  PASS  BLOCKS  CTX PEAK AVG  CTX PEAK  IN/RUN  OUT/RUN  CACHE R/RUN  CACHE W/RUN       USD  USD/RUN  TIME/RUN
cart-totals-1       impl       1.0  2026-09-26     1   83%       0           19%       31%     433   105.1k       28.27M       539.2k     11.40    11.40   38m 20s
cart-totals-2       impl_fast  1.0  2026-09-26     1  100%       0           14%       22%     276    67.1k       18.05M       344.3k      7.28     7.28   28m 40s
Total                                              2             0                                                                        18.68

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
| `IN/SESSION`, `OUT/SESSION`, `CACHE R/SESSION`, `CACHE W/SESSION` | Each token class, divided by `SESSIONS` |
| `USD` | Total cost |
| `USD/SESSION` | Cost per session |
| `CTX PEAK AVG` | The mean of its sessions' own peak context reading, each as a share of its model's window |
| `CTX PEAK` | The largest context reading any session banked |
| `TIME/SESSION` | Wall time per session |

`by session` lists one row per session instead, newest first.

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

A `Total` line closes the table: the session count and `USD` under `by dir`, and every token
class, `USD` and `TIME` under `by session`.

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
`ctx_peak_avg` pair, and `pipeline_version`. `--json` prints an object, `{"by", "rows",
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
