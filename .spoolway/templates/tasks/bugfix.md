## Context

Three to five facts, one line each. You cannot read the plan this came from, so
this is where its ground goes: what the surrounding code is for, and what is already
known about where the bug lives. Facts and names, not argument, and no theory of the
cause — that is what reproducing is for.

- [[what the affected code is responsible for]]
- [[what is already ruled in or out]]

## Bug

[[What goes wrong, in a sentence or two, for someone who has not seen it happen.]]

## How to see it

The reproduce step turns this into a failing test or script; give it enough to
start from.

- observed: [[what actually happens, verbatim where possible]]
- expected: [[what should happen instead]]
- since / where: [[a version, commit, or condition that narrows it, if known]]

## Mockup

What this looks like when it is done, drawn as the thing itself. Build what
is here. If it cannot be built as drawn, say so in your report rather than
improvising something near it.

    [[the panels this task has to match, copied from the plan — delete this
    whole heading if the task does not change anything a person opens]]

## Non-goals

Out of scope. The fix should be no larger than the bug.

- refactoring around the fix
- anything not required to make the repro pass

## Acceptance criteria

- the repro written by the reproduce step fails before the fix and passes after it
- [[anything else that must stay true — the regression the fix must not cause]]

## References

Read these before you start. They describe the system as it is; work from them
rather than restating them — a path here replaces a paragraph above, and stays
true after the code moves.

- [[path — why it matters]]
