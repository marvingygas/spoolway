# Documentation

Every page of the spoolway documentation, grouped in the order most people meet them. The
pages themselves live in [`docs/`](docs/README.md) and each one can be read on its own.

## Start here

| Page | What it covers |
|---|---|
| [Concepts](docs/concepts.md) | The vocabulary — projects, tasks, plans, pipelines, steps, lanes — and how the pieces fit together |
| [Installation and setup](docs/installation.md) | Installing, scaffolding a project, keeping it current, and what each platform supports |

## Doing work

| Page | What it covers |
|---|---|
| [Tasks and the queue](docs/tasks.md) | Writing and queueing work, dependencies, conflicts, and what a task file holds |
| [Planning](docs/planning.md) | From one goal to an approved breakdown, the queue screen in full, trials and routines |
| [The dispatcher](docs/dispatcher.md) | Scheduling, the board, gates, escalation, backends, and taking over a lane |
| [Jobs](docs/jobs.md) | Firing a routine on a cron schedule from the dispatcher's own pass |

## Shaping the system

| Page | What it covers |
|---|---|
| [Pipelines](docs/pipelines.md) | The pipeline file in full: steps, routing, and converting a workflow you already run |
| [Prompts](docs/prompts.md) | The roles, the contract they are written against, what they are handed, and writing your own |
| [Agents and models](docs/agents.md) | Agent profiles, supported agent kinds, local models, effort |
| [Documentation and templates](docs/documentation.md) | Domain documents and task skeletons, and restyling either |

## Safety, visibility, cost

| Page | What it covers |
|---|---|
| [Cost accounting](docs/cost.md) | The ledger, model pricing, and reading what a run came to |
| [Comparing versions](docs/eval.md) | What editing a prompt or a pipeline did to what the work costs, and reading a trial's arms |
| [Collaboration](docs/collaboration.md) | Handing in-flight work to a colleague, and adopting theirs |

## Reference

| Page | What it covers |
|---|---|
| [Configuration](docs/configuration.md) | Every setting in the config file, what it means, and how to edit it |
| [CLI reference](docs/cli-reference.md) | Every command, subcommand and flag |
| [Testing](docs/testing.md) | The suites, the fixtures, and running them with or without models |

## The shape of the thing, in one paragraph

You write a task — a goal, some non-goals, acceptance criteria — and queue it. The dispatcher
gives it a git worktree and starts an agent in a lane to work it. When that agent reports
success the task moves to the step the pipeline routes a pass to; when it fails, to whichever
step the file says a failure goes. Each step names an agent profile and a prompt, and nothing
about the flow is decided anywhere but that one file. The dispatcher itself uses no model at
all — every decision it makes is a lookup in a task file or in the list of live lanes.
