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

## 0.3.0

### Highlights
- A project's home is now found by an id stamped into the checkout's own git directory instead of by the checkout's folder name, so renaming or moving a checkout no longer starts an empty queue, and two clones of the same repository each get a home of their own instead of silently sharing one. (#106, #108, #118)
- `spoolway eval` gains `dirs` and `sessions` views showing what work done outside the lanes — a person running `claude` or `pi` by hand in a watched directory — cost, filterable by directory and skill and exportable like every other view; which directories count is set in the project's new `[watch]` config table. (#112, #115, #120)
- An open pause panel's new `s` key schedules the pause instead of carrying it out: the running step is left to finish and the task lands on `paused` by itself the moment it passes, so stopping the queue no longer throws away a turn that is nearly done. (#110)
- Panes now split in the Fibonacci spiral a person expects, and a dispatched task keeps exactly one tab — its workspace's own — instead of leaving empty anchor tabs behind from an earlier run. (#111, #117)

### Breaking changes and migration
- A project's state home under `~/.spoolway/` is now named `<checkout>-<id>` instead of plain `<checkout>`. The first command run after upgrading moves an existing 0.2 home onto its id-keyed path automatically — queue, archive, ledger and dispatched worktrees included — and prints what moved, e.g. `moved ~/.spoolway/api/ -> ~/.spoolway/api-k7f2q9/`; nothing is deleted and the move runs once. The move is refused, safely and repeatably, while a dispatcher is running over that home or a worktree under it is still checked out. If you renamed the checkout's folder before upgrading, spoolway cannot find the old home by itself and binds a fresh, empty one instead — recover the old queue with `spoolway init --adopt <old-name>` (`ls ~/.spoolway/` shows the name it is still filed under). (#106, #108, #118)

### Cost and eval
- Directories named in a project's new `.spoolway/config.toml` `[watch]` table (`dirs = [...]`, or `spoolway config set watch.dirs ~/notes,docs`) have their own `claude`/`pi` sessions banked into `usage.jsonl` alongside dispatched lanes, caught up on every `spoolway eval` and `spoolway spend` read. (#112, #115)
- `spoolway eval`'s new `dirs` and `sessions` tabs show that spend by directory and by session, with `dir` and `skill` filters and the same CSV export (`e`) as every other view. (#120)

### Queueing
- Pausing a task from a running step's panel now offers a third answer, `s`, that waits for the step to finish before parking the task on `paused`, rather than interrupting it mid-turn. (#110)

### Panes
- A tab's panes now split in a Fibonacci spiral — first side by side, then the right pane top and bottom, then the bottom-right side by side again — and stay in that order as panes close, with no repair needed. (#111)
- A dispatched task now keeps exactly one tab, the one its workspace opened with; tabs left standing empty from an earlier run are swept. (#117)
- A command step's pane now closes as soon as its outcome is known — on success when the exit code is read, and on failure once the task leaves that step for good — rather than only when the run happened to succeed. (#113)

### Reliability
- A pane herdr reports as momentarily busy no longer counts against a step's three launch-failure strikes; the task stays on the step and starts on the dispatcher's next pass instead of parking at `blocked` with nothing in the lane to explain why. (#119)
- `spoolway update` relied on a background version cache and could report "no new version" and install nothing on a machine whose cache had not caught up; it now asks npm directly, with a bounded wait so an unreachable registry degrades to the cached answer instead of hanging. (#100, #101)
- A dispatcher started in a linked worktree filed every task's herdr workspace under the repository's main checkout instead of the checkout it was actually dispatched from; workspaces now anchor to the dispatching checkout. (#122)
- On Windows, two spellings of the same temp directory — the 8.3 short form and the verbatim `\\?\` form — compared unequal to each other, so a fresh clone's own checkout could be misread as belonging to a different one; every path spoolway compares or hands to git now resolves through one spelling. (#124)
- `tmux list-sessions` failing with "server exited unexpectedly" — one of three ways tmux reports that its server is gone — was read as a hard error instead of "no server," which could abort a dispatcher pass or a pane heal right after the last spoolway pane closed; all three are now read the same way. (#125)

### Upgrading
- Install or update with `npm install -g spoolway@0.3.0`, or run it without installing via `npx spoolway@0.3.0`.
- The `spoolway` wrapper package selects one of six platform packages at install time: linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, darwin-arm64, darwin-x64 and win32-x64.
- After upgrading, run `spoolway whats-new` to read this record back from the installed binary; expect the one-time home migration described above on your first command in each project.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.3.0

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
