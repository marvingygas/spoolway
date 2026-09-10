---
id: release-spoolway
title: "chore(release): cut the next spoolway release"
group: release-spoolway
pipeline: release
touches:
  - "Cargo.toml"
  - "Cargo.lock"
  - "CHANGELOG.md"
  - ".github/workflows/release.yml"
---

## Context

- A release is a `v*` tag; the release workflow publishes six platform binaries, seven npm packages,
  and a GitHub release with archives and checksums.
- `Cargo.toml` is the only version source; generated npm manifests must never be edited by hand.
- `CHANGELOG.md` is the single release record. One approved section per version is committed beside
  the version bump, compiled into every binary built from the tag by `include_str!`, and used by the
  workflow as the GitHub release body; the contract it must follow is written at the top of the file.
- Nothing runs on the forge before release, so local checks and a non-publishing workflow rehearsal
  are the complete gate before tagging.
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

## Acceptance criteria

- clean main passes formatting, clippy, and locked tests before release work begins
- the selected version follows the repository's major-zero semver policy and both Cargo files agree
- human-approved release notes are grounded in the complete diff and merged pull requests since the
  previous tag, and archived task files clarify intent when present without ever standing in for that
  proof
- the approved section is committed to `CHANGELOG.md` in the release commit, satisfies the contract at
  the top of that file, adds nothing to older sections, and is confirmed by the locked tests and by
  `spoolway whats-new` before the rehearsal
- the rehearsal assembles six platforms, dry-runs seven packages, and reports correct provenance
- the approved tag publishes seven packages and six archives plus `SHA256SUMS`
- the GitHub release body is created by the workflow from the tagged changelog section and reads back
  byte-for-byte identical to the approved notes
- a fresh public npm install selects one platform package and reports the approved version
- every partial failure and recovery is recorded

## References

- `/home/marvin/.claude/skills/release-spoolway/SKILL.md` — the source release routine
- `.github/workflows/release.yml` — the build, pack, publish, and release mechanism
- `CHANGELOG.md` — the release-record contract the committed section must satisfy
- `src/release_notes.rs` — the parser that enforces that contract at test time
- `scripts/build-npm.mjs` — package assembly and version stamping
- `npm/targets.json` — the platform matrix
