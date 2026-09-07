---
domain: testing
covers: ["scripts/e2e/**", "scripts/e2e-*.sh", ".github/workflows/**"]
---

# Testing

spoolway drives models, and models are neither cheap nor deterministic. So the tests are
split by the **question they answer**, not by how fast they are:

| Instrument | Where | Answers | Runs |
|---|---|---|---|
| Unit | `src/**` (`#[cfg(test)]`) | Does each piece decide correctly? | `cargo test` |
| Suites | `scripts/e2e/suites/` | Does it still work when it is really running processes? | `scripts/e2e/run.sh` |
| Plans | `scripts/e2e/plans/` | What does a real run *do* — panes, gates, focus? | You queue one |

The first two are offline and free, and every task runs both through its own `test` step. The
third spends GPU time on a machine with a model server and a multiplexer, and happens when a
person queues it.

**Nothing overlaps, and the suites have the narrowest remit of the three.** Anything decidable
from files and exit codes is a unit test, where about a thousand of them decide it against
the code rather than against a fixture. A suite earns its place only when the thing under test
needs something a unit test cannot have: a real git repository, a real detached process, or a
real forge. Anything only a screen can show belongs to a plan.

That remit was applied in earnest once. Fifteen suites — `queue`, `config`, `settings`,
`pipelines`, `prompts`, `plan`, `plans`, `gates`, `sessions`, `eval`, `bugfix`, `kinds`,
`parallel`, `escalation` and `large` — asserted things a unit test already decided, in seven
thousand lines of shell, and were deleted. The `pr` tier went from nineteen suites and about
eleven minutes to five suites and under two.

**The end-to-end test of record is a plan, not a suite.** A person queues a plan under
`scripts/e2e/plans/` into a project `scripts/e2e/scaffold.sh` builds, and a real agent drives a
real dispatcher against a real model — the same thing a person does, done by an agent. That is
the test that answers whether spoolway works. The suites are a fast mechanical floor beneath
it.

## Running the suites

```sh
scripts/e2e/run.sh                        # the pr tier, what every push and every pull request runs
scripts/e2e/run.sh --tier smoke           # the fast signal, for a person running it by hand
scripts/e2e/run.sh --suite flow           # one suite
scripts/e2e/run.sh --list                 # the suites, and the setting-to-case map
```

It uses whatever `spoolway` is on `PATH`. To test a build:

```sh
cargo build --release
SPOOLWAY="$PWD/target/release/spoolway" scripts/e2e/run.sh --tier nightly
```

