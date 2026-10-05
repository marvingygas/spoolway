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
| 0.6.x to 0.7.x | Bare `spoolway` is the one screen, and `dispatch`, `queue`, `jobs` and `eval` are plain commands; `dispatch.worktree_root` and `issue_tracking.on_fail` are gone; finished tasks are kept forever until `housekeeping.archive_retention_days` says otherwise; `issue_tracking.key_in_names` defaults to `true`; the tracker hooks are rewritten and the closing workflow and tracking templates are retired; `queue conflicts`, `touches:`, `parallel:`, `dispatch --plain`/`--force` and `init --adopt`/`--new-id`/`--take-over` are removed; `config path`, `eval` and `doctor` print differently; `sync` migrates only from 0.6.0 and applies nothing implicitly. | Run `spoolway sync`, replace your tracker hook, delete the closing workflow, drop the removed flags and keys from scripts, and run `spoolway herdr bind` once. |
| 0.5.x to 0.6.x | Retired pipeline shapes are migrated on update; `loop:` counts arrivals; installed skills are always rewritten; `/spoolway-doctor` and `spoolway spend` are gone; `spoolway eval` flags change. | Open spoolway, apply the update, read what it migrated, and update scripts that call `spend` or the removed `eval` flags. |
| 0.4.x to 0.5.x | `sync` takes over from `update`; `dispatch.interval`, `dispatch.default_pipeline` and the tmux backend are gone; tasks name their own `pipeline:` and `base:`. | Delete the retired keys, set `pipeline:` and `base:` on every task, and apply the update. |
| 0.3.x to 0.4.x | Retired config, template and pipeline-generation features are removed. | Remove the retired entries described below. |
| 0.2.x to 0.3.x | Project state moves from a checkout-name directory to an id-keyed home. | Stop live work, then run any spoolway command and let the automatic move finish. |
| 0.1.x to 0.2.x | Projects must be claimed; commands and state names change. | Run `spoolway init` and update old scripts and config keys. |

Install the target version, then read its embedded notes:

