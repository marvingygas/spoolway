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

spoolway is a descriptive agent workflow builder run by a minimal command line state machine.
It turns coding agents into pipelines you can reason about. Determinism where possible.
**Terminal native. Built for herdr.**

Create YAML pipelines that mix agents and commands, cloud and local models. Plan the work
however you like, then cut spoolway tasks and assign each to a suitable pipeline.
Measurable, comparable, repeatable.

Supported providers:

- **`claude`**
- **`codex`**
- **`pi`**

## Why

Running one coding agent is easy. Running five is hard: which one is done, which one is
stuck, and which one is rewriting a file another one needs. The order lives in your head.

spoolway keeps that order for you. Its dispatcher has no model inside, so scheduling does not
add model costs or depend on an agent's judgment.

## How does it work?

spoolway gives each agent a system prompt that tells it how to report its outcome with
`spoolway report`. The task document carries context between steps. Through herdr, agents can
interact with one another across harnesses.

An agent can report four outcomes:

| Outcome | What happens |
|---|---|
| `pass` | Moves to the step's `on_pass` destination. |
| `fail` | Moves to `on_fail`, often an earlier step, or to `blocked` if none is set. |
| `block` | Moves to `blocked`. In an unattended run, the configured unblocker agent takes over. |
| `pause` | Lets an unblocker hand an unresolved issue to a person. |

A gate can pause a task for a person after a step reports.

## Features

- **A dispatcher with no model in it.** Every scheduling decision is a counter, a
  timestamp, or a position in the pipeline file. It costs nothing and is safe to interrupt.
- **Any mix of agents.** Each step names its own agent and model: a local model for
  implementation, a cloud model for review, a shell command for the tests.
- **A worktree per task.** Every task works on its own branch in its own checkout.
  Parallel tasks never touch each other's files.
- **Optional, token-free GitHub pull requests.** **`spoolway stack`** commits, squashes,
  pushes and opens a pull request without an LLM call. Dependent tasks can become an ordered
  stack of pull requests. Where or whether you call the command is up to you.
- **Session reuse.** A step can resume its prompt's earlier conversation. It stops
  reusing when the model's window is too full.
- **Unattended runs.** When a task blocks, a configurable unblocker agent takes over.
  It pauses the task for you only when it cannot resolve the issue.
- **Trials.** Fork a group into one arm per task, each on its own pipeline, and compare
  the arms in eval.
- **Routines.** Keep the tasks you run more than once in `.spoolway/routines/`.
- **Jobs.** Run a routine on a cron schedule. A running dispatcher fires it.
- **Eval built in.** Every lane's spend and outcome go into a ledger. Raise a pipeline's
  `version:` when you change it, and compare pass rate and price across versions.

## Install

```
npm install -g spoolway
```

The package is a small wrapper around a prebuilt binary. It runs on Linux (x64, arm64,
musl) and macOS (Apple Silicon, Intel). `spoolway update` installs a newer release. The next
time you open spoolway in a project, it lists the files the new release rewrites and applies
them when you confirm. `spoolway whats-new` prints the release notes offline.

From source instead, in a clone of this repository:

```
cargo install --path .
```

…or, inside herdr:

```
herdr plugin install marvingygas/spoolway
spoolway herdr bind        # optional: four keys
```

Platform notes and requirements in full: **[Installation and setup](docs/installation.md)**.

## Quick start

### 1. Set up inside the project repo

```
spoolway init
```

`init` leaves `model: ""` on every agent step. Set a model on each one before you dispatch.
`spoolway pipeline check` names every step still missing one, and `spoolway doctor` checks
that everything the configured pipeline needs is present.

### 2. Create queueable tasks

Use `/spoolway-tasks` to cut an agreed plan or any other defined scope into Markdown task
documents. It assigns each task to a suitable pipeline and writes the documents to
`~/.spoolway/<label>-<id>/pending/`. You can create specialized pipelines for different kinds
of work. If you want help shaping the work first, use `/spoolway-plan`.

You can also create tasks with your own skills or scripts. This command prints the frontmatter
each task document must carry:

