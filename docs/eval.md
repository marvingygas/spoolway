---
domain: eval
covers: ["src/eval.rs", "src/version.rs", "src/screen.rs"]
---

# Comparing versions

What changing a prompt or a pipeline did to what the work costs.

## The idea in one paragraph

Every edit to your setup changes how the pipeline behaves, and every ledger line already says
which setup a lane ran under: a fingerprint of the tracked configuration, the commit that last
touched it, and what the lane reported. `spoolway eval` is a **view over the ledger you already
have** — no second store, no new file, nothing to keep in step.

```
spoolway eval
```

```
pipelines
default
VERSION    SINCE        RUNS   L/RUN  PASS  BLOCKS  CTX PEAK  USD/RUN   TIME/RUN
c310d7e2   2026-08-16      1     2.0   50%       0       68%     4.42    12m 40s
b210d1a8   2026-08-15      4     4.8   74%       5       91%    70.26     1h 04m
```

## The screen

Bare `spoolway eval`, with no flag at all — not even `--limit` spelled out to its own
default — opens a screen instead of printing the block above, provided stdout is a real
terminal. Every flag, `--json` included, still takes the printing path; so does a bare
invocation whose stdout is redirected or piped, since the screen draws with raw mode and
full-screen escape sequences that only belong on a terminal.

```
┌─ eval · pipelines ─────────────────────────────────────────── proj · 2026-08-01 → now ─┐
│ Pipeline: default                                                                      │
│ VERSION    SINCE        RUNS   L/RUN  PASS  BLOCKS  CTX PEAK  USD/RUN   TIME/RUN       │
│>c310d7e2   2026-08-16      1     2.0   50%       0       68%     4.42    12m 40s       │
│ b210d1a8   2026-08-15      4     4.8   74%       5       91%    70.26     1h 04m       │
└────────────────────────────────────────────────────────────────────────────────────────┘
  Cost is a floor — no price configured for: some-local-model
  ↑↓ move   tab view   f filters   e export   r refresh   q quit
```

The note above the keys line only prints when a lane on screen spent tokens under a model
with no configured price; it sits between the frame's own bottom border and the keys line, so
it never takes a body row.

It reads keys the way `spoolway queue`'s own screen does — one byte at a time, whether stdin
is a real tty in raw mode or a pipe, ending the moment either runs out — and shares that
reader with it through `src/screen.rs`. It never starts a dispatcher: this reads a ledger, it
does not queue work.

`tab` cycles four views, named in the top border along with every filter currently applied:

| View | What it shows |
|---|---|
| `pipelines` | The same block-per-pipeline table the printing path draws |
| `steps` | Per pipeline, one row per step, aggregated over the whole window and the current filters |
| `runs` | One row per run, newest first |
| `skills` | The same skill blocks `spoolway eval` prints below the pipelines |

`↑↓` moves a cursor over the rows.

`f` opens a filter panel of four rows: `pipeline`, `step`, `since`, `until`. `scope` and
`limit` are not among them — the screen only ever opens bare, so both hold the default it
opened with (this project, ten versions per block) for as long as it is up. `--project`,
`--all` and `--limit` are what move either, and typing any of them takes the printing path
instead of the screen; `limit` is not even named on the top border, for the same reason —
nothing on screen can move it. `pipeline` and `step` change with `←`/`→`, cycling `all` and
then each name the ledger actually has.

`since` and `until` are set from a calendar rather than typed: `enter` on either opens a month
grid over the row, in place of applying. `←`/`→` moves the grid's own cursor by a day, `↑`/`↓`
by a week, and `pgup`/`pgdn` steps a month at a time. `enter` inside the grid picks the
highlighted day and returns to the panel; `x` clears the bound back to none and returns; `esc`
returns with the row exactly as it was. The grid opens on the row's own bound when it has one
and on today when it does not, and knows nothing about the ledger — it does not mark which
days have lanes, and lands on an empty month without complaint.

```
┌─ since ───────────────────────────────────────┐
│                ‹ August 2026 ›                │
│      Mo Tu We Th Fr Sa Su                     │
│                      1  2                     │
│       3  4  5  6  7  8  9                     │
│      10 11 12 13 14 15 16                     │
│      17 18 19 20[21]22 23                     │
│      24 25 26 27 28 29 30                     │
│      31                                       │
│                                               │
│  pgup/pgdn month      x no bound              │
│                                               │
│  ←→ day   ↑↓ week   enter pick   esc back     │
└───────────────────────────────────────────────┘
```

