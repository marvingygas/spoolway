---
name: spoolway-calibrate
description: Review real spoolway runs to explain review failures, blocked sessions, loops, context pressure and spend. Compare lane-written task records with the prompts, pipelines, settings, skills, scripts and code that produced them. Present every finding in tables, one per kind of fix, and apply the ones the person picks. Use when someone wants to learn from finished work and tune how spoolway runs.
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
  contract` prints. Skip a task whose only steps were command steps that ended it at `done`,
  such as a routine's check that found nothing to do.
- Read the project's problem log, `~/.spoolway/logs/<label>-<id>.log`, for the window. Read the
  failed command logs in the folder beside it, `~/.spoolway/logs/<label>-<id>/`. If that folder
  is missing or empty, go on without it.
- Read `## Status Log`, `## Handoff` and `## Blocker` — the three fixed sections a lane writes to
  in every archived task — and follow the sequence of events. Work out why review sent work
  back, why a session blocked, what an agent misunderstood, and what later cleared it.
- Read `spoolway eval --by step --per-task --since <window>`. Check pass rates, block counts,
  context pressure (the `CTX AVG` column), turns, tokens per task, cost and time. Use `--step` to inspect
  a troubled step and `spoolway eval --by task --since <window>` to connect its figures to task
  records. Read a single task's peak context as `CTX PEAK` there. Use
  figures where they help; do not make them a gate for findings. Its "Command steps" table
  counts gate and suite runs. BLOCKS counts lane blocks only.
- Read where the tokens went, for the steps that cost the most:
  - Turns and context: cache reads grow with TURNS/TASK times CTX AVG. Name which of the two moved.
  - Between versions of the step: `spoolway eval --by version --step <step> --per-task`.
  - Inside one version: `spoolway eval --by task --sort when:asc --per-task`.
  - Per complexity point, where tasks carry `complexity`: a task far above its neighbours' cost
    per point is a finding, even when it passed.
- Look across tasks. The same cause, missing tool or note in several tasks' records is one
  finding, with its count.
- Look for steps that spend tokens and change nothing: no diff, no capture, no edit. Name the
  step and how often it ran empty.
- For a lane that went wrong, read `spoolway prompt contract --task <id> --step <step>` to see what it was handed.
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

Never propose:

- a change to how an agent's permissions are set: permission mode, allow lists or sandbox
  settings. Drop a finding whose only fix is one of those.
- more knowledge in the prompt of the step that does the main work, usually the longest-running
  one. Move the fix to a cheaper step, a gate or a skill, or leave it out.

A review sending work back is expected. A low review pass rate is a finding only when another
file could remove its cause.

Trace each finding from the task record to whatever caused or failed to prevent it: a prompt, a
setting, a skill, a script, a test or the source. Separate evidence from inference. Cite the task and the relevant
lane-written text.
Use counts and spend when they add meaning, but do not invent precision or drop a sound finding
only because it has no useful number.

Keep the report short. Present every grounded finding, wherever its fix lands. For each one,
state the problem, the evidence, the likely cause, and the fix. Say when the evidence is limited
or another cause is still possible.

Write the report from `assets/report.md`, beside this skill: copy its shape and fill it, keeping
its headings and its columns. The report is tables and nothing else. Print nothing above the
first table and nothing below the last. This rule covers the findings report only. The report
after rows are applied is described under "Make improvements".

- Make one table per kind of fix, under a `###` heading. The kinds, in this order, are pipeline,
  prompt, config, template, skill, script, test, source and docs. Leave out a kind that has no
  row.
- The columns are `# | Finding | Evidence | Proposed fix | Risk | Reward`. Row numbers run on
  across tables, so the person can name a row by its number alone.
- Evidence is the task name and the lane's own words, short enough to sit in the cell.
- Rate Reward as `+`, `++` or `+++`. Use `+` for a small saving, or one task's trouble. Use `++`
  for a steady saving across several tasks. Use `+++` for a failure or a cost that recurs across
  the window.
- Rate Risk as `-`, `--` or `---`. Use `-` when one file and one step are touched and the change
  is easy to undo. Use `--` when several steps or pipelines read the file. Use `---` when every
  lane reads it, or it changes what reaches main.
- Order the rows in each table by reward first, highest on top. Break a tie by lowest risk.
- Where a finding has two workable fixes, keep both in the Proposed fix cell and name the one to
  take.

Make prompt fixes exact and short. Add only what is needed to stop the issue from happening
again, without repeating rules already present. Also check existing prompts for stale text.
Recommend removal only when the run record or current control plane gives a clear reason that
the text is no longer true or useful. Name the text and the reason. For a pipeline fix, name
the exact step, route or setting to change.

## Make improvements

Change nothing until the person has picked rows from the tables. Then apply exactly those, each
with the file kind's own care:

- A prompt, pipeline, template or config: read its matching contract from the spoolway binary
  first. Use an override for a trial and a tracked edit for a change meant to stay. Run
  `spoolway pipeline check` after a prompt or pipeline change.
- A skill, script, test or source file: a direct, tracked edit, written the way the surrounding
  code is written. Run the project's own checks for what you touched — its tests, its formatter,
  the gate a command step runs.

Some fixes are too large to make well here, such as a refactor or a feature. Say so in that
row's Proposed fix cell. The person then chooses between a direct edit now and a task to queue.
Do not hand a change to another skill unless the
person asks. Do not edit archived task records.

Report what changed, what evidence led to it, and how to undo an override.