```
spoolway task contract
```

### 3. Queue

```
spoolway queue
```

<img src="docs/screenshots/queue.png" alt="the queue screen">

The queue groups pending tasks. Select a group and press `enter` to queue it.

### 4. Dispatch

```
herdr
spoolway dispatch
```

<img src="docs/screenshots/dispatch.png" alt="the dispatcher board">

Every task on the board is in one of a few states:

| State | Meaning |
|---|---|
| `queued` | Waiting for its dependencies and a free slot. |
| `waiting` | Held at a `serial:` step while another task's run of it finishes. |
| `running` | An agent is working the task's current step, or it has just moved there and a lane is starting. |
| `prompt` | A lane's pane is holding a permission prompt. Press a key in the pane. |
| `paused` | Waiting for you on purpose: a gated step, or an unresolvable issue. |
| `blocked` | A step reported a block and needs help. An unattended run hands it to the unblocker prompt. |
| `done` | Finished: the worktree removed, the task archived. |

### 5. Calibrate

The `/spoolway-calibrate` skill reads your archived tasks, their step-level evaluation results
and the spend ledger. It compares them with the prompts, pipelines and settings that produced
them. The comparison explains review failures, blocked sessions and wasted loops.

## A task is what travels the line, and you define it

```markdown
---
id: sessions
title: "feat(auth): add session tokens on top of login"
group: auth
pipeline: default
base: main
touches:
  - src/auth/**
depends_on:
  - login
---
## Context
## Intend
## Non-goals
## Acceptance criteria
## References
```

**The frontmatter is spoolway's, the body is yours.** spoolway never reads the body. The
agent works from it, and you own the skeleton it is written from.

## The pipeline is a file

- Mix agents and commands
- Set model and effort levels per step
- Reuse previous sessions until the context threshold is met
- Give the pipeline a `version:` and raise it when you change the pipeline, so eval can compare
  versions
- Run a command step only on a chain's root task with `first:`, or one task at a time with
  `serial:`

```yaml
# .spoolway/pipelines/default.yml
steps:
  - id: implement
    description: Write the code to satisfy the task's acceptance criteria.
    agent: pi
    prompt: implementer
    model: Ornith-1.5-35B-A3B
    session: true
    loop: 2
    on_pass: review

  - id: review
    description: Check the diff against the acceptance criteria and project standards.
    agent: claude
    prompt: reviewer
    model: claude-opus-5
    effort: high
    session: true
    on_pass: document
    on_fail: implement

  - id: document
    description: Bring the domain documents in line with what this task changed.
    agent: pi
    prompt: archivist
    model: Ornith-1.5-35B-A3B
    on_pass: handover

  - id: handover
    description: Create a stacked pull request without an LLM call.
    run: spoolway stack
    on_pass: done
```

The `/spoolway-config` skill can write and edit pipelines for you.

## Jobs

A job runs a routine on a schedule. It has three parts: a cron expression, a pipeline, and a
routine saved under `.spoolway/routines/`.

The dispatcher fires a due job at the start of its pass. While any job is enabled, the
dispatcher stays up on an empty queue. A `spoolway dispatch` left running overnight is all a
job needs.

```
spoolway jobs              # the screen: write, edit, pause, delete, or fire a job
spoolway jobs list         # every job, its schedule, and when it fires next
spoolway jobs run <name>   # fire one now, ignoring its schedule
```

<img src="docs/screenshots/jobs.png" alt="the jobs screen">

*The jobs screen. `n` asks for three things: the routine, the cron expression, and the
pipeline. A job fires once per matching minute. It skips a window while its previous run is
still in the queue. A window that passes while no dispatcher runs is not caught up later.*

A job is a few lines of TOML in `~/.spoolway/<label>-<id>/jobs.toml`. Put one in
`.spoolway/jobs.toml` inside the checkout to share it with the team.

```toml
[jobs.nightly-audit]
schedule = "0 3 * * 1-5"   # weekdays at 03:00, local time
pipeline = "impl_fast"
routine  = "nightly"       # a folder or a single .md under .spoolway/routines/
```

