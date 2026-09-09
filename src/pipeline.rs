//! The pipeline definition: a graph of named steps a task moves through.
//!
//! This is data, not code. The dispatcher is a generic interpreter over it, so
//! adding a step, reordering the flow, or swapping which agent handles a step
//! is a config edit rather than a code change.
//!
//! Loaded from `.spoolway/pipelines/`, one file per pipeline: `default` for a
//! unit of work, and `bugfix` for a reproduce-first fix. A task picks one with
//! its `pipeline:` field.

use std::collections::{BTreeMap, HashSet};
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
/// binary can keep true — so it is fenced, and `spoolway update` rewrites it.
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
/// `pub(crate)` rather than private: `pipeline_check` reads this directly, so
/// the two shipped pipelines are validated in full whether or not this
/// project's own `.spoolway/pipelines/` shadows them with an override — see
/// `commands::pipeline_check`.
pub(crate) fn builtin_pipelines() -> Result<BTreeMap<String, Pipeline>> {
    BUILTIN_PIPELINES
        .iter()
        .map(|(name, raw)| {
            let pipeline = Pipeline::parse(name, raw)
                .with_context(|| format!("built-in pipeline `{name}`"))?;
            Ok((name.to_string(), pipeline))
        })
        .collect()
}

/// Every `<name>.yml` in the pipeline directory, or `None` when the directory
/// is not there at all.
///
/// An empty directory is `Some(empty)` rather than `None`, so that deleting the
/// last pipeline is an error about having no pipelines instead of a silent
/// fallback to the built-in ones.
fn read_pipeline_dir(dir: &Path) -> Result<Option<Vec<(String, String)>>> {
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
        files.push((name.to_string(), raw));
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
/// it goes any further. `spoolway resume` sends it on; `spoolway resume
/// --reject` sends it back round.
///
/// Distinct from [`BLOCKED`] because the two ask a person for opposite things.
/// A block is "something is in the way, and I could not finish"; the answer is
/// to clear it and resume *at the same step*. A pause is "I finished, and you
/// said you wanted to see this before it goes on"; the answer is to let it
/// *past* the step. Parking both on `blocked` would put a successful step in
/// the column a person scans for failures, and resume it into a lane that has
/// nothing left to do.
///
/// Not a terminal either, and this is what keeps `graph.rs` honest about it: a
/// paused task is `DepState::Pending`, still moving, so its dependents wait
/// quietly rather than being reported as stranded behind a block.
pub const PAUSED: &str = "paused";

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
    /// leaves on the same pass it started, and the process runs on unwatched.
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
    ///
    /// A free string, not a closed set: which levels a model accepts is the
    /// model's own fact, changes when the model does, and a copy of that list
    /// in this binary would only go stale silently. `pipeline check` refuses
    /// this on a kind with no effort flag, and refuses the literal `auto` —
    /// spoolway resolved that itself once, against sensitive paths nothing
    /// computes any more, and nothing resolves it now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,

    /// Skills invoked at the top of this step's opening prompt, one `/name`
    /// line per skill, in declaration order — see [`crate::compose::opening_prompt`].
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
    /// Unattended is the exception the mode already defines: there is no person
    /// to approve, so `gate:` does not apply and the pass routes as written. See
    /// `unattended.enabled`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate: bool,

    /// Step to move to on a `pass`. Absent means the task stays put.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_pass: Option<String>,

    /// Step to move to on a `fail`. Absent falls back to the pipeline's
    /// `blocked` step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fail: Option<String>,

    /// Most times a task may arrive at this step *from a given step* — one lap
    /// of the loop — before it is escalated instead of going round again.
    ///
    /// A round is a lap, not a conversation. Counting conversations charged for
    /// the expensive event, a cold start, but a loop that keeps its session
    /// (`session: true`) went round free, and the bound stopped describing
    /// anything a person could reason about: the same three visits cost the
    /// same tokens whether one lap opened a conversation or three did, and only
    /// the lap count can be written down in advance and mean the same thing
    /// every run. What still bounds a conversation's own cost is
    /// an enabled `agents.<profile>.session_reuse_ctx`, on the agent profile,
    /// entirely separate from this.
    ///
    /// Counted per route in, not per step, because a step several loops come
    /// back to is several loops: `review` and `e2e` both send failures to
    /// `fix`, and one shared budget means whichever loop ran first spends the
    /// other's. A bare number is that limit on every route in; a map sets one
    /// route at a time.
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

    /// Where a task goes when this step's `loop:` is spent. Absent carries the
    /// task on to `on_pass`, exactly as an ordinary pass would — a review that
    /// has argued four times has said what it has to say, and running it a
    /// fifth time buys nothing, so the default is to let the change through
    /// with its findings attached rather than park it for a person.
    ///
    /// On the step rather than in config.toml because a review loop and a
    /// rebase loop want different endings in the same installation: findings a
    /// reviewer could not get fixed are read at the pull request anyway, so
    /// carrying on is right there, while a rebase that will not converge, or a
    /// tree that will not build, has nowhere useful to go but a person —
    /// `on_loop_max: blocked` says so explicitly. It sits beside the budget it
    /// answers for, and [`Pipeline::destinations`] returns it when it differs
    /// from `on_pass`, so the loop checker and the terminal-reachability walk
    /// both see the edge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_loop_max: Option<String>,

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
    /// It bounds a background run too, which nothing routes on — there the
    /// alternative is a process outliving the task that started it, on a task
    /// that never reaches cleanup because it blocked on the way.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "optional_duration"
    )]
    pub timeout: Option<Duration>,

    /// Let the task move on while the command keeps running.
    ///
    /// The exit code is nobody's to route on afterwards — the task is already
    /// somewhere else — so [`Pipeline::validate`] refuses `on_fail` here rather
    /// than let a file read as though a late failure were handled. What the
    /// command wrote is in its log, and a run still going at cleanup is stopped
    /// with the task.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,

    /// Run this step's command detached, with no pane, exactly the way every
    /// command step used to run.
    ///
    /// `false`, the default, gives the command a pane of its own — split off
    /// the task's own tab, under the herdr and tmux backends — so a suite or
    /// a `gh pr checks --watch` is something a person can look at while it
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

    /// Tear down the task's worktree and branch, and archive its file, on
    /// arrival. Only meaningful on a terminal step.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cleanup: bool,

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

