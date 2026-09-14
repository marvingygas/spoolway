# Lane prompts

What spoolway types into a lane's pane, plus `## arrived-by-fail`, the one
paragraph of its system prompt this file also covers. One `##` section per
message; a section left out falls back to spoolway's own words.

## opening

{skills}
Read {task_file} before anything else.

## resume

You blocked, and a person has unblocked you. Same session, continued: do not
start over, and do not re-read what you are still holding. The last `##
Status Log` entry in {task_file} is what they did — decide whether it clears
what stopped you and carry on. If your work was already done, say so and
`--pass`. If the same thing is still in the way, `--block` again and say so
rather than working around it.

## resume-unattended

You blocked, and nobody is coming — this run is unattended, so it has sent
you straight back. Same session, continued: do not start over, and do not
re-read what you are still holding. Nothing changed while you waited. What
stopped you is the last `## Blocker` entry in {task_file}, exactly as you
left it, and clearing it is yours:

1. Reproduce it. Believe what you see over what you remember.
2. Clear the smallest thing in the way.
3. Run what failed and see it not fail. Then carry on with your step.

Write down what you tried under `## Blocker` before you finish, whatever the
outcome. If your work was already done, say so and `--pass`. If what is in
the way needs a decision that is not yours — spending money, changing what
the code is supposed to do, touching something outside this task — `--block`
again with what you found and what you tried.

## carry

Same session, continued: your next visit to this task, with nobody in
between. What it asked for, or left, has been done. Do not start over, and
do not re-read what you are still holding. What changed is in the `##
Status Log` of {task_file}.

## park

A person stopped your turn with a keypress — not anything you reported — and
has put you back. Nothing was blocked and nothing changed: no work of yours
was undone, nothing was added to the task, nothing new is in your way. Same
session, continued: pick up where the interrupt cut you off.

## reminder

`{step}` ended its turn without reporting. Here is the report contract
again:

{report_contract}

## arrived-by-fail

Fix pass. `{from}` failed this task back to you; its findings are in
{task_file}'s `## Handoff`, credited to `{from}`. Address exactly those. Do
not re-litigate the verdict.
