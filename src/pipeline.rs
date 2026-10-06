//! The pipeline definition: a graph of named steps a task moves through.
//!
//! This is data, not code. The dispatcher is a generic interpreter over it, so
//! adding a step, reordering the flow, or swapping which agent handles a step
//! is a config edit rather than a code change.
//!
//! Loaded from `.spoolway/pipelines/`, one file per pipeline: `default` for a
//! unit of work, and `bugfix` for a reproduce-first fix. A task picks one with
//! its `pipeline:` field.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// How long a command step may run when it names no `timeout:` of its own.
///
/// Half an hour, and the number is a judgement about which mistake costs more.
/// Too low kills work that was going to finish: a release build with a cold
/// cache, a full test suite and an integration run are all ordinary at ten or
/// twenty minutes, and a bound that reaps those turns a working pipeline into
/// an intermittent one — the worst kind to debug, because the command is
/// blameless. Too high only means a genuinely hung command sits there longer,
/// which costs one task's progress and nothing else: no worker slot, no model,
/// no other task held up.
///
/// So it is set above what real work takes rather than close to it, and a step
/// that knows better says so — `timeout: 2h` for a nightly, `timeout: 60s` for
/// a lint that should never take longer.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Directory under `.spoolway/` holding one file per pipeline.
pub const PIPELINE_DIR: &str = "pipelines";

/// The single file the directory replaced. Still read when it is the only
/// thing there, so upgrading spoolway never stops a project mid-run.
pub const PIPELINE_FILE: &str = "pipeline.yml";

/// The built-in pipelines, shipped as the starting point for a new project: one
/// unit of work, and a reproduce-first bug fix.
///
/// There were three. The closeout — document the plan, then merge it to main —
/// was a second pipeline with a task of its own, queued by a command; it folded
/// into `default` as two conditional steps, and then into the steps every task
/// already runs. What that deleted is worth naming: the file, `spoolway plan
/// close` and `plan_close()` behind it, its base-derivation blocker,
/// `SPOOLWAY_MERGE_INTO`, and the `when:` key that carried the condition.
///
/// Both pipelines here are unconditional: every step runs for every task, so
/// reading the file tells you the whole flow. `document` is per-task and scoped
/// to that task's own diff, and `handover` — the last step of both — ends at a
/// green pull request. Nothing is merged: a plan's stack is landed bottom-up
/// into the mainline by a person.
pub const BUILTIN_PIPELINES: &[(&str, &str)] = &[
    ("default", include_str!("../assets/pipelines/default.yml")),
    ("bugfix", include_str!("../assets/pipelines/bugfix.yml")),
];

/// Where the key reference sits in a pipeline file: between the two markers,
/// and nothing outside them is read.
///
/// A pipeline file's header is two things with different owners. The title line
/// on top says what *this* pipeline is for, which only the project can write.
/// The table under it lists every key this binary understands, which only the
/// binary can keep true — so it is fenced, and `spoolway sync` rewrites it.
/// A file without the markers has not opted in and is never written to.
pub const KEY_BLOCK: crate::skeleton::Region = crate::skeleton::Region::Comment(
    crate::assets::PIPELINE_KEYS_BEGIN,
    crate::assets::PIPELINE_KEYS_END,
);

/// That reference as this binary writes it, read out of the shipped `default`.
///
/// Read rather than declared, so the block a project is handed is the block the
/// shipped pipelines demonstrate it against. A constant beside them would be a
/// second copy, and the first thing to fall behind.
pub fn key_block() -> &'static str {
    let shipped = BUILTIN_PIPELINES
        .iter()
        .find(|(name, _)| *name == "default")
        .expect("a built-in pipeline named `default`");
    KEY_BLOCK
        .read(shipped.1)
        .expect("the shipped default pipeline carries the key block")
}

/// The built-in set, parsed. Falls out of [`BUILTIN_PIPELINES`] so there is one
/// copy of the text and no second definition to keep in step.
///
/// Test-only: production no longer falls back to the built-ins when a
/// project has none of its own (see [`Pipelines::load_impl`]), so the only
/// callers left are the `#[cfg(test)]` [`Pipelines::builtin`] and
/// [`Pipelines::shipped`] fixtures.
#[cfg(test)]
fn builtin_pipelines() -> Result<BTreeMap<String, Pipeline>> {
    BUILTIN_PIPELINES
        .iter()
        .map(|(name, raw)| {
            let pipeline = Pipeline::parse(name, raw)
                .with_context(|| format!("built-in pipeline `{name}`"))?;
            Ok((name.to_string(), pipeline))
        })
        .collect()
}

/// Every `<name>.yml` (or `.yaml`) in the pipeline directory, with the exact
/// path each one was read from, or `None` when the directory is not there at
/// all.
///
/// The path is carried out alongside the name and the raw text because a
/// caller cannot safely rebuild it: [`Pipelines::file_in`] always guesses
/// `.yml`, which is wrong for a project that wrote `.yaml` — see
/// `merge_private`, which prints this path in a refusal and records it as
/// [`Pipeline::private_file`], both of which have to name the real file.
///
/// `Some(empty)` and `None` both reach the same `no pipelines defined` bail in
/// [`Pipelines::load_impl`] now — there is no fallback left for either to opt
/// into. The split still matters for one thing: only `None` goes on to check
/// for the old single-file `pipeline.yml`, so a project carrying one gets that
/// migration message instead of the generic one.
fn read_pipeline_dir(dir: &Path) -> Result<Option<Vec<(String, PathBuf, String)>>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display()))?,
    };

    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading {}", dir.display()))?
            .path();
        let is_yaml = path
            .extension()
            .is_some_and(|ext| ext == "yml" || ext == "yaml");
        if !is_yaml {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        files.push((name.to_string(), path.clone(), raw));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Some(files))
}

/// What a prompt (or the dispatcher) says happened, which is all a step needs
/// in order to decide where the task goes next.
///
/// Prompts report an outcome, never a destination — that is what keeps them
/// reusable across differently-shaped pipelines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// The step's work succeeded.
    Pass,
    /// The work was attempted and did not meet the bar. Routes to `on_fail`.
    Fail,
    /// Something outside the agent's control is in the way. Routes to the
    /// pipeline's `blocked` step regardless of the current step.
    Block,
    /// Only means anything reported from `blocked` itself: nothing short of a
    /// person can clear this. `commands::report` refuses it from anywhere
    /// else, so [`Step::destination`]'s arm for it is never actually reached —
    /// kept only so the match stays exhaustive.
    Pause,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pass => "pass",
            Outcome::Fail => "fail",
            Outcome::Block => "block",
            Outcome::Pause => "pause",
        }
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Outcome {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pass" => Ok(Outcome::Pass),
            "fail" => Ok(Outcome::Fail),
            "block" => Ok(Outcome::Block),
            "pause" => Ok(Outcome::Pause),
            other => bail!("unknown outcome `{other}` (valid: pass, fail, block, pause)"),
        }
    }
}

/// The stage a task sits on between being queued and its first step running.
///
/// Built in rather than declared. Every pipeline used to open with a `kind:
/// wait` step, always first, always named `queued`, always the entry, and
/// nothing ever routed back to it — a state wearing a step's clothes. Worse, the
/// dependency gate lived inside that step's arm, so `depends_on` was honoured
/// only because every shipped pipeline happened to open with one. Built in, the
/// gate is unconditional.
pub const QUEUED: &str = "queued";

/// The stage a task that finished sits on. Reserved, not declared.
pub const DONE: &str = "done";

/// The stage a task that needs a person sits on. Reserved, not declared.
///
/// Together with [`DONE`] this is the whole of how a task can end, and the model
/// was already binary: `graph.rs` classifies any terminal that is not `blocked`
/// as done, which *releases dependents*. So a third declared terminal —
/// `superseded`, `rejected`, `abandoned` — would silently let downstream work
/// proceed on a task that never finished. Declaring terminals only ever created
/// room to declare one that misbehaves.
pub const BLOCKED: &str = "blocked";

/// The stage a task waits on for a person's approval. Reserved, not declared.
///
/// Where a gated step's pass goes. It is not an ending and not a failure: the
/// work is done, it went well, and [`Step::gate`] says a person decides whether
/// it goes any further. `spoolway resume` sends it on.
///
/// Distinct from [`BLOCKED`] because the two ask a person for opposite things.
/// A block is "something is in the way, and I could not finish"; the answer is
/// to clear it and resume *at the same step*. A pause is "I finished, and you
/// said you wanted to see this before it goes on"; the answer is to let it
/// *past* the step. Parking both on `blocked` would put a successful step in
/// the column a person scans for failures, and resume it into a lane that has
/// nothing left to do.
///
/// Not a terminal either: the work finished cleanly, so there is nothing
/// behind it that could ever fail to arrive — a person only has to let it
/// past.
pub const PAUSED: &str = "paused";

/// The moment a task leaves [`QUEUED`] for its entry step — an
/// `[issue_tracking]` hook event, not a stage: nothing ever writes this to a
/// task's own `stage:` field, so it never joins [`RESERVED`] the way
/// `QUEUED`, `DONE`, `BLOCKED` and `PAUSED` do. A pipeline step is free to
/// use this name — there is no stage for it to collide with — because
/// `on_pass`/`on_fail` never name it as a destination in the first place;
/// see `crate::dispatch::Dispatcher::route_reserved_stage`, which fires it
/// once, synchronously gated, at the same moment it already checks
/// `depends_on` for [`QUEUED`] itself.
pub const STARTED: &str = "started";

/// Stage names a pipeline may not give a step, because the dispatcher already
/// means something by them.
pub const RESERVED: &[&str] = &[QUEUED, DONE, BLOCKED, PAUSED];

/// The description every materialised `blocked` step carries — the same
/// wherever it runs, because `description` is not one of the five keys a
/// pipeline's own override may set. See [`Pipelines::assemble`].
const BLOCKED_DESCRIPTION: &str = "Staffed only in an unattended run. Reads why the task \
    stopped and either clears it — a pass carries the task on from the step it blocked on — \
    or, only when the thing genuinely cannot be done, pauses it for a person with the same \
    destination waiting. One turn either way: nothing routes back onto `blocked` itself.";

/// What the dispatcher does when a task sits on a step.
///
/// Derived from the keys the step carries rather than declared: `agent:` runs a
/// prompt, `run:` runs a command, `end: true` finishes the task. The keys
/// already partitioned perfectly, and a `kind:` beside them was a second
/// declaration the file had to keep consistent with itself — along with every
/// "you said kind X but wrote keys for Y" error that consistency check existed
/// to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepKind {
    /// Run an agent with a prompt, and route on the outcome it reports.
    Agent,
    /// The task stops here. Nothing is scheduled for it again.
    Terminal,
    /// Run one command line in the task's worktree, and route on what it exits
    /// with. No agent, no model, no worker slot — a build, a test run, a deploy
    /// script, anything a shell can start.
    ///
    /// Blocking by default: the task sits here until the process ends, and its
    /// exit code picks `on_pass` or `on_fail`. With `background: true` the task
    /// leaves on the same pass it started, and the process runs on; if it
    /// declares `on_fail`, a later pass that finds it exited non-zero routes
    /// the task there, wherever it has reached by then.
    Command,
}

impl StepKind {
    /// How the kind reads in a message about a step. There is no `kind:` key to
    /// match any more, so these name what the step *is* rather than what it
    /// would have been spelled.
    pub fn as_str(self) -> &'static str {
        match self {
            StepKind::Agent => "agent",
            StepKind::Terminal => "terminal",
            StepKind::Command => "command",
        }
    }
}

/// One node in the pipeline graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Stable identifier. This is the literal value written to a task file's
    /// `stage:` field, so renaming a step renames the stage.
    pub id: String,

    /// The task stops here. Nothing is scheduled for it again.
    ///
    /// A step with no `agent:`, no `run:` and no transitions *is* structurally
    /// terminal, so this could be inferred too. It is not, on purpose: a
    /// mistyped `agnet: local` would then silently become an ending rather than
    /// an error, and endings are worth declaring.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub end: bool,

    /// Human-facing one-liner, shown by `spoolway pipeline show`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Agent profile from `config.toml`'s `[agents.*]` that runs this step.
    /// Required for `kind: agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,

    /// Prompt file (without extension) under the prompts directory.
    /// Defaults to the step id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,

    /// The model this step runs. Required on every agent step — spoolway
    /// names no model of its own, so a step whose `model:` is missing or
    /// blank is refused by `pipeline check` and `doctor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// How hard `model:` thinks, handed straight through to the flag its
    /// agent kind carries an effort on — `--effort` for claude, dropped
    /// entirely for a kind with none, such as pi.
    /// Blank is the explicit form of no effort choice and sends no flag.
    ///
    /// A free string, not a closed set: which levels a model accepts is the
    /// model's own fact, changes when the model does, and a copy of that list
    /// in this binary would only go stale silently. `pipeline check` refuses
    /// this on a kind with no effort flag, and refuses the literal `auto` —
    /// spoolway resolved that itself once, against sensitive paths nothing
    /// computes any more, and nothing resolves it now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// Skills invoked at the top of this step's opening, one `/name` message
    /// per skill, in declaration order, each sent on its own ahead of the
    /// briefing — see [`crate::compose::opening_messages`]. A skill gets no
    /// argument, because whatever follows a `/name` on its line is passed to
    /// it, and the briefing is sent once, after them. Each message stays one
    /// line: herdr sends a multi-line message as a paste, and Claude Code
    /// never expands a slash command inside a paste.
    ///
    /// Written as one comma-separated line rather than a YAML list, because
    /// that is how it reads once turned into slash invocations: `skills:
    /// code-review, spoolway-doctor` names the same two things a lane is
    /// handed as `/code-review` and `/spoolway-doctor`. A leading `/` on a
    /// name is accepted and stripped, so a name copied from the skill listing
    /// and one copied from an existing invocation both work.
    ///
    /// No validation in the parse itself — `skills_field` below only splits
    /// and trims. A name holding whitespace is refused by `Pipeline::validate`,
    /// since that is a fact about the shape of the item and needs no config
    /// to see; a `skills:` on a command step, or one on an agent kind that
    /// cannot load skills, needs the pipeline's agents and is `pipeline_check`'s
    /// to refuse instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty", with = "skills_field")]
    pub skills: Vec<String>,

    /// Whether this step's conversation carries over from an earlier step
    /// in this task that ran the same prompt, rather than opening fresh.
    /// `false`, same as absent: every visit opens fresh, which is every
    /// step's behaviour today.
    ///
    /// A plain switch: the step says *whether*, and the agent profile that
    /// runs it may say *how far* — a nonzero
    /// `agents.<profile>.session_reuse_ctx` bounds how large the earlier
    /// session may be, as a percentage of the model's context window, and
    /// `agents.<profile>.session_reuse_uncached` says
    /// whether a session whose prompt cache has gone cold is still worth
    /// resuming. Both used to live here as the two shapes `session:` took —
    /// a bare `true` or a percentage — and moved to the profile because they
    /// answer "how", which is a fact about the agent running the step, not
    /// about the step itself; see [`crate::config::AgentProfile`].
    ///
    /// The prompt identifies the conversation rather than the step, because
    /// the cases that want this are not all same-step revisits —
    /// `implement` and `fix` are the same prompt doing the same work, and a
    /// session resumed under a prompt it was not opened with would be
    /// incoherent anyway: the prompt is the system prompt.
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        deserialize_with = "deserialize_session"
    )]
    pub session: bool,

    /// Whether running this step consumes one of the agent profile's
    /// concurrency slots. Cheap steps (a cloud review) can opt out.
    ///
    /// The profile's budget, and nothing else. A model's own `slots` and its
    /// `exclusive` are not opt-out-able here: `[agents.<profile>] concurrency`
    /// is a number about a harness, chosen for politeness, while
    /// `[models.<glob>] slots` is a number about a machine — how many of these
    /// weights the card actually serves at once. A step that talked its way
    /// out of the first would otherwise have talked its way onto a GPU that
    /// has no room for it.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub slot: bool,

    /// A person approves this step's work before the task goes any further.
    ///
    /// For a step that changes something without leaving a pull request behind
    /// — a deploy, a release, anything irreversible nobody would otherwise see
    /// first. A pull request *is* a checkpoint: somebody reads it and merges it,
    /// so gating the step that opens one is asking the same question twice.
    ///
    /// **Spoolway holds the gate by itself; the lane is only told a person will
    /// read its pane.** A gated lane runs and reports exactly like any other;
    /// what changes is what spoolway does with a `--pass` — it does not act on
    /// it. The task lands on [`PAUSED`] and waits for `spoolway resume`.
    ///
    /// It used to ask more of the lane than that: the opening prompt carried a
    /// paragraph asking the lane to stop and ask, and that paragraph was the
    /// entire enforcement. Spoolway's own half was real but passive — it
    /// exempted the lane from the silence clock and let it hold its slot — so a
    /// gate held only for a model that chose to honour it. Watched over three
    /// plan runs against a small local model, not one gate held: each lane read
    /// the instruction, decided the work was fine, reported a pass and the task
    /// moved on. A checkpoint that a model can talk itself out of is not one,
    /// and the model was carrying it because nothing else was.
    ///
    /// So that instruction is gone for good — a lane cannot approve its own
    /// work, and a mechanism that needs no cooperation cannot be talked out of
    /// anything. What is worth keeping is smaller: the lane is told, not asked
    /// to act on it, so it leaves anything viewable running instead of tearing
    /// it down and writes a short account for the person who really does read
    /// this pane.
    ///
    /// Holds in an unattended run too — a gate is a person's decision by
    /// design, and nobody staffing the run is a reason to wait longer for
    /// them, not a reason to let the pass through. See `unattended.enabled`.
    ///
    /// The gate belongs to the step, whoever does its work: an unblocker's
    /// `--pass` from `blocked` is held here exactly the same way, at the gate
    /// of the step named by the task's own `blocked_from` rather than of
    /// `blocked` itself. See `commands::report::route` and
    /// `commands::gate_hold`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate: bool,

    /// Step to move to on a `pass`. Absent means the task stays put.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_pass: Option<String>,

    /// Step to move to on a `fail`. Absent falls back to the pipeline's
    /// `blocked` step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fail: Option<String>,

    /// Most times this step may arrive with a task — by any route in,
    /// counting the first — before the next arrival is escalated instead of
    /// landing here again.
    ///
    /// An arrival is a lap, not a conversation. Counting conversations charged
    /// for the expensive event, a cold start, but a loop that keeps its session
    /// (`session: true`) went round free, and the bound stopped describing
    /// anything a person could reason about: the same three visits cost the
    /// same tokens whether one lap opened a conversation or three did, and only
    /// the lap count can be written down in advance and mean the same thing
    /// every run. What still bounds a conversation's own cost is
    /// an enabled `agents.<profile>.session_reuse_ctx`, on the agent profile,
    /// entirely separate from this.
    ///
    /// Counted on the step that carries it, not on the step that sends a task
    /// back — the same number the board draws as `↻`, read straight off
    /// [`crate::task::Task::rounds_at`]. Bounding the *sender*'s own route used
    /// to let a step reached from more than one place run once per sender
    /// times its own limit, and it let a hand `spoolway resume --stage` skip
    /// the count entirely by taking a route no `loop:` was watching. A map
    /// form (`loop: { fix: 2 }`) named the old per-route shape and is refused
    /// at parse now — see [`Pipeline::validate`].
    #[serde(default, skip_serializing_if = "Loop::is_unbounded")]
    pub r#loop: Loop,

    /// Retired spelling of [`Step::r#loop`], present only to be refused by
    /// name. Never `Some`: [`deserialize_retired_max_new_sessions`] fails on
    /// any value at all.
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "deserialize_retired_max_new_sessions"
    )]
    #[allow(dead_code)]
    pub max_new_sessions: Option<serde_norway::Value>,

    /// Retired spelling of [`Step::r#loop`], one hop further back, present
    /// only to be refused by name. Never `Some`: [`deserialize_max_rounds`]
    /// fails on any value at all.
    #[serde(default, skip_serializing, deserialize_with = "deserialize_max_rounds")]
    #[allow(dead_code)]
    pub max_rounds: Option<serde_norway::Value>,

    /// Retired `on_loop_max:` key, kept only so that a file still naming it is
    /// refused by [`refuse_retired_step_keys`] — which, unlike a
    /// `deserialize_with` on this field, knows which pipeline and which step
    /// the key is on, and a project whose files `spoolway sync` will not edit
    /// has to be told both. See that function for why it is neither a
    /// `deserialize_with` nor a check inside [`Pipeline::validate`].
    ///
    /// A spent loop budget parks on `blocked` and nowhere else now, so the key
    /// chooses nothing, and a key that is parsed and dropped is how a file goes
    /// on believing it was honoured — which is why
    /// [`crate::overrides::apply_step_patch`] refuses it on the override layer
    /// too, where this field's `skip_serializing` would otherwise drop it
    /// unmentioned.
    #[serde(default, skip_serializing)]
    pub on_loop_max: Option<serde_norway::Value>,

    /// The command line a `kind: command` step runs, and refused on every other
    /// kind. Run through the environment's own shell — `sh -c` on Unix, the
    /// platform's shell elsewhere — in the task's worktree, so a shebang, a
    /// pipeline or an `&&` all mean what they mean at a prompt.
    ///
    /// It is the operator's own command in a file only people write, and it
    /// runs no agent at all: this is a Makefile target, not a lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,

    /// How long this command may run before it is killed. Absent takes
    /// [`DEFAULT_COMMAND_TIMEOUT`].
    ///
    /// This is the one bound a command step has, and without it a blocking
    /// command that hangs parks its task forever: nothing else in a pass has an
    /// opinion about a process that is simply still going, and a hung build
    /// looks exactly like a slow one. What tells them apart is a number
    /// somebody wrote down.
    ///
    /// It bounds a background run too, but stopping one at its timeout is not
    /// a verdict `on_fail` can route on — the process is killed with no exit
    /// code left behind, the same as one interrupted any other way. What it
    /// stops is a process outliving the task that started it, on a task that
    /// never reaches cleanup because it blocked on the way.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_duration"
    )]
    pub timeout: Option<Duration>,

    /// Let the task move on while the command keeps running.
    ///
    /// If the step declares `on_fail`, a later pass that finds the run exited
    /// non-zero routes the task there — wherever it has reached by then, even
    /// a step further down the pipeline than this one. A run that exits zero,
    /// or is still going, changes nothing about where the task is. What the
    /// command wrote is in its log either way, and a run still going at
    /// cleanup is stopped with the task.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,

    /// Run this step's command detached, with no pane, exactly the way every
    /// command step used to run.
    ///
    /// `false`, the default, gives the command a pane of its own — split off
    /// the task's own tab, under the herdr backend — so a long suite like
    /// `scripts/e2e-pr.sh` is something a person can look at while it
    /// runs. A backend with no pane to offer, headless, runs it detached
    /// either way: this key only ever turns a pane *off*, never demands one
    /// a backend cannot give.
    ///
    /// A different axis from [`Step::background`]: that says whether the task
    /// waits, this says whether a person can watch. The two combine — a
    /// background command may still stand in a visible pane until it ends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub headless: bool,

    /// Run this step only on the last task of a chain. Every other task walks
    /// past it to `on_pass` without starting the command.
    ///
    /// For work that is about the stack rather than about one task in it. A
    /// plan is a chain of stacked pull requests, each branch rebased onto the
    /// one below, so the task at the top carries every change beneath it: a
    /// suite the whole stack has to pass is one run at the top, not one run per
    /// task over a subset somebody above will cover again.
    ///
    /// The question asked on arrival is **is any task still open above me** —
    /// another task of the same plan that depends on this one, directly or
    /// through others. Not "do I have dependents", which is what the retired
    /// `when: last` asked and why it went: that counted tasks already finished
    /// and archived, so nothing ever looked last.
    ///
    /// What falls out of asking it that way:
    ///
    /// - A **chain** names exactly one task, the top, whatever order the rest
    ///   finished in.
    /// - A **fan** names all of them. Nobody depends on anybody, so no branch
    ///   contains another and each pull request stands alone — every one of
    ///   them runs it, which is right rather than a degradation.
    /// - **Two leaves** name two tasks. Two leaves are two stacks, so two runs
    ///   is the answer, not the defect it was under `when: last`.
    /// - A task with **no plan** runs it. There is no chain to be last in.
    ///
    /// A command step's key. [`Pipeline::validate`] refuses it on an agent
    /// step: a lane walked past is a prompt that never reports, and the whole
    /// point here is work nobody has to judge.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub last: bool,

    /// Run this step only on a chain's declared root: a task whose own
    /// `depends_on` is empty. Every dependent task walks past it to
    /// `on_pass` without starting the command.
    ///
    /// `last:` asks whether anything is still open above a task, through the
    /// live graph, because an archived task's dependents have to keep
    /// counting it as finished. `first:` asks a simpler question that the
    /// graph cannot answer at all: a task's declared `depends_on` survives
    /// in its own file after whatever it named is archived, but the graph's
    /// edges are built from the open queue and drop that name the moment the
    /// dependency leaves it — see [`crate::graph`]. So this reads the task's
    /// own `depends_on` directly rather than asking the graph, and an
    /// archived dependency still keeps the dependent from being first.
    ///
    /// What falls out of asking it that way:
    ///
    /// - A **chain** names exactly one task, the root, whatever order the
    ///   rest were declared in.
    /// - A **fan** of independent roots names every one of them — none
    ///   declares a dependency, so none walks past.
    /// - A task with **no declared dependency at all** runs it. There is no
    ///   chain above it to have already run this for.
    ///
    /// A command step's key, exactly as `last:` is — [`Pipeline::validate`]
    /// refuses it on an agent step for the same reason: a lane walked past
    /// is a prompt that never reports. It also refuses a command step that
    /// declares both `first:` and `last:`, since a step naming both has no
    /// single command left to be, root or not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub first: bool,

    /// Let only one task at a time run this step's command. Every other task
    /// that reaches it waits on the step, unstarted — no pane, no run files,
    /// no timeout clock — until the run ahead of it has exited, and its own
    /// run starts on the first pass after that.
    ///
    /// For a command that shares something outside the worktree: a setup
    /// script creating a database, a suite binding one port. Two groups
    /// dispatched together used to run it at once, because a task's `Fresh`
    /// run never looked at any other task's. The dispatcher holds the run
    /// back rather than the script taking a lock of its own, since `flock`
    /// is not on macOS.
    ///
    /// "One at a time" is one step id, in one pipeline, in one project: a
    /// step of the same name in another pipeline does not hold this one. A
    /// run ahead that is still going holds the step even once its task has
    /// moved on, which is what a `background: true` run's task does — see
    /// [`crate::command_step::Runs::serial_holder`].
    ///
    /// A command step's key, like `last:` — [`Pipeline::validate`] refuses
    /// it on an agent step, whose lane is not a run the dispatcher can hold
    /// back for its turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub serial: bool,

    /// Retired: used to tear the task's worktree and branch down, and archive
    /// its file, on arrival at a declared terminal step — reaching the
    /// reserved `done` stage does this unconditionally now, at
    /// [`crate::dispatch::Dispatcher::clean_up`], so there was no second value
    /// this key ever chose between. Kept only so a file still naming it
    /// parses, the way `blocked_on_write:` below does: 0.1.0's shipped
    /// pipelines documented `cleanup: true` beside `end: true`, and refusing
    /// it stopped every routing command on a project that wrote what the
    /// docs told it to. Dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    pub cleanup: Option<serde_norway::Value>,

    /// Where an old `blocked_on_write:` on this step lands so an existing
    /// pipeline file still parses. Retired along with the check that read
    /// it — see [`crate::config::Config`]'s own absorbing field of the same
    /// name. `deny_unknown_fields` on [`Step`] would otherwise refuse a file
    /// that still names it. Dropped unconditionally on the next save.
    ///
    /// `pub(crate)` rather than private: [`crate::graph`]'s own tests build a
    /// [`Step`] as a literal, field by field, and this is the one field on it
    /// that is never meant to be set to anything but empty.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    pub(crate) blocked_on_write: Vec<String>,
}

