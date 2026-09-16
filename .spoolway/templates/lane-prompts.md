# Lane prompts

What spoolway types into a lane's pane, plus `## arrived-by-fail`, the one
paragraph of its system prompt this file also covers. One `##` section per
message; a section left out falls back to spoolway's own words.

## opening

{skills}
Read {task_file} before anything else.

## resume

This lane was blocked and a person has cleared it. Same session: do not
start over. The last `## Status Log` entry in {task_file} is what they did.
Decide whether it clears what stopped you, then carry on.

## resume-unattended

This lane was blocked and an unblocker lane has since run on this task. Same
session: do not start over. The last `## Status Log` entry in {task_file} is
what it did, and `## Blocker` is what it tried. Decide whether that clears
what stopped you, then carry on. If it does not, `--block` again with what
you found.

## carry

Same session, continued: your next visit to this task, with nobody in
between. What it asked for, or left, has been done. Do not start over, and
do not re-read what you are still holding. What changed is in the `##
Status Log` of {task_file}.

## park

A person stopped this lane's turn with a keypress and has put it back.
Nothing was blocked and nothing changed. Same session: pick up where the
interrupt cut you off.

## reminder

`{step}` ended its turn without reporting. Here is the report contract
again:

{report_contract}

## arrived-by-fail

Fix pass. `{from}` failed this task back to you; its findings are in
{task_file}'s `## Handoff`, credited to `{from}`. Address exactly those. Do
not re-litigate the verdict.