On `pipeline` and `step`, `enter` still applies the whole draft at once and reloads; `esc`
leaves the table exactly as it was.

`e` writes the rows on screen, in whichever view they are in, to
`.spoolway/evals/eval-YYYY-MM-DD-HHMM.csv` — the pipelines view with the same header
`--csv` writes, the other three views with a header built from the same column names, since
none of them has a flag-form export of its own. A panel names the path it wrote and how many
rows. `.spoolway/evals/` is a person's own export rather than the project's setup, but
spoolway writes no `.gitignore` rules of its own any more — the runtime state that needed them
left the checkout for `~/.spoolway/<project>/`, `usage.jsonl` included — so keeping the
exports out of git is the project's own line to write.

`r` re-reads the ledger under the current filters and redraws — the same reason `spoolway
eval` itself sweeps before reading. `q` ends the screen from every mode.

## What a version is

A version is a fingerprint over the parts of `.spoolway/` that decide how work is done:
`config.toml`, every pipeline, every prompt. Change one word in one prompt and that is a
new version — which is the point, because that word is exactly the kind of change worth
measuring.

`queue/` and `archive/` are deliberately not in it. They are the work, not the setup, and
folding them in would mint a new version on every task.

**spoolway stores no change history of its own.** `.spoolway/` is tracked, so git already
holds every version of your pipelines and prompts. Recording a second copy would be one more
thing that can disagree with git.

## A block per pipeline

One fingerprint covers every pipeline in the project, so the same version can appear in more
than one block: a config edit mints one version, and it is the version every pipeline reports
under until the next edit. Naming the pipeline in a column would repeat it down every row and
still leave the reader sorting; naming it once, over a block, is what the skills table below
already does.

Blocks lead with the pipeline whose newest version is newest, so this morning's work heads the
stack, whatever pipeline it ran on. The key prints even where `--pipeline` has left one block
on screen — a block always says which pipeline it is.

`PROJECT` stays a column inside the block rather than joining the key: `--all` is what puts
more than one project on screen, and it is already a deliberate act. It only prints, though,
when the rows actually on screen span more than one project — an ordinary single-project run
never carries a column that would say the same word down every row.

Every block repeats the column header, one row per version, newest first.

## Reading a row

```
spoolway eval                          every pipeline, the last ten versions
spoolway eval --pipeline default       one pipeline's block
spoolway eval --step review            one step, across every pipeline
spoolway eval --limit 3                fewer versions per block
spoolway eval --since 30d              a window, as `--by` takes them
```

`--since` and `--until` take a duration back from now (`30d`, `4h`), a local date
(`2026-08-21`), or a whole month (`2026-08`). A month behaves like a date: `--since 2026-08`
starts on the 1st of August, and `--until 2026-08` lands on the 1st of September, so the whole
of August is included.

| Column | What it is |
|---|---|
| `RUNS` | Distinct runs that touched this row — any lane of theirs matched, not necessarily every one |
| `L/RUN` | Lanes launched, over `RUNS` — how many rounds a task typically took |
| `PASS` | Share of this row's lanes that reported `pass` |
| `BLOCKS` | Lanes that ended blocked |
| `CTX PEAK` | The largest context reading any lane on this row banked, as a share of that model's window |
| `USD/RUN` | What every lane cost, over `RUNS` |
| `TIME/RUN` | Every lane's own wall time, summed and then divided by `RUNS` |

Every one of these but `PASS` used to be a plain total, and a total mostly says how many runs
a version happened to draw — one version that ran twice as many runs always shows twice the
`LANES`, twice the spend, twice the time, whatever the setup actually did. Dividing by `RUNS`
is what makes two rows worth holding side by side; `RUNS` itself stays a column rather than
folding away, because it is the sample size every other figure has to be read against.

`CTX PEAK` reads the largest single reading, not a sum: a version that runs longer tasks in
more, smaller turns is not "using more context" in the sense this column means. Where the model
that banked the peak has no `context_window` configured, `CTX PEAK` prints the raw token count
instead of guessing at a percentage — `1.23M` rather than a blank that would read as "nothing
happened" when something did. A row with no `ctx_peak` on any of its lanes — every line banked
before that field existed — prints `—`.

### Tasks that straddle a version

