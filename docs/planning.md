---
domain: planning
covers: ["assets/skills/**"]
---

# Planning

A plan is a set of related tasks with a shared branch, and a file arguing the shape of the
work so a person can approve it before any of it is queued. This page covers the whole arc:
proposing a plan, slicing it into tasks, queueing them, watching them, and closing the plan out
onto the mainline.

## Why a plan is a branch

A plan is, mechanically, "the tasks queued from one branch": `queue_add` reads the branch of
the checkout you run it in and writes it into each task's `base:` (`src/commands/queue.rs`) —
that is the plan's shared branch, whatever it is. The plan file, when there is one, is the
human-readable half of the same idea.

What makes a particular branch a legitimate one to queue from is your project's own workflow —
spoolway does not check it. The shipped pipelines do add a requirement of their own, though,
because `handover` stacks: the foot of the stack is based on `base:`, so `base:` has to be a
branch somebody actually merges, or the stack it builds can never land — see [`spoolway stack`
hands the change over](pipelines.md#spoolway-stack-hands-the-change-over). A project whose
`handover` opens flat pull requests instead has no such requirement.

So, running the shipped prompt, queue a plan the same way you'd queue anything else for it:
from the branch the work is meant to land in — the mainline, or a release branch — rather than
from a branch created only to hold the plan. Several plans queued this way do not collide: the
queue belongs to the project, not to a worktree (see [Project](concepts.md#project)), so one
checkout can queue as many plans as you like, all served by one queue and one dispatcher.

## Two skills, cut at approval

Working out what to build is a conversation; turning that into ids, globs and a pipeline choice
is arithmetic against a lane-name cap and a queue. `spoolway-plan` owns the first half and
`spoolway-tasks` the second, and the cut between them is not only a skill boundary but a gate:
nothing in `spoolway-tasks` runs until a person has approved the shape.

`spoolway-plan` understands the goal, settles every question with the person in the room,
and argues the shape as a decision record, naming no task, no pipeline, no glob — none of that
is decided yet. The page itself is offered rather than assumed: once the questions are settled
it asks whether to write one at all, and a person who is happy with the shape as discussed can
go straight to cutting the tasks. Publishing the page is not the end of the session either: it
opens the page, prints the path, and asks — cut the tasks now, revise the page first, or leave
it — so the gate is a question a person answers rather than a step they have to know to ask
for. Only once the person says the shape is right does its own step 7 invoke `spoolway-tasks`
to do the decomposing: the pipeline first, then the candidate splits, the ids, the globs and
the dependency order. It is the one
procedure that estimates size and complexity, the one procedure that writes a task document,
and the one procedure that reads pipeline names at all — shared rather than reimplemented,
because a second skill that cuts a breakdown calls it too instead of carrying its own copy.

When the goal names an issue — a ref, a link, "issue 57" — both skills read it the same way,
through `spoolway issue show <ref>`, before anything else: `spoolway-plan` at the start of its
own conversation, `spoolway-tasks` at its own step 1 when it is invoked directly with no page
behind it. The issue a person filed stays theirs — its own URL becomes the eventual task
documents' `source:`, never their `ticket:`, so the `open` hook still opens spoolway's own epic
and tickets, additionally hung under the filed issue. See [Queueing a
plan](#queueing-a-plan) below for where `source:` and `plan:` land on the document itself.

Actually queueing what `spoolway-tasks` cut is a different act, done later, from the
`spoolway queue` screen — see [Queueing a plan](#queueing-a-plan) below. A plan page is not a
precondition for any of this. The screen reads task documents, not pages, so anything that can
write a document reaches it: a goal small enough to skip planning altogether, a Jira ticket, a
GitHub issue, a script. What those producers share is the `group:` their documents name, which
is the row the screen draws.

## The plan file

Where there is one, the plan file is a review aid with a short life: read once by a person,
then left to go stale once its tasks are done. It is not documentation — [domain
documents](documentation.md) are that — and it never reaches git: it lives outside the
checkout entirely, under the project's own home, because spoolway itself never opens a plan
page back up.

It is one self-contained HTML file and nothing beside it — a stylesheet inlined into its own
`<style>` block, and its two lockups as data URIs — so it renders wherever it is moved,
attached to a message, or opened weeks later from a different checkout, with no second file
that has to travel with it. Nothing in spoolway manages where it lives, or reads it: `spoolway-plan`'s
default is `~/.spoolway/<project>/plans/<YYYY-MM-DD>-<slug>.html`, stamped with the day the
plan was cut, a convention the skill holds rather than a setting anything resolves, and a
person who wants it elsewhere says so.

The skeleton spoolway-plan fills is not configurable either, and lives under the skill's own
`assets/`, at `assets/skills/claude/spoolway-plan/assets/template.html` — read it to see what a
plan looks like here. It carries no comments of its own any more: the mechanics of copying it,
what every `[[slot]]` takes, and how a record's and a mockup's markup is built live in a reference beside
it, `assets/skills/claude/spoolway-plan/assets/page.md`, which the skill opens only at the step
that fills a page.

**The binary does not read a plan page at all.** There is no `spoolway plan` command, no
`plan check`, and `queue add --from` no longer accepts an `.html` file. spoolway stopped knowing
what a plan is: what it reads is task documents, and the page is a thing people look at.
`spoolway group list` answers the question `plan list` used to — see [Queueing a
plan](#queueing-a-plan) below.

### The spine

Every plan argues on the same three sections, in the same order: Context, Decisions and
Mockup. The page ends there. The breakdown is not on it — a task is a document, written beside
the page — so a page carries no `id`, no `pipeline` and no `touches` glob at any point in its
life. It argues a shape; `spoolway-tasks` is where that shape becomes a real `touches`, a real
`depends_on`, a real task id, in files of their own.

## Writing a good breakdown

`spoolway-tasks` encodes a set of rules that matter whether or not you use it, at the
moment it decides ids, globs and dependencies — not at planning time, since none of that exists
yet while the page's Context, Decisions and Mockup are still being argued.

**Plan against the system as it is.** A person describing a detailed solution is telling you
about a problem; the solution is evidence, not specification.

**Size each task for the lane, not for a person.** A lane is an agent in a fresh worktree with
a context window, not a developer with an afternoon, so "a session" and "a day's work" are the
wrong units. A task's size is estimated from judgement on the split ballot,
never from a token count of the task itself.

**Which way to lean is the routed pipeline's window to decide.** Each subject is routed to a
pipeline first, and that pipeline is what supplies the one number the sizing turns on: its
window. A pipeline's window is the smallest window among the models on its own agent steps,
each of those models looked up in `spoolway models`. Two subjects in the same breakdown may
therefore be sized against two different numbers, because they were routed to two different
pipelines. A hosted frontier model carries a window in the millions, and there the bias is to
**lean bigger** — every extra task pays for another lane to read the codebase, the prompt and the
task from scratch before it writes a line, and runs `review`, `e2e` and `document` again on top,
while the implement lane a split relieves almost always had room to spare. A model served from
your own machine carries a hundred thousand or so, and there the bias inverts: **cut smaller**,
because a task that overflows a local window does not slow down, it forgets the contract it was
given and fails the step. A model with no resolvable window is treated as the small case.

The ballot is the recommended count and the four below it, floored at 1 — a recommendation of 6
offers 2, 3, 4, 5 and 6, and one of 3 offers 1, 2 and 3. Only the four largest fit as options,
so the smallest member of a five-wide ballot is typed in rather than clicked.

**Write acceptance criteria that can be checked mechanically.** "Works well" is not a
criterion. "`GET /health` returns 200 with `{"status":"ok"}`" is. These are what the review
step judges against, so vagueness here becomes a failed review later.

**Give every task non-goals.** A local model with no boundary invents one. Name the adjacent,
tempting, wrong thing somebody reaches for when a task finishes early: the refactor next
door, the extra endpoint, the test framework swap.

**Reference paths, never copied prose.** A domain document beats any summary of it, and
stays true after the summary rots. The lane is already told its dependency tasks when it
starts, from the frontmatter — so reference what that cannot know: the specific file, the
example to copy, the interface to match. The plan page itself is not among what a lane is
told: a plan page never reaches a lane's worktree, so a mockup that matters to a task is
copied into that task's own `## Mockup` section instead, by `spoolway-tasks` below, rather than
referenced from the page.

**Draw a mockup from what already runs.** A panel for something that already runs starts from
that thing's own captured output — a real screenshot, a real command's real output — and is
edited from there, never redrawn from memory of what it probably looks like. A panel may quote
a command that runs today, never one that doesn't, and writing or running anything to produce
a panel's contents is refused — a scratch script, a prototype, a measurement is implementation,
and this session does not do that. A panel for something that does not exist yet has nothing to
capture, so it names, in its own bar, the bound it was drawn to hold, and the task cut from it
carries that bound as a criterion.

**Get the globs right.** They are not documentation of intent. Each task's own `document` step
resolves *its* globs against every document's declared coverage to work out what it has to
bring up to date, and the lane's own prompt is composed from them. Vague globs produce
documentation that quietly goes stale, in the pull request that should have carried it.

**Choose the pipeline per task where it earns it.** A bug found mid-plan wants `bugfix` — a
repro before a fix, and the same repro again after — while the feature work beside it wants
`default`. Routing happens per subject, before that subject is sized. `spoolway pipeline show`
prints every pipeline's own `description:`, and that sentence is what a subject is matched
against. The routing is not settled once for the batch: a plan cut into five tasks may run five
pipelines. Every routed pipeline is named on the split ballot, carrying its own description, so
a person sees and approves the routing before anything is written. If none of them fit, the
same ballot offers "Generate a pipeline for this plan" — it hands the plan to
`spoolway pipeline gen`, prints one line, and ends the session with nothing cut, so the
generated pipeline exists before a task is ever written against it. See [Generating a
pipeline](pipelines.md#generating-a-pipeline).

**Make tasks disjoint, or order them.** Overlapping globs between tasks with no dependency
between them conflict at merge time. Checking for conflicts flags it, but it is cheaper not
to create it. A genuine fan — two tasks with nothing to say about each other's diff — may skip
the chain between them, but mark both `parallel: true` when it does: it costs nothing about
how they run, and it is what tells `queue conflicts` and whoever reads the queue later that the
missing edge was chosen, not forgotten.

## Queueing a plan

Cutting a shape into tasks and putting those tasks in the queue are two different moments.
`spoolway-tasks`, invoked from `spoolway-plan`'s step 7, decides the split and writes each task
as its own document into the project's pending directory —
`~/.spoolway/<project>/pending/<task-id>.md` — with a body,
`touches`, `depends_on`, `pipeline`, everything `queue add --from` would need. The body's own
`## Mockup` section carries the mockup steps that task is responsible for, copied in from the
page's Mockup above, and only when the task changes something a person opens; a task with
nothing on screen or in a document to show for it deletes the heading whole.

Every document names the same `group:`, and that string is what holds the breakdown together.
It is read verbatim and never path-parsed, so a bare name and a path are different groups even
when they share a file stem. A document may also name a `source:` — an issue URL, the plan
page's own path, a ticket reference — which spoolway never parses. Where step 1 read an issue
through `spoolway issue show`, its own URL is what goes into `source:`, and the plan page's own
absolute path moves to a `plan:` key beside it instead; with no issue behind it, the page's path
stays in `source:` and there is no `plan:` at all.

Step 6 proves the set before it says it is done, with `spoolway task contract --from` pointed at
the pending directory: the same validation `queue add --from` runs, stopping short of the save.
Each task id is measured there, because a lane is named `<id> · <step>` and stops at 32 bytes.
How much room an id has is not a fixed number — it is `28 - <longest step of the pipeline that
document names>`, so the same id can be fine on `default` and too long on `bugfix`, whose
longest step is `reproduce-again`.

Nothing is queued yet at that point. Actually entering the queue is `spoolway queue`, bare, with
no subcommand: a terminal screen that lists one row per distinct `group:` across the pending
documents on the left, newest-written first, and the highlighted group's tasks — what each waits
on, and its own title — on the right. A task's `touches` globs are not shown: they were the
widest thing on the pane, and nobody picks what to queue by glob.

`space` selects a group, whose documents go to the queue as one chain, and `g` sets or clears a
`gate_at` on the highlighted task, picked from that document's own pipeline. `o` opens the
highlighted document in an editor — `$VISUAL`, else `$EDITOR`, else the platform's own default
— in a pane the multiplexer opens for it, the same resolution the board's own `o` uses, and `f`
opens a filter box that narrows the group list to what you type: `enter` keeps the filter, `esc`
clears it. `r` swaps the left pane for the project's routines — [below](#routines) — and `s`
saves the highlighted group into them.

`enter` validates the selection and writes it, with nothing drawn in between. A validation
failure — a reserved key, an unknown `depends_on`, a cycle among the selection — shows the
refusal and leaves the pending directory untouched; `q` quits, from every mode but the two that
read a typed line — the `f` filter and `s`'s own name field — where it is an ordinary letter.
The screen no longer checks the selection for overlapping `touches` globs itself: an
overlap across two groups is a collision at merge time, not at queue time, since each group
merges on its own branch, and `spoolway queue conflicts` — below — is where it is reported, for
a pair ordered by nothing.

With nothing to refuse, `enter` hands every selected group's documents to the same `--from`
path `queue add` always used, in one batch. Once every task file is written — and only then —
the screen deletes exactly those documents from the pending directory, leaving every other
group's where they are. A submission that fails validation deletes nothing. What is left on
screen is the same report `queue add --from` prints for the same batch — one pair of lines per
task, then the branch every one of them was based on — under one question: start a dispatcher
here now? `y` answers it, and the dispatcher takes over the same terminal. Every other key
declines and goes back to the screen, because only `y` should ever be able to start a run. The
question is not asked at all where a dispatcher already holds the queue's lock: the report names
that process's pid and says it picks these up on its next pass, since a second one would only be
refused once it actually ran.

Because queueing removes a group's documents from the pending directory, the shown half of the
screen drops to nothing once every group has been queued — the ordinary resting state, not a
sign anything is wrong; `h` still reveals what it is hiding. A group whose tasks the queue
already holds — because a producer re-ran over work already submitted, or because the screen
itself just queued it — cannot be queued again, so it draws a bare `queued` tail, no checkbox,
and stays behind `h`. Submitting a group does not clear its row, either: the row is built from
whichever directory still holds the group's documents, pending or queue, so it survives the
submission and leaves only once the dispatcher archives every one of them.

`add`, `list`, `show` and `conflicts` are still the ordinary subcommands underneath — the screen
only replaces what used to be a skill typing `queue add` on a person's behalf. `queue add --from`
pointed at the pending directory queues every `.md` document there without a screen involved at
all, which is the path a script takes; unlike the screen, it does not own the directory and
deletes nothing.

### Trials

`p` on a highlighted task opens a picker, over the same screen, for running that one task on
several pipelines at once — to see which does the work better. The picker lists the project's
pipelines first; `space` ticks one, and once at least one is ticked, the union of the ticked
pipelines' own steps is listed below it, each with its own `space` to tick as a step to skip.
`enter` queues one arm per ticked pipeline; `esc` leaves the ticks exactly as they were and
closes the picker without queueing anything.

Each arm is a copy of the highlighted task's document, queued under its own `pipeline:`, the
ticked steps in its own `skip:`, and the same `group:` the source document names. If that document
has already been through a run, it gets the same reset a save does before an arm is built from it
— see [Routines](#routines). Its id is minted, not typed: the source document's own id with the
lowest number free in both the queue and the archive appended — `<id>-1`, `<id>-2`, and so on —
and that minted id is measured against the arm's own pipeline the same way `spoolway task
contract` measures an ordinary one; an id too long for its pipeline's budget is refused, naming
the budget and by how much it is over, and nothing in the trial is written. The source document
itself is a template for the arms, not one of them — it is left in the pending directory exactly
as it was, and no bare-id copy of it ever reaches the queue.

The ticked steps land in `skip:` so that a trial arm never pushes a branch or opens a pull
request on its own — see [`skip`](tasks.md#the-task-file). Every arm banks the source
document's own id as its trial id, so `spoolway eval --runs --trial <id>` finds them together
and puts them side by side on pass rate, cost and time — the comparison a trial exists to
answer. See [Comparing versions](eval.md#spend-by-spoolway-spend).

### Routines

Work a project means to run more than once does not belong in the pending directory, because
queueing a group deletes its documents from there. It lives in `.spoolway/routines/` instead —
in the checkout, tracked in git, nested into folders however the project likes. Nothing creates
that directory: not `spoolway init`, and no routine ships in `assets/`. An empty `r` screen
names the path so a person knows where to make it.

`r` swaps the queue screen's left pane for that folder tree, one row per folder with a count of
every document at or below it. `→` opens what is highlighted, `←` backs out of it, and `r` a
second time returns to the pending screen. A folder holding both its own documents and
subfolders takes `→` twice: the first press focuses its own documents on the right, the second
descends into its subfolders. `←` unwinds those same two steps in the same order.

`space` ticks a folder, and `enter` queues every document at or below every ticked folder as one
batch, through the same validation the pending screen submits through. `space` over a single
document on the right queues that one task alone, with its `depends_on` emptied first — the
edges are dropped rather than refused, so a task can be pulled out of its chain and run on its
own.

Ids are minted, never taken as written: a routine exists to be queued twice, and the second run
would collide with the first at the id the document itself always names. A `depends_on` naming a
sibling in the same batch is rewritten onto that sibling's own minted id, so a chain saved
together still resolves. Everything else travels unchanged — the body, and the document's own
`group:`. The files under `.spoolway/routines/` are never moved, rewritten or deleted by any of
this.

`s` on a highlighted pending group is the way work gets in there. It opens a panel over the
pending screen, prefilled with the group's own name, and `enter` copies that group's documents
into `.spoolway/routines/<name>/`, bare ids and all. A document that has already been through a
run is reset on its way out: every key spoolway stamped on that run is dropped, and so are `epic:`
and `ticket:`, which named the finished run's own ticket. What is left is the keys a document's
author writes, whatever metadata the project itself added, and the body byte for byte — so the
saved routine is a document a person could have written for a fresh run. The file the document was
read from is never touched. The name is one plain folder name: a `/` or a `..` is refused rather
than joined, and so is a folder that already holds documents — a save never merges into one. `esc`
closes the panel without writing anything, and `q` is an ordinary letter in the name field, not a
quit.

Beside the screen there are two commands worth knowing:

```
spoolway queue conflicts
```

which reports overlapping globs with no ordering between them, over the queue as it stands —
the screen itself checks nothing before it writes — and

```
spoolway group list
```

which prints one line per group with tasks still open — its `group:` string, read verbatim, so
a bare name and a path to a page are different groups even when they share a file stem — how
many tasks are open, and which ones. `queue add` refuses a task with no `group:`, so nothing
queued the ordinary way is missing from it; a task file hand-edited to drop the key is skipped,
with no line here at all, and a group every one of whose tasks has reached `done` loses its line
too, once those tasks are archived out of the queue.

It is how a person sees what a group has left before landing its stack — see [Closing a plan
out](#closing-a-plan-out). There is no `--json`, and — since the binary keeps no store of plan
pages — no other way to enumerate groups than the ones the queue itself already knows about.

## Closing a plan out

There is nothing to run, and no step that exists only for the ending. Every task documents its
own diff and hands its change over; the plan closes out when the last task still open does the
same:

```yaml
  - id: document
    agent: pi
    prompt: archivist
    on_pass: handover

  - id: handover
    run: spoolway stack
    on_pass: done
```

1. **Document.** Each task brings the domain documents in line with what *its own* diff
   changed. Which documents those are is a lookup rather than guesswork: that task's `touches`
   globs, resolved against each document's declared coverage. It happens before the handover,
   so the documentation lands inside the same pull request as the behaviour.
2. **Hand over.** What this does — and what it never does — is `spoolway stack`'s; see
   [`spoolway stack` hands the change
   over](pipelines.md#spoolway-stack-hands-the-change-over). It ends at a green pull request,
   per task, whether or not that task turns out to be the last one still open.

Nothing computes which task that is in advance. A step used to carry `when: last`, resolved at
dispatch as "this task has no dependents" — a different question, and one that said *two* tasks
when a plan's graph ended in two leaves. That is gone: every task's handover ends the same way.

A command step may still say it belongs to the chain rather than to one task, with `last:` —
see [`last:` — a step the chain runs once](pipelines.md#last--a-step-the-chain-runs-once). It
asks a different question than `when:` did, about tasks *still open* above this one rather
than about the declared graph, so a chain names its top and a fan names everybody. It is for
work an exit code answers, and `handover` is not that: a pull request is opened per task,
here as before.

`spoolway plan close` used to queue this as a separate task on a `merge-and-doc` pipeline,
running in the checkout that had the plan branch out. That is gone, and with it the command,
the pipeline file, its base derivation, `$SPOOLWAY_MERGE_INTO` and the special case for a lane
whose worktree *is* the main checkout.

## Why a plan is a chain

Every task `depends_on` the one before it. That is not about avoiding conflicts — it is what
makes stacked pull requests possible at all: a dependent's worktree is cut straight from its
dependency's branch, so the ancestry a stack needs is a fact of the cut rather than something
built afterwards with a rebase — see [`spoolway stack` hands the change
over](pipelines.md#spoolway-stack-hands-the-change-over) for what the pull request `handover`
opens is based on. A task with no dependency has nothing to stack on.

A **chain** — every task depending on the one before it — is what a breakdown should be: it
stacks. A **fan** — several tasks, no dependencies — degrades gracefully: no stack, each task
opens a flat pull request against its own `base:`, nothing lost. A **join** — one task
depending on two — is the shape that fails invisibly: it can be rebased onto only one of its
parents, and ships without the other's work. That is the shape `spoolway-tasks`'s own
chain check names specifically, and keeps the table for. A fan whose members are all marked
`parallel: true` is read as the shape working as declared rather than a broken chain.

A fan's members are still refused a `touches` overlap: `parallel: true` records that the missing
edge between them was chosen, not that an overlap between them is fine. `queue conflicts` reads
a `touches` overlap between two declared-parallel tasks of the same plan as a mistake in the
breakdown, the same way it would for any other unordered pair.

The chain is produced and checked by `spoolway-tasks`, in one session, not by the binary.
A plan with no chain — a fan, above — has nothing for that check to say, so it says nothing.

**This is a convention, not an invariant.** A skill check does not stop a task file being
edited afterwards. Given the failure mode is a wrong-looking pull request rather than
destroyed work, that is the right strictness.

There is no base-freshening pass any more, and it is the rebase that removed the need for one.
The dispatcher used to push and fast-forward every base the queue named before cutting a
worktree from it, and hold back any task whose base was behind — because under the old model a
dependency merged into the plan branch before the next worktree was cut. Nothing merges first
now: the dependency's work arrives through the rebase, from the branch itself.
