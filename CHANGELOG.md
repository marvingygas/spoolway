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

## 0.2.0 — A run you can leave alone

Spoolway's second release is about what happens when nobody is watching: work arrives on a schedule, a stop leaves every lane standing, and one honest `paused` state replaces the several ways a task used to go quiet.

### Highlights
- Cron jobs run a routine on a schedule, with a screen to write one from and a catch-up pass so a window between two dispatcher passes is not missed.
- Stopping a run no longer takes anything down: lanes, worktrees and branches stay exactly where they are, so a stop is a pause rather than a teardown.
- Everything that holds a task for a person — a gate, a question, a park, a lane that went quiet — now reads as `paused` and resumes with one key.
- `spoolway doctor` checks the project over and refuses to start a run that cannot succeed, naming the missing piece and the command that supplies it, alongside new `config`, `prompt`, `agent`, `group` and `whats-new` commands.
- Model prices come from a layered table refreshed straight from litellm, so spend is priced against current rates rather than whatever shipped.

### Breaking changes and migration
- A project must be claimed by `spoolway init` before any other command will run in it. Earlier versions created the project's state directory silently on first use, which let a checkout on the wrong branch quietly adopt a directory belonging to something else. Run `spoolway init` once in each checkout you use; projects already initialised need nothing.
- `spoolway handover` and `spoolway adopt` are gone. The work they did is carried by `spoolway stack` and the ordinary pipeline steps; remove any script that calls them.
- `spoolway queue list --json` reports `"state": "paused"` where 0.1.0 reported `waiting_on_you`. Scripts matching on the old value need updating; the `next` field still says which kind of hold it is.
- Retired settings are now dropped rather than refused: a config carrying `tear_lanes_on_stop` or `cleanup_on_stop`, or a pipeline step carrying `cleanup:`, loads with a note and is cleaned up on the next save.

### Queueing
- A task document may name its own `base:`, so one checkout can queue work against several long-lived branches. A document that leaves it out is still based on the branch the checkout has out.
- `spoolway queue remove <task>` takes a task out of the queue and carries its document back to pending, refusing anything with a lane, a command step or a worktree still in flight.
- `spoolway queue add --dry-run` validates a batch and prints the project, home directory and base it resolved without writing anything.

### Fixes
- On Windows, a job whose `routine` began with a leading `/` escaped `.spoolway/routines/` and resolved against the drive root instead. Such a path is now refused on every platform, as it already was elsewhere.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.2.0

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
