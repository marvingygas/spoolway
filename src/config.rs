//! Everything configurable lives here, in one `.spoolway/config.toml`.
//!
//! The file is the interface: every setting carries its explanation as a
//! comment above the key, regenerated on every save. The pipeline graphs live
//! next door in `.spoolway/pipelines/` — see [`crate::pipeline`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::platform::PathExt;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use toml_edit::{DocumentMut, Item};

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
/// Repeatable tasks a project keeps to re-run, nested however it
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

/// Directory under the project's home holding the optional patch layer —
/// see [`crate::overrides`]. A segment, like [`SCRATCH_DIR`], never a path:
/// [`crate::repo::Repo::overrides_dir`] is the one accessor that joins it
/// onto a project's home, and [`crate::overrides::dir_for`] the one that
/// does the same from a bare checkout path for the three load functions that
/// cannot reach a whole `Repo`.
pub const OVERRIDES_DIR: &str = "overrides";

/// The tracked control plane's own directory for `checkout`: `.spoolway/`
/// under it in the ordinary, repo-mode case, or a home-mode workspace's own
/// `config/` when `checkout` carries no `.spoolway/` of its own but some
/// workspace's `project.toml` lists it by path instead — see
/// [`crate::repo::workspace_clone`]. Every reader of a setup file goes
/// through this (for the handful of callers, like [`Config::load`] and
/// [`crate::pipeline::Pipelines::load`], that only have a bare path) or
/// through [`crate::repo::Repo::setup_dir`], its `Repo`-typed twin — see the
/// `setup-dir-accessor` task. Moving the tracked setup somewhere else, the
/// reason this exists, is a change to this one function and nothing else —
/// this is what makes "the setup accessor returns the workspace's `config/`"
/// true for every one of the dozens of callers above without each of them
/// having to ask; a checkout that does carry a real `.spoolway/` never pays
/// for the workspace check at all, since [`tracked_setup_dir_in`]'s own
/// `is_dir` short-circuits it.
pub fn setup_dir_in(checkout: &Path) -> PathBuf {
    let tracked = tracked_setup_dir_in(checkout);
    if tracked.is_dir() {
        return tracked;
    }
    match crate::repo::workspace_clone(checkout) {
        Some(clone) => clone.config_dir(),
        None => tracked,
    }
}

/// `.spoolway/` under `checkout`, the join itself and nothing past it — what
/// [`setup_dir_in`] checks before it ever asks whether a workspace lists
/// `checkout` instead. Also `crate::repo::bind`'s own way of asking the
/// identical question, for the one case it has to answer that
/// `setup_dir_in` cannot: a checkout carrying both a tracked `.spoolway/`
/// and a workspace's clone entry is a conflict `setup_dir_in` would just
/// silently resolve by preferring the tracked directory, never reporting
/// that a workspace was in the running at all — `bind` has to see the
/// tracked directory's existence directly to refuse on it instead.
pub(crate) fn tracked_setup_dir_in(checkout: &Path) -> PathBuf {
    let candidate = checkout.join(STATE_DIR);
    // `checkout` is `$HOME` itself exactly when this join lands on
    // `crate::mux::state_root()` — spoolway's own state directory, not a
    // project's tracked setup, but a real directory all the same, so every
    // caller's plain `is_dir()`/`exists()` would otherwise read it as one.
    // A path holding a NUL can never name a real file on any platform this
    // runs on, so every such *check* answers `false` without this
    // function's callers needing to special-case the one checkout none of
    // them should ever treat as a project — but nothing here stops a
    // caller from *using* this path to write with, which is why
    // `commands::init::Placement::choose` refuses repo mode in this one
    // checkout, by name, before `init` ever reaches a writer at all (home
    // mode never writes into the checkout, so it is let through). See
    // [`is_state_root_checkout`], the same identity check this and that
    // refusal both go through.
    if is_state_root_checkout(checkout) {
        return candidate.join("\0not-a-project");
    }
    candidate
}

/// Whether `checkout` is `$HOME` itself — the one directory a project's
/// tracked `.spoolway/` can never be, because `crate::mux::state_root()`
/// joins `.spoolway` onto exactly this path and nowhere else, making it
/// spoolway's own state directory rather than any project's. Compared
/// against `crate::platform::home_dir()` directly rather than joining
/// `STATE_DIR` a second time here: that join is a setup-path constant, and
/// `nothing_joins_state_dir_except_the_one_accessor` keeps every one of its
/// joins inside the handful of accessors already named for it —
/// [`tracked_setup_dir_in`] is that one join for this identity check too.
pub(crate) fn is_state_root_checkout(checkout: &Path) -> bool {
    // Both sides resolved: `$HOME` may be a symlink to the checkout, and
    // `checkout` arrives already resolved, so comparing the raw spelling let
    // a symlinked home pass for an ordinary project.
    crate::platform::home_dir().map(|home| home.comparable()) == Some(checkout.comparable())
}

/// `full` — one of the constants above ([`PROMPTS_DIR`], [`TASK_TEMPLATES_DIR`],
/// [`ROUTINES_DIR`], [`JOBS_FILE`]), always
/// spelled whole from the checkout (`.spoolway/prompts`, never bare
/// `prompts`), because a project's own docs and `commands::init`'s
/// scaffolding both display it that way — rebased onto `setup_dir`, an
/// answer [`setup_dir_in`] or [`crate::repo::Repo::setup_dir`] already gave
/// the caller, by stripping the shared [`STATE_DIR`] prefix back off. Takes
/// the folder rather than a checkout so a caller that already resolved one
/// — `commands::init`'s own `state`, [`crate::repo::Repo::under_setup`] — is
/// never asked to resolve it a second time just to hand it straight back
/// in.
pub fn under_setup(setup_dir: &Path, full: &str) -> PathBuf {
    let relative = Path::new(full)
        .strip_prefix(STATE_DIR)
        .unwrap_or_else(|_| panic!("{full} is not under {STATE_DIR}"));
    setup_dir.join(relative)
}

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
             project's home directory, and a task id also crosses the wire as a multiplexer \
             session name or the alias recorded for one"
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
    /// Everything spoolway does for its own upkeep, with no bearing on how a
    /// task runs. See [`HousekeepingConfig`].
    pub housekeeping: HousekeepingConfig,
    /// Extra directories whose own agent sessions count beside this
    /// project's lanes. See [`WatchConfig`] and [`Config::watch_roots`].
    pub watch: WatchConfig,
    /// Where a project's issue tracker lives, so the dispatcher can tell it
    /// about a task's arrival at `queued`, `blocked`, `paused` or `done` — the
    /// four states nothing inside a pipeline file can already put a `run:`
    /// step on, since none of the four is a step a pipeline may declare. See
    /// [`crate::tracking`] for what actually fires.
    ///
    /// Sits directly under `[watch]`, above every `[agents.*]` and
    /// `[models.*]` table — those lists only grow, and this is the one
    /// section a project sets once and otherwise ignores. `Config`'s own
    /// field order is `config.toml`'s section order (see [`Config::render`]),
    /// so this is declared here rather than after every open-ended table.
    pub issue_tracking: IssueTrackingConfig,
    /// Agent profiles, referenced by name from a pipeline step's `agent:`.
    pub agents: BTreeMap<String, AgentProfile>,
    /// What a model costs, and how big its context window is, keyed by a glob
    /// over the model name. Consulted by `spoolway eval`.
    ///
    /// Aliased to `pricing`, its name before the window joined the rates: an
    /// old `[pricing]` table is read into this field and written back as
    /// `[models]`.
    #[serde(alias = "pricing")]
    pub models: BTreeMap<String, crate::usage::ModelPrice>,

    /// Every top-level key this binary does not know, kept rather than
    /// refused — see `Frontmatter::extra`, which this matches.
    ///
    /// `deny_unknown_fields` used to sit on this struct instead, which reads
    /// an install a version behind a project's config as if the config were
    /// wrong: a key a newer binary added is not a typo, and refusing to parse
    /// it took down every command with it, `doctor` included — the one whose
    /// job is explaining what broke.
    ///
    /// Never written back by [`Config::render`] — `skip_serializing` — so the
    /// struct itself forgets the key. The file does not: [`strip_refused_keys`] names every key here that
    /// is not on [`RETIRED_TABLES`], `config set` edits the document in place,
    /// and `spoolway sync` carries the key over with [`crate::confdoc::keep`].
    /// A key on that list is a retired one, and `sync` drops it.
    #[allow(dead_code)]
    #[serde(flatten, skip_serializing)]
    extra: BTreeMap<String, toml::Value>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            dispatch: DispatchConfig::default(),
            unattended: UnattendedConfig::default(),
            housekeeping: HousekeepingConfig::default(),
            watch: WatchConfig::default(),
            issue_tracking: IssueTrackingConfig::default(),
            agents: AgentProfile::defaults(),
            // Empty for the same reason no model name ships in `[agents]`:
            // spoolway does not know what you run, and a guessed price is worse
            // than an admitted blank. Every model a shipped pipeline names is
            // already covered by the built-in table once it exists — a row
            // here only corrects one, or prices a model nobody publishes.
            models: BTreeMap::new(),
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
/// The defaults are "no issue tracking configured, but ready once it is":
/// `hook` and `project_key` blank, `key_in_names` true. A blank `hook` runs
/// nothing and changes nothing about a task's four events or about `queue
/// add`'s generated names, whatever `key_in_names` holds — so the default
/// being on costs a project with no tracker nothing; it only matters once a
/// hook actually answers a `slug=` line.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Whether the issue's key rides into every name `queue add` generates.
    /// On by default: with the tracker hook answering a `slug=` line, `queue
    /// add` prefixes the `group:`, the `branch:` (`task/<slug>-<id>`) and the
    /// worktree directory with that slug, so `git branch` shows which issue a
    /// branch belongs to, and a merge workflow can read the group key straight
    /// back off the branch name. spoolway still parses no tracker identifier
    /// of its own — the slug comes from the hook, the one thing that knows
    /// the tracker, and is only checked against [`check_id`]'s alphabet. With
    /// no hook configured, or a blank `hook`, this changes nothing: there is
    /// no `slug=` line to prefix names with either way.
    pub key_in_names: bool,
}

impl Default for IssueTrackingConfig {
    fn default() -> Self {
        Self {
            hook: String::new(),
            project_key: String::new(),
            key_in_names: true,
        }
    }
}

/// `[housekeeping]`: everything spoolway does for its own upkeep, with no
/// bearing on how any task runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HousekeepingConfig {
    /// Whether this project's checkouts are told about a newer release.
    ///
    /// The project's rather than the machine's because a team that does not
    /// want its pipeline output disturbed decides that once, in a file
    /// everybody has. The other direction — one person, one laptop — is
    /// [`crate::release::ENV_SKIP`], which needs no file to be committed.
    pub update_check: bool,

    /// How far back a calibration session reads: archived tasks
    /// finished inside the window, and the ledger entries beside them.
    ///
    /// Same spelling `--since` already takes, and the same parser —
    /// [`human_duration`] — so `30d` here and `--since 30d` on `spoolway
    /// eval` mean the same thing. Read by the `spoolway-calibrate` skill,
    /// not by this binary.
    #[serde(with = "human_duration")]
    pub calibrate_window: Duration,

    /// How many days an entry sits in a byproduct directory before
    /// [`crate::retain`] deletes it, read off the entry's own modification
    /// time. `0` keeps everything forever — what every install does before
    /// this key existed, and still does until somebody lowers it.
    pub retention_days: u64,

    /// How many days a finished task sits in `archive/` before
    /// [`crate::retain`] deletes it, read off the file's own modification
    /// time. `0` keeps every finished task forever, which is the default:
    /// the archive is the record `spoolway eval`, `queue add` and the
    /// calibrate skill read from, so losing it is a decision a person makes,
    /// not an age the project guesses. [`HousekeepingConfig::retention_days`]
    /// no longer reaches this folder.
    pub archive_retention_days: u64,

    /// How old the shared model-price table may be before `spoolway doctor`
    /// notes it. `0` turns the note off without changing which table answers
    /// model lookups. Refreshing remains the explicit `spoolway models
    /// refresh` command however old the active table becomes.
    pub price_max_age_days: u64,
}

