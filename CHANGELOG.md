# Changelog

This file is spoolway's durable, offline release record. Release sections use
the following contract so the binary can parse and replay them:

- A section starts with `## X.Y.Z`, using canonical ASCII digits with no
  prefix, suffix, plus sign, or leading zero in a multi-digit component. The
  heading carries the version and nothing else.
- `### Highlights` comes first, with three to five `- ` bullets.
- `### Breaking changes and migration` follows only when migration work exists;
  every migration is a `- ` bullet and the section must not be empty.
- Other `###` sections may follow and are replayed in their written order.
- The last line is `Release: https://github.com/marvingygas/spoolway/releases/tag/vX.Y.Z`.
- Versions are unique. File order is not significant; commands sort releases by
  version and print ranges oldest first.

The highlights, migrations, and release URL are public copy. Keep internal task
bookkeeping out of them and describe user-visible outcomes.

## 0.2.0

### Highlights
- Cron jobs run a routine on a schedule, with a `spoolway jobs` screen to write one from and a catch-up pass so a window between two dispatcher passes is never missed. (#17, #20, #86)
- Stopping a run no longer takes anything down: lanes, worktrees and branches stay exactly where they are, so a stop is a pause rather than a teardown. (#68)
- Everything that holds a task for a person — a gate, a question, a park, a lane that went quiet — now reads as `paused` and resumes with one key. (#50, #51, #63, #77)
- `spoolway doctor` refuses to start a run that cannot succeed, naming the missing piece and the command that supplies it, and opens a real pane instead of leaving the check to fail silently later. (#69)
- Model prices come from a layered table refreshed straight from litellm, with `spoolway models refresh` and an age report, so spend is priced against current rates rather than whatever shipped. (#36, #38, #42)

### Breaking changes and migration
- A project must be claimed by `spoolway init` before any other command will run in it. Earlier versions created the project's state directory silently on first use, which let a checkout on the wrong branch quietly adopt a directory belonging to something else. Run `spoolway init` once in each checkout you use; projects already initialised need nothing.
- `spoolway handover` and `spoolway adopt` are gone. The work they did is carried by `spoolway stack` and the ordinary pipeline steps; remove any script that calls the retired verbs.
- `spoolway queue list --json` reports `"state": "paused"` where 0.1.0 reported `waiting_on_you`. Update any script matching on the old value; the `next` field still says which kind of hold it is.
- Retired settings are now dropped rather than refused: a config carrying `tear_lanes_on_stop` (or its older spelling `cleanup_on_stop`), or a pipeline step carrying `cleanup:`, still parses, loads with a note that the key is no longer read, and is removed on the next save.

### Commands
- `spoolway override` reads a patch layer from outside the checkout and applies it to pipelines, prompts and config at dispatch time, stamped and confirmed before it runs; `spoolway override promote` and `drop` manage what's active. (#76, #78, #82)
- `spoolway template` and `spoolway hook` print the pipeline and step conventions that skills previously had to hardcode, so a skill reads them from the binary instead of guessing. (#74)
- `spoolway whats-new` prints the changelog record for the version currently installed, or a chosen range with `--since`. (#37)

### Queueing
- A task document may name its own `base:`, so one checkout can queue work against several long-lived branches. A document that leaves it out is still based on the branch the checkout has out.
- `spoolway queue remove <task>` takes a task out of the queue and carries its document back to pending, refusing anything with a lane, a command step or a worktree still in flight.
- `spoolway queue add --dry-run` validates a batch and prints the project, home directory and base it resolved without writing anything.

### Reliability
- A command step's exit file could be read while the wrapper was still writing it, and empty content was treated as exit code 1, so a step that had actually succeeded could be routed down `on_fail` at random. Reading now waits for a real answer instead of guessing at a half-written file. (#97)
- Deleting a finished run's leftover status files could silently fail on Windows, because a just-exited wrapper's handle can still hold them; a step re-run at the same key could then be routed on the previous attempt's exit code without having run at all. Those files are now emptied when they cannot be deleted, which a stale read can no longer mistake for an answer. (#98)
- On Windows, re-running a command step could also lose the previous run's log to the same lingering-handle problem; the rename that rolls it aside is now retried for up to two seconds before giving up. (#97)
- Stopping an already-finished run could signal the wrong process on Windows, because a recycled process id can belong to something else by the time the stop reaches it. Stopping now checks that the run is still actually running before it signals, on Windows only; Unix's process-group signal is unchanged, since it is still correct to reach a group whose leader has already exited. (#99)

### Fixes
- On Windows, a job whose `routine` began with a leading `/` escaped `.spoolway/routines/` and resolved against the drive root instead. Such a path is now refused on every platform, as it already was elsewhere.

### Upgrading
- Install or update with `npm install -g spoolway@0.2.0`, or run it without installing via `npx spoolway@0.2.0`.
- The `spoolway` wrapper package selects one of six platform packages at install time: linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, darwin-arm64, darwin-x64 and win32-x64.
- After upgrading, run `spoolway whats-new` to read this record back from the installed binary.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.2.0

## 0.1.0

### Highlights
- A model-free dispatcher moves tasks through declared pipeline steps without spending an LLM call on orchestration.
- Each task runs in its own git worktree, so concurrent agents do not overwrite one another's changes.
- Claude, Codex, and Pi agents can share a pipeline, with tmux and herdr providing interactive lane control.
- Stacked pull requests, unattended runs, repeatable routines, and a built-in spend ledger keep longer workflows reviewable.
- Prebuilt npm packages cover Linux, macOS, and Windows, while source installation remains available through Cargo.

### Packaging
- Publishing can safely resume after a partial npm release by skipping platform packages whose exact version already exists.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.1.0
