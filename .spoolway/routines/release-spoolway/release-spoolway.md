---
id: release-spoolway
title: "chore(release): cut the next spoolway release"
group: release-spoolway
pipeline: release
touches:
  - "Cargo.toml"
  - "Cargo.lock"
  - ".github/workflows/release.yml"
---

## Context

- A release is a `v*` tag; the release workflow publishes six platform binaries, seven npm packages,
  and a GitHub release with archives and checksums.
- `Cargo.toml` is the only version source; generated npm manifests must never be edited by hand.
- Nothing runs on the forge before release, so local checks and a non-publishing workflow rehearsal
  are the complete gate before tagging.
- The tag is irreversible in ordinary operation and therefore follows human approval of the exact
  candidate version and release notes.

## Goal

Cut a spoolway release — bump the version, rehearse the build, tag it, and verify what npm and GitHub
actually ended up with. Publish state-of-the-art release notes that make benefits, breaking changes,
migration steps, upgrade commands, provenance, and contributing pull requests clear.

## Non-goals

- unrelated product changes
- hand-editing generated npm package versions
- tagging before the dry-run workflow is fully green
- claiming success from workflow output without checking the registry, assets, notes, and real install

## Acceptance criteria

- clean main passes formatting, clippy, and locked tests before release work begins
- the selected version follows the repository's major-zero semver policy and both Cargo files agree
- human-approved release notes are grounded in the complete diff since the previous tag
- the rehearsal assembles six platforms, dry-runs seven packages, and reports correct provenance
- the approved tag publishes seven packages and six archives plus `SHA256SUMS`
- the GitHub release body exactly uses the approved notes
- a fresh public npm install selects one platform package and reports the approved version
- every partial failure and recovery is recorded

## References

- `/home/marvin/.claude/skills/release-spoolway/SKILL.md` — the source release routine
- `.github/workflows/release.yml` — the build, pack, publish, and release mechanism
- `scripts/build-npm.mjs` — package assembly and version stamping
- `npm/targets.json` — the platform matrix
