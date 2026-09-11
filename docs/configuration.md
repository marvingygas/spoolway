---
domain: configuration
covers: ["src/config.rs", "src/confkv.rs", "src/confdoc.rs", "src/tracking.rs", "src/retain.rs", "assets/tracking/**", "assets/hooks/**"]
---

# Configuration

Everything configurable lives in one file: `.spoolway/config.toml`. The pipeline graph itself
lives next door in `.spoolway/pipelines/` — see [Pipelines](pipelines.md). What this file holds
is deliberately narrow: facts about *this machine* — where lanes run, what a lane may reach,
what a model costs here. What a step runs, which model it names and how hard that model thinks
are pipeline facts now, written once in the graph rather than duplicated in both places.

## Editing it

```
spoolway config edit           # open the file in $EDITOR, re-validated on save
spoolway config show           # the whole config
spoolway config list           # every scalar key, as `key = value`, in key order
spoolway config path           # where the file is
spoolway config get agents.pi.kind
spoolway config set models.claude-opus-5.input 5.0
```

The file is the interface: a reference table on top names every key, its possible values,
its default and one sentence about it, and points at this document for the rest. There is no
comment above any individual key any more — one table, once, replaced a paragraph repeated
above every key it explained. `config set` edits the file as a document rather than rewriting
it from the parsed struct — one key changes, and every other byte, the table and blank lines
included, is copied through unread; migrating the file whole is `spoolway update`'s job, never
a side effect of setting a value. `config set` refuses a key that is not already in the file.
Two things are allowed to grow on first write instead. `[models]` is one: a price or a window
for a model spoolway never named cannot already be there. `agents.<profile>.concurrency` is the
other, because it is *omitted* wherever a profile asserts no cap — a `0` there would read as a
number somebody chose rather than as a question nobody answered. The profile itself still has
to exist, so a misspelt profile name is refused exactly as before.

The table and the settings screen render from the same register, `confkv::REFERENCE` — one
sentence per setting, not the whole story. What that sentence drops is written out below,
here, once per setting rather than in the binary at all.

`show`, `path`, `get` and `edit` all answer for the checkout a command was run in — a lane
standing in its own linked worktree reads that worktree's `config.toml`, not the project's,
because that is the file actually beside it. `set` is the one exception: the dispatcher only
ever reads the project's own copy, so writing anywhere else would sit unread until the branch
merges. Run it from a linked worktree and it refuses, naming the `-C` invocation that writes to
the project instead:

```
$ spoolway config set dispatch.interval 30s
spoolway: the dispatcher reads the project's config, not this worktree's.
  spoolway -C /home/you/dev/project/spoolway config set dispatch.interval 30s
```

Nothing is written either side. In the main checkout the checkout and the project are the same
directory, so `set` behaves exactly as it does everywhere else in this section.

## Runtime state

