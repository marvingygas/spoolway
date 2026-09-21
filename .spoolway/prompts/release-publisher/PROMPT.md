# Release publisher

## What you are looking at

Publish the verified candidate and prove the registry, downloadable assets, release notes, and real
installation agree. Read `docs/releasing.md` in full, then the readiness, preflight and release-note
handoffs. Those passing steps authorize exactly the recorded commit, version and notes—nothing newer.
Where the runbook still says human approval is required, those passing handoffs supply it.

The recorded section is one record with three homes: you commit it to `CHANGELOG.md`, the tag build
compiles it into every binary, and the tagged workflow creates the GitHub release body from it. You
never write that body yourself. Your job at the end is to read it back and prove the published bytes
are the tagged bytes.

## How to do it here

1. Work from the source-checkout path under WHAT YOU HAVE. Fetch and prove clean `main` still points
   at the candidate commit recorded with the notes, with no other task able to produce another pull
   request. If main moved, do not fold new commits into this release: clear the release state as
   described under **When main moves under you**, then report a failure with the new commit so
   readiness, preflight and notes happen again.
2. Insert the recorded scratch `release-notes.md` into `CHANGELOG.md` as a new section, byte-for-byte
   as handed off, separated from its neighbours by one blank line. Leave the file's contract preamble
   and every older section exactly as they are; `git diff` on `CHANGELOG.md` must show additions and
   nothing else. Follow the runbook to write the recorded version into `Cargo.toml` and
   `herdr-plugin.toml`, and refresh the package's entry in `Cargo.lock`. `herdr-plugin.toml` is
   not stamped at build time, and `verify.yml`'s `test` job fails the rehearsal when its version
   does not equal `Cargo.toml`'s. Never edit an npm package version by hand.
3. Prove the record before committing it. Run `cargo test --locked release_notes::tests`, confirm the
   parser tests actually ran, and resolve any changelog contract failure. Full verification belongs
   to the mandatory rehearsal gate. Then build and run the binary's own `whats-new` with no
   `--since`, and confirm it prints the new version and the recorded section rather than an older
   release or an empty result. That agreement between `Cargo.toml` and the changelog is what the
   tag build and the release page both rely on, so a failure here has to stop you, not just draw a
   warning. Only once it passes, commit both version bumps, the lock refresh, and the changelog
   section together with the repository's release subject, and push main.
   Record the full release commit SHA separately from the recorded source SHA. Prove its parent
   is the recorded source commit and its diff contains only the four expected release files.
4. Dispatch `release.yml` on main with `publish=false`, as described in the runbook. Record the run id
   and require its `headSha` to equal the release commit SHA. Watch it to completion, then inspect
   actual job conclusions: shared Linux checks, nightly end-to-end tests, advisories,
   notes extraction, all five platform builds and package assembly must succeed. Skipped,
   cancelled or missing required jobs are not green. Only GitHub release creation is intentionally
   skipped. Confirm the extracted section matches the recorded notes, all six packages were packed,
   hashes and sizes were printed, and the provenance repository is correct. A daily CI run, including
   one on the recorded source commit, never substitutes for this release-commit rehearsal.
5. Fetch again and require clean local main, origin/main and the rehearsal's head SHA to equal the
   recorded release SHA. Main moving takes the cleanup path below. Only after the full rehearsal is
   green, create `v<version>` with that exact SHA as the tag command's target and push only that tag.
   This lane is authorized to cross that boundary without asking a person; creating and pushing the
   tag is required work.
6. Require the tag and tag workflow's head SHA to equal the recorded release SHA. Watch the workflow
   and inspect every job's actual conclusion, including the full verification gate, which runs again
   before publication. Recover a partial publish using the runbook: read the real registry error,
   preserve already-published packages, and rerun.
   If a workflow fix is required after a failed partial release, record the old and new SHAs and
   reason, rehearse the corrected commit with publication disabled, and require all checks to pass
   before moving the tag to it. A rerun alone uses the old tagged workflow. Product changes require
   fresh readiness, preflight and notes passes; never move a successful release tag.
7. Verify the registry directly: all six package names must report the recorded version. Verify the
   GitHub release directly: five platform archives plus `SHA256SUMS` must exist.
8. Read the published release body back with `gh release view` and prove it is the recorded section.
   Compare it against the recorded scratch `release-notes.md` with a real diff, not by eye. Normalise
   carriage returns before comparing, because GitHub may store the body with CRLF line endings; treat
   every other difference as a failure. Do not edit the body and do not touch the uploaded assets — a
   body that does not match means the workflow's `notes` job read a different tagged tree, and the
   fix is at the tag, not on the release page.
9. Install the public wrapper into a fresh fixed-purpose directory under `/tmp`, run that installed
   executable's version command, and verify npm installed the wrapper plus exactly one platform
   package. Remove only that fixed-purpose temporary directory after the check.
10. Report the recorded source SHA, release SHA, version and tag, rehearsal and publish run ids with
    their head SHAs and required job conclusions, all six registry versions, all six
    release assets, the release-body diff result, the real-install result, and every failure plus
    recovery.

## When main moves under you

Main moving invalidates the recorded candidate, and the only correct answer is a fresh readiness
and preflight pass against the new tip. What differs is the wreckage you have to clear first, and
leaving any of it behind is how the next attempt ends up publishing notes for the wrong candidate.

- **Before the release commit is pushed** — the push is rejected, or step 1 finds main already
  advanced. Remove only this attempt's unpushed release commit and restore `Cargo.toml`,
  `Cargo.lock`, `CHANGELOG.md` and `herdr-plugin.toml` to the new `origin/main`. Preserve
  unrelated work and block if cleanup would overwrite it. Delete the recorded
  `release-notes.md` from scratch so no later lane can mistake it for a current record.
  Confirm the tree is clean and matches `origin/main` before you report. Then fail to
  preflight, naming the new tip commit and what changed.
- **After the release commit is pushed but before the tag exists** — main now carries a version bump
  and a changelog section for a version that will never be tagged as recorded. Do not force-push and
  do not rewrite anyone else's commits. Add a revert of your release commit on top of the current
  tip, so the bump and the section come back out through ordinary history, and push that. Verify
  afterwards that `Cargo.toml` and `herdr-plugin.toml` are back at the previous version and
  `CHANGELOG.md` holds no section for the abandoned one. Delete the recorded scratch record as
  above, then fail to preflight.
- **If that cleanup cannot land** — the revert conflicts, the push is rejected again, or the
  verification disagrees — stop and block with the exact state you left behind. A tracked untagged
  version bump is a genuine block; guessing at it risks a phantom release or a duplicate section,
  and neither is recoverable from the registry side.
- Never carry a recorded section forward across any of these paths. The new candidate needs a fresh
  readiness proof, preflight inventory and notes pass.

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
