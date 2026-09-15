<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo/lockup-a-mark-left-invert.png">
    <img src="docs/logo/lockup-a-mark-left.png" alt="spoolway" width="350">
  </picture>
</p>

<p align="center">
  <a href="LICENSE"><img alt="license" src="https://img.shields.io/github/license/marvingygas/spoolway?style=flat-square&label=license&labelColor=3f3f46&color=18181b"></a>
  <a href="https://github.com/marvingygas/spoolway/actions/workflows/ci.yml"><img alt="ci" src="https://img.shields.io/github/actions/workflow/status/marvingygas/spoolway/ci.yml?branch=main&style=flat-square&label=ci&labelColor=3f3f46"></a>
  <a href="https://github.com/marvingygas/spoolway/releases/latest"><img alt="release" src="https://img.shields.io/github/v/release/marvingygas/spoolway?style=flat-square&label=release&labelColor=3f3f46&color=18181b"></a>
</p>

Minimalistic command line state machine for turning coding agents into a pipeline you can actually reason about. No dependencies, terminal native. Runs headless or controls supported multiplexers. Worktree management and built-in support for GitHub's stacked pull requests.

Supported providers:

- **`claude`**
- **`codex`**
- **`pi`**

Supported multiplexers:

- **`tmux`**
- **`herdr`**

## Why

Running one coding agent is easy. Running five is tab-juggling: which one is done, which
one is stuck, and which one is quietly rewriting a file another one needs. The order lives
in your head, and it stops the moment you look away.

It replaces the orchestrating agent that is supposed to keep everything together — and
sometimes does. spoolway won't surprise you with a bill or an opinion.

## Features

- **A dispatcher with no model in it.** Every scheduling decision is mechanical — a
  counter, a timestamp, a position in the pipeline file. It never drifts, costs nothing,
  and is safe to interrupt at any point.
- **Any mix of agents.** Each step names its own agent and model: a local model for
  implementation, a cloud model for review, a shell command for the test suite.
- **A worktree per task.** Every task builds on its own branch in its own checkout, so
  parallel tasks never step on each other.
- **Stacked pull requests.** A dependent task's branch is cut from its dependency's
  branch, so a chain of tasks arrives as one ordered stack of PRs. Nothing merges itself;
  you land the stack. Optional, and driven by **`spoolway stack`**.
- **Session reuse.** A step can resume its prompt's earlier conversation instead of
  paying to rebuild context, bounded by how full the model's window already is.
- **Unattended runs.** Overnight, nothing parks for a person: blocked work is resumed by
  an unblocker prompt, with a token or dollar ceiling as the brake.
- **Trials.** Fork a whole group into one arm per task, each on its own pipeline, and queue
  them together, then read the arms side by side in eval.
- **Routines.** Keep the tasks you run over and over in `.spoolway/routines/`.
- **Jobs.** Run a routine on a cron schedule. The dispatcher fires it from its own pass, so
  nightly work needs nothing but a dispatcher left running.
- **Eval built in.** Every lane's spend and outcome land in a ledger, so you can see what
  your last pipeline edit did to pass rate and price.

## Install

```
npm install -g spoolway
```

A prebuilt binary, not a Node program — the package is a thin wrapper that runs it. Linux
(x64, arm64, musl), macOS (Apple Silicon, Intel) and Windows (x64, experimental). Successful
self-updates show what changed, and `spoolway whats-new` reads the installed release notes
offline from any directory.

From source instead, in a clone of this repository:

```
cargo install --path .
```

Platform notes and requirements in full: **[Installation and setup](docs/installation.md)**.

## Quick start

### 1. Set up inside the project repo

```
spoolway init
```

`spoolway doctor` checks that everything the configured pipeline needs is actually
present, any time you want to confirm the project would run.

### 2. Plan, or create queueable spoolway tasks directly

Tell the `/spoolway-plan` skill what you want built. It argues the shape with you until
every question is settled, and writes the result as one plan page. Approve the page and it
cuts the plan into tasks.

<img src="docs/screenshots/plan.png" alt="a plan page written by /spoolway-plan">

Already know the shape you want? `/spoolway-tasks` skips the argument and cuts the task
documents straight away.

**Both skills are optional.** A task is only a Markdown file in
`~/.spoolway/<project>/pending/`, so anything that writes Markdown can produce one — an
issue exporter, a script, or a model you already talk to. The following command prints
the frontmatter a task document must carry:

```
spoolway task contract
```

### 3. Queue

```
spoolway queue
```

<img src="docs/screenshots/queue.png" alt="the queue screen">

*The queue screen lists the groups on the left and previews the selected group's tasks on the
right. Each task names its own pipeline. `enter` queues what is checked and offers to start
dispatching on the spot. `g` gates a task on the fly, `p` forks one across several pipelines as
a trial, `s` saves a group as a routine, and `r` opens the routines you keep in
`.spoolway/routines/`.*

### 4. Dispatch

