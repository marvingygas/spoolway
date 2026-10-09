---
id: review-dependabot-prs
title: "chore(deps): review and merge open Dependabot pull requests"
group: dependabot
group_description: |
  Review every open Dependabot pull request, merge the safe ones behind the required checks, and
  queue a bugfix task for each one that needs code changes.
pipeline: dependabot
base: main
tracking: off
---

## Context

- Dependabot opens pull requests for Cargo crates and GitHub Actions every Monday, and security
  updates on any day. This task is queued every two hours, and the `check` step ends it without
  an agent when no pull request needs review.
- `.github/workflows/dependabot-auto-merge.yml` already turns on auto-merge for low-risk Cargo
  updates. This task covers what it leaves open: GitHub Actions, major bumps, minor bumps of 0.x
  crates, and failing pull requests.
- Branch protection on `main` requires `verify / test` and `verify / audit`, so nothing merges red.
- Third-party actions are pinned to commit SHAs on purpose (b7d33b3).

## Goal

Every open Dependabot pull request is either on its way to merging, held with a reason a person can
read on the pull request, or has a bugfix task queued that makes it pass.

## Non-goals

- changing `.github/dependabot.yml` or any workflow
- fixing a failing update in this task; that is the queued bugfix task's job
- pull requests not opened by Dependabot

## Acceptance criteria

- every open Dependabot pull request with no `dependency-reviewer` comment for its current head
  has exactly one verdict in `tmp/dependabot/verdicts.tsv`, except approvals past the fifth
- every `merge` verdict names the head SHA that was reviewed
- every `fix` verdict has exactly one bugfix task queued or already queued for it
- no more than five `merge` verdicts

## References

- `scripts/dependabot-merge.sh` — the rules every approval is checked against again
- `.github/dependabot.yml` — what Dependabot updates and how it groups them
