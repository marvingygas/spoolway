---
id: nightly-e2e
title: "test(e2e): run the complete nightly routine"
group: e2e-nightly
pipeline: e2e_nightly
touches:
  - "scripts/e2e/**"
  - "src/**"
  - "docs/**"
  - "assets/**"
  - ".spoolway/**"
---

## Context

- GitHub Actions is switched off for this repository.
- The nightly routine is the only place the real Windows tests and watched plan runs happen.
- The live Codex tier must exercise a real local model, and the cloud tier must exercise a real
  Claude transcript.
- The dispatcher running this task must never have its installed binary overwritten.

## Goal

Run this repo's end-to-end coverage — the headless suites, and the plan runs against a real local
model — then report what failed and what is still uncovered.

## Non-goals

- product changes not demonstrated by a failing routine check
- weakening, deleting, or skipping a correct check
- leaving plan fixtures, panes, forges, worktrees, or model processes behind

## Acceptance criteria

- the nightly, cloud, and live-Codex tiers have explicit results
- the Windows GNU target tests actually run and have an explicit result
- every listed plan is run against the worktree binary and a real local model, watched, and cleaned
- the settings map's bare `no case` count is compared with the baseline of 20 from 2026-08-28
- runtime pipeline drift against `assets/pipelines/default.yml` is reported
- every fix, failure, intervention, retained observations file, and uncovered setting is reported
- any repository fixes are handed over as a pull request; a clean run opens no empty pull request

## References

- `/home/marvin/.claude/skills/e2e-nightly-routine/SKILL.md` — the source routine
- `scripts/e2e/run.sh` — the tier runner and settings map
- `scripts/e2e/plans/` — the watched plan definitions
- `scripts/e2e/runtime/end-to-end.yml` — the plan pipeline to compare with the shipped default
