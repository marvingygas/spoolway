//! Everything configurable lives here, in one `.spoolway/config.toml`.
//!
//! The file is the interface: every setting carries its explanation as a
//! comment above the key, regenerated on every save. The pipeline graphs live
//! next door in `.spoolway/pipelines/` — see [`crate::pipeline`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Directory, relative to the repo root, holding the project's tracked
/// control plane — config, pipelines, prompts, task templates — and marking
/// the directory `Repo::root` walks up looking for.
pub const STATE_DIR: &str = ".spoolway";
pub const CONFIG_FILE: &str = "config.toml";

/// The tracked control plane, relative to the repo root and inside
/// [`STATE_DIR`]. Constants because nothing ever moves them: every caller
/// already goes through the accessors on [`crate::repo::Repo`].
pub const PROMPTS_DIR: &str = ".spoolway/prompts";
pub const TASK_TEMPLATES_DIR: &str = ".spoolway/templates/tasks";
/// The two ticket-body templates `queue add`'s open hook renders —
/// `epic.md` and `ticket.md` — beside the task templates but their own
/// sibling directory: a task skeleton is the body a lane starts from, these
/// are the body a tracker's issue starts from, and the two are never
/// selected the same way (one by pipeline name, these by a fixed pair).
pub const TRACKING_TEMPLATES_DIR: &str = ".spoolway/templates/tracking";
/// Repeatable task documents a project keeps to re-run, nested however it
/// likes and tracked in git alongside the prompts and task templates above.
/// Unlike those, `spoolway init` never writes this directory and never seeds
/// it — a routine only exists once a person saves one with `s` from the
/// queue screen, so an ordinary project has no directory here at all rather
/// than an empty one `init` put there. See [`crate::repo::Repo::routines_dir`].
pub const ROUTINES_DIR: &str = ".spoolway/routines";
/// The project-scoped cron-job store, tracked in the checkout and shared
/// with the team. The user-scoped store is the same base name
/// ([`JOBS_STORE`]) in this machine's per-project home, never tracked. See
/// [`crate::jobs`] and [`crate::repo::Repo::jobs_file`].
pub const JOBS_FILE: &str = ".spoolway/jobs.toml";
/// The bare name of a cron-job store, for the user-scoped copy that lives
/// directly under the project's machine home beside `lanes.json`.
pub const JOBS_STORE: &str = "jobs.toml";
/// The pull request template `spoolway stack`'s summary prompt fills in,
/// beside the task templates but a single file rather than one per pipeline —
/// there is one shape of pull request, whatever the pipeline that opened it.
pub const PULL_REQUEST_TEMPLATE: &str = ".spoolway/templates/pull-request.md";
/// Every typed message a lane's pane receives, one `##` section per state —
/// beside the task templates, a single file rather than one per pipeline,
/// the same shape as the pull request template above. See
/// [`crate::lane_prompts`].
pub const LANE_PROMPTS_TEMPLATE: &str = ".spoolway/templates/lane-prompts.md";
/// What belongs under each of the three headings spoolway appends to a task
/// file, one `##` section per heading — beside the task templates, a single
/// file rather than one per pipeline, the same shape as the two above. See
/// [`crate::task_log`].
pub const TASK_LOG_TEMPLATE: &str = ".spoolway/templates/task-log.md";

/// Runtime state, kept out of the checkout entirely — see
/// [`crate::repo::Repo::home`]. Bare segment names rather than paths from the
/// root: every caller joins them onto the project's home directory through
/// the accessors on [`crate::repo::Repo`], which is what lets a name here
/// change without a signature changing anywhere.
pub const QUEUE_DIR: &str = "queue";
pub const ARCHIVE_DIR: &str = "archive";
pub const PENDING_DIR: &str = "pending";

/// Directory under the project's home holding one scratch worktree per task,
/// for the short-lived worktrees a rebase needs. A segment rather than a full
/// path like its siblings above, because every caller joins it onto a task id.
pub const SCRATCH_DIR: &str = "scratch";

/// Is `id` usable as the name of a file under the project's home directory —
/// see [`crate::repo::Repo::home`]?
///
/// Task ids and step ids are both path components before they are anything
/// else. A lane's composed prompt is `prompts/<task> · <step>.md`, a headless
/// lane's record is `headless/<task> · <step>.json`, a command step's output is
/// `commands/<task> · <step>.log`, and a task's worktree directory is its
/// branch flattened to one component — `task-<id>`, or `task-<slug>-<id>` when
/// a tracker prefixed it — four directories, two ids, and not one of them a
/// fixed string. An id holding `/` or `..` therefore writes outside the
/// directory it was supposed to name,
/// which is a confusing failure rather than a breach: both ids come from files
/// a lane may not write, and a pipeline that wanted to run something arbitrary
/// has `commands:` for that. It is refused because a name that means one thing
/// in the queue and another on disk is worth nothing.
///
/// Checked where an id is *read* — parsing a task file, validating a pipeline —
/// rather than only where one is written, because `queue add` is not the only
/// way a file gets into the project's `queue/`: a person edits one, and a plan
/// writes several.
///
/// `kind` names what is being checked, for the message: `task id`, `step id`.
pub fn check_id(kind: &str, id: &str) -> Result<()> {
    let mut chars = id.chars();
    match chars.next() {
        None => bail!("a {kind} cannot be empty"),
        Some(first) if !first.is_ascii_lowercase() => bail!(
            "{kind} `{id}` must start with a lowercase letter — it names files under the \
             project's home directory, and a task id also names a multiplexer session"
        ),
        Some(_) => {}
    }
    match chars.find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')) {
        Some(bad) => bail!(
            "{kind} `{id}` contains `{bad}` — use lowercase letters, digits and hyphens, \
             which is all a name under the project's home directory may hold"
        ),
        None => Ok(()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub dispatch: DispatchConfig,
    /// Everything that means something only in an unattended run — see
    /// [`UnattendedConfig`]. Its own table, right after `[dispatch]`, because
    /// every other `[dispatch]` key applies whether or not a person is
    /// watching and these do not.
    pub unattended: UnattendedConfig,
    /// Where an old `[paths]` table lands so an existing config still parses.
    /// See [`LegacyPaths`]; the directories it named are constants now.
    /// Dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    paths: LegacyPaths,
    /// What `spoolway pipeline gen` opens, and what it tells the generation
    /// procedure. See [`PipelineGenConfig`].
    pub pipeline_gen: PipelineGenConfig,
    /// Whether spoolway says a newer release is out. See [`UpdateConfig`].
    pub update: UpdateConfig,
    /// What window `spoolway-calibrate` reads. See [`CalibrateConfig`].
    pub calibrate: CalibrateConfig,
    /// How long a byproduct directory keeps what it holds before
    /// [`crate::retain`] deletes it. See [`RetentionConfig`].
    pub retention: RetentionConfig,
    /// When the shared model-price table is old enough to mention. See
    /// [`PricesConfig`].
    pub prices: PricesConfig,
    /// Where an old `[plans]` table lands so an existing config still
    /// parses. See [`LegacyPlans`]; the binary keeps no notion of a plan
    /// store any more — spoolway-plan writes its page wherever it is told
    /// to. Dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    plans: LegacyPlans,
    /// Where an old `[docs]` table lands so an existing config still parses.
    /// See [`LegacyDocs`]; spoolway keeps no notion of documentation any
    /// more — where documents live, and what each covers, is
    /// `assets/prompts/archivist/PROMPT.md`'s to say. Dropped
    /// unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    docs: LegacyDocs,
    /// `spoolway stack`'s own settings — see [`StackConfig`].
    pub stack: StackConfig,
    /// Agent profiles, referenced by name from a pipeline step's `agent:`.
    pub agents: BTreeMap<String, AgentProfile>,
    /// Where an old `[effort]` table lands so an existing config still
    /// parses. See [`LegacyEffort`]; a step's `effort:` needs no machine
    /// setting to mean anything now. Dropped unconditionally on the next
    /// save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    effort: LegacyEffort,
    /// Where an old `[sandbox]` table lands so an existing config still
    /// parses. See [`LegacySandbox`]; the layer it configured is gone, and
    /// nothing inside spoolway replaces the one goal it carried — see
    /// `blocked_on_write` above. Dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    sandbox: LegacySandbox,
    /// Where an old `[criteria]` table lands so an existing config still
    /// parses. See [`LegacyCriteria`]; standards live in the prompt now.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub criteria: LegacyCriteria,
    /// What a model costs, and how big its context window is, keyed by a glob
    /// over the model name. Consulted by `spoolway eval --by`.
    ///
    /// Aliased to `pricing`, its name before the window joined the rates: an
    /// old `[pricing]` table is read into this field and written back as
    /// `[models]`.
    #[serde(alias = "pricing")]
    pub models: BTreeMap<String, crate::usage::ModelPrice>,

    /// Which skills `spoolway eval` gives a block of their own, named the way
    /// the ledger banks them — a `spoolway-` prefix already stripped. Every
    /// other name the ledger holds, including `interactive`, is left off
    /// `spoolway eval` entirely rather than growing the table by one block
    /// per skill anybody has ever typed; its spend still reaches
    /// `spoolway eval --by`, which never consults this list.
    ///
    /// Tracked, so naming a skill here is itself an edit that mints a new
    /// version — the same as editing a prompt or a pipeline is.
    pub skills: Vec<String>,

    /// Where a project's issue tracker lives, so the dispatcher can tell it
    /// about a task's arrival at `queued`, `blocked`, `paused` or `done` — the
    /// four states nothing inside a pipeline file can already put a `run:`
    /// step on, since none of the four is a step a pipeline may declare. See
    /// [`crate::tracking`] for what actually fires.
    pub issue_tracking: IssueTrackingConfig,

    /// Every top-level key this binary does not know, kept rather than
    /// refused — see `Frontmatter::extra`, which this matches.
    ///
    /// `deny_unknown_fields` used to sit on this struct instead, which reads
    /// an install a version behind a project's config as if the config were
    /// wrong: a key a newer binary added is not a typo, and refusing to parse
    /// it took down every command with it, `doctor` included — the one whose
    /// job is explaining what broke. Never written back — `skip_serializing`,
    /// the same as every other retired table above — so a key this binary
    /// truly does not know is dropped on the next save exactly as
    /// `blocked_on_write` and `blocked_on_overreach` used to be by name; the
    /// two of them needed no field of their own once this existed to catch
    /// them.
    #[allow(dead_code)]
    #[serde(flatten, skip_serializing)]
    extra: BTreeMap<String, toml::Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dispatch: DispatchConfig::default(),
            unattended: UnattendedConfig::default(),
            paths: LegacyPaths::default(),
            pipeline_gen: PipelineGenConfig::default(),
            update: UpdateConfig::default(),
            calibrate: CalibrateConfig::default(),
            retention: RetentionConfig::default(),
            prices: PricesConfig::default(),
            plans: LegacyPlans::default(),
            docs: LegacyDocs::default(),
            stack: StackConfig::default(),
            agents: AgentProfile::defaults(),
            effort: LegacyEffort::default(),
            sandbox: LegacySandbox::default(),
            criteria: LegacyCriteria::new(),
            // Empty for the same reason no model name ships in `[agents]`:
            // spoolway does not know what you run, and a guessed price is worse
            // than an admitted blank. Every model a shipped pipeline names is
            // already covered by the built-in table once it exists — a row
            // here only corrects one, or prices a model nobody publishes.
            models: BTreeMap::new(),
            // Only `spoolway-plan` is worth a block of its own — the others
            // spoolway ships are queried, not edited, and drift far less
            // often than the prompt a planning session shapes. Named in
            // full, the same string the slash command has: the ledger banks
            // whatever ran, unrewritten.
            skills: vec!["spoolway-plan".to_string()],
            issue_tracking: IssueTrackingConfig::default(),
            extra: BTreeMap::new(),
        }
    }
}

/// `[issue_tracking]`: a project's own tracker, named once here rather than
/// wired into a pipeline. A pipeline step's `run:` cannot fire on `queued`,
/// `blocked`, `paused` or `done` at all — those four are reserved, never
/// steps a pipeline may declare — so a project that wants a ticket touched on
/// one of them has nowhere else to say so.
///
/// The defaults are "no issue tracking": `hook`, `project_key` and `on_fail`
/// blank, `key_in_names` false. A blank `hook` runs nothing and changes
/// nothing about a task's four events or about `queue add`'s generated names,
/// whatever the other three keys hold.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IssueTrackingConfig {
    /// A bare filename, resolved inside `.spoolway/hooks/` in the checkout —
    /// never a path. `crate::tracking::is_bare_filename` refuses to run
    /// anything holding a separator or naming `.`/`..`, so a hook always
    /// names a script this project actually ships rather than one reached by
    /// climbing out of that directory. Blank means no hook runs at all.
    pub hook: String,

    /// Opaque to spoolway: `owner/repo` on github, a project key on jira,
    /// whatever the hook script itself expects. Never parsed or validated —
    /// it reaches the script exactly as this holds it, as
    /// `SPOOLWAY_PROJECT_KEY`.
    pub project_key: String,

    /// What a non-zero hook exit does to the task it ran for. Blank behaves
    /// as `"ignore"`: the failure is recorded — see [`crate::tracking`]'s own
    /// count, which the board's footer prints — and nothing else changes.
    /// `"pause"` additionally holds the task: on `queued` it lands on
    /// `paused`, and on `done` it stays out of the archive. A failure on
    /// `blocked` or `paused` is only ever recorded, whichever this holds —
    /// both are already stopped for a person.
    pub on_fail: String,

    /// Whether the issue's key rides into every name `queue add` generates.
    /// Off by default: nothing changes, and every group, branch and worktree
    /// directory is named exactly as it is today.
    ///
    /// On, and with the tracker hook answering a `slug=` line, `queue add`
    /// prefixes the `group:`, the `branch:` (`task/<slug>-<id>`) and the
    /// worktree directory with that slug, so `git branch` shows which issue a
    /// branch belongs to. spoolway still parses no tracker identifier of its
    /// own — the slug comes from the hook, the one thing that knows the
    /// tracker, and is only checked against [`check_id`]'s alphabet.
    pub key_in_names: bool,
}