/// Most times a step may arrive with a task, by any route in.
///
/// A bare number — the only form left, since a limit now counts arrivals at
/// the step that carries it rather than moves on the route that sent them, so
/// there is only ever one number to give a step, not one per route:
///
/// ```yaml
/// loop: 3
/// ```
///
/// [`Loop::Map`] is not a second way to write that number. It exists only so
/// the old per-route shape (`loop: { fix: 2 }`) still deserialises long
/// enough for [`Pipeline::validate`] to refuse it by name, with the pipeline
/// and the step it was found on — the same reason [`Step::on_loop_max`] and
/// [`Step::max_rounds`] still parse.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Loop {
    /// What a step that never wrote `loop:` gets. A separate case from a
    /// written zero so [`Pipeline::validate`] can refuse `loop: 0` instead of
    /// reading it as no limit, which it can only do if the two differ.
    #[serde(skip_deserializing)]
    #[default]
    Unset,
    /// The limit written in the file. Zero is refused by
    /// [`Pipeline::validate`]: a loop is 1 or more, and silently reading it as
    /// no limit would leave a cycle its author thought bounded running forever.
    Bare(u32),
    /// The retired per-route shape, kept only to be refused by name — see
    /// this type's own doc.
    Map(BTreeMap<String, u32>),
}

impl Loop {
    /// The limit on an arrival here, or `None` for no limit.
    pub fn limit(&self) -> Option<u32> {
        match self {
            Loop::Unset | Loop::Bare(0) => None,
            Loop::Bare(n) => Some(*n),
            // Never reaches a caller that acts on it: refused at
            // `Pipeline::validate` before anything downstream asks.
            Loop::Map(_) => None,
        }
    }

    /// Whether this bounds nothing at all, which is what is left out of a
    /// rendered pipeline file.
    pub fn is_unbounded(&self) -> bool {
        match self {
            Loop::Unset => true,
            Loop::Bare(_) | Loop::Map(_) => false,
        }
    }

    /// How this reads in `spoolway pipeline show`.
    pub fn describe(&self) -> String {
        match self {
            Loop::Unset => String::new(),
            Loop::Bare(n) => n.to_string(),
            Loop::Map(_) => String::new(),
        }
    }
}

/// The retired `max_new_sessions:` key, kept in the struct only so that a file
/// still naming it is refused by name.
///
/// `deny_unknown_fields` would refuse it anyway, with serde's own list of every
/// key a step may carry — which says the key is wrong without saying what
/// replaced it, and a reader who has just been handed thirty alternatives is no
/// closer to the one they want. The counter this bounds now counts laps of the
/// loop rather than fresh conversations, so the rename is the message: a file
/// that kept the old key would otherwise read as a bounded loop while bounding
/// something nothing reads any more.
fn deserialize_retired_max_new_sessions<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<serde_norway::Value>, D::Error> {
    let _ = serde_norway::Value::deserialize(d)?;
    Err(serde::de::Error::custom(
        "`max_new_sessions:` is now `loop:` — it bounds how many times this step may send a \
         task on to a given one, a lap of the loop, not how many fresh conversations that \
         cost. A step with `session: true` may re-prompt a live session as often as it needs; \
         what may bound a conversation's own size is `session_reuse_ctx` on the agent \
         profile, entirely separate from this. Rename the key, and consider raising the \
         number: a whole conversation was stingier than a lap now is",
    ))
}

/// The retired `max_rounds:` key, one hop further back, kept only so that a
/// file still naming it is refused by name rather than serde's own list of
/// every other key a step may carry.
fn deserialize_max_rounds<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<serde_norway::Value>, D::Error> {
    let _ = serde_norway::Value::deserialize(d)?;
    Err(serde::de::Error::custom(
        "`max_rounds:` is now `loop:` — it bounds how many times this step may send a task \
         on to a given one. Rename the key, and consider raising the number: what it \
         counts has changed twice since",
    ))
}

/// A `timeout:` in a pipeline file, written the way config.toml writes one.
///
/// The inner half is [`crate::config::human_duration`] — one spelling of a
/// duration across every file spoolway reads, rather than a second parser that
/// accepts `30m` in one file and not the other.
mod optional_duration {
    use super::Duration;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => crate::config::human_duration::format(*d).serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let Some(raw) = Option::<String>::deserialize(d)? else {
            return Ok(None);
        };
        crate::config::human_duration::parse(&raw)
            .map(Some)
            .map_err(serde::de::Error::custom)
    }
}

/// A `skills:` in a pipeline file, written as one comma-separated line rather
/// than a YAML list.
///
/// Free text rather than a list because a list is not how this reads once it
/// becomes slash invocations — one name per line either way, so the file's
/// own shape and the prompt's should not disagree about which is the natural
/// one. Round-trips back out the same way: `["a", "b"]` in memory becomes
/// `skills: a, b` on disk, same as it was typed.
mod skills_field {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(names: &[String], s: S) -> Result<S::Ok, S::Error> {
        names.join(", ").serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        let raw = String::deserialize(d)?;
        Ok(raw
            .split(',')
            .map(|name| name.trim().trim_start_matches('/').trim().to_string())
            .filter(|name| !name.is_empty())
            .collect())
    }
}

/// A `session:` in a pipeline file. `true` or `false` only now — the
/// percentage `session:` used to take moved to
/// `agents.<profile>.session_reuse_ctx`, so anything else here is refused
/// with a message naming where it went rather than serde's own "invalid
/// type" text about a Rust bool.
fn deserialize_session<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<bool, D::Error> {
    let raw = serde_norway::Value::deserialize(d)?;
    match raw {
        serde_norway::Value::Bool(b) => Ok(b),
        other => {
            let shown = match &other {
                serde_norway::Value::String(s) => s.clone(),
                serde_norway::Value::Number(n) => n.to_string(),
                _ => "?".to_string(),
            };
            Err(serde::de::Error::custom(format!(
                "`session: {shown}` is not `true` or `false` — the bound that used to live \
                 here now lives on the agent profile, as `agents.<profile>.session_reuse_ctx`"
            )))
        }
    }
}

/// Rendering a default back out is noise in a file somebody has to read, so
/// every key that has one is skipped when it still holds it. What is written is
/// what the pipeline actually says.
fn is_true(value: &bool) -> bool {
    *value
}

fn default_true() -> bool {
    true
}

/// What an absent `version:` reads as.
fn default_pipeline_version() -> String {
    "1.0".to_string()
}

fn is_default_pipeline_version(value: &str) -> bool {
    value == "1.0"
}

impl Step {
    /// What this step is, read off the keys it carries.
    ///
    /// `end: true` wins over everything: a step declaring the task stops here
    /// and also naming an agent is refused by [`Pipeline::validate`], and until
    /// it is, an ending is the safer reading.
    pub fn kind(&self) -> StepKind {
        if self.end {
            StepKind::Terminal
        } else if self.run.is_some() {
            StepKind::Command
        } else {
            StepKind::Agent
        }
    }

    /// Prompt file stem for this step.
    pub fn prompt_name(&self) -> &str {
        self.prompt.as_deref().unwrap_or(&self.id)
    }

    /// How many times a task may arrive here, by any route, before the next
    /// arrival escalates instead — or `None` if this step is unbounded.
    /// Counted against [`crate::task::Task::rounds_at`] for this step's own
    /// id, which is where `set_stage` banks an arrival the moment it is made
    /// — the same count the board draws as `↻`.
    pub fn arrival_limit(&self) -> Option<u32> {
        self.r#loop.limit()
    }

    /// Where a spent loop sends a task from this step: `blocked`, always.
    ///
    /// A step used to be able to name somewhere else with `on_loop_max:`, and
    /// across seven pipelines and twenty-eight bounded steps nobody ever named
    /// anything but `blocked` — a loop that will not converge has nowhere
    /// useful to go but a person, whatever it was looping over. The key is
    /// retired, and a file still carrying it is refused by name at parse, in
    /// [`refuse_retired_step_keys`].
    ///
    /// Still a method rather than a bare constant at every call site, because
    /// what a spent budget does is a property of the step, and the one caller
    /// that matters — [`crate::commands::apply_loop_budget`] — reads it as
    /// one.
    pub fn loop_exit(&self) -> &str {
        BLOCKED
    }

    /// How long this step's command may run. There is no "no limit" here, and
    /// deliberately: an unbounded command is the one thing a pass cannot notice
    /// has gone wrong.
    pub fn command_timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_COMMAND_TIMEOUT)
    }

    /// Where a task goes when this step reports `outcome`.
    ///
    /// `blocked` is reserved, used for an explicit block and as the fallback
    /// when a step declares no `on_fail`.
    pub fn destination(&self, outcome: Outcome) -> Option<&str> {
        match outcome {
            Outcome::Pass => self.on_pass.as_deref(),
            Outcome::Fail => Some(self.on_fail.as_deref().unwrap_or(BLOCKED)),
            Outcome::Block => Some(BLOCKED),
            // Never actually asked: `commands::report` intercepts a `blocked`
            // step's own outcome before it reaches this call at all. Same
            // fallback as `Block` purely so the match has one.
            Outcome::Pause => Some(BLOCKED),
        }
    }
}

/// A validated pipeline graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pipeline {
    /// Name of this pipeline, taken from the file it was read out of.
    ///
    /// `skip` rather than `default`, so that `deny_unknown_fields` refuses a
    /// `name:` key written into the file: the file name is the name, and a
    /// second spelling of it is one that can disagree.
    #[serde(skip)]
    pub name: String,

    /// Yours to raise when this pipeline changed enough to compare, in `x.y`
    /// form. Absent reads `1.0`.
    ///
    /// A plain `String`, read as written — `1.10` stays `1.10` rather than
    /// folding to the `1.1` an `f64` would read it as. spoolway never checks
    /// its format, never compares it against a previous run, and never bumps
    /// or warns about it: it is a person's own note to themselves, recorded
    /// onto every lane's ledger line so a later `spoolway eval` can group by
    /// it, and nothing more.
    #[serde(
        default = "default_pipeline_version",
        skip_serializing_if = "is_default_pipeline_version"
    )]
    pub version: String,

    /// What this pipeline is for, in a few sentences of free prose — read by
    /// a person choosing between pipelines, and by `spoolway-tasks`' own
    /// step 1 doing the same thing on their behalf.
    ///
    /// Free text rather than a closed set of tags, because the question it
    /// answers — "which tasks belong here" — is exactly the kind of judgement
    /// a short paragraph settles and a label cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Skeleton a task queued on this pipeline is written from, by name under
    /// the task-templates directory. Absent means the pipeline's own name, and
    /// a pipeline with no file of that name takes `default`.
    ///
    /// The escape hatch, not the mechanism: two pipelines that want one shape
    /// and neither of them called `default` is the case it exists for. Most
    /// pipelines should never name one, the same way most steps never name a
    /// `prompt:`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_template: Option<String>,

    /// Where an old pipeline-level `blocked_on_write:` lands so an existing
    /// pipeline file still parses. Retired along with the check that read
    /// it — see [`crate::config::Config`]'s own absorbing field of the same
    /// name. `deny_unknown_fields` on this struct would otherwise refuse a
    /// file that still names it. Dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    blocked_on_write: Vec<String>,

    /// Whether this pipeline's own file declared a `blocked` step, set only
    /// by [`Pipelines::assemble`] from what the file held right before it
    /// appended or merged the config-materialised one onto it.
    ///
    /// Never (de)serialized — like [`Self::name`], it is a fact about where a
    /// `Pipeline` came from, not something a file could state about itself —
    /// so a `Pipeline::parse` used directly, outside `Pipelines::assemble`
    /// (every test in this module, `install.rs`), always reads `false`
    /// regardless of what it declared. `spoolway pipeline show` reads it to
    /// mark the row `from config` or `overridden in <name>.yml`.
    #[serde(skip)]
    pub blocked_declared: bool,

    /// The file this pipeline was loaded from under `local/pipelines/`, for
    /// a pipeline that came from the private layer rather than the tracked
    /// `.spoolway/pipelines/` — set only by [`Pipelines::load_impl`]. `None`
    /// for a tracked pipeline. Read by `pipeline list` (text and `--json`)
    /// and `pipeline show` to mark the pipeline `private` and name the file
    /// it came from, so nobody mistakes it for a tracked pipeline of the
    /// same shape. See [`crate::local`].
    #[serde(skip)]
    pub private_file: Option<PathBuf>,

    pub steps: Vec<Step>,
}

impl Pipeline {
    /// The skeleton name this pipeline asks for, before the fallback to
    /// `default` that happens when no such file is on disk.
    pub fn task_template_name(&self) -> &str {
        self.task_template.as_deref().unwrap_or(&self.name)
    }

    /// Parse one pipeline file, taking its name from the file rather than from
    /// anything inside it.
    ///
    /// A `name:` key is refused by `deny_unknown_fields` — the field is
    /// `#[serde(skip)]` — which is the point: two places that could disagree
    /// about what a pipeline is called is one place too many.
    ///
    /// Test-only: production parses through [`parse_unchecked`] instead,
    /// deferring `validate()` to [`Pipelines::assemble`]; the callers left
    /// here are `builtin_pipelines` and every test that wants a pipeline
    /// straight from a validated string.
    #[cfg(test)]
    pub fn parse(name: &str, raw: &str) -> Result<Pipeline> {
        let mut pipeline: Pipeline = serde_norway::from_str(raw).context("parsing pipeline")?;
        pipeline.name = name.to_string();
        refuse_retired_step_keys(&pipeline)?;
        pipeline.validate()?;
        Ok(pipeline)
    }
}

// ---------------------------------------------------------------------------
// The four retired shapes' own messages, each written once: `validate` and
// `refuse_retired_step_keys` bail on the first with these, and
// `Pipeline::retired_shape_problems` collects every one — the same wording
// either way, since `scripts/e2e/suites/upgrade.sh` greps these exact
// strings and a copy that drifted from its original would break silently.
// ---------------------------------------------------------------------------

/// A step's `on_pass:` or `on_fail:` naming its own id.
fn self_route_message(step_id: &str, key: &str) -> String {
    format!(
        "step `{step_id}`: `{key}` names `{step_id}` itself — a step may not route back to \
         its own id. Send the failure to a step that leaves, or delete the step."
    )
}

/// A step's `loop:` still written as the retired per-route map.
fn loop_map_message(step_id: &str, by_route: &BTreeMap<String, u32>) -> String {
    let give = by_route
        .keys()
        .map(|to| format!("give `{to}` a `loop: <n>` of its own"))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "step `{step_id}` declares `loop:` as a map — a limit now counts arrivals at the step \
         that carries it. Delete it here and {give}."
    )
}

/// A step's `loop: 0`, which 0.7 ran as no limit. The fix that keeps that
/// meaning is deleting the line, not `loop: 1`, so the message says so.
fn loop_zero_message(step_id: &str) -> String {
    format!("step `{step_id}` has loop: 0 — a loop is 1 or more; delete `loop:` for no limit")
}

/// A step still declaring the retired `on_loop_max:`.
fn on_loop_max_message(step_id: &str) -> String {
    format!(
        "step `{step_id}` declares `on_loop_max:` — a spent loop budget now always parks on \
         `blocked`, so the key no longer chooses anything; delete it."
    )
}

