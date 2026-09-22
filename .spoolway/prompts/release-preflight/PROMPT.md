# Release preflight

## What you are looking at

Freeze the exact `main` already proven by the readiness loop and assemble the evidence the
release-note writer needs. Read `docs/releasing.md` in full before acting. You are the bounded,
read-heavy release-record pass: inspect and verify the version chosen at the gate, but do not repair,
reinterpret that choice or publish anything.

## How to do it here

1. Work from the source-checkout path given under WHAT YOU HAVE, not the disposable task worktree.
   Confirm it is clean `main`, fast-forwarded to `origin/main`, matches what readiness verified —
   its locally and host-verified SHA — and has no other queued work able to produce a pull request.
   This release task explains why its own dispatcher is running.
2. Audit readiness's findings: record its local commands, hosted run id, head SHA and actual job
   conclusions, including skipped jobs. Missing, older or mismatched evidence fails back to the
   readiness fixer; it does not prove this candidate. The publisher's new release commit must still
   pass its own full rehearsal.
3. Read the version lane's handoff and the status-log entry that resumed its gate. When the person's
   resume message names a valid `X.Y.Z`, that is the selected version; otherwise use the version
   lane's recommendation. Record which source selected it. Never change its semver class or argue
   for another bump. Read the current version in `Cargo.toml`, the latest reachable `v*` tag, and the
   exact commit at the tip of main. Confirm `Cargo.lock` agrees with the manifest and that the
   selected version does not already have a tag or changelog section. A malformed, existing or
   non-increasing version is a blocker for the person, not a fixer finding or grounds to choose a
   replacement.
4. Inspect every commit and merged pull request since the last tag. Separate user-visible
   behaviour, breaking interfaces, changed defaults, renamed keys or flags, fixes, platform work,
   packaging, documentation, and internal-only changes. Use diffs and current help/contracts as evidence; do not infer impact from commit
   subjects alone.
5. The complete diff and the merged pull requests are the proof of coverage, and they are always
   available. Archived task files may explain why something was done when they happen to still be
   there, so read them when they are; a retention policy that removed them, or a change that crossed
   several task boundaries, never reduces what you owe the notes lane. Never report a gap in coverage
   that is really a gap in archived tasks.
6. Read `CHANGELOG.md` and confirm it still satisfies the contract written at the top of that file.
   The newest section must be the version now in `Cargo.toml`, and no section may already exist for
   the selected version. Report the previous section verbatim so the notes lane can
   match its house style, and report the exact `Release: https://…/releases/tag/vX.Y.Z` line the new
   section will need.
7. Prove the selected version itself before the notes step, not after it. Readiness tested `main`, where
   `Cargo.toml` still holds the *old* version, so nothing it ran has seen the number you just
   selected. In a scratch copy of the checkout, write that version into `Cargo.toml` and
   `Cargo.lock`, run the runbook's local gate and `scripts/e2e/run.sh --suite upgrade`, and then
   throw the copy away. Anything that only goes red once the version moves — a test coupled to
   the manifest, a suite demanding `scripts/e2e/fixtures/<version>/` — is a `fail` for the fixer
   now. Reaching the candidate step before it is found costs a full lap and another pull request; three of the
   four releases before this one paid it. Report the commands, the exit codes and the version you
   ran them at. Never leave the bump behind in the real checkout.
8. Check that the release workflow still describes six platform binaries, seven npm packages, the
   dry-run rehearsal, tag/version agreement, provenance, and creation of the GitHub release. Confirm
   its `notes` job still extracts the tagged changelog section and that the release is still created
   with `--notes-file` rather than generated prose. Rehearsal must extract the section too.
   Confirm release calls the shared verification workflow with the nightly tier and tests enabled,
   every checkout uses the run's SHA, and both npm publication and GitHub release creation depend
   on successful verification and are disabled in rehearsal. Report drift instead of editing it.
9. Give the main commit, readiness run id, current version, previous tag, selected version and its
   gate source, categorized
   changes with pull-request numbers, breaking changes and migrations, the changelog contract
   findings from step 6, and all local and hosted verification results with
   their SHAs. If this is a return from publishing because main moved, say exactly what changed
   since the previously recorded candidate, and say whether an earlier attempt left an untagged
   version bump or changelog section behind.

## Never

- Never edit a version, commit, push, tag, dispatch a workflow, publish a package, or create a
  release. The throwaway copy step 7 bumps is the one exception, and it is an exception only
  because nothing in it is ever committed, pushed or left on disk.
- Never write, edit, or reorder a `CHANGELOG.md` section; you report on the record, the notes lane
  drafts it, and the publisher commits it.
- Never let a missing or archived task file stand in for the diff and the merged pull requests.
- Never treat this task's own running dispatcher as unreleased work; only other tasks count.
- Never replace, downgrade, upgrade or second-guess the version chosen at the gate.
- Never hide a dirty tree, open pull request, failed check, workflow drift, or manifest/lock mismatch.
- Never block on a repository change the readiness fixer can make; report a failure with the exact
  file, command and evidence it needs.
