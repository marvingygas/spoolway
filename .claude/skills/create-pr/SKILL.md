---
name: create-pr
description: Open one small pull request for work done outside the spoolway dispatcher — put the asked-for changes on a feature branch, commit, push, open the pull request with a bullet-point summary, then switch back to the branch the person started on. Use when someone asks to make, open or create a PR for changes in the working tree.
---

# create-pr

Turn the changes in the working tree into one pull request, and leave the person where they
started.

## Steps

1. **Note where you are.** `git rev-parse --abbrev-ref HEAD` is the branch to return to (a
   detached `HEAD` returns by its commit). The pull request's base is that branch, or `main` when
   that branch is `main` itself.
2. **Pick what goes in.** Only the changes the person asked for. Stage files by path, never with
   `git add -A` or `git add .`: untracked or unrelated work they did not mention stays out of the
   pull request. When it is unclear whether a file belongs, ask.
3. **Branch.** `git switch -c <type>/<short-slug>`, where `<type>` is `feat`, `fix`, `chore`,
   `docs` or `refactor` and the slug names the change in two to four words.
4. **Commit.** One commit per change that stands on its own, subjects in this repository's style:
   `type(scope): what changed`, read off `git log --oneline`. End each message with the commit
   attribution line the session gives you.
5. **Push and open.** `git push -u origin <branch>`, then `gh pr create --base <base> --title
   "<the subject of the main commit>" --body-file <file>`, with the body below.
6. **Switch back.** `git switch <the branch from step 1>`. Work you left out in step 2 comes along
   unchanged. Run `git status` to confirm it.

## The pull request body

```markdown
## Summary

- <One bullet per change: what changed and why, as one or two plain sentences.>
- <Name files, steps and settings in backticks.>

## Checks

- `<command>`: <its result>

Co-Authored-By: Claude Code
```

- The summary is the point of the body. Lead each bullet with the change, not the file. Keep it
  to what a reviewer needs to judge the diff, with no history of how the work went.
- `## Checks` lists only what actually ran, with the real result. Leave the section out when
  nothing ran.
- The body ends with `Co-Authored-By: Claude Code`, the line spoolway's own pull requests carry.

## Report

Give the pull request's URL, its branch, and the branch you are back on. Say plainly that the
committed changes now live on the feature branch until it is merged.

## Never

- Never commit straight to `main`, force-push, or merge the pull request.
- Never stage files the person did not ask for.
