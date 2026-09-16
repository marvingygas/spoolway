---
name: spoolway-config
description: Change anything in spoolway's control plane — the pipelines in `.spoolway/pipelines/*.yml`, the prompts their agent steps run, `config.toml`, the task and lane templates, and the issue-tracking hooks — as a temporary override that leaves the checkout clean, or as an edit to the tracked file. Use it whenever somebody wants a different model, effort, timeout, concurrency, prompt or step anywhere in spoolway, whether they said override, try, tweak, change or set. Fetches every format from the binary itself and checks what it wrote.
---

# spoolway-config

Two different jobs share this skill because they share a graph: a step is
placement, a prompt is behaviour, and a value on either — a model, an
effort, a timeout, a concurrency — is what makes the step run the way
somebody wants. Designing a flow from scratch and tweaking one already
running are the same territory at different sizes.

The flow itself is the person's design. This skill sets one up from
scratch, or edits one they already run, and keeps every contract holding
while it does — it does not decide the shape for them, and it does not
decide which layer a change belongs in without saying so.

## Before you write

    spoolway pipeline contract    the graph: every key, every rule, a blank to copy
    spoolway pipeline show        every graph this project runs, defaults resolved
    spoolway prompt contract      what a lane is handed, and the shape to write
    spoolway config contract      every setting, its values, its default
    spoolway override contract    what a patch may say, and what it may not
    spoolway override list        what is layered right now, and over what
    spoolway template contract    the task, lane-prompt and PR shapes
    spoolway hook contract        the environment an issue-tracking hook is handed
    spoolway queue list           what stands on which step — renaming one strands it

Never write a format from memory. Every one above is printed, and the
printed one is what the loader enforces.

## Preferences

- Planning, research and review run on the strongest model available, at
  high effort. These steps decide what every later step does.
- Implementation and documentation run on the cheapest model that can do
  them — a local one where the contract says local models are in play.
- Prefer fewer steps, but not because fewer is always cheaper. A step
  asked to hold too much in one pass pays for it in laps, context
  pressure and blocked lanes, and splitting it can cost less than keeping
  it whole. Judge by the prompt: one role, one pass, one thing to
  produce. Where a prompt wants two of those, put splitting to the person
  as an option; otherwise leave the step whole.

## Procedure

- Read what the project has, from the commands above, never documentation.

**Building or reshaping a flow** — a new pipeline, a new step, a step split
off an existing one:

- Ask what you cannot decide, through AskUserQuestion, 2–3 concrete options
  each. Always worth asking what one pass has to produce.
- Write the pipeline from the contract's own template, and a prompt for
  every step the project does not already have one for.
- Every pipeline gets a `description:` — the sentence a reader chooses
  between pipelines by. Take it from the human's own words, or from the plan
  `pipeline gen` handed you, verbatim except for the one narrow rewrite this
  skill ever makes to a human's prose: compressing it to a single sentence
  when it runs longer, per the exception below.
- Check it clean: `spoolway pipeline check`, which reads every prompt
  against the steps that run it too.

**Changing a value** — a model, an effort, a timeout, a concurrency, a
whole prompt, or any key `spoolway config contract` lists:

- Reach for the override layer by default. It is what leaves the checkout
  clean:
  - a step's field: `spoolway pipeline override <name> --set <step>.<key>=<value>`
  - a whole prompt: `spoolway prompt override <name>`, then edit the fork it writes
  - a config key: `spoolway config override` (opens `overrides/config.toml`),
    or set one key by hand in that file
- Edit the tracked file instead only when asked to make the change
  permanent, or when a layered patch is already right and ready to keep —
  then `spoolway override promote <target>` writes it in and clears the
  layer, an ordinary diff ready to review and commit.
- Say which of the two you did, every time, and how to undo it: `spoolway
  override drop <target>` clears a layered change; a tracked-file edit is
  undone the way any tracked change is, by hand or with git.

- Say what changed: the paths touched, and `spoolway pipeline show` where a
  step moved.

## Started by `spoolway pipeline gen`

Same procedure, the flow-building half. The command hands you a plan path
to read; nothing else is answered for you.

## Never

- Never rewrite prose the human brought. Transcribe it, and flag what would
  stop the pipeline running instead of fixing it yourself — with one
  exception: a `description:` longer than one sentence is compressed to
  one, since that field is what `spoolway pipeline show` renders and a
  reader chooses a pipeline by.
- Never edit a shipped prompt to make a new role — a role is a file of its
  own, written from the contract's shape.
- Never queue tasks or start a dispatcher. That is the queue screen's job,
  and a human's decision.
- Never rename or delete a step a task is standing on without saying so —
  `spoolway queue list` is how you find out.
- Never reach past `.spoolway/` and its override layer. Every format this
  skill routes to is printed from inside that boundary; a setting outside it
  is not spoolway's to change.
- Never teach a model choice to a lane's own prompt — the models and
  subagents a step's prompt spins up inside its own turn are outside
  spoolway entirely, appear in no ledger, and are not this skill's to
  configure. `agent:`/`model:`/`effort:` on a *step* are the whole of what
  spoolway sets.
