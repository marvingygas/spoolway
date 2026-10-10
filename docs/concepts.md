---
domain: concepts
covers: ["src/repo.rs", "src/platform.rs"]
---

# Concepts

The terms spoolway uses. Every other page assumes them.

| Term | Meaning |
|---|---|
| Project | A git repository spoolway runs on: one with a tracked `.spoolway/` directory (repo mode), or one a workspace elsewhere lists by path (home mode). |
| Task | One unit of work, stored as one Markdown file in the queue. |
| Plan | A group of tasks queued together from one branch. |
| Pipeline | A named graph of steps in `.spoolway/pipelines/`. |
| Step | One node of a pipeline: an agent running a prompt, or a command. |
| Lane | One running agent, on one task, at one step. |
| Worker slot | One unit of an agent profile's concurrency. |
| Prompt | A Markdown file a step runs its lane with. |
| Outcome | What a step reports: `pass`, `fail`, `block` or `pause`. |
| Gate | A step whose pass a person must let through. |

## Project

A project is a git repository set up with `spoolway init`. The checkout holds `.spoolway/`
with the config, the pipelines, the prompts and the templates. It is tracked in git.

What spoolway writes while it runs lives elsewhere, under `~/.spoolway/<label>-<id>/`. The
`<id>` is a short id stamped into the project's `.git` directory, which every branch,
subdirectory and linked worktree of one clone shares. The `<label>` is a cleaned-up form of the
checkout's name, cut to 64 characters. A name that is blank gets the label `project`. The home records the same id and its checkout in its own `project.toml`. Every
command checks the two against each other and refuses when they disagree. When a checkout has
lost its stamp and the home still records it, the refusal compares this clone's first commit
with the one the home recorded. It then says whether to restore the stamp by hand or to delete
the `project.toml` entry and run `spoolway init` for a fresh id. A clone or home with no
recorded first commit gets the fresh id first. See [Runtime
state](configuration.md#runtime-state).

Commands find the project through git. A command run inside a task worktree reaches the
project's real queue. The queue belongs to the project, so every worktree and every branch
shares one queue and one dispatcher.

A command reads `.spoolway/` from the checkout it runs in. When that differs from the main
checkout, the command prints a [`checkout:` line](cli-reference.md#the-checkout-line) first.
A linked worktree on a branch with no `.spoolway/` reads the main checkout's instead, and the
line names the main checkout. `spoolway sync` refuses to write a setup into such a worktree.

There is one project per clone, and its `.spoolway/` sits at the top of the repo. A
`.spoolway/` anywhere below the top is refused by name, and the refusal says where the setup
belongs. Spoolway does not move or delete it for you.

## Home mode

A checkout with no `.spoolway/` can still be a project. This works when some workspace under
`~/.spoolway/` lists the checkout's path. Home mode refuses a checkout with no git repository
behind it. A workspace is an ordinary folder there, holding a
`config/` and a `project.toml`. The `project.toml` lists a `clones` entry for each checkout
that shares it: the checkout's absolute path, and the name of a `dispatchers/` subfolder that
holds its queue, archive, worktrees and lock.

`spoolway init --setup home` puts a checkout into home mode. With no workspace yet, or with
`--workspace new`, it creates one: `~/.spoolway/<label>-<id>/`, the same name shape a repo-mode
home takes, with a `dispatchers/<name>/` for this checkout and a `config/` scaffolded the same
way `spoolway init` scaffolds a repo-mode checkout's `.spoolway/`. See [Scaffolding a
project](installation.md#scaffolding-a-project). With `--workspace <name>`, it adds this
checkout to that workspace instead, with a dispatcher folder of its own — the checkout's
directory name, with `-2` added when that name is already taken — and leaves the workspace's
`config/` exactly as it is. When exactly one of that workspace's entries shares this checkout's
root commit and its folder no longer exists, joining takes over that entry instead of adding a
new one, so this checkout carries on its queue, archive and worktrees without asking. A checkout
with no root commit of its own, such as a shallow clone, never takes over an entry. Nothing is
written into the checkout or its `.git` either way, and skills install into the coding agent's
user folder instead of the project's own.

`--setup home` is refused when a branch of the repository tracks a `.spoolway/` of its own,
even from a checkout on some other branch that carries none: switching to that branch would find
a tracked setup in conflict with the home-mode one. The default branch comes from `origin/HEAD`,
then `init.defaultBranch`, then `main` or `master`. When none of these names a branch, any local
branch that tracks `.spoolway/` is refused. The refusal names that branch.

`--setup home` is also refused on a checkout whose repo-mode home still holds queued tasks,
because home mode starts an empty queue. The refusal names the tasks and the steps that clear
them.

A home-mode checkout reads its config, pipelines and prompts from the workspace's `config/`.
Its runtime state lives in the workspace's `dispatchers/<dispatcher>/`, in place of the
`~/.spoolway/<label>-<id>/` a repo-mode project uses. Nothing is read from or written to the
checkout's own `.git`.

`spoolway init --force` in one clone rewrites that shared `config/` for every clone in the
workspace, so it names the other clones that share it before rewriting it.

A checkout that has both a tracked `.spoolway/` and a clone entry in some workspace is refused.
The error names both paths.

A workspace's `project.toml` that cannot be read or parsed stops only a checkout it might have
listed. Every other command prints one note naming the file and carries on. A checkout that
matches no readable workspace refuses instead, naming the file, because the file might be the
one that would have listed it. `spoolway init` refuses the same way instead of falling back to
repo mode for a checkout the file might list. The refusal says to repair the file, or to move the
whole workspace folder out of `~/.spoolway/`. [`spoolway config
path`](cli-reference.md#spoolway-config-show--list--path--get-key--set-key-value--edit) answers
anyway. It prints `mode: null` and the workspace list instead of refusing. That list is what
`spoolway init --workspace <name>` needs next.

A workspace whose `config/` is missing — deleted, or left behind by a join that failed before
writing it — refuses a new join, naming the missing path. A clone the workspace already lists
refuses every command the same way, naming the missing path, instead of falling through to the
generic not-found message. `spoolway init` there refuses too, instead of writing a fresh
`config/` into the workspace.

A checkout listed more than once, in one workspace's `clones` or across several, is refused,
naming every file and entry. A clone's `dispatcher` must be one plain name, with no path
separator and no `..`. A workspace file holding any other value refuses only the checkouts it
lists, naming the file, the value and the entry. Every other command prints one note naming the
file and carries on. Nothing can join or move into such a workspace. Two entries in one file
may not name the same dispatcher folder. Both entries' checkouts are refused, naming both
entries. A clone's path must be valid UTF-8; `spoolway init` refuses a checkout path that is
not, rather than storing a path that can never match it again.

A checkout whose folder moved or was deleted still leaves its old entry in a workspace, pointing
at a path that no longer exists. Running `spoolway init --workspace <name>` from the checkout
that replaces it re-attaches that entry, because joining a workspace takes over a gone entry
that shares this checkout's root commit instead of adding a new one. The "no spoolway project
found" error prints this exact command, shell-quoted, for every workspace where that takeover
would actually happen, naming the path the entry was at.

A clone also moves between workspaces through `spoolway init`'s own menu, or `--workspace
<other>` on a checkout a workspace already lists: picking another workspace moves the checkout,
carrying its dispatcher folder — queue, archive and worktrees — along. It refuses, writing
nothing, while any of the checkout's tasks holds a worktree, naming each one, and for a
workspace of another repository, with no flag to force it. A checkout no workspace lists may
not join a workspace of another repository either.

spoolway never deletes a workspace folder. A workspace the move leaves with no checkout listed
is kept. The move prints the folder's path and says to remove it by hand. A workspace that lists
no checkout can be joined again.

`spoolway doctor` names which mode a project runs in, repo mode or home mode. It also prints a
note for each workspace under `~/.spoolway/` that lists no checkout. Another note names each
project home whose recorded checkout no longer exists, so you can delete the folder.

## Task

A task is a Markdown file with YAML frontmatter in the queue directory. The frontmatter is
spoolway's and holds every scheduling fact. The body is the project's, and only the agent
reads it. See [Tasks and the queue](tasks.md).

## Plan

A plan is a group of tasks that share one `base:` branch and one `group:`. The queue screen
lists and queues a group as one unit. A plan page is an HTML file a person reads to approve
the shape. spoolway never reads the page. See [Planning](planning.md).

## Pipeline

A pipeline is a named graph of steps in `.spoolway/pipelines/`. A task names one in its
required `pipeline:` field.

Two pipelines ship: `default` for one change, and `bugfix` for a reproduce-first fix. They are
samples. Edit them, or replace them with the flow your team runs. See
[Converting a workflow you already run](pipelines.md#converting-a-workflow-you-already-run).

## Step

A step is one node of a pipeline. Its keys say what it is: `agent:` runs a prompt on a model,
`run:` runs a command. The step id is written into the task's `stage:` field.

Four stages belong to the dispatcher. No step may be named `queued`, `done` or `paused`.
Every pipeline gets a `blocked` step from `[unattended]` unless it declares its own. See
[Staffing `blocked`](pipelines.md#staffing-blocked).

| Stage | Meaning |
|---|---|
| `queued` | Waiting for its dependencies and a worker slot. |
| `done` | Finished. The worktree is removed, the branch deleted once pushed, the file archived. |
| `paused` | Held for a person after a gate. `r` on the board sends it on, or to a step you pick instead. |
| `blocked` | Needs help. `spoolway resume` continues it. An [unattended run](pipelines.md#unattended-runs) starts the unblocker lane instead. |

A step names where the task goes next with `on_pass` and `on_fail`. Prompts report an outcome
and never a destination.

## Lane

A lane is one running agent on one task at one step. It is a terminal pane in herdr that you
can watch, type into and take over. It is named `<task> · <step>`, so `login · implement` is
the implementer working on `login`.

A lane takes one turn. When it reports an outcome, the task moves on. A lane that stops
without reporting is usually asking a question. See [The dispatcher](dispatcher.md).

A lane that has reported stays open, idle, in its own pane until the task is done. See
[Finished lanes keep their pane](dispatcher.md#finished-lanes-keep-their-pane).

## Worker slot

Each agent profile declares a `concurrency`: how many of its lanes run at once. A step with
`slot: false` takes no slot. Command steps take none.

## Prompt

A prompt is a Markdown file named by a step's `prompt:` field. It says what the lane's role is.
The dispatcher composes it into the lane's system prompt. See [Prompts](prompts.md).

## Outcome

| Outcome | Meaning | Where the task goes |
|---|---|---|
| `pass` | The step's work succeeded. | The step's `on_pass`. |
| `fail` | The work did not meet the bar. | The step's `on_fail`, or `blocked`. |
| `block` | Something outside the step is in the way. | `blocked`. |
| `pause` | Only a person can clear this. Allowed from `blocked` only. | `paused`, with the destination a pass would reach. |

## Gate

A step with `gate: true` stops the task on `paused` after its pass. A single task can do the
same with `gate_at: <step>` in its task, and a `gate_at` catches whatever the step reports:
pass, fail, block, or a loop-max bound for `blocked`. A `gate_at` is spent when it fires. The
task pauses once, and a later report from the same step runs straight through unless something
writes a fresh `gate_at`. `spoolway resume` sends a caught pass or fail on by `on_pass`, and
sends a caught block or loop-max to `blocked`, the same place it would have reached unheld.
`blocked` has no `on_pass`. A `gate_at: blocked` that caught the unblocker's pass sends the task
to the `on_pass` of the step it stopped at, and hands a command step back to itself.
A command step has no lane to report, so its exit is the report: `gate: true` holds a passing
exit, and `gate_at` holds any exit. `spoolway resume` sends a held failing command exit down the
step's `on_fail`.
Picking another step in the board's resume picker sends it to a step you name instead.

A gate holds in unattended runs too. Use it for a step no pull request shows first, such as a
deploy or a release.

## Branches and pull requests

A dependent task's worktree is cut from its dependency's branch. The optional `spoolway stack`
command can turn that chain into one stack of GitHub pull requests without calling a model or
spending tokens. spoolway never merges branches. Where or whether you call the command is up
to you. See [`spoolway stack`](pipelines.md#spoolway-stack).

## The design rule underneath all of it

The dispatcher uses no model. Every decision is a lookup.

| Question | Answer |
|---|---|
| Is the lane stuck, or still working? | Time since its transcript was last written to. |
| Is this a repeat failure? | A round counter in the task file. |
| What runs first? | Position in the pipeline file. Later steps go first. |

The dispatcher keeps no state between passes, so it is safe to interrupt at any point.
