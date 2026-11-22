---
name: spoolway-stress-test
description: Stress-test spoolway end to end — boot throwaway scratch projects, queue realistic tasks, and try to make spoolway break, hunting for holes where spoolway itself stops, stalls or misroutes a pipeline, or loses a commit or a branch. Delegates all the testing to parallel subagents, then merges their findings into one markdown report in this repo. Use when someone wants to battle-test, stress-test or break spoolway before or after a release.
disable-model-invocation: true
---

# spoolway-stress-test

Find the places where spoolway, not the model, makes a pipeline stop, stall, loop or do the
wrong thing, or loses work. Config, core routing and branch safety matter most. spoolway must
never lose a commit or a branch.

Report only what a person could realistically run into. Every fix can bring a new regression,
so a finding earns a fix only when it is likely enough and costly enough to be worth that risk.
A bug that needs hand-made git state, a kill timed to the millisecond, or input nobody would
write gets one line in an "Unlikely" list and is left alone.

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
   Give the strongest model and the highest effort to config, routing and branches.

   | Area | What it covers |
   |---|---|
   | Config | `config.toml`, `config set`, the override layer, precedence, `[models]` rows, edits while a dispatcher runs, `sync`, `doctor`, mixed binary versions |
   | Core routing | pass, fail, block and pause outcomes, loops and their bounds, gates, resume and restart, unattended escalation, task chains and groups, pipeline edits mid-run, `route_sim` against real runs |
   | Branches and unqueue | how worktrees and `task/<id>` branches are cut for groups, stacked groups and `starts_from:`; `queue unqueue` of a group's first, middle or last task while the others are queued, running, paused, blocked or handed over; unqueue racing a pass, a cut or a teardown; queueing again after an unqueue; teardown and cleanup deleting branches; `stack` squashing and force-pushing |
   | Dispatcher runtime | kills and crashes mid-step, two dispatchers at once, races with CLI commands, worktrees and git edge cases, retention and cleanup, cron jobs |
   | Realistic end to end | two or three small real projects, realistic tasks queued the way a person would, cheap real agents, `spoolway stack` against a stub `gh`, then the eval ledger and costs |
   | Tasks and command line | task files and frontmatter, Windows line endings and byte-order marks, groups, trials, routines, templates, hooks, `--json` output, exit codes |

   The branches tester holds spoolway to one rule: no commit and no branch is ever lost. A
   commit is lost when no branch or tag, local or on `origin`, reaches it any more. The reflog
   and dangling objects do not count, because git prunes them and spoolway never points a
   person at them. A branch is lost when it held such a commit and is gone. A commit that a
   handover squashed on purpose is not lost, as long as a reachable commit holds the same
   changes. The tester keeps
   a ledger. After every move in a scenario, it records the tip of every `task/*` branch,
   local and on `origin`, and every commit a lane, a command step or a "person" made. At
   the end of the scenario it checks that each recorded commit is still reachable. A delete
   the person asked for is not a loss. Examples are `unqueue` deleting the unqueued task's
   own branch, `eval --discard`, and a trial removing the arms it calls disposable. A loss
   is spoolway deleting or overwriting work the person did not ask it to touch. Examples are
   another task's branch, a remote branch, or work spoolway was holding for recovery.
   The tester also checks that each task in a group is cut from the right commit, and that
   the group's last pull request carries every earlier task's commits.

