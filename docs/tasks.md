---
domain: tasks
covers: ["src/task.rs", "src/graph.rs", "src/globs.rs", "src/task_log.rs", "assets/task-log.md"]
---

# Tasks and the queue

A task is one unit of work: a change small enough for one agent to finish in one sitting.
This page covers writing one, queueing it, expressing order between tasks, and reading the
queue.

## The task file

A task is a Markdown file with YAML frontmatter, living in the project's queue directory —
`~/.spoolway/<project>/queue/`, outside the checkout. It has two owners, and they divide
cleanly.

### The frontmatter is spoolway's

It is spoolway's for the task's whole life — every dispatch decision is a lookup in it. It is
generated from a struct in the binary and re-serialised on every save, so what a task records
can change with a spoolway release without a single file in your repository having to move.

| Field | Set by | Meaning |
|---|---|---|
| `id` | you, at queue time | The task's name. Used as the filename and the branch suffix |
| `title` | you, at queue time | A Conventional Commits line naming what the task does — a type, the area of code in parentheses, a colon, and one short present-tense sentence: `feat(queue): add a --dry-run flag`. The squashed commit's subject, verbatim and always, and the pull request's title when no summary model is configured. Required — `queue add` refuses a document without one. The shape is a convention, not a check: `queue add` only refuses a blank one |
| `stage` | the pipeline | The step the task is sitting on. Always a step id from its pipeline |
| `pipeline` | you, optionally | Which pipeline this task runs on. Absent means the file's declared default |
| `group` | you, at queue time | The chain of work this task belongs to, read verbatim — never path-parsed, so a GitHub issue URL groups exactly like any other opaque string. Required — `queue add` refuses a document that leaves it blank, since a lane runs in the tab its group shares with its siblings. A task with no `group:` is a group of one |
| `source` | you, optionally | A reference to where this task came from — an issue URL, a plan page path, a bare name. Nothing in spoolway parses it. The issue a planning skill read through `spoolway issue show`, whenever a task started at one; a plan page's own path only when there was no issue behind it |
| `plan` | you, optionally | The plan page that argued this task's shape, as an absolute path — set only when a page was written *and* `source` holds an issue instead of it, since a page with no issue behind it keeps its own path in `source` there. Nothing in spoolway parses this either |
| `touches` | you | Globs this task is expected to modify. Drives conflict detection and review effort |
| `depends_on` | you | Task ids that must finish before this one may start |
| `parallel` | you | Marks the *absent* `depends_on` between this task and another `parallel: true` task of the same group as deliberate rather than forgotten. It does not excuse a `touches` overlap: an overlap between two declared-parallel tasks is read as a mistake in the group — see [Declaring a fan on purpose](#declaring-a-fan-on-purpose). Read only by `queue conflicts`, `queue list`, and the two planning skills — nothing that schedules or bases a task looks at it |
| `gate_at` | you, optionally | Pauses the task on `paused` once this step passes, on the same terms a step's own `gate: true` does — see [Paused is the other one, and it is not a block](#paused-is-the-other-one-and-it-is-not-a-block). Lets a document hold work for a person without giving the task a pipeline of its own |
| `borrowed` | the dispatcher | Whether the task's checkout was already there rather than cut for it. Cleanup reads it to know the checkout and branch are not its to remove |
| `base` | the dispatcher | The branch the group lands in, recorded at queue time from the checkout `queue add` ran in — the base of the *foot* of the stack's pull request. `spoolway stack` opens every other task's pull request against `cut_from` instead |
| `branch` | the dispatcher | The branch for this task: `task/<id>`, or `task/<slug>-<id>` when `issue_tracking.key_in_names` had `queue add` prefix it with a tracker slug. spoolway derives it and a document may not set it to either shape by hand — see [What a document may set](#what-a-document-may-set) |
| `run` | the dispatcher | Minted once, when the task's worktree is cut, and copied onto every ledger line banked for it from then on — the id `spoolway eval --runs` gathers a run's lanes under |
| `cut_from` | the dispatcher | What the worktree was actually cut from, and what `spoolway stack` opens its pull request against: the first dependency's own `branch:` field when `depends_on` names one, `base` otherwise. That field is read straight off the dependency, whichever shape `queue add` stamped it — `task/<dep>` or a slug-prefixed `task/<slug>-<dep>`. A dependent's worktree is cut from its dependency's branch rather than `base`, so the two can disagree — `spoolway queue show` prints this on a line of its own, distinct from `base` |
| `base_commit` | the dispatcher | The commit `cut_from` pointed at when the worktree was cut. `cut_from` is a branch name and branches move — by the time anyone reads it back the branch may be merged and gone; this is the fixed point recorded instead. Only written where a worktree was actually cut, never for a borrowed checkout |
| `patch` | the dispatcher | Files, insertions and deletions the task's branch came to against `base_commit`, measured at cleanup — the last instant the branch still exists to diff |
| `worktree_path`, `workspace_id`, `pane_id`, `tab_id` | the dispatcher | Where the task's work is physically happening. Machine-specific; never travels between machines |
| `attempts` | the dispatcher | How many times a lane has been *launched* at the current step. Zeroed by every transition |
| `usage_limit_hold` | the dispatcher | Whether this task is held for its agent kind's own usage-limit message rather than for a dead launch. Marks the first logged hold so repeated observations update `parked_until` without appending duplicate status-log entries. Cleared wherever `attempts` is, and by re-queueing the document |
| `parked_until` | the dispatcher | Epoch seconds before which no lane is started for this task and no reminder is sent to a lane already running one. Written ahead of a launch, when the profile's kind is at or above its `quota_ceiling`, from the tripped window's own `resets_at`; and for a lane whose pane already carries its kind's usage-limit phrase, from an observed exhausted window's reset, or a separate quota-recheck backoff when none is known. Unavailable admission readings also park the task with that backoff. The park lives on the file rather than in the dispatcher, so a dispatcher stopped for the whole wait honours it from cold without taking a reading. Cleared only on a true exit from the hold, never by a deadline simply running out — an expired `parked_until` is left exactly as the file carries it, the pass rechecking quota in memory and answering `false` without touching the park, so no reader sees a half-cleared hold; the same pass then resolves the whole park in one write, re-parking with a fresh deadline or launching a lane that drops all three park fields. Also cleared by re-queueing the document |
| `quota_retries` | the dispatcher | Consecutive quota rechecks, independent of launch attempts. Controls a one-minute-to-one-hour doubling backoff and survives restarts. Cleared on stage changes, requeueing, successful admission, or a running lane leaving its quota hold. Repeated holds do not append duplicate status-log entries |
| `parked_window` | the dispatcher | Which of the kind's two windows `parked_until` came from, `five_hour` or `seven_day`. It only decides how the clock is drawn — a seven-day park keeps its dated shape on its last day instead of collapsing to a bare `HH:MM`. `unknown` for an unavailable pre-launch reading; empty for a mid-turn hold without an observed exhausted window |
| `parked_at` | the dispatcher | Epoch seconds when one continuous quota or usage-limit park began — fixed for the whole hold, unlike `parked_until` beside it, which is only the next recheck deadline and moves every time the park is re-probed. Written once, by whichever dispatcher check first parks the task, and left untouched on every later pass that finds the same hold still in force, including a re-park against a deadline that has run out: a re-park is the same uninterrupted hold, so its age keeps counting from here. The intended reading is the park's age, `now - parked_at`, for a display that wants how long this has been held rather than when the next probe falls; nothing consumes it that way yet, so the field is only written and round-tripped for now. Cleared on a true exit from the hold, wherever `parked_until` and `parked_window` are cleared: `set_stage` and `set_stage_unbanked`, `launch_landed` when a lane of it is seen running, the launch clear in `start_one` once the lane has actually started, and re-queueing the document |
| `launched_at` | the dispatcher | When the last of those lanes was launched. What an unattended run's backoff spaces its retries by, and what the board's TIME column reads while the lane is live |
| `prompts` | the dispatcher | Every lane launched, per route, keyed `from->to` — retries included. What the board and the usage ledger read |
| `rounds` | the dispatcher | How many times the task has arrived at each step, per route, keyed the same way — one per transition, whatever a lane there goes on to do. The only counter a step's `loop` budget is spent from |
| `arrived_from` | the dispatcher | The step the task last moved here from, which is what keys the two counters above: they are banked at launch, and by then the stage is already the destination |
| `launch_failures` | the dispatcher | Consecutive launches of a step that never got as far as running, keyed by step id rather than by route — a launch that never started never had a route to count against. Bumped for both roads a launch takes: an agent lane's start, and a command step's first spawn. On the third the task is routed by that step's `on_fail`, defaulting to `blocked`, with `blocked_from` set to the step it could not start, and the reason written once to the task's `## Status Log` and once to the project's problem log. Cleared the instant a launch of that step actually starts, whatever it goes on to do; cleared again on arrival at a step, so a later visit counts from zero instead of inheriting a spent count from an earlier one. Also cleared by re-queueing the document |
| `last_report` | `spoolway report` | The last outcome a lane reported, with the step it belongs to and when it was banked — what the ledger banks as the lane's verdict, and what a settled lane is checked against to tell a report that has not yet moved the stage apart from a lane that never reported at all |
| `blocked_from` | the dispatcher | The step the task was on when it was escalated. Clearing the block carries it on from there — see `unattended.skip_blocked_lane` |
| `parked_from` | the dispatcher | The step a person interrupted the task from — with the board's `p`, or with their own Escape typed straight into the lane's pane — rather than one it was escalated from — nothing failed, so putting it back banks no lap and gives nothing back, unlike a real block |
| `paused_at` | the dispatcher | The gated step the task is paused on, waiting for a person to let it past. The step rather than the destination it would go to, because the destination is the pipeline's to say and the pipeline may have been edited since — `resume` sends it wherever that step's `on_pass` points *now*, `--reject` by its `on_fail`. Cleared by the resume |
| `resume` | `spoolway resume` | The step whose lane is to be *continued* rather than started fresh, for exactly one launch. Written by every road back to a step a task stopped on, and spent by the next launch of that step whatever comes of it — so a later retry of the same step is a fresh conversation again |
| `skip` | the queue screen's `p` picker | Steps the dispatcher passes this task through without starting a lane. The only *task-side* way to walk past one — a pipeline's own `last:` is the other, and it is a fact about the step. Written for a trial arm, so a throwaway comparison run never pushes a branch or opens a pull request — see [Trials](planning.md#trials) |
| `trial` | the queue screen's `p` picker | The trial this task is one arm of, minted once per trial and stamped on every arm forked together. Absent on a task queued the ordinary way. What lets `spoolway eval --runs --trial <id>` find a trial's arms together in the ledger, since their ids otherwise share nothing but a minted-together prefix (`solo-1`, `solo-2`, …) that is not itself recorded anywhere — see [Trials](planning.md#trials) |
| `replay_of` | nothing, any more | The run a task replayed, for a task `spoolway eval --replay` once wrote. That command, and the `--show` that read this back, are both gone; the field is kept only so a task file written under it still parses and round-trips |

Unrecognised keys are preserved rather than rejected, so a project can carry its own
metadata alongside — `size:`, `complexity:`, anything a project's own tooling wants to read
back later. They round-trip through `Frontmatter`'s `extra` map untouched.

`epic` and `ticket` are the sanctioned example of this: opaque strings written and read
verbatim, with no typed field of their own on `Frontmatter`. `spoolway queue add`'s `open`
hook writes them for you when `[issue_tracking]` is configured — see [`open` — a fifth event,
run by `queue add` itself](configuration.md#open--a-fifth-event-run-by-queue-add-itself). The
same hook writes `slug:` and `url:` the same way, through the same `extra` map, except that
those two are checked once at `queue add` time before they are stored — the slug against
`check_id`'s alphabet, the url as an absolute `http`/`https` address. A
document may also set `ticket:` by hand, to hand a document a ticket it already knows about
rather than have `open` mint one — the same key an inbound producer sets, and the way a failed
batch's own resume leaves a document behind: one that already sets `ticket:` is reported
`kept` at `queue add` and the hook never runs for it.

### What a document may set

A task document — the whole file `spoolway queue add --from` reads — is a producer's only way
onto the queue, so the table above is public surface, and "Set by you" in it means "set by the
document": `id`, `title`, `group`, `source`, `plan`, `touches`, `depends_on`, `parallel`,
`pipeline`, `gate_at`, plus whatever unrecognised keys it carries. `id`, `title` and `group` are
required — a document leaving any of them blank is refused, naming the document and the
field, and so is one with an empty body: an agent would have nothing to work from.

Six keys are spoolway's alone, over a task's whole life, and a document setting one is
refused before anything is queued — the error names both the key and the document it came
from:

```
$ spoolway queue add --from mine.md
mine.md sets `run:`, which spoolway sets on every task itself — remove it from the document
```

`stage`, `run`, `attempts`, `base_commit`, `cut_from` and `branch` are those six. `base` is not
in that list because it is not a document key at all: the checkout `queue add` runs in answers
for it, whatever a document happens to say there. Every other field of `Frontmatter` the table
above lists as "Set by the dispatcher" — `borrowed`, `patch` and the rest — is not refused
either: a document may write one, and it is simply thrown away, the same as `base` is.

`branch` is refused, not thrown away, because a task body is content an agent wrote and
`spoolway stack` force-pushes a squashed commit onto whatever `branch:` names. A document may
not point that anywhere. The check runs every time a task file is loaded, not only at `queue
add`, so a file dropped straight into `queue/` cannot carry a `branch:` past it unless the
value is one spoolway itself could have stamped —
`task/<id>`, or `task/<slug>-<id>` with a slug in `check_id`'s alphabet.

`spoolway task contract`, with no arguments, prints this whole section as JSON — the required,
optional, refused and ignored keys, one sentence per settable key on how to fill it, and the
body skeleton itself, together with what follows below — so a producer that has never read
this page can generate a queueable document from the contract alone rather than from prose.
This page is that contract's human rendering, and the two are checked to agree: a field added
to `Frontmatter` and forgotten in `task contract`'s own groups fails spoolway's build.

### Two constraints no key can express

An id is also a lane name, `<task> · <step>`, at every agent step it visits — the widest one
decides — and a lane name stops at 34 bytes. `check_task_id` in `src/mux.rs` is what
enforces it, at queue time rather than only when a lane for it finally tries to start: an id
budget of `30 - len(longest agent step of that pipeline)` characters, the 4 the separator itself
costs already spent. `spoolway task contract | jq '.pipelines.<name>.id_budget'` prints it for any
pipeline this project defines, rather than requiring the arithmetic to be redone by hand.

`gate_at`, if set, has to name a step of the task's own pipeline — `spoolway task contract` lists
every step id a given pipeline accepts, under that same `.pipelines.<name>` entry, as `gate_at`.
Unlike the id budget above, this is not actually checked: nothing in `queue add --from` validates
a `gate_at` against its pipeline's steps today, so a document naming a step that does not exist
is accepted without complaint — `spoolway report` only ever compares `gate_at` against the step
a lane is *currently* on, so a name that never matches any step simply never pauses the task,
silently. It is true of the system regardless, and is printed for a producer to check itself
against before ever handing a document over.

### What only holds across a set

A `depends_on` is checked against the whole submission, not one document at a time: it must
name a task already in the queue or the archive, or a document queued in this same batch, must
not name the document's own `id`, and must not close a cycle. A task that finished long enough
ago to have aged out of the archive under `retention.days` (see
[`[retention]`](configuration.md#retention--how-long-a-byproduct-directory-keeps-what-it-holds))
can no longer be named this way; the refusal says the age is why. A dependency and its dependent
must also share the same `base` and the same `group` — see [Expressing order](#expressing-order)
below for why. And because `base` is read from the checkout at queue time, every task in one
`depends_on` chain has to be queued from that one checkout: a document naming a dependency
queued from somewhere else can still be accepted, but the base rule above then refuses the pair
the moment both are known.

A `depends_on` naming more than one id is also reordered here, not just checked: the id whose
own history already reaches every other one named beside it is moved to the front, because that
is the one the task's worktree gets cut from — see [Expressing order](#expressing-order). A
document with no such id is refused outright, naming which of its parents would be missing from
that worktree.

### The body is the project's

The body is written once, at queue time, as the rest of the document after its closing `---`
fence. **spoolway never reads it.** No heading in it is required and none is validated — it
exists for the agent that will do the work, and for whoever reads the task afterwards. Nothing
of spoolway's is in it either: no marker, no generated region, and no update touches it. A body
may be a single `## Goal` and everything still works, because `## Status Log`, `## Handoff`
and `## Blocker` are created in the task file if they are not there.

The shipped skeleton has seven sections, and each earns its place:

```markdown
## Context

Three to five facts, one line each. The plan that produced this task is not
something you can read, so this is where its ground goes: what the surrounding
system does today, the decision this task implements, and the constraint that makes
the obvious approach wrong. Facts and names, not argument.

- what is true today that this task changes
- the decision it implements, in one line
- the constraint that would otherwise be discovered the hard way

## Goal

What this task achieves, in a sentence or two, for someone who has not read the
plan it came from.

## Mockup

What this looks like when it is done, drawn as the thing itself. Build what
is here. If it cannot be built as drawn, say so in your report rather than
improvising something near it.

    the panels this task has to match, copied from the plan — delete this
    whole heading if the task does not change anything a person opens

## Non-goals

Out of scope. Doing any of these is a review failure, not a bonus.

- the adjacent thing this task must not do
- anything not required by the acceptance criteria below

## Acceptance criteria

- a statement that is checkably true or false the moment the task is done

## End-to-end coverage

Add or update these. Do not run them: an end-to-end run is CI's, or a person's
with whatever this change actually needs in front of them.

- the end-to-end test this change reaches, or "none", and why

## References

Read these before you start. They describe the system as it is; work from them
rather than restating them — a path here replaces a paragraph above, and stays
true after the code moves.

- path — why it matters
```

`## Context` is the one section with no `## Goal`, `## Mockup` or the rest to lean on: it is
where the plan's own reasoning survives, since the plan itself is not something a lane can read.
`## Mockup` sits between `## Goal` and `## Non-goals`, and it is deleted whole on a task that
changes nothing a person opens — a fix inside a function nobody looks at, a refactor with no
visible face. Where it is present, it is the specification: a reviewer fails a change that does
not match its panels the same way it fails a missing acceptance criterion, and an implementer
who cannot build it as drawn reports that rather than approximating something near it.
`## End-to-end coverage`, between the acceptance criteria and the references, names the
end-to-end test the change reaches — written or updated by the task, never run by it.

Three more sections appear as the task moves, created if they are not there, and appended to
rather than replaced so no writer can clobber another's:

| Section | Owner |
|---|---|
| `## Status Log` | Everyone. One timestamped line per transition |
| `## Handoff` | Any step, via `--handoff`: what the next one should know |
| `## Blocker` | The dispatcher, recording why a task needs a person |

The three headings are spoolway's own — it appends under those names and no others — but what
belongs under each is a project's to say, in `.spoolway/templates/task-log.md`. That file is
prose, one `##` section per heading above, describing in plain language what a lane should
write there; spoolway never renders it and never reads it back, only sends it to a lane as the
`WHAT YOU WRITE DOWN` block of its system prompt — see [What a lane is
handed](prompts.md#what-a-lane-is-handed). A heading the file leaves out, or leaves blank, gets
no row in that block at all, so a lane is told nothing about it; the file being absent
altogether is different — every heading then falls back to spoolway's own built-in wording.
`spoolway init` writes the shipped file; `spoolway update` never touches it once it exists;
`spoolway update --replace .spoolway/templates/task-log.md` takes the shipped wording back.

### What a lane is told that the body does not say

Some of what a lane needs cannot be promised by a body written at queue time, so it is
computed when the lane starts, from the frontmatter: the tasks it waited on — pointed at
their own `## Handoff`, where a finished task's lanes leave what the next one should know —
and the scope its globs set. The plan a task came from is not part of this any more: it is a
path or a slug from whatever checkout queued the task, and neither resolves from inside a
lane's own worktree, so naming it there handed a lane a reference it could never open. What a
task's mockup has to match travels a different way now — copied into the task body's own
`## Mockup` section at plan time, so it arrives with the file itself.

Worked out then rather than written in at queue time, none of it can be forgotten by whoever
queued the task, and none of it goes stale in between — a document renamed after queueing is
still found.

Likewise, anything a prompt could not have known is said by the dispatcher at lane start,
per project and per pass: where the scratch directory is, what the
task file's sections mean. So none of it can go stale in a prompt. Nothing spoolway sends
says anything about git — what a lane does with its branch is the prompt's, in one file.

## Queueing a task

### Get the skeleton

Bare `spoolway queue add`, with no `--from`, prints a document skeleton for the project's
default pipeline: a frontmatter block with the keys most tasks set, and the pipeline's own
body skeleton underneath. Save it, fill it in, and hand it back through `--from`.

Task body skeletons live under `.spoolway/templates/tasks/`, one per pipeline: `bugfix.md`
for the `bugfix` pipeline, falling back to `default.md` for any pipeline without a file of
its own. Name a different pipeline in the document's `pipeline:` key to get that one's body
instead.

### Add it

`spoolway queue add --from` is the path every task takes into the queue — the `spoolway queue`
screen submits through it too. Every document it names is validated together, as one set, and
written all or none:

```
spoolway queue add --from task.md               # one file
spoolway queue add --from a.md --from b.md      # several, in the order named
spoolway queue add --from tasks/                # every *.md file in a directory, by filename
spoolway queue add --from -                     # a `---`-separated stream on standard input
```

The directory a producer writes to by convention is `~/.spoolway/<project>/pending/`, because
that is the one the queue screen lists — see [Queueing a
plan](planning.md#queueing-a-plan). Pointing `--from` at it queues everything waiting there
without a screen involved. `--from` deletes nothing, though; only the screen clears a group's
documents out of that directory, and only once their task files are written.

The queue screen reads documents from a second place too: `.spoolway/routines/` in the
checkout, where a project keeps the work it means to run more than once. That directory is
tracked in git, and its documents are the same shape as any other — see
[Routines](planning.md#routines). Nothing writes to it on a document's way to the queue: a routine
is copied out under a minted id and left where it is.

A document is the same shape as a queued task file: `---\n<frontmatter>\n---\n<body>`. Its
frontmatter is whichever of the keys in [the field table above](#the-frontmatter-is-spoolways)
are yours to set; its body is whatever markdown follows, taken as given. Because the whole set
is checked before anything is written, a `depends_on` naming a sibling document in the same
`--from` is satisfied with nothing sorted first — there is no ordering requirement between
`--from` arguments beyond the order documents are read in.

`--from -` splits the stream on any line that is exactly `---`, the same way it splits multiple
documents apart everywhere else. That means a task's body may not contain a line reading
`---` on its own; put a longer rule (`----`) or fence the line in code if the project's own
markdown needs one.

The task starts on its pipeline's entry step, and its `base` is recorded from the branch the
checkout you ran `queue add` from has out — at that moment, from that checkout. That is what
makes groups independent of one another, and why `base` is never a document's to set.

### Read the queue

```
spoolway queue list          # the dispatcher, and where every task is sitting
spoolway queue show <task>   # one task file, in full
```

A `*.md` in the queue that will not parse — a hand edit left without its closing `---`, an
`id:` that fails validation — is skipped rather than failing the read. Every other task still
loads. The dispatcher, the board and the pending listing all name the bad file so a person can
fix it, and the board prints it in amber under its table.

## Expressing order

Tasks of different groups are independent: separate branches, separate worktrees, nothing to
coordinate. Ordering only matters inside a group, and one field expresses it.

```
---
id: sessions
group: auth
depends_on: [login]
---
## Goal
...
```

A task sits in the built-in `queued` state until every task it names has reached `done`. That
gate is the dispatcher's own and unconditional — it is not a step some pipeline has to
remember to declare — and a dispatch run's board says which task a queued one is waiting
on.

Reaching `done` is what releases a dependent, and it is the only thing that does: a project's
own declared terminal does not, which closes the footgun where a task ending in `superseded`
would quietly let the work behind it proceed.

A dependency is refused at queue time if it names a task that does not exist, names itself,
or would close a cycle. `spoolway doctor` catches the same in a hand-edited task file, and a
dependency graph that cannot be traversed is reported rather than waited on.

A dependency **across two bases** is refused outright: a dependent is cut straight from its
dependency's branch, but that branch only ever merges back into its own group's base, so naming
a dependency on another base would end the wait with the work still out of reach.

A dependency **across two groups** is refused outright too: an edge only ever comes from a plan
page now, and a plan cuts one group at a time, so a `depends_on` naming a task of another group
is always a mistake — a chain does not cross a group. Queue one group, let it land, then queue
the other.

A `depends_on` naming more than one id has to **start with the id that contains the rest**. A
dependent's worktree is cut straight from `depends_on`'s first id — see
[What only holds across a set](#what-only-holds-across-a-set) — and that branch is the only one
its own pull request opens against, so a later id in the list buys nothing but a wait unless the
first id's own history already holds its work too. Queueing a submission reorders each task's
list to put that id first on its own, using the same graph that already answers whether one task
reaches another; a list with no such id — two parents that genuinely have nothing to say about
each other — is refused, naming which of them would be missing from the worktree the first id
would be cut from. A group may still fan out, since two tasks cut from one shared parent are
each cut from a branch holding everything they wait on; what it may not do is join back.

If a dependency ends up blocked, everything behind it reads `unreachable` on the board, and
every dispatch pass says so. The tasks stay where they are, so releasing the
root releases the lot.

```
TASK       STEP       NOTE
login      implement  writes the code
sessions   queued     waiting on: login
profile    queued     unreachable — login is blocked
```

## Finding the edges you are missing

```
spoolway queue conflicts
```

This is what tells you a dependency is *needed*. It reports tasks whose `touches` globs can
name the same file — `src/**` and `src/api/**` overlap, they do not have to match — and says
whether the graph already keeps the pair apart, however many hops separate them. The check
itself, `overlaps()` in `src/globs.rs`, is deliberately approximate in the direction of saying
yes: it compares each glob's literal prefix segment by segment, so a false positive costs one
extra pair in front of this command, which is cheaper than full glob intersection.

Two tasks writing the same file with no ordering between them is how a group produces a
conflict nobody chose. Two tasks that overlap but are already ordered are fine, and are
reported as such.

### Declaring a fan on purpose

Two tasks of one group can also have nothing to say about each other's diff — no chain to
build, because neither depends on what the other does. The dispatcher already starts every
ready task at once, so nothing has to be added for that fan to run; what `parallel: true`
buys is a way to tell "these were never going to depend on each other" from "somebody forgot
the edge", which nothing else in the queue can say.

```
---
id: left
group: fan
touches: [notes/left.md]
parallel: true
---
## Goal
...
---
id: right
group: fan
touches: [notes/right.md]
parallel: true
---
## Goal
...
```

Set on both halves of the pair, it changes what `queue conflicts` and `queue list` say and
nothing else — not `base:`, not the `queued` gate, not which lane a dispatch pass starts. A
`touches` overlap between two `parallel: true` tasks of the same group is reported as a mistake
in the group rather than a missing edge, and `queue list` marks each of them within its group
block. It is a record, not an enforcement: `queue add` still accepts the overlap either way,
and nothing refuses it.

## When a task needs a person

A task escalates to its pipeline's blocked step when a step reports `block`, when a step
spends its `loop` budget and its exit resolves to `blocked`, or when a lane has been started
too many times without leaving a session behind. A spent `loop` that carries on instead —
the default, or an `on_loop_max:` naming somewhere else — writes what happened to `## Status
Log` instead: nothing here is blocked on it. Any other reason is written into the task's
`## Blocker` section, and the board pins the task at the top of its group.

Put it back:

```
spoolway resume <task>
spoolway resume <task> --stage review -m "credentials rotated"
```

`resume` reads which kind of stop a task is on, so you never have to say which. By default it
resumes at the step it was blocked on — a task that blocked at the handover step has a branch
pushed and possibly a pull request open, and re-running the steps that did that would produce
a second one. `--stage` overrides that.

That resume is the blocked lane's own session, continued, not a new one on the same step.
A block is something outside the lane's control, so its work stands — often all of it, with
only the report left to make — and a fresh session would pay to read the task, walk the tree
and rediscover all of it just to arrive back where the last one already was. The lane wakes
up knowing what it did, is told only that a person has been and gone, and carries on. Its
transcript is banked once: the ledger records what the second turn added, not the whole
conversation again. It also hands the task its full launch budget back, since every
transition zeroes the counter.

Two cases start fresh instead, both deliberately. `--stage <step>` is a reroute rather than a
resume, so the step a person names begins its own conversation. And an ordinary retry — a
step reached again through the pipeline, `review` → `fix` → `review` — is a new session every
time, because a retry that reopened the failed attempt would argue with it instead of
starting over.

### Paused is the other one, and it is not a block

A task that passes a step declaring [`gate: true`](pipelines.md#gates) lands on `paused`
rather than moving on. Nothing went wrong: the step did its work and reported a pass, and the
gate says you decide whether it goes further.

A task's own `gate_at: <step>` does the same thing, for one task rather than every task on a
pipeline — set in the document, by whoever wrote it, so holding one task for a person costs a
frontmatter key rather than a pipeline of its own. Both land on `paused` the same way, and
`resume` reads it back the same way whichever one fired.

```
spoolway resume <task>
spoolway resume <task> --reject -m "the migration has not run yet"
```

With no `--stage`, `resume` sends a paused task on by the step's `on_pass` route; `--reject`
sends it back round by `on_fail` instead — or onto `blocked`, same as any other fail, when the
gated step declares no `on_fail` of its own — with your message written into `## Handoff`,
credited to the gated step, for the next lane to work from. `--stage` still reroutes it exactly
as it would a blocked task — naming a step by hand is you overriding the route, gate or no
gate — and `--reject` against a task that is not paused is refused, since there is no gate
there waiting on you.

## Reporting an outcome

This is what a prompt calls when its work is done. You rarely run it yourself.

```
spoolway report --pass -m "implemented and tests pass"
spoolway report --fail -m "acceptance criterion 2 is not met"
spoolway report --block -m "needs a credential I do not have"
```

The task id defaults to the one in the lane's environment, so a prompt names no task.
(Jumping a task to an arbitrary step is `spoolway resume <task> --stage <step>` — a
person's tool, and the same one that restores the launch budget.)

There is a fourth outcome, `--pause`, and it only means anything on `blocked` — every other
step refuses it. It is how a staffed `blocked` step says nothing short of a person can clear
this, parking the task on `paused` at the destination a pass out of `blocked` would have
reached rather than sending it round again; see [Escalation](dispatcher.md#escalation).

A report can also leave a note for whichever step runs next, one `--handoff` per thing worth
saying — independent of the outcome:

```
spoolway report --pass -m "shipped" \
  --handoff "the migration script wants a dry run before the next deploy"
```

Each one lands in the task file's `## Handoff` as `` - `<step>` — <text> ``, credited to the
step that said it, in the same save that routes the task. A dependent task's reading list
points at it — see [What a lane is told that the body does not say](#what-a-lane-is-told-that-the-body-does-not-say)
above — and `spoolway queue show <task>` is how anyone else reads it back.

## Archiving

A terminal step with cleanup enabled removes the task's worktree and branch and moves its
file into the archive directory. The usage ledger is not archived with it — one line per
lane was appended when the lane settled, and that record outlives the task file deliberately,
so cost history survives cleanup.