impl Default for HousekeepingConfig {
    fn default() -> Self {
        Self {
            // On: a check nobody switched on is a check nobody has, and this
            // one costs a file read on the command path and nothing else.
            update_check: true,
            // Two to three weeks of dispatching is enough archive to see a
            // pattern repeat without also asking a person to read a
            // half-year of history the first time they run this.
            calibrate_window: Duration::from_secs(14 * 86_400),
            // Thirty days is enough to look back at last week's run without
            // ever having to, while still bounding the logs, composed
            // prompts, scratch space and headless records that otherwise
            // grow without end.
            retention_days: 30,
            // Off: see the field's own doc.
            archive_retention_days: 0,
            price_max_age_days: 30,
        }
    }
}

/// [`Config::watch`]: directories, beside the repo root, whose own agent
/// sessions should count as this project's spend. The project root itself is
/// never named here; it is always in the resolved set — see
/// [`Config::watch_roots`], which is also the only reader of this list so
/// far. Nothing here reads a transcript.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    /// Absolute, `~`-relative, or relative to the repo root. An entry
    /// resolving to a directory that does not exist is dropped rather than
    /// failing the whole config load — see [`Config::watch_roots`].
    pub dirs: Vec<String>,
}

/// Tab 1 — the run loop itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DispatchConfig {
    /// Where lanes run: a real herdr pane. `headless` is spoolway's own
    /// test backend — see [`Backend::Headless`] — refused outside the
    /// harness that sets `SPOOLWAY_TEST_BACKEND`.
    pub backend: Backend,

    /// How long a lane may say nothing before the dispatcher reminds it to
    /// report.
    ///
    /// Patience, which is not the same quantity as how often a pass looks —
    /// it used to be read off that instead. How often a pass *looks* at a lane says nothing
    /// about how long that lane may reasonably be quiet, and at the shipped
    /// ten-second interval the two together meant a lane had ten seconds to
    /// speak or be nudged. Turning the poll rate down to react faster silently
    /// bought less patience, which is not a trade anybody asked for.
    ///
    /// Ten seconds is not a stuck lane; it is a lane thinking. The case that
    /// forced this apart is an agent step that ends its turn and waits minutes
    /// on a background job — a build, or the same 45-minute `scripts/gate.sh`
    /// the shipped `test` step runs. A settled lane and a lane that
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
    /// same reason as [`Self::priority`] below: a lane here routinely runs a
    /// binary built from a branch behind main, and a binary before unknown keys
    /// were tolerated (0.8.0 and earlier) refuses one with a hard parse error
    /// rather than ignoring it.
    #[serde(
        with = "human_duration",
        skip_serializing_if = "is_default_lane_child_ceiling"
    )]
    pub lane_child_ceiling: Duration,

    /// Retired: how a tmux run was laid out in the multiplexer. The tmux
    /// backend is gone — see [`Backend::Herdr`]'s own note — so there is no
    /// multiplexer left for this to lay out. Kept only so an existing config
    /// still parses, whatever it holds; dropped unconditionally on the next
    /// save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    tmux_mode: Option<toml::Value>,

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

    /// Retired: the pipeline a task ran on when its own `pipeline:` was
    /// absent. Every task now names its pipeline itself — a task with
    /// none is refused before it can queue, and dispatch refuses the whole
    /// start over any live task still missing one — so there is nothing left
    /// for a project-wide default to answer. Kept only so an existing config
    /// still parses; dropped unconditionally on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    default_pipeline: String,

    /// Retired: where a dispatched task's worktree was cut, when it held a
    /// path. Every worktree now lands under the project's own home, with no
    /// way to move it — see [`crate::repo::Repo::worktree_root`]. Kept only so an
    /// existing config still parses; dropped unconditionally on the next
    /// save. A non-blank value earns a note on every ordinary
    /// [`Config::load`] (see `load_with_notices`), and `spoolway sync` and
    /// `spoolway doctor` both go further and name a queued task whose
    /// worktree still sits at the old path, rather than dropping the key in
    /// silence or calling a worktree cut there safe to remove.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    worktree_root: String,

    /// Whether spoolway commits a lane's leftover work when its step settles.
    ///
    /// The one guarantee spoolway makes about git, and the only reason it runs
    /// a git verb of its own: reaching `done` removes the
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
    /// reason as [`Self::lane_child_ceiling`] above: a lane here routinely
    /// runs a binary built from a branch behind main, and a binary before
    /// unknown keys were tolerated (0.8.0 and earlier) refuses one with a hard
    /// parse error rather than ignoring it.
    #[serde(skip_serializing_if = "Priority::is_group")]
    pub priority: Priority,

    /// Whether a finished step's agent stays open in its own pane until its
    /// task is done. On by default. Off, each new agent step is placed by the
    /// same spiral split as ever, and the task's most recent kept pane is
    /// closed once that step's boot is prepared, so a task shows one agent
    /// pane at a time. Panes already kept when it is turned off stay until
    /// their task is done: only the most recent one goes with each new step.
    ///
    /// Not written to `config.toml` while it holds its default, for the same
    /// reason as [`Self::lane_child_ceiling`] above: a lane here routinely
    /// runs a binary built from a branch behind main, and a binary before
    /// unknown keys were tolerated (0.8.0 and earlier) refuses one with a hard
    /// parse error rather than ignoring it.
    #[serde(skip_serializing_if = "is_true")]
    pub keep_finished_lanes: bool,

    /// Retired: whether stopping the dispatcher ended the run's live lanes
    /// and took their worktrees with it. Stopping never does that any more —
    /// see [`crate::dispatch::Dispatcher::sweep_on_stop`] — so there is no
    /// longer a second value for this key to choose between: every
    /// interrupted lane is left exactly where it stood, with its spend
    /// banked and its launch counter forgiven, whichever way the run stopped.
    /// Kept, under this spelling or its own predecessor `cleanup_on_stop`,
    /// only so an existing config still parses — 0.1.0 printed the key in
    /// every scaffolded config's reference header, and refusing it took every
    /// command down with the file, `config set` included, leaving a hand edit
    /// as the only way out. [`Config::load`] says once that it is no longer
    /// read; dropped unconditionally on the next save.
    #[serde(alias = "cleanup_on_stop", default, skip_serializing)]
    pub(crate) tear_lanes_on_stop: Option<toml::Value>,

    /// Retired: how a herdr run was laid out — `split`, a workspace per task,
    /// or `grouped`, every task a pane in one tab its project shared. Every
    /// task now runs in a herdr workspace of its own, so there is no longer a
    /// second layout for this key to choose. Kept, whatever value it holds,
    /// only so an existing config still parses — every scaffolded config's
    /// reference header listed the key, and refusing it would take every
    /// command down with the file. [`Config::load`] says once that it is no
    /// longer read; dropped unconditionally on the next save.
    #[serde(default, skip_serializing)]
    pub(crate) herdr_mode: Option<toml::Value>,
}

/// Skips an hour on the way out — see [`DispatchConfig::lane_child_ceiling`]
/// for why a key that holds its default is better off absent here.
fn is_default_lane_child_ceiling(value: &Duration) -> bool {
    *value == Duration::from_secs(3600)
}

/// Skips a `true` on the way out — see [`DispatchConfig::keep_finished_lanes`]
/// for why a key that holds its default is better off absent here.
fn is_true(value: &bool) -> bool {
    *value
}