`RUNS` counts every run that touched a row — a run whose lanes ran partly under one version
and partly under the next counts on both rows, exactly as its lanes, tokens and cost already
do. A single footer line names how many runs are counted twice, so the total is never quietly
inflated without saying so:

```
2 run(s) spanned a version change and are counted in both rows.
```

### Why `PASS` is a share of lanes

`PASS` is the share of this row's lanes that reported `pass` — the same grain `L/RUN` and
`BLOCKS` already count, so every ledger line counts exactly once. A lane that never
reported — killed, or silent — is left out of the share rather than counted against it: it is
not a failure, it is a lane nobody heard from.

## One step, across pipelines

```
spoolway eval --step review
```

`--step` filters without implying a grouping: the blocks stay pipeline-keyed, restricted to
lanes of that one step, which is what lets `--pipeline` and `--step` combine.

```
spoolway eval --pipeline default --step review
```

## Runs

Every task's worktree gets a `run` the moment it is cut — minted once, and copied onto every
ledger line banked for that task from then on. It is what a comparison actually holds still,
one grain finer than a version: not "what did this setup cost", but "what did this one attempt
cost."

```
spoolway eval --runs
```

```
TASK    WHEN        VERSION   PIPELINE  LANES  PASS  BLOCKS  CTX PEAK      OUT  COST USD      TIME
login   2026-08-04  8b21ee90  default      4  100%       0       73%    12.1k      0.41    9m 20s
signup  2026-08-09  3f9a1c04  default      6   67%       1         —    24.0k      1.02   14m 02s
```

A model with no configured price prints a "Cost is a floor — no price configured for: …" line
under the table, naming it.

One row per run, oldest first. `PASS` here is the same question the pipeline blocks ask, asked
of one run's own lanes instead of a whole row's — and `CTX PEAK` the same reading, over that
run's own lanes rather than a whole row's. This table keeps every total as itself, unlike the
pipeline blocks above it: one run is already one grain of work, so there is no `RUNS` to divide
it by.

A ledger line written before `run` existed carries none, so it is named instead:
`<task>@<date of its earliest round>`, qualified with the project where two projects would
otherwise mint the same name on the same day.

This is also the table a trial is read in. The queue screen's `p` forks one task into an arm
per pipeline, each queued under its own minted id and the same trial id, so the arms arrive
here as one row each — same work, different pipeline, side by side.

