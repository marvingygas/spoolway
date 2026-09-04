---
domain: concepts
covers: ["src/repo.rs", "src/platform.rs"]
---

# Concepts

The vocabulary spoolway uses, and how the pieces fit together. Everything else in this
documentation assumes these terms.

## Project

A project is a git repository that has been set up with spoolway. Setup creates a
`.spoolway/` directory in the checkout holding the configuration, the pipeline definitions
and the prompts — tracked, and committed alongside the rest of the repository. Everything
spoolway *writes while it runs* — the task queue included — lives elsewhere: at
`~/.spoolway/<basename of the checkout>/`, outside the repository entirely. See [Runtime
state](configuration.md#runtime-state).

Every spoolway command finds the project through git rather than by walking up the
filesystem for a `.spoolway/` directory. This matters because a task's worktree carries a
copy of the tracked `.spoolway/` — the config, the pipeline, the prompts — but has no queue
of its own to find: the queue was never in the checkout to copy. Resolving through git means
a command run inside a task worktree still reaches the project's real queue rather than an
empty one beside it.

One consequence worth internalising: **the queue belongs to the project, not to a
worktree**. Whatever branch a task was queued from — the same one for every task in a plan,
possibly a different one for the next — every worktree checked out from this project shares
one queue and one dispatcher.

The tracked control plane is the other way round. The pipeline definitions, the prompts
and the task skeletons travel with a branch, so a command reads *them* from the checkout it
is actually running in — the task's own worktree, when it is one — rather than from the
main checkout the queue is resolved against. A lane validates its own branch's files this
way, not the main checkout's copy of them. Where that answer differs from the main checkout's,
the commands that read the tracked control plane say so with one [`checkout:`
line](cli-reference.md#the-checkout-line) above their output; in the main checkout, where almost
every command runs, nothing extra is printed.

## Task

A task is one unit of work: a change small enough for one agent to finish in one sitting.
It lives as a Markdown file with YAML frontmatter in the project's queue directory.

- The **frontmatter** is spoolway's. It holds the task's id, the step it is sitting on, the
  branch and base it uses, its dependencies, its file globs, its round counters. Every
  scheduling decision is a lookup in it.
- The **body** is the project's. It is written once, at queue time, from a skeleton the
  project owns. spoolway never reads it — no heading in it is required and none is
  validated. It is what the agent reads to know what to build.

See [Tasks and the queue](tasks.md).

## Plan

A plan is a set of related tasks that share one `base:` — whatever branch you were standing on
when you queued them. Each task brings the domain documentation in line with its own change on
the way through, and hands its own change over the same way every other task does; see
[`spoolway stack` hands the change
over](pipelines.md#spoolway-stack-hands-the-change-over) for what that means.

A plan is also a page — an argued shape you read and approve before any of it is queued. The
breakdown itself is not on the page: it is a set of task documents waiting in the project's
pending directory, all naming the same `group:`, which is what the queue screen lists and
submits as one unit. spoolway reads those documents and never the page. The screen keeps a
group's row after it is submitted too — built from its documents in the queue directory once its
pending ones are gone, marked `queued` and hidden behind `h` — until every one of them is
archived. See [Planning](planning.md).

## Pipeline

A pipeline is a named graph of steps, defined as data in `.spoolway/pipelines/`. A task
names one with its `pipeline:` field; without one it takes the file's declared default.

Two pipelines ship as a **sample workflow**: `default`, for one change, and `bugfix`, for a
reproduce-first fix. They run side by side and a project can define as many more as it wants.

Every step a pipeline lists runs for every task on it — a pipeline carries no conditions, so
reading the file tells you the whole flow. A step that should run only sometimes is a second
pipeline, and a task picks between them.

They are a template, not the shape of the product. They exist so a project scaffolded five
minutes ago runs on its first pass, and so there is something concrete to read while working
out what you actually want — extend them, cut the steps you have no use for, or delete both
and describe the flow your team already runs. Nothing in the binary knows either file by name.
See [Converting a workflow you already
run](pipelines.md#converting-a-workflow-you-already-run).

See [Pipelines](pipelines.md).

## Step

A step is one node of a pipeline. It has an id, and the keys it carries say what it is:
`agent:` runs a prompt on a model, `run:` runs a command line, `end: true` finishes the task.

The step's id is written into the task's `stage:` field, so **renaming a step renames the
stage**. There is no `kind:` key: the keys already partition, so they are the discriminator.

Four stages belong to the dispatcher rather than to any pipeline, and no step may be named
after `queued`, `done` or `paused`. `blocked` is the fourth, and the one exception: every
pipeline gets a `blocked` step, materialised from `[unattended]`'s `blocked_*` keys where the
pipeline declares none of its own, and a pipeline may declare it to override five of those
keys — see [Staffing `blocked`](pipelines.md#staffing-blocked).

| Stage | What it is |
|---|---|
| `queued` | Where every task starts. It waits here for its dependencies and a worker slot |
| `done` | Finished. The worktree is removed, the local branch deleted, the file archived |
| `paused` | A gate's pass, held for a person. `spoolway resume <task>` sends it on, `spoolway resume --reject` sends it back — see [Gate](#gate) |
| `blocked` | Needs help. Attended, `spoolway resume <task>` resumes it at the step it stopped on. In an [unattended run](pipelines.md#unattended-runs) a lane is started here instead — every pipeline stages `blocked`, and that lane's pass carries the task on under `unattended.skip_blocked_lane` |

A step says where a task goes next with `on_pass` and `on_fail`. Prompts never learn this
routing: they report an outcome, and the pipeline resolves the destination.

## Lane

A lane is one running agent, working on one task, at one step. It is named
`<task> · <step>`, so `login · implement` is the implementer working on the `login` task.

Where a lane lives depends on the configured backend:

- **Under a multiplexer** (herdr or tmux), a lane is a real terminal pane. You can watch it, attach
  to it, type into it, and take over.
- **Headless**, a lane is a detached process writing to a log file. There is no pane, but
  the log outlives the lane, and the lane's own session can be resumed from any terminal.

A lane takes one turn. It does not resume: when the agent finishes and reports an outcome,
the lane is done and the task moves on. A lane that ends its turn *without* reporting an
outcome has usually stopped to ask a question, and is left exactly where it is, waiting for
an answer.

See [The dispatcher](dispatcher.md).

## Worker slot

Each agent profile declares a concurrency: how many of its lanes may run at once. Anything
sharing one local model server wants a real cap here, because those lanes are competing for
the same server's slots and context.

A step can opt out of consuming a slot with `slot: false`. Steps with no agent at all — a
`terminal` step, and a `run:` command step — never consume one.

## Prompt

A prompt is a Markdown prompt file, named by a step's `prompt:` field. It says what the
lane's role is and how it behaves. Nothing registers a prompt, so adding one is a file plus
the step that runs it — and nothing in the binary names one, so the six that ship are the
sample workflow's staffing rather than a set of roles spoolway understands.

A prompt file is Markdown a project owns outright — no markers, nothing generated, nothing
spoolway parses. At lane start the dispatcher composes it into a **system prompt** — its own
framing before it, this pass's policy after it — so no upgrade ever touches a prompt and
nothing a project's config decides can be lost by rewriting one.

See [Prompts](prompts.md).

## Reach

What a lane can open. The answer is short: whatever the person who started the dispatcher can
open. A lane runs as you, on your machine, with your credentials — no confinement, and no key
anywhere that changes it.

Five mechanisms used to sit here, and all five are gone. The **sandbox** confined every
local lane with Landlock and took a `[sandbox]` table of paths, ports and domains to widen.
An **`allow:`** list named git verbs a step was permitted — `push`, `merge`, `pr`, `rebase` —
refused inside the lane before each command ran; every capability it knew was a git verb, so
policing git was not a subset of that system, it *was* that system, and which git commands a
lane may run belongs to harness hooks rather than here. **`credentials: true`** outlived it
as a per-step grant of the forge paths, and went for a different reason: one question, "what
can this lane open", answered in two files. **`blocked_on_write`** and
**`blocked_on_overreach`** went last, and for a reason neither of the first three shared: both
only ever matched this project's own tracked files — `git status --porcelain`, what each read,
never lists a git-ignored path, and every runtime file either was written to protect was
git-ignored. A check that can only match the prompts and pipelines it was meant to guard
alongside is not a bound on reach at all; it is a check on what a lane hands back, run after
the write it might have caught has already happened.

Nothing left in spoolway bounds a lane's reach. Confinement, where a project wants any, is
the person's own agent settings — outside this repository entirely. See [what confines a
profile](agents.md#what-confines-a-profile) for the plain version of the trade.

## Outcome

What a step reports when it is done. Three route a task from any step, and a fourth is
reportable only from `blocked`:

| Outcome | Meaning | Where the task goes |
|---|---|---|
| `pass` | The step's work succeeded | The step's `on_pass` |
| `fail` | The work was attempted and did not meet the bar | The step's `on_fail`, or the pipeline's blocked step |
| `block` | Something outside this step's control is in the way | The pipeline's blocked step, regardless of the current step |
| `pause` | Nothing short of a person can clear this. Refused from any step but `blocked` | `paused`, holding the destination a pass would have reached |

Prompts report an outcome, never a destination. That is what makes a prompt movable
between differently-shaped pipelines without editing it.

## Gate

A gate is a step whose pass a person has to let past. The lane runs and reports like any
other — spoolway simply does not act on its `--pass` — but it is told that a person opens
this pane once the pass lands, so it leaves anything viewable running and writes them a short
account. The task lands on `paused` and waits for `spoolway resume`, which sends it on, or
`spoolway resume --reject`, which sends it back round by the step's `on_fail` route — or onto
`blocked` when the step declares none; `spoolway pipeline check` warns about a gate shaped that
way. A step declares `gate: true` and that is the whole of it — or a single task's document
names a step in its `gate_at:`, which holds that one task on the same terms without giving it a
pipeline of its own.

A gate holds in every run, unattended included. An unattended run skips the checks that exist
only to catch a lane going wrong with nobody there to escalate to — a `blocked` step resumes
itself, a broken launch backs off instead of piling up. A gate is not one of those: it is a
person's decision by design, and a run with nobody in it is a reason to wait longer for that
person, not a reason to make the decision without them.

Holding it outside the session is what makes it a mechanism rather than a request. It used to
be a paragraph in the lane's prompt asking it to stop and ask — and a model that read it,
decided the work was fine and reported a pass went straight through.

No step of any shipped pipeline declares one. A gate is for a step that changes something no
pull request would show anybody first — a deploy, a release, anything irreversible. Handing a
change over is not one of those: the pull request *is* the checkpoint, and asking in a pane
as well would be one checkpoint too many.

## Handover

Handing a change over is the moment it leaves the lane. **Nothing in a pipeline marks it as a
special kind of step** — `handover` is a command step like any other, distinguished only by
what it runs and where it sits in the flow. A `handover: true` flag survived here for a while
after the merge modes it once selected between were deleted, saying only which step it was, and
went once it was clear that nothing read the answer.

What actually happens is `spoolway stack`'s: commit what is uncommitted, squash to one commit,
push with a lease, and open the pull request against the branch the worktree was cut from — or
reuse the one already there. No model, and no rebase: by the time a task reaches `handover` its
worktree already sits on its dependency's branch, cut that way from the start. When git or `gh`
refuse something `spoolway stack` cannot resolve on its own, the step's failure routes to
`blocked`, where a person takes over. Swapping `handover`'s own `run:` line is how a project
changes what the mechanical half does. See [`spoolway stack` hands the change
over](pipelines.md#spoolway-stack-hands-the-change-over).

## How a change reaches the mainline

Task branches do not merge anywhere. Each one is pushed and opened as a pull request, stacked on
the task before it, so a plan can arrive as one ordered stack rather than as *n* independent
changes.

The ancestry is the point, not the naming. A dependent's worktree is cut from its dependency's
branch, not from the plan's `base:` — `task/sessions` sits directly on top of `task/login` from
the moment it is cut, not merely at the moment its pull request opens. That is what makes the
pull request `spoolway stack` opens for it based on the branch it actually sits on rather than on
a branch it happens to render correctly against: GitHub diffs from the merge base, so a pull
request pointed at the wrong ancestor still *looks* right and only breaks at the merge, going
`mergeable: false` where the two branches touched the same lines. Cutting from the dependency
in the first place is what keeps that conflict — if there is one — visible to whoever is working
on the dependent, rather than surfacing on whoever comes to land the stack days later; see [Why a
plan is a chain](planning.md#why-a-plan-is-a-chain) for the shape that makes it possible.

No task lands the stack. Every task's `handover` ends at its own green pull request, and a
person merges the chain bottom-up into the mainline — see [Closing a plan
out](planning.md#closing-a-plan-out).

Plans still do not have to take turns: every plan shares one queue and one dispatcher, whatever
branch each was queued from — see [Project](#project).

## The design rule underneath all of it

**The dispatcher uses no model.** Every judgement a prose dispatcher would make has a
mechanical answer that is cheaper and never drifts:

| Question | Answer |
|---|---|
| Is it stuck, or still thinking? | Time since its transcript was last written to |
| Is this a repeat failure? | A round counter in the task file |
| Is it blocked, or hung? | How long since its transcript was last written to |
| What should run first? | Position in the pipeline file; later steps outrank earlier ones |

A model may write text into a task file. It never decides a transition. This is also why the
dispatcher remembers nothing between passes: it reconciles from two sources of truth — each
task file's stage, and the live lane list — so it is safe to interrupt at any point.