Every lane runs a stand-in agent — one script per launchable kind under `scripts/e2e/agents/`
(`pi`, `claude`, `codex`; `codex` is a thin front over `pi` that reads the
prompt and session home the way its kind is handed them), ordinary scripts copied onto the
lane's `PATH` and configured through the environment — and
every project is built from a seed written inline (`fixture.sh`'s `new_repo`). The whole
harness needs `git`, `bash`, `curl`, `setsid` and `flock` and nothing else — no toolchain, no
model, no multiplexer.

The exceptions are the `cloud` and `live` tiers, below — the two that run a real agent
binary instead of a stand-in.

### Flags

| Flag | Default | What it does |
|---|---|---|
| `--tier smoke\|pr\|nightly\|cloud\|live` | `pr` | Runs a named set of suites |
| `--suite <name>` | — | Runs one suite. Repeatable; overrides `--tier` |
| `--keep` | off | Leaves each suite's scratch tree on disk for a postmortem |
| `--list` | — | Prints the suites, every setting, and which case covers it |

`KEEP=1` in the environment does what `--keep` does, for a caller that cannot pass a flag.

## Coverage is enumerated, and the gaps are printed

`--list` does not read a table anybody maintains. It enumerates the settings surface from its
two sources — the keys in `.spoolway/config.toml`, and the step and top-level keys documented
in the comment block of `assets/pipelines/default.yml` — and matches each against the cases
that claim it:

```
settings:
  dispatch.auto_commit         off, a lane's leftovers are reported and not committed
  dispatch.tear_lanes_on_stop  a stop with the teardown off leaves every worktree and lane
                                exactly where it stood
                               a stop with the teardown on ends the lane before it takes the
                                worktree, and keeps the branch
  models                       a model with no window is never sized, and says so
                               a window from the project's own table beats the built-in one
  ...
  1 of 45 settings have no case here.
```

A setting two cases cover reads as two lines. That is information rather than a conflict, and
the map prints every claim it finds, sorted — which claim you see no longer depends on the order
a directory happened to be walked in. It used to take the first, and it bit: two suites both
claimed `session_reuse_ctx`, and the weaker "a bad value is refused" won on some runs and not
others.

Most settings now read `no case — unit <path>`. That is the expected answer, not a gap: the
setting is covered where it is decided. A bare `no case` is the real one.

A case claims a setting with a `# covers: <setting> — <what the case is>` line in its own suite
file; a plan claims one the same way inside its coverage block; a unit test claims one with a
`// covers:` line beside the test. So the map is assembled from the files that do the covering,
and deleting a case takes its claim with it.

Four readings, and they mean different things:

- **a case** — a suite fails when that setting stops working.
- **`no case — plans/x`** — nothing headless can see it; the named plan is where a person does.
- **`no case — unit <path>`** — no end-to-end instrument can reach it at all, and a unit test in
  that file is what holds it instead.
- **`no case`** — a real gap. `scripts/e2e/run.sh --list` names 18 today: `calibrate.window`,
  `dispatch.default_pipeline`, `dispatch.tmux_mode`, the six `pipeline_gen.pipeline_*` keys,
  the six `unattended.blocked_*` keys plus `unattended.skip_blocked_lane`, `update.check`,
  `step.description`, and `step.headless`.

The last three all count toward the tally, because none of them is an end-to-end case. What the
line after the tally buys is knowing which are gaps and which are simply covered elsewhere.

Adding a key to `config.toml` or to the pipeline key table adds a row here. That is the point.

## Tiers

A tier is a named set of suites, so a command step has something short to name.

| Tier | Suites | Used by |
|---|---|---|
| `smoke` | flow | Nothing automatic; the fast signal for a person running it by hand |
| `pr` | flow, commands, stacking, stack, conflicts, forge, disaster, lock, trials, routines, jobs, jobs-screen, restart, quota | The last task of a chain, through the `suite` step in `.spoolway/pipelines/*.yml` |
| `nightly` | the same as `pr` | The nightly routine, run by a person |
| `cloud` | warmth | Nothing automatic. It spends real tokens, and refuses to run without `SPOOLWAY_E2E_CLOUD=1` |
| `live` | live | Nothing automatic. It runs the real `codex` binary, and skips it unless `SPOOLWAY_E2E_CODEX_MODEL` names it a model |

`cloud` and `live` are in none of the others. `cloud` is the one place this project spends a
cloud model — see [The one suite that spends money](#the-one-suite-that-spends-money); `live`
spends nothing when its models resolve to a local endpoint — see
[The suite that runs the real binaries](#the-suite-that-runs-the-real-binaries).

## The suites

| Suite | Covers | Why not a unit test |
|---|---|---|
| `flow` | A task's whole life: queued → implement → review → handover → archived, including its command run files under `commands/` being reclaimed once it archives | Real detached processes, a real worktree, a real pull request |
| `commands` | Command steps: a `run:` line in the graph, its exit code routing, `background:`, `timeout:`, and that nothing confines it; the queue screen submitting a group and clearing its documents from the pending directory | A real spawned process with a pid, a log and an exit file; real keystrokes piped into the real binary |
| `stacking` | Three chained tasks: each pull request targets the branch it is cut from, and really sits on it — the third names both earlier ones and is cut from, and stacks on, the deeper of the two | A real rebase, in a real git repository |
| `stack` | `spoolway stack` itself: the squash to one commit and that a rejecting `commit-msg` hook cannot strand it, a refused lease, the empty-diff refusal, a `branch:` that is not `task/<id>` refused when the task loads, and `[stack.summary]`'s modes — the body taken verbatim from the task file, a model turn's printed output as the whole body, and the refusals for a half-set table or a missing template | Real git, and a forge double the command really shells out to |
| `conflicts` | A base that moves under a waiting branch, and the rebase that rescues it | The same, with the base actually moving |
| `forge` | The `gh` test double, and a hand-off that hands nothing over | A real forge interaction |
| `disaster` | The ways a run ends badly: a hard kill with lanes live, the stale lock it leaves, a restart over a still-running lane, a lane that reports with nobody listening, a stop that sweeps and one that does not, a retention sweep that spares a still-queued task's scratch tree and headless record, an `eval` read that banks no catch-up line for a lane still in flight, and a multiplexer that dies under worktrees that outlive it | Real detached processes, a real lock file, and — for the last case — a real tmux server on a scratch socket of its own |
| `lock` | `run.sh`'s own `pr`-tier lock: a second `--tier pr` invocation blocks until the first releases it, rather than running beside it and contending for the same disk and CPU | Two real `run.sh` invocations, pointed at a lock file of their own through `SPOOLWAY_E2E_PR_LOCK` so the suite never nests against the lock the run driving it is already holding |
| `trials` | One task forked across two pipelines from the queue screen's own `p` picker: an arm per pipeline lands in the queue directory under a minted id, with the right `pipeline:`, `skip:` and `group:` on it. Nothing here drives a dispatcher | Real keystrokes piped into the real binary — `run_screen` is exercised headlessly in Rust, but never as the whole binary reading a real pipe |
| `routines` | The repeatable documents under `.spoolway/routines/`, through the queue screen's `r` pane and `s` panel: `enter` lands the right task files under minted ids with their bodies untouched, and `s` copies a pending group's documents back into the checkout | The same, against a real tracked `.spoolway/routines/` tree |
| `jobs` | A cron job in a store, fired by a real dispatcher pass against a matching minute: the routine's documents reach the queue under minted ids with `depends_on` remapped and the job's pipeline set, the routine tree is left untouched, and `spoolway doctor` names a job whose expression will not parse, never comes round, points at a missing routine, or names an undefined pipeline | A real dispatcher driving a real `.spoolway/routines/` tree |
| `jobs-screen` | The `spoolway jobs` screen writing a job: the routine/schedule/pipeline walk lands a `[jobs.<name>]` table in the user store with the typed expression, the picked routine and the default pipeline, `jobs list` then shows it, `space` pauses and resumes it, and `x` then `y` deletes it | Real keystrokes piped into the real binary, against a real store file on disk |
| `restart` | `spoolway dispatch`'s restart guard: four starts in a row against a held lock each report exit 4, a fifth is refused with exit 5, `--force` starts one anyway and clears the count, and an empty queue reports exit 3 without ever tripping the guard | A real lock file, and real process exit codes across repeated real invocations |
| `warmth` | **cloud tier only.** Real `claude-haiku-4-5` lanes, because a stand-in's transcript agrees with the parser by construction | A real model writes the transcript |
| `live` | **live tier only.** The real `codex` binary through `agent verify --live` — a turn, then a resume — because a stand-in written from the adapter row cannot notice a CLI changing its flag grammar | The real binary |

Each suite runs in a process and a scratch tree of its own, so a suite that ends in `blocked`
on purpose — several do — cannot make the next one's assertions a fiction.

### A suite writes its own prompts

`configure_project` rewrites every shipped prompt name out of the project's pipelines and
writes the prompts that replace them: `builder`, `judge`, `stacker`, `closer`, `repro`.
An assertion that a `(closer)` lane ran is still an assertion about routing — which
step started which role — it just no longer breaks when somebody rewords
`assets/prompts/archivist/PROMPT.md`.

No suite names a shipped prompt any more. The one that did — `suites/prompts.sh` — was about
the shipped set itself, which `src/prompt.rs` decides without a fixture. A `grep` for those
names across `scripts/e2e/suites/` should match nothing.

### The resident dispatcher

`dispatcher_start` runs `spoolway dispatch` in a loop inside a `setsid` process group, for
every suite that needs a dispatcher actually driving a task. That loop is bounded, so a suite
that leaves something re-queuing itself fails quickly and says why, rather than spinning until
the outer timeout kills it with nothing in the postmortem but a bare timeout.

Between rounds it waits, starting at 0.5 seconds and doubling on a round that found nothing to
do, up to a 4 second ceiling; a round that did real work (exit 0) resets the wait back down to
0.5 seconds. Every round after the first logs the previous round's exit status and how long it
waited, so a postmortem reads what the supervisor was doing rather than inferring it from
timing.

It also stops on its own after `E2E_DISPATCH_MAX_ROUNDS` rounds, 60 by default, and writes a
line naming the cap it reached — the harness's own backstop against a `dispatch` that keeps
exiting cleanly forever, distinct from the restart guard `spoolway` itself enforces against a
caller in a tight restart loop. Exit 3 (an empty queue) is treated as an ordinary ending, the
same as exit 0, and rounds again rather than failing.

When `drive` gives up waiting for a task to reach a stage, or the log shows the dispatcher was
refused, it prints everything still alive in the supervisor's own process group — a wedged run
this way names what is holding it, rather than leaving the log tail as the only evidence.

Alongside `works` (exit 0 only) and `refuses` (any non-zero exit), the harness has `exit_code`,
which asserts a command ends on one exact exit code — the assertion `restart` needs to tell
`spoolway dispatch`'s exit 3, 4 and 5 apart from each other and from an ordinary 0.

## The one suite that spends money

`warmth` is the `cloud` tier, and it exists for a chain nothing else can reach:

```
a real transcript → usage::touched_at → carried_session → resume
```

The size bound and the idle bound are both unit tests in `src/dispatch.rs`, decided against a
transcript written to order — and a transcript written to order agrees with spoolway's parser by
construction. The idle horizon is read straight off the store's own mtime rather than off
anything inside the transcript, so a stand-in's fixture can prove it by moving the file's clock
back, which is exactly what those tests do. `warmth` still runs the same bound against a
real `claude` session on top of that, because that is the one place this project spends a cloud
model at all, and it is worth keeping an eye on the shape a real transcript actually writes even
where this particular reading no longer depends on it.

```sh
SPOOLWAY_E2E_CLOUD=1 scripts/e2e/run.sh --tier cloud
```

Eight turns of `claude-haiku-4-5` against a fixture repo of two files — six through the
pipeline scenarios, two more through `agent verify --live` at the end, fractions of a cent
altogether — and it skips itself without that variable. It uses your real `$HOME`, because the
real `claude` needs your credentials; what it writes there is its own new sessions in a
project directory of its own. The one transcript it edits is one of those, and only its clock:
proving the idle half means a store older than `session_reuse_idle`, which is not a wait a
test can perform, so that case backdates the file's own mtime and leaves every other field as
Claude Code wrote it.

Two things it needs that a stand-in suite does not, both on the lane's `PATH`: the real
`claude`, and **the build under test**. A stand-in calls `"$SPOOLWAY" report` by absolute path;
a real lane runs the prompt's own words, and the prompt says `spoolway report` — which
resolves to whatever is installed on the machine. The first run of this suite reported to a
`~/.local/bin/spoolway` that predated `paused`, and every scenario silently measured a
conversation that had never stopped.

## The suite that runs the real binaries

`live` is the `live` tier, and it is `warmth`'s shape aimed at the other kind that needs a
real binary. The codex adapter row was settled by hand against one version of the CLI —
codex-cli 0.147.0; `docs/agents.md` records what the probe said — and the
stand-in the harness drives was written *from* that row, so it agrees with it by
construction. When the CLI changes its flag grammar, the first thing to notice must not be
a person's plan run failing an hour in.

One command is the whole suite: `spoolway agent verify codex --live` runs a real
turn, reads every accounting clause back through the same functions the dispatcher uses, then
resumes the session once and asserts the resume spelling — `codex exec resume --last` — still
means "continue this lane's session" on the binary installed today.

```sh
SPOOLWAY_E2E_CODEX_MODEL=<model> \
  scripts/e2e/run.sh --tier live
```

codex is opt-in by naming it a model, and skipped with a pointer otherwise — so no
machine is required to carry this CLI, and a machine that opts it in must have it: a
model named while the binary is missing fails rather than skips. The name is expected to
resolve to a local endpoint through codex's own provider config (it needs
`wire_api = "responses"`), in which case the turns cost nothing. It uses your real `$HOME`,
because the real binary needs its own credentials and provider config; what it writes is a
scratch tree and per-session homes under spoolway's own state directory, the same as any lane.

## Plan runs: the half with a person in it

Workspaces, tabs, panes, focus, a gate somebody answers, a notification arriving — none of that
can be asserted headlessly, and a suite that stubbed it would be asserting about the stub. It is
covered instead by small plans you queue by hand into a throwaway project:

```sh
scripts/e2e/scaffold.sh --list                    # the plans there are
scripts/e2e/scaffold.sh --plan gates              # build a project for one
cd ~/dev/project/spoolway-e2e-gates
# claude, then: open scripts/e2e/plans/gates.html, approve it, and let
# /spoolway-plan cut the tasks — then: spoolway queue, to open the screen
# and submit them
spoolway dispatch                                 # and watch
scripts/e2e/scaffold.sh --plan gates --clean      # when you are done
```

`scaffold.sh` seeds a repo, builds a local forge out of `scripts/e2e-fake-gh.sh`, installs
`scripts/e2e/runtime/end-to-end.yml` — every step `agent: pi`, so a run spends nothing but
GPU time — prints the `SPOOLWAY_GH` export that points `handover`'s `spoolway stack` at the
local forge, and applies whatever `config:` lines
the plan itself asks for. It refuses to build anything until `pi` is on `PATH` and the model
server has actually heard of the model the lanes will name.

| Plan | What it is for |
|---|---|
| `escalation` | A block, a review that runs out of conversations, a lane that goes quiet |
| `gates` | A gated step stopping in its pane, and the notification that fires |
| `workspace` | Two lanes, two worktrees, one file between them, and an interrupted run |
| `observer` | The backgrounded observer, and the record it leaves in `observations.md` |

Each plan is a page small enough to read without scrolling, styled inline so it reads right
opened on its own, and carrying a `spoolway-plan` JSON block naming its tasks and the settings
it reaches — the shape a plan page argued in before the block went with the store, kept here
because `queue-plan.sh` still reads it directly rather than through the skill. Its tasks ask a
local model for a **behaviour** — block on your first turn, say nothing for three minutes —
because what is under test is spoolway's answer to that behaviour, not the model's answer to a
problem.

### The observer

`scripts/e2e/observe.sh` is started by the `observe` step of the runtime pipeline: a
backgrounded `run:` step every task passes through on its way in, and the pipeline's entry, so
the observer is up before the first lane is. It takes a lock, so exactly
one runs whatever the task count, and polls `spoolway lane` — with no argument to list the
lanes, with one to read a pane — appending lane, time and what it saw to `observations.md` in
the project root. No spoolway feature is behind it; that is the whole mechanism.

## What is deliberately *not* an end-to-end suite

A hollow suite is worse than none, so two domains are covered elsewhere:

- **Backend parity.** Comparing `herdr` against `headless` needs a multiplexer, and these suites
  have to pass on a machine with none. The backend's behaviour is unit-tested in
  `src/headless.rs`; what a multiplexer actually does is `scripts/e2e/plans/`.
- **The board.** What a dispatch run draws between passes is rendering, not routing: the pieces
  with any logic in them are unit-tested in `src/status.rs`.
- **A herdr multiplexer dying.** `disaster` proves the heal path — a workspace verified live
  before it is reused, and a fresh pane opened when it is not — against a real tmux server on a
  scratch socket of its own, because tmux alone can run detached, on nothing but the machine
  this harness already needs. The same case against herdr would need a herdr of its own to run
  in, and herdr cannot be nested inside the one this project itself runs under — so that case
  stays a plan a person queues under `scripts/e2e/plans/`, never a suite.

## There is no CI

`.github/workflows/ci.yml` is switched off on GitHub. It ran three jobs on every push and every
pull request, one of them on a Windows runner billed at ten times the Linux rate, and this
account was running out of Actions minutes. That last part no longer holds — the repository is
public, and public repositories get Actions free — so what keeps it off is now only that the
laptop already covers everything it did. The file is left on disk unchanged so that
`gh workflow enable ci` is the whole of turning it back on.

Everything it did now happens on a laptop, and none of it is optional there. **The `test` and
`suite` steps in each of `.spoolway/pipelines/*.yml` are the whole mechanical verdict on a
change.** Both are command steps, so each is an exit code rather than a lane's own report, and
both run before the `handover` step opens a pull request.

`test` runs for every task:

    cargo fmt --check
    cargo deny check advisories
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --all-targets --locked
    cargo check --target x86_64-pc-windows-gnu --all-targets --locked
    cargo build --release
    ./target/release/spoolway pipeline check

`suite` runs the `pr` tier, and carries `last:` — see [`last:` — a step the chain runs
once](pipelines.md#last--a-step-the-chain-runs-once):

    scripts/e2e/run.sh --tier pr

They are two steps rather than one because `last:` applies to a whole step. Folded together,
every task but the top of a stack would lose fmt, clippy and the compiler along with the
suite, and those are the checks worth having per task. The suite is not: every branch in a
plan is rebased onto the one below, so the task at the top carries every change beneath it and
one run there covers all of them.

Cheapest first, so the commonest failure costs the least to find. A failure in either routes
the task back to the step before them — `e2e` in every `impl*.yml`, `reproduce-again` in
`bugfix.yml` — bounded to two laps from each of `test` and `suite`, and then `on_loop_max`
parks it for a person.

`pipeline check` is in the list because this project is developed with the spoolway it builds, so
`.spoolway/` here is a live control plane rather than a fixture. A retired pipeline key reaching
main once stopped the dispatcher from loading at all. It runs against the task's own worktree,
using the binary the line above it just built.

One check does not belong in a per-task verdict and moved to the nightly
routine instead, where a person runs it by hand:

- `cargo test --target x86_64-pc-windows-gnu --all-targets --locked`, the Windows tests actually
  run rather than only compiled. WSL interop executes Windows binaries on the machine this
  project is developed on, which is the only reason it is possible at all.

`cargo deny check advisories` runs per task instead, in `test` above: the advisory database
moves without this repository moving, so a nightly-only run would leave a task landing on a
green check that had already gone stale.

**Nothing runs on a pull request any more.** A pull request opens with an empty check list and
stays that way; that is the finished state, not something still in flight. It is also why this
project's own pipelines carry no `checks` step: `gh pr checks` on a pull request with no checks
fails rather than passing, so the step would park every task — see [The shipped `default`
pipeline](pipelines.md#the-shipped-default-pipeline).

What is now checked nowhere: a pull request as a whole, against a tree nobody has a local copy of
— every check above runs in the task's own worktree, so a change that is fine alone and broken
beside another task's change is only found when a person lands the stack.

Nothing is installed and nothing needs to be, except `cargo-deny` and the
`x86_64-pc-windows-gnu` target. The four checked-in fixture projects that used to drag `python3`
and `node` in are gone; a suite builds its own subject.

The one layer no gate reaches is the plans, and that is deliberate: they need a model server, a
multiplexer and somebody watching. `--list` says which settings they cover and which are covered
nowhere.
