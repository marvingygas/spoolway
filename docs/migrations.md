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
| 0.14.1 to the next release | `dispatch.backend` is retired; every run uses herdr, and only the test harness picks the headless backend, with `SPOOLWAY_TEST_BACKEND=headless`. A step's `gate:` is `on_pass`, `on_fail` or `always`, and older binaries refuse those values. A `gate:` on a `run:` step and a `gate_at:` that names a command step are refused. | Run `spoolway sync` and commit what it writes, export `SPOOLWAY_TEST_BACKEND=headless` in any script that ran `spoolway config set dispatch.backend headless`, and move each `gate:` on a `run:` step to the agent step after it or delete it. |
| 0.13.0 to 0.14.0 | A step's `headless:` key is now `hide:` and older binaries refuse `hide:`; a new project keeps three lanes and shows command steps; `housekeeping.price_max_age_days` is retired; tasks are cut from `origin/<base>`; a lane never runs in the main checkout; matching `[models]` rows merge value by value; `eval` adds TURNS and COMPLEXITY; `queue list --json` lists `opening ticket` rows; `pipeline check` and `doctor` fail a stale `gate_at:` and a routine id held twice; `spoolway stack` runs only in its task's worktree; lanes cannot write the private overrides layer. | Run `spoolway sync` and commit what it writes, upgrade CI and teammates to the same release, set `visible_lanes` and `hide_commands` for the new defaults, push a task's base before queueing it, read `spoolway models` for merged rows, update scripts that read `eval` exports or `queue list --json`, and clear what `spoolway doctor` fails. |
| 0.12.0 to 0.13.0 | The shipped pipelines end at a `create-pr` agent step and `spoolway stack` is deprecated; the shipped tracker hooks close a ticket with no pull request; a failing issue hook stops its task on `hook_failed` instead of `paused`; `eval --per-run` is `--per-task` and its export keys and columns change; unqueuing a routine copy deletes it; `sync` writes every `[dispatch]` key. | Run `spoolway sync` and `spoolway sync --replace` on your tracker hook, replace each `spoolway stack` step with a `create-pr` agent step, and update scripts that read `eval` exports, `--per-run`, held tasks' `state` or unqueued routine drafts. |
| 0.11.0 to 0.12.0 | A model with no `prompt_cache_ttl` has no age limit; a task keeps its five newest lanes and `keep_finished_lanes` is retired; command steps run without a pane; herdr lanes are named `<step>-<tag>`; step ids are limited to 26 characters; resuming a caught `--fail` goes down `on_fail`; issue and git hooks have fixed time limits; `queue add` refuses misspelt keys, placeholders, ids in use and unreachable `gate_at:`; four commands refuse `--json`; `queue list --json` `stage` names the stopped step; deleting `dispatch.pid` stops the dispatcher; `doctor` fails more settings. | Run `spoolway sync`, set `prompt_cache_ttl`, rename long step ids and quoted `[models]` tables, set `visible_lanes` or `hide_commands` to taste, add `# spoolway-login:` to tracker hooks, update scripts that read lane names, `--json` or `stage`, and clear what `spoolway doctor` fails. |
| 0.10.0 to 0.11.0 | A background step with `on_fail` is refused when the pipeline loads and its exit never moves the task; `sync`, `init` and `install` refuse a file that matches no copy spoolway has shipped and delete the version marker; the issue question is a checklist of groups and its popups changed; `queue add` calls no `open` hook for a `tracking: off` task; a routine runs one copy at a time and reads by its name on the board, the lane, the workspace and `eval`; `routine:` is refused in a task file; `eval --by task` writes WHEN with the time; `open` reads `~/.spoolway/editor.toml` and falls back to `vim`. | Fix every background step with `on_fail`, run `spoolway sync` and `spoolway sync --replace` for each refused file, commit the deleted marker, update scripts that drive the issue popups, find routine panes by id or read `eval`'s `when`, and rename a `routine:` key of your own. |
| 0.9.0 to 0.10.0 | Row actions move behind `o` and popups take no letter answers; a job with no `base` never fires; `sync` writes a version marker older binaries refuse to overwrite; long task and step ids are refused; `--json` is refused where a command has no JSON form; `pipeline check` reads the live queue; resuming an archived task fails; silent lanes are reminded instead of escalated; agent launch failures go to `blocked`; a gated command reruns after an unblocker pass; background checks and failed removals hold a task on `done`; `eval` and the ledger carry command steps; `[models]` globs match more names; requeued tasks fire their hooks again; `doctor` fails more settings. | Learn the options menu, add `base` to every job, commit `.spoolway/spoolway-version`, shorten long ids, drop `--json` where refused, update scripts that read `eval`, the ledger or `queue list`, and clear what `spoolway doctor` fails. |
| 0.8.0 to 0.9.0 | A job table with an unknown key never fires; a first cut refuses a leftover `task/<id>` branch; no command prints the update or sync notice; hook failures leave the board and make `doctor` exit non-zero; a partial `[models]` row keeps the price table's rates and window; `slots` caps a whole row; recorded costs include subagents and Claude Code's own total; `blocked.*` overrides apply; config reloads every pass; unknown keys load with a note; unattended escalations go to `blocked`; `openrouter/` models leave the price table. | Fix unknown job keys, clear leftover task branches, run `spoolway sync` in each project, review cost and session limits, read `spoolway doctor` and `spoolway override list`, and set `context_window` for `openrouter/` models. |
| 0.7.1 to 0.8.0 | A pipeline with `end: true` or `on_pass: blocked` is refused when it loads; `session_reuse_idle` is renamed `prompt_cache_ttl` and defaults to five minutes on hosted models; `session_reuse_ctx` and `session_blocked_ctx` default to 20 and 40; `R` on the board is gone; `dispatch.herdr_mode` and `models.<glob>.exclusive` are retired; every step keeps its pane until its task is done; `loop:` limits every move; a running dispatcher routes on the pipelines it started with; a nested `.spoolway/` is refused; `config set` refuses values `doctor` would fail; `queue add --from` rows say `group issue` and `task issue`; `eval` gains USD columns. | Replace `end: true` and `on_pass: blocked`, set `prompt_cache_ttl` and `context_window` where the defaults do not fit, run `spoolway sync`, and restart the dispatcher after editing a pipeline. |
| 0.7.0 to 0.7.1 | Pipelines that could never run are refused when they load: `loop: 0`, which 0.7.0 read as no limit, a first step of `blocked`, and a `gate_at:` that names no step; `spoolway jobs run` is removed; an unset or empty `HOME` is refused; `resume` refuses queued and running tasks; `gate: true` and `gate_at:` now work on a command step; leaving `blocked` resets every `loop:` count; each worktree builds into its own `target/`; workspace folders are never deleted; `init --force` keeps the provider and tracker; `sync --replace` writes `.bak.N`. | Delete every `loop: 0` line, drop `spoolway jobs run` from scripts, set `HOME` in CI, check queued tasks' `gate_at:`, remove the old `.cargo-target` folder once older tasks finish, and run `spoolway sync`. |
| 0.6.x to 0.7.x | Bare `spoolway` is the one screen, and `dispatch`, `queue`, `jobs` and `eval` are plain commands; `dispatch.worktree_root` and `issue_tracking.on_fail` are gone; finished tasks are kept forever until `housekeeping.archive_retention_days` says otherwise; `issue_tracking.key_in_names` defaults to `true`; the tracker hooks are rewritten and the closing workflow and tracking templates are retired; `queue conflicts`, `touches:`, `parallel:`, `dispatch --plain`/`--force` and `init --adopt`/`--new-id`/`--take-over` are removed; `config path`, `eval` and `doctor` print differently; `sync` migrates only from 0.6.0 and applies nothing implicitly. | Run `spoolway sync`, replace your tracker hook, delete the closing workflow, drop the removed flags and keys from scripts, and run `spoolway herdr bind` once. |
| 0.5.x to 0.6.x | Retired pipeline shapes are migrated on update; `loop:` counts arrivals; installed skills are always rewritten; `/spoolway-doctor` and `spoolway spend` are gone; `spoolway eval` flags change. | Open spoolway, apply the update, read what it migrated, and update scripts that call `spend` or the removed `eval` flags. |
| 0.4.x to 0.5.x | `sync` takes over from `update`; `dispatch.interval`, `dispatch.default_pipeline` and the tmux backend are gone; tasks name their own `pipeline:` and `base:`. | Delete the retired keys, set `pipeline:` and `base:` on every task, and apply the update. |
| 0.3.x to 0.4.x | Retired config, template and pipeline-generation features are removed. | Remove the retired entries described below. |
| 0.2.x to 0.3.x | Project state moves from a checkout-name directory to an id-keyed home. | Stop live work, then run any spoolway command and let the automatic move finish. |
| 0.1.x to 0.2.x | Projects must be claimed; commands and state names change. | Run `spoolway init` and update old scripts and config keys. |

