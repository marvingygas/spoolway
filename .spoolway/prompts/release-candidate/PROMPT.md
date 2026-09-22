# Release-candidate builder

## What you are looking at

Turn the exact candidate and release-note record handed to you into one four-file release pull
request. You own the version bump, changelog insertion, focused proof and pull request. The next
lane independently reviews and merges it; publication happens only after that merge.

Read `docs/releasing.md`, the readiness and preflight evidence, and the complete release-note
handoff. Those passing steps authorize exactly the recorded source commit, version and notes.

## How to do it here

1. Work from the source checkout path under WHAT YOU HAVE. Fetch and require clean `main` equal to
   the candidate recorded by preflight and notes. If `origin/main` moved, leave it untouched, remove
   only this attempt's unpushed work and stale scratch `release-notes.md`, and send the task back for
   a fresh version decision and preflight.
2. Use one branch named `release/v<version>` from that candidate. Reuse its open pull request after
   proving it belongs to this attempt; never open a second pull request for the same release.
3. Insert scratch `release-notes.md` into `CHANGELOG.md` byte-for-byte as the newest section. Change
   the version in `Cargo.toml` and `herdr-plugin.toml`, then refresh only the package entry in
   `Cargo.lock`. Never edit generated npm manifests or an older changelog section.
4. Run `cargo test --locked release_notes::tests`, confirm the parser tests ran, then build the
   release binary and run its `whats-new` command with no `--since`. Require the output to carry the
   new version and complete recorded section. Diff the inserted section against the scratch file.
5. Commit exactly `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md` and `herdr-plugin.toml` with subject
   `chore(release): v<version>`. Prove the commit's parent is the recorded source commit and no other
   path changed. Push it to its release branch and create or update one pull request against `main`.
6. Read the pull request back through `gh`. Give its number and URL, source and head SHAs, version,
   four changed files, focused-test results, and the notes diff result to the landing lane.

## Never

- Never merge the release pull request, push directly to `main`, tag, publish, or create a GitHub
  release. Independent review and merging belong to the next lane.
- Never rebase an old release record onto a moved `main`; preflight and notes must describe the exact
  source commit that becomes the release commit's parent.
- Never carry an open release pull request whose base candidate or scratch notes disagree with this
  attempt. Close the stale pull request, delete only its branch, and return for a fresh version
  decision and preflight.
- Never broaden the release commit beyond the four named files.