`--runs --trial <id>` narrows the table to one trial's own arms and adds a delta line under it
per arm, against the first: how far its pass rate, cost and time moved from the trial's
baseline, the sign saying which way rather than which arm is better. `--runs` also takes
`--task <id>`, for one task's own runs, and `--group <name>`, for every run whose task carries
that `group:`. See [Trials](planning.md#trials).

## As CSV

```
spoolway eval --pipeline default --csv
```

```
project,pipeline,version,since,tasks,lanes,lanes_per_task,pass,blocks,ctx_peak_tokens,ctx_peak_pct,out_tokens,out_per_task,cost_usd,cost_per_task,unpriced,time_s,time_per_task_s
spoolway,default,c310d7e2,2026-08-16,1,2,2.00,0.50,0,68000,0.68,40300,40300,4.42,4.42,0,760,760
spoolway,default,b210d1a8,2026-08-15,4,19,4.75,0.74,5,91000,0.91,712400,178100,281.05,70.26,0,15480,3870
```

`--csv` prints the same rows and the same order as the table it is on — blocks flatten into a
`pipeline` column, in whichever order the blocks would have printed. Header names match the
`--json` keys, so the two exports describe the same shape. Machine
spellings throughout: raw seconds, raw tokens, no `$`, no `k`, no colour — and every total the
table itself divides by `RUNS`, kept here alongside the per-run figure the table shows, so
nothing the screen stopped printing is actually lost.

An empty `ctx_peak_pct` cell means the model that banked `ctx_peak_tokens` has no
`context_window` configured, not that nothing was read; both cells are empty only where no lane
on the row banked a peak at all.

`unpriced` is how many of `lanes` are missing from `cost_usd` — the same count the "Cost is a
floor" note under the printed tables names. `cost_usd` and `cost_per_task` are left blank rather than a false `0.00` when every
one of `lanes` is unpriced; where only some are, the two cells still print the number they
always did, a floor rather than a total, with `unpriced` beside them saying so. A zero-token
line — an enrolment line, a synthetic turn that answered nothing — spends nothing, so it is
never counted in `unpriced` even when it carries no price of its own. The `--json` row carries
the same `unpriced` count, but nulls `cost_usd` and `cost_per_task` outright rather than
leaving a blank cell — a consumer reading JSON has no blank to fall back on, so `null` is what
tells it a row is wholly unpriced rather than a plain zero.

`--runs --csv` prints the run table the same way, with a `run` column in place of the block
key and the same `unpriced` column ahead of `time_s`.

`--csv` and `--json` are refused together — they are two different exports of the same rows,
never both at once.

## Spend, by `spoolway spend`

Every flag above reads the ledger as a version comparison. `spoolway spend` reads the same
ledger a different way: a spend table, grouped by task, group, step, model, project, month or
skill, or one row per lane with `spoolway spend lane`.

```
spoolway spend step
```

```
STEP          LANES         IN        OUT    CACHE R    CACHE W    COST USD      WALL
implement         4      52.2k      15.4k     794.4k          0           0       49m
review            4        536     222.1k     27.22M     568.3k       24.85       53m
pipeline         17     118.4k     339.9k     36.57M     683.2k       31.12     3h29m
```

`eval --by` still works as a deprecated alias for `spoolway spend`, with a note to stderr
pointing there. How a figure gets there, how a model is priced, what a skill session is, and
the rest of what the table can group and window by — see [Cost accounting](cost.md).

## Skills, a block each

```
spoolway eval
```

```
skills
spoolway-plan
VERSION   SINCE       SESSIONS  COST USD  USD/SESSION
04712e02  2026-08-16        12     14.16         1.18
b210d1a8  2026-08-15         5     12.00         2.40
```

What deciding the work cost, under each version — see [Skill
sessions](cost.md#skill-sessions). It answers the question the pipeline blocks answer for
prompts, asked of the skills instead: did editing a skill make that skill cheaper.

The ledger banks whatever slash command actually ran, under its own full name — so
`/spoolway-plan`, `/my-plan` and `/code-review` are recorded as themselves rather than folded
into one `interactive` bucket. Which of those names get a block of their own is `skills` in
`config.toml`, a list of names — defaulting to `["spoolway-plan"]`, the one spoolway skill
whose own editing is worth watching the cost of. Every other name, `interactive` included, is
left off `spoolway eval`'s skills block entirely: it is still counted in full in the ledger,
which never consults this list.

```
skills = ["spoolway-plan", "my-plan", "grilling"]
```

Blocks are ordered by total cost, biggest spender first. Each block is a bold key, then that
key's versions newest first — the same block shape the pipelines above it use. There is no
`RUNS` and no `PASS` here: a conversation is judged by nobody and belongs to no task. `COST USD`
sits beside `USD/SESSION` because a version's total spend and its average both matter, and
neither one alone tells you which changed.

Registering a skill later still explains every session swept since — the ledger already banked
it under its real name, `skills` only decides whether it gets a block. Lines already banked as
`interactive`, from before a command's real name was recorded, stay that way: the ledger is
append-only, and the command they came from was never written down.

The block prints only on the plain table, not under `--pipeline` or `--step` — a skill session
belongs to no pipeline at all.

## The honest limit

**Every task is a different piece of work.** A version that happened to draw easy tasks looks
better than one that drew hard ones, and nothing in this view corrects for that. What it can
do is print the sample size beside every figure, which is why `RUNS` is a column: a reader
who sees four runs next to a striking `PASS` number can discount it, and will do that better
than any threshold in the binary could.

So `spoolway eval` on the work you happened to run is a **drift monitor** — good for catching
"review got a third pricier after I edited that prompt", weak at anything finer.

### Cost without an outcome would reward the wrong edit

A number that only counts money makes "don't bother running the tests" the winning prompt.
That is why `outcome` is recorded alongside the spend and why `PASS` sits next to `USD/RUN`
rather than on a separate screen.

## Where the fields come from

`spoolway report` is the one command every lane goes through, so it is the one place that can
know how a lane went. It writes the verdict to the task's `last_report:`, along with the step
it belongs to; the dispatcher reads it back when it banks that lane's usage.

A lane that never reported has no outcome, which the table counts as neither a pass nor a
failure.

`run` is written once, when a task's worktree is cut, and copied onto every ledger line banked
for the task from then on.

## Versions before this existed

Lines written before versions were recorded have none, and group under `unversioned`. They
are kept rather than dropped: a project's history did not start when this feature landed, and
their cost is still real.