```
spoolway dispatch      # watch the board, and step in only where you are needed
```

<img src="docs/screenshots/dispatch.png" alt="the dispatcher board">

*One row per task, grouped by `group:`. The board says what each lane is spending as it
spends it, what every queued task is waiting on, and which tasks are paused for you. NEXT
distinguishes a lane holding a question from a task waiting to be resumed past a gate. The
ledger at the bottom shows the slots in use and every scheduled job with its next firing.*

Every task on the board is in one of a few states:

| State | Meaning |
|---|---|
| `queued` | Waiting for its dependencies and a free slot. |
| `running` | An agent is working the task's current step, or it has just moved there and a lane is starting. |
| `paused` | Waiting for you on purpose: a gate, a question in its pane, or a park. `r` on the board resumes it. |
| `blocked` | A step reported a block, or ran out of loops. Read the task's `## Blocker`, then `spoolway resume`. An unattended run hands it to the unblocker prompt instead. |
| `unreachable` | A task it depends on is blocked, so it cannot start until you clear that one. |
| `done` | Finished: the branch is handed over, the worktree removed, the task archived. |

### 5. Calibrate

The `/spoolway-calibrate` skill reads the parts of archived task files written by lanes and
the step-level evaluation and spend data, then compares them with the prompts, pipelines and
settings that produced them. It uses both numbers and the agents' own reports to explain
review failures, blocked sessions and wasted loops.

It walks the useful findings with you and can apply the prompt or pipeline changes you choose.
This is how a pipeline that blocks constantly turns into one that runs unattended.

## A task is what travels the line, and you define it

```markdown
---
id: sessions
title: feat(auth): add session tokens on top of login
group: auth
touches:
  - src/auth/**
depends_on:
  - login
---
## Context
## Goal
## Non-goals
## Acceptance criteria
## References
```

**The frontmatter is spoolway's, the body is yours.** spoolway never reads the body; it is
what the agent works from, written from a skeleton you own.

## The pipeline is a file

- Mix agents and commands
- Set model and effort levels per step
- Create loops and inject skills
- Re-use previous sessions until the context threshold is met

```yaml
# .spoolway/pipelines/default.yml
steps:
  - id: implement
    description: Write the code to satisfy the task's acceptance criteria.
    agent: pi
    prompt: implementer
    model: Ornith-1.5-35B-A3B
    session: true
    on_pass: review
    on_fail: blocked

  - id: review
    description: Check the diff against the acceptance criteria and project standards.
    agent: claude
    prompt: reviewer
    model: claude-opus-5
    effort: high
    session: true
    loop:
      implement: 2
    on_pass: document
    on_fail: implement

  - id: document
    description: Bring the domain documents in line with what this task changed.
    agent: pi
    prompt: archivist
    model: Ornith-1.5-35B-A3B
    on_pass: handover
    on_fail: blocked

  - id: handover
    description: Commit, squash, push and open this task's pull request — no model,
      no rebase. Nothing is merged here; a person lands it.
    run: spoolway stack
    on_pass: checks
    on_fail: blocked

  - id: checks
    description: Wait for the pull request's checks, and fail if they are red.
    run: gh pr checks --watch --fail-fast
    timeout: 45m
    on_pass: done
    on_fail: blocked
```

**You do not have to write one by hand.** The `/spoolway-config` skill writes a pipeline
for you, and edits the one you already have.

## Jobs

A job runs a routine on a schedule. It is three things: a cron expression, a pipeline, and a
routine you saved under `.spoolway/routines/`.

spoolway has no clock of its own. The dispatcher fires a due job at the top of its pass, and
while any job is enabled it stays up on an empty queue instead of exiting. A `spoolway dispatch`
left running overnight is all a job needs.

```
spoolway jobs              # the screen: write, edit, pause, delete, or fire a job
spoolway jobs list         # every job, its schedule, and when it fires next
spoolway jobs run <name>   # fire one now, ignoring its schedule
```

<img src="docs/screenshots/jobs.png" alt="the jobs screen">

*The jobs screen. `n` walks three choices: the routine, the cron expression, and the pipeline.
A job fires once per matching minute and skips a window while its previous run is still in the
queue. A window that passes while no dispatcher is running is not caught up later.*

A job is a few lines of TOML. Yours live in `~/.spoolway/<project>/jobs.toml`. Put one in
`.spoolway/jobs.toml` inside the checkout to share it with the team.

```toml
[jobs.nightly-audit]
schedule = "0 3 * * 1-5"   # weekdays at 03:00, local time
pipeline = "impl_fast"
routine  = "nightly"       # a folder or a single .md under .spoolway/routines/
```

### Routines

A routine is work you run more than once. Queueing a group deletes its documents from
`pending/`, so repeatable tasks live in `.spoolway/routines/` instead, tracked in git. Press `s`
on a group in the queue screen to save it there, and `r` to browse and queue what is saved.
Every queued copy gets a fresh id, so a routine can run again without colliding with its last
run. The saved files are never changed.

