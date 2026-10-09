## Context

Three to five facts, one line each: what is true today, why it has to be
seen running rather than read, and what the check needs that is easy to get
wrong. Facts and names, not argument.

- [[what the check is about, as it stands today]]
- [[why only a live run can answer it]]
- [[what the run depends on: a model, a config key, a binary]]

## Intend

[[The question this probe answers, in a sentence or two, for someone who has
not read the plan it came from.]]

## Non-goals

Out of scope. A probe changes no code, so a fix it points at is a follow-up
task, never this one.

- [[the adjacent thing this probe must not do]]
- changing any file in the repository

## Acceptance criteria

Each criterion names a run and the evidence that settles it, not the result
you hope for. A probe that shows the wrong thing has still done its job.

- [[how many runs, staged how, and what each one records]]
- [[the evidence the Handoff holds for every run]]

## References

Read these before you start. They describe the system as it is.

- [[`/abs/path/to/plan.html#d-record` — the decision record this probe checks]]
- [[path — why it matters]]