```
npm install -g spoolway@0.7.0
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

## 0.6.x to 0.7.x

Upgrade every machine and CI job that reads the project first: once `sync` writes
`housekeeping.archive_retention_days`, a 0.6.x binary refuses `config.toml` with `unknown field`.
Then run `spoolway sync` (or `spoolway sync --dry-run` to read what it would do) and work through
the list. Nothing applies an update by itself any more: a command in a checkout that is behind
prints `Run spoolway sync to apply the last update.` and carries on.

- `sync` now only knows how to bring a project forward from 0.6.0. A project that was never
  brought to 0.6 — one whose pipeline files still carry a shape 0.6 retired, such as a
  self-routing `on_fail:` or the old map form of `loop:` — is no longer migrated
  automatically. A plain `spoolway init` keeps every file that already exists, so it does not
  replace an old-shaped pipeline. Set the project up again with `spoolway init --force`, or
  remove the old `.spoolway/` setup and run `spoolway init`. Otherwise hand-edit each file to
  the current shape before applying this update.
- `spoolway sync` and `sync --dry-run` now exit non-zero, naming `config.toml`, when it cannot be
  read or parsed; 0.6.0 exited 0. Fix the file with `spoolway config edit` before a CI job runs
  `sync --dry-run` as a check.
- Jira-tracked projects: replace the hook with `spoolway sync --replace .spoolway/hooks/jira.sh`.
  `sync` never refreshes hook scripts, and the 0.6.0 `jira.sh` fails every `spoolway queue add`
  with `ticket FAILED`, after creating its Story. Delete the orphan Stories it left, and upgrade
  `acli` to 1.3.39 or newer; the new hook also needs `gh` 2.97.0 or newer, `jq` and bash.
- GitHub-tracked projects: replace the hook with
  `spoolway sync --replace .spoolway/hooks/github.sh`, then re-apply local edits from the `.bak`
  it saves. The 0.6.0 hook still opens issues but logs `cat: '': No such file or directory` and
  drops the `Mirrors task …` lines. The new one marks an issue in progress on the new `started`
  event, applies `labels:`, and on `done` comments `Ready for review in <PR URL>` on the issue.
- Delete `.github/workflows/spoolway-issues.yml` if 0.6.0 wrote it. `init` no longer writes it and
  the new `github.sh` no longer posts the marker it reads. Close issues on merge as
  `docs/configuration.md` describes under "Closing tickets on merge".
- `issue_tracking.key_in_names` now defaults to `true`. A project 0.6.0 set up keeps the
  `key_in_names = false` it wrote. A config that never names it gets `key_in_names = true` from
  `spoolway sync`, and the report says so. Run `spoolway config set issue_tracking.key_in_names
  false` to turn it off, or `true` to put the tracker key on group, branch and worktree names
  (`task/gh-123-…`).
- `.spoolway/templates/tracking/epic.md` and `ticket.md` are retired, and `sync` deletes both
  whatever they hold. Copy any wording you kept there into your `open` hook first; the hook no
  longer receives `SPOOLWAY_EPIC_BODY` or `SPOOLWAY_TICKET_BODY`. The empty `templates/tracking/`
  folder stays and can be removed.
- `dispatch.worktree_root` is retired: every dispatched worktree now lands under the project
  home, with no setting to move it. Delete any `dispatch.worktree_root = ...` line from
  `.spoolway/config.toml`; a config that still sets it loads, with a note naming `spoolway
  sync`, and `spoolway sync` drops the key on the next save. The report names the old
  directory, and, when a queued task still has a worktree there, names that task too.
- `issue_tracking.on_fail` is retired the same way: a failing `queued`, `started` or `done`
  hook always pauses its task now, with nothing left to configure. Delete any
  `issue_tracking.on_fail = ...` line; a config that still sets it loads with a note, and
  `spoolway sync` drops the key on the next save. A hook that must not stop work has to exit 0
  for that failure, or the task is queued with `tracking: off`. `spoolway resume` re-runs a hook
  that paused its task.
- Finished tasks in `archive/` are no longer pruned by `housekeeping.retention_days`. They are kept
  forever until `housekeeping.archive_retention_days` is set; `sync` adds it as `0`. To prune as
  0.6.0 did, set `housekeeping.archive_retention_days` to the `housekeeping.retention_days` your
  project had, for example `spoolway config set housekeeping.archive_retention_days 14`.
- `dispatch --plain` and `dispatch --force` are removed, and so is exit code 5. Drop both flags
  from any script that calls `spoolway dispatch`, and match exit 4 (`Dispatcher already running`),
  which now also means a bare `spoolway` screen holds the project.
- `spoolway queue` and `spoolway jobs` with no subcommand print usage, and `spoolway eval` always
  prints its table. Open bare `spoolway` for the screens.
- `init --adopt`, `init --new-id` and `init --take-over` are removed, and each exits 2 naming
  its replacement. Drop `--take-over`; it already did nothing. For `--adopt`, run
  `spoolway init --workspace <name>` from the checkout that replaces the old one, otherwise
  `spoolway init`. For `--new-id`, delete `.git/spoolway-id` and the old home's
  `~/.spoolway/<label>-<id>/project.toml`, then run `spoolway init` again; the old home's tasks
  stay on disk. `init --yes` still works in scripts. The advice to run `init --adopt` or
  `init --new-id` in the 0.2.x to 0.3.x section below applies to those versions only.
- `spoolway init` writes `.spoolway/hooks/` only when a tracker is chosen. A script that expects the
  folder after a plain `init` must pass `--tracker github` or `--tracker jira`.
- `spoolway queue conflicts` is removed, and the `touches:` task key is no longer read. Order tasks
  with `depends_on`.
- `parallel:` is removed and `queue add` refuses it. Every group is one chain: chain tasks with
  `depends_on`, put independent work in a group of its own, and let a group's first task
  `depends_on` another group's last task to stack. `spoolway stack` prints `conflicts` where it
  printed `siblings`.
- `cut_from:` is now `starts_from:`; the old spelling is still read, so only tooling that reads
  task files changes. `queue add` refuses a batch when a task's start branch exists nowhere: set
  `starts_from:` on that task and requeue.
- `spoolway config path` prints `setup:`, `local:` and `overrides:` folder lines instead of the
  `config.toml` path. Read `spoolway config path --json` and append `config.toml` to `setup`.
- `spoolway eval` shows row totals by default. Add `--per-run` to a script that scrapes the old
  `IN/RUN` … `TIME/RUN` columns, or read `--csv` or `--json`. A clean `spoolway doctor` ends
  `N checks passed.`, and `doctor --live` forces the live pane check outside herdr.
- The shipped prompts and the `default` and `bugfix` task skeletons changed, and `sync` keeps your
  copies. To take the new ones, run `spoolway sync --replace .spoolway/prompts/<name>/PROMPT.md`
  and `spoolway sync --replace .spoolway/templates/tasks/<name>.md`.
- If you ran `spoolway herdr bind` on 0.6.0, run it once more. It removes the retired
  `prefix+alt+d` → `spoolway dispatch` and `prefix+alt+q` → `spoolway queue` keys and binds
  `prefix+alt+d` to bare `spoolway`, leaving `prefix+alt+q` free. `spoolway herdr unbind` removes
  all four 0.6.0 keys.
- The embedded price table was refreshed, so `spoolway eval` costs and context-window thresholds
  follow the new figures. A project's own `[models.*]` entries still win.
- The four spoolway skills (`spoolway-plan`, `spoolway-tasks`, `spoolway-config` and
  `spoolway-calibrate`) are kept current by name, with no `.installed-by-spoolway` marker
  file needed to tell a shipped skill from a hand-written one. Keep edits to a shipped skill
  under a name of your own.

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

Two commands changed shape. The update does not rewrite scripts that call them:

- `spoolway spend` is removed. `spoolway eval` is the one command that reads the ledger. It
  shows tokens and cost by group, task, pipeline, step or version. The `spend` cuts by model,
  project, month and lane have no direct replacement.
- `spoolway eval` no longer takes `--runs`, `--limit` or `--month`. `--by` picks what one row
  stands for: `group`, `task`, `pipeline` (the default), `step` or `version`. Use `--by task`
  where you used `--runs`. Use `--since 2026-08 --until 2026-08` where you used
  `--month 2026-08`. The new `--pipeline-version <x.y>` keeps only lanes that ran under one
  pipeline `version:`.

## 0.4.x to 0.5.x

- `dispatch.interval` is retired: the dispatcher now polls at one fixed rate and there is
  nothing to configure. Delete any `dispatch.interval = ...` line from `.spoolway/config.toml`;
  a config that still sets it loads with a note and drops the key on the next save.
- The tmux backend and `dispatch.tmux_mode` are gone. Change `dispatch.backend` to `herdr` or
  `headless` in `.spoolway/config.toml`, and delete `dispatch.tmux_mode`; a config still naming
  `tmux` loads as `herdr` with a note and rewrites both keys on the next save.
- `dispatch.default_pipeline` is gone. Add `pipeline: <name>` naming one of the project's
  pipelines to every task that relied on the default; a run refuses to start with one
  missing.
- A task no longer inherits `base:` from the checked-out branch. Set `base: <branch>` on every
  task that relied on the fallback.
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
