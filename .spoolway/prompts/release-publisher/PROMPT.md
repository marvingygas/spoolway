# Release publisher

## What you are looking at

Publish the human-approved candidate and prove the registry, downloadable assets, release notes, and
real installation agree. Read the release routine named in the task's References in full, then read
the preflight and release-note handoffs. The approval before this lane is authority for exactly the
recorded commit, version, and notes—nothing newer.

## How to do it here

1. Work from the source-checkout path under WHAT YOU HAVE. Fetch and prove clean `main` still points
   at the candidate commit approved with the notes, with no other task able to produce another pull
   request. If main moved, do not fold new commits into this release: report a failure with the new
   commit so preflight and human approval happen again.
2. Follow the source routine to write the approved version only in `Cargo.toml`, refresh the package's
   entry in `Cargo.lock`, commit with the repository's release subject, and push main. Never edit an
   npm package version by hand.
3. Run the non-publishing workflow rehearsal and watch it to completion. Inspect conclusions rather
   than trusting the watcher's exit code. Prove all six platforms were assembled, all seven dry-run
   packages were packed, hashes and sizes were printed, and the provenance repository is correct.
4. Only after the rehearsal is green, create and push the approved `v<version>` tag. This is the
   irreversible boundary already approved by the human gate.
5. Watch the tag workflow and inspect every job's actual conclusion. Recover a partial publish using
   the source routine: read the real registry error, preserve already-published packages, and rerun.
   If a workflow fix is required, commit it and move the tag to that commit before rerunning; a rerun
   alone uses the old tagged workflow.
6. Verify the registry directly: all seven package names must report the approved version. Verify the
   GitHub release directly: six platform archives plus `SHA256SUMS` must exist.
7. Replace the generated GitHub release body with the approved scratch `release-notes.md`, then read
   the release back and verify its body matches the approved file. Do not alter its uploaded assets.
8. Install the public wrapper into a fresh fixed-purpose directory under `/tmp`, run that installed
   executable's version command, and verify npm installed the wrapper plus exactly one platform
   package. Remove only that fixed-purpose temporary directory after the check.
9. Report the version and tag, rehearsal and publish run ids, all seven registry versions, all seven
   release assets, release-note verification, real-install result, and every failure plus recovery.

## Never

- Never tag before a fully green rehearsal or publish a commit different from the approved candidate.
- Never trust a workflow watch exit code in place of job conclusions or the registry's own state.
- Never hand-edit versions under `npm/`, publish the wrapper before platform packages, or republish a
  package version already present.
- Never erase or replace release assets when applying the approved notes.
- Never force-move a tag unless a failed partial release requires a workflow fix, and never do it
  without recording the old tag commit, new commit, failure, and reason.
- Never report success until the real public install runs and reports the approved version.
