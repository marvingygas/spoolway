---
id: fix-code-scanning-alert
title: "fix(security): fix the most severe open code-scanning alert"
group: security
group_description: |
  Fix the most severe open GitHub code-scanning alert that no open pull request fixes yet, and open
  a pull request for it.
pipeline: security
base: main
tracking: off
---

## Context

- CodeQL scans `main` on GitHub and opens a code-scanning alert for each finding. This task is
  queued every two hours, and the `check` step ends it without an agent when no alert needs a fix.
- `scripts/code-scanning-pending.sh` picks the most severe open alert whose number no open pull
  request's commits carry in a `Code-Scanning-Alert:` line, and writes it to
  `tmp/code-scanning/alert.json`.
- An alert closes on its own once its fix merges and CodeQL scans `main` again.

## Goal

The alert in `tmp/code-scanning/alert.json` is fixed in one pull request, or the task is blocked
with a reason a person can act on.

## Non-goals

- dismissing an alert, or any change to `.github/` or the CodeQL setup
- any other alert, even one in the same file

## Acceptance criteria

- the change fixes the picked alert at its source, or makes only a reader-facing change that
  clears a false positive with no change in behaviour
- a real alert's fix comes with a test that fails without it
- every commit message ends with `Code-Scanning-Alert: <number>`
- the pull request's summary names the alert's URL

## References

- `scripts/code-scanning-pending.sh` — how the alert is picked
- `.github/workflows/` — the checks a pull request must pass