/// What `spoolway pipeline gen` opens, and what it hands the generation
/// procedure — everything the `spoolway-pipeline` skill needs that is a
/// per-project preference rather than a fact the skill decides for itself.
///
/// No `prompt` key here, deliberately: the skill *is* the whole brief for
/// this session, the same way a prompt is for a lane's — a second file
/// layered on top would only be one more place the instructions could
/// disagree with each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PipelineGenConfig {
    /// A profile from `[agents.*]` — which binary the generation session
    /// runs as.
    pub pipeline_agent: String,

    /// The model that session runs. Blank refuses the command outright: a
    /// generation session with no model named would launch and then have
    /// nothing to say about what it is.
    pub pipeline_model: String,

    /// How hard that model thinks, handed straight through the same way a
    /// step's `effort:` is. Blank means the kind's own default.
    pub pipeline_effort: String,

    /// `true` skips asking and takes the procedure's own recommendation —
    /// see the `spoolway-pipeline` skill's generation procedure.
    pub pipeline_auto: bool,

    /// The loop budget every loop a generated pipeline writes starts at.
    /// Binds generation only: it is typed into the file the procedure
    /// writes, and never reaches back to change a shipped pipeline's own
    /// numbers.
    pub pipeline_loop_default: u32,

    /// Whether local models are involved in what gets generated. Pre-answers
    /// the procedure's first question rather than skipping it — the
    /// procedure still says so out loud, it just does not have to ask.
    pub pipeline_local_models: bool,
}

impl Default for PipelineGenConfig {
    fn default() -> Self {
        Self {
            pipeline_agent: "claude".into(),
            pipeline_model: String::new(),
            pipeline_effort: String::new(),
            pipeline_auto: false,
            pipeline_loop_default: 1,
            pipeline_local_models: false,
        }
    }
}

/// Whether this project's checkouts are told about a newer release.
///
/// One setting, and it is the project's rather than the machine's because a
/// team that does not want its pipeline output disturbed decides that once,
/// in a file everybody has. The other direction — one person, one laptop —
/// is [`crate::release::ENV_SKIP`], which needs no file to be committed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    pub check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        // On: a check nobody switched on is a check nobody has, and this one
        // costs a file read on the command path and nothing else.
        Self { check: true }
    }
}

/// `spoolway-calibrate`'s own table: how far back it looks.
///
/// One key, the same shape as [`UpdateConfig`], because the skill needs
/// nothing else from config — its scope (which control-plane paths a finding
/// may touch) is fixed in the skill's own procedure, not a per-project
/// setting, the same way a pipeline step's prompt is named in the pipeline
/// rather than here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CalibrateConfig {
    /// How far back a calibration session reads: archived task documents
    /// finished inside the window, and the ledger entries beside them.
    ///
    /// Same spelling `--since` already takes, and the same parser —
    /// [`human_duration`] — so `30d` here and `--since 30d` on `spoolway
    /// eval` mean the same thing.
    #[serde(with = "human_duration")]
    pub window: Duration,
}

impl Default for CalibrateConfig {
    fn default() -> Self {
        Self {
            // Two to three weeks of dispatching is enough archive to see a
            // pattern repeat without also asking a person to read a
            // half-year of history the first time they run this.
            window: Duration::from_secs(14 * 86_400),
        }
    }
}

/// [`crate::retain`]'s own table: how long a byproduct directory keeps what
/// it holds.
///
/// One key, on purpose. Which directories are byproducts and which are live
/// state is a fact about spoolway's own layout, not a project's to redraw —
/// see [`crate::retain`]'s own doc for the fixed split. All this table gives
/// a project is the one knob that actually varies by install: how long
/// somebody wants the logs kept around before they go.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// How many days an entry sits in a byproduct directory before
    /// [`crate::retain`] deletes it, read off the entry's own modification
    /// time. `0` keeps everything forever — what every install does before
    /// this key existed, and still does until somebody lowers it.
    pub days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        // Thirty days is enough to look back at last week's run without
        // ever having to, while still bounding a home that otherwise grows
        // without end — see the plan's own cost line on the archive.
        Self { days: 30 }
    }
}

/// The shared model-price tables' own freshness setting.
///
/// One key, on purpose: age is only reported, never a trigger for a fetch or
/// a reason for a command to fail. Refreshing remains the explicit
/// `spoolway models refresh` command however old the active table becomes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PricesConfig {
    /// How old the table may be before `spoolway doctor` notes it. `0` turns
    /// the note off without changing which table answers model lookups.
    pub max_age_days: u64,
}

impl Default for PricesConfig {
    fn default() -> Self {
        Self { max_age_days: 30 }
    }
}

/// `spoolway stack`'s own table. There is deliberately no switch here that
/// turns stacking itself on or off — a pipeline's step is what does that, by
/// naming `run: spoolway stack` or not — so this holds only the one thing a
/// project may want to add on top of the git-and-`gh` mechanics: a model that
/// writes the pull request's title and a short summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StackConfig {
    /// Always written, blank `agent` and `model` and all — see
    /// [`StackSummary`] for what blank means.
    pub summary: StackSummary,
}

/// The model `spoolway stack` runs on the task file before opening a pull
/// request, named the same way a pipeline step names one — `agent`, `model`
/// and `effort` mean exactly what they mean on a `Step`, because this is the
/// same call with no worker slot and no prompt held by a pipeline.
///
/// Blank `agent` and `model` are what put `spoolway stack` in task-file
/// mode — the same shape `pipeline_gen.pipeline_model` already uses, where a
/// blank model refuses the command. There is deliberately no separate switch
/// naming the mode: two blank strings already say it, and a second key could
/// only ever disagree with them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StackSummary {
    /// Agent profile from `[agents.*]` the summary runs on. Blank alongside
    /// `model` means no summary turn runs at all.
    pub agent: String,
    /// Model that profile's kind is started with. Blank alongside `agent`
    /// means no summary turn runs at all.
    pub model: String,
    /// Passed to the agent kind's effort flag, same as a step's `effort:`.
    /// Blank means no flag is sent.
    pub effort: String,
    /// Prompt (without extension) under the prompts directory. This
    /// is the only place a prompt is named outside a pipeline step.
    pub prompt: String,
}

impl Default for StackSummary {
    fn default() -> Self {
        Self {
            agent: String::new(),
            model: String::new(),
            effort: String::new(),
            prompt: "summariser".to_string(),
        }
    }
}

/// Tab 1 — the run loop itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DispatchConfig {
    /// Where lanes run: a multiplexer, or no multiplexer at all.
    pub backend: Backend,

    /// How a herdr run is laid out in the multiplexer. Read only under
    /// `backend = "herdr"`; headless has no workspaces to lay out.
    pub herdr_mode: MuxMode,

    /// How a tmux run is laid out in the multiplexer. Read only under
    /// `backend = "tmux"`. Same two answers as `herdr_mode`, and a setting of
    /// its own because a machine can point the two multiplexers at different
    /// layouts.
    pub tmux_mode: MuxMode,

    /// Where a dispatched task's worktree is cut. Empty means
    /// `~/.spoolway/<project>/worktrees` — see [`crate::mux::worktree_root`].
    ///
    /// Every backend and both layout modes cut the same way now: spoolway
    /// cuts every dispatched checkout itself, with git, and only ever hands
    /// the multiplexer a path that already exists — see
    /// [`crate::mux::cut_worktree`]. Nested under the project's own directory
    /// rather than under the shared dispatch workspace — see
    /// [`crate::mux::dispatch_workspace_label`] — because a worktree cut
    /// anywhere inside that shared directory never registers as a workspace
    /// of its own the way one cut at a repository's root would, and the
    /// project's own directory holds no checkout of its own either.
    ///
    /// Deliberately outside the checkout: a worktree under `.spoolway/` would
    /// sit beside the prompts every lane already reads, and a lane building
    /// there could rewrite any other task's checkout by name.
    /// Deliberately outside `~/.herdr/` too — that is herdr's directory, and a
    /// checkout spoolway cut is not herdr's to know about.
    pub worktree_root: String,

    /// How long to wait between passes when running as a background job.
    ///
    /// Ten seconds by default, which is about the floor worth having: below it,
    /// each pass's mux IPC round-trip and full reread of every task file cost
    /// more in polling overhead than they buy in reaction time, since a lane
    /// takes real wall-clock minutes regardless of how often it is looked at.
    ///
    /// None of this applies to a local llama.cpp: its KV cache has no TTL and is
    /// evicted by other lanes competing for slots, so `concurrency` governs it
    /// and this does not.
    #[serde(with = "human_duration")]
    pub interval: Duration,

    /// How long a lane may say nothing before the dispatcher reminds it to
    /// report.
    ///
    /// Patience, which is not the same quantity as [`Self::interval`] and used
    /// to be read off it. How often a pass *looks* at a lane says nothing
    /// about how long that lane may reasonably be quiet, and at the shipped
    /// ten-second interval the two together meant a lane had ten seconds to
    /// speak or be nudged. Turning the poll rate down to react faster silently
    /// bought less patience, which is not a trade anybody asked for.
    ///
    /// Ten seconds is not a stuck lane; it is a lane thinking. The case that
    /// forced this apart is an agent step that ends its turn and waits minutes
    /// on a background job — a build, a suite, a `gh pr checks --watch` like the
    /// shipped `checks` step's 45-minute one. A settled lane and a lane that
    /// forgot to report look identical from outside, so the only honest answer
    /// is to wait long enough that silence means something.
    ///
    /// This bounds the wait before *each* reminder, not the whole round trip:
    /// `MAX_REMINDERS` still caps it at three, so a genuinely dead lane is
    /// escalated after four of these rather than sitting forever.
    #[serde(with = "human_duration")]
    pub lane_quiet: Duration,

    /// How long a lane may be held open by a process it started — one still
    /// running, past a settled turn that stopped writing to its transcript —
    /// before the reminder loop stops excusing it and treats it as silent.
    ///
    /// A lane genuinely mid-tool-call is not silent, whatever its transcript
    /// says, and [`crate::dispatch::Dispatcher::note_progress`] excuses it
    /// for exactly that reason, on a backend able to say so via
    /// [`crate::mux::Mux::lane_process_alive`]. But a process that never
    /// exits — a server the turn started and forgot, a build looping on its
    /// own — would excuse the lane forever with nothing here to say
    /// otherwise, which is the ceiling this settles: not "is the lane
    /// silent", answered already, but "for how long can not-silent excuse
    /// it".
    ///
    /// An hour by default: long enough for a real build or test suite to
    /// finish inside it, short enough that a lane wedged behind a runaway
    /// child is still found the same afternoon.
    ///
    /// **Not written to `config.toml` while it holds its default**, for the
    /// same reason as [`Self::tear_lanes_on_stop`] below: a lane here
    /// routinely runs a binary built from a branch behind main, and
    /// `deny_unknown_fields` makes an unknown key a hard parse error rather
    /// than something to ignore.
    #[serde(
        with = "human_duration",
        skip_serializing_if = "is_default_lane_child_ceiling"
    )]
    pub lane_child_ceiling: Duration,

    /// Retired: which branches a task's base could not be. Which branch is
    /// safe to build on is a fact about this project's own git workflow, not
    /// one spoolway is in a position to guess at — so the guard came out
    /// rather than ship an opinion nobody asked for. Kept only so an existing
    /// config still parses; dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    protected_branches: Vec<String>,

    /// Retired: a shell command run when a task needed a person. Kept only so
    /// an existing config still parses; dropped unconditionally on the next
    /// save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    notify: String,

    /// Retired: a terminal opened on the blocked lane, on whatever machine
    /// the dispatcher happened to run on — useless with an all-cloud lane on
    /// a screen nobody is looking at, so it went with the keys. Kept only so
    /// an existing config still parses; dropped unconditionally on the next
    /// save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    open_on_escalation: bool,

    /// Retired along with `open_on_escalation`. Kept only so an existing
    /// config still parses; dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    open: String,

    /// Retired: how many times a task's lane could be *launched* at the step
    /// it is on before a person was asked instead. Never a setting anybody
    /// tuned in practice, so the guard it sized is now a constant — see
    /// [`crate::dispatch::MAX_LAUNCHES`]. Kept only so an existing config
    /// still parses; dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(alias = "max_attempts", default, skip_serializing)]
    max_launches: u32,

    /// Retired: whether a pipeline's `unblock:` step actually ran. The step and
    /// the prompt behind it are gone — see [`crate::pipeline::Pipeline`] — and
    /// [`UnattendedConfig`] is what the run it was reached for is now. Kept
    /// only so an existing config still parses; dropped unconditionally on the
    /// next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    auto_unblock: bool,

    /// Pipeline a task runs on when its `pipeline:` field is absent.
    ///
    /// Here rather than in a pipeline file because no pipeline can answer it:
    /// naming the default is a statement about the set, and a file that claimed
    /// to be the default would be a second one waiting to claim it too.
    pub default_pipeline: String,

    /// Whether spoolway commits a lane's leftover work when its step settles.
    ///
    /// The one guarantee spoolway makes about git, and the only reason it runs
    /// a git verb of its own: a terminal step with `cleanup: true` removes the
    /// worktree and deletes the branch, so work that is uncommitted at that
    /// moment has nowhere left to exist. Everything else about git — rebasing,
    /// pushing, opening a pull request — is the `handover` step's, running
    /// `spoolway stack`. Nothing merges; a person lands it.
    ///
    /// On by default, because the failure it prevents is silent and total.
    /// A project whose agents are trusted to commit for themselves, or one that
    /// wants the history a lane wrote and nothing else, turns it off and keeps
    /// the residue report.
    pub auto_commit: bool,

    /// Whether a free slot is filled from every ready task, or from the group
    /// already landing first. See [`Priority`].
    ///
    /// Not written to `config.toml` while it holds its default, for the same
    /// reason as [`Self::tear_lanes_on_stop`] below: a lane here routinely
    /// runs a binary built from a branch behind main, and
    /// `deny_unknown_fields` makes an unknown key a hard parse error rather
    /// than something to ignore.
    #[serde(skip_serializing_if = "Priority::is_group")]
    pub priority: Priority,

    /// Whether stopping the dispatcher ends the run's live lanes and takes
    /// their worktrees with it.
    ///
    /// A task that reaches `done` already tears its own worktree and branch
    /// down, so what this sweeps is whatever the run was still holding when it
    /// stopped: every live lane the run holds and its background runs, then
    /// the workspace and worktree of every task spoolway cut one for, and then
    /// the dispatch workspace behind them.
    ///
    /// **Branches are not swept.** A task interrupted mid-step keeps its place
    /// in the queue, and the commits on its branch are the only record of what
    /// its agent got done — so the branch outlives the worktree, and the next
    /// run cuts a fresh one on it and carries on from there. Only a task that
    /// *finished* takes its local branch with it, on the way to the archive.
    ///
    /// **A task on `blocked` is never swept.** Its pane is being kept for a
    /// person to read, and `spoolway resume` needs the checkout underneath it —
    /// so anything spared keeps the dispatch workspace open too. That is also
    /// the only case this fires in at all: a blocked task keeps the loop
    /// running, so a dispatcher that stops on its own has nothing left to sweep.
    ///
    /// Nothing on a remote is touched, and neither is a borrowed checkout: that
    /// branch was cut by a person and the worktree is theirs.
    ///
    /// **Not written to `config.toml` while it holds its default**, for the
    /// same reason as [`UnattendedConfig::skip_blocked_lane`] below: a lane
    /// here routinely runs a binary built from a branch behind main, and
    /// `deny_unknown_fields` makes an unknown key a hard parse error rather
    /// than something to ignore.
    ///
    /// Renamed from `cleanup_on_stop`, which described the mechanism —
    /// tidying up worktrees — rather than what changed when the key fired: a
    /// task's live agent stopped running out from under a directory that was
    /// about to disappear. `#[serde(alias)]` keeps the old spelling parseable
    /// so an un-upgraded config file still loads.
    #[serde(alias = "cleanup_on_stop", skip_serializing_if = "is_yes")]
    pub tear_lanes_on_stop: bool,
}

