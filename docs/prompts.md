---
domain: prompts
covers: ["src/prompt.rs", "src/compose.rs", "assets/prompts/**"]
---

# Prompts

A prompt is the prompt file a step runs its lane as. It is a mechanism, not a cast list:
spoolway has no built-in roles, no reviewer it knows about, no implementer it treats
specially. A step names a file, the file is prose, and that is the whole of it — adding a role
to the pipeline is two files, the prompt and the step that runs it.

Six prompts ship, and they are there because the [sample workflow](#the-sample-workflows-cast)
has to be staffed by somebody. Read them as a worked example of the shape, not as the set to
work from: a project that deletes all six and writes its own is not working around anything,
and nothing in the binary will notice.

## Where a prompt lives

A prompt is a directory under `.spoolway/prompts/` holding a `PROMPT.md` — and whatever
assets the role carries beside it — named by a step's `prompt:` field, which defaults to the
step's own id. So a step with `id: review` and no `prompt:` runs `review/PROMPT.md`; the
shipped pipelines point their review step at `reviewer` instead. A flat
`.spoolway/prompts/<name>.md` from a project set up before the directory shape still
resolves, so an upgrade finds its prompts.

## A prompt is prose, and nothing else

There is no format. A prompt file is Markdown a project writes, and spoolway never parses a
word of it — no markers, no fingerprint, no generated region:

```markdown
You review one task's diff and deliver a verdict. You do not fix anything.

## What to do
...
## Never
...
```

The shipped prose is a starting point: a project sharpens it in place, wherever its own
commands, conventions and standards belong, rather than in one section set aside for them.
**`spoolway update` never touches a prompt**, so upgrading spoolway cannot disturb a word
you wrote in one. The price is that a sharper default in a later
release does not reach you on its own: [`spoolway prompt check`](#writing-your-own) is
what tells you when your prose has fallen behind the CLI, and `spoolway update --replace
.spoolway/prompts/<name>/PROMPT.md` is how to take a shipped one back on purpose.

## A prompt is a section of the system prompt, not the whole of it

A lane is sent two things, and the prompt is inside the first of them.

**The system prompt** is composed at launch, in three parts and this order: spoolway's
framing (what a lane is, which step and task this is, what follows from that, `WHAT YOU HAVE`
and `WHAT YOU WRITE DOWN`), then the project's prompt *verbatim*, then everything true of this
pass — the gate, a fix pass's own paragraph, a failed command step — and the report contract
that closes it. It is written to `<task> · <step>.md` under the project's own home
(`~/.spoolway/<project>/system-prompts/`) and handed to the agent through the profile's
`{prompt_file}`.

**The typed message** is what arrives in the pane once the agent is up, and it is one
sentence: where the task file is. The report contract lives in the system prompt above, not
here.

The file on disk is untouched by any of this. `.spoolway/prompts/<name>/PROMPT.md` stays
the project's outright, spoolway never rewrites a word of it, and the composition happens
per lane. What the composition buys is that **spoolway's half cannot be dropped**: a project
that deletes every shipped prompt and writes its own still gets the framing: what a lane is,
what the outcomes mean, where it may write. Before, that prose rode in the typed message —
the half a model reads as *a request* rather than as *the rules* — and a project could keep
the config while losing everything that made it mean anything.

Everything conditional is **derived from the step the lane is on**, so each paragraph is
sent exactly where it is true — and never has to hedge about whether it applies:

| Paragraph | Sent when |
|---|---|
| That the step the task arrived from failed it back, and to work from exactly what that step wrote to `## Handoff` rather than re-litigating the verdict | the step the task arrived from routes its `on_fail` to this step and its `on_pass` somewhere else |
| That a person will read this pane once it reports, and to leave anything viewable running and write them a short account | the step declares `gate:`, or the task's own `gate_at:` names it |
| That a command step failed into this one, and the path to the log holding its whole output | the step the task arrived from is a command step whose `on_fail` is this step |
| That it blocked, that nobody is coming, and that clearing the obstacle is now its own job | the lane is being resumed in an [unattended run](pipelines.md#unattended-runs) |
| `spoolway queue list`, `spoolway queue show` and `spoolway lane`, offered outright rather than named | the step is `blocked` |

**`WHAT YOU HAVE`**, inside `YOUR LANE`, is what replaced naming a spoolway fact and leaving a
prompt to compose the command from it: the diff command and the log command for this
lane's own change, its scratch path, and what it sits on — a dependency's branch, or its own
`base:` — with one `spoolway queue show` line per entry of `depends_on`, naming what
finishing each one left behind. Every value is resolved for the real task rather than
described, which is also why a prompt never has to say `base:`, `depends_on:` or
`$SPOOLWAY_SCRATCH` to get at any of it.

**`WHAT YOU WRITE DOWN`** sits directly after `WHAT YOU HAVE`, drawn the same way: one row per
heading spoolway appends to a task file — `Status Log`, `Handoff`, `Blocker` — with what
belongs under it. The prose is a project's own, in `.spoolway/templates/task-log.md`; a
heading that file leaves out, or leaves blank, gets no row at all, and the file being absent
altogether falls every heading back to spoolway's own wording — see [Tasks and the
queue](tasks.md#the-body-is-the-projects).

Three paragraphs used to live here and no longer do — one per value of a `merge:` setting,
each telling the lane what merging meant on this project. They were the reason the merge
prompt could not be replaced: whatever a project wrote in that file, spoolway composed its
own instructions about git around it. Nothing spoolway sends now says anything about git.

`spoolway prompt contract` prints both halves for a given step, rendered from this
project's own pipeline rather than from documentation.

## What every lane is told about spoolway

The system prompt opens by situating the lane, before it says anything about the role. It
names the step and the task the lane actually has — and it defines "lane" in the sentence
that introduces it, because that is spoolway's word and the reader has no glossary. Five
things follow:

- **One step's worth of the job, and nothing enforces it.** Other steps of this task, and
  other tasks elsewhere, are somebody else's turn.
- **Not a conversation.** Nobody is reading the pane, and no follow-up is coming — unless the
  step is gated, in which case this bullet itself says a person reads this pane once the pass
  lands, and a paragraph in `THIS PASS` tells the lane what to do about that: leave anything
  viewable running and write a short account for them.
- **Nothing will wake you.** Not a background job, not a timer — anything the lane has to
  wait for, it polls inside the turn.
- **Reporting is the only exit.** A turn ended any other way stalls the task.
- **Commit as you go.** Anything left uncommitted is committed for you in one lump when the
  lane reports.

Five short bullets rather than four long paragraphs: each keeps only the clause that makes it
binding, and the reasoning that used to surround it is cut. `spoolway prompt contract`
prints the words that actually reach a lane; this list is a summary of them.

That section exists because a model handed only a task file and a role reads a solo
assignment. The four mistakes that follow — widening past the step, asking a question into
a pane nobody watches, ending the turn on something that was going to wake it, treating a
stated boundary as an obstacle — all rest on the same missing premise, and a few sentences
remove it. It is in the binary rather than in every prompt file for the
same reason as everything else here: it is a fact about the lane, not about the role — and a
project that has replaced the shipped prompts wholesale still gets it.

**One step gets its own first bullet, and its own block of tools.** A staffed `blocked` step
is the one lane for which "one step's worth of the job" is not true: it is reached by
whatever stopped *this* task, on whichever step that was, and the fix is as often outside the
task's own worktree — the mainline, a missing tool — as inside it. `spoolway prompt
contract` on a step named `blocked` shows the rewritten opening; every other step's framing
comes out byte for byte the same as it always has. That lane alone also gets a `READING THE
RUN` block, sitting right after the prompt and before `THIS PASS`, naming `spoolway queue
list`, `spoolway queue show` and `spoolway lane` outright: clearing a block is often a
question about another task or another lane's own transcript, and every other step goes
without the tokens and the wider command surface. `eval`, `doctor` and `config get` reach no
lane at all — each answers for a run or an installation as a whole, which is a person's
question.

The third is the one a capable agent walks into and a weak one never reaches. Told to wait
for something slow, it backgrounds the wait and ends the turn, because in the harness it was
trained in a finished background job wakes it up again; here nothing does, and what it leaves
is the settled pane a lane holding a question leaves. The board cannot tell those apart —
[it deliberately does not read the pane to guess](dispatcher.md#a-lane-that-settles-without-reporting)
— so it reports `waiting on you` and names a pane with no question in it.

**A report can leave a note for whichever step runs next.** `--handoff` rides on any outcome:
one per thing the next step should know, written into the task file's `## Handoff` as
`` - `<step>` — <text> `` in the same save that routes the report. It repeats.


## What a lane is handed

The contract is emitted by the binary rather than written down, so it cannot go stale:

```
spoolway prompt contract                    # the default pipeline's first agent step
spoolway prompt contract --step review      # what that lane is handed, and may do
spoolway prompt contract --task login       # what a real lane was handed, when one misbehaved
```

It prints seven sections:

**1 — Where the prompt goes.** The file path this step will read, and the flag it reaches
the agent through.

**2 — The system prompt this lane is started with.** The whole composed thing, rendered
against a sample task or against a real one with `--task`: the framing, the prompt in the
middle, and this pass's policy after it. Only the middle section is the project's, and a
prompt should not restate the rest. This is where anything a prompt could not have known
is said — whether the step is gated, which command step failed into this one and where its
log is, in `WHAT YOU HAVE`, the actual diff command, log command and scratch path this
lane's own change is read and written through, and in `WHAT YOU WRITE DOWN`, what belongs
under each heading this project's `task-log.md` describes.

**3 — The message typed into its pane.** One sentence naming the task file, with a step's
`skills:` invocations ahead of it if it names any. The report contract is above, in the
system prompt, not repeated here.

**4 — The environment every lane has.**

| Variable | What it is |
|---|---|
| `SPOOLWAY_TASK` | The task's id — which is why reporting needs no argument |
| `SPOOLWAY_TASK_FILE` | The task file's path, the same one the typed message names |
| `SPOOLWAY_REPO` | The project root — not your worktree |
| `SPOOLWAY_STEP` | Which step this is. Not yours to read: a prompt that branches on it is two prompts |
| `SPOOLWAY_WORKTREE` | Your worktree — already the working directory, so rarely needed |
| `SPOOLWAY_SCRATCH` | Writable space outside the worktree, one per task, removed when the task is archived |

**5 — What a lane may reach.** Whatever the person running the dispatcher can. Nothing
confines a lane to its worktree or bounds which commands it runs, which is why this section
is two short paragraphs rather than a table: a prompt is the only place spoolway states what
a step is *for*, so a role that should stay narrow has to say so in its own prose and hold
itself to it. Nothing inside spoolway checks or holds whatever a prompt says — see
[Reach](concepts.md#reach).

**6 — How a lane finishes.** The three outcomes, and where each one takes this task in this
pipeline. On a `blocked` step it is two forms rather than three: `--pass` and `--pause`.

**7 — The shape to write.** The headings this project's prompts are written in, and the rule
behind them: write only what the model does not already know.

A seventh section once said something else — "enforced, not requested", listing what the
guardrails refused before a command ran. It went with them.

## The sample workflow's cast

The `default` and `bugfix` pipelines need somebody standing on each of their steps, and five
of these six are who; the sixth, `summariser`, is run by `spoolway stack` rather than by a
step. They are a demonstration of the division the contract describes — one role, one
step's worth of the job, no routing, no powers — rather than the roles spoolway is built
around. Not one of them is named anywhere in the binary.

| Prompt | Its role in the sample workflow |
|---|---|
| `implementer` | Implements one task in one worktree, then stops. Reads before it writes, checks its assumptions, works in steps small enough to bisect, holds itself to the task's acceptance criteria and nothing wider, and treats a task's `## Mockup` as the specification — reporting `--block` rather than approximating one it cannot build as drawn |
| `reviewer` | Reviews one task's diff and delivers a verdict. Judges acceptance and standards as two separate things, fixes nothing, and fails a change that does not match the task's own `## Mockup` — while holding a task with no mockup to no such standard |
| `reproducer` | Captures a bug as a failing repro and runs it. Run twice by the bugfix pipeline — before the fix, where the repro failing is the point, and after it, where the same repro passing is. Fixes nothing |
| `archivist` | Maintains the domain documents. One document per domain, describing the system as it is now — never a changelog. Scoped to its own task's diff, so what it writes lands in the pull request that changed the behaviour |
| `unblocker` | Staffs the `blocked` step, only in an unattended run. Reads why a task stopped and either clears it — a broken mainline, a stale branch, a missing tool — or, only when the thing genuinely cannot be done, pauses the task for a person. Never merges, never touches another lane's pane, never marks a task done — see [Staffing `blocked`](pipelines.md#staffing-blocked) |
| `summariser` | Writes one pull request's title and body from the task file, for `spoolway stack` when `[stack.summary]` names an agent and a model. The title is a Conventional Commits line — `feat(queue): add a --dry-run flag`. No git, no `gh`, no diff, no file of its own — the command around it does all of that and reads only what the prompt prints. Named by no pipeline step — see [`spoolway stack` hands the change over](pipelines.md#spoolway-stack-hands-the-change-over) |

## Writing your own

Write `.spoolway/prompts/auditor/PROMPT.md`. There is no command that does it for you and
nothing to register it with: the file is the whole of it, and the step naming it is the only
place it is ever mentioned.

The shape is one opening line saying what the role does and where its work stops, then the
headings `spoolway prompt contract`'s own seventh section prints — `## What you are looking
at`, `## How to do it here`, `## Never` — as headings and bullets a small local model can
skim, one default per choice rather than a menu. `spoolway prompt show implementer` is the
house style, and the `spoolway-pipeline` skill carries the same shape in its own procedure.

Then wire the step that runs it, in the pipeline file — the two are written together,
because each is half of the same decision:

```yaml
  - id: audit
    agent: claude
    prompt: auditor
    on_pass: document
    on_fail: implement
```

**The step declares placement; the prompt declares behaviour.** That division is the whole
design, and nothing enforces it at run time — a lane reaches whatever the person who started
the dispatcher reaches. So the checks read the prose instead, before a lane is ever started:

```
spoolway prompt check               # read every prompt against the steps that run it
spoolway prompt check auditor       # just one
spoolway pipeline check             # includes the above
```

What they catch:

- **a `spoolway` command or flag this release does not have**, validated against the CLI's
  own definitions. This is the check that replaces the update cycle: nothing rewrites a
  prompt for you, so prose that has fallen behind the binary has to be found by reading
- a prompt that names routing (`on_pass`, `on_fail`, `stage:`), which is the pipeline's
  business and the thing that stops a prompt being movable
- spoolway's own vocabulary, named where a prompt no longer needs it: a frontmatter field
  (`base:`, `depends_on:`, `touches`), a `SPOOLWAY_…` variable, or a `spoolway` command.
  Everything one of those would have said is resolved and handed to the lane in its prompt,
  under `WHAT YOU HAVE`, so a prompt still naming it has learned not to trust its own
  briefing. `eval`, `doctor` and `config` are a person's view of the run or the
  install, never a lane's, and a finding for one of them says to delete the line rather than
  pointing at a replacement. Naming the tool itself, with none of the above, is caught too —
  `.spoolway/`, this project's own layout, is exempted

The `spoolway-pipeline` skill walks the whole procedure with a coding agent: read the
contract, settle the role with you, write the file, wire the step, check both halves. It owns
the graph and the roles on it together, because a step and the prompt it runs are halves of
the same decision.

**A whole flow is the same skill, one level up.** A review procedure your team already
follows, a release runbook, the sequence a CI config half-encodes — describe it and it works
out which parts are steps and which are instructions belonging inside one, then writes both.
See [Converting a workflow you already
run](pipelines.md#converting-a-workflow-you-already-run).

## Reading what you have

```
spoolway prompt list          # each prompt, and what runs it
spoolway prompt show <name>   # one file
```

The list says nothing about the state of a file, because there is no state to report:
spoolway would do nothing to any of them. Whether a project has tailored its prompts or is
still running the shipped prose is not spoolway's to have an opinion about, and a column
reporting it would be an opinion.

## Why prompts do not drift

Four properties, together:

1. **A prompt says nothing about routing.** It reports an outcome; the graph resolves the
   destination. So a prompt survives a reshaped pipeline unedited.
2. **A prompt says nothing about powers.** There are none to name: every lane reaches what
   the person running the dispatcher reaches, so there is nothing a rewrite could quietly
   acquire.
3. **A prompt says nothing it could not have known.** Project-specific, run-specific and
   policy-specific facts are said by the dispatcher at lane start, in the sections it
   composes around the prompt — not written into a file that goes stale. Which also means
   a project cannot lose them by rewriting its prompts: what its config decides is said by
   spoolway either way.
4. **Nothing in a prompt is generated.** So there is no half of the file an upgrade has to
   keep current, and no upgrade that can lose what you wrote. What can still drift is a
   command name, and `spoolway prompt check` reads for exactly that.

What was ever missing was not the mechanism — it was knowing what to write against. That is
what the contract prints.