Install the target version, then read its embedded notes:

```
npm install -g spoolway@0.10.0
spoolway whats-new --since <your-current-version>
```

The [changelog](https://github.com/marvingygas/spoolway/blob/main/CHANGELOG.md) is the complete
release record. This guide collects only the steps that may require action.

## Syncing with an older spoolway

`spoolway sync` compares each skill file, pipeline key block and config reference table with the
copies spoolway has shipped. A file that matches none was written by a newer spoolway or edited by
hand, so it is refused and left as it is. Update the binary, or run `spoolway sync --replace
<path>` to take this binary's copy. See [Files spoolway
refuses](installation.md#files-spoolway-refuses).

## Windows

The last native Windows release is 0.4.x. `@spoolway/win32-x64` was published up to 0.4.0, and
`npm/targets.json` no longer carries that target, so no release after 0.4.x has a Windows
package. On Windows, install the Linux package under WSL:

```
wsl npm install -g spoolway
```

## 0.14.1 to the next release

Run `spoolway sync` in each project and commit what it writes. It drops the retired
`dispatch.backend` line and rewrites `gate: true` to `gate: on_pass`. It refuses a pipeline file
that puts `gate:` on a `run:` step.

- `dispatch.backend` is retired. Every run uses herdr, and only the end-to-end harness picks the
  headless backend, by exporting `SPOOLWAY_TEST_BACKEND=headless`. A config that still holds
  `backend = "herdr"` or `backend = "headless"` loads without a note, and `spoolway sync` drops the
  line and its row in the reference header. `spoolway config get` and `config set` refuse the key,
  as they do `dispatch.tmux_mode`. A script that ran `spoolway config set dispatch.backend headless`
  must export `SPOOLWAY_TEST_BACKEND=headless` instead.
- A step's `gate:` is `on_pass`, `on_fail` or `always`. `on_pass` holds a pass, `on_fail` holds a
  fail and `always` holds either. `gate: true` still loads and reads as `on_pass`, and `gate:
  false` reads as no gate. `spoolway sync` rewrites `gate: true` to `gate: on_pass` in the
  checkout's pipeline files, the private pipelines and the private patches, and prints one line
  per step. It deletes `gate: false` in the pipeline files and keeps it in a private patch, where
  it switches off a gate the tracked step declares. See [Gates](pipelines.md#gates).
- Older spoolway binaries refuse a `gate:` value that is not `true` or `false`. Once a repo is
  synced, every machine and CI job that runs it needs the new release, or its pipelines will not
  load.
- A `gate:` on a step with `run:` is refused when the pipeline loads, by `spoolway pipeline check`
  and by `spoolway sync`. The message names the step. Delete the `gate:`, or put it on the agent
  step after the command. `spoolway pipeline override --set <step>.gate=<value>` on a
  command step is refused too.
- A `gate_at:` that names a command step is refused by `spoolway queue add` and
  `spoolway task contract`. For a task already queued, `spoolway pipeline check` and
  `spoolway doctor` fail, the dispatcher parks a queued one on `paused`, and a running one carries
  on with the command's exit not held. Change the task's `gate_at:` to an agent step or delete it.
  `gate_at: blocked` is accepted.
- The board's pause panel offers `pause when <step> finishes` only on a running agent step. A
  running command step offers `pause now` alone.
- `spoolway queue route --json` and `spoolway pipeline show --json` print a step's `gate` as
  `"on_pass"`, `"on_fail"`, `"always"` or `null`. A script that tests it for `true` or `false`
  reads the string or `null` instead.

## 0.13.0 to 0.14.0

Run `spoolway sync` in each project and commit what it writes. It renames `headless:` to `hide:`,
drops `housekeeping.price_max_age_days` and takes the new planning and calibrate skills. Then read
the notes `spoolway doctor` prints, work through the list, and restart the dispatcher.

- A step's `headless:` key is now `hide:`. The new release refuses `headless:` with any value,
  naming the step. `spoolway sync` renames `headless: true` to `hide: true` in the checkout's
  pipeline files, the private patches and the private pipelines. It deletes `headless: false`
  and writes no `hide:`. `hide: false` forces a pane even when `dispatch.hide_commands` is true,
  while `headless: false` only spelled out the default and followed that setting. A step that
  carries both keys, or a `headless:` in flow style, is refused and left as it is. See
  [Background and headless](pipelines.md#background-and-headless).
- Check each private patch whose `headless: false` overrode a tracked step's `headless: true`.
  Once both are synced, the tracked `hide: true` wins and the step runs without a pane. The note
  `sync` prints under the patch names the step. Set `hide: false` in the patch to keep the pane.
- Older spoolway binaries refuse `hide:`. Once a repo is synced, every machine and CI job that
  runs it needs the new release, or its pipelines will not load.
- Choose your lane and pane defaults. A new project keeps three of a task's lanes open and shows
  command steps in panes. A project 0.13.0 set up or synced already has `visible_lanes = 5` and
  `hide_commands = true` written in `config.toml`, so it keeps 0.13.0's behaviour, and `sync`
  changes neither. To take the new defaults, run:

  ```sh
  spoolway config set dispatch.visible_lanes 3
  spoolway config set dispatch.hide_commands false
  ```

- `housekeeping.price_max_age_days` is retired. `spoolway sync` drops it from `config.toml` and
  names it in its list of retired settings, so there is nothing to delete by hand. `spoolway
  config get` and `config set` refuse it, so drop it from scripts. `spoolway models` still shows
  the price table's age.
- Push what a task should start from before you queue it. A task whose start point is its own
  `base:` is cut from `origin/<base>` after one fetch, so unpushed commits on your local base
  branch are no longer in it. With no `origin`, or no base branch there, the task is cut from the
  local branch and the startup warnings say so. A dependent task still starts from its parent's
  local `task/<id>` branch.
- Keep the main checkout off task branches. A lane never runs in the main checkout, so a task
  whose branch is out there fails its start with a message naming the branch and the checkout.
  Switch the checkout to another branch and resume the task. A task with no `worktree_path` on
  record is blocked with a message saying to remove `workspace_id` from its task file.
- Review your `[models]` rows. Matching rows now combine value by value, where 0.13.0 let the most
  literal row hide the others, and a row that sets one tier rate keeps the price table's other
  tier rates instead of reading them as $0. Costs, `unattended.max_cost_usd`, slot caps and
  `compact_ctx` can change. `spoolway models` prints a `rows:` line naming the rows that supply a
  model's values. Set each value on the row you mean it for.
- Update scripts that read `spoolway eval` exports by column position. Every lane view gains
  `turns`, or `turns_per_task` under `--per-task`, after `blocks`, and `--by task` gains
  `complexity` between `when` and `tasks`. CSV whole numbers round a tie away from zero, so a
  value on .5 can differ by one from 0.13.0's export.
- Skip `opening ticket` rows when a script reads `spoolway queue list --json`. While a routine's
  `open` hook runs, `tasks` holds a row for an id that has no task file yet.
- Fix what `spoolway pipeline check` and `spoolway doctor` now fail. A queued task whose
  `gate_at:` names a step its pipeline lacks fails both, and the dispatcher parks it on `paused`
  until you change the `gate_at:`. Two routine files that hold one task `id:` fail both too; give
  one of them an `id:` of its own.
- Run `spoolway stack` only from its task's worktree. It now exits 1 anywhere else, because it
  rewrote and force-pushed whatever branch it ran on. The shipped pipelines use `create-pr`
  instead.
- Make private-layer changes outside a lane. Inside a lane's worktree, `override drop`, `pipeline
  override`, `prompt override`, `config override`, `pipeline copy` and `prompt copy` refuse and
  write nothing, and a refused `config set`, `pipeline promote` or `override promote` no longer
  prints a `spoolway -C <main checkout>` line. A lane writes the setting it wanted into its
  report instead.

## 0.12.0 to 0.13.0

Run `spoolway sync` in each project and commit what it writes. It writes every `[dispatch]` key
into `config.toml`, defaults included, names each one it adds, and takes the new planning and
calibrate skills. No value changes. Then read the notes `spoolway doctor` prints, work through the
list, and restart the dispatcher.

- Replace each step that runs `spoolway stack`. The shipped pipelines now end at a `create-pr`
  agent step, and `spoolway stack` is deprecated and may be removed in a future release.
  `spoolway doctor` and `spoolway sync` note ``Still run by: <pipeline> · <step>`` for each one.
  `sync` changes no pipeline step, so do it by hand. Run `spoolway init`, which adds only the
  missing `.spoolway/prompts/create-pr/PROMPT.md`, then change each `handover` step and the
  `on_pass` that leads to it:

  ```yaml
    - id: create-pr
      description: >-
        Push this task's branch and open its pull request, with a short
        plain-English summary of the diff. Nothing is merged here.
      agent: claude
      prompt: create-pr
      model: ""
      effort: ""
      on_pass: done
  ```

  Give the step a model and effort; a cheap model is enough. Unlike `handover`, it spends an agent
  turn. `spoolway pipeline check` names a step left without a model.
- Take the shipped tracker hook. `github.sh` and `jira.sh` now close a task's ticket at `done` when
  its branch has no pull request, where 0.12.0's failed and stopped the task. `sync` does not
  rewrite hooks. Run `spoolway sync --replace .spoolway/hooks/github.sh` or
  `.spoolway/hooks/jira.sh`, then re-apply your own edits from the `.bak` file it saves. In
  `jira.sh`, set the new `status_done` to your workflow's done status if it is not `Done`.
- Update scripts that look for tasks held on a failed hook. Such a task now stands on its own
  `hook_failed` stage, where 0.12.0 paused it, so `spoolway queue list --json` writes
  `state: "hook_failed"`, and moving there sets off no `paused` hook event. A task 0.12.0 paused on
  a hook is read as `hook_failed`. `spoolway resume <task>` still reruns the hook, and
  `spoolway resume <task> --skip-hook` or `[k]` on the board carries the task on without it.
- Rename `spoolway eval --per-run` to `--per-task` in scripts. The old flag exits 2. In `--csv`,
  `--json` and `--sort`, `runs` is now `tasks`, every `*_per_run` key is `*_per_task`, and
  `ctx_peak_avg_tokens` and `ctx_peak_avg_pct` are `ctx_avg_tokens` and `ctx_avg_pct`. The lanes
  table reads `TASKS`, `X/TASK` and `CTX AVG`, and draws `CTX PEAK` only under `--by task`.
- Update anything that reads an `eval` export. `--csv` and `--json` now carry only the columns the
  chosen view draws, starting with `project`. `by`, `lanes`, `unpriced` and the other view's
  columns are gone, and the closing row is labelled `total` or `average`. Add `--per-task` for the
  per-task figures. `--sort by` is refused, and the screen's `[e]` file is named
  `eval-by-<by>-<view>-<stamp>.csv`.
- Expect an unqueued routine copy to be deleted. 0.12.0 turned it into a pending draft such as
  `nightly-3.md`; now the copy is gone and its routine stays, ready to queue again. The board's
  `unqueue`, `spoolway queue unqueue` with or without `--all`, and `--force` on a started copy all
  do this, and print ``deleted `<id>` `` in place of ``unqueued `<id>` ``.

## 0.11.0 to 0.12.0

Run `spoolway sync` in each project and commit what it writes. It rewrites the settings reference in
`config.toml`, drops a `dispatch.keep_finished_lanes` value in favour of `dispatch.visible_lanes`,
adds a commented cache-lifetime example, and takes the new `spoolway-plan` and `spoolway-tasks`
skills and pipeline key blocks. Then read the notes and failures `spoolway doctor` prints, work
through the list, and restart the dispatcher.

- Set `prompt_cache_ttl` for your hosted models. 0.11.0 held every model not marked `local` to five
  minutes; a model with no value now resumes its session however long it has been idle, and
  `spoolway doctor` notes ``models.'claude-*'.prompt_cache_ttl is not set``. Run
  `spoolway config set 'models."claude-*".prompt_cache_ttl' 5m` to keep 0.11.0's limit, or
  uncomment the example `sync` wrote and set your provider's own. Do the same for `gpt-*` on a
  `codex` profile. `local` no longer changes the limit.
- Rename every `[models.'"<glob>"']` table in `config.toml` to `[models."<glob>"]`. 0.11.0's
  `spoolway config set 'models."<glob>".<key>'` wrote the quote marks into the table's name, so
  the row matches no model and its values apply to nothing. This release leaves such a table as it
  is.
- Choose how many lanes a task keeps open. A task now keeps its five newest, and each new agent
  step closes the oldest finished ones; 0.11.0 kept every lane until the task was done. Run
  `spoolway config set dispatch.visible_lanes all` to keep them all. `dispatch.keep_finished_lanes`
  is retired: a value in the file loads as `visible_lanes`, `false` as `1`, until `sync` rewrites
  it, and `spoolway config get` and `set` refuse the old key. See
  [Finished lanes keep their pane](dispatcher.md#finished-lanes-keep-their-pane).
- Expect command steps to run without a pane. Their output is still in
  `commands/<task> · <step>.log` in the project home. Run
  `spoolway config set dispatch.hide_commands false` to give each command a pane again; unlike
  0.11.0's, it stays open after the command exits and counts toward `visible_lanes`.
- Update scripts that find a lane in herdr by its old name. A lane is now named `<step>-<tag>`, such
  as `review-spec-5f2`, where 0.11.0 used `<task>__<step>` or an `l<hex>` alias. `lane_aliases.json`
  in the project home records every name. Lanes 0.11.0 started are still found.
- Rename every pipeline step id longer than 26 characters. A lane's herdr name is its step plus a
  short task tag, and herdr refuses a name past 32 characters, so the limit fell from 64 to 26.
  `spoolway pipeline check`, `spoolway doctor` and the dispatcher refuse a longer id
  with ``step id `<id>` is <n> characters long, and the limit is 26``. The longest step id in a
  shipped pipeline is 15 characters. The task id limit stays 100.
- Pick the `on_pass` row to wave a caught failure through. Resuming a task whose gate caught an agent
  step's `--fail` now goes down that step's `on_fail`, or to `blocked`, where 0.11.0 went down
  `on_pass`. The `on_pass` step is one key down in the board's resume popup, and
  `spoolway resume <task> --stage <step>` names it from a shell.
- Make slow hooks return within their limit. Issue hooks are stopped after 1 minute for `fetch` and
  `open` and after 10 minutes for the lifecycle events, and a `post-checkout` git hook during a
  worktree cut or a commit hook on the leftover-work commit after 10 minutes. 0.11.0 waited for
  ever. No key changes the limits.
- Add a `# spoolway-login:` line to each tracker hook, such as
  `# spoolway-login: gh auth status`, or run `spoolway sync --replace .spoolway/hooks/<hook>.sh`.
  `sync` does not rewrite hooks, and `spoolway doctor` notes a hook without the line. Once it is
  there, a queue add with no terminal, such as a job firing, queues a batch whose login has lapsed
  with `tracking: off` instead of refusing it.
- Fix task files `queue add` now refuses: a key spelt like a contract key with another case or a
  `-`, such as `gate-at:`; a section still holding the skeleton's `[[…]]` placeholder; an id
  another queue file holds or a routine is named; an id whose `task/<id>` branch or worktree is
  still on disk; and a `gate_at:` on a step the task walks past. Each refusal names its fix.
- Drop `--json` from scripts that call `spoolway queue show`, `spoolway prompt show`,
  `spoolway whats-new` or `spoolway queue add --from`. They now exit 1 with it.
- Update scripts that read `spoolway queue list --json`. On a paused row, or a blocked row no
  unblocker is working, `stage` is the step the task stopped at, not `paused` or `blocked`; match
  `state` instead. A task naming a missing pipeline is an `unknown` row, and the command exits 0.
- Do not delete `dispatch.pid` to start a second dispatcher. The running one now stops at its next
  pass when the file is gone or names another. With a `jobs.toml` that does not parse,
  `spoolway dispatch` keeps running instead of exiting 3 on an empty queue, so a supervisor that
  restarts on exit 3 waits. In a home-mode workspace a project job fires once per workspace.
- Expect a task whose worktree was deleted mid-step to block. 0.11.0 cut a fresh worktree and
  routed the lost result. Resume the task to run the step again. The advice for a worktree that
  cannot be removed is now a `chmod`, `rm` and `git worktree prune` line.
- Know where a write lands in a linked worktree. Inside one with its own `.spoolway/` on a branch,
  `spoolway config set`, `spoolway override promote` and `spoolway pipeline promote` now write that
  worktree's files. The dispatcher keeps the main checkout's copy until the branch merges.
- Replace a zero `dispatch.lane_child_ceiling` or `dispatch.lane_quiet` with a duration such as `1h`
  or `15m`. `config set` refuses zero, and `spoolway doctor` fails one already in the file.
- Clear what `spoolway doctor` now fails: a `compact_ctx` outside 1 to 100, a `permission_mode` the
  agent does not accept, a hook script with CRLF line endings or no exec bit, a `prompt:` that
  starts with `/` or holds `..`, a missing `task_template:`, duplicate queue ids, a queued task
  named like a routine, a job whose routine cannot be queued, and a deleted task's leftover
  worktree or branch. The dispatcher refuses a step with a bad `compact_ctx` or permission mode.
- Expect higher cost figures on `[models]` rows with their own base rates. They now keep the price
  table's `above_<N>k_tokens` tier, so `spoolway eval` and `unattended.max_cost_usd` count large
  turns at the tier rate. Set `compact_ctx` on a model rather than exporting
  `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`; a lane no longer inherits it.
- Expect a `last:` step to run once in a chain that mixes pipelines. 0.11.0 skipped it for the whole
  chain when any task above it walked past.

## 0.10.0 to 0.11.0

Upgrade every machine and CI job that reads the project first. 0.10.0 refuses to write over newer
files only by the version marker, and this release deletes that marker, so a 0.10.0 binary left
running `sync`, `init` or `install` writes its older skill files over yours and the marker back.
Then run `spoolway sync` in each project, commit the deleted `.spoolway/spoolway-version`, read
the notes and failures `spoolway doctor` prints, work through the list, and restart the dispatcher.

- Fix every `background: true` step that declares `on_fail`. Such a pipeline no longer loads, and
  `spoolway pipeline check`, `spoolway doctor`, the dispatcher and `spoolway sync` all refuse it
  with ``step `<id>` runs in the background but declares `on_fail` ``. To keep the step a sidecar,
  delete `on_fail:`. To make it a check the task waits for, delete `background:`. A background
  run never moves its task now, and a task no longer waits on `done` for one. A run that exits
  non-zero or hits its `timeout:` writes one line to the task's `## Status Log` instead.
- Run `spoolway sync --replace <path>` for each file `sync` refuses. `sync` no longer overwrites a
  skill file, a pipeline key block or a `config.toml` settings reference that matches no copy
  spoolway has shipped, such as one you edited by hand. 0.10.0 replaced a hand-edited key block
  and always rewrote skill files. `--replace` now takes a project or user-level skill file, a
  pipeline file, whose key block alone is swapped, and `config.toml`, whose values are kept.
  `init` and `install` write no skill file while any is refused, `--force` included. See
  [Files spoolway refuses](installation.md#files-spoolway-refuses).
- Commit the deletion of `.spoolway/spoolway-version`. `sync`, `init` and `install` delete it, and
  `.spoolway-version` in each user skills folder, and list each as `removed`. See [The retired
  version marker](installation.md#the-retired-version-marker).
- Update any pty or tmux script that answers the queue screen's or the routines tab's issue
  question. It is now a checklist of groups headed `deactivate issue tracking for selected
  groups:`, with nothing ticked, and the `create and queue` and `queue only` rows are gone.

  | 0.10.0 | 0.11.0 |
  |---|---|
  | `create and queue`, then `enter` | `enter` |
  | `queue only`, then `enter` | `a`, then `enter` |
  | The `opening issues` popup, one row per ticket | `running the hook: <n> of <total> tasks` |
  | The `issues created` popup | The `queued` popup, opening on `hook executed successfully` |

  `spoolway queue add --no-tracking <group>` gives the same answer from a shell. See
  [`open`](configuration.md#open--a-fifth-event-run-by-queue-add-itself).
- Expect `spoolway queue add` to call no `open` hook for a task that sets `tracking: off`. A group
  whose tasks all set it needs no `group_description:` and gets no `key_in_names` prefix, so its
  branches read `task/<id>`.
- Let a routine's copy finish before you queue the routine again. Queueing a routine is refused
  while any task in the queue, paused and blocked ones included, carries its name in `routine:` or
  as its id. A job skips its window instead and says ``job <name> · due now · skipped, <routine> is
  still in the queue, at step <step>``. Resume, restart or unqueue a stopped copy to free the
  routine. See [Routines](planning.md#routines).
- Rename a `routine:` key in your own task files. spoolway sets it on every queued copy of a
  routine, so `spoolway queue add` refuses a task file that sets it.
- Update scripts that find a routine copy by its numbered id. Its lane is now named
  `<routine> · <step>`, its herdr workspace `spoolway/<routine>`, its tab `<routine>`, and its
  system prompt file `system-prompts/<routine> · <step>.md`. The board and the `queued` popup show
  the routine's name. `spoolway lane` takes either name while the copy is in the queue, and
  `spoolway queue list` still prints the numbered id.
- Update scripts that read `spoolway eval --by task`. WHEN, and `when` under `--json` and
  `--csv`, is now the run's start as `YYYY-MM-DD HH:MM` in local time, not a date. A routine
  copy's run is labelled by its routine in `task`, and rows read newest start first. `--task`
  still matches the task id. `usage.jsonl` lines for a routine copy gain a `routine` key.
- Expect the board's `open` to read `~/.spoolway/editor.toml` before `$VISUAL` and `$EDITOR`, and
  to run `vim`, not `vi`, when none of them is set. `spoolway config edit` and
  `spoolway config override` open the same editor, with the same `vim` fallback. See
  [Editor](dispatcher.md#editor).

## 0.9.0 to 0.10.0

Upgrade every machine and CI job that reads the project first. 0.9.0 does not read the version
marker 0.10.0 writes, so it still overwrites newer files, and it refuses a job table that sets
`base`. Then run `spoolway sync` in each project, commit the new `.spoolway/spoolway-version`, read
the notes and failures `spoolway doctor` prints, work through the list, and restart the dispatcher.

- Learn the options menu. A row's actions moved behind `o`, and popups list their answers as rows
  you pick with `↑`, `↓` and `enter`, with `esc` to close. Letter answers are gone.

  | Screen | 0.9.0 | 0.10.0 |
  |---|---|---|
  | Board | `o`, `p`, `s`, `u` | `o`, then `open`, `pause`, `restart` or `unqueue` |
  | Board | `U` | `spoolway queue unqueue --all` |
  | Queue, tasks pane | `o`, `g` | `o`, then `open` or `gate` |
  | Queue, groups pane | `t`, `s` | `o`, then `trial` or `save as routine` |
  | Routines | `n`, `x` | `o`, then `new job` or `delete` |
  | Jobs | `e`, `space`, `x` | `o`, then `edit`, `pause` or `resume`, or `delete` |
  | Popups | `s`, `i`, `x`, `n`, `g` | pick the row, then `enter` |

  Update any pty or tmux script that drives these screens, such as one that answers the start gate
  of `spoolway dispatch` with `x`.
- Add `base = "<branch>"` to every `[jobs.<name>]` table, in `.spoolway/jobs.toml` and in
  `~/.spoolway/<project>/jobs.toml`, or set `base:` in every routine task. A job whose routine
  tasks set no `base:` no longer fires, where 0.9.0 cut them from the branch the main checkout had
  out. The dispatcher names it with ``job <name>: `<task>` sets no `base:`, and the job has no
  `base` key.`` The routines tab has no `base` field, so add the key by hand.
- Commit `.spoolway/spoolway-version`. A binary older than the version it names refuses to write
  the project, and the same holds for `.spoolway-version` in each user skills folder. Delete the
  marker to take an older binary's copies on purpose.
- Finish or unqueue every task whose id is longer than 100 characters, then queue it again under a
  shorter id. Rename every pipeline step id longer than 64 characters. Both are refused whenever
  spoolway reads them, with ``task id `<id>` is <n> characters long, and the limit is 100``.
- Drop `--json` from commands that have no JSON form. These now exit non-zero under it:
  `pipeline check`, `pipeline override`, `prompt contract`, `prompt list`, `prompt override`,
  `config show`, `config get`, `config contract`, `config override`, `template contract`,
  `hook contract`, `override contract`, `group list`, `sync`, and `queue add` without `--from`. To
  skip the confirmation panel of `sync` at a terminal, run `spoolway sync </dev/null`. In a linked
  worktree, read the `{"path","branch"}` checkout line from stderr, not stdout.
- Expect `spoolway pipeline check` to fail while a queued task names a pipeline or step that no
  longer exists. Fix or requeue the task it names, or resume it onto a step that exists with
  `spoolway resume <id> --stage <step>`; a plain resume of such a task is refused. While a
  dispatcher runs, `queue route` and `queue list` show the pipelines it loaded, so restart it
  before checking an edit with them.
- Queue a new task to run an archived task's work again. `spoolway resume` and
  `spoolway queue resume` on an archived task now exit non-zero. Run `spoolway queue pause <id>`
  before resuming a `blocked` task whose unblocker lane is live.
- Restart a silent lane yourself. A lane that writes nothing after a reminder is no longer
  escalated to `paused` or `blocked`. It stays on its step, is reminded every `dispatch.lane_quiet`,
  and shows as `● silent · <time>` on the board. Pick `restart` from the board's options menu when
  its pane is dead. Accept `"state": "silent"` in scripts that read `spoolway queue list --json`.
- Expect `blocked`, not `on_fail`, after three failed starts of an agent lane. Only a command step
  whose own process will not start still takes `on_fail`.
- Check unattended runs for tasks on `paused`. A command step that fails again after three
  unblocker passes now parks there. A `gate: true` command step runs again at once after the
  unblocker passes it, so have the unblocker `--pause` if a person must look first. Check tasks
  0.9.0 left on `paused` at such a gate before resuming them, because they now resume onto the
  command, not its `on_pass`.
- Allow for a task held on `done`. A `background: true` command step with `on_fail` now holds its
  task, its `done` hook and its dependents until the check ends, up to its `timeout:`. A task whose
  worktree cannot be removed also stays on `done`, and its message names the
  `git worktree remove --force --force <path>` that clears it. Leave the new `removing/` folder in
  the project home alone.
- Start long-lived services outside spoolway. Background processes a blocking `run:` line starts
  are now stopped at the step's next arrival, at cleanup and by `queue unqueue --force`. Keep
  reading only the first line of `commands/*.pid`; it gains a `started` line.
- Update scripts that read `spoolway eval` or the ledger. `--json` gains a `commands` key.
  `usage.jsonl` holds command step lines with a `command` object and no model, tokens or cost, so
  skip them when you read lanes. Under `--json` or `--csv`, an empty result is an empty `rows` or a
  header and total row, not `Nothing to compare in <scope> yet.` The no-match message for `--step`
  now reads ``no lane or command step ran step `<step>` in that window``.
- Run `spoolway models` if a `[models]` glob repeats its literal in a model name, such as `*-5`.
  Such a glob now matches `claude-sonnet-5-5` as well as `claude-opus-5`, so more models take its
  prices, window, `compact_ctx` and `slots`.
- Make `queued` and `started` tracking hooks safe to run twice. A task queued again after
  `spoolway queue unqueue` fires them again.
- Fix or move every task file that does not parse out of `queue/` and routine folders.
  `spoolway dispatch` no longer stops on a queue of such files, and a routine folder that holds one,
  a `README.md` with no frontmatter included, is refused whole.
- Clear the new `spoolway doctor` failures. Switch a headless project back with
  `spoolway config set dispatch.backend herdr`. Correct or delete an `[agents.<name>]` whose `kind`
  spoolway cannot launch; it is no longer dropped on save, and `pipeline check` refuses its steps.
  Correct an out-of-range or reversed `session_reuse_ctx` and `session_blocked_ctx`, or an invalid
  `permission_mode`, in the tracked `config.toml`; `spoolway config edit` also exits non-zero over
  them.
- Set `unattended.max_cost_usd` to `0`, not a negative number, for no limit. `config set` refuses a
  negative, `inf` or `NaN` rate or limit.
- Replace a symlinked `.spoolway/config.toml` with a copy if its settings should stay with one
  project. `config set`, `sync` and `init --force` now write into the file the link points at.
- Fetch slow remote-only bases yourself. `git fetch origin <base>` during a worktree cut is
  stopped after 10 seconds. Run it in the checkout, then resume the task.

## 0.8.0 to 0.9.0

Upgrade every machine and CI job that reads the project first: once anyone sets
`models.<glob>.compact_ctx` or writes TOML 1.1 syntax such as a multi-line inline table, a 0.8.0
binary refuses `config.toml`. Then run `spoolway sync` in each project. Nothing reminds you to any
more: the `Run spoolway sync to apply the last update.` line and the update popups are gone, and
`spoolway update` ends with `Run spoolway sync in each project to apply the update.` Read the notes
`spoolway doctor` prints, work through the list, and restart the dispatcher once.

- Fix every unknown key in a `[jobs.<name>]` table. A job table may hold only `schedule`,
  `pipeline`, `routine` and `enabled`, and a table with any other key never fires, where 0.8.0
  dropped the key and fired the job. The dispatcher, `spoolway jobs list` and `spoolway doctor`
  name it with ``job `<name>` has unknown key `<key>` in <file> — it will not fire until it is
  removed or corrected``. A misspelt `enable = false` meant to pause a job now stops it.
- Rename or delete a leftover `task/<id>` branch before you queue a task with that id. 0.9.0 never
  runs a new task on an existing branch, and blocks it with ``<id>: the branch `task/<id>` already
  exists and this task has no checkout on record.`` The message names both fixes:

  ```
  git branch -m task/<id> <name>    # keep it, then set starts_from: <name> in the task
  git branch -D task/<id>           # drop it
  ```

  This also covers a task sent again after `spoolway queue unqueue --force` kept its branch.
- Delete `~/.spoolway/<label>-<id>/sync-stamp`. Nothing reads it now.
- Expect `spoolway doctor` to exit non-zero while a task hook has failed. Hook failures left the
  board and are `FAIL` rows labelled `issue_tracking hooks` in `doctor` and in the popup before
  dispatching. Resume the task to clear a `queued`, `started` or `done` hook; the others clear once
  the task is archived.
- Review `unattended.max_cost_usd` and `session_blocked_ctx` for every model a `[models]` row
  names with only some fields, such as `prompt_cache_ttl = "1h"` on `claude-*` or a `slots`-only
  row. Such a row now takes its rates and window from the price table, where 0.8.0 read them as 0,
  so its cost is real and the session size limits apply to it.
- Write one `[models]` row per model where you want separate `slots` caps. `slots` now caps every
  model name that resolves to one row together, so `[models."claude-*"] slots = 2` allows two lanes
  across every matching model.
- Expect higher recorded costs for the same work. A lane's line now includes its subagents, and a
  claude session is banked at no less than Claude Code's own total. Do not compare costs across the
  upgrade, and raise `unattended.max_cost_usd` if it now trips too soon.
- Read `spoolway override list`. A `blocked.<key>` pipeline override that 0.8.0 reported active
  but never applied now takes effect, so an unblocker may switch model. Drop what you do not want
  with `spoolway override drop <pipeline>`.
- Expect a config edit to reach a running dispatcher on its next pass. `dispatch.backend`,
  `unattended.enabled` and the `unattended.blocked_*` keys still need a restart, and pipeline files
  and pipeline overrides still load only when the dispatcher starts. Update any script that matched
  `active on the next dispatcher pass.` in the output of `spoolway pipeline override`; it now reads
  `a running dispatcher uses it once restarted, as with an edit to the pipeline file.`
- Read the notes about unknown keys. A misspelt key that 0.8.0 refused now loads with a note and
  does nothing.
- Expect unblocker lanes in unattended runs. A lane that ends without reporting, holds a child past
  `lane_child_ceiling` or crosses `session_blocked_ctx` now sends its task to `blocked`, where
  0.8.0 parked it on `paused`. Attended runs still park on `paused`.
- Expect a dependent to wait until its dependency is archived. A dependency held on `blocked` or
  `paused` by a failed `done` hook or cleanup now holds every dependent until a person resumes it.
- Raise `loop:` on a step a task walks past by `skip:`, `first:` or `last:`, if it should allow more
  walk-pasts. Each walk-past now spends one arrival there, so a task may now block on it.
- Do not count on a missed job window firing. A window that passed while no dispatcher ran, or
  while the job was disabled, is skipped.
- Make `queued`, `started` and `done` tracking hooks safe to run twice. A run killed without an
  exit code is now run again, up to three times in a row.
- Keep hook slugs to 64 lowercase letters, digits and hyphens. A longer `slug=` is ignored with a
  note.
- Read only the first line of `commands/*.pid` in scripts. The second line is the process start
  time.
- Shorten a project path, task id or `$EDITOR` that makes a pane command longer than 512 bytes.
  spoolway refuses it with ``refusing to type <n> bytes into pane <pane>; a pane takes 512
  intact``.
- Set `context_window` for every `openrouter/` model a step uses. The bundled price table no longer
  has them, because litellm removed them. Without a window, a `session: true` step opens a fresh
  session every time and `session_blocked_ctx` never stops it. Add rates too if the agent reports
  no cost of its own:

  ```
  spoolway config set models.'openrouter/anthropic/claude-opus-5.5'.context_window 1000000
  ```

## 0.7.1 to 0.8.0

Run `spoolway sync` (or `spoolway sync --dry-run` to read what it would do), then work through the
list. `spoolway sync` drops `dispatch.herdr_mode` and `models.<glob>.exclusive`. `spoolway sync`
and `spoolway pipeline check` both name each pipeline file and step that still uses a shape below.
Neither rewrites a step for you. Restart the dispatcher once you are done, because a running
dispatcher keeps routing on the pipelines it loaded when it started.

- Replace `end: true` with `on_pass: done`. A step with `end: true` stranded its task, and its
  dependents waited forever. 0.8.0 refuses the pipeline with ``step `X` sets end: true, which is
  gone — use on_pass: done``. Delete the ending step, and point the `on_pass:` that reached it at
  `done`.
- Replace `on_pass: blocked` with `gate: on_pass`. In an unattended run the first stopped a task
  with no lane to pick it up. The pipeline is refused with ``step `X` has on_pass: blocked — use
  gate: on_pass to stop for a person``. `gate: on_pass` holds the step's pass on `paused` and
  keeps where it was headed, and `spoolway resume` sends it there. Write the `on_pass:` the step
  should take once a person resumes it.
- A private pipeline under `~/.spoolway/<label>-<id>/local/pipelines/` is outside sync's reach, so
  look there yourself.
- Check how long your sessions may be continued. `models.<glob>.session_reuse_idle` and
  `cache_ttl` are now `prompt_cache_ttl`; both old names still load and are rewritten on the next
  save, and `spoolway doctor` notes them. A model not marked `local = true`, including one with no
  `[models]` entry, now opens a fresh session five minutes after the last reply, where 0.7.1 had
  no limit unless you set one. The same check now applies when `spoolway resume` continues a task
  from `blocked` or a park. Set `prompt_cache_ttl = "1h"` on `claude-*` if your lanes write the
  one-hour cache, or `"0"` to turn the limit off:

  ```
  spoolway config set models.'claude-*'.prompt_cache_ttl 1h
  ```

- Check the new session size limits. Every profile now defaults to `session_reuse_ctx = 20` and
  `session_blocked_ctx = 40`, where 0.7.1 had both off. A `session: true` step opens a fresh
  session when the earlier one passed 20% of the model's `context_window`, or when neither
  `[models]` nor the price table gives the model a window. A running lane past 40% is stopped and
  its task blocked. Set `context_window` under `[models."<glob>"]` for a model the price table does
  not know, such as a local one, or set either key to `0` on the profile to keep the 0.7.1
  behaviour:

  ```
  spoolway config set agents.claude.session_blocked_ctx 0
  ```

- Use `r` on the board where you used `R`. `R` does nothing now. `r` opens a picker of the task's
  steps with the next one preselected, so `r` `enter` resumes as before, and any other step
  reroutes the task there.
- Nothing to edit for `dispatch.herdr_mode`. Every task now runs in a herdr workspace of its own,
  the key loads with a note until `spoolway sync` drops it, and a task whose pane still sits in the
  shared `spoolway-dispatcher` workspace moves on its next step. Once no 0.7.1 dispatcher is
  running, remove the folder nothing reads any more: `rm -rf ~/.spoolway/.dispatcher`.
- Size `slots` and `concurrency` yourself if you relied on `models.<glob>.exclusive`. It does
  nothing now, and `spoolway sync` drops it without a note.
- Expect one idle agent process per agent step a task has run. Each step keeps its pane until the
  task is done. To have each new agent step close the last one's pane instead, run
  `spoolway config set dispatch.keep_finished_lanes false`.
- Re-check `loop:` on steps a task reaches by a resume, a cleared block, a command step's exit or a
  walk-past. `loop:` now counts every move onto a step, not only a lane's report, so a task 0.7.1
  let return more often may now block with the step and the count named. Raise that `loop:` if it
  should.
- Expect a command step to run its command on every arrival. A run killed three times in a row now
  blocks the task and names the run's log.
- Restart the dispatcher after you edit a pipeline. While it runs, `spoolway report`,
  `spoolway resume` and `spoolway queue add` use the pipelines it loaded when it started, and
  `queue add` refuses a task whose pipeline it did not load. The dispatch tab footer and
  `spoolway pipeline check` name each pipeline file changed since it started.
- Handle exit 1 from `spoolway resume <task> --stage <step>` when the step is one the task walks
  past by `skip:`, `first:` or `last:`. It moves nothing and names the rule, for example
  ``` `suite` does not run for `fix-cart-rounding`: it is not last in its chain. nothing was resumed. ```
- Move a `.spoolway/` that sits below the top of the repository up to the top, or remove it. 0.8.0
  refuses it with ``<folder> is not at the top of the repo.`` A linked worktree on a branch with no
  `.spoolway/` now reads the main checkout's setup, and `spoolway sync` refuses to write a setup
  into it.
- Run the command a refusal names if a checkout has lost its clone id. 0.8.0 never falls back to a
  home named for the folder.
- Handle a refusal from `spoolway config set` in scripts. It now runs `spoolway doctor`'s checks:
  a value that can never be right, such as an unknown `dispatch.backend`, is refused with the
  allowed values, and a value naming something not set up yet is saved with a warning.
- Update any script that reads the rows `spoolway queue add --from` prints. They say `group issue`
  and `task issue` where 0.7.1 wrote `epic` and `ticket`.
- Update tooling that reads `spoolway eval` by column position. The lanes and directory tables add
  `IN USD`, `OUT USD`, `CACHE R USD` and `CACHE W USD`, and `--csv` and `--json` add `in_usd`,
  `out_usd`, `cache_read_usd` and `cache_write_usd` with their `_per_run` twins. `CTX PEAK` now
  reads the highest share of a lane's own window. Do not compare a tiered model's `USD` across the
  upgrade: a lane with turns over the model's price threshold now pays the higher rate for them.
- Expect `spoolway models refresh` to refuse a table that keeps fewer than half the rows of the one
  it replaces, and to drop rows with out-of-range rates on a `refused` line. A script that
  refreshes from a small hand-made table through `SPOOLWAY_MODEL_PRICES_URL` must handle this.

## 0.7.0 to 0.7.1

Run `spoolway sync` (or `spoolway sync --dry-run` to read what it would do), then work through the
list. Nothing here changes a config default, and a project exactly as 0.7.0 shipped it needs no
edit. `spoolway pipeline check` names any pipeline that no longer loads.

- Delete every `loop: 0` line from your pipelines. 0.7.0 read it as no limit; 0.7.1 refuses the
  pipeline with ``step `X` has loop: 0 — a loop is 1 or more; delete `loop:` for no limit``. A step
  with no `loop:` has no limit, and `loop: 1` is not the same because it blocks on the second
  arrival. `spoolway sync` refuses a tracked pipeline file that carries `loop: 0` and keeps the
  project behind until you fix it. A private pipeline under
  `~/.spoolway/<label>-<id>/local/pipelines/` is outside sync's reach, so look there yourself.
- Drop `spoolway jobs run` from scripts. It exits 2, and a job now only fires on its schedule. To
  queue a routine now, open bare `spoolway`, tick the routine on the routines tab and press enter.
  `spoolway sync` removes the matching line from each installed `spoolway-config` skill.
- Put a working step first in any pipeline whose first step is `blocked`. 0.7.1 refuses it with
  the message ``pipeline `default`: `blocked` is the first step — a task would start blocked``.
- Route or delete any step `spoolway pipeline check` reports as ``reached by no route``. It is a
  warning, and the command still exits 0.
- Check the `gate_at:` of every task already queued. `queue add` and `task contract` now refuse a
  `gate_at:` that names no step, but a task queued by 0.7.0 is not flagged and runs with its
  checkpoint dropped. Compare each `gate_at:` in `~/.spoolway/<label>-<id>/queue/*.md` with
  `spoolway queue route <id>`, then run `spoolway queue unqueue <id>` and add the task again with
  the right step.
- Set `HOME` in every CI job and wrapper script that runs spoolway. An unset or empty `HOME` now
  exits 1 with `spoolway: HOME is not set (or is empty)`, where 0.7.0 looked under
  `/tmp/.spoolway`. New project labels are cut to 64 characters; existing labels are kept.
- Handle exit 1 in any script that runs `spoolway resume <id>`, with or without `--stage`, on a
  queued or running task. Use `spoolway queue pause <task>` to stop a running task first.
  `--stage` also refuses while a `depends_on` task is not done, and
  `spoolway queue unqueue <id> --force` refuses a started task that a queued task depends on.
- Rust projects: once every task cut before the upgrade has finished, run
  `rm -rf ~/.spoolway/<label>-<id>/.cargo-target`. Each worktree now builds into its own
  `target/`, and spoolway never removes the old folder. Deleting it earlier makes cargo fail with
  `File exists (os error 17)` in an older worktree until you also remove that worktree's
  `target/debug` link.
- A task whose `gate_at:` names a command step, such as `handover`, now pauses when that command
  exits, where 0.7.0 ignored it. After the upgrade, run `spoolway resume <id>` for each such task
  that pauses; a held pass goes down `on_pass` and a held failing exit goes down `on_fail`.
- `gate: true` on a foreground command step now loads and holds a passing exit; 0.7.0 refused it.
  Nothing a 0.7.0 project can contain needs an edit. If a new pipeline puts `gate: true` on a
  `background: true` step or on a step that ends the task, delete that `gate:` line, as the
  message says.
- Re-check any `loop:` you set to stop a task that kept returning from `blocked`. Every way out of
  `blocked` now resets every step's arrival count, so the same `loop: n` allows more arrivals.
  Lower `n` if you relied on the old stop.
- Do not compare `BLOCKS` in `spoolway eval` across the upgrade. A lane whose `--pass` or `--fail`
  a spent loop sent to `blocked` is now counted, in every `--by`, `--csv` and `--json`, and
  ledger lines already written are unchanged.
- Remove a workspace folder yourself once nothing in it is needed: run
  `rm -rf ~/.spoolway/<name>` for the folder that `spoolway doctor` reports as ``lists no
  checkout``. No command deletes it for you any more, and the last checkout leaving prints its
  path.
- `spoolway init --force` now keeps the project's provider, tracker and project key. It still
  resets models and config values, so set again anything you had changed with
  `spoolway config set <key> <value>`, for example
  `spoolway config set models.'<model-glob>'.input <usd per 1M>`. `spoolway init --setup home`
  refuses while the repo-mode home holds queued tasks; finish or unqueue them first.
- Expect `.bak.1`, `.bak.2` and so on from `spoolway sync --replace <file>`. It never overwrites
  an earlier `.bak`, prints the path it wrote and keeps the file's mode.
- Update any script that parses the `spoolway sync` report. Refused files are listed first, and
  the report adds `(migrated: … replaced; edits are not kept)` and
  `(set issue_tracking.key_in_names = true, the default)` lines.
- No edit is needed for `spoolway config set`, `spoolway init --tracker` and
  `spoolway override promote`; they now work on a 0.6.0 config that has not been synced, including
  retired keys inside inline tables.
- Check any step whose `run:` line relied on `exec` hiding its exit code. A `run:` line now runs in
  a child `sh -c`, so `exec` records its exit code, and a command that a late background failure
  pulls its task off is stopped.
- Read the step name from `spoolway queue list --json` again in any tooling that parses it. A
  blocked row's `[r]` and the JSON now name the step `spoolway resume` really goes to, and a task
  on a stage its pipeline lacks reads as `unknown` rather than `blocked`.
- No edit is needed for a step with `skills:`. Each skill is now sent as its own message, then the
  briefing once.

## 0.6.x to 0.7.x

Upgrade every machine and CI job that reads the project first: once `sync` writes
`housekeeping.archive_retention_days`, a 0.6.x binary refuses `config.toml` with `unknown field`.
Then run `spoolway sync` (or `spoolway sync --dry-run` to read what it would do) and work through
the list. Nothing applies an update by itself. Run `spoolway sync` in each project.

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
  it saves. A second replace of the same file saves to `.bak.1`, then `.bak.2`, and never
  overwrites an earlier backup. The 0.6.0 hook still opens issues but logs `cat: '': No such file or directory` and
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
  directory, and, when a queued task still has a worktree there, names that task too. This holds
  for a `~/` path and for a key set only in the private overrides layer. `spoolway doctor` names
  the same tasks for a key in `config.toml`.
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
  from any script that calls `spoolway dispatch`, and match exit 4 (`dispatcher already running`),
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
