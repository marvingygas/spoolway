## What to do

1. **Read before you write.** Open the files the change will touch, and the ones that call
   them. A codebase has already decided how it names things, how it reports errors, where its
   tests live and what it considers a layer. Those decisions are the specification for *how*.
2. **Say what you expect to be true, then check it.** The failures that cost a whole lane are
   assumptions nobody tested — a helper that already exists, a field that is nullable, a call
   site you did not know about, a config key spelled differently in the one place that matters.
   Spend the one command that confirms it: grep the other callers, read the type, run the thing
   once.
3. **Work in the smallest verifiable steps you can.** One coherent change, run what covers it,
   then the next. Size the step by how long you would be willing to spend bisecting it.
4. **Change what the task asks for, and leave the rest.** A refactor you were not asked for, a
   rename sweeping files the task never named, a dependency bump you noticed on the way — each
   is a second change hiding inside the first, and the review pays for it. Something genuinely
   wrong outside your scope stays out of your diff.
5. **Handle what actually goes wrong.** The interesting inputs are the empty one, the absent
   one, the one that arrives twice and the wrong type — not the example in the task. Errors go
   up the way the surrounding module already sends them.
6. **A mockup in the task is the specification, not an illustration.** Build what it draws.
   If it cannot be built as drawn, that is not yours to repair, and what stopped it stays out of
   your diff — not a nearby thing you improvised instead.
7. **Test the behaviour you changed and the callers it affects.** Add or update tests at the
   level this project already uses. Run its own test command scoped to the relevant tests,
   module or target, and confirm the intended tests actually ran: zero matching tests proves
   nothing. Broaden the run when shared interfaces, dependencies, build changes or unexplained
   failures make the impact wider than those tests can establish. A failure you cannot explain
   is not a flake until you have looked at it.
8. **Leave the tree the way the project keeps it.** A formatter or linter configured in the repo
   is part of the build. Run the formatter before you stop and the checks relevant to your
   change. When a downstream command step or required CI check runs the full tests and lint,
   leave routine full validation to it; otherwise run the project's full checks before you stop.
   After a failure, diagnose it, fix it, and rerun the affected checks. For changes with no
   executable behaviour, use the relevant format or contract check and explain why no behaviour
   test applies. Record exact commands, results and coverage limits, distinguishing checks you
   ran from checks still owed by a downstream gate.
9. **Know what "done" means before you call it done.** Re-read the acceptance criteria against
   what you built, one at a time. A criterion you cannot point at a line for is not met.
