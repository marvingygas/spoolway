---
name: spoolway-calibrate
description: Review real spoolway runs to explain review failures, blocked sessions, loops, context pressure and spend. Compare lane-written task records with the prompts, pipelines and settings that produced them, then recommend and apply control-plane improvements. Use when someone wants to learn from finished work and tune how spoolway runs.
disable-model-invocation: true
---

# spoolway-calibrate

Use real runs to improve spoolway's prompts, pipelines, templates and config. Read both the
numbers and what lanes wrote in the task files. Counts show scale and cost. The lane-written
record can show causes that numbers miss. A useful finding may be quantitative, qualitative,
or both.

## Inspect the runs

- Read `housekeeping.calibrate_window` and the archived tasks completed in that window under
  `~/.spoolway/<project>/archive/`.
- Read `## Status Log`, `## Handoff` and `## Blocker` — the three fixed sections a lane writes to
  in every archived task — and follow the sequence of events. Work out why review sent work
  back, why a session blocked, what an agent misunderstood, and what later cleared it.
- Read `spoolway eval --since <window>` by step. Check pass rates, block counts, lanes per run,
  context pressure, cost and time. Use `--step` to inspect a troubled step and
  `spoolway eval --runs --since <window>` to connect its figures to task records.
- Read `spoolway spend step --since <window>` for total step costs. Use figures where they
  help; do not make them a gate for findings.
- Read the pipelines, prompts, templates and settings involved in those runs. Compare what the
  agents did with what the control plane asked them to do.

## Form findings

Use the step figures to find low pass rates, high block counts and other costly trouble. Use the
lane-written record to explain why it happened. Look for repeated waste and for clear failures
in a single run. Check instructions, step ownership, routing, information passed between steps,
gates, task shape, model fit, context pressure, timeouts and concurrency.

Trace each finding from the task record to the prompt or setting that caused or failed to
prevent it. Separate evidence from inference. Cite the task and the relevant lane-written text.
Use counts and spend when they add meaning, but do not invent precision or drop a sound finding
only because it has no useful number.

Keep the report short. Present every grounded finding, ranked by likely value. For each one,
state the problem, the evidence, the likely cause, and a recommended prompt or pipeline fix.
Say when the evidence is limited or another cause is still possible.

Make prompt fixes exact and short. Add only what is needed to stop the issue from happening
again, without repeating rules already present. Also check existing prompts for stale text.
Recommend removal only when the run record or current control plane gives a clear reason that
the text is no longer true or useful. Name the text and the reason. For a pipeline fix, name
the exact step, route or setting to change.

## Make improvements

Calibration includes updating the control plane. Walk the useful findings with the person. Once
they choose a change, edit the relevant prompt, pipeline, template or config directly. Use an
override for a trial and a tracked edit for a change meant to stay. Do not turn the change into
a task or hand it to another skill unless the person asks.

Before writing, read the matching contract from the spoolway binary. After writing, run the
relevant check, including `spoolway pipeline check` for prompt or pipeline changes. Report what
changed, what evidence led to it, and how to undo an override.

Stay inside spoolway's control plane. Report product or source-code problems when the run
reveals them, but do not change them as part of calibration. Do not edit archived task records.