impl Pipeline {
    /// The step a task starts on once its dependencies are in.
    ///
    /// The first one in the list, rather than a key naming it. It already was,
    /// in every pipeline anyone had written — and the list is ordered for
    /// scheduling ("later steps outrank earlier ones, so work already in flight
    /// finishes before anything new starts"), which puts the entry at the bottom
    /// of the priority order regardless. The two meanings agree, so inferring
    /// one from the other costs nothing.
    ///
    /// The trade to know: reordering steps for priority would move the entry
    /// with them. Aligned today, but it is one list serving two purposes.
    pub fn entry(&self) -> &str {
        self.steps.first().map(|s| s.id.as_str()).unwrap_or(QUEUED)
    }

    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }

    /// Look up a step, with an error naming the valid ones.
    pub fn require_step(&self, id: &str) -> Result<&Step> {
        self.step(id).with_context(|| {
            format!(
                "`{id}` is not a step in pipeline `{}` (valid: {})",
                self.name,
                self.step_ids().join(", ")
            )
        })
    }

    pub fn step_ids(&self) -> Vec<&str> {
        self.steps.iter().map(|s| s.id.as_str()).collect()
    }

    /// The step a task moves to when `from` passes, following `on_pass`.
    ///
    /// Every step in a pipeline runs for every task on it, so this is one hop
    /// and not a walk. `None` means nothing runs after this one: the chain
    /// ends, and the task is done when this step passes.
    pub fn next_running_step(&self, from: &str) -> Option<&str> {
        let next = self.step(self.step(from)?.on_pass.as_deref()?)?;
        Some(next.id.as_str())
    }

    /// Every step still ahead of `from`, following `on_pass` link by link
    /// until nothing more runs. Where [`Pipeline::next_running_step`] answers
    /// one hop, this is the whole walk — what the board's RECENT ticker draws
    /// as the chain following the step a task just arrived at.
    ///
    /// Only `on_pass`: a step's `on_fail` is a different question, and one
    /// this never had to answer either, since a chain drawn forward from a
    /// pass is read the same way whichever step it starts from. Guarded
    /// against a cycle — no shipped pipeline has one, but this walks whatever
    /// a task file and a pipeline document say right now, and a malformed
    /// pair must end the walk rather than loop forever.
    pub fn pass_chain(&self, from: &str) -> Vec<&str> {
        let mut chain = Vec::new();
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut current = from;
        while let Some(next) = self.next_running_step(current) {
            if !seen.insert(next) {
                break;
            }
            chain.push(next);
            current = next;
        }
        chain
    }

    /// A step's raw position in this pipeline's own step list, counting from
    /// zero at the entry step. There is no override — a project that wants a
    /// different order writes its steps in that order.
    ///
    /// Meaningful only within one pipeline: this project's pipelines are not
    /// all the same length, so a raw index cannot be compared between two of
    /// them on its own. The dispatcher's candidate sort turns it into steps
    /// left before comparing across pipelines — see
    /// [`crate::dispatch::Candidate::steps_left`], which is where "later
    /// steps outrank earlier ones" actually happens.
    pub fn priority(&self, id: &str) -> i32 {
        self.steps
            .iter()
            .position(|s| s.id == id)
            .map(|index| index as i32)
            .unwrap_or(0)
    }

    /// Reject a graph that would strand tasks: unknown targets, duplicate ids,
    /// missing entry/blocked steps, or agent steps with no agent.
    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            bail!("pipeline has no steps");
        }

        let mut seen = HashSet::new();
        for step in &self.steps {
            // A step id is a path component before it is a graph node: it names
            // this step's prompt, its headless record and its command output.
            // See [`crate::config::check_id`], which is the same rule a task id
            // is held to.
            crate::config::check_id("step id", &step.id)?;
            if !seen.insert(step.id.as_str()) {
                bail!("duplicate step id `{}`", step.id);
            }
        }

        // Nothing may claim a name the dispatcher already means something by —
        // except `blocked`, which a pipeline may now declare as an ordinary
        // agent step, staffed only in an unattended run. `queued`, `done` and
        // `paused` stay off limits: nothing routes a lane to those, and
        // `graph.rs` still reads a bare `blocked` (undeclared here) as the
        // dispatcher's own parked state.
        for step in &self.steps {
            if step.id != BLOCKED && RESERVED.contains(&step.id.as_str()) {
                bail!(
                    "step `{}` uses a reserved stage name (reserved: {}) — those are states \
                     the dispatcher already means something by, not steps to declare",
                    step.id,
                    RESERVED.join(", ")
                );
            }
        }

        // A `skills:` item is a bare name, never a name plus arguments — the
        // comma is what separates skills, so whitespace inside one item can
        // only mean it was meant to be two, or copied from somewhere that
        // wrote it differently. Caught here rather than left for the slash
        // invocation it becomes: a name with a space in it still renders,
        // just as a line no agent harness would expand as one command.
        for step in &self.steps {
            for name in &step.skills {
                if name.chars().any(char::is_whitespace) {
                    bail!(
                        "step `{}` names skill `{name}`, which holds whitespace — a `skills:` \
                         item is a bare name, never a name plus arguments",
                        step.id
                    );
                }
            }
        }

        // A task starts on the first step, so a first step of `blocked` parks
        // every task the moment it is queued: its pass is read from where the
        // task stopped, and a task that never ran anything stopped nowhere.
        if self.entry() == BLOCKED {
            bail!(
                "`{BLOCKED}` is the first step — a task would start blocked; put a working \
                 step first"
            );
        }

        // `loop: 0` is not "no limit": a step with no `loop:` says that.
        // Without this refusal the cycle check below would count a written
        // zero as a bound, while [`Loop::limit`] reads it as no limit at run
        // time, so a cycle bounded only by `loop: 0` would pass and then run
        // forever.
        for step in &self.steps {
            if step.r#loop == Loop::Bare(0) {
                bail!(loop_zero_message(&step.id));
            }
        }

        // A transition may name a declared step or either reserved terminal.
        let known = |id: &str| id == DONE || id == BLOCKED || self.steps.iter().any(|s| s.id == id);

        for step in &self.steps {
            for (label, target) in [("on_pass", &step.on_pass), ("on_fail", &step.on_fail)] {
                if let Some(target) = target
                    && !known(target)
                {
                    bail!(
                        "step `{}`: {label} points at unknown step `{target}`",
                        step.id
                    );
                }
            }

            // `blocked` routes itself, in one turn: a pass is read from where
            // the task stopped rather than from this step. For an agent step
            // that is one step past there, on the unblocker's word that the
            // work is done; a command step is always handed back to itself,
            // whatever kind of pass it was. Anything else (`--fail`,
            // `--block`, or `--pause`) parks the task on `paused` for a
            // person, never back onto `blocked` itself, and a person resuming
            // it hands it back to the step it blocked on rather than past it —
            // see `commands::report::resume_road`. Each of the keys that
            // would otherwise say one of those things is refused by name,
            // pointing at what actually decides it instead of leaving a
            // reader to wonder why the graph disagrees with the file.
            if step.id == BLOCKED {
                if step.on_pass.is_some() {
                    bail!(
                        "step `blocked` declares `on_pass:` — where its pass goes is read from \
                         the step the task blocked on, not from here: past that step for an \
                         agent step, always back onto it for a command step; delete `on_pass:`"
                    );
                }
                if step.on_fail.is_some() {
                    bail!(
                        "step `blocked` declares `on_fail:` — a fail or a block from `blocked` \
                         parks the task on `paused` for a person, never back on `blocked` \
                         itself; delete `on_fail:`"
                    );
                }
                if step.gate {
                    bail!(
                        "step `blocked` declares `gate:` — nobody approves a block clearing, \
                         the task just carries on from the step it blocked on; delete `gate:`"
                    );
                }
                if step.end {
                    bail!(
                        "step `blocked` declares `end: true` — it always sends the task on from \
                         the step it blocked on, or back to `blocked`, never stops it; \
                         delete `end:`"
                    );
                }
                // The five keys above route or gate the step, which `blocked` never does. These
                // three are not wrong the way those are — a command, a slot opt-out and a loop
                // bound all mean something on an ordinary step — but `blocked` is materialised
                // from `[unattended]`'s own keys, and a pipeline's override is only the five
                // `Pipelines::assemble` actually merges: `agent`, `model`, `effort`, `session`,
                // `prompt`. Anything else here would silently do nothing once assembled, which
                // is worse than refusing it by name.
                //
                // `description:` belongs on this same list — it is not one of the five either —
                // but it cannot be checked here: `assemble` writes the canonical description
                // onto the materialised step itself, and `validate()` runs again on that
                // already-assembled state every time `pipeline check` or `doctor` calls it. A
                // check here would then refuse the very description `assemble` just gave it.
                // [`refuse_declared_blocked_description`] is the same refusal, made once, on the
                // step as the file declared it — before `assemble` has touched it at all.
                const OVERRIDE_KEYS: &str = "the five keys it may override are `agent`, `model`, `effort`, `session` \
                     and `prompt`";
                if step.run.is_some() {
                    bail!("step `blocked` declares `run:` — {OVERRIDE_KEYS}; delete `run:`");
                }
                if !step.slot {
                    bail!(
                        "step `blocked` declares `slot: false` — {OVERRIDE_KEYS}; delete `slot:`"
                    );
                }
                if !step.r#loop.is_unbounded() {
                    bail!("step `blocked` declares `loop:` — {OVERRIDE_KEYS}; delete `loop:`");
                }
            }

            let kind = step.kind();

            match kind {
                StepKind::Agent => {
                    if step.agent.is_none() {
                        bail!(
                            "step `{}` names no `agent:` and no `run:` — a step that runs \
                             nothing and does not `end:` is one whose keys were mistyped",
                            step.id
                        );
                    }
                    if step.on_pass.is_none() && step.id != BLOCKED {
                        bail!(
                            "step `{}` runs an agent but has no on_pass — a task reaching it \
                             would never leave",
                            step.id
                        );
                    }
                }
                StepKind::Terminal => {
                    if step.on_pass.is_some() || step.on_fail.is_some() {
                        bail!(
                            "step `{}` declares `end: true` and a transition — a task that \
                             stops here goes nowhere",
                            step.id
                        );
                    }
                    if step.agent.is_some() || step.run.is_some() {
                        bail!(
                            "step `{}` declares `end: true` and names something to run — \
                             a task that stops here runs nothing",
                            step.id
                        );
                    }
                }
                StepKind::Command => {
                    if step.run.as_ref().is_none_or(|run| run.trim().is_empty()) {
                        bail!(
                            "step `{}` declares an empty `run:` — there is nothing for it to do",
                            step.id
                        );
                    }
                    if step.agent.is_some() {
                        bail!(
                            "step `{}` names both `run:` and `agent:` — a step runs a process \
                             or a model, not both",
                            step.id
                        );
                    }
                    if step.on_pass.is_none() {
                        bail!(
                            "step `{}` runs a command but has no on_pass — a task reaching it \
                             would never leave",
                            step.id
                        );
                    }
                    // `timeout: 0s` reads as "no limit" and means the opposite:
                    // every run of it is over the bound the moment it starts.
                    if step.timeout == Some(Duration::ZERO) {
                        bail!(
                            "step `{}` sets `timeout: 0s`, which would kill the command as soon \
                             as it started. There is no way to say `no limit` here — write the \
                             longest this command may reasonably take",
                            step.id
                        );
                    }
                }
            }

            // Everything below is a lane's, and neither a command step nor an
            // ending starts one.
            if kind != StepKind::Agent {
                for (key, set) in [
                    ("prompt", step.prompt.is_some()),
                    ("model", step.model.is_some()),
                    ("effort", step.effort.is_some()),
                    ("session", step.session),
                    ("gate", step.gate),
                ] {
                    if set {
                        bail!(
                            "step `{}` is a {} step but declares `{key}` — that is a lane's, \
                             and this step starts none",
                            step.id,
                            kind.as_str()
                        );
                    }
                }
            }
            if kind != StepKind::Command {
                if step.background {
                    bail!(
                        "step `{}` declares `background:` but runs no command — only a `run:` \
                         can be left running",
                        step.id
                    );
                }
                if step.timeout.is_some() {
                    // `timeout:` bounds a shell command, and an agent step runs
                    // no shell command of its own — only the `run:` a step
                    // declares is bounded this way.
                    bail!(
                        "step `{}` declares `timeout:` but runs no command — only a `run:` is \
                         bounded that way",
                        step.id
                    );
                }
                if step.headless {
                    bail!(
                        "step `{}` declares `headless:` but runs no command — only a `run:` \
                         has a pane to turn off",
                        step.id
                    );
                }
                // A lane walked past is a prompt that never reports, and
                // whatever the step was for goes unjudged rather than undone.
                // `last:` is for work an exit code answers.
                if step.last {
                    bail!(
                        "step `{}` declares `last:` but runs no command — a step every task \
                         but one walks past has to be a `run:`, since a lane nobody started \
                         reports nothing",
                        step.id
                    );
                }
                // Same reasoning as `last:` above, for the opposite end of a
                // chain.
                if step.first {
                    bail!(
                        "step `{}` declares `first:` but runs no command — a step every task \
                         but the root walks past has to be a `run:`, since a lane nobody \
                         started reports nothing",
                        step.id
                    );
                }
            }
            // Only a command step's run is something a pass starts and can
            // therefore hold back; an agent step's lane has no such turn to
            // wait for. Named apart from the `kind != Command` refusals
            // above, whose wording is for an ending as much as a lane.
            if step.serial && kind == StepKind::Agent {
                bail!(
                    "step `{}` declares `serial:` but is an agent step — only a command step's \
                     run can be held for its turn",
                    step.id
                );
            }
            // `first:` and `last:` ask opposite questions about a chain, so a
            // command step naming both has no single command left to be, root
            // or not.
            if step.first && step.last {
                bail!(
                    "step `{}` declares both `first:` and `last:` — a command step can be the \
                     one a chain's root runs or the one only its top runs, not both",
                    step.id
                );
            }
        }

        // A step may not name itself as where its own `on_pass` or `on_fail`
        // leads. That shape used to be legal as long as
        // `loop:` bounded the lap — `Report::self_route` and
        // `just_self_routed` existed only to keep such a pass from spinning
        // the dispatcher's wait loop. Refusing the route outright at load
        // retires all of that: a retry now has to go through a step that
        // actually leaves, or not happen at all.
        for step in &self.steps {
            for (key, target) in [
                ("on_pass", step.on_pass.as_deref()),
                ("on_fail", step.on_fail.as_deref()),
            ] {
                if target == Some(step.id.as_str()) {
                    bail!(self_route_message(&step.id, key));
                }
            }
        }

        // A `session:` step's conversation is keyed on its prompt — every
        // step in this pipeline running that prompt is the same
        // conversation — so two `session:` steps that resolve to the same
        // prompt but name different agent profiles could never agree on
        // whose session it is. By now every `session:` step is an agent
        // step: a non-agent one was already refused above.
        let mut session_prompts: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
        for step in &self.steps {
            if !step.session {
                continue;
            }
            let agent = step.agent.as_deref().unwrap_or_default();
            let prompt = step.prompt_name();
            match session_prompts.get(prompt) {
                Some(&(prior_agent, prior_id)) if prior_agent != agent => {
                    bail!(
                        "step `{}` and step `{prior_id}` both run prompt `{prompt}` with \
                         `session:`, but name different agent profiles (`{prior_agent}`, \
                         `{agent}`) — one session, two profiles",
                        step.id
                    );
                }
                Some(_) => {}
                None => {
                    session_prompts.insert(prompt, (agent, step.id.as_str()));
                }
            }
        }

        // Every step must be able to reach an ending, or a task can circulate
        // forever with nothing ever finishing it. `done` and `blocked` are
        // always reachable — every unrouted failure falls to `blocked` — so
        // what this actually catches is a loop with no passing way out.
        let mut terminals: HashSet<&str> = [DONE, BLOCKED].into_iter().collect();
        terminals.extend(
            self.steps
                .iter()
                .filter(|s| s.kind() == StepKind::Terminal)
                .map(|s| s.id.as_str()),
        );

        for step in &self.steps {
            if !self.reaches_terminal(&step.id, &terminals) {
                bail!(
                    "step `{}` can never reach a terminal step — tasks would loop forever",
                    step.id
                );
            }
        }

        // `loop:` as a map is the retired per-route shape — see [`Loop`]'s own
        // doc — refused here, by name, with the one step and the number(s) a
        // person actually wrote, so the message can say where the limit
        // belongs now instead of just that the old key is gone.
        for step in &self.steps {
            if let Loop::Map(by_route) = &step.r#loop {
                bail!(loop_map_message(&step.id, by_route));
            }
        }

        self.check_bounded_loops()?;

        Ok(())
    }

    /// Every occurrence of the four retired step shapes this pipeline's own
    /// steps carry — `on_loop_max:`, an `on_fail:` naming its own step,
    /// `loop:` written as the old per-route map, and `loop: 0`, which 0.7 ran
    /// as no limit — each named by its step,
    /// collected without stopping at the first the way [`Self::validate`]
    /// must.
    ///
    /// For `pipeline check` and `doctor`'s `pipelines load` row, which want
    /// the whole list in one pass rather than one refusal per run, and for
    /// `crate::sync`, which refuses a file carrying one rather than report an
    /// upgrade done over a pipeline that no longer loads. Scoped to
    /// exactly these four shapes rather than
    /// every way `validate` can refuse a pipeline: the rest of `validate`'s
    /// checks stay bail-at-the-first, which is what every other caller
    /// wants from a pipeline that genuinely cannot run.
    pub fn retired_shape_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for step in &self.steps {
            if step.on_loop_max.is_some() {
                problems.push(on_loop_max_message(&step.id));
            }
            for (key, target) in [
                ("on_pass", step.on_pass.as_deref()),
                ("on_fail", step.on_fail.as_deref()),
            ] {
                if target == Some(step.id.as_str()) {
                    problems.push(self_route_message(&step.id, key));
                }
            }
            if let Loop::Map(by_route) = &step.r#loop {
                problems.push(loop_map_message(&step.id, by_route));
            }
            if step.r#loop == Loop::Bare(0) {
                problems.push(loop_zero_message(&step.id));
            }
        }
        problems
    }

    /// Steps that gate but declare no `on_fail`, so a `spoolway report
    /// --fail` at that step has nowhere named to go but `blocked`.
    ///
    /// Not one of `validate`'s own refusals: a gated step with no `on_fail`
    /// is a legal shape — [`Step::destination`] already falls back to
    /// `blocked` for an outcome a step names no route for, and `blocked` is
    /// exactly where a failed piece of work belongs with nowhere else named
    /// for it — a diagnostic for whoever wrote the pipeline, not a shape
    /// this refuses to run.
    pub fn gate_warnings(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter(|step| step.gate && step.on_fail.is_none())
            .map(|step| {
                format!(
                    "{}: `{}` gates but declares no `on_fail`, so `spoolway report --fail` \
                     there parks the task on `{BLOCKED}`.",
                    self.name, step.id
                )
            })
            .collect()
    }

    /// Steps declaring `on_fail: blocked` — exactly the step [`Step::
    /// destination`] already falls back to for a `Fail` with no `on_fail` of
    /// its own, so the key changes nothing about where a failure routes.
    ///
    /// Not a refusal, the same as [`Self::gate_warnings`]: a pipeline
    /// carrying the key still runs identically to one without it. Worth a
    /// warning anyway, because it also silences [`Self::gate_warnings`] on a
    /// gated step without changing where a `report --fail` there goes — see
    /// that method's own doc comment — so a person reading `pipeline
    /// check`'s clean gate report has no way to know the step still has
    /// nowhere of its own to send a fail.
    pub fn redundant_on_fail_warnings(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter(|step| step.on_fail.as_deref() == Some(BLOCKED))
            .map(|step| {
                format!(
                    "{}: `{}` declares `on_fail: blocked`, which is where a fail goes with no \
                     `on_fail` at all — delete the key.",
                    self.name, step.id
                )
            })
            .collect()
    }

    /// Steps no route from the first step reaches, following `on_pass` and
    /// `on_fail` (and the implicit route to `blocked`) the way
    /// [`Pipeline::destinations`] does.
    ///
    /// A warning, not a refusal: a task can still be sent to such a step by
    /// hand with `resume --stage`, and a pipeline mid-edit commonly has one.
    /// `blocked` is left out because the dispatcher parks a task there on a
    /// `--block` without any route naming it.
    pub fn unreachable_warnings(&self) -> Vec<String> {
        let mut reached: HashSet<&str> = HashSet::new();
        let mut stack = vec![self.entry()];
        while let Some(id) = stack.pop() {
            if !reached.insert(id) {
                continue;
            }
            if let Some(step) = self.step(id) {
                stack.extend(self.destinations(step));
            }
        }
        self.steps
            .iter()
            .filter(|step| step.id != BLOCKED && !reached.contains(step.id.as_str()))
            .map(|step| {
                format!(
                    "pipeline {}: step `{}` is reached by no route",
                    self.name, step.id
                )
            })
            .collect()
    }

    /// A missing `description:`, and one long enough to stop being scannable
    /// in the one-line-per-pipeline list `pipeline list` prints.
    ///
    /// Warnings, on `gate_warnings`'s own channel, never problems: a pipeline
    /// with no description still runs every task on it correctly, and a long
    /// one still says something true — it is only harder to choose between
    /// pipelines by, which `validate` has no business failing over.
    pub fn description_warnings(&self) -> Vec<String> {
        const TOO_LONG: usize = 400;
        match &self.description {
            None => vec![format!(
                "pipeline `{}` has no `description:` — nothing reading `pipeline list` can \
                 tell what it is for",
                self.name
            )],
            // Chars, not bytes: an em-dash or any other multi-byte character
            // in the prose would otherwise inflate the count past what the
            // message claims to say, and trip the threshold on text that
            // reads as well under it.
            Some(description) if description.chars().count() > TOO_LONG => vec![format!(
                "pipeline `{}`'s `description:` runs to {} characters — it is read to choose \
                 between pipelines, so keep it to a few sentences",
                self.name,
                description.chars().count()
            )],
            Some(_) => Vec::new(),
        }
    }

    /// Refuse a cycle that nothing bounds.
    ///
    /// Reaching a terminal step is not enough on its own: `review → fix →
    /// review` reaches `done` on every pass, and still spins forever on a pair
    /// of agents that keep saying fail. A person only ever hears about it from
    /// the bill.
    ///
    /// A cycle terminates if any one of its steps carries a `loop:` of its
    /// own: an arrival there past the limit escalates to [`Step::loop_exit`],
    /// which is `blocked` and so leaves every cycle there is. So the check is
    /// not "enumerate the cycles", which is exponential, but the same
    /// statement inside out: drop every edge that arrives at a bounded step
    /// from the graph, and what is left must be acyclic.
    ///
    /// A bounded route used to be able to give up back into the very loop it
    /// was meant to break, when `on_loop_max:` could name a step inside it.
    /// That shape is gone with the key, and with it the suggestion this
    /// message used to carry about which member of the cycle to move `loop:`
    /// to: a budget anywhere along the cycle now breaks it.
    ///
    /// A route is bounded by the step it *arrives at*, not the one it leaves:
    /// `loop: 2` on `fix` bounds every edge into `fix`, so the edge this walk
    /// drops is the one the budget actually stops being taken — see [`Loop`].
    fn check_bounded_loops(&self) -> Result<()> {
        // Grey while on the current path, black once explored. A route back to
        // something grey closes a cycle, and the path holds its steps.
        let mut state: BTreeMap<&str, u8> = BTreeMap::new();
        let mut path: Vec<&str> = Vec::new();

        for step in &self.steps {
            if let Some(cycle) = self.find_unbounded_cycle(&step.id, &mut state, &mut path) {
                bail!(
                    "steps {} form a loop nothing bounds — no step along it carries a `loop:`, \
                     so a task could go round it forever. Give one of them a `loop: <n>`: the \
                     arrival past it parks on `blocked`.",
                    cycle
                        .iter()
                        .map(|id| format!("`{id}`"))
                        .collect::<Vec<_>>()
                        .join(" → ")
                );
            }
        }
        Ok(())
    }

    /// One depth-first walk of [`check_bounded_loops`], returning the first
    /// cycle it closes over routes no budget stops being taken.
    fn find_unbounded_cycle<'a>(
        &'a self,
        id: &'a str,
        state: &mut BTreeMap<&'a str, u8>,
        path: &mut Vec<&'a str>,
    ) -> Option<Vec<&'a str>> {
        const GREY: u8 = 1;
        const BLACK: u8 = 2;

        match state.get(id) {
            Some(&BLACK) => return None,
            Some(_) => {
                // Closed a cycle: report it from where it was first entered.
                let start = path.iter().position(|seen| *seen == id).unwrap_or(0);
                let mut cycle: Vec<&str> = path[start..].to_vec();
                cycle.push(id);
                return Some(cycle);
            }
            None => {}
        }

        let step = self.step(id)?;
        state.insert(&step.id, GREY);
        path.push(&step.id);

        for next in self.destinations(step) {
            // An edge into a bounded step is not walked at all: once `next`
            // has taken its own limit of arrivals, the one after escalates to
            // `blocked` instead of landing there again — so the edge cannot be
            // what holds a loop open, and `next` is never reached by way of
            // it a second time. Colouring it here would mark a step visited on
            // a route nothing takes past its limit.
            //
            // `blocked` is walked like any other destination, and harmlessly:
            // it has no [`Pipeline::destinations`] of its own, so the walk
            // turns straight back out of it. No guard for it here, because
            // there is nothing for one to prevent.
            if self.step(next).is_some_and(|s| s.arrival_limit().is_some()) {
                continue;
            }
            if let Some(cycle) = self.find_unbounded_cycle(next, state, path) {
                return Some(cycle);
            }
        }

        path.pop();
        state.insert(&step.id, BLACK);
        None
    }

    /// Every step this one can move a task to: its two outcomes, and the
    /// implicit route to `blocked`.
    ///
    /// Where a spent loop lands needs no edge of its own: it is `blocked`,
    /// which an agent or command step already routes to here for an unrouted
    /// failure, and which is a terminal for every walk that reads this list
    /// anyway.
    ///
    /// `blocked` itself has none: `validate` refuses it both `on_pass` and
    /// `on_fail`, so there is nothing declared to report, and its real
    /// routing — read from the step the task blocked on for a pass, back to
    /// itself on a block — is decided at runtime from task state and the
    /// reported verb, not from the graph. Reporting a self-edge here would
    /// read as the very unbounded cycle `blocked` is deliberately exempt
    /// from.
    pub fn destinations<'a>(&'a self, step: &'a Step) -> Vec<&'a str> {
        if step.id == BLOCKED {
            return Vec::new();
        }
        let mut out: Vec<&str> = Vec::new();
        if let Some(target) = step.on_pass.as_deref() {
            out.push(target);
        }
        match step.on_fail.as_deref() {
            Some(target) => out.push(target),
            None if step.kind() != StepKind::Terminal => out.push(BLOCKED),
            None => {}
        }
        out
    }

    /// Whether an unattended run staffs this pipeline's `blocked` step with a
    /// lane, rather than parking the task there for a person.
    ///
    /// One predicate for every place that used to read a task's stage as a
    /// proxy for "parked, waiting on a person": the scheduler's own skip, the
    /// pane-holding arm of `free_finished_lanes`, the concurrency tally in
    /// `start_lanes`, `sweep_on_stop`, and `slots_used` on the board. The
    /// proxy and the reality agreed until `blocked` could carry a lane —
    /// and now that `Pipelines::assemble` materialises it from `[unattended]`
    /// for every pipeline that does not declare its own, `step(BLOCKED)` is
    /// `Some` for any pipeline that actually went through `Pipelines::load`,
    /// so this is `unattended` in practice. It stays a method on `Pipeline`
    /// rather than collapsing to that bare bool because a `Pipeline` built by
    /// hand — every test in this module, `install.rs` — never runs
    /// `assemble` and may genuinely have no `blocked` step at all.
    pub fn blocked_is_staffed(&self, unattended: bool) -> bool {
        unattended && self.step(BLOCKED).is_some()
    }

    fn reaches_terminal(&self, from: &str, terminals: &HashSet<&str>) -> bool {
        let mut visited = HashSet::new();
        let mut stack = vec![from.to_string()];

        while let Some(id) = stack.pop() {
            if terminals.contains(id.as_str()) {
                return true;
            }
            if !visited.insert(id.clone()) {
                continue;
            }
            if let Some(step) = self.step(&id) {
                for next in [step.on_pass.as_deref(), step.on_fail.as_deref()]
                    .into_iter()
                    .flatten()
                {
                    stack.push(next.to_string());
                }
                // An unrouted failure always goes to `blocked`.
                if step.on_fail.is_none() && step.kind() != StepKind::Terminal {
                    stack.push(BLOCKED.to_string());
                }
            }
        }
        false
    }

    /// Agent profile names this pipeline references, for config validation.
    pub fn referenced_agents(&self) -> BTreeMap<&str, Vec<&str>> {
        let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for step in &self.steps {
            if let Some(agent) = step.agent.as_deref() {
                map.entry(agent).or_default().push(&step.id);
            }
        }
        map
    }
}

