---
domain: migrations
covers: ["CHANGELOG.md", "src/update.rs", "src/release_notes.rs", "src/repo.rs", "src/sync.rs"]
---

# Migration guide

Use this page when moving an existing project between spoolway releases. For a fresh project,
install the current release and run `spoolway init`; none of the earlier-version steps apply.

## Version overview

| Upgrade | What changes | What you need to do |
|---|---|---|
| 0.5.x to 0.6.x | Retired pipeline shapes are migrated on update; `loop:` counts arrivals; installed skills are always rewritten; `/spoolway-doctor` is gone. | Open spoolway, apply the update, and read what it migrated. |
| 0.4.x to 0.5.x | `sync` takes over from `update`; `dispatch.interval`, `dispatch.default_pipeline` and the tmux backend are gone; tasks name their own `pipeline:` and `base:`. | Delete the retired keys, set `pipeline:` and `base:` on every task, and apply the update. |
| 0.3.x to 0.4.x | Retired config, template and pipeline-generation features are removed. | Remove the retired entries described below. |
| 0.2.x to 0.3.x | Project state moves from a checkout-name directory to an id-keyed home. | Stop live work, then run any spoolway command and let the automatic move finish. |
| 0.1.x to 0.2.x | Projects must be claimed; commands and state names change. | Run `spoolway init` and update old scripts and config keys. |

Install the target version, then read its embedded notes:

```
npm install -g spoolway@0.6.0
spoolway whats-new --since <your-current-version>
```

The [changelog](https://github.com/marvingygas/spoolway/blob/main/CHANGELOG.md) is the complete
release record. This guide collects only the steps that may require action.

## Windows

The last native Windows release is 0.4.x. `@spoolway/win32-x64` was published up to 0.4.0, and
`npm/targets.json` no longer carries that target, so no release after 0.4.x has a Windows
package. On Windows, install the Linux package under WSL:

```
wsl npm install -g spoolway
```

## 0.5.x to 0.6.x

Open spoolway after upgrading and apply the update. It migrates every pipeline shape this release
retired, and names each change:

- A step whose `on_fail:` named itself loses it. A failure there now waits on `blocked` for a
  person instead of retrying. The shipped pipelines drop their `checks` step entirely.
- Each `loop:` map becomes a bare `loop:` on the step it named, set to the most arrivals 0.5
  allowed there: one, plus every map that named it. `loop:` now counts every arrival, the first
  included. The shipped pipelines use `loop: 2`.
- `on_loop_max:` is deleted. A spent limit always parks on `blocked`.
- Installed skills are rewritten whenever they differ from the shipped copy. Keep a changed skill
  under a name of your own.
- `/spoolway-doctor` is removed. `/spoolway-config` diagnoses and repairs a project.

## 0.4.x to 0.5.x

- `dispatch.interval` is retired: the dispatcher now polls at one fixed rate and there is
  nothing to configure. Delete any `dispatch.interval = ...` line from `.spoolway/config.toml`;
  a config that still sets it loads with a note and drops the key on the next save.
- The tmux backend and `dispatch.tmux_mode` are gone. Change `dispatch.backend` to `herdr` or
  `headless` in `.spoolway/config.toml`, and delete `dispatch.tmux_mode`; a config still naming
  `tmux` loads as `herdr` with a note and rewrites both keys on the next save.
- `dispatch.default_pipeline` is gone. Add `pipeline: <name>` naming one of the project's
  pipelines to every task document that relied on the default; a run refuses to start with one
  missing.
- A task no longer inherits `base:` from the checked-out branch. Set `base: <branch>` on every
  task document that relied on the fallback.
- `[pipeline_gen]` and the rest of pipeline generation are gone, including the fallback that
  answered a missing `.spoolway/pipelines/` directory from the pipelines built into the binary.
  Delete the `[pipeline_gen]` table from `.spoolway/config.toml`; if the directory is missing,
  write the project's pipelines again with the `spoolway-config` skill.
- `update` no longer brings a project's files forward, only the binary. Bring `config.toml`,
  tracked pipeline docs and installed skills forward by opening spoolway and applying the
  update there, or with the `spoolway-config` skill.
- The last release to publish a Windows binary is 0.4.x. See Windows above for installing under
  WSL.

## 0.3.x to 0.4.x

- Delete `unattended.skip_blocked_lane` from `.spoolway/config.toml`. A config that still has
  it loads with a note and drops it the next time spoolway saves the file.
- Delete `.spoolway/templates/task-log.md`; task status wording is now built in.
- Remove `[pipeline_gen]` from the config and replace any `spoolway pipeline gen` call. If the
  project has no pipelines, run `spoolway init` to restore the two shipped samples.

The behavior of a staffed `blocked` step also changed. A pass follows that step's `on_pass`;
a pause, failure or block returns the task to the step that originally blocked after you run
`spoolway resume`.

## 0.2.x to 0.3.x

Before 0.3, a project's home was `~/.spoolway/<checkout-name>/`. It is now
`~/.spoolway/<label>-<id>/`, keyed by an id shared by every branch and worktree of the clone.
The first command after upgrading moves the queue, archive, ledger and dispatched worktrees
automatically and prints the old and new paths.

The move refuses while a dispatcher or worktree process is live. Let the work finish or stop
it, then run the same command again; the old home remains untouched until the move succeeds.

If you renamed the checkout directory before upgrading, spoolway cannot discover the old
home. List `~/.spoolway/`, then bind the checkout to the old home explicitly:

```
spoolway init --adopt <old-home-name>
```

Use `spoolway init --new-id` instead when you want a fresh, empty home and intend to leave the
old state alone.

## 0.1.x to 0.2.x

- Run `spoolway init` once in every existing checkout. Projects already initialized need no
  extra action.
- Replace scripts that call the retired `spoolway handover` or `spoolway adopt` commands.
  Use the optional `spoolway stack` command if you want the old command's GitHub pull-request
  behavior, or use any other delivery process.
- Update scripts that read `spoolway queue list --json`: the state formerly named
  `waiting_on_you` is now `paused`.
- Remove `tear_lanes_on_stop`, `cleanup_on_stop` and pipeline-step `cleanup:` entries. They are
  ignored and removed the next time spoolway saves the affected file.