## Routines

A routine is a saved task or group of tasks that you can queue again. It lives in
`.spoolway/routines/`.
Press `s` on a group in the queue screen to save it there, and `r` to browse and queue what is
saved.

## Trials

A trial answers one question: which pipeline fits this task best? Press `t` on a group in the
queue screen, pick a pipeline per task, and tick any steps to skip. Every task becomes one arm
under its chosen pipeline, and all arms share one trial id. Compare them with
`spoolway eval --by task --trial <id>`. An arm never pushes a branch or opens a pull request. When
the last arm finishes, every arm's copy is removed. The source group and the ledger rows stay.

## Issue tracker

Event hooks can sync tasks with an issue tracker. Sample scripts for GitHub and Jira ship with
`spoolway init`; neither integration is required.

| Event | When it fires |
|---|---|
| `fetch` | `spoolway issue show <ref>` reads one issue out of the tracker |
| `open` | `spoolway queue add` opens a ticket per document |
| `queued` | A task arrives in the queue |
| `blocked` | A task comes to rest on `blocked` |
| `paused` | A task arrives on the persisted `paused` stage |
| `done` | A task finishes |

The sample GitHub flow creates one group issue and one child issue per task, comments when a
task blocks or pauses, and marks the task issue ready for review when the pipeline hands it to
a pull request. A sample GitHub Actions workflow closes that task issue after the pull request
merges, then closes the group issue when all of its children are done.

**The two shipped scripts are editable samples.** `spoolway init` copies `github.sh` and
`jira.sh` into `.spoolway/hooks/`, where they belong to your project. Change them, replace
them, or leave issue tracking disabled; `spoolway hook contract` describes the events and
environment available to any custom script.

See **[Issue Tracking](docs/configuration.md#issue_tracking--a-hook-fired-on-four-task-events)**.

## Configurable per project

- Unattended mode hands **`blocked`** tasks to a prompt you define, so the pipeline keeps
  running while nobody is watching.

```toml
[dispatch]
herdr_mode = "split"         # "split": a workspace per task; "grouped": one shared tab, a pane per task
worktree_root = ""           # where a task's worktree is cut; blank is ~/.spoolway/<label>-<id>/worktrees
lane_quiet = "15m"           # silence before a lane is reminded to report
auto_commit = true           # commit a lane's leftover work when its step settles

[unattended]
enabled = true               # the overnight switch
max_output_tokens = 0        # output-token ceiling for a run with nobody watching; 0 is none
max_cost_usd = 0.0           # dollar ceiling for the same run; 0 is none
blocked_agent = "claude"     # who staffs `blocked` when nobody is at the keyboard
blocked_model = "claude-opus-5"
blocked_effort = "medium"
blocked_session = true       # the unblocker carries its own earlier session forward
blocked_prompt = "unblocker"

[housekeeping]
update_check = true          # tell a person at a keyboard that a newer release is out
calibrate_window = "14d"     # how far back `/spoolway-calibrate` reads
retention_days = 30          # how long run records and archived tasks are kept; 0 keeps everything
price_max_age_days = 30      # how old the price table may be before `spoolway doctor` says so

[issue_tracking]
hook = ""                    # a script in .spoolway/hooks/, e.g. "github.sh"; blank runs none
project_key = ""             # handed to the hook verbatim, e.g. owner/repo
on_fail = "ignore"           # what a failing hook does: ignore it, or pause the task
key_in_names = false         # prefix branch and worktree names with the tracker's slug

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
```

See **[Configuration](docs/configuration.md)**.

## Eval every run

When a lane finishes, its transcript is read and written to a ledger: tokens, cost, wall
time, and what the lane reported. `spoolway eval` shows what each pipeline costs to run, by
pass rate and price.

```
spoolway eval
```

The eval screen opens on the lanes table, grouped by pipeline. `tab` switches to the directory
table, `f` filters, and `e` exports CSV. `spoolway eval --by version` compares a pipeline's
versions.

## Documentation

See **[Documentation](DOCS.md)**.

## License

MIT. See [LICENSE](LICENSE).