/// Skips a `true` on the way out — see [`DispatchConfig::tear_lanes_on_stop`]
/// for why a key that holds its default is better off absent here.
fn is_yes(value: &bool) -> bool {
    *value
}

/// Skips an hour on the way out — see [`DispatchConfig::lane_child_ceiling`]
/// for why a key that holds its default is better off absent here.
fn is_default_lane_child_ceiling(value: &Duration) -> bool {
    *value == Duration::from_secs(3600)
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            backend: Backend::default(),
            herdr_mode: MuxMode::default(),
            tmux_mode: MuxMode::default(),
            interval: Duration::from_secs(10),
            // Four of these (`MAX_REMINDERS` + 1) comfortably outlast the 45
            // minutes the shipped `checks` step waits on `gh pr checks
            // --watch`, so an agent step parked behind a long background job is
            // never the thing this catches; a lane that is really dead still
            // escalates, four of these later.
            lane_quiet: Duration::from_secs(15 * 60),
            lane_child_ceiling: Duration::from_secs(3600),
            worktree_root: String::new(),
            protected_branches: Vec::new(),
            notify: String::new(),
            open_on_escalation: false,
            open: String::new(),
            max_launches: 0,
            auto_unblock: false,
            default_pipeline: "default".into(),
            auto_commit: true,
            priority: Priority::default(),
            tear_lanes_on_stop: true,
        }
    }
}

/// Tab 1½ — everything that means something only in an unattended run:
/// `spoolway dispatch --unattended`, or this table's own [`Self::enabled`]
/// left on so every run is one. Split out of [`DispatchConfig`] because every
/// other key there applies whether or not a person is watching, and these
/// three — plus the five below, still unread — do not.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UnattendedConfig {
    /// Whether this run stops for a person, or for nobody.
    ///
    /// Off, a task that cannot go on parks on `blocked`, its pane is kept, you
    /// are told, and `spoolway resume` resumes it. On, there is nobody to park
    /// in front of: every road to `blocked` instead resumes the lane that hit
    /// it — the same session, its round budgets handed back, the blocker in its
    /// prompt — which is exactly what `spoolway resume` does by hand. Nothing
    /// waits, and no task ever sits on `blocked`.
    ///
    /// Two things stop meaning anything with it on, because both exist only to
    /// hand a decision to somebody who is not there: a `loop` whose exit
    /// resolves to `blocked` — a later resume would hand the budget straight
    /// back — and the launch ceiling that parks a task whose lane keeps dying
    /// (that one backs off instead — see
    /// [`crate::dispatch::relaunch_backoff`]). What still holds is every check
    /// that catches a lane going wrong rather than a person being needed: the
    /// reminder loop, and a command step's own `timeout:`.
    ///
    /// A step's `gate:` still holds too, and is not one of the things this
    /// lifts. A gate is a person's decision by design, and a run with nobody
    /// in it is not a reason to take that decision unattended — it is a reason
    /// to wait longer for the person who will.
    ///
    /// **The brake is `max_output_tokens`, and on an ungated pipeline it is
    /// the only one.** With no block able to park a task, a pipeline that
    /// cannot converge will spend until something says stop, and in an
    /// unattended run nobody is watching to say it.
    ///
    /// Off by default, and a per-run decision as much as a per-project one:
    /// `spoolway dispatch --unattended` is the overnight run, `--attended` the
    /// one you sit with, without either editing this file.
    pub enabled: bool,

    /// Output tokens one unattended run may spend before the dispatcher stops
    /// starting work. `0` — the default — is no ceiling at all.
    ///
    /// Read only when `enabled` is on. An attended run has a person at a
    /// keyboard and `ctrl-c` is their brake; this is the brake for the run that
    /// has neither, and the failure it catches is the one autonomy creates —
    /// two agents that will not converge, looping until morning.
    ///
    /// Output alone, of the four token classes, for the reason the board's own
    /// footer counts it: it tracks work done rather than context carried. A
    /// lane re-reading the same repo on every pass moves `cache_read` and
    /// almost nothing else, so a ceiling on total tokens is mostly a ceiling on
    /// how big the repo is. `max_cost_usd`, below, is the better meter where a
    /// figure in money means more than one in tokens.
    ///
    /// Counted over the run, from the moment the dispatcher took its lock —
    /// the same window the board's footer totals — and across every lane
    /// together.
    ///
    /// **It drains rather than kills.** Reaching it starts no further lane;
    /// whatever is live finishes, and the dispatcher stops once nothing is. The
    /// tasks stay exactly where they are for the next run — parking them on
    /// `blocked` would put back the one thing an unattended run is defined by
    /// not having.
    pub max_output_tokens: u64,

    /// Dollars one unattended run may spend before the dispatcher stops
    /// starting work. `0.0` — the default — is no ceiling at all, and nothing
    /// about an existing config changes by this shipping off.
    ///
    /// `max_output_tokens`'s own counterpart in money rather than tokens, read
    /// the same way and under the same two conditions: only when `enabled` is
    /// on, and only against lines a lane actually banked. Once thought
    /// impossible to meter honestly — `[models]` used to ship empty, so a
    /// priced ceiling would silently never fire on an install that never
    /// filled the table in — until `assets/model-prices.json` started
    /// vendoring the litellm price list into the binary itself: every ledger
    /// line already prices itself off that table when `[models]` says
    /// nothing, so this ceiling reads figures that were already being
    /// computed rather than asking for a new one.
    ///
    /// A model the table has never heard of, and that `[models]` does not
    /// price either, contributes nothing to the sum — never estimated, the
    /// same rule every other reader of `Entry::cost_usd` follows. Set this
    /// alongside `max_output_tokens` and whichever ceiling is reached first
    /// stops the run; either alone is enough on its own.
    ///
    /// **Drains rather than kills**, exactly as `max_output_tokens` does: see
    /// its own doc for what that means.
    pub max_cost_usd: f64,

    /// Whether the `blocked` step's pass counts as the blocked step's own pass.
    ///
    /// The unblocker is told to *do the blocked step's work* — write the code,
    /// fix the check, make the call. If it did, then handing the task back to
    /// that step re-runs work that is already finished, and re-running an agent
    /// step means paying for it a second time. So a pass here takes the task to
    /// wherever the blocked step's `on_pass` pointed, one step past where it
    /// stopped.
    ///
    /// **This includes command steps, deliberately.** A task blocked on `test`
    /// resumes at whatever `test` passes to, without `test` running again — so
    /// the unblocker's word that the build is green is taken on trust and
    /// nothing re-checks it. That is the trade this key names: skipping the
    /// re-run is the saving, and an unverified claim reaching the next step is
    /// what it costs. Set it `false` where the claim matters more than the lap.
    ///
    /// Set `false` to hand the task back to the step it blocked on instead,
    /// which is what spoolway did before this key existed.
    ///
    /// Two things are unaffected either way. A task with no recorded origin
    /// still has nowhere forward to go, so it is not carried anywhere — see
    /// [`crate::commands::resume_target`]. And a blocked step whose origin
    /// declares no `on_pass` hands back, because there is no next step to name.
    pub skip_blocked_lane: bool,

    /// Which agent profile staffs `blocked` in an unattended run.
    ///
    /// `Pipelines::assemble` materialises a `blocked` step from this and the
    /// four keys below onto every pipeline that does not declare its own —
    /// so a project stops copying the same eight lines into every pipeline
    /// file that wants one. A pipeline may still declare `- id: blocked`
    /// itself, to override `agent`, `model`, `effort`, `session` or
    /// `prompt` — see [`crate::pipeline::Pipelines::assemble`]. Every other
    /// key on that step is refused by name.
    pub blocked_agent: String,

    /// Which model that profile runs, staffing `blocked`. See
    /// [`Self::blocked_agent`].
    ///
    /// Blank is allowed here — a blank config still has to load, so
    /// `spoolway config set` can be used to fix it — but blank and
    /// `unattended.enabled = true` together refuse to start a run: an
    /// unattended run with nothing to clear a block is a run that cannot
    /// finish once one lands. See `spoolway dispatch` and `spoolway doctor`.
    pub blocked_model: String,

    /// How hard that model thinks, staffing `blocked`. See
    /// [`Self::blocked_agent`].
    pub blocked_effort: String,

    /// Whether the lane staffing `blocked` carries its own earlier session
    /// forward, the same as a step's `session:`. See
    /// [`Self::blocked_agent`].
    pub blocked_session: bool,

    /// Prompt the lane staffing `blocked` runs. See
    /// [`Self::blocked_agent`].
    pub blocked_prompt: String,
}

impl Default for UnattendedConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_output_tokens: 0,
            max_cost_usd: 0.0,
            // The unblocker is asked to do the blocked step's work, so the
            // default takes it at its word and carries on from there.
            skip_blocked_lane: true,
            // What every shipped pipeline's own `blocked` step used to spell
            // out by hand, before this table replaced it.
            blocked_agent: "claude".into(),
            blocked_model: "claude-opus-5".into(),
            blocked_effort: String::new(),
            blocked_session: true,
            blocked_prompt: "unblocker".into(),
        }
    }
}

/// Where spoolway runs its lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// A multiplexer, where every agent runs in a real pane and any lane can be
    /// watched, attached to, or taken over by hand.
    #[default]
    Herdr,

    /// No multiplexer at all: each turn is its own detached process, logging to
    /// a file, and the conversation is carried across turns by the session id
    /// the profile already pins with `{session_id}`.
    ///
    /// Every scheduling decision is identical — the dispatcher reconciles from
    /// the task file's `stage:` and the live lane list either way. What changes
    /// is what a *person* can do: there is no pane to attach to, so watching a
    /// lane means reading its log and answering one means resuming its session.
    /// The dispatch run's own board narrates the run either way.
    Headless,

    /// tmux, driven through its CLI against the default server. Sessions,
    /// windows and panes stand where herdr's workspaces, tabs and panes do,
    /// and every lane is likewise a real pane a person can attach to.
    Tmux,
}

/// How a run is laid out in its multiplexer: one shared group for every run
/// of every project, or one group per task. One vocabulary for both
/// multiplexers — `herdr_mode` and `tmux_mode` each hold one of these —
/// because the question is the same whichever is answering it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MuxMode {
    /// One workspace (tmux: session) shared by every project that dispatches
    /// on this machine, named `spoolway-dispatcher` and holding no checkout
    /// of its own — see [`crate::mux::dispatch_workspace_label`]. Each
    /// project gets one tab (tmux: window) of its own inside it, carrying a
    /// placeholder pane in the project root plus one pane per running task —
    /// no tab per task any more. Spoolway cuts every task's worktree itself,
    /// with git, which also means nothing a task owns ever appears in the
    /// sidebar.
    ///
    /// The old spelling `workspace` still parses; the next save rewrites it.
    #[default]
    #[serde(alias = "workspace")]
    Grouped,

    /// No group for the run at all: every task is a top-level group of its
    /// own, rows named `spoolway/<task>` — under herdr a workspace, under
    /// tmux a session — and the dispatcher draws where it was started.
    ///
    /// The layout to pick when a row per task is what you want to look at.
    ///
    /// The old spellings `worktrees` and `worktree` both still parse; the
    /// next save rewrites either to this one.
    #[serde(alias = "worktrees", alias = "worktree")]
    Split,
}

