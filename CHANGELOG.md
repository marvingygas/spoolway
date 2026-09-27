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
