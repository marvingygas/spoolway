# Release-candidate builder

## What you are looking at

Turn the exact candidate and release-note record handed to you into one release pull request: four
files, plus `docs/migrations.md` when notes wrote a scratch `migrations.md`, and
`assets/model-prices.json` when the `prices` step refreshed it. You own the
version bump, changelog and migration-guide insertion, focused proof and pull request. The next
lane independently reviews it, a person merges it on GitHub, and publication happens only after
that merge.

Read `docs/releasing.md`, the readiness and preflight evidence, and the complete release-note
handoff. Those passing steps authorize exactly the recorded source commit, version and notes.

## How to do it here

1. Work from the source checkout path under WHAT YOU HAVE. Fetch and require clean `main` equal to
   the candidate recorded by preflight and notes. If `origin/main` moved, leave it untouched, remove
   only this attempt's unpushed work and stale scratch `release-notes.md` and `migrations.md`, and
   send the task back for a fresh version decision and preflight.
2. Use one branch named `release/<run>` from that candidate, where `<run>` is the output of `scripts/release-run-name.sh` run from this task's own worktree (your starting directory), never the source checkout, which has no `.release-run/`.
   `scripts/release-publish.sh` finds the release commit by its subject rather than its branch, but
   `scripts/release-await-merge.sh` waits on this exact prefix, so a differently named branch leaves
   nothing for it to find. Reuse its open pull request after proving it belongs to this attempt;
   never open a second pull request for the same release.
3. Insert scratch `release-notes.md` into `CHANGELOG.md` byte-for-byte as the newest section. Change
   the version in `Cargo.toml` and `herdr-plugin.toml`, then refresh only the package entry in
   `Cargo.lock`. When scratch `migrations.md` exists, copy it over `docs/migrations.md`
   byte-for-byte and check its diff only adds. The `prices` step refreshed
   `assets/model-prices.json` in this task's own worktree, not in the source checkout. When
   `cmp` finds that file differs from the source checkout's copy (not `git status`, because the
   dispatcher commits lane work onto the task branch), copy it over the source checkout's
   byte-for-byte. Require the notes section to carry its price-table bullet
   exactly when the file changed. Never edit generated npm manifests or an older changelog section.
4. Run `cargo test --locked release_notes::tests`, confirm the parser tests ran, then build the
   release binary and run its `whats-new` command with no `--since`. Require the output to carry the
   new version and complete recorded section. Diff the inserted section against the scratch file.
5. Commit exactly `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, `herdr-plugin.toml`, plus
   `docs/migrations.md` when notes wrote it and `assets/model-prices.json` when `prices` changed it,
   with subject `chore(release): v<version>`. Prove the commit's
   parent is the recorded source commit and no other path changed. Push it to its release branch
   and create or update one pull request against `main`.
6. Read the pull request back through `gh`. Give its number and URL, source and head SHAs, version,
   changed files, focused-test results, and the notes diff result to the reviewer.

## Never

- Never merge the release pull request, push directly to `main`, tag, publish, or create a GitHub
  release. Independent review is the next lane's job, and only a person merges a release pull
  request.
- Never rebase an old release record onto a moved `main`; preflight and notes must describe the exact
  source commit that becomes the release commit's parent.
- Never carry an open release pull request whose base candidate or scratch notes disagree with this
  attempt. Close the stale pull request, delete only its branch, and return for a fresh version
  decision and preflight.
- Never broaden the release commit beyond the four named files, the migration guide notes wrote,
  and the price table `prices` refreshed.