3. **Shared rules for every tester.** Copy these into each brief:
   - Work only under `~/dev/sandbox/spoolway-battle/<area>/`, in throwaway git repos with a
     local bare `origin`.
   - Never run spoolway against this repo, and never change it.
   - Every tester, the end-to-end one included, runs every command with
     `HOME=~/dev/sandbox/spoolway-battle/<area>/home` and `SPOOLWAY_SKIP_VERSION_CHECK=1`.
     spoolway puts each project's home under `$HOME/.spoolway`, so with the real `HOME` every
     scratch project is left behind in the person's `~/.spoolway`. A tester that runs real
     agents links only their sign-in files into that home, such as `~/.claude/.credentials.json`
     or `~/.codex/auth.json`, and never runs with the real `HOME` itself. Every command also
     sets `GIT_CONFIG_GLOBAL` to a file in the tester's folder, so nothing lands in the
     person's global git config.
   - Use the headless backend only, by running every command with
     `SPOOLWAY_TEST_BACKEND=headless`. Only that exact value selects headless. Any other
     value, `1` included, selects herdr, and the `dispatch.backend` key was retired in
     0.15.0. The switch has changed before, so the coordinator reads `headless_requested`
     in `src/mux.rs` before briefing. Never open herdr panes in the person's session. The
     session shell may carry `HERDR_ENV` and a herdr socket, so every command runs with each
     `HERDR_*` variable unset. Each tester also puts a guard script named `herdr` first on
     `PATH`, which logs the call and exits non-zero. A run that falls back to herdr then
     fails loudly instead of opening panes.
   - Run a copy of the spoolway binary from a folder in the sandbox that holds nothing else.
     Lanes put the binary's own folder first on `PATH`, and `~/.local/bin` holds the real
     `claude` and `herdr` beside `spoolway`. Run from there, a lane skips the tester's stubs
     and its `herdr` guard.
   - Prefer command steps and stub agents, which cost no tokens. Real agents use the cheapest
     model at low effort, with a small cap on lanes.
   - Every pipeline step that runs a claude agent names `model: claude-haiku-5-5` and
     `effort: low`. The scratch project's `config.toml` sets
     `[models."claude-haiku-5-5"]` with `context_window = 1000000` and `compact_ctx = 9`.
     Lanes then compact at about 88,000 tokens, below the 100k tier where Haiku 5.5's rates
     go up fivefold. `compact_ctx` takes whole percents only. Compaction runs between
     turns, so one turn of many parallel reads can still send a request past the tier.
     Real-agent tasks keep their reads small. A test that needs another
     model or threshold says why in its findings file.
   - Only the end-to-end tester may use the local llama.cpp models (see
     `~/dev/tools/local-llm`). It must not start, restart or reconfigure the router.
   - No pushes to GitHub and no real pull requests. Use a stub `gh` on `PATH`.
   - Only kill processes the tester started itself. Stop every dispatcher before finishing.
     Leave the scratch repos and their homes under the sandbox folder as evidence.
   - Reproduce each bug at least twice from a clean repo, and name the likely cause as
     `file:line`. Check `log.md`, `CHANGELOG.md` and the GitHub issues (read-only) for whether
     it is already known. Keep confirmed and suspected findings apart. Skip cosmetic nits.
   - Whenever a scenario kills, races, tears down or recuts a task that holds commits, check
     afterwards that every commit is still reachable, by the branches tester's rule above.
   - Test what people actually do. That is normal use, plus the ordinary mistakes and
     accidents around it: a wrong edit, a closed terminal, a laptop that sleeps, a command
     typed while the dispatcher runs, a task unqueued at the wrong moment. Before writing a
     finding up in full, ask whether someone using spoolway day to day would plausibly hit it.
   - Rate each finding's likelihood. **Common** means normal use hits it. **Plausible** means
     an ordinary mistake or accident hits it. **Contrived** means only a crafted setup hits
     it, such as hand-made git state, a kill timed inside a window of milliseconds, or input
     nobody would write. A contrived finding gets one line in an "Unlikely" list at the end
     of the file, with no work spent past the first sighting. A loss of work that a plausible
     accident causes is always written up in full.
   - Rate each finding's fix risk. **Small** means a local change with few callers.
     **Moderate** means a change to code several paths share. **Large** means a change to the
     dispatcher pass, the routing core, teardown, or anything most tasks go through. Then
     recommend one of: fix, fix only if cheap, or leave.
   - Write findings to `<scratchpad>/findings/<area>.md`. Each finding has an ID, severity,
     likelihood with the realistic road to it, fix risk, a recommendation, status, version,
     repro, expected and observed, likely cause, whether it is already known, and an
     evidence path. Start the file with what held up fine. End it with the "Unlikely" list.

   Severity: **work-losing** means a commit or a branch is lost, as the branches tester
   defines it, whatever area finds it. It ranks above every other severity.
   **Pipeline-stopping** means a task, a chain or the whole queue is stuck until a
   person edits files or git by hand. **Wrong-behavior** means the run completes but does the
   wrong thing. **Degraded** means recoverable but misleading or costly. **Minor** means a
   small real bug. Severity and likelihood are separate. A wrong-behavior bug that every
   group hits is worth more than a pipeline-stopping one that needs a crafted race.

4. **Relay each report as it arrives**, briefly: the count by severity and likelihood, and
   the worst few realistic ones. Say
   when a tester's summary contradicts its own file. Flag any slip that touched the
   person's real home or config.

5. **Check side effects yourself** once every tester is done. Look for leftover spoolway
   processes, `insteadOf` or other entries in the global git config, and new project homes
   in the real `~/.spoolway`. A new home there is a slip: name the tester, and once nothing
   runs in it, delete it, since its evidence is the scratch repo in the sandbox.

6. **Merge with one more agent.** It reads every findings file, merges duplicates (two
   testers finding one bug raises confidence), checks each cited `file:line` against `src/`
   for the serious findings, and settles inconsistent severities. It filters at least as hard
   as the testers should have. It challenges each finding's road to it, and moves any it
   judges contrived to the "Unlikely" list, whatever the tester rated it. It writes one
   report to `stress-test-findings-<YYYY-MM-DD>.md` at the repo root, and that is the only
   repo file it touches.

   The report holds a summary table of common and plausible findings only, sorted by
   severity with config, routing and branches first, with columns for likelihood, fix risk
   and recommendation. Next comes a short "Fix first" list of at most about five findings.
   Each one is common or plausible, costs real work or time when hit, and has a small or
   moderate fix risk. Then comes a "Lost work" section listing every realistic road to a lost
   commit or branch, from any area, with the ledger check that proved it. Then come sections
   per area, doc mismatches, what held up, what was not covered, and leftovers on this
   machine. The report ends with the "Unlikely" list, one line per finding. When there was
   an earlier report, it also lists which old findings are now fixed, and recommends
   closing any old finding that is contrived.

7. **Finish** with the totals, the "Fix first" list, the report's path, and the leftovers the
   person may want to delete. Everything the run built sits under
   `~/dev/sandbox/spoolway-battle/`, so offer to delete that one folder once the person has
   read the report. Do not commit the report. If the person wants tasks cut from
   it, that is the `spoolway-tasks` skill's job.