/// Parse one pipeline file without validating it.
///
/// The production parser: [`Pipelines::load`]'s own directory loop calls this,
/// not [`Pipeline::parse`] (test-only now — see its own doc), because a
/// file's `blocked` step may declare an override missing some of its five
/// keys on purpose, and validating before [`Pipelines::assemble`] has filled
/// the rest in from config would refuse a step that is about to become a
/// perfectly good one.
///
/// `pub(crate)` for `pipeline_copy` too — reading `from`'s own
/// `task_template:` without pulling in the whole project's set (which a
/// broken, unrelated pipeline file elsewhere would then refuse to load)
/// needs exactly this same unchecked parse, at exactly this same trust
/// level: good enough to read a field off, not yet proven to run.
pub(crate) fn parse_unchecked(name: &str, raw: &str) -> Result<Pipeline> {
    let mut pipeline: Pipeline = serde_norway::from_str(raw).context("parsing pipeline")?;
    pipeline.name = name.to_string();
    refuse_retired_step_keys(&pipeline)?;
    Ok(pipeline)
}

/// Refuse a step key that is retired but still deserialises, checked against
/// the file exactly as it was written.
///
/// Here rather than in [`Pipeline::validate`], and rather than in a
/// `deserialize_with` on the field itself, because each of those loses
/// something this needs. A `deserialize_with` fails before there is a
/// `Pipeline` to ask which file or which step the key was on — and for a
/// project whose pipeline files `spoolway sync` will not edit, that pair is
/// the whole of the migration. `validate()` knows both, but runs after
/// [`crate::overrides::apply_pipeline_patch`] has round-tripped every
/// patched step through serde, and a `skip_serializing` field does not
/// survive that trip: the key would be dropped, unmentioned, on exactly the
/// steps an installation had bothered to override. Parse time is the one
/// place that has the names and still has the key.
fn refuse_retired_step_keys(pipeline: &Pipeline) -> Result<()> {
    for step in &pipeline.steps {
        if step.on_loop_max.is_some() {
            bail!("{}: {}", pipeline.name, on_loop_max_message(&step.id));
        }
    }
    Ok(())
}

/// A `blocked` step for a pipeline that declares none of its own, built
/// straight from `[unattended]`'s `blocked_*` keys.
///
/// `description` is left `None` here on purpose — see the comment in
/// [`Pipelines::assemble`] on why it is filled in only after `validate()` has
/// run.
fn blocked_step_from_config(unattended: &crate::config::UnattendedConfig) -> Step {
    Step {
        id: BLOCKED.to_string(),
        end: false,
        description: None,
        agent: opt(&unattended.blocked_agent),
        prompt: opt(&unattended.blocked_prompt),
        model: opt(&unattended.blocked_model),
        effort: opt(&unattended.blocked_effort),
        skills: Vec::new(),
        session: unattended.blocked_session,
        slot: true,
        gate: false,
        on_pass: None,
        on_fail: None,
        r#loop: Loop::default(),
        max_new_sessions: None,
        max_rounds: None,
        on_loop_max: None,
        run: None,
        timeout: None,
        background: false,
        headless: false,
        last: false,
        first: false,
        serial: false,
        cleanup: None,
        blocked_on_write: Vec::new(),
    }
}

/// Refuse a `description:` on a pipeline's own declared `blocked` step,
/// checked against the step exactly as the file wrote it.
///
/// Called once, in [`Pipelines::assemble`], before anything about the step is
/// touched — never folded into [`Pipeline::validate`], which runs again on
/// the already-assembled pipeline every time `pipeline check` or `doctor`
/// calls it, by which point `assemble` has given the step the same canonical
/// description every `blocked` step carries. A check inside `validate()`
/// would then refuse the description `assemble` itself just wrote.
fn refuse_declared_blocked_description(step: &Step) -> Result<()> {
    if step.description.is_some() {
        bail!(
            "step `blocked` declares `description:` — every pipeline's `blocked` step reads \
             the same description, and the five keys it may override are `agent`, `model`, \
             `effort`, `session` and `prompt`; delete `description:`"
        );
    }
    Ok(())
}

/// Fill in whichever of the five keys a pipeline's own declared `blocked`
/// step left out, from `[unattended]`'s `blocked_*` keys.
///
/// `session` is a plain `bool`, with no way to tell "the file said `session:
/// false`" apart from "the file said nothing" — same as every other bare
/// `bool` key a step carries. Read as "omitted" either way, consistently with
/// how `Pipeline::validate` already reads `gate` and `end` on this very step:
/// a step wanting `session: false` gets it by naming a config whose own
/// `blocked_session` is `false`.
fn apply_blocked_config_fallback(step: &mut Step, unattended: &crate::config::UnattendedConfig) {
    if step.agent.is_none() {
        step.agent = opt(&unattended.blocked_agent);
    }
    if step.prompt.is_none() {
        step.prompt = opt(&unattended.blocked_prompt);
    }
    if step.model.is_none() {
        step.model = opt(&unattended.blocked_model);
    }
    if step.effort.is_none() {
        step.effort = opt(&unattended.blocked_effort);
    }
    if !step.session {
        step.session = unattended.blocked_session;
    }
}

