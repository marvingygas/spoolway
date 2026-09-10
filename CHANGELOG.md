# Changelog

This file is spoolway's durable, offline release record. Release sections use
the following contract so the binary can parse and replay them:

- A section starts with `## X.Y.Z — Theme`, using canonical ASCII digits with
  no prefix, suffix, plus sign, or leading zero in a multi-digit component.
- The first non-empty paragraph is the overview.
- `### Highlights` follows with three to five `- ` bullets.
- `### Breaking changes and migration` follows only when migration work exists;
  every migration is a `- ` bullet and the section must not be empty.
- Other `###` sections may follow and are replayed in their written order.
- The last line is `Release: https://github.com/marvingygas/spoolway/releases/tag/vX.Y.Z`.
- Versions are unique. File order is not significant; commands sort releases by
  version and print ranges oldest first.

The overview, theme, highlights, migrations, and release URL are public copy.
Keep internal task bookkeeping out of them and describe user-visible outcomes.

## 0.1.0 — Deterministic agent pipelines arrive

Spoolway's first release turns multi-agent work into a local-first pipeline whose scheduling, isolation, and handoff rules remain explicit and inspectable.

### Highlights
- A model-free dispatcher moves tasks through declared pipeline steps without spending an LLM call on orchestration.
- Each task runs in its own git worktree, so concurrent agents do not overwrite one another's changes.
- Claude, Codex, and Pi agents can share a pipeline, with tmux and herdr providing interactive lane control.
- Stacked pull requests, unattended runs, repeatable routines, and a built-in spend ledger keep longer workflows reviewable.
- Prebuilt npm packages cover Linux, macOS, and Windows, while source installation remains available through Cargo.

### Packaging
- Publishing can safely resume after a partial npm release by skipping platform packages whose exact version already exists.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.1.0
