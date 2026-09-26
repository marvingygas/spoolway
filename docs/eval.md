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

`spoolway eval` with no flags, in a terminal, opens a screen. Any flag, or a redirected stdout,
prints the lanes table instead.

<img src="screenshots/eval.png" alt="the eval screen">

The screen holds two tables. The lanes table groups dispatched lanes. The directory table
groups sessions run by hand in a watched directory. `tab` switches between them.

| Key | What it does |
|---|---|
| `[↑↓]` | Move the cursor |
| `[tab]` | Switch to the other table |
| `[f]` | Open the filter panel |
| `[e]` | Export the rows on screen to a CSV file |
| `[r]` | Re-read the ledger |
| `[q]` | Quit |

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

`[f]` opens a panel of eight rows: `by`, `group`, `task`, `pipeline`, `step`, `version`,
`since` and `until`. `[↑↓]` moves between rows, `[←→]` cycles a row's value, `[enter]` on
`since` or `until` opens a calendar. The `task` row cycles only the tasks the rest of the
panel's filters still admit. `[enter]` applies the panel, `[esc]` cancels it.

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
| `--discard <ID>` | Throw a whole trial away now: every arm's task document, worktree, branch, pane and run files. The ledger rows and the source group stay. |
| `--force` | `--discard` only: stop live lanes and discard anyway |
| `--csv` | Print the rows as CSV |
| `--json` | Print the rows as JSON |

A trial forks one group into one arm per task, each under its own pipeline, all under one
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
says which way it moved, not which arm is better.

## The directory table

`[tab]` switches to the directory table. It covers every watched root, this project's own
directory included, even one with no sessions yet. It opens on `by dir`, one row per watched
directory.

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

A `Total` line closes the table: the session count and `USD` under `by dir`, and every token
class, `USD` and `TIME` under `by session`.

Its filter panel has five rows: `by`, `dir`, `skill`, `since` and `until`. The `skill` filter
keeps whole sessions whose transcript names that skill. There is no command-line flag for the
directory table; it is read from the screen only.

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
