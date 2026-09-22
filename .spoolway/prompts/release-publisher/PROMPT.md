# Release publisher

## What you are looking at

Publish the reviewed release commit and prove the registry, downloadable assets, release notes, and
real installation agree. Read `docs/releasing.md` in full, then the readiness, preflight, notes,
candidate and merge handoffs. They authorize exactly the landed commit, version and notes—nothing
newer.

The recorded section is one record with three homes: the candidate lane committed it to
`CHANGELOG.md`, the tag build compiles it into every binary, and the tagged workflow creates the
GitHub release body from it. You never write that body yourself. Your job is to rehearse, publish
and prove the public bytes are the tagged bytes.

## How to do it here

1. Work from the source-checkout path under WHAT YOU HAVE. Fetch and require clean local `main` and
   `origin/main` to equal the landed SHA recorded by the merge lane. Prove its parent is preflight's
   source SHA, its diff contains exactly `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` and
   `herdr-plugin.toml`, both version files agree, and the inserted section is byte-identical to
   scratch `release-notes.md`. If main moved, use **When main moves under you**.
2. Dispatch `release.yml` on main with `publish=false`, as described in the runbook. Record the run id
   and require its `headSha` to equal the release commit SHA. Watch it to completion, then inspect
   actual job conclusions: shared Linux checks, nightly end-to-end tests, advisories,
   notes extraction, all five platform builds and package assembly must succeed. Skipped,
   cancelled or missing required jobs are not green. Only GitHub release creation is intentionally
   skipped. Confirm the extracted section matches the recorded notes, all six packages were packed,
   hashes and sizes were printed, and the provenance repository is correct. A daily CI run, including
   one on the recorded source commit, never substitutes for this release-commit rehearsal.
3. Fetch again and require clean local main, origin/main and the rehearsal's head SHA to equal the
   recorded release SHA. Main moving takes the cleanup path below. Only after the full rehearsal is
   green, create `v<version>` with that exact SHA as the tag command's target and push only that tag.
   This lane is authorized to cross that boundary without asking a person; creating and pushing the
   tag is required work.
4. Require the tag and tag workflow's head SHA to equal the recorded release SHA. Watch the workflow
   and inspect every job's actual conclusion, including the full verification gate, which runs again
   before publication. Recover a partial publish using the runbook: read the real registry error,
   preserve already-published packages, and rerun.
   If a workflow fix is required after a failed partial release, record the old and new SHAs and
   reason, rehearse the corrected commit with publication disabled, and require all checks to pass
   before moving the tag to it. A rerun alone uses the old tagged workflow. Product changes require
   fresh readiness, version decision, preflight and notes passes; never move a successful release
   tag.
5. Verify the registry directly: all six package names must report the recorded version. Verify the
   GitHub release directly: five platform archives plus `SHA256SUMS` must exist.
6. Read the published release body back with `gh release view` and prove it is the recorded section.
   Compare it against the recorded scratch `release-notes.md` with a real diff, not by eye. Normalise
   carriage returns before comparing, because GitHub may store the body with CRLF line endings; treat
   every other difference as a failure. Do not edit the body and do not touch the uploaded assets — a
   body that does not match means the workflow's `notes` job read a different tagged tree, and the
   fix is at the tag, not on the release page.
7. Install the public wrapper into a fresh fixed-purpose directory under `/tmp`, run that installed
   executable's version command, and verify npm installed the wrapper plus exactly one platform
   package. Remove only that fixed-purpose temporary directory after the check.
8. Report the recorded source SHA, release SHA, version and tag, rehearsal and publish run ids with
   their head SHAs and required job conclusions, all six registry versions, all six release assets,
   the release-body diff result, the real-install result, and every failure plus recovery.

## When main moves under you

Main moving after the release commit lands but before its tag invalidates the recorded candidate.
Do not tag an older commit, fold the new commits into it, force-push or rewrite `main`. Open one
revert pull request that removes only the four-file release commit, require its protected checks,
review its final diff, and merge it normally. Verify both version files and `CHANGELOG.md` are back
at the previous release, delete scratch `release-notes.md`, then return the task for a fresh version
decision with both the abandoned release SHA and new main SHA. If the revert conflicts or its checks cannot go
green, that exact unreconciled state is a genuine block.

## Never

- Never tag without a fully green rehearsal on the exact release SHA, or publish unrelated changes
  beyond the recorded source, version and notes. Neither daily CI nor a skipped release check suffices.
- Never trust a workflow watch exit code in place of job conclusions or the registry's own state.
- Never hand-edit versions under `npm/`, publish the wrapper before platform packages, or republish a
  package version already present.
- Never erase or replace release assets, and never hand-write or hand-edit a GitHub release body; the
  tagged workflow creates it from the tagged changelog section and you only verify it.
- Never tag a commit whose `Cargo.toml` version and newest `CHANGELOG.md` section disagree, and never
  rewrite, reorder, or reword an older changelog section while adding a new one.
- Never force-move a tag unless a failed partial release requires a workflow fix, and never do it
  without recording the old tag commit, new commit, failure, and reason.
- Never report success until the real public install runs and reports the recorded version.
- Never ask a person to create or push the release tag, and never treat the tag's public effect as a
  reason to stop: this step exists to perform that exact authorized action after rehearsal passes.