### Trials

A trial answers one question: which pipeline does this task best? Press `p` on a group in the
queue screen, pick a pipeline per task, and tick any steps to skip. Every task becomes one arm,
queued under its chosen pipeline, and all arms share one trial id. Read them side by side with
`spoolway eval --runs --trial <id>`. An arm never pushes a branch or opens a pull request. When
the last arm finishes, every arm's copy is removed, and only the source group and the ledger
rows stay.

## Issue tracker

Use event hooks to sync with project management tools. GitHub and Jira sample scripts are shipped.

| Event | When it fires |
|---|---|
| `fetch` | `spoolway issue show <ref>` reads one issue out of the tracker |
| `open` | `spoolway queue add` opens a ticket per document |
| `queued` | A task arrives in the queue |
| `blocked` | A task comes to rest on `blocked` |
| `paused` | A task arrives on the persisted `paused` stage; a live-step row whose public state is `paused` does not fire it |
| `done` | A task finishes |

**The two shipped scripts are samples.** `spoolway init` writes `github.sh` and `jira.sh` into
`.spoolway/hooks/` — the `.ps1` pair on a native Windows install.

See **[Issue Tracking](docs/configuration.md#issue_tracking--a-hook-fired-on-four-task-events)**.

## Configurable per project

- Unattended mode delegates **`blocked`** tasks to a prompt you define. It clears
  obstacles on its own and keeps your pipeline running while nobody is watching.
- Set a specific model for generating pipelines.

```toml
[dispatch]
backend = "herdr"            # herdr, tmux, or headless
herdr_mode = "split"         # "split": a workspace per task; "grouped": one shared tab, a pane per task
tmux_mode = "grouped"        # "grouped": one session for the run; "split": a session per task
worktree_root = ""           # where a task's worktree is cut; blank is ~/.spoolway/<project>/worktrees
interval = "10s"             # how long the dispatcher waits between passes
lane_quiet = "15m"           # silence before a lane is reminded to report
default_pipeline = "default" # which pipeline a task runs when it names none
auto_commit = true           # commit a lane's leftover work when its step settles

[unattended]
enabled = true               # the overnight switch
max_output_tokens = 0        # output-token ceiling for a run with nobody watching; 0 is none
max_cost_usd = 0.0           # dollar ceiling for the same run; 0 is none
skip_blocked_lane = true     # a cleared block carries the task past the step it blocked on
blocked_agent = "claude"     # who staffs `blocked` when nobody is at the keyboard
blocked_model = "claude-opus-5"
blocked_effort = "medium"
blocked_session = true       # the unblocker carries its own earlier session forward
blocked_prompt = "unblocker"

[pipeline_gen]
pipeline_agent = "claude"    # who `spoolway pipeline gen` opens its session as
pipeline_model = "claude-opus-5"
pipeline_effort = "medium"

[housekeeping]
update_check = true          # tell a person at a keyboard that a newer release is out
calibrate_window = "14d"     # how far back `/spoolway-calibrate` reads
retention_days = 30          # how long run records and archived tasks are kept; 0 keeps everything
price_max_age_days = 30      # how old the price table may be before `spoolway doctor` says so

[agents.claude]
kind = "claude"
concurrency = 3              # most lanes of this profile at once
session_reuse_ctx = 50       # % of the window before a carried session restarts fresh
session_blocked_ctx = 0      # % of the window at which a running lane is stopped and blocked; 0 is off
permission_mode = "auto"

[agents.codex]
kind = "codex"
concurrency = 3
session_reuse_ctx = 50
session_blocked_ctx = 0
permission_mode = "never"

[agents.pi]
kind = "pi"
session_reuse_ctx = 50
session_blocked_ctx = 0

[models."Ornith-1.5-35B-A3B"]  # a local model, served by llama.cpp
context_window = 100096
slots = 2                    # parallel lanes the local server can actually hold
exclusive = true             # never alongside another exclusive model
local = true                 # runs on hardware you own

[issue_tracking]
hook = ""                    # a script in .spoolway/hooks/, e.g. "github.sh"; blank runs none
project_key = ""             # handed to the hook verbatim, e.g. owner/repo
on_fail = "ignore"           # what a failing hook does: ignore it, or pause the task
key_in_names = false         # prefix branch and worktree names with the tracker's slug
```

See **[Configuration](docs/configuration.md)**.

## Eval every run

Every lane's transcript is read when it settles and banked to a ledger: tokens, cost, wall
time, and what the lane reported. Every edit to your pipelines, prompts or config mints a
new version, so you can see what your last change did to pass rate and price.

```
spoolway eval
```

<img src="docs/screenshots/eval.png" alt="the eval screen">

*The eval screen on its runs view, one row per attempt at a task — `tab` cycles the three
views: the version comparisons per pipeline and per step, and this one. `f` filters, `e`
exports CSV.*

## Documentation

See **[Documentation index](DOCS.md)**. 

## License

MIT. See [LICENSE](LICENSE).
