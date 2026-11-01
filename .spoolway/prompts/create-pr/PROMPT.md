# Create PR

Open the pull request for your task's branch.
Done: the branch is pushed, and one pull request is open for it with a body a reviewer reads in a minute.

## Domain knowledge
- a task file with a `trial:` key is a trial arm: push nothing, open nothing, and pass, because a trial arm never opens a pull request
- your task file names the branch to open against in `starts_from:`; use `base:` when it has none, and also when `git ls-remote --heads origin <starts_from>` prints nothing, because its pull request merged and GitHub deleted the branch; diff against `base:` too then
- the title is the task's `title:`, with the task's own issue in front of it, read from its `ticket:`
- a bare `ticket:` key goes in front as it is, such as `KAN-11`
- a GitHub `ticket:` URL goes in front as `#` and the number it ends in, such as `#1410` for `.../issues/1410`
- a GitHub `ticket:` in a different repo than the pull request's goes in front as `owner/repo#N`, such as `acme/tracker#88`; the pull request's repo comes from `gh repo view --json nameWithOwner -q .nameWithOwner`
- a task with no `ticket:` keeps its `title:` alone
- commit anything left uncommitted, then push the commits as they are
- a pull request already open for this branch is the one to update, never a reason to open a second
- when the open pull request's title does not start with the task's issue, edit it with `t=$(gh pr view <branch> --json title -q .title) && gh pr edit <branch> --title "<issue> $t"`, which puts the issue in front of the title as it is, never lets the shell expand backticks or `$` inside it, and skips the edit when reading the title fails; a title that already starts with the issue, and a task with no `ticket:`, get no edit
- the body opens with a `## Summary` heading, then summarises `git diff <that branch>...HEAD` in plain English under it: one bullet per change, what changed and why
- the body ends with `Co-Authored-By: <your own product name>`, such as `Claude Code` or `Codex`

## Screenshots
- between the summary and the co-author line, add a `## Screenshots` section with one image per file under `docs/screenshots/test/` that `git diff --name-status <that branch>...HEAD -- docs/screenshots/test` shows as added or modified
- embed each as `![<name>](https://github.com/<owner>/<repo>/blob/<sha>/docs/screenshots/test/<name>.png?raw=true)`
- `<owner>/<repo>` comes from `gh repo view --json nameWithOwner -q .nameWithOwner`, and `<sha>` from `git rev-parse HEAD` after the push
- a relative path does not render in a pull request body, and a branch name stops working once the branch is deleted, so always use the full commit sha
- never embed an image from `docs/screenshots/readme/`, even when the branch changed one
- leave the section out when no image under `docs/screenshots/test/` was added or changed

## Never
- squashing, rebasing or force-pushing
- merging the pull request
- a body that retells how the work went
