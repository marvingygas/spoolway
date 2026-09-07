# Nightly watched plan runs

## What you are looking at

You own the third section of the nightly routine named in the task's References: every plan run
against a real local model, watched while its lanes work. Read that source routine in full before
acting. The preflight lane has already run the headless, cloud, live-Codex, Windows, and coverage-map
checks; read its handoffs before starting.

One pass must leave a cold-readable report of what passed, what failed, what remains uncovered,
and any fixes made. If the routine changed the repository, it must also leave a pull request for
those changes.

## How to do it here

1. Read the task status log and handoffs. Inspect any preflight changes and rebuild the release
   executable before using them.
2. List the plans. For each one, read its `<pre>` block and run the source routine's exact scaffold,
   queue, dispatch, observe, resume, and cleanup lifecycle. Use the release binary from this task's
   worktree for the nested dispatcher and put its directory first on `PATH`, so real lanes report
   through the build under test without overwriting the outer dispatcher's installed inode. Poll the
   nested run and watch `observations.md` as it fills.
3. At a plan's stop, use the current CLI's resume mechanism for both kinds of stop. For a real block
   include what cleared it; for a completed gated step resume it without inventing a blocker. Answer
   a lane through the current lane command only when that live lane ended on a question.
4. Always finish a plan with `scripts/e2e/scaffold.sh --plan <name> --clean`. Confirm its
   `observations.md` was retained under `~/.spoolway/e2e-observations/` and read it before moving on.
5. Fix demonstrated defects on the spot. A stale assertion or missed rename can be corrected;
   something needing a design decision becomes a handoff naming the file, reasoning, and approach.
   Rebuild and rerun the affected plan after a fix.
6. Before reporting, summarize every preflight tier and plan, the `no case` count and drift check,
   every intervention, every retained observations file, every changed file, and everything still
   uncovered. If this task branch differs from its cut point, use the current CLI's task-handover
   command to commit, squash, push, and open its pull request. A clean branch opens no empty pull
   request.

## Never

- Never copy or install the binary while the outer dispatcher is running.
- Never use the installed binary as evidence about the worktree build.
- Never leave a plan's checkout, forge, worktrees, panes, or model process behind.
- Never hide an uncovered setting, an intervention, or a failed cleanup from the report.
- Never omit the final outcome required by the lane contract; it comes after any handover.
