---
name: spoolway-pipeline
description: Build or change a spoolway pipeline — the graph of steps in `.spoolway/pipelines/<name>.yml`, and the prompts its agent steps run. Fetches the format from the binary itself, works out the graph, writes the YAML and any prompt it needs, then checks the two agree. Triggered by a human who wants a new flow, a change to one they already run, or a new kind of lane in it.
disable-model-invocation: true
---

# spoolway-pipeline

A step is placement; a prompt is behaviour. Adding a step usually means
writing the prompt it runs. One skill owns both.

## Before you write

    spoolway pipeline contract    the format: every key, every rule, a blank to copy
    spoolway pipeline show        every graph this project runs, defaults resolved
    spoolway prompt contract      what a lane is handed, and the shape to write
    spoolway queue list           what stands on which step — renaming one strands it

Never write the format from memory. It is printed, and the printed one is
what the loader enforces.

## Preferences

- Planning, research and review run on the strongest model available, at
  high effort. These steps decide what every later step does.
- Implementation and documentation run on the cheapest model that can do
  them — a local one where the contract says local models are in play.
- Fewer steps is cheaper. Cut a step before you cut a model: a compact
  pipeline on good models beats a long one on weak ones.

## Procedure

- Read what the project has, from the commands above, never documentation.
- Ask what you cannot decide, through AskUserQuestion, 2–3 concrete options
  each. Always worth asking what one pass has to produce.
- Write the pipeline from the contract's own template, and a prompt for
  every step the project does not already have one for.
- Every pipeline gets a `description:` — the sentences a reader chooses
  between pipelines by. Take it from the human's own words, or from the plan
  `pipeline gen` handed you, verbatim: never write one yourself.
- Check both halves until clean: `spoolway pipeline check`, then
  `spoolway prompt check <name>`.
- Say what changed: the paths, and `spoolway pipeline show`.

## Started by `spoolway pipeline gen`

Same procedure. The command hands you a plan path to read and prints this
project's preferences — take those as answered rather than asking again,
and set every loop the file ends up with to the `loop_default` it names.

## Never

- Never rewrite prose the human brought. Transcribe it, and flag what would
  stop the pipeline running instead of fixing it yourself.
- Never edit a shipped prompt to make a new role — a role is a file of its
  own, written from the contract's shape.
- Never queue tasks or start a dispatcher. That is the queue screen's job,
  and a human's decision.
- Never rename or delete a step a task is standing on without saying so —
  `spoolway queue list` is how you find out.
