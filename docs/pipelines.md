---
domain: pipelines
covers: ["src/pipeline.rs", "src/command_step.rs", "assets/pipelines/**"]
---

# Pipelines

A pipeline is a named graph of steps. Pipelines are data, not code: they live in
`.spoolway/pipelines/`, and reshaping a flow is a file edit with no prompt changes and
nothing to rebuild.

## One file each

A pipeline is one file, and the file name is the pipeline name.

```
.spoolway/pipelines/
  default.yml        # one unit of work: implement, review, document, hand over
  bugfix.yml         # reproduce first, fix, run the repro again
  hotfix.yml         # yours, the moment you write it
```

```yaml
# .spoolway/pipelines/default.yml
steps:
  - id: implement       # the first step is where a task starts
    agent: pi
    prompt: implementer
    session: true
    on_pass: review
    on_fail: blocked
  # …
```

There is no preamble. `queued`, `done` and `paused` are the dispatcher's own states, never a
step to declare, and where a task starts is the first step in the list. `blocked` is the
dispatcher's too, and the one a pipeline may declare — to restaff it, not to opt into it; see
[Staffing `blocked`](#staffing-blocked).

Nothing registers a pipeline: adding one is writing a file, and deleting one is deleting a
file. A `name:` key inside is refused — the file name is the name, and a second spelling of
it is one that can disagree.

Tracked, so it travels with a branch. A command reads `.spoolway/pipelines/` from the
checkout it is actually running in — a task's own worktree, when it is one — rather than
from the project's main checkout, so a lane sees its own branch's pipelines even before
they are merged anywhere. See [Project](concepts.md#project).

A task picks its pipeline with its own `pipeline:` field. Which one it gets when it names
none is `dispatch.default_pipeline` in the config, because naming the default is a statement
about the *set* and no single file can make it. (The old single `pipeline.yml` is no longer
read; a project still carrying one is told exactly how to split it.)

Two pipelines ship: `default`, for one change, and `bugfix`, for a reproduce-first fix.
`default` is what `dispatch.default_pipeline` ships pointing at, hence its name.

**Every step a pipeline lists runs for every task on it.** There is no condition to read and no
key that carries one: what the file says is what happens, in order. A step that should only run
sometimes is a second pipeline file, and a task picks between them with its `pipeline:` field.

**They are a sample workflow, not the workflow.** They ship so that a project scaffolded five
minutes ago runs on its first pass, and so that every key on this page is demonstrated
somewhere you can read it rather than only described here. Extend them, cut the steps you have
no use for, or delete both files and write your own — nothing in the binary knows either by
name, and nothing degrades when they are gone. Where to start on the last of those is
[Converting a workflow you already run](#converting-a-workflow-you-already-run).

There used to be a third, `merge-and-doc`, queued by `spoolway plan close` as a task of its
own. It folded into `default` as two conditional steps, and then into the steps every task
already runs. What that deleted is worth naming: the file, the command, its base derivation,
the `SPOOLWAY_MERGE_INTO` variable, the special case for a lane whose worktree *is* the main
checkout, and the `when:` key that carried the condition. Each task documents its own diff, in
its own branch, inside its own pull request; the plan's ending is the last step of the last
task still open — see [Closing a plan out](planning.md#closing-a-plan-out).

## Per-pipeline keys

| Key | Default | Meaning |
|---|---|---|
| `description` | — | What this pipeline is for, in a few sentences — read to choose between pipelines |
| `task_template` | the pipeline's own name, falling back to `default` | Which task skeleton a task queued here is written from |
| `steps` | — | The flow, in order. The first one is where a task starts |

Five of these used to exist and no longer do. `entry:` is the first step — it already was,
in every pipeline anyone had written, and the list is ordered for scheduling anyway, so the
two meanings agree. `blocked:` named a step every pipeline declared identically. `merge:` and
the pipeline-level `gate:` are both gone with the thing they configured; see
[`spoolway stack` hands the change over](#spoolway-stack-hands-the-change-over) and
[Gates](#gates). `blocked_on_write:`
went last, retired along with the check that read it — see [Reach](concepts.md#reach). A
pipeline file still naming it keeps loading: the key is absorbed and ignored, the same way
`config.toml` absorbs it — see [Retired: `blocked_on_write` and
`blocked_on_overreach`](configuration.md#retired-blocked_on_write-and-blocked_on_overreach).

There is no `worktree:` key, and deliberately. Every lane runs in a worktree; the only
question is whether spoolway cut it or borrowed one, and git answers that — see
[Whose worktree](#whose-worktree).

`task_template` is rarely needed: writing `<pipeline>.md` beside the other skeletons is the usual
way to give a pipeline a shape of its own, and costs no configuration at all.

## Per-step keys

| Key | Default | Meaning |
|---|---|---|
| `id` | — | Written to the task's `stage:`. Renaming a step renames the stage |
| `description` | — | One-liner shown when reading status and the pipeline |
| `agent` | — | Profile from the config's agent table. **Its presence makes this an agent step** |
| `run` | — | A command line. **Its presence makes this a command step** |
| `end` | `false` | **`true` makes this a terminal step**: the task stops here |
| `prompt` | the step id | Prompt file under the prompts directory |
| `model` | — | The model this step runs, by name. **Required on every agent step** — spoolway names none of its own, so `pipeline check` and `doctor` refuse a project's own step whose model is missing or blank. The shipped pipelines carry an explicit blank (`model: ""`) on every agent step as the starting point a fresh project fills in; see [The shipped `default` pipeline](#the-shipped-default-pipeline) |
| `effort` | — | How hard `model:` thinks, handed straight to the flag its agent kind carries an effort on. A free string — see [Effort](agents.md#effort) — dropped entirely on a kind with no such flag. An explicit blank (`effort: ""`) is the same as absent: no flag is sent, which is how a fresh scaffold writes the choice down without choosing it
| `skills` | — | Skills invoked at the top of this step's opening prompt, one `/name` per line in declaration order. Comma-separated, leading slash optional, names only |
| `session` | `false` | Whether this step resumes its prompt's earlier conversation on the task instead of opening fresh. How far one may carry is the profile's — see [A step that carries its own session](dispatcher.md#a-step-that-carries-its-own-session) |
| `slot` | `true` | Whether running this consumes one of the profile's lanes. The profile's budget only: a model's own `slots` and `exclusive` hold for every step naming it, `slot: false` included — see [Dispatcher](dispatcher.md) |
| `gate` | `false` | Hold this step's pass for a person: the task lands on `paused` until `spoolway resume` — see [Gates](#gates) |
| `on_pass` | — | Where a success goes. Absent means the task stays put |
| `on_fail` | `blocked` | Where a failure goes |
| `loop` | no limit | Escalate after this many *arrivals* here from a given step — a lap of the loop. A number, or a map keyed by that step |
| `on_loop_max` | `on_pass` | Where a task goes when `loop` is spent |
| `background` | `false` | Command steps only: let the task move on while the command keeps running |
| `headless` | `false` | Command steps only: run detached, with no pane, instead of in a pane of its own |
| `last` | `false` | Command steps only: run this only on the last task of a chain — see [`last:` — a step the chain runs once](#last--a-step-the-chain-runs-once) |
| `timeout` | `30m` | Command steps only: how long the command may run before it is killed |
| `cleanup` | `false` | Terminal steps only: remove the worktree, delete the local branch, archive the task file. Reaching `done` already does this |

A step with id `blocked` routes itself — a pass is read from the step the task blocked on,
a fail or a block parks the task on `paused` for a person instead — so `pipeline check` refuses
`on_pass`, `on_fail`, `on_loop_max`, `gate` and `end` on it. No pipeline file declares one in
the ordinary case at all: `blocked` is configured in `config.toml`, not written into the
file, and a pipeline that
does declare `- id: blocked` may only name five of the keys above — `agent`, `model`, `effort`,
`session`, `prompt` — every other key refused by the same name. See
[Staffing `blocked`](#staffing-blocked).

**The same table is in the file.** Every pipeline file opens with this list of keys, fenced between
`# >>> spoolway >>>` and `# <<< spoolway <<<`, one row per value a key can take with its default
marked. It is one line each, and this page is where the reasoning is; `spoolway update` rewrites
what is between those markers, so the copy in your pipeline is this binary's and not whichever
release wrote the file. A pipeline you wrote yourself has no markers and is never written to —
paste the pair in to opt it in.

### The keys are the discriminator

There is no `kind:`. What a step *is* falls out of what it carries:

- `agent:` present → it runs a prompt on a model.
- `run:` present → it runs a command line.
- `end: true` → the task stops here.

One key per step type, each carrying its own configuration, and no second key restating what
the first already said. `kind:` was that second key, and the validation around it was nothing
but a consistency check between the two — along with every "you said kind X but wrote keys for
Y" error that check existed to produce.

**Why `end:` is declared rather than inferred.** A step with no `agent:`, no `run:` and no
transitions *is* structurally terminal. But so is a step whose `agent:` was mistyped, and
reading that as an ending would turn a typo into a task that silently stops. Endings are worth
declaring.

**`end`, not `terminal`.** The old set mixed two nouns and an adjective. A verb-shaped flag
reads correctly beside `cleanup: true`, which is a terminal's key anyway.

### The four states nobody declares

| Stage | What it is |
|---|---|
| `queued` | Where every task starts. It waits here for its dependencies and a worker slot, and the dispatcher promotes it to the first step when `depends_on` is satisfied |
| `done` | The task finished. Its worktree is removed and its file is archived. The local branch spoolway cut for it is deleted too, unless something still queued names it in `depends_on`, in which case it is kept until that stops being true. The published branch is left alone — deleting a merged pull request's head branch is a repository setting the forge applies itself |
| `blocked` | The task needs help: something stopped it. Attended, `spoolway resume <task>` resumes it at the step it stopped on. Unattended, a lane staffs it instead, configured by `[unattended]`'s `blocked_*` keys in config.toml; see [Staffing `blocked`](#staffing-blocked) |
| `paused` | The task passed a `gate:` step and needs a person to let it past. `spoolway resume <task>` sends it on. Nothing is wrong with it, and its dependents wait quietly — see [Gates](#gates) |

`queued`, `done` and `paused` may not be named by a step, and every pipeline may route to
`done` and `blocked` without declaring either. `blocked` is no exception to that: no pipeline
declares it in the ordinary case, because `Pipelines::assemble` materialises a `blocked` step
for every pipeline, from `[unattended]`'s own `blocked_*` keys, before validation runs. A
pipeline may still write `- id: blocked` itself — not to opt into staffing, which every
pipeline already has, but to override up to five of that step's keys: `agent`, `model`,
`effort`, `session`, `prompt`. Whichever it leaves out still falls back to config.

`queued` used to be a `kind: wait` step. In every shipped pipeline it appeared exactly once,
was always first, was always called `queued`, was always the entry, and nothing ever routed
back to it — a built-in state wearing a step's clothes. Worse, the dependency gate lived
*inside that step's arm*, so `depends_on` was honoured only because every pipeline happened to
open with one; a pipeline whose entry was an agent step would have ignored dependencies
silently. Built in, the gate is unconditional.

`done` and `blocked` are reserved for a sharper reason. The model was already binary: anything
terminal that is not `blocked` releases dependents. So a third declared ending —
`superseded`, `rejected`, `abandoned` — would have let downstream work proceed on a task that
never finished, silently. Declaring terminals only ever created room to declare one that
misbehaves.

### Order is priority

Steps further down the file are scheduled first, and that is the whole of it: work that is
nearly done finishes before anything new starts, so a task at the `handover` step beats a fresh
one for a slot. There is no override key — a project that wants another order writes its steps in
that order.

There was a `priority:` on a step once. Nothing ever read it: the order was always the
position, and the key only claimed otherwise. It is gone rather than implemented, so a file
still carrying one is refused by `pipeline check` with the field named — which is the news a
line that never did anything deserves.

## Routing

A step names where a task goes on success and on failure. Prompts never learn this shape:
they finish by reporting an outcome — pass, fail or block — and the pipeline resolves the
destination.

```yaml
  - id: review
    agent: claude
    prompt: reviewer
    model: claude-opus-5
    effort: high
    on_pass: document
    on_fail: implement
```

That is the whole of why a prompt is movable: reshaping the flow is a config edit, and the
prompt files do not change.

`loop` bounds a cycle. The shipped `review` step allows one lap back to `implement` before
escalating, so a task bouncing between the two carries on rather than looping forever.

**A round is a lap, not a conversation.** Both halves of the shipped review loop carry
`session: true`, so a reviewer coming back for a second look resumes the session it opened
the first time instead of reading the whole diff cold — and so does the implementer it sent
the work back to. That used to matter to the count —
only a cold start spent the budget, so the bound was really a bound on conversations, and the
same three visits could cost wildly different tokens depending on which ones happened to open
fresh. Now every arrival counts the same, whether or not a lane there opened a conversation of
its own: what still bounds a conversation's size is `agents.<profile>.session_reuse_ctx`,
entirely separate from this.

It is counted **per route in**, because a step that several loops come back to is several
loops. In the shipped pipeline `review` fails back to `implement`, and `implement` always
returns through `review` — so `review` is where the lap is counted: a budget on the step that
*answers* a loop only sees the routes that happen to enter it directly, while the step that
*asks* the question sees every lap that ever comes back to it.

Where one loop deserves a different number, name the route instead of writing one number:

```yaml
  - id: review
    session: true
    loop:
      fix: 3
      handover: 5
    on_loop_max: blocked
```

A route left out of that map is unbounded, and a name that never routes to this step is
refused — a typo there would read as a bounded loop while bounding nothing.

### Where a spent loop goes

`on_loop_max` names it. Absent, a spent loop carries the task on to `on_pass` — exactly as an
ordinary pass would: a review that has argued three times has said what it has to say, and
running it a fourth time buys nothing, so the change goes through rather than parking the task
for a person to re-run the same argument. What happened is written to `## Status Log`, naming
the round count, so nothing about it is silent.

`on_loop_max: blocked` is the other useful answer, for a loop whose output nobody else will
ever read — a rebase that will not converge, or a tree that will not build, is not a
quality-versus-cost trade anybody wants made for them.

An [unattended run](#unattended-runs) takes this exit like any other: `on_loop_max` is a
destination the pipeline named, and most of what it can name needs nobody — carrying on is
the default, and `on_loop_max: handover` reads the same way. An exit that resolves to
`blocked` binds no differently: every pipeline [staffs it](#staffing-blocked) in an unattended
run now, so the budget lands there exactly as it would in an attended run, with a lane rather
than a person reading it.

### Every loop must be bounded

A pipeline is refused at load if any cycle in it has no `loop` anywhere along it, or if the
only bound it has leads straight back inside — a spent loop whose exit is still part of the
same cycle bounds nothing, because the task just goes round again however it got there.
Reaching a terminal step is not enough on its own: `review → implement → review` reaches
`done` on every passing run and still spins forever on two agents that keep reporting fail,
and the first a person hears of it is the bill. Bounding one route on the cycle so its exit actually
leaves is enough — that route escalates, and the task leaves too.

This is what makes adding a prompt of your own safe. Drop a step into a loop, and if you
forget its limit — or put it on a step whose own exit re-enters the loop — `spoolway pipeline
check` names the steps involved and, where it can tell, the step to move `loop:` to instead,
rather than letting the first task through it run all night.

## Unattended runs

A run can be told to stop for nobody. Set `enabled = true` under `[unattended]` in
config.toml, or pass `--unattended` to a single `spoolway dispatch`. What happens to a task
that would otherwise park on `blocked` — a lane's own `--block` report, a `fail` with nowhere
left to route, a silent lane the dispatcher gave up on, a dead launch, a spent `loop` whose
exit resolves to `blocked` — is always the same: the task lands on `blocked` and a lane is
started there, staffed by whatever `[unattended]` configures — see
[Staffing `blocked`](#staffing-blocked). Nothing is skipped or handed back on this road:
`loop` on the step the budget escalated out of binds exactly as it would in an attended run,
because `blocked` is a lane now, not a person to skip past.

That lane is not a retry from cold. It is exactly what [`spoolway resume`](cli-reference.md)
does by hand, through the same code once it clears the block: the lane that stopped is
**continued**, holding everything it had already read and worked out, and the loop budgets
out of the step it is returning to are handed back. What it reads on the way back in says
plainly that nobody has been and nothing has changed, and asks it to reproduce what stopped
it, clear the smallest thing in the way, and prove the path is clear before carrying on.

One more thing stops meaning anything in an unattended run, on **either** kind of
pipeline, because parking there is itself a request for a person to unpark it:

- **The launch ceiling**, which parks a task whose lane keeps dying at launch. Unattended it
  becomes a backoff instead: the task keeps its place and is retried on a doubling delay, up
  to an hour, so a broken agent install costs a slow retry rather than a lane spawned every
  ten seconds all night.

`gate:` is not on this list. It holds a step's pass for a person to release whether or not the
run is unattended — see [Gates](#gates). Unattended does not conjure a person to release it, so
a gated task simply waits longer for one; it does not change what spoolway does with a report
that reaches a gate.

What still holds is every check that catches a *lane* going wrong rather than a person being
needed: the reminder loop, and a command step's own `timeout:`.

### Staffing `blocked`

`blocked` is configured, not declared. `[unattended]` in config.toml carries five keys —
`blocked_agent`, `blocked_model`, `blocked_effort`, `blocked_session`, `blocked_prompt` — and
`Pipelines::assemble` builds a `blocked` step from them for every pipeline, before that
pipeline is validated:

```toml
[unattended]
blocked_agent   = "claude"
blocked_model   = "claude-opus-5"
blocked_effort  = ""
blocked_session = true
blocked_prompt = "unblocker"
```

No pipeline file needs to say anything for this to run — it is what every pipeline already
gets. A pipeline that wants something different for its own `blocked` declares the step
itself, naming only the keys it wants to change:

```yaml
  - id: blocked
    model: claude-sonnet-5
    effort: high
```

Every key left off — here, `agent`, `session` and `prompt` — falls back to `[unattended]`,
exactly as if the pipeline had named it explicitly. Any key besides those five is refused by
name: `description`, `run`, `timeout`, `background`, `headless`, `last`, `cleanup`, `slot`,
`loop`, `on_pass`, `on_fail`, `on_loop_max`, `gate` and `end` all mean something on an ordinary step,
and none of them is one `Pipelines::assemble` merges — so `pipeline check` refuses the file
rather than silently ignoring a key that would otherwise do nothing. `blocked` routes itself,
in one turn: a lane staffed here either clears what stopped the task with a pass, or — only
when the thing genuinely cannot be done — pauses it for a person with `spoolway report --pause`
(a `--fail` or a `--block`, the old habit, are read the same way). Nothing routes back onto
`blocked` itself; a pause parks the task on `paused` with the same destination a pass would
have reached, `spoolway resume` carries it there, and there is no round trip left to bound.

A pass carries the task **past** the step it blocked on, to that step's own `on_pass`. The
unblocker prompt is told to do the blocked step's work, so its pass is read as that step's
pass, and handing the task back would pay an agent to reach a verdict that already exists.

This applies to command steps as well. A task blocked on `test` resumes at whatever `test`
passes to, without `test` running again, on the unblocker's word that the build is green.
Set `unattended.skip_blocked_lane = false` where that claim matters more than the extra lap:
the task then lands back on the step it blocked on, which is where `spoolway resume` sends
it by hand, and the lane already there is continued rather than replaced.

`blocked_session = true` resumes the prompt's own earlier session on this task — the same
conversation that read the first blocker is the one asked to look at the second, if there is
one. `spoolway prompt` prints this step's HOW A LANE FINISHES correctly for it: a pass shown
as moving the task on from where it blocked, never as "stays put".

Attended, none of this runs: the task parks with its pane exactly as it always has.

**A blank `unattended.blocked_model` does not stop the config from loading** —
`spoolway config set` has to stay usable to fix it — but it does stop a run from *starting*.
`spoolway dispatch` with `unattended.enabled = true` and no model named for `blocked` refuses
outright, naming both keys, because a run with nobody to clear a block is a run that cannot
finish once one lands. `spoolway doctor` reports the same thing as a `FAIL`.

spoolway ships a sample prompt for it, `unblocker` — see [Prompts](prompts.md). It reads
the task file and the run (`queue list`, `lane`, `eval`, `doctor`, and any pane through a
multiplexer's own read command); whatever stands between the task and its next step is its to
do — the code, the tests, the docs, a rebase, a broken mainline, a missing tool, a red check,
even a rebuild and install of the binary. Three things are never its: merging or landing
anything, stopping the dispatcher, and destroying work it cannot restore. Nothing in the
binary depends on its name —
`blocked` is a step id the dispatcher means something by, and the prompt it runs is whatever
`blocked_prompt` names, or a pipeline's own `prompt:` override. Rewrite it, or point
`blocked` at a prompt of your own, freely.

A staffed `blocked` lane counts against its agent's and its model's `concurrency` like any
other running lane, and its pane is freed once it settles rather than held open for a person
who is not there.

`spoolway pipeline show` marks where each pipeline's `blocked` step came from: `from config`
for the ordinary case, `overridden in <name>.yml` where a pipeline declares its own.

### The brake

With nothing able to park a task, an unattended run has two stop conditions short of an empty
queue or `ctrl-c`: a ceiling in output tokens and a ceiling in dollars.

```toml
[unattended]
enabled = true
max_output_tokens = 2_000_000
max_cost_usd = 20.0
```

Reaching either starts no further lane and lets whatever is live finish, then the dispatcher
stops and says so. Set both and whichever is reached first stops the run; either alone is
enough. The tasks stay exactly where they are — parking them on `blocked` would put back the
one thing the mode is defined by not having — and the next `spoolway dispatch` picks the
queue up where it stands.

`max_output_tokens` counts output tokens, of the four classes, for the reason the board's own
footer counts them: they track work done rather than context carried, and a lane re-reading
the same repo on every pass moves `cache_read` and almost nothing else. `max_cost_usd` is the
money meter: every ledger line already prices itself off the litellm table vendored into the
binary as `assets/model-prices.json`, so this ceiling reads figures that were already being
computed. A model neither that table nor `[models]` knows contributes nothing to the sum.

`spoolway doctor` reports it when `unattended` is on with neither ceiling set. That
combination is legitimate — it is a run bounded only by its queue — but it is not one to
arrive at by accident.

## Command steps

Not everything in a pipeline is worth a model. A build, a test suite, a formatter, a deploy
script, a benchmark — these are commands, and a step that runs one says so:

```yaml
  - id: build
    description: Build the release binary before anything reviews it.
    run: cargo build --release
    on_pass: review
    on_fail: fix
```

`run:` is a command line, not an argv: it goes to the shell whole, so a pipe, an `&&`, a
redirect and a glob all mean what they mean at a prompt. It runs in the task's own worktree,
with the task's environment — `SPOOLWAY_TASK`, `SPOOLWAY_STEP`, `SPOOLWAY_REPO`,
`SPOOLWAY_WORKTREE`, `SPOOLWAY_TASK_FILE` — laid over the dispatcher's own process
environment, so a value the dispatcher started with reaches the command the same way under
every backend, and a named value here wins over an inherited one of the same name.

**The exit code is the outcome.** Zero takes `on_pass`, anything else takes `on_fail`. There
is no report to make and no prompt to write: a command step is the one kind whose verdict is
already a number.

Everything a lane needs is refused here, because there is no lane: `prompt`, `model`,
`effort`, `session` and `gate` are all rejected at load. A command step holds no
worker slot either — nothing is competing for the model server.

### Waiting, or not

```yaml
  - id: bench
    run: ./scripts/bench.sh --full
    background: true
    on_pass: review
```

| | Blocking (the default) | `background: true` |
|---|---|---|
| The next step | starts when the command exits | starts immediately |
| Routing | exit 0 → `on_pass`, else `on_fail` | `on_pass`, taken at once |
| `on_fail` | where a failure goes | **refused at load** |
| `timeout` | routes to `on_fail` | the run is killed, and nothing routes |
| The output | `<task> · <step>.log`, under the project's own home | the same |

`on_fail` on a background step is refused rather than ignored, and that is the whole design of
the key: by the time the command exits the task has been somewhere else for minutes, and a
route nothing can take reads as a handled failure while handling nothing. What the command
wrote is in its log either way, and a run still going when the task is cleaned up is stopped
with it rather than left writing into a worktree that has been removed.

### Watching, or not

A command step runs in a pane of its own by default, under the herdr and tmux backends, so a
person can watch it the way they watch an agent step. `headless: true` runs it detached, with
no pane at all:

```yaml
  - id: handover
    run: spoolway stack
    headless: true
    on_pass: done
    on_fail: blocked
```

A pane changes only where the output is shown. The exit code still comes from the wrapper's
own `EXIT` trap, the log still holds everything the pane shows, and `background:` combines
with either choice — the two are independent. A pane that finishes a passing run closes on
its own; one that fails or times out stands until the task next reaches that step or is
cleaned up, so a stuck pane is something to look at rather than something to hunt for. Under
`backend = "headless"`, which offers no pane at all, a command step runs detached regardless
of `headless:`.

A blocking command does not block the *pass*. The dispatcher starts it, moves on to every
other task, and reads its exit code on a later pass — so a four-minute build is four minutes
for that one task and for nothing else.

### `last:` — a step the chain runs once

```yaml
  - id: suite
    run: ./scripts/e2e.sh --full
    last: true
    on_pass: document
    on_fail: fix
```

Some work is about the stack rather than about one task in it. A plan is a chain of stacked
pull requests, each branch rebased onto the one below, so the task at the top carries every
change beneath it. A suite the whole stack has to pass is one run at the top. Running it on
every task spends the same time over a subset somebody above will cover again.

`last: true` says so. A task that is not last walks past the step to its `on_pass`, without
the command being started, and the dispatcher writes a line saying which task walked past
what.

**The question it asks is whether anything is still open above you.** Another task that
depends on this one, at any depth, and has not finished yet. The queue is the open set — a
finished task is archived out of it — so a dependent that counts here is one that has yet to
run.

That is the whole difference from `when: last`, which this replaces. `when: last` asked
whether the *declared* graph gave a task dependents, which never stopped being true, so a task
in the middle of a chain was never last however long ago the rest finished.

What falls out of asking it the other way:

| Shape | Who runs it |
|---|---|
| A chain | The task at the top, and only that one, whatever order the rest finished in |
| A fan | All of them. No branch contains another, so each pull request stands alone |
| Two leaves | Both. Two leaves are two stacks, so two runs is the answer |
| A task with no plan | It runs. There is no chain to be last in |

**It is a command step's key.** `pipeline check` refuses it anywhere else: a lane nobody
started is a prompt that never reports, so whatever the step was for goes unjudged rather
than undone. Work an exit code can answer is what belongs here.

### Every command is bounded

```yaml
  - id: build
    run: cargo build --release
    timeout: 45m          # 30m unless the step says otherwise
    on_pass: review
    on_fail: fix
```

A command that hangs is the one way a command step can park a task forever: nothing else in
a pass has an opinion about a process that is simply still going, and a hung build looks
exactly like a slow one. So every command step has a timeout whether it writes one or not,
and `pipeline show` prints the resolved number rather than only a written override.

**Running out of time is a failure**, routed like any other: `on_fail` if the step has one,
a person otherwise. Never a quiet pass — a build that never finished did not succeed. The
command's whole process group is killed, so nothing it started is left behind either.

There is no way to write "no limit", and `timeout: 0s` is refused rather than read as one:
it would mean the opposite, killing the command the moment it started.

**Why 30 minutes.** The two mistakes are not symmetrical. Too low reaps work that was going
to finish — a release build with a cold cache, a full test suite, an integration run are all
ordinary at ten or twenty minutes — and turns a working pipeline into an intermittently
failing one, which is the worst kind to debug because the command is blameless. Too high
only means a genuinely hung command sits there longer, costing one task's progress and
nothing else: no worker slot, no model, no other task held up. So the default sits well
above what real work takes, and a step that knows better says so — `timeout: 2h` for a
nightly, `timeout: 60s` for a lint that should never take longer.

A background command is bounded by the same key, even though nothing routes on its outcome.
There the timeout is not a verdict on the work: it is what stops a process outliving
everything that knew about it, on a task that blocked on its way to cleanup.

### Which shell, and nothing to configure

The environment's own — `sh -c` on Unix; on Windows, PowerShell, `pwsh` where it is installed
and `powershell` otherwise, started with `-NoProfile -NonInteractive` so a person's profile
cannot print into the run and nothing can sit waiting for a prompt inside a dispatch pass.
There is no `shell:` key, for the same reason there is no `worktree:` key: the environment already
answers this, and a second spelling of it is one that can disagree. A script that needs a
particular interpreter says so in its shebang, or the `run:` line names it — `run: python3
scripts/bench.py` costs nothing and is exact.

### Nothing confines it

**A command step is not an agent, and is not treated as one.** Deliberately, and it is the
one thing to know before writing a `run:` line.

A `run:` line is written by a person, in a file only people write, and it runs with the
privileges of whoever started the dispatcher — a Makefile target, not a lane. So `cargo build`
fetching a new crate, `npm install`, a tool writing `~/.config` on first run, a deploy script
reaching `~/.ssh` all simply work.

The trade-off, stated plainly: most commands execute something inside the worktree — a script
under `scripts/`, `build.rs`, a `package.json` entry — and earlier agent steps can write
those. A command step therefore runs agent-influenced code. That is accepted: it is equally
true of building the change by hand after it merges, and in the shipped pipeline a review
sits between the two.

## Nothing bounds what a lane can write

A lane runs in its own worktree with the privileges of whoever started the dispatcher.
Nothing takes privileges away from it while it runs — there is no kernel confinement and no
per-command refusal, and a pipeline that promised either would be promising something it
cannot deliver.

There used to be a tripwire here: `blocked_on_write` and its sibling `blocked_on_overreach`,
checked at the moment a lane reported and routing the task to `blocked` on a match. Both are
retired — see [Retired: `blocked_on_write` and
`blocked_on_overreach`](configuration.md#retired-blocked_on_write-and-blocked_on_overreach) —
and nothing inside spoolway replaces them. If you need a bound on what a lane can reach, put
the lane somewhere that has one: your own agent harness's settings, outside this repository.

### Credentials and git verbs are not a pipeline's business

Two keys used to make them so, and both are gone.

`credentials: true` on a step granted a forge token and ssh keys to a lane that was otherwise
confined. There is no confinement now, so there is nothing to grant: a step that needs the
token says nothing about it at all, and `gh` finds what whoever started the dispatcher can
already reach.

An `allow:` list named seven capabilities a step was permitted — `push`, `force-push`,
`force-with-lease`, `merge`, `rebase`, `branch`, `pr` — refused before each command ran.
Every one of them was a git verb, so policing git was not a *subset* of that system, it
**was** that system, and it went whole: the list, `$SPOOLWAY_ALLOW`, and the prompt lint
that read prose for verbs. A project that wants a particular verb stopped writes a harness
hook, which is the layer that can see every agent kind rather than one extension per
provider reimplementing a shell parser.

## `spoolway stack` hands the change over

Handing a change over — commit what is uncommitted, squash to one commit, push with
`--force-with-lease`, open the pull request against the branch the worktree was cut from (or
reuse the one already there), register the GitHub stack — is mechanical, and `spoolway stack`
is the command that does it with git and `gh` alone. No rebase: the dependent's worktree already
sits on its dependency's branch by the time a task reaches `handover` (see
[Dispatching](dispatcher.md)), so there is nothing left to rebase there. The one model turn it
can run is the pull request body itself, when `[stack.summary]` names one — see the body
paragraph above.

```yaml
  - id: handover
    run: spoolway stack
    headless: true
    on_pass: checks
    on_fail: blocked
```

A command step, exactly like `checks` beside it — no worker slot, no prompt, its exit code is
its outcome. It refuses to open a pull request when the branch's three-dot diff against its cut
point is empty, and reports a lease refused by a moved remote distinctly from every other git or
`gh` failure.

**In task-file mode — blank `agent` and `model` — the pull request's title is the task's own
`title:` and its body is the task file, both verbatim.** The body is everything after the
frontmatter fence, with a trailer under it. The trailer names no address — not the git
`user.email` that opened it, and none for the model either. It closes with
`Co-Authored-By: Claude Code`, and above that tag, informationally, any file the branch
touched that the task's own `touches` does not cover, and any open `parallel: true` task `git
merge-tree --write-tree` predicts a conflict with. Neither of those refuses the pull request;
both are for whoever reads it next.

**Every project's `config.toml` carries a `[stack.summary]` table**, naming an `agent`, `model`,
`effort` and `prompt` — the same four a pipeline step's agent half carries. Blank `agent` and
`model` are task-file mode, above; exactly one of the two set refuses the command outright,
naming whichever is blank, rather than guessing at a mode from half a configuration. Fill both
in — `agent = "claude"`, `model = "claude-sonnet-5"` — and `spoolway stack` reads
`.spoolway/templates/pull-request.md` (from `assets/pull-request.md`, written once by `init` and
restorable with `spoolway update --replace`, the way a prompt or a task skeleton is) and passes
its text to the prompt inside the prompt — the prompt never opens it itself. It runs that
prompt on the task file for one turn before anything else: its first printed line becomes the
pull request's title — a Conventional Commits line, which is what the template's opening
comment asks for — and everything it prints is the pull request's whole body — the task
file's own text does not also appear below it. A missing or empty template refuses the command
the same way a blank half does, naming the path and the `spoolway update --replace` that restores
it, and both refusals land before the branch is pushed or a pull request opened, so a broken
setup never reaches the remote. The shipped `summariser` prompt does nothing else — no git, no
`gh`, not even the diff.

```toml
[stack.summary]
agent = ""
model = ""
effort = ""
prompt = "summariser"
```

## When `spoolway stack` cannot

Nothing in a pipeline says how a change reaches the mainline beyond the mechanics above, and
nothing in the binary knows what a stacked pull request is past what `spoolway stack` does
itself. A git or `gh` refusal it could not resolve on its own — a real conflict, a permission
the forge denied, a stack that refuses to form — routes `handover`'s failure to `blocked`, the
same as any other step's: a person's call, not a second agent step waiting behind the first.
`spoolway stack` already reuses a pull request the branch has (it calls `gh pr view` before it
opens one), so resuming `handover` from `blocked` costs nothing extra even when nothing was
actually wrong with it.

One shape is not a failure at all, and is not routed to `blocked` even though the git and forge
calls that would have joined it are refused. A GitHub stack is a single linear chain, and two
tasks that `depends_on` the same task are siblings — both want the one slot directly above their
dependency's pull request, and only the first to run `handover` can have it. The second's own
change is exactly as done as the first's; there is simply nowhere in a linear stack for its pull
request to stand, so `handover` says so on its `stack` line —
`none — \`<task>\` is a sibling of #<n> on #<m>, outside stack #<k>` — and passes, rather than
failing over a shape nothing could have joined.

There is no `merge:` setting to pick a mode with, no per-mode capability list, no paragraph the
dispatcher composes into the system prompt to tell the lane which mode it is in, and no
`handover:` flag naming the step. All four existed once, around a prompt that owned git up to
the pull request; every shipped pipeline now hands over with `spoolway stack` instead, and that
prompt is gone with them. A project whose `handover` needs to do more than `spoolway stack`
does — a different stack shape, git know-how the shipped command does not carry — has no
extension point left to reach for. Losing it is a recorded cost, not a gap this project fills.

### `skip:` — walking past a step

A task's own `skip:` field walks a named step to its `on_pass` without starting a lane. Nothing
in a pipeline file declares it — it is a task's own field, and its one writer is the queue
screen's `p` trial picker, which stamps a trial arm's own ticked steps here so a throwaway run
never pushes a branch or opens a pull request. Only steps the arm's own pipeline actually has
are written, since the arms of one trial rarely share every step. It chains: `skip: [document,
handover]` walks both in the one pass that first reaches the tail, not one pass per step. See
[Runs](eval.md#runs) for reading the arms back.

`last:` walks past a step the same way, and the two differ in who decides. `skip:` is a fact
about one task, naming steps by id. `last:` is a fact about the step, and the pipeline states
it once for every task that will ever reach it.

## Gates

This is the `gate:` key, which any step may carry. **Do not name a step after it.** A step
called `gate` and the `gate:` key are two different things, and every sentence about either
one afterwards has to say which it means — this project's own installed pipelines used to call
their mechanical check `gate`, and it is `test` now for that reason. Name a step for the work
it does.

One key, on a step:

```yaml
  - id: release
    agent: pi
    gate: true          # this step's pass waits for you
```

Two things earn a step this, not one. The pipeline's own `gate: true`, above, holds the step
for every task that reaches it; a single task's own `gate_at:` frontmatter field names one
step to hold for that task alone, whatever the pipeline says — see [Tasks](tasks.md).
Everything below is true of both.

**Spoolway holds the gate; the lane only learns that a person will read its pane.** A gated lane
runs exactly like any other, and reports exactly like any other — nothing it does changes what
`gate:` does. What changes is what spoolway does with its `--pass`: the task lands on `paused`,
its pane is kept open for you to read, and it waits. The lane is told that in its prompt, so it
leaves anything viewable running instead of tearing it down, and writes a short account for you
rather than for nobody — but it is not asked to hold, wait for, or approve anything, and nothing
it reasons or writes changes where its pass parks.

```
spoolway resume <task>                     # let it past: the step's on_pass route
spoolway resume <task> --reject -m "why"   # send it back round: the on_fail route
```

A rejection is a verdict, so your message is written into `## Handoff`, credited to the step
being rejected — the same place any other step's own notes land. A gated step that declares no
`on_fail` has nowhere to send a rejection back round to, so it parks the task on `blocked`
instead, same as any other fail; `spoolway pipeline check` warns about a gate shaped that way.

`paused` is not `blocked`, and the two ask you for opposite things. A block is *something is in
the way and I could not finish*; the answer clears it and resumes at the same step. A pause is
*I finished, and you said you wanted to see this first*; the answer lets it past. Nothing is
wrong with a paused task, its dependents wait quietly rather than being reported as stranded,
and `spoolway resume` on one lets it past the gate rather than resuming it at the same step.

**It used to ask more of the lane than this.** The lane was sent a paragraph asking it to print
its question and end its turn, and that paragraph was the entire enforcement — spoolway's own
half was real but passive: it exempted the lane from the silence clock and let it hold its slot.
Watched over three plan runs against a small local model, not one gate held. Each lane read the
instruction, decided the work was fine, ran `spoolway report --pass`, and the task moved on. A
checkpoint a model can talk itself out of is not one, so that instruction is gone for good — a
lane cannot approve its own work, and the mechanism lives outside the session, where there is
nothing to argue with. What is left is smaller: the lane is told a person is coming, not asked
to act as one.

**Gates are not for pull requests.** A pull request *is* a checkpoint: somebody reads it and
merges it. Gating the step that opens one *and* reviewing the pull request is one checkpoint
too many, so no shipped step declares `gate:` and the shipped pipelines carry none. What it is
for is a step that changes something without leaving a pull request behind — a deploy, a
release, anything irreversible nobody would otherwise see first.

`gate:` says a person decides here, and that holds in an [unattended run](#unattended-runs)
too. Nothing there conjures a person to release the gate — the task waits on `paused` until
one does, the same as in an attended run.

There used to be a second `gate:` at the pipeline level, saying whether pausing was that
pipeline's policy. It existed to reconcile two shipped answers — nothing reaches the mainline
without a person, and a task's own merge proceeds alone — and both shipped pipelines hand over
the same way now, with no gate on either. It also ended a non-local read: `gate: true` on a
step now tells you whether the step gates.

## The shipped `default` pipeline

```
  queued → implement →  review  → document → handover  →  checks  → done
    ⌛     implementer  reviewer   archivist  run: spoolway  run: gh    ✓
                 ↑         ↓                    stack       pr checks
                 └─────────┘                       ↓             ↓
                  on_fail                      `blocked`   `blocked`

  anything that blocks → `blocked`, for you   (attended)
                       → `blocked`, staffed by the unblocker   (unattended)
```

| Step | Runs | Uses the forge |
|---|---|---|
| `implement` | the implementer, on the `claude` profile, keeping its session for the round trip back | no |
| `review` | the reviewer, on the `claude` profile, at a blank `effort`; a failure goes back to `implement`, once | no |
| `document` | the archivist, on this task's own diff | no |
| `handover` | `run: spoolway stack`, `headless: true` — commit, squash, push, open the pull request; a failure routes straight to `blocked` | **yes** |
| `checks` | `run: gh pr checks --watch --fail-fast`, `timeout: 45m` — wait for the checks on it; red routes straight to `blocked` | **yes** |
| `blocked` | the unblocker, staffed only in an unattended run — see [Staffing `blocked`](#staffing-blocked) | no |

Every task runs all of them, the same way — see [`spoolway stack` hands the change
over](#spoolway-stack-hands-the-change-over). `document` sits before `handover` so that
documentation lands inside the same pull request as the behaviour it describes, scoped to that
task's diff rather than to the plan's. `checks` is what used to be prose inside the prompt that
owned git up to the pull request — three paragraphs on polling `gh pr checks` without looking
wedged to the watchdog, because a lane was doing the waiting. A `run:` step has a `timeout:` and
no transcript to go quiet in, so there is nothing left to contort: `handover` opens the pull
request and stops, and `checks` waits.

There is deliberately no end-to-end step: running the change for real assumes a runnable
surface, and a library, a data repo or a prose repo has none. A project that has one adds an
agent step between `review` and `document`, and writes the prompt that staffs it — spoolway
ships none, because a role that runs a project can only be written against a project. What
"start it and exercise it" means is a server on a port, a CLI, or a build artifact, and a
prompt hedging across all three is advice rather than a role.

**This project's own installed pipelines are not this file, and differ from it three ways.**

`.spoolway/pipelines/impl.yml` — this repository's own default, named by its
`dispatch.default_pipeline` — is what a project scaffolded from
`assets/pipelines/default.yml` grows into once it has a build and a test of its own: an
end-to-end step, and two command steps after it. `test` runs `cargo fmt --check`, `cargo
clippy`, `cargo test`, a Windows cross-compile check, a release build and `pipeline check`
against this repository's own control plane — as an exit code rather than a lane's own report.
`suite` runs the end-to-end suite's `pr` tier, and carries [`last:`](#last--a-step-the-chain-runs-once),
so a stack runs it once at the top rather than once per task over work the task above will
cover again. The full list is in [There is no CI](testing.md#there-is-no-ci).
`assets/pipelines/default.yml`, above, carries none of that on purpose: what to build and what
to test is the installing project's own answer, not something a file shipped to every project
can guess.

The second difference runs the other way. **The installed pipelines have no `checks` step**,
because GitHub Actions is switched off on this repository — see [There is no
CI](testing.md#there-is-no-ci). With nothing running on the forge there is nothing for a
`checks` step to wait for, and `gh pr checks` on a pull request with no checks fails rather than
passing, so the step would park every task. The shipped files keep it, because a project that
installs spoolway probably does have CI and `gh pr checks` means the same thing there.

A third difference sits in `handover` itself: the shipped files run `run: spoolway stack`,
resolved through `PATH`, while `.spoolway/pipelines/impl.yml` runs
`run: ./target/release/spoolway stack`, a path fixed to this repository. Here, `test` builds
that binary fresh in the lane's own worktree before `handover` ever reaches it, so it really
is there to run — and it has to be: installing a new build over the one the running
dispatcher's own `spoolway report` resolves through `PATH` would write through the inode that
dispatcher is executing, so a task whose own fix lives in that binary could never use it if
`handover` ran the installed one instead. A project scaffolded from the shipped files has no
`test` step at all — see above — so nothing in its own pipeline ever puts a binary at that
path unless the project's own build does, incidentally, put one there, which is why the
shipped line resolves through `PATH` instead: `spoolway`, whatever actually runs it in the
installing project.

None of the three is drift. One is the mechanical verdict a project can only write for
itself; another is this repository having no forge-side checks to wait on; the third is that
only this repository's own `test` step ever puts a binary at that repo-local path.

Every agent step in the shipped files runs on the `claude` profile with a blank `model` and
`effort` written down explicitly. That Claude identity is a parseable default so the binary can
use the shipped files as test fixtures before any project exists; `spoolway init` specializes
them, rewriting every `agent: claude` to the profile you plan in. A Codex scaffold therefore
ships every agent step running on `codex`.

The review step runs on the `claude` profile so that reviews queue on their own concurrency
rather than competing with local lanes for the model server.

## The shipped `bugfix` pipeline

```
  queued → reproduce → fix → review → reproduce-again → document → handover → checks → done
    ⌛      reproducer  impl. reviewer reproducer        archivist  stack      gh       ✓
               ↓        ↑     ↓            ↓
          `blocked`     └─────┴────────────┘

  `handover` runs `spoolway stack`; `checks` runs `gh pr checks --watch --fail-fast`
  `document`, `handover` and `checks` each fail straight to `blocked`

  anything that blocks → `blocked`, for you   (attended)
                       → `blocked`, staffed by the unblocker   (unattended)
```

`impl.` is the `implementer`, and both `reproduce` steps are the same `reproducer` file —
the two ids exist because a step routes one way, not because the work differs.

One rule lives in this file: no fix lands without a failing repro that now passes. The
repro is a test or a script in the repo, not a running system, so it works on any project
the default pipeline works on.

The reproduce step appears twice, and that is the whole design. The `reproducer` prompt
does the same thing on both visits — make sure the repro exists, run it, report what
happened — and the two steps differ only in which outcome their description calls a pass:
before the fix, the repro *failing* is the pass, because the bug is captured; after review,
the same repro *passing* is.

| Step | Runs | Uses the forge |
|---|---|---|
| `reproduce` | the reproducer — capture the bug, and see it fail | no |
| `fix` | the implementer, keeping its session across the round trips back | no |
| `review` | the reviewer, on the `claude` profile, at a blank `effort`; a failure goes back to `fix`, once | no |
| `reproduce-again` | the same reproducer file — same work, opposite expectation; a failure goes back to `fix`, and arrivals here from `review` are bounded at two | no |
| `document` | the archivist, on this fix's own diff | no |
| `handover` | `run: spoolway stack`, `headless: true` — commit, squash, push, open the pull request; a failure routes straight to `blocked` | **yes** |
| `checks` | `run: gh pr checks --watch --fail-fast`, `timeout: 45m` — wait for the checks on it; red routes straight to `blocked` | **yes** |

The first visit's failure does not go back around: a bug that will not reproduce is a
question, and it leaves the loop — to you, or, in an [unattended run](#unattended-runs),
back to `reproduce` to have another go at it. The second visit's goes to `fix` — the fix
didn't take — with its own round budget, per route in as everywhere else. A fix documents and
hands over exactly as any other task does, and its `handover` carries a plan's ending on the
same terms — a bugfix task in a plan is still a task in that plan.

This shipped file carries `checks` for the same reason `default` does, and no mechanical build
gate for the same reason either — see [the note above](#the-shipped-default-pipeline) on the
two ways this project's own installed pipelines differ from the pair shipped here.

## Whose worktree

Every lane runs in a git worktree. There is no setting for it, because there is no decision
to make: **a branch cannot be checked out twice**, so a task whose branch somebody already
has out borrows that checkout, and a task whose branch is nowhere gets one cut for it.

```
task's branch → git worktree list → already out → borrow it, and leave it alone
                                  → nobody has it → cut one, and remove it at cleanup
```

No knob says so, and none ever did — it is a fact about git rather than a mode. What used to
depend on it was the plan closeout, which ran in the main checkout on purpose; that is gone,
every task now working only in its own worktree,
and what is left is the ordinary case of a person having a task's branch out while spoolway
reaches it. It also
decides what cleanup may touch: a borrowed checkout and the branch in it were somebody
else's before the task started and are still theirs after it, so a terminal step with
`cleanup: true` removes neither. The task file records which it was, as `borrowed:`, because
afterwards the two look identical to git.

## Working with pipelines

```
spoolway pipeline show      # your graphs, with every default resolved
spoolway pipeline check     # validate against the config they will run with
spoolway pipeline contract  # the format itself: every key, every rule, a blank to copy
```

`pipeline check` validates the graph and its agent references, and includes the prompt
checks: a prompt that has learned the shape of the graph, or names a `spoolway` subcommand
this release does not have, is caught before the lane ever starts rather than mid-run in a
pane nobody is watching. It also refuses an agent step whose `model:` is missing or blank,
and an `effort:` on a kind with no flag to carry one — or the literal `auto`, which nothing
resolves any more. A `skills:` is refused the same way on a step that starts no lane — a
command step or a terminal one — and on an agent kind that loads no skills at all, which
would launch and quietly ignore the invocation. The names themselves are never looked up:
spoolway can see a project's own skills directory and the user's, but a plugin installs its
skills where spoolway cannot enumerate them, so "in neither directory" does not mean "not
installed", and the agent resolves the name at launch, which is the only place it can be
resolved. It also walks
every queued task's own `skip:` list and refuses a name that
is not a step of the pipeline the task runs on — a pipeline's steps can be renamed out from
under a queued task's own copy, and this is where that shows up rather than at the dispatch pass that
would otherwise silently do nothing with a name that never matches.

This validates only the project's own loaded pipelines. The two samples shipped under
`assets/pipelines/` are no longer opened by this command — `spoolway init` specializes them as
test fixtures, and their parse, structure and neutrality about Pi, model and effort choices are
release-time proof held by a repository test in `src/assets.rs`, not by any one project's config.

`pipeline contract` prints every key a pipeline and a step may carry, the rules refused at
load, and an annotated blank pipeline to copy from — read straight off the struct and the
validation `pipeline check` runs, so it cannot drift from what the loader actually enforces.
Picking up keys added since your project was scaffolded is not its job any more: `spoolway
update` rewrites the fenced key reference in every pipeline file that carries the markers.

The `spoolway-pipeline` skill walks the whole procedure with a coding agent: it asks what one
pass has to produce, writes the YAML and any prompt its steps name, and runs both checks. The
files are the output — a pipeline is short enough that reading it beats reading a description
of it, and it is in git either way. It ships no template of its own any more: it runs
`spoolway pipeline contract` at the start of every session and copies the blank that command
prints.

## Generating a pipeline

`spoolway pipeline gen [--plan <path>]` opens a fresh agent session, in a pane of the current
checkout, and prompts the `spoolway-pipeline` skill — the same procedure as "convert a
workflow", below, this time starting from a plan rather than a description typed by hand.
Nothing is written by the command itself: it starts the session and hands the skill a plan
path to read, and this project's own preferences to take as already answered rather than
asking again, so the procedure writes straight from them instead of stopping to ask. See
[`[pipeline_gen]`](configuration.md#pipeline_gen--generating-a-pipeline) for the profile,
model and preferences that session runs with, and [`spoolway pipeline
gen`](cli-reference.md#spoolway-pipeline-gen---plan-path) for the command itself.

`spoolway-tasks`'s own step 1 offers this alongside the pipeline names `pipeline list`
returns, for a batch of tasks that needs a graph none of the existing ones fit. That step comes
before the split is decided, because the pipeline names the model that implements each task and
that model's window is what decides how big a task may be.

## Converting a workflow you already run

Almost nobody starts from nothing. There is a sequence somebody already follows — a review
procedure, a release runbook, the stages in a checklist, the flow a CI config half-encodes —
and getting it into `.spoolway/pipelines/` is mostly translation rather than design. The
translation asks the same handful of questions every time:

| Your workflow has | It becomes |
|---|---|
| A stage where somebody exercises judgement | An agent step: a `prompt:` on a `model:` |
| A stage that is a command with an exit code | A `run:` step; the exit code picks `on_pass` or `on_fail` |
| "…and if that fails, go back and fix it" | `on_fail` pointing back, plus a `loop` bounding it |
| "…check with me before this one" | `gate: true`, then `spoolway resume` |
| A handoff that exists so a person knows the last stage finished | Nothing. Delete it |

The last row is the one worth being ruthless about. A workflow written for people carries
checkpoints that exist because somebody had to be *told* the previous stage was done, and a
pipeline tells nobody anything — the task file already says where the work stands. Keep the
checkpoints where a human actually decides something; the rest are latency.

Two things do not translate, and looking for them wastes an afternoon. **A step is not a
sub-workflow** — if a stage in your process contains three stages, it is three steps, or it is
one step whose prompt describes all three; there is no nesting. And **a pipeline says nothing
about what a lane may reach**: a stage described as "the deploy stage, which has the
production credentials" becomes a step, and nothing else — every lane reaches what whoever
started the dispatcher can reach, and a pipeline has nothing to say on the subject at all —
see [Reach](concepts.md#reach).

The `spoolway-pipeline` skill does the whole conversion with you: describe the flow you run
today, and it works out which parts are steps and which are instructions belonging inside one,
then writes the graph and the prompts its steps need — see [Writing your
own](prompts.md#writing-your-own) for what goes in one of those.

The shipped `default` is a reasonable thing to copy and cut down, and an equally reasonable
thing to ignore. It carries stacked pull requests, a documentation step and a handover whose
checks are waited on because that is how a project *with* CI ships; a project that merges to
trunk and reads its own docs wants a much shorter graph, and there is nothing to switch off to
get one — those steps are simply not in the file.

## Moving a pipeline between projects

A pipeline is a file, and moving one is copying it:

```
cp .spoolway/pipelines/default.yml ../other-project/.spoolway/pipelines/
cp -r .spoolway/prompts/reviewer ../other-project/.spoolway/prompts/   # each prompt its steps name
cd ../other-project && spoolway pipeline check
```

`pipeline check` on the other side is the half that matters: it reports every prompt and
agent profile the graph names and the project does not have. What it names and does not carry
is deliberate — an agent profile is a binary on somebody's own machine, so it resolves
locally rather than travelling. The model and the effort each step asks for travel with the
graph itself now, since both are the pipeline's to say.

`pipeline contract`'s annotated blank is a starting point for one of your own — the key
reference inside a file you already have is `spoolway update`'s.

## Adding a step

A new role in the pipeline is two files: the prompt, and the step that runs it.

```yaml
  - id: audit
    description: Check the change against the security checklist.
    agent: claude
    prompt: auditor
    model: claude-opus-5
    on_pass: document
    on_fail: implement
```

Nothing registers a prompt — a step's `prompt:` is a bare filename with a default of the
step id. See [Prompts](prompts.md) for what to write in the file, and note that
`spoolway prompt contract --step audit` prints exactly what that lane will be handed.
