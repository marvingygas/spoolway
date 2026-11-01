# Create PR

Open the pull request for your task's branch.
Done: the branch is pushed, and one pull request is open for it with a body a reviewer reads in a minute.

## Domain knowledge
- your task file names the branch to open against in `starts_from:`; use `base:` when it has none
- the title is the task's `title:`, with the task's own issue in front of it, read from its `ticket:`
- a bare `ticket:` key goes in front as it is, such as `KAN-11`
- a GitHub `ticket:` URL goes in front as `#` and the number it ends in, such as `#1410` for `.../issues/1410`
- a GitHub `ticket:` in a different repo than the pull request's goes in front as `owner/repo#N`, such as `acme/tracker#88`; the pull request's repo comes from `gh repo view --json nameWithOwner -q .nameWithOwner`
- a task with no `ticket:` keeps its `title:` alone
- commit anything left uncommitted, then push the commits as they are
- a pull request already open for this branch is the one to update, never a reason to open a second
- when the open pull request's title does not start with the task's issue, edit it with `gh pr edit <branch> --title "<issue> <its current title>"`, putting the issue in front of the title as it is; a title that already starts with the issue, and a task with no `ticket:`, get no edit
- the body summarises `git diff <that branch>...HEAD` in plain English: one bullet per change, what changed and why
- when that branch is `base:` and `origin/<that branch>` exists, diff against `origin/<that branch>` instead: the task was cut from it, and a local copy that is behind it shows other people's merged commits as yours
- the body ends with `Co-Authored-By: <your own product name>`, such as `Claude Code` or `Codex`

## Never
- squashing, rebasing or force-pushing
- merging the pull request
- a body that retells how the work went
