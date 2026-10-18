# Create PR

Open the pull request for your task's branch.
Done: the branch is pushed, and one pull request is open for it with a body a reviewer reads in a minute.

## Domain knowledge
- a task file with a `trial:` key is a trial arm: push nothing, open nothing, and pass, because a trial arm never opens a pull request
- your task file names the branch to open against in `starts_from:`; use `base:` when it has none, and also when `git ls-remote --heads origin <starts_from>` prints nothing, because its pull request merged and GitHub deleted the branch; diff against `base:` too then
- the title is the task's `title:`, with a bare `ticket:` key such as `KAN-11` in front of it
- commit anything left uncommitted, then push the commits as they are
- a pull request already open for this branch is the one to update, never a reason to open a second
- the body opens with a `## Summary` heading, then summarises `git diff <that branch>...HEAD` in plain English under it: one bullet per change, what changed and why
- the body ends with `Co-Authored-By: <your own product name>`, such as `Claude Code` or `Codex`

## Screenshots
- between the summary and the co-author line, add a `## Screenshots` section with one image per file under `docs/screenshots/` that `git diff --name-status <that branch>...HEAD -- docs/screenshots` shows as added or modified
- embed each as `![<name>](https://github.com/<owner>/<repo>/blob/<sha>/docs/screenshots/<name>.png?raw=true)`
- `<owner>/<repo>` comes from `gh repo view --json nameWithOwner -q .nameWithOwner`, and `<sha>` from `git rev-parse HEAD` after the push
- a relative path does not render in a pull request body, and a branch name stops working once the branch is deleted, so always use the full commit sha
- leave the section out when no image under `docs/screenshots/` was added or changed

## Never
- squashing, rebasing or force-pushing
- merging the pull request
- a body that retells how the work went
