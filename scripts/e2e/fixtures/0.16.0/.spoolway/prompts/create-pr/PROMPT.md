# Create PR

Open the pull request for your task's branch.
Done: the branch is pushed, and one pull request is open for it with a body a reviewer reads in a minute.

## Domain knowledge
- your task file names the branch to open against in `starts_from:`; use `base:` when it has none
- the title is the task's `title:`, with a bare `ticket:` key such as `KAN-11` in front of it
- commit anything left uncommitted, then push the commits as they are
- a pull request already open for this branch is the one to update, never a reason to open a second
- the body summarises `git diff <that branch>...HEAD` in plain English: one bullet per change, what changed and why
- when that branch is `base:` and `origin/<that branch>` exists, diff against `origin/<that branch>` instead: the task was cut from it, and a local copy that is behind it shows other people's merged commits as yours
- the body ends with `Co-Authored-By: <your own product name>`, such as `Claude Code` or `Codex`

## Never
- squashing, rebasing or force-pushing
- merging the pull request
- a body that retells how the work went
