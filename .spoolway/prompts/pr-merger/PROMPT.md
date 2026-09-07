# Pull-request merger

## What you are looking at

Merge every open pull request into `main` locally. Resolve conflicts directly, preserve both accepted
behaviours, verify the merged tree, push it, and prune the branches. Nothing runs on the forge for
this repository, so your local verification is the whole mechanical verdict.

The survey handoff is a starting point, not authority: remote state may have moved. A conflict is
normally yours to resolve. Block only when the resolution would drop behaviour one of the tasks was
accepted for.

## How to do it here

1. Read the merge routine named in the task's References in full, then read the survey handoffs and
   work from the source-checkout path given under WHAT YOU HAVE. Refresh the pull-request list, queue
   state, and remote refs before acting. This routine's own task explains why the dispatcher is
   running. Wait only for other tasks that can still produce pull requests, then refresh again.
2. Update main before any merge:

       git checkout main
       git merge --ff-only origin/main

3. Recompute stack tips from `baseRefName`. Merge each tip once; it carries every pull request below
   it. Merge standalone branches one at a time. Use `--no-ff` and this repository's message shape:
   `Merge PR #161: board-key-map`, or `Merge PRs #158, #161: cursor-in-gap + board-key-map`.
4. Resolve conflicts by preserving both intents. For Rust restructured on both sides, keep main's
   surviving structure and reapply the branch's behaviour inside it; reject any hunk naming a binding
   with no definition. For doc comments keep both facts. For ASCII frames, transform main's frame
   mechanically and then check headings still align with values. Search the entire tree for conflict
   markers before continuing.
5. Even after a clean automatic merge, inspect files both sides touched and prove each side's added
   behaviour survived. A CLEAN label is not evidence against a silent semantic loss.
6. Verify before pushing, in this order:

       cargo fmt --check
       cargo clippy --all-targets -- -D warnings
       cargo test --all-targets
   Then read the debug build's queue rendering; the installed binary is old evidence.
7. Push `main`, then run `gh pr list --state open`. Anything whose head landed but remains open must
   be explained and cleared. If other tasks produced new pull requests, return to the fresh survey
   and merge loop before declaring the pile empty.
8. Delete merged remote and local branches where safe, accept an already-deleted remote ref as
   success, and finish with `git fetch origin --prune`. Do not delete a branch still used by a
   worktree. Do not clean worktrees owned by herdr, Claude, or another tool.
9. Hand off every merged pull request, every conflict and its resolution, the verification results,
   the pushed main commit, anything still open, and the branches pruned.

## Never

- Never use `gh pr merge`; merging is local, verified, then pushed.
- Never merge a stack bottom separately from its tip.
- Never push before all verification and visual inspection pass.
- Never copy or install the binary, and never refresh its installed control-plane files, while this
  dispatcher is running. Report the final install, control-plane refresh, and doctor phase as a
  required after-dispatch handoff; it cannot be made safe inside the pipeline that keeps the
  dispatcher alive.
- Never queue a fix task during this pass.
- Never drop accepted behaviour merely to settle a conflict.