`config.toml`, `.spoolway/pipelines/` and `.spoolway/prompts/` are the tracked control
plane, and stay in the checkout. Everything spoolway *writes while it runs* — the queue, the
archive, pending task documents, scratch worktrees, composed prompts, the headless backend's
own records, command-step logs, `lanes.json`, `usage.jsonl`, `dispatch.pid`, `jobs.state.json`,
and the two scratch queue indexes — lives outside it, at `~/.spoolway/<basename of the
checkout>/`.
`spoolway init` claims that name by writing `~/.spoolway/<name>/project.toml`, holding the
checkout's own path, and refuses to run in a second checkout that would claim a name already
taken by a different one — rename one of the two directories to get past it. When the
registered checkout no longer exists, `init` reclaims the name outright if its state holds no
archive and no queued tasks, and otherwise refuses, naming `spoolway init --take-over`, which
claims the name and keeps whatever archive or queue was already there. `.dispatcher` is refused
as a name too, because that one belongs to the shared
dispatch workspace every project's lanes open in — see [One home for every run, in every
project](dispatcher.md#one-home-for-every-run-in-every-project).

The default cron-job store, `jobs.toml`, sits here too, beside `lanes.json`. A job may
instead be kept in `.spoolway/jobs.toml` in the checkout, tracked and shared with the team.
Neither is a config key — see [Jobs](jobs.md).

Every directory under it is created silently the first time anything resolves it, so a fresh
clone or a home directory deleted by hand gets one back without a command failing or a note
being printed — an empty queue is the honest answer for a machine that has run nothing yet.
`system-prompts/`, `commands/`, `tracking/`, `headless/`, `scratch/` and `archive/` hold what
a pass left behind rather than work still in flight, so `[retention]` below ages their contents
out;
`queue/`, `pending/`, `worktrees/` and `plans/` hold work itself and are never swept. Delete the
whole `~/.spoolway/<project>/` directory to forget every task, plan and lane a project has ever
run; the checkout is untouched, and nothing in it points back at what was deleted.

## `[dispatch]` — the run loop

```toml
[dispatch]
backend = "herdr"
herdr_mode = "grouped"
tmux_mode = "grouped"
worktree_root = ""
interval = "10s"
lane_quiet = "15m"
default_pipeline = "default"
auto_commit = true
```

| Key | Default | Meaning |
|---|---|---|
| `backend` | `herdr` | Where lanes run. `herdr` and `tmux` put every agent in a real pane you can watch, attach to and take over; `headless` needs no multiplexer at all |
| `herdr_mode` | `grouped` | How a herdr run is laid out in the multiplexer, and read only under `backend = "herdr"`. `grouped` shares the one `spoolway-dispatcher` workspace with every project, a tab of its own per project and a pane per running task — see [One home for every run, in every project](dispatcher.md#one-home-for-every-run-in-every-project). `split` gives each task a herdr workspace of its own, cut with git and labelled `spoolway/<task>`, and leaves the dispatcher where it was started. The old spellings `workspace` and `worktrees` still parse, and so does the singular `worktree` |
| `tmux_mode` | `grouped` | How a tmux run is laid out, read only under `backend = "tmux"`, with the same two answers: `grouped` shares the one `spoolway-dispatcher` session with every project, a window per project and a pane per running task, and `split` is a session per task, named `spoolway/<task>`, the dispatcher left where it was started. See [tmux](dispatcher.md#tmux) |
| `worktree_root` | *(blank)* | Where a dispatched task's worktree is cut, for every backend and every layout. Blank means `~/.spoolway/<project>/worktrees`. The directory under it is the branch flattened to one component, `task-<id>`, for every backend — or `task-<slug>-<id>` when a tracker slug prefixed the branch |
| `interval` | `10s` | How long to wait between passes when looping. About the floor worth having: below it a pass's polling overhead buys no reaction time, since a lane takes wall-clock minutes regardless |
| `lane_quiet` | `15m` | How long a lane may say nothing before the dispatcher reminds it to report. Patience, which is a different quantity from `interval` and used to be read off it — how often a pass *looks* at a lane says nothing about how long that lane may reasonably be quiet, and reading one off the other meant turning the poll rate down silently bought less patience. Ten seconds is not a stuck lane, it is a lane thinking: a step whose prompt runs the end-to-end `pr` tier ends its turn and waits on a background job for as long as that run takes. This bounds the wait before *each* reminder, and three reminders still cap the round trip, so a genuinely dead lane is escalated four of these later |
| `default_pipeline` | `default` | Which pipeline a task runs on when its `pipeline:` field is absent. Here rather than in a pipeline file because naming the default is a statement about the set, and a file claiming to be it would be one of several |
| `auto_commit` | `true` | Whether spoolway commits what a lane left uncommitted when its step settles, as `wip(<task>): <step>` — also the first thing `spoolway stack` does at `handover`, on the same rule. A lane that made its own commits is left alone and its leftovers reported as residue. `false` reports and commits nothing. Either way, a task on its way to a cleanup terminal is held at `blocked` rather than archived when work is still in its worktree that spoolway could not record — `auto_commit` off with leftovers behind, or a git command that failed — so a worktree is never torn down over work that was never committed |
| `priority` | `group` | Whether a free slot is filled from every ready task, or from the group already landing first. `group` is the shipped default: it sorts a candidate whose own group is already open ahead of one whose group has never run, and stops there, so a slot the open group has no work for goes to the next group in the sort rather than sitting idle. `any` drops that tier, weighing every ready candidate on steps left, group size and dependents alone |
| `tear_lanes_on_stop` | `true` | Whether stopping the dispatcher ends the run's live lanes and takes their worktrees with it: every task's agents stop, then the workspace (or, under `grouped`, just the checkout) of every task it cut one for. The tab or window of every group the sweep emptied closes too — never the board's own, and never the shared `spoolway-dispatcher` workspace or session itself, which another project's lanes may still be live in; that row is only ever closed by hand. **The branches stay** — an interrupted task's commits live nowhere else, so the next run cuts a fresh worktree on the branch it finds and resumes there. **A task on `blocked` is never swept** — its pane is being kept for you to read and [`spoolway resume`](cli-reference.md) needs the checkout underneath it, so anything spared is still there when the run is over. Nothing on a remote is touched, and neither is a checkout you already had open. `false` leaves everything exactly where it stood, agents included |
| `lane_child_ceiling` | `1h` | How long a lane may be excused the reminder loop for a process it started that is still running, on a backend able to say so. A lane sitting quietly on its transcript because it is mid-tool-call looks identical to one genuinely idle, so this bounds how long "not silent" may keep excusing it before the reminder loop treats it as silent regardless — long enough for a real build or test suite to finish, short enough that a lane wedged behind a runaway child is still found the same afternoon. Not written to a defaulted `config.toml`, for the same reason as `tear_lanes_on_stop` above: a lane here routinely runs a binary built from a branch behind `main`, and an unknown key is a hard parse error rather than something to ignore |

**`worktree_root` is deliberately outside the checkout.** A worktree under `.spoolway/` would
sit beside the queue and the prompts every lane already reads, and a lane building there
could rewrite any other task's checkout by name.

It is deliberately outside `~/.herdr/` too, which it did not used to be. That directory is
herdr's own, and the checkouts spoolway cuts are ones herdr never hears about until it is
pointed at one — filing them there invited the multiplexer to make sense of directories it did
not make. One root, one setting, one answer to where a dispatched checkout lives — nested under
the project's own home (`~/.spoolway/<project>/`) by default, beside its queue and archive, so
a worktree cut here never registers as a workspace of its own the way one cut at a
repository's root did.

**`interval` is not a cache-tuning knob.** A step's lane takes one turn and never resumes,
so no prompt cache ever helps it. None of this applies to a local server, whose cache has no timer
and is evicted by lanes competing for slots — so `models."<glob>".slots` governs that, and
this does not.

### Retired: the launch guard, the terminal it used to open, and two settings for a workflow this is not spoolway's to referee

`max_launches` (how many times a task's lane may be *launched* at the step it is on before a
person is asked instead) was never a number anybody tuned, so it is a constant now —
`dispatch::MAX_LAUNCHES`, one launch before a person. `open_on_escalation` and `open` opened a
terminal on the blocked lane, on whatever machine the dispatcher happened to run on — useless
with an all-cloud lane on a screen nobody is watching, so the whole path went with the keys.
`protected_branches` refused to dispatch, or to cut a task's worktree, while based on a
listed branch — which branch is safe to build on is a fact about a project's own git workflow,
not one spoolway is in a position to guess at, so the guard came out. `notify` ran a shell
command when a task needed a person; the board already names the pane to go and look at, and
`spoolway lane --attach` is untouched.

All five still parse in a config saved before this — they are read, and dropped on the next
save, the way `[criteria]` was retired.

### What hands a change over, and what asks first

Neither is here, and neither is a setting any more. `handover:` and `gate:` are step keys in
the pipeline that owns them — see [Pipelines](pipelines.md). Both were keyed by pipeline name
from this side of the fence once, which meant `gate: true` on a step did not tell you whether
the step gated. They live beside the step they act on now, and there is no pipeline name to
misspell.

What a hand-off *does* with the branch is not configured anywhere: it is `spoolway stack`'s,
mechanically — commit, squash, push, open the pull request — and when that fails it routes to
`blocked`, where a person takes over. There is no merge mode to pick.

### `[stack.summary]` — a model for the pull request's title and text

```toml
[stack.summary]
agent = ""
model = ""
effort = ""
prompt = "summariser"
```

Every project's `config.toml` carries this table, and blank `agent` and `model` are what say
"no model": `spoolway stack` opens the pull request with the task's `title:` and its own body
verbatim, the whole task file as the body. Fill both in and it runs one turn of the named
prompt on the task file first, using its first printed line as the title and everything below
it as the body. Either way the title is a Conventional Commits line — `feat(queue): add a
--dry-run flag` — because that is what a `title:` is written as, and what
`.spoolway/templates/pull-request.md` asks the prompt for.

| Key | Default | Meaning |
|---|---|---|
| `agent` | (blank) | An agent profile from `[agents.*]` — the same table a pipeline step's `agent:` names one from. Blank alongside `model`, the whole task file is the pull request body instead. |
| `model` | (blank) | The model that profile's kind is started with. Blank alongside `agent`, the whole task file is the pull request body instead. |
| `effort` | (blank) | Passed to the agent kind's effort flag, same as a step's `effort:`. Blank means no flag is sent. |
| `prompt` | `summariser` | Prompt under `.spoolway/prompts/` — the shipped one does nothing but fill in `.spoolway/templates/pull-request.md` |

There is deliberately no separate key here that turns the summary model on or off — blank
`agent` and blank `model` already say it, the same shape `pipeline_gen.pipeline_model` uses for
its own refusal. Nor is there a key that turns stacking itself on or off: the pipeline's
`handover` step, naming `run: spoolway stack`, is what does that. This table only ever adds a
model on top of mechanics that already run without one.

## `[unattended]` — the overnight run

```toml
[unattended]
enabled = false
max_output_tokens = 0
max_cost_usd = 0.0
skip_blocked_lane = true
blocked_agent = "claude"
blocked_model = "claude-opus-5"
blocked_effort = ""
blocked_session = true
blocked_prompt = "unblocker"
```

Everything here means something only in an unattended run — split out of `[dispatch]` because
every other key there applies whether or not a person is watching.

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `false` | Whether this run stops for a person, or for nobody — see [Unattended runs](pipelines.md#unattended-runs). `true` sends no task to `blocked` at all: every block resumes the lane that hit it, in the same session, with its round budgets handed back. A step's `loop` stops applying, since it exists to hand a decision to somebody who is not there, and the launch ceiling that parks a task whose lane keeps dying backs off instead — `gate:` still holds, and parks the task on `paused` for a person regardless. What else still holds is every check that catches a lane going wrong rather than a person being needed: the reminder loop, and a command step's own `timeout:`. `spoolway dispatch --unattended` / `--attended` decides it for one run without editing this |
| `max_output_tokens` | `0` | Output tokens one **unattended** run may spend before the dispatcher stops starting work; `0` is no ceiling. Ignored when `enabled` is off — an attended run has you and `ctrl-c`. Output alone of the four token classes, for the reason the board's footer counts it: it tracks work done rather than context carried. Reaching it starts no further lane and lets whatever is live finish; the queue keeps its place for the next run |
| `max_cost_usd` | `0.0` | Dollars one **unattended** run may spend before the dispatcher stops starting work; `0.0` is no ceiling. `max_output_tokens`'s own counterpart in money, priced the same way every other cost figure is — see [Cost accounting](cost.md) — a model neither `[models]` nor either price table prices contributes nothing to the sum, never estimated. Set alongside `max_output_tokens` and whichever ceiling is reached first stops the run; either alone is enough |
| `skip_blocked_lane` | `true` | Whether clearing a block on an *agent* step carries the task past the step it blocked on, on the grounds that the unblocker did that step's work, or hands it back to that step instead — a command step is always handed back to itself, whatever this says. See [Staffing `blocked`](pipelines.md#staffing-blocked) |
| `blocked_agent` | `claude` | Which `[agents.*]` profile staffs `blocked` in an unattended run. `Pipelines::assemble` builds the `blocked` step from these five keys for every pipeline that does not declare its own; a pipeline may override `agent`, `model`, `effort`, `session` and `prompt` for its own `blocked` step, and nothing else |
| `blocked_model` | `claude-opus-5` | The model that profile runs, staffing `blocked` — see `blocked_agent`. Starting an unattended run with this blank is refused: with nobody staffing `blocked`, a run has no way to clear one |
| `blocked_effort` | *(blank)* | How hard that model thinks, staffing `blocked` — see `blocked_agent` |
| `blocked_session` | `true` | Whether the lane staffing `blocked` carries its own earlier session forward, the same as a step's `session:` — see `blocked_agent` |
| `blocked_prompt` | `unblocker` | Prompt the lane staffing `blocked` runs — see `blocked_agent` |

## `[update]` — the release notice

```toml
[update]
check = true
```

Whether spoolway tells a person at a keyboard that a newer release is out. The check reads a
cached answer and never waits on the network: a lookup older than a day refreshes it in the
background, for the next command rather than this one. Nothing is printed inside a lane, under
`--json`, or when output is not a terminal, so only a person at a keyboard ever sees the
line — and a binary npm did not install is told to upgrade the way it was installed, never to
run a command that would refuse. `SPOOLWAY_SKIP_VERSION_CHECK=1` is the same switch for one
machine, without editing this file.

## `[calibrate]` — how far back `spoolway-calibrate` reads

```toml
[calibrate]
window = "14d"
```

How far back `spoolway-calibrate` reads: archived tasks finished inside the window, and the
ledger entries beside them. Takes the same duration forms `spoolway eval --since` does — `30d`,
`36h`, `90m` — and round-trips through `spoolway config set calibrate.window 30d`.

A project that raises this past `retention.days` silently loses the archived tasks
`spoolway-calibrate` reads, once `retain` sweeps them out of `archive/` — neither side warns.

## `[retention]` — how long a byproduct directory keeps what it holds

```toml
[retention]
days = 30
```

| Key | Default | Meaning |
|---|---|---|
| `days` | `30` | How many days an entry sits in a byproduct directory before it is deleted, read off the entry's own modification time. `0` keeps everything forever — every install's behaviour before this key existed |

The split between what this ages out and what it never touches is fixed in code, not
configurable per directory: `system-prompts/`, `commands/`, `tracking/`, `headless/`,
`scratch/` and `archive/` are swept once an entry passes `days`; `queue/`, `pending/`,
`worktrees/` and `plans/` hold work in flight and are never swept, at any age.

`scratch/` and `headless/` are the exception inside that first group. An entry there is
named for a task, and the sweep loads the queue once and spares any entry whose leading
task id still names a file in `queue/`. This holds whatever stage the task sits on,
`paused` and `blocked` included, since those are the stages a task can rest on for longer
than `days`. So `spoolway resume` always finds a paused or blocked lane's scratch tree and
headless record intact. Only once the task is archived do its scratch directory and its
headless record age out like anything else.

Archiving a task also reclaims its leftovers straight away, without waiting for `days`.
Its hook run files under `tracking/`, its command-step run files under `commands/`, and the
per-session home an agent that mints its own session id was given are all removed when the
task moves to `archive/`. One consequence is on the board: `failure_count` only counts a
`<task> · <event>` key whose task is still in the queue, so a failed hook from an archived
task stops showing as "N hook failures" even on a home where an older build archived that
task without reclaiming its files. A `fetch` run file is keyed on an issue reference rather
than a task id, so nothing reclaims it and it only ages out; a failed `spoolway issue show`
still counts on the board.

Only directories under `~/.spoolway/<project>/` are ever swept. `.spoolway/prompts/` in the
checkout holds the project's tracked prompt templates, and nothing here touches it, however
old a template file is.

The sweep runs at most once per process, gated by the same `Once`-and-budget shape
`src/scratch.rs` uses for its own test-fixture sweep, and stops at a deletion budget so a
long-neglected home drains its backlog over several runs rather than stalling the first command
that trips it.

Sweeping `archive/` has one consequence worth knowing: `queue add` resolves a `depends_on`
against the queue and the archive, so a dependency that finished more than `days` days ago can
no longer be named. The refusal says so, naming the age, whenever `retention.days` is above
zero; with it at `0` the same missing dependency is reported the plain way, since sweeping
cannot be why.

## `[prices]` — how stale the shared price table may be before it is mentioned

```toml
[prices]
max_age_days = 30
```

| Key | Default | Meaning |
|---|---|---|
| `max_age_days` | `30` | How many days the active shared price table may be before `spoolway doctor` notes it. `0` turns the note off entirely — the table is never mentioned, however old it gets |

The age is taken from whichever table actually answered model lookups: the machine-wide
`~/.spoolway/model-prices.json` when it parses, the built-in table otherwise. It is reported,
never acted on — a `note` in `spoolway doctor`, never a fetch and never a failed check. The note
fires only once the table is past the limit, so a table inside it is never mentioned, and `0`
means the note never fires at all. Making the table current stays the explicit
`spoolway models refresh`, however old it has become. See [Pricing](cost.md#pricing).

```toml
[pipeline_gen]
pipeline_agent = "claude"
pipeline_model = ""
pipeline_effort = ""
pipeline_auto = false
pipeline_loop_default = 1
pipeline_local_models = false
```

What `spoolway pipeline gen` opens, and what it hands the `spoolway-pipeline` skill's
generation procedure once the session starts — see [`spoolway pipeline
gen`](cli-reference.md#spoolway-pipeline-gen---plan-path).

**`pipeline_agent`** and **`pipeline_model`** are which `[agents.*]` profile the session runs
as and which model it runs. `pipeline_model` ships blank, and blank refuses the command — a
generation session with nothing to say about what it runs is not one worth opening.
**`pipeline_effort`** is handed straight to that profile's effort flag, the same way a step's
own `effort:` is; blank means the kind's own default.

**`pipeline_auto`**, **`pipeline_loop_default`** and **`pipeline_local_models`** are printed
to the session as one line of preferences, which the skill takes as already answered rather
than asking about them again. **`pipeline_loop_default`** is also the budget every loop a
generated pipeline writes starts at; it binds generation only — `assets/pipelines/*.yml` keep
whatever numbers they already have, and this key never reaches back to change them.

There is no `prompt` key here. The `spoolway-pipeline` skill is the whole brief for this
session, deliberately — a second file layered on top would only be one more place the
instructions could disagree with each other.

## `[agents.*]` — who runs a step

```toml
[agents.pi]
kind = "pi"
session_reuse_ctx = 0
session_blocked_ctx = 0

[agents.claude]
kind = "claude"
session_reuse_ctx = 0
session_blocked_ctx = 0
permission_mode = "auto"
```

No `concurrency` line under `pi`, and that is the setting: absent means this profile asserts no
cap of its own. `spoolway config set agents.pi.concurrency 4` writes one if you want it, and
setting it back to `0` takes the line out again. `config get` still answers `0` for it either
way — a key whose unset form is its absence is still a key you can ask about.

No `permission_mode` line under `pi` either, for the same reason: `pi` runs its tools with
nothing gating them, so there is no mode for the key to hold. **`permission_mode`** is present
on every profile whose kind offers one — `claude` and `codex` among the shipped three — and it
holds the mode a lane is actually started with, not a placeholder for it: `claude`'s ships
`"auto"` and `codex`'s ships `"never"`, each its kind's own first, strongest-unattended mode.
`spoolway config set agents.claude.permission_mode bypassPermissions` changes it; `""` is
refused by name — a config that refuses to load cannot be fixed by the command that fixes
configs, so `Config::load` never yields one, and the only way to type a blank is by hand.

Covered in full on [Agents and models](agents.md), including every argument placeholder and
every supported kind. The argv itself — `--model {model}`, `--append-system-prompt
{prompt_file}`, and so on — is not a config key at all: it is fixed per `kind` in the binary's
`agent::ADAPTERS`, because every flag in it is spoolway addressing its own CLI with paths and
ids only spoolway computes.

Three profiles ship, each named after the kind it runs: `pi`, `codex` and
`claude` — see [Profiles](agents.md#profiles) for why a profile is named for its binary
rather than for a role. A fresh scaffold keeps only the one you plan in: `spoolway init`
retains just that profile and drops the others, so a freshly initialized project carries a
single `[agents.*]` row. All three remain the built-in defaults an existing project keeps. `pi` and `codex` run against a local server and share one set of
limits; `claude` is a cloud kind with no local option — see [Concurrency and the model
server](agents.md#concurrency-and-the-model-server) for why no shipped profile asserts a
harness cap of its own. `codex` is referenced by no
shipped step and exists so that pointing a step at that CLI is an edit to its `agent:`
rather than a profile somebody has to write first. A profile is a fact about *this machine* — which binary, under
what limits, how long before it is judged stuck. **Which model runs, and how big
that model's window is, are not here any more** — a profile running several steps could only
ever name one model for all of them. `model:` moved onto the step (see [Pipelines](pipelines.md)),
and a model's window moved to `[models]` below, beside its price.

**`session_reuse_ctx`** is the bound on a step whose `session:` asks for its prompt's earlier
conversation back — see [A step that carries its own
session](dispatcher.md#a-step-that-carries-its-own-session). The step's own `session:` says only
*whether*; this says *how far*, per profile, because how large a session may be is a fact
about where a lane runs rather than about which step wants it. It is a percentage of the
model's `context_window`, 1..=100, and is refused outside that range with the range named in
the message. `0` — the default on every shipped profile — is off: a carried session is never
refused on size, so a `session: true` step reuses whenever its store is warm enough without
needing a window to measure against. A nonzero value is `1..=100`. Serialised even at its
default, so an existing `.spoolway/config.toml` gains it on its next save with nothing typed
by hand.

**`session_blocked_ctx`** is the ceiling on a *running* lane's size, checked on every dispatch
pass rather than only at launch. It is a percentage of the model's `context_window`, the same
reading `session_reuse_ctx` compares against, but taken from a session that may still be
mid-turn rather than one already settled. `0` — the default — is off: nothing watches a live
lane's size at all. When a lane's last completed turn exceeds it, the dispatcher stops the
lane, records its usage, and escalates its task to `blocked` rather than let it compact away
the `spoolway report` contract or degrade quietly as its window fills. The reading is only
taken at turn boundaries, so the ceiling can be overshot — a lane at 79% can end its next turn
well past 100%.

It must be set above `session_reuse_ctx` when both are set: a task blocked at or below the
reuse threshold would have the very session that just blocked it carried right back in on the
next visit, over the size that tripped the ceiling, and block again immediately. With either
guard off there is no threshold to sit above, so the check holds only when both are nonzero.
`spoolway
config set` refuses either edit that would put the two the wrong way round:

```
$ spoolway config set agents.claude.session_reuse_ctx 40
$ spoolway config set agents.claude.session_blocked_ctx 30
error: `session_blocked_ctx` (30) must be above `session_reuse_ctx` (40) — a task blocked below
  the reuse threshold carries the same session back and re-blocks
```

`spoolway doctor` warns separately when a profile sets `session_blocked_ctx` but its pipeline
steps run against a model that resolves to no `context_window` — the ceiling never fires for
that profile, and the run still proceeds.

The second bound a carried session used to face — whether it was still worth resuming once its
prompt cache had gone cold — is not here any more. It is `models.<glob>.session_reuse_idle`
now, a fact about the model rather than the profile running it; see [Retired:
`agents.<profile>.session_reuse_uncached`](#retired-agentsprofilesession_reuse_uncached) below
and `session_reuse_idle`'s own entry under `[models]`.

An old `agents.*.model`, `agents.*.context_window`, or `agents.*.args` still parses, and is
dropped on the next save.

## Retired: `agents.<profile>.env`

`agents.<profile>.env` used to export extra environment into a lane's pane before it started —
the one lever that let two profiles of the same kind differ, a second `claude` pointed at
another endpoint, say. Every shipped table was empty from the day it was written:
every agent it fronted already has a config file of its own for exactly that job, so a lane's
environment is computed now — which task, which repo, which step, its kind's own template,
a session home for a kind that mints its own id — never configured. A project that had filled
one in moves it to that agent's own config, or exports it before starting the dispatcher.

An old `agents.<profile>.env` table still parses, and `Config::load` prints a note naming the
profile the first time it finds one that was not empty; either way it is dropped on the next
save.

## Retired: `agents.<profile>.session_reuse_uncached`

Two settings used to govern one decision — whether a carried session was still worth resuming
once its prompt cache had gone cold — and the second was inert unless some model declared a
lifetime, which every doc that mentioned it had to spend a sentence on. That lifetime is the
whole of it now: `models.<glob>.session_reuse_idle`, below. Leaving it unset already says what
setting `session_reuse_uncached = true` used to; the capability lost is "I know this model's
lifetime and want to resume regardless", spelled the same way.

An old `agents.<profile>.session_reuse_uncached` still parses, and is dropped on the next save.
`spoolway doctor` names it for a project that still writes it, against the models that profile's
steps actually run.

## Retired: `[effort]`

A step's `effort:` used to name a *tier*, and `[effort.tier_models]` said which model each tier
ran as — the whole reason it existed was that a step could not otherwise name its own model.
Now that it can, `effort:` means what it says: how hard that model thinks, handed straight to
the flag its agent kind carries an effort on. There is nothing left for a machine-level setting
to answer, so `[effort]` is gone. See [Effort](agents.md#effort) for what a step writes
instead.

An old `[effort]` table still parses, and is dropped on the next save.

## Retired: `blocked_on_write` and `blocked_on_overreach`

`blocked_on_write` used to name path globs, repo-relative, that no lane on any step of any
pipeline could write; a pipeline and a step could each name more, and the three lists
unioned. `blocked_on_overreach` was its sibling, scoped to one task instead of the whole
project: on, a lane whose changed paths fell outside its own task's `touches` blocked the
same way. Both were checked at the same moment — a lane reporting — over the same
`git status --porcelain` set in its worktree, and both are gone: `git status --porcelain`
never lists a git-ignored path, and every runtime file either check was written to protect
was git-ignored, so the only paths either could ever match were this project's own tracked
files. See [Reach](concepts.md#reach).

Nothing inside spoolway replaces them. Confinement, where a project wants any, is the
person's own agent settings — outside this repository entirely; see [what confines a
profile](agents.md#what-confines-a-profile).

An old config still naming either key still parses, and both are dropped on the next save.
A pipeline file still naming `blocked_on_write:` at either level still parses too, the key
ignored the same way.

## Retired: `[sandbox]`

Lanes were once confined by the kernel — Landlock, through a shim on every lane's `PATH` —
and `[sandbox]` was the table that widened what they could reach: `mode`, `read`, `write`,
`deny`, `ports`, `domains`. The whole layer is gone, along with the per-profile `sandbox` and
`sandbox_extension` switches, and nothing inside spoolway replaces the one goal it was
actually load-bearing for — see [Retired: `blocked_on_write` and
`blocked_on_overreach`](#retired-blocked_on_write-and-blocked_on_overreach). An old
`[sandbox]` table still parses and is dropped on the next save.

The honest summary of what that means is in [What confines a
profile](agents.md#what-confines-a-profile): a lane can do what you can do.

## Retired: `[paths]`

Every directory it named was never a choice a project got to make — a project never chooses
where its own state lives, and every caller already goes through the accessors on `Repo`.
They are constants now, beside `STATE_DIR` in `src/config.rs`, and there is nowhere left to
put them. See [Runtime state](#runtime-state) for where those constants actually resolve to
today.

`docs` used to be the exception, carried across onto `docs.path` — a real directory in *your*
project, not a fact about `.spoolway/`. That target is gone too now; see [Retired:
`[docs]`](#retired-docs) below.

An old `[paths]` table still parses, and the whole of it — `docs` included — is dropped on the
next save.

## Retired: `[docs]`

`[docs]` used to say what the archivist wrote a domain document as, and where: `docs.path`
named the directory, `docs.format` chose between Markdown and a project's own shape. Both are
gone with `src/docs.rs` — spoolway keeps no notion of documentation in the binary at all now.
Where documents live, and what each covers, is `assets/prompts/archivist/PROMPT.md`'s to say,
and it goes looking for its own documents rather than being handed a computed list. A project
whose `docs.path` was ever set away from the default gets a note, not a migration: the value
moves into that prompt's prose by hand, the same edit a project makes to restyle any other
prompt.

An old `[docs]` table still parses, and `Config::load` prints a note — naming the file, and
pointing at the prompt — the first time it finds one whose `path` or `format` was ever set
away from the default; a table nobody touched says nothing. Either way, the table is dropped on
the next save.

## Retired: `[plans]`

A plan used to be the binary's to place: `plans.store` chose between three directories,
`plans.format` chose between two shapes, and `plans.template` named a skeleton's path — three
keys, none of them read any more. spoolway keeps no notion of a plan store at all now:
spoolway-plan writes one self-contained page — a stylesheet inlined into its own `<style>`
block, its two lockups as data URIs — wherever it is told to, by convention at
`~/.spoolway/<project>/plans/<YYYY-MM-DD>-<slug>.html`, stamped with the day the plan was cut.
**The binary never opens that page.** What it reads
is task documents, waiting in `~/.spoolway/<project>/pending/` — see
[Planning](planning.md#queueing-a-plan) — and `spoolway group list` groups the queue's own
tasks by the `group:` string each was given.

All three keys still parse in a config saved before this — read, and dropped on the next save,
the way `[effort]` and `[sandbox]` are.

## `[models."<glob>"]` — what a model costs, and how big its window is

```toml
[models."claude-opus-5"]
context_window = 1000000
input = 5.0
output = 25.0
cache_read = 0.5
cache_write_5m = 6.25
cache_write_1h = 10.0
session_reuse_idle = "5m"
```

Per million tokens, plus the window one session gets — both keyed by a glob over the model
name, matched by the same most-literal-wins rule as everywhere else spoolway matches a glob:
`claude-opus-5` beats `claude-*` for that one model.

**A zero is written by leaving the key out.** Only the fields you have actually set appear, so
a local model reads as the few lines that say something rather than as nine, five of
which would be rates saying a free model is free:

```toml
[models."*Qwen3.6-35B-A3B"]
context_window = 100096
slots = 3
exclusive = true
```

Nothing is lost by the omission — zero is what an absent field already meant, `config get`
answers `0` for one, and `config set` creates it on first write and removes it again when it
goes back to zero.

`session_reuse_idle` is how long a carried session may sit before `carried_session` refuses to
resume it — measured against the store's own age, not against anything a transcript declares.
Unset, and a session under this model is never refused for its age. Do not set it on a local
model: a llama.cpp cache is not on a timer, and any number here would be a fiction the
dispatcher acts on. Renamed from `cache_ttl`: a serde alias keeps an existing config parsing,
and the rename is written back on the next save. See [Cache warmth is a model's
fact](agents.md#cache-warmth-is-a-models-fact).

Three more fields on the same row are not prices either — how many lanes the model itself may
run at once, whether it shares a card with another model like it, and whether it runs on
hardware you own:

```toml
[models."your-local-model"]
slots = 1
exclusive = true
local = true
```

**`slots`** replaces its profile's `agents.<profile>.concurrency` for a step naming this model,
when set: `dispatch::start_lanes` counts live lanes by the model their step names rather than
by their profile, and caps against this figure instead. A profile's `concurrency` caps a
*binary* — how many `pi` processes this machine runs at once — but a local server's own limit
is per *model*: swap `llama.cpp`'s loaded weights for a smaller one and the number of parallel
slots changes with it, whatever the profile still says. Unset (`0`, the default), a model
falls back to its profile's `concurrency` — which the two local profiles do not set, so a
local step capped by neither runs unlimited lanes and `spoolway doctor` names it. This is a
different key from a step's own
`slot:` in the pipeline file, which only says whether that step consumes one of the count set
here — one character apart and answering different questions, so set the config key and not the
pipeline one.

**`exclusive`** says this model never runs alongside a *different* model also carrying
`exclusive = true`, however many slots either has and whatever the waiting step's own `slot:`
says — a statement that a local server holding one set of weights at a time evicts one when
the other is swapped in, not a request for more concurrency. A candidate whose model is
`exclusive` waits while a live lane runs a different `exclusive` model, and the wait names the
resident model. Set `slots` on an `exclusive` model too, or it falls back to its profile's
`concurrency` for how many of *itself* may run at once — which on a local profile is no cap at
all. `spoolway doctor` reports a model carrying `exclusive = true` with no `slots`.

**`local`** marks a model as running on hardware you own rather than a metered API. It sizes
nothing and caps nothing. It never reaches the scheduler, so a run takes the same decisions
whether it is set or absent.

Its one effect is on the dispatcher's board. When a task in the queue routes through a step
naming this model, the footer carries a standing line below the slots block. The line names
the model and reminds a person that sessions they start by hand are not counted against the
slot pool. It stands in for a slot count that cannot see those sessions.

spoolway never infers `local`. A model that sets `slots` or `exclusive` describes the same
kind of hardware, but so does a local model nobody has sized. So `spoolway doctor` only notes
a `slots` or `exclusive` model that has not set `local`, rather than assuming either way. The
board line is drawn nowhere else — not under `--plain`, and not into a pipe.

`spoolway config set models.'<glob>'.slots 3` writes any of these fields the same way any
other `[models]` key is set.

`context_window` is what `agents.<profile>.session_reuse_ctx` takes its percentage *of* at run
time — so a step with `session: true` decides whether to carry a conversation over by measuring
the last turn against this number. A model with no window here is never sized, and the pass
opens fresh rather than carrying blind — but only when a reuse ceiling is set and so needs a
window to measure against. With `session_reuse_ctx` off (`0`), no window is needed and a
carried session is reused on its age alone. Sizing a task against a session is not
this any more — `spoolway-tasks` estimates size and complexity directly, on the split
ballot. It does read `spoolway models` for one thing: the window on whichever model its chosen
pipeline gives the implementing step, which is what decides whether it cuts larger tasks or
smaller ones. That is a lookup, not a token count of the task.

**This is what spoolway believes, not what the server serves.** Nothing here is asked of the
provider, and nothing checks the two agree — an agent's own status bar may well report a
different figure, because it got its number from the agent and the server rather than from
here. Keep them in step yourself: if this table is wrong, the reuse arithmetic is wrong in
whichever direction it is wrong. For a local model the honest number is usually the server's
per-slot window rather than the model's nominal one — llama.cpp's `--ctx-size` divided by
`--parallel`, say, not the 128k the model card claims.

`[models]` ships **empty**, and empty now means *covered*: every model your pipelines name is
already in the built-in table (once one exists), so a row here only corrects a price, or
describes a local model nobody publishes. An unpriced model is reported as unpriced rather than
counted as free — see [Cost accounting](cost.md).

Renamed from `[pricing]`, which held the five rates alone. An old `[pricing]` table is read
into this same field and written back under the new name.

## `[issue_tracking]` — a hook fired on four task events

```toml
[issue_tracking]
hook = ""
project_key = ""
on_fail = ""
key_in_names = false
```

Where a project's issue tracker lives, so the dispatcher can tell it about a task's arrival at
`queued`, `blocked`, `paused` or `done` — the four states reserved in `src/pipeline.rs`, so no
pipeline's own `run:` step can ever fire on one of them. This table is the only place a tracker
is named; no pipeline file mentions one.

| Key | Default | Meaning |
|---|---|---|
| `hook` | *(blank)* | A bare filename, resolved inside `.spoolway/hooks/` in the checkout — never a path. Blank runs no hook at all and changes nothing about a task's four events, or about `spoolway issue show`. Anything that is not one plain filename — a name with a separator, one naming `.` or `..`, or one carrying a Windows drive prefix like `C:` — is refused the same way blank is, and `spoolway doctor` reports it |
| `project_key` | *(blank)* | Opaque to spoolway — `owner/repo` on github, a project key on jira, whatever the hook script itself expects. Never parsed or validated; it reaches the script exactly as this holds it, as `SPOOLWAY_PROJECT_KEY` |
| `on_fail` | *(blank, reads as `ignore`)* | What a non-zero hook exit does to the task it ran for. `ignore` records the failure and changes nothing else. `pause` additionally holds the task: on `queued` it lands on `paused`, and on `done` it stays out of the archive. A failure on `blocked` or `paused` is only ever recorded, whichever this holds — both are already stopped for a person |
| `key_in_names` | `false` | Whether the tracker's own key rides into the names `queue add` generates. Off means every `group:`, `branch:` and worktree directory is named exactly as it is with no tracker at all. On, and with the `open` hook answering a `slug=` line, `queue add` prefixes the `group:`, the `branch:` (`task/<slug>-<id>`) and the worktree directory with that slug, so `git branch` shows which issue a branch belongs to. spoolway parses no tracker identifier of its own — the slug comes from the hook, and is kept only if it passes the same `check_id` alphabet a task id does |

`spoolway doctor` reports a finding when `hook` is set but `project_key` is blank — a hook
that runs with nothing to hand it opens no ticket, ever, on any of the four events:

```
!  .spoolway/config.toml: [issue_tracking] names github.sh but
   project_key is empty — no ticket will ever open. Set it, or
   clear hook to switch issue tracking off.
```

It reports nothing when all three keys are blank, and nothing when all three are set. A
separate check reports a `hook` that is not a bare filename the same way.

It also reports a configured hook whose own script carries no `fetch` branch anywhere in its
text — the shape every install had before this event existed — naming the script and what to
add:

```
!  .spoolway/config.toml: [issue_tracking] names github.sh, whose script has
   no `fetch` branch — `spoolway issue show` needs one to read an issue back
   out of the tracker. Add a case for `SPOOLWAY_EVENT=fetch` the way the
   shipped samples do, or regenerate one with `spoolway init` into a fresh
   directory to copy the branch across by hand.
```

It reports one more standing gap when `key_in_names` is on but the configured hook's own
script never writes a `slug=` line anywhere in its text. `spoolway update` never rewrites a
hook a project already has, so a project that turned the flag on without adding the line gets
no prefix at all, silently, on every `queue add`:

```
!  .spoolway/config.toml: [issue_tracking] key_in_names is on, but github.sh
   never writes `slug=` — groups, branches and worktrees will be named without
   a prefix. Add a slug= line the way the shipped samples do, or clear
   key_in_names.
```

The hook is spawned detached, the same shape a pipeline's own `run:` step uses — see [What a
pass does](dispatcher.md#what-a-pass-does) — so no pass ever waits on it, once per task per
event. It runs with `SPOOLWAY_TASK`, `SPOOLWAY_EVENT`, `SPOOLWAY_FROM`, `SPOOLWAY_SOURCE`,
`SPOOLWAY_GROUP`, `SPOOLWAY_BRANCH`, `SPOOLWAY_TITLE`,
`SPOOLWAY_TASK_FILE`, `SPOOLWAY_PROJECT_KEY`, `SPOOLWAY_GROUP_SIZE` and
`SPOOLWAY_EPIC`/`SPOOLWAY_TICKET` — the last two read off the task's own frontmatter, blank
where neither is set — all set. The `done` event of a group's last still-open task additionally
carries `SPOOLWAY_GROUP_LAST=1`.

Every run's output is written to one log under `~/.spoolway/<project>/tracking/`, named the
way a command step's own log is. The board prints one line, `issue_tracking: N hook
failures — see tracking/`, whenever any hook has failed — under either `on_fail` — and nothing
at all when none has.

### `open` — a fifth event, run by `queue add` itself

`spoolway queue add` calls the same hook script for a fifth event, `open`, before it writes
anything: once for every document in the batch that does not already set a `ticket:`, in
dependency order, so a task's own call always has its dependencies' ticket ids in hand. A
document already naming a `ticket:` is reported `kept`, and no hook runs for it. Unlike the
four task events above, `open` runs synchronously — `queue add` waits for the script to exit
before it decides whether to queue anything at all — and it is not a task stage, so nothing a
pipeline declares can collide with it. `fetch`, below, is the other synchronous event.

The script gets the same `SPOOLWAY_TASK`, `SPOOLWAY_SOURCE`, `SPOOLWAY_GROUP`,
`SPOOLWAY_BRANCH`, `SPOOLWAY_TITLE`, `SPOOLWAY_TASK_FILE` and `SPOOLWAY_PROJECT_KEY` as the
four task events, `SPOOLWAY_EVENT=open`, and three more of its own: `SPOOLWAY_GROUP_SIZE` (how many
documents in this batch share the task's `group:`), `SPOOLWAY_EPIC` (the group's epic id if one
has already been created — the first task's call to answer with one, or a document that already
named one — blank otherwise), and `SPOOLWAY_DEPENDS_TICKETS` (the ticket ids of this task's own
`depends_on`, space-separated, read off the batch or off the queue). It answers by writing
`epic=` and `ticket=` lines, either optional, to the file named in `SPOOLWAY_OUT`; spoolway
writes whatever it finds there into the document's own `epic:` and `ticket:` frontmatter keys,
opaque and unparsed, exactly as `group:` and `source:` already are.

Two more answer lines are optional, `slug=` and `url=`. A hook writing neither reads back
blank, exactly as a missing `epic=` does.

`slug=` is the short tracker handle for `key_in_names`. It is consulted only when that flag is
on, and kept only if it passes the same `check_id` alphabet a task id does — lowercase
letters, digits and hyphens. A slug that fails prints `issue_tracking: slug ... is not a
valid name ... — ignored` and the batch queues with no prefix rather than refusing.

`url=` is the issue's web address. It is stored on the task as a `url:` frontmatter key
whatever `key_in_names` holds — it is there for later work to use and is displayed nowhere
now — and kept only if it parses as an absolute `http` or `https` URL. One that fails prints
`issue_tracking: url ... is not an absolute http(s) address — dropped` and the batch still
queues.

When `key_in_names` is on and a slug is kept, `queue add` applies the prefix in a pass of its
own, after every `open` call has returned and before any task is saved. It prints one line per
distinct prefix, naming the slug it applied and the new group name.

One slug wins per group: the first non-blank answer. The winner is seeded from tasks already
in the queue as well as from this batch. So a second `queue add` naming the bare `group:`
strips the recognised `<slug>-` prefix off its queued siblings before the epic lookup, and
reuses the first call's epic instead of opening a second one.

Two more variables, `SPOOLWAY_EPIC_BODY` and `SPOOLWAY_TICKET_BODY`, name the paths of two
rendered files the script can read as the tracker issue's own body. They come from
`.spoolway/templates/tracking/epic.md` and `ticket.md`, which `spoolway init` seeds into the
project alongside everything else — yours to edit from that point on, and never touched again
by `spoolway update` — with every `${SPOOLWAY_*}` placeholder substituted for the value above
it names; a placeholder naming something the environment has no value for renders empty rather
than failing. A project that has deleted one of those two files gets a single line naming the
task instead of spoolway's own prose, so a project's tracker never carries words nobody asked
to see there.

A non-zero exit from any `open` call refuses the whole `queue add` — nothing in the batch is
queued — but every `epic:`, `ticket:`, `slug:` and `url:` a call before it already secured is
written back into its document in `pending/` first, so running the same command again resumes
rather than opening a second set for work already done.

### `fetch` — a sixth event, run by `spoolway issue show`

`spoolway issue show <ref>` calls the same hook script for a sixth event, `fetch`, to read one
issue out of the tracker rather than to open anything. It writes nothing: no task exists yet
when this runs, since reading an issue is usually the first step of turning it into one. The
same as `open`, this runs synchronously — the command blocks until the script exits before it
prints anything — and it is not a task stage either.

The script gets only `SPOOLWAY_EVENT=fetch`, `SPOOLWAY_REF` (the reference typed on the command
line, exactly as typed, and never parsed) and `SPOOLWAY_PROJECT_KEY` — none of a task's own
fields, since none exists yet. It answers by writing one JSON object to `SPOOLWAY_OUT`, carrying
`ref`, `url`, `title`, `state`, `labels`, `body` and `comments`; `spoolway issue show` parses
that object and reprints it, in the same key order the hook wrote it, on stdout.

`spoolway issue show` refuses by name, rather than running anything, when no hook is configured
at all, or when the configured hook's own script has no `fetch` branch anywhere in its text —
the same static check `spoolway doctor` runs, so an install whose hook predates this event is
told what is missing instead of reading back an issue with nothing on it.

### The shipped hook scripts

`spoolway init` writes every hook script it ships into `.spoolway/hooks/`, whatever the
project answered when asked which tracker it uses — turning tracking on later, or switching
trackers, is a `spoolway config set issue_tracking.hook` away, not a second `init`. Only one
pair can run on a given install: `github.sh` and `jira.sh` on Unix, `github.ps1` and
`jira.ps1` on a native Windows install — the same choice spoolway makes elsewhere between
running `sh -c` and running PowerShell. `spoolway update` never touches one of these once `init` has
written it, the same rule a prompt or a task skeleton already follows.

Both scripts take their tracker's project from `SPOOLWAY_PROJECT_KEY`, answer `fetch` by
reading one issue back as JSON, create the epic and ticket on `open`, comment with the task
file on `blocked` and `paused`, and close or transition the epic on the `done` event that
carries `SPOOLWAY_GROUP_LAST=1`. Otherwise they exit zero without contacting anything.

On `open`, both scripts also hang the epic they just created — or the single ticket, for a
group of one that opened no epic — under the issue named in `SPOOLWAY_SOURCE`, doing nothing at
all when that value does not name an issue in this project: most of the time `SPOOLWAY_SOURCE`
names a plan page's path rather than an issue, and spoolway never parses that field either way,
so this is the script's own job to recognise.

On `open`, all four samples also write a `slug=` and a `url=` line, keyed on the epic when
there is one and the ticket otherwise. `github.sh`/`github.ps1` take the issue number off the
end of the URL `gh` returned and prefix it `gh-`, so the slug starts with a letter, and pass
that same URL through as `url=`. `jira.sh`/`jira.ps1` lowercase the key — `PROJ-12` becomes
`proj-12` — and build the browse link from the site read back off the work item the way
`fetch` does it.

`github.sh`/`github.ps1` call `gh`, which a project already has logged in if it uses GitHub at
all. `gh` has no sub-issue command, so the parent link goes through `gh api`'s `sub_issues`
endpoint directly, and no attachment command either, so the task file travels inline, folded
into the comment body. The sub-issue link is matched by the tail of the issue's URL — anything
ending `/<repo>/issues/<n>` for this project's own `repo` — rather than a fixed `github.com`
host, so it also recognises an issue on a GitHub Enterprise Server install.

`jira.sh`/`jira.ps1` call `acli`, and the `.sh` pair additionally needs `jq` on `PATH` to read
a ticket's key back out of `acli`'s JSON output — a create call that comes back with no key
exits loudly rather than writing an empty `epic=`/`ticket=` line. Three things want a project's
own Jira site to confirm, named in the script's own header comment: that `workitem create
--json` puts the new key under `.key`, that the project spells its link type `Blocks` and its
epic status `Done`, and — for `fetch` — that `workitem view --json` names the summary, status
and label fields the way the script's own `jq` filter reads them. `hang_under`'s own link type,
`Relates`, is this codebase's guess at one every site ships, and the one edit to make where a
site differs. The Jira pair does not send the task file: attaching it would need a
Jira site, an account email and an API token, a second set of credentials spoolway does not
hold, and `acli` itself has no upload command of its own — its `workitem attachment` group
only lists and deletes. The Jira comment names the task and leaves the file where it already
is, in the queue.

## When the config will not parse

Every command dies on a config error — including the one you would reach for to find it. So
`spoolway doctor` is the exception: it reports the parse error with the line it is on, and
still runs the checks that read no settings at all.
