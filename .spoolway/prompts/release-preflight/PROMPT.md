# Release preflight

## What you are looking at

Prove that the repository is ready for a release and assemble the evidence the release-note writer
needs. Read the release routine named in the task's References in full before acting. You are the
bounded, read-heavy pass: inspect and verify, but do not choose the final version or publish anything.

## How to do it here

1. Work from the source-checkout path given under WHAT YOU HAVE, not the disposable task worktree.
   Confirm it is clean `main`, fast-forwarded to `origin/main`, and that no other queued work can
   still produce a pull request. This release task explains why its own dispatcher is running.
2. Run the release routine's complete local gate in the prescribed order. A failing formatter,
   clippy run, or locked test run is a block, not evidence to release anyway.
3. Read the version in `Cargo.toml`, the latest reachable `v*` tag, and the exact commit at the tip
   of main. Confirm `Cargo.lock` agrees with the manifest before making any recommendation.
4. Inspect every commit and merged pull request since the last tag. Separate user-visible behaviour,
   breaking interfaces, changed defaults, renamed keys or flags, fixes, platform work, packaging,
   documentation, and internal-only changes. Use diffs and current help/contracts as evidence; do
   not infer impact from commit subjects alone.
5. Recommend a semver bump using the source routine's rule for major zero: changed user-facing shape
   means minor, while fixes and internals alone mean patch. Name each fact that controls the choice.
6. Check that the release workflow still describes six platform binaries, seven npm packages, the
   dry-run rehearsal, tag/version agreement, provenance, and creation of the GitHub release. Report
   drift instead of silently editing it.
7. Hand off the main commit, current version, previous tag, proposed version and reason, categorized
   changes with pull-request numbers, breaking changes and migrations, contributor credits, and all
   verification results. If this is a return from publishing because main moved, say exactly what
   changed since the previously approved candidate.

## Never

- Never edit a version, commit, push, tag, dispatch a workflow, publish a package, or create a release.
- Never treat this task's own running dispatcher as unreleased work; only other tasks count.
- Never recommend a version from commit-message prefixes alone.
- Never hide a dirty tree, open pull request, failed check, workflow drift, or manifest/lock mismatch.
