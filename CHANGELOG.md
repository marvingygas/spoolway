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

## 0.7.1

### Highlights
- A pipeline that could never run is now refused when it loads, with the edit that fixes it, instead of failing later on the board: a first step of `blocked`, a task's `gate_at:` that names no step, and `loop: 0` are all refused, a step no route reaches gets a warning, and `spoolway sync` stops reporting success over a pipeline file that will not load. See the migration bullets below for `loop: 0` and `blocked`. (#689, #703)
- A command step can now hold a task for a person: `gate: true` on a foreground command step pauses the task when it exits cleanly, a task's `gate_at:` may name a command step, and `spoolway resume` sends a held pass down `on_pass` and a held failing exit down `on_fail`. `gate: true` is refused, with the edit named, on a background step and on a step that ends the task. (#705, #706)
- `spoolway resume` and `spoolway report` no longer let a task be pushed somewhere it cannot go: resuming a queued or running task is refused with the command that does what you meant, `--stage` is refused while a `depends_on` task is not done, a blocked row's `[r]` names the step resume really goes to, and leaving `blocked` by any road now resets every step's `loop:` count so a spent loop no longer parks the task again. (#687, #690, #700)
- Setting up and syncing a project loses fewer ways to damage your files: every new clone gets its own home under a real `~/.spoolway`, no command deletes a workspace folder any more, `spoolway init --force` keeps your provider and tracker, `spoolway sync --replace` never overwrites an earlier `.bak`, and the sync report lists the files it refused and the values it set itself. (#691, #692, #693, #695, #696)
- Two tasks building the same Rust crate no longer overwrite each other's debug test binaries, because each worktree now builds into its own `target/` instead of a shared `target/debug` link; and a `run:` line now runs in a child `sh -c`, so a line that ends in `exec` records its exit code and a command a late background failure pulls its task off is stopped. (#684, #697)

### Breaking changes and migration
- `loop: 0`, which 0.7.0 read as no limit, is refused: the pipeline does not load, and every command says ``step `X` has loop: 0 — a loop is 1 or more; delete `loop:` for no limit``. Delete the `loop:` line from that step; a step with no `loop:` has no limit, as before, and `loop: 1` is not the same because it blocks on the second arrival. `spoolway sync` refuses a tracked pipeline file that carries it, writes nothing to the file and keeps the project behind until you fix it; a private pipeline under `~/.spoolway/<label>-<id>/local/pipelines/` is outside sync's reach, so sync reports success there while every other command names the file and the edit. (#689, #703)
- `spoolway jobs run` is removed and exits 2 with ``error: `spoolway jobs run` is gone. A job only fires on its schedule; to queue its routine now, open bare `spoolway`'s routines tab, tick the routine and press enter.`` Drop it from scripts; the `r` key on the jobs tab does nothing now. `spoolway sync` removes the matching line from each installed `spoolway-config` skill. (#699)
- A pipeline whose first step is `blocked` is refused with ``pipeline `default`: `blocked` is the first step — a task would start blocked; put a working step first``. Put a working step first. (#689)
- A step that no route reaches now prints ``warning: pipeline default: step `orphan` is reached by no route`` and the command still exits 0. Route the step or delete it. (#689)
- `spoolway queue add` and `spoolway task contract` refuse a task whose `gate_at:` names no step of its pipeline, listing the steps it may name, and exit 1. A task already queued by 0.7.0 with such a `gate_at:` is not flagged by `queue list`, `queue route` or `doctor`, and still runs with the checkpoint dropped. Check each `gate_at:` in `~/.spoolway/<label>-<id>/queue/*.md` against `spoolway queue route <id>`, then unqueue and re-add any that is wrong; `task contract --from` refuses queue files, so it cannot do this check. (#689)
- An unset or empty `HOME` is refused with ``spoolway: HOME is not set (or is empty); spoolway keeps its state under ~/.spoolway. Set HOME to your home directory and run it again`` and exit 1, where 0.7.0 looked under `/tmp/.spoolway`. Set `HOME` in any CI job that runs spoolway. `spoolway init` in a symlinked home folder now refuses as it does in a plain one, new project labels are cut to 64 characters while existing ones are kept, and `spoolway sync` refuses a copied checkout. (#691)
- `spoolway resume <id>`, with or without `--stage`, exits 1 on a queued task (``task `X` is queued, not stopped — it starts on its own when the dispatcher reaches it, so there is nothing to resume.``) and on a running task, where the message names `spoolway queue pause <task>`. `--stage` is also refused while a `depends_on` task is not done, a lane on `blocked` cannot resume its own task, and `spoolway queue unqueue --force` refuses a started task that a queued task depends on. A script that resumed any of these must now handle exit 1. (#687)
- Rust projects with a root `Cargo.toml`: 0.7.0 linked every worktree's `target/debug` into `~/.spoolway/<label>-<id>/.cargo-target/debug`. Each worktree now builds into its own `target/`, and `.cargo-target` is never removed for you, so disk use grows per worktree. Once every task cut before the upgrade has finished, run `rm -rf ~/.spoolway/<label>-<id>/.cargo-target`; deleting it earlier makes cargo fail with `File exists (os error 17)` in those worktrees until you also remove their `target/debug` link. (#697)
- A task whose `gate_at:` names a command step, for example `handover`, now pauses when that command exits, pass or fail; 0.7.0 ignored it. A task queued by 0.7.0 with such a `gate_at:` therefore pauses after the upgrade. Resume it with `spoolway resume <id>`, which sends a held pass down `on_pass` and a held failing exit down `on_fail`. (#705)
- `gate: true` on a foreground command step, which 0.7.0 refused, now loads and holds a passing exit on `paused`. Nothing a 0.7.0 project could contain changes meaning, so no edit is needed; it is refused on a `background: true` step and on a step that ends the task, and the message names the line to delete. (#705, #706)
- Leaving `blocked` by any road, whether an unblocker's report, a person's resume or the board's, now resets every step's `loop:` arrival count to zero, and an unblocker's `--pass` after a spent loop goes on to the step instead of parking on `paused`. The same `loop: n` therefore allows more arrivals than it did on 0.7.0; lower it if you relied on the old stop. (#700)
- `spoolway eval` counts a lane whose `--pass` or `--fail` a spent loop sent to `blocked` in the `BLOCKS` column, in every `--by`, `--csv` and `--json`. Ledger lines already written are unchanged, so `BLOCKS` before and after the upgrade cannot be compared. (#698)
- No command deletes a workspace folder any more. When the last checkout leaves a workspace, spoolway prints ``Workspace `<name>` lists no checkout now. Its folder is kept: ~/.spoolway/<name>`` and `spoolway doctor` notes it, so remove the folder yourself once nothing in it is needed. A bad `dispatcher` value now stops only that workspace's checkouts, and two clone entries naming one dispatcher folder, or joining another repository's workspace, are refused. (#692)
- `spoolway init --force` keeps the project's provider, tracker and project key, where 0.7.0 switched to `claude` and blanked the tracker; it still resets models and config values. `spoolway init --setup home` refuses while the repo-mode home still holds queued tasks. (#693)
- `spoolway sync --replace <file>` never overwrites a backup: it writes `<file>.bak.1`, then `.bak.2`, prints the path and keeps the file's mode. (#696)
- The `spoolway sync` report lists refused files first and adds `(migrated: … replaced; edits are not kept)` and `(set issue_tracking.key_in_names = true, the default)` lines. A missing or unreadable stamp now counts as behind, so the `Run spoolway sync to apply the last update.` notice shows until `sync` has run. A script that parses the report should expect the new lines. (#695)
- `spoolway config set`, `spoolway init --tracker` and `spoolway override promote` now work on a 0.6.0 config that has not been synced, including past keys 0.7 retired inside inline tables. No action is needed. (#694)
- A `run:` line now runs in a child `sh -c`, so `exec` records its exit code, and a command stopped by a late background failure is stopped on the step that failure pulls the task off. Check any step whose `run:` line relied on `exec` hiding its exit code. (#684)
- A blocked row's `[r]` and `spoolway queue list --json` now name the step `spoolway resume` really goes to, and a task on a stage its pipeline lacks reads as `unknown` rather than `blocked`. Tooling that reads the `--json` step name or state should expect this. (#690)
- A step with `skills:` now sends each skill as its own message and then the briefing once, rather than the briefing once per skill. No action is needed. (#704)

### Features
- The dispatch tab checks the pipelines on disk before it dispatches and shows a load error in a popup, instead of using the copy it took at startup. (#686)
- `spoolway init` detects the repository's default branch, refuses with a message that names the fix when a repo-mode setup cannot create `.spoolway/`, and stamps only the files it wrote. (#693, #695)
- `spoolway sync` and `spoolway doctor` find a queued task still working under a `~/` `dispatch.worktree_root`, including one set only in the private overrides layer, and name it. (#696)
- Resuming a held failing command exit takes the step's `on_fail`, and the pipeline check warns ``handover gates but declares no `on_fail`, so a failing exit there parks the task on `blocked` `` when a gated command step has none. (#705, #706)

### Fixes
- A blank folder name now makes the project label `project`, a lost project stamp is refused with a message that says what to do, and a bad workspace file no longer stops every checkout, only its own. (#691, #692)
- The dispatcher deletes a half-written queue file instead of listing it as a task, and a `spoolway report` from a queued or paused task is refused. (#690)
- The sync notice stays while a pipeline file is refused or the stamp is missing, rather than clearing after one run. (#695)

### Upgrading
- Install or update with `npm install -g spoolway@0.7.1`, or run it without installing via `npx spoolway@0.7.1`.
- The `spoolway` wrapper package selects one of five platform packages at install time: linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, darwin-arm64 and darwin-x64; the GitHub release carries one archive per platform and `SHA256SUMS`.
- After upgrading, run `spoolway sync` to refresh installed skills, then work through the breaking changes above; `spoolway doctor` lists what is still behind, and `spoolway pipeline check` names any pipeline that no longer loads. The 0.7.0 to 0.7.1 section of `docs/migrations.md` gives the same steps.
- Run `spoolway whats-new` to read this record back from the installed binary.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.7.1

## 0.7.0

### Highlights
- Everything you do day to day now lives behind one command: bare `spoolway` opens a single screen with `queue`, `routines`, `jobs`, `eval` and `dispatch` tabs, and the `dispatch` tab starts and stops the dispatcher, with `enter` on a running dispatcher asking whether to let steps finish or interrupt them. `spoolway dispatch`, `queue`, `jobs` and `eval` become plain commands, and only one `spoolway` runs per project at a time. (#437, #438, #440, #443, #480, #546)
- A project's setup no longer has to be committed to the repository: `spoolway init` asks whether it lives in the repo or in a shared workspace under `~/.spoolway/`, so several clones can share one setup, and `spoolway pipeline copy`, `spoolway prompt copy` and `spoolway pipeline promote` let you try a pipeline or prompt privately under `local/` before moving it into `.spoolway/`. (#569, #576, #577, #578, #625)
- Issue tracking is rebuilt around the ticket: the shipped `jira.sh` opens one Story per group with a Sub-task per task, tasks can carry `labels:` that both hooks apply, a new `started` hook event fires once a task leaves `queued`, and a failing hook now always pauses its task instead of being recorded and ignored. The hooks are your own files, so existing projects must refresh them; see the migration bullets below. (#549, #550, #552, #553, #622, #623)
- `spoolway eval` shows what a run cost at a glance: row totals (`IN`, `OUT`, `CACHE R`, `CACHE W`, `USD`, `TIME`) and a pinned `Total` line are now the default, any column sorts with `--sort <column>[:asc|:desc]`, sessions you run by hand inside a project's worktrees are counted, and the eval tab gains a trials table for the group trials the queue screen queues, one copy of the group per ticked pipeline. (#520, #612, #613, #617, #665, #669)
- A task says plainly where it starts and where it lands: `starts_from:` (renamed from `cut_from:`) can be set by hand, a start branch that exists nowhere pauses the task or refuses the batch with the one line that fixes it, and the new `spoolway queue route <task>` prints the pipeline in step order with where `pass` and `fail` lead and where resuming sends the task. (#668, #671, #674)

### Breaking changes and migration
- Jira-tracked projects: the 0.6.0 `.spoolway/hooks/jira.sh` stops working. `spoolway sync` does not refresh hook scripts and the binary no longer sets `SPOOLWAY_EPIC_BODY` or `SPOOLWAY_TICKET_BODY`, so every `spoolway queue add` ends with `ticket FAILED — <group>` and `the open hook exited 1`, after the Story was already created, leaving one orphan Story per attempt. Run `spoolway sync --replace .spoolway/hooks/jira.sh` (your old copy is saved beside it with a `.bak` suffix; re-apply local edits from it), delete any orphan Stories, and upgrade `acli` to 1.3.39 or newer; the hook now also needs `gh` 2.97.0 or newer, `jq` 1.6 or newer and bash 3.2 or newer. (#553, #570, #622, #623, #650)
- GitHub-tracked projects: the 0.6.0 `.spoolway/hooks/github.sh` still opens issues, but it logs `cat: '': No such file or directory` and drops the `Mirrors task …` lines from issue bodies. Run `spoolway sync --replace .spoolway/hooks/github.sh` and re-apply local edits from the `.bak`. The new hook marks an issue in progress on `started` instead of `queued`, applies `labels:`, and on `done` comments `Ready for review in <PR URL>` on the issue instead of posting a marker on the pull request. (#550, #552, #622, #623, #650)
- `spoolway init` no longer writes `.github/workflows/spoolway-issues.yml`, and once `github.sh` is replaced nothing posts the `<!-- spoolway-issue: URL -->` marker that workflow reads, so a copy left from 0.6.0 silently stops closing issues. Delete `.github/workflows/spoolway-issues.yml` and wire closing on merge as `docs/configuration.md` describes under "Closing tickets on merge". (#567, #623, #630)
- `issue_tracking.key_in_names` now defaults to `true`, so the tracker's key leads group, branch and worktree names (`task/gh-123-…`). A project set up by 0.6.0 keeps the `key_in_names = false` that `init` wrote and behaves as before; run `spoolway config set issue_tracking.key_in_names true` to adopt it, which merge automation that reads the key from the branch name needs. (#623)
- `issue_tracking.on_fail` is retired: a failing `queued`, `started` or `done` hook now always pauses its task, where 0.6.0's default `on_fail = ""` only recorded the failure and carried on. Every 0.6.0 config carries the key, so every command prints a `note:` until `spoolway sync` drops it and moves `[issue_tracking]` under `[watch]`; make a hook exit 0 for failures that should not stop work, queue the task with `tracking: off`, and run `spoolway resume` to re-run a hook that paused a task. (#541, #549, #632)
- `dispatch.worktree_root` is retired: every worktree now lands under `~/.spoolway/<label>-<id>/worktrees`, and `spoolway config get dispatch.worktree_root` and `config set` fail as an unknown key. `spoolway sync` drops the key and, when it was set, names the old directory and any queued task still working there; remove old worktrees yourself once no queued task uses them. (#608, #632)
- `.spoolway/templates/tracking/epic.md` and `ticket.md` are retired: `spoolway sync` deletes both whatever they hold, printing `(the issue body is the hook's own)`, and the empty `templates/tracking/` folder stays. Copy any wording you kept there into your `open` hook before running `sync`. (#650, #652)
- Finished tasks in `archive/` are no longer pruned by `housekeeping.retention_days`; they are kept forever (about 25 KB each) until you set the new `housekeeping.archive_retention_days`, which `sync` adds as `0`. To keep 0.6.0's behaviour run `spoolway config set housekeeping.archive_retention_days 30`; headless lane logs move to `headless/logs/` on their own. (#664)
- Once `sync` writes `archive_retention_days`, a 0.6.x binary refuses the project's `config.toml` with `unknown field`. Upgrade every machine and CI job that reads the project before you commit what `sync` wrote. (#664)
- `spoolway dispatch --plain` and `--force` are removed, and so is exit code 5. `spoolway dispatch` prints one line per pass and never draws the board; drop both flags, use bare `spoolway` for the board, and match exit 4 (`Dispatcher already running`), which now also means a bare `spoolway` screen holds the project. (#440, #443)
- `spoolway queue` and `spoolway jobs` with no subcommand print usage instead of opening a screen, and `spoolway eval` always prints its table; open bare `spoolway` for the screens. (#443)
- `spoolway init --take-over`, `--adopt` and `--new-id` are removed and exit 2 naming their replacement. Drop `--take-over`, which already did nothing; for `--adopt` run `spoolway init --workspace <name>` from the checkout that replaces it, otherwise `spoolway init`; for `--new-id` delete `.git/spoolway-id` and the old home's `~/.spoolway/<label>-<id>/project.toml`, then run `spoolway init` again (the old home's tasks stay on disk). `spoolway init --yes` still works in scripts. (#626, #678, #679)
- `spoolway init` writes `.spoolway/hooks/` only when a tracker is chosen (the default `--tracker none` writes none), and at a terminal it asks `Install the example setup?` (`--examples` or `--no-examples`) and no longer asks `Set up this project?`. A script that expects the hooks folder after a plain `init` must pass `--tracker github` or `--tracker jira`. (#567, #667)
- `spoolway queue conflicts` is removed with no replacement, and the task `touches:` key is no longer read or listed by `spoolway task contract` (a task that still sets it loads). Order tasks with `depends_on`. (#436)
- Task front matter `parallel:` is removed and `spoolway queue add` and `spoolway task contract` refuse it. Every group must be one chain, and a fan or a join is refused: drop `parallel:`, chain tasks with `depends_on`, and put independent work in a group of its own; a group's first task may `depends_on` another group's last task to stack. `spoolway stack` prints `conflicts` where it printed `siblings`. (#571)
- Task front matter `cut_from:` is renamed `starts_from:`; the old spelling is still read, so only tooling that reads task files needs to change. `spoolway queue add` now refuses a whole batch when a task's start branch exists nowhere, naming the task; set `starts_from:` on it and requeue. (#668, #674)
- `spoolway sync` migrates only from 0.6.0. A project never brought to 0.6 is not migrated any more: run `spoolway init --force`, or remove its `.spoolway/` and run `spoolway init`. (#629)
- Nothing applies updates implicitly any more: a command run in a checkout behind the binary prints `Run spoolway sync to apply the last update.` on stderr at a terminal and carries on, bare `spoolway` shows it in a popup, and an interactive `spoolway sync` lists its changes and waits for `enter`. `spoolway sync` and `sync --dry-run` now exit non-zero naming `config.toml` when it cannot be read or parsed, where 0.6.0 exited 0, so a CI check that ran `sync --dry-run` now fails on a broken config. (#482, #484, #601)
- `spoolway config path` no longer prints the one `config.toml` path; it prints `setup:`, `local:` and `overrides:` folder lines. Scripts should read `spoolway config path --json` and append `config.toml` to `setup`. (#579, #648)
- `spoolway eval` shows row totals by default, so a script scraping the old `IN/RUN`, `OUT/RUN`, `CACHE R/RUN`, `CACHE W/RUN`, `USD/RUN` and `TIME/RUN` columns must pass `--per-run` (or read `--csv` or `--json`, which carry both sets). A clean `spoolway doctor` now ends `N checks passed.` without `Everything checks out.`, and its live pane check runs only inside the herdr session; pass `spoolway doctor --live` to force it elsewhere. (#570, #611, #665)
- The shipped prompts and the `default` and `bugfix` task skeletons changed, but `sync` keeps your 0.6.0 copies and the old ones still load and run. To take the new ones run `spoolway sync --replace .spoolway/prompts/<name>/PROMPT.md` and `spoolway sync --replace .spoolway/templates/tasks/<name>.md`. Installed skills are always rewritten to the shipped copy, so keep edits to a shipped skill under a name of your own. (#503, #635, #651)
- If you ran `spoolway herdr bind` on 0.6.0, you still have `prefix+alt+d` running `spoolway dispatch` and `prefix+alt+q` running `spoolway queue`, and neither `sync` nor `doctor` mentions them. Run `spoolway herdr bind` once: it removes both and binds `prefix+alt+d` to bare `spoolway`, leaving `prefix+alt+q` free; or run `spoolway herdr unbind` to remove all four. (#443, #680)
- The built-in model price table was refreshed: 78 models added, including `claude-sonnet-5-5`, 1 removed, and 100 with changed prices or context windows, so `spoolway eval` costs and context-window session thresholds follow the new figures. Your own `[models.*]` entries still win. (#638)

### Features
- Tasks may carry `labels:`, a list of plain words that `queue add` refuses if one holds whitespace or a comma; every hook event receives them comma-joined in `SPOOLWAY_LABELS`. (#552)
- Declining issue creation from the queue screen or `jobs run` writes `tracking: off` into each task, which fires no issue-tracking hook event and never holds the task, and with tracking on the queue screen asks before opening tickets. (#442, #541)
- The routines tab lists one row per top-level folder under `.spoolway/routines/`, makes a job for the highlighted routine with `n`, and deletes a routine with its jobs with `x`; `spoolway jobs contract` prints both job store paths, every `[jobs.<name>]` key, the cron grammar and a sample table. (#544, #546, #559, #566, #647)
- `spoolway config path` also reports the setup `mode` (`repo` or `home`), the routines folder, both job stores and the workspaces, and now exits 0 in a checkout no project claims. (#648)
- `spoolway stack` prefixes a pull request title with a bare ticket key (`KAN-11 feat(...)`), folds the status log, handoff and blocker into one collapsed "Run history" section at the foot, and opens a pull request against the branch a merged base went into once the base branch is gone. (#474, #511, #553)
- A `base:` that exists only on `origin` is accepted by the queue screen, `spoolway queue add` and `spoolway task contract --from`. (#472)
- When a dispatcher pass claims several slots, the claimed lanes' agents boot concurrently instead of one after another, and a lane shows `◌ starting` until its agent is up. (#483, #486)
- The shipped `spoolway-plan`, `spoolway-tasks`, `spoolway-config` and `spoolway-calibrate` skills are rewritten: plans carry one Context section and a risk and mitigation table per decision, tasks link their plan steps and mockups instead of copying them, groups split by subject first and are always one chain, and `spoolway-calibrate` proposes fixes wherever they land and changes nothing until you pick rows. (#502, #531, #574, #621, #634, #644, #645, #655)
- Every lane's briefing now tells its agent to do what a person asks in its pane, to write that change into the task file for later steps, and to say where resuming sends the task. (#672)

### Fixes
- A stale override that names a missing pipeline, step, prompt or config key is skipped with one `spoolway: override ignored — …` line instead of making every command refuse to load, and `spoolway override list` and `spoolway doctor` mark it. (#470)
- An unblocker's `--pass` from `blocked` on a task stuck at a `gate: true` step now lands on `paused` at that gate instead of skipping it. (#471)
- A step with `skills:` now sends its lane `/a /b Read <task> before anything else.`, so Claude Code expands each skill as a command instead of receiving a pasted block. (#532)
- A task parked with Escape in a lane's pane, or with `p` on the board, now gets back to its step on `r` or `spoolway resume` and accepts its lane's `spoolway report`, instead of being parked again. (#640)
- On Linux, the dispatch tab can start the dispatcher again after the `spoolway` binary was replaced while the screen was open. (#587)
- `spoolway eval` counts a skill whether it was typed as `/name` or run through the Skill tool, and folds a subagent's tokens, cost and skills into its parent session. (#519)
- The shipped plan, tasks and calibrate skills take the project home from `spoolway task contract`'s `output.dir`, so plans land where lanes read them. (#557)
- `spoolway init` run from a linked worktree acts on the main checkout, or refuses naming `-C <main checkout>`, instead of setting up the worktree. (#598)
- `spoolway pipeline contract` and `spoolway prompt contract` work in a project with no pipelines, and `spoolway init` with no terminal and no `--yes` says nothing was written and names `--yes`. (#602)
- A repeat `spoolway init` keeps the project's existing `issue_tracking.project_key` and provider instead of blanking the key and falling back to `claude`, and `init --provider pi` is accepted. (#603)

### Reliability and performance
- The dispatch board, queue tab and routines tab read the project on a background thread and re-parse a task file only when its bytes changed, so keys draw without waiting on `git`, `herdr` or a full re-parse. (#639, #641, #646, #654)
- Every archived task gets one line in `archive/index.jsonl`, rebuilt automatically when missing or stale, and the board, queue tab and `queue add`'s `depends_on` check read it instead of parsing every archived file. (#660, #661)
- The eval tab locates and reads each session transcript once per load and rereads only transcripts whose size or modification time changed on a reload. (#592, #594)
- Every redrawing screen paints through one synchronized frame writer and bare `spoolway` draws on the terminal's alternate screen, so scrolling a herdr pane shows no stale frames and no frame flickers. (#517, #547)

### Platform and packaging
- `cargo install` on macOS no longer prints a dead-code warning, and CI now runs clippy for `aarch64-apple-darwin`. (#505)

### Upgrading
- Install or update with `npm install -g spoolway@0.7.0`, or run it without installing via `npx spoolway@0.7.0`.
- The `spoolway` wrapper package selects one of five platform packages at install time: linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, darwin-arm64 and darwin-x64; the GitHub release carries one archive per platform and `SHA256SUMS`.
- After upgrading, run `spoolway sync` to migrate `config.toml` and refresh installed skills, then work through the breaking changes above; `spoolway doctor` lists what is still behind. The 0.6.x to 0.7.x section of `docs/migrations.md` gives the same steps.
- Run `spoolway whats-new` to read this record back from the installed binary.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.7.0

## 0.6.0

### Highlights
- spoolway now runs its own releases end to end: queue `release-spoolway`, approve or override the recommended version at the one planned stop, and every other step — repair, review, merge, changelog, rehearsal, tag, publish and verification — runs unattended from there. This 0.6.0 release is the first one cut by that pipeline. (#336)
- Command steps can now be scoped with two new pipeline keys: `first: true` runs a step only on a chain's undeclared-dependency root, and `serial: true` lets only one task's run of that step go at a time, with the board and `spoolway queue list` showing a waiting task as `○ waiting … serial: after <task>`. (#370, #372, #391)
- Upgrading a project is now closer to automatic: `spoolway sync` (or just opening spoolway once a project falls behind) migrates every pipeline shape a release retires and names each change it made, and installed skills are rewritten whenever they differ from the shipped copy. (#404, #405)
- `spoolway eval` now groups by an explicit pipeline `version:` a person raises, rather than an automatic fingerprint: `--by version` lists every version newest first, and `--pipeline-version X.Y` filters to lanes that ran under one. (#392, #394)
- The dispatcher board's header now names the running dispatcher's own version next to its pid, and adds `(restart to use latest installed version)` once a newer `spoolway` binary sits on `PATH`. (#374)

### Breaking changes and migration
- `spoolway spend` is removed; running it now fails as an unknown command, with no retirement hint. Use `spoolway eval` instead: `--by task`, `--by group` and `--by step` cover the matching old `spend` cuts, but `--by model`, `--by project`, `--by month` and `--by lane` have no direct replacement — `--since`/`--until` still take a `YYYY-MM` month to bound the window, just not to group by it. (#395)
- `spoolway eval` drops `--runs`, `--limit` and `--month`. `--by` is now always applied (default `pipeline`), taking `group`, `task`, `pipeline`, `step` or `version`, and `--pipeline-version X.Y` is new. Replace `spoolway eval --runs` with `spoolway eval --by task`, and `spoolway eval --month 2026-08` with `spoolway eval --since 2026-08 --until 2026-08`. (#392, #394)
- A pipeline's `loop:` written as a map, keyed by the step a failure is sent back from, is now refused at parse, naming the step that should carry the limit instead; `on_loop_max:` is refused the same way, since a spent loop always parks on `blocked` now. Run `spoolway sync` to fold every map form into a bare `loop:` on the step it named — set to one plus the sum of its entries — and to delete every `on_loop_max:` key. (#352, #393)
- A step whose `on_fail:` names its own id is now refused by `spoolway pipeline check`, naming the step and the key, and the shipped pipelines' `checks` step is dropped entirely. Run `spoolway sync` to strip a self-routing `on_fail:` from an existing pipeline file; the step then waits on `blocked` for a person instead of retrying. (#337, #338)
- `/spoolway-doctor` is retired. Use `/spoolway-config` instead, which now diagnoses and repairs a project as well as changing it. (#405)

### Features
- Any command run against a checkout that has fallen behind now says `Open spoolway to apply them.` instead of naming `spoolway sync` directly, so the fix always routes through the board. (#407)
- Every provider's `spoolway-plan` skill now requires a clear decision on anything that would otherwise be left aside — a plan can no longer say something will be handled later, and "out of scope" is the person's own call. (#342)

### Reliability
- A group's banked totals on the board no longer double-count a step's arrivals against its own loop budget, and only count the slots the dispatcher actually enforces. (#333, #384)
- The usage ledger banks only the time a lane was actually busy, and counts each lane once. (#389)
- The shared Cargo target directory is now linked only into a Cargo worktree, not into every worktree spoolway creates. (#350)
- `spoolway sync` keeps a hook script it replaces executable, instead of dropping its permission bit. (#334)
- `spoolway sync` now prints `Nothing updating.` when a run writes or removes nothing, instead of claiming files were overwritten. (#335)
- A task id is no longer capped to fit inside Herdr's agent-name length; the full id is used everywhere, including in Herdr panes and tabs. (#369)
- Herdr tab and pane labels now name the task and the step running in them. (#371)
- The prompt contract check now reads the pipelines in the task's own checkout, not the project root's, so a worktree with its own pipeline files is checked against the right ones. (#408)

### Upgrading
- Install or update with `npm install -g spoolway@0.6.0`, or run it without installing via `npx spoolway@0.6.0`.
- The `spoolway` wrapper package selects one of five platform packages at install time: linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, darwin-arm64 and darwin-x64.
- After upgrading, open spoolway (or run `spoolway sync`) to migrate any retired pipeline shapes and refresh installed skills, per the breaking changes above.
- Run `spoolway whats-new` to read this record back from the installed binary.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.6.0

## 0.5.0

### Highlights
- GitHub issue tracking now closes itself: `spoolway init` installs `.github/workflows/spoolway-issues.yml`, and once a task's pull request merges, the workflow closes that task's issue and, once every task in a group has closed, the group's own epic too — the shipped hook itself only leaves a `<!-- spoolway-issue: URL -->` marker comment at `done`, since nothing has actually shipped before the merge. (#157, #194, #195, #199)
- A required tool below its version floor, or missing from `PATH`, now stops a ticket from opening instead of failing partway through it: `spoolway doctor` reads each hook script's own `# spoolway-requires: <tool> >= <version>` lines, and every submit route — the queue screen's `enter`, `queue add --from`, and a routine — checks the same lines and gates the submission on them. (#172, #176)
- spoolway is now installable as a herdr plugin: `herdr plugin install marvingygas/spoolway` downloads the matching release binary and falls back to a source build on any miss, and `spoolway herdr bind`/`unbind` write or remove its four keybindings straight into herdr's own `config.toml`. `init` run from a herdr popup now prints the project directory it resolved and waits for confirmation before writing anything. (#283, #284, #287)
- `spoolway sync` takes over the file-bringing-forward half of `update`: it rewrites `config.toml`'s values in place while keeping every comment, refreshes each pipeline file's generated key-reference block without touching the prose around it, and refreshes installed skills. `--dry-run` previews the change and `--replace <path>` takes a shipped file back wholesale, saving your version beside it as `.bak`; everything it writes is tracked in git, so `git diff` is the review. (#193)

### Breaking changes and migration
- `dispatch.interval` is retired: the dispatcher now polls at one fixed rate and there is nothing to configure. `spoolway config get dispatch.interval` and `spoolway config set dispatch.interval <value>` now fail with a retirement hint instead of resolving it. Delete any `dispatch.interval = ...` line from `.spoolway/config.toml`; a config that still sets it loads with a note and drops the key on the next save. (#251)
- The tmux backend is deleted along with `dispatch.tmux_mode`. `dispatch.backend = "tmux"` now loads as `herdr` with a migration note, and both the rewritten `backend` value and the dropped `tmux_mode` key are written back on the next save. Run `spoolway config set dispatch.backend herdr` (or `headless`) to make the switch explicit. (#250)
- `dispatch.default_pipeline` is gone, and `spoolway dispatch` no longer falls back to it: every task document must set its own `pipeline:` naming one of the project's pipelines, and the dispatcher's start preflight refuses the whole run if one is missing. Add `pipeline: <name>` to any task document that relied on the old default. (#174)
- A task no longer inherits `base:` from the checkout's currently-checked-out branch. It must be set on the task document, or with `queue add --base <branch>`; a submission giving neither is refused, naming the document. Set one of the two explicitly on any task that relied on the old fallback. (#143)
- `spoolway pipeline gen` and the `[pipeline_gen]` config table it read are removed, along with the fallback that answered a missing `.spoolway/pipelines/` directory from the pipelines built into the binary — a missing directory now reports the same `no pipelines defined` error an empty one already gave, and the `spoolway-tasks` skill no longer offers to generate a pipeline for a plan. Run `spoolway init --provider <name>` to install the two shipped pipelines into a project that has none. (#267, #272)
- `spoolway update` no longer brings a project's files forward — it only checks npm for a newer release and reinstalls the binary. Use `spoolway sync` (`--dry-run` to preview first) for `config.toml`, tracked pipeline docs and installed skills. (#193)
- The last release to publish a Windows binary is 0.4.x; `npm/targets.json` no longer builds `win32-x64` and the release workflow no longer publishes it. Run the Linux build under WSL: `wsl npm install -g spoolway`. (#153, #154, #156)

### Features
- `spoolway queue unqueue <task>` (and the board's `u`/`U`) carries a not-started task's document back to the pending directory, with `--force` able to tear down and unqueue a task that already started; `queue add --from` takes the result back unchanged. (#130, #139)
- Before a run starts, the queue screen now runs `doctor`'s cheap checks and shows a warnings screen — naming a `config.toml` behind this binary's own version, or a missing prompt — and waits for a key instead of starting straight into a run that might fail immediately. (#197, #240, #246)

### Reliability
- The board cursor no longer points at nothing when its row leaves the board, such as its group finishing — it falls back to the first row instead. (#280)
- The wait between dispatcher passes now redraws the board once a second on every platform, not only when the Linux-only directory watch is active, and a slow pass keeps redrawing on the same cadence instead of freezing until it finishes. (#317)
- A group's spend total on the board now counts only work that has actually finished, instead of also adding a running lane's own unbanked spend on top. (#317)
- A task routed back to the step it just left now waits out the full poll interval before its next try, so a bounded self-route's attempts land spread apart instead of firing inside the same second. (#313)

### Documentation
- Docs now publish to a GitHub Pages site at https://marvingygas.github.io/spoolway/, built by `.github/workflows/pages.yml` from `docs/`. (#309)

### Upgrading
- Install or update with `npm install -g spoolway@0.5.0`, or run it without installing via `npx spoolway@0.5.0`.
- The `spoolway` wrapper package now selects one of five platform packages at install time: linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, darwin-arm64 and darwin-x64. `@spoolway/win32-x64` stops at 0.4.0 and is not published for this release.
- After upgrading, run `spoolway whats-new` to read this record back from the installed binary.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.5.0

## 0.4.0

### Highlights
- `/spoolway-plan` now writes a machine-readable copy of the approved plan into a `<script type="text/markdown" id="plan">` block at the foot of the page, and `/spoolway-tasks` reads that block instead of parsing the rendered page; a plan revision only needs the block rewritten in the same pass as the rest of the page. (#134)
- `spoolway update` now writes the checkout it is run in (`repo.checkout`) rather than always the project root (`repo.root`), so running it inside a linked worktree updates that worktree's own tracked files, config and `.gitignore` instead of the main checkout's; it prints a `checkout:` line first, the same way `pipeline`, `config` and `doctor` already do. (#132)
- Status Log entries are now stamped with a readable local `YYYY-MM-DD HH:MM` wall clock instead of a UTC RFC 3339 timestamp, so a task's own history reads the way a person on the board would say it. (#133)

### Breaking changes and migration
- A `--pass` out of a staffed `blocked` step now carries the task to that step's `on_pass` (or back to itself for a command step) instead of always advancing past it, and a `--pause`, `--fail` or `--block` from that lane now hands the task back to the step it blocked on via `spoolway resume` — before, all three outcomes advanced the task past the blocked step. The `unattended.skip_blocked_lane` config key that used to control this is retired: `spoolway config get unattended.skip_blocked_lane` and `spoolway config set unattended.skip_blocked_lane <value>` now fail with `no config key` instead of resolving it. Delete any `unattended.skip_blocked_lane = ...` line from `.spoolway/config.toml`; a config that still names it loads with a note that the key is no longer read and drops it on the next save. (#147)
- `spoolway template contract` now lists three template shapes instead of four. `.spoolway/templates/task-log.md` (and the shipped `assets/task-log.md` fallback) is retired — the Status Log/Handoff/Blocker wording a lane is told to write is fixed and built into the binary rather than project-overridable. Delete any project-local `.spoolway/templates/task-log.md`; it is no longer read. (#135)
- `spoolway pipeline gen` is retired, along with the `[pipeline_gen]` config table it read (a project still carrying one loads it and drops it on the next save). A missing `.spoolway/pipelines/` directory is no longer answered from the pipelines built into the binary — it now reports the same `no pipelines defined` error an empty directory already gave. The `spoolway-tasks` skill no longer offers to generate a pipeline for a plan; where a project has none, it installs the two shipped pipelines with `spoolway init --provider` instead. Run `spoolway init` to restore the shipped pipelines and prompts on a project that has neither. (#261)

### Removed
- **Windows support.** The last release carrying a Windows binary is 0.3.x. Windows was always marked experimental and never had a headless backend. Run the Linux build under WSL: `wsl npm install -g spoolway`. `@spoolway/win32-x64` is deprecated on npm; published versions still install.

### Reliability
- A Windows process that lost the race to migrate a project's legacy state directory (the `~/.spoolway/<name>/` → `~/.spoolway/<label>-<id>/` move from 0.3.0) could surface a bare "Access is denied" error out of a migration that had in fact just succeeded beside it — Windows reports a losing racer's rename as `ERROR_ACCESS_DENIED`, not the `ENOENT` spoolway checked for. The race is now read off the outcome (the legacy home gone, the id-keyed home standing as a directory) rather than off one errno, and a rename Windows is refusing over a held handle is retried for up to 500ms before it is treated as a real failure. (#149)
- The same legacy-home migration had a second, platform-independent race one step further on: a process finishing an interrupted migration could hold a stale parse error over `project.toml` after a competing migration finished upgrading that same record in the window between the two, surfacing a raw `missing field id` out of a migration that had already succeeded. The record is now judged off a fresh read taken at the point of failure, not off the read that produced the now-stale error. (#151)
- The anchor-tab sweep no longer targets tabs opened on the project checkout itself, only tabs on a task's own worktree, fixing a bug where a person's own workspace on the checkout — or the dispatcher's own tab — could be swept as if it were a stale task tab. (#131)

### Upgrading
- Install or update with `npm install -g spoolway@0.4.0`, or run it without installing via `npx spoolway@0.4.0`.
- The `spoolway` wrapper package selects one of five platform packages at install time: linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, darwin-arm64 and darwin-x64.
- After upgrading, run `spoolway whats-new` to read this record back from the installed binary.

Release: https://github.com/marvingygas/spoolway/releases/tag/v0.4.0

## 0.3.0

### Highlights
- Spend now reaches outside the dispatcher: naming a directory in `[watch] dirs` (`spoolway config set watch.dirs ~/notes`) banks its own agent sessions — planning, skill runs, anything run by hand — as the project's own spend, alongside the lanes rather than invisible to them. (#112, #115)
- `spoolway eval` gains two ledger views for that directory spend, `dirs` (one row per watched directory) and `sessions` (one row per session outside the lanes), reachable with `tab` beside `pipelines`, `steps` and `runs`. (#120)
- `spoolway spend` can now group by `project` and `month` and take `--all`/`--project`, so a monthly bill or a cross-project split no longer needs external scripting. (#115)
- A pause on the board can wait for the step it would interrupt: `s` on a pause panel schedules the pause instead of cutting the running step short. (#110)
- `spoolway update` now asks npm for the actual latest version when it runs, on a short bounded lookup, instead of trusting a cache that has not caught up yet; it still falls back to that cache if npm cannot be reached in time. (#100, #101)

### Breaking changes and migration
- A project's own state directory has moved from `~/.spoolway/<checkout name>/` to an id-keyed `~/.spoolway/<label>-<id>/`, where `<id>` is stamped once into the project's `.git` directory and shared by every branch and worktree of that clone. The first `spoolway` command run against an already-initialised project after upgrading migrates its existing `~/.spoolway/<name>/` home onto the new location automatically; nothing manual is needed for a project claimed for the first time on this version. The move refuses to run while a dispatcher still holds the old home's lock, or while one of its worktrees is checked out and in use, to avoid moving the ground out from under live work — let the run finish or stop it, then run any `spoolway` command once to complete the migration. (#106, #108, #118)

### Reliability
- A pane that reported busy for a moment — a Windows quirk under load — could park a task that was never actually stuck; a busy pane is now retried instead of treated as a dead end. (#119)
- Windows resolves the same directory three different ways — an 8.3 short form, git's own long form, and `canonicalize`'s verbatim `\\?\` form — and half of spoolway compared paths without normalising first, so a checkout could read as a different checkout from its own recorded root, or a scratch worktree as outside the scratch directory holding it. Every path spoolway compares or hands to git now goes through one spelling. (#124)
- A tmux client connecting while the last session's server is still shutting down hears `server exited unexpectedly`, not `no server running`; spoolway recognised only the latter and could surface a shutting-down server as a real error. Both messages, and every other way tmux reports no server, now read the same. (#125)
- A workspace now reuses the tab it already has instead of opening a new one, and leftover anchor tabs from earlier runs are swept up rather than left behind. (#117)
- A command step's pane now closes once the command is over, wherever the task has gone since — it no longer lingers if the task moved on to a different step or lane in the meantime. (#113)

### Upgrading
- Install or update with `npm install -g spoolway@0.3.0`, or run it without installing via `npx spoolway@0.3.0`.
- The `spoolway` wrapper package selects one of six platform packages at install time: linux-x64-gnu, linux-arm64-gnu, linux-x64-musl, darwin-arm64, darwin-x64 and win32-x64.
- After upgrading, run `spoolway whats-new` to read this record back from the installed binary.

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