impl Default for DispatchConfig {
    fn default() -> Self {
        Self {
            backend: Backend::default(),
            // Four of these (`MAX_REMINDERS` + 1) comfortably outlast the 45
            // minutes the shipped pipelines allow their longest command —
            // `scripts/gate.sh` and `scripts/e2e-pr.sh` both carry
            // `timeout: 45m` — so an agent step running that same gate by hand
            // is never the thing this catches; a lane that is really dead still
            // escalates, four of these later.
            lane_quiet: Duration::from_secs(15 * 60),
            lane_child_ceiling: Duration::from_secs(3600),
            tmux_mode: None,
            protected_branches: Vec::new(),
            notify: String::new(),
            open_on_escalation: false,
            open: String::new(),
            max_launches: 0,
            auto_unblock: false,
            default_pipeline: String::new(),
            worktree_root: String::new(),
            auto_commit: true,
            priority: Priority::default(),
            keep_finished_lanes: true,
            tear_lanes_on_stop: None,
            herdr_mode: None,
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
    /// in front of. Where a lane staffs `blocked` — the normal case, since
    /// every loaded pipeline gets one — the task goes there and the unblocker
    /// answers it; leaving `blocked` by any road starts every step's `loop:`
    /// count again, so a spent limit never stops the run. Only where nothing
    /// staffs `blocked` does every road to it instead resume the lane that hit
    /// it — the same session, the blocker in its prompt, continued rather than
    /// replaced — which is exactly what `spoolway resume` does by hand, minus
    /// the person. A task never sits on `blocked` in that case, so no count is
    /// reset and nothing this resumes hands any budget back.
    ///
    /// Two things stop meaning anything with it on, because both exist only to
    /// hand a decision to somebody who is not there: a `loop` whose exit
    /// resolves to `blocked` — the run answers its own exit by resuming itself
    /// straight back onto the spent step, so `apply_loop_budget` skips the
    /// bound outright rather than spending it against a wall this resume could
    /// only walk back into — and the launch ceiling that parks a task whose
    /// lane keeps dying (that one backs off instead — see
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

    /// Retired: whether the `blocked` step's pass counted as the blocked
    /// step's own pass, `false` handing the task back to the step it blocked
    /// on instead. That question is answered by the reported verb now, not a
    /// setting — a `--pass` always takes the unblocker at its word, and a
    /// `--pause`, `--fail` or `--block` always hands the task back once a
    /// person resumes it — see [`crate::commands::cleared_block_target`].
    /// Kept only so an existing config still parses; dropped unconditionally
    /// on the next save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    skip_blocked_lane: bool,

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
            skip_blocked_lane: false,
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
    ///
    /// The old value `tmux` still parses, to this: the tmux backend is gone,
    /// and a config that named it loads as herdr instead — see
    /// [`Config::migrate`]'s own note on the switch. The next save rewrites
    /// the key.
    #[default]
    #[serde(alias = "tmux")]
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
    ///
    /// Nothing here draws anywhere a person can see, so `spoolway dispatch`
    /// refuses to start on this backend unless `SPOOLWAY_TEST_BACKEND` is set
    /// in the environment — see `crate::headless::TEST_BACKEND_ENV` — which
    /// only the end-to-end harness exports. A config edited onto `headless`
    /// by hand is refused the same way a herdr run outside any pane is.
    Headless,
}

impl Backend {
    /// The spellings a person may type for `dispatch.backend`, in the order
    /// error messages list them. `tmux` is not among them: it still loads, as
    /// the alias on [`Backend::Herdr`], but only so an old file keeps working.
    pub const NAMES: [&'static str; 2] = ["herdr", "headless"];

    /// The backend `text` names, or an error naming the values that are
    /// allowed.
    ///
    /// `config set` calls this before it writes, because deserialising alone
    /// accepts the retired `tmux` and saves `herdr` in its place — a different
    /// value from the one typed.
    pub fn parse_typed(text: &str) -> Result<Backend> {
        match text {
            "herdr" => Ok(Backend::Herdr),
            "headless" => Ok(Backend::Headless),
            other => bail!(
                "`{other}` is not a backend — use {}",
                Self::NAMES.join(" or ")
            ),
        }
    }
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
/// - `{git_dir}`      absolute path to the git directory a lane needs write
///   access to for `git add`/`git commit` to work (see
///   [`crate::repo::git_dir`]) — a linked worktree's objects and branch ref
///   live in the main checkout's shared `.git`, outside `{worktree}` and
///   otherwise read-only to a lane confined to it
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
    /// belonged to the sandbox, which is gone. Kept
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
    /// A cap on the *harness*: how many `claude` lanes may be in flight can be
    /// a fact about an account and its rate limits, and the profile is the
    /// place to state it explicitly. No shipped profile guesses that fact.
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
    /// than how large it now is. The default is `20`; `0` disables this
    /// ceiling; otherwise the value is 1..=100.
    pub session_reuse_ctx: u8,

    /// How large a *running* lane's last completed turn may get, as a
    /// percentage of the model's context window, before the dispatcher stops
    /// the lane outright rather than let it carry on. The default is `40`;
    /// `0` is off: nothing watches a live lane's size at all. Checked the same
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
    /// Must be set above `session_reuse_ctx` when both are set: a task
    /// blocked at or below the reuse threshold would have the very session
    /// that just blocked it carried right back in on the next visit, over
    /// the size that tripped the ceiling, and block again immediately.
    /// `confkv::set` refuses either edit that would put the two the wrong
    /// way round.
    pub session_blocked_ctx: u8,

    /// Retired: the ceiling on this profile's own kind's cached usage
    /// percentage, checked before a pass started a new lane of it. The quota
    /// gate and the usage-limit detector it fed are gone outright rather
    /// than repaired — a usage limit is now an ordinary quiet pane, handled
    /// by `Dispatcher::check_unreported` like any other. Kept only so an
    /// existing config still parses; dropped unconditionally on the next
    /// save.
    #[allow(dead_code)]
    #[serde(default, skip_serializing)]
    quota_ceiling: u8,

    /// Retired: whether a carried session was still worth resuming once its
    /// prompt cache had gone cold. Two settings governed one decision, and
    /// the second was inert unless some model declared a lifetime — that
    /// lifetime is now the whole of it, as `models.<glob>.prompt_cache_ttl`.
    /// That key now defaults to five minutes on a hosted model, so what this
    /// saying `true` used to do (resume however cold) is `"0"` there. Kept only so an
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
            session_reuse_ctx: 20,
            session_blocked_ctx: 40,
            quota_ceiling: 0,
            session_reuse_uncached: false,
            env: BTreeMap::new(),
            permission_mode: String::new(),
        }
    }
}

impl AgentProfile {
    /// The built-in profile definitions. Fresh-project init retains only the
    /// selected provider's row, whichever of Claude, Codex or Pi it is; the
    /// defaults kept in memory still carry all three for test and runtime
    /// assembly.
    ///
    /// None carries a `concurrency`: a fresh project does not know the
    /// account or model capacity it would need to assert one. See the field's
    /// own doc for where an explicit harness cap belongs.
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
            session_reuse_ctx: 20,
            session_blocked_ctx: 40,
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
            // Unset: spoolway cannot infer an account's safe parallelism.
            concurrency: 0,
            context_window: 0,
            session_reuse_ctx: 20,
            session_blocked_ctx: 40,
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
            session_reuse_ctx: 20,
            session_blocked_ctx: 40,
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
        // Scaffolded pipelines write the choice down explicitly as `""`.
        // Treat that visible blank exactly like an absent key instead of
        // launching a CLI with an effort flag whose value is empty.
        let Some(effort) = effort.filter(|value| !value.trim().is_empty()) else {
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

/// Every key this binary has retired that a config it still upgrades from
/// could hold, as `(table, key)`. `*` stands for any `[agents.<name>]`
/// profile, or for any `[models."<glob>"]` row under `models.*`, which
/// [`is_retired_key`] matches by its last segment alone because a glob may
/// hold dots. These are the `skip_serializing` fields kept on the config
/// structs (in `src/usage.rs` for a models row) only so an old file still
/// parses (each under every spelling serde accepts for it), plus the two
/// [`strip_refused_keys`] removes before the parse.
///
/// [`crate::overrides::retired_config_patch_keys`] drops only keys on this
/// list from a private override. It used to treat every key the config did
/// not know as retired, and `spoolway sync` deleted a plain typo
/// (`dispatch.lane_quite`, 2026-10-02) as though it were one. A mistake is
/// for whoever wrote it to fix, so a key not named here stays in the layer.
/// A field retired later belongs here as well as under `skip_serializing`.
const RETIRED_KEYS: &[(&str, &str)] = &[
    ("dispatch", "interval"),
    ("dispatch", "tmux_mode"),
    ("dispatch", "herdr_mode"),
    ("dispatch", "protected_branches"),
    ("dispatch", "notify"),
    ("dispatch", "open_on_escalation"),
    ("dispatch", "open"),
    ("dispatch", "max_launches"),
    ("dispatch", "max_attempts"),
    ("dispatch", "auto_unblock"),
    ("dispatch", "default_pipeline"),
    ("dispatch", "worktree_root"),
    ("dispatch", "tear_lanes_on_stop"),
    ("dispatch", "cleanup_on_stop"),
    ("unattended", "skip_blocked_lane"),
    ("issue_tracking", "on_fail"),
    ("agents.*", "args"),
    ("agents.*", "model"),
    ("agents.*", "sandbox"),
    ("agents.*", "sandbox_extension"),
    ("agents.*", "context_window"),
    ("agents.*", "quota_ceiling"),
    ("agents.*", "session_reuse_uncached"),
    ("agents.*", "env"),
    ("models.*", "exclusive"),
];

/// Every top-level key this binary has retired, which a config written for an
/// older one may still hold. `Config::extra` catches each of them on load, so
/// unlike a retired key inside a table they are not fields of any struct;
/// this list is what tells them apart from a key that was never known.
/// [`unknown_keys`] skips them, and `spoolway sync` therefore still drops them.
/// `pricing` is not here because it is read as `models`.
const RETIRED_TABLES: &[&str] = &[
    "update",
    "calibrate",
    "retention",
    "prices",
    "effort",
    "stack",
    "sandbox",
    "blocked_on_write",
    "blocked_on_overreach",
    "paths",
    "docs",
    "plans",
    "pipeline_gen",
    "skills",
];

/// Whether `name` is a top-level table this binary has retired.
pub(crate) fn is_retired_table(name: &str) -> bool {
    RETIRED_TABLES.contains(&name)
}

/// Whether `dotted` (`dispatch.worktree_root`, `agents.claude.env.FOO`)
/// names a key on [`RETIRED_KEYS`], or a leaf inside one such as an entry of
/// a retired `env` table.
pub(crate) fn is_retired_key(dotted: &str) -> bool {
    // A model glob may hold a dot of its own
    // (`models.*Ornith-1.5-35B-A3B.exclusive`), so a models row is matched by
    // its last segment alone, split the way every models key is.
    if let Some(parts) = crate::confkv::split_models_key(dotted) {
        let field = parts[parts.len() - 1];
        return RETIRED_KEYS
            .iter()
            .any(|(table, key)| *table == "models.*" && *key == field);
    }
    let parts: Vec<&str> = dotted.split('.').collect();
    RETIRED_KEYS.iter().any(|(table, key)| {
        let pattern: Vec<&str> = table.split('.').chain(std::iter::once(*key)).collect();
        parts.len() >= pattern.len()
            && pattern
                .iter()
                .zip(&parts)
                .all(|(want, got)| *want == "*" || want == got)
    })
}

/// Every key `raw` holds that the structs below would refuse, stripped from
/// the text, with one note per key.
///
/// Two groups. `dispatch.interval` and `issue_tracking.on_fail` are retired
/// hard enough that `DispatchConfig` and [`IssueTrackingConfig`]'s own
/// `deny_unknown_fields` refuses a file that still names either, rather than
/// quietly dropping it the way every other retired key does — on purpose, so
/// a project only discovers a key is gone the moment something tries to read
/// it. The other group is every key [`unknown_keys`] finds inside a table
/// the structs know: a typo, or a key a newer binary added. Those are refused
/// by the same `deny_unknown_fields`, and a file written by a newer binary
/// must not take an older one down with it.
///
/// Both groups are removed from the text handed to the typed parse and
/// nowhere else. The file on disk keeps them: [`Config::save_key`] edits the
/// document in place, and `spoolway sync` grafts them back with
/// [`crate::confdoc::keep`] after its re-render. A top-level key needs no
/// strip — [`Config::extra`] accepts it — but earns its note here all the
/// same, so a misspelt table name such as `[unatended]` is not silently
/// ignored.
///
/// Every caller that loads the file runs this before its parse, which is how
/// a project upgraded from 0.6.0 with either retired key still set can run
/// more than `spoolway sync`. `spoolway sync` is still the one command that
/// writes a retired key away for good, so its note sends a person there.
/// Stripping a key the file never had is a no-op.
fn strip_refused_keys(raw: &str, path: &Path) -> Result<(String, Vec<String>)> {
    const RETIRED: [(&str, &str); 2] = [("dispatch", "interval"), ("issue_tracking", "on_fail")];
    let mut stripped = raw.to_string();
    let mut notices = Vec::new();
    for (table, key) in RETIRED {
        let present = toml::from_str::<toml::Value>(&stripped)
            .ok()
            .and_then(|v| v.get(table)?.get(key).cloned())
            .is_some();
        if present {
            notices.push(format!(
                "note: {table}.{key} in {} is retired — loaded past it; run `spoolway sync` to \
                 drop the key for good.",
                path.display(),
            ));
            stripped = crate::confdoc::remove(&stripped, &[table, key])?;
        }
    }
    for unknown in unknown_keys(&stripped) {
        let shown = unknown.join(".");
        if let [_] = unknown.as_slice() {
            notices.push(format!(
                "note: `{shown}` in {} is not a table or setting this spoolway knows — kept in \
                 the file and otherwise ignored. A newer spoolway may read it; if not, it is \
                 a typo.",
                path.display(),
            ));
        } else {
            let parts: Vec<&str> = unknown.iter().map(String::as_str).collect();
            stripped = crate::confdoc::remove(&stripped, &parts)?;
            notices.push(format!(
                "note: `{shown}` in {} is not a setting this spoolway knows — loaded past it \
                 and kept in the file. A newer spoolway may read it; if not, it is a typo.",
                path.display(),
            ));
        }
    }
    Ok((stripped, notices))
}

/// Does deserializing `{ key = value }` as a `T` fail on the key itself?
///
/// This asks serde rather than keeping a second list of field names, so the
/// answer cannot drift from the struct. Any other failure — a value of the
/// wrong type, say — is a real error the typed parse reports, not an unknown
/// key.
fn refuses_key<T: serde::de::DeserializeOwned>(key: &str, value: &toml::Value) -> bool {
    let mut one = toml::Table::new();
    one.insert(key.to_string(), value.clone());
    match toml::Value::Table(one).try_into::<T>() {
        Ok(_) => false,
        Err(err) => err.to_string().contains(&format!("unknown field `{key}`")),
    }
}

/// Add the path of every key in `table` that `T` refuses to `out`.
fn collect_refused<T: serde::de::DeserializeOwned>(
    prefix: &[&str],
    table: &toml::Table,
    out: &mut Vec<Vec<String>>,
) {
    for (key, value) in table {
        if refuses_key::<T>(key, value) {
            let mut path: Vec<String> = prefix.iter().map(|part| part.to_string()).collect();
            path.push(key.clone());
            out.push(path);
        }
    }
}

/// Every key in `raw` this binary does not know, as a path of key names.
///
/// A path is a list rather than a dotted string because a `[models]` glob may
/// itself hold dots. Three places can hold one: the top level, a table the
/// structs know (`[dispatch]`, `[agents.<name>]`, `[models."<glob>"]`, and a
/// model's price-tier sub-table), and nowhere deeper — every table below
/// those has a fixed shape. Text that is not valid TOML has none, since the
/// typed parse is the one that reports that.
///
/// `pricing` is the old name of `models` and is scanned as the same table.
pub(crate) fn unknown_keys(raw: &str) -> Vec<Vec<String>> {
    let Ok(toml::Value::Table(root)) = toml::from_str::<toml::Value>(raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();

    // What `Config` serialises is every key it has; `pricing` is the alias.
    let known: Vec<String> = toml::Value::try_from(Config::default())
        .ok()
        .and_then(|value| value.as_table().map(|t| t.keys().cloned().collect()))
        .unwrap_or_default();
    for key in root.keys() {
        if key != "pricing" && !known.contains(key) && !RETIRED_TABLES.contains(&key.as_str()) {
            out.push(vec![key.clone()]);
        }
    }

    let sub = |name: &str| root.get(name).and_then(toml::Value::as_table);
    if let Some(table) = sub("dispatch") {
        collect_refused::<DispatchConfig>(&["dispatch"], table, &mut out);
    }
    if let Some(table) = sub("unattended") {
        collect_refused::<UnattendedConfig>(&["unattended"], table, &mut out);
    }
    if let Some(table) = sub("housekeeping") {
        collect_refused::<HousekeepingConfig>(&["housekeeping"], table, &mut out);
    }
    if let Some(table) = sub("watch") {
        collect_refused::<WatchConfig>(&["watch"], table, &mut out);
    }
    if let Some(table) = sub("issue_tracking") {
        collect_refused::<IssueTrackingConfig>(&["issue_tracking"], table, &mut out);
    }
    if let Some(agents) = sub("agents") {
        for (name, profile) in agents {
            if let Some(table) = profile.as_table() {
                collect_refused::<AgentProfile>(&["agents", name], table, &mut out);
            }
        }
    }
    for models_name in ["models", "pricing"] {
        let Some(models) = sub(models_name) else {
            continue;
        };
        for (glob, row) in models {
            let Some(table) = row.as_table() else {
                continue;
            };
            collect_refused::<crate::usage::ModelPrice>(&[models_name, glob], table, &mut out);
            // `ModelPrice` refuses a table-valued key that is not a price tier
            // in words of its own, so serde's "unknown field" never reaches
            // `refuses_key`. Such a table is a key a newer binary added, or a
            // misspelt tier; either way it is left out of the parse and
            // named, and the model is priced at its base rate without it.
            for (key, value) in table {
                // A known field handed a table is a wrong-typed value, which
                // the typed parse reports; only a key `ModelPrice` has no
                // field for is unknown. A scalar stands in for the table so
                // that the probe fails on the key and on nothing else.
                if value.is_table()
                    && crate::usage::PriceTier::threshold_in(key).is_none()
                    && refuses_key::<crate::usage::ModelPrice>(key, &toml::Value::Integer(0))
                {
                    out.push(vec![models_name.to_string(), glob.clone(), key.clone()]);
                }
            }
            // Only a real tier name is scanned for keys of its own.
            for (tier, rates) in table {
                if crate::usage::PriceTier::threshold_in(tier).is_some()
                    && let Some(rates) = rates.as_table()
                {
                    collect_refused::<crate::usage::Rates>(
                        &[models_name, glob, tier],
                        rates,
                        &mut out,
                    );
                }
            }
        }
    }
    // A retired key inside a table is on its way out already, and `sync` drops it.
    out.retain(|path| !is_retired_key(&path.join(".")));
    out
}

impl Config {
    /// `.spoolway/config.toml` under `root`.
    ///
    /// `root` here is whatever directory a caller hands it — every reading
    /// call site hands `repo.checkout` now, so a command answers about the
    /// file actually in front of it. `config set`'s writing path is the one
    /// exception: it still hands `repo.root`, and refuses first if a linked
    /// worktree's `checkout` differs from it — see `commands::config_set`.
    pub fn path_in(root: &Path) -> PathBuf {
        setup_dir_in(root).join(CONFIG_FILE)
    }

    /// Load config from a directory holding `.spoolway/`, falling back to
    /// defaults if absent, with `overrides/config.toml` merged onto it by
    /// dotted key — see [`crate::overrides`]. See [`Self::path_in`] for
    /// which directory a caller should hand it.
    pub fn load(root: &Path) -> Result<Config> {
        let overrides = crate::overrides::dir_if_identified(root)?;
        Config::load_impl(root, overrides.as_deref())
    }

    /// [`Config::load`], with no patch layer applied — for a caller that
    /// must see only the tracked file: `commands::override_promote`'s own
    /// read of it, ahead of writing each patched key through
    /// [`Config::save_key`].
    pub fn load_tracked(root: &Path) -> Result<Config> {
        Config::load_impl(root, None)
    }

    fn load_impl(root: &Path, overrides: Option<&Path>) -> Result<Config> {
        Config::load_and_print(root, overrides).map(|(config, _)| config)
    }

    /// [`Config::load_impl`], also handing back the notices it actually
    /// printed, so a test can see the dedupe below at work.
    fn load_and_print(root: &Path, overrides: Option<&Path>) -> Result<(Config, Vec<String>)> {
        let (config, notices, ignored) = Config::load_with_notices(root, overrides)?;
        // One plain command loads its config more than once on the way to
        // answering — `main.rs` builds a `Pipelines` ahead of its own
        // dispatch match, and most commands read their own copy again right
        // after — and every one of those loads runs the same notices. Without
        // this, `queue list` over a config still naming `issue_tracking.
        // on_fail` prints the same note twice, and `doctor` three times.
        // `note_due` is the exact dedup `print_ignored_notices` already leans
        // on for the override layer's own notices, keyed by the rendered line
        // rather than by which notice it was, so it works here unchanged. It
        // also says nothing at all under bare `spoolway` — see
        // `overrides::QUIET_NOTES`.
        let mut printed = Vec::new();
        for notice in notices {
            if crate::overrides::note_due(&notice) {
                eprintln!("{notice}");
                printed.push(notice);
            }
        }
        crate::overrides::print_ignored_notices(&ignored);
        Ok((config, printed))
    }

    /// [`Config::load_tracked`] with every notice silenced — no "retired"
    /// note, no tmux-backend note, nothing from an `[agents.*]` table — and
    /// `config.migrate()` run directly, with no call through
    /// [`Config::load_with_notices`] at all.
    ///
    /// [`Config::load_tracked`] already reads the same file through the same
    /// patch-free path, so the two agree on every value; this exists only
    /// because `spoolway sync`'s own config step builds its own report of
    /// what changed (`refresh.dropped`, the `Migrated` notes) from the raw
    /// text itself, and would otherwise print the exact same things twice —
    /// once from here, once from its own summary. Nothing else should reach
    /// for this over [`Config::load_tracked`]: a caller that wants the
    /// ordinary notices gets them from that one for free.
    pub fn load_dropping_retired_keys(root: &Path) -> Result<Config> {
        let path = Config::path_in(root);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let (stripped, _notices) = strip_refused_keys(&raw, &path)?;
        let mut config: Config =
            toml::from_str(&stripped).with_context(|| format!("parsing {}", path.display()))?;
        config.migrate();
        Ok(config)
    }

    /// [`Config::load_impl`] with its retired-table notices handed back
    /// rather than printed, so a test can see which ones a given file on disk
    /// earns. `load_impl` is the only caller outside tests; it prints them.
    fn load_with_notices(
        root: &Path,
        overrides: Option<&Path>,
    ) -> Result<(Config, Vec<String>, Vec<crate::overrides::Ignored>)> {
        let path = Config::path_in(root);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                // `dispatch.interval` and `issue_tracking.on_fail` fail every
                // caller outright otherwise — see `strip_refused_keys`'s
                // own doc — so this strips them before the struct ever sees
                // the file, and the notices it returns say so, naming
                // `spoolway sync` as the one command that writes the key
                // away for good.
                let (stripped, mut notices) = strip_refused_keys(&raw, &path)?;
                let mut config: Config = toml::from_str(&stripped)
                    .with_context(|| format!("parsing {}", path.display()))?;
                // The alias on `Backend::Herdr` already turned a `tmux` value
                // into `Herdr` by the time `config` exists — this is only
                // what tells a person it happened, since the typed value
                // alone cannot be told apart from a file that always said
                // `herdr`. Read off a second, untyped parse of the same
                // `raw` text rather than the alias itself, which is exactly
                // what [`Config::migrate`] cannot see either.
                let named_tmux_backend = toml::from_str::<toml::Value>(&raw)
                    .ok()
                    .and_then(|v| {
                        v.get("dispatch")?
                            .get("backend")?
                            .as_str()
                            .map(str::to_string)
                    })
                    .as_deref()
                    == Some("tmux");
                if named_tmux_backend {
                    notices.push(format!(
                        "note: dispatch.backend = \"tmux\" in {} — the tmux backend is gone, \
                         so this now loads as \"herdr\". The key is rewritten on the next save.",
                        path.display(),
                    ));
                }
                // `dispatch.worktree_root` still parses — the field stays on
                // `DispatchConfig` for exactly this — but nothing reads it
                // any more, so a project that set it to a real path deserves
                // the same kind of note the two hard-retired keys above earn,
                // not silence until `doctor` or `sync` happens to mention it.
                // A blank value was never a real decision — nothing to say
                // until it names a path. `spoolway sync`'s own note on the
                // same key (`crate::sync::config`) and `doctor`'s
                // `worktree_root_note` both go further, naming a queued task
                // whose worktree still sits there; this one only says the
                // key is gone, since a plain load has no task list to check.
                if !config.dispatch.worktree_root.trim().is_empty() {
                    notices.push(format!(
                        "note: dispatch.worktree_root in {} names {} — the setting is retired, \
                         every worktree now lands under the project home; run `spoolway sync` \
                         to drop the key.",
                        path.display(),
                        config.dispatch.worktree_root,
                    ));
                }
                // Said here for the same reason: a person who set it wanted
                // their worktrees kept across a stop, and that is now what
                // every stop does.
                if config.dispatch.tear_lanes_on_stop.is_some() {
                    notices.push(format!(
                        "note: dispatch.tear_lanes_on_stop in {} is no longer read — stopping \
                         the dispatcher never removes a worktree, workspace, pane or tab any \
                         more: every interrupted lane is left standing, its spend banked and \
                         its launch counter forgiven, so the next run resumes it where it \
                         stood. The key is dropped on the next save.",
                        path.display(),
                    ));
                }
                // A person who chose `grouped` wanted every task in one tab per
                // project; one who chose `split` already has what every task now
                // gets. Either way the note says the same thing.
                if config.dispatch.herdr_mode.is_some() {
                    notices.push(format!(
                        "note: dispatch.herdr_mode in {} is no longer read — every task now \
                         runs in a herdr workspace of its own. Run `spoolway sync` to drop \
                         the key.",
                        path.display(),
                    ));
                }
                for (name, profile) in &config.agents {
                    if !profile.env.is_empty() {
                        notices.push(format!(
                            "note: [agents.{name}.env] in {} is no longer read — a lane \
                             inherits the dispatcher's environment, and anything one agent \
                             needs belongs in that agent's own config. Dropped on the next \
                             save.",
                            path.display(),
                        ));
                    }
                    if crate::agent::adapter(&profile.kind).is_none() {
                        notices.push(format!(
                            "note: [agents.{name}] in {} names kind `{}`, which spoolway no \
                             longer knows how to launch. Dropped on the next save.",
                            path.display(),
                            profile.kind,
                        ));
                    }
                }
                config.migrate();
                // Last, so the notices above read the config as the tracked
                // file actually spells it. `apply_config_patch` routes every
                // leaf through `confkv::set`, which round-trips the whole
                // config through `Value::try_from` -> `try_into` and re-runs
                // `migrate()`. Every table those notices point at is
                // `skip_serializing`, so the round-trip clears it and
                // `migrate()` drops any profile on a retired kind — a patch
                // applied any earlier silences the notices for a file that
                // still spells those tables out.
                let mut ignored = Vec::new();
                if let Some(overrides) = overrides {
                    let (patched, ig) = crate::overrides::apply_config_patch(config, overrides)?;
                    config = patched;
                    ignored = ig;
                }
                Ok((config, notices, ignored))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut config = Config::default();
                let mut ignored = Vec::new();
                if let Some(overrides) = overrides {
                    let (patched, ig) = crate::overrides::apply_config_patch(config, overrides)?;
                    config = patched;
                    ignored = ig;
                }
                Ok((config, Vec::new(), ignored))
            }
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
    /// what `spoolway sync` rewrites an existing one to. Rendering keeps a
    /// struct's values and nothing else, which is exactly the config contract:
    /// what a setting is set to is the project's, and the table above it is
    /// spoolway's. There used to be a comment standing above each key instead
    /// — one register of prose, repeated at every key it explained. A table
    /// says the same things once, in the shape a person scanning the whole
    /// surface actually wants, and [`crate::confkv::REFERENCE`] is the one
    /// place that prose is written now: this table and the settings screen
    /// both render from it. `watch.dirs` is the single carve-out —
    /// [`annotate_watch_dirs`] stands its `REFERENCE` sentence above the key
    /// too, because a bare `dirs = []` gives no hint on its own that an
    /// entry may be `~`-relative or that the project root need not be named.
    ///
    /// It is **not** how one setting is written. `config set` goes through
    /// [`Config::save_key`], which edits the document in place, because a person
    /// changing an interval has not asked for their file to be rewritten around
    /// them. See [`crate::confdoc`].
    pub fn render(&self) -> Result<String> {
        let rendered = toml::to_string_pretty(self).context("serialising config")?;
        let rendered = annotate_watch_dirs(&rendered)?;
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
        // Stripped as a load strips it: a file still naming a retired key
        // loads with a note, so an edit that leaves that key in place must
        // not be refused for it. Only `spoolway sync` removes the key. The
        // notes are dropped — the load that produced `self` already printed
        // them.
        let (text, _notices) = strip_refused_keys(text, &Config::path_in(Path::new("")))?;
        let mut reparsed: Config = toml::from_str(&text).context("it no longer parses")?;
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

    /// Every directory whose own agent sessions should count beside this
    /// project's lanes: `repo_root`, always and first, followed by each
    /// `watch.dirs` entry that resolves to a directory that still exists.
    ///
    /// An entry naming nothing that exists is dropped rather than failing —
    /// a stale line in this list is a fact about the filesystem, not the
    /// config, and is no reason to refuse a load. Every path is
    /// canonicalised before it is compared, so a `dirs` entry naming the
    /// repo root — by any spelling that resolves to it, symlinks included —
    /// never appears twice.
    ///
    /// Nothing here reads a transcript; this only says which directories a
    /// later reader should look in — [`crate::usage::sweep`]'s directory walk
    /// is that reader.
    pub fn watch_roots(&self, repo_root: &Path) -> Vec<PathBuf> {
        let home = crate::platform::home_dir();
        let mut roots = Vec::new();

        let mut push = |path: &Path| {
            let Ok(canon) = path.canonical() else {
                return;
            };
            if canon.is_dir() && !roots.contains(&canon) {
                roots.push(canon);
            }
        };

        push(repo_root);
        for raw in &self.watch.dirs {
            push(&resolve_watch_dir(raw, home.as_deref(), repo_root));
        }

        roots
    }
}

/// The ids of the tasks whose worktree sits under `raw`, a `worktree_root`
/// as a config file spells it, sorted.
///
/// A leading `~/` is expanded first: 0.6.0 expanded it when it cut a
/// worktree, so a project could have written `~/wt` while every task's
/// `worktree_path` holds the absolute path. Matching the raw text would say
/// no task is there, and a person trusting that deletes work in progress.
/// The match is by path component, so `/wt` does not claim `/wt2`.
pub fn tasks_under_worktree_root<'a>(raw: &str, tasks: &'a [crate::task::Task]) -> Vec<&'a str> {
    // Trimmed first, as 0.6.0 did before expanding: a padded value cut its
    // worktrees at the trimmed folder.
    let root = resolve_watch_dir(
        raw.trim(),
        crate::platform::home_dir().as_deref(),
        Path::new(""),
    );
    let mut ids: Vec<&str> = tasks
        .iter()
        .filter(|t| {
            t.front
                .worktree_path
                .as_deref()
                .is_some_and(|wt| Path::new(wt).starts_with(&root))
        })
        .map(crate::task::Task::id)
        .collect();
    ids.sort_unstable();
    ids
}

/// One `watch.dirs` entry, expanded against the home directory and the repo
/// root — not yet checked for existence, which [`Config::watch_roots`] does
/// once, after every entry has been resolved the same way.
fn resolve_watch_dir(raw: &str, home: Option<&Path>, repo_root: &Path) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    } else if raw == "~"
        && let Some(home) = home
    {
        return home.to_path_buf();
    }

    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    }
}

