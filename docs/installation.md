---
domain: installation
covers: ["src/install.rs", "src/update.rs", "src/release.rs", "src/release_notes.rs", "CHANGELOG.md", "src/assets.rs", "src/ask.rs", "src/gitignore.rs", "npm/**", "scripts/build-npm.mjs"]
---

# Installation and setup

Installing spoolway, scaffolding a project, checking that it would actually run, and keeping
it current as spoolway changes.

## Installing

```
npm install -g spoolway
```

The npm package is a thin wrapper around a prebuilt binary — there is nothing to compile and
no Node program running underneath. Prebuilt targets:

| Platform | Architectures |
|---|---|
| Linux | x64, arm64, and a musl build of x64 |
| macOS | Apple Silicon, Intel |
| Windows | x64 (experimental — see below) |

From source instead:

```
cargo install --path .
```

Or by hand: every release attaches one archive per target — `spoolway-<triple>.tar.gz`, a
`.zip` for Windows — with a `SHA256SUMS` beside them. Unpack it and put the binary on your
`PATH`; it is the same executable npm would have installed, and it depends on nothing else.

## What spoolway needs

| Requirement | What it means |
|---|---|
| **A multiplexer** | herdr or tmux, unless you run the headless backend, which needs no multiplexer at all. On Linux, headless also wants `setsid` to detach a lane from the dispatcher |
| **Agent binaries** | `pi` for local lanes, `claude` for the review step — or whatever your own configuration points at |
| **git** | And `gh`, if your pipeline opens pull requests |

None of these is checked at install time. `spoolway doctor` checks all of them against the
pipeline you have actually configured, which is the useful version of the question.

## Scaffolding a project

Run the initialiser inside a git repository:

```
spoolway init
```

At a terminal it asks five things before it writes anything — the coding agent you plan in,
the issue tracker to name in `[issue_tracking]` and the project its tickets file into, the
agent kind your local pipeline steps run on, and the model those steps name. Fixed choices
use an arrow-key selector; move with ↑/↓ and accept with Enter or Space. Each has a flag
(`--provider`, `--tracker`, `--project-key`, `--agent`, `--model`), and giving it means the
question is not asked. Run without a terminal — a script, CI, a pipe — nothing is asked and
the defaults are taken: claude, no tracker, pi, and the model placeholder left standing for
`doctor` to report.

On success, `init` prints only that the skills were installed and the project was initialized,
plus any actionable warning. Running it again to add another provider's skills omits the project
message. The narrower `spoolway install <provider>` command uses the same concise skills message.

`--agent` settles **one profile**, `agents.pi`, and is not a project-wide choice of agent.
A pipeline step names an agent *profile*; a profile names a kind; and a project may define as
many profiles of as many kinds as it likes — the config `init` writes already carries a second
one, `agents.claude`, running claude. So a pipeline can run one step on pi, the next on codex
and the next on claude, whatever this question was answered with. Add or change a profile
afterwards with `spoolway config set`; see [Agents and models](agents.md).

It creates, in one pass:

