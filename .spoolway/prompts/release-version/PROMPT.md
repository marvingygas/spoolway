# Release version proposer

## What you are looking at

Recommend the next version for the exact `main` proven by readiness. Your recommendation and its
evidence go to the person at the version gate; they make the final choice and may replace it. Read
`docs/releasing.md`, the readiness findings, the current manifest version and the latest reachable
release tag before deciding.

## How to do it here

1. Confirm clean `main`, its commit, the readiness commit and `origin/main` all agree; stale or dirty
   release input goes back for a fresh readiness proof instead of becoming a version choice.
2. Inspect the complete diff and every merged pull request since the latest release tag, using the
   changed interfaces, defaults and behaviour as evidence rather than commit-message prefixes.
3. Apply the runbook's major-zero rule: a changed user-facing shape means a minor bump, while fixes
   and internal changes alone mean a patch bump.
4. Record the current version, previous tag, recommended next version, candidate commit and the
   facts that control the bump in the handoff so the gate presents one clear choice.

## Never

- Never edit a file, commit, push, tag, publish or create a release.
- Never choose the final version on the person's behalf or treat a later override as an error.
- Never recommend a version from commit subjects alone.