/// Whether a free slot is filled from every ready task, or from the group
/// already landing first.
///
/// `group` is the shipped default. It sorts a candidate whose own group is
/// already open ahead of one whose group has never run, and stops there: a
/// slot the open group has no work for goes to the next group in the sort,
/// never idle. `any` drops that tier, so every ready candidate is weighed on
/// steps left, `group_open` and `dependents` alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Priority {
    #[default]
    Group,
    Any,
}

impl Priority {
    /// Skips a `group` on the way out — see [`DispatchConfig::priority`] for
    /// why a key that holds its default is better off absent from the file.
    fn is_group(&self) -> bool {
        matches!(self, Priority::Group)
    }
}

/// Tab 2 — one named agent a pipeline step can be pointed at.
///
/// The argv a lane is started with is fixed per `kind` — [`crate::agent::
/// ADAPTERS`]'s `args` row — rather than spelled out here: those flags are
/// spoolway talking to a specific CLI about paths and ids it alone computes,
/// not a preference a project has reason to hand-tune, and a provider that
/// changes its flags needs a spoolway update regardless of where the old
/// spelling was written down. Pointing a step at a different CLI is still a
/// config edit — it is what `kind` is for.
///
/// The template substitutes:
///
/// - `{model}`        resolved model for this step
/// - `{prompt_file}` absolute path to the step's prompt
/// - `{task_file}`    absolute path to the canonical task file
/// - `{worktree}`     absolute path to the task's worktree
/// - `{repo}`         absolute path to the project root
/// - `{state_dir}`    absolute path to the project's `.spoolway/` — the
///   prompts a lane reads from outside its worktree
/// - `{project_home}` absolute path to the project's own home (see
///   [`crate::repo::Repo::home`]) — the task file a lane reads from outside
///   its worktree
/// - `{session_id}`   session id spoolway minted for this lane, so that the
///   transcript the agent writes can be found again and its token usage
///   recorded. Drop it and the lane still runs — it just spends unaccounted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentProfile {
    /// Agent kind the multiplexer knows how to start: `pi`, `claude`, `codex`,
    /// `gemini`, and so on.
    pub kind: String,

    /// Retired: this profile's own argv template. Which flags a kind is
    /// started with is fixed in `agent::ADAPTERS` now — see the struct doc —
    /// because every flag here was spoolway addressing its own CLI, computed
    /// values and all, not a project's to tune. Kept only so an existing
    /// config still parses; dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    args: Vec<String>,

    /// Retired: the model this profile ran. A step names its own model now —
    /// see [`crate::pipeline::Step::model`] — because one profile running
    /// several steps could only ever name one. Kept only so an existing
    /// config still parses; dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    model: String,

    /// Retired: whether this profile's lanes were confined by the kernel, and
    /// whether they loaded their kind's own network-egress extension. Both
    /// belonged to the sandbox, which is gone — see [`LegacySandbox`]. Kept
    /// only so an existing config still parses; dropped unconditionally on the
    /// next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    sandbox: bool,
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    sandbox_extension: bool,

    /// Most lanes of this profile running at once. Zero means unlimited.
    ///
    /// A cap on the *harness*, which is why only `claude` ships with one: how
    /// many `claude` lanes may be in flight is a fact about an account and its
    /// rate limits, and the profile is the only thing that knows it.
    ///
    /// It is the wrong axis for a local profile, and the shipped local
    /// profiles leave it at zero for that reason. `pi` and `codex` are
    /// harnesses in front of one server, and what that server can serve at
    /// once is a property of the weights it currently holds — swap a 3-slot
    /// model for a 1-slot one and the real limit changes while this number
    /// does not. `models."<glob>".slots` is where that belongs, and it
    /// replaces this for any step naming a model that sets it.
    ///
    /// Consequence worth stating plainly: a local step whose model sets no
    /// `slots` is capped by nothing at all. `spoolway doctor` says so.
    ///
    /// **Absent rather than `0`.** Every other key is written out even at its
    /// default, so that nothing hides until it is set — but a `0` here does not
    /// read as a default, it reads as a cap of none, which is a thing somebody
    /// might have meant. The key's absence says what is true: this profile
    /// asserts no cap. `spoolway config set agents.<profile>.concurrency <n>`
    /// creates it on first write, the way a `[models]` glob is created — see
    /// [`crate::confkv::set`] — so leaving it out costs nothing but the line.
    /// `default` on the field and not just on the struct: the container's
    /// `default` fills a missing field from [`AgentProfile::default`], and a
    /// key that is *omitted when zero* must come back as zero rather than as
    /// whatever that profile would otherwise have carried. The two disagreeing
    /// is how "this profile asserts no cap" reloads as a cap of one —
    /// [`Config::agrees_with`] refuses the rewrite that would do it.
    #[serde(default, skip_serializing_if = "unlimited")]
    pub concurrency: usize,

    /// Retired: what one session of this profile got to work in, in tokens.
    /// A window is a fact about a *model*, not about a launch profile, and
    /// now lives at `models."<glob>".context_window` beside that model's
    /// rates. Kept only so an existing config still parses; dropped
    /// unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    context_window: usize,

    /// How large an earlier session may be, as a percentage of the model's
    /// context window, before a step with `session: true` opens a fresh one
    /// instead of carrying it over — see
    /// [`crate::pipeline::Step::session`]. Checked against the size of the
    /// last turn alone, read straight from the transcript against the
    /// model's `context_window` in `[models]`: a ledger entry's own token
    /// count is a running sum across every turn the session has ever spent,
    /// and a sum only grows, so it says what the session has cost rather
    /// than how large it now is. 1..=100.
    pub session_reuse_ctx: u8,

    /// How large a *running* lane's last completed turn may get, as a
    /// percentage of the model's context window, before the dispatcher stops
    /// the lane outright rather than let it carry on. `0` — the default — is
    /// off: nothing watches a live lane's size at all. Checked the same
    /// reading `session_reuse_ctx` is — the last turn's own usage against
    /// `context_window` in `[models]` — but on every dispatch pass rather
    /// than only at launch, and against a session that may still be mid-turn
    /// rather than one already settled.
    ///
    /// A person may turn the agent's own compaction off and let this be the
    /// brake instead: a lane stopped here lands its task on `blocked` rather
    /// than compacting away the `spoolway report` contract, or degrading
    /// quietly as its window fills.
    ///
    /// **The reading exists only at turn boundaries, so the ceiling can be
    /// overshot.** A lane at 79% can end its next turn well past 100% before
    /// the next pass ever looks — this is a property of when the number is
    /// taken, not a bug to chase.
    ///
    /// Must be set above `session_reuse_ctx` if it is set at all: a task
    /// blocked at or below the reuse threshold would have the very session
    /// that just blocked it carried right back in on the next visit, over
    /// the size that tripped the ceiling, and block again immediately.
    /// `confkv::set` refuses either edit that would put the two the wrong
    /// way round.
    pub session_blocked_ctx: u8,

    /// The ceiling on this profile's own kind's cached usage percentage,
    /// checked before a pass starts a new lane of it — see
    /// [`crate::agent::Adapter::quota`] and `Dispatcher::quota_over_ceiling`
    /// in `dispatch.rs`. `0`, the default, is off: nothing is read and no
    /// candidate is ever parked for it.
    ///
    /// At or above this, in either window, a pass starts no new lane of this
    /// profile and writes `parked_until:` on every candidate task instead,
    /// set from the probe's own `resets_at` — the task file carries the
    /// park, not the dispatcher, so it survives a restart.
    ///
    /// An enabled ceiling holds new launches when the reading is missing,
    /// malformed, stale, or expired. Rechecks back off independently of
    /// launch attempts; `spoolway agent verify` diagnoses the source.
    /// 1..=100, or `0` for off.
    pub quota_ceiling: u8,

    /// Retired: whether a carried session was still worth resuming once its
    /// prompt cache had gone cold. Two settings governed one decision, and
    /// the second was inert unless some model declared a lifetime — that
    /// lifetime is now the whole of it, as `models.<glob>.session_reuse_idle`:
    /// unset already says what this saying `true` used to. Kept only so an
    /// existing config still parses; dropped unconditionally on the next
    /// save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    session_reuse_uncached: bool,

    /// Retired: extra environment exported into the lane's pane before it
    /// starts. The only lever that let two profiles of one kind differ, but
    /// every kind it fronted already has a config file of its own for that —
    /// and all four shipped tables have been empty since the day they were
    /// written. Kept only so an existing config still parses; dropped
    /// unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    env: BTreeMap<String, String>,

    /// Whether this profile's lanes stop and ask a person about a tool call,
    /// named as a mode rather than spelled as a flag. Which flag carries it, and
    /// which modes exist, is the kind's row in [`crate::agent::ADAPTERS`].
    ///
    /// Holds the mode a lane is actually started with, on a kind that has any
    /// — [`Config::load`]'s `migrate` fills a blank with the kind's own
    /// default the moment the file is read, so nothing here is ever hollow.
    /// Set it when your provider needs something else: some organisations
    /// disable `bypassPermissions` by policy, and others want exactly it.
    ///
    /// Refused by `spoolway doctor` on a kind with no such notion, and on a mode
    /// the kind does not accept — see [`crate::agent::Permissions`].
    ///
    /// **Absent on a kind with no permission modes at all.** `pi` has none,
    /// so there is nothing here for the file to say — `skip_serializing_if`
    /// drops the key rather than writing an empty string nobody could tell
    /// apart from "unset".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub permission_mode: String,
}

impl Default for AgentProfile {
    fn default() -> Self {
        Self {
            kind: "pi".into(),
            args: Vec::new(),
            model: String::new(),
            sandbox: false,
            sandbox_extension: false,
            // Zero, matching the field's own serde default, so a profile that
            // omits the key and a profile built from this agree about what it
            // means. Asserting a cap of one here was arbitrary anyway.
            concurrency: 0,
            context_window: 0,
            session_reuse_ctx: 50,
            session_blocked_ctx: 0,
            quota_ceiling: 0,
            session_reuse_uncached: false,
            env: BTreeMap::new(),
            permission_mode: String::new(),
        }
    }
}

impl AgentProfile {
    /// The profiles a scaffolded project starts with: `pi` and `claude`,
    /// which the built-in pipelines reference, plus `codex`, which is there
    /// so pointing a step at it is an edit to a step's `agent:` rather than a
    /// profile somebody has to write first.
    ///
    /// Two of the three run against a local server, and neither of those two
    /// carries a `concurrency` — see the field's own doc for why that number
    /// belongs to the model rather than to the harness in front of it. The
    /// other is a cloud kind with no local option, and carries a cap of its
    /// own.
    pub fn defaults() -> BTreeMap<String, AgentProfile> {
        let pi = AgentProfile {
            kind: "pi".into(),
            args: Vec::new(),
            model: String::new(),
            sandbox: false,
            sandbox_extension: false,
            // Unset: how many lanes one local model may run at once is that
            // model's `slots`, not this profile's business.
            concurrency: 0,
            context_window: 0,
            // How *large* a carried session may get is not a local-or-hosted
            // question, and is the same 50% for every profile.
            session_reuse_ctx: 50,
            // Off, like every shipped profile: nothing watches a live
            // lane's size until a person turns this on.
            session_blocked_ctx: 0,
            quota_ceiling: 0,
            session_reuse_uncached: false,
            env: BTreeMap::new(),
            // pi has no such mode; its row in `agent::ADAPTERS` says so.
            permission_mode: String::new(),
        };

        let claude = AgentProfile {
            kind: "claude".into(),
            args: Vec::new(),
            model: String::new(),
            sandbox: false,
            sandbox_extension: false,
            concurrency: 1,
            context_window: 0,
            session_reuse_ctx: 50,
            session_blocked_ctx: 0,
            quota_ceiling: 0,
            session_reuse_uncached: false,
            env: BTreeMap::new(),
            // `auto`, claude's own first mode: the strongest one that still
            // lets a lane finish alone. Written down rather than left blank,
            // so the file says the mode that actually goes out on the command
            // line.
            permission_mode: "auto".into(),
        };

        // Runs against the same local server `pi` does — the row in
        // `agent::ADAPTERS` was settled by pointing codex at a local
        // OpenAI-compatible endpoint — so it takes the local profile's
        // absent `concurrency`.
        let codex = AgentProfile {
            kind: "codex".into(),
            args: Vec::new(),
            model: String::new(),
            sandbox: false,
            sandbox_extension: false,
            concurrency: 0,
            context_window: 0,
            session_reuse_ctx: 50,
            session_blocked_ctx: 0,
            quota_ceiling: 0,
            session_reuse_uncached: false,
            env: BTreeMap::new(),
            // `never`, codex's own first mode: the one approval setting that
            // lets a lane finish with nobody there. Written down rather than
            // left blank, so the file says the mode that actually goes out
            // on the command line.
            permission_mode: "never".into(),
        };

        BTreeMap::from([
            ("pi".into(), pi),
            ("claude".into(), claude),
            ("codex".into(), codex),
        ])
    }

