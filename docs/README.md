# spoolway documentation

spoolway is a command-line tool for running coding agents as a pipeline. You describe the
flow once, in a file; a background dispatcher moves every piece of work through it, in real
terminal panes or headless processes, and asks you only where a decision is actually yours.

This is the full technical documentation. Each page covers one domain of the system and can
be read on its own, but the order below is the order most people meet them in.

## Start here

| Page | What it covers |
|---|---|
| [Concepts](concepts.md) | The vocabulary: projects, tasks, plans, pipelines, steps, lanes, and how they relate |
| [Installation and setup](installation.md) | Installing, scaffolding a project, keeping it current, and what each platform supports |

## Doing work

| Page | What it covers |
|---|---|
| [Tasks and the queue](tasks.md) | Writing tasks, queueing them, dependencies, conflicts, routine folders, and what a task file holds |
| [Planning](planning.md) | Turning a goal into a plan, queueing it, and closing it out onto the mainline |
| [The dispatcher](dispatcher.md) | Running the pipeline: scheduling, gates, escalation, backends, and taking over a lane |
| [Jobs](jobs.md) | Firing a routine on a cron schedule from the dispatcher's own pass |

## Shaping the system

| Page | What it covers |
|---|---|
| [Pipelines](pipelines.md) | The pipeline file in full: steps, routing, and converting a workflow you already run |
| [Prompts](prompts.md) | The prompt files steps run, the contract they are written against, and writing your own |
| [Agents and models](agents.md) | Agent profiles, supported agent kinds, session pinning and cache warmth, local models |
| [Documentation and templates](documentation.md) | Domain documents and task skeletons, and how to restyle either |

## Safety, visibility, cost

| Page | What it covers |
|---|---|
| [Cost accounting](cost.md) | The usage ledger, model pricing, and reading what a run came to |
| [Comparing versions](eval.md) | What editing a prompt or a pipeline did to what the work costs |

## Reference

| Page | What it covers |
|---|---|
| [Configuration](configuration.md) | Every setting in the config file, what it means, and how to edit it |
| [CLI reference](cli-reference.md) | Every command, subcommand and flag |
| [Testing](testing.md) | How spoolway is tested: the suites, the fixtures, and running them with or without models |

## The shape of the thing, in one paragraph

You write a task — a goal, some non-goals, acceptance criteria — and queue it. The
dispatcher gives it a git worktree and starts an agent in a lane to work it. When that agent
reports success the task moves to the step the pipeline routes a pass to; when it fails, to
whichever step the file says a failure goes. Each step names an agent profile and a prompt,
and nothing about the flow is decided anywhere but that one file. The dispatcher itself uses
no model at all — every decision it makes is a lookup in a task file or in the list of live
lanes.

## What ships is a sample, not the product

Two pipelines and six prompts come in the box — implement, review, document, summarise a pull
request, unblock, plus a reproduce-first variant for bugs. They are there so that a project
scaffolded a minute ago runs on its first pass, and so that there is something concrete to read
while working out what you want. **They are a template.** Extend them, cut the steps you have no use for, or delete
both files and describe the workflow your team already follows; nothing in the binary knows
any of them by name, and nothing degrades when they are gone.

Most projects are in the third case, and it is a procedure rather than a blank file:
[Converting a workflow you already run](pipelines.md#converting-a-workflow-you-already-run).
