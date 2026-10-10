---
name: spoolway-stress-test
description: Stress-test spoolway end to end — boot throwaway scratch projects, queue realistic tasks, and try to make spoolway break, hunting for holes where spoolway itself stops, stalls or misroutes a pipeline. Delegates all the testing to parallel subagents, then merges their findings into one markdown report in this repo. Use when someone wants to battle-test, stress-test or break spoolway before or after a release.
disable-model-invocation: true
---

# spoolway-stress-test

Find the places where spoolway, not the model, makes a pipeline stop, stall, loop or do the
wrong thing. Config and core routing matter most.

You coordinate and do not test. Everything that touches a scratch project is done by
subagents. Your own work is limited to briefing them, checking on side effects, and handing
the merge to one more agent.

## Steps

1. **Get oriented.** Note the installed binary's version (`spoolway --version`) and what `main`
   is (`git describe --tags`). Check for spoolway processes that are already running, so
   nobody kills them later. Read the last report if one exists: `stress-test-findings*.md` at
   the repo root, and every `plans/*/stress-test-findings*.md` under the project home, where a
   report moves once tasks are cut from it. The testers re-check its open findings, and the
   new report says which ones are fixed and which came back.

2. **Launch the testers in parallel**, one area each, in a single message. Leave the attack
   ideas to them. Brief each one on its area, the shared rules below, and the output shape.
   Give the strongest model and the highest effort to config and routing.

   | Area | What it covers |
   |---|---|
   | Config | `config.toml`, `config set`, the override layer, precedence, `[models]` rows, edits while a dispatcher runs, `sync`, `doctor`, mixed binary versions |
   | Core routing | pass, fail, block and pause outcomes, loops and their bounds, gates, resume and restart, unattended escalation, task chains and groups, pipeline edits mid-run, `route_sim` against real runs |
   | Dispatcher runtime | kills and crashes mid-step, two dispatchers at once, races with CLI commands, worktrees and git edge cases, retention and cleanup, cron jobs |
   | Realistic end to end | two or three small real projects, realistic tasks queued the way a person would, cheap real agents, `spoolway stack` against a stub `gh`, then the eval ledger and costs |
   | Tasks and command line | task files and frontmatter, Windows line endings and byte-order marks, groups, trials, routines, templates, hooks, `--json` output, exit codes |

3. **Shared rules for every tester.** Copy these into each brief:
   - Work only under `~/dev/sandbox/spoolway-battle/<area>/`, in throwaway git repos with a
     local bare `origin`.
   - Never run spoolway against this repo, and never change it.
   - Every tester, the end-to-end one included, runs every command with
     `HOME=~/dev/sandbox/spoolway-battle/<area>/home` and `SPOOLWAY_SKIP_VERSION_CHECK=1`.
     spoolway puts each project's home under `$HOME/.spoolway`, so with the real `HOME` every
     scratch project is left behind in the person's `~/.spoolway`. A tester that runs real
     agents links only their sign-in files into that home, such as `~/.claude/.credentials.json`
     or `~/.codex/auth.json`, and never runs with the real `HOME` itself.
   - Use the headless backend only: `dispatch.backend headless` with `SPOOLWAY_TEST_BACKEND=1`.
     Never open herdr panes in the person's session.
   - Prefer command steps and stub agents, which cost no tokens. Real agents use the cheapest
     model at low effort, with a small cap on lanes.
   - Only the end-to-end tester may use the local llama.cpp models (see
     `~/dev/tools/local-llm`). It must not start, restart or reconfigure the router.
   - No pushes to GitHub and no real pull requests. Use a stub `gh` on `PATH`.
   - Only kill processes the tester started itself. Stop every dispatcher before finishing.
     Leave the scratch repos and their homes under the sandbox folder as evidence.
   - Reproduce each bug at least twice from a clean repo, and name the likely cause as
     `file:line`. Check `log.md`, `CHANGELOG.md` and the GitHub issues (read-only) for whether
     it is already known. Keep confirmed and suspected findings apart. Skip cosmetic nits.
   - Write findings to `<scratchpad>/findings/<area>.md`. Each finding has an ID, severity,
     status, version, repro, expected and observed, likely cause, whether it is already
     known, and an evidence path. Start the file with what held up fine.

   Severity: **pipeline-stopping** means a task, a chain or the whole queue is stuck until a
   person edits files or git by hand. **Wrong-behavior** means the run completes but does the
   wrong thing. **Degraded** means recoverable but misleading or costly. **Minor** means a
   small real bug.

4. **Relay each report as it arrives**, briefly: the count by severity and the worst few. Say
   when a tester's summary contradicts its own file. Flag any slip that touched the
   person's real home or config.

5. **Check side effects yourself** once every tester is done. Look for leftover spoolway
   processes, `insteadOf` or other entries in the global git config, and new project homes
   in the real `~/.spoolway`. A new home there is a slip: name the tester, and once nothing
   runs in it, delete it, since its evidence is the scratch repo in the sandbox.

6. **Merge with one more agent.** It reads every findings file, merges duplicates (two
   testers finding one bug raises confidence), checks each cited `file:line` against `src/`
   for the serious findings, and settles inconsistent severities. It writes one report to
   `stress-test-findings-<YYYY-MM-DD>.md` at the repo root, and that is the only repo file it
   touches. The report holds a summary table sorted by severity with config and routing
   first, a short "Fix first" list, sections per area, doc mismatches, what held up, what was
   not covered, and leftovers on this machine. When there was an earlier report, it also
   lists which old findings are now fixed.

7. **Finish** with the totals, the worst findings, the report's path, and the leftovers the
   person may want to delete. Everything the run built sits under
   `~/dev/sandbox/spoolway-battle/`, so offer to delete that one folder once the person has
   read the report. Do not commit the report. If the person wants tasks cut from
   it, that is the `spoolway-tasks` skill's job.