    /// A profile that is nothing but a kind, for asking what that kind's args
    /// render to.
    ///
    /// `spoolway agent verify` needs a profile to render through, and has no
    /// project to take one from: the kind it is asked about is usually the one
    /// no profile names yet. Every other field is the default, which is what
    /// makes the render it checks the render a fresh profile would get.
    pub fn for_kind(kind: &str) -> Self {
        Self {
            kind: kind.to_string(),
            ..Self::default()
        }
    }

    /// Substitute placeholders into this kind's argv template — see
    /// `agent::Adapter::args` for where that template comes from and what
    /// each placeholder carries.
    pub fn render_args(&self, values: &BTreeMap<&str, String>) -> Result<Vec<String>> {
        let Some(adapter) = crate::agent::adapter(&self.kind) else {
            bail!(
                "spoolway does not know agent kind `{}` — see `agent::ADAPTERS`",
                self.kind
            );
        };
        if adapter.args.is_empty() {
            bail!(
                "spoolway does not yet know how to launch kind `{}` — its row in \
                 `agent::ADAPTERS` has no `args`",
                self.kind
            );
        }

        let mut rendered = Vec::with_capacity(adapter.args.len());
        for arg in adapter.args {
            rendered.push(substitute(arg, values, "args")?);
        }

        rendered.extend(self.permission_args());
        Ok(rendered)
    }

    /// How this profile's `permission_mode` reads, or why it is wrong.
    ///
    /// `Ok(None)` for a kind with no such notion and nothing set. A blank on a
    /// kind that does have modes is refused rather than resolved to a
    /// default: `Config::load`'s `migrate` fills that blank the moment a file
    /// is read, so the only way this method ever sees one is a person typing
    /// it directly at `spoolway config set` — a config that refuses to load
    /// cannot be fixed by the command that fixes configs, so the refusal has
    /// to live here instead. The check as a whole has to live here rather
    /// than in serde because it is about two fields at once — a mode means
    /// nothing without the `kind` that defines it — and both `spoolway
    /// config set` and `spoolway doctor` need the same answer.
    pub fn permission_mode_status(&self) -> Result<Option<String>> {
        let named = self.permission_mode.trim();
        let row = crate::agent::adapter(&self.kind).and_then(|a| a.permissions.as_ref());
        match (named, row) {
            ("", None) => Ok(None),
            ("", Some(permissions)) => bail!(
                "`permission_mode` is blank, and `{}` has modes — pick one of: {} (a loaded \
                 config never holds a blank here; this can only be set by hand)",
                self.kind,
                permissions.modes.join(", ")
            ),
            (mode, None) => bail!(
                "`{}` has no permission modes, so `{mode}` means nothing to it",
                self.kind
            ),
            (mode, Some(permissions)) if permissions.accepts(mode) => Ok(Some(mode.to_string())),
            (mode, Some(permissions)) => bail!(
                "`{mode}` is not a mode `{}` accepts — pick one of: {}",
                self.kind,
                permissions.modes.join(", ")
            ),
        }
    }

    /// The permission-mode flag this profile's lanes are started with, if its
    /// kind has one.
    ///
    /// Appended here rather than baked into the kind's argv template so that a
    /// project sets a *mode* and never a flag spelling, and so that the
    /// dispatcher gets it without remembering to.
    fn permission_args(&self) -> Vec<String> {
        let Some(permissions) =
            crate::agent::adapter(&self.kind).and_then(|a| a.permissions.as_ref())
        else {
            return Vec::new();
        };

        let mode = match self.permission_mode.trim() {
            "" => permissions.default_mode(),
            named => named,
        };
        vec![permissions.flag.to_string(), mode.to_string()]
    }

    /// The effort flag a lane of this profile is started with, if the step
    /// asks for one and this kind carries a flag to put it on.
    ///
    /// A step's setting, not the profile's — the same profile can run one
    /// step at `effort: high` and the next at none at all, which is exactly
    /// what naming the model per step already lets it do. Silently nothing
    /// on a kind with no such flag, the same way an unset `permission_mode`
    /// is: `pipeline check` is where that mismatch is refused, not here.
    pub fn effort_args(&self, effort: Option<&str>) -> Vec<String> {
        let Some(effort) = effort else {
            return Vec::new();
        };
        let Some(row) = crate::agent::adapter(&self.kind).and_then(|a| a.effort.as_ref()) else {
            return Vec::new();
        };
        row.args
            .iter()
            .map(|arg| arg.replace("{effort}", effort))
            .collect()
    }
}

/// Whether a `concurrency` is the absent one. Taken by reference and named for
/// what it means rather than for the number, because it is a serde predicate
/// and reads at the field it guards.
fn unlimited(concurrency: &usize) -> bool {
    *concurrency == 0
}