| Path | What it is |
|---|---|
| `.spoolway/config.toml` | Every setting, with its defaults and its explanatory comments |
| `.spoolway/pipelines/` | The sample workflow's two pipelines, documented inline. A template — extend them, or replace them outright |
| `.spoolway/prompts/<name>/PROMPT.md` | The six prompts that staff the sample workflow, as Markdown. Yours from this moment; no update touches them |
| `.spoolway/prompts/archivist/assets/` | The document skeletons the archivist fills — one per domain, one per project. Yours too |
| `.spoolway/templates/tasks/` | One task skeleton per shipped pipeline |
| `.spoolway/templates/task-log.md` | What belongs under each heading spoolway appends to a task file. Yours from this moment; no update touches it |
| `.spoolway/templates/tracking/` | The `epic.md` and `ticket.md` bodies a tracker hook renders for a new issue |
| `.spoolway/hooks/` | The tracker hook scripts — `github.sh`/`jira.sh` on Unix, `github.ps1`/`jira.ps1` on a native Windows install — written whichever tracker you answered, or none at all. See [`[issue_tracking]`](configuration.md#issue_tracking--a-hook-fired-on-four-task-events) |
| `~/.spoolway/<project>/project.toml` | The pointer that claims this project's name — see below |
| The provider's skills directory | The five pipeline skills, in the convention of whichever provider you chose. See [the pipeline skills](#the-pipeline-skills) below |

An existing file is kept rather than overwritten. `--force` overwrites.

Run `init` again in a project that already has a config and it installs skills and changes
nothing else — which is how you add a second provider. `--agent` and `--model` are reported
as not applied rather than quietly dropped: rewriting a config a project has been running on
is not what a second `init` is for. `spoolway config set` is, and `--force` takes the
shipped config back.

**Everything spoolway writes while it runs — the queue, the archive, plans, lane
bookkeeping, the usage ledger — lives outside the checkout, at
`~/.spoolway/<basename of the checkout>/`.** `init` claims that name by writing the pointer
file above; running `init` again in a different checkout that would claim the same name is
refused, naming both. Where the checkout a name was registered to is gone but its archive or
queue is still there, `init --take-over` claims the name and keeps that state rather than
deleting it — refused without the flag, because state that old is not one to walk into by
accident. Delete the whole directory to forget every task, plan and lane a
project has ever run — nothing under it is tracked, and nothing in the checkout points back
at it besides the name. See [Runtime state](configuration.md#runtime-state).

`init` never writes to your `.gitignore` any more — runtime state moved out of the checkout,
so there is nothing left for a rule to keep out of git. A project set up before the move has
a marked block in its `.gitignore`, between `# >>> spoolway >>>` and `# <<< spoolway <<<`;
`spoolway update` takes it back out, once, and writes no rules of its own in its place.

### The pipeline skills

`init` installs these — you only run the command below to add a second provider, or to take
newer skills without touching anything else:

```
spoolway install codex
```

This writes five skills for a coding agent, so that planning, reshaping the flow and
diagnosing a project are procedures the agent already knows rather than things you explain
each time:

| Skill | What it does |
|---|---|
| `spoolway-plan` | Turns one goal into a reviewable plan page, then — once you approve it — cuts the task breakdown into the project's pending directory as one document per task. Queues nothing; `spoolway queue`, the screen, does that |
| `spoolway-tasks` | The task-cutting procedure `spoolway-plan`'s own step 7 invokes: settle the pipeline, decompose against the window its implementing step runs on, offer the shape, write and prove the documents. A second skill that cuts a breakdown calls it too, rather than carrying its own copy |
| `spoolway-pipeline` | Turns a workflow you describe into a pipeline, or changes one you have, and writes any prompt its steps need — against the contract `spoolway prompt contract` prints |
| `spoolway-doctor` | Runs every read-only check, collects the findings into one report, and changes nothing until you pick |
| `spoolway-calibrate` | Reads a window of archived tasks and the spend ledger, turns what actually cost loops and money into findings with the fix's exact lines, and walks them one at a time so the person sends each to `spoolway-tasks`, to `spoolway-plan`, or nowhere |

Four of these are human-invoked only — none of them fires on its own. `spoolway-tasks` is the
exception: it carries no `disable-model-invocation`, because it has to stay reachable from
inside another skill's own procedure, not only from a person's own prompt. Each lands as
`<provider's directory>/<name>/SKILL.md`: one directory per skill, which is the layout all
three providers discover. None brings any file beside it — `spoolway-pipeline` used to ship an
annotated pipeline to copy from, but that template is now printed by `spoolway pipeline
contract` instead, so the skill fetches it at runtime rather than carrying a copy that can
drift from the binary that enforces the format.

**One of these skills says almost nothing in the session.** `spoolway-plan` produces a page
and then prints one line: the absolute path to it. Everything it worked out is on the page,
where it can be read, revised and reread — and anything still undecided is marked in violet,
so a document with none of it on it is one with nothing left to decide. `spoolway-pipeline`
writes no page at all: a pipeline is a short YAML file and a prompt is prose, and both are
read faster in the files themselves than in any description of them.

**Three providers, one set of skills.** The content is shared; a provider decides only where
the directory goes, because all three converged on the same layout:

| Provider | Skills go in |
|---|---|
| `claude` | `.claude/skills/` |
| `codex` | `.agents/skills/` |
| `pi` | `.pi/skills/` — loaded only once the project is trusted, so answer pi's trust prompt or start it with `--approve` |

Which one you plan in is a separate question from what your lanes run: planning in Claude
Code while the pipeline's local steps run on pi is the arrangement spoolway itself is
developed under. `--provider` is the first; `--agent`, and `spoolway agent list`, are the
second.

`gemini` is not on the list. It has no adapter row either — nothing about it has been
settled against the binary, and a guessed row is worse than no row.

Start a fresh agent session after installing, so it picks the skills up.

### Checking the setup

```
spoolway doctor
```

`doctor` answers one question: would this pipeline actually run on this machine, right now?
It checks, among other things:

- the config parses, and the pipelines validate against it
- the task dependency graph can be traversed
- you are on a real branch, not a detached HEAD
- each base branch the queue names is publishable, and whether a remote exists at all
- each configured agent binary is on `PATH`, has a model set, and accepts the permission
  mode you asked for
- every prompt named by a step exists, and reads clean against the step that runs it

Findings come in two weights. **Problems** set a non-zero exit code. **Notes** do not, because
half of them are a project's settled choice — but the other half is a lane running with no
prompt, a profile naming a kind that cannot be launched, or a step whose model no `[models]`
entry prices.
The `spoolway-doctor` skill exists to read the notes, since they go unread when the only
thing anyone looks at is the failure count.

`doctor` is also the one command that still runs when the config file does not parse. Every
other command dies on that error, including the one you would reach for to find it; `doctor`
reports the error with the line it is on, and still runs the checks that read no settings.

A pipeline file that does not parse is handled the same way. `doctor` and `spoolway pipeline
check` report the load failure as one failed check and run everything that does not need the
pipeline graph, rather than exiting before they can tell you which file is broken.

## Keeping a project current

spoolway writes files into your repository, and a few of them carry something a machine
reads that has to agree with the binary. The update command takes what a newer spoolway
writes without touching what you wrote:

```
spoolway update --dry-run     # what it would change, and nothing else
spoolway update               # take it
```

It is also what takes the newer spoolway. When a release is out and npm is what installed
this binary, `update` runs `npm install -g spoolway@<version> --ignore-scripts` first and
then hands over to the binary it just installed, so the files written are that release's own.
At a terminal, a successful handover finishes by showing the old and new versions, a compact
digest of what changed, every migration that applies, and links to the full notes. A one-release
update includes three to five highlights; a jump across releases keeps one theme per release
instead and points to `spoolway whats-new --since <old-version>` for the full history. That
digest is deliberately absent from dry runs, file-only updates, unmanaged or dispatcher-blocked
upgrades, and non-terminal output.

The same history is compiled into the binary and needs neither a checkout nor a network call:

```
spoolway whats-new                 # this installed release in full
spoolway whats-new --since 0.1.0   # every later embedded release, oldest first
```

The second form refuses anything other than an `X.Y.Z` version and says explicitly when no
embedded release follows it. The source record and its format contract live in
[`CHANGELOG.md`](../CHANGELOG.md); every binary version must have a valid section there.
Two things stop the install and neither stops the files:

| What stops it | What happens |
|---|---|
| **npm did not install this binary** | `cargo install`, an unpacked archive, a Nix store path. You are told which release is out and upgrade it however you installed it — spoolway will not guess |
| **a dispatcher is running** | Replacing the executable underneath a live run kills it mid-pass, so the binary is left alone until the run ends |

You are told a release is out by one line, on stderr, before whatever you actually typed:

```
Update available: 0.2.0. Run "spoolway update"
```

It is for a person and nobody else — nothing is printed inside a lane, under `--json`, or when
output is not a terminal. The check reads a cached answer and never waits on the network: an
answer older than a day is refreshed by a background process for the *next* command, so no
command is ever slower for it. Turn it off for a project with `update.check = false`, or for
one machine with `SPOOLWAY_SKIP_VERSION_CHECK=1`.

What it does, file by file:

| File | What is replaced |
|---|---|
| Pipeline file | Only the key reference in its header, between `# >>> spoolway >>>` and `# <<< spoolway <<<`. Your title line, your steps and your comments are copied through unread — and a pipeline file without the markers is left completely alone |
| `.gitignore` | Only its own marked block, if a project set up before runtime state moved out of the checkout still has one — taken back out, once, and nothing written in its place |
| **Prompt** | **Nothing. Ever.** |
| **Document skeleton** | **Nothing. Ever.** |
| **Task skeleton** | **Nothing. Ever.** |

Plan pages are not in this table at all: `init` never places a plan skeleton in your project,
so there is nothing here for `update` to bring forward.

It never merges anything. A block you have edited by hand stops the update on that file
rather than being overwritten. `spoolway update --replace <path>` gives those edits up
deliberately, saving what was there beside it first. The report says only which files were
written, so a file it declined to touch is named by `spoolway doctor` rather than there.

A pipeline's key reference is the one block that does not ask. It is not a shape you filled
in, it is a table of what *this binary* understands — every line in it is a claim about the
program — so an edit inside the markers is a claim that has stopped being true, and it is
rewritten every time, edits and all. That is `config.toml`'s bargain, one fence at a time:
see [Pipelines](pipelines.md#per-step-keys).

**Prompts and task skeletons are outside this entirely.** Both are prose you own outright,
with nothing generated inside them to keep current: how a lane finishes comes from the
system prompt the dispatcher composes at lane start, and a task's frontmatter is serialised
from a struct in the binary on every save. So an upgrade cannot disturb a word you wrote in
either, and changing what a task records is a spoolway release rather than a migration in
your repository.

The trade is that a sharper default in a later release does not reach you on its own.
`spoolway prompt check` is what tells you when your prose has fallen behind the CLI — it
validates every `spoolway` command and flag in a prompt against the real command tree.

For any file, `spoolway update --replace <path>` writes the shipped one over yours, saving
what was there beside it first. That is also how to take an updated default prompt or
skeleton on purpose. Paths are named one at a time: losing a file you asked for is a
decision, losing eleven you forgot about is an accident.

`spoolway doctor` reports when any of this is outstanding.

Separately, `spoolway pipeline contract` prints the pipeline format itself, including an
annotated blank pipeline — the starting point for one of your own, read straight off the
struct and the validation that enforces it rather than kept in sync by hand.

## Platform notes

Nothing on any platform confines a lane. There was a Landlock layer on Linux once, and it is
gone — see [what confines a profile](agents.md#what-confines-a-profile) — so what is left
below is genuinely everything the platforms differ on.

### Linux

The reference platform, and the only one the headless backend runs on: it detaches a turn
with `setsid` and reads a lane's liveness out of `/proc`.

### macOS

Everything works except the headless backend, which wants a `setsid` binary and a `/proc` to
read. Run lanes under herdr there — it refuses at lane start rather than pretending.

### Windows

**Experimental.** It builds, the test suite runs on Windows, and lanes start — but it has had
far less real use than the Linux build.

- **Panes run PowerShell.** spoolway types a lane's environment at the pane's prompt, which is
  the only channel the multiplexer offers, so it emits PowerShell rather than POSIX syntax.
  Pointing the multiplexer's default shell at `cmd.exe` or Git Bash breaks that, and the
  symptom is a lane that starts with none of its environment set.
- **`spoolway init` writes the PowerShell hook pair**, `github.ps1` and `jira.ps1`, in place
  of the `.sh` pair a Unix install gets. See [`[issue_tracking]`](configuration.md#issue_tracking--a-hook-fired-on-four-task-events).
- **No headless backend**, for the same reason as macOS.
- **Command steps run all the same.** A `run:` line is emitted as PowerShell rather than `sh`,
  a step spawns detached through a process group and a job object in place of `setsid`, and its
  liveness is read the same way a lock file's owner is, rather than out of `/proc` — none of
  which needs the headless backend above, so `handover` and `checks` cross the shipped pipeline
  here too.

Everything else is the same binary and the same commands. Running under WSL gets you the
Linux build, and is the better option if the headless backend matters to you.
