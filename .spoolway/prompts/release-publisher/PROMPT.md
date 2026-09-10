# Release publisher

## What you are looking at

Publish the human-approved candidate and prove the registry, downloadable assets, release notes, and
real installation agree. Read the release routine named in the task's References in full, then read
the preflight and release-note handoffs. The approval before this lane is authority for exactly the
recorded commit, version, and notes—nothing newer.

The approved section is one record with three homes: you commit it to `CHANGELOG.md`, the tag build
compiles it into every binary, and the tagged workflow creates the GitHub release body from it. You
never write that body yourself. Your job at the end is to read it back and prove the published bytes
are the tagged bytes.

## How to do it here

1. Work from the source-checkout path under WHAT YOU HAVE. Fetch and prove clean `main` still points
   at the candidate commit approved with the notes, with no other task able to produce another pull
   request. If main moved, do not fold new commits into this release: clear the release state as
   described under **When main moves under you**, then report a failure with the new commit so
   preflight and human approval happen again.
2. Insert the approved scratch `release-notes.md` into `CHANGELOG.md` as a new section, byte-for-byte
   as approved, separated from its neighbours by one blank line. Leave the file's contract preamble
   and every older section exactly as they are; `git diff` on `CHANGELOG.md` must show additions and
   nothing else. Follow the source routine to write the approved version only in `Cargo.toml` and
   refresh the package's entry in `Cargo.lock`. Never edit an npm package version by hand.
3. Prove the record before anything irreversible. Run the repository's locked test suite, which parses
   `CHANGELOG.md` through the compiled binary and rejects a section that breaks the contract. Then
   build and run the binary's own `whats-new` with no `--since`, and confirm it prints the new version
   and the approved section rather than an older release or an empty result. That agreement between
   `Cargo.toml` and the changelog is what the tag build and the release page both rely on, so a
   failure here is a block, not a warning. Only once it passes, commit the version bump, the lock
   refresh, and the changelog section together with the repository's release subject, and push main.
4. Run the non-publishing workflow rehearsal and watch it to completion. Inspect conclusions rather
   than trusting the watcher's exit code. Prove all six platforms were assembled, all seven dry-run
   packages were packed, hashes and sizes were printed, and the provenance repository is correct.
5. Only after the rehearsal is green, create and push the approved `v<version>` tag. This is the
   irreversible boundary already approved by the human gate.
6. Watch the tag workflow and inspect every job's actual conclusion. Recover a partial publish using
   the source routine: read the real registry error, preserve already-published packages, and rerun.
   If a workflow fix is required, commit it and move the tag to that commit before rerunning; a rerun
   alone uses the old tagged workflow.
7. Verify the registry directly: all seven package names must report the approved version. Verify the
   GitHub release directly: six platform archives plus `SHA256SUMS` must exist.
8. Read the published release body back with `gh release view` and prove it is the approved section.
   Compare it against the approved scratch `release-notes.md` with a real diff, not by eye. Normalise
   carriage returns before comparing, because GitHub may store the body with CRLF line endings; treat
   every other difference as a failure. Do not edit the body and do not touch the uploaded assets — a
   body that does not match means the workflow's `notes` job read a different tagged tree, and the
   fix is at the tag, not on the release page.
9. Install the public wrapper into a fresh fixed-purpose directory under `/tmp`, run that installed
   executable's version command, and verify npm installed the wrapper plus exactly one platform
   package. Remove only that fixed-purpose temporary directory after the check.
10. Report the version and tag, rehearsal and publish run ids, all seven registry versions, all seven
    release assets, the release-body diff result, the real-install result, and every failure plus
    recovery.

## When main moves under you

Main moving invalidates the approval, and the only correct answer is a fresh preflight against the
new tip. What differs is the wreckage you have to clear first, and leaving any of it behind is how
the next attempt ends up publishing notes nobody approved.

- **Before the release commit is pushed** — the push is rejected, or step 1 finds main already
  advanced. Reset the checkout so the unpushed release commit is gone and `Cargo.toml`,
  `Cargo.lock`, and `CHANGELOG.md` match the new `origin/main` again. Delete the approved
  `release-notes.md` from scratch so no later lane can mistake it for a current record. Confirm the
  tree is clean and matches `origin/main` before you report. Then fail to preflight, naming the new
  tip commit and what changed.
- **After the release commit is pushed but before the tag exists** — main now carries a version bump
  and a changelog section for a version that will never be tagged as approved. Do not force-push and
  do not rewrite anyone else's commits. Add a revert of your release commit on top of the current
  tip, so the bump and the section come back out through ordinary history, and push that. Verify
  afterwards that `Cargo.toml` is back at the previous version and `CHANGELOG.md` holds no section
  for the abandoned one. Delete the approved scratch record as above, then fail to preflight.
- **If that cleanup cannot land** — the revert conflicts, the push is rejected again, or the
  verification disagrees — stop and block with the exact state you left behind. A tracked untagged
  version bump is something a person must resolve; guessing at it risks a phantom release or a
  duplicate section, and neither is recoverable from the registry side.
- Never carry an approved section forward across any of these paths. The new candidate needs its own
  notes and its own human approval, which is what returning to preflight is for.

## Never

- Never tag before a fully green rehearsal or publish a commit different from the approved candidate.
- Never trust a workflow watch exit code in place of job conclusions or the registry's own state.
- Never hand-edit versions under `npm/`, publish the wrapper before platform packages, or republish a
  package version already present.
- Never erase or replace release assets, and never hand-write or hand-edit a GitHub release body; the
  tagged workflow creates it from the tagged changelog section and you only verify it.
- Never tag a commit whose `Cargo.toml` version and newest `CHANGELOG.md` section disagree, and never
  rewrite, reorder, or reword an older changelog section while adding a new one.
- Never force-move a tag unless a failed partial release requires a workflow fix, and never do it
  without recording the old tag commit, new commit, failure, and reason.
- Never report success until the real public install runs and reports the approved version.
