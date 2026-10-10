---
name: spoolway-calibrate
description: Review real spoolway runs to explain review failures, blocked sessions, loops, context pressure and spend. Compare lane-written task records with the prompts, pipelines, settings, skills, scripts and code that produced them, then present every finding in one ranked table and apply the ones the person picks. Use when someone wants to learn from finished work and tune how spoolway runs.
disable-model-invocation: true
---

# spoolway-calibrate

Use real runs to improve how work moves through spoolway: its prompts, pipelines, templates and
config, and anything else the runs show is costing them — a script a command step runs, a skill,
a flaky test, the project's own source. Read both the numbers and what lanes wrote in the task
files. Counts show scale and cost. The lane-written
record can show causes that numbers miss. A useful finding may be quantitative, qualitative,
or both.

## Inspect the runs

- Read `housekeeping.calibrate_window` and the archived tasks completed in that window under
  the project home's `archive/`. The home is `~/.spoolway/<label>-<id>/`, never a folder named
  after the repo alone: it is the directory above the `output.dir` that `spoolway task
  contract` prints.
- Read `## Status Log`, `## Handoff` and `## Blocker` — the three fixed sections a lane writes to
  in every archived task — and follow the sequence of events. Work out why review sent work
  back, why a session blocked, what an agent misunderstood, and what later cleared it.
- Collect every `Follow-up:` and `Sibling:` line lanes left in a Handoff. Each is a defect a lane
  saw outside its task; one that no later task fixed is a finding of its own.
- Read `spoolway eval --by step --per-run --since <window>`. Check pass rates, block counts,
  context pressure, tokens per run, cost and time. Use `--step` to inspect a troubled step and
  `spoolway eval --by task --since <window>` to connect its figures to task records. Use
  figures where they help; do not make them a gate for findings. `eval` has no rows for command
  steps, and BLOCKS counts lane blocks only: count gate and suite failures and arrivals at
  `blocked` from each archived task's `steps:` edges and Status Log.
- Read the pipelines, prompts, templates and settings involved in those runs. Compare what the
  agents did with what the control plane asked them to do.
- Read every skill named in `skills:` on a step those runs passed through. A skill lives in the
  project's own skills directory for the step's agent kind (`.claude/skills`, `.agents/skills` or
  `.pi/skills`), in the user's home, or in a plugin.
- Read the scripts command steps run, and the code, tests and tooling lanes kept tripping over —
  a gate that fails for reasons outside the diff, a test several tasks fixed separately, a
  shared resource that hands one lane another's state.

## Form findings

Use the step figures to find low pass rates, high block counts and other costly trouble. Use the
lane-written record to explain why it happened. Look for repeated waste and for clear failures
in a single run. Check instructions, step ownership, routing, information passed between steps,
gates, task shape, model fit, context pressure, timeouts and concurrency.

Trace each finding from the task record to whatever caused or failed to prevent it: a prompt, a
setting, a skill, a script, a test or the source. Separate evidence from inference. Cite the task and the relevant
lane-written text.
Use counts and spend when they add meaning, but do not invent precision or drop a sound finding
only because it has no useful number.

Keep the report short. Present every grounded finding, ranked by likely value, wherever its fix
lands. For each one, state the problem, the evidence, the likely cause, and the fix. Say when the
evidence is limited or another cause is still possible.

Write the report from `assets/report.md`, beside this skill: copy its shape and fill it, keeping
its headings and its table's columns. Every finding is one row of that table, with the fix you
propose, the kind of file it lands in, and the files it touches with the size of each edit in
lines. Whoever reads it is choosing which rows to apply, and the cost of a change belongs beside
the case for it. The evidence — the task name and the lane's own words — goes in the prose above
the table, never inside a cell. Where a finding has two honest fixes, put both in the cell with
their two sizes, and say which you would take.

Make prompt fixes exact and short. Add only what is needed to stop the issue from happening
again, without repeating rules already present. Also check existing prompts for stale text.
Recommend removal only when the run record or current control plane gives a clear reason that
the text is no longer true or useful. Name the text and the reason. For a pipeline fix, name
the exact step, route or setting to change.

## Make improvements

Change nothing until the person has picked rows from the table. Then apply exactly those, each
with the file kind's own care:

- A prompt, pipeline, template or config: read its matching contract from the spoolway binary
  first. Use an override for a trial and a tracked edit for a change meant to stay. Run
  `spoolway pipeline check` after a prompt or pipeline change.
- A skill, script, test or source file: a direct, tracked edit, written the way the surrounding
  code is written. Run the project's own checks for what you touched — its tests, its formatter,
  the gate a command step runs.

A fix too large to make well here — a refactor, a feature — says so in its size, and the person
chooses between a direct edit now and a task to queue. Do not hand a change to another skill
unless the person asks. Do not edit archived task records.

Report what changed, what evidence led to it, and how to undo an override.
