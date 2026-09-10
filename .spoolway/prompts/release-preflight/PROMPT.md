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
4. Inspect every commit and merged pull request since the last tag, and name every contributor who
   authored or co-authored one. Separate user-visible behaviour, breaking interfaces, changed
   defaults, renamed keys or flags, fixes, platform work, packaging, documentation, and internal-only
   changes. Use diffs and current help/contracts as evidence; do not infer impact from commit
   subjects alone.
5. The complete diff and the merged pull requests are the proof of coverage, and they are always
   available. Archived task files may explain why something was done when they happen to still be
   there, so read them when they are; a retention policy that removed them, or a change that crossed
   several task boundaries, never reduces what you owe the notes lane. Never report a gap in coverage
   that is really a gap in archived tasks.
6. Read `CHANGELOG.md` and confirm it still satisfies the contract written at the top of that file.
   The newest section must be the version now in `Cargo.toml`, and no section may already exist for
   the version you are about to recommend. Report the previous section verbatim so the notes lane can
   match its house style, and report the exact `Release: https://…/releases/tag/vX.Y.Z` line the new
   section will need.
7. Recommend a semver bump using the source routine's rule for major zero: changed user-facing shape
   means minor, while fixes and internals alone mean patch. Name each fact that controls the choice.
8. Check that the release workflow still describes six platform binaries, seven npm packages, the
   dry-run rehearsal, tag/version agreement, provenance, and creation of the GitHub release. Confirm
   its `notes` job still extracts the tagged changelog section and that the release is still created
   with `--notes-file` rather than generated prose. Report drift instead of silently editing it.
9. Hand off the main commit, current version, previous tag, proposed version and reason, categorized
   changes with pull-request numbers, breaking changes and migrations, contributor credits, the
   changelog contract findings from step 6, and all verification results. If this is a return from
   publishing because main moved, say exactly what changed since the previously approved candidate,
   and say whether an earlier attempt left an untagged version bump or changelog section behind.

## Never

- Never edit a version, commit, push, tag, dispatch a workflow, publish a package, or create a release.
- Never write, edit, or reorder a `CHANGELOG.md` section; you report on the record, the notes lane
  drafts it, and the publisher commits it.
- Never let a missing or archived task file stand in for the diff and the merged pull requests.
- Never treat this task's own running dispatcher as unreleased work; only other tasks count.
- Never recommend a version from commit-message prefixes alone.
- Never hide a dirty tree, open pull request, failed check, workflow drift, or manifest/lock mismatch.
