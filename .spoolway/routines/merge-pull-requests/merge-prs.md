---
id: merge-prs
title: "chore(merge): merge every open pull request"
group: merge-pull-requests
pipeline: merge_prs
touches:
  - "**"
---

## Context

- A dispatch run leaves one pull request per task, sometimes as a stack.
- Pull requests are merged locally into main, never through `gh pr merge`.
- Nothing runs on the forge, so the merged tree must be verified before it is pushed.
- This task's own dispatcher must remain running until the pipeline reports, so reinstalling the
  binary belongs immediately after the dispatcher exits.

## Goal

Merge every open pull request on this repository into main — order the stacks, resolve the conflicts
directly, verify the merged tree, push, prune the branches, and reinstall the binary.

## Non-goals

- queueing new work discovered during the merge
- dropping accepted behaviour to avoid a conflict
- merging through the forge
- deleting worktrees or branches still owned by another tool

## Acceptance criteria

- every open pull request is accounted for and each stack is merged once at its tip
- conflicts and clean overlapping edits preserve both accepted behaviours
- format, clippy, all-target tests, and the debug queue rendering pass before main is pushed
- GitHub shows no pull request left open whose head was merged
- dead branches are pruned safely
- the report names every merged pull request and every conflict resolution
- the report leaves the exact after-dispatch install, update, and doctor commands when that phase
  cannot safely run inside the live dispatcher

## References

- `/home/marvin/.claude/skills/merge-pull-requests/SKILL.md` — the source routine
- `src/commands/stack.rs` — how task pull requests and stacks are handed over
- `docs/tasks.md` — task branches, groups, and worktree ownership
