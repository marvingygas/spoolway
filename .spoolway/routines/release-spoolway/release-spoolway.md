---
id: release-spoolway
title: "chore(release): cut the next spoolway release"
group: release-spoolway
pipeline: release
touches:
  - "Cargo.toml"
  - "Cargo.lock"
  - "CHANGELOG.md"
  - "herdr-plugin.toml"
  - ".github/workflows/release.yml"
---

## Context

- A release is a `v*` tag; the release workflow publishes five platform binaries, six npm packages,
  and a GitHub release with archives and checksums.
- `Cargo.toml` is the only version source; generated npm manifests must never be edited by hand.
- `CHANGELOG.md` is the single release record. One approved section per version is committed beside
  the version bump, compiled into every binary built from the tag by `include_str!`, and used by the
  workflow as the GitHub release body; the contract it must follow is written at the top of the file.
- Daily CI verifies main, but release rehearsal and publication each require the shared full
  verification gate on the exact release commit. An older green run is not release approval.
- `docs/releasing.md` is the repository-local runbook used by every release role.
- The tag is irreversible in ordinary operation and therefore follows human approval of the exact
  candidate version and release notes.

## Goal

Cut a spoolway release — bump the version, commit the approved changelog section, rehearse the build,
tag it, and verify what npm and GitHub actually ended up with. That one section is the release notes
everywhere they appear, and it must make benefits, breaking changes, migration steps, upgrade
commands, provenance, and contributing pull requests clear.

## Non-goals

- unrelated product changes
- hand-editing generated npm package versions
- rewriting an older changelog section, or hand-writing a GitHub release body
- carrying an approved section forward onto a candidate main has moved past
- tagging before the dry-run workflow is fully green
- claiming success from workflow output without checking the registry, assets, notes, and real install
- running `npm deprecate` from a lane: it is an irreversible write to a public registry, needs a
  token, and belongs to a person at release time

## Acceptance criteria

- clean main passes formatting, clippy, and locked tests before release work begins
- the selected version follows the repository's major-zero semver policy and both Cargo files agree
- human-approved release notes are grounded in the complete diff and merged pull requests since the
  previous tag, and archived task files clarify intent when present without ever standing in for that
  proof
- the approved section is committed to `CHANGELOG.md` in the release commit, satisfies the contract at
  the top of that file, adds nothing to older sections, and is confirmed by the locked tests and by
  `spoolway whats-new` before the rehearsal
- the rehearsal's head SHA equals the release commit; Linux checks, nightly end-to-end tests,
  and advisories all pass without the daily skip policy
- the rehearsal validates the approved changelog section, assembles five platforms, dry-runs six
  packages, and reports correct provenance without publishing to npm or GitHub
- the tag names the exact rehearsed release commit, and publication requires the full gate again
- the approved tag publishes six packages and five archives plus `SHA256SUMS`
- the GitHub release body is created by the workflow from the tagged changelog section and reads back
  byte-for-byte identical to the approved notes
- a fresh public npm install selects one platform package and reports the approved version
- the released version's upgrade fixture is on `main` under `scripts/e2e/fixtures/<version>/`,
  scaffolded by that version's own binary at its own tag — the one thing the release owes that
  cannot be made before the tag and is not demanded until the next bump
- every partial failure and recovery is recorded
- once a release stops publishing a platform, a person runs `npm deprecate` against every version
  of that platform's package that is still on the registry, pointing installers at the replacement:

      npm deprecate "@spoolway/win32-x64@<=0.4.0" \
        "Windows support ended after 0.4.x. Run the Linux build under WSL:
         wsl npm install -g spoolway"

  This is a manual registry write outside the release workflow — nothing in CI runs it and nothing
  proves it was made, so it is not part of the rehearsal or publish gates above.

## References

- `docs/releasing.md` — the self-contained release runbook
- `.github/workflows/verify.yml` — shared Linux, end-to-end and advisory verification
- `.github/workflows/release.yml` — the build, pack, publish, and release mechanism
- `CHANGELOG.md` — the release-record contract the committed section must satisfy
- `src/release_notes.rs` — the parser that enforces that contract at test time
- `scripts/build-npm.mjs` — package assembly and version stamping
- `npm/targets.json` — the platform matrix
