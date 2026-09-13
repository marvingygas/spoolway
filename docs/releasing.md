# Releasing spoolway

This is the repository's release runbook. The saved task in
`.spoolway/routines/release-spoolway/release-spoolway.md` selects the `release` pipeline:
preflight gathers evidence, notes drafts the release record for human approval, and publish
prepares, verifies and ships it. All instructions and references live in this repository.

## The two commits

Record the full SHA of the clean `main` commit reviewed by preflight. Human approval covers
that source commit, the chosen version and the exact changelog section. The publisher then
makes one release commit containing only the approved changes to `Cargo.toml`, `Cargo.lock`
and `CHANGELOG.md`. Record its full SHA separately. That release commit is what the rehearsal
must verify and the `v<version>` tag must identify. New product changes require new preflight
and approval; they cannot be included under the old approval.

Daily CI is useful evidence about its own `headSha`, but cannot approve a later release
commit. Both the release rehearsal and tag workflow call `.github/workflows/verify.yml`
with the `nightly` tier and tests enabled. This runs Linux formatting, Clippy, all Cargo test
targets, the release build, end-to-end tests, the pipeline contract, dependency advisories and
real Windows tests. It never uses daily CI's decision to skip an unchanged commit. Every
checkout in verification and release uses the caller's immutable SHA.

## Preflight and approval

Work from the source checkout named by the task, not its disposable worktree. Confirm the
branch is `main`, the tree is clean, and `git pull --ff-only origin main` succeeds. Check the
queue and open pull requests: no other task may still land work during this release. The
release task's own dispatcher is expected and does not need to be stopped.

Run the local gate in order and record the results:

```sh
cargo fmt --check
cargo deny check advisories
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo check --target x86_64-pc-windows-gnu --all-targets --locked
cargo build --release --locked
./target/release/spoolway pipeline check
```

Inspect recent CI runs with `gh run list --workflow ci.yml`. Record each relevant run's id,
`headSha`, conclusion and actual job conclusions. A green daily run whose test jobs were
skipped is not a fresh full test run. Missing or older hosted evidence does not replace or
excuse the local gate; the release workflow will verify the new release commit independently.
A known unresolved failure on the candidate must be investigated before proceeding.

Read every commit and merged pull request since the latest reachable `v*` tag. Credit authors
and co-authors; archived tasks are supplementary evidence only. For major zero, bump minor for
changed user-facing shape (new behavior, changed defaults, renamed or removed interfaces),
and patch for fixes or internal changes alone. Confirm manifest and lock versions agree.

Draft one section according to the contract at the top of `CHANGELOG.md`, in the task's scratch
`release-notes.md`. Have the person approve its full text, version and source SHA. Do not edit
older sections. The approved section becomes the committed changelog, embedded notes and GitHub
release body; the publisher never writes a separate release body.

## Prepare the release commit

Fetch again and verify clean local and remote `main` still identify the approved source SHA.
Insert the approved section byte-for-byte and change only the package version in `Cargo.toml`.
Run `cargo check --offline` to refresh the root package's lock entry. Inspect `Cargo.lock` and
refuse unrelated dependency changes. Never edit generated npm versions by hand.

Run `cargo test --locked release_notes::tests` and confirm the intended tests actually ran.
Build with `cargo build --release --locked`, then run `./target/release/spoolway whats-new`
without `--since` and compare the new version and section with the approved record. A mismatch
blocks the release. Commit only the three release files with subject `chore(release): v<version>`
and push `main`. Record `git rev-parse HEAD` as the release SHA and prove its parent is the
approved source SHA and its diff contains only those approved release edits.

## Rehearse and tag

Dispatch the release workflow on `main`, with publication disabled:

```sh
gh workflow run release.yml --ref main -f publish=false
```

Find the newly dispatched run, record its id, and inspect it:

```sh
gh run list --workflow release.yml --event workflow_dispatch --branch main --commit <release-sha>
gh run view <run-id> --json event,headSha,status,conclusion,jobs
gh run watch <run-id>
```

Check `headSha` against the recorded release SHA before accepting any result. A branch name
is not proof of which commit ran. After waiting, inspect the actual job conclusions again:
`verify / test`, `verify / audit`, `verify / test-windows`, `matrix`, `notes`, all six `build`
jobs, and `publish` (which only packs in this mode) must succeed. Missing, failed, cancelled
or skipped required jobs are not a pass. The `assets` job is intentionally skipped in a
rehearsal; neither npm publication nor GitHub release creation is permitted with `publish=false`.
Confirm the extracted release-body artifact matches the approved notes, all seven packages
were packed, and package assembly reports the expected repository, hashes and sizes.

Fetch once more. Local `main`, `origin/main` and the rehearsal's `headSha` must all equal the
recorded release SHA, and the tree must be clean. If main moved, use the cleanup below and
return to preflight. Otherwise create the tag with the SHA explicitly supplied:

```sh
git tag v<version> <release-sha>
git push origin refs/tags/v<version>
```

The human approval authorizes this tag only after these checks pass. A tag push runs the full
verification again before platform builds and publication; it does not trust a previous run
or a mutable branch. The npm and GitHub publishing jobs depend on the verification result.

## Verify publication

Record the tag workflow run id and prove its `headSha` and the tag's commit equal the release
SHA. Inspect every required job conclusion, including `assets`. Verify all six platform package
names from `npm/targets.json` plus the `spoolway` wrapper report the approved version through
`npm view <package>@<version> version`. Confirm the GitHub release contains six platform archives
and `SHA256SUMS`.

Read the release body using `gh release view v<version> --json body --jq .body` and compare it
with the approved scratch file using a real diff. Normalize CRLF to LF on both sides; ignore
no other differences. In a fresh directory made with `mktemp -d`, install `spoolway@<version>`,
run `./node_modules/.bin/spoolway --version`, and verify the wrapper selected exactly one
platform package. Remove only that recorded temporary directory afterwards.

Report source SHA, release SHA, version, tag, both workflow run ids and job results, registry
versions, assets, release-body comparison and installation result.

## When a release cannot continue

A failing rehearsal leaves an untagged candidate, not permission to publish. Diagnose the
specific failure. An infrastructure retry may reuse the same commit; a code or workflow fix
changes the release SHA and needs a new rehearsal. Product changes also need renewed approval.
Never accept an older green run after the candidate changes.

If main moves before the release commit is pushed, remove only this attempt's unpushed release
commit and restore the three release files to the new main. Preserve other work and block if
cleanup would overwrite it. After the release commit was pushed but before tagging, revert
that commit through ordinary history on current main; do not force-push. Verify the abandoned
version and section are gone. In both cases invalidate the approved scratch notes and recorded
rehearsal, then return to preflight. A cleanup conflict is a block with the exact state recorded.

After partial publication, inspect the registry before retrying. Published npm versions are
immutable; the workflow skips packages already present and publishes the wrapper after the
platform packages. An unchanged workflow can be retried with `gh run rerun <run-id> --failed`.
A rerun uses its original commit, so it cannot pick up a workflow fix. If a failed partial
release requires moving a tag for such a fix, record the old and new SHAs and reason, rehearse
the corrected commit with publication disabled, and follow the publisher prompt's recovery
rules. Never move a successful release tag, erase assets, or hand-edit the release body.
