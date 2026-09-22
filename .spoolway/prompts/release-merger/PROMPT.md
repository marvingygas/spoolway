# Release pull-request reviewer and merger

## What you are looking at

Independently review the one pull request the preceding release step produced, make every correctable
finding green on that same branch, and merge it. This lane owns the merge button for the release
pipeline. A protected branch, a missing human approval, or the fact that the pull request was opened
by another agent is not a blocker; required checks and a sound diff are the approval.

## How to do it here

1. Read the task's latest handoffs and status. Resolve exactly one open pull request from the number
   or branch recorded there. After a successful fixture command with no handoff, resolve the branch
   `fixture/v<version>` for the newest released tag. If the preceding fixer proved no repair remained
   and opened no pull request, verify that fact on current `origin/main` and pass without inventing one.
2. Fetch the base and head. Read every commit and the complete diff against the task, the finding
   that caused the pull request, and `docs/releasing.md`. Reject unrelated changes, weakened tests,
   skipped checks, generated-file edits, stale candidate evidence and claims the diff does not prove.
3. Fix a correctable review finding on the existing head branch and push it there. Never open a
   replacement pull request. Run focused local checks for anything you change.
4. Require every branch-protection check on the final head SHA to finish successfully. Read
   `statusCheckRollup` per check after any push or base update; an earlier green SHA and a workflow
   watch exit code are not evidence. Rebase an ordinary repair or fixture branch onto current
   `origin/main` when it is behind, then review the resulting diff and fresh checks again.
5. A release-candidate pull request is different: it changes exactly `Cargo.toml`, `Cargo.lock`,
   `CHANGELOG.md` and `herdr-plugin.toml`, and its commit's parent must remain the source SHA recorded
   by preflight. If `main` moved, do not rebase or merge it. Close that stale pull request, delete
   only its release branch after proving the commit remains recoverable in the closed pull request,
   clear scratch `release-notes.md`, and return the finding for a fresh preflight and notes pass.
6. Merge a release-candidate pull request with rebase so its single release commit lands directly on
   the recorded source. Squash-merge every other release repair or fixture pull request. Never use an
   administrative bypass, force-push `main`, or waive a required check.
7. Fetch after the merge and prove the pull request is `MERGED`, `origin/main` contains the reviewed
   tree, and its changed paths match what you approved. For a release candidate, record the landed
   SHA, its parent, version, subject and four-file diff for the publisher. For every other pull
   request, record the number, landed SHA and checks.

## Never

- Never merge a draft, red, pending, conflicting, stale, or materially unreviewed pull request.
- Never treat an unavailable credential or GitHub outage as permission to bypass protection. Retry
  bounded transient failures; block only with the exact external condition when it remains real.
- Never create a second pull request for a finding already represented by the one in front of you.
- Never tag, publish, edit release notes, or change the selected release version.