/// A config string as a step field: blank reads the same as absent, the same
/// way [`Step::model`] and its siblings already treat a blank value as a
/// missing one.
fn opt(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Every pipeline defined for a project.
///
/// Assembled rather than parsed: each pipeline is its own file under
/// `.spoolway/pipelines/`, named by that file. There is no project default —
/// every task names the pipeline it runs on, and a task that does not is
/// refused before it can reach one.
#[derive(Debug, Clone)]
pub struct Pipelines {
    pub pipelines: BTreeMap<String, Pipeline>,
    /// Every override left out of the merge because it no longer fit the
    /// checkout — empty for [`Pipelines::load_tracked`], which never merges
    /// anything at all. See [`crate::overrides::Ignored`].
    pub(crate) ignored_overrides: Vec<crate::overrides::Ignored>,
}

impl Pipelines {
    /// The directory holding one file per pipeline.
    pub fn dir_in(root: &Path) -> PathBuf {
        crate::config::setup_dir_in(root).join(PIPELINE_DIR)
    }

    /// Where a named pipeline's file lives.
    pub fn file_in(root: &Path, name: &str) -> PathBuf {
        Pipelines::dir_in(root).join(format!("{name}.yml"))
    }

    /// Every retired-shape refusal this project's pipeline files carry right
    /// now — see [`Pipeline::retired_shape_problems`] — one line per
    /// occurrence, named by its file and step, collected across every file
    /// rather than stopping at the first the way [`Pipelines::load`]'s `?`
    /// chain must (right for the dispatcher, which can only ever act on one
    /// problem at a time). `pipeline check` and `doctor`'s `pipelines load`
    /// row call this once loading has already failed, so a project meets
    /// every retired shape at once instead of one refusal per run.
    ///
    /// A file that will not deserialise at all — a genuine syntax error, or a
    /// key `deny_unknown_fields` or one of the still-parsing retired keys
    /// (`max_rounds:`, `max_new_sessions:`) refuses outright — is named here
    /// too, by its own parse error, rather than skipped: a project with one
    /// broken file and one carrying a retired shape hears about both, not
    /// only the shape.
    ///
    /// Empty only when nothing here found anything to say at all — no
    /// pipeline directory, or every file both parsed and carried none of the
    /// four shapes — which means the original load failed for a reason
    /// outside this function's own scope (a `validate` refusal none of the
    /// four shapes explains), and a caller falls back to that error, the
    /// same one it always printed.
    pub fn refusals(root: &Path) -> Vec<String> {
        let dir = Pipelines::dir_in(root);
        let files = match read_pipeline_dir(&dir) {
            Ok(Some(files)) => files,
            Ok(None) | Err(_) => return Vec::new(),
        };
        let mut problems = Vec::new();
        for (name, _path, raw) in &files {
            match serde_norway::from_str::<Pipeline>(raw) {
                Ok(mut pipeline) => {
                    pipeline.name = name.clone();
                    for problem in pipeline.retired_shape_problems() {
                        problems.push(format!("{name}.yml: {problem}"));
                    }
                }
                Err(err) => problems.push(format!("{name}.yml: {err:#}")),
            }
        }
        problems
    }

    /// Load and validate from a repo root, with any patch under
    /// `~/.spoolway/<project>/overrides/pipelines/` merged onto each
    /// pipeline first — see [`crate::overrides`]. This is what every
    /// ordinary caller wants: the dispatcher, `pipeline show`, `pipeline
    /// check`, the status screen, none of which is the one place that
    /// should have to know a second source exists.
    pub fn load(root: &Path, config: &crate::config::Config) -> Result<Pipelines> {
        let overrides = crate::overrides::dir_for(root)?;
        Pipelines::load_impl(root, config, Some(&overrides), true, true)
    }

    /// [`Pipelines::load`], but an override that no longer fits is not
    /// announced on stderr. For the screen, which holds the terminal in its
    /// alternate screen: a line written there lands between two frames and is
    /// painted over before anyone can read it.
    pub fn load_quietly(root: &Path, config: &crate::config::Config) -> Result<Pipelines> {
        let overrides = crate::overrides::dir_for(root)?;
        Pipelines::load_impl(root, config, Some(&overrides), true, false)
    }

    /// [`Pipelines::load`], with no patch layer applied — for a caller that
    /// must see only the tracked file: `commands::pipeline_override`, to
    /// check a step id and a key against what the tracked file actually
    /// has, and `commands::override_promote`'s own read of it.
    pub fn load_tracked(root: &Path, config: &crate::config::Config) -> Result<Pipelines> {
        Pipelines::load_impl(root, config, None, true, true)
    }

    /// One private pipeline, by name, read straight off `local/pipelines/`
    /// with no patch layer applied — [`load_tracked`]'s own private-layer
    /// twin, for `commands::pipeline_override`, which must find a private
    /// pipeline the way `pipeline list` and `pipeline show` both do rather
    /// than report it missing just because [`load_tracked`] never looks
    /// there. `Ok(None)` for a checkout with no such file, or outside repo
    /// mode, where there is no `local/` to read.
    pub(crate) fn private(root: &Path, name: &str) -> Result<Option<Pipeline>> {
        if !crate::local::is_repo_mode(root) {
            return Ok(None);
        }
        let local = crate::local::dir_for(root)?;
        let dir = crate::local::pipelines_dir(&local);
        let Some(files) = read_pipeline_dir(&dir)? else {
            return Ok(None);
        };
        for (file_name, path, raw) in files {
            if file_name != name {
                continue;
            }
            let mut pipeline = parse_unchecked(&file_name, &raw)
                .with_context(|| format!("in {}", path.display()))?;
            pipeline.private_file = Some(path);
            return Ok(Some(pipeline));
        }
        Ok(None)
    }

    /// [`Pipelines::load`], but an empty `pipelines/` is not an error —
    /// for `pipeline contract` and `prompt contract`, which print a format
    /// rather than report on this project's own pipelines, and so have no
    /// need of one existing. `spoolway init --no-examples` leaves exactly
    /// that empty directory, and the skill route that is supposed to write
    /// the first pipeline from this contract could not even read it.
    /// Every other caller of `load` still needs real pipelines to run
    /// against, so only these two opt into this; a file that is there but
    /// fails to parse is still a real problem and still fails here.
    pub fn load_or_empty(root: &Path, config: &crate::config::Config) -> Result<Pipelines> {
        let overrides = crate::overrides::dir_for(root)?;
        Pipelines::load_impl(root, config, Some(&overrides), false, true)
    }

    /// One source: the directory. A missing directory and an empty one now
    /// reach the same `no pipelines defined` error from
    /// [`Pipelines::validate`] — a project with neither has nothing to run,
    /// and it hears that where it happens rather than the built-ins standing
    /// in and the gap surfacing three commands later at dispatch, once a
    /// step needs a `PROMPT.md` that was never written.
    /// [`Pipelines::load_or_empty`] is the one caller that does not want
    /// that bail: `pipeline contract` and `prompt contract` print a format
    /// rather than run anything, so an empty directory is a legitimate
    /// project state for them rather than an error. The old single
    /// `pipeline.yml` is still called out on its own, so a project carrying
    /// one is told what to do with it rather than reading a generic
    /// "no pipelines defined" for a file that is
    /// actually right there.
    ///
    /// `overrides` is applied to each pipeline right after it is parsed and
    /// before [`Pipelines::assemble`] runs — assembling first would
    /// materialise a `blocked` step that a pipeline declaring none of its
    /// own never had in the file, letting a patch reach a step that, from
    /// the file's own perspective, does not exist.
    ///
    /// `print_notices` says whether an override that was skipped is printed
    /// to stderr once per process; see [`Pipelines::load_quietly`] for why a
    /// caller turns it off.
    fn load_impl(
        root: &Path,
        config: &crate::config::Config,
        overrides: Option<&Path>,
        require_nonempty: bool,
        print_notices: bool,
    ) -> Result<Pipelines> {
        let dir = Pipelines::dir_in(root);
        let files = match read_pipeline_dir(&dir)? {
            Some(files) => files,
            None => {
                let old = crate::config::setup_dir_in(root).join(PIPELINE_FILE);
                if old.exists() {
                    bail!(
                        "{} is the old single-file shape, which this spoolway no longer \
                         reads. Split it by hand: one `.spoolway/pipelines/<name>.yml` per \
                         `pipelines:` entry (the file name is the pipeline name, so drop the \
                         map key and the old `default:` — every task now names its own \
                         pipeline instead), then delete the old file. `spoolway pipeline \
                         contract` prints the annotated blank for reference.",
                        old.display()
                    );
                }
                Vec::new()
            }
        };

        let mut pipelines = BTreeMap::new();
        let mut ignored = Vec::new();
        for (name, path, raw) in files {
            // Unvalidated: a file's own `blocked` step may declare only
            // some of its five keys on purpose, leaning on
            // `Pipelines::assemble` to fill the rest in from config
            // before anything checks that the step is a runnable one.
            let mut pipeline =
                parse_unchecked(&name, &raw).with_context(|| format!("in {}", path.display()))?;
            if let Some(overrides) = overrides {
                ignored.extend(crate::overrides::apply_pipeline_patch(
                    &mut pipeline,
                    overrides,
                )?);
            }
            pipelines.insert(name, pipeline);
        }

        // The private layer: `local/pipelines/` and `local/prompts/`, read
        // beside the tracked files rather than merged onto them — gated on
        // `overrides.is_some()` the same way the patch layer above is, so
        // `load_tracked` (the probe `pipeline_override` and
        // `override_promote` use) sees only the tracked files, and on repo
        // mode, since home mode's whole setup is already private and has no
        // `local/` to read. See `crate::local`.
        let mut private_dir = None;
        let mut private_prompt_names = None;
        if overrides.is_some() && crate::local::is_repo_mode(root) {
            let local = crate::local::dir_for(root)?;
            private_dir = Some(crate::local::pipelines_dir(&local));
            private_prompt_names = Some(merge_private(root, &mut pipelines)?);
        }

        // A patch naming a pipeline the checkout no longer has is never
        // reached by the loop above at all — it iterates the tracked files,
        // never the layer — so it is caught here instead, once, rather than
        // silently applying nothing and saying nothing. Checked only now,
        // after the private layer has joined `pipelines`, so a patch
        // waiting on a private pipeline of the same name is told apart from
        // one naming a pipeline that genuinely does not exist anywhere —
        // the private pipeline's own patch was never reachable by the
        // tracked-only loop above either way, tracked or not.
        if let Some(overrides) = overrides {
            for name in crate::overrides::list_pipeline_patches(overrides)? {
                match pipelines.get(&name) {
                    Some(pipeline) if pipeline.private_file.is_some() => {
                        ignored.push(crate::overrides::Ignored::private_pipeline(&name));
                    }
                    Some(_) => {}
                    None => ignored.push(crate::overrides::Ignored::missing_pipeline(&name)),
                }
            }
        }

        // Named against whichever directory(ies) a failing pipeline could
        // actually have come from — never only the tracked one, since a
        // validation error from `set.validate()` (each already carrying its
        // own `pipeline \`{name}\`` context) can just as well be a private
        // pipeline's, and pointing only at `dir` there would send a person
        // to a file that was never the problem.
        let set = Pipelines::assemble(pipelines, config, ignored, require_nonempty).with_context(
            || match &private_dir {
                Some(private_dir) => {
                    format!("in {} or {}", dir.display(), private_dir.display())
                }
                None => format!("in {}", dir.display()),
            },
        )?;

        // Run only now, against the steps `set` will actually dispatch —
        // including a `blocked` step `assemble` has just materialised from
        // `config.unattended.blocked_prompt`, or filled in on a bare
        // `blocked:` step — rather than inside `merge_private`, which saw
        // only the file's own declared steps.
        if let Some((tracked_prompt_names, local_prompt_names)) = &private_prompt_names {
            let local_prompts_dir = crate::local::prompts_dir(&crate::local::dir_for(root)?);
            refuse_tracked_steps_naming_a_private_only_prompt(
                &set.pipelines,
                tracked_prompt_names,
                local_prompt_names,
                &local_prompts_dir,
            )?;
        }

        if overrides.is_some() && print_notices {
            crate::overrides::print_ignored_notices(&set.ignored_overrides);
        }
        Ok(set)
    }

    /// Put a parsed set together with the one setting that is not
    /// per-pipeline, materialise every pipeline's `blocked` step from
    /// `[unattended]` — or merge that table into the five keys a pipeline's
    /// own declared step may override — then validate the whole thing.
    ///
    /// Materialising before `validate()` runs is what lets a declared
    /// override name only some of its five keys: `agent`, `model`, `effort`,
    /// `session` and `prompt` are filled in here from config wherever the
    /// pipeline left them out, so the step `validate()` actually checks is
    /// always a complete one — the same graph a pipeline that named every
    /// key by hand would have produced.
    fn assemble(
        mut pipelines: BTreeMap<String, Pipeline>,
        config: &crate::config::Config,
        ignored_overrides: Vec<crate::overrides::Ignored>,
        require_nonempty: bool,
    ) -> Result<Pipelines> {
        for (name, pipeline) in pipelines.iter_mut() {
            match pipeline.steps.iter_mut().find(|s| s.id == BLOCKED) {
                Some(step) => {
                    refuse_declared_blocked_description(step)
                        .with_context(|| format!("pipeline `{name}`"))?;
                    pipeline.blocked_declared = true;
                    apply_blocked_config_fallback(step, &config.unattended);
                }
                None => {
                    pipeline.blocked_declared = false;
                    // Appended last, so `priority()` ranks it exactly as a
                    // pipeline that declared its own trailing `blocked` step
                    // always has.
                    pipeline
                        .steps
                        .push(blocked_step_from_config(&config.unattended));
                }
            }
        }

        let mut set = Pipelines {
            pipelines,
            ignored_overrides,
        };
        set.validate_impl(require_nonempty)?;

        // Every `blocked` step's description is still `None` here: a
        // declared override's own `description:` was already refused above,
        // by `refuse_declared_blocked_description`, before this loop had a
        // chance to give the step one of its own. Setting it any earlier
        // would have erased that refusal's evidence — an override's
        // forbidden `description:` and the canonical one this writes would
        // have become the same value.
        for pipeline in set.pipelines.values_mut() {
            if let Some(step) = pipeline.steps.iter_mut().find(|s| s.id == BLOCKED) {
                step.description = Some(BLOCKED_DESCRIPTION.to_string());
            }
        }

        Ok(set)
    }

    /// The built-in definitions, already parsed — a test fixture standing in
    /// for a project's own `.spoolway/pipelines/`, which production always
    /// loads from disk with no fallback of its own.
    #[cfg(test)]
    pub fn builtin() -> Pipelines {
        let mut pipelines = Pipelines::assemble(
            builtin_pipelines().expect("built-in pipelines must parse"),
            &crate::config::Config::default(),
            Vec::new(),
            true,
        )
        .expect("built-in pipelines must be valid");

        // Most dispatcher tests need a runnable pipeline fixture, while the
        // assets now deliberately scaffold explicit blanks. Hydrate only this
        // test helper with representative choices; production always loads a
        // project's specialized files from disk.
        for pipeline in pipelines.pipelines.values_mut() {
            for step in &mut pipeline.steps {
                if step.kind() != StepKind::Agent || step.id == BLOCKED {
                    continue;
                }
                if step.id == "review" {
                    step.agent = Some("claude".into());
                    step.model = Some("claude-opus-5".into());
                    step.effort = Some("high".into());
                } else {
                    step.agent = Some("pi".into());
                    step.model = Some(crate::models::PLACEHOLDER.into());
                    step.effort = None;
                }
            }
        }
        pipelines
    }

    /// The two shipped pipelines, assembled against `config`. `spoolway
    /// pipeline check` no longer opens these — it derives every finding from
    /// a project's own loaded set — so this is release-time proof only:
    /// `src/assets.rs`'s own tests hold `assets/pipelines/*.yml` to the same
    /// structural rules and to neutrality about Pi, model and effort
    /// choices.
    #[cfg(test)]
    pub(crate) fn shipped(config: &crate::config::Config) -> Result<Pipelines> {
        Pipelines::assemble(builtin_pipelines()?, config, Vec::new(), true)
    }

    /// Look up a pipeline by name, with an error listing the defined ones.
    pub fn get(&self, name: &str) -> Result<&Pipeline> {
        self.pipelines.get(name).with_context(|| {
            format!(
                "no pipeline `{name}` (defined: {})",
                self.names().join(", ")
            )
        })
    }

    /// The pipeline a task runs on. Every task must name one by now —
    /// [`crate::commands::dispatch`]'s own start preflight refuses the whole
    /// run before any lane reaches this — so a task still missing one here
    /// is a defensive refusal, not the first place the absence is caught.
    pub fn for_task(&self, task: &crate::task::Task) -> Result<&Pipeline> {
        let name = task
            .front
            .pipeline
            .as_deref()
            .with_context(|| format!("task `{}` has no `pipeline:`", task.id()))?;
        self.get(name)
            .with_context(|| format!("task `{}` names a pipeline that does not exist", task.id()))
    }

    pub fn names(&self) -> Vec<&str> {
        self.pipelines.keys().map(String::as_str).collect()
    }

    /// Step ids across every pipeline, for matching lane names. A lane belongs
    /// to one task, and that task to one pipeline, so a step id shared between
    /// pipelines is not ambiguous in practice.
    pub fn all_step_ids(&self) -> Vec<&str> {
        let mut ids: Vec<&str> = self
            .pipelines
            .values()
            .flat_map(|p| p.steps.iter().map(|s| s.id.as_str()))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Every agent profile this file names, and what names it.
    pub fn referenced_agents(&self) -> BTreeMap<&str, Vec<&str>> {
        let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for pipeline in self.pipelines.values() {
            for (agent, steps) in pipeline.referenced_agents() {
                map.entry(agent).or_default().extend(steps);
            }
        }
        map
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_impl(true)
    }

    /// [`Pipelines::validate`], with the empty-set bail made optional — see
    /// [`Pipelines::load_or_empty`], the only caller that passes `false`.
    fn validate_impl(&self, require_nonempty: bool) -> Result<()> {
        if require_nonempty && self.pipelines.is_empty() {
            bail!("no pipelines defined");
        }
        for (name, pipeline) in &self.pipelines {
            pipeline
                .validate()
                .with_context(|| match &pipeline.private_file {
                    // Named by its own file rather than only `pipeline
                    // \`<name>\``: the generic wrapper `Pipelines::load_impl`
                    // adds around this ("in <tracked dir> or <private dir>")
                    // never says which of the two a private pipeline's error
                    // actually came from, and a person reading only that is
                    // sent to both directories to find one file.
                    Some(file) => format!("private pipeline `{name}` ({})", file.display()),
                    None => format!("pipeline `{name}`"),
                })?;
        }
        Ok(())
    }
}

/// Read `local/pipelines/` and `local/prompts/` for `root` and add them
/// beside the already-loaded tracked `pipelines` — called only in repo mode,
/// and only from [`Pipelines::load`] (never `load_tracked`), by
/// [`Pipelines::load_impl`]. See [`crate::local`].
///
/// One refusal here, fatal rather than skipped the way a stale override is:
/// a private pipeline or prompt whose name already belongs to a tracked one
/// (nothing private may stand in for a tracked file, so this cannot be
/// allowed to sit there silently shadowed).
///
/// A *tracked* pipeline's step naming a prompt that exists only privately —
/// that pipeline would break the moment it ran anywhere else — is just as
/// fatal, but is not checked here: the two name sets it needs are handed
/// back instead, for the caller to check once [`Pipelines::assemble`] has
/// materialised every pipeline's `blocked` step — see
/// [`refuse_tracked_steps_naming_a_private_only_prompt`]. Checking here,
/// against `pipeline.steps` as merely parsed, would for any pipeline relying
/// on `config.unattended.blocked_prompt`, or declaring a bare `blocked:`
/// step, check the wrong prompt — not the one the step will actually run.
fn merge_private(
    root: &Path,
    pipelines: &mut BTreeMap<String, Pipeline>,
) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let local = crate::local::dir_for(root)?;
    let local_pipelines_dir = crate::local::pipelines_dir(&local);
    let local_prompts_dir = crate::local::prompts_dir(&local);

    // The prompt name sets are gathered before either loop below touches
    // `pipelines`, so "found only in `local/`" always means what it says —
    // a tracked pipeline never gets to check itself against a set a private
    // pipeline has already added an entry to. Directory shape only for the
    // private side — see `crate::prompt::local_names_in` — so this agrees
    // with the one shape `crate::prompt::path_for` ever looks for privately.
    let tracked_prompt_names = crate::prompt::tracked_names_in(root)?;
    let local_prompt_names = crate::prompt::local_names_in(&local_prompts_dir)?;
    for name in &local_prompt_names {
        if tracked_prompt_names.contains(name) {
            bail!(
                "private prompt `{name}` clashes with the tracked one — both {} and {} name \
                 `{name}`; rename the private prompt",
                crate::prompt::tracked_path_in(root, name).display(),
                local_prompts_dir
                    .join(name)
                    .join(crate::assets::PROMPT_FILE)
                    .display(),
            );
        }
    }

    if let Some(files) = read_pipeline_dir(&local_pipelines_dir)? {
        for (name, file, raw) in files {
            if let Some(existing) = pipelines.get(&name) {
                // Two entries can reach this one name two ways: a genuine
                // clash with the tracked file, or two private files of the
                // same name under different extensions (`foo.yml` and
                // `foo.yaml`) — `read_pipeline_dir` returns both, and the
                // second one to land here would otherwise be blamed on the
                // tracked file it never touched, naming a tracked path that
                // does not even exist. `private_file` tells the two apart:
                // set only once this loop has already inserted one of this
                // pair, never by the tracked-parsing loop above it.
                if let Some(other_private) = &existing.private_file {
                    bail!(
                        "two private files name the pipeline `{name}` — both {} and {}; keep \
                         one and rename or delete the other",
                        other_private.display(),
                        file.display(),
                    );
                }
                // `file_in` always spells `.yml`; a tracked `.yaml` would be
                // named as a file that does not exist, the same slip the
                // private side's own path once made.
                let yml = Pipelines::file_in(root, &name);
                let tracked_file = if yml.is_file() {
                    yml
                } else {
                    Pipelines::dir_in(root).join(format!("{name}.yaml"))
                };
                bail!(
                    "private pipeline `{name}` clashes with the tracked one — both {} and {} \
                     name `{name}`; rename the private file",
                    tracked_file.display(),
                    file.display(),
                );
            }
            let mut pipeline =
                parse_unchecked(&name, &raw).with_context(|| format!("in {}", file.display()))?;
            // No `apply_pipeline_patch` here: the override layer patches a
            // pipeline the checkout already tracks, and a private pipeline
            // is not one — see the task's own non-goal against touching
            // that layer.
            pipeline.private_file = Some(file);
            pipelines.insert(name, pipeline);
        }
    }

    Ok((tracked_prompt_names, local_prompt_names))
}

/// A tracked pipeline naming a prompt found only privately would break on
/// every other machine, so it is refused — but only once every step a
/// tracked pipeline actually runs has been materialised: a pipeline relying
/// on `config.unattended.blocked_prompt`, or declaring a bare `blocked:`
/// step of its own, has no real `prompt:` on that step until
/// [`Pipelines::assemble`] has filled it in, so this must run after that,
/// not inside `merge_private`. A private pipeline (`private_file` set) is
/// exempt — it may freely name a private prompt, since it never runs
/// anywhere the private layer is absent.
fn refuse_tracked_steps_naming_a_private_only_prompt(
    pipelines: &BTreeMap<String, Pipeline>,
    tracked_prompt_names: &BTreeSet<String>,
    local_prompt_names: &BTreeSet<String>,
    local_prompts_dir: &Path,
) -> Result<()> {
    for (name, pipeline) in pipelines {
        if pipeline.private_file.is_some() {
            continue;
        }
        for step in &pipeline.steps {
            if step.kind() != StepKind::Agent {
                continue;
            }
            let prompt_name = step.prompt_name();
            if !tracked_prompt_names.contains(prompt_name)
                && local_prompt_names.contains(prompt_name)
            {
                bail!(
                    "tracked pipeline `{name}`/`{}` names prompt `{prompt_name}`, found only in \
                     {} — a tracked pipeline may only name a tracked prompt, since a private one \
                     would break on every other machine",
                    step.id,
                    local_prompts_dir.join(prompt_name).display(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: pipeline.steps — the steps a pipeline declares are what it runs, in the order it declares them

    #[test]
    fn built_in_pipeline_is_valid() {
        let pipeline = Pipelines::builtin().get("default").unwrap().clone();
        // The entry is the first step, not a key naming one.
        assert_eq!(pipeline.entry(), "implement");
        assert!(pipeline.step("review").is_some());
        assert!(pipeline.step("handover").is_some());
        // `queued`, `done` and `paused` are the dispatcher's, so no pipeline
        // declares them. `blocked` is the one exception — `Pipelines::builtin`
        // assembles it from `[unattended]`'s own defaults, the same as a
        // project's own pipelines get from config.toml.
        for reserved in RESERVED {
            if reserved == &BLOCKED {
                assert!(
                    pipeline.step(reserved).is_some(),
                    "`{reserved}` should be materialised from config"
                );
                continue;
            }
            assert!(
                pipeline.step(reserved).is_none(),
                "`{reserved}` is reserved and must not be a declared step"
            );
        }
    }

    /// A pipeline whose first step is `blocked` is refused, because a task
    /// would start parked and nothing could ever run it. The message names
    /// the step and says to put a working step first.
    #[test]
    fn a_pipeline_whose_first_step_is_blocked_is_refused() {
        let err = Pipeline::parse(
            "p15",
            "steps:\n  \
             - id: blocked\n    agent: claude\n    prompt: p\n    model: x\n  \
             - id: a\n    agent: claude\n    prompt: p\n    model: m\n    on_pass: done\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`blocked` is the first step"), "{err}");
        assert!(err.contains("put a working step first"), "{err}");
    }

    /// `loop: 0` is refused rather than read as no limit, and the message
    /// says a loop is 1 or more and names the edit that keeps 0.7's meaning:
    /// deleting the line, since `loop: 1` would block after one arrival.
    #[test]
    fn a_loop_of_zero_is_refused() {
        let err = Pipeline::parse(
            "sk",
            "steps:\n  \
             - id: a\n    agent: claude\n    prompt: p\n    model: m\n    loop: 0\n    \
               on_pass: b\n  \
             - id: b\n    agent: claude\n    prompt: p\n    model: m\n    on_pass: done\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("step `a`"), "{err}");
        assert!(err.contains("a loop is 1 or more"), "{err}");
        assert!(err.contains("delete `loop:` for no limit"), "{err}");
    }

    /// A step no route from the first step reaches is a warning, never a
    /// refusal, and `blocked` is not counted as one.
    #[test]
    fn a_step_no_route_reaches_is_warned_about_not_refused() {
        let pipeline = Pipeline::parse(
            "t",
            "steps:\n  \
             - id: a\n    agent: claude\n    prompt: p\n    model: m\n    on_pass: done\n  \
             - id: old\n    agent: claude\n    prompt: p\n    model: m\n    on_pass: done\n",
        )
        .expect("an unreachable step still loads");
        let warnings = pipeline.unreachable_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("step `old` is reached by no route"),
            "{warnings:?}"
        );

        let reached = Pipeline::parse(
            "t",
            "steps:\n  \
             - id: a\n    agent: claude\n    prompt: p\n    model: m\n    on_pass: b\n  \
             - id: b\n    agent: claude\n    prompt: p\n    model: m\n    on_pass: done\n",
        )
        .unwrap();
        assert!(reached.unreachable_warnings().is_empty());
    }

    /// An absent `version:` reads as `1.0` — the built-in `default` pipeline
    /// never sets one, per the task's own non-goal against touching this
    /// project's shipped pipelines.
    #[test]
    fn an_absent_version_reads_as_1_0() {
        let pipeline = Pipelines::builtin().get("default").unwrap().clone();
        assert_eq!(pipeline.version, "1.0");
    }

    /// `version:` is read into a plain `String`, as written — `1.10` stays
    /// `1.10` rather than folding to the `1.1` an `f64` would read it as,
    /// and spoolway never checks, compares or bumps it.
    #[test]
    fn version_is_read_as_written_not_folded_as_a_number() {
        let pipeline = Pipeline::parse(
            "solo",
            "version: 1.10\n\
             steps:\n  \
             - id: a\n    agent: pi\n    prompt: implementer\n    model: m\n    \
               on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(pipeline.version, "1.10");
    }

    /// `on_fail: blocked` is exactly what an absent `on_fail` already
    /// resolves to, so it warns — and a step with no `on_fail` at all draws
    /// nothing, since there is no redundant key there to name.
    #[test]
    fn redundant_on_fail_blocked_warns_once_per_step() {
        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  \
             - id: a\n    agent: pi\n    prompt: implementer\n    model: m\n    \
               on_pass: b\n    on_fail: blocked\n  \
             - id: b\n    agent: pi\n    prompt: implementer\n    model: m\n    \
               on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();

        let warnings = pipeline.redundant_on_fail_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("`a` declares `on_fail: blocked`"),
            "{warnings:?}"
        );
    }

    /// The doc comment on `redundant_on_fail_warnings` and `gate_warnings`
    /// both make a claim: the redundant key silences the gate warning
    /// without changing where a `report --fail` at that gate goes. This is
    /// the proof — a gated step declaring `on_fail: blocked` draws the
    /// redundant-key warning and not the gate warning; delete the key and
    /// the two swap places.
    #[test]
    fn a_gated_steps_redundant_on_fail_silences_its_own_gate_warning() {
        let with_key = Pipeline::parse(
            "solo",
            "steps:\n  \
             - id: a\n    agent: pi\n    prompt: implementer\n    model: m\n    \
               gate: true\n    on_pass: z\n    on_fail: blocked\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(with_key.redundant_on_fail_warnings().len(), 1);
        assert_eq!(with_key.gate_warnings().len(), 0);

        let without_key = Pipeline::parse(
            "solo",
            "steps:\n  \
             - id: a\n    agent: pi\n    prompt: implementer\n    model: m\n    \
               gate: true\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(without_key.redundant_on_fail_warnings().len(), 0);
        assert_eq!(without_key.gate_warnings().len(), 1);

        // Deleting the key changed nothing about where a fail or a block from
        // `a` actually goes — the whole point of the key being redundant.
        let a_with = with_key.step("a").unwrap();
        let a_without = without_key.step("a").unwrap();
        assert_eq!(
            a_with.destination(Outcome::Fail),
            a_without.destination(Outcome::Fail)
        );
        assert_eq!(
            a_with.destination(Outcome::Block),
            a_without.destination(Outcome::Block)
        );
    }

    /// A pull request is its own checkpoint — somebody reads it and merges it —
    /// so asking in a lane's pane as well is one checkpoint too many. `gate:`
    /// survives for a step that changes something no pull request would show
    /// anybody first, and the shipped pipelines have none of those.
    #[test]
    fn nothing_shipped_stops_to_ask() {
        for pipeline in Pipelines::builtin().pipelines.values() {
            for step in &pipeline.steps {
                assert!(
                    !step.gate,
                    "pipeline `{}` step `{}` gates",
                    pipeline.name, step.id
                );
            }
        }
    }

    /// A gate with nowhere named for a `report --fail` to go is legal — it
    /// parks on `blocked` the same as any other fail — but it is worth
    /// naming rather than leaving silent, so `gate_warnings` does.
    #[test]
    fn a_gate_with_no_on_fail_is_warned_about_by_name() {
        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: verify\n    agent: a\n    prompt: p\n    model: m\n    \
             gate: true\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let warnings = pipeline.gate_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("solo"), "{}", warnings[0]);
        assert!(warnings[0].contains("`verify`"), "{}", warnings[0]);
        assert!(warnings[0].contains("on_fail"), "{}", warnings[0]);
        assert!(warnings[0].contains(BLOCKED), "{}", warnings[0]);
    }

    /// A gate that does declare `on_fail` has somewhere a `report --fail`
    /// actually goes, so nothing here is worth a warning.
    #[test]
    fn a_gate_with_an_on_fail_is_not_warned_about() {
        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: verify\n    agent: a\n    prompt: p\n    model: m\n    \
             gate: true\n    on_pass: z\n    on_fail: fix\n  - id: fix\n    agent: a\n    \
             prompt: p\n    model: m\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        assert!(pipeline.gate_warnings().is_empty());
    }

    /// A pipeline with no `description:` still runs every task on it
    /// correctly — see `Pipeline::description_warnings` — so this is a
    /// warning, not a problem `validate` refuses.
    #[test]
    fn a_pipeline_with_no_description_is_warned_about_by_name() {
        let pipeline = Pipeline::parse("solo", "steps:\n  - id: z\n    end: true\n").unwrap();
        let warnings = pipeline.description_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("`solo`"), "{}", warnings[0]);
        assert!(warnings[0].contains("description"), "{}", warnings[0]);
    }

    /// A description long enough to stop being scannable in the one-line-per-
    /// pipeline list `pipeline list` prints is warned about too, but never
    /// truncated — the file keeps whatever it says.
    #[test]
    fn a_pipeline_with_a_long_description_is_warned_about_by_length() {
        let long = "x".repeat(401);
        let pipeline = Pipeline::parse(
            "solo",
            &format!("description: {long}\nsteps:\n  - id: z\n    end: true\n"),
        )
        .unwrap();
        let warnings = pipeline.description_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("`solo`"), "{}", warnings[0]);
        assert!(warnings[0].contains("401"), "{}", warnings[0]);
    }

    /// A three-byte em-dash must count as the one character it reads as —
    /// otherwise a description well under the threshold in the characters a
    /// person actually counts trips it anyway, on bytes nobody sees.
    #[test]
    fn a_description_with_multibyte_characters_is_counted_by_char_not_byte() {
        // 350 em-dashes: 1050 bytes, but 350 characters — under `TOO_LONG`.
        let description = "—".repeat(350);
        assert!(
            description.len() > 400,
            "test needs bytes past the threshold"
        );
        assert!(
            description.chars().count() < 400,
            "test needs chars under the threshold"
        );
        let pipeline = Pipeline::parse(
            "solo",
            &format!("description: {description}\nsteps:\n  - id: z\n    end: true\n"),
        )
        .unwrap();
        assert!(pipeline.description_warnings().is_empty());
    }

    /// A description within the length that stays scannable draws no warning
    /// at all.
    #[test]
    fn a_pipeline_with_a_short_description_is_not_warned_about() {
        let pipeline = Pipeline::parse(
            "solo",
            "description: A pipeline for a test.\nsteps:\n  - id: z\n    end: true\n",
        )
        .unwrap();
        assert!(pipeline.description_warnings().is_empty());
    }

    /// Both shipped pipelines end the same way, and end unconditionally: every
    /// task documents its own diff, then hands it over. `handover` is the
    /// last step either one runs — there is no `checks` step waiting behind
    /// it any more, since `gh pr checks --watch --fail-fast`'s exit code
    /// cannot tell a red build from GitHub not having registered a check yet,
    /// and that is not a thing spoolway tries to route on. A red build after
    /// handover is a person's to pick up. `land` is not a step any more, and
    /// nothing in a pipeline decides whether a step runs.
    ///
    /// `handover` is a command step — `run: spoolway stack`, no model and no
    /// rebase — and its failure routes straight to `blocked`: `spoolway
    /// stack` calls `gh pr view` first, so re-running `handover` from
    /// `blocked` is safe and there is no second, LLM-run escalation step to
    /// fall back to any more.
    // covers: step.end — a terminal step is where a pipeline stops, and every route has to reach one
    #[test]
    fn both_pipelines_end_at_document_then_handover() {
        for name in ["default", "bugfix"] {
            let pipeline = Pipelines::builtin().get(name).unwrap().clone();
            assert!(
                pipeline.step("land").is_none(),
                "pipeline `{name}` still has a `land` step"
            );
            assert!(
                pipeline.step("checks").is_none(),
                "pipeline `{name}` still has a `checks` step"
            );
            let document = pipeline.step("document").expect("`document`");
            assert_eq!(
                document.on_pass.as_deref(),
                Some("handover"),
                "pipeline `{name}`"
            );
            let handover = pipeline.step("handover").expect("`handover`");
            assert!(
                handover.agent.is_none() && handover.run.is_some(),
                "pipeline `{name}`: `handover` should be a command step running `spoolway stack`"
            );
            assert_eq!(handover.on_pass.as_deref(), Some(DONE), "pipeline `{name}`");
            // `on_fail` is absent, not `blocked`: that key is redundant with
            // no `on_fail` at all — see `Pipeline::redundant_on_fail_warnings`
            // — so the routing this test cares about is read from
            // `destination`, the one place it is actually decided.
            assert_eq!(
                handover.destination(Outcome::Fail),
                Some(BLOCKED),
                "pipeline `{name}`: a failed `spoolway stack` is a person's call, not \
                 another agent step"
            );
        }
    }

    /// Every shipped pipeline carries the same key reference, fenced, and it is
    /// the one `spoolway sync` writes. Four files hold a copy of this block
    /// and only one of them is the source; a `default` that drifted from a
    /// `bugfix` would hand half the projects the wrong table.
    #[test]
    fn every_shipped_pipeline_carries_the_same_fenced_key_reference() {
        let block = key_block();
        assert!(
            block.starts_with(crate::assets::PIPELINE_KEYS_BEGIN),
            "{block}"
        );
        assert!(block.trim_end().ends_with(crate::assets::PIPELINE_KEYS_END));
        assert!(block.contains("# Top level"));
        assert!(block.contains("# Steps"));

        for (name, raw) in BUILTIN_PIPELINES {
            assert_eq!(
                KEY_BLOCK.read(raw),
                Some(block),
                "pipeline `{name}` carries a different key reference"
            );
        }
    }

    /// The reference lists what a step may carry, so a key the parser knows and
    /// the block does not is a key nobody reading the file finds out about.
    #[test]
    fn the_key_reference_names_every_key_a_step_can_carry() {
        let block = key_block();
        for key in [
            "version",
            "task_template",
            "steps",
            "id",
            "description",
            "agent",
            "run",
            "end",
            "prompt",
            "model",
            "effort",
            "session",
            "slot",
            "gate",
            "on_pass",
            "on_fail",
            "loop",
            "timeout",
            "background",
            "headless",
            "last",
            "first",
            "serial",
        ] {
            assert!(
                block.contains(&format!("#   {key} ")),
                "the key reference says nothing about `{key}`"
            );
        }
    }

    /// A file still naming the retired `cleanup:` parses, the same way
    /// `blocked_on_write:` does — 0.1.0's shipped pipelines told projects to
    /// write it beside `end: true`, and reaching `done` already does what it
    /// used to opt a declared terminal into. It is ignored, and gone on the
    /// next save.
    #[test]
    fn the_retired_cleanup_key_parses_and_drops() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n  \
             - id: z\n    end: true\n    cleanup: true\n",
        )
        .expect("a pipeline naming the retired cleanup key must still parse");
        assert!(pipeline.step("z").unwrap().end);
        let rendered = serde_norway::to_string(&pipeline).unwrap();
        assert!(!rendered.contains("cleanup"), "{rendered}");
    }

    /// What the board's NEXT column asks. One hop along `on_pass` — nothing in
    /// a pipeline can make a step fall through any more.
    #[test]
    fn the_next_step_is_the_one_on_pass_names() {
        let pipeline = Pipelines::builtin().get("default").unwrap().clone();
        assert_eq!(pipeline.next_running_step("implement"), Some("review"));
        assert_eq!(pipeline.next_running_step("review"), Some("document"));
        assert_eq!(pipeline.next_running_step("document"), Some("handover"));
        // `handover` is the last thing that runs — `done` is not a step.
        assert_eq!(pipeline.next_running_step("handover"), None);
        assert_eq!(pipeline.next_running_step("nowhere"), None);
    }

    #[test]
    fn later_steps_outrank_earlier_ones_by_default() {
        let pipeline = Pipelines::builtin().get("default").unwrap().clone();
        assert!(
            pipeline.priority("handover") > pipeline.priority("implement"),
            "work near the end should be scheduled before new work"
        );
    }

    /// A failed review goes back to the step that wrote the code, in that
    /// step's own session — not to a `fix` step of its own.
    ///
    /// There used to be one. It named the same agent, the same prompt and the
    /// same model as `implement`, and differed only in carrying `session:
    /// true`, so the pipeline drew a hand-off between two steps that were the
    /// same worker. What it bought was a rewind: the reviewer's finding
    /// travelled to a step, which travelled back to the reviewer. `implement`
    /// carries the session now and the step is gone.
    #[test]
    fn review_routes_failures_back_to_the_step_that_wrote_the_code() {
        let pipeline = Pipelines::builtin().get("default").unwrap().clone();
        let review = pipeline.step("review").unwrap();
        assert_eq!(review.destination(Outcome::Fail), Some("implement"));
        assert_eq!(review.destination(Outcome::Pass), Some("document"));
        assert_eq!(review.destination(Outcome::Block), Some(BLOCKED));
        assert!(
            pipeline.step("fix").is_none(),
            "a `fix` step is a rewind with a name"
        );
        assert!(
            pipeline.step("implement").unwrap().session,
            "the implementer has to keep its conversation for the round trip to be worth anything"
        );
    }

    fn parse(yaml: &str) -> Result<Pipeline> {
        let pipeline: Pipeline = serde_norway::from_str(yaml)?;
        pipeline.validate()?;
        Ok(pipeline)
    }

    /// `headless:` is a command step's key, same as `background:` and
    /// `timeout:` beside it — an agent step declaring it names a lane's
    /// pane, which is not what this key turns off.
    #[test]
    fn headless_is_refused_on_a_step_that_runs_no_command() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    headless: true\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("step `a`"), "{err}");
        assert!(err.to_string().contains("headless"), "{err}");
    }

    /// A pipeline may declare `blocked` as an ordinary agent step, and one
    /// that does not still validates exactly as before.
    #[test]
    fn a_pipeline_may_declare_blocked_as_a_step() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n  \
             - id: z\n    end: true\n  \
             - id: blocked\n    agent: pi\n    session: true\n",
        )
        .unwrap();
        assert!(pipeline.step(BLOCKED).is_some());
        assert!(pipeline.blocked_is_staffed(true));
        assert!(!pipeline.blocked_is_staffed(false));
    }

    /// The other three reserved names are exactly as reserved as before —
    /// `blocked` is the one carve-out, not a hole in the rule.
    #[test]
    fn queued_done_and_paused_stay_off_limits() {
        for reserved in [QUEUED, DONE, PAUSED] {
            let err = parse(&format!(
                "steps:\n  - id: {reserved}\n    agent: pi\n    on_pass: {reserved}\n"
            ))
            .unwrap_err();
            assert!(err.to_string().contains("reserved stage name"), "{err}");
        }
    }

    /// Each of the four keys `blocked` routes itself with is refused by name,
    /// with a message that says what actually decides it.
    #[test]
    fn blocked_refuses_the_keys_it_routes_itself_with() {
        let cases: &[(&str, &str)] = &[
            ("on_pass: z\n", "on_pass"),
            ("on_fail: z\n", "on_fail"),
            ("gate: true\n", "gate"),
            ("end: true\n", "end"),
        ];
        for (key, message) in cases {
            let err = parse(&format!(
                "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n  \
                 - id: blocked\n    agent: pi\n    {key}  \
                 - id: z\n    end: true\n"
            ))
            .unwrap_err();
            assert!(err.to_string().contains(message), "{message}: {err}");
        }
    }

    /// A step may not name itself as where its own `on_pass` or `on_fail`
    /// leads — the shape a bounded retry used to be built on, now refused
    /// outright at load rather than raced by the dispatcher's wait loop.
    #[test]
    fn a_step_may_not_route_back_to_its_own_id() {
        let cases: &[(&str, &str)] = &[
            ("on_pass: a\n", "on_pass"),
            ("on_pass: z\n    on_fail: a\n", "on_fail"),
        ];
        for (keys, key) in cases {
            let err = parse(&format!(
                "steps:\n  - id: a\n    agent: pi\n    {keys}  \
                 - id: z\n    end: true\n"
            ))
            .unwrap_err();
            let message = err.to_string();
            assert!(message.contains("step `a`"), "{key}: {message}");
            assert!(message.contains(&format!("`{key}`")), "{key}: {message}");
            assert!(message.contains("names `a` itself"), "{key}: {message}");
            assert!(
                message.contains("may not route back to its own id"),
                "{key}: {message}"
            );
        }
    }

    /// `blocked` is exempt from the on_pass-required check every other agent
    /// step is held to, and from the unbounded-loop check — it has no
    /// declared destinations at all, so it cannot self-edge into one.
    #[test]
    fn blocked_needs_no_on_pass_and_forms_no_loop() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: done\n  - id: blocked\n    agent: pi\n",
        )
        .unwrap();
        let blocked = pipeline.step(BLOCKED).unwrap();
        assert!(pipeline.destinations(blocked).is_empty());
    }

    /// One pipeline, one step besides the entry, so every test below can name
    /// a small pipeline without repeating the boilerplate that makes it a
    /// valid graph on its own.
    ///
    /// Built with [`parse_unchecked`], the same as `Pipelines::load` builds
    /// one — never the local `parse()` above, which validates immediately
    /// and so would refuse a declared `blocked` override before
    /// `Pipelines::assemble` has had the chance to fill in whatever it left
    /// out.
    fn one_step_pipeline(id: &str, extra: &str) -> BTreeMap<String, Pipeline> {
        let yaml = format!(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n  \
             - id: z\n    end: true\n{extra}"
        );
        BTreeMap::from([(id.to_string(), parse_unchecked(id, &yaml).unwrap())])
    }

    /// A pipeline that declares no `blocked` step of its own gets one built
    /// straight from `[unattended]`'s `blocked_*` keys, appended last, and
    /// `pipeline_check` — via `Pipelines::validate` — sees the same step
    /// `pipeline.step("blocked")` finds.
    #[test]
    fn assemble_materialises_blocked_from_config_for_a_pipeline_declaring_none() {
        let mut config = crate::config::Config::default();
        config.unattended.blocked_agent = "pi".into();
        config.unattended.blocked_model = "my-model".into();
        config.unattended.blocked_effort = "high".into();
        config.unattended.blocked_prompt = "clearer".into();
        config.unattended.blocked_session = false;

        let set =
            Pipelines::assemble(one_step_pipeline("solo", ""), &config, Vec::new(), true).unwrap();
        let pipeline = set.get("solo").unwrap();

        assert!(
            !pipeline.blocked_declared,
            "the file named no `blocked` step"
        );
        let blocked = pipeline.step(BLOCKED).unwrap();
        assert_eq!(pipeline.steps.last().unwrap().id, BLOCKED, "appended last");
        assert_eq!(blocked.agent.as_deref(), Some("pi"));
        assert_eq!(blocked.model.as_deref(), Some("my-model"));
        assert_eq!(blocked.effort.as_deref(), Some("high"));
        assert_eq!(blocked.prompt.as_deref(), Some("clearer"));
        assert!(!blocked.session);
        assert_eq!(blocked.description.as_deref(), Some(BLOCKED_DESCRIPTION));
    }

    /// A pipeline's own `- id: blocked` may leave any of its five keys out,
    /// and each one left out falls back to `[unattended]` — independently of
    /// the others.
    #[test]
    fn assemble_fills_in_whatever_a_declared_override_left_out() {
        let mut config = crate::config::Config::default();
        config.unattended.blocked_agent = "claude".into();
        config.unattended.blocked_model = "config-model".into();
        config.unattended.blocked_effort = "medium".into();
        config.unattended.blocked_prompt = "unblocker".into();
        config.unattended.blocked_session = true;

        let set = Pipelines::assemble(
            one_step_pipeline("ui", "  - id: blocked\n    model: override-model\n"),
            &config,
            Vec::new(),
            true,
        )
        .unwrap();
        let pipeline = set.get("ui").unwrap();

        assert!(pipeline.blocked_declared);
        let blocked = pipeline.step(BLOCKED).unwrap();
        // Named on the step: kept.
        assert_eq!(blocked.model.as_deref(), Some("override-model"));
        // Left out: every one of the other four falls back to config.
        assert_eq!(blocked.agent.as_deref(), Some("claude"));
        assert_eq!(blocked.effort.as_deref(), Some("medium"));
        assert_eq!(blocked.prompt.as_deref(), Some("unblocker"));
        assert!(blocked.session);
        assert_eq!(blocked.description.as_deref(), Some(BLOCKED_DESCRIPTION));
    }

    /// Anything besides the five keys is refused by name on a declared
    /// override, whether or not the resulting graph would otherwise be a
    /// valid one.
    #[test]
    fn assemble_refuses_anything_but_the_five_keys_on_a_declared_override() {
        let config = crate::config::Config::default();
        let cases: &[(&str, &str)] = &[
            ("description: nope\n", "description"),
            ("slot: false\n", "slot"),
            ("loop: 3\n", "loop"),
        ];
        for (key, message) in cases {
            let pipelines = one_step_pipeline("p", &format!("  - id: blocked\n    {key}"));
            let err = Pipelines::assemble(pipelines, &config, Vec::new(), true).unwrap_err();
            // The context `assemble` wraps this in (`pipeline \`p\``) only
            // shows up under the alternate `{:#}` format — anyhow's plain
            // `Display` prints just the outermost message.
            assert!(format!("{err:#}").contains(message), "{message}: {err:#}");
        }
    }

    /// `Pipelines::validate` — the check `pipeline_check` and `doctor` both
    /// run again on an already-assembled set — must not itself refuse the
    /// canonical description `assemble` just wrote onto a materialised
    /// `blocked` step. Regression: the description refusal used to live
    /// inside `Pipeline::validate`, which runs on every call, and so refused
    /// its own work the moment anything re-validated the assembled pipeline.
    #[test]
    fn revalidating_an_assembled_pipeline_does_not_refuse_its_own_blocked_description() {
        let config = crate::config::Config::default();
        let set =
            Pipelines::assemble(one_step_pipeline("solo", ""), &config, Vec::new(), true).unwrap();
        set.validate()
            .expect("an assembled set must validate again cleanly");
    }

    /// A blank `unattended.blocked_model` does not stop the config, or the
    /// pipelines built from it, from loading — the refusal is at
    /// `spoolway dispatch` and `spoolway doctor`, not here, so
    /// `spoolway config set` stays usable to fix it.
    #[test]
    fn a_blank_blocked_model_still_assembles() {
        let mut config = crate::config::Config::default();
        config.unattended.blocked_model = String::new();

        let set =
            Pipelines::assemble(one_step_pipeline("solo", ""), &config, Vec::new(), true).unwrap();
        assert_eq!(set.get("solo").unwrap().step(BLOCKED).unwrap().model, None);
    }

    /// A pipeline file still naming `blocked_on_write:` at either level must
    /// still parse, ignored on both `Pipeline` and `Step` the same way an old
    /// key in `config.toml` is — see `crate::config`'s own test for the
    /// absorbing field this mirrors.
    #[test]
    fn an_old_blocked_on_write_at_either_level_still_parses() {
        let pipeline = parse(
            "blocked_on_write:\n  - pipeline-only/**\nsteps:\n  \
             - id: a\n    agent: pi\n    blocked_on_write:\n      - step-only/**\n    \
             on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .expect("a pipeline naming the retired key must still parse");
        assert!(pipeline.step("a").is_some());
    }

    /// A step id becomes a file name three times over — the lane's prompt, its
    /// headless record, a command step's log — so one that can climb out of
    /// `.spoolway/` is refused where the pipeline is validated, not discovered
    /// as a file in the repo root.
    // covers: step.id — a step id has to be usable as a lane name and a path segment
    #[test]
    fn a_step_id_that_escapes_its_directory_is_refused() {
        let escaping = parse(
            "steps:\n  - id: ../../pwn\n    run: make\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        );
        assert!(escaping.is_err(), "`../../pwn` was accepted as a step id");

        // The empty id it used to be the only shape refused, still refused.
        assert!(parse("steps:\n  - id: \"\"\n    end: true\n").is_err());
    }

    /// The shape a command step is written in, and the whole of what it needs:
    /// a line to run and somewhere to go afterwards.
    #[test]
    fn accepts_a_command_step() {
        let pipeline = parse(
            "steps:\n  - id: a\n    run: cargo test\n    \
             on_pass: z\n    on_fail: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let step = pipeline.step("a").unwrap();
        assert_eq!(step.run.as_deref(), Some("cargo test"));
        assert!(
            !step.background,
            "a command step waits unless it says not to"
        );
    }

    /// The bound every command step has whether it writes one or not — an
    /// unbounded command is the one thing a pass cannot notice has gone wrong.
    #[test]
    fn a_command_step_is_bounded_whether_it_says_so_or_not() {
        let pipeline = parse(
            "steps:\n  - id: a\n    run: make\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(
            pipeline.step("a").unwrap().command_timeout(),
            DEFAULT_COMMAND_TIMEOUT
        );

        let pinned = parse(
            "steps:\n  - id: a\n    run: make\n    \
             timeout: 2h\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(
            pinned.step("a").unwrap().command_timeout(),
            Duration::from_secs(7200),
            "a step that knows better than the default must be able to say so"
        );
    }

    /// `timeout: 0s` reads as "no limit" and would mean the opposite: killed the
    /// moment it starts.
    #[test]
    fn rejects_a_zero_timeout() {
        let err = parse(
            "steps:\n  - id: a\n    run: make\n    \
             timeout: 0s\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("as soon as it started"), "{err}");
    }

    /// `timeout:` bounds a shell command. An agent step runs no command of its
    /// own, so `timeout:` on one would bound nothing.
    #[test]
    fn rejects_a_timeout_on_a_step_that_runs_no_command() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    timeout: 5m\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("declares `timeout:`"), "{err}");
    }

    /// A step that names nothing to run and does not `end:` is a step whose
    /// keys were mistyped, and there is one error for it rather than one per
    /// kind it might have meant to be. That collapse is what dropping `kind:`
    /// bought: the keys are the discriminator, so there is nothing left to be
    /// inconsistent with.
    #[test]
    fn rejects_a_step_that_names_nothing_to_run() {
        let err = parse(
            "steps:\n  - id: a\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("no `run:`"), "{err}");
    }

    /// `run:` and `agent:` are the two discriminators, so a step carrying both
    /// says it is two things. Refused rather than resolved by precedence: one
    /// of the two lines is a mistake, and guessing which is worse than saying so.
    // covers: step.agent — a step names an agent or a command, never both
    #[test]
    fn rejects_a_step_that_names_both_a_command_and_an_agent() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    run: cargo test\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("names both `run:` and `agent:`"),
            "{message}"
        );
    }

    /// A command step starts no lane, so every key that configures one is a key
    /// written in the belief that it does something here.
    #[test]
    fn rejects_a_command_step_that_configures_a_lane() {
        for (key, line) in [
            ("agent", "    agent: pi\n"),
            ("prompt", "    prompt: builder\n"),
            ("model", "    model: some-model\n"),
            ("effort", "    effort: high\n"),
            ("session", "    session: true\n"),
            ("gate", "    gate: true\n"),
            ("allow", "    allow: [push]\n"),
        ] {
            let yaml = format!(
                "steps:\n  - id: a\n    run: make\n\
                 {line}    on_pass: z\n  - id: z\n    end: true\n"
            );
            let err = parse(&yaml).unwrap_err();
            assert!(
                err.to_string().contains(key),
                "`{key}` on a command step went unnoticed: {err}"
            );
        }
    }

    /// `last:` walks every task but one past the step, and a lane nobody
    /// started is a prompt that never reports — so whatever the step was for
    /// goes unjudged rather than undone. It is a command step's key, and every
    /// other kind is refused it.
    #[test]
    fn rejects_last_on_a_step_that_runs_no_command() {
        for kind in [
            "steps:\n  - id: a\n    agent: pi\n    model: m\n    last: true\n    \
             on_pass: z\n  - id: z\n    end: true\n",
            "steps:\n  - id: a\n    agent: pi\n    model: m\n    on_pass: z\n  \
             - id: z\n    end: true\n    last: true\n",
        ] {
            let err = parse(kind).unwrap_err();
            let message = err.to_string();
            assert!(message.contains("`last:`"), "{message}");
            assert!(message.contains("runs no command"), "{message}");
        }
    }

    /// `first:` runs a command step for a chain's declared root and walks
    /// every other task past it — the same shape as `last:`, so it is
    /// refused wherever `last:` is: a lane nobody started reports nothing.
    #[test]
    fn rejects_first_on_a_step_that_runs_no_command() {
        for kind in [
            "steps:\n  - id: a\n    agent: pi\n    model: m\n    first: true\n    \
             on_pass: z\n  - id: z\n    end: true\n",
            "steps:\n  - id: a\n    agent: pi\n    model: m\n    on_pass: z\n  \
             - id: z\n    end: true\n    first: true\n",
        ] {
            let err = parse(kind).unwrap_err();
            let message = err.to_string();
            assert!(message.contains("`first:`"), "{message}");
            assert!(message.contains("runs no command"), "{message}");
        }
    }

    /// `first:` and `last:` ask opposite questions about the same chain, so a
    /// command step declaring both has no single command left to be, root or
    /// not.
    #[test]
    fn rejects_a_command_step_that_declares_both_first_and_last() {
        let err = parse(
            "steps:\n  - id: a\n    run: make\n    first: true\n    last: true\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("`first:`"), "{message}");
        assert!(message.contains("`last:`"), "{message}");
    }

    /// `first` parses and round-trips as a boolean, defaulting to false the
    /// same way `last` does.
    #[test]
    fn first_parses_and_defaults_to_false() {
        let pipeline = parse(
            "steps:\n  - id: a\n    run: make\n    first: true\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        assert!(pipeline.step("a").unwrap().first);
        assert!(
            !pipeline.step("z").unwrap().first,
            "absent defaults to false"
        );

        let rendered = serde_norway::to_string(&pipeline).unwrap();
        assert!(rendered.contains("first: true"), "{rendered}");
    }

    /// `serial` parses and round-trips as a boolean, defaulting to false the
    /// same way `first` does.
    #[test]
    fn serial_parses_and_defaults_to_false() {
        let pipeline = parse(
            "steps:\n  - id: a\n    run: make\n    serial: true\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        assert!(pipeline.step("a").unwrap().serial);
        assert!(
            !pipeline.step("z").unwrap().serial,
            "absent defaults to false"
        );

        let rendered = serde_norway::to_string(&pipeline).unwrap();
        assert!(rendered.contains("serial: true"), "{rendered}");
    }

    /// A lane is not a run the dispatcher starts and can hold back, so
    /// `serial:` on an agent step is refused — worded as the task's mockup
    /// draws it, under the `pipeline `<name>`:` prefix loading adds.
    #[test]
    fn rejects_serial_on_an_agent_step() {
        let err = parse(
            "steps:\n  - id: review\n    agent: pi\n    model: m\n    serial: true\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "step `review` declares `serial:` but is an agent step — only a command \
             step's run can be held for its turn"
        );
    }

    /// `handover:` marked which step opened the pull request and `credentials:`
    /// named what a step could reach. Both are gone — a lane reaches whatever
    /// the person running the dispatcher can, and which step hands over is
    /// legible from the step itself. A pipeline file still carrying either was
    /// written against a spoolway that read them, and silently ignoring the key
    /// is how it would go on believing they were honoured.
    #[test]
    fn rejects_the_retired_forge_keys() {
        for (key, line) in [
            ("handover", "    handover: true\n"),
            ("credentials", "    credentials: true\n"),
        ] {
            let yaml = format!(
                "steps:\n  - id: a\n    agent: pi\n    model: m\n\
                 {line}    on_pass: z\n  - id: z\n    end: true\n"
            );
            let err = parse(&yaml).unwrap_err();
            assert!(
                err.to_string().contains(key),
                "`{key}` survived its own removal: {err}"
            );
        }
    }

    /// `session:` is a plain bool now — `true` round-trips, and a step that
    /// sets none writes nothing back out.
    #[test]
    fn session_parses_and_serialises_as_a_bool() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    session: true\n    on_pass: b\n  \
             - id: b\n    agent: pi\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        assert!(pipeline.step("a").unwrap().session);
        assert!(!pipeline.step("b").unwrap().session);

        let rendered = serde_norway::to_string(&pipeline).unwrap();
        assert!(rendered.contains("session: true"), "{rendered}");
        // Step `b` names no `session:` at all, and writes nothing back — the
        // omission is what `skip_serializing_if` buys.
        assert_eq!(
            rendered.matches("session").count(),
            1,
            "a step that sets no `session:` should not gain one on the way out: {rendered}"
        );
    }

    /// `session: false` says the same nothing as leaving the key out — the
    /// bound this key used to carry moved to the agent profile, so there is
    /// nothing left for `false` to be a mistaken spelling of.
    #[test]
    fn session_false_parses_as_the_default() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    session: false\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        assert!(!pipeline.step("a").unwrap().session);
    }

    /// The percentage form used to live here and is gone: anything that is
    /// not `true` or `false` is refused with a message naming where the
    /// bound lives now, rather than serde's own "invalid type" text about a
    /// Rust bool.
    #[test]
    fn rejects_anything_that_is_not_a_bool() {
        for bad in ["60%", "60", "abc"] {
            let yaml = format!(
                "steps:\n  - id: a\n    agent: pi\n    session: {bad}\n    on_pass: z\n  \
                 - id: z\n    end: true\n"
            );
            let err = match parse(&yaml) {
                Err(err) => err,
                Ok(_) => panic!("`session: {bad}` should be refused"),
            };
            let message = err.to_string();
            assert!(
                message.contains("agents.<profile>.session_reuse_ctx"),
                "`session: {bad}` → {message}"
            );
        }
    }

    /// Two steps that run the same prompt under `session:` are one
    /// conversation, so they had better agree on which agent profile it
    /// runs under — a session cannot be resumed by two different CLIs.
    #[test]
    fn rejects_two_session_steps_on_the_same_prompt_with_different_agents() {
        let err = parse(
            "steps:\n  \
             - id: review\n    agent: claude\n    prompt: worker\n    session: true\n    \
             on_pass: z\n    on_fail: fix\n  \
             - id: fix\n    agent: pi\n    prompt: worker\n    session: true\n    \
             on_pass: review\n  \
             - id: z\n    end: true\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("prompt `worker`"), "{message}");
        assert!(message.contains("one session, two profiles"), "{message}");
    }

    /// Same prompt, same agent, from two different steps: the ordinary
    /// shape of a `session:` pair (`implement` and `fix` sharing the
    /// `implementer` prompt), and nothing here should refuse it.
    #[test]
    fn accepts_two_session_steps_on_the_same_prompt_and_agent() {
        parse(
            "steps:\n  \
             - id: implement\n    agent: pi\n    prompt: implementer\n    session: true\n    \
             on_pass: fix\n  \
             - id: fix\n    agent: pi\n    prompt: implementer\n    session: true\n    \
             on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
    }

    /// `skills: a, b` names two skills, in the order written.
    // covers: step.skills — a comma-separated line becomes one name per skill
    #[test]
    fn skills_parses_a_comma_separated_line_into_one_name_each() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    skills: code-review, spoolway-doctor\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(
            pipeline.step("a").unwrap().skills,
            vec!["code-review", "spoolway-doctor"]
        );
    }

    /// A name copied from an existing slash invocation carries its leading
    /// `/`, and should work exactly as well as one copied from the skill
    /// listing, which never has one.
    // covers: step.skills — a leading slash is optional and stripped either way
    #[test]
    fn skills_strips_a_leading_slash() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    skills: /code-review, /spoolway-doctor\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(
            pipeline.step("a").unwrap().skills,
            vec!["code-review", "spoolway-doctor"]
        );
    }

    /// Whitespace around a name — from a line wrapped or padded for
    /// alignment — is not part of the name.
    // covers: step.skills — surrounding whitespace is trimmed off each name
    #[test]
    fn skills_trims_surrounding_whitespace() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    skills: \"  code-review ,  spoolway-doctor  \"\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(
            pipeline.step("a").unwrap().skills,
            vec!["code-review", "spoolway-doctor"]
        );
    }

    /// A trailing comma, a doubled one, or a blank line should not leave an
    /// empty name sitting in the list — there is nothing to turn into a
    /// slash invocation for it.
    // covers: step.skills — an empty item between or after commas is dropped
    #[test]
    fn skills_drops_empty_items() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    skills: \"code-review, , spoolway-doctor,\"\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(
            pipeline.step("a").unwrap().skills,
            vec!["code-review", "spoolway-doctor"]
        );
    }

    /// An item is a bare name, never a name plus arguments — a comma
    /// separates skills, so whitespace surviving trim inside one item means
    /// it was two words, not two skills.
    // covers: Pipeline::validate — a skill name holding whitespace is refused
    #[test]
    fn skills_refuses_a_name_holding_whitespace() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    skills: \"code review\"\n    \
             on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("code review"), "{message}");
        assert!(message.contains("whitespace"), "{message}");
    }

    /// A step naming no `skills:` writes nothing back — the same as any other
    /// key that skips a default.
    // covers: step.skills — absent skills serialize away entirely
    #[test]
    fn skills_absent_serializes_without_the_key() {
        let pipeline =
            parse("steps:\n  - id: a\n    agent: pi\n    on_pass: z\n  - id: z\n    end: true\n")
                .unwrap();
        assert!(pipeline.step("a").unwrap().skills.is_empty());
        let rendered = serde_norway::to_string(&pipeline).unwrap();
        assert!(!rendered.contains("skills"), "{rendered}");
    }

    #[test]
    fn rejects_a_transition_to_an_unknown_step() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: nowhere\n  - id: b\n    end: true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown step `nowhere`"));
    }

    /// Two steps that only ever route to each other. Both routes are
    /// explicit on both, so neither ever falls through to the reserved
    /// `blocked` either — which is the one way a graph can still strand a
    /// task now that both endings are built in.
    #[test]
    fn rejects_a_cycle_with_no_way_out() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: b\n    on_fail: b\n  \
             - id: b\n    agent: pi\n    on_pass: a\n    on_fail: a\n",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("can never reach a terminal step"),
            "{err}"
        );
    }

    /// The loop that reaches a terminal step on every passing run and still
    /// never ends: two agents that keep saying fail to each other. Nothing in
    /// the reachability check sees it, which is why this one exists.
    #[test]
    fn rejects_a_loop_nothing_bounds() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n    on_fail: b\n  - id: b\n    agent: pi\n    on_pass: z\n    on_fail: a\n  - id: z\n    end: true\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("form a loop nothing bounds"), "{message}");
        assert!(message.contains("`a`"), "{message}");
        assert!(message.contains("`b`"), "{message}");
    }

    /// One bounded route is enough for the whole cycle: it escalates, its
    /// exit leaves, and the task leaves too. Demanding one on every route
    /// would refuse the pipeline we ship.
    #[test]
    fn accepts_a_loop_bounded_anywhere_along_it() {
        parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n    on_fail: b\n  - id: b\n    agent: pi\n    loop: 2\n    on_pass: z\n    on_fail: a\n  - id: z\n    end: true\n",
        )
        .unwrap();
    }

    /// A bounded route used to break a cycle only if its exit left it, and
    /// `fix` bounding its move to `build` and giving up straight back into
    /// `build` was the shape that refusal existed for. A spent budget parks on
    /// `blocked` now, which leaves every cycle from wherever it is spent, so
    /// this same file is a bounded loop and the check accepts it — the bound
    /// living on `build`, the step the cycle's edge arrives at, rather than on
    /// `fix`, the step that sends it.
    #[test]
    fn a_bounded_route_breaks_its_cycle_wherever_the_bound_sits() {
        parse(
            "steps:\n  - id: fix\n    agent: pi\n    on_pass: build\n    on_fail: blocked\n  \
             - id: build\n    agent: pi\n    loop: 3\n    on_pass: fix\n    on_fail: blocked\n",
        )
        .unwrap();
    }

    /// `loop:` as a map is the retired per-route shape, refused whether or
    /// not the step it names would have routed there — the shape itself is
    /// gone, not just a typo inside it — and the message says where the
    /// limit belongs now: on the step the map named, as a bare number of its
    /// own.
    #[test]
    fn rejects_the_retired_loop_map_form_by_name() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n    on_fail: b\n  - id: b\n    agent: pi\n    loop:\n      a: 2\n    on_pass: a\n    on_fail: a\n  - id: z\n    end: true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("step `b`"), "{err}");
        assert!(err.contains("declares `loop:` as a map"), "{err}");
        assert!(err.contains("give `a` a `loop: <n>` of its own"), "{err}");
    }

    /// The rename is the message, one hop further back than the last one: a
    /// file that kept `max_rounds:` is bounding a counter that no longer
    /// exists either, so it is refused by name rather than by serde's list of
    /// every other key a step may carry.
    #[test]
    fn the_retired_max_rounds_key_is_refused_by_name() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    max_rounds: 2\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`loop:`"), "{err}");
    }

    /// The key it was renamed to before that, refused the same way.
    #[test]
    fn the_retired_max_new_sessions_key_is_refused_by_name() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    max_new_sessions: 2\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`loop:`"), "{err}");
        assert!(err.contains("session_reuse_ctx"), "{err}");
    }

    /// `on_loop_max:` chose where a spent loop budget went, and across seven
    /// pipelines and twenty-eight bounded steps nobody ever chose anything but
    /// `blocked`. The key is retired, and a file still carrying it is refused
    /// by name — this message is what a hand-written file that still names
    /// the key meets, and it has to name the file to open and the step to
    /// find in it without a reader going to the release notes.
    // covers: step.on_loop_max — retired, and refused by name rather than by `deny_unknown_fields`
    #[test]
    fn the_retired_on_loop_max_key_is_refused_by_name() {
        let err = Pipeline::parse(
            "impl_ui",
            "steps:\n  - id: implement\n    agent: pi\n    loop: 2\n    on_pass: review\n  \
             - id: review\n    agent: pi\n    \
             on_loop_max: blocked\n    on_pass: z\n    on_fail: implement\n  \
             - id: z\n    end: true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("impl_ui"), "the pipeline to open: {err}");
        assert!(err.contains("step `review`"), "the step to edit: {err}");
        assert!(err.contains("`on_loop_max:`"), "the key by name: {err}");
        assert!(err.contains("delete it"), "what to do about it: {err}");
        // Not serde's own list of every other key a step may carry, which is
        // what `deny_unknown_fields` would have answered with.
        assert!(!err.contains("unknown field"), "{err}");
    }

    /// Every retired shape this pipeline's steps carry, listed rather than
    /// stopping at the first — the collecting form `Pipelines::refusals`
    /// uses so `pipeline check` and `doctor` can say all of it in one pass.
    #[test]
    fn retired_shape_problems_lists_every_occurrence_not_just_the_first() {
        let raw = "steps:\n  \
                    - id: a\n    agent: pi\n    on_pass: b\n    on_fail: a\n  \
                    - id: b\n    agent: pi\n    loop:\n      a: 1\n    on_pass: z\n    on_fail: a\n  \
                    - id: z\n    end: true\n    on_loop_max: blocked\n  \
                    - id: w\n    agent: pi\n    loop: 0\n    on_pass: done\n";
        let pipeline: Pipeline = serde_norway::from_str(raw).unwrap();
        let problems = pipeline.retired_shape_problems();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("step `a`") && p.contains("may not route back")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("step `b`") && p.contains("declares `loop:` as a map")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("step `z`") && p.contains("on_loop_max:")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("step `w`") && p.contains("delete `loop:` for no limit")),
            "{problems:?}"
        );
        assert_eq!(problems.len(), 4, "{problems:?}");
    }

    /// `Pipelines::refusals` across a whole directory, not one pipeline: two
    /// files, each carrying a retired shape, both named — acceptance
    /// criterion 4's "across every pipeline file", proven directly rather
    /// than only through `pipeline_check`'s own count.
    #[test]
    fn refusals_names_every_file_and_step_across_the_whole_directory() {
        let root = crate::scratch::root("pipeline-refusals-across-files");
        let _ = std::fs::remove_dir_all(&root);
        let dir = Pipelines::dir_in(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("default.yml"),
            "steps:\n  \
             - id: implement\n    agent: pi\n    on_pass: checks\n  \
             - id: checks\n    run: gh pr checks\n    loop:\n      checks: 3\n    \
             on_pass: done\n    on_fail: checks\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("bugfix.yml"),
            "steps:\n  \
             - id: fix\n    agent: pi\n    on_pass: review\n  \
             - id: review\n    agent: pi\n    loop:\n      fix: 2\n    on_pass: done\n    \
             on_fail: fix\n",
        )
        .unwrap();

        let problems = Pipelines::refusals(&root);
        // default.yml: `checks` self-routes and its own `loop:` is a map —
        // two. bugfix.yml: `review`'s `loop:` is a map, naming `fix` — one.
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("default.yml:") && p.contains("may not route back")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("default.yml:") && p.contains("declares `loop:` as a map")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("bugfix.yml:") && p.contains("step `review`")),
            "{problems:?}"
        );
    }

    /// A file that will not deserialise at all is named by its own parse
    /// error rather than skipped — otherwise a project with one broken file
    /// and one carrying a retired shape would hear about only the shape.
    #[test]
    fn refusals_names_a_file_that_will_not_parse_instead_of_skipping_it() {
        let root = crate::scratch::root("pipeline-refusals-unparseable");
        let _ = std::fs::remove_dir_all(&root);
        let dir = Pipelines::dir_in(&root);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("broken.yml"),
            "steps:\n  - id: a\n    unknown_key: yes\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("default.yml"),
            "steps:\n  \
             - id: implement\n    agent: pi\n    on_pass: implement\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();

        let problems = Pipelines::refusals(&root);
        assert!(
            problems.iter().any(|p| p.starts_with("broken.yml:")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("default.yml:") && p.contains("may not route back")),
            "{problems:?}"
        );
    }

    /// And refused on a step an installation's own override also patches.
    /// The refusal cannot wait for `validate()`: `apply_pipeline_patch` merges
    /// a patch by round-tripping the step through serde, and `on_loop_max` is
    /// `skip_serializing`, so by then the key has been dropped — silently, on
    /// exactly the steps somebody cared enough about to override. Refused at
    /// parse instead, before any patch runs.
    #[test]
    fn the_retired_on_loop_max_key_is_refused_under_an_override_too() {
        with_override_fixture("retired-on-loop-max", |root| {
            std::fs::write(
                Pipelines::file_in(root, "impl"),
                "steps:\n  \
                 - id: implement\n    agent: pi\n    model: base-model\n    loop: 1\n    \
                 on_pass: review\n  \
                 - id: review\n    agent: pi\n    model: base-model\n    \
                 on_loop_max: blocked\n    on_pass: done\n    \
                 on_fail: implement\n",
            )
            .unwrap();
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            std::fs::write(
                overrides.join("pipelines").join("impl.yml"),
                "steps:\n  review:\n    model: claude-opus-5\n",
            )
            .unwrap();

            // `{:#}` rather than `to_string()`: the refusal is raised inside
            // the directory loop and wears two layers of `with_context`, so
            // the bare display is only the file it came from.
            let err = format!(
                "{:#}",
                Pipelines::load(root, &crate::config::Config::default()).unwrap_err()
            );
            assert!(err.contains("step `review`"), "{err}");
            assert!(err.contains("`on_loop_max:`"), "{err}");
        });
    }

    /// And skipped, not refused, when the key is in the override file itself
    /// rather than the tracked one — the half `apply_step_patch` owns, since
    /// serde would drop it on the way through and the layer would sit there
    /// doing nothing. The step is left exactly as the tracked file wrote it.
    #[test]
    fn the_retired_on_loop_max_key_is_skipped_in_an_override_file() {
        with_override_fixture("retired-on-loop-max-patch", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            std::fs::write(
                overrides.join("pipelines").join("impl.yml"),
                "steps:\n  review:\n    on_loop_max: blocked\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default()).unwrap();
            let review = pipelines.get("impl").unwrap().step("review").unwrap();
            assert!(
                review.on_loop_max.is_none(),
                "the retired key must never have been applied"
            );
            let ignored = &pipelines.ignored_overrides;
            assert_eq!(ignored.len(), 1, "{ignored:?}");
            assert!(ignored[0].target.contains("step `review`"), "{ignored:?}");
            assert!(ignored[0].reason.contains("`on_loop_max:`"), "{ignored:?}");
        });
    }

    /// A spent loop budget parks on `blocked` from every step, whatever the
    /// step routes a pass or a failure to — and `blocked` is not an edge the
    /// graph walks need to see, since an agent or command step already routes
    /// there for an unrouted failure.
    // covers: Step::loop_exit — where a spent loop budget lands
    #[test]
    fn a_spent_loop_budget_parks_on_blocked_from_every_step() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    loop: 2\n    on_pass: b\n    on_fail: b\n  \
             - id: b\n    agent: pi\n    \
             on_pass: a\n    on_fail: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        for id in ["a", "b", "z"] {
            assert_eq!(pipeline.step(id).unwrap().loop_exit(), BLOCKED);
        }
        // `b` routes a pass to `a` and a failure to `z`, and those are the
        // whole of its edges: the spent budget adds none.
        assert_eq!(
            pipeline.destinations(pipeline.step("b").unwrap()),
            vec!["a", "z"]
        );
    }

    /// The step a review sends work back to is where the budget now lives,
    /// not the step that sends it — read straight off `implement`'s own
    /// `loop:` in the shipped `default` pipeline.
    #[test]
    fn the_shipped_implement_step_bounds_its_own_arrivals() {
        let pipeline = Pipelines::builtin().get("default").unwrap().clone();
        assert_eq!(
            pipeline.step("implement").unwrap().arrival_limit(),
            Some(2),
            "`implement` is what a failed review sends work back to, so that is where the \
             budget lives now"
        );
        assert_eq!(
            pipeline.step("review").unwrap().arrival_limit(),
            None,
            "and `review` is bounded by nothing of its own: a passing `implement` always \
             reaches it"
        );
    }

    /// A two-step tracked pipeline on disk, named `impl` — the fixture every
    /// `overrides/pipelines/` test below patches.
    fn write_tracked_impl(root: &Path) {
        std::fs::create_dir_all(Pipelines::dir_in(root)).unwrap();
        std::fs::write(
            Pipelines::file_in(root, "impl"),
            "steps:\n  \
             - id: implement\n    agent: pi\n    model: base-model\n    effort: low\n    on_pass: review\n  \
             - id: review\n    agent: pi\n    model: base-model\n    on_pass: done\n",
        )
        .unwrap();
    }

    /// A scratch root and a scratch `$HOME` swapped in for `f`, so
    /// `crate::overrides::dir_for` — which every test below reaches for
    /// directly, to know where to write its own fixture rather than
    /// hard-coding a second copy of that path — resolves under a directory
    /// this test owns rather than the real one.
    fn with_override_fixture<T>(name: &str, f: impl FnOnce(&Path) -> T) -> T {
        let root = crate::scratch::root(&format!("pipeline-override-{name}"));
        let home = crate::scratch::root(&format!("pipeline-override-{name}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
        write_tracked_impl(&root);

        let result = crate::platform::test_home::with_home(&home, || f(&root));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
        result
    }

    /// `overrides/pipelines/impl.yml` setting one key on one step is read on
    /// top of the tracked file: the key it names changes, every other key on
    /// that step and every key on every other step still comes from disk.
    #[test]
    fn an_override_sets_one_key_and_leaves_the_rest_tracked() {
        with_override_fixture("set-one-key", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            std::fs::write(
                overrides.join("pipelines").join("impl.yml"),
                "steps:\n  implement:\n    model: claude-opus-5\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default()).unwrap();
            let pipeline = pipelines.get("impl").unwrap();
            let implement = pipeline.step("implement").unwrap();
            assert_eq!(implement.model.as_deref(), Some("claude-opus-5"));
            assert_eq!(
                implement.effort.as_deref(),
                Some("low"),
                "a key the patch never named still comes from the tracked file"
            );
            let review = pipeline.step("review").unwrap();
            assert_eq!(
                review.model.as_deref(),
                Some("base-model"),
                "a step the patch never named is untouched"
            );
        });
    }

    /// An override naming a step id the tracked pipeline does not have is
    /// stale: it is left out of the merge, load still succeeds, and the
    /// reason names both the id and the pipeline — list order decides slot
    /// priority, so a patch may set a value on a step that already exists
    /// and nothing more.
    #[test]
    fn an_override_naming_an_unknown_step_is_skipped_with_a_notice() {
        with_override_fixture("unknown-step", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            std::fs::write(
                overrides.join("pipelines").join("impl.yml"),
                "steps:\n  nonesuch:\n    model: claude-opus-5\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default()).unwrap();
            assert!(
                pipelines.get("impl").unwrap().step("nonesuch").is_none(),
                "no such step exists to have received the value"
            );
            let ignored = &pipelines.ignored_overrides;
            assert_eq!(ignored.len(), 1, "{ignored:?}");
            assert!(ignored[0].target.contains("nonesuch"), "{ignored:?}");
            assert!(ignored[0].target.contains("impl"), "{ignored:?}");
            assert!(ignored[0].reason.contains("nonesuch"), "{ignored:?}");
        });
    }

    /// A patch setting `id:` on a step it names is stale: renaming a step in
    /// place is the one way a patch could otherwise add or drop one from the
    /// graph, so it is skipped rather than applied, and the step is left
    /// exactly as the tracked file wrote it.
    #[test]
    fn an_override_setting_a_step_id_is_skipped() {
        with_override_fixture("sets-id", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            std::fs::write(
                overrides.join("pipelines").join("impl.yml"),
                "steps:\n  implement:\n    id: other\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default()).unwrap();
            assert_eq!(
                pipelines.get("impl").unwrap().step("implement").unwrap().id,
                "implement",
                "the id must never have been renamed"
            );
            let ignored = &pipelines.ignored_overrides;
            assert_eq!(ignored.len(), 1, "{ignored:?}");
            assert!(ignored[0].target.contains("implement"), "{ignored:?}");
            assert!(ignored[0].reason.contains("`id:`"), "{ignored:?}");
        });
    }

    /// A patch that would leave the graph unreachable is stale: applying it
    /// leaves the pipeline invalid by `Pipelines::validate()`'s own rules, so
    /// it is skipped rather than applied, and every other pipeline still
    /// loads.
    #[test]
    fn a_patch_that_breaks_the_graph_is_skipped() {
        with_override_fixture("breaks-graph", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            std::fs::write(
                overrides.join("pipelines").join("impl.yml"),
                "steps:\n  implement:\n    on_pass: nowhere\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default()).unwrap();
            assert_eq!(
                pipelines
                    .get("impl")
                    .unwrap()
                    .step("implement")
                    .unwrap()
                    .on_pass
                    .as_deref(),
                Some("review"),
                "the tracked transition must still be in place"
            );
            let ignored = &pipelines.ignored_overrides;
            assert_eq!(ignored.len(), 1, "{ignored:?}");
            assert!(ignored[0].reason.contains("unknown step"), "{ignored:?}");
        });
    }

    /// An override for a pipeline the checkout no longer has at all — the
    /// loop over tracked files never even reaches it — is caught and
    /// reported the same way a stale step is, and every pipeline that does
    /// still exist loads normally.
    #[test]
    fn an_override_naming_a_pipeline_the_checkout_lacks_is_skipped_with_a_notice() {
        with_override_fixture("unknown-pipeline", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            std::fs::write(
                overrides.join("pipelines").join("nosuchpipeline.yml"),
                "steps:\n  implement:\n    model: claude-opus-5\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default()).unwrap();
            assert!(
                pipelines.get("impl").is_ok(),
                "the real pipeline still loads"
            );
            let ignored = &pipelines.ignored_overrides;
            assert_eq!(ignored.len(), 1, "{ignored:?}");
            assert!(ignored[0].reason.contains("nosuchpipeline"), "{ignored:?}");
        });
    }

    /// A stale override is never edited or deleted — the file's bytes after
    /// a load that skips it are exactly what they were before.
    #[test]
    fn a_stale_override_file_is_untouched_by_the_load_that_skips_it() {
        with_override_fixture("untouched-file", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            let patch_path = overrides.join("pipelines").join("impl.yml");
            let raw = "steps:\n  review:\n    run: echo hi\n";
            std::fs::write(&patch_path, raw).unwrap();

            Pipelines::load(root, &crate::config::Config::default()).unwrap();

            assert_eq!(std::fs::read_to_string(&patch_path).unwrap(), raw);
        });
    }

    /// The Mockup's own scenario: a step patched to add `run:` beside an
    /// existing `agent:` is stale — a step runs a process or a model, not
    /// both — and the stderr notice reads exactly as the Mockup shows it.
    #[test]
    fn a_step_patch_naming_both_run_and_agent_is_skipped_with_the_mockups_own_notice() {
        with_override_fixture("run-and-agent", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            std::fs::write(
                overrides.join("pipelines").join("impl.yml"),
                "steps:\n  review:\n    run: echo hi\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default()).unwrap();
            let review = pipelines.get("impl").unwrap().step("review").unwrap();
            assert!(review.run.is_none(), "the patch must never have applied");
            assert!(review.agent.is_some(), "the tracked agent is untouched");

            let ignored = &pipelines.ignored_overrides;
            assert_eq!(ignored.len(), 1, "{ignored:?}");
            assert_eq!(
                ignored[0].notice(),
                "spoolway: override ignored — pipelines/impl.yml step `review`: names both \
                 `run:` and `agent:` — a step runs a process or a model, not both"
            );
        });
    }

    /// With no `overrides/` directory on disk at all, `Pipelines::load`
    /// answers exactly what it did before this layer existed.
    #[test]
    fn with_no_overrides_directory_load_is_unchanged() {
        with_override_fixture("absent", |root| {
            let config = crate::config::Config::default();
            let loaded = Pipelines::load(root, &config).unwrap();
            let tracked = Pipelines::load_tracked(root, &config).unwrap();
            assert_eq!(
                loaded.get("impl").unwrap().step("implement").unwrap().model,
                tracked
                    .get("impl")
                    .unwrap()
                    .step("implement")
                    .unwrap()
                    .model,
            );
        });
    }

    /// `Pipelines::load_tracked` is the second entry point the plan calls
    /// for: it answers the tracked set even where a patch is sitting right
    /// there on disk, for a caller that must not see the merge.
    #[test]
    fn load_tracked_ignores_a_patch_on_disk() {
        with_override_fixture("load-tracked", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(overrides.join("pipelines")).unwrap();
            std::fs::write(
                overrides.join("pipelines").join("impl.yml"),
                "steps:\n  implement:\n    model: claude-opus-5\n",
            )
            .unwrap();

            let config = crate::config::Config::default();
            let tracked = Pipelines::load_tracked(root, &config).unwrap();
            assert_eq!(
                tracked
                    .get("impl")
                    .unwrap()
                    .step("implement")
                    .unwrap()
                    .model
                    .as_deref(),
                Some("base-model"),
                "load_tracked must not see the patch"
            );
            let merged = Pipelines::load(root, &config).unwrap();
            assert_eq!(
                merged
                    .get("impl")
                    .unwrap()
                    .step("implement")
                    .unwrap()
                    .model
                    .as_deref(),
                Some("claude-opus-5"),
                "load, the ordinary entry point, still sees it"
            );
        });
    }

    // -----------------------------------------------------------------
    // The private layer — `local/pipelines/` and `local/prompts/`, read
    // beside the tracked files in repo mode. See `crate::local` and
    // `merge_private`.
    // -----------------------------------------------------------------

    /// A private pipeline joins the tracked set, marked with the file it
    /// came from, and the tracked pipeline beside it is untouched.
    #[test]
    fn a_private_pipeline_loads_beside_the_tracked_ones() {
        with_override_fixture("private-loads", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("impl-strict.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default()).unwrap();

            let private = pipelines.get("impl-strict").unwrap();
            assert_eq!(
                private.private_file.as_deref(),
                Some(dir.join("impl-strict.yml").as_path())
            );

            let tracked = pipelines.get("impl").unwrap();
            assert!(
                tracked.private_file.is_none(),
                "a tracked pipeline must never be marked private"
            );
        });
    }

    /// `Pipelines::load_tracked` — the probe `pipeline_override` and
    /// `override_promote` use — must see only the tracked files, the same
    /// way it already ignores the override layer.
    #[test]
    fn load_tracked_never_sees_a_private_pipeline() {
        with_override_fixture("private-load-tracked", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("impl-strict.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();

            let tracked = Pipelines::load_tracked(root, &crate::config::Config::default()).unwrap();
            assert!(
                tracked.get("impl-strict").is_err(),
                "load_tracked must not see it"
            );
        });
    }

    /// A private pipeline whose name already belongs to a tracked one is
    /// refused outright, naming both files — never silently shadowed.
    #[test]
    fn a_private_pipeline_naming_a_tracked_pipeline_is_refused() {
        with_override_fixture("private-pipeline-clash", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("impl.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();

            let err = Pipelines::load(root, &crate::config::Config::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(&Pipelines::file_in(root, "impl").display().to_string()),
                "{message}"
            );
            assert!(
                message.contains(&dir.join("impl.yml").display().to_string()),
                "{message}"
            );
        });
    }

    /// A private prompt whose name already belongs to a tracked one is
    /// refused, naming both files, the same as a private pipeline's clash.
    #[test]
    fn a_private_prompt_naming_a_tracked_prompt_is_refused() {
        with_override_fixture("private-prompt-clash", |root| {
            let tracked = crate::prompt::directory_form_in(root, "shared");
            std::fs::create_dir_all(tracked.parent().unwrap()).unwrap();
            std::fs::write(&tracked, "# tracked\n").unwrap();

            let local = crate::local::dir_for(root).unwrap();
            let private_dir = crate::local::prompts_dir(&local).join("shared");
            std::fs::create_dir_all(&private_dir).unwrap();
            std::fs::write(private_dir.join(crate::assets::PROMPT_FILE), "# private\n").unwrap();

            let err = Pipelines::load(root, &crate::config::Config::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(&tracked.display().to_string()),
                "{message}"
            );
            assert!(
                message.contains(
                    &private_dir
                        .join(crate::assets::PROMPT_FILE)
                        .display()
                        .to_string()
                ),
                "{message}"
            );
        });
    }

    /// A tracked pipeline's step naming a prompt that exists only privately
    /// is refused — that pipeline would break the moment it ran on a
    /// machine with no copy of the private prompt. `write_tracked_impl`'s
    /// `review` step names no `prompt:`, so its prompt name is its own id.
    #[test]
    fn a_tracked_pipeline_naming_a_private_only_prompt_is_refused() {
        with_override_fixture("tracked-wants-private-prompt", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::prompts_dir(&local).join("review");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(crate::assets::PROMPT_FILE), "# private\n").unwrap();

            let err = Pipelines::load(root, &crate::config::Config::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("impl"), "{message}");
            assert!(message.contains("review"), "{message}");
        });
    }

    /// `merge_private`'s private-only-prompt check runs before
    /// `Pipelines::assemble` materialises a pipeline's implicit `blocked`
    /// step from `[unattended]`, so a tracked pipeline that declares no
    /// `blocked` step of its own is never checked against the prompt that
    /// step will actually run — `config.unattended.blocked_prompt` naming a
    /// private-only prompt must be refused the same as any other tracked
    /// step naming one, but today it loads clean.
    #[test]
    fn a_blocked_prompt_from_config_naming_a_private_only_prompt_is_refused() {
        with_override_fixture("blocked-prompt-wants-private", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::prompts_dir(&local).join("impl2");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(crate::assets::PROMPT_FILE), "# private\n").unwrap();

            let mut config = crate::config::Config::default();
            config.unattended.blocked_prompt = "impl2".into();

            let err = Pipelines::load(root, &config).expect_err(
                "a tracked pipeline's blocked step naming a private-only prompt must be refused",
            );
            let message = format!("{err:#}");
            assert!(message.contains("impl2"), "{message}");
        });
    }

    /// The reverse of the ordering bug above: a tracked pipeline that
    /// declares its own bare `blocked` step (naming no `prompt:`) is read by
    /// `merge_private` before `apply_blocked_config_fallback` ever runs, so
    /// the check sees the step's id, `blocked`, as the prompt name it will
    /// run — not the prompt the fallback will actually fill in. A private
    /// prompt that merely happens to be named `blocked` then fails every
    /// load of this pipeline, even though the step never runs that prompt.
    #[test]
    fn a_bare_blocked_step_is_checked_against_the_prompt_it_actually_runs() {
        with_override_fixture("bare-blocked-step", |root| {
            std::fs::write(
                Pipelines::file_in(root, "impl"),
                "steps:\n  \
                 - id: implement\n    agent: pi\n    model: base-model\n    effort: low\n    \
                 on_pass: review\n  \
                 - id: review\n    agent: pi\n    model: base-model\n    on_pass: blocked\n  \
                 - id: blocked\n",
            )
            .unwrap();

            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::prompts_dir(&local).join("blocked");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(crate::assets::PROMPT_FILE), "# private\n").unwrap();

            Pipelines::load(root, &crate::config::Config::default()).expect(
                "a bare `blocked` step must be checked against the prompt the config fallback \
                 gives it, not its own id",
            );
        });
    }

    /// A private pipeline naming a private prompt is fine — nothing about
    /// the refusal above applies to a pipeline that is itself private.
    #[test]
    fn a_private_pipeline_may_name_a_private_prompt() {
        with_override_fixture("private-names-private-prompt", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let pipelines_dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&pipelines_dir).unwrap();
            std::fs::write(
                pipelines_dir.join("impl-strict.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    prompt: helper\n    \
                 model: base-model\n    on_pass: done\n",
            )
            .unwrap();
            let prompt_dir = crate::local::prompts_dir(&local).join("helper");
            std::fs::create_dir_all(&prompt_dir).unwrap();
            std::fs::write(prompt_dir.join(crate::assets::PROMPT_FILE), "# helper\n").unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default())
                .expect("a private pipeline naming a private prompt must load");
            assert!(pipelines.get("impl-strict").is_ok());
        });
    }

    /// `local/prompts/` has one shape only, the directory one: a flat
    /// `local/prompts/review.md` is not a private prompt at all. Before the
    /// review round of 2026-09-30 it counted as one here while
    /// `crate::prompt::path_for` never resolved it, so the tracked `impl`
    /// — whose `review` step names prompt `review` — was refused as naming
    /// a private-only prompt over a file nothing would ever read.
    #[test]
    fn a_flat_private_prompt_file_is_not_a_private_prompt() {
        with_override_fixture("private-flat-prompt", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::prompts_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("review.md"), "# flat\n").unwrap();

            Pipelines::load(root, &crate::config::Config::default())
                .expect("a flat local/prompts/review.md must not make impl name a private prompt");
        });
    }

    /// A private pipeline written as `.yaml` is marked with that file, not a
    /// `.yml` sibling that does not exist — the path was once rebuilt as
    /// `<name>.yml` rather than read from disk.
    #[test]
    fn a_private_yaml_pipeline_is_marked_with_its_own_file() {
        with_override_fixture("private-yaml", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("impl-strict.yaml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(root, &crate::config::Config::default()).unwrap();
            assert_eq!(
                pipelines
                    .get("impl-strict")
                    .unwrap()
                    .private_file
                    .as_deref(),
                Some(dir.join("impl-strict.yaml").as_path())
            );
        });
    }

    /// The clash refusal names the private file as it is on disk, `.yaml`
    /// included, so the person is sent to a file that exists.
    #[test]
    fn a_private_yaml_pipeline_clashing_with_a_tracked_one_names_the_yaml_file() {
        with_override_fixture("private-yaml-clash", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("impl.yaml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();

            let err = Pipelines::load(root, &crate::config::Config::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(&dir.join("impl.yaml").display().to_string()),
                "{message}"
            );
        });
    }

    /// The clash refusal names a tracked `.yaml` file as it is on disk too,
    /// not the `.yml` spelling `Pipelines::file_in` would give it.
    #[test]
    fn a_private_pipeline_clashing_with_a_tracked_yaml_file_names_the_tracked_yaml() {
        with_override_fixture("private-clash-tracked-yaml", |root| {
            let tracked_yml = Pipelines::file_in(root, "impl");
            let tracked_yaml = Pipelines::dir_in(root).join("impl.yaml");
            std::fs::rename(&tracked_yml, &tracked_yaml).unwrap();

            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("impl.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();

            let err = Pipelines::load(root, &crate::config::Config::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(&tracked_yaml.display().to_string()),
                "{message}"
            );
        });
    }

    /// A private pipeline that fails validation points the person at
    /// `local/pipelines/` too, not only at the tracked directory it never
    /// came from — and names its own exact file, not just the directory.
    #[test]
    fn a_private_pipeline_failing_validation_names_the_private_directory() {
        with_override_fixture("private-invalid", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("impl-strict.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: base-model\n    \
                 on_pass: nowhere\n",
            )
            .unwrap();

            let err = Pipelines::load(root, &crate::config::Config::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains(&dir.display().to_string()), "{message}");
            assert!(message.contains("impl-strict"), "{message}");
            assert!(
                message.contains(&dir.join("impl-strict.yml").display().to_string()),
                "a private pipeline's validation error must name its own file: {message}"
            );
        });
    }

    /// Two private files of the same name but different extensions —
    /// `foo.yml` beside `foo.yaml` — are two private files for one name,
    /// never a clash with a tracked file that was never touched: before this
    /// fix, the second one to load found the name already taken by the
    /// first's own insert and blamed it on a tracked path that does not
    /// exist.
    #[test]
    fn two_private_files_for_one_name_are_reported_as_such() {
        with_override_fixture("private-two-private-files", |root| {
            let local = crate::local::dir_for(root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("extra.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();
            std::fs::write(
                dir.join("extra.yaml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();

            let err = Pipelines::load(root, &crate::config::Config::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(&dir.join("extra.yml").display().to_string()),
                "{message}"
            );
            assert!(
                message.contains(&dir.join("extra.yaml").display().to_string()),
                "{message}"
            );
            assert!(
                !message.contains("clashes with the tracked"),
                "two private files must never be reported as a tracked clash: {message}"
            );
        });
    }

    /// A private prompt clashing with a tracked one names the tracked file
    /// that actually exists — the legacy flat `<name>.md`, when that is the
    /// shape on disk, rather than always the directory form nothing there
    /// uses.
    #[test]
    fn a_private_prompt_clashing_with_a_flat_tracked_one_names_the_flat_file() {
        with_override_fixture("private-prompt-clash-flat", |root| {
            let flat = crate::config::under_setup(
                &crate::config::setup_dir_in(root),
                crate::config::PROMPTS_DIR,
            )
            .join("helper.md");
            std::fs::create_dir_all(flat.parent().unwrap()).unwrap();
            std::fs::write(&flat, "# flat tracked\n").unwrap();

            std::fs::write(
                Pipelines::file_in(root, "impl"),
                "steps:\n  \
                 - id: implement\n    agent: pi\n    prompt: helper\n    model: base-model\n    \
                 on_pass: done\n",
            )
            .unwrap();

            let local = crate::local::dir_for(root).unwrap();
            let prompt_dir = crate::local::prompts_dir(&local).join("helper");
            std::fs::create_dir_all(&prompt_dir).unwrap();
            std::fs::write(prompt_dir.join(crate::assets::PROMPT_FILE), "# private\n").unwrap();

            let err = Pipelines::load(root, &crate::config::Config::default()).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(&flat.display().to_string()),
                "a flat tracked prompt must be named by its own flat path: {message}"
            );
        });
    }

    /// Home mode has no private layer of its own — the whole setup is
    /// already private — so a `local/pipelines/` sitting under the
    /// workspace's own dispatcher home is never read. Written by hand, the
    /// same way `crate::repo`'s own `home-mode-discovery` tests build a
    /// workspace fixture, since this only needs `workspace_clone` to answer
    /// for `root`.
    #[test]
    fn in_home_mode_the_private_layer_is_never_read() {
        let root = crate::scratch::root("pipeline-private-home-mode");
        let home = crate::scratch::root("pipeline-private-home-mode-home");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&root).unwrap();

        crate::platform::test_home::with_home(&home, || {
            let workspace = crate::mux::state_root().join("pipeline-private-home-mode-ws");
            std::fs::create_dir_all(workspace.join("config").join("pipelines")).unwrap();
            std::fs::write(
                workspace.join("config").join("pipelines").join("solo.yml"),
                "steps:\n  - id: a\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();
            std::fs::write(
                workspace.join("project.toml"),
                format!(
                    "id = \"pipeline-private-home-mode-ws\"\nclones = [{{ root = {:?}, \
                     dispatcher = \"d\" }}]\n",
                    root.display(),
                ),
            )
            .unwrap();

            // A private pipeline that, if home mode read `local/` at all,
            // would load beside the workspace's own tracked `solo`.
            let local_pipelines = workspace
                .join("dispatchers")
                .join("d")
                .join(crate::local::LOCAL_DIR)
                .join("pipelines");
            std::fs::create_dir_all(&local_pipelines).unwrap();
            std::fs::write(
                local_pipelines.join("private-only.yml"),
                "steps:\n  - id: a\n    agent: pi\n    model: base-model\n    on_pass: done\n",
            )
            .unwrap();

            let pipelines = Pipelines::load(&root, &crate::config::Config::default()).unwrap();
            assert!(
                pipelines.get("solo").is_ok(),
                "the workspace's own pipeline loads"
            );
            assert!(
                pipelines.get("private-only").is_err(),
                "home mode must never read local/"
            );
        });

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// End-to-end coverage the task calls for: a repo-mode fixture with one
    /// private pipeline naming one private prompt, checked by
    /// `pipeline check` and `pipeline list`.
    #[test]
    fn a_private_pipeline_naming_a_private_prompt_checks_and_lists() {
        let root = crate::scratch::root("pipeline-private-e2e");
        let home = crate::scratch::root("pipeline-private-e2e-home");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);

        // A tracked pipeline with its own prompt present, so `pipeline
        // check` has nothing tracked to complain about.
        std::fs::create_dir_all(Pipelines::dir_in(&root)).unwrap();
        std::fs::write(
            Pipelines::file_in(&root, "impl"),
            "steps:\n  - id: implement\n    agent: pi\n    model: base-model\n    on_pass: done\n",
        )
        .unwrap();
        let tracked_prompt = crate::prompt::directory_form_in(&root, "implement");
        std::fs::create_dir_all(tracked_prompt.parent().unwrap()).unwrap();
        std::fs::write(&tracked_prompt, "# implement\n").unwrap();
        // Every pipeline's `blocked` step is materialised from
        // `[unattended]` and needs a prompt of its own — see
        // `blocked_step_from_config` — so both pipelines below need it too.
        let unblocker_prompt = crate::prompt::directory_form_in(&root, "unblocker");
        std::fs::create_dir_all(unblocker_prompt.parent().unwrap()).unwrap();
        std::fs::write(&unblocker_prompt, "# unblocker\n").unwrap();

        crate::platform::test_home::with_home(&home, || {
            let local = crate::local::dir_for(&root).unwrap();
            let pipelines_dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&pipelines_dir).unwrap();
            std::fs::write(
                pipelines_dir.join("impl-strict.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    prompt: helper\n    \
                 model: base-model\n    on_pass: done\n",
            )
            .unwrap();
            let prompt_dir = crate::local::prompts_dir(&local).join("helper");
            std::fs::create_dir_all(&prompt_dir).unwrap();
            std::fs::write(prompt_dir.join(crate::assets::PROMPT_FILE), "# helper\n").unwrap();

            let config = crate::config::Config::default();
            let pipelines = Pipelines::load(&root, &config).unwrap();
            assert_eq!(
                pipelines
                    .get("impl-strict")
                    .unwrap()
                    .private_file
                    .as_deref(),
                Some(pipelines_dir.join("impl-strict.yml").as_path())
            );

            let repo = crate::repo::Repo {
                checkout: root.to_path_buf(),
                root: root.to_path_buf(),
                config,
                home: crate::mux::project_home(&root).unwrap(),
            };

            crate::commands::pipeline_check(&repo, Ok(pipelines.clone()), false)
                .expect("the private pipeline and its private prompt must both check out");
            crate::commands::pipeline_list(&repo, &pipelines, false)
                .expect("listing must not fail with a private pipeline present");
        });

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
    }
}