/// How many laps of a loop a step allows, by the route the task arrives on.
///
/// The two forms say the same kind of thing at different resolutions, so a
/// pipeline that wants one number writes one number:
///
/// ```yaml
/// loop: 3      # three laps on every route in
/// loop:        # or one route at a time
///   review: 3
///   e2e: 5
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Loop {
    /// One limit, applied to each route in separately. Zero means no limit,
    /// which is what a step that says nothing gets.
    Every(u32),
    /// A limit per step the task arrives from. A route not named here is
    /// unbounded — and a name that routes nowhere near this step is refused by
    /// [`Pipeline::validate`], so a typo cannot quietly unbound a loop.
    PerRoute(BTreeMap<String, u32>),
}

impl Default for Loop {
    fn default() -> Self {
        Loop::Every(0)
    }
}

impl Loop {
    /// The limit on arriving from `from`, or `None` for no limit.
    pub fn limit(&self, from: &str) -> Option<u32> {
        match self {
            Loop::Every(0) => None,
            Loop::Every(n) => Some(*n),
            Loop::PerRoute(by_route) => by_route.get(from).copied().filter(|n| *n > 0),
        }
    }

    /// Whether this bounds nothing at all, which is what is left out of a
    /// rendered pipeline file.
    pub fn is_unbounded(&self) -> bool {
        match self {
            Loop::Every(n) => *n == 0,
            Loop::PerRoute(by_route) => by_route.values().all(|n| *n == 0),
        }
    }