/// One template of an agent row with its placeholders filled in.
///
/// `what` names the half being rendered, so an error says which row to go and
/// look at rather than only that something did not fill in.
fn substitute(template: &str, values: &BTreeMap<&str, String>, what: &str) -> Result<String> {
    let mut out = template.to_string();
    for (key, value) in values {
        out = out.replace(&format!("{{{key}}}"), value);
    }
    if let Some(unknown) = leftover_placeholder(&out) {
        bail!(
            "unknown placeholder `{unknown}` in this agent's {what} (known: {})",
            values.keys().copied().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(out)
}

/// The first `{placeholder}` a render left behind, if it left one.
///
/// A placeholder is a brace, a bare name — lowercase, digits, underscores — and
/// a closing brace. Nothing looser: an earlier row's template embedded a whole
/// JSON document, and a check that called any brace a placeholder would have
/// refused the one row that needed them. Tight is still enough for the
/// failure this exists to catch, which is a row naming a value no lane start
/// supplies — unchecked, that reaches the binary as a literal `{state_dir}`
/// in its argv.
fn leftover_placeholder(rendered: &str) -> Option<String> {
    let mut chars = rendered.char_indices();
    while let Some((start, ch)) = chars.next() {
        if ch != '{' {
            continue;
        }
        let mut name = String::new();
        for (at, ch) in chars.by_ref() {
            match ch {
                'a'..='z' | '0'..='9' | '_' => name.push(ch),
                '}' if !name.is_empty() => return Some(rendered[start..=at].to_string()),
                _ => break,
            }
        }
    }
    None
}

/// An old `[effort]` table: `tier_models` picked a model per named tier, and
/// `sensitive_paths` decided what a step's `effort: auto` meant.
///
/// Both are gone. A step's `effort:` is a free string now, handed straight
/// through to the flag its agent kind carries one on — no tier, no model
/// swap, and so no machine-level setting for either to consult. This exists
/// only so a config written before that still parses; it is never read, and
/// dropped on the next save.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct LegacyEffort {
    tier_models: BTreeMap<String, String>,
    sensitive_paths: Vec<String>,
}

/// An old `[sandbox]` table: the kernel confinement every local lane ran
/// under, and the paths, ports and domains that widened it.
///
/// The whole layer is gone — Landlock, the shims, the guardrails artifacts and
/// this table with them — so there is nothing left for any of these keys to
/// configure, and nothing inside spoolway replaces it: confinement is the
/// person's own agent settings now, outside this repository. Deserialised as
/// a free map because the point is only that an existing config still opens:
/// it is never read, and dropped on the next save.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
struct LegacySandbox(#[allow(dead_code)] BTreeMap<String, toml::Value>);

/// An old `[paths]` table. Every directory it named is a constant now —
/// `QUEUE_DIR` and friends, beside [`STATE_DIR`] — because a project never
/// chooses where its own state lives, and every caller already goes through
/// the accessors on [`crate::repo::Repo`]. `docs` used to be the exception,
/// carried across onto a `[docs]` table this binary also no longer keeps —
/// see [`LegacyDocs`] — so nothing here is read again either way. Dropped on
/// the next save.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
struct LegacyPaths(#[allow(dead_code)] BTreeMap<String, toml::Value>);

/// An old `[plans]` table: where a plan page was written, and how.
///
/// The binary kept an opinion about that once — a store to write into, a
/// shared skeleton, a stylesheet and two lockups every page pointed at by
/// relative path. None of it is spoolway's business any more: spoolway-plan
/// writes one self-contained page wherever it is told to, and reads no
/// config to decide it. Deserialised as a free map for the same reason
/// [`LegacySandbox`] is — an existing config still opens, `store`, `format`
/// and `template` all included; nothing here is read, and the table is
/// dropped on the next save.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
struct LegacyPlans(#[allow(dead_code)] BTreeMap<String, toml::Value>);

/// An old `[docs]` table: what the archivist wrote a domain document as, and
/// where.
///
/// Retired the way `[plans]` was: `src/docs.rs` is gone, and spoolway keeps no
/// notion of documentation any more — where documents live, and what each
/// covers, is `assets/prompts/archivist/PROMPT.md`'s to say, and a project
/// whose layout was never the default gets a note rather than a migration —
/// see [`Config::load`]. Named fields rather than an opaque map, unlike most
/// of its retired siblings, because [`Config::load`]'s note only fires when a
/// value here was ever set away from the default: an untouched `[docs]`
/// table, which every config written before this carries, says nothing worth
/// printing.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
struct LegacyDocs {
    path: PathBuf,
    format: LegacyDocFormat,
}

impl Default for LegacyDocs {
    fn default() -> Self {
        Self {
            path: PathBuf::from("docs"),
            format: LegacyDocFormat::default(),
        }
    }
}

/// The shape an old `[docs]` table's `format` held. Never read for anything
/// but the comparison [`LegacyDocs`]'s doc explains.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LegacyDocFormat {
    #[default]
    Markdown,
    Custom,
}

/// A `[criteria]` table left over from before review standards moved into the
/// reviewer prompt.
///
/// Kept only because [`Config`] denies unknown fields: without somewhere for it
/// to land, an existing project's config would stop parsing on the next command
/// rather than telling anyone what changed. It is never written back — the
/// `skip_serializing_if` on the field is what makes the next `spoolway config
/// set` drop it — and [`Config::load`] says so once when it finds one.
pub type LegacyCriteria = BTreeMap<String, Vec<String>>;

impl Config {
    /// `.spoolway/config.toml` under `root`.
    ///
    /// `root` here is whatever directory a caller hands it — every reading
    /// call site hands `repo.checkout` now, so a command answers about the
    /// file actually in front of it. `config set`'s writing path is the one
    /// exception: it still hands `repo.root`, and refuses first if a linked
    /// worktree's `checkout` differs from it — see `commands::config_set`.
    pub fn path_in(root: &Path) -> PathBuf {
        root.join(STATE_DIR).join(CONFIG_FILE)
    }

    /// Load config from a directory holding `.spoolway/`, falling back to
    /// defaults if absent. See [`Self::path_in`] for which directory a caller
    /// should hand it.
    pub fn load(root: &Path) -> Result<Config> {
        let path = Config::path_in(root);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let mut config: Config =
                    toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
                // Said here rather than in `doctor`, because the standards stop
                // reaching the reviewer the moment this file is the only place
                // they are written down, and nothing else would ever mention it.
                if !config.criteria.is_empty() {
                    eprintln!(
                        "note: [criteria] in {} is no longer read — a project's review standards \
                         live at the bottom of {PROMPTS_DIR}/reviewer/{}, where they can be \
                         edited as prose. Move them across and delete the table.",
                        path.display(),
                        crate::assets::PROMPT_FILE,
                    );
                }
                // Only where a value was somebody's decision: an untouched
                // `[docs]` table — every config written before this carries
                // one — says nothing worth a note.
                if config.docs != LegacyDocs::default() {
                    eprintln!(
                        "note: [docs] in {} is no longer read — spoolway keeps no notion of \
                         documentation. Where documents live, and what each covers, is the \
                         archivist's: {PROMPTS_DIR}/archivist/PROMPT.md. The table is dropped \
                         on the next save.",
                        path.display(),
                    );
                }
                for (name, profile) in &config.agents {
                    if !profile.env.is_empty() {
                        eprintln!(
                            "note: [agents.{name}.env] in {} is no longer read — a lane \
                             inherits the dispatcher's environment, and anything one agent \
                             needs belongs in that agent's own config. Dropped on the next \
                             save.",
                            path.display(),
                        );
                    }
                    if crate::agent::adapter(&profile.kind).is_none() {
                        eprintln!(
                            "note: [agents.{name}] in {} names kind `{}`, which spoolway no \
                             longer knows how to launch. Dropped on the next save.",
                            path.display(),
                            profile.kind,
                        );
                    }
                }
                config.migrate();
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Fill in what a loaded config must never hold blank, and clear what it
    /// must never hold at all.
    ///
    /// A blank `permission_mode` on a kind that offers modes used to mean "the
    /// kind's own default", resolved only once a lane started — so the file
    /// said nothing and a flag went out on the command line anyway. This
    /// settles it onto the struct instead, at the one place every config
    /// passes through, so `Config::load` never yields a profile whose
    /// approval mode the file cannot show. The other direction matters just
    /// as much: a profile switched onto a kind with no modes at all — `pi` —
    /// carries a mode left over from whatever kind it used to be, which now
    /// means nothing, so it is cleared the same way a fresh profile of that
    /// kind never has one. Split out from [`Config::load`]
    /// because anything that parses the file has to reach the same config the
    /// rest of spoolway is holding, and [`Config::save_key`] parses it back to
    /// check its own work.
    ///
    /// A profile naming a kind `agent::ADAPTERS` no longer carries a row for
    /// — one that was retired after a project's config was written — is
    /// dropped outright, the same as any other retired setting: it parses on
    /// the way in, `Config::load` says so, and it is simply absent on the way
    /// back out.
    pub(crate) fn migrate(&mut self) {
        self.agents
            .retain(|_, profile| crate::agent::adapter(&profile.kind).is_some());
        for profile in self.agents.values_mut() {
            let permissions =
                crate::agent::adapter(&profile.kind).and_then(|a| a.permissions.as_ref());
            match permissions {
                Some(permissions) if profile.permission_mode.trim().is_empty() => {
                    profile.permission_mode = permissions.default_mode().to_string();
                }
                None => profile.permission_mode.clear(),
                Some(_) => {}
            }
        }
    }

    /// The config as it would be written from nothing: one reference table on
    /// top, naming every key, its possible values, its default and one
    /// sentence about it, followed by the serialised struct with no comment
    /// above any key.
    ///
    /// What `init` writes, what a missing file is restored from, and — because
    /// `self` is by then the config loaded from the very file being replaced —
    /// what `spoolway update` rewrites an existing one to. Rendering keeps a
    /// struct's values and nothing else, which is exactly the config contract:
    /// what a setting is set to is the project's, and the table above it is
    /// spoolway's. There used to be a comment standing above each key instead
    /// — one register of prose, repeated at every key it explained. A table
    /// says the same things once, in the shape a person scanning the whole
    /// surface actually wants, and [`crate::confkv::REFERENCE`] is the one
    /// place that prose is written now: this table and the settings screen
    /// both render from it.
    ///
    /// It is **not** how one setting is written. `config set` goes through
    /// [`Config::save_key`], which edits the document in place, because a person
    /// changing an interval has not asked for their file to be rewritten around
    /// them. See [`crate::confdoc`].
    pub fn render(&self) -> Result<String> {
        let rendered = toml::to_string_pretty(self).context("serialising config")?;
        Ok(format!("{}{rendered}", crate::confkv::reference_table()))
    }

    /// Write the file from scratch, discarding whatever was there.
    ///
    /// For `init`, and for a file that does not exist yet. Anything editing a
    /// config a person already has wants [`Config::save_key`].
    pub fn save(&self, root: &Path) -> Result<()> {
        let path = Config::path_in(root);
        crate::task::write_atomic(&path, &self.render()?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Write one key into the file on disk, touching nothing else.
    ///
    /// `self` is the config as it should end up — already validated by
    /// [`crate::confkv::set`], which is the only thing that knows whether a
    /// typed value means anything. What happens here is the narrowest edit that
    /// achieves it: the one key's value is replaced in the document, or the key
    /// is added with its note if the file never had it, and every other byte —
    /// every comment, every blank line, the order of everything — is copied
    /// through unread.
    ///
    /// The result is parsed back and compared against `self` before it lands,
    /// so a document edit that somehow did not mean what the struct means is a
    /// refusal rather than a config quietly holding the wrong thing.
    pub fn save_key(&self, root: &Path, key: &str) -> Result<()> {
        let path = Config::path_in(root);
        let Ok(text) = std::fs::read_to_string(&path) else {
            // No file to preserve, so there is nothing to be careful about.
            return self.save(root);
        };

        let parts = crate::confkv::parts(self, key);
        let value = toml::Value::try_from(self).context("serialising config")?;

        // Absent from the serialised config means this key was just set to the
        // value whose spelling is its absence — a `concurrency` of nothing, a
        // rate of zero. The document edit for that is a removal, not a write:
        // putting `= 0` back would be the file disagreeing with the struct it
        // was rendered from, and `agrees_with` below would refuse it anyway.
        let edited = match crate::confdoc::at(&value, &parts) {
            Some(new) => crate::confdoc::set(&text, &parts, new, crate::confkv::note(key))
                .with_context(|| format!("editing {}", path.display()))?,
            None => crate::confdoc::remove(&text, &parts)
                .with_context(|| format!("editing {}", path.display()))?,
        };

        self.agrees_with(&edited).with_context(|| {
            format!(
                "setting `{key}` would have changed more of {} than that one key — \
                 nothing was written",
                path.display()
            )
        })?;

        crate::task::write_atomic(&path, &edited)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Does `text`, read as a config file, mean exactly what `self` means?
    ///
    /// Every document edit checks its own work with this before anything lands.
    /// A rewritten document is only ever meant to change how the file reads —
    /// which keys are written down, and what stands above them — so a rewrite
    /// that changes what any of them is *set to* is a bug, and the file it
    /// would have produced is worth more unwritten than written.
    pub(crate) fn agrees_with(&self, text: &str) -> Result<()> {
        let mut reparsed: Config = toml::from_str(text).context("it no longer parses")?;
        reparsed.migrate();

        let before = toml::Value::try_from(self).context("serialising config")?;
        let after = toml::Value::try_from(&reparsed).context("serialising config")?;
        if before != after {
            bail!("it no longer holds the same settings");
        }
        Ok(())
    }

    /// Look up an agent profile, with an error naming the defined ones.
    pub fn agent(&self, name: &str) -> Result<&AgentProfile> {
        self.agents.get(name).with_context(|| {
            format!(
                "no agent profile `{name}` in config (defined: {})",
                self.agents.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }
}

/// One note, as the comment lines that stand above its key.
///
/// The single spelling of a note-as-comment: `config set` uses it in
/// [`crate::confdoc`] when it adds a key the file never had, so that a key
/// arriving alone still carries its one-sentence explanation. Everything
/// else about the surface — the reference table [`Config::render`] writes on
/// top of a whole file — reads the same sentence straight out of
/// [`crate::confkv::REFERENCE`] rather than through a comment at all.
pub(crate) fn comment_block(note: &str) -> String {
    let mut out = String::new();
    for chunk in wrap(note, 74) {
        out.push_str("# ");
        out.push_str(&chunk);
        out.push('\n');
    }
    out
}

/// Greedy word wrap, so a long note reads as a comment block rather than one
/// line that runs off the side of an editor.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        if !current.is_empty() && current.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        lines.push(current);
    }

    lines
}

/// Durations are written the way a person would: `10m`, `90s`, `1h30m`.
///
/// Shared with [`crate::pipeline`], so a `timeout:` in a pipeline file is
/// written the same way `dispatch.interval` in config.toml is. One spelling
/// of a duration across every file spoolway reads.
pub(crate) mod human_duration {
    use super::*;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format(*d))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let raw = String::deserialize(d)?;
        parse(&raw).map_err(serde::de::Error::custom)
    }

    /// The same spelling where the setting may simply be absent.
    ///
    /// For a fact a project states about a model rather than one spoolway has a
    /// default for — [`crate::usage::ModelPrice::session_reuse_idle`] is the
    /// one. Absent and zero have to stay distinct there: unset means "nobody
    /// has said", and a carried session under that model is never refused for
    /// its age, rather than being refused as though its store had just gone
    /// stale.
    pub mod optional {
        use super::*;

        pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
            match d {
                Some(d) => s.serialize_str(&format(*d)),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
            let Some(raw) = Option::<String>::deserialize(d)? else {
                return Ok(None);
            };
            parse(&raw).map(Some).map_err(serde::de::Error::custom)
        }
    }

    pub fn format(d: Duration) -> String {
        let mut secs = d.as_secs();
        let mut out = String::new();
        for (unit, size) in [("d", 86_400u64), ("h", 3600), ("m", 60), ("s", 1)] {
            if secs >= size {
                out.push_str(&std::format!("{}{unit}", secs / size));
                secs %= size;
            }
        }
        if out.is_empty() {
            out.push_str("0s");
        }
        out
    }

    pub fn parse(raw: &str) -> std::result::Result<Duration, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("empty duration".into());
        }

        let mut total = 0u64;
        let mut digits = String::new();
        let mut saw_unit = false;

        for ch in raw.chars() {
            if ch.is_ascii_digit() {
                digits.push(ch);
                continue;
            }
            let value: u64 = digits
                .parse()
                .map_err(|_| std::format!("`{raw}`: expected a number before `{ch}`"))?;
            let multiplier = match ch {
                's' => 1,
                'm' => 60,
                'h' => 3600,
                'd' => 86_400,
                other => {
                    return Err(std::format!(
                        "`{raw}`: unknown unit `{other}` (use s, m, h, d)"
                    ));
                }
            };
            // `checked_*`, because a `strip = true` release build wraps rather
            // than panics: `213503982334602d` would otherwise be stored as a
            // near-zero duration and set the dispatcher polling every few
            // seconds instead of being refused.
            let seconds = value
                .checked_mul(multiplier)
                .and_then(|s| total.checked_add(s))
                .ok_or_else(|| std::format!("`{raw}`: too large"))?;
            total = seconds;
            digits.clear();
            saw_unit = true;
        }

        if !digits.is_empty() {
            // A bare number means seconds.
            let value = digits
                .parse::<u64>()
                .map_err(|_| std::format!("`{raw}`: not a number"))?;
            total = total
                .checked_add(value)
                .ok_or_else(|| std::format!("`{raw}`: too large"))?;
        } else if !saw_unit {
            return Err(std::format!("`{raw}`: not a duration"));
        }

        Ok(Duration::from_secs(total))
    }
}

#[allow(unused_imports)]
pub use human_duration::{format as format_duration, parse as parse_duration};

#[cfg(test)]
mod tests {
    use super::*;

    /// The reason the rule exists: an id is joined onto a directory, so
    /// anything that can leave that directory is not a name.
    #[test]
    fn an_id_that_could_leave_its_directory_is_not_a_name() {
        for escape in ["../../pwn", "..", "a/b", "/etc/passwd", ".hidden", "-lead"] {
            assert!(
                check_id("step id", escape).is_err(),
                "`{escape}` was accepted as a name"
            );
        }
        assert!(check_id("task id", "").is_err());
        assert!(check_id("step id", "Reproduce").is_err());
        assert!(check_id("step id", "has_underscore").is_err());

        assert!(check_id("step id", "reproduce-again").is_ok());
        assert!(check_id("task id", "slug-subcommand2").is_ok());
    }

    #[test]
    fn durations_round_trip() {
        for text in ["30s", "10m", "1h", "1h30m", "2d"] {
            let parsed = parse_duration(text).unwrap();
            assert_eq!(format_duration(parsed), text, "round-tripping {text}");
        }
        assert_eq!(parse_duration("45").unwrap(), Duration::from_secs(45));
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("").is_err());
    }

    /// A value that would overflow `u64` seconds is refused, not wrapped —
    /// `strip = true` release builds wrap plain arithmetic silently, which
    /// turned `213503982334602d` into a near-zero poll interval (finding 70).
    #[test]
    fn an_overflowing_duration_is_refused() {
        // `value * multiplier` overflows.
        let err = parse_duration(&format!("{}d", u64::MAX / 86_400 + 1)).unwrap_err();
        assert!(err.contains("too large"), "{err}");

        // `total += ...` overflows across two terms.
        let err = parse_duration(&format!("{}s{}s", u64::MAX, u64::MAX)).unwrap_err();
        assert!(err.contains("too large"), "{err}");

        // A term with a trailing bare number overflows on the final add.
        let err = parse_duration(&format!("{}s{}", u64::MAX, u64::MAX)).unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[test]
    fn default_config_round_trips_through_toml() {
        let original = Config::default();
        let text = toml::to_string_pretty(&original).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();

        assert_eq!(parsed.dispatch.interval, original.dispatch.interval);
        assert_eq!(parsed.agents["claude"].kind, "claude");
        // The cloud profile carries a `concurrency`, and the two local ones
        // do not: a cap on the harness is a cloud fact, and a local model's
        // own count is `models."<glob>".slots`. Zero has to survive the round
        // trip as zero rather than being written out and read back as
        // something else.
        assert_eq!(parsed.agents["claude"].concurrency, 1);
        for name in ["pi", "codex"] {
            assert_eq!(parsed.agents[name].concurrency, 0, "{name}");
        }
        // Every model your pipelines name is already covered by the built-in
        // table once it exists; nothing here is a guess spoolway made for you.
        assert!(parsed.models.is_empty());
        assert_eq!(parsed.prices.max_age_days, 30);
    }

    #[test]
    fn written_config_explains_the_price_age_key_and_round_trips_it() {
        let mut config = Config::default();
        config.prices.max_age_days = 7;
        let rendered = config.render().unwrap();
        assert!(rendered.contains("prices.max_age_days"));
        assert!(rendered.contains("before `spoolway doctor` says so"));
        assert!(rendered.contains("[prices]\nmax_age_days = 7"));

        let parsed: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed.prices.max_age_days, 7);
    }

    /// A setting that changes nothing at runtime is exactly the one people
    /// misread, so the file says so — once, in the reference table on top,
    /// rather than as a comment repeated above every key it explains.
    #[test]
    fn a_noted_setting_is_explained_in_the_reference_table_on_top() {
        let rendered = Config::default().render().unwrap();
        let lines: Vec<&str> = rendered.lines().collect();
        let table_end = lines
            .iter()
            .position(|l| !l.starts_with('#'))
            .expect("the file is nothing but comments");
        let table = lines[..table_end].join("\n");
        let key_at = lines
            .iter()
            .position(|l| l.starts_with("permission_mode"))
            .expect("permission_mode is not in the rendered config");

        assert!(
            table.contains("permission_mode") && table.contains("Which approval mode"),
            "the reference table does not explain permission_mode:\n{table}"
        );
        // The key itself carries no comment of its own any more — every
        // explanation moved into the one table on top.
        assert!(
            !lines[key_at - 1].starts_with('#'),
            "a per-key comment is back above permission_mode"
        );

        // A comment is not a value: the file must still parse.
        let parsed: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed.agents["pi"].permission_mode, "");
    }

    #[test]
    fn every_agent_the_built_in_pipeline_names_is_defined() {
        let config = Config::default();
        let pipeline = crate::pipeline::Pipelines::builtin()
            .get("default")
            .unwrap()
            .clone();
        for (agent, steps) in pipeline.referenced_agents() {
            assert!(
                config.agents.contains_key(agent),
                "pipeline steps {steps:?} reference undefined agent profile `{agent}`"
            );
        }
    }

    #[test]
    fn args_template_substitutes_placeholders() {
        let profile = &AgentProfile::defaults()["pi"];
        let values = BTreeMap::from([
            ("model", "a-model".to_string()),
            ("prompt_file", "/p/implementer.md".to_string()),
            (
                "session_id",
                "0198e2c0-0000-4000-8000-000000000000".to_string(),
            ),
        ]);
        let args = profile.render_args(&values).unwrap();

        assert!(args.contains(&"a-model".to_string()));
        assert!(args.contains(&"/p/implementer.md".to_string()));
        assert!(args.contains(&"0198e2c0-0000-4000-8000-000000000000".to_string()));
    }

    /// Everything a lane start supplies, as `<name>`. Kept in step with the
    /// `values` map in `dispatch::start_lane`.
    fn values() -> BTreeMap<&'static str, String> {
        [
            "model",
            "session_id",
            "prompt_file",
            "task_file",
            "worktree",
            "repo",
            "state_dir",
            "project_home",
        ]
        .iter()
        .map(|key| (*key, format!("<{key}>")))
        .collect()
    }

    /// A profile that names no mode still gets its kind's default, so a lane
    /// runs unattended without anyone having written that down.
    ///
    /// The shipped `claude` profile itself no longer demonstrates the blank
    /// case — see the test below this one — so this builds one directly from
    /// [`AgentProfile::default`], the way a hand-rolled profile in someone's
    /// own config could still arrive.
    #[test]
    fn a_kind_with_modes_gets_its_default_when_the_profile_names_none() {
        let profile = AgentProfile {
            kind: "claude".into(),
            ..AgentProfile::default()
        };
        assert!(profile.permission_mode.is_empty());

        let args = profile.render_args(&values()).unwrap();
        let at = args.iter().position(|a| a == "--permission-mode").expect(
            "a claude profile must be started with a permission mode or it stops at its \
             first tool call",
        );
        assert_eq!(args[at + 1], "auto");
    }

    /// The shipped profiles are what `init` writes into a fresh
    /// `.spoolway/config.toml`, so they carry the mode a lane is actually
    /// started with rather than a blank a reader has to know resolves to one
    /// — a real mode on every kind that has one, no key at all on a kind
    /// that does not.
    #[test]
    fn a_shipped_profile_writes_down_the_mode_it_actually_passes() {
        let defaults = AgentProfile::defaults();
        assert_eq!(defaults["claude"].permission_mode, "auto");
        assert_eq!(defaults["codex"].permission_mode, "never");
        // pi has no permission modes at all.
        assert!(defaults["pi"].permission_mode.is_empty());

        let rendered = toml::to_string(&Config::default()).unwrap();
        assert!(rendered.contains("permission_mode = \"auto\""));
        assert!(rendered.contains("permission_mode = \"never\""));

        let pi_section = rendered
            .split("[agents.pi]")
            .nth(1)
            .and_then(|rest| rest.split("[agents.").next())
            .expect("an [agents.pi] section");
        assert!(
            !pi_section.contains("permission_mode"),
            "pi has no permission modes, so the key must not appear at all:\n{pi_section}"
        );
    }

    /// The mode a project names is the mode its lanes get.
    // covers: agents.<profile>.permission_mode — the mode a profile names is the flag its lanes are started with
    #[test]
    fn a_named_mode_is_what_the_lane_is_started_with() {
        let mut profile = AgentProfile::defaults()["claude"].clone();
        profile.permission_mode = "bypassPermissions".into();

        let args = profile.render_args(&values()).unwrap();
        let at = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[at + 1], "bypassPermissions");
        assert_eq!(
            args.iter().filter(|a| *a == "--permission-mode").count(),
            1,
            "the flag must appear once, or the two copies disagree"
        );
    }

    /// A kind with no such notion is handed no flag, whatever the profile says.
    ///
    /// pi is that kind: it runs its tools with nothing gating them, so there
    /// is no prompt to switch off. Passing it a claude flag would be an agent
    /// that dies at launch.
    #[test]
    fn a_kind_without_modes_is_handed_no_permission_flag() {
        let mut profile = AgentProfile::defaults()["pi"].clone();
        assert_eq!(profile.kind, "pi");
        assert!(
            !profile
                .render_args(&values())
                .unwrap()
                .iter()
                .any(|a| a == "--permission-mode")
        );

        // Even set — `doctor` is what complains; a lane still starts clean.
        profile.permission_mode = "auto".into();
        assert!(
            !profile
                .render_args(&values())
                .unwrap()
                .iter()
                .any(|a| a == "--permission-mode")
        );
    }

    /// A step's `effort:` renders into the flag its kind carries one on.
    #[test]
    fn a_kind_with_an_effort_flag_renders_a_steps_effort() {
        let cloud = AgentProfile::defaults()["claude"].clone();
        assert_eq!(cloud.kind, "claude");
        assert_eq!(cloud.effort_args(Some("high")), ["--effort", "high"]);
        assert!(cloud.effort_args(None).is_empty());
    }

    /// pi's `--thinking` is a token budget, not a named level — a different
    /// axis from a step's `effort:` — so pi carries no flag for it at all.
    #[test]
    fn a_kind_without_an_effort_flag_is_handed_no_effort_flag() {
        let local = AgentProfile::defaults()["pi"].clone();
        assert_eq!(local.kind, "pi");
        assert!(local.effort_args(Some("high")).is_empty());
    }

    /// Every mode the adapter offers is one the profile round-trips, and the
    /// default is among them.
    #[test]
    fn every_offered_mode_is_accepted_and_the_default_is_one_of_them() {
        for adapter in crate::agent::ADAPTERS {
            let Some(permissions) = &adapter.permissions else {
                continue;
            };
            assert!(
                !permissions.modes.is_empty(),
                "`{}` offers a permission row with no modes in it",
                adapter.kind
            );
            assert!(permissions.accepts(permissions.default_mode()));
            for mode in permissions.modes {
                assert!(permissions.accepts(mode));
            }
        }
    }

    /// Every placeholder a shipped profile uses is one a lane start actually
    /// fills in.
    ///
    /// The failure this catches is total and silent until runtime: a profile
    /// that names a placeholder no caller supplies renders to an error, so
    /// *every* lane of that profile fails to start — and nothing before this
    /// point looks at the two lists together. Kept in step with the `values`
    /// map in `dispatch::start_lane`.
    #[test]
    fn every_shipped_profile_only_uses_placeholders_a_lane_supplies() {
        for (name, profile) in AgentProfile::defaults() {
            let rendered = profile.render_args(&values());
            assert!(
                rendered.is_ok(),
                "profile `{name}` uses a placeholder no lane start supplies: {}",
                rendered.unwrap_err()
            );
        }
    }

    /// Every shipped profile pins its session, because a lane whose transcript
    /// cannot be found again spends tokens nothing ever accounts for.
    ///
    /// Two ways count. `pi` and `claude` take an id spoolway mints and write
    /// it into a path spoolway can predict. `codex` refuses any id but its
    /// own — so it is pinned by the *directory* it writes into instead, which
    /// `Adapter::home` moves to one per lane. A profile with neither is the
    /// failure this catches: its transcript lands somewhere nothing goes
    /// looking.
    #[test]
    fn every_shipped_profile_pins_its_session() {
        for (name, profile) in AgentProfile::defaults() {
            let adapter = crate::agent::adapter(&profile.kind).unwrap();
            assert!(
                adapter.args.contains(&"{session_id}") || adapter.home.is_some(),
                "profile `{name}` pins its session by neither mechanism, \
                 so its transcript cannot be found again"
            );
        }
    }

    /// A profile naming a kind `agent::ADAPTERS` has no `args` row for is
    /// refused rather than launched with no flags at all.
    ///
    /// `gemini` rather than `codex`: codex has a real `args` row now, and
    /// `gemini` is the row that is blank on purpose and stays that way.
    #[test]
    fn a_kind_with_no_args_template_is_refused() {
        let profile = AgentProfile {
            kind: "gemini".into(),
            ..AgentProfile::default()
        };
        let err = profile.render_args(&values()).unwrap_err();
        assert!(err.to_string().contains("gemini"));
    }

    /// Rendering a lane's args never consults the accounting half. That is the
    /// whole of what `accounting: None` costs at a lane start — nothing — and
    /// the refusal an unfillable row used to imply must not come back as one
    /// here.
    ///
    /// Asserted over the table rather than against one kind chosen as the
    /// example, because which kind is the unmetered one is not a fact this test
    /// should own: every launchable kind meters today, and the guarantee is
    /// about what `render_args` reads, not about who currently needs it.
    #[test]
    fn rendering_a_lanes_args_never_consults_the_accounting_half() {
        for adapter in crate::agent::ADAPTERS {
            let rendered = AgentProfile::for_kind(adapter.kind).render_args(&values());
            assert_eq!(
                rendered.is_ok(),
                adapter.launches(),
                "`{}` renders differently from what its `args` row declares",
                adapter.kind
            );
            // And a kind that is refused is refused for the args it does not
            // have, never for an accounting row it also does not have.
            if let Err(err) = rendered {
                let message = err.to_string();
                assert!(message.contains("args"), "{message}");
                assert!(!message.contains("account"), "{message}");
            }
        }
    }

    /// A placeholder a lane start does not fill in renders to a clear error
    /// rather than a lane launched with a literal `{state_dir}` in its argv.
    #[test]
    fn args_template_rejects_an_unfilled_placeholder() {
        let cloud = AgentProfile::defaults()["claude"].clone();
        let mut incomplete = values();
        incomplete.remove("state_dir");
        let err = cloud.render_args(&incomplete).unwrap_err();
        assert!(err.to_string().contains("{state_dir}"));
    }

    /// spoolway is not in the business of choosing anyone's models, and
    /// `[models]` ships empty for the same reason `[agents.*]` ships no
    /// model name — a step names its own now, and a built-in price table is
    /// a later task's job.
    #[test]
    fn no_model_data_ships_in_the_defaults() {
        assert!(Config::default().models.is_empty());
    }

    /// A `[paths]` table left by a config saved before the directories under
    /// `.spoolway/` became constants must still parse. `docs` used to be the
    /// exception that survived onto `docs.path` — that target is gone too
    /// now, so nothing carries across any more: a project whose `docs` ever
    /// named somewhere other than the default gets a note, not a migration,
    /// and moves the value into its archivist prompt by hand.
    #[test]
    fn an_old_paths_table_still_parses_and_carries_nothing_across() {
        let dir = crate::scratch::root("config-legacy-paths");
        std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
        std::fs::write(
            Config::path_in(&dir),
            "[paths]\n\
             queue = \".spoolway/queue\"\n\
             archive = \".spoolway/archive\"\n\
             plans = \".spoolway/plans\"\n\
             docs = \"website/docs\"\n\
             prompts = \".spoolway/prompts\"\n\
             task_templates = \".spoolway/templates/tasks\"\n",
        )
        .unwrap();

        Config::load(&dir).expect("an old [paths] table must still parse");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// An `[effort]` table, the three retired `[dispatch]` keys, and the
    /// whole `[plans]` table — `store` included, the once-live key beside
    /// its two already-retired siblings — must all still parse, the way
    /// `[criteria]` does, and none of them come back out on the next save.
    ///
    /// `[plans]` matters more than the rest of them: it used to deny unknown
    /// fields, so without somewhere for every key to land, a project that
    /// ever wrote one would stop parsing its own config on the first command
    /// after an update. Now the whole table is opaque, so `store` retires
    /// alongside `format` and `template` rather than surviving them.
    #[test]
    fn retired_keys_parse_and_drop_on_the_next_save() {
        let raw = "[dispatch]\n\
                    max_launches = 1\n\
                    open_on_escalation = true\n\
                    open = \"kitty\"\n\
                    [effort]\n\
                    sensitive_paths = [\"src/auth/**\"]\n\
                    [effort.tier_models]\n\
                    high = \"claude-opus-5\"\n\
                    [plans]\n\
                    format = \"custom\"\n\
                    template = \"docs/my-plan.html\"\n\
                    store = \"project\"\n";
        let config: Config = toml::from_str(raw).expect("a retired key must still parse");

        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("max_launches"));
        assert!(!rendered.contains("open_on_escalation"));
        assert!(!rendered.contains("tier_models"));
        assert!(!rendered.contains("sensitive_paths"));
        assert!(!rendered.contains("[plans"));
        assert!(!rendered.contains("template"));
        assert!(!rendered.contains("store ="));
        assert!(!rendered.contains("format ="));
    }

    /// A profile naming a kind `agent::ADAPTERS` has no row for any more —
    /// because the kind it named was retired, the way a real one once was —
    /// parses as an ordinary but unrecognised kind. It has to survive
    /// parsing, or every project whose config still names a kind spoolway
    /// dropped stops loading its own config outright; and it has to be gone
    /// once `Config::load` has migrated the file, the same as the retired
    /// `args` and `model` fields on a profile that does still resolve.
    #[test]
    fn a_profile_naming_a_retired_kind_parses_and_drops_on_the_next_save() {
        let dir = crate::scratch::root("config-retired-agent-kind");
        std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
        std::fs::write(
            Config::path_in(&dir),
            "[agents.claude]\n\
             kind = \"claude\"\n\
             [agents.gone]\n\
             kind = \"a-kind-spoolway-no-longer-ships\"\n",
        )
        .unwrap();

        let config = Config::load(&dir).expect("a retired agent kind must still parse");
        assert!(
            !config.agents.contains_key("gone"),
            "a profile naming a kind spoolway can no longer launch must not survive migrate()"
        );
        assert!(
            config.agents.contains_key("claude"),
            "a profile naming a kind that still resolves is untouched"
        );

        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("a-kind-spoolway-no-longer-ships"));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A whole table missing from a config reads as its *shipped* defaults,
    /// not as the zero value of each field's type.
    ///
    /// `unattended.skip_blocked_lane` is the field that actually broke this
    /// way once, back when it lived on `[dispatch]` as `blocked_takes_over`:
    /// `#[serde(default)]` on the field itself, rather than on the
    /// container, gives `bool::default()` — `false` — and every project
    /// that had not rewritten its config would have quietly gone on doing
    /// the old thing. That is exactly what happened for one commit. It is
    /// written out unconditionally now, but the container-level default this
    /// test checks is what protects the next field like it, whichever table
    /// it lands on.
    #[test]
    fn a_config_missing_a_whole_table_reads_its_shipped_defaults() {
        let config: Config = toml::from_str("[dispatch]\ninterval = \"10s\"\n")
            .expect("a config predating [unattended] must still parse");
        assert!(
            config.unattended.skip_blocked_lane,
            "a file that has never heard of [unattended] means its shipped default, not `false`"
        );

        // Set deliberately, it survives the round trip.
        let off: Config = toml::from_str("[unattended]\nskip_blocked_lane = false\n").unwrap();
        assert!(!off.unattended.skip_blocked_lane);
        assert!(
            toml::to_string(&off)
                .unwrap()
                .contains("skip_blocked_lane = false")
        );
    }

    /// `tear_lanes_on_stop` is the renamed `cleanup_on_stop`: a defaulted
    /// config renders no line for it, and a file still holding the old
    /// spelling parses to the same value the new one would.
    #[test]
    fn tear_lanes_on_stop_stays_out_of_a_defaulted_config_and_aliases_the_old_name() {
        let config = Config::default();
        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("tear_lanes_on_stop"), "{rendered}");
        assert!(!rendered.contains("cleanup_on_stop"), "{rendered}");

        let old: Config = toml::from_str("[dispatch]\ncleanup_on_stop = false\n").unwrap();
        assert!(!old.dispatch.tear_lanes_on_stop);

        let new: Config = toml::from_str("[dispatch]\ntear_lanes_on_stop = false\n").unwrap();
        assert_eq!(
            old.dispatch.tear_lanes_on_stop,
            new.dispatch.tear_lanes_on_stop
        );
    }

    /// `lane_child_ceiling` stays out of a defaulted config the same way
    /// `tear_lanes_on_stop` does, so a lane running a binary behind main —
    /// one that predates this key entirely — still parses the file. Setting
    /// it away from its default puts it back.
    #[test]
    fn lane_child_ceiling_stays_out_of_a_defaulted_config() {
        let config = Config::default();
        assert_eq!(
            config.dispatch.lane_child_ceiling,
            Duration::from_secs(3600)
        );
        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("lane_child_ceiling"), "{rendered}");

        let custom: Config = toml::from_str("[dispatch]\nlane_child_ceiling = \"30m\"\n").unwrap();
        assert_eq!(
            custom.dispatch.lane_child_ceiling,
            Duration::from_secs(30 * 60)
        );
        assert!(
            toml::to_string(&custom)
                .unwrap()
                .contains("lane_child_ceiling = \"30m\"")
        );
    }

    /// `dispatch.priority` parses from TOML, defaults to `group`, and stays
    /// out of a defaulted config the same way `tear_lanes_on_stop` does —
    /// so a lane behind main on this key still loads the file.
    #[test]
    fn priority_parses_defaults_to_group_and_stays_out_of_a_defaulted_config() {
        let config = Config::default();
        assert_eq!(config.dispatch.priority, Priority::Group);
        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("priority"), "{rendered}");

        let any: Config = toml::from_str("[dispatch]\npriority = \"any\"\n").unwrap();
        assert_eq!(any.dispatch.priority, Priority::Any);
        assert!(
            toml::to_string(&any)
                .unwrap()
                .contains("priority = \"any\"")
        );

        let group: Config = toml::from_str("[dispatch]\npriority = \"group\"\n").unwrap();
        assert_eq!(group.dispatch.priority, Priority::Group);
        assert!(!toml::to_string(&group).unwrap().contains("priority"));
    }

    /// A config written before the sandbox was retired must still open. The
    /// whole `[sandbox]` table and both per-profile switches go the way of
    /// every other retired key: parsed, never read, and gone on the next save
    /// — because a hard parse error on upgrade would leave a project unable
    /// to run any command at all, `doctor` included.
    #[test]
    fn an_old_sandbox_table_and_its_profile_switches_parse_and_drop() {
        let raw = "[sandbox]\n\
                    mode = \"strict\"\n\
                    write = [\"~/.cargo/registry\"]\n\
                    domains = [\"api.example.com\"]\n\
                    [agents.pi]\n\
                    kind = \"pi\"\n\
                    sandbox = true\n\
                    sandbox_extension = true\n";
        let config: Config = toml::from_str(raw).expect("a config with [sandbox] must still parse");
        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("[sandbox"));
        assert!(!rendered.contains("sandbox ="));
        assert!(!rendered.contains("sandbox_extension"));
    }

    /// A config written before `blocked_on_write` and `blocked_on_overreach`
    /// were retired must still open, the two keys ignored and gone on the
    /// next save — the same shape every other retired key takes.
    #[test]
    fn an_old_write_guard_still_parses_and_drops_on_the_next_save() {
        let raw = "blocked_on_write = [\".spoolway/**\"]\n\
                    blocked_on_overreach = false\n";
        let config: Config =
            toml::from_str(raw).expect("a config naming the write guard must still parse");
        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("blocked_on_write"));
        assert!(!rendered.contains("blocked_on_overreach"));
    }

    /// A config written by a binary newer than this one must still parse,
    /// whatever top-level key it added — the failure `deny_unknown_fields`
    /// used to be, and the one `doctor` least of all could afford, since its
    /// whole job is explaining what is wrong with a config it could not even
    /// open. `extra` catches a whole unrecognised table the same way it
    /// catches a bare scalar key, and either is quietly dropped again on the
    /// next save rather than carried forward — the same shape every retired
    /// key here already takes.
    #[test]
    fn a_key_only_a_newer_binary_knows_still_parses_and_is_dropped_on_save() {
        let raw = "a_future_key = \"whatever it means\"\n\n\
                    [a_future_table]\n\
                    also_unknown = 1\n\n\
                    [dispatch]\n\
                    interval = \"10s\"\n";
        let config: Config =
            toml::from_str(raw).expect("an unknown key from a newer binary must still parse");
        assert_eq!(config.dispatch.interval, Duration::from_secs(10));

        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("a_future_key"));
        assert!(!rendered.contains("a_future_table"));
        assert!(!rendered.contains("also_unknown"));
    }

    /// An old `[pricing]` table is exactly what `[models]` holds now, so it
    /// reads into the same field and is written back under the new name.
    #[test]
    fn an_old_pricing_table_reads_into_models_and_is_written_back_renamed() {
        let raw = "[pricing.\"claude-opus-5\"]\n\
                    input = 5.0\n\
                    output = 25.0\n\
                    cache_read = 0.5\n\
                    cache_write_5m = 6.25\n\
                    cache_write_1h = 10.0\n";
        let config: Config = toml::from_str(raw).expect("an old [pricing] table must parse");
        assert_eq!(config.models["claude-opus-5"].input, 5.0);
        // The window is new, and a `[pricing]` table never had one to carry.
        assert_eq!(config.models["claude-opus-5"].context_window, 0);

        let rendered = toml::to_string(&config).unwrap();
        assert!(rendered.contains("[models."));
        assert!(!rendered.contains("[pricing"));
    }

    /// `local` marks a model as running on hardware you own. It defaults to
    /// false, is left out of the file at that value the way `slots` and
    /// `exclusive` are, and comes back as it went in when it is set.
    #[test]
    fn a_models_local_flag_round_trips_and_is_omitted_when_false() {
        let raw = "[models.\"*Ornith-1.5-35B-A3B\"]\n\
                    context_window = 100096\n\
                    slots = 3\n\
                    exclusive = true\n\
                    local = true\n";
        let config: Config = toml::from_str(raw).expect("a [models] entry with local must parse");
        assert!(config.models["*Ornith-1.5-35B-A3B"].local);

        let rendered = toml::to_string(&config).unwrap();
        assert!(rendered.contains("local = true"));

        // Unset is written by leaving the key out — the same rule every zero
        // in the table follows.
        let plain: Config = toml::from_str("[models.\"cloud-*\"]\ninput = 5.0\n").unwrap();
        assert!(!plain.models["cloud-*"].local);
        assert!(!toml::to_string(&plain).unwrap().contains("local ="));
    }

    /// `agents.*.model` and `agents.*.context_window` are two more retired
    /// keys, and the same rule applies: an old file with either still
    /// parses, and neither comes back on the next save.
    ///
    /// Scoped to the `[agents.claude]` table rather than the whole rendered
    /// file: `[pipeline_gen]` carries a `model` key of its own now, a
    /// legitimate one, so a whole-file search for `model =` would flag it
    /// too.
    #[test]
    fn a_profiles_retired_model_and_window_parse_and_drop() {
        let raw = "[agents.claude]\n\
                    kind = \"claude\"\n\
                    model = \"claude-sonnet-5\"\n\
                    context_window = 100000\n";
        let config: Config = toml::from_str(raw).expect("a retired profile key must still parse");
        let rendered = toml::to_string(&config).unwrap();
        let agents_claude = rendered
            .split("[agents.claude]")
            .nth(1)
            .expect("the profile must still be there")
            .split("\n[")
            .next()
            .unwrap();
        assert!(!agents_claude.contains("model ="));
        assert!(!agents_claude.contains("context_window"));
    }

    /// `session_reuse_uncached` retires the way `model`/`context_window` did:
    /// an existing config still parses, and it is gone on the next save
    /// because nothing reads it any more — the horizon it argued about is
    /// `models.<glob>.session_reuse_idle` now, and an unset one already says
    /// what this saying `true` used to.
    #[test]
    fn a_profiles_retired_session_reuse_uncached_parses_and_drops() {
        let raw = "[agents.claude]\n\
                    kind = \"claude\"\n\
                    session_reuse_uncached = true\n";
        let config: Config =
            toml::from_str(raw).expect("a retired session_reuse_uncached must still parse");
        assert!(
            !toml::to_string(&config)
                .unwrap()
                .contains("session_reuse_uncached")
        );
    }

    /// `models.<glob>.cache_ttl` is the field's old name, kept as a serde
    /// alias so a project's `[models]` table does not stop parsing the day
    /// this ships — and rewritten under its new name, `session_reuse_idle`,
    /// on the next save.
    #[test]
    fn a_models_cache_ttl_parses_as_session_reuse_idle_and_is_renamed_on_save() {
        let raw = "[models.\"claude-*\"]\ncache_ttl = \"5m\"\n";
        let config: Config = toml::from_str(raw).expect("the old cache_ttl spelling must parse");
        assert_eq!(
            config.models["claude-*"].session_reuse_idle,
            Some(Duration::from_secs(300))
        );

        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("cache_ttl"));
        assert!(rendered.contains("session_reuse_idle"));
    }

    /// A project configured back when the format was still called `artifact`
    /// must keep starting — the whole `[plans]` table is opaque now, so this
    /// is really the same check as the one above, for the one alias that
    /// predates the key itself.
    #[test]
    fn the_old_artifact_format_still_parses_and_drops_on_the_next_save() {
        let config: Config = toml::from_str("[plans]\nformat = \"artifact\"\n")
            .expect("a config written before the rename no longer parses");
        assert!(!toml::to_string(&config).unwrap().contains("[plans"));
    }

    /// `[docs]` and a non-empty `agents.<profile>.env` both retire the way
    /// `[plans]` did — parsed, never read, and dropped on the next save.
    #[test]
    fn an_old_docs_table_and_a_non_empty_env_table_parse_and_drop() {
        let raw = "[docs]\n\
                    path = \"website/docs\"\n\
                    format = \"custom\"\n\
                    [agents.claude]\n\
                    kind = \"claude\"\n\
                    [agents.claude.env]\n\
                    ANTHROPIC_BASE_URL = \"https://example.test\"\n";
        let config: Config = toml::from_str(raw).expect("a retired [docs]/env pair must parse");

        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("[docs"));
        assert!(!rendered.contains("website/docs"));
        assert!(!rendered.contains("ANTHROPIC_BASE_URL"));
    }

    /// The note `Config::load` prints for `[docs]` fires only where a value
    /// was somebody's decision — an untouched table, the shape every config
    /// written before this carries, says nothing. This is the predicate that
    /// decides it.
    #[test]
    fn the_docs_note_fires_only_when_the_table_was_not_already_the_default() {
        let default = "[docs]\npath = \"docs\"\nformat = \"markdown\"\n";
        let parsed: Config = toml::from_str(default).unwrap();
        assert_eq!(
            parsed.docs,
            LegacyDocs::default(),
            "the shipped default must compare equal to itself"
        );

        let customised = "[docs]\npath = \"website/docs\"\nformat = \"markdown\"\n";
        let parsed: Config = toml::from_str(customised).unwrap();
        assert_ne!(parsed.docs, LegacyDocs::default());

        // And a file that never wrote `[docs]` at all — `Config::default()`'s
        // own starting point — is the same as the default table written out.
        assert_eq!(Config::default().docs, LegacyDocs::default());
    }
}