/// Stand `watch.dirs`'s own reference sentence directly above the key, the
/// one field in a fresh render that gets this. Every other key's
/// explanation lives only in the header table on top, but a bare
/// `dirs = []` gives no hint that an entry may be `~`-relative or that the
/// project root need not be named, and that is worth restating right where
/// a person is about to edit it.
fn annotate_watch_dirs(rendered: &str) -> Result<String> {
    let mut doc: DocumentMut = rendered.parse().context("re-parsing rendered config")?;
    let note = crate::confkv::note("watch.dirs").context("watch.dirs has no reference entry")?;
    if let Some(mut key) = doc
        .get_mut("watch")
        .and_then(Item::as_table_mut)
        .and_then(|table| table.key_mut("dirs"))
    {
        key.leaf_decor_mut().set_prefix(comment_block(note));
    }
    Ok(doc.to_string())
}

/// One note, as the comment lines that stand above its key.
///
/// The single spelling of a note-as-comment: `config set` uses it in
/// [`crate::confdoc`] when it adds a key the file never had, so that a key
/// arriving alone still carries its one-sentence explanation. Everywhere
/// else about the surface — the reference table [`Config::render`] writes on
/// top of a whole file — reads the same sentence straight out of
/// [`crate::confkv::REFERENCE`] rather than through a comment at all, with
/// one exception: [`annotate_watch_dirs`] calls this too, to stand
/// `watch.dirs`'s sentence above that one key on every render, not only
/// when `config set` adds it.
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
/// written the same way `dispatch.lane_quiet` in config.toml is. One spelling
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
    /// For a fact a project may leave unstated —
    /// [`crate::usage::ModelPrice::prompt_cache_ttl`] is the one. Absent and
    /// zero have to stay distinct there: absent means "nobody has said", so
    /// the default limit applies, and zero means "no limit", which a bare `"0"`
    /// spells.
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

    /// A sibling folder sharing the root's name as a prefix is not under it,
    /// and a padded `~/` value still names the folder 0.6.0 cut worktrees in.
    #[test]
    fn tasks_under_a_worktree_root_match_by_component_and_ignore_padding() {
        let task = |id: &str, wt: &str| {
            crate::task::Task::parse(
                PathBuf::from(format!("{id}.md")),
                &format!("---\nid: {id}\nstage: paused\nworktree_path: {wt}\n---\n"),
            )
            .unwrap()
        };
        let tasks = [
            task("in", "/old/wt/task-in"),
            task("sibling", "/old/wt2/task-sibling"),
        ];
        assert_eq!(tasks_under_worktree_root("/old/wt", &tasks), ["in"]);
        assert_eq!(tasks_under_worktree_root(" /old/wt ", &tasks), ["in"]);

        let Some(home) = crate::platform::home_dir() else {
            return;
        };
        let at_home = [task("z1", &format!("{}/p25-wt/task-z1", home.display()))];
        assert_eq!(tasks_under_worktree_root("~/p25-wt ", &at_home), ["z1"]);
    }

    /// `$HOME` reached through a symlink is still the home folder, so a
    /// checkout resolved to its real path must be recognised as it.
    #[test]
    fn a_symlinked_home_is_still_the_state_root_checkout() {
        let base = crate::scratch::root("config-test-symlinked-home");
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        crate::platform::test_home::with_home(&link, || {
            assert!(is_state_root_checkout(&real.canonicalize().unwrap()));
            assert!(!is_state_root_checkout(&base));
        });
    }

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

    /// `~/.spoolway` is spoolway's own state directory, not a project's
    /// tracked setup — a command run directly in `$HOME` must never read it
    /// as one, the same way `Repo::root`'s ancestor walk already refuses to
    /// find a project there. `tracked_setup_dir_in` is the one place every
    /// other check in this area (`Placement::choose`, `bind`,
    /// `local::is_repo_mode`) goes through, so fixing it here is what
    /// keeps `init --setup home --yes` from reading
    /// "already has a tracked `.spoolway/`" when run in `$HOME` itself.
    #[test]
    fn home_itself_never_carries_a_tracked_setup() {
        let home = crate::scratch::root("config-test-home-not-a-project");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        crate::platform::test_home::with_home(&home, || {
            // The directory really is there on disk — spoolway's own state
            // root, created the way any real use of this machine would.
            std::fs::create_dir_all(crate::mux::state_root()).unwrap();

            assert!(
                !tracked_setup_dir_in(&home).is_dir(),
                "{} must not read as a tracked project setup",
                home.display()
            );
            assert_ne!(
                setup_dir_in(&home),
                crate::mux::state_root(),
                "the general setup-dir accessor must agree"
            );
        });
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

        assert_eq!(parsed.dispatch.lane_quiet, original.dispatch.lane_quiet);
        assert_eq!(parsed.agents["claude"].kind, "claude");
        // No profile guesses a harness cap. Zero has to survive the round
        // trip as zero rather than being written out and read back as
        // something else.
        for name in ["pi", "claude", "codex"] {
            assert_eq!(parsed.agents[name].concurrency, 0, "{name}");
            assert_eq!(parsed.agents[name].session_reuse_ctx, 20, "{name}");
            assert_eq!(parsed.agents[name].session_blocked_ctx, 40, "{name}");
        }
        assert!(
            !text.contains("concurrency ="),
            "a fresh profile must omit its zero concurrency cap:\n{text}"
        );
        // Every model your pipelines name is already covered by the built-in
        // table once it exists; nothing here is a guess spoolway made for you.
        assert!(parsed.models.is_empty());
        assert_eq!(parsed.housekeeping.price_max_age_days, 30);
    }

    /// `[issue_tracking]` sits directly under `[watch]` now — above every
    /// open-ended `[agents.*]`/`[models.*]` table, which only ever grow —
    /// rather than after all of them, where `Config`'s own field order used
    /// to put it. A fresh `init` writes it there because `render` renders
    /// straight from that field order.
    #[test]
    fn a_fresh_config_puts_issue_tracking_directly_under_watch() {
        let rendered = Config::default().render().unwrap();
        // `\n[` rather than a bare `[`, or the reference table's own prose —
        // `unattended.blocked_agent`'s sentence names `[agents.*]` — would
        // match first, long before any real table heading does.
        let watch_at = rendered.find("\n[watch]").unwrap();
        let issue_tracking_at = rendered.find("\n[issue_tracking]").unwrap();
        let agents_at = rendered.find("\n[agents.").unwrap();
        assert!(
            watch_at < issue_tracking_at && issue_tracking_at < agents_at,
            "expected [watch] < [issue_tracking] < [agents.*]:\n{rendered}"
        );
    }

    #[test]
    fn written_config_explains_the_price_age_key_and_round_trips_it() {
        let mut config = Config::default();
        config.housekeeping.price_max_age_days = 7;
        let rendered = config.render().unwrap();
        assert!(rendered.contains("housekeeping.price_max_age_days"));
        assert!(rendered.contains("before `spoolway doctor` says so"));
        assert!(rendered.contains("price_max_age_days = 7"));

        let parsed: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed.housekeeping.price_max_age_days, 7);
    }

    #[test]
    fn watch_dirs_parses_and_round_trips() {
        let text = "[watch]\ndirs = [\"~/notes\"]\n";
        let parsed: Config = toml::from_str(text).unwrap();
        assert_eq!(parsed.watch.dirs, vec!["~/notes".to_string()]);

        let rendered = parsed.render().unwrap();
        let reparsed: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(reparsed.watch.dirs, parsed.watch.dirs);
    }

    /// The one key whose sentence is written twice: once in the header table
    /// every key gets, and again right above `dirs` itself, in the same
    /// wrapped voice [`comment_block`] gives every other note it writes.
    #[test]
    fn written_config_explains_watch_dirs_above_the_key_too() {
        let rendered = Config::default().render().unwrap();
        assert!(rendered.contains("watch.dirs"), "{rendered}");
        assert!(
            rendered.contains(
                "# Directories whose own sessions are counted beside the lanes. The project\n\
                 # root is always watched; these are extra. Absolute, or ~-relative.\n\
                 dirs = []"
            ),
            "the comment above `dirs` does not match the reference sentence:\n{rendered}"
        );

        let parsed: Config = toml::from_str(&rendered).unwrap();
        assert!(parsed.watch.dirs.is_empty());
    }

    /// An absent `[watch]` table is exactly the same as an empty `dirs`: no
    /// existing config fails to load because of this key.
    #[test]
    fn an_absent_watch_table_loads_as_no_extra_dirs() {
        let parsed: Config = toml::from_str("[dispatch]\nauto_commit = false\n").unwrap();
        assert!(parsed.watch.dirs.is_empty());
    }

    /// With no `dirs` at all, the resolved set is the project root alone.
    #[test]
    fn watch_roots_with_no_dirs_is_the_project_root_alone() {
        let root = crate::scratch::root("watch-roots-alone");
        std::fs::create_dir_all(&root).unwrap();
        let config = Config::default();

        let roots = config.watch_roots(&root);

        assert_eq!(roots, vec![root.canonical().unwrap()]);
    }

    /// Every named form resolves: `~`-relative against the home directory,
    /// relative against the repo root, and absolute as itself — and the
    /// project root is never duplicated even though it is also named
    /// explicitly, by a relative spelling that lands right back on it.
    #[test]
    fn watch_roots_resolves_every_form_and_drops_what_does_not_exist() {
        let root = crate::scratch::root("watch-roots-forms");
        let home = crate::scratch::root("watch-roots-home");
        std::fs::create_dir_all(home.join("notes")).unwrap();
        let absolute = crate::scratch::root("watch-roots-absolute");
        std::fs::create_dir_all(&absolute).unwrap();
        std::fs::create_dir_all(root.join("logs")).unwrap();

        let mut config = Config::default();
        config.watch.dirs = vec![
            "~/notes".to_string(),
            "logs".to_string(),
            absolute.display().to_string(),
            ".".to_string(),
            "nowhere/at/all".to_string(),
        ];

        let roots = crate::platform::test_home::with_home(&home, || config.watch_roots(&root));

        let expected: Vec<_> = [
            root.to_path_buf(),
            home.join("notes"),
            root.join("logs"),
            absolute.to_path_buf(),
        ]
        .into_iter()
        .map(|p| p.canonical().unwrap())
        .collect();
        assert_eq!(roots.len(), expected.len(), "{roots:?}");
        for path in &expected {
            assert!(roots.contains(path), "{path:?} missing from {roots:?}");
        }
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
            "git_dir",
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
        assert!(cloud.effort_args(Some("")).is_empty());
        assert!(cloud.effort_args(Some("   ")).is_empty());
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

    /// Both agent kinds a lane can be dispatched with are launched with a
    /// third `--add-dir`, naming the git directory the lane's worktree
    /// actually uses — the grant that lets `git add`/`git commit` create
    /// `index.lock` there instead of failing on a read-only filesystem.
    #[test]
    fn both_kinds_carry_the_git_dir_as_a_third_add_dir() {
        for kind in ["claude", "codex"] {
            let profile = AgentProfile::defaults()[kind].clone();
            let args = profile.render_args(&values()).unwrap();
            let add_dirs: Vec<&str> = args
                .windows(2)
                .filter(|pair| pair[0] == "--add-dir")
                .map(|pair| pair[1].as_str())
                .collect();
            assert_eq!(
                add_dirs,
                ["<state_dir>", "<project_home>", "<git_dir>"],
                "`{kind}` must carry `{{state_dir}}`, `{{project_home}}` and \
                 `{{git_dir}}` as its three `--add-dir` grants, in that order"
            );
        }
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

    /// An `[effort]` table, five retired `[dispatch]` keys, and the
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
                    default_pipeline = \"impl\"\n\
                    tmux_mode = \"grouped\"\n\
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
        assert!(!rendered.contains("default_pipeline"));
        assert!(!rendered.contains("tmux_mode"));
        assert!(!rendered.contains("tier_models"));
        assert!(!rendered.contains("sensitive_paths"));
        assert!(!rendered.contains("[plans"));
        assert!(!rendered.contains("template"));
        assert!(!rendered.contains("store ="));
        assert!(!rendered.contains("format ="));
    }

    /// `dispatch.interval` and `issue_tracking.on_fail` are retired hard
    /// enough that `DispatchConfig` and `IssueTrackingConfig`'s own
    /// `deny_unknown_fields` refuses a file still naming either — this used
    /// to be stripped only by `spoolway sync`'s own loader,
    /// [`Config::load_dropping_retired_keys`], leaving every other command
    /// — `queue list`, `config get`, `resume`, `doctor` — refusing to load a
    /// project upgraded from 0.6.0 at all. [`strip_refused_keys`] is
    /// now shared by [`Config::load`] and [`Config::load_tracked`] too, so
    /// this loads past the retired key with a note naming `spoolway sync`,
    /// the one command that still writes it away for good.
    #[test]
    fn a_retired_key_that_only_sync_strips_still_loads_everywhere_else() {
        let dir = crate::scratch::root("config-retired-key-outside-sync");
        std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
        std::fs::write(Config::path_in(&dir), "[issue_tracking]\non_fail = \"\"\n").unwrap();

        Config::load(&dir).expect(
            "a config holding a key only `load_dropping_retired_keys` strips must still load \
             outside `sync`, with a note naming `spoolway sync`",
        );

        // One note for the one key the file names, naming `spoolway sync` —
        // not zero, which is the whole fix. That one load never printed it
        // twice is `the_same_retired_key_note_prints_once_however_often_a_command_loads`'s
        // to show.
        let notices = Config::load_with_notices(&dir, None).unwrap().1;
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("issue_tracking.on_fail"), "{notices:?}");
        assert!(notices[0].contains("spoolway sync"), "{notices:?}");
    }

    /// One command runs `Config::load` several times — `main.rs` builds a
    /// `Pipelines` first, then most commands read their own copy — and
    /// before `load_impl` deduped its notices, `queue list` over a 0.6.0
    /// config printed the `issue_tracking.on_fail` note twice and `doctor`
    /// three times (seen 2026-10-02). The second load here must print
    /// nothing.
    #[test]
    fn the_same_retired_key_note_prints_once_however_often_a_command_loads() {
        let dir = crate::scratch::root("config-retired-key-note-once");
        std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
        std::fs::write(Config::path_in(&dir), "[issue_tracking]\non_fail = \"\"\n").unwrap();

        let first = Config::load_and_print(&dir, None).unwrap().1;
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(first[0].contains("issue_tracking.on_fail"), "{first:?}");

        let second = Config::load_and_print(&dir, None).unwrap().1;
        assert!(second.is_empty(), "a second load printed again: {second:?}");
    }

    /// Only a key this binary actually retired counts as one: a typo with no
    /// near live key (`lane_quite`) is not on the list, while a retired key,
    /// a retired spelling (`max_attempts`) and a leaf under a retired agent
    /// table all are.
    #[test]
    fn only_a_key_on_the_retired_list_counts_as_retired() {
        assert!(is_retired_key("dispatch.worktree_root"));
        assert!(is_retired_key("dispatch.max_attempts"));
        assert!(is_retired_key("issue_tracking.on_fail"));
        assert!(is_retired_key("agents.claude.env.FOO"));
        assert!(!is_retired_key("dispatch.lane_quite"));
        assert!(!is_retired_key("dispatch.lane_quiet"));
        assert!(!is_retired_key("agents.claude"));
        assert!(!is_retired_key("models.opus.context_window"));
        assert!(is_retired_key("models.my-local-*.exclusive"));
        assert!(is_retired_key("models.*Ornith-1.5-35B-A3B.exclusive"));
    }

    /// `dispatch.worktree_root` is only soft-retired — the field still
    /// parses on its own, with no `deny_unknown_fields` to trip — so it
    /// never needed `strip_refused_keys` to load. The plan's own
    /// `d-upgrade-floor` still asks for a note naming `spoolway sync` on
    /// every ordinary load, the same as the two hard-retired keys, so an
    /// otherwise silent command (`queue list`, before this fix) still says
    /// something.
    #[test]
    fn a_real_worktree_root_earns_a_load_note_naming_sync() {
        let dir = crate::scratch::root("config-worktree-root-load-note");
        std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
        std::fs::write(
            Config::path_in(&dir),
            "[dispatch]\nworktree_root = \"/old/worktrees\"\n",
        )
        .unwrap();

        let notices = Config::load_with_notices(&dir, None).unwrap().1;
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("dispatch.worktree_root"), "{notices:?}");
        assert!(notices[0].contains("/old/worktrees"), "{notices:?}");
        assert!(notices[0].contains("spoolway sync"), "{notices:?}");
    }

    /// A blank `dispatch.worktree_root` was never a real decision — the
    /// mockup's own 0.6.0 fixture carries one and shows no note for it, the
    /// same as `doctor`'s and `sync`'s own notes on the same key.
    #[test]
    fn a_blank_worktree_root_earns_no_load_note() {
        let dir = crate::scratch::root("config-worktree-root-load-note-blank");
        std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
        std::fs::write(Config::path_in(&dir), "[dispatch]\nworktree_root = \"\"\n").unwrap();

        let notices = Config::load_with_notices(&dir, None).unwrap().1;
        assert!(notices.is_empty(), "{notices:?}");
    }

    /// A config naming `dispatch.herdr_mode`, whichever of its two old values it
    /// holds, still loads and earns one note naming the key and the one layout
    /// left. Neither value comes back out on the next save.
    #[test]
    fn the_retired_herdr_mode_key_loads_with_one_note_for_either_value() {
        for value in ["grouped", "split"] {
            let dir = crate::scratch::root(&format!("config-herdr-mode-{value}"));
            std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
            std::fs::write(
                Config::path_in(&dir),
                format!("[dispatch]\nherdr_mode = \"{value}\"\n"),
            )
            .unwrap();

            let (config, notices, _) = Config::load_with_notices(&dir, None).unwrap();
            assert_eq!(notices.len(), 1, "{value}: {notices:?}");
            assert!(notices[0].contains("dispatch.herdr_mode"), "{notices:?}");
            assert!(
                notices[0].contains("herdr workspace of its own"),
                "{notices:?}"
            );
            assert!(notices[0].contains("spoolway sync"), "{notices:?}");
            assert!(!toml::to_string(&config).unwrap().contains("herdr_mode"));
        }
        assert!(Config::default().dispatch.herdr_mode.is_none());
    }

    /// A config still holding a key 0.7 retired loads with a note, so saving
    /// one other key into it must work as well: the one key is written, the
    /// retired key stays exactly where it was, and nothing else in the file
    /// moves. Only `spoolway sync` removes the retired key.
    #[test]
    fn saving_one_key_into_a_config_with_a_retired_key_writes_only_that_key() {
        let dir = crate::scratch::root("config-save-key-past-retired");
        std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
        let before = "[issue_tracking]\nhook = \"\"\non_fail = \"\"\nkey_in_names = false\n";
        std::fs::write(Config::path_in(&dir), before).unwrap();

        let config = Config::load(&dir).unwrap();
        let key = "dispatch.lane_quiet";
        let config = crate::confkv::set(&config, key, "25m").unwrap();
        config
            .save_key(&dir, key)
            .expect("saving one key must not be refused over a key that loads with a note");

        let after = std::fs::read_to_string(Config::path_in(&dir)).unwrap();
        assert!(
            after.contains("on_fail = \"\""),
            "retired key was removed:\n{after}"
        );
        assert!(after.contains("lane_quiet"), "{after}");
        assert!(
            after.starts_with(before),
            "bytes above the edit moved:\n{after}"
        );
    }

    /// A retired key written inside an inline table loads the same way as one
    /// in a plain table: the load succeeds and names the key in its note.
    #[test]
    fn a_retired_key_inside_an_inline_table_loads_with_the_same_note() {
        let dir = crate::scratch::root("config-retired-key-inline-table");
        std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
        std::fs::write(
            Config::path_in(&dir),
            "issue_tracking = { hook = \"\", project_key = \"\", on_fail = \"pause\", \
             key_in_names = false }\n",
        )
        .unwrap();

        let notices = Config::load_with_notices(&dir, None)
            .expect("an inline table naming a retired key must load")
            .1;
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].contains("issue_tracking.on_fail"), "{notices:?}");
        assert!(notices[0].contains("spoolway sync"), "{notices:?}");
    }

    /// A whole `[pipeline_gen]` table, all six keys it ever carried — the
    /// three live ones `spoolway pipeline gen` read plus the three already
    /// retired inside it — must still parse now that the command and the
    /// table are both gone, and none of it comes back on the next save.
    #[test]
    fn a_pipeline_gen_table_parses_and_drops_on_the_next_save() {
        let raw = "[pipeline_gen]\n\
                    pipeline_agent = \"claude\"\n\
                    pipeline_model = \"claude-opus-5\"\n\
                    pipeline_effort = \"high\"\n\
                    pipeline_auto = true\n\
                    pipeline_loop_default = 3\n\
                    pipeline_local_models = true\n";
        let config: Config =
            toml::from_str(raw).expect("an old [pipeline_gen] table must still parse");

        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("[pipeline_gen"));
        assert!(!rendered.contains("pipeline_agent"));
        assert!(!rendered.contains("pipeline_model"));
        assert!(!rendered.contains("pipeline_effort"));
        assert!(!rendered.contains("pipeline_auto"));
        assert!(!rendered.contains("pipeline_loop_default"));
        assert!(!rendered.contains("pipeline_local_models"));
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
    /// `unattended.skip_blocked_lane` was the field that actually broke this
    /// way once, back when it lived on `[dispatch]` as `blocked_takes_over`:
    /// `#[serde(default)]` on the field itself, rather than on the
    /// container, gives `bool::default()` — `false` — and every project
    /// that had not rewritten its config would have quietly gone on doing
    /// the old thing. That is exactly what happened for one commit.
    /// `blocked_session` is a live field with the same shape — written out
    /// unconditionally, defaulting to `true` — so the container-level
    /// default this test checks is what protects it, and the next field like
    /// it, whichever table it lands on.
    #[test]
    fn a_config_missing_a_whole_table_reads_its_shipped_defaults() {
        let config: Config = toml::from_str("[dispatch]\nlane_quiet = \"10m\"\n")
            .expect("a config predating [unattended] must still parse");
        assert!(
            config.unattended.blocked_session,
            "a file that has never heard of [unattended] means its shipped default, not `false`"
        );

        // Set deliberately, it survives the round trip.
        let off: Config = toml::from_str("[unattended]\nblocked_session = false\n").unwrap();
        assert!(!off.unattended.blocked_session);
        assert!(
            toml::to_string(&off)
                .unwrap()
                .contains("blocked_session = false")
        );
    }

    /// `unattended.skip_blocked_lane` retires the way `session_reuse_uncached`
    /// and `quota_ceiling` do: an existing config still parses, and the key
    /// is gone on the next save because nothing reads it any more — clearing
    /// a block now reads the reported verb instead, see
    /// [`crate::commands::cleared_block_target`].
    #[test]
    fn skip_blocked_lane_parses_and_drops() {
        let raw = "[unattended]\nskip_blocked_lane = false\n";
        let config: Config =
            toml::from_str(raw).expect("a retired skip_blocked_lane must still parse");
        assert!(
            !toml::to_string(&config)
                .unwrap()
                .contains("skip_blocked_lane")
        );
    }

    /// A config naming `tear_lanes_on_stop`, or its own predecessor
    /// `cleanup_on_stop`, still parses — it was a documented 0.1.0 key, and
    /// refusing it took every command down with the file — and neither
    /// spelling comes back out on the next save, the same shape every other
    /// retired key takes.
    #[test]
    fn the_retired_tear_lanes_on_stop_key_parses_and_drops_by_either_spelling() {
        for raw in [
            "[dispatch]\ntear_lanes_on_stop = false\n",
            "[dispatch]\ncleanup_on_stop = false\n",
        ] {
            let config: Config =
                toml::from_str(raw).expect("a retired tear_lanes_on_stop must still parse");
            assert!(config.dispatch.tear_lanes_on_stop.is_some(), "{raw}");
            let rendered = toml::to_string(&config).unwrap();
            assert!(!rendered.contains("tear_lanes_on_stop"), "{rendered}");
            assert!(!rendered.contains("cleanup_on_stop"), "{rendered}");
        }
        assert!(Config::default().dispatch.tear_lanes_on_stop.is_none());
    }

    /// A config naming `backend = "tmux"` — the tmux backend is gone — still
    /// parses, loads as `Backend::Herdr`, earns a notice saying so, and never
    /// comes back out as `tmux` on the next save.
    #[test]
    fn a_backend_of_tmux_loads_as_herdr_with_a_notice() {
        with_override_fixture("backend-tmux", "[dispatch]\nbackend = \"tmux\"\n", |root| {
            let (config, notices, _ignored) = Config::load_with_notices(root, None).unwrap();
            assert_eq!(config.dispatch.backend, Backend::Herdr);
            assert!(
                notices.iter().any(|n| n.contains("dispatch.backend")
                    && n.contains("tmux")
                    && n.contains("herdr")),
                "no notice explained the switch: {notices:?}"
            );
            let rendered = toml::to_string(&config).unwrap();
            assert!(rendered.contains("backend = \"herdr\""), "{rendered}");
        });
    }

    /// `lane_child_ceiling` stays out of a defaulted config so a lane running
    /// a binary behind main — one that predates this key entirely — still
    /// parses the file. Setting it away from its default puts it back.
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

    /// `keep_finished_lanes` stays out of a defaulted config, so a lane running
    /// an older binary can still read the file, and is written once a person
    /// turns it off.
    #[test]
    fn keep_finished_lanes_stays_out_of_a_defaulted_config() {
        let config = Config::default();
        assert!(config.dispatch.keep_finished_lanes);
        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("keep_finished_lanes"), "{rendered}");

        let absent: Config = toml::from_str("[dispatch]\nauto_commit = true\n").unwrap();
        assert!(absent.dispatch.keep_finished_lanes);

        let off: Config = toml::from_str("[dispatch]\nkeep_finished_lanes = false\n").unwrap();
        assert!(!off.dispatch.keep_finished_lanes);
        assert!(
            toml::to_string(&off)
                .unwrap()
                .contains("keep_finished_lanes = false")
        );
    }

    /// `dispatch.priority` parses from TOML, defaults to `group`, and stays
    /// out of a defaulted config the same way `lane_child_ceiling` does —
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

    /// A config written before `skills` was retired — skill spend is no
    /// longer a `spoolway eval` reader's to bucket by name — must still open,
    /// the key ignored and gone on the next save, the same shape every other
    /// retired key takes.
    #[test]
    fn an_old_skills_list_still_parses_and_drops_on_the_next_save() {
        let raw = "skills = [\"spoolway-plan\", \"code-review\"]\n";
        let config: Config =
            toml::from_str(raw).expect("a config naming `skills` must still parse");
        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("skills"));
    }

    /// A config written by a binary newer than this one must still parse,
    /// whatever top-level key it added — the failure `deny_unknown_fields`
    /// used to be, and the one `doctor` least of all could afford, since its
    /// whole job is explaining what is wrong with a config it could not even
    /// open. `extra` catches a whole unrecognised table the same way it
    /// catches a bare scalar key, and the struct forgets either on a render.
    /// The file keeps them — see `an_unknown_key_survives_a_sync` in
    /// `sync.rs`.
    #[test]
    fn a_key_only_a_newer_binary_knows_still_parses_and_the_struct_forgets_it() {
        let raw = "a_future_key = \"whatever it means\"\n\n\
                    [a_future_table]\n\
                    also_unknown = 1\n\n\
                    [dispatch]\n\
                    lane_quiet = \"10m\"\n";
        let config: Config =
            toml::from_str(raw).expect("an unknown key from a newer binary must still parse");
        assert_eq!(config.dispatch.lane_quiet, Duration::from_secs(600));

        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("a_future_key"));
        assert!(!rendered.contains("a_future_table"));
        assert!(!rendered.contains("also_unknown"));
    }

    /// A typo inside a known table, in a `[models]` row (glob dots and a price
    /// tier included) and a misspelt table name are all unknown; a retired
    /// table and the `pricing` alias are not.
    #[test]
    fn unknown_keys_finds_typos_in_tables_and_rows_but_not_retired_ones() {
        let raw = "[dispatch]\nlane_quite = \"5m\"\n\n\
                    [agents.claude]\nkind = \"claude\"\nmodle = \"x\"\n\n\
                    [models.\"qwen3.5\"]\nslots = 2\nflavour = 1\n\
                    [models.\"qwen3.5\".above_100k_tokens]\ninput = 1.0\nbogus = 2.0\n\n\
                    [unatended]\nx = 1\n\n[sandbox]\nenabled = true\n\n\
                    [pricing.\"old\"]\ninput = 1.0\n";
        let found: Vec<String> = unknown_keys(raw).iter().map(|p| p.join(".")).collect();
        assert_eq!(
            found,
            [
                "unatended",
                "dispatch.lane_quite",
                "agents.claude.modle",
                "models.qwen3.5.flavour",
                "models.qwen3.5.above_100k_tokens.bogus",
            ],
        );
    }

    /// Every unknown key loads, whichever table holds it, and earns a note
    /// that names it. A key that is only misspelt is not an error.
    #[test]
    fn unknown_keys_load_with_a_note_naming_each() {
        let raw = "[dispatch]\nlane_quite = \"5m\"\nbackend = \"headless\"\n\n\
                    [models.\"m\"]\nflavour = 1\n\n[unatended]\nx = 1\n";
        with_override_fixture("unknown-keys", raw, |root| {
            let (config, notices, _ignored) = Config::load_with_notices(root, None).unwrap();
            assert_eq!(config.dispatch.backend, Backend::Headless);
            for name in ["dispatch.lane_quite", "models.m.flavour", "`unatended`"] {
                assert!(
                    notices
                        .iter()
                        .any(|n| n.starts_with("note: ") && n.contains(name)),
                    "no note named {name}: {notices:?}"
                );
            }
        });
    }

    /// Bare `spoolway` prints none of a load's notes, so each one must have a
    /// row in doctor's cheap findings, which the "before dispatching" popup
    /// shows. Each pair names a note and the row that says it there. The test
    /// covers the notes its fixture `raw` earns, and every one of them must
    /// match a pair. A note added to `load_with_notices` later needs a line in
    /// `raw` that earns it and a pair here. The ignored-override note's row is
    /// the overrides gate instead —
    /// `commands::dispatch::tests::a_newly_ignored_override_brings_a_hidden_gate_back`
    /// — and the workspace notes' rows are
    /// `commands::doctor::tests::each_workspace_note_has_a_row_in_the_warnings_popup`.
    #[test]
    fn each_load_note_has_a_row_in_the_warnings_popup() {
        let raw = "[dispatch]\ninterval = \"5m\"\nbackend = \"tmux\"\n\
                   worktree_root = \"/elsewhere/worktrees\"\ntear_lanes_on_stop = true\n\
                   herdr_mode = \"split\"\nlane_quite = \"5m\"\n\n\
                   [issue_tracking]\non_fail = \"pause\"\n\n\
                   [agents.old]\nkind = \"nonesuch\"\n\n\
                   [agents.envy]\nkind = \"claude\"\n\n[agents.envy.env]\nFOO = \"1\"\n\n\
                   [unatended]\nx = 1\n";
        let pairs: &[(&str, &[&str])] = &[
            (
                "dispatch.interval in",
                &["dispatch.interval is retired in this checkout's config"],
            ),
            (
                "issue_tracking.on_fail in",
                &["issue_tracking.on_fail is retired in this checkout's config"],
            ),
            ("`unatended` in", &["does not know:", "unatended"]),
            (
                "`dispatch.lane_quite` in",
                &["does not know:", "dispatch.lane_quite"],
            ),
            (
                "dispatch.backend = \"tmux\" in",
                &["dispatch.backend = \"tmux\" in this checkout's config"],
            ),
            (
                "dispatch.worktree_root in",
                &["dispatch.worktree_root in this checkout's config names"],
            ),
            (
                "dispatch.tear_lanes_on_stop in",
                &["dispatch.tear_lanes_on_stop in this checkout's config"],
            ),
            (
                "dispatch.herdr_mode in",
                &["dispatch.herdr_mode in this checkout's config"],
            ),
            (
                "[agents.envy.env] in",
                &["[agents.envy.env] in this checkout's config"],
            ),
            (
                "[agents.old] in",
                &["[agents.old] in this checkout's config names kind `nonesuch`"],
            ),
        ];
        with_override_fixture("note-rows", raw, |root| {
            let (_, notices, _) = Config::load_with_notices(root, None).unwrap();
            for notice in &notices {
                assert!(
                    pairs.iter().any(|(note, _)| notice.contains(note)),
                    "a note with no row paired here: {notice}"
                );
            }
            let repo = crate::repo::Repo {
                borrowed: false,
                checkout: root.to_path_buf(),
                root: root.to_path_buf(),
                config: Config::default(),
                home: root.join(".home"),
            };
            let rows: Vec<String> = crate::commands::cheap_findings(
                &repo,
                &crate::pipeline::Pipelines::builtin(),
                &repo.config,
            )
            .into_iter()
            .map(|warning| match warning {
                crate::commands::Warning::Setting(t)
                | crate::commands::Warning::File(t)
                | crate::commands::Warning::Problem(t) => t,
            })
            .collect();
            for (note, row) in pairs {
                assert!(
                    notices.iter().any(|n| n.contains(note)),
                    "the fixture never earns {note}: {notices:?}"
                );
                assert!(
                    rows.iter().any(|r| row.iter().all(|part| r.contains(part))),
                    "no row says {note}: {rows:#?}"
                );
            }
        });
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
    /// false, is left out of the file at that value the way `slots` is, and
    /// comes back as it went in when it is set.
    #[test]
    fn a_models_local_flag_round_trips_and_is_omitted_when_false() {
        let raw = "[models.\"*Ornith-1.5-35B-A3B\"]\n\
                    context_window = 100096\n\
                    slots = 3\n\
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

    /// `models.<glob>.exclusive` is retired: a config that still sets it
    /// loads, the key is never written back, and it is on the retired list
    /// that `spoolway sync` drops from a private override.
    #[test]
    fn a_models_retired_exclusive_parses_and_is_not_written_back() {
        let raw = "[models.\"*Ornith-1.5-35B-A3B\"]\n\
                    slots = 3\n\
                    exclusive = true\n\
                    local = true\n";
        let config: Config = toml::from_str(raw).expect("a retired exclusive must still parse");
        assert_eq!(config.models["*Ornith-1.5-35B-A3B"].slots, 3);

        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("exclusive"), "{rendered}");
        assert!(rendered.contains("slots = 3"), "{rendered}");
        assert!(is_retired_key("models.*Ornith-1.5-35B-A3B.exclusive"));
    }

    /// `agents.*.model` and `agents.*.context_window` are two more retired
    /// keys, and the same rule applies: an old file with either still
    /// parses, and neither comes back on the next save.
    ///
    /// Scoped to the `[agents.claude]` table rather than the whole rendered
    /// file, so a `model =` key legitimate elsewhere in the config would not
    /// flag it too.
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
    /// `models.<glob>.prompt_cache_ttl` now, where `"0"` says what this
    /// saying `true` used to.
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

    /// `quota_ceiling` retires the way `session_reuse_uncached` did: the
    /// quota gate it configured is gone outright, along with the usage-limit
    /// detector it shared a name with — a usage limit is now an ordinary
    /// quiet pane, handled like any other. An existing config still parses,
    /// and the key is gone on the next save because nothing reads it any
    /// more.
    #[test]
    fn a_profiles_retired_quota_ceiling_parses_and_drops() {
        let raw = "[agents.claude]\n\
                    kind = \"claude\"\n\
                    quota_ceiling = 85\n";
        let config: Config = toml::from_str(raw).expect("a retired quota_ceiling must still parse");
        assert!(!toml::to_string(&config).unwrap().contains("quota_ceiling"));
    }

    /// `models.<glob>.cache_ttl` and `session_reuse_idle` are the field's old
    /// names, kept as serde aliases so a project's `[models]` table does not
    /// stop parsing the day this ships — and rewritten under its new name,
    /// `prompt_cache_ttl`, on the next save.
    #[test]
    fn a_models_retired_ttl_names_parse_as_prompt_cache_ttl_and_are_renamed_on_save() {
        for old in ["cache_ttl", "session_reuse_idle"] {
            let raw = format!("[models.\"claude-*\"]\n{old} = \"5m\"\n");
            let config: Config = toml::from_str(&raw)
                .unwrap_or_else(|err| panic!("the old {old} spelling must parse: {err}"));
            assert_eq!(
                config.models["claude-*"].prompt_cache_ttl,
                Some(Duration::from_secs(300))
            );

            let rendered = toml::to_string(&config).unwrap();
            assert!(!rendered.contains(&format!("\n{old} =")), "{rendered}");
            assert!(rendered.contains("prompt_cache_ttl"), "{rendered}");
        }
    }

    /// A bare `"0"` is a statement that there is no limit, so it parses as a
    /// zero duration rather than as an absent key, and survives a save.
    #[test]
    fn a_zero_prompt_cache_ttl_parses_and_round_trips() {
        let raw = "[models.\"claude-*\"]\nprompt_cache_ttl = \"0\"\n";
        let config: Config = toml::from_str(raw).expect("\"0\" must parse");
        assert_eq!(
            config.models["claude-*"].prompt_cache_ttl,
            Some(Duration::ZERO)
        );
        let again: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(
            again.models["claude-*"].prompt_cache_ttl,
            Some(Duration::ZERO)
        );
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
    fn a_non_empty_agent_env_table_parses_and_drops() {
        let raw = "[agents.claude]\n\
                    kind = \"claude\"\n\
                    [agents.claude.env]\n\
                    ANTHROPIC_BASE_URL = \"https://example.test\"\n";
        let config: Config = toml::from_str(raw).expect("a retired env table must parse");

        let rendered = toml::to_string(&config).unwrap();
        assert!(!rendered.contains("ANTHROPIC_BASE_URL"));
    }

    /// A scratch project with a tracked `config.toml`, and a scratch `$HOME`
    /// swapped in for `f` so `crate::overrides::dir_for` resolves under a
    /// directory this test owns.
    fn with_override_fixture<T>(name: &str, tracked: &str, f: impl FnOnce(&Path) -> T) -> T {
        let root = crate::scratch::root(&format!("config-override-{name}"));
        let home = crate::scratch::root(&format!("config-override-{name}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
        crate::scratch::stamped(&root);
        std::fs::create_dir_all(root.join(STATE_DIR)).unwrap();
        std::fs::write(Config::path_in(&root), tracked).unwrap();

        let result = crate::platform::test_home::with_home(&home, || f(&root));

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
        result
    }

    /// A checkout with no id has no overrides layer, and loading its config
    /// reads the tracked file alone instead of refusing: the first command in
    /// a fresh clone reads its config before anything has stamped it.
    #[test]
    fn a_checkout_with_no_id_loads_its_tracked_config_alone() {
        let root = crate::scratch::root("config-no-id");
        let home = crate::scratch::root("config-no-id-home");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(STATE_DIR)).unwrap();
        crate::scratch::git_init(&root, &["-q"]);
        std::fs::write(
            Config::path_in(&root),
            "[unattended]\nblocked_agent = \"tracked\"\n",
        )
        .unwrap();
        crate::platform::test_home::with_home(&home, || {
            let config = Config::load(&root).unwrap();
            assert_eq!(config.unattended.blocked_agent, "tracked");
            assert!(!crate::mux::state_root().exists());
        });
    }

    /// `overrides/config.toml` merges onto the tracked file by dotted key —
    /// a key the patch names changes, a key it does not name still comes
    /// from the tracked file.
    #[test]
    fn an_override_merges_by_dotted_key_onto_the_tracked_config() {
        with_override_fixture(
            "dotted-key",
            "[unattended]\nenabled = false\nblocked_agent = \"tracked\"\n",
            |root| {
                let overrides = crate::overrides::dir_for(root).unwrap();
                std::fs::create_dir_all(&overrides).unwrap();
                std::fs::write(
                    overrides.join(CONFIG_FILE),
                    "[unattended]\nenabled = true\n",
                )
                .unwrap();

                let config = Config::load(root).unwrap();
                assert!(config.unattended.enabled, "the patched key must win");
                assert_eq!(
                    config.unattended.blocked_agent, "tracked",
                    "a key the patch never named must still come from the tracked file"
                );
            },
        );
    }

    /// A config override key the tracked config would refuse — a bad value
    /// for a typed field — is skipped, reported in `load_with_notices`'s own
    /// `Ignored` list, and every other key in the same patch still applies.
    #[test]
    fn a_config_key_the_tracked_config_would_refuse_is_skipped_with_the_rest_applied() {
        with_override_fixture(
            "bad-value",
            "[unattended]\nenabled = false\nblocked_agent = \"tracked\"\n",
            |root| {
                let overrides = crate::overrides::dir_for(root).unwrap();
                std::fs::create_dir_all(&overrides).unwrap();
                std::fs::write(
                    overrides.join(CONFIG_FILE),
                    "[unattended]\nenabled = \"not-a-bool\"\nblocked_agent = \"patched\"\n",
                )
                .unwrap();

                let (config, _notices, ignored) =
                    Config::load_with_notices(root, Some(&overrides)).unwrap();
                assert!(
                    !config.unattended.enabled,
                    "the refused key must never have applied — the tracked value stands"
                );
                assert_eq!(
                    config.unattended.blocked_agent, "patched",
                    "a key in the same patch that the config accepts still applies"
                );
                assert_eq!(ignored.len(), 1, "{ignored:?}");
                assert_eq!(ignored[0].fields, "unattended.enabled");
            },
        );
    }

    /// `Config::load_tracked` is the second entry point the plan calls for:
    /// it answers the tracked config even where a patch is sitting right
    /// there on disk, for a caller that must not see the merge.
    #[test]
    fn load_tracked_ignores_a_patch_on_disk() {
        with_override_fixture("load-tracked", "[unattended]\nenabled = false\n", |root| {
            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                overrides.join(CONFIG_FILE),
                "[unattended]\nenabled = true\n",
            )
            .unwrap();

            assert!(!Config::load_tracked(root).unwrap().unattended.enabled);
            assert!(Config::load(root).unwrap().unattended.enabled);
        });
    }

    /// A patch on disk does not silence the retired-table notices: they are
    /// about what the *tracked* file still spells out, and the patch merge
    /// round-trips the config through `confkv::set`, clearing every
    /// `skip_serializing` table those notices point at. So the merge runs
    /// last, after the notices and after `migrate()`.
    #[test]
    fn a_patch_on_disk_does_not_silence_the_retired_table_notices() {
        let tracked = "[agents.leftover]\nkind = \"nosuchkind\"\n\
                       [agents.pi]\nkind = \"pi\"\n\
                       [agents.pi.env]\nFOO = \"bar\"\n\
                       [unattended]\nenabled = false\n";
        with_override_fixture("keeps-notices", tracked, |root| {
            let bare = Config::load_with_notices(root, None).unwrap().1;
            assert_eq!(
                bare.len(),
                2,
                "the tracked file alone earns both notices: {bare:?}"
            );

            let overrides = crate::overrides::dir_for(root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                overrides.join(CONFIG_FILE),
                "[unattended]\nenabled = true\n",
            )
            .unwrap();

            let (config, patched, _ignored) =
                Config::load_with_notices(root, Some(&overrides)).unwrap();
            assert!(config.unattended.enabled, "the patch still applies");
            assert_eq!(
                patched, bare,
                "a patch on an unrelated key must not change which notices print"
            );
            for table in ["[agents.pi.env]", "[agents.leftover]"] {
                assert!(
                    patched.iter().any(|n| n.contains(table)),
                    "{table} notice missing with a patch on disk: {patched:?}"
                );
            }
        });
    }

    /// With no `overrides/` directory on disk at all, `Config::load` answers
    /// exactly what it did before this layer existed.
    #[test]
    fn with_no_overrides_directory_load_is_unchanged() {
        with_override_fixture("absent", "[unattended]\nenabled = true\n", |root| {
            assert_eq!(
                Config::load(root).unwrap().unattended.enabled,
                Config::load_tracked(root).unwrap().unattended.enabled,
            );
        });
    }

    /// `spoolway config set` on a tier rate writes the sub-table into the
    /// file, and the file loads back with the same value.
    #[test]
    fn saving_a_tier_rate_writes_the_sub_table_and_reads_back() {
        let dir = crate::scratch::root("config-save-tier-rate");
        std::fs::create_dir_all(dir.join(STATE_DIR)).unwrap();
        std::fs::write(
            Config::path_in(&dir),
            "[models.\"claude-haiku-5-5\"]\ninput = 0.1\n",
        )
        .unwrap();

        let key = "models.claude-haiku-5-5.above_100k_tokens.input";
        let config = crate::confkv::set(&Config::load(&dir).unwrap(), key, "0.5").unwrap();
        config.save_key(&dir, key).unwrap();

        let after = std::fs::read_to_string(Config::path_in(&dir)).unwrap();
        assert!(
            after.contains("[models.\"claude-haiku-5-5\".above_100k_tokens]"),
            "{after}"
        );
        let loaded = Config::load(&dir).unwrap();
        assert_eq!(crate::confkv::get(&loaded, key).unwrap(), "0.5");
        assert_eq!(loaded.models["claude-haiku-5-5"].input, 0.1);
    }

    /// A misspelt tier sub-table loads, is named in a note, and is left out:
    /// the model is priced at its base rate until the name is fixed.
    #[test]
    fn a_misnamed_tier_sub_table_loads_with_a_note() {
        let raw = "[models.\"claude-haiku-5-5\"]\ninput = 0.25\n\n\
                    [models.\"claude-haiku-5-5\".above_100K_token]\ninput = 0.5\n";
        with_override_fixture("misnamed-tier", raw, |root| {
            let (config, notices, _ignored) = Config::load_with_notices(root, None).unwrap();
            assert_eq!(config.models["claude-haiku-5-5"].input, 0.25);
            assert!(config.models["claude-haiku-5-5"].tier.is_none());
            assert!(
                notices.iter().any(|n| n.contains("above_100K_token")),
                "{notices:?}"
            );
        });
    }

    /// A known field handed a table is a wrong-typed value, not an unknown
    /// key: it is not named as unknown, and the typed parse still refuses it.
    #[test]
    fn a_known_models_key_with_a_table_value_is_refused_not_called_unknown() {
        let raw = "[models.\"m\"]\nslots = { n = 3 }\n";
        assert!(unknown_keys(raw).is_empty(), "{:?}", unknown_keys(raw));
        with_override_fixture("table-valued-slots", raw, |root| {
            assert!(Config::load_with_notices(root, None).is_err());
        });
    }
}