    /// How this reads in `spoolway pipeline show`.
    pub fn describe(&self) -> String {
        match self {
            Loop::Every(n) => n.to_string(),
            Loop::PerRoute(by_route) => by_route
                .iter()
                .filter(|(_, n)| **n > 0)
                .map(|(from, n)| format!("{from}:{n}"))
                .collect::<Vec<_>>()
                .join(","),
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
        "`max_new_sessions:` is now `loop:` — it bounds how many times a task may arrive at \
         this step from a given one, a lap of the loop, not how many fresh conversations that \
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
        "`max_rounds:` is now `loop:` — it bounds how many times a task may arrive at this \
         step from a given one. Rename the key, and consider raising the number: what it \
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

    /// How many times a task may arrive here from `from` before escalating —
    /// or `None` if that route is unbounded.
    pub fn round_limit(&self, from: &str) -> Option<u32> {
        self.r#loop.limit(from)
    }

    /// Where a spent loop sends a task from this step: `on_loop_max` if the
    /// step named one, otherwise `on_pass` — the same place an ordinary pass
    /// goes, since carrying on is the default. `blocked` only if neither is
    /// set, which is not a shape `Pipeline::validate` lets an agent or command
    /// step take.
    pub fn loop_exit(&self) -> &str {
        self.on_loop_max
            .as_deref()
            .or(self.on_pass.as_deref())
            .unwrap_or(BLOCKED)
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
    pub fn parse(name: &str, raw: &str) -> Result<Pipeline> {
        let mut pipeline: Pipeline = serde_norway::from_str(raw).context("parsing pipeline")?;
        pipeline.name = name.to_string();
        pipeline.validate()?;
        Ok(pipeline)
    }
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
            // the task stopped rather than from this step — one step past
            // there under `unattended.skip_blocked_lane`, back onto it when
            // that is off — and anything else (`--fail`, `--block`, or
            // `--pause`) parks the task on `paused` for a person, never back
            // onto `blocked` itself. Each of the keys that would otherwise say
            // one of those things is refused by name, pointing at what
            // actually decides it instead of leaving a reader to wonder why
            // the graph disagrees with the file.
            if step.id == BLOCKED {
                if step.on_pass.is_some() {
                    bail!(
                        "step `blocked` declares `on_pass:` — where its pass goes is read from \
                         the step the task blocked on, not from here: past that step, or back \
                         onto it under `unattended.skip_blocked_lane = false`; delete `on_pass:`"
                    );
                }
                if step.on_fail.is_some() {
                    bail!(
                        "step `blocked` declares `on_fail:` — a fail or a block from `blocked` \
                         parks the task on `paused` for a person, never back on `blocked` \
                         itself; delete `on_fail:`"
                    );
                }
                if step.on_loop_max.is_some() {
                    bail!(
                        "step `blocked` declares `on_loop_max:` — there is no per-task bound on \
                         how many times it may round-trip; delete `on_loop_max:` (and `loop:`, \
                         if that is what it was answering for)"
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
                    // A background command's exit code arrives after the task
                    // has gone, so there is nothing left to route. Refused
                    // rather than ignored: an `on_fail` here reads as a handled
                    // failure and handles nothing.
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
                    if step.background && step.on_fail.is_some() {
                        bail!(
                            "step `{}` is `background: true` and declares `on_fail` — the task \
                             has already moved on by the time it exits, so nothing could route \
                             there. Drop `background:` to wait for it, or drop `on_fail:`",
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
            }
            if step.cleanup && kind != StepKind::Terminal {
                bail!(
                    "step `{}` declares `cleanup:` but is a {} step — a worktree torn down \
                     under a task still running is a task with nowhere to work",
                    step.id,
                    kind.as_str()
                );
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

        // A `loop:` map names the steps a task can arrive from, so a name that
        // routes nowhere near this step is a typo — and a typo here reads as a
        // bounded loop while bounding nothing.
        for step in &self.steps {
            if let Loop::PerRoute(by_route) = &step.r#loop {
                for from in by_route.keys() {
                    if !self.routes_to(from, &step.id) {
                        bail!(
                            "step `{}` sets `loop` for arrivals from `{from}`, but `{from}` \
                             never routes to it",
                            step.id
                        );
                    }
                }
            }
        }

        // An `on_loop_max` naming nothing is a spent budget with nowhere to
        // land, and it would only be discovered by a task actually spending
        // one. Refused with the budget too: a destination for a limit no
        // route has reads as a handled case and handles nothing.
        for step in &self.steps {
            if let Some(target) = step.on_loop_max.as_deref() {
                if !known(target) {
                    bail!(
                        "step `{}`: on_loop_max points at unknown step `{target}`",
                        step.id
                    );
                }
                if step.r#loop.is_unbounded() {
                    bail!(
                        "step `{}` declares `on_loop_max:` but no `loop:` — there is no budget \
                         for it to answer for",
                        step.id
                    );
                }
            }
        }

        self.check_bounded_loops()?;

        Ok(())
    }

    /// Steps that gate but declare no `on_fail`, so a rejection at that gate
    /// has nowhere else to go.
    ///
    /// Not one of `validate`'s own refusals: a gated step with no `on_fail`
    /// is a legal shape — [`Step::destination`] already falls back to
    /// `blocked` for an outcome a step names no route for, and `blocked` is
    /// exactly where a person rejecting a piece of work belongs. What is
    /// worth naming is only that `--reject`'s help in `src/cli.rs` used to
    /// promise the step's own `on_fail` route, when a step with none of its
    /// own always parks on `blocked` instead — a diagnostic for whoever wrote
    /// the pipeline, not a shape this refuses to run.
    pub fn gate_warnings(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter(|step| step.gate && step.on_fail.is_none())
            .map(|step| {
                format!(
                    "{}: `{}` gates but declares no `on_fail`, so `spoolway resume --reject` \
                     parks the task on `{BLOCKED}` rather than sending it back round.",
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

    /// Whether `from` names `to` as either of its destinations.
    fn routes_to(&self, from: &str, to: &str) -> bool {
        match self.step(from) {
            Some(step) => {
                step.on_pass.as_deref() == Some(to)
                    || match step.on_fail.as_deref() {
                        Some(target) => target == to,
                        // An unrouted failure always goes to `blocked`.
                        None => step.kind() != StepKind::Terminal && to == BLOCKED,
                    }
            }
            None => false,
        }
    }

    /// Refuse a cycle that nothing bounds, or that a bounded route only ever
    /// leads back into.
    ///
    /// Reaching a terminal step is not enough on its own: `review → fix →
    /// review` reaches `done` on every pass, and still spins forever on a pair
    /// of agents that keep saying fail. A person only ever hears about it from
    /// the bill.
    ///
    /// A cycle terminates if any one of its routes is bounded *and its exit
    /// actually leaves* — that route escalates, carries on to `loop_exit()`,
    /// and the task is gone. So the check is not "enumerate the cycles", which
    /// is exponential, but the same statement inside out: redirect every
    /// bounded route to where its budget sends the task, and what is left must
    /// be acyclic. A bounded route whose exit re-enters the very cycle it was
    /// meant to break bounds nothing — the task just goes round again, however
    /// it got there.
    fn check_bounded_loops(&self) -> Result<()> {
        // Grey while on the current path, black once explored. A route back to
        // something grey closes a cycle, and the path holds its steps.
        let mut state: BTreeMap<&str, u8> = BTreeMap::new();
        let mut path: Vec<&str> = Vec::new();

        for step in &self.steps {
            if let Some(cycle) = self.find_unbounded_cycle(&step.id, &mut state, &mut path) {
                let suggestion = self
                    .suggest_loop_step(&cycle)
                    .map(|id| {
                        format!(
                            " Move `loop:` to `{id}` — what follows `{id}` is `{}`, which \
                             leaves the loop.",
                            self.step(id).map(Step::loop_exit).unwrap_or(BLOCKED)
                        )
                    })
                    .unwrap_or_default();
                bail!(
                    "steps {} form a loop that gives up into itself — a spent budget carries on \
                     to a step still inside the loop, so a task could go round it forever.{}",
                    cycle
                        .iter()
                        .map(|id| format!("`{id}`"))
                        .collect::<Vec<_>>()
                        .join(" → "),
                    suggestion
                );
            }
        }
        Ok(())
    }

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
            let mut redirects: Vec<&str> = Vec::new();
            if let Some(cycle) = self.walk_edge(&step.id, next, state, path, &mut redirects) {
                return Some(cycle);
            }
        }

        path.pop();
        state.insert(&step.id, BLACK);
        None
    }

    /// Walk one edge, `from` → `next`, in the bounded-loop check: if `next`
    /// escalates on arrivals from `from`, that edge cannot be what holds a
    /// loop open, so the walk is redirected to `next`'s exit instead of
    /// following the route as an ordinary one — and the exit is walked the
    /// same way in turn, since a chain of spent budgets ending back inside
    /// the loop is exactly as open as one link of it would be.
    ///
    /// `redirects` is this chain's own history, distinct from `path`: a
    /// redirect never visits `next` the way an ordinary route would, so it is
    /// never pushed onto `path` or coloured — and without some other memory,
    /// a ring of steps that only ever escalate into one another would recurse
    /// forever rather than being reported as the loop that it is.
    ///
    /// A separate function from [`Pipeline::find_unbounded_cycle`] because a
    /// redirected exit is evaluated as *its own* edge, out of `next` rather
    /// than out of `from` — `next`'s budget answers for `next`, not for
    /// whoever happened to route into it — and re-entering
    /// `find_unbounded_cycle` directly would check the wrong route, missing
    /// a bounded step chained right behind an unbounded-looking exit.
    fn walk_edge<'a>(
        &'a self,
        from: &'a str,
        next: &'a str,
        state: &mut BTreeMap<&'a str, u8>,
        path: &mut Vec<&'a str>,
        redirects: &mut Vec<&'a str>,
    ) -> Option<Vec<&'a str>> {
        if let Some(target) = self.step(next)
            && target.round_limit(from).is_some()
        {
            if let Some(start) = redirects.iter().position(|seen| *seen == next) {
                let mut cycle: Vec<&str> = redirects[start..].to_vec();
                cycle.push(next);
                return Some(cycle);
            }
            redirects.push(next);
            return self.walk_edge(next, target.loop_exit(), state, path, redirects);
        }
        self.find_unbounded_cycle(next, state, path)
    }

    /// A step in `cycle` to move `loop:` to instead: a member of the cycle
    /// itself whose own `on_pass` already leaves it, rather than re-entering
    /// it.
    ///
    /// The graph does not know which step the author meant to ask "has this
    /// gone round too many times" — only that wherever it is now, the answer
    /// carries the task straight back in. A step whose ordinary pass already
    /// exits the cycle is where a loop bound would carry the same task the
    /// same way on the round where it gives up, which is the property a
    /// bound placed anywhere else in the cycle cannot have.
    fn suggest_loop_step<'a>(&'a self, cycle: &[&'a str]) -> Option<&'a str> {
        for step in &self.steps {
            if !cycle.contains(&step.id.as_str()) {
                continue;
            }
            if let Some(target) = step.on_pass.as_deref()
                && !cycle.contains(&target)
            {
                return Some(step.id.as_str());
            }
        }
        None
    }

    /// Every step this one can move a task to: its two outcomes, the implicit
    /// route to `blocked`, and wherever a spent loop sends it if that differs
    /// from `on_pass`.
    ///
    /// `on_loop_max` is an edge like the other two when it names somewhere —
    /// a task really does arrive there, and a graph walk that cannot see it
    /// would judge reachability and cycles against a smaller pipeline than the
    /// one that runs. Left out when absent or when it agrees with `on_pass`:
    /// the default carries a spent loop to `on_pass`, which is already in this
    /// list, and repeating it would not add an edge the walks do not already
    /// see.
    ///
    /// `blocked` itself has none: `validate` refuses it `on_pass`, `on_fail`
    /// and `on_loop_max`, so there is nothing declared to report, and its real
    /// routing — read from the step the task blocked on for a pass, back to
    /// itself on a block — is decided at runtime from task state and
    /// `unattended.skip_blocked_lane`, not from the graph. Reporting a
    /// self-edge here would read as the very unbounded cycle `blocked` is
    /// deliberately exempt from.
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
        if let Some(target) = step.on_loop_max.as_deref()
            && !out.contains(&target)
        {
            out.push(target);
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
                for next in [
                    step.on_pass.as_deref(),
                    step.on_fail.as_deref(),
                    step.on_loop_max.as_deref(),
                ]
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
/// [`Pipeline::parse`] is what every other caller wants — a file that has to
/// stand on its own, with nothing still to arrive from outside it. This is
/// only for [`Pipelines::load`]'s own directory loop, where a file's
/// `blocked` step may declare an override missing some of its five keys on
/// purpose, and validating before [`Pipelines::assemble`] has filled the rest
/// in from config would refuse a step that is about to become a perfectly
/// good one.
fn parse_unchecked(name: &str, raw: &str) -> Result<Pipeline> {
    let mut pipeline: Pipeline = serde_norway::from_str(raw).context("parsing pipeline")?;
    pipeline.name = name.to_string();
    Ok(pipeline)
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
        cleanup: false,
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

/// Every pipeline defined for a project, and which one a task gets by default.
///
/// Assembled rather than parsed: each pipeline is its own file under
/// `.spoolway/pipelines/`, named by that file, and the two fields that are not
/// per-pipeline come from config.toml.
#[derive(Debug, Clone)]
pub struct Pipelines {
    /// Pipeline a task runs on when it names none. `dispatch.default_pipeline`.
    pub default: String,

    pub pipelines: BTreeMap<String, Pipeline>,
}

impl Pipelines {
    /// The directory holding one file per pipeline.
    pub fn dir_in(root: &Path) -> PathBuf {
        root.join(crate::config::STATE_DIR).join(PIPELINE_DIR)
    }

    /// Where a named pipeline's file lives.
    pub fn file_in(root: &Path, name: &str) -> PathBuf {
        Pipelines::dir_in(root).join(format!("{name}.yml"))
    }

    /// Load and validate from a repo root.
    ///
    /// Two sources, in order: the directory, and the built-in definitions. The
    /// old single `pipeline.yml` is no longer read — a project still carrying
    /// one is told what to do with it rather than silently served the
    /// built-ins beside it.
    pub fn load(root: &Path, config: &crate::config::Config) -> Result<Pipelines> {
        let dir = Pipelines::dir_in(root);
        if let Some(files) = read_pipeline_dir(&dir)? {
            let mut pipelines = BTreeMap::new();
            for (name, raw) in files {
                // Unvalidated: a file's own `blocked` step may declare only
                // some of its five keys on purpose, leaning on
                // `Pipelines::assemble` to fill the rest in from config
                // before anything checks that the step is a runnable one.
                let pipeline = parse_unchecked(&name, &raw)
                    .with_context(|| format!("in {}", Pipelines::file_in(root, &name).display()))?;
                pipelines.insert(name, pipeline);
            }
            return Pipelines::assemble(pipelines, config)
                .with_context(|| format!("in {}", dir.display()));
        }

        let old = root.join(crate::config::STATE_DIR).join(PIPELINE_FILE);
        if old.exists() {
            bail!(
                "{} is the old single-file shape, which this spoolway no longer reads. \
                 Split it by hand: one `.spoolway/pipelines/<name>.yml` per `pipelines:` entry \
                 (the file name is the pipeline name, so drop the map key), move `default:` to \
                 `dispatch.default_pipeline` in config.toml, \
                 then delete the old file. `spoolway pipeline contract` prints the annotated \
                 blank for reference.",
                old.display()
            );
        }

        Pipelines::assemble(builtin_pipelines()?, config)
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
            default: config.dispatch.default_pipeline.clone(),
            pipelines,
        };
        set.validate()?;

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

    /// The built-in definitions, already parsed. Production code loads from
    /// disk and falls back to the same files; this is the test fixture.
    #[cfg(test)]
    pub fn builtin() -> Pipelines {
        Pipelines::assemble(
            builtin_pipelines().expect("built-in pipelines must parse"),
            &crate::config::Config::default(),
        )
        .expect("built-in pipelines must be valid")
    }

    /// The two shipped pipelines, assembled against `config` rather than the
    /// built-in default — what `pipeline_check` validates alongside whatever
    /// this project's own `.spoolway/pipelines/` loaded, so a shipped
    /// pipeline is gated whether or not an override shadows it.
    ///
    /// `dispatch.default_pipeline` is forced to `default` first: this is a
    /// fixed two-pipeline reference set, not the project's real routing, and
    /// `assemble`'s own [`Pipelines::validate`] refuses a default that names
    /// a pipeline outside the set it is validating — which a project whose
    /// real default is `impl`, say, would otherwise trip on every time.
    pub(crate) fn shipped(config: &crate::config::Config) -> Result<Pipelines> {
        let mut config = config.clone();
        config.dispatch.default_pipeline = "default".to_string();
        Pipelines::assemble(builtin_pipelines()?, &config)
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

    /// The pipeline a task runs on: the one it names, or the default.
    pub fn for_task(&self, task: &crate::task::Task) -> Result<&Pipeline> {
        let name = task.front.pipeline.as_deref().unwrap_or(&self.default);
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

    /// Whether every step with this id names its own model.
    ///
    /// A name no step defines answers false rather than vacuously true:
    /// nothing there can be carrying a model, so it counts as having none.
    /// Blank counts the same as absent — spoolway ships no model of its own,
    /// so an empty `model:` is exactly as unrunnable as a missing one.
    ///
    /// Command steps are skipped, because a step id is shared across
    /// pipelines and a `run:` step names no model by design. `handover` is
    /// the live case: `default` runs it as `spoolway stack` with no model at
    /// all, while `local` still hands it to one, and without this the
    /// command step would report the model step's agent as unrunnable.
    pub fn step_has_model(&self, step_id: &str) -> bool {
        let mut steps = self
            .pipelines
            .values()
            .filter_map(|pipeline| pipeline.step(step_id))
            .filter(|step| step.run.is_none())
            .peekable();
        steps.peek().is_some()
            && steps.all(|step| step.model.as_deref().is_some_and(|m| !m.trim().is_empty()))
    }

    pub fn validate(&self) -> Result<()> {
        if self.pipelines.is_empty() {
            bail!("no pipelines defined");
        }
        if !self.pipelines.contains_key(&self.default) {
            bail!(
                "`dispatch.default_pipeline` names `{}`, and no file defines it \
                 (defined: {}). Set it with `spoolway config set dispatch.default_pipeline \
                 <name>`, or add the file it is asking for.",
                self.default,
                self.names().join(", ")
            );
        }
        for (name, pipeline) in &self.pipelines {
            pipeline
                .validate()
                .with_context(|| format!("pipeline `{name}`"))?;
        }
        Ok(())
    }
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

    /// `handover` is a command step in one shipped pipeline and a model step
    /// in another, and `spoolway doctor` asks about it by id across all of
    /// them. A `run:` step names no model on purpose, so counting it as a
    /// missing one would report a perfectly runnable agent as unrunnable.
    // covers: step.run — a command step runs no agent, so it carries no model to be missing
    #[test]
    fn a_command_step_does_not_count_as_a_step_missing_its_model() {
        let pipelines = Pipelines::builtin();

        let command_steps: Vec<&str> = pipelines
            .pipelines
            .values()
            .flat_map(|pipeline| &pipeline.steps)
            .filter(|step| step.run.is_some())
            .map(|step| step.id.as_str())
            .collect();
        assert!(
            command_steps.contains(&"handover"),
            "the shipped set should still have a command `handover`: {command_steps:?}"
        );

        // Every agent a shipped pipeline names has a model for every step it
        // really runs — the command steps sharing those ids change nothing.
        for (agent, steps) in pipelines.referenced_agents() {
            for step in steps {
                assert!(
                    pipelines.step_has_model(step),
                    "step `{step}` on agent `{agent}` reads as having no model"
                );
            }
        }
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

    /// A gate with nowhere to send a rejection is legal — it parks on
    /// `blocked` the same as any other fail — but it is the shape
    /// `--reject`'s own help used to overpromise a real `on_fail` route for,
    /// so `gate_warnings` names it rather than leaving it silent.
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

    /// A gate that does declare `on_fail` has somewhere a reject actually
    /// goes, so nothing here is worth a warning.
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
    /// task documents its own diff, hands it over, then waits for its checks
    /// — the wait a lane used to do itself, in prose no prompt carries any
    /// more. `checks` is the last step either one runs. `land` is not a step
    /// any more, and nothing in a pipeline decides whether a step runs.
    ///
    /// `handover` is a command step — `run: spoolway stack`, no model and no
    /// rebase — and its failure routes straight to `blocked`: `spoolway
    /// stack` calls `gh pr view` first, so re-running `handover` from
    /// `blocked` is safe and there is no second, LLM-run escalation step to
    /// fall back to any more. A red `checks` routes straight to `blocked`
    /// too, since nothing about a failed check is fixed by handing the
    /// change over again.
    // covers: step.end — a terminal step is where a pipeline stops, and every route has to reach one
    #[test]
    fn both_pipelines_end_at_document_then_handover() {
        for name in ["default", "bugfix"] {
            let pipeline = Pipelines::builtin().get(name).unwrap().clone();
            assert!(
                pipeline.step("land").is_none(),
                "pipeline `{name}` still has a `land` step"
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
            assert_eq!(
                handover.on_pass.as_deref(),
                Some("checks"),
                "pipeline `{name}`"
            );
            assert_eq!(
                handover.on_fail.as_deref(),
                Some(BLOCKED),
                "pipeline `{name}`: a failed `spoolway stack` is a person's call, not \
                 another agent step"
            );
            let checks = pipeline.step("checks").expect("`checks`");
            assert_eq!(checks.on_pass.as_deref(), Some(DONE), "pipeline `{name}`");
            assert_eq!(
                checks.on_fail.as_deref(),
                Some(BLOCKED),
                "pipeline `{name}`: a red check is a person's call, not another handover"
            );
        }
    }

    /// Every shipped pipeline carries the same key reference, fenced, and it is
    /// the one `spoolway update` writes. Four files hold a copy of this block
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
            "on_loop_max",
            "timeout",
            "background",
            "headless",
            "last",
            "cleanup",
        ] {
            assert!(
                block.contains(&format!("#   {key} ")),
                "the key reference says nothing about `{key}`"
            );
        }
    }

    /// What the board's NEXT column asks. One hop along `on_pass` — nothing in
    /// a pipeline can make a step fall through any more.
    #[test]
    fn the_next_step_is_the_one_on_pass_names() {
        let pipeline = Pipelines::builtin().get("default").unwrap().clone();
        assert_eq!(pipeline.next_running_step("implement"), Some("review"));
        assert_eq!(pipeline.next_running_step("review"), Some("document"));
        assert_eq!(pipeline.next_running_step("document"), Some("handover"));
        assert_eq!(pipeline.next_running_step("handover"), Some("checks"));
        // `checks` is the last thing that runs — `done` is not a step.
        assert_eq!(pipeline.next_running_step("checks"), None);
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

    /// Each of the five keys `blocked` routes itself with is refused by name,
    /// with a message that says what actually decides it.
    #[test]
    fn blocked_refuses_the_keys_it_routes_itself_with() {
        let cases: &[(&str, &str)] = &[
            ("on_pass: z\n", "on_pass"),
            ("on_fail: z\n", "on_fail"),
            ("loop: 3\n    on_loop_max: z\n", "on_loop_max"),
            ("gate: true\n", "gate"),
            ("end: true\n", "end"),
        ];
        for (key, message) in cases {
            let err = parse(&format!(
                "steps:\n  - id: blocked\n    agent: pi\n    {key}  \
                 - id: z\n    end: true\n"
            ))
            .unwrap_err();
            assert!(err.to_string().contains(message), "{message}: {err}");
        }
    }

    /// `blocked` is exempt from the on_pass-required check every other agent
    /// step is held to, and from the unbounded-loop check — it has no
    /// declared destinations at all, so it cannot self-edge into one.
    #[test]
    fn blocked_needs_no_on_pass_and_forms_no_loop() {
        let pipeline = parse("steps:\n  - id: blocked\n    agent: pi\n").unwrap();
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

    /// [`crate::config::Config::default`], pointed at `name` — every test
    /// below builds one pipeline and has to name it as the default, or
    /// `Pipelines::validate` refuses the set for a reason that has nothing to
    /// do with what the test is about.
    fn config_for(name: &str) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.dispatch.default_pipeline = name.to_string();
        config
    }

    /// A pipeline that declares no `blocked` step of its own gets one built
    /// straight from `[unattended]`'s `blocked_*` keys, appended last, and
    /// `pipeline_check` — via `Pipelines::validate` — sees the same step
    /// `pipeline.step("blocked")` finds.
    #[test]
    fn assemble_materialises_blocked_from_config_for_a_pipeline_declaring_none() {
        let mut config = config_for("solo");
        config.unattended.blocked_agent = "pi".into();
        config.unattended.blocked_model = "my-model".into();
        config.unattended.blocked_effort = "high".into();
        config.unattended.blocked_prompt = "clearer".into();
        config.unattended.blocked_session = false;

        let set = Pipelines::assemble(one_step_pipeline("solo", ""), &config).unwrap();
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
        let mut config = config_for("ui");
        config.unattended.blocked_agent = "claude".into();
        config.unattended.blocked_model = "config-model".into();
        config.unattended.blocked_effort = "medium".into();
        config.unattended.blocked_prompt = "unblocker".into();
        config.unattended.blocked_session = true;

        let set = Pipelines::assemble(
            one_step_pipeline("ui", "  - id: blocked\n    model: override-model\n"),
            &config,
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
        let config = config_for("p");
        let cases: &[(&str, &str)] = &[
            ("description: nope\n", "description"),
            ("slot: false\n", "slot"),
            ("loop: 3\n", "loop"),
        ];
        for (key, message) in cases {
            let pipelines = one_step_pipeline("p", &format!("  - id: blocked\n    {key}"));
            let err = Pipelines::assemble(pipelines, &config).unwrap_err();
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
        let config = config_for("solo");
        let set = Pipelines::assemble(one_step_pipeline("solo", ""), &config).unwrap();
        set.validate()
            .expect("an assembled set must validate again cleanly");
    }

    /// A blank `unattended.blocked_model` does not stop the config, or the
    /// pipelines built from it, from loading — the refusal is at
    /// `spoolway dispatch` and `spoolway doctor`, not here, so
    /// `spoolway config set` stays usable to fix it.
    #[test]
    fn a_blank_blocked_model_still_assembles() {
        let mut config = config_for("solo");
        config.unattended.blocked_model = String::new();

        let set = Pipelines::assemble(one_step_pipeline("solo", ""), &config).unwrap();
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

    /// The one that would otherwise read as a handled failure: by the time a
    /// background command exits, the task has been somewhere else for a while
    /// and nothing could route on it.
    #[test]
    fn rejects_a_background_command_that_declares_on_fail() {
        let err = parse(
            "steps:\n  - id: a\n    run: ./bench.sh\n    \
             background: true\n    on_pass: z\n    on_fail: z\n  - id: z\n    end: true\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("already moved on"), "{message}");
        // And it says what to do about it, both ways round.
        assert!(message.contains("Drop `background:`"), "{message}");
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

    /// A step that only ever routes to itself. Both routes are explicit, so it
    /// never falls through to the reserved `blocked` either — which is the one
    /// way a graph can still strand a task now that both endings are built in.
    #[test]
    fn rejects_a_cycle_with_no_way_out() {
        let err = parse("steps:\n  - id: a\n    agent: pi\n    on_pass: a\n    on_fail: a\n")
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
        assert!(
            message.contains("form a loop that gives up into itself"),
            "{message}"
        );
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

    /// A bounded route only breaks a cycle if its exit actually leaves. One
    /// that carries straight back inside — `fix`'s own `on_pass`, which is
    /// `build`, is exactly where the route in came from — bounds nothing: the
    /// task just goes round again however it got there.
    #[test]
    fn rejects_a_bounded_route_whose_exit_re_enters_the_loop() {
        let err = parse(
            "steps:\n  - id: fix\n    agent: pi\n    loop:\n      build: 3\n    on_pass: build\n    on_fail: blocked\n  \
             - id: build\n    agent: pi\n    on_pass: fix\n    on_fail: blocked\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("form a loop that gives up into itself"),
            "{message}"
        );
    }

    /// The plan's own canonical mistake: the loop is on `fix`, which counts
    /// arrivals from `review` but always exits through `build` — still inside
    /// — so the walk never even names `fix`, only the closed ring behind it.
    /// The fix is to move `loop:` to `review`, the one member of that ring
    /// whose own `on_pass` already leaves it, and the message has to say so.
    #[test]
    fn a_refused_loop_names_the_cycle_member_whose_own_pass_leaves_it() {
        let err = parse(
            "steps:\n  \
             - id: review\n    agent: pi\n    on_pass: checkpoint\n    on_fail: fix\n  \
             - id: fix\n    agent: pi\n    loop:\n      review: 3\n    on_pass: build\n    \
             on_fail: blocked\n  \
             - id: build\n    agent: pi\n    on_pass: verify\n    on_fail: blocked\n  \
             - id: verify\n    agent: pi\n    on_pass: review\n    on_fail: blocked\n  \
             - id: checkpoint\n    end: true\n",
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("form a loop that gives up into itself"),
            "{message}"
        );
        assert!(
            message.contains("Move `loop:` to `review`"),
            "the only ring member whose own pass leaves is `review`: {message}"
        );
        assert!(
            message.contains("what follows `review` is `checkpoint`"),
            "{message}"
        );
    }

    /// A limit for a route that does not exist bounds nothing while looking
    /// like it bounds something, so the loop check would pass and the loop
    /// would still be open.
    #[test]
    fn rejects_a_loop_map_naming_a_step_that_routes_elsewhere() {
        let err = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n    on_fail: b\n  - id: b\n    agent: pi\n    loop:\n      z: 2\n    on_pass: z\n    on_fail: a\n  - id: z\n    end: true\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("never routes to it"), "{err}");
    }

    #[test]
    fn a_loop_map_bounds_the_route_it_names_and_no_other() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: b\n    on_fail: b\n  - id: b\n    agent: pi\n    loop:\n      a: 4\n    on_pass: z\n    on_fail: a\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let b = pipeline.step("b").unwrap();
        assert_eq!(b.round_limit("a"), Some(4));
        assert_eq!(b.round_limit("somewhere-else"), None);
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

    /// An `on_loop_max` naming nothing would only be discovered by a task
    /// actually spending a budget, which is the worst moment to find out.
    // covers: step.on_loop_max — where a spent loop sends the task, and that it must name a step that exists
    #[test]
    fn on_loop_max_must_name_a_real_step_and_answer_for_a_budget() {
        let unknown = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n    on_fail: a\n    \
             loop: 2\n    on_loop_max: nowhere\n  - id: z\n    end: true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(
            unknown.contains("on_loop_max points at unknown step"),
            "{unknown}"
        );

        let budgetless = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: z\n    on_loop_max: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap_err()
        .to_string();
        assert!(budgetless.contains("no `loop:`"), "{budgetless}");
    }

    /// `on_loop_max` is a route a task really takes, so the walks that decide
    /// reachability and cycles have to see it like any other edge — and a
    /// step that names none still carries on to `on_pass` rather than falling
    /// to `blocked`.
    #[test]
    fn on_loop_max_is_an_edge_the_graph_walks_see() {
        let pipeline = parse(
            "steps:\n  - id: a\n    agent: pi\n    on_pass: b\n    on_fail: b\n  \
             - id: b\n    agent: pi\n    loop:\n      a: 2\n    \
             on_loop_max: z\n    on_pass: a\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let b = pipeline.step("b").unwrap();
        assert!(pipeline.destinations(b).contains(&"z"));
        assert_eq!(b.loop_exit(), "z");
        // And a step that names none carries on to its own `on_pass`.
        let a = pipeline.step("a").unwrap();
        assert_eq!(a.loop_exit(), "b");
    }

    /// The whole point of counting per route: `review` is entered from
    /// several places in the shipped pipeline, and each of them is its own
    /// loop.
    #[test]
    fn the_shipped_review_step_bounds_the_loop_that_comes_back_through_it() {
        let pipeline = Pipelines::builtin().get("default").unwrap().clone();
        let review = pipeline.step("review").unwrap();
        assert_eq!(
            review.round_limit("implement"),
            Some(2),
            "`implement` always returns through `review`, so that is where the budget lives"
        );
    }
}
