---
domain: migrations
covers: ["CHANGELOG.md", "src/update.rs", "src/release_notes.rs", "src/repo.rs"]
---

# Migration guide

Use this page when moving an existing project between spoolway releases. For a fresh project,
install the current release and run `spoolway init`; none of the earlier-version steps apply.

## Version overview

| Upgrade | What changes | What you need to do |
|---|---|---|
| 0.3.x to 0.4.x | Retired config, template and pipeline-generation features are removed. | Remove the retired entries described below. |
| 0.2.x to 0.3.x | Project state moves from a checkout-name directory to an id-keyed home. | Stop live work, then run any spoolway command and let the automatic move finish. |
| 0.1.x to 0.2.x | Projects must be claimed; commands and state names change. | Run `spoolway init` and update old scripts and config keys. |

Install the target version, then read its embedded notes:

```
npm install -g spoolway@0.4.0
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
