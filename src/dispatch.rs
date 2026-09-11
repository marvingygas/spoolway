//! The dispatcher: one deterministic pass over the pipeline graph, repeated on
//! an interval.
//!
//! Nothing here consults a model. Every decision is a lookup — a task's stage,
//! a lane's status, a counter, a timestamp. The only judgement calls the old
//! prose dispatcher made are replaced by:
//!
//! - "is it stuck or just thinking" -> time since its output last changed
//! - "is this a repeat failure"     -> a round counter in the task file
//! - "is it blocked or hung"        -> whether its step declares a human gate
//!
//! A pass is safe to interrupt at any point: it derives all its state fresh
//! from task files and the multiplexer, and never remembers anything.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::AgentProfile;
use crate::graph::Graph;
use crate::mux::{Lane, LaneSpec, LaneStatus, Mux, Vacated, lane_name, parse_lane_name, tab_label};
use crate::pipeline::{Pipeline, Pipelines, Step, StepKind};
use crate::repo::Repo;
use crate::task::Task;

pub(crate) const LANES_FILE: &str = "lanes.json";

/// How many times a task's lane may be *launched* at the step it is on
/// before a person is asked instead.
///
/// Not a retry budget for the work, which is why it is not called one. Work
/// is never retried: a lane that finished its turn without reporting keeps
/// its pane, gives its slot back, and waits for a person — nothing here
/// watchdogs it, times it out, or starts it again. What this catches is the
/// case with no session to wait on at all: an agent binary that dies at
/// launch leaves nothing behind, so the task looks unstarted on the next
/// pass and would be re-spawned every pass forever.
///
/// Launched once and never relaunched. Never a number anybody has needed to
/// tune, so it is a constant rather than a setting nobody would change — the
/// old `dispatch.max_launches` still parses in an existing config, and is
/// dropped on the next save.
///
/// An unattended run has no person to ask, so the ceiling becomes a backoff
/// instead — see [`relaunch_backoff`].
const MAX_LAUNCHES: u32 = 1;

/// How many times [`Dispatcher::check_unreported`] reminds a settled lane
/// before giving up and escalating instead of sending a fourth.
///
/// Not configurable until something asks for that: three is patience enough
/// for a person genuinely mid-answer, and a ceiling that never bites is the
/// bug this exists to fix — see `.spoolway/plans/stop-the-blocked-loop.html`.
const MAX_REMINDERS: u32 = 3;

/// How many starts in a row that could not run at all get refused, by
/// [`crate::lock::Restarts`] — see `commands::dispatch`.
///
/// Four rather than one or two: a person starting the dispatcher twice by
/// habit, or a supervisor's own retry after a blip, is not the storm this
/// guards against. A caller that is still failing to run a fifth time in a
/// row, all inside [`RESTART_WINDOW`], is not going to stop on its own.
pub const RESTART_MAX: u32 = 4;

/// The window [`RESTART_MAX`] counts inside.
///
/// Long enough to catch a supervisor restarting at typical intervals of a
/// few seconds, short enough that a caller which gave up for a while and
/// tried again later — a person back at their desk, a cron job hours apart —
/// starts its own count fresh rather than inheriting an old storm.
pub const RESTART_WINDOW: Duration = Duration::from_secs(30);

/// How long a task's next step waits for the step before it to let go of the
/// task's pane.
///
/// A lane runs `spoolway report` mid-turn and keeps talking afterwards, so the
/// pass that reads the moved stage still sees the old lane working. Starting
/// the next step there and then is what put two live panes in one task's tab.
/// The next step waits instead — but not for ever, because a lane can stop
/// reporting progress and never settle, and a task whose one pane is waited on
/// for ever would never run another step.
///
/// Two minutes, against a `dispatch.interval` of ten seconds: a dozen passes,
/// which is far more than the second or two a lane spends finishing its
/// sentence, and short enough that a lane that is never coming back does not
/// hold its task up for long. A constant rather than a config key for the same
/// reason [`MAX_REMINDERS`] is one — nothing has needed to tune it, and the
/// figure only has to be bigger than "a moment" and smaller than "for ever".
///
/// Past it, the next step starts anyway: its pane is split first and the stuck
/// lane's pane is closed after, so the task's tab is never momentarily empty.
const HANDOVER_WAIT: Duration = Duration::from_secs(120);

/// How long a task waits before its lane is launched again, in an unattended
/// run, after `attempts` launches that left nothing behind.
///
/// The attended answer to a lane that dies at launch is to stop and ask, on the
/// grounds that a broken agent binary is not something more launches fix. That
/// is still true unattended — but there is nobody to ask, and parking the task
/// is the one thing the mode is defined by not doing. So the task keeps its
/// place and is simply tried less and less often: a dispatcher that re-spawns a
/// dying agent every ten seconds all night is the busy loop the ceiling existed
/// to prevent, and one that tries again in twenty minutes costs nothing and
/// picks the work straight back up when the install is repaired.
///
/// Doubling from the pass interval, capped: an hour is long enough that a
/// wedged install is nearly free, and short enough that a run left going over a
/// weekend still recovers on its own.
pub fn relaunch_backoff(interval: Duration, attempts: u32) -> Duration {
    const CAP: Duration = Duration::from_secs(3600);
    let doublings = attempts.saturating_sub(MAX_LAUNCHES).min(12);
    interval
        .saturating_mul(1u32 << doublings)
        .min(CAP)
        .max(interval)
}

/// Quota retries do not launch anything. Start at one minute, double on
/// each failed recheck, and cap at one hour, independently of launch attempts.
fn quota_backoff(retries: u32) -> i64 {
    (60i64 * (1i64 << retries.min(6))).min(3600)
}

/// What one pass did, for printing and for the loop's own decisions.
#[derive(Debug, Default)]
pub struct Report {
    pub actions: Vec<String>,
    pub problems: Vec<String>,
    /// Nothing is running and nothing is waiting to run.
    pub quiet: bool,
    /// This run has spent its `max_output_tokens` and started nothing, with the
    /// figures. The run ends once [`Report::lanes_live`] goes false.
    pub ceiling: Option<String>,
    /// Whether any lane of this run was still open at the end of the pass.
    pub lanes_live: bool,
}

/// Per-lane bookkeeping the multiplexer does not keep for us: when a lane
/// started, and when its output last changed. `last_progress` is what the
/// reminder loop reads; `started_at` is the usage ledger's alone now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LaneRecord {
    started_at: i64,
    last_progress: i64,
    output_hash: u64,
    /// Whether this lane's pane is holding a question for a person. Set once
    /// when the lane settles on its step, so a lane waiting overnight is
    /// marked one time rather than once per pass.
    ///
    /// Cleared again the moment the lane is seen working, because a lane
    /// mid-turn is holding nothing anybody can answer — and cleared when its
    /// pane is held for a block, where what waits is the task rather than the
    /// pane.
    #[serde(default)]
    notified: bool,
    /// When this lane was last sent the report contract again, in
    /// `last_progress`'s own clock. `None` until the first reminder.
    ///
    /// The whole rule `check_unreported` runs is one comparison against this:
    /// due for another reminder exactly when `last_progress` has moved past
    /// it, which is to say the lane has written something since. Nothing
    /// resets it early — a lane going back to work and settling again is
    /// judged the same way the next time, by whether it wrote anything in
    /// between.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reminded_at: Option<i64>,
    /// How many times this lane has been sent the report contract again.
    ///
    /// Lives beside `reminded_at` and needs no clearing: it counts up for the
    /// life of the record and dies with it, the same way `reminded_at` does.
    /// `check_unreported` reads it to cap the round trip — a lane genuinely
    /// waiting on a person gets patience, but not an unbounded amount of it.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    reminders: u32,
    /// Session id handed to this lane's agent, and what its transcript is
    /// found by once the lane settles. Empty for a lane started by a spoolway
    /// that predates the ledger, or by a project whose `args` drop
    /// `{session_id}` — both simply go unaccounted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    session: String,
    /// Agent kind and profile, kept here because the lane is read at teardown,
    /// when the step that chose them may already have been superseded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    model: String,
    /// Where the lane's branch stood at launch. Read only when the lane is torn
    /// down without having reported, which is the one path where nothing else
    /// knows whether the work in the worktree is the lane's whole turn or the
    /// residue of a turn it already committed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    head: String,
    /// This lane is over, and its pane is kept open only so a person can read
    /// the session that stopped. Set when its task lands on `blocked`, cleared
    /// by the pane being closed once the task is unblocked.
    ///
    /// It is also the "already paid for" mark: a held lane's usage was booked
    /// when it was held, so the close that comes later must not book it twice.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    held_for_block: bool,
    /// Set while a held lane's pane is busy, and read back the moment it goes
    /// idle again — that transition is a person's round finishing, and the
    /// one thing worth committing the worktree for. Cleared once that commit
    /// has run, so a pane that stays idle afterwards is not committed again
    /// on every later pass.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    person_turn_busy: bool,
    /// When the task this lane belongs to first arrived on a step this lane is
    /// not, and so first wanted its pane back. `None` until that happens.
    ///
    /// The clock [`HANDOVER_WAIT`] is measured against, and it lives here
    /// because a dispatcher builds itself fresh for every pass — the lane
    /// records file is the only thing that carries a number from one pass to
    /// the next. It dies with the record, which is dropped the moment the lane
    /// is freed, so a lane that hands its pane over cleanly never carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handing_over_since: Option<i64>,
    /// The pane [`Dispatcher::retire`] left standing when its lane's task
    /// routed back to the very step it was already on. `None` for every
    /// ordinary lane.
    ///
    /// A cross-step handover has a next pass ready to spend it, because that
    /// pass is the one starting the step after it — see [`PaneHandover`]. A
    /// step that retired on its own name has no such pass: the same name is
    /// what starts again, on some later pass, and by then a fresh
    /// `Dispatcher` has forgotten anything that was not written down. This is
    /// the write-down, kept under the retired lane's own name so
    /// `start_lanes` finds it the moment that name is started again — and
    /// removed then, so it is spent at most once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retired_pane: Option<RetiredPane>,
    /// When this lane was first seen holding a process it started open while
    /// the transcript signal called it silent. `None` whenever the two
    /// signals agree — the lane is settled and quiet, or genuinely busy —
    /// which is what lets [`Dispatcher::note_progress`] restart the clock the
    /// moment the child goes away rather than the moment somebody notices it
    /// has: a lane that becomes busy again for a real reason gets the
    /// ceiling's full hour, not whatever was left of an old one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    child_since: Option<i64>,
}

/// A pane [`Dispatcher::retire`] left behind for its own lane name to inherit,
/// the next time that name is started — see [`LaneRecord::retired_pane`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RetiredPane {
    pane_id: String,
    /// Whether the pane is empty and standing at its shell —
    /// [`crate::mux::Vacated::Shell`]. False is a session that would not
    /// leave: the pane is only closed once the lane started in its place has
    /// a pane of its own, exactly as a [`PaneHandover`] that is not `ready`.
    ready: bool,
}

impl LaneRecord {
    /// A record for a lane this dispatcher did not start — an interrupted pass
    /// picking up where it left off. There is nothing to attribute its usage to,
    /// so the ledger fields stay blank and only `started_at` and
    /// `last_progress` are real.
    fn adopted(now: i64) -> LaneRecord {
        LaneRecord {
            started_at: now,
            last_progress: now,
            output_hash: 0,
            notified: false,
            reminded_at: None,
            reminders: 0,
            session: String::new(),
            kind: String::new(),
            agent: String::new(),
            model: String::new(),
            head: String::new(),
            held_for_block: false,
            person_turn_busy: false,
            handing_over_since: None,
            retired_pane: None,
            child_since: None,
        }
    }

    /// [`LaneRecord::adopted`], with the usage ledger's own last line for
    /// `name` recovered into it where one exists.
    ///
    /// `lanes.json` is only written back at the end of a pass — see
    /// `Dispatcher::pass`, which now writes it on the error paths too but
    /// still not if the process is killed outright — so a dispatcher that
    /// dies between launching a lane and finishing that pass loses the
    /// in-memory record the very same launch just built, and the next pass
    /// re-adopts the lane with nothing to attribute its usage to. Blank
    /// fields there used to mean
    /// `record_usage` quietly banked nothing for it — inverting the one
    /// promise the ledger makes, that the runs which went wrong are the ones
    /// it remembers best. The ledger itself still has the lane's `session`,
    /// `kind`, `agent` and `model` from whatever it last banked under this
    /// same name, so this reads them back rather than leaving them blank.
    ///
    /// Still blank, same as `adopted`, for a lane the ledger has never heard
    /// from: one from a spoolway that predates it, or a project whose `args`
    /// drop `{session_id}`. Nothing here can recover what was never written
    /// down.
    pub(crate) fn readopted(name: &str, now: i64, ledger: &[crate::usage::Entry]) -> LaneRecord {
        let mut record = LaneRecord::adopted(now);
        if let Some(entry) = ledger
            .iter()
            .rev()
            .find(|entry| lane_name(&entry.step, &entry.task) == name && !entry.session.is_empty())
        {
            record.session = entry.session.clone();
            record.kind = entry.kind.clone();
            record.agent = entry.agent.clone();
            record.model = entry.model.clone();
        }
        record
    }

    /// Remove the per-session agent home spoolway made for this lane, once
    /// the lane has been banked and its task archived.
    ///
    /// Only a kind that mints its own session id and will not take one —
    /// codex today — is handed a home of its own (see [`crate::agent::Home`]);
    /// for every other kind [`crate::agent::session_home`] answers `None` and
    /// this is a no-op. Left behind, that home holds a link — or, on a
    /// platform that will not make one, a full copy — of `auth.json` per
    /// lane, which after a credential rotation is so much stale litter
    /// (review finding 63).
    pub(crate) fn reclaim_session_home(&self) {
        if self.session.is_empty() || self.kind.is_empty() {
            return;
        }
        if let Some(home) = crate::agent::session_home(&self.kind, &self.session) {
            let _ = std::fs::remove_dir_all(home);
        }
    }

    /// A stub record carrying nothing but the pane [`Dispatcher::retire`]
    /// left standing. Its usage fields are blank like [`LaneRecord::adopted`]'s
    /// — `record_usage` is a no-op on an empty `session`, so a stale caller
    /// still holding this record from before it retired cannot bank it twice.
    fn retired(now: i64, pane: RetiredPane) -> LaneRecord {
        LaneRecord {
            retired_pane: Some(pane),
            ..LaneRecord::adopted(now)
        }
    }
}

/// A pane a finished lane left behind, on its way to the task's next step.
///
/// Built by [`Dispatcher::free_finished_lanes`] and spent by
/// [`Dispatcher::start_lanes`] in the same pass — one pass is where a handover
/// happens, because the pass that frees a lane is the pass that starts the
/// step after it. Anything still here at the end of the pass is closed rather
/// than left in the tab.
#[derive(Debug, Clone)]
struct PaneHandover {
    /// The lane the pane came from. Its record goes when the pane does.
    lane: String,
    pane_id: String,
    /// Whether the pane is empty and standing at its shell, and so can simply
    /// be started in again — [`crate::mux::Vacated::Shell`].
    ///
    /// False is a pane that still has an agent in it: the session was asked to
    /// leave and did not, or its lane never settled at all. Such a pane is
    /// never handed to the next step, and is only closed once that step's own
    /// pane has been split.
    ready: bool,
}

pub struct Dispatcher<'a> {
    // `pub(crate)`: read and written directly by `crate::teardown`'s own
    // `impl Dispatcher` block, the same way every method here already does.
    pub(crate) repo: &'a Repo,
    pub(crate) pipelines: &'a Pipelines,
    pub(crate) mux: &'a dyn Mux,
    pub(crate) lanes: HashMap<String, LaneRecord>,
    pub(crate) dry_run: bool,
    /// Whether this run stops for a person. Settled once when the run takes its
    /// lock and carried here rather than re-read per decision, so that one pass
    /// cannot make half its choices in each mode.
    unattended: bool,
    /// What each session in the usage ledger has already banked, by session
    /// id — folded once per pass out of [`Dispatcher::ledger`], rather than
    /// once per lane in [`Dispatcher::record_usage`], which used to re-read
    /// the whole ledger for every lane a pass tore down. Updated in place as
    /// this pass banks more, so a second lane banked against the same carried
    /// session within one pass still sees the first one's totals without a
    /// second read of the file.
    ///
    /// `None` until the first lane this pass actually banks — a pass that
    /// tears nothing down never builds it. Populating it in
    /// [`Dispatcher::new`] unconditionally was the bug: every pass paid to
    /// parse the whole ledger whether or not it had anything to bank.
    usage_banked: Option<HashMap<String, BankedTotals>>,
    /// The whole ledger, parsed once on first use and shared for the rest of
    /// the pass — see [`Dispatcher::ledger`]. The ceiling checks,
    /// `carried_session`, `readopted` and `lane_session` all used to parse
    /// `usage.jsonl` end to end on their own (review finding 33); now they
    /// answer from this one snapshot, and `usage_banked` is folded out of it
    /// rather than triggering a second read. `None` until something in the
    /// pass first needs the ledger, for the same reason `usage_banked` is.
    ledger: Option<std::sync::Arc<Vec<crate::usage::Entry>>>,
    /// Each task's `last_report.at` as this pass first read it, by task id.
    /// A lane's `spoolway report` is the only thing that ever advances that
    /// field, so a disk copy whose value is higher than this means a report
    /// landed mid-pass — and [`Dispatcher::persist`] must not then write the
    /// pass's stale in-memory copy over it. Rebuilt at the top of every
    /// [`Dispatcher::run_pass`]. See review finding 2.
    report_seen: HashMap<String, i64>,
}

/// What one session has banked to the usage ledger so far — the running
/// total [`Dispatcher::record_usage`] diffs a transcript's own cumulative
/// reading against, so a resumed session is never counted twice.
#[derive(Debug, Clone, Default)]
struct BankedTotals {
    tokens: crate::usage::Tokens,
    turns: u32,
    cost_usd: f64,
}

/// A task that wants a lane started for it, and the step to start.
struct Candidate {
    task_index: usize,
    step_id: String,
    priority: i32,
    /// How many steps its pipeline has in total, so the sort key below can
    /// turn `priority` — a raw index, meaningless across pipelines of
    /// different lengths — into steps left, which is comparable across any
    /// two. Carried here rather than looked up again at sort time because a
    /// `Candidate` no longer borrows the `Pipeline` it came from.
    pipeline_len: usize,
    /// The gate's answer for this candidate — see [`group_gate_holds`]. The
    /// first sort tier: `false` (its own group is open, or the gate does
    /// not apply) ranks ahead of `true`, so a never-run group only loses
    /// ties against one already landing rather than being dropped from the
    /// pass outright.
    gated: bool,
    /// Unfinished tasks in this task's group. Fewer is more urgent.
    group_open: usize,
    /// Tasks this one is holding up. More is more urgent.
    dependents: usize,
    pipeline: String,
}

impl Candidate {
    /// Steps still ahead of this candidate in its own pipeline, zero at the
    /// last step. The second sort tier, under the gate: fewer is more
    /// urgent, whatever pipeline — and whatever length pipeline — the task
    /// is on.
    ///
    /// The subtraction lives here rather than on [`Pipeline::priority`],
    /// which keeps returning a step's raw index: that number means something
    /// on its own pipeline and nothing across two of different lengths, so
    /// turning it into a comparable quantity is this sort key's job, not the
    /// pipeline type's.
    fn steps_left(&self) -> i32 {
        self.pipeline_len as i32 - 1 - self.priority
    }
}

/// What [`Dispatcher::route_reserved_stage`] decided the loop in
/// [`Dispatcher::collect_candidates`] should do next, once a task's stage was
/// checked against the dispatcher's own reserved ones.
enum Routed {
    /// The stage was not one of the reserved ones (or was a staffed
    /// `blocked`, which settles like any other step) — carry on past it into
    /// the ordinary per-step handling.
    NotReserved,
    /// This task is done for the pass — the caller's `'tasks` loop should
    /// move on to the next one.
    NextTask,
    /// The stage moved to one worth re-examining within this same pass — the
    /// caller's inner loop should round again rather than wait for the next
    /// pass.
    RetryStep,
}

/// What reading back the issue-tracking hook's exit code says about a task
/// sitting on a stage a hook was fired for — see
/// [`Dispatcher::tracking_gate`].
enum TrackingGate {
    /// No hook is configured to hold this stage, or this is a dry run with
    /// nothing to read yet — proceed as if nothing were asked.
    Inactive,
    /// The hook fired but has not exited yet.
    Pending,
    /// The hook exited clean.
    Clean,
    /// The hook exited nonzero, with the code.
    Failed(i32),
}

/// What [`Dispatcher::collect_candidates`]'s loop should do with a step once
/// [`fall_through`] has weighed in — see its own doc comment for why a step
/// falls through at all.
enum FallThrough {
    /// This step runs normally for this task; keep it.
    Runs,
    /// It falls through, but its own `on_pass` names nowhere to go — nothing
    /// more this task can do this pass.
    Stuck,
    /// It falls through to `destination`, for the reason named.
    To {
        destination: String,
        why: &'static str,
    },
}

/// Whether `step` falls through to `on_pass` for `task` without starting a
/// lane, and why.
///
/// A step named in the task's own `skip:` list does not run for it. It is
/// not skipped in the routing sense — the task arrives here and leaves by
/// `on_pass` exactly as a passing lane would have sent it — it just does not
/// start one on the way through.
///
/// Nothing in this binary writes that list any more — `spoolway eval
/// --replay`, the one command that used to, is gone, and `skip:` is kept on
/// the task only so an old task file written under it still parses. A
/// pipeline itself cannot skip anything except through `last:`, below.
///
/// `last: true` says this step belongs to the chain rather than to any one
/// task in it, so only the task at the top runs it.
///
/// The question is whether anything is still open *above* this task — an
/// unfinished task that depends on it, at any depth. `Graph`'s reverse edges
/// are built from the queue, which is the open set: a finished task is
/// archived out of it, so a dependent that counts here is one that has yet
/// to run. That is the whole difference from the retired `when: last`, which
/// asked whether the pipeline's *declared* graph gave this task dependents
/// and so found none at the top and everything below it forever.
///
/// A chain names one task. A fan names all of them, because no branch there
/// contains another and each pull request stands alone. A task with no
/// dependents and no group runs it too: there is no chain to be last in.
fn fall_through(step: &Step, task: &Task, dependents: usize) -> FallThrough {
    let skipped_by_name = task.front.skip.iter().any(|named| named == &step.id);
    let walked_past_as_not_last = step.last && dependents > 0;

    if !skipped_by_name && !walked_past_as_not_last {
        return FallThrough::Runs;
    }
    match step.on_pass.clone() {
        None => FallThrough::Stuck,
        Some(destination) => FallThrough::To {
            destination,
            why: if skipped_by_name {
                "skip"
            } else {
                "not last in its chain"
            },
        },
    }
}

impl<'a> Dispatcher<'a> {
    pub fn new(
        repo: &'a Repo,
        pipelines: &'a Pipelines,
        mux: &'a dyn Mux,
        dry_run: bool,
    ) -> Dispatcher<'a> {
        Dispatcher {
            repo,
            pipelines,
            mux,
            lanes: load_lane_records(repo),
            dry_run,
            // From the lock this run already holds, so the dispatcher and every
            // lane's own `spoolway report` answer the question the same way.
            unattended: repo.unattended(),
            usage_banked: None,
            ledger: None,
            report_seen: HashMap::new(),
        }
    }

    /// Save `task`, but only under its per-task lock and only if no lane's
    /// `spoolway report` has landed on the file since this pass read it —
    /// see [`Dispatcher::report_seen`] and review finding 2. When a report
    /// has landed, the pass's in-memory copy is stale: the write is dropped
    /// and the next pass redoes this pass's bookkeeping against what the
    /// lane actually wrote.
    ///
    /// The lock is held only around the reload check and the write — never
    /// across a multiplexer call, which every caller already sequences
    /// before or after its own `persist`.
    fn persist(&self, task: &mut Task) -> Result<()> {
        persist_task(self.repo, task, &self.report_seen)
    }

    /// The whole usage ledger, parsed once per pass and shared thereafter.
    /// Deferred to first use — most passes tear nothing down and never read it
    /// — see [`Dispatcher::ledger`](Self::ledger)'s field doc.
    pub(crate) fn ledger(&mut self) -> std::sync::Arc<Vec<crate::usage::Entry>> {
        let repo = self.repo;
        self.ledger
            .get_or_insert_with(|| {
                std::sync::Arc::new(crate::usage::read(repo).unwrap_or_default())
            })
            .clone()
    }

    /// This pass's own running total per session, folded out of
    /// [`Dispatcher::ledger`] on first use and kept for the rest of the pass.
    /// Updated in place as this pass banks more, so a second lane banked
    /// against the same carried session sees the first one's totals.
    fn usage_banked(&mut self) -> &mut HashMap<String, BankedTotals> {
        if self.usage_banked.is_none() {
            let ledger = self.ledger();
            self.usage_banked = Some(banked_totals(&ledger));
        }
        self.usage_banked.as_mut().expect("just set")
    }

    /// One full reconciliation: read the queue and the multiplexer, decide
    /// what each task's stage means for it this pass, and spend whatever
    /// budget is left starting the work that leaves ready.
    ///
    /// The two big steps in the middle are
    /// [`Dispatcher::collect_candidates`], which walks every task once and
    /// either settles it on the spot or turns it into a raw candidate to
    /// start a lane, and [`Dispatcher::rank_candidates`], which turns that
    /// raw list into the order `start_lanes` spends its budget in.
    pub fn pass(&mut self) -> Result<Report> {
        let outcome = self.run_pass();
        // `lanes.json` is written whatever came of the pass — error paths
        // included. A `?` partway through `run_pass` used to return before
        // its final save, discarding every lane record built earlier in the
        // same pass: the next pass re-adopted those lanes with a stale or
        // blank session, and their spend was never banked. See review
        // finding 8.
        if let Err(err) = save_lane_records(self.repo, &self.lanes) {
            crate::problem_log::append(
                self.repo,
                &format!("could not write lanes.json after this pass: {err}"),
            );
        }
        outcome
    }

    fn run_pass(&mut self) -> Result<Report> {
        let mut report = Report::default();
        let step_ids = self.pipelines.all_step_ids();
        let all_lanes = self.mux.list_lanes()?;

        // Fire any cron job whose expression matches this minute, before the
        // queue is read below, so its freshly queued documents are dispatched
        // by this same pass. A dry run makes no changes and so fires nothing.
        // Trouble with a job is reported like any other pass trouble and
        // never fails the pass.
        if !self.dry_run {
            crate::jobs::fire_due(
                self.repo,
                self.pipelines,
                &mut report.actions,
                &mut report.problems,
            );
        }

        let (mut tasks, load_problems) = self.repo.tasks_and_problems()?;

        // What each task's `last_report` reads as right now, so a `persist`
        // later this pass can tell a report that landed while the pass was
        // working from the pass's own stale copy. See [`Dispatcher::persist`].
        self.report_seen = tasks
            .iter()
            .map(|task| {
                (
                    task.id().to_string(),
                    task.front
                        .last_report
                        .as_ref()
                        .map_or(0, |report| report.at),
                )
            })
            .collect();
        // A queue file that will not parse no longer fails the pass — it is
        // skipped and named here, so a person sees which file to fix rather
        // than a board that has silently stopped moving. See
        // [`crate::task::load_dir`].
        for problem in &load_problems {
            report.problems.push(format!(
                "{} did not parse and was skipped: {}",
                problem.path.display(),
                problem.error
            ));
        }

        // A task queued before bases were recorded has none. The branch this
        // dispatcher's own checkout is on is what it would have been given, so
        // it is what it is cut from — decided once here rather than at each of
        // the several places below that ask a task what it is based on.
        if let Ok(branch) = self.repo.branch() {
            for task in &mut tasks {
                task.front.base.get_or_insert_with(|| branch.clone());
            }
        }

        let mine = our_checkouts(self.repo, &tasks);

        // Only sessions we named, in a directory that is ours, are ours.
        // Anything else in this multiplexer belongs to a person or to another
        // project, and is never counted, prompted, or torn down.
        let mut owned: Vec<(String, String, &Lane)> = all_lanes
            .iter()
            .filter(|lane| owns_cwd(&mine, &lane.cwd))
            .filter_map(|lane| {
                parse_lane_name(&lane.name, &step_ids)
                    .map(|(step, task)| (step.to_string(), task.to_string(), lane))
            })
            .collect();

        // The dependency graph, resolved once for the whole pass. It owns its
        // data, so the rest of the pass is free to keep writing task files.
        let graph = Graph::build_for_run(
            &tasks,
            self.pipelines,
            &self.repo.archive_dir(),
            self.unattended,
        );

        // The list is read once at the top of the pass, so a pane closed during
        // it would otherwise be looked at again by the rest of the pass as if it
        // were still there. That matters for exactly one lane: the pane held
        // open for a task that has since been unblocked, whose step is the
        // task's current one again — the rest of the pass would take it for a
        // live session sitting on that step, tell a person it is waiting, and
        // leave the step unstarted for a pass.
        //
        // The panes those lanes hand back go in `handovers` — see
        // [`PaneHandover`]. Filled here, spent by `start_lanes` below, and
        // emptied before this pass returns.
        let mut handovers: HashMap<String, PaneHandover> = HashMap::new();
        let freed = self.free_finished_lanes(&owned, &mut tasks, &mut handovers, &mut report)?;
        owned.retain(|(_, _, lane)| !freed.contains(&lane.name));

        let (candidates, archived) =
            self.collect_candidates(&mut tasks, &owned, &graph, &mut handovers, &mut report)?;
        let candidates = self.rank_candidates(candidates, &tasks, &archived, &graph);

        // The two things that end an unattended run short of an empty queue —
        // checked here, between deciding what could start and starting it, so
        // a run that has spent its budget stops *opening* lanes without
        // interrupting any that are mid-turn: a lane killed halfway has spent
        // its tokens and produced nothing, which is the one way to make a
        // ceiling cost more than it saves. Tokens first, since it is the
        // ceiling most configs still carry; whichever fires, the other is
        // moot for this pass.
        match self
            .over_output_ceiling()
            .or_else(|| self.over_cost_ceiling())
        {
            Some(note) => {
                report.actions.push(note.clone());
                report.ceiling = Some(note);
            }
            None => {
                self.start_lanes(&mut tasks, &owned, candidates, &mut handovers, &mut report)?
            }
        }

        // Whatever no step took. A pane handed on is only worth keeping if
        // something starts in it, and one nothing started in is clutter in the
        // task's tab — or, for a pane whose session never left, an agent still
        // sitting there with nothing left to do. Both close here, after every
        // pane that replaces one has already been split.
        let left_over: Vec<PaneHandover> = handovers.into_values().collect();
        for handover in left_over {
            self.close_handover(&handover, &mut report);
        }

        report.quiet = report.actions.is_empty() && owned.is_empty();
        report.lanes_live = !owned.is_empty();
        self.prune_stale_lane_records(&tasks, &step_ids);
        // The write itself is `pass`'s, so an early `?` above still leaves
        // the lane records it built on disk — see [`Dispatcher::pass`].

        Ok(report)
    }

    /// Walk every task once, deciding what its stage means for it this pass.
    ///
    /// Three things can happen to a task, in order: its stage may be one of
    /// the dispatcher's own reserved ones, handled entirely by
    /// [`Dispatcher::route_reserved_stage`]; its step may fall through to
    /// `on_pass` without starting a lane, decided by [`fall_through`]; or
    /// what is left settles on the spot (a terminal or command step) or
    /// becomes a raw [`Candidate`] (an agent step with nothing already
    /// running in its pane). Neither of the first two is sorted or gated
    /// yet — see [`Dispatcher::rank_candidates`] for that.
    ///
    /// Answers with the raw candidates and the ids of tasks this pass
    /// archived.
    #[allow(clippy::needless_range_loop)]
    // `tasks[index]` is borrowed and re-borrowed across helper calls
    fn collect_candidates(
        &mut self,
        tasks: &mut [Task],
        owned: &[(String, String, &Lane)],
        graph: &Graph,
        handovers: &mut HashMap<String, PaneHandover>,
        report: &mut Report,
    ) -> Result<(Vec<Candidate>, Vec<String>)> {
        let mut archived: Vec<String> = Vec::new();
        let mut candidates: Vec<Candidate> = Vec::new();

        'tasks: for index in 0..tasks.len() {
            let pipeline = match self.pipelines.for_task(&tasks[index]) {
                Ok(pipeline) => pipeline.clone(),
                Err(err) => {
                    report.problems.push(format!("{err:#}"));
                    continue;
                }
            };

            // A step carrying `last:` that this task is not last for, or whose
            // id is in the task's own `skip:`, falls through to `on_pass`
            // without starting a lane — see [`fall_through`]. That can chain
            // (`skip: [handover, document, land]` walks all three), so the
            // stage is re-examined right away rather than
            // waiting for the next pass, bounded by the pipeline's step count
            // so a fall-through cycle in a pipeline's own wiring cannot spin
            // this pass forever. [`Dispatcher::route_reserved_stage`]'s own
            // `RetryStep` rounds the same loop for the same reason, on the
            // one reserved stage (`queued`) that can move a task straight
            // into a step worth looking at again this same pass.
            let mut step = None;
            for _ in 0..=pipeline.steps.len() {
                let stage = tasks[index].stage().to_string();

                match self.route_reserved_stage(
                    &mut tasks[index],
                    index,
                    &stage,
                    &pipeline,
                    graph,
                    owned,
                    &mut candidates,
                    &mut archived,
                    report,
                )? {
                    Routed::NextTask => continue 'tasks,
                    Routed::RetryStep => continue,
                    Routed::NotReserved => {}
                }

                let Some(this_step) = pipeline.step(&stage).cloned() else {
                    report.problems.push(format!(
                        "{}: sitting on `{stage}`, which pipeline `{}` does not define",
                        tasks[index].id(),
                        pipeline.name
                    ));
                    continue 'tasks;
                };

                // A background command was walked away from on purpose, so the
                // step that started it will never look at it again — this is
                // the only place left that can. Without it, `background: true`
                // plus a task that blocks on the way is a process nothing ever
                // stops.
                self.reap_stale_runs(&tasks[index], &pipeline, report);

                match fall_through(
                    &this_step,
                    &tasks[index],
                    graph.dependents(tasks[index].id()),
                ) {
                    FallThrough::Runs => {
                        step = Some(this_step);
                        break;
                    }
                    FallThrough::Stuck => continue 'tasks,
                    FallThrough::To { destination, why } => {
                        report.actions.push(format!(
                            "{}: `{}` does not run for this task ({why}) — moving to \
                             `{destination}`",
                            tasks[index].id(),
                            this_step.id,
                        ));
                        tasks[index].set_stage(&destination, None);
                        self.persist(&mut tasks[index])?;
                        continue;
                    }
                }
            }
            let Some(step) = step else {
                report.problems.push(format!(
                    "{}: fall-through did not settle on a step within {} tries — \
                     the pipeline's `on_pass` wiring likely cycles",
                    tasks[index].id(),
                    pipeline.steps.len() + 1,
                ));
                continue;
            };

            // A quota probe or a usage-limit pane parked this task — see
            // `Dispatcher::quota_over_ceiling` and `Dispatcher::
            // usage_limit_hold`, both below. Read here, ahead of everything
            // else a pass does with a task on a real step; the other half of
            // the same gate sits in `route_reserved_stage`, for a task that
            // is still on `queued`. See [`Dispatcher::parked`].
            if self.parked(&mut tasks[index], report)? {
                continue;
            }

            let lane = owned
                .iter()
                .find(|(lane_step, lane_task, _)| {
                    *lane_step == step.id && lane_task == tasks[index].id()
                })
                .map(|(_, _, lane)| *lane);

            match step.kind() {
                StepKind::Terminal => {
                    if step.cleanup && self.clean_up(&mut tasks[index], owned, report)? {
                        archived.push(tasks[index].id().to_string());
                    }
                }

                StepKind::Command => {
                    let id = tasks[index].id().to_string();
                    let Some(destination) = self.run_command(&mut tasks[index], &step, report)?
                    else {
                        continue;
                    };
                    // A command step's own `on_fail` is a route like any
                    // other, and a mechanical gate's whole point is to fail
                    // back to the step behind it — so its arrival is bound by
                    // that step's `loop:` exactly as an agent's report would
                    // be, or a gate that never turns green never stops.
                    let destination = crate::commands::apply_loop_budget(
                        &pipeline,
                        &mut tasks[index],
                        &step.id,
                        destination,
                        self.unattended,
                    );

                    // The third road to `blocked`, and the one nothing used to
                    // write an origin for. `report` records it on its own route
                    // there, and `escalate` on its own; a command step's failure
                    // is no different, whether its `on_fail` named `blocked`
                    // outright or a spent budget above redirected it there.
                    // Without this the task arrives with no `blocked_from`, and
                    // `resume_target` has nothing to carry it back to.
                    if destination == crate::pipeline::BLOCKED {
                        crate::commands::set_blocked_from(&mut tasks[index], &step.id);
                    }

                    // Same two shapes a watch step routes into, for the same
                    // reason: a step that runs an agent has to queue for a slot
                    // like any other work, and everything else is a stage
                    // change with nothing to wait for.
                    match self.queues_for_a_slot(&pipeline, &destination) {
                        true => candidates.push(Candidate {
                            task_index: index,
                            priority: pipeline.priority(&destination),
                            pipeline_len: pipeline.steps.len(),
                            step_id: destination,
                            // Filled in once, below, after every candidate
                            // this pass could possibly field is collected —
                            // see the gate tier comment ahead of the sort.
                            gated: false,
                            group_open: graph.group_open(&id),
                            dependents: graph.dependents(&id),
                            pipeline: pipeline.name.clone(),
                        }),
                        false => {
                            tasks[index].set_stage(&destination, None);
                            self.persist(&mut tasks[index])?;
                            let cleans = pipeline
                                .step(&destination)
                                .is_some_and(|s| s.kind() == StepKind::Terminal && s.cleanup);
                            if cleans && self.clean_up(&mut tasks[index], owned, report)? {
                                archived.push(id);
                            }
                        }
                    }
                }

                StepKind::Agent => {
                    // The context ceiling, checked first and ahead of every
                    // other question this arm asks: a lane that has already
                    // read past `session_blocked_ctx` is stopped on the spot,
                    // busy or settled-but-not-yet-reported alike, rather than
                    // let run another turn toward a wall that a fresh
                    // `spoolway report` contract could not survive.
                    // `escalate_clock` is the same road a dead lane takes, so
                    // its usage is banked and its task lands on `blocked` the
                    // same way.
                    //
                    // **Not a lane that has already reported.** A step that
                    // routes back to itself — `blocked` reporting `--block`
                    // again — is the same lane name settled on the same step
                    // a pass later, and the settled arm below tells that
                    // apart from a genuinely stuck lane by reading whether
                    // the task's own `last_report` lands after this lane
                    // `started_at`. The same read here, ahead of the same
                    // question: a lane that already reported is not "live"
                    // in the sense the ceiling means, and escalating it would
                    // overwrite a real report with a blocker claiming the
                    // conversation is gone when it plainly is not — see
                    // `retire`, which is what a reported lane is for.
                    let already_reported = lane.is_some_and(|lane| {
                        lane.status.is_settled()
                            && self
                                .lanes
                                .get(&lane.name)
                                .map(|record| record.started_at)
                                .is_some_and(|since| tasks[index].reported_since(since))
                    });
                    // Ahead of the context ceiling and everything after it: a
                    // lane whose pane already carries its kind's usage-limit
                    // phrase is not a context problem, and reading its
                    // transcript for one would be wasted work on a lane that
                    // is about to be left alone either way. Checked whether
                    // the lane reads `Working` or settled — the one thing
                    // `check_unreported`'s own hold, further down, could
                    // never do, since it only ever ran once a lane had
                    // stopped — so a limit surface that keeps ticking (the
                    // agent's own "continuing automatically") is caught the
                    // same pass it lands rather than only once it goes quiet.
                    if !already_reported
                        && let Some(lane) = lane
                        && self.usage_limit_park(&mut tasks[index], &step, lane, report)?
                    {
                        continue;
                    }
                    if !already_reported
                        && let Some(lane) = lane
                        && let Some(reason) = self.ctx_ceiling_hold(&step, lane)
                    {
                        self.escalate_clock(
                            &mut tasks[index],
                            &pipeline,
                            &step,
                            lane,
                            &reason,
                            report,
                        )?;
                        continue;
                    }

                    // A lane of this task's is *working*, so the launch that
                    // made it did not die at launch — which is the only thing
                    // `attempts` counts. Forgiven here, at the first pass that
                    // can see the evidence, so the counter never outlives the
                    // question it answers.
                    //
                    // Busy, and not merely present. A settled lane that never
                    // reported is exactly what an agent binary dying at launch
                    // looks like from here, and the counter is what is left if
                    // its pane then goes too. A settled lane that *did* report
                    // has `set_stage` behind it, which zeroes the counter on
                    // every report — whether or not the destination it landed
                    // on differs from where the lane started. It used to be
                    // enough to say that difference was the tell; on a step
                    // that routes back to itself, like `blocked`, it is not,
                    // which is what the settled arm twenty lines below now
                    // reads `last_report` for instead of inferring from stage
                    // movement.
                    let running = lane.is_some_and(|lane| lane.status.is_busy());
                    if running && !self.dry_run && tasks[index].launch_landed() {
                        self.persist(&mut tasks[index])?;
                    }
                    match lane {
                        // A busy lane is working. There is nothing to *decide*
                        // about one until its turn ends. The
                        // watchdog that used to run here read a profile's
                        // `max_runtime` and `silence_timeout`, both retired:
                        // a count of reminders is now the only thing that
                        // ever takes a lane away from itself, and that count
                        // is only meaningful once a lane has settled.
                        //
                        // What is left is bookkeeping, not a clock: a lane
                        // mid-turn is not holding a question, so the mark that
                        // says it is comes off. Only `Working` clears it —
                        // `Blocked` is busy too, and means the very thing the
                        // mark is for.
                        Some(lane) if lane.status.is_busy() => {
                            if lane.status == LaneStatus::Working {
                                self.withdraw_waiting(&lane.name);
                            }
                            // A parked task whose lane is busy again is not
                            // waiting on a launch — a person typed straight
                            // into the pane `spoolway resume` just put back on
                            // this step, and that turn is what is running.
                            // Spending `parked_from` (and the one-shot
                            // `resume` it travels with) here is what
                            // `start_one` would otherwise do on the launch
                            // this lane no longer needs; sending it the
                            // report contract on top of what the person just
                            // asked for is exactly what `park_prompt`'s own
                            // doc warns a busy lane must never get.
                            self.unpark_quietly(&mut tasks[index], &step, report)?;
                        }

                        // The lane finished a turn and its task is still on this
                        // step, so either it asked something and is waiting for an
                        // answer in its own pane, or it stopped without reporting —
                        // or it reported and this pass simply has not caught up to
                        // the stage move yet, a report and a reminder racing on the
                        // same pass. Stage movement answers the first two correctly
                        // by itself; the race is what reading `last_report.at`
                        // against this lane's own `started_at` is for instead, which
                        // needs no counter and nothing cleared, and never mistakes a
                        // lane that already reported for one still holding a
                        // question.
                        //
                        // Either way the board is marked once, naming the pane to
                        // go and look at.
                        Some(lane) if lane.status.is_settled() => {
                            self.announce_waiting(&lane.name);

                            let started_at = self.lanes.get(&lane.name).map(|r| r.started_at);
                            if started_at.is_some_and(|since| tasks[index].reported_since(since)) {
                                self.retire(&tasks[index], &pipeline, &step, lane, report)?;
                                continue;
                            }

                            // A person's own Escape, not a forgotten report —
                            // read off the transcript itself, ahead of the
                            // `lane_quiet` gate `check_unreported` opens with,
                            // so this is caught within the pass that sees it
                            // settled rather than after the quiet timer nudges
                            // it. See `Dispatcher::lane_ended_on_abort`.
                            if self.lane_ended_on_abort(lane) {
                                self.park_after_interrupt(&mut tasks[index], &step, lane, report)?;
                                continue;
                            }

                            // A gated step used to be exempted here, on the grounds
                            // that it was *expected* to end its turn on a question
                            // and a person taking an hour to answer is not a stuck
                            // lane. There is no such turn any more: a gated lane is
                            // told a person will read its pane, but asked for
                            // nothing that would end its turn early, and it reports
                            // like any other before its task lands on `paused` —
                            // which this arm never sees, because the stage moved.
                            // What is left settled on a gated step is a lane that
                            // did not report, and that is the same fault it is
                            // anywhere else.
                            self.check_unreported(
                                &mut tasks[index],
                                &pipeline,
                                &step,
                                lane,
                                report,
                            )?;
                        }

                        // The multiplexer cannot say what this session is doing.
                        // Not knowing is not a reason to act on it: leave the pane
                        // alone and ask again next pass.
                        Some(_) => {}

                        None => {
                            // A step of this task's is not started while an
                            // earlier one is still alive. The lane that moved
                            // the stage reported mid-turn and is still
                            // talking, so it still holds the task's pane —
                            // starting here would put a second live pane in
                            // the task's tab, which is the whole of the bug
                            // this guard closes.
                            if self.handover_pending(
                                &tasks[index],
                                &pipeline,
                                &step.id,
                                owned,
                                handovers,
                                report,
                            ) {
                                continue 'tasks;
                            }
                            let id = tasks[index].id().to_string();
                            candidates.push(Candidate {
                                task_index: index,
                                priority: pipeline.priority(&step.id),
                                pipeline_len: pipeline.steps.len(),
                                step_id: step.id.clone(),
                                // Filled in once, below, after every
                                // candidate this pass could possibly field is
                                // collected — see the gate tier comment ahead
                                // of the sort.
                                gated: false,
                                group_open: graph.group_open(&id),
                                dependents: graph.dependents(&id),
                                pipeline: pipeline.name.clone(),
                            })
                        }
                    }
                }
            }
        }

        Ok((candidates, archived))
    }

    /// Whether `task` is still inside a quota or usage-limit park.
    ///
    /// Checked against nothing but the clock the task's own file carries: a
    /// dispatcher stopped for as long as the wait takes, and started again
    /// from cold, honours the park on its very first pass without taking a
    /// fresh reading.
    ///
    /// An expired park is *not* touched here — the three park fields are left
    /// exactly as the file carries them, and the method only answers `false`
    /// so the task falls straight through to the ordinary walk on the same
    /// pass. The hold is resolved by one of exactly three writes, and each
    /// moves the whole of it — `parked_until`, `parked_window`, `parked_at`
    /// and the `quota_retries` streak — in a single save: a re-park in
    /// [`Dispatcher::start_lanes`], the clear in [`start_one`] once
    /// `mux.start_lane` has actually started the lane, or a stage move.
    /// Nothing that can persist the task before then rewrites any of those
    /// fields: [`ensure_workspace`]'s placement bookkeeping carries them
    /// through byte for byte, and the recheck-count reset waits for the
    /// re-park or launch write rather than taking a save of its own. A start
    /// that is refused therefore leaves the whole hold standing, retry count
    /// and all — a launch that never happened is not an exit — and its final
    /// state does not depend on whether a placement fixup happened to flush.
    /// A pass that reaches none of the three (the task is waiting on a
    /// dependency, say) likewise leaves the whole park on disk with its
    /// deadline simply in the past, which is the honest state: still held,
    /// not yet decided.
    ///
    /// Called from two places, because the two kinds of parked task reach
    /// the decision by different routes. A task sitting on a real step meets
    /// this in [`Dispatcher::collect_candidates`]'s own loop. A task sitting
    /// on `queued` never gets that far — [`Dispatcher::route_reserved_stage`]
    /// turns it into a candidate and sends the loop straight on to the next
    /// task — so without the second call its park was re-probed, re-written
    /// and re-logged on every single pass for as long as the park lasted.
    fn parked(&mut self, task: &mut Task, report: &mut Report) -> Result<bool> {
        let Some(until) = task.front.parked_until else {
            return Ok(false);
        };
        let now = now_secs();
        if until > now {
            let dated = task.front.parked_window == crate::quota::Window::SevenDay.key();
            report.actions.push(format!(
                "{}: parked until {}",
                task.id(),
                crate::task::format_until(until, now, dated),
            ));
            return Ok(true);
        }
        // The deadline has passed. Leave every park field alone and let the
        // task fall through: the re-park, the post-start clear in
        // `start_one`, or a stage move is the one write that resolves the
        // hold, park fields and retry count together. Clearing anything here
        // would let a persist that lands before that decision — a placement
        // fixup in `start_one`, say — flush a hold with only some of it
        // cleared, the half-cleared frame this task removes.
        Ok(false)
    }

    /// Handle a task's stage when it is one of the dispatcher's own reserved
    /// ones — `queued`, `done`, `blocked`, or `paused`, see
    /// `crate::pipeline::RESERVED` — and say what
    /// [`Dispatcher::collect_candidates`]'s loop should do next.
    ///
    /// `queued` is where every task starts, and the *only* place the
    /// dependency gate is applied — unconditionally, rather than because the
    /// pipeline happened to open with a step that consulted it. `done` and
    /// `blocked` are the two ways a task ends, and the model was already
    /// binary: anything that is not `blocked` releases dependents, so a
    /// third declared ending would let downstream work proceed on a task
    /// that never finished.
    ///
    /// These same four names are `[issue_tracking]`'s own four events, and
    /// `RESERVED` already lists exactly them — so firing the hook here,
    /// once, ahead of everything below that is specific to one of the four,
    /// covers every arrival at any of them without repeating the check four
    /// times. `fire` is idempotent for the run's whole lifetime, so a task
    /// sitting on one of these across many passes only ever starts it once.
    /// A dry run starts nothing real, the same way it writes no stage change
    /// below.
    #[allow(clippy::too_many_arguments)]
    fn route_reserved_stage(
        &mut self,
        task: &mut Task,
        task_index: usize,
        stage: &str,
        pipeline: &Pipeline,
        graph: &Graph,
        owned: &[(String, String, &Lane)],
        candidates: &mut Vec<Candidate>,
        archived: &mut Vec<String>,
        report: &mut Report,
    ) -> Result<Routed> {
        if !crate::pipeline::RESERVED.contains(&stage) {
            return Ok(Routed::NotReserved);
        }
        if !self.dry_run {
            let group_open = graph.group_open(task.id());
            if let Err(err) = crate::tracking::fire(self.repo, task, stage, group_open) {
                report.problems.push(format!(
                    "{}: issue_tracking hook on `{stage}`: {err:#}",
                    task.id()
                ));
            }
        }
        if stage == crate::pipeline::QUEUED {
            let id = task.id().to_string();
            // A hook that has not yet exited clean holds the task here
            // rather than let it start — see
            // [`crate::config::IssueTrackingConfig::on_fail`]. `Pending`
            // covers both "still running" and "not started yet", which is
            // not reachable the moment `dry_run` is off, since the fire
            // above already ran this same pass — a real hook (an HTTP call,
            // say) almost never exits inside the few milliseconds `fire`'s
            // own `start` waits for a pid, so holding on anything but a
            // clean exit is what actually stops the task rather than only
            // reacting to a code this pass happened to already have. A
            // clean exit falls through to the dependency gate below, same
            // as `on_fail = "ignore"` always does.
            //
            // `TrackingGate::Inactive` covers both a blank `hook` (or one
            // that fails `is_bare_filename`, which never starts a run) and
            // a dry run — see [`Dispatcher::tracking_gate`] — so this never
            // holds every task at `queued` forever for a project that asked
            // for no hook at all, and never reports a real pass's "would
            // start" as "nothing to do".
            match self.tracking_gate(task, stage) {
                TrackingGate::Inactive | TrackingGate::Clean => {}
                TrackingGate::Failed(code) => {
                    let key = crate::command_step::Runs::key(stage, task.id());
                    task.set_stage(
                        crate::pipeline::PAUSED,
                        Some(&format!(
                            "issue_tracking hook exited {code} on `queued` — see \
                             tracking/{key}"
                        )),
                    );
                    self.persist(task)?;
                    return Ok(Routed::NextTask);
                }
                TrackingGate::Pending => return Ok(Routed::NextTask),
            }
            // Before the dependency gate, and so before this task can become
            // a candidate at all: a park is a clock, and a task waiting one
            // out has no business being ranked for a worker slot.
            //
            // Nothing read the park on `queued` at all before this, which
            // cost two different things. A park the pass could not derive
            // again from a fresh reading was ignored outright and the task
            // started anyway. A park it could derive again was re-probed,
            // re-written and re-logged every `dispatch.interval` — a
            // seven-day-window park appended tens of thousands of
            // `## Status Log` lines to one task file — while the board went
            // on reading the row as an ordinary `queued`, so the run looked
            // wedged rather than held.
            if self.parked(task, report)? {
                return Ok(Routed::NextTask);
            }
            if graph.ready(&id) {
                let next = pipeline.entry().to_string();
                // The same two shapes the Command arm routes into, and
                // for the same reason: only a step that runs an agent
                // has to queue for a slot. Everything else is a stage
                // change with nothing to wait for — and going through
                // the slot allocator with one would mean teaching it
                // about work it has nothing to allocate for, which is
                // why a `run:` entry used to be dropped there in
                // silence and leave its task in `queued` forever.
                match pipeline.step(&next).map(|s| s.kind()) {
                    Some(StepKind::Agent) => candidates.push(Candidate {
                        task_index,
                        priority: pipeline.priority(&next),
                        pipeline_len: pipeline.steps.len(),
                        step_id: next,
                        // Filled in once, below, after every
                        // candidate this pass could possibly field is
                        // collected — see the gate tier comment ahead
                        // of the sort.
                        gated: false,
                        group_open: graph.group_open(&id),
                        dependents: graph.dependents(&id),
                        pipeline: pipeline.name.clone(),
                    }),
                    // A lane's dry run is `start_lanes`' to report;
                    // this is the other kind, and it has to be said
                    // here because the write below is what a dry run
                    // may not do.
                    _ if self.dry_run => report
                        .actions
                        .push(format!("would start `{next}` for {id}")),
                    _ => {
                        task.set_stage(&next, None);
                        self.persist(task)?;
                        // Round the inner loop rather than the outer
                        // one, so the command runs on the pass this
                        // task's dependencies came in rather than the
                        // one after.
                        return Ok(Routed::RetryStep);
                    }
                }
            }
            // A task waiting on a dependency that will never finish —
            // dead or simply not there — is left exactly where it is:
            // unblocking the root revives the whole subtree, and moving
            // it here would mean unblocking each of them by hand
            // instead. Not reported to the ticker: the row's own NEXT
            // column already says this, by the same
            // `crate::commands::dependency_note` the row reads, and a
            // wait that is still waiting next pass, and the pass after
            // that, is not news.
            return Ok(Routed::NextTask);
        }
        if stage == crate::pipeline::DONE {
            // A hook still running, or one that failed under
            // `on_fail = "pause"`, holds the task here rather than let
            // it archive — see
            // [`crate::config::IssueTrackingConfig::on_fail`]. `Pending`
            // covers both "not started yet" (unreachable the moment
            // `dry_run` is off, since the fire above already ran this
            // same pass) and "still running": either way there is
            // nothing to route on yet, so the task waits for a pass
            // that can see a code. A clean exit clears whatever
            // `retry_if_failed` may have left recorded, then falls
            // through to the ordinary archive below, same as
            // `on_fail = "ignore"` always does.
            //
            // `done` has no later step to carry a task past the way
            // `queued` failing into `paused` does, so `retry_if_failed`
            // is the road out this hold needs instead: a failed run is
            // forgotten so the next pass's `fire` above starts it over,
            // rather than this task reading the same stale exit code
            // forever.
            match self.tracking_gate(task, stage) {
                TrackingGate::Inactive => {}
                TrackingGate::Clean => crate::tracking::clear_retry_marker(self.repo, task, stage),
                TrackingGate::Pending | TrackingGate::Failed(_) => {
                    crate::tracking::retry_if_failed(self.repo, task, stage);
                    return Ok(Routed::NextTask);
                }
            }
            // Reaching `done` is what tears the worktree down. It used
            // to be a `cleanup: true` on a declared terminal, which
            // every shipped pipeline wrote identically — a key whose
            // only correct value was the one it always had.
            if self.clean_up(task, owned, report)? {
                archived.push(task.id().to_string());
            }
            return Ok(Routed::NextTask);
        }
        // Both of the two ways a task waits for a person, and both
        // wait on the same verb now: `spoolway resume`. Nothing here
        // starts, times or reroutes either — until then the only
        // correct thing to do with them is leave them alone.
        if stage == crate::pipeline::PAUSED
            || (stage == crate::pipeline::BLOCKED && !pipeline.blocked_is_staffed(self.unattended))
        {
            return Ok(Routed::NextTask);
        }
        // A staffed `blocked` is the one reserved stage left standing —
        // it settles like any other step, so it falls through to the
        // ordinary per-step handling below exactly as a non-reserved
        // stage would.
        Ok(Routed::NotReserved)
    }

    /// Reads back what the issue-tracking hook fired for `stage` (see
    /// `crate::tracking::fire`, called once in
    /// [`Dispatcher::route_reserved_stage`] ahead of both places this is
    /// asked from) said, if anything is configured to hold on it at all.
    ///
    /// `queued` and `done` each wait on a hook the same way — read whether
    /// one is configured to hold this stage, and if so what it said — and
    /// used to ask with the same few lines of code typed out twice. Reading
    /// it here once leaves each call site free to still act on the answer
    /// differently, which they do: `queued` pauses the task on a failing
    /// hook, `done` retries it.
    fn tracking_gate(&self, task: &Task, stage: &str) -> TrackingGate {
        // `!self.dry_run`: a dry run has nothing to read either, since
        // `fire` never ran for it, and holding on `holds_on_fail` alone
        // would report a real pass's "would start" as "nothing to do" —
        // the opposite of what a dry run is for.
        if self.dry_run || !crate::tracking::holds_on_fail(self.repo) {
            return TrackingGate::Inactive;
        }
        match crate::tracking::exit_code(self.repo, task, stage) {
            Some(0) => TrackingGate::Clean,
            Some(code) => TrackingGate::Failed(code),
            None => TrackingGate::Pending,
        }
    }

    /// Turn the raw candidates [`Dispatcher::collect_candidates`] built into
    /// the order `start_lanes` spends its budget in.
    ///
    /// A cycle in `depends_on` is a deadlock, not a wait: none of its
    /// members will ever be ready. Not reported to the ticker either —
    /// every task in it already reads `Unreachable` on the board, through
    /// the same `dependency_note` a dead or missing dependency does above,
    /// and a deadlock that is still a deadlock next pass is not news. This
    /// is a standalone remark, not a description of the retain below it —
    /// `graph.cycles()` used to be walked here to build that line, and
    /// nothing has taken its place.
    fn rank_candidates(
        &self,
        mut candidates: Vec<Candidate>,
        tasks: &[Task],
        archived: &[String],
        graph: &Graph,
    ) -> Vec<Candidate> {
        candidates.retain(|c| !archived.contains(&tasks[c.task_index].front.id));
        // The gate's answer, one lookup per candidate now that every
        // candidate this pass could possibly field already sits in the
        // list — see [`group_gate_holds`] for what it answers and why it no
        // longer drops anything.
        for c in &mut candidates {
            c.gated = group_gate_holds(
                self.repo.config.dispatch.priority,
                graph,
                &tasks[c.task_index],
            );
        }
        // Four tiers, in order:
        //   0. the gate: under `dispatch.priority = "group"`, a candidate
        //      whose own group has never run ranks behind one from a group
        //      already able to move — see [`group_gate_holds`]. A tier
        //      rather than the drop this used to be, so an open group that
        //      has run out of ready work no longer holds a slot idle: a
        //      never-run group only loses ties against one still landing,
        //      it is never removed from the pass;
        //   1. fewest steps left in the pipeline, so what is nearly done
        //      finishes before anything new is started — steps left rather
        //      than raw step index, since pipelines in this project are not
        //      all the same length and a `local` task's step 7 is not a
        //      `default` task's step 7;
        //   2. the group with the fewest tasks left, because a group only
        //      reaches main once all of it is merged — spreading effort over
        //      several groups finishes none of them;
        //   3. within a group, whatever is holding up the most other tasks.
        // Ties fall through to task id, since the sort is stable and tasks load
        // in id order.
        candidates.sort_by_key(|c| {
            (
                c.gated,
                c.steps_left(),
                c.group_open,
                std::cmp::Reverse(c.dependents),
            )
        });
        candidates
    }

    /// Drop any lane record for a task this pass did not read from the queue.
    ///
    /// `free_finished_lanes` only removes a record once the multiplexer's own
    /// lane list shows the pane it belongs to — see its doc comment. A pane
    /// that closed while the dispatcher itself was down is never in that list
    /// again, so its record is never reached that way, and sits in
    /// `lanes.json` forever once the task it belongs to has left the queue.
    /// This is the other half: anything whose task id no longer appears among
    /// `tasks`, or whose name does not even parse into a known step, is
    /// stale by definition and is dropped here instead.
    ///
    /// Checked against `tasks`, the queue this pass already read at the top,
    /// rather than reading the queue directory a second time — the two must
    /// agree on what counts as current, and re-reading risks them not.
    fn prune_stale_lane_records(&mut self, tasks: &[Task], step_ids: &[&str]) {
        self.lanes.retain(|name, _| {
            parse_lane_name(name, step_ids)
                .is_some_and(|(_, task_id)| tasks.iter().any(|t| t.id() == task_id))
        });
    }

    /// Whether `task` is parked in front of a person rather than in front of
    /// this dispatcher — the one question both [`Dispatcher::sweep_on_stop`]
    /// and [`Dispatcher::free_finished_lanes`] need answered the same way, so
    /// a paused task and a staffed-vs-unstaffed blocked one are treated alike
    /// everywhere that matters and not just in the one place each was first
    /// written.
    ///
    /// `paused` always is: the whole stage exists to wait for `spoolway
    /// release` and nothing else ever moves a task off it. `blocked` only
    /// counts when nobody is coming to look — see
    /// [`Pipeline::blocked_is_staffed`] — because a staffed `blocked` lane is
    /// answered by another lane, not by a person, and settles like any other
    /// step.
    pub(crate) fn parked_for_a_person(&self, task: &Task) -> bool {
        parked_for_a_person(self.pipelines, self.unattended, task)
    }

    /// End any lane whose work is done, and take its pane back for the task.
    ///
    /// Taking it back rather than closing it is what gives a task one pane for
    /// its whole life instead of one per step: a kind that has a way to leave
    /// its pane is asked to, and the pane it leaves standing is handed to the
    /// task's next step through `handovers`. Every other kind, and every
    /// backend whose pane is the agent itself, closes as it always did and
    /// hands nothing on.
    ///
    /// With one exception, which is the whole of what a person sees when a task
    /// blocks: a lane whose task has landed on `blocked` keeps its pane, and is
    /// focused once. A stage change and fifteen lines of tail do not tell
    /// anybody what the session was doing when it stopped, and by the time they
    /// come to look the only copy of that is the pane. It is held until the task
    /// leaves `blocked` — closed on the pass after it is unblocked, before the
    /// step's new lane is started, because the two would want the same name.
    ///
    /// Answers with the lanes whose panes are gone, which the rest of the pass
    /// must stop counting as sessions.
    fn free_finished_lanes(
        &mut self,
        owned: &[(String, String, &Lane)],
        tasks: &mut [Task],
        handovers: &mut HashMap<String, PaneHandover>,
        report: &mut Report,
    ) -> Result<HashSet<String>> {
        let mut freed = HashSet::new();
        for (step_id, task_id, lane) in owned {
            let held = self
                .lanes
                .get(&lane.name)
                .is_some_and(|record| record.held_for_block);
            // Both stages that park in front of a person keep their pane, for
            // the same reason: what the session was doing is only in the pane,
            // and by the time somebody comes to look it is the only copy. A
            // paused task's pane is the more useful of the two — it holds the
            // work being approved. `blocked` only counts when nobody is coming
            // to look — a staffed `blocked` lane settles like any other step,
            // and its pane is freed like any other's.
            //
            // ...and only on a backend where a pane is a thing to keep. Same
            // question `tear_down_and_escalate` asks before it holds a
            // blocked one, for the same reason: on headless there is no pane
            // to read, only a turn's process left running with nobody able to
            // look at it. Without this a pass on that backend "held" a paused
            // task's pane forever — nothing ever closes it, because nothing
            // ever unparks a task nobody can see to answer.
            let parked = self.mux.resident_while_waiting()
                && tasks
                    .iter()
                    .find(|t| t.id() == *task_id)
                    .is_some_and(|t| self.parked_for_a_person(t));

            // A held pane is a person's now, and its busy/idle status means
            // something the rest of this loop never has to consider: busy is
            // them mid-round, and idle right after busy is a round finished —
            // the one moment worth reaching into the worktree for. Handled
            // ahead of the general busy check below, which would otherwise
            // skip a busy held pane outright and never notice the round.
            if held && parked {
                self.settle_person_turn(lane, task_id, step_id, tasks, report)?;
                continue;
            }

            if lane.status.is_busy() {
                continue;
            }

            let current = tasks.iter().find(|t| t.id() == task_id);

            if parked {
                // `held` is false here — the `held && parked` case above
                // already claimed and skipped every other pass.
                if self.dry_run {
                    report
                        .actions
                        .push(format!("would keep and focus the pane of {}", lane.name));
                    continue;
                }
                self.hold_for_block(lane, task_id, step_id, current, report);
                continue;
            }

            // Still the task's current step: it is either mid-conversation
            // behind a gate or it exited silently. Both are handled per-task.
            // A held pane is neither — it is the leftover of a block that has
            // since been cleared, and the step it belongs to is about to be
            // started again.
            let still_current = current.map(|t| t.stage() == step_id).unwrap_or(false);
            if still_current && !held {
                continue;
            }

            if self.dry_run {
                report
                    .actions
                    .push(format!("would free pane of {}", lane.name));
                continue;
            }

            // The pane is the task's, not this step's, so it is asked for back
            // rather than closed: the task's next step starts in the pane this
            // one is leaving, and the task keeps one `pane_id` for its whole
            // life. What actually makes a session leave is per kind —
            // see [`crate::agent::Quit`] — and a kind with no gesture, or a
            // backend whose pane *is* the agent, answers
            // [`Vacated::PaneClosed`]: exactly the close-and-re-split this
            // call has always done.
            //
            // The record's kind is preferred over the multiplexer's own
            // because it is what spoolway launched with, and so what the
            // adapter table was read for; a lane this dispatcher did not start
            // has no record, and the multiplexer's answer is all there is.
            let kind = self
                .lanes
                .get(&lane.name)
                .map(|record| record.kind.clone())
                .filter(|kind| !kind.is_empty())
                .unwrap_or_else(|| lane.kind.clone());

            // A pane that will not let go is one task's tab left cluttered, not
            // a reason to abandon the pass: every other task still deserves its
            // turn, and the next pass tries this one again.
            let left = match self.mux.vacate_lane(&lane.name, &kind, &lane.pane_id) {
                Ok(left) => left,
                Err(err) => {
                    report.problems.push(format!("{}: {err:#}", lane.name));
                    continue;
                }
            };
            // `readopted` rather than a plain `remove`: a dispatcher that
            // died between launching this lane and finishing that pass never
            // wrote its record to `lanes.json`, so a restarted one reaching
            // this free with nothing under `lane.name` still recovers what
            // the ledger remembers of it rather than banking nothing for a
            // step that finished clean — see `LaneRecord::readopted`.
            let ledger = self.ledger();
            let record = self
                .lanes
                .remove(&lane.name)
                .unwrap_or_else(|| LaneRecord::readopted(&lane.name, now_secs(), &ledger));
            // A pane that is still there is the task's to hand on. `Shell` is
            // handed to the next step as it stands; `StillOccupied` is a
            // session that would not go, and is only closed once that step's
            // own pane has been split — see [`PaneHandover`].
            if left != Vacated::PaneClosed {
                self.stash_handover(
                    handovers,
                    task_id,
                    PaneHandover {
                        lane: lane.name.clone(),
                        pane_id: lane.pane_id.clone(),
                        ready: left == Vacated::Shell,
                    },
                    report,
                );
            }
            // Banked whether or not the lane was held. `record_usage` diffs
            // the transcript against what `usage_banked` says is already on
            // the ledger, so the hold-time line is subtracted and only the
            // rounds a person added in that pane while it was held are
            // appended now — without this, that spend was lost entirely
            // (review finding 34).
            let pipeline = current
                .and_then(|t| self.pipelines.for_task(t).ok())
                .map(|p| p.name.clone())
                .unwrap_or_default();
            self.record_usage(&record, task_id, step_id, current, &pipeline);
            freed.insert(lane.name.clone());
            report.actions.push(format!("freed {}", lane.name));
        }
        Ok(freed)
    }

    /// Whether `task`'s next step has to wait before it starts, because a lane
    /// of this task's on an earlier step is still alive and still holds the
    /// task's pane.
    ///
    /// A lane runs `spoolway report` mid-turn: the stage moves on the spot and
    /// the lane keeps talking, so the pass that reads the moved stage finds the
    /// old lane still working. `free_finished_lanes` is right to leave a busy
    /// lane alone; what was wrong is that the scan below it never asked, found
    /// no lane under the new step's own name, and started one anyway.
    ///
    /// Bounded by [`HANDOVER_WAIT`], measured from the first pass that wanted
    /// the pane back — kept on the old lane's own record, because a dispatcher
    /// builds itself fresh for every pass. Past the bound the old lane is
    /// written off: what it spent is booked, its record is dropped, and its
    /// pane is put in `handovers` to be closed *after* the next step's pane has
    /// been split.
    fn handover_pending(
        &mut self,
        task: &Task,
        pipeline: &Pipeline,
        step_id: &str,
        owned: &[(String, String, &Lane)],
        handovers: &mut HashMap<String, PaneHandover>,
        report: &mut Report,
    ) -> bool {
        let Some((old_step, _, old)) = owned
            .iter()
            .find(|(lane_step, lane_task, _)| lane_task == task.id() && lane_step != step_id)
        else {
            return false;
        };

        // A dry run may not write a record, and so has no clock to expire
        // against. It says what it sees and leaves it there, which is the
        // truthful answer: this step is waiting on that lane.
        if self.dry_run {
            report.actions.push(format!(
                "{}: `{step_id}` would wait for `{old_step}` to hand the task's pane over",
                task.id()
            ));
            return true;
        }

        let now = now_secs();
        let ledger = self.ledger();
        let record = self
            .lanes
            .entry(old.name.clone())
            .or_insert_with(|| LaneRecord::readopted(&old.name, now, &ledger));
        let since = *record.handing_over_since.get_or_insert(now);
        if now.saturating_sub(since) < HANDOVER_WAIT.as_secs() as i64 {
            report.actions.push(format!(
                "{}: `{step_id}` is waiting for `{old_step}` to hand the task's pane over",
                task.id()
            ));
            return true;
        }

        // Past the bound. A lane that has held the pane this long after its
        // task moved on is not finishing a sentence, and a task whose one pane
        // is waited on for ever would never run another step.
        let stuck = PaneHandover {
            lane: old.name.clone(),
            pane_id: old.pane_id.clone(),
            ready: false,
        };
        if let Some(record) = self.lanes.remove(&stuck.lane) {
            self.record_usage(&record, task.id(), old_step, Some(task), &pipeline.name);
        }
        report.actions.push(format!(
            "{}: `{old_step}` has held the task's pane for {} without finishing — starting \
             `{step_id}` beside it",
            task.id(),
            crate::config::human_duration::format(HANDOVER_WAIT)
        ));
        self.stash_handover(handovers, task.id(), stuck, report);
        false
    }

    /// Put a pane aside for `task_id`'s next step.
    ///
    /// A task has one pane, so it has one entry — and a second pane arriving
    /// for the same task means the one already there is not going to be
    /// started in by anybody. It is closed here rather than dropped, because a
    /// forgotten pane is one nothing ever closes again.
    fn stash_handover(
        &mut self,
        handovers: &mut HashMap<String, PaneHandover>,
        task_id: &str,
        handover: PaneHandover,
        report: &mut Report,
    ) {
        if let Some(displaced) = handovers.insert(task_id.to_string(), handover) {
            self.close_handover(&displaced, report);
        }
    }

    /// Close a pane no step took over, and forget the lane it came from.
    ///
    /// Both halves matter. A pane nothing started in is clutter in the task's
    /// tab, and a lane record outliving its pane is a session the next pass
    /// would go on counting as live. The record is usually gone already —
    /// `free_finished_lanes` drops it when it books what the lane spent — so
    /// this is the second half of the one path that does not, a lane written
    /// off for holding its pane past [`HANDOVER_WAIT`].
    fn close_handover(&mut self, handover: &PaneHandover, report: &mut Report) {
        if let Err(err) = self.mux.close_pane(&handover.pane_id) {
            report.problems.push(format!("{}: {err:#}", handover.lane));
            return;
        }
        self.lanes.remove(&handover.lane);
    }

    /// Keep a parked task's pane, book what it spent, and put it in front of
    /// the person it is now waiting on — `spoolway resume`, whichever kind of
    /// stop it is.
    ///
    /// The record is what makes this survive: it is the only thing that later
    /// tells a pane held for a person from a lane still holding a question, and
    /// it is written to disk with the rest, so a dispatcher restarted overnight
    /// does not mistake one for the other.
    fn hold_for_block(
        &mut self,
        lane: &Lane,
        task_id: &str,
        step_id: &str,
        task: Option<&Task>,
        report: &mut Report,
    ) {
        let ledger = self.ledger();
        let record = self
            .lanes
            .entry(lane.name.clone())
            .or_insert_with(|| LaneRecord::readopted(&lane.name, now_secs(), &ledger));
        record.held_for_block = true;
        // Not a lane holding a question any more: answering this pane does
        // nothing, and the board must stop offering it as somewhere to go and
        // reply. What is waiting now is the task, and its stage says so.
        record.notified = false;
        let record = record.clone();
        let pipeline = task
            .and_then(|t| self.pipelines.for_task(t).ok())
            .map(|p| p.name.clone())
            .unwrap_or_default();
        self.record_usage(&record, task_id, step_id, task, &pipeline);

        // Failing to focus is a pane somebody has to find themselves, not a
        // reason to spoil the pass — the notification named it either way.
        if let Err(err) = self.mux.focus_lane(&lane.name) {
            report.problems.push(format!("{}: {err:#}", lane.name));
        }
        let stage = task.map(|t| t.stage()).unwrap_or(crate::pipeline::BLOCKED);
        report.actions.push(format!(
            "{task_id}: {stage} at `{step_id}` — its pane `{}` is left open and focused",
            lane.name
        ));
    }

    /// Notice a person's round in a held pane, and commit it the moment it
    /// ends.
    ///
    /// `hold_for_block` only ever runs once — every pass after that, a held
    /// lane's pane used to be skipped outright, busy or not, because nothing
    /// told the two apart from a lane still holding a question. That left a
    /// person's turn with nowhere to land: whatever they typed and whatever
    /// the agent then did sat in the worktree, uncommitted, for as long as the
    /// task stayed parked. This is the other half — called instead of the
    /// usual busy/settle handling for exactly a held, parked pane, on every
    /// pass while it stays that way.
    ///
    /// Busy is recorded and nothing else is touched: a round in progress has
    /// nothing finished to commit yet. Idle only matters if the pane was seen
    /// busy since the last commit — an idle pane that was already idle is a
    /// person who has not come back, not a round that just ended.
    fn settle_person_turn(
        &mut self,
        lane: &Lane,
        task_id: &str,
        step_id: &str,
        tasks: &mut [Task],
        report: &mut Report,
    ) -> Result<()> {
        let busy = lane.status.is_busy();

        // Ahead of both mutations below, not after them: `pass()` saves
        // `self.lanes` unconditionally, so a dry run that flipped
        // `person_turn_busy` — set on the way in, or cleared on the way
        // out — would persist that flip without having committed anything.
        // A dry run that caught the idle-after-busy moment would then have
        // told the *next*, real pass the round was already handled, and the
        // work would never be committed at all.
        if self.dry_run {
            if !busy
                && self
                    .lanes
                    .get(&lane.name)
                    .is_some_and(|record| record.person_turn_busy)
            {
                report
                    .actions
                    .push(format!("would commit a person's round in {}", lane.name));
            }
            return Ok(());
        }

        let Some(record) = self.lanes.get_mut(&lane.name) else {
            return Ok(());
        };
        if busy {
            record.person_turn_busy = true;
            return Ok(());
        }
        if !record.person_turn_busy {
            return Ok(());
        }
        record.person_turn_busy = false;
        let started_at = record.head.clone();

        let Some(task) = tasks.iter_mut().find(|t| t.id() == task_id) else {
            return Ok(());
        };
        let Some(worktree) = task.front.worktree_path.clone() else {
            return Ok(());
        };
        let worktree = std::path::Path::new(&worktree);
        if let Some(note) =
            crate::commands::auto_commit(self.repo, worktree, &started_at, task_id, step_id).note()
        {
            task.append_to_section(
                "## Status Log",
                &format!("- a person's round in the held pane — {note}\n"),
            );
            self.persist(task)?;
            report.actions.push(format!(
                "{task_id}: committed a person's round in `{}`",
                lane.name
            ));
        }

        // `head` tracks where the branch stood the last time this was
        // checked, not where it stood at launch — otherwise the *next*
        // round's own commit would read as residue from a round the lane
        // already committed (see `lane_committed`) and never get swept in.
        // Read fresh rather than trusted from `auto_commit`'s note, which
        // only says whether it committed, not what HEAD became.
        if let Ok(now) = crate::repo::run(worktree, "git", &["rev-parse", "HEAD"])
            && let Some(record) = self.lanes.get_mut(&lane.name)
        {
            record.head = now.trim().to_string();
        }
        Ok(())
    }

    /// Add what one finished lane spent to the ledger.
    ///
    /// Everything here is best-effort by design. A transcript that cannot be
    /// found, an agent whose format is not known, a ledger that will not open —
    /// none of them are worth failing a dispatch pass over, because accounting
    /// is a record of the pipeline, not a part of it.
    ///
    /// **Not under [`crate::lock::LedgerLock`].** The append below is
    /// unlocked, and the diff is against this pass's [`Dispatcher::ledger`]
    /// snapshot rather than a fresh read — criterion 3's one-read-per-pass.
    /// The lock covers the paths that are *not* the dispatcher: `bank_ambient`,
    /// `sweep`, and `bank_lane` (a `queue pause` or board `p`/`P` in another
    /// process). The dispatcher is the only writer of lane lines in the common
    /// case, so its own appends are serial. The one gap is a `bank_lane`
    /// racing this call for the *same carried session* — a narrow window
    /// accepted in favour of not re-reading the ledger per lane.
    pub(crate) fn record_usage(
        &mut self,
        record: &LaneRecord,
        task_id: &str,
        step_id: &str,
        task: Option<&Task>,
        pipeline: &str,
    ) {
        if self.dry_run || record.session.is_empty() {
            return;
        }
        // Read before the harvest, and used verbatim as the line's `ts` below.
        // A record appended to the transcript between the harvest reaching EOF
        // and a clock read *after* it would land with a file mtime older than
        // that reading — and `usage::catch_up_settled_lane` gates a later
        // sweep on exactly that mtime against this line's `ts`, so a `ts`
        // chosen after the read would strand the unread tail forever.
        let banked_at = chrono::Utc::now();
        let Some(harvest) = crate::usage::harvest(&record.kind, &record.session) else {
            return;
        };

        // The transcript is the authority on which model actually answered; the
        // configured one is only what was asked for.
        let model = if harvest.model.is_empty() {
            record.model.clone()
        } else {
            harvest.model.clone()
        };
        // A resumed lane carries the session id of the lane that blocked, and a
        // transcript's totals are cumulative — so banking the whole thing again
        // would count that first lane's tokens twice. Bank only what has
        // arrived since. `usage_banked` is this pass's own running total per
        // session, read from the ledger at most once per pass rather than
        // re-read here for every lane a pass tears down.
        //
        // Costs nothing for the ordinary lane, whose freshly minted session
        // appears nowhere in the ledger and whose delta is therefore its total.
        let banked = self
            .usage_banked()
            .entry(record.session.clone())
            .or_default();
        let tokens = harvest.tokens.since(&banked.tokens);
        let banked_cost = banked.cost_usd;
        let banked_turns = banked.turns;
        // The mutable borrow of `self` from `usage_banked()` above ends
        // here, at its last use — needed before the immutable borrow of
        // `self.repo` just below, which the borrow checker cannot see is a
        // disjoint field the way it could when this read `self.usage_banked`
        // directly rather than through a method.
        //
        // Prices are linear in tokens, so pricing the delta is the same as
        // differencing two priced totals — and it stays right when the agent
        // reports its own cost instead.
        let cost_usd = match harvest.cost_usd {
            Some(total) => Some((total - banked_cost).max(0.0)),
            None => crate::usage::price(&self.repo.config.models, &model, &tokens),
        };

        // Only this step's own verdict counts. A report left over from the
        // previous step means this lane never reported one, and an absent
        // outcome says that plainly rather than borrowing a neighbour's.
        let outcome = task
            .and_then(|t| t.front.last_report.as_ref())
            .filter(|report| report.step == step_id)
            .map(|report| report.outcome.clone());

        let stamp = crate::version::stamp(self.repo);

        let entry = crate::usage::Entry {
            ts: banked_at.to_rfc3339(),
            task: task_id.to_string(),
            plan: task.and_then(|t| t.front.group.clone()),
            step: step_id.to_string(),
            pipeline: pipeline.to_string(),
            agent: record.agent.clone(),
            kind: record.kind.clone(),
            model,
            session: record.session.clone(),
            round: task.map(|t| t.prompts_at(step_id)).unwrap_or(0),
            wall_s: (now_secs() - record.started_at).max(0),
            turns: harvest.turns.saturating_sub(banked_turns),
            tokens,
            cost_usd,
            // The transcript's own peak, not a delta against what was already
            // banked — a peak from an earlier lane of a carried session is
            // still a real peak, and taking the max of two banked figures
            // would need every past line re-read for one that is already the
            // largest reading in the transcript spoolway just harvested.
            ctx_peak: Some(harvest.ctx_peak),
            version: Some(stamp.version),
            commit: stamp.commit,
            outcome,
            run: task.and_then(|t| t.front.run.clone()),
            trial: task.and_then(|t| t.front.trial.clone()),
            // A lane ran a step, not a skill. Absent here is what tells the
            // two kinds of line apart everywhere they are read.
            skill: None,
            // Never written: the ledger's own location says which project this
            // is, and only a reader spanning several needs the answer.
            project: String::new(),
        };
        let _ = crate::usage::append(self.repo, &entry);
        // Folded into the running total immediately, so a second lane banked
        // against this same carried session later in the same pass sees this
        // entry without a fresh read of the file. Already populated by the
        // call above — this can never itself trigger the read.
        let banked = self
            .usage_banked()
            .entry(record.session.clone())
            .or_default();
        banked.tokens.add(&entry.tokens);
        banked.turns += entry.turns;
        banked.cost_usd += entry.cost_usd.unwrap_or(0.0);
    }

    /// Fold what a lane has said into its record, and answer with how long it
    /// has been quiet.
    ///
    /// The reminder loop is this and one comparison. Kept in one place because
    /// it is the only place that needs to agree with itself about what "quiet"
    /// means.
    ///
    /// **The transcript, not the pane.** This used to hash the pane's last sixty
    /// lines and call a changed hash progress, which is a reading of the
    /// *screen* — and an agent mid-tool-call is still drawing one. Under a
    /// multiplexer a `pi` lane renders a spinner frame, an elapsed counter and a
    /// token meter while it waits, so the hash never settled, `last_progress`
    /// was refreshed every pass, and `silent_for` never approached the budget: a
    /// lane wedged inside a long tool call — precisely the case a person fears —
    /// was never judged silent. What the old screen reading caught instead was
    /// a lane that had stopped drawing altogether, which is a different and
    /// much rarer fault.
    ///
    /// A transcript answers the question that was being asked all along. A turn
    /// appends to it — the message, each tool call, each result — and a wedged
    /// call appends nothing, whatever the screen is doing. It is also the same
    /// answer on every backend, where the pane was two answers: a stand-in's
    /// `sleep 300` under `headless` genuinely emits no bytes, so the two
    /// disagreed about what silence meant and only the one nobody runs in
    /// production behaved as documented.
    ///
    /// The pane hash survives as the fallback and nothing more, for a lane with
    /// no transcript to read: one this dispatcher did not start, or a project
    /// whose `args` never passed `{session_id}` through. There the screen really
    /// is all there is.
    ///
    /// `started_at` stays on the record even though this no longer answers with
    /// how long a lane has been alive — the usage ledger is its other reader,
    /// and dropping the field would take that away too.
    fn note_progress(&mut self, lane: &Lane, now: i64) -> Duration {
        // Read before the record is borrowed, and only for a lane that named a
        // session — the fallback below is what the rest get.
        let wrote_at = self
            .lanes
            .get(&lane.name)
            .filter(|record| !record.session.is_empty() && !record.kind.is_empty())
            .and_then(|record| crate::usage::last_written(&record.kind, &record.session));

        let fallback = match wrote_at {
            Some(_) => None,
            None => Some(hash_of(&self.mux.read(&lane.name, 60).unwrap_or_default())),
        };

        let ledger = self.ledger();
        let record = self
            .lanes
            .entry(lane.name.clone())
            .or_insert_with(|| LaneRecord {
                output_hash: fallback.unwrap_or_default(),
                ..LaneRecord::readopted(&lane.name, now, &ledger)
            });

        match (wrote_at, fallback) {
            // Never before the lane started: a transcript resumed from an
            // earlier lane carries that lane's mtime until this one writes, and
            // read literally it would say this lane has been quiet since before
            // it existed.
            (Some(at), _) => record.last_progress = record.last_progress.max(at),
            (None, Some(hash)) => {
                if record.output_hash != hash {
                    record.output_hash = hash;
                    record.last_progress = now;
                }
            }
            (None, None) => {}
        }

        // A second signal, independent of the transcript above: a lane mid
        // tool-call is holding a process it started, and that process writes
        // nothing to the transcript until it returns — so a genuinely busy
        // lane and a genuinely quiet one can read identically here. Where the
        // backend can say whether anything is still running, that answer is
        // kept beside the transcript's own rather than folded into it, so
        // `check_unreported` can excuse the lane by it without disturbing
        // `last_progress`, which every other reading of this record still
        // takes as "when did the transcript last move".
        //
        // Cleared the moment the child goes away rather than left to expire
        // with `last_progress`, so a lane that goes on to a second, unrelated
        // long call is judged by a fresh clock rather than one still running
        // from the first.
        match self.mux.lane_process_alive(&lane.name) {
            Some(true) => {
                record.child_since.get_or_insert(now);
            }
            _ => record.child_since = None,
        }

        Duration::from_secs((now - record.last_progress).max(0) as u64)
    }

    /// The last of what a lane's pane says, for an escalation to carry.
    ///
    /// Read here and nowhere else, on the one path that needs it: a person
    /// reading `## Blocker` afterwards has the stage change and this, and
    /// nothing else, to say what the session was doing when it stopped. It is
    /// deliberately not part of [`Dispatcher::note_progress`] any more — a
    /// screen read every pass, for a tail almost every pass throws away, was
    /// most of the reason the watchdog was reading the screen at all.
    fn pane_tail(&self, lane: &Lane) -> String {
        self.mux.read(&lane.name, 60).unwrap_or_default()
    }

    /// `Some(reason)` when `lane` — running or settled, it makes no
    /// difference here — has already crossed its profile's
    /// `session_blocked_ctx`: its last *completed* turn reads past that
    /// percentage of its model's resolved `context_window`. Checked on
    /// every pass, against the same reading [`carried_session`] takes of a
    /// settled session, through [`crate::usage::last_turn`] — a lane mid-turn
    /// is sized by the turn before the one still in flight, since there is no
    /// reading of an unfinished one to take.
    ///
    /// `None` on a ceiling of `0` (off, the default, and every shipped
    /// profile's), on a lane with no session recorded yet to size, and on a
    /// model whose `context_window` never resolves — a config like that never
    /// fires and never gets a reason here; `spoolway doctor` is where that
    /// gap is called out instead, because a silent `None` is exactly what a
    /// person setting the ceiling would not expect.
    fn ctx_ceiling_hold(&self, step: &Step, lane: &Lane) -> Option<String> {
        let agent_name = step.agent.as_deref()?;
        let profile = self.repo.config.agent(agent_name).ok()?;
        if profile.session_blocked_ctx == 0 {
            return None;
        }
        let record = self.lanes.get(&lane.name)?;
        if record.kind.is_empty() || record.session.is_empty() {
            return None;
        }
        let model = resolve_model(step);
        let window = crate::models::resolve(&self.repo.config.models, &model)
            .price
            .map(|price| price.context_window)
            .filter(|window| *window > 0)?;
        let size = crate::usage::last_turn(&record.kind, &record.session)?;
        if !exceeds_percent(window, profile.session_blocked_ctx, size) {
            return None;
        }
        let pct = size.saturating_mul(100) / window as u64;
        Some(format!(
            "its last completed turn read {pct}% of `{model}`'s context window — over the \
             `session_blocked_ctx` ceiling ({}%) — stopped here rather than run to the wall; \
             the worktree still holds whatever it left uncommitted, and the conversation is \
             gone",
            profile.session_blocked_ctx,
        ))
    }

    /// `Some((reason, parked_until, window))` when `lane`'s own pane ends on its
    /// agent kind's usage-limit message — see
    /// [`crate::agent::Adapter::usage_limit`]. `None` for a kind with no such
    /// message established, or a step naming no agent at all — a command
    /// step never reaches this, but the lookup is written defensively rather
    /// than assumed.
    ///
    /// Whether the tail *is* a limit is answered by the adapter, not by a
    /// pattern kept here: the exact wording is a fact about one CLI, and
    /// belongs on that CLI's own row in `agent.rs`.
    ///
    /// An observed exhausted window supplies its reset. Otherwise rechecks
    /// use their own persistent backoff; launch attempts never grow while
    /// the same lane remains held. A five-hour clock is not evidence of a
    /// weekly limit resetting.
    fn usage_limit_hold(
        &self,
        task: &Task,
        step: &Step,
        lane: &Lane,
    ) -> Option<(String, i64, String)> {
        let agent_name = step.agent.as_deref()?;
        let profile = self.repo.config.agent(agent_name).ok()?;
        let adapter = crate::agent::adapter(&profile.kind)?;
        let tail = self.pane_tail(lane);
        if !adapter.is_usage_limit(&tail) {
            return None;
        }
        let now = now_secs();
        let hit = crate::quota::trusted(&profile.kind)
            .ok()
            .and_then(|reading| reading.over_ceiling(100).cloned());
        let until = hit
            .as_ref()
            .map(|hit| hit.resets_at.timestamp())
            .unwrap_or_else(|| now + quota_backoff(task.front.quota_retries));
        let window = hit
            .as_ref()
            .map(|hit| hit.window.key())
            .unwrap_or("")
            .to_string();
        Some((
            format!(
                "`{}` hit its usage limit — pane held, parked until {}; the lane resumes its \
                 own turn",
                step.id,
                crate::task::format_until(until, now, window == "seven_day"),
            ),
            until,
            window,
        ))
    }

    /// Park `task` for its lane's own usage-limit hold, if `lane`'s pane
    /// shows one — see [`Dispatcher::usage_limit_hold`]. Answers whether it
    /// did, so the caller can `continue` past everything else a pass would
    /// otherwise do with this lane.
    ///
    /// **Touches nothing of the lane's.** No `stop_lane`, no lane record
    /// removed, no worktree touched — the pane, its session and its
    /// worktree are left exactly as they are, because the agent resumes its
    /// own turn on its own clock and tearing any of that down would destroy
    /// a session that was already coming back. The only thing that changes
    /// is the task: `parked_until` is what keeps the reminder loop and the
    /// scheduler off it until that clock passes.
    fn usage_limit_park(
        &mut self,
        task: &mut Task,
        step: &Step,
        lane: &Lane,
        report: &mut Report,
    ) -> Result<bool> {
        let Some((reason, until, window)) = self.usage_limit_hold(task, step, lane) else {
            return Ok(false);
        };
        if self.dry_run {
            report
                .actions
                .push(format!("would hold {} — {reason}", lane.name));
            return Ok(true);
        }
        if !task.front.usage_limit_hold {
            task.append_to_section("## Status Log", &format!("- {reason}\n"));
        }
        task.front.usage_limit_hold = true;
        task.front.quota_retries = task.front.quota_retries.saturating_add(1);
        task.front.parked_until = Some(until);
        task.front.parked_window = window;
        // The first observation of this hold starts its clock; every later
        // recheck moves only the deadline, never the age.
        if task.front.parked_at.is_none() {
            task.front.parked_at = Some(now_secs());
        }
        self.persist(task)?;
        report.actions.push(format!("{}: {reason}", task.id()));
        Ok(true)
    }

    /// Disabled protection takes no reading. Enabled protection requires a
    /// fresh reading; a failure is a hold, never permission to launch.
    fn quota_over_ceiling(
        &self,
        kind: &str,
        ceiling: u8,
    ) -> Result<Option<crate::quota::WindowReading>, String> {
        if ceiling == 0 {
            return Ok(None);
        }
        let reading = crate::quota::trusted(kind)?;
        Ok(reading.over_ceiling(ceiling).cloned())
    }

    /// Tear a lane down for failing the dispatcher's one remaining check — a
    /// settled lane that never reported — and hand its task to `blocked` with
    /// `reason` as the whole of what a person reads. The busy-lane watchdog
    /// that used to share this with [`Dispatcher::check_unreported`] is gone:
    /// a busy lane is never escalated any more, whatever it is doing, and a
    /// dry run only says so, a real pass tears the lane down and moves the
    /// task.
    fn escalate_clock(
        &mut self,
        task: &mut Task,
        pipeline: &Pipeline,
        step: &Step,
        lane: &Lane,
        reason: &str,
        report: &mut Report,
    ) -> Result<()> {
        if self.dry_run {
            report
                .actions
                .push(format!("would escalate {} — {reason}", lane.name));
            return Ok(());
        }

        let output = self.pane_tail(lane);
        self.tear_down_and_escalate(
            task,
            pipeline,
            step,
            lane,
            &output,
            &format!("`{}` {reason}", step.id),
        )?;

        report
            .actions
            .push(format!("{}: stuck at `{}` — {reason}", task.id(), step.id));
        Ok(())
    }

    /// Whether `lane`'s transcript ends in a turn a person aborted with
    /// Escape — see [`crate::usage::last_turn_aborted`], which this reads
    /// off the same `kind`/`session` pair [`Dispatcher::note_progress`]
    /// already resolves every pass. `false` for a lane with no record at
    /// all, or one naming no session — a lane this dispatcher did not start,
    /// or a profile whose `args` never pass `{session_id}` through, has no
    /// transcript to read and so nothing to answer with beyond "no".
    fn lane_ended_on_abort(&self, lane: &Lane) -> bool {
        self.lanes.get(&lane.name).is_some_and(|record| {
            !record.kind.is_empty()
                && !record.session.is_empty()
                && crate::usage::last_turn_aborted(&record.kind, &record.session)
        })
    }

    /// Park `task` on `paused` the same way the board's `p` key does — see
    /// [`crate::status::park`] — for a lane whose own transcript says a
    /// person's Escape, not a forgotten report, is why it settled. Neither
    /// nudged nor escalated: [`Dispatcher::check_unreported`] is never
    /// reached for this lane at all, which is the whole point of asking
    /// [`Dispatcher::lane_ended_on_abort`] ahead of it.
    fn park_after_interrupt(
        &mut self,
        task: &mut Task,
        step: &Step,
        lane: &Lane,
        report: &mut Report,
    ) -> Result<()> {
        if self.dry_run {
            report.actions.push(format!(
                "would park {} — its own Escape ended the turn",
                lane.name
            ));
            return Ok(());
        }
        crate::status::park(
            task,
            &format!("`{}` ended its turn on a person's own Escape", step.id),
        );
        self.persist(task)?;
        report.actions.push(format!(
            "{}: parked at `{}` — its own Escape ended the turn",
            task.id(),
            step.id
        ));
        Ok(())
    }

    /// Spend a still-live `parked_from` (and the one-shot `resume` it
    /// travels with) without ever launching anything — the other half of a
    /// resume racing a lane the person restarted by hand. `start_one` is
    /// what ordinarily spends both, on the launch a resume queues for; a
    /// lane already busy again needs no launch, so this is what finishes the
    /// unpark in its place. A no-op for any task not actually mid-resume on
    /// this exact step, so it costs nothing to call from every busy lane
    /// this pass sees settled into work.
    fn unpark_quietly(&mut self, task: &mut Task, step: &Step, report: &mut Report) -> Result<()> {
        if task.front.parked_from.as_deref() != Some(step.id.as_str()) {
            return Ok(());
        }
        if self.dry_run {
            report.actions.push(format!(
                "{}: would un-park `{}` — its lane is already busy, nothing would be sent",
                task.id(),
                step.id
            ));
            return Ok(());
        }
        task.front.parked_from = None;
        task.front.resume = None;
        self.persist(task)?;
        report.actions.push(format!(
            "{}: un-parked `{}` — its lane was already busy, so nothing was sent",
            task.id(),
            step.id
        ));
        Ok(())
    }

    /// A lane that ended its turn and left its task where it found it has not
    /// reported. Reading the transcript to decide whether that is a forgotten
    /// `spoolway report` or a genuine question is exactly the judgement the
    /// dispatcher does not make, so this does not try: it answers the
    /// ambiguity instead of resolving it, by sending the report contract
    /// again through [`Mux::prompt`] — the same call `spoolway lane -m` sends
    /// a person's own words through. A lane that forgot acts on it; a lane
    /// genuinely stuck on a question gets a message that costs it nothing.
    ///
    /// The rule is one comparison: remind a lane that has written to its
    /// transcript since its last reminder, or has never been reminded at all.
    /// Capped at three — a lane genuinely waiting on a person deserves
    /// patience, but not an hour of it purely because its replies count as
    /// progress. The count lives on the lane record beside `reminded_at` and
    /// needs no clearing: it dies with the record.
    ///
    /// Two exits bound the loop, both counted rather than timed. A lane
    /// reminded three times and still not reporting is escalated in the
    /// `due` branch below rather than sent a fourth reminder. And a lane that
    /// goes fully quiet *after* a reminder — nothing further in its
    /// transcript — is the dead session a reminder cannot reach, because
    /// there is no lane left running to act on the message; that is the `not
    /// due` branch, and it escalates on the very next pass rather than
    /// waiting out a clock that no longer exists. A lane is due for another
    /// reminder only once it has written something since the last one, so a
    /// lane that goes silent right after being reminded is never due again —
    /// without this exit it would sit on its step forever.
    ///
    /// Runs for every settled lane alike, gated step or not — there is no
    /// exemption left to make here; see the caller. A person taking an hour to
    /// answer a question looks identical to a lane that forgot to report, and
    /// tearing the pane down on the first silence would destroy the
    /// conversation the answer belongs in — which is exactly why a reminder,
    /// not an escalation, is the first thing this does.
    fn check_unreported(
        &mut self,
        task: &mut Task,
        pipeline: &Pipeline,
        step: &Step,
        lane: &Lane,
        report: &mut Report,
    ) -> Result<()> {
        // A lane on its own kind's usage-limit message never reaches here at
        // all any more — [`Dispatcher::usage_limit_park`] catches it ahead of
        // this call, for a busy lane the same as a settled one, which this
        // function's caller never was.
        let now = now_secs();
        let silent_for = self.note_progress(lane, now);

        // A lane still holding a process it started is not silent, whatever
        // its transcript says — that is `note_progress`'s other signal, and
        // this is the one place it is read. Excused entirely below the
        // ceiling: no reminder is sent, and the reminder count this lane may
        // already be carrying does not move, so it picks up exactly where it
        // was once the child goes away. Past the ceiling the excuse itself is
        // the problem — a process that has run this long unsupervised is what
        // `dispatch.lane_child_ceiling` exists to catch — and this escalates
        // directly rather than falling through to the reminder loop below,
        // which a lane that never stops writing to its transcript would
        // never reach on its own.
        if let Some(child_since) = self.lanes.get(&lane.name).and_then(|r| r.child_since) {
            let ceiling = self.repo.config.dispatch.lane_child_ceiling;
            let held_for = Duration::from_secs((now - child_since).max(0) as u64);
            if held_for < ceiling {
                return Ok(());
            }
            let reason = format!(
                "has held a process open for over {} — its pane is `{}`",
                crate::config::format_duration(ceiling),
                lane.pane_id
            );
            self.escalate_clock(task, pipeline, step, lane, &reason, report)?;
            return Ok(());
        }

        let record = self.lanes.get(&lane.name);
        let last_progress = record.map_or(now, |r| r.last_progress);
        let due = match record.and_then(|r| r.reminded_at) {
            None => true,
            Some(reminded_at) => last_progress > reminded_at,
        };

        if due {
            // `dispatch.lane_quiet` before the first reminder, and before
            // every one after it. `note_progress` reads `last_progress` off
            // the transcript's own write time rather than off this pass
            // noticing it, so the pass that first sees a lane settled can
            // already read a few seconds of silence just from catching up.
            //
            // This used to be `dispatch.interval`, which is a different
            // quantity wearing the same name: how often a pass looks at a
            // lane, not how long that lane may be quiet. At the shipped ten
            // seconds it meant a lane had ten seconds to speak or be nudged,
            // and a step whose prompt runs the end-to-end `pr` tier — which
            // `assets/prompts/e2e` requires, and which the shipped `suite`
            // step allows 45 minutes — ends its turn and waits on a
            // background job. Three reminders and an escalation later, a lane
            // doing exactly what it was told was sitting on `blocked` inside
            // a minute, having burned a turn answering each nudge.
            if silent_for < self.repo.config.dispatch.lane_quiet {
                return Ok(());
            }

            // The ceiling: three reminders is the whole of the patience this
            // extends. A fourth due reminder is not a fourth chance, it is the
            // same silent lane costing another round — so this escalates in
            // its place, naming the lane and the pane a person would have to
            // go look at.
            let reminders = record.map_or(0, |r| r.reminders);
            if reminders >= MAX_REMINDERS {
                let reason = format!(
                    "reminded {reminders} times and never reported — its pane is `{}`",
                    lane.pane_id
                );
                self.escalate_clock(task, pipeline, step, lane, &reason, report)?;
                return Ok(());
            }

            if self.dry_run {
                report.actions.push(format!(
                    "would remind {} to report ({}/{MAX_REMINDERS})",
                    lane.name,
                    reminders + 1
                ));
                return Ok(());
            }

            let nudge = crate::compose::reminder_prompt(self.repo, pipeline, step);
            self.mux.prompt(&lane.name, &nudge)?;
            // Baselined *after* the nudge lands, not before. A lane with no
            // session to read its transcript by falls back to hashing the
            // pane itself, and typing the nudge into that pane is a write by
            // this dispatcher's own hand — folded in here, rather than left
            // for the next pass to discover, so it never reads back as the
            // lane's own and reminds it forever for having said nothing.
            self.note_progress(lane, now_secs());
            if let Some(record) = self.lanes.get_mut(&lane.name) {
                record.reminded_at = Some(record.last_progress);
                record.reminders += 1;
            }
            report.actions.push(format!(
                "{}: reminded at `{}` to report ({}/{MAX_REMINDERS})",
                task.id(),
                step.id,
                reminders + 1
            ));
            return Ok(());
        }

        // Reminded, and nothing since — not a lane declining to report, but
        // the dead session a reminder cannot help. There is no clock left to
        // wait out: the pass that finds this blocks it on the spot.
        let reason = "produced no output since its last reminder".to_string();
        self.escalate_clock(task, pipeline, step, lane, &reason, report)?;
        Ok(())
    }

    /// End a settled lane's round on a step it reported on that routed back to
    /// itself — `blocked` reporting `--block` again, most often.
    ///
    /// The task already went where the report sent it: on a step that routes
    /// to itself, that is exactly where it already sits, so there is no
    /// further stage change to make here — nothing this pass is going to start
    /// in this pane. What is left is the same vacate
    /// [`Dispatcher::free_finished_lanes`] gives a lane whose task moved off
    /// its step: bank what the lane spent, drop its record, and let the pane
    /// go the way its kind allows.
    ///
    /// A pane that comes back empty is not closed. There is no `PaneHandover`
    /// for it to ride in — that mechanism is spent within the pass that fills
    /// it, and the pass that starts this same step again is a later one — so
    /// it is written down on [`LaneRecord::retired_pane`] instead, under this
    /// exact lane name, for `start_lanes` to find and inherit whenever that
    /// name is started next. A kind with no gesture, or a session that would
    /// not leave, is handled the same as any other pane a name might inherit:
    /// see [`RetiredPane::ready`].
    fn retire(
        &mut self,
        task: &Task,
        pipeline: &Pipeline,
        step: &Step,
        lane: &Lane,
        report: &mut Report,
    ) -> Result<()> {
        if self.dry_run {
            report
                .actions
                .push(format!("would retire {} — reported", lane.name));
            return Ok(());
        }
        // The record's kind is preferred over the multiplexer's own for the
        // same reason `free_finished_lanes` prefers it: it is what spoolway
        // launched with, and so what the adapter table was read for.
        let kind = self
            .lanes
            .get(&lane.name)
            .map(|record| record.kind.clone())
            .filter(|kind| !kind.is_empty())
            .unwrap_or_else(|| lane.kind.clone());
        let left = self.mux.vacate_lane(&lane.name, &kind, &lane.pane_id)?;
        if let Some(record) = self.lanes.remove(&lane.name) {
            self.record_usage(&record, task.id(), &step.id, Some(task), &pipeline.name);
        }
        if left != Vacated::PaneClosed {
            let now = now_secs();
            self.lanes.insert(
                lane.name.clone(),
                LaneRecord::retired(
                    now,
                    RetiredPane {
                        pane_id: lane.pane_id.clone(),
                        ready: left == Vacated::Shell,
                    },
                ),
            );
        }
        report
            .actions
            .push(format!("freed {} — reported", lane.name));
        Ok(())
    }

    /// Close a lane spoolway has given up on, and move its task to `blocked`
    /// with the last of what its pane said.
    ///
    /// The tail is the point: an escalation that did not carry it would leave a
    /// person a stage change and no account of what the session was doing when
    /// it stopped. Where this escalation is going to stop — on `blocked`, in
    /// front of a person — the pane it came out of says it better than fifteen
    /// lines can, so the pane stays and only the lane's bookkeeping ends. A
    /// task diverted to an unblock step is going to a lane, not a person, and
    /// its pane goes as it always did.
    fn tear_down_and_escalate(
        &mut self,
        task: &mut Task,
        pipeline: &Pipeline,
        step: &Step,
        lane: &Lane,
        output: &str,
        reason: &str,
    ) -> Result<()> {
        // ...and only on a backend where a pane is a thing to keep. Headless
        // has none: what "keeping" it means there is a turn's process left
        // running with nobody able to look at it, and a `sleep` or a model
        // mid-thought outliving the run that started it. The log is what a
        // person reads on that backend, and the log already outlives the lane.
        // Same question as `holds_a_slot` asks, for the same reason — ask the
        // backend rather than assume one.
        let hold = self.parks_on_blocked() && self.mux.resident_while_waiting();
        if hold {
            // A held pane is not necessarily a settled one any more — the
            // context ceiling can reach this with a lane still mid-turn —
            // and keeping the pane open must not mean leaving the agent
            // running past the very ceiling that just stopped it. Escape
            // ends the turn without ending the session, the same as
            // `Mux::interrupt_lane` does for a person's own keypress, so the
            // pane a person reads next still holds the conversation, just
            // not one still spending.
            if lane.status.is_busy() {
                let _ = self.mux.interrupt_lane(&lane.name);
            }
        } else {
            self.mux.stop_lane(&lane.name, &lane.pane_id)?;
        }
        // A lane that went wrong spent exactly as much as one that went right,
        // and is the one you most want to find in `spoolway eval --by` afterwards.
        // Booked here rather than in `free_finished_lanes`, which never sees a
        // lane this path has already torn down — or, for a held one, sees it
        // every pass and would book it on each.
        let ledger = self.ledger();
        let record = match hold {
            true => {
                let record = self
                    .lanes
                    .entry(lane.name.clone())
                    .or_insert_with(|| LaneRecord::readopted(&lane.name, now_secs(), &ledger));
                record.held_for_block = true;
                // See `hold_for_block`: a pane kept to be read is not a lane
                // anybody can answer.
                record.notified = false;
                Some(record.clone())
            }
            false => Some(
                self.lanes
                    .remove(&lane.name)
                    .unwrap_or_else(|| LaneRecord::readopted(&lane.name, now_secs(), &ledger)),
            ),
        };
        if let Some(record) = &record {
            self.record_usage(record, task.id(), &step.id, Some(task), &pipeline.name);
        }
        if hold {
            let _ = self.mux.focus_lane(&lane.name);
        }

        // The other end of the backstop. This lane never reached `spoolway
        // report`, so nothing has committed what it managed to do — and the
        // task is about to sit on `blocked`, where a person may well resume it
        // at a step that cleans up. Where it stood at launch is the lane
        // record's, because there is no lane left to ask.
        if !self.dry_run
            && let Some(worktree) = task.front.worktree_path.clone()
        {
            let started_at = record.as_ref().map(|r| r.head.as_str()).unwrap_or("");
            if let Some(note) = crate::commands::auto_commit(
                self.repo,
                std::path::Path::new(&worktree),
                started_at,
                task.id(),
                &step.id,
            )
            .note()
            {
                task.append_to_section("## Status Log", &format!("- {note}\n"));
            }
        }

        let tail: String = output
            .lines()
            .rev()
            .take(15)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|l| format!("      {l}\n"))
            .collect();
        task.append_to_section("## Blocker", &format!("  Last output:\n\n{tail}\n"));
        self.escalate(task, pipeline, step, reason)
    }

    /// Start as many lanes as each candidate's cap allows — a resolved
    /// model's own `slots` when it has any, its profile's `concurrency`
    /// otherwise — and refuse a candidate whose model is `exclusive` while a
    /// live lane runs a different `exclusive` model.
    ///
    /// A model's `slots` and its `exclusive` hold whatever the step's `slot:`
    /// says; only the profile's `concurrency` can be opted out of. The two are
    /// different axes: one is how much of the machine a set of weights takes,
    /// the other is how many lanes of a harness it is polite to run.
    ///
    /// Every lane that exists is counted, not only the ones the multiplexer
    /// calls busy this second — see the counting loop for what that cost.
    fn start_lanes(
        &mut self,
        tasks: &mut [Task],
        owned: &[(String, String, &Lane)],
        candidates: Vec<Candidate>,
        handovers: &mut HashMap<String, PaneHandover>,
        report: &mut Report,
    ) -> Result<()> {
        // Count what is already running against each profile's cap, and
        // against each named model's own — a live lane is counted by the
        // model its step names, the same name `resolve_model` hands the
        // agent, not by anything resolved from `[models]`.
        let mut in_flight: BTreeMap<String, usize> = BTreeMap::new();
        let mut model_in_flight: BTreeMap<String, usize> = BTreeMap::new();
        for (step_id, task_id, _) in owned {
            let Some(task) = tasks.iter().find(|t| t.id() == task_id) else {
                continue;
            };
            let Ok(pipeline) = self.pipelines.for_task(task) else {
                continue;
            };
            // A parked task's pane is kept for a person to read, and a pane
            // being read is not work in flight. Both stages, for the same
            // reason: two forgotten panes are two of `[agents.pi]
            // concurrency = 2` — the whole pipeline, held by nothing that is
            // running. A staffed `blocked` lane is not one of them: it is a
            // running lane like any other, and does count against its cap.
            let parked = task.stage() == crate::pipeline::PAUSED
                || (task.stage() == crate::pipeline::BLOCKED
                    && !pipeline.blocked_is_staffed(self.unattended));
            if parked {
                continue;
            }
            // Every surviving lane counts, whatever the multiplexer says it is
            // doing this second. It used to be `is_busy()` only — `working` or
            // `blocked` — and that is the bug that put five lanes on a
            // three-slot model: a lane spends its first seconds `idle`
            // ("started but never prompted", and `unknown` while the backend
            // is still labelling its pane), so the pass ten seconds after the
            // one that started three of them counted none of the three and
            // filled the model up again. A settled lane mid-conversation is
            // the same story more slowly: it holds a session, its weights and
            // its pane until something frees it.
            //
            // What "surviving" means is already decided, above this call:
            // `free_finished_lanes` has closed the panes that are finished and
            // `owned` has had them retained out of it, so anything still here
            // is a session that exists. Status is how a lane *is*, not whether
            // it *is* — and a cap is about occupancy.
            let Some(step) = pipeline.step(step_id) else {
                continue;
            };
            if let Some(agent) = step.agent.clone() {
                *in_flight.entry(agent).or_insert(0) += 1;
            }
            if let Some(model) = step.model.as_deref().filter(|m| !m.trim().is_empty()) {
                *model_in_flight.entry(model.to_string()).or_insert(0) += 1;
            }
        }

        // Which live model, if any, is `exclusive`. At most one is ever let
        // in by the check below, so the first one found is the resident.
        let mut resident_exclusive: Option<String> = model_in_flight
            .iter()
            .filter(|(_, running)| **running > 0)
            .find(|(model, _)| {
                crate::models::resolve(&self.repo.config.models, model)
                    .price
                    .is_some_and(|p| p.exclusive)
            })
            .map(|(model, _)| model.clone());

        for candidate in candidates {
            let Ok(pipeline) = self.pipelines.get(&candidate.pipeline).cloned() else {
                continue;
            };
            let step = match pipeline.step(&candidate.step_id) {
                Some(step) => step.clone(),
                None => continue,
            };
            // Every candidate is put here by an arm that has already asked
            // what kind of step it is, so one with no `agent:` is a scheduler
            // invariant broken rather than a step to skip. Skipping it in
            // silence is what made a pipeline opening on a `run:` step look
            // like an empty queue.
            let Some(agent_name) = step.agent.clone() else {
                report.problems.push(format!(
                    "{}: `{}` reached the scheduler with no `agent:`",
                    tasks[candidate.task_index].id(),
                    step.id
                ));
                continue;
            };
            // A missing profile is a per-candidate problem, the same as a
            // missing `agent:` above — never a `?`. A pipeline edited to
            // name a profile config does not define would otherwise fail
            // the whole pass here, after higher-ranked candidates were
            // already started, discarding every lane record built this pass
            // (`save_lane_records` is the pass's last line). See review
            // finding 8.
            let profile = match self.repo.config.agent(&agent_name) {
                Ok(profile) => profile.clone(),
                Err(err) => {
                    report.problems.push(format!(
                        "{}: `{}` names agent profile `{agent_name}`, which config does not \
                         define ({err})",
                        tasks[candidate.task_index].id(),
                        step.id
                    ));
                    continue;
                }
            };

            // A profile at its own `quota_ceiling` starts nothing of this
            // kind this pass — checked here, ahead of the attempts ceiling
            // below and every other reason a candidate might be skipped,
            // because a quota already over its ceiling makes every one of
            // those questions moot. `parked_until` is what a restarted
            // dispatcher reads back without taking a fresh reading — see the
            // top of this pass's own per-task loop.
            let hit = match self.quota_over_ceiling(&profile.kind, profile.quota_ceiling) {
                Ok(hit) => hit,
                Err(why) => {
                    let task = &mut tasks[candidate.task_index];
                    let until = now_secs() + quota_backoff(task.front.quota_retries);
                    // `why` names where the reading was looked for and how
                    // old the best one found was, so what is left to say is
                    // the way out: running the agent once is what writes a
                    // fresh reading, and `--live` is spoolway doing that for
                    // you.
                    let reason = format!(
                        "{} quota unavailable: {why}; new launches held — run {} once, or `spoolway agent verify {} --live`",
                        profile.kind, profile.kind, profile.kind
                    );
                    report.actions.push(format!(
                        "{}{}: {reason}",
                        if self.dry_run { "would park " } else { "" },
                        task.id()
                    ));
                    if !self.dry_run {
                        if task.front.quota_retries == 0 {
                            task.append_to_section("## Status Log", &format!("- {reason}\n"));
                        }
                        task.front.quota_retries = task.front.quota_retries.saturating_add(1);
                        task.front.parked_until = Some(until);
                        task.front.parked_window = "unknown".into();
                        // The start of the hold, stamped once — an unavailable
                        // reading that keeps a task parked across passes is one
                        // continuous park, not a fresh one each recheck.
                        if task.front.parked_at.is_none() {
                            task.front.parked_at = Some(now_secs());
                        }
                        self.persist(&mut tasks[candidate.task_index])?;
                    }
                    continue;
                }
            };
            // A fresh reading came back, so any unavailable-recheck streak on
            // `quota_retries` is stale. It is *not* zeroed here on its own:
            // that would be a save whose outcome depends on whether a
            // placement fixup in `start_one` happens to flush it. The reset
            // instead rides on the one write that resolves this pass's park —
            // the re-park just below, or the post-start clear in `start_one`.
            // A pass that reaches neither (the attempts ceiling, no free
            // slot, a refused start) leaves the count exactly as it was: a
            // recheck that decided nothing has not ended the streak, and
            // `launch_landed` still zeroes it once the lane is seen running.
            if let Some(hit) = hit {
                let until = hit.resets_at.timestamp();
                let now = now_secs();
                // Pinned to whichever window actually tripped, so the shape
                // decided here survives however close `now` later gets to
                // `until` — see [`crate::task::Frontmatter::parked_window`].
                let dated = hit.window == crate::quota::Window::SevenDay;
                let reason = format!(
                    "`{}` parked until {} — {} at {}% of its {} window, ceiling is {}",
                    step.id,
                    crate::task::format_until(until, now, dated),
                    profile.kind,
                    hit.utilization,
                    hit.window.human(),
                    profile.quota_ceiling,
                );
                if self.dry_run {
                    report.actions.push(format!(
                        "would park {} — {reason}",
                        tasks[candidate.task_index].id()
                    ));
                    continue;
                }
                tasks[candidate.task_index].front.parked_until = Some(until);
                tasks[candidate.task_index].front.parked_window = hit.window.key().to_string();
                // Stamped only if this is the start of the hold — a re-park
                // against a deadline that has run out is one continuous park,
                // so its age keeps counting from the first reading.
                if tasks[candidate.task_index].front.parked_at.is_none() {
                    tasks[candidate.task_index].front.parked_at = Some(now);
                }
                // The unavailable-recheck streak, if any, ends with this
                // reading — folded into the re-park's own write rather than
                // a save of its own.
                tasks[candidate.task_index].front.quota_retries = 0;
                tasks[candidate.task_index]
                    .append_to_section("## Status Log", &format!("- {reason}\n"));
                self.persist(&mut tasks[candidate.task_index])?;
                report
                    .actions
                    .push(format!("{}: {reason}", tasks[candidate.task_index].id()));
                continue;
            }

            // Checked before the candidate takes a slot, because a task that has
            // used up its budget must not also take a lane away from one that
            // has not. Nothing else bounds this: a lane whose agent dies at
            // launch leaves no session behind, so the task looks unstarted again
            // on the next pass and would be re-spawned for as long as the
            // dispatcher runs.
            let attempts = tasks[candidate.task_index].front.attempts;
            if attempts >= MAX_LAUNCHES {
                // Unattended, the same fact gets the only answer available: try
                // again, later each time. Escalating here would be worse than
                // useless — the resume it performs zeroes `attempts` on its way
                // through `set_stage`, so the very next pass would launch the
                // dying agent again with a clean counter, which is the busy loop
                // this guard exists to prevent, now unbounded.
                //
                // A usage-limit hold no longer reaches here at all: it moved
                // off `attempts`/`relaunch_backoff` entirely onto
                // `parked_until`, checked once at the top of this pass's own
                // task loop — see [`Dispatcher::usage_limit_hold`] — so a task
                // held for its quota never becomes a candidate in the first
                // place and this ceiling only ever sees a lane that genuinely
                // died at launch with nothing behind it.
                //
                // The backoff is measured from the last launch and paced off the
                // configured interval rather than a `--interval` override: what
                // it wants is a sense of how often this run looks at anything,
                // and the project's own figure is that within a doubling.
                if self.unattended {
                    let wait = relaunch_backoff(self.repo.config.dispatch.interval, attempts);
                    let since = tasks[candidate.task_index]
                        .front
                        .launched_at
                        .map(|at| now_secs().saturating_sub(at))
                        .unwrap_or(i64::MAX);
                    if since < wait.as_secs() as i64 {
                        let left = crate::config::human_duration::format(Duration::from_secs(
                            (wait.as_secs() as i64 - since).max(0) as u64,
                        ));
                        report.actions.push(format!(
                            "{}: `{}` has been started {attempts} time(s) and left \
                             nothing behind — trying again in {left}",
                            tasks[candidate.task_index].id(),
                            step.id,
                        ));
                        continue;
                    }
                    // Past the wait: fall through and launch, keeping `attempts`
                    // so the next wait is longer than this one.
                } else {
                    // `lanes.json` is only written back at the end of a pass
                    // — see `Dispatcher::pass` — so a dispatcher that dies
                    // between launching a lane and finishing that pass loses
                    // the record the very same launch just wrote. The very
                    // next pass, from a dispatcher restarted in its place,
                    // reads that silence exactly the way it would read a
                    // genuinely dead launch — there is nothing left on disk
                    // to say otherwise — and used to hand the task to a
                    // person over a failure that never happened.
                    //
                    // `self.lanes` is this pass's own read of that same file,
                    // taken fresh in `Dispatcher::new` before anything below
                    // could have touched it: a name missing from it here is a
                    // launch this dispatcher has not itself watched happen.
                    // One launched by a run that watched it live and then
                    // vanish carries a record from the pass that watched it,
                    // so this is `false` for a launch that really did die —
                    // which still escalates on the very pass that notices,
                    // exactly as before.
                    //
                    // A wall-clock window measured against `launched_at`
                    // used to stand here instead, and it was wrong: a real
                    // restart lands whenever a person or an init system
                    // notices, which is almost never inside one
                    // `dispatch.interval` of the launch, so the grace never
                    // actually fired for the case it exists for. What
                    // distinguishes "interrupted" from "failed" is not how
                    // long ago the launch was — it is whether *this*
                    // dispatcher has had even one pass to look. So the grace
                    // is durable rather than timed: the first pass to find
                    // no record marks this name seen, in `self.lanes`, which
                    // `Dispatcher::pass` writes back to `lanes.json` same as
                    // any other lane record — surviving `prune_stale_lane_records`,
                    // since the task is still queued — so the very next pass,
                    // whenever it lands, reads it as witnessed and escalates
                    // like any ordinary dead launch if the lane is still gone.
                    // Exactly one pass of grace, never more.
                    //
                    // Gated on `launched_at` being set at all: `start_one`
                    // never bumps `attempts` without setting it in the same
                    // breath, so a task that has one is, in practice, one
                    // this dispatcher genuinely tried to launch — the case
                    // the grace exists for. Without that gate, a task that
                    // reaches `attempts >= MAX_LAUNCHES` some other way —
                    // hand-edited, or carried over from a spoolway that
                    // never wrote the field — would get a free pass with
                    // nothing behind it to be graceful about.
                    let lane = lane_name(&step.id, tasks[candidate.task_index].id());
                    if tasks[candidate.task_index].front.launched_at.is_some()
                        && !self.lanes.contains_key(&lane)
                    {
                        let ledger = self.ledger();
                        self.lanes.insert(
                            lane.clone(),
                            LaneRecord::readopted(&lane, now_secs(), &ledger),
                        );
                        report.actions.push(format!(
                            "{}: `{}` has been started {attempts} time(s) and left \
                             nothing behind — this dispatcher never saw it launch, so \
                             giving it one more pass before asking a person",
                            tasks[candidate.task_index].id(),
                            step.id,
                        ));
                        continue;
                    }
                    let reason = format!(
                        "`{}` was started {attempts} time(s) and the task never left it — an \
                         agent that dies at launch leaves no session to wait on, so spoolway \
                         launches a step's lane once (max_launches) before asking a person",
                        step.id
                    );
                    if self.dry_run {
                        report.actions.push(format!(
                            "would hand {} to a person — {reason}",
                            tasks[candidate.task_index].id()
                        ));
                        continue;
                    }
                    let task = &mut tasks[candidate.task_index];
                    self.escalate(task, &pipeline, &step, &reason)?;
                    report.actions.push(format!("{}: {reason}", task.id()));
                    continue;
                }
            }

            let model_name = resolve_model(&step);
            let model_price = crate::models::resolve(&self.repo.config.models, &model_name).price;

            // Exclusivity is checked whatever `step.slot` says: it is a
            // statement about what the server behind the model can hold, not
            // about the profile's own slot budget.
            if model_price.is_some_and(|p| p.exclusive)
                && let Some(resident) = &resident_exclusive
                && resident != &model_name
            {
                report.actions.push(format!(
                    "{}: waiting for `{resident}` to finish — `{model_name}` is exclusive",
                    tasks[candidate.task_index].id()
                ));
                continue;
            }

            // A model's `slots` is checked whatever `step.slot` says, for the
            // same reason exclusivity is: the two describe what the machine
            // behind the model can hold at once, and a step cannot opt out of
            // physics. `slot: false` is a statement about the *profile's*
            // budget — "this cloud review is cheap, don't let it hold a lane
            // back" — and a harness's politeness and a card's memory are not
            // the same axis. A step that opted out of the first used to opt
            // out of the second by accident, which is a `slot: false` step
            // able to put a fourth lane on a three-slot model.
            match model_price.filter(|p| p.slots > 0).map(|p| p.slots) {
                Some(slots) => {
                    let running = model_in_flight.get(&model_name).copied().unwrap_or(0);
                    if running >= slots as usize {
                        report.actions.push(format!(
                            "{}: waiting for a `{model_name}` slot ({running}/{slots})",
                            tasks[candidate.task_index].id()
                        ));
                        continue;
                    }
                }
                // Only in the absence of a model's own number does the
                // profile's cap apply — and only then may a step opt out of it.
                None => {
                    if step.slot && profile.concurrency > 0 {
                        let running = in_flight.get(&agent_name).copied().unwrap_or(0);
                        if running >= profile.concurrency {
                            report.actions.push(format!(
                                "{}: waiting for a `{agent_name}` slot ({running}/{})",
                                tasks[candidate.task_index].id(),
                                profile.concurrency
                            ));
                            continue;
                        }
                    }
                }
            }

            let task = &mut tasks[candidate.task_index];
            if self.dry_run {
                report.actions.push(format!(
                    "would start `{}` for {} on agent `{agent_name}`",
                    step.id,
                    task.id()
                ));
                *in_flight.entry(agent_name).or_insert(0) += 1;
                *model_in_flight.entry(model_name.clone()).or_insert(0) += 1;
                if model_price.is_some_and(|p| p.exclusive) {
                    resident_exclusive.get_or_insert(model_name);
                }
                continue;
            }

            // A retry reuses the lane name, so the previous attempt's record is
            // about to be overwritten. Bank what it spent first: those tokens
            // were real, and a task that took three attempts should say so.
            // It may also carry a pane [`Dispatcher::retire`] left standing
            // under this exact name on an earlier pass — see
            // [`LaneRecord::retired_pane`] — pulled out before the record goes.
            let previous = self.lanes.remove(&lane_name(&step.id, task.id()));
            let retired_pane = previous
                .as_ref()
                .and_then(|record| record.retired_pane.clone());
            if let Some(previous) = previous {
                self.record_usage(&previous, task.id(), &step.id, Some(task), &pipeline.name);
            }

            // The pane the step before this one left behind, if this pass took
            // one back, or — with no such pass, because it is this step's own
            // name starting again — the one `retired_pane` just carried
            // forward from whichever pass `Dispatcher::retire` ran on. A
            // `ready` pane is standing empty at its shell, so the lane starts
            // in it and the task carries the same `pane_id` across the step
            // change instead of churning a pane per step. Anything else is a
            // pane with an agent still in it: `start_one` splits a fresh one,
            // and the stuck pane is closed below — after that split, never
            // before, or a tab whose last pane it was would go with it.
            let handover = handovers.remove(task.id()).or_else(|| {
                retired_pane.map(|pane| PaneHandover {
                    lane: lane_name(&step.id, task.id()),
                    pane_id: pane.pane_id,
                    ready: pane.ready,
                })
            });
            let inherited = handover
                .as_ref()
                .filter(|handover| handover.ready)
                .map(|handover| handover.pane_id.clone());

            // The pass's one ledger snapshot, built on first use here and
            // reused for every later candidate — `start_one`'s session
            // lookups answer from it rather than each parsing `usage.jsonl`
            // (review finding 33). A pass with no candidates never builds it.
            let ledger = self.ledger();
            let outcome = start_one(
                self.repo,
                &pipeline,
                self.mux,
                task,
                &step,
                &profile,
                inherited.as_deref(),
                &self.report_seen,
                &ledger,
            );
            if let Some(stuck) = handover.filter(|handover| !handover.ready) {
                match outcome.is_ok() {
                    true => self.close_handover(&stuck, report),
                    // Nothing replaced it, so it is not closed here. Handed
                    // back instead, and closed with the rest of what this pass
                    // did not place — a stuck pane left for the next pass is a
                    // lane it would count as live.
                    false => {
                        handovers.insert(task.id().to_string(), stuck);
                    }
                }
            }
            match outcome {
                Ok(started) => {
                    let name = started.name;
                    let action = match &started.note {
                        Some(note) => format!("started {name} — {note}"),
                        None => format!("started {name}"),
                    };
                    self.lanes.insert(
                        name.clone(),
                        LaneRecord {
                            started_at: now_secs(),
                            last_progress: now_secs(),
                            output_hash: 0,
                            notified: false,
                            reminded_at: None,
                            reminders: 0,
                            session: started.session,
                            kind: profile.kind.clone(),
                            agent: agent_name.clone(),
                            model: started.model,
                            head: started.head,
                            held_for_block: false,
                            person_turn_busy: false,
                            handing_over_since: None,
                            retired_pane: None,
                            child_since: None,
                        },
                    );
                    *in_flight.entry(agent_name).or_insert(0) += 1;
                    *model_in_flight.entry(model_name.clone()).or_insert(0) += 1;
                    if model_price.is_some_and(|p| p.exclusive) {
                        resident_exclusive.get_or_insert(model_name);
                    }
                    report.actions.push(action);
                }
                Err(err) => report.problems.push(format!(
                    "{}: could not start `{}`: {err:#}",
                    task.id(),
                    step.id
                )),
            }
        }

        Ok(())
    }

    /// Start, or look in on, the command a task is sitting on — and say where it
    /// goes when there is an answer. `None` is "still running", which is what
    /// most passes over a blocking command see.
    ///
    /// Nothing here consults a model, and nothing confines the process: a `run:`
    /// line is the operator's own command, from a file only people write. See
    /// [`crate::command_step`].
    /// Whether arriving at `destination` means queueing for a worker slot.
    ///
    /// Only an agent step runs a lane, and `blocked` is the one agent step
    /// that may not: an attended run parks there for a person.
    ///
    /// The guard at the top of a pass already leaves a task *sitting* on
    /// `blocked` alone. A task routed there during a pass never reached it —
    /// it went straight into the slot allocator and its unblocker was started
    /// in the same pass, so in an attended run the lane reported, the task
    /// went back to where it blocked from, and the whole flow came round
    /// again. A gate that never turns green circled forever in exactly the run
    /// that had a person to stop for, and the task file recorded eight
    /// milliseconds on `blocked` between the two.
    fn queues_for_a_slot(&self, pipeline: &Pipeline, destination: &str) -> bool {
        if destination == crate::pipeline::BLOCKED && !pipeline.blocked_is_staffed(self.unattended)
        {
            return false;
        }
        pipeline.step(destination).map(|s| s.kind()) == Some(StepKind::Agent)
    }

    fn run_command(
        &mut self,
        task: &mut Task,
        step: &Step,
        report: &mut Report,
    ) -> Result<Option<String>> {
        let id = task.id().to_string();
        let Some(run) = step.run.clone() else {
            // Refused at load, so reaching this means a pipeline was rewritten
            // under a running task. Say so rather than silently parking it.
            report.problems.push(format!(
                "{id}: step `{}` is kind `command` but names no `run:`",
                step.id
            ));
            return Ok(None);
        };
        let runs = crate::command_step::Runs::new(&self.repo.commands_dir());
        let key = crate::command_step::Runs::key(&step.id, &id);

        match runs.state(&key) {
            crate::command_step::RunState::Running => {
                // A background run still going is not this task's wait. The
                // pass that started it already routed the task on — but that
                // answer lives only in that pass's return value, so a pass
                // that could not *place* the destination (no free slot, a
                // launch that failed) left the task sitting here, and every
                // later pass used to read the long-running command as work to
                // wait out. An observer with a six-hour timeout parked a task
                // for six hours over a placement that failed once. Re-answer
                // `on_pass` instead, every pass, until the destination lands.
                if step.background {
                    return Ok(step.on_pass.clone());
                }
                // The one bound a command step has. A hung command is
                // indistinguishable from a slow one by looking, so what tells
                // them apart is the number the step wrote down — and without it
                // the task would sit here for as long as the dispatcher runs.
                let limit = step.command_timeout();
                if runs.elapsed(&key).unwrap_or_default() < limit {
                    return Ok(None);
                }
                runs.stop(&key);
                let reason = format!(
                    "`{}` ran past its timeout of {} and was stopped",
                    step.id,
                    crate::config::human_duration::format(limit)
                );
                report.actions.push(format!("{id}: {reason}"));
                // A timeout is a failure of the step and routes like one — never
                // silently a pass, because a build that never finished did not
                // succeed.
                Ok(Some(
                    step.on_fail
                        .clone()
                        .unwrap_or_else(|| crate::pipeline::BLOCKED.to_string()),
                ))
            }

            crate::command_step::RunState::Exited(code) => {
                // Read once and cleared, so a task that comes back round to this
                // step runs the command again instead of routing on the code the
                // last arrival left behind.
                runs.forget(&key);
                // A passing command's pane has nothing left to show — closed
                // here, the same instant the step itself is judged done. A
                // failing one is left standing: see the `Fresh` arm below,
                // which is what replaces it on the next arrival.
                if code == 0
                    && let Some(pane) = runs.pane(&key)
                {
                    let _ = self.mux.close_pane(&pane);
                    runs.forget_pane(&key);
                }
                let destination = match code {
                    0 => step.on_pass.clone(),
                    _ => Some(
                        step.on_fail
                            .clone()
                            .unwrap_or_else(|| crate::pipeline::BLOCKED.to_string()),
                    ),
                };
                // Named on the task, not only in this pass's own report: a
                // failure here used to leave the task file saying only that
                // it moved to `blocked`, with the reason living solely in a
                // pass report nobody clearing the block reads. `Runs::forget`
                // above keeps the log on disk on purpose, so the step, the
                // code and that path are what the lane sent in to clear the
                // block needs to see what actually broke.
                if code != 0 {
                    task.append_to_section(
                        "## Status Log",
                        &format!(
                            "- `{}` exited {code} — see {}\n",
                            step.id,
                            runs.log_path(&key).display()
                        ),
                    );
                }
                report.actions.push(format!(
                    "{id}: `{}` exited {code} — {}",
                    step.id,
                    match &destination {
                        Some(to) => format!("moving to `{to}`"),
                        None => "staying put".to_string(),
                    }
                ));
                Ok(destination)
            }

            crate::command_step::RunState::Interrupted => {
                // Not a verdict, so it routes nowhere. Clearing the run leaves
                // the task on this step with nothing started, which the next
                // pass reads as `Fresh` and runs again.
                //
                // Running it again is the only honest answer: the command was
                // never allowed to finish, so nothing knows whether it would
                // have passed. It is also cheap, because nothing here spends a
                // model — where routing this down `on_fail` spends an agent
                // turn re-diagnosing a failure that never happened, and a
                // second one costs the task its loop budget.
                //
                // Nothing counts the re-runs. A command that reaches this arm
                // twice was `SIGKILL`ed twice, and the thing doing the killing
                // is a dispatcher shutting down or the machine going down —
                // neither of which is a loop this could break out of by
                // escalating instead.
                runs.forget(&key);
                // Closed rather than left standing: the run is about to be
                // started again from `Fresh`, which would only replace it
                // anyway, and a killed multiplexer is the one case a pane
                // this backend still thinks is there may not actually be.
                if let Some(pane) = runs.pane(&key) {
                    let _ = self.mux.close_pane(&pane);
                    runs.forget_pane(&key);
                }
                report.actions.push(format!(
                    "{id}: `{}` was interrupted without an exit code — running it again",
                    step.id
                ));
                Ok(None)
            }

            crate::command_step::RunState::Fresh => {
                if self.dry_run {
                    report
                        .actions
                        .push(format!("would run `{}` for {id}: {run}", step.id));
                    return Ok(None);
                }

                let (worktree, _) = ensure_workspace(self.repo, self.mux, task, &self.report_seen)?;
                let env = BTreeMap::from([
                    (crate::commands::TASK_ENV.to_string(), id.clone()),
                    (ENV_STEP.to_string(), step.id.clone()),
                    (
                        "SPOOLWAY_REPO".to_string(),
                        self.repo.root.display().to_string(),
                    ),
                    (
                        "SPOOLWAY_WORKTREE".to_string(),
                        worktree.display().to_string(),
                    ),
                    (
                        "SPOOLWAY_TASK_FILE".to_string(),
                        task.path.display().to_string(),
                    ),
                ]);
                // A pane a previous, failing arrival at this step left
                // standing — see the `Exited` arm above — is replaced rather
                // than piled onto, whether or not this arrival still wants
                // one: a step edited to `headless: true` after a failure must
                // not leave that old pane standing forever either.
                if let Some(old_pane) = runs.pane(&key) {
                    let _ = self.mux.close_pane(&old_pane);
                    runs.forget_pane(&key);
                }

                let started = if step.headless {
                    runs.start(&key, &run, &worktree, &env)
                } else {
                    self.start_command_in_pane(task, &key, &run, &worktree, &env, &runs)
                };
                match started {
                    Ok(_) => {}
                    Err(err) => {
                        report
                            .problems
                            .push(format!("{id}: could not run `{}`: {err:#}", step.id));
                        return Ok(None);
                    }
                }

                // A background step is done with the moment it has started: the
                // task moves on, and the exit code is nobody's to route on —
                // which is why `on_fail` is refused on one. What it wrote is in
                // its log either way.
                if step.background {
                    report.actions.push(format!(
                        "{id}: started `{}` in the background — {}",
                        step.id,
                        runs.log_path(&key).display()
                    ));
                    return Ok(step.on_pass.clone());
                }
                report.actions.push(match runs.pane(&key) {
                    // A pane of its own, split off the task's own tab: a
                    // person watching this task's panes sees the command
                    // running rather than whatever the last agent step left
                    // on screen.
                    Some(_) => format!(
                        "{id}: running `{}` in its own pane — {}",
                        step.id,
                        runs.log_path(&key).display()
                    ),
                    // `headless: true`, or a backend with no pane to offer —
                    // see [`crate::mux::Mux::run_in_pane`]. Naming the log
                    // here is the only notice anybody gets that this step is
                    // doing something: its pane shows whatever the last
                    // agent step left on screen, and a lane forty minutes
                    // into a suite looks exactly like one whose agent died.
                    // `spoolway lane <lane>` reads the same file.
                    None => format!(
                        "{id}: running `{}` — {}",
                        step.id,
                        runs.log_path(&key).display()
                    ),
                });
                Ok(None)
            }
        }
    }

    /// Start a command step's run where a person can watch it: a pane split
    /// off the task's own tab, the same one `start_one` splits an agent
    /// lane's pane from — `task.front.tab_id`, falling back to
    /// `task.front.pane_id` for a backend with no tabs.
    ///
    /// Falls back to the old detached run whenever the backend has none to
    /// offer: [`crate::mux::Mux::run_in_pane`] answers `None` for headless,
    /// which is what makes `backend = "headless"` silently keep working —
    /// nothing here treats a declined pane as a problem to report.
    ///
    /// `run_in_pane` is handed the dispatcher's own environment as an
    /// inherited layer, with `env` — the step's named map — applied on top,
    /// same key wins to the named value. Under `headless` this whole question
    /// answers itself: `runs.start` below is a child of this process, so it
    /// inherits everything the dispatcher was started with, `SPOOLWAY_GH`
    /// among it, with no forwarding to write. A herdr or tmux pane has no
    /// such thing — the multiplexer server spawned it, from *its* own
    /// environment, which the dispatcher's exports never reached — so
    /// without handing this layer across explicitly a command step loses
    /// every variable that arrived on the dispatcher's own command line the
    /// moment it runs in a pane instead of headless. `script` itself is still
    /// built from `env` alone: what the wrapper writes into the script text
    /// is the small, named contract a step actually declares, not everything
    /// this process happens to be carrying.
    fn start_command_in_pane(
        &self,
        task: &Task,
        key: &str,
        run: &str,
        worktree: &Path,
        env: &BTreeMap<String, String>,
        runs: &crate::command_step::Runs,
    ) -> Result<u32> {
        let tab = task
            .front
            .tab_id
            .clone()
            .or_else(|| task.front.pane_id.clone())
            .context("task has a workspace but no recorded tab or pane")?;
        let script = runs.script_for_pane(key, run, env)?;
        // This process's own environment, because a pane belongs to the
        // multiplexer's server rather than to the dispatcher: without this the
        // command would run with whatever environment that server was started
        // with, not the one the person who started the dispatcher had.
        //
        // What the shell keeps for itself is left out. `PWD` is the one that
        // bites: a pane is opened in the task's worktree, but the dispatcher
        // runs from the main checkout, and handing its `PWD` over means a
        // `run:` line reading `$PWD` resolves against the main checkout while
        // every relative path in the same line resolves against the worktree.
        // That is how `SPOOLWAY="$PWD/target/release/spoolway"
        // scripts/e2e/run.sh` came to run this worktree's suites against the
        // main checkout's months-old binary. `OLDPWD`, `SHLVL` and `_` are the
        // same kind of shell bookkeeping and are dropped for the same reason.
        const SHELL_OWNED: [&str; 4] = ["PWD", "OLDPWD", "SHLVL", "_"];
        let mut pane_env: BTreeMap<String, String> = std::env::vars()
            .filter(|(name, _)| !SHELL_OWNED.contains(&name.as_str()))
            .collect();
        pane_env.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
        match self
            .mux
            .run_in_pane(&tab, worktree, key, &script, &pane_env)?
        {
            Some(pane) => {
                runs.record_pane(key, &pane)?;
                runs.await_started(key)
            }
            None => runs.start(key, run, worktree, env),
        }
    }

    /// Stop any of this task's command runs that have outstayed their step's
    /// `timeout:` — except the one on the step it is sitting on, which
    /// [`Dispatcher::run_command`] bounds itself and can say more about.
    ///
    /// In practice this is the background ones. Nothing routes on those, so
    /// their timeout is not a verdict on the work: it is the only thing standing
    /// between `background: true` and a process that outlives everything that
    /// knew about it.
    fn reap_stale_runs(&self, task: &Task, pipeline: &Pipeline, report: &mut Report) {
        if self.dry_run {
            return;
        }
        let runs = crate::command_step::Runs::new(&self.repo.commands_dir());
        // A key is `<task> · <step>` — see `crate::command_step::Runs::key`.
        let prefix = format!("{} · ", task.id());
        for key in runs.keys_for_task(task.id()) {
            let Some(step_id) = key.strip_prefix(&prefix) else {
                continue;
            };
            if step_id == task.stage() {
                continue;
            }
            let Some(step) = pipeline.step(step_id) else {
                continue;
            };
            if runs.state(&key) != crate::command_step::RunState::Running {
                continue;
            }
            let limit = step.command_timeout();
            if runs.elapsed(&key).unwrap_or_default() < limit {
                continue;
            }
            runs.stop(&key);
            report.actions.push(format!(
                "{}: background `{step_id}` ran past its timeout of {} and was stopped — see {}",
                task.id(),
                crate::config::human_duration::format(limit),
                runs.log_path(&key).display()
            ));
        }
    }

    /// Move a task to the blocked step and tell a person about it — or, in an
    /// unattended run whose pipeline does not staff `blocked`, send it back to
    /// have another go, because there is nobody to tell.
    ///
    /// A pipeline that does staff `blocked` gets neither: the task lands on
    /// `blocked` exactly as an attended run's would, and the scheduler starts
    /// an ordinary lane there on the next pass — see
    /// [`Pipeline::blocked_is_staffed`]. The old resume is what a pipeline with
    /// no such step still gets, unchanged: the same one `spoolway resume`
    /// performs by hand, through the same code — the lane that stopped is
    /// continued rather than replaced, and the loop budgets out of the step it
    /// is going back to are handed back. What it does *not* do is announce
    /// anything. A notification is a request for attention, and an unattended
    /// run has already been told there is none to ask for; a night of them is
    /// a night of noise nobody read.
    fn escalate(
        &mut self,
        task: &mut Task,
        pipeline: &Pipeline,
        step: &Step,
        reason: &str,
    ) -> Result<()> {
        task.append_to_section("## Blocker", &format!("- {reason}\n"));
        // Where it stopped, so resuming — by hand, by the run, or by a
        // staffed `blocked` lane's own pass — carries on rather than
        // restarting. A watchdog killing that lane itself calls this with
        // `step.id == blocked`; the guard inside keeps the *origin* step
        // rather than overwriting it with `blocked`.
        crate::commands::set_blocked_from(task, &step.id);

        // A `blocked` lane itself is what died or went silent, or is what a
        // spent launch ceiling gave up on starting. `blocked` has nowhere
        // else this escalation could send it — no `on_fail`, and every other
        // road here ends up back on `blocked` by definition — which is the
        // unbounded loop this whole task exists to close, so it takes the
        // same road `commands::report` gives a `--fail` or `--block` reported
        // from `blocked`: parked on `paused`, with `blocked_from` (just set,
        // or already there, by the guard above) surviving so a resume can
        // still reach the destination a pass would have.
        if step.id == crate::pipeline::BLOCKED {
            let origin = task
                .front
                .blocked_from
                .clone()
                .unwrap_or_else(|| crate::commands::resume_target(task, pipeline));
            task.front.paused_at = Some(origin);
            task.set_stage(crate::pipeline::PAUSED, Some(reason));
            self.persist(task)?;
            return Ok(());
        }

        if self.unattended && !pipeline.blocked_is_staffed(self.unattended) {
            let target = crate::commands::resume_target(task, pipeline);
            crate::commands::resume_at(task, pipeline, &target);
            task.set_stage(&target, Some(reason));
            self.persist(task)?;
            return Ok(());
        }

        task.set_stage(crate::pipeline::BLOCKED, Some(reason));
        self.persist(task)?;
        Ok(())
    }

    /// Whether this run has spent its output-token ceiling, and the sentence
    /// saying so.
    ///
    /// Only an unattended run has one. An attended run's brake is the person at
    /// the keyboard, and a ceiling there would stop a run somebody is sitting in
    /// front of — who would simply start it again, having learned nothing, since
    /// their own blocked tasks were already parked in front of them.
    ///
    /// Counted from the pass's one [`Dispatcher::ledger`] snapshot, over the
    /// same window as the board's footer: entries stamped at or after the
    /// moment this run took its lock. Each ceiling check used to parse the
    /// whole `usage.jsonl` on its own, every pass (review finding 33); now
    /// both read the snapshot every other consumer in the pass shares. The
    /// alternative — a running total in memory — would lose the count on a
    /// dispatcher restart and let a run that has already spent the budget
    /// spend it again.
    ///
    /// **Lane entries only.** The ledger also carries the operator's own
    /// interactive session — `usage::bank_ambient` banks planning and
    /// queueing against the project, which is right, and those lines have no
    /// `task`. Summed with the rest they make the one brake an unattended run
    /// has answer to whoever is *watching* it: sit in a Claude session reading
    /// an overnight run and your own context reads stop the dispatcher starting
    /// work. The ceiling is documented as the output tokens one unattended run
    /// may spend, and a lane is the only thing that run started.
    fn over_output_ceiling(&mut self) -> Option<String> {
        if !self.unattended {
            return None;
        }
        let ceiling = self.repo.config.unattended.max_output_tokens;
        if ceiling == 0 {
            return None;
        }
        let start = crate::status::run_start(self.repo);
        let ledger = self.ledger();
        let spent: u64 = ledger
            .iter()
            .filter(|entry| {
                start
                    .as_ref()
                    .is_none_or(|start| entry.ts.as_str() >= start.as_str())
            })
            .filter(|entry| !entry.task.is_empty())
            .map(|entry| entry.tokens.output)
            .sum();
        if spent < ceiling {
            return None;
        }
        Some(format!(
            "this run has spent {spent} output tokens against a max_output_tokens of {ceiling} — \
             starting nothing further. Lanes still open will finish, and the queue keeps its \
             place for the next run"
        ))
    }

    /// [`Dispatcher::over_output_ceiling`]'s own counterpart in money rather
    /// than tokens — see `unattended.max_cost_usd`.
    ///
    /// Priced the same way every ledger line already is: `crate::usage::price`
    /// through project config, the refreshed table, then the one vendored into
    /// the binary. The premise `max_output_tokens` was written against — that
    /// `[models]` ships empty and a priced ceiling would silently never fire —
    /// stopped holding the moment that table shipped. A line this run's own
    /// `record_usage` could not price at all — a model the tables have never
    /// heard of, with nothing in `[models]` either — contributes nothing to the
    /// sum rather than being estimated, the same rule `Entry::cost_usd` follows
    /// everywhere else: never invented, only read.
    fn over_cost_ceiling(&mut self) -> Option<String> {
        if !self.unattended {
            return None;
        }
        let ceiling = self.repo.config.unattended.max_cost_usd;
        if ceiling <= 0.0 {
            return None;
        }
        let start = crate::status::run_start(self.repo);
        let ledger = self.ledger();
        let spent: f64 = ledger
            .iter()
            .filter(|entry| {
                start
                    .as_ref()
                    .is_none_or(|start| entry.ts.as_str() >= start.as_str())
            })
            .filter(|entry| !entry.task.is_empty())
            .filter_map(|entry| entry.cost_usd)
            .sum();
        if spent < ceiling {
            return None;
        }
        Some(format!(
            "this run has spent ${spent:.2} against a max_cost_usd of ${ceiling:.2} — starting \
             nothing further. Lanes still open will finish, and the queue keeps its place for \
             the next run"
        ))
    }

    /// Whether an escalation from here is going to stop in front of a person,
    /// which is what decides whether its pane is kept.
    ///
    /// Nothing parks in an unattended run, so nothing keeps a pane there
    /// either: the pane is held open for somebody to read, and the lane is
    /// about to be continued in it regardless.
    fn parks_on_blocked(&self) -> bool {
        !self.unattended
    }

    /// Mark, once, that a lane is waiting on an answer in its pane — read back
    /// by [`lanes_awaiting_a_person`] for the board.
    ///
    /// The flag lives on the lane record rather than on the task, so a lane
    /// that waits overnight is marked once and not once per pass. A lane this
    /// dispatcher did not start has no record yet — an interrupted pass
    /// picking up where it left off — and gets one here.
    ///
    /// **Not on the first settled reading.** The caller cannot tell a lane
    /// holding a question from one that ended its turn on a background job it
    /// started, and the second is what the `e2e` prompt does every time: it
    /// starts a suite run allowed 45 minutes and stops talking. Marking that
    /// immediately put `● paused — look at pane …` on the board against a
    /// pane where nobody had asked anything, and sent a person to go and
    /// look at it.
    ///
    /// So a lane earns the mark the same way it earns a reminder, by being
    /// quiet for `dispatch.lane_quiet` — the patience `b00bb5c` gave the
    /// watchdog and never gave this. Silence is read off `last_progress`,
    /// which is the transcript's own write time rather than this pass noticing
    /// anything, so a lane that is typing is never quiet.
    ///
    /// A lane with no record is one this dispatcher did not start. It gets a
    /// record and no mark: nothing knows yet how long it has been quiet, and
    /// the next pass will.
    fn announce_waiting(&mut self, lane_name: &str) {
        if self.dry_run {
            return;
        }

        let now = now_secs();
        let quiet = self.repo.config.dispatch.lane_quiet;
        let ledger = self.ledger();
        let record = self
            .lanes
            .entry(lane_name.to_string())
            .or_insert_with(|| LaneRecord::readopted(lane_name, now, &ledger));
        let silent_for =
            Duration::from_secs(now.saturating_sub(record.last_progress).max(0) as u64);
        if silent_for >= quiet {
            record.notified = true;
        }
    }

    /// Take the mark back off a lane that has gone back to work.
    ///
    /// The mark is read as "this pane is holding a question", and a lane
    /// mid-turn is not: the question was answered, or the lane only looked
    /// settled for a moment between turns. Leaving it set is what had the
    /// board reading `● paused` at a pane the agent was visibly still
    /// working in, for the rest of the lane's life — the mark used to be
    /// cleared by the watchdog that ran here, and nothing took that over when
    /// the clocks were retired.
    ///
    /// Only ever clears an existing record. A busy lane with no record is one
    /// this dispatcher did not start, and there is nothing to take back.
    fn withdraw_waiting(&mut self, lane_name: &str) {
        if self.dry_run {
            return;
        }
        if let Some(record) = self.lanes.get_mut(lane_name) {
            record.notified = false;
        }
    }
}

/// What a task's branch came to against `base_commit`, read from
/// `git diff --shortstat` in the worktree while it still exists.
///
/// Best-effort, like the rest of accounting: a worktree already gone, or a
/// commit git no longer has, costs a `patch` nobody can measure rather than a
/// failed cleanup.
pub(crate) fn measure_patch(worktree: &Path, base_commit: &str) -> Option<crate::task::Patch> {
    let stat = crate::repo::run(
        worktree,
        "git",
        &["diff", "--shortstat", base_commit, "HEAD"],
    )
    .ok()?;
    parse_shortstat(&stat)
}

/// `" 4 files changed, 168 insertions(+), 44 deletions(-)"` — any of the three
/// clauses may be missing when that count is zero, so each is hunted for
/// independently rather than assumed to be in a fixed position.
fn parse_shortstat(stat: &str) -> Option<crate::task::Patch> {
    let stat = stat.trim();
    if stat.is_empty() {
        // No lines differ, which is a real and common answer — a step that
        // touched nothing still gets a patch, just an empty one.
        return Some(crate::task::Patch::default());
    }
    let number_before = |needle: &str| -> usize {
        stat.find(needle)
            .and_then(|at| stat[..at].trim_end().rsplit(' ').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    Some(crate::task::Patch {
        files: number_before(" file"),
        insertions: number_before(" insertion"),
        deletions: number_before(" deletion"),
    })
}

/// The step a lane was started for. `spoolway report` reads it to refuse a
/// report against any other step. Never read by a prompt: one that branches
/// on it has learned how many steps run it, and two roles are two files.
pub const ENV_STEP: &str = "SPOOLWAY_STEP";

/// The session id spoolway minted for this lane.
///
/// Most kinds' argv carries this as `{session_id}` too; codex refuses any id
/// but its own, so it is pinned by the home `CODEX_HOME` points at instead —
/// see [`crate::agent::Home`]. Set into the environment unconditionally
/// rather than only for a kind that needs it told to the *backend* rather
/// than the agent — the same reasoning `ENV_STEP` already rests on — so a
/// future kind that needs it some other way costs no new plumbing.
pub const ENV_SESSION: &str = "SPOOLWAY_SESSION";

#[cfg(test)]
thread_local! {
    /// Task-file writes this thread has made through [`persist_task`], which
    /// is the dispatcher's only route to disk — [`Dispatcher::persist`] goes
    /// through it too, and nothing else in a pass calls `Task::save`.
    ///
    /// Counted so a test can assert *how many times* a pass wrote, not only
    /// what the file ended up saying. The park contract needs both: a clear
    /// followed by a re-park leaves exactly the same final fields as one
    /// resolving write, so the fields alone cannot tell the two apart. Read
    /// through [`saves_during`].
    static SAVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Run `body` and say how many task files it wrote, alongside whatever it
/// returned. Counts every task the pass touched, so a fixture asserting an
/// exact number wants one task in its queue.
#[cfg(test)]
fn saves_during<T>(body: impl FnOnce() -> T) -> (T, usize) {
    SAVES.with(|n| n.set(0));
    let out = body();
    (out, SAVES.with(std::cell::Cell::get))
}

/// [`Dispatcher::persist`] for the free functions in the launch path, which
/// have no `self` to reach the pass's `report_seen` through. Same rule: take
/// the task's per-task lock, and skip the write when a lane's `spoolway
/// report` has advanced `last_report` past what this pass first read — the
/// lost-update review finding 2 guards against.
fn persist_task(repo: &Repo, task: &mut Task, report_seen: &HashMap<String, i64>) -> Result<()> {
    // Bound to a name, not discarded: an `Ok` holds the lock in it until
    // this function returns, which is what keeps the reload check and the
    // write below atomic against a lane's `spoolway report`.
    let lock = crate::lock::TaskLock::acquire(&repo.task_lock_file(task.id()));
    if lock.is_err() {
        // A live holder that never let go in three seconds. Rare enough to
        // log and press on unlocked rather than fail a whole pass for one
        // task.
        crate::problem_log::append(
            repo,
            &format!("{}: task lock still held, saving without it", task.id()),
        );
    }
    let disk_at = Task::load(&task.path)
        .ok()
        .and_then(|disk| disk.front.last_report.map(|report| report.at))
        .unwrap_or(0);
    if disk_at > report_seen.get(task.id()).copied().unwrap_or(0) {
        return Ok(());
    }
    let saved = task.save();
    #[cfg(test)]
    if saved.is_ok() {
        SAVES.with(|n| n.set(n.get() + 1));
    }
    saved
}

/// Create the task's worktree if it has none, start its agent in that pane,
/// label everything, and send the opening prompt.
/// The checkout this task's work happens in, cut or borrowed on first need.
///
/// Shared by everything that runs *something* for a task — an agent lane and a
/// command step both — because both need the same directory, and two answers to
/// "where does this task's work live" would be one too many.
fn ensure_workspace(
    repo: &Repo,
    mux: &dyn Mux,
    task: &mut Task,
    report_seen: &HashMap<String, i64>,
) -> Result<(PathBuf, Option<String>)> {
    // Where a task's lane sits — its worktree, workspace and panes — is a fact
    // about one machine, and the only part of a task file that is. A task file
    // that arrives from somewhere else carries a path that does not exist here,
    // and trusting it would skip cutting a worktree and then address a pane in
    // a multiplexer this machine has never seen.
    //
    // So it is verified rather than believed, which also covers the case that
    // has nothing to do with anyone else: a worktree deleted by hand, or a
    // workspace herdr lost, used to wedge the task forever.
    if let Some(recorded) = task.front.worktree_path.clone()
        && !recorded.is_dir()
    {
        task.front.worktree_path = None;
        task.front.workspace_id = None;
        task.front.pane_id = None;
        task.front.tab_id = None;
        persist_task(repo, task, report_seen)?;
    }

    // The directory can survive a restart of the multiplexer fronting it even
    // though every id it ever handed out did not — a reboot, a `tmux
    // kill-server`, herdr restarted. Believing `workspace_id` and `tab_id`
    // then means `start_one` splits into a tab nothing answers to, on this
    // pass and on every pass after it, so they are checked against the live
    // multiplexer rather than trusted outright.
    //
    // Only the multiplexer ids move here — `worktree_path` and `branch` are
    // left exactly as recorded, and `borrowed` is never touched. Routing this
    // through the cut/borrow logic below instead would ask git for the
    // branch's checkout again, find this task's own worktree already there,
    // and read that as a *borrow* — which would silently stop the normal
    // cleanup from ever removing a worktree this task actually cut for
    // itself.

    // A pane this call has just now handed the task exclusively — nobody
    // else's lane is in it, so `start_one` uses it directly as the task's
    // very first pane instead of splitting one off it. Set only in a branch
    // below that establishes the task's placement from scratch; `None` on a
    // call that finds it already alive and does nothing here at all.
    let mut fresh_pane: Option<String> = None;

    if let Some(workspace_id) = task.front.workspace_id.clone()
        && !mux.workspace_alive(&workspace_id, task.front.tab_id.as_deref())?
    {
        task.front.workspace_id = None;
        task.front.pane_id = None;
        task.front.tab_id = None;

        let checkout = task
            .front
            .worktree_path
            .clone()
            .context("workspace went stale but the task has no worktree to reopen a pane on")?;
        // `reopen_owned_pane`, never the plain `create_pane`, for a checkout
        // this task cut for itself: a backend that marks ownership on the
        // session rather than asking the multiplexer directly (tmux's
        // `@spoolway_checkout`) stamps only through that call, and a pane
        // opened without it answers to `remove_workspace` as though the
        // checkout were borrowed — cleanup then refuses to take it back and
        // the worktree leaks. A genuinely borrowed checkout gets the
        // unstamped pane it always did: it is not this task's to remove
        // either way.
        match mux.task_owns_workspace() {
            true if task.front.borrowed => {
                let workspace = mux.create_pane(&checkout, &format!("spoolway/{}", task.id()))?;
                task.front.workspace_id = Some(workspace.workspace_id);
                task.front.pane_id = Some(workspace.pane_id.clone());
                task.front.tab_id = workspace.tab_id;
                fresh_pane = Some(workspace.pane_id);
            }
            true => {
                let workspace =
                    mux.reopen_owned_pane(&checkout, &format!("spoolway/{}", task.id()))?;
                task.front.workspace_id = Some(workspace.workspace_id);
                task.front.pane_id = Some(workspace.pane_id.clone());
                task.front.tab_id = workspace.tab_id;
                fresh_pane = Some(workspace.pane_id);
            }
            false => {
                let tab = project_tab(repo, mux, Some(&checkout))?
                    .context("the run has no shared tab to open this task's pane in")?;
                task.front.workspace_id = Some(tab.workspace_id);
                task.front.pane_id = tab.opened_pane.clone();
                task.front.tab_id = Some(tab.tab_id);
                fresh_pane = tab.opened_pane;
            }
        }
        persist_task(repo, task, report_seen)?;
    }

    let base = match &task.front.base {
        Some(base) => base.clone(),
        None => repo.branch()?,
    };
    // Every lane runs in a worktree. The only question is whose, and git
    // answers it rather than a setting: a branch cannot be checked out twice, so
    // a task whose branch somebody already has out is *borrowing* that checkout
    // and a task whose branch is nowhere gets one cut for it.
    //
    // No knob says so, and none ever did: it is a fact about git rather than a
    // mode. What used to depend on it was the plan closeout, which ran in the
    // main checkout on purpose; that is gone — the last task of a plan documents
    // on its own branch like any other work — and what is left is the ordinary
    // case of a person having a task's branch out while spoolway reaches it.
    let branch = task
        .front
        .branch
        .clone()
        .unwrap_or_else(|| crate::task::default_branch(task.id()));

    if task.front.workspace_id.is_none() {
        // Under `split` a task cuts a workspace (or a pane) of its own, named
        // for itself; under `grouped` every task shares the tab its *project*
        // has in the run's workspace, so what it needs is that tab's own
        // bookkeeping rather than a row of its own — see [`project_tab`].
        let owns = mux.task_owns_workspace();
        match repo.worktree_for(&branch)? {
            // Borrowed. There is no worktree of ours under this workspace, and
            // cleanup has to know that, so it is written down rather than
            // guessed at later — by then our own worktree would look the same.
            Some(checkout) => {
                match owns {
                    true => {
                        let workspace =
                            mux.create_pane(&checkout, &format!("spoolway/{}", task.id()))?;
                        task.front.workspace_id = Some(workspace.workspace_id);
                        task.front.pane_id = Some(workspace.pane_id.clone());
                        task.front.tab_id = workspace.tab_id;
                        fresh_pane = Some(workspace.pane_id);
                    }
                    false => {
                        let tab = project_tab(repo, mux, Some(&checkout))?
                            .context("the run has no shared tab to open this task's pane in")?;
                        task.front.workspace_id = Some(tab.workspace_id);
                        task.front.pane_id = tab.opened_pane.clone();
                        task.front.tab_id = Some(tab.tab_id);
                        fresh_pane = tab.opened_pane;
                    }
                };
                task.front.borrowed = true;
                task.front.branch = Some(branch);
                task.front.base = Some(base);
                task.front.worktree_path = Some(checkout);
                // A run is minted whenever a task's worktree is first set up,
                // borrowed included — it is what a lane's ledger line is
                // banked under from here on. `base_commit` is not: a borrowed
                // checkout was cut from something before this task ever
                // touched it, at a moment nothing here witnessed, so there is
                // no honest commit to pin.
                task.front.run = Some(crate::usage::new_run_id());
                persist_task(repo, task, report_seen)?;
            }
            // Cut. One worktree per task, created once and reused by every
            // later step.
            None => {
                // A dependency has always reached `done` before this task is
                // ready to start — `Graph::ready` in `src/graph.rs` is what
                // enforces that — so its branch is finished and ancestry is a
                // fact rather than something to build later with a rebase.
                // `base` keeps its own meaning — the branch the plan lands in
                // — regardless: only the commit the worktree actually starts
                // from moves.
                //
                // The dependency's branch is read from its own task file, not
                // rebuilt as `task/<dep>`: `issue_tracking.key_in_names` can
                // stamp `task/<slug>-<dep>`, and a wrong name here becomes a
                // failed `git worktree add` rather than a wrong
                // diff. An unresolvable dependency fails by name here rather
                // than at the cut — see [`Repo::dependency_branch`].
                let cut_from = match task.front.depends_on.first() {
                    Some(dep) => repo.dependency_branch(dep)?,
                    None => base.clone(),
                };
                // Resolved just before the cut, so it names the exact commit
                // the new branch's history starts from — the one thing a
                // replay of this task can pin to once `cut_from` itself has
                // moved on or been deleted.
                let base_commit = repo
                    .git(&["rev-parse", &cut_from])
                    .ok()
                    .map(|c| c.trim().to_string());
                match owns {
                    true => {
                        let workspace = mux.create_workspace(
                            &repo.root,
                            &branch,
                            &cut_from,
                            &format!("spoolway/{}", task.id()),
                        )?;
                        task.front.workspace_id = Some(workspace.workspace_id);
                        task.front.pane_id = Some(workspace.pane_id.clone());
                        task.front.tab_id = workspace.tab_id;
                        task.front.worktree_path = Some(workspace.checkout_path);
                        fresh_pane = Some(workspace.pane_id);
                    }
                    false => {
                        // Cut with git directly rather than opened through the
                        // multiplexer: the shared tab has no worktree of its
                        // own for `Mux::create_workspace` to cut one under —
                        // see `Mux::remove_checkout`, the removal this pairs
                        // with.
                        let path = crate::mux::worktree_root(&repo.root, &repo.config.dispatch)
                            .join(task.id());
                        crate::mux::cut_worktree(&repo.root, &path, &branch, &cut_from)?;
                        // Opened on this exact worktree — the first task
                        // through here gives the project's shared tab a real
                        // home instead of the bare project root, and its pane
                        // is this task's own rather than a placeholder to
                        // split from.
                        let tab = project_tab(repo, mux, Some(&path))?
                            .context("the run has no shared tab to open this task's pane in")?;
                        task.front.workspace_id = Some(tab.workspace_id);
                        task.front.pane_id = tab.opened_pane.clone();
                        task.front.tab_id = Some(tab.tab_id);
                        task.front.worktree_path = Some(path);
                        fresh_pane = tab.opened_pane;
                    }
                };

                task.front.borrowed = false;
                task.front.branch = Some(branch);
                task.front.base = Some(base);
                task.front.cut_from = Some(cut_from);
                task.front.base_commit = base_commit;
                task.front.run = Some(crate::usage::new_run_id());
                persist_task(repo, task, report_seen)?;
            }
        }
    }

    Ok((
        task.front
            .worktree_path
            .clone()
            .unwrap_or_else(|| repo.root.clone()),
        fresh_pane,
    ))
}

/// The tab every lane of this project is split into, in the run's shared
/// dispatch workspace — a pane for every task of this project the run
/// currently has going, whatever plan each belongs to, and nothing else:
/// there is no anchor pane sitting in the project root taking up space any
/// more, now that a task owns its pane for its whole life instead of
/// splitting a new one every step.
///
/// One tab per project rather than one per plan: what a person switches
/// between is projects, not plans, and the dispatcher draws in the caller's
/// own pane now rather than a tab of its own — see
/// [`crate::commands::dispatch`] — so there is no board tab left to keep
/// separate from this one.
///
/// The label is [`crate::mux::project_label`], which is what the sidebar
/// shows.
///
/// Found rather than made whenever this project already has one: the tab a
/// previous run opened is still there under this project's label, which is
/// what makes a second `spoolway dispatch` join it instead of opening
/// another beside it. Asked of the multiplexer rather than reconstructed
/// from what the queue recorded, because the queue is not a complete
/// record of it: a project whose tasks are all between steps holds no tab
/// id anywhere.
///
/// `None` under [`crate::config::MuxMode::Split`] or from a backend with no
/// shared workspace at all.
///
/// `open_on` says whether to open the shared workspace, and then this
/// project's tab in it, when neither turns up anything — and, when it does,
/// the worktree to open the tab on: the first task through here gives the
/// tab a real home instead of the bare project root, so the pane it opens
/// with is one this task can use directly rather than a placeholder to
/// split from — see [`ProjectTab::opened_pane`]. `None` only for a caller
/// that wants to look without opening; every caller here passes `Some`, and
/// the stop sweep closes the tab from the id already recorded on the task,
/// so it never calls this at all.
pub fn project_tab(
    repo: &Repo,
    mux: &dyn Mux,
    open_on: Option<&Path>,
) -> Result<Option<ProjectTab>> {
    let Some(workspace_id) = mux.dispatch_workspace(&repo.root, open_on.is_some())? else {
        return Ok(None);
    };

    let label = crate::mux::project_label(&repo.root);
    if let Some(tab_id) = mux.find_tab(&workspace_id, &label)? {
        return Ok(Some(ProjectTab {
            workspace_id,
            tab_id,
            opened_pane: None,
        }));
    }

    let Some(cwd) = open_on else {
        return Ok(None);
    };
    let opened = mux.open_tab(&workspace_id, cwd, &label)?;
    Ok(Some(ProjectTab {
        workspace_id: opened.workspace_id,
        tab_id: opened
            .tab_id
            .context("a multiplexer with tabs opened one with no id")?,
        opened_pane: Some(opened.pane_id),
    }))
}

/// What [`project_tab`] found or made.
pub struct ProjectTab {
    pub workspace_id: String,
    pub tab_id: String,
    /// Set only when this call is the one that just opened the tab: its own
    /// pane, sitting exactly on the worktree the caller gave, and free to
    /// use directly as that task's first lane rather than splitting one off
    /// it — see `ensure_workspace`, the only caller. `None` when the tab was
    /// already there: every pane inside it already belongs to some other
    /// task's lane, and the caller splits its own the ordinary way.
    pub opened_pane: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn start_one(
    repo: &Repo,
    pipeline: &Pipeline,
    mux: &dyn Mux,
    task: &mut Task,
    step: &Step,
    profile: &AgentProfile,
    // A pane the step before this one left standing and empty, for this lane
    // to start in rather than splitting one of its own. `None` on the first
    // step of a task, on a kind with no way to leave a pane behind, and on a
    // backend whose pane is the agent itself.
    inherited: Option<&str>,
    report_seen: &HashMap<String, i64>,
    // The pass's one usage-ledger snapshot, for the session lookups below —
    // see [`Dispatcher::ledger`] and review finding 33.
    ledger: &[crate::usage::Entry],
) -> Result<Started> {
    let name = lane_name(&step.id, task.id());

    // Read before `ensure_workspace`, which is the one thing here that can cut
    // the task a new workspace underneath us. An inherited pane belongs to the
    // placement the task had when the pane was taken back; if that placement
    // has just been replaced, the pane id names something in a workspace that
    // is gone, and splitting a fresh one is the only correct answer.
    let placement = task.front.workspace_id.clone();
    let (_, fresh_pane) = ensure_workspace(repo, mux, task, report_seen)?;
    let inherited: Option<String> = inherited
        .filter(|_| placement.is_some() && placement == task.front.workspace_id)
        .map(str::to_string)
        // `ensure_workspace` just placed the task in a pane nobody else is
        // using — the tab it just opened, or the workspace it just cut or
        // reopened — so this step starts in it directly instead of splitting
        // one more off it. Gated on a real tab: headless leaves `tab_id`
        // unset and reaches `Mux::split_pane` for every step regardless, and
        // this must never change that — see [`crate::headless::Headless`].
        .or_else(|| fresh_pane.filter(|_| task.front.tab_id.is_some()));

    // The tab recorded on the task is where its lane's pane is split — its
    // own, under `split`, or its project's shared one under `grouped`. Which
    // pane in it actually splits is the backend's own decision, every time —
    // see [`Mux::split_pane`].
    //
    // Not every backend has tabs, though, and one without them is not a task
    // that went wrong: `headless` records a pane and never a tab (its
    // `create_workspace` and `create_pane` both answer `tab_id: None`), and
    // the pane it recorded is exactly the parent to split from. So the tab is
    // preferred and the pane is the fallback — demanding a tab here failed
    // every headless lane before it started.
    let tab = task
        .front
        .tab_id
        .clone()
        .or_else(|| task.front.pane_id.clone())
        .context("task has a workspace but no recorded tab or pane")?;
    let worktree = task
        .front
        .worktree_path
        .clone()
        .unwrap_or_else(|| repo.root.clone());

    // Which prompt runs is the pipeline's `merge:` as much as the step's own
    // `prompt:` — merging with git and merging through a forge are two roles
    // rather than one role with a branch in it.
    let prompt_path = crate::prompt::path_for(repo, step.prompt_name());
    let prompt = std::fs::read_to_string(&prompt_path).with_context(|| {
        format!(
            "step `{}` needs prompt {} — run `spoolway init`",
            step.id,
            prompt_path.display()
        )
    })?;

    // What the agent is actually handed: the project's prompt wrapped in the
    // framing and the policy it cannot know. Written per lane rather than
    // pointed at the project's own file, so that spoolway's half of the system
    // prompt is unskippable — see [`system_prompt`].
    let system_prompt_text = crate::compose::system_prompt(repo, task, pipeline, step, &prompt)?;
    let prompt_file = write_system_prompt(repo, &name, &system_prompt_text)?;

    let model = resolve_model(step);
    // spoolway names no model of its own, so a step nobody has given one
    // reaches here with nothing to run. Saying so is far better than starting
    // an agent with an empty `--model` and reading its output to find out why
    // it failed. `pipeline check` and `doctor` catch this before a lane ever
    // starts; this is the backstop for a pipeline neither has seen.
    if model.trim().is_empty() {
        anyhow::bail!(
            "step `{}` has no model: give it one with `model:` in pipeline.yml",
            step.id
        );
    }

    // Minted here rather than derived from the task and step, because a step
    // can be retried and each attempt is its own conversation: a deterministic
    // name would make the second attempt reopen the first one's transcript and
    // bill its tokens twice.
    //
    // A task coming back from `blocked` to the step it stopped on is the one
    // exception, and it is not a retry — it is the same attempt, continued. Its
    // lane had read the task, walked the tree and often finished the work
    // before something outside it got in the way, and a fresh session pays for
    // all of that again to arrive back where that one already was. So the
    // session is looked up rather than minted, and the argv is rewritten to
    // continue it. `record_usage` banks only what arrives after this point, so
    // the transcript is still counted once.
    //
    // Two reasons a lane may continue an earlier conversation instead of
    // opening fresh: the one-shot `resume:`, set by the two places that send
    // a task back to where it stopped, and the step's own standing
    // `session:` — its prompt already holds one on this task, still under
    // the declared size. Only for a kind whose resume flag was established
    // against the real binary, either way.
    let adapter = crate::agent::adapter(&profile.kind);
    let resuming = task.front.resume.as_deref() == Some(step.id.as_str());
    // A `p` park sets `resume` exactly the way a real block does, so this
    // launch continues the same lane either way — but it is not the same
    // *fact* to hand the lane back: nothing stopped it, a person's own
    // keypress did. `parked_from` is what survives to tell the two apart
    // here, still set at this point because `unpark` deliberately leaves it
    // for this launch to spend — see the note beside it.
    let parked = task.front.parked_from.as_deref() == Some(step.id.as_str());
    let one_shot = resuming
        .then(|| lane_session_in(repo, ledger, &name))
        .flatten();
    // `session:` is a different question from the one-shot flag, not a
    // fallback for it — a task coming back from `blocked` names an exact
    // session to continue, and a miss there says that lane's session is
    // gone, not that any prompt match will do instead. So this is only
    // tried when `resuming` is false to begin with.
    let (carried, session_miss) = match (resuming, step.session) {
        (false, true) => match carried_session(repo, pipeline, task, step, profile, &model, ledger)
        {
            Ok(found) => (Some(found), None),
            Err(reason) => (None, Some(reason.describe(step.prompt_name(), &model))),
        },
        _ => (None, None),
    };
    // Whether this resume, if any, comes from the prompt carrying over
    // rather than the one-shot flag — decides which prompt a resumed lane
    // reads, so it is settled before either is consumed below.
    let via_session = !resuming && carried.is_some();
    let previous = one_shot
        .or(carried)
        .filter(|(kind, _)| *kind == profile.kind)
        .filter(|_| adapter.is_some_and(|adapter| adapter.resumes()));
    let session = match &previous {
        Some((_, session)) => session.clone(),
        None => crate::usage::new_session_id(),
    };

    // A lane's own files sit under `worktree`, but a linked worktree writes
    // the objects `git add` creates and moves the branch ref `git commit`
    // advances in the main checkout's shared `.git` — read-only to a lane
    // confined to `worktree`, so both failed with `Read-only file system`
    // before this grant existed. Resolved from inside `worktree` rather than
    // assembled from `dispatch.worktree_root` and the branch, so a borrowed
    // checkout (whose git directory is already the repo's own `.git`) gets
    // the same directory it already reads and writes, not a guess at one —
    // see `crate::repo::git_dir`.
    let git_dir = crate::repo::git_dir(&worktree)?;

    let values: BTreeMap<&str, String> = BTreeMap::from([
        ("model", model.clone()),
        ("session_id", session.clone()),
        ("prompt_file", prompt_file.display().to_string()),
        ("task_file", task.path.display().to_string()),
        ("worktree", worktree.display().to_string()),
        ("repo", repo.root.display().to_string()),
        (
            "state_dir",
            repo.root
                .join(crate::config::STATE_DIR)
                .display()
                .to_string(),
        ),
        // The task file moved out of `state_dir` along with the rest of a
        // project's runtime state — see `crate::repo::Repo::home` — so a
        // lane's own `--add-dir` grant needs this too, or a kind confined to
        // what it names can no longer read the file it is working from.
        ("project_home", repo.home().display().to_string()),
        ("git_dir", git_dir.display().to_string()),
    ]);
    let mut args = profile.render_args(&values)?;
    args.extend(profile.effort_args(step.effort.as_deref()));
    // Rendered first and rewritten after, so a resumed lane and a fresh one are
    // launched by the same code with the same flags — only the one that names
    // the session differs.
    let args = match previous.is_some() {
        true => adapter
            .and_then(|adapter| adapter.resume_args(&args))
            .unwrap_or(args),
        false => args,
    };

    // What a lane's environment carries. `agents.<profile>.env` is retired —
    // every agent it fronted has a config file of its own for the same job —
    // so the only layer left is, for a kind that mints its own session id and
    // will not take one, the home that pins its session instead. Made here
    // rather than at first read, because the agent is about to be told to
    // use it. An empty vec for every other kind, which is why this needs no
    // branch on `kind`.
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    if let Some(adapter) = adapter {
        crate::agent::prepare_session_home(&profile.kind, &session, &worktree);
        env.extend(adapter.session_env(&session));
    }
    env.insert(crate::commands::TASK_ENV.to_string(), task.front.id.clone());
    env.insert("SPOOLWAY_REPO".to_string(), repo.root.display().to_string());
    env.insert(ENV_STEP.to_string(), step.id.clone());
    env.insert(ENV_SESSION.to_string(), session.clone());
    env.insert(
        "SPOOLWAY_WORKTREE".to_string(),
        worktree.display().to_string(),
    );
    // Where the branch stood before this lane touched it. `spoolway report` uses
    // it to tell a lane that committed its own work from one that committed
    // nothing, which decides whether the leftovers are work to rescue or residue
    // to leave alone. Kept on the lane record too, for the lane that dies
    // without ever reaching `spoolway report`.
    let head = crate::repo::run(&worktree, "git", &["rev-parse", "HEAD"])
        .map(|head| head.trim().to_string())
        .unwrap_or_default();
    if !head.is_empty() {
        env.insert("SPOOLWAY_HEAD".to_string(), head.clone());
    }
    env.insert(
        "SPOOLWAY_TASK_FILE".to_string(),
        task.path.display().to_string(),
    );

    // A scratch directory outside the worktree, for the short-lived worktrees
    // a rebase needs. Every lane gets one: it is named after this task,
    // reclaimed when the task is archived, and reachable by nothing else — so
    // there was never a decision here to gate, only a directory to make.
    let scratch = repo.scratch_dir().join(&task.front.id);
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
    env.insert(
        "SPOOLWAY_SCRATCH".to_string(),
        scratch.display().to_string(),
    );

    // The pane this lane runs in: the one the step before it handed over, or a
    // fresh split off the task's tab when there is none.
    //
    // A handed-over pane has already been checked, not assumed — it is only
    // offered here after the multiplexer said the session left and the pane
    // came back to its shell, which is the one thing that must be true before
    // anything is typed into it. Everything else still splits: the first step
    // of a task has nothing to inherit, and a kind with no way to leave its
    // pane closes it as it always did.
    //
    // Split last, after everything that can refuse this start has been checked,
    // and taken back if the launch itself fails — a pane left behind by every
    // failed attempt would fill the tab over a few dispatch passes.
    // A tab id this multiplexer has never heard of is the other half of a task
    // file that came from another machine: the worktree path happened to exist
    // here, so it was kept, but the ids beside it are somebody else's. Let go of
    // the placement rather than failing forever — the next pass cuts a workspace
    // of its own and the task carries on.
    let label = tab_label(task.id(), &step.id);
    let pane_id = match inherited {
        Some(pane_id) => pane_id,
        None => match mux.split_pane(&tab, &worktree) {
            Ok(pane_id) => pane_id,
            Err(err) => {
                task.front.worktree_path = None;
                task.front.workspace_id = None;
                task.front.pane_id = None;
                task.front.tab_id = None;
                persist_task(repo, task, report_seen)?;
                return Err(err.context(format!(
                    "tab `{tab}` is not in this multiplexer; the task's placement has \
                     been cleared and the next pass will cut it a workspace of its own"
                )));
            }
        },
    };
    let launched = mux.start_lane(&LaneSpec {
        name: &name,
        // The lane's own name, on its own pane — a grid of panes is only
        // readable if each says what it is.
        label: &label,
        kind: &profile.kind,
        pane_id: &pane_id,
        args: &args,
        env: &env,
        path_prefix: None,
    });
    if let Err(err) = launched {
        // Taking a pane back rather than leaving it behind means the same
        // step retries into a fresh one instead of the tab filling up over a
        // few dispatch passes. Under `split`, on the very first step of a
        // task, this pane is the *only* thing in its workspace — the one
        // `Mux::create_workspace` opened — so closing it takes the tab and
        // the workspace with it, exactly what `Mux::close_pane`'s own doc
        // warns against. That is meant here, not a bug: the next pass's
        // `Mux::workspace_alive` check reads the now-gone workspace as an
        // ordinary stale placement and reopens one, the same recovery a
        // worktree deleted by hand gets.
        let _ = mux.close_pane(&pane_id);
        return Err(err);
    }

    // Only record a transition when this is genuinely a new step. A retry of
    // the same step arrived by the route that is already recorded.
    if task.stage() != step.id {
        task.set_stage(&step.id, None);
    }
    // Spent, whether or not a session was found to continue: a resume that
    // could not find one has still had its go, and leaving the flag set would
    // make the next ordinary retry of this step reopen a conversation that a
    // retry is meant to start again.
    if resuming {
        task.front.resume = None;
    }
    // Spent the same way, and for the same reason: whether or not this park
    // found a session to carry, its one launch has now happened, and leaving
    // the flag set would make the next ordinary retry of this step read as
    // one more park nobody asked for.
    if parked {
        task.front.parked_from = None;
    }
    // The lane has actually started now — `mux.start_lane` above returned
    // `Ok`, and a failure there took the early return. This is the one true
    // exit for a task that launches from the step it was parked on, so the
    // whole quota or usage-limit hold comes off here — all three park
    // fields, and the `quota_retries` streak with them — and rides out on
    // `persist_task` below with everything else this launch changed.
    //
    // Deliberately after the start, not before: `Dispatcher::parked` leaves
    // an expired park's fields set, and `ensure_workspace`'s own bookkeeping
    // saves earlier in this function carry them through unchanged rather
    // than publishing a clear. A start that never happened is not an exit —
    // the task stays parked, keeps its retry count, and the next pass
    // decides again. `set_stage` above clears the same fields, but only on a
    // real step change, which a task parked on its current step does not
    // make.
    task.front.parked_at = None;
    task.front.parked_until = None;
    task.front.parked_window = String::new();
    task.front.quota_retries = 0;
    // `prompts` is banked here, unconditionally — a prompt is a launch, so a
    // retry banks a second one, it was a second prompt and it was paid for —
    // except for a park whose session was actually carried: that lane never
    // stopped being the one already counted, so continuing it costs nothing
    // new. A park that opened fresh instead (`previous` came back empty) is
    // a second prompt like any other and is banked below like any other.
    // `rounds` is not banked here at all any more — that is `set_stage`
    // above's, once per arrival, whatever a lane here goes on to do.
    //
    // Never before the `set_stage` above, which is what writes the route this
    // is banked against.
    // `set_stage` above writes `arrived_from` for every task the dispatcher
    // moves, including the first move off `queued` — so the fallback is only
    // ever reached by a task file that was written already sitting on the step
    // its first lane runs, and `queued` is where such a task came from.
    if !(parked && previous.is_some()) {
        let from = task
            .front
            .arrived_from
            .clone()
            .unwrap_or_else(|| crate::pipeline::QUEUED.to_string());
        task.bank_launch(&from, &step.id);
    }
    // Counted here, and never before the `set_stage` above: that call zeroes the
    // counter, so a start banked first would be wiped by the very transition it
    // belongs to and the budget would never bite. Arriving at a step is what
    // resets it; starting a lane there is what spends it.
    task.front.attempts += 1;
    task.front.launched_at = Some(now_secs());
    // Why a `session:` step opened fresh, written down rather than only said.
    //
    // It is also handed back as the pass's own note, and that is where it used
    // to end: under `--plain` a log line, and under the board — which is how a
    // person actually runs the dispatcher — a line in `RECENT` that scrolls away
    // within a pass or two. So the question `session_reuse_ctx` exists to be
    // asked about, *why did my expensive review conversation not get reused*,
    // had no answer available afterwards at all. One line per fresh session, in
    // the place somebody auditing a task already looks.
    if let Some(note) = &session_miss {
        task.append_to_section("## Status Log", &format!("- `{}`: {note}\n", step.id));
    }
    persist_task(repo, task, report_seen)?;

    // `parked` takes the match before `via_session` gets a say: `resuming` is
    // always true for a park (see the note beside it above), so without this
    // a continued park would read `via_session == false` and fall into
    // `resume_prompt` — the one prompt that must never reach a lane nothing
    // ever blocked.
    let prompt = match (previous.is_some(), via_session, parked) {
        (true, _, true) => crate::compose::park_prompt(repo, task, pipeline),
        (true, true, false) => crate::compose::carry_prompt(repo, task, pipeline),
        (true, false, false) => {
            crate::compose::resume_prompt(repo, task, pipeline, repo.unattended())
        }
        (false, _, _) => crate::compose::opening_prompt(repo, task, pipeline, step),
    };
    // Take the lane back when its briefing does not land, exactly as the
    // `start_lane` failure above takes its pane back — and for a sharper
    // reason. A lane that was started but never prompted is a live session
    // sitting at an empty input box, and every check the dispatcher makes
    // afterwards reads it as a lane hard at work: it is in `herdr agent
    // list`, so no pass re-staffs the step, and the board draws the task as
    // `running`. Nothing corrects that until `dispatch.lane_quiet` runs out
    // — fifteen minutes, shipped — and what arrives then is three reminders
    // to report, sent to a session that was never told what to do, followed
    // by an escalation to `blocked`. The better part of an hour, and then a
    // stop for a person, over a prompt the very next pass would have
    // delivered.
    //
    // Torn down here instead, so the step is simply unstaffed again. The
    // task keeps the `attempts` and `launched_at` written just above, which
    // is what paces the retry through `relaunch_backoff` and stops it at
    // `MAX_LAUNCHES` — a prompt that fails every time still ends up in front
    // of a person, just not an hour late and not disguised as a lane that
    // was working.
    if let Err(err) = mux.prompt(&name, &prompt) {
        let _ = mux.stop_lane(&name, &pane_id);
        return Err(err);
    }
    Ok(Started {
        name,
        session,
        model,
        head,
        // Only worth a word when `session:` tried and missed — an ordinary
        // step, and a hit either way, need nothing said about it.
        note: session_miss,
    })
}

/// What a successful lane start hands back: its name, and what the ledger needs
/// to find its transcript afterwards.
struct Started {
    name: String,
    session: String,
    model: String,
    /// Where the lane's branch stood when it started, so that a lane torn down
    /// without ever reporting can still be told apart from one that committed
    /// its own work. Empty when the worktree could not be read.
    head: String,
    /// Why a `session:` step opened fresh instead of carrying its prompt's
    /// conversation over — `None` on an ordinary step, and on a hit either
    /// way, because neither is worth a person's attention.
    note: Option<String>,
}

/// A step's model: exactly what it names, and nothing else.
///
/// Nothing here reads how many times the task has been through this step, or
/// falls back to anything the agent profile carries: a step's model is a
/// property of the pipeline, so reading the file tells you what will run.
fn resolve_model(step: &Step) -> String {
    step.model.clone().unwrap_or_default()
}

/// Where the composed system prompt of each lane is written.
///
/// Under the state directory rather than the worktree: a lane may not write
/// here, and the file is machine state — regenerated before every launch,
/// ignored by git, gone with the checkout. Named for the lane so two lanes of
/// one task never share a file.
pub const SYSTEM_PROMPTS_DIR: &str = "system-prompts";

/// Write a lane's composed system prompt and return the path the agent is
/// pointed at.
///
/// Overwritten on every launch rather than accumulated: a step can be retried,
/// and the second attempt's policy is not the first's — a stale file would hand
/// a fix pass the findings paragraph of the round before it. It survives the
/// lane on purpose, though, because `spoolway lane --attach` reopens a
/// session and the agent may re-read what it was started with.
fn write_system_prompt(repo: &Repo, lane: &str, body: &str) -> Result<PathBuf> {
    let path = repo.system_prompts_dir().join(format!("{lane}.md"));
    crate::task::write_atomic(&path, body)?;
    Ok(path)
}

/// The agent kind and session id a lane was started with, for `spoolway
/// attach` to reopen it. From the lane records first — the dispatcher's own
/// bookkeeping — and the usage ledger as the fallback for a lane whose record
/// a later pass has already retired.
///
/// One-shot callers (the board, `spoolway lane`) read the ledger here;
/// [`start_one`], inside a pass, passes the pass's one snapshot to
/// [`lane_session_in`] so the ledger is not parsed again per resume (review
/// finding 33).
pub fn lane_session(repo: &Repo, lane: &str) -> Option<(String, String)> {
    lane_session_in(repo, &crate::usage::read(repo).unwrap_or_default(), lane)
}

fn lane_session_in(
    repo: &Repo,
    ledger: &[crate::usage::Entry],
    lane: &str,
) -> Option<(String, String)> {
    if let Some(record) = load_lane_records(repo).get(lane)
        && !record.session.is_empty()
        && !record.kind.is_empty()
    {
        return Some((record.kind.clone(), record.session.clone()));
    }

    // Composed with `lane_name` rather than matched by hand, so a step id or
    // a task id holding the separator still names the same lane both here
    // and everywhere else one is built.
    ledger
        .iter()
        .rev()
        .find(|entry| lane_name(&entry.step, &entry.task) == lane && !entry.session.is_empty())
        .map(|entry| (entry.kind.clone(), entry.session.clone()))
}

/// Why a `session:` step did not carry its prompt's conversation over, said
/// in as many words for the pass that fell through to a fresh one instead.
#[derive(Debug, PartialEq, Eq)]
enum SessionMiss {
    /// No earlier ledger entry for this task resolves to this prompt, or one
    /// did but its transcript couldn't be read to size it — missing, rotated
    /// away, or holding no assistant turn.
    NotFound,
    /// Found, but its last turn is past `session_reuse_ctx`.
    OverSize,
    /// Found with a size ceiling enabled, but this model's window is unset,
    /// so the session cannot be measured against that ceiling.
    WindowUnset,
    /// Found and under size, but its store has sat longer than the model's
    /// `session_reuse_idle`.
    Stale,
}

impl SessionMiss {
    fn describe(&self, prompt: &str, model: &str) -> String {
        match self {
            SessionMiss::NotFound => {
                format!("no readable earlier `{prompt}` session on this task — opened fresh")
            }
            SessionMiss::OverSize => {
                "its earlier session is over the declared size — opened fresh".to_string()
            }
            SessionMiss::WindowUnset => {
                format!("`{model}`'s context window is not set — opened fresh")
            }
            SessionMiss::Stale => {
                format!(
                    "its earlier session's store has sat past `{model}`'s session_reuse_idle — \
                     opened fresh"
                )
            }
        }
    }
}

/// The session `step`'s prompt already holds on this task, if one is both
/// findable and still worth resuming under `profile`'s two bounds.
///
/// Identity comes from the usage ledger rather than any state of its own: the
/// newest entry for this task whose step resolves, in `pipeline`, to the same
/// prompt this step runs — the same correlation [`lane_session`] already
/// falls back to, so a repeated visit under the same prompt is simply a
/// repeated finding.
///
/// Size comes from the transcript that entry names, because a ledger entry is
/// a sum and a sum is not a context — see [`crate::usage::last_turn`] —
/// and is checked when the profile enables a ceiling. A zero
/// `session_reuse_ctx` leaves reuse unbounded by percentage; `session:` on the
/// step still says whether to look at all.
/// `session_reuse_ctx` is `profile`'s, not the step's, per that split.
///
/// Age is checked after size, and only refuses a session that is otherwise
/// carried: a session whose store's `touched_at` cannot be read, or whose
/// model sets no `session_reuse_idle` at all, refuses nothing — there is no
/// per-profile override left, only the model's own horizon.
fn carried_session(
    repo: &Repo,
    pipeline: &Pipeline,
    task: &Task,
    step: &Step,
    profile: &AgentProfile,
    model: &str,
    ledger: &[crate::usage::Entry],
) -> std::result::Result<(String, String), SessionMiss> {
    let prompt = step.prompt_name();
    let entry = ledger
        .iter()
        .rev()
        .find(|entry| {
            entry.task == task.front.id
                && !entry.session.is_empty()
                && pipeline
                    .step(&entry.step)
                    .is_some_and(|s| s.prompt_name() == prompt)
        })
        .ok_or(SessionMiss::NotFound)?;

    let price = crate::models::resolve(&repo.config.models, model).price;
    if profile.session_reuse_ctx != 0 {
        let window = price
            .map(|price| price.context_window)
            .filter(|window| *window > 0)
            .ok_or(SessionMiss::WindowUnset)?;
        let size =
            crate::usage::last_turn(&entry.kind, &entry.session).ok_or(SessionMiss::NotFound)?;
        if exceeds_percent(window, profile.session_reuse_ctx, size) {
            return Err(SessionMiss::OverSize);
        }
    }

    if let Some(idle) = price.and_then(|price| price.session_reuse_idle) {
        let stale = crate::usage::session_path(&entry.kind, &entry.session)
            .and_then(|path| crate::usage::touched_at(&path))
            .and_then(|touched| std::time::SystemTime::now().duration_since(touched).ok())
            .is_some_and(|age| age > idle);
        if stale {
            return Err(SessionMiss::Stale);
        }
    }

    Ok((entry.kind.clone(), entry.session.clone()))
}

/// Whether `size` tokens is past `pct`% of a `window`-token model — the
/// arithmetic behind [`SessionMiss::OverSize`], pulled out on its own because
/// it is the one part of [`carried_session`] that needs no ledger, no
/// transcript and no repo to be worth checking.
fn exceeds_percent(window: usize, pct: u8, size: u64) -> bool {
    size > (window as u64) * (pct as u64) / 100
}

/// Where this project's lanes work: the checkout itself, and the worktree each
/// task recorded when its workspace was cut.
///
/// A multiplexer is shared by every project on the machine — and by the person
/// using it — so a name alone does not say whose a lane is. Lane names are
/// `<task> · <step>`, and a person's own session in a worktree cut from a
/// branch called `fix/herdr-layout` is named after the branch, with no
/// separator a task or step id could ever hold — it neither parses as one nor
/// is meant to. Acting on such a session means tearing down a pane somebody is
/// working in; counting it means reporting a cap fuller than it is.
///
/// Shared by the pass and by the board's footer, because the two answering
/// differently is the same bug twice: the board said `3/5` while the dispatcher
/// had one lane in flight and four slots free.
pub fn our_checkouts(repo: &Repo, tasks: &[Task]) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    for path in std::iter::once(repo.root.clone())
        .chain(tasks.iter().filter_map(|t| t.front.worktree_path.clone()))
    {
        // Both spellings: the path as recorded, and its canonical form. A
        // backend that resolves symlinks when it reports a lane's `cwd`
        // (herdr, or tmux's `pane_current_path`) hands back the same
        // directory under a different name, and [`owns_cwd`] checks against
        // whichever this set happens to hold.
        if let Ok(canon) = std::fs::canonicalize(&path) {
            out.insert(canon);
        }
        out.insert(path);
    }
    out
}

/// Whether `cwd` — a lane's own working directory, as the multiplexer
/// reported it — is one of `mine`.
///
/// A plain set membership first, then the same test on the canonicalised
/// path. tmux stamps [`crate::tmux`]'s `OPT_CWD` with the exact string the
/// dispatcher recorded, so the first test is normally enough; the fallback
/// is for a backend that canonicalises, where byte-equality alone dropped
/// every lane and escalated every task. See review finding 38.
pub fn owns_cwd(mine: &HashSet<PathBuf>, cwd: &std::path::Path) -> bool {
    mine.contains(cwd)
        || std::fs::canonicalize(cwd)
            .map(|canon| mine.contains(&canon))
            .unwrap_or(false)
}

/// Whether `task` cannot move without a person: parked on `paused`, or on an
/// unstaffed `blocked`. The one question [`Dispatcher::sweep_on_stop`] and
/// [`Dispatcher::free_finished_lanes`] both need answered the same way, so a
/// free function rather than a method on [`Dispatcher`] alone.
///
/// `paused` always is: the whole stage exists to wait for `spoolway
/// release` and nothing else ever moves a task off it. `blocked` only
/// counts when nobody is coming to look — see
/// [`crate::pipeline::Pipeline::blocked_is_staffed`] — because a staffed
/// `blocked` lane is answered by another lane, not by a person, and settles
/// like any other step.
fn parked_for_a_person(pipelines: &Pipelines, unattended: bool, task: &Task) -> bool {
    match task.stage() {
        crate::pipeline::PAUSED => true,
        crate::pipeline::BLOCKED => pipelines
            .for_task(task)
            .is_ok_and(|p| !p.blocked_is_staffed(unattended)),
        _ => false,
    }
}

/// Whether the gate ranks `task` behind a still-open group, under
/// `dispatch.priority = "group"` — its own group has never run.
///
/// `false` covers every case the gate does not apply: `dispatch.priority` is
/// `"any"`, `task` carries no `group:` — an ungrouped task has no siblings
/// holding up a pull request, so it is never ranked behind one — or its own
/// group is already open.
///
/// The gate only ever ranks now, as the first tier of the candidate sort in
/// [`Dispatcher::pass`]; it drops nothing. That is why a parked-for-a-person
/// open group needs no special case here any more, unlike the version of
/// this function that used to decide whether a candidate survived the pass
/// at all: a parked group contributes no candidates of its own, so under
/// ranking its being open never changes what anything else sorts against.
pub(crate) fn group_gate_holds(
    priority: crate::config::Priority,
    graph: &Graph,
    task: &Task,
) -> bool {
    if priority != crate::config::Priority::Group {
        return false;
    }
    let Some(group) = task.front.group.as_deref() else {
        return false;
    };
    !graph.group_is_open(group)
}

/// Lanes whose pane is holding a question for a person, by lane name.
///
/// Read straight out of the bookkeeping the dispatcher already dedups its
/// notification with, so the board and the notification that woke you say
/// the same thing without the two having to agree on anything else. A dispatcher
/// that is not running leaves the last pass's answer behind, which is exactly
/// what it was: those panes are still open and still waiting.
pub fn lanes_awaiting_a_person(repo: &Repo) -> BTreeSet<String> {
    load_lane_records(repo)
        .into_iter()
        .filter(|(_, record)| record.notified)
        .map(|(name, _)| name)
        .collect()
}

fn lanes_path(repo: &Repo) -> PathBuf {
    repo.lanes_file()
}

/// The session id of every lane a dispatcher still owns, out of `lanes.json`.
///
/// [`crate::usage::sweep`] reads this to leave a live lane alone. A lane still
/// in flight is the dispatcher's to bank at teardown, and its spend is diffed
/// against the ledger snapshot [`Dispatcher::record_usage`] took once at the
/// start of the pass — so a catch-up line appended behind its back is banked a
/// second time when that snapshot is diffed. An empty session is dropped: a
/// lane from a spoolway that predates the ledger, or a project whose `args`
/// never passed `{session_id}` through, names no session to exclude.
pub(crate) fn live_lane_sessions(repo: &Repo) -> HashSet<String> {
    load_lane_records(repo)
        .into_values()
        .map(|record| record.session)
        .filter(|session| !session.is_empty())
        .collect()
}

pub(crate) fn load_lane_records(repo: &Repo) -> HashMap<String, LaneRecord> {
    let path = lanes_path(repo);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // Absent or unreadable is the ordinary empty state — no dispatcher
        // has run for this project yet, or none since it was cleaned.
        Err(_) => return HashMap::new(),
    };
    match serde_json::from_str(&raw) {
        Ok(records) => records,
        Err(err) => {
            // A corrupt `lanes.json` — one bad byte from a hand edit or a
            // disk error — used to map to an empty map that the pass then
            // saved back over, dropping every lane's session id, reminder
            // count, `held_for_block` and `retired_pane` at once with no
            // message. Keep the bytes as a `.bad` copy and say so in the
            // problem log before the pass overwrites the file. See review
            // finding 31.
            let bad = path.with_extension("json.bad");
            let kept = std::fs::write(&bad, &raw).is_ok();
            crate::problem_log::append(
                repo,
                &format!(
                    "lanes.json did not parse ({err}) — {}; starting this pass from no lane records",
                    match kept {
                        true => format!("kept a copy at {}", bad.display()),
                        false => "could not keep a copy".to_string(),
                    }
                ),
            );
            HashMap::new()
        }
    }
}

pub(crate) fn save_lane_records(repo: &Repo, lanes: &HashMap<String, LaneRecord>) -> Result<()> {
    let rendered = serde_json::to_string_pretty(lanes)?;
    crate::task::write_atomic(&lanes_path(repo), &rendered)
}

/// The usage ledger folded into a running total per session — what
/// [`Dispatcher::record_usage`] used to recompute from scratch, by re-reading
/// the whole file, for every lane a pass banked. Reads nothing itself now: the
/// pass's one [`Dispatcher::ledger`] snapshot is folded here.
fn banked_totals(ledger: &[crate::usage::Entry]) -> HashMap<String, BankedTotals> {
    let mut totals: HashMap<String, BankedTotals> = HashMap::new();
    for entry in ledger {
        if entry.session.is_empty() {
            continue;
        }
        let banked = totals.entry(entry.session.clone()).or_default();
        banked.tokens.add(&entry.tokens);
        banked.turns += entry.turns;
        banked.cost_usd += entry.cost_usd.unwrap_or(0.0);
    }
    totals
}

pub(crate) fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn hash_of(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mux::{LaneStatus, Workspace};
    use crate::task::Frontmatter;
    use std::cell::RefCell;
    use std::path::Path;

    /// Records what the dispatcher asked of the multiplexer, and answers from a
    /// scripted lane list.
    struct FakeMux {
        lanes: Vec<Lane>,
        calls: RefCell<Vec<String>>,
        /// A tab the multiplexer will refuse to close, being its workspace's
        /// only one.
        last_tab: Option<String>,
        /// Hands each split a distinct pane id, the way a real one does.
        splits: RefCell<usize>,
        /// Make `agent start` refuse, the way a multiplexer does when the
        /// agent binary is missing.
        refuse_start: bool,
        /// Make the briefing refuse to land, the way a real backend does when
        /// a freshly started session never begins its turn — the lane is up,
        /// and `Mux::prompt` still comes back an error.
        refuse_prompt: bool,
        /// Whether a waiting lane keeps a process alive, as a multiplexer's
        /// does and a headless lane's does not.
        resident: bool,
        /// The dispatch workspace this backend hands out, and how many times it
        /// was asked to. A backend with no workspaces leaves it `None`, which
        /// is what headless does.
        workspace: Option<String>,
        workspace_calls: RefCell<usize>,
        /// Whether a task's recorded workspace is the task's own. False is a
        /// herdr run laid out as `workspace`, where every task is a tab of the
        /// one workspace the run opened.
        task_owns_workspace: bool,
        /// The tabs `open_tab` has opened, by label — what `find_tab` answers
        /// from, exactly as a real backend answers from the multiplexer's own
        /// listing rather than from anything the queue recorded.
        tabs: RefCell<HashMap<String, Workspace>>,
        /// What `read` answers for a lane, mutated by `prompt` the way a real
        /// pane's screen is: typing a message into it changes what is on it.
        /// Absent for a lane nothing has prompted, which is most of them —
        /// `read` falls back to empty exactly as it always did.
        screen: RefCell<HashMap<String, String>>,
        /// Whether a session asked to leave its pane refuses to go — the one
        /// failure [`Vacated::StillOccupied`] exists for. False is the
        /// ordinary case, where a kind with a gesture leaves and the pane
        /// comes back to its shell.
        stubborn: bool,
        /// Workspace and tab ids this backend has "forgotten" — what a real
        /// multiplexer answers once it has restarted out from under a
        /// recorded id. Empty by default: a fake that remembers everything is
        /// what every other test wants, and `workspace_alive` answers `true`
        /// for anything not named here.
        forgotten: RefCell<HashSet<String>>,
        /// Whether `remove_workspace` refuses, the way herdr refuses a
        /// workspace it is no longer holding a worktree against —
        /// `not_linked_worktree`. False is the ordinary case, where the one
        /// call takes the checkout and the row together.
        unbound_workspace: bool,
        /// Whether this backend actually offers a pane to
        /// [`crate::mux::Mux::run_in_pane`], the way herdr and tmux do. False
        /// is every other test's backend, headless included: `run_in_pane`
        /// answers `None`, exactly the trait's own default, and a command
        /// step falls back to the detached run it always used.
        run_commands_in_pane: bool,
        /// Lane names a caller has marked as still holding a process they
        /// started — a backend able to see a child of the lane's still
        /// running in the process table, independent of whatever
        /// [`LaneStatus`] its screen reads. Every other lane gets nothing to
        /// say here, the same as `Herdr` today.
        busy_children: RefCell<HashSet<String>>,
    }

    impl FakeMux {
        fn new(lanes: Vec<Lane>) -> FakeMux {
            FakeMux {
                lanes,
                calls: RefCell::new(Vec::new()),
                last_tab: None,
                splits: RefCell::new(0),
                refuse_start: false,
                refuse_prompt: false,
                resident: true,
                workspace: Some("wD".into()),
                workspace_calls: RefCell::new(0),
                task_owns_workspace: true,
                tabs: RefCell::new(HashMap::new()),
                screen: RefCell::new(HashMap::new()),
                stubborn: false,
                forgotten: RefCell::new(HashSet::new()),
                unbound_workspace: false,
                run_commands_in_pane: false,
                busy_children: RefCell::new(HashSet::new()),
            }
        }
        /// Mark `name` as still holding a process it started, from now until
        /// the test says otherwise.
        fn with_busy_child(&self, name: &str) {
            self.busy_children.borrow_mut().insert(name.to_string());
        }
        /// A backend whose agents are asked to leave their pane and stay put —
        /// the modal sitting there with nobody to answer it.
        fn refusing_to_leave(mut self) -> FakeMux {
            self.stubborn = true;
            self
        }
        /// A backend that puts every task in a tab of the run's one workspace,
        /// which is what `herdr_mode = "workspace"` is.
        fn tabs_in_one_workspace(mut self) -> FakeMux {
            self.task_owns_workspace = false;
            self
        }
        /// A backend with no notion of a workspace to cut against.
        fn without_workspaces(mut self) -> FakeMux {
            self.workspace = None;
            self
        }
        /// A backend whose lanes do not survive between turns — what
        /// `backend = "headless"` is.
        fn detached(mut self) -> FakeMux {
            self.resident = false;
            self
        }
        /// A backend whose row has lost its grip on the checkout under it, so
        /// the one call that was meant to take both removes neither.
        fn with_unbound_workspace(mut self) -> FakeMux {
            self.unbound_workspace = true;
            self
        }
        /// A backend that actually offers a command step a pane — herdr or
        /// tmux, rather than every other test's fake, which declines exactly
        /// as headless does.
        fn offering_panes(mut self) -> FakeMux {
            self.run_commands_in_pane = true;
            self
        }
        fn with_last_tab(mut self, tab: &str) -> FakeMux {
            self.last_tab = Some(tab.to_string());
            self
        }
        fn refusing_to_start(mut self) -> FakeMux {
            self.refuse_start = true;
            self
        }
        /// A backend that starts a lane and then will not carry its briefing
        /// into it — herdr's `agent prompt` stalling on a session that is up
        /// but not yet reading its input.
        fn refusing_to_prompt(mut self) -> FakeMux {
            self.refuse_prompt = true;
            self
        }
        /// A multiplexer that has forgotten `id` — a workspace or tab a
        /// restart wiped out from under a worktree that survived it. See
        /// `workspace_alive`.
        fn forgetting(self, id: &str) -> FakeMux {
            self.forgotten.borrow_mut().insert(id.to_string());
            self
        }
        fn log(&self, call: String) {
            self.calls.borrow_mut().push(call);
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
        fn did(&self, prefix: &str) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|c| c.starts_with(prefix))
                .collect()
        }
        /// Forgets what has been called so far, without forgetting `screen` —
        /// for a test reusing one mux across several simulated passes (the
        /// pane has to persist; the log of one pass's calls should not bleed
        /// into the next one's assertions).
        fn clear_calls(&self) {
            self.calls.borrow_mut().clear();
        }
    }

    impl Mux for FakeMux {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable(&self) -> String {
            "the fake multiplexer is always available".into()
        }
        /// Stands in for a multiplexer, so it answers like one. The headless
        /// backend's `false` is exercised through `FakeMux::detached` below.
        fn resident_while_waiting(&self) -> bool {
            self.resident
        }
        fn list_lanes(&self) -> Result<Vec<Lane>> {
            Ok(self.lanes.clone())
        }
        /// Answers `Some(true)` for a lane a test has marked with
        /// `with_busy_child`, the same sturdier-than-the-screen signal a
        /// backend able to read the process table gives. Everything else
        /// falls through to the trait's own default, `None` — this fake has
        /// nothing to say either, same as `Herdr` today.
        fn lane_process_alive(&self, name: &str) -> Option<bool> {
            self.busy_children.borrow().contains(name).then_some(true)
        }
        fn workspace_alive(&self, workspace_id: &str, tab_id: Option<&str>) -> Result<bool> {
            // Never `workspace ...`: `did("workspace")` is how other tests
            // assert that nothing renamed or otherwise touched a task's
            // workspace, and a verification call logged under that prefix
            // would fail them for asking a question, not changing anything.
            self.log(format!("verify_workspace {workspace_id}"));
            let forgotten = self.forgotten.borrow();
            if forgotten.contains(workspace_id) {
                return Ok(false);
            }
            if let Some(tab_id) = tab_id
                && forgotten.contains(tab_id)
            {
                return Ok(false);
            }
            Ok(true)
        }
        fn dispatch_workspace(&self, _root: &Path, create: bool) -> Result<Option<String>> {
            *self.workspace_calls.borrow_mut() += 1;
            self.log(match create {
                true => "dispatch_workspace find-or-create".to_string(),
                false => "dispatch_workspace find-only".to_string(),
            });
            Ok(self.workspace.clone())
        }

        /// A tab per label, the way a real backend has one: the first is
        /// `w9:t1`, and a second label gets `w9:t2` beside it rather than the
        /// same tab again. Remembered so `find_tab` can answer with it.
        fn open_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<Workspace> {
            self.log(format!("open_tab {workspace_id} {label}"));
            let mut tabs = self.tabs.borrow_mut();
            let n = tabs.len() + 1;
            let opened = Workspace {
                workspace_id: workspace_id.to_string(),
                pane_id: format!("w9:p{n}"),
                tab_id: Some(format!("w9:t{n}")),
                checkout_path: cwd.to_path_buf(),
            };
            tabs.insert(label.to_string(), opened.clone());
            Ok(opened)
        }

        fn find_tab(&self, _workspace_id: &str, label: &str) -> Result<Option<String>> {
            self.log(format!("find_tab {label}"));
            Ok(self
                .tabs
                .borrow()
                .get(label)
                .and_then(|tab| tab.tab_id.clone()))
        }

        fn move_self_into(&self, workspace_id: &str) -> Result<()> {
            self.log(format!("move_self_into {workspace_id}"));
            Ok(())
        }

        fn task_owns_workspace(&self) -> bool {
            self.task_owns_workspace
        }

        fn remove_checkout(&self, path: &Path) -> Result<()> {
            self.log(format!("remove_checkout {}", path.display()));
            Ok(())
        }

        fn create_workspace(
            &self,
            _cwd: &Path,
            branch: &str,
            base: &str,
            _label: &str,
        ) -> Result<Workspace> {
            self.log(format!("create_workspace on {branch} from {base}"));
            let checkout_path = PathBuf::from("/tmp/spoolway-fake-worktree");
            // A real (if minimal) git repo, not just a path: a launch now
            // resolves `{git_dir}` by asking git from inside the checkout,
            // and this constant stands in for a cut worktree everywhere the
            // fake mux is used for one. Guarded on `.git` already existing,
            // rather than run unconditionally, because every dispatch test
            // shares this one path and runs concurrently with the others —
            // two racing `git init`s on the same directory each fail trying
            // to lock the other's half-written `config`.
            if !checkout_path.join(".git").exists() {
                std::fs::create_dir_all(&checkout_path).unwrap();
                let _ = crate::repo::run(&checkout_path, "git", &["init", "-q"]);
            }
            Ok(Workspace {
                workspace_id: "w9".into(),
                pane_id: "w9:p1".into(),
                tab_id: Some("w9:t1".into()),
                checkout_path,
            })
        }
        fn close_workspace(&self, id: &str) -> Result<()> {
            self.log(format!("close_workspace {id}"));
            Ok(())
        }
        fn close_tab(&self, id: &str) -> Result<()> {
            self.log(format!("close_tab {id}"));
            // Model the multiplexer's own refusal: a workspace may not be left
            // with no tabs, so closing its last one fails.
            if self.last_tab.as_deref() == Some(id) {
                anyhow::bail!("cannot close the last tab in a workspace");
            }
            Ok(())
        }
        fn remove_workspace(&self, id: &str) -> Result<()> {
            self.log(format!("remove_workspace {id}"));
            if self.unbound_workspace {
                anyhow::bail!("workspace is not a Herdr-managed worktree checkout");
            }
            Ok(())
        }
        fn create_pane(&self, _cwd: &Path, label: &str) -> Result<Workspace> {
            self.log(format!("create_pane {label}"));
            Ok(Workspace {
                workspace_id: "w0".into(),
                pane_id: "w0:p9".into(),
                tab_id: Some("w0:t1".into()),
                checkout_path: _cwd.to_path_buf(),
            })
        }
        fn split_pane(&self, tab_id: &str, _cwd: &Path) -> Result<String> {
            let mut splits = self.splits.borrow_mut();
            *splits += 1;
            let pane = format!("{tab_id}.s{splits}");
            self.log(format!("split_pane {tab_id} -> {pane}"));
            Ok(pane)
        }
        #[cfg(unix)]
        fn run_in_pane(
            &self,
            tab_id: &str,
            cwd: &Path,
            label: &str,
            script: &str,
            env: &BTreeMap<String, String>,
        ) -> Result<Option<String>> {
            if !self.run_commands_in_pane {
                return Ok(None);
            }
            let mut splits = self.splits.borrow_mut();
            *splits += 1;
            let pane = format!("{tab_id}.s{splits}");
            self.log(format!("run_in_pane {tab_id} -> {pane} ({label})"));
            // The names in its environment, the same way `start_lane` logs a
            // lane's own — so a test can assert this backend was handed one at
            // all, without depending on the script text `script_for_pane` also
            // wrote it into.
            let mut names: Vec<&str> = env.keys().map(String::as_str).collect();
            names.sort_unstable();
            self.log(format!("run_in_pane env {} {}", pane, names.join(" ")));
            // A real backend hands the script to a live pane, which runs it as
            // its own foreground process; the fake actually runs it, detached
            // the same way `command_step::Runs::start` spawns one, so the
            // pid/exit files a test drives the dispatcher against behave
            // exactly as they would under a real backend.
            let spawned = std::process::Command::new("setsid")
                .arg("sh")
                .arg("-c")
                .arg(script)
                .current_dir(cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            crate::headless::reap_when_it_ends(spawned);
            Ok(Some(pane))
        }
        fn close_pane(&self, pane_id: &str) -> Result<()> {
            self.log(format!("close_pane {pane_id}"));
            Ok(())
        }
        fn start_lane(&self, spec: &LaneSpec<'_>) -> Result<()> {
            self.log(format!("start {}", spec.name));
            // The real backend labels the pane as the last thing `start_lane`
            // does; mirroring it here keeps the label assertable.
            self.log(format!("pane {}", spec.label));
            // Recorded separately so a test can assert on how the lane was
            // launched without every other assertion having to know about it.
            self.log(format!("launch {} pane={}", spec.name, spec.pane_id));
            // The rendered argv too, so a test can assert what a kind's lane
            // is actually handed.
            self.log(format!("args {} {}", spec.name, spec.args.join(" ")));
            // And the names in its environment, so the contract a prompt is
            // handed can be checked against the lane that is actually started.
            let mut names: Vec<&str> = spec.env.keys().map(String::as_str).collect();
            names.sort_unstable();
            self.log(format!("env {} {}", spec.name, names.join(" ")));
            if self.refuse_start {
                anyhow::bail!("agent_start_failed");
            }
            Ok(())
        }
        fn prompt(&self, name: &str, text: &str) -> Result<()> {
            self.log(format!("prompt {name}"));
            if self.refuse_prompt {
                anyhow::bail!("submission stalled even after Enter");
            }
            // A real pane's screen changes the moment the text lands on it —
            // see `screen`'s own doc comment for why that matters.
            self.screen
                .borrow_mut()
                .entry(name.to_string())
                .and_modify(|s| {
                    s.push('\n');
                    s.push_str(text);
                })
                .or_insert_with(|| text.to_string());
            Ok(())
        }
        fn read(&self, name: &str, _lines: usize) -> Result<String> {
            Ok(self.screen.borrow().get(name).cloned().unwrap_or_default())
        }
        fn interrupt_lane(&self, name: &str) -> Result<()> {
            self.log(format!("interrupt {name}"));
            Ok(())
        }
        fn stop_lane(&self, name: &str, pane_id: &str) -> Result<()> {
            self.log(format!("stop {name}"));
            self.log(format!("close_pane {pane_id}"));
            Ok(())
        }
        /// Answers the way a real backend does, and for the same reason: what
        /// makes a session leave is per kind, so a kind with no `quit` row
        /// closes the pane exactly as `stop_lane` would and a kind with one
        /// leaves it standing at its shell. `refusing_to_leave` is the third
        /// answer — the gesture sent, the agent still there at the bound.
        fn vacate_lane(&self, name: &str, kind: &str, pane_id: &str) -> Result<Vacated> {
            if crate::agent::adapter(kind)
                .and_then(|adapter| adapter.quit.as_ref())
                .is_none()
            {
                self.stop_lane(name, pane_id)?;
                return Ok(Vacated::PaneClosed);
            }
            self.log(format!("vacate {name}"));
            match self.stubborn {
                true => Ok(Vacated::StillOccupied),
                false => Ok(Vacated::Shell),
            }
        }
        fn focus_lane(&self, name: &str) -> Result<()> {
            self.log(format!("focus {name}"));
            Ok(())
        }
        fn rename_tab(&self, _tab: &str, label: &str) -> Result<()> {
            self.log(format!("tab {label}"));
            Ok(())
        }
        fn rename_workspace(&self, _workspace: &str, label: &str) -> Result<()> {
            self.log(format!("workspace {label}"));
            Ok(())
        }
        fn rename_pane(&self, _pane: &str, label: &str) -> Result<()> {
            self.log(format!("pane {label}"));
            Ok(())
        }
    }

    /// A lane of `repo`'s own: the dispatcher only recognises a session whose
    /// working directory is one of this project's, so the fixture's root is not
    /// decoration here.
    fn lane(repo: &Repo, name: &str, status: LaneStatus) -> Lane {
        lane_at(&repo.root, name, status)
    }

    /// Every task sitting on `step`, as the lane a pass has just started for
    /// it: in the task's own worktree, and `idle` — the state a lane is in
    /// between the multiplexer opening its pane and the agent taking its first
    /// turn.
    fn idle_lanes(repo: &Repo, step: &str) -> Vec<Lane> {
        repo.tasks()
            .unwrap()
            .iter()
            .filter(|task| task.stage() == step)
            .map(|task| Lane {
                name: lane_name(step, task.id()),
                status: LaneStatus::Idle,
                cwd: task.front.worktree_path.clone().unwrap(),
                ..lane_at(&repo.root, "", LaneStatus::Idle)
            })
            .collect()
    }

    /// A lane in a specific pane, distinct from [`lane_at`]'s default
    /// `w1:p1` — for a test that needs to tell a split-off pane apart from
    /// the one a task started in.
    fn lane_in(repo: &Repo, name: &str, status: LaneStatus, pane: &str) -> Lane {
        Lane {
            pane_id: pane.to_string(),
            ..lane(repo, name, status)
        }
    }

    fn lane_at(cwd: &Path, name: &str, status: LaneStatus) -> Lane {
        Lane {
            name: name.to_string(),
            kind: "pi".into(),
            status,
            pane_id: "w1:p1".into(),
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            cwd: cwd.to_path_buf(),
        }
    }

    /// With `scope_line` and `reading_block` both retired, a lane's `THIS
    /// PASS` block carries no statement of what its task is scoped to and no
    /// reading list of its own — the report contract is the first thing in
    /// it whenever there is no gate, fix pass or failed command to say
    /// first.
    #[test]
    fn this_pass_opens_on_the_report_contract_with_nothing_else_to_say() {
        let repo = fixture("no-scope-line");
        add_task_with(&repo, "earlier", "done", |f| {
            f.branch = Some("task/earlier".into());
        });
        let path = add_task_with(&repo, "login", "implement", |front| {
            front.touches = vec!["src/api/**".into(), "src/routes/**".into()];
            front.depends_on = vec!["earlier".into()];
        });
        let task = Task::load(&path).unwrap();
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get(&pipelines.default).unwrap();
        let step = pipeline.require_step("implement").unwrap();

        let prompt = sent(&repo, &task, pipeline, step);
        assert!(!prompt.contains("expected to change"), "{prompt}");
        assert!(!prompt.contains("out of scope"), "{prompt}");
        assert!(!prompt.contains("is not refused as it happens"), "{prompt}");
        assert!(!prompt.contains("Read these before you start"), "{prompt}");
        let (_, this_pass) = prompt.split_once("THIS PASS").unwrap();
        let after_header = this_pass.splitn(3, '\n').nth(2).unwrap();
        assert!(
            after_header
                .trim_start()
                .starts_with("Your last action is one `spoolway report` command"),
            "{after_header}"
        );
    }

    /// A dependency is named in `WHAT YOU HAVE` now — `spoolway queue show
    /// <dep>` — since `reading_block` is gone and its job folded in there.
    /// Worked out from frontmatter: dependencies, and nothing else —
    /// spoolway keeps no notion of documentation, so there is no document to
    /// name. The group is not named either, even though this task carries
    /// `group:` — no lane can open a plan page from inside its worktree, so
    /// nothing ever points at one.
    #[test]
    fn what_you_have_names_the_dependency_and_omits_the_group() {
        let repo = fixture("reading-list");
        add_task_with(&repo, "earlier-task", "done", |f| {
            f.branch = Some("task/earlier-task".into());
        });
        let path = add_task_with(&repo, "login", "implement", |front| {
            front.touches = vec!["src/api/routes.rs".into()];
            front.depends_on = vec!["earlier-task".into()];
            front.group = Some("auth".into());
        });
        let task = Task::load(&path).unwrap();

        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get(&pipelines.default).unwrap();
        let step = pipeline.require_step("implement").unwrap();
        let prompt = sent(&repo, &task, pipeline, step);

        assert!(
            prompt.contains("`spoolway queue show earlier-task` — what it left you"),
            "{prompt}"
        );
        assert!(
            !prompt.contains("the group this task came from"),
            "{prompt}"
        );
        assert!(!prompt.contains("auth"), "{prompt}");
    }

    /// `what_you_have`'s own choice of base is the subtle part of it: `base:`
    /// with no dependency, and the dependency's own recorded `branch:` —
    /// never `task.front.base` — when there is one. Pinned here rather than
    /// left to the wider prompt tests, which would keep passing if the
    /// branch chosen quietly went back to being wrong.
    #[test]
    fn what_you_have_reads_the_dependencys_branch_not_the_tasks_own_base() {
        let repo = fixture("what-you-have");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get(&pipelines.default).unwrap();
        let step = pipeline.require_step("implement").unwrap();

        let solo = Task::load(&add_task_with(&repo, "solo", "implement", |front| {
            front.base = Some("main".into());
        }))
        .unwrap();
        let prompt = sent(&repo, &solo, pipeline, step);
        assert!(prompt.contains("`git diff main...HEAD`"), "{prompt}");
        assert!(
            prompt.contains("`git log --oneline main..HEAD`"),
            "{prompt}"
        );
        assert!(prompt.contains("you sit on    `main`"), "{prompt}");

        // `base:` is deliberately something other than the dependency's own
        // branch, the way a task queued up front from a shared plan branch
        // is — see the `implement` handoff. The dependency's branch, read
        // from its own task file, must win regardless.
        add_task_with(&repo, "earlier", "done", |f| {
            f.branch = Some("task/earlier".into());
        });
        let stacked = Task::load(&add_task_with(&repo, "stacked", "implement", |front| {
            front.depends_on = vec!["earlier".into()];
            front.base = Some("plan/live".into());
        }))
        .unwrap();
        let prompt = sent(&repo, &stacked, pipeline, step);
        assert!(
            prompt.contains("`git diff task/earlier...HEAD`"),
            "{prompt}"
        );
        assert!(
            prompt.contains("`git log --oneline task/earlier..HEAD`"),
            "{prompt}"
        );
        assert!(
            prompt.contains("you sit on    `earlier`, branch `task/earlier`"),
            "{prompt}"
        );
        assert!(
            prompt.contains("`spoolway queue show earlier` — what it left you"),
            "{prompt}"
        );
        assert!(!prompt.contains("plan/live"), "{prompt}");
    }

    /// spoolway keeps no store to resolve `group:` against any more, and no
    /// lane can open the page it names from inside its worktree either way
    /// — an absolute path is exactly as unreachable as any other string, so
    /// it is never shown, whatever `group:` was recorded as.
    #[test]
    fn the_group_value_is_never_shown_however_it_was_recorded() {
        let repo = fixture("group-verbatim");
        let path = add_task_with(&repo, "login", "implement", |front| {
            front.group = Some("/abs/path/to/demo.html".into());
        });
        let task = Task::load(&path).unwrap();
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get(&pipelines.default).unwrap();
        let step = pipeline.require_step("implement").unwrap();

        assert!(!sent(&repo, &task, pipeline, step).contains("/abs/path/to/demo.html"));
    }

    /// Spoolway stopped naming headings to its lanes: what a task file looks
    /// like inside is the project's, and a prompt that says otherwise is the
    /// binary having an opinion about a file it does not read.
    #[test]
    fn the_opening_prompt_names_no_heading_of_the_task_body() {
        let repo = fixture("headings");
        let path = add_task(&repo, "login", "implement");
        let task = Task::load(&path).unwrap();
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get(&pipelines.default).unwrap();
        let step = pipeline.require_step("implement").unwrap();

        let prompt = sent(&repo, &task, pipeline, step);
        for heading in [
            "## Goal",
            "## Non-goals",
            "## Acceptance criteria",
            "## References",
        ] {
            assert!(
                !prompt.contains(heading),
                "still names {heading}:\n{prompt}"
            );
        }
    }

    /// A throwaway git repo with spoolway state in it. Real git, because the
    /// dispatcher resolves a task's base branch from the actual checkout.
    /// The reason the two halves are composed rather than sent separately.
    ///
    /// A project may delete every shipped prompt and write its own — that is
    /// the supported path, and prompts are prose spoolway never touches. What
    /// must survive is the framing: their prose reaches the lane whole, inside
    /// spoolway's own, and nothing parses or paraphrases it.
    ///
    /// There used to be a route to check here too — three near-identical
    /// paragraphs about merging, one per value of a setting, written by the
    /// dispatcher so that a prompt could not contradict the config. The
    /// setting is gone and so are they: what `handover` does with a branch is
    /// its own step's business, and the dispatcher having an opinion about it
    /// is what made replacing that one file impossible.
    // covers: step.prompt — the prompt a step names reaches its lane whole
    #[test]
    fn a_projects_own_prompt_reaches_its_lane_whole() {
        let repo = fixture("prompt-prompt");
        let task = reload(&add_task(&repo, "demo", "handover"));
        let theirs = "You hand work over. Be brief.";

        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("handover").unwrap();

        let composed = crate::compose::system_prompt(&repo, &task, pipeline, step, theirs).unwrap();
        assert!(composed.contains(theirs), "the prompt was not carried");
        // And nothing spoolway writes says how to use git.
        for gone in [
            "merges with git",
            "merges through the forge",
            "SPOOLWAY_MERGE_INTO",
        ] {
            assert!(!composed.contains(gone), "`{gone}` is back:\n{composed}");
        }
    }

    /// Everything a lane is sent, in the order it reads it: the composed system
    /// prompt, then the message typed into its pane.
    ///
    /// The assertions below are about what reaches the model, not about which
    /// of the two carries it — moving a paragraph between them is a decision
    /// this file gets to make without rewriting a dozen tests. The prompt is a
    /// placeholder because none of these are about a role: a real one is a
    /// project's file, and `system_prompt` never reads it, only frames it.
    fn sent(repo: &Repo, task: &Task, pipeline: &Pipeline, step: &Step) -> String {
        format!(
            "{}\n\n{}",
            crate::compose::system_prompt(repo, task, pipeline, step, "[the project's prompt]")
                .unwrap(),
            crate::compose::opening_prompt(repo, task, pipeline, step),
        )
    }

    /// A checkout path that exists on disk, which is what a task recorded as
    /// already placed needs: a task whose worktree has gone is one the
    /// dispatcher cuts a fresh workspace for, and a test asserting on the
    /// workspace it recorded would be asserting on the replacement.
    ///
    /// A real (if minimal) git repo, not just a directory: a launch now
    /// resolves `{git_dir}` by asking git from inside the worktree, and a
    /// plain directory answers "not a git repository" instead.
    fn a_checkout(name: &str) -> PathBuf {
        let path = crate::scratch::root(name);
        std::fs::create_dir_all(&path).unwrap();
        crate::repo::run(&path, "git", &["init", "-q"]).unwrap();
        path
    }

    fn fixture(name: &str) -> Repo {
        let root = crate::scratch::root(&format!("dispatch-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".spoolway/prompts")).unwrap();

        for shipped in crate::assets::PROMPTS {
            let path = root
                .join(".spoolway/prompts")
                .join(format!("{}.md", shipped.name));
            std::fs::write(path, shipped.body).unwrap();
        }

        for args in [
            vec!["init", "-q", "-b", "work"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "test"],
            vec!["commit", "-q", "--allow-empty", "-m", "root"],
        ] {
            crate::repo::run(&root, "git", &args).unwrap();
        }

        // A model is the step's to name now, not the profile's — the shipped
        // pipeline this fixture runs on (`Pipelines::builtin()`) already names
        // one on every agent step, so there is nothing left to configure here.
        let mut config = Config::default();
        // Patience, cut to something a test can age past in one line. The
        // shipped default is minutes, for the reasons `lane_quiet`'s own doc
        // comment gives, and a suite that had to age every lane past fifteen
        // real minutes would say nothing extra for it. What the default
        // actually is, and that it is long enough to sit out an end-to-end
        // run, is asserted on its own in
        // `the_shipped_patience_outlasts_a_lane_waiting_on_its_test_run`.
        config.dispatch.lane_quiet = Duration::from_secs(10);

        // The project's own home, scratch beside the checkout rather than the
        // real `~/.spoolway/<basename>/` — every test below writes its queue,
        // lanes and usage here through `Repo`'s own accessors, which create it
        // on demand, so nothing about this fixture depends on `$HOME`.
        let home = root.join(".home");

        // The one thing `home` above does *not* answer for. A worktree root is
        // read from the config by `crate::mux::worktree_root`, not from
        // `Repo::home`, and the fallback it takes when the config names none is
        // `~/.spoolway/<basename>/worktrees` in the real home. A fixture root's
        // basename carries a fresh process id every run, so that fallback could
        // never reuse or clean what the last run left: it put one directory per
        // run in the developer's home and kept it there. Naming a root here is
        // what stops it, and `no_fixture_cuts_a_worktree_in_the_real_home`
        // fails if this line goes away.
        //
        // A sibling of `root`, not a child: `worktree_root`'s own doc comment
        // gives the reason a cut checkout stays out of the project checkout,
        // and a fixture that put one inside would be testing a layout the
        // product never uses.
        config.dispatch.worktree_root = sibling(&root, "worktrees").display().to_string();

        Repo {
            checkout: root.clone(),
            root,
            config,
            home,
        }
    }

    /// A path beside `root`, named for what it holds — `<root>-worktrees` for
    /// `sibling(root, "worktrees")`.
    ///
    /// Beside rather than under, so a directory git is asked to treat as a
    /// separate checkout never sits inside the checkout it was cut from. It
    /// inherits `root`'s uniqueness, which already carries the process id and
    /// a sequence number, so two fixtures never share one.
    fn sibling(root: &Path, name: &str) -> PathBuf {
        let base = root
            .file_name()
            .expect("a scratch root always has a final component")
            .to_string_lossy();
        root.with_file_name(format!("{base}-{name}"))
    }

    fn add_task(repo: &Repo, id: &str, stage: &str) -> PathBuf {
        add_task_with(repo, id, stage, |_| {})
    }

    fn add_task_with(
        repo: &Repo,
        id: &str,
        stage: &str,
        edit: impl FnOnce(&mut Frontmatter),
    ) -> PathBuf {
        let path = repo.queue_dir().join(format!("{id}.md"));
        let mut front = Frontmatter {
            id: id.to_string(),
            title: String::new(),
            stage: stage.to_string(),
            touches: Vec::new(),
            depends_on: Vec::new(),
            parallel: false,
            borrowed: false,
            last_report: None,
            blocked_from: None,
            parked_from: None,
            resume: None,
            pipeline: None,
            group: None,
            source: None,
            plan: None,
            gate_at: None,
            branch: None,
            base: None,
            run: None,
            cut_from: None,
            base_commit: None,
            patch: None,
            skip: Vec::new(),
            trial: None,
            replay_of: None,
            worktree_path: None,
            workspace_id: None,
            pane_id: None,
            tab_id: None,
            attempts: 0,
            usage_limit_hold: false,
            quota_retries: 0,
            parked_until: None,
            parked_window: String::new(),
            parked_at: None,
            paused_at: None,
            launched_at: None,
            prompts: Default::default(),
            rounds: Default::default(),
            arrived_from: None,
            extra: Default::default(),
        };
        edit(&mut front);

        let task = Task {
            path: path.clone(),
            front,
            body: "## Goal\ndemo\n".into(),
        };
        task.save().unwrap();
        path
    }

    fn reload(path: &Path) -> Task {
        Task::load(path).unwrap()
    }

    fn run_pass(repo: &Repo, mux: &FakeMux) -> Report {
        Dispatcher::new(repo, &Pipelines::builtin(), mux, false)
            .pass()
            .unwrap()
    }

    fn run_pass_with(repo: &Repo, mux: &FakeMux, pipelines: &Pipelines) -> Report {
        Dispatcher::new(repo, pipelines, mux, false).pass().unwrap()
    }

    /// The shipped pipelines, minus their `blocked` step — the shape every
    /// pipeline had before one could declare it, and still the shape a
    /// pipeline that does not stage `blocked` has today.
    fn unstaffed_builtin() -> Pipelines {
        let mut pipelines = Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            pipeline.steps.retain(|s| s.id != crate::pipeline::BLOCKED);
        }
        pipelines
    }

    #[test]
    fn a_queued_task_gets_a_worktree_and_a_lane() {
        let repo = fixture("queued");
        let path = add_task(&repo, "demo", "queued");
        let mux = FakeMux::new(vec![]);

        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("create_workspace"),
            ["create_workspace on task/demo from work"]
        );
        assert_eq!(mux.did("start"), ["start demo · implement"]);
        assert_eq!(mux.did("prompt"), ["prompt demo · implement"]);
        // `create_workspace`'s own pane is the lane's pane directly — there
        // is no anchor to split it off any more, and a task running its
        // first step has exactly this one pane in its tab.
        assert!(mux.did("split_pane").is_empty(), "{:?}", mux.calls());
        assert_eq!(
            mux.did("launch"),
            ["launch demo · implement pane=w9:p1"],
            "{:?}",
            mux.calls()
        );

        let task = reload(&path);
        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.base.as_deref(), Some("work"));
        assert_eq!(task.front.workspace_id.as_deref(), Some("w9"));
        assert_eq!(task.front.pane_id.as_deref(), Some("w9:p1"));
        assert_eq!(task.prompts_at("implement"), 1);
    }

    /// A run of two plans is still one tab: every lane of one project shares
    /// it, whatever plan each task came from. Found the second and third
    /// time rather than opened again, which is what makes a second
    /// `spoolway dispatch` (or a second task) join the tab a first one
    /// already opened.
    #[test]
    fn every_lane_of_a_project_shares_one_tab_whatever_its_plan() {
        let repo = fixture("plan-tabs");
        let mux = FakeMux::new(vec![]).tabs_in_one_workspace();

        let wing = project_tab(&repo, &mux, Some(&repo.root)).unwrap().unwrap();
        let keel = project_tab(&repo, &mux, Some(&repo.root)).unwrap().unwrap();

        assert_eq!(
            wing.tab_id,
            keel.tab_id,
            "one project is one tab, whatever plan asks for it: {:?}",
            mux.calls()
        );
        assert_eq!(
            mux.did("open_tab"),
            [format!(
                "open_tab wD {}",
                crate::mux::project_label(&repo.root)
            )],
            "opened once and found every time after: {:?}",
            mux.calls()
        );
    }

    /// Under `grouped`, the first task of a project's run gives the shared
    /// tab a real home instead of the bare project root, and starts directly
    /// in the pane that opened it — no anchor left idle, and no split to
    /// make one. A second task joining that same tab has no pane of its own
    /// waiting for it, so it still splits one the ordinary way.
    #[test]
    fn a_projects_first_task_starts_in_the_pane_that_opened_its_tab() {
        let repo = fixture("queued-grouped");
        let first = add_task(&repo, "first", "queued");
        let mux = FakeMux::new(vec![]).tabs_in_one_workspace();

        run_pass(&repo, &mux);

        assert!(
            mux.did("split_pane").is_empty(),
            "the first task's own pane is the one the tab just opened with: {:?}",
            mux.calls()
        );
        assert_eq!(
            mux.did("launch"),
            ["launch first · implement pane=w9:p1"],
            "{:?}",
            mux.calls()
        );
        let task = reload(&first);
        assert_eq!(task.front.tab_id.as_deref(), Some("w9:t1"));
        assert_eq!(task.front.pane_id.as_deref(), Some("w9:p1"));

        let second = add_task(&repo, "second", "queued");
        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("split_pane"),
            ["split_pane w9:t1 -> w9:t1.s1"],
            "the tab already has a pane in it, so the second task's own is split: {:?}",
            mux.calls()
        );
        let task = reload(&second);
        assert_eq!(task.front.tab_id.as_deref(), Some("w9:t1"));
        assert_eq!(task.front.pane_id, None);
    }

    /// A project's tab is closed by its *last* task and by none of the ones
    /// before it: closing it while a sibling is still queued would take a
    /// lane that project is about to run — or one it is running right now —
    /// with it.
    #[test]
    fn a_projects_tab_goes_with_the_last_task_of_the_project() {
        let repo = fixture("plan-tab-last");
        let planned = |front: &mut Frontmatter| {
            front.workspace_id = Some("wD".into());
            front.tab_id = Some("wD:t7".into());
        };
        let first = add_task_with(&repo, "first", "implement", planned);
        let second = add_task_with(&repo, "second", "implement", planned);

        let mux = FakeMux::new(vec![]).tabs_in_one_workspace();
        let pipelines = Pipelines::builtin();
        let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        let mut report = Report::default();

        dispatcher
            .clean_up(&mut reload(&first), &[], &mut report)
            .unwrap();
        assert!(
            mux.did("close_tab").is_empty(),
            "the project still has a task in the queue: {:?}",
            mux.calls()
        );

        dispatcher
            .clean_up(&mut reload(&second), &[], &mut report)
            .unwrap();
        assert_eq!(
            mux.did("close_tab"),
            ["close_tab wD:t7"],
            "and the project's last task takes its tab with it: {:?}",
            mux.calls()
        );
    }

    /// The sparing itself, shared by the two stages that park a task in
    /// front of a person. A live lane — so the assertions actually exercise
    /// the stop_lane call rather than passing because there was nothing to
    /// stop: a parked task's pane must be spared, not merely absent. The
    /// lane is named for `lane_stage` because a pane keeps the name of the
    /// step that got the task here, not of the stage it parked at.
    fn a_stop_spares_the_parked_task(name: &str, stage: &str, lane_stage: &str) {
        let repo = fixture(name);
        let path = add_task_with(&repo, "demo", stage, |f| {
            f.workspace_id = Some("w1".into());
            f.worktree_path = Some(PathBuf::from("/tmp/spoolway-fake-worktree"));
            f.branch = Some("task/demo".into());
        });

        let lane = Lane {
            name: format!("demo · {lane_stage}"),
            kind: "claude".into(),
            status: LaneStatus::Working,
            pane_id: "w1:p1".into(),
            tab_id: "w1:t1".into(),
            workspace_id: "w1".into(),
            cwd: PathBuf::from("/tmp/spoolway-fake-worktree"),
        };
        let mux = FakeMux::new(vec![lane]);
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        assert!(mux.did("stop").is_empty(), "{:?}", mux.calls());
        assert!(mux.did("remove_workspace").is_empty(), "{:?}", mux.calls());
        assert!(mux.did("close_workspace").is_empty(), "{:?}", mux.calls());
        let task = reload(&path);
        assert_eq!(task.front.workspace_id.as_deref(), Some("w1"));
        assert!(path.exists(), "a parked task is never archived by a stop");
    }

    /// The one leftover a stop must not touch. A blocked task's pane is being
    /// held open for a person, and `resume` resumes it against the checkout
    /// underneath — sweeping that answers the question by deleting it.
    #[test]
    fn a_blocked_task_keeps_its_checkout_when_the_run_stops() {
        a_stop_spares_the_parked_task("stop-blocked", crate::pipeline::BLOCKED, "blocked");
    }

    /// The same sparing a blocked task gets, but for the other stage that
    /// parks in front of a person — and unconditionally, unlike `blocked`,
    /// which only counts when nobody is staffed to answer it.
    #[test]
    fn a_paused_task_keeps_its_checkout_when_the_run_stops() {
        a_stop_spares_the_parked_task("stop-paused", crate::pipeline::PAUSED, "implement");
    }

    /// Whether a branch is still there afterwards, asked of the fixture's own
    /// repository rather than of anything the sweep reported.
    fn has_branch(repo: &Repo, branch: &str) -> bool {
        crate::repo::run(
            &repo.root,
            "git",
            &["rev-parse", "--verify", "--quiet", branch],
        )
        .is_ok()
    }

    /// Stand in for a real `git push`: a remote-tracking ref pointing at
    /// `branch`'s own tip, which is all `Dispatcher::branch_fully_pushed`
    /// ever reads. No actual remote is needed for that check to answer yes.
    fn mark_pushed(repo: &Repo, branch: &str) {
        let sha = crate::repo::run(&repo.root, "git", &["rev-parse", branch]).unwrap();
        crate::repo::run(
            &repo.root,
            "git",
            &[
                "update-ref",
                &format!("refs/remotes/origin/{branch}"),
                sha.trim(),
            ],
        )
        .unwrap();
    }

    /// Interrupted, not finished: the checkout goes back but the task file stays
    /// in the queue, so the next run picks it up instead of treating it as done.
    ///
    /// And it picks it up *where it was*. The branch outlives the worktree that
    /// held it, because those commits are the only record of what the agent got
    /// done before the run stopped — a sweep that deleted it would reset the
    /// task to base while its file still claimed to be mid-step.
    #[test]
    fn an_interrupted_task_gives_its_checkout_back_but_keeps_its_branch() {
        let repo = fixture("stop-inflight");
        crate::repo::run(&repo.root, "git", &["branch", "task/demo"]).unwrap();
        let path = add_task(&repo, "demo", "implement");
        let mut task = reload(&path);
        task.front.workspace_id = Some("w1".into());
        task.front.worktree_path = Some(PathBuf::from("/tmp/spoolway-fake-worktree"));
        task.front.branch = Some("task/demo".into());
        task.save().unwrap();

        let mux = FakeMux::new(vec![]);
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        assert_eq!(mux.did("remove_workspace"), ["remove_workspace w1"]);
        assert!(path.exists(), "an interrupted task is not archived");
        let task = reload(&path);
        assert_eq!(task.stage(), "implement", "and it keeps its place");
        assert_eq!(task.front.workspace_id, None, "but holds no checkout now");
        assert!(
            has_branch(&repo, "task/demo"),
            "the interrupted work is still on its branch"
        );
        assert_eq!(
            task.front.branch.as_deref(),
            Some("task/demo"),
            "and the task still points at it, or the next run cuts a new one"
        );
    }

    /// The multiplexer's one call that was meant to take a checkout and the
    /// row above it together can simply refuse: herdr holds that pair only for
    /// a workspace it opened *onto* the checkout, and answers
    /// `not_linked_worktree` for any other row pointed at one — a task resumed
    /// by an older build, a row someone reopened by hand.
    ///
    /// The refusal used to be dropped on the floor, which left a finished task
    /// holding both its worktree and a stray workspace for good. Now it is read,
    /// and the two are taken apart separately: git removes the checkout, which
    /// needs no such binding, and the row is closed on its own.
    #[test]
    fn a_workspace_that_lost_its_checkout_still_gives_both_back() {
        let repo = fixture("stop-unbound");
        crate::repo::run(&repo.root, "git", &["branch", "task/demo"]).unwrap();
        let path = add_task(&repo, "demo", "implement");
        let mut task = reload(&path);
        task.front.workspace_id = Some("w1".into());
        task.front.worktree_path = Some(PathBuf::from("/tmp/spoolway-fake-worktree"));
        task.front.branch = Some("task/demo".into());
        task.save().unwrap();

        let mux = FakeMux::new(vec![]).with_unbound_workspace();
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        assert_eq!(mux.did("remove_workspace"), ["remove_workspace w1"]);
        assert_eq!(
            mux.did("remove_checkout"),
            ["remove_checkout /tmp/spoolway-fake-worktree"],
            "the worktree goes with git when the multiplexer will not take it"
        );
        assert_eq!(
            mux.did("close_workspace"),
            ["close_workspace w1"],
            "and the row it was under goes too, rather than standing for ever"
        );
    }

    /// A borrowed checkout is somebody else's, and the fallback above must
    /// never reach it. Its row is closed and its worktree is left exactly where
    /// it stands.
    #[test]
    fn a_borrowed_checkout_is_never_removed_by_the_fallback() {
        let repo = fixture("stop-borrowed-unbound");
        crate::repo::run(&repo.root, "git", &["branch", "task/demo"]).unwrap();
        let path = add_task(&repo, "demo", "implement");
        let mut task = reload(&path);
        task.front.workspace_id = Some("w1".into());
        task.front.tab_id = Some("w1:t1".into());
        task.front.worktree_path = Some(PathBuf::from("/tmp/spoolway-fake-worktree"));
        task.front.branch = Some("task/demo".into());
        task.front.borrowed = true;
        task.save().unwrap();

        let mux = FakeMux::new(vec![]).with_unbound_workspace();
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        assert!(mux.did("remove_workspace").is_empty(), "{:?}", mux.calls());
        assert!(mux.did("remove_checkout").is_empty(), "{:?}", mux.calls());
        assert_eq!(mux.did("close_tab"), ["close_tab w1:t1"]);
    }

    /// A task holding a worktree with no workspace recorded against it — its
    /// row was closed by hand, or the multiplexer lost it — used to have
    /// nowhere to hang the removal, so the checkout stayed behind. The worktree
    /// is spoolway's own cut either way, and goes back with git.
    #[test]
    fn a_checkout_with_no_workspace_left_is_still_given_back() {
        let repo = fixture("stop-no-workspace");
        crate::repo::run(&repo.root, "git", &["branch", "task/demo"]).unwrap();
        let path = add_task(&repo, "demo", "implement");
        let mut task = reload(&path);
        task.front.worktree_path = Some(PathBuf::from("/tmp/spoolway-fake-worktree"));
        task.front.branch = Some("task/demo".into());
        task.save().unwrap();

        let mux = FakeMux::new(vec![]);
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        assert_eq!(
            mux.did("remove_checkout"),
            ["remove_checkout /tmp/spoolway-fake-worktree"]
        );
    }

    /// Where a task is a *pane* in its project's shared tab, tearing it down
    /// must remove only its checkout by hand — with git, since its
    /// project's shared tab holds no worktree of its own to remove one from —
    /// and touch neither that tab nor the workspace behind it: both hold
    /// every other lane.
    #[test]
    fn a_task_that_shares_its_project_tab_gives_back_only_its_checkout() {
        let repo = fixture("stop-tabbed");
        crate::repo::run(&repo.root, "git", &["branch", "task/demo"]).unwrap();
        add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("wD".into());
            f.tab_id = Some("wD:t2".into());
            f.worktree_path = Some(PathBuf::from("/tmp/spoolway-fake-worktree"));
            f.branch = Some("task/demo".into());
        });

        let mux = FakeMux::new(vec![]).tabs_in_one_workspace();
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        assert_eq!(
            mux.did("remove_checkout"),
            ["remove_checkout /tmp/spoolway-fake-worktree"]
        );
        assert!(
            mux.did("remove_workspace").is_empty(),
            "the run's workspace is not this task's to remove: {:?}",
            mux.calls()
        );
        assert!(
            mux.did("close_workspace").is_empty(),
            "nor to close: {:?}",
            mux.calls()
        );
        // The project's own tab goes with the sweep's very last step, not
        // with this task's own teardown — see
        // `a_projects_tab_closes_only_when_nothing_was_spared`.
        assert_eq!(mux.did("close_tab"), ["close_tab wD:t2"]);
    }

    /// The fault this task fixes: under `grouped`, tearing a checkout down is
    /// nothing but `git worktree remove --force` on the directory, which
    /// never touches the pane. A stop has to end the lane itself first, or
    /// the agent keeps running against a directory that no longer exists.
    #[test]
    fn stopping_ends_a_lane_before_it_removes_the_checkout_under_grouped() {
        let repo = fixture("stop-ends-lane");
        crate::repo::run(&repo.root, "git", &["branch", "task/demo"]).unwrap();
        add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("wD".into());
            f.tab_id = Some("wD:t2".into());
            f.worktree_path = Some(PathBuf::from("/tmp/spoolway-fake-worktree"));
            f.branch = Some("task/demo".into());
        });

        let lane = Lane {
            name: "demo · implement".into(),
            kind: "claude".into(),
            status: LaneStatus::Working,
            pane_id: "wD:p9".into(),
            tab_id: "wD:t2".into(),
            workspace_id: "wD".into(),
            cwd: PathBuf::from("/tmp/spoolway-fake-worktree"),
        };
        let mux = FakeMux::new(vec![lane]).tabs_in_one_workspace();
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        let calls = mux.calls();
        let stop_at = calls.iter().position(|c| c == "stop demo · implement");
        let remove_at = calls.iter().position(|c| c.starts_with("remove_checkout"));
        assert!(
            stop_at.is_some() && remove_at.is_some(),
            "both calls must happen: {calls:?}"
        );
        assert!(
            stop_at < remove_at,
            "the lane must be ended before its checkout goes: {calls:?}"
        );
        assert_eq!(
            report.actions,
            ["stopping: ended 1 lane(s) and gave back 1 worktree(s)"]
        );
    }

    /// The same layout, with a checkout the task only borrowed: nothing here
    /// is this task's to give back at all — its pane is already stopped, the
    /// checkout is a person's, and the project's shared tab is not this
    /// task's to close.
    #[test]
    fn a_borrowed_task_under_a_shared_tab_gives_back_nothing_of_its_own() {
        let repo = fixture("stop-tabbed-borrowed");
        let path = add_task(&repo, "demo", "implement");
        let mut task = reload(&path);
        task.front.workspace_id = Some("wD".into());
        task.front.tab_id = Some("wD:t2".into());
        task.front.borrowed = true;
        task.front.worktree_path = Some(repo.root.clone());
        task.save().unwrap();

        let mux = FakeMux::new(vec![]).tabs_in_one_workspace();
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        assert!(
            mux.did("remove_checkout").is_empty(),
            "a borrowed checkout is never removed: {:?}",
            mux.calls()
        );
        assert!(
            mux.did("close_workspace").is_empty(),
            "and the run's workspace stays: {:?}",
            mux.calls()
        );
        // Only the sweep's final step touches the project's tab.
        assert_eq!(mux.did("close_tab"), ["close_tab wD:t2"]);
    }

    /// The other half of that deal, so sparing the interrupted case does not
    /// quietly become sparing everything: a task that *finished* is archived,
    /// and its local branch is spent litter.
    #[test]
    fn a_finished_task_takes_its_branch_with_it() {
        let repo = fixture("cleanup-branch");
        crate::repo::run(&repo.root, "git", &["branch", "task/demo"]).unwrap();
        mark_pushed(&repo, "task/demo");
        let path = add_task(&repo, "demo", "implement");
        let mut task = reload(&path);
        task.front.workspace_id = Some("w1".into());
        task.front.branch = Some("task/demo".into());
        task.save().unwrap();

        let mux = FakeMux::new(vec![]);
        let mut report = Report::default();
        let archived = Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .clean_up(&mut task, &[], &mut report)
            .unwrap();

        assert!(archived);
        assert!(!path.exists(), "a finished task leaves the queue");
        assert!(
            !has_branch(&repo, "task/demo"),
            "and does not leave its branch behind"
        );
    }

    /// The whole point of this check: a `handover` that never ran, or a push
    /// that failed silently, must not turn into the deletion of the only
    /// copy of a finished task's work. Nothing under `task/demo` is on any
    /// remote here, so the branch survives its own task's archiving, and the
    /// reason is on the run's own problem list.
    #[test]
    fn a_branch_with_unpushed_commits_survives_its_own_tasks_archiving() {
        let repo = fixture("cleanup-branch-unpushed");
        crate::repo::run(&repo.root, "git", &["branch", "task/demo"]).unwrap();
        let path = add_task(&repo, "demo", "implement");
        let mut task = reload(&path);
        task.front.workspace_id = Some("w1".into());
        task.front.branch = Some("task/demo".into());
        task.save().unwrap();

        let mux = FakeMux::new(vec![]);
        let mut report = Report::default();
        let archived = Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .clean_up(&mut task, &[], &mut report)
            .unwrap();

        assert!(archived, "the task itself still finishes");
        assert!(!path.exists(), "and still leaves the queue");
        assert!(
            has_branch(&repo, "task/demo"),
            "but its branch is kept — no remote has its commits"
        );
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("demo") && p.contains("task/demo")),
            "and the reason is recorded on the run's problem list, naming the \
             task and the branch: {:?}",
            report.problems
        );
    }

    /// The other exemption a finished task's branch gets, and the one no
    /// existing archiving test reached: a borrowed checkout's branch is
    /// somebody's own, not spoolway's, whatever `git rev-list` would say
    /// about it — so it is never even asked.
    #[test]
    fn a_borrowed_checkouts_branch_survives_archiving_even_fully_pushed() {
        let repo = fixture("cleanup-branch-borrowed");
        crate::repo::run(&repo.root, "git", &["branch", "task/demo"]).unwrap();
        mark_pushed(&repo, "task/demo");
        let path = add_task(&repo, "demo", "implement");
        let mut task = reload(&path);
        task.front.workspace_id = Some("w1".into());
        task.front.branch = Some("task/demo".into());
        task.front.borrowed = true;
        task.save().unwrap();

        let mux = FakeMux::new(vec![]);
        let mut report = Report::default();
        let archived = Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .clean_up(&mut task, &[], &mut report)
            .unwrap();

        assert!(archived);
        assert!(!path.exists(), "a finished task leaves the queue");
        assert!(
            has_branch(&repo, "task/demo"),
            "a borrowed checkout's branch is never spoolway's to delete"
        );
    }

    /// A branch kept once for lacking a remote is not stuck forever: once it
    /// is pushed, the next cleanup to pass over it — `sweep_orphaned_branches`,
    /// run at the end of every `clean_up` — is what frees it, the same way it
    /// already frees one a queued dependent stopped needing.
    #[test]
    fn an_unpushed_branch_is_freed_once_pushed_by_the_next_cleanups_sweep() {
        let repo = fixture("cleanup-branch-unpushed-then-pushed");
        crate::repo::run(&repo.root, "git", &["branch", "task/first"]).unwrap();
        let first = add_task(&repo, "first", "implement");
        let mut first_task = reload(&first);
        first_task.front.workspace_id = Some("w1".into());
        first_task.front.branch = Some("task/first".into());
        first_task.save().unwrap();

        let mux = FakeMux::new(vec![]);
        let pipelines = Pipelines::builtin();
        let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        let mut report = Report::default();

        dispatcher
            .clean_up(&mut first_task, &[], &mut report)
            .unwrap();
        assert!(
            has_branch(&repo, "task/first"),
            "unpushed, so kept by its own archiving"
        );

        // The push `handover` should have made, arriving late.
        mark_pushed(&repo, "task/first");

        // A second, unrelated task finishing is what runs the orphan sweep
        // next — nothing revisits `first` on its own once it is archived.
        let second = add_task(&repo, "second", "implement");
        let mut second_task = reload(&second);
        dispatcher
            .clean_up(&mut second_task, &[], &mut report)
            .unwrap();

        assert!(
            !has_branch(&repo, "task/first"),
            "now pushed and nothing needs it, so the orphan sweep frees it"
        );
    }

    /// A project's tab is worth keeping open for exactly one reason:
    /// something of it is blocked and still in it. Emptied by the sweep
    /// otherwise, and closed with it — but the shared workspace behind it is
    /// never spoolway's to close; see
    /// [`a_stop_never_closes_the_shared_workspace`].
    #[test]
    fn a_projects_tab_closes_only_when_nothing_was_spared() {
        for (stage, closes) in [("implement", true), (crate::pipeline::BLOCKED, false)] {
            let repo = fixture(&format!("stop-close-{stage}"));
            let path = add_task(&repo, "demo", stage);
            let mut task = reload(&path);
            task.front.workspace_id = Some("wD".into());
            task.front.tab_id = Some("wD:t7".into());
            task.front.branch = Some("task/demo".into());
            task.save().unwrap();

            let mux = FakeMux::new(vec![]).tabs_in_one_workspace();
            let mut report = Report::default();
            Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
                .sweep_on_stop(&mut report)
                .unwrap();

            assert_eq!(
                mux.did("close_tab") == ["close_tab wD:t7"],
                closes,
                "stage {stage}: {:?}",
                mux.calls()
            );
        }
    }

    /// The shared workspace is never spoolway's to close — only a person's,
    /// by hand — because another project's lanes may be live in it. Only the
    /// project's own tab, once the sweep has emptied it, ever closes.
    #[test]
    fn a_stop_never_closes_the_shared_workspace() {
        let repo = fixture("stop-close-self");
        let path = add_task(&repo, "demo", "implement");
        let mut task = reload(&path);
        task.front.workspace_id = Some("wD".into());
        task.front.tab_id = Some("wD:t7".into());
        task.front.branch = Some("task/demo".into());
        task.save().unwrap();

        let mux = FakeMux::new(vec![]).tabs_in_one_workspace();
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        assert_eq!(
            mux.did("close_workspace"),
            Vec::<String>::new(),
            "{:?}",
            mux.calls()
        );
        assert_eq!(
            mux.did("close_tab"),
            ["close_tab wD:t7"],
            "the project's tab goes back: {:?}",
            mux.calls()
        );
    }

    /// A run that opened no tab of its own must not open one on its way out
    /// just to close it again — a sweep with nothing to give back asks the
    /// multiplexer for nothing at all.
    #[test]
    fn stopping_never_opens_a_workspace_to_close_it() {
        let repo = fixture("stop-no-workspace");
        let mux = FakeMux::new(vec![]).tabs_in_one_workspace();
        let mut report = Report::default();
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        assert!(
            mux.did("dispatch_workspace").is_empty(),
            "{:?}",
            mux.calls()
        );
        assert!(mux.did("open_tab").is_empty(), "{:?}", mux.calls());
        assert!(mux.did("close_tab").is_empty(), "{:?}", mux.calls());
    }

    /// A task that owns its own workspace cuts its worktree through
    /// `Mux::create_workspace`, whatever the shared dispatch workspace itself
    /// answers — that call is only ever made under `MuxMode::Grouped`, and a
    /// task that owns its own row never reaches it.
    #[test]
    fn a_task_that_owns_its_workspace_cuts_its_own_worktree() {
        let repo = fixture("cut-against");
        add_task(&repo, "demo", "queued");
        let mux = FakeMux::new(vec![]);

        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("create_workspace"),
            ["create_workspace on task/demo from work"]
        );
    }

    /// Headless has no workspaces at all, and neither does a herdr too old to
    /// answer — and a task that owns its own workspace does not need one:
    /// `create_workspace` cuts its worktree with git either way.
    #[test]
    fn a_backend_with_no_workspaces_still_cuts_its_own_worktree() {
        let repo = fixture("cut-against-none");
        add_task(&repo, "demo", "queued");
        let mux = FakeMux::new(vec![]).without_workspaces();

        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("create_workspace"),
            ["create_workspace on task/demo from work"]
        );
    }

    /// A task's own `skip:` list walks a named step to its `on_pass` without
    /// starting a lane. It is the only thing that can: a pipeline runs every
    /// step it lists. `spoolway eval --replay` used to be what wrote this
    /// list; that command is gone, and nothing writes it any more — the field
    /// is exercised here directly, by hand, rather than through a command
    /// that no longer exists.
    #[test]
    fn a_named_step_in_skip_falls_through_without_a_lane() {
        let repo = fixture("skip-fallthrough");
        let path = add_task_with(&repo, "demo", "review", |f| {
            f.skip = vec!["review".into()];
        });
        let mux = FakeMux::new(vec![]);

        let report = run_pass(&repo, &mux);

        assert!(
            mux.did("start").iter().all(|a| !a.contains("review")),
            "no lane should have started for the skipped step: {:?}",
            mux.did("start")
        );
        assert!(
            report.actions.iter().any(|a| a.contains("(skip)")),
            "{:?}",
            report.actions
        );
        // `document` runs for real — it is not named in `skip:` — so the same
        // pass that walks past `review` reaches it too, and starts its lane
        // exactly as it would for a task arriving there any other way.
        assert_eq!(mux.did("start"), ["start demo · document"]);
        let task = reload(&path);
        assert_eq!(task.stage(), "document");
    }

    /// A `skip:` tail walks itself all the way to `done` in the pass that
    /// first reaches it — one dispatch pass, not one per skipped step —
    /// because the stage is re-examined right after each fall-through instead
    /// of waiting for the next pass to look again.
    #[test]
    fn a_skipped_tail_reaches_done_in_one_pass() {
        let repo = fixture("skip-tail");
        let path = add_task_with(&repo, "demo", "document", |f| {
            f.skip = vec!["document".into(), "handover".into(), "checks".into()];
        });
        let mux = FakeMux::new(vec![]);

        run_pass(&repo, &mux);

        assert!(
            mux.did("start").is_empty(),
            "no lane should have started for any step in the skipped tail"
        );
        assert!(!path.exists(), "task file should have left the queue");
        assert!(repo.archive_dir().join("demo.md").exists());
    }

    /// A pipeline whose one command step carries `last: true`, so a task
    /// either runs it or walks past it. `suite` is a `run:` rather than a lane
    /// because that is the only kind of step `last:` is allowed on — a lane
    /// nobody started reports nothing.
    fn last_pipelines() -> Pipelines {
        let yaml = "steps:\n  \
             - id: suite\n    run: true\n    last: true\n    on_pass: closeout\n  \
             - id: closeout\n    end: true\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);
        pipelines
    }

    /// Two pipelines of different lengths for
    /// [`fewer_steps_left_sorts_before_a_higher_raw_step_index`]: a four-step
    /// `default` where `document` is one step from the end, and a six-step
    /// `local` where `checkpoint`'s raw index (3) is higher than
    /// `document`'s (2) but is two steps from the end rather than one.
    fn priority_test_pipelines() -> Pipelines {
        let short = crate::pipeline::Pipeline::parse(
            "default",
            "steps:\n  \
             - id: implement\n    agent: pi\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: review\n  \
             - id: review\n    agent: pi\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: document\n  \
             - id: document\n    agent: pi\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: handover\n  \
             - id: handover\n    end: true\n",
        )
        .unwrap();
        let long = crate::pipeline::Pipeline::parse(
            "local",
            "steps:\n  \
             - id: a\n    agent: pi\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: b\n  \
             - id: b\n    agent: pi\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: c\n  \
             - id: c\n    agent: pi\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: checkpoint\n  \
             - id: checkpoint\n    agent: pi\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: e\n  \
             - id: e\n    agent: pi\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: f\n  \
             - id: f\n    end: true\n",
        )
        .unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), short);
        pipelines.pipelines.insert("local".into(), long);
        pipelines
    }

    /// The candidate sort's first tier is steps left, not raw step index — so
    /// a task nearly done on a short pipeline outranks a task on a long
    /// pipeline whose raw step index happens to be higher, even though it
    /// still has many steps ahead of it. Run through [`Dispatcher::pass`]
    /// itself, same as [`the_group_with_least_left_to_do_is_started_first`],
    /// rather than sorting hand-built candidates with a copy of the key —
    /// that would stay green even if the production sort reverted to the
    /// raw index it replaced.
    #[test]
    fn fewer_steps_left_sorts_before_a_higher_raw_step_index() {
        let mut repo = fixture("steps-left-order");
        add_task_with(&repo, "nearly-done", "document", |_| {});
        add_task_with(&repo, "far-along", "checkpoint", |f| {
            f.pipeline = Some("local".into());
        });

        // Only one slot free this pass.
        repo.config.agents.get_mut("pi").unwrap().concurrency = 1;

        let mux = FakeMux::new(vec![]);
        run_pass_with(&repo, &mux, &priority_test_pipelines());

        assert_eq!(mux.did("start"), ["start nearly-done · document"]);
    }

    /// In a chain, only the task at the top runs a `last:` step. The two below
    /// it have an unfinished task depending on them, so they walk past to
    /// `on_pass` without the command being started at all.
    ///
    /// Starts a real `run:` — see `crate::command_step` — but only checks
    /// that the dispatcher started it, never that it succeeds, so this needs
    /// nothing from the platform beyond a working `Runs::start`.
    #[test]
    fn only_the_top_of_a_chain_runs_a_last_step() {
        let repo = fixture("last-chain");
        let pipelines = last_pipelines();
        let mux = FakeMux::new(vec![]);

        for (id, deps) in [
            ("foot", vec![]),
            ("middle", vec!["foot"]),
            ("top", vec!["middle"]),
        ] {
            add_task_with(&repo, id, "suite", |f| {
                f.group = Some("stack".into());
                f.depends_on = deps.iter().map(|d| d.to_string()).collect();
            });
        }

        let report = run_pass_with(&repo, &mux, &pipelines);

        for id in ["foot", "middle"] {
            assert!(
                report
                    .actions
                    .iter()
                    .any(|a| a.contains(id) && a.contains("not last in its chain")),
                "`{id}` should have walked past `suite`: {:?}",
                report.actions
            );
        }
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("top") && a.contains("running `suite`")),
            "the top of the chain should have started `suite` for real: {:?}",
            report.actions
        );
    }

    /// A fan is not a stack. Nobody depends on anybody, so no task's branch
    /// contains another's and every one of them runs the step — the answer
    /// `when: last` got wrong by asking about declared dependents instead.
    #[test]
    fn every_task_of_a_fan_runs_a_last_step() {
        let repo = fixture("last-fan");
        let pipelines = last_pipelines();
        let mux = FakeMux::new(vec![]);

        for id in ["one", "two", "three"] {
            add_task_with(&repo, id, "suite", |f| {
                f.group = Some("spread".into());
                f.parallel = true;
            });
        }

        let report = run_pass_with(&repo, &mux, &pipelines);

        for id in ["one", "two", "three"] {
            assert!(
                report
                    .actions
                    .iter()
                    .any(|a| a.contains(id) && a.contains("running `suite`")),
                "`{id}` should have started `suite`: {:?}",
                report.actions
            );
        }
    }

    /// A task belonging to no group runs it too. There is no chain to be last
    /// in, and nothing above it will ever cover the work.
    #[test]
    fn a_task_with_no_group_runs_a_last_step() {
        let repo = fixture("last-lone");
        let pipelines = last_pipelines();
        let mux = FakeMux::new(vec![]);
        add_task(&repo, "alone", "suite");

        let report = run_pass_with(&repo, &mux, &pipelines);

        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("alone") && a.contains("running `suite`")),
            "a task with no group should have started `suite`: {:?}",
            report.actions
        );
    }

    /// A grid of panes is only readable if each says what it is: the pane is
    /// labelled with the lane's own name, the same string every later call
    /// addresses it by — one name, not a display name over a hidden key.
    #[test]
    fn a_pane_is_labelled_with_the_lanes_own_name() {
        let repo = fixture("pane-label");
        add_task(&repo, "demo", "queued");
        let mux = FakeMux::new(vec![]);

        run_pass(&repo, &mux);

        assert_eq!(mux.did("pane"), ["pane demo · implement"]);
        assert_eq!(mux.did("start"), ["start demo · implement"]);
    }

    /// `headless` records a pane for a task and never a tab — its
    /// `create_workspace`/`create_pane` both answer `tab_id: None` — so
    /// `start_one` must fall back to the recorded pane rather than demand a
    /// tab that will never come. Before this fell back, every headless lane
    /// failed to start with "task has a workspace but no recorded tab or
    /// pane", which hung the dispatcher retrying forever; 621 other unit
    /// tests stayed green because none of them modelled a backend with no
    /// tabs at all.
    #[test]
    fn a_lane_starts_from_its_recorded_pane_when_the_backend_has_no_tabs() {
        let repo = fixture("no-tabs");
        let worktree = repo.root.join("wt-demo");
        std::fs::create_dir_all(&worktree).unwrap();
        add_task_with(&repo, "demo", "implement", |front| {
            front.workspace_id = Some("w1".into());
            front.tab_id = None;
            front.pane_id = Some("w1:p1".into());
            front.worktree_path = Some(worktree);
        });
        let mux = FakeMux::new(vec![]);

        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("split_pane"),
            ["split_pane w1:p1 -> w1:p1.s1"],
            "the recorded pane is what a tab-less backend splits from: {:?}",
            mux.calls()
        );
        assert_eq!(mux.did("start"), ["start demo · implement"]);
    }

    /// A claude lane gets its prompt as file contents, not as a path pasted
    /// into the prompt.
    #[test]
    fn a_claude_lane_is_launched_with_its_prompt_as_a_file() {
        let repo = fixture("claude-lane");
        add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(a_checkout("dispatch-claude-lane"));
        });
        let mux = FakeMux::new(vec![]);

        run_pass(&repo, &mux);

        let args = mux.did("args demo · review");
        assert_eq!(args.len(), 1, "the review lane did not start");
        assert!(
            args[0].contains("--append-system-prompt-file"),
            "claude takes literal text in --append-system-prompt, so the file \
             variant is the only one that delivers the prompt: {}",
            args[0]
        );
    }

    /// A profile naming a kind `agent::ADAPTERS` has no row for is a config
    /// mistake, and it reads as one — not as a lane silently not starting.
    // covers: agents.<profile>.kind — the kind a profile names is what its lanes launch, and an unknown one is refused
    #[test]
    fn a_profile_naming_an_unknown_kind_is_refused_rather_than_launched() {
        let mut repo = fixture("unknown-kind");
        repo.config.agents.get_mut("pi").unwrap().kind = "mystery".into();
        add_task(&repo, "demo", "queued");
        let mux = FakeMux::new(vec![]);

        let report = run_pass(&repo, &mux);

        assert_eq!(report.problems.len(), 1);
        assert!(
            report.problems[0].contains("does not know agent kind"),
            "{}",
            report.problems[0]
        );
        assert!(mux.did("launch demo · implement").is_empty());
    }

    /// Set here rather than taken from the shipped profile: `pi` ships
    /// without a `concurrency`, because how many lanes one local model serves
    /// is that model's `slots` and not the harness's business. The cap itself
    /// is unchanged and is what `claude` still runs on, so the test states the
    /// number it is exercising.
    #[test]
    fn the_concurrency_cap_is_enforced_across_tasks() {
        let mut repo = fixture("cap");
        repo.config.agents.get_mut("pi").unwrap().concurrency = 2;

        for id in ["a", "b", "c"] {
            add_task(&repo, id, "queued");
        }
        let mux = FakeMux::new(vec![]);

        run_pass(&repo, &mux);

        // `concurrency = 2`, so the third waits.
        assert_eq!(mux.did("start").len(), 2, "started: {:?}", mux.did("start"));
    }

    #[test]
    fn a_running_lane_already_counts_against_the_cap() {
        let mut repo = fixture("cap-running");
        repo.config.agents.get_mut("pi").unwrap().concurrency = 2;
        add_task_with(&repo, "busy", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        add_task(&repo, "next", "queued");
        add_task(&repo, "later", "queued");

        let mux = FakeMux::new(vec![lane(&repo, "busy · implement", LaneStatus::Working)]);
        run_pass(&repo, &mux);

        // One of two slots is taken, so exactly one new lane starts.
        assert_eq!(mux.did("start").len(), 1);
    }

    #[test]
    fn work_nearest_the_end_is_scheduled_first() {
        let mut repo = fixture("priority");
        add_task(&repo, "fresh", crate::pipeline::QUEUED);
        add_task_with(&repo, "nearly-done", "document", |f| {
            f.workspace_id = Some("w2".into());
            f.tab_id = Some("w2:t1".into());
            f.pane_id = Some("w2:p1".into());
        });

        // Only one slot free this pass.
        repo.config.agents.get_mut("pi").unwrap().concurrency = 1;

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("start"), ["start nearly-done · document"]);
    }

    /// spoolway names no model of its own, so a step with none has to be
    /// told — starting an agent with an empty `--model` fails somewhere far
    /// less legible.
    #[test]
    fn a_step_with_no_model_configured_refuses_to_start_a_lane() {
        let repo = fixture("no-model");
        let mut pipeline = Pipelines::builtin().get("default").unwrap().clone();
        pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap()
            .model = None;
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);

        add_task(&repo, "demo", "queued");

        let mux = FakeMux::new(vec![]);
        let report = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();

        assert!(mux.did("start").is_empty(), "no lane should have started");
        assert_eq!(report.problems.len(), 1);
        assert!(
            report.problems[0].contains("has no model"),
            "the problem must say how to fix it: {}",
            report.problems[0]
        );
    }

    /// A pipeline step naming an agent profile config does not define is a
    /// per-candidate problem, not a `?` that aborts the whole pass — which
    /// used to discard every lane record built earlier in the same pass and
    /// fill the problem log with the same line every interval. See review
    /// finding 8.
    #[test]
    fn a_step_naming_an_undefined_agent_profile_is_a_problem_not_a_pass_abort() {
        let repo = fixture("undefined-agent");
        let mut pipeline = Pipelines::builtin().get("default").unwrap().clone();
        pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap()
            .agent = Some("ghost".into());
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);

        add_task(&repo, "demo", "implement");

        let mux = FakeMux::new(vec![]);
        let report = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .expect("an undefined profile does not abort the pass");

        assert!(mux.did("start").is_empty(), "no lane should have started");
        assert!(
            report.problems.iter().any(|p| p.contains("ghost")),
            "the problem names the missing profile: {:?}",
            report.problems
        );
    }

    /// The lost-update review finding 2: a lane's `spoolway report` lands
    /// while the pass holds a stale in-memory copy, and the pass's next
    /// save must not overwrite the report. `persist_task` reloads under the
    /// per-task lock and drops its write when `last_report` has moved past
    /// what the pass first read.
    #[test]
    fn persist_task_does_not_overwrite_a_report_that_landed_mid_pass() {
        let repo = fixture("persist-guards-a-report");
        let path = add_task(&repo, "demo", "implement");

        // What the pass read: no report yet.
        let mut stale = Task::load(&path).unwrap();
        let seen: HashMap<String, i64> = [("demo".to_string(), 0)].into_iter().collect();

        // A lane reports: `last_report` is stamped and the stage moves.
        let mut reported = Task::load(&path).unwrap();
        reported.front.last_report = Some(crate::task::LastReport {
            step: "implement".into(),
            outcome: "pass".into(),
            at: 5_000,
        });
        reported.set_stage("review", Some("done"));
        reported.save().unwrap();

        // The pass now writes its stale copy back — an escalation for a step
        // the task has already left.
        stale.set_stage(crate::pipeline::BLOCKED, Some("stale escalation"));
        persist_task(&repo, &mut stale, &seen).unwrap();

        let on_disk = Task::load(&path).unwrap();
        assert_eq!(
            on_disk.stage(),
            "review",
            "the report's move stands, not the pass's stale one"
        );
        assert_eq!(on_disk.front.last_report.unwrap().at, 5_000);
    }

    /// Review finding 38: a backend that reports a lane's `cwd` with its
    /// symlinks resolved must still be recognised as ours. `our_checkouts`
    /// records both spellings of every worktree path, and `owns_cwd` falls
    /// back to a canonical comparison — either half alone closes this, and
    /// the test exercises both.
    #[cfg(unix)]
    #[test]
    fn owns_cwd_matches_a_worktree_path_the_backend_canonicalised() {
        let repo = fixture("owns-cwd-symlink");
        let base = repo.root.join("wt");
        let real = base.join("real-worktree");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("linked-worktree");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let resolved = std::fs::canonicalize(&link).unwrap();
        assert_ne!(
            link, resolved,
            "the symlink and its target spell differently"
        );

        // The task recorded the symlink spelling; the backend reports the
        // resolved one.
        add_task_with(&repo, "demo", "implement", |front| {
            front.worktree_path = Some(link.clone());
        });
        let tasks = repo.tasks().unwrap();
        let mine = our_checkouts(&repo, &tasks);
        assert!(
            owns_cwd(&mine, &resolved),
            "a lane whose cwd is the resolved path is still ours"
        );
        assert!(
            !owns_cwd(&mine, &base.join("someone-elses")),
            "an unrelated path is not"
        );

        // And the other way round: `owns_cwd`'s own fallback, with only the
        // resolved spelling in the set and the symlink spelling as the cwd.
        let only_resolved: HashSet<PathBuf> = std::iter::once(resolved).collect();
        assert!(owns_cwd(&only_resolved, &link));
    }

    /// A corrupt `lanes.json` is kept as a `.bad` copy rather than silently
    /// discarded and overwritten, and the pass still runs. See review
    /// finding 31.
    #[test]
    fn a_corrupt_lanes_json_is_kept_as_a_bad_copy() {
        let repo = fixture("corrupt-lanes");
        add_task(&repo, "demo", "queued");
        let lanes = repo.lanes_file();
        std::fs::create_dir_all(lanes.parent().unwrap()).unwrap();
        std::fs::write(&lanes, "{ this is not json").unwrap();

        let mux = FakeMux::new(vec![]);
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .pass()
            .expect("a corrupt lanes.json does not fail the pass");

        let bad = lanes.with_extension("json.bad");
        assert_eq!(
            std::fs::read_to_string(&bad).unwrap(),
            "{ this is not json",
            "the original bytes are kept for a person to look at"
        );
    }

    /// A model's own `slots` replaces its profile's `concurrency` when set —
    /// swapping a local model to a smaller one shrinks the pool without
    /// anybody touching `[agents.pi]`.
    // covers: models.<glob>.slots — a model's own cap replaces its profile's concurrency
    #[test]
    fn a_models_slots_cap_lanes_instead_of_the_profiles_concurrency() {
        let mut repo = fixture("model-slots");
        repo.config.models.insert(
            "your-local-model".to_string(),
            crate::usage::ModelPrice {
                slots: 1,
                ..Default::default()
            },
        );

        for id in ["a", "b", "c"] {
            add_task(&repo, id, "queued");
        }
        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        // `[agents.pi] concurrency = 2` would let two through; the
        // model's own `slots = 1` is what actually caps this.
        assert_eq!(mux.did("start").len(), 1, "started: {:?}", mux.did("start"));
    }

    /// The incident this pair of assertions is named after: five concurrent
    /// lanes on a model declaring `slots = 3`. The first pass was right — it
    /// started three and made the other two wait. The pass ten seconds later
    /// was the one that broke, because it asked the multiplexer what those
    /// three lanes were *doing* and none of them was doing anything yet: a
    /// lane's first seconds are `idle`, "started but never prompted", so a cap
    /// counting only busy lanes saw an empty machine and filled it again.
    ///
    /// Nothing about the second pass is contrived. Ten seconds is this
    /// project's own `interval`, and a local model loading 35B of weights is
    /// not mid-turn ten seconds after its pane opened.
    #[test]
    fn a_lane_that_has_not_started_working_yet_still_holds_its_models_slot() {
        let mut repo = fixture("model-slots-idle");
        repo.config.models.insert(
            "*local-model".to_string(),
            crate::usage::ModelPrice {
                slots: 3,
                ..Default::default()
            },
        );

        for id in ["a", "b", "c", "d", "e"] {
            add_task(&repo, id, "queued");
        }

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);
        assert_eq!(
            mux.did("start").len(),
            3,
            "the first pass fills the model exactly once: {:?}",
            mux.did("start")
        );

        let mux = FakeMux::new(idle_lanes(&repo, "implement"));
        let report = run_pass(&repo, &mux);
        assert!(
            mux.did("start").is_empty(),
            "three lanes still hold the model, however idle they look: {:?}",
            mux.did("start")
        );
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("waiting for a `your-local-model` slot (3/3)")),
            "the wait must say which slot ran out: {:?}",
            report.actions
        );
    }

    /// Exclusivity has the same blind spot and the same fix: the resident is
    /// whichever exclusive model has a lane, not whichever has a lane that
    /// happens to be mid-turn. Weights are on the card from the moment the
    /// lane opens, and a second set of them arriving is the swap this flag
    /// exists to prevent.
    #[test]
    fn an_idle_lane_is_still_the_resident_exclusive_model() {
        let mut repo = fixture("exclusive-idle");
        for model in ["your-local-model", "other-local-model"] {
            repo.config.models.insert(
                model.to_string(),
                crate::usage::ModelPrice {
                    exclusive: true,
                    ..Default::default()
                },
            );
        }

        let mut pipeline = Pipelines::builtin().get("default").unwrap().clone();
        pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "document")
            .unwrap()
            .model = Some("other-local-model".to_string());
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);

        add_task_with(&repo, "busy", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        add_task(&repo, "waiting", "document");

        let mux = FakeMux::new(vec![lane(&repo, "busy · implement", LaneStatus::Idle)]);
        let report = run_pass_with(&repo, &mux, &pipelines);

        assert!(mux.did("start").is_empty(), "no lane should have started");
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("your-local-model") && a.contains("exclusive")),
            "the wait must name the resident model: {:?}",
            report.actions
        );
    }

    /// `slot: false` buys a step out of its profile's `concurrency` — a cheap
    /// cloud review not worth holding a lane back — and out of nothing else. A
    /// model's `slots` is how many of these weights the machine serves at
    /// once, and no step's opinion changes that number.
    // covers: step.slot — a step exempted from its profile's slots still answers to its model's
    #[test]
    fn a_step_that_takes_no_profile_slot_still_takes_a_models() {
        let mut repo = fixture("model-slots-optout");
        repo.config.models.insert(
            "your-local-model".to_string(),
            crate::usage::ModelPrice {
                slots: 1,
                ..Default::default()
            },
        );

        let mut pipeline = Pipelines::builtin().get("default").unwrap().clone();
        for step in &mut pipeline.steps {
            step.slot = false;
        }
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);

        for id in ["a", "b"] {
            add_task(&repo, id, "queued");
        }
        let mux = FakeMux::new(vec![]);
        run_pass_with(&repo, &mux, &pipelines);

        assert_eq!(
            mux.did("start").len(),
            1,
            "one slot, one lane, whatever the step says: {:?}",
            mux.did("start")
        );
    }

    /// Two models both carrying `exclusive = true` never run at once, however
    /// many slots either has and whatever the waiting step's own `slot:`
    /// says — the resident model is named in the wait.
    // covers: models.<glob>.exclusive — one set of weights on the card at a time
    #[test]
    fn two_exclusive_models_never_run_in_the_same_pass() {
        let mut repo = fixture("exclusive");
        repo.config.models.insert(
            "model-a".to_string(),
            crate::usage::ModelPrice {
                exclusive: true,
                ..Default::default()
            },
        );
        repo.config.models.insert(
            "model-b".to_string(),
            crate::usage::ModelPrice {
                exclusive: true,
                ..Default::default()
            },
        );

        let mut pipeline = Pipelines::builtin().get("default").unwrap().clone();
        for (id, model) in [("implement", "model-a"), ("document", "model-b")] {
            pipeline
                .steps
                .iter_mut()
                .find(|s| s.id == id)
                .unwrap()
                .model = Some(model.to_string());
        }
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);

        add_task_with(&repo, "busy", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        add_task(&repo, "waiting", "document");

        let mux = FakeMux::new(vec![lane(&repo, "busy · implement", LaneStatus::Working)]);
        let report = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();

        assert!(mux.did("start").is_empty(), "no lane should have started");
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("model-a") && a.contains("exclusive")),
            "the wait must name the resident model: {:?}",
            report.actions
        );
    }

    #[test]
    fn a_settled_lane_on_a_superseded_step_is_freed() {
        let repo = fixture("free");
        // The prompt already reported, so the task moved to `review` while
        // `implement`'s finished session still occupies the pane.
        add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("stop"), ["stop demo · implement"]);
    }

    /// The bug `pane-per-task` exists to close: a report moves the task's
    /// stage on the spot, but the lane that wrote it keeps talking and so is
    /// still `Working` — busy, not settled — when this pass reads the queue.
    /// `free_finished_lanes` leaves a busy lane alone, which is right; what is
    /// wrong is that the candidate scan below it does not know the old lane
    /// is still alive, finds no lane recorded under the new step's own name,
    /// and starts one anyway. For a moment the task holds two panes in its
    /// tab. The next step must instead wait for `implement`'s lane to settle
    /// before it is allowed to start at all.
    #[test]
    fn a_task_holds_only_one_lane_while_its_old_step_is_still_busy() {
        let repo = fixture("pane-per-task");
        add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(a_checkout("dispatch-pane-per-task"));
        });

        // `implement` reported and the task moved to `review`, but its lane
        // is still mid-sentence — `Working`, not settled — so nothing has
        // freed its pane yet.
        let mux = FakeMux::new(vec![lane_in(
            &repo,
            "demo · implement",
            LaneStatus::Working,
            "w1:p7",
        )]);
        run_pass(&repo, &mux);

        assert!(
            mux.did("split_pane").is_empty(),
            "`review` must not split a second pane while `implement`'s lane \
             is still alive: {:?}",
            mux.did("split_pane")
        );
    }

    /// A kind with no way to leave its pane keeps the lifecycle it always had:
    /// its pane closes with it, and the next step splits one of its own. `pi`
    /// is such a kind — nothing in the adapter table says how to talk one out
    /// of a pane, so there is nothing to send and nothing to wait for.
    ///
    /// The tab recorded on the task is what every such pane is split from, and
    /// it is never closed on a step's account — closing it would take the
    /// workspace with it, and with the workspace the task's worktree.
    #[test]
    fn a_kind_with_no_way_to_leave_still_gets_a_pane_of_its_own() {
        let repo = fixture("pane-per-step");
        add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(a_checkout("dispatch-pane-per-step"));
        });

        // `implement` reported and the task moved on, but its agent is still
        // sitting in the pane it was given.
        let mux = FakeMux::new(vec![lane_in(
            &repo,
            "demo · implement",
            LaneStatus::Done,
            "w1:p7",
        )]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("split_pane"), ["split_pane w1:t1 -> w1:t1.s1"]);
        assert_eq!(
            mux.did("launch demo · review")[0],
            "launch demo · review pane=w1:t1.s1",
            "the new step has to be launched into the pane just split, not the tab itself"
        );
        assert_eq!(
            mux.did("close_pane"),
            ["close_pane w1:p7"],
            "the finished lane's pane goes, and nothing else does"
        );
    }

    /// The pane is the task's, not the step's. A `claude` lane that has
    /// settled is talked out of its pane rather than having it closed, and the
    /// step after it starts in that same pane — so the task's `pane_id` is the
    /// one thing about it that does not change from step to step.
    #[test]
    fn a_settled_lane_hands_its_pane_to_the_next_step() {
        let repo = fixture("pane-handover");
        add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(a_checkout("dispatch-pane-handover"));
        });

        // `implement` reported, the task moved on, and its lane has finished
        // talking — settled, on a kind that knows how to leave a pane.
        let mux = FakeMux::new(vec![Lane {
            kind: "claude".into(),
            ..lane_in(&repo, "demo · implement", LaneStatus::Done, "w1:p7")
        }]);
        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("vacate"),
            ["vacate demo · implement"],
            "the finished lane is asked to leave its pane, not closed out of it"
        );
        assert!(
            mux.did("split_pane").is_empty(),
            "nothing is split: the pane the last step left is the pane this one \
             starts in — {:?}",
            mux.did("split_pane")
        );
        assert!(
            mux.did("close_pane").is_empty(),
            "and nothing is closed either — {:?}",
            mux.did("close_pane")
        );
        assert_eq!(
            mux.did("launch demo · review")[0],
            "launch demo · review pane=w1:p7",
            "the next step is launched into the pane its predecessor handed over"
        );
    }

    /// The one failure a handover has to survive: the gesture is sent and the
    /// agent stays put, which is a modal in the pane with nobody there to
    /// answer it. The pane cannot simply be closed — under a shared tab, a
    /// pane closed with nothing beside it takes the tab with it — so the next
    /// step's pane is split first and the stuck one closed after.
    #[test]
    fn a_session_that_will_not_leave_is_closed_only_after_its_replacement_is_split() {
        let repo = fixture("pane-stuck");
        add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(a_checkout("dispatch-pane-stuck"));
        });

        let mux = FakeMux::new(vec![Lane {
            kind: "claude".into(),
            ..lane_in(&repo, "demo · implement", LaneStatus::Done, "w1:p7")
        }])
        .refusing_to_leave();
        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("split_pane"),
            ["split_pane w1:t1 -> w1:t1.s1"],
            "a pane that would not empty is not handed on: `review` splits its own"
        );
        assert_eq!(
            mux.did("close_pane"),
            ["close_pane w1:p7"],
            "and the stuck one goes, once there is something else in the tab"
        );
        let order: Vec<String> = mux
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("split_pane") || call.starts_with("close_pane"))
            .collect();
        assert_eq!(
            order,
            ["split_pane w1:t1 -> w1:t1.s1", "close_pane w1:p7"],
            "split first, close second — the other order leaves the tab empty for a moment"
        );
    }

    /// A lane that reported and then never stopped talking would hold its
    /// task's pane for ever, and with it every step the task has left. The
    /// wait is bounded: past `HANDOVER_WAIT` the next step starts anyway,
    /// splitting its own pane before the stuck lane's is closed.
    #[test]
    fn a_lane_that_never_settles_stops_holding_the_next_step_up() {
        let repo = fixture("handover-bound");
        add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(a_checkout("dispatch-handover-bound"));
        });

        let mux = FakeMux::new(vec![lane_in(
            &repo,
            "demo · implement",
            LaneStatus::Working,
            "w1:p7",
        )]);

        // The first pass starts the clock and waits, as it should.
        run_pass(&repo, &mux);
        assert!(
            mux.did("split_pane").is_empty(),
            "still inside the bound: {:?}",
            mux.did("split_pane")
        );

        // A lane still working a whole `HANDOVER_WAIT` after its task moved on
        // is not finishing a sentence.
        age_handover(
            &repo,
            "demo · implement",
            HANDOVER_WAIT + Duration::from_secs(1),
        );
        mux.clear_calls();
        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("split_pane"),
            ["split_pane w1:t1 -> w1:t1.s1"],
            "past the bound `review` starts anyway, in a pane of its own"
        );
        assert_eq!(
            mux.did("close_pane"),
            ["close_pane w1:p7"],
            "and only then is the lane that would not let go closed"
        );
    }

    /// Every failed start would otherwise leave a pane behind, and a step that
    /// keeps failing is retried every dispatch pass.
    #[test]
    fn a_start_that_fails_takes_its_pane_back_with_it() {
        let repo = fixture("pane-leak");
        add_task(&repo, "demo", "queued");

        let mux = FakeMux::new(vec![]).refusing_to_start();
        let report = run_pass(&repo, &mux);

        // `create_workspace`'s own pane is this task's first pane directly —
        // nothing splits it, so what a failed launch takes back is that same
        // pane rather than one just split off it.
        assert!(mux.did("split_pane").is_empty(), "{:?}", mux.calls());
        assert_eq!(mux.did("close_pane"), ["close_pane w9:p1"]);
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("agent_start_failed")),
            "problems: {:?}",
            report.problems
        );
    }

    /// A lane that starts and never gets its briefing is the worst shape a
    /// failed launch can take, because it does not look like a failure. The
    /// session is up and listed, so every later pass reads the step as
    /// staffed and the board draws the task as running — for the whole of
    /// `dispatch.lane_quiet`, then three reminders to report sent to a
    /// session nobody ever told what to do, then `blocked`. Taking the lane
    /// back here is what turns that hour into one retry.
    #[test]
    fn a_prompt_that_never_lands_takes_its_lane_back_with_it() {
        let repo = fixture("prompt-leak");
        let path = add_task(&repo, "demo", "queued");

        let mux = FakeMux::new(vec![]).refusing_to_prompt();
        let report = run_pass(&repo, &mux);

        assert_eq!(
            mux.did("start"),
            ["start demo · implement"],
            "the lane really did start — this is not the `refusing_to_start` path"
        );
        assert_eq!(
            mux.did("stop"),
            ["stop demo · implement"],
            "and it is torn down again rather than left sitting at an empty input box"
        );
        assert!(
            report
                .problems
                .iter()
                .any(|p| p.contains("submission stalled")),
            "problems: {:?}",
            report.problems
        );

        // What the retry is paced by: the launch was banked before the prompt
        // was tried, so `relaunch_backoff` and `MAX_LAUNCHES` still bound a
        // step whose briefing never lands.
        let task = reload(&path);
        assert_eq!(task.front.attempts, 1);
        assert!(task.front.launched_at.is_some());
    }

    #[test]
    fn a_busy_lane_is_never_torn_down() {
        let repo = fixture("busy");
        add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Working)]);
        run_pass(&repo, &mux);

        assert!(mux.did("stop").is_empty());
        assert!(mux.did("start").is_empty());
    }

    #[test]
    fn escalating_records_where_the_task_stopped() {
        let repo = fixture("blocked-from");
        // A task whose lane has been started as often as the budget allows, and
        // whose session is gone again, goes to a person rather than round again.
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.attempts = MAX_LAUNCHES;
        });

        run_pass(&repo, &FakeMux::new(vec![]));

        let task = reload(&path);
        assert_eq!(task.stage(), "blocked");
        assert_eq!(
            task.front.blocked_from.as_deref(),
            Some("implement"),
            "unblocking has to know where to resume, or it restarts the task"
        );
    }

    /// `escalate`'s own road to `blocked` — a lane there itself dying, going
    /// silent, or a spent launch ceiling giving up on starting one — has
    /// nowhere else to send the task any more than `commands::report` does:
    /// acceptance criterion 3. Exercised directly against `escalate` rather
    /// than through the whole reminder loop, which only ever proves the same
    /// call is reached.
    #[test]
    fn an_escalation_from_blocked_itself_lands_on_paused_not_back_on_blocked() {
        let repo = unattended_fixture("blocked-escalates");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let path = add_task_with(&repo, "demo", crate::pipeline::BLOCKED, |f| {
            f.blocked_from = Some("implement".into());
        });

        let mux = FakeMux::new(vec![]);
        let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        let mut task = reload(&path);
        let step = pipeline.step(crate::pipeline::BLOCKED).unwrap();

        dispatcher
            .escalate(
                &mut task,
                pipeline,
                step,
                "its pane went quiet and stayed quiet",
            )
            .unwrap();

        let task = reload(&path);
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(
            task.front.paused_at.as_deref(),
            Some("implement"),
            "paused_at names the step it originally blocked on"
        );
        assert_eq!(
            task.front.blocked_from.as_deref(),
            Some("implement"),
            "blocked_from survives, for a resume to reach the same destination a pass would"
        );
    }

    /// A repo whose runs stop for nobody.
    ///
    /// Set on the config rather than through a lock, because a fixture has no
    /// dispatcher holding one — and `Repo::unattended` falls back to exactly
    /// this when there is no run to ask about.
    fn unattended_fixture(name: &str) -> Repo {
        let mut repo = fixture(name);
        repo.config.unattended.enabled = true;
        repo
    }

    /// The same escalation with nobody to escalate to, on a pipeline that
    /// does not stage `blocked`: the task goes back to the step it stopped on
    /// instead of parking, and nobody is called over.
    ///
    /// This is the whole of what replaced the second, dedicated lane a block
    /// used to be sent to, for a pipeline with no `blocked` step of its own.
    /// The lane that stopped is continued rather than a second one being
    /// sent to work out what the first was doing. A pipeline that *does*
    /// stage `blocked` — the shipped ones, now — gets a lane started there
    /// instead; see `blocked_is_staffed`.
    ///
    /// Driven through the reminder loop, which is the escalation an
    /// unattended run still has: it catches a lane going wrong rather than a
    /// person being needed, so unlike `loop` and the launch ceiling it
    /// keeps its teeth in both modes.
    #[test]
    fn an_unattended_escalation_resumes_the_step_instead_of_parking() {
        let repo = unattended_fixture("unattended-resume");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        let pipelines = unstaffed_builtin();

        // The same mux instance throughout: a real backend's pane persists
        // between passes, and the reminder below has to land on the pane it
        // is measured against next, not a fresh empty one.
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass_with(&repo, &mux, &pipelines);
        // A pass later it is reminded rather than escalated — unattended is no
        // exception, since it is still the same lane and the same pane.
        age_lane(&repo, "demo · implement", Duration::from_secs(15));
        run_pass_with(&repo, &mux, &pipelines);
        assert_eq!(reload(&path).stage(), "implement", "reminded, not parked");
        // Nothing written since that reminder: the dead session a reminder
        // could not reach. One quiet pass is all it takes now — no clock to
        // wait out.
        age_lane(&repo, "demo · implement", Duration::from_secs(700));
        run_pass_with(&repo, &mux, &pipelines);

        let task = reload(&path);
        assert_eq!(task.stage(), "implement", "it never parks");
        assert_eq!(
            task.front.resume.as_deref(),
            Some("implement"),
            "the lane that stopped is continued, not replaced by a cold one"
        );
        assert_eq!(
            task.front.blocked_from, None,
            "it is not waiting on anything any more"
        );
        assert!(mux.did("focus").is_empty(), "nobody was called over");
    }

    /// The blocker is still written down. A resumed lane reads it to find out
    /// what stopped it, and it is the only record that the run went round here
    /// at all — the stage never changes to say so.
    #[test]
    fn an_unattended_escalation_still_records_what_stopped_the_task() {
        let repo = unattended_fixture("unattended-blocker-kept");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);
        age_lane(&repo, "demo · implement", Duration::from_secs(15));
        run_pass(&repo, &mux);
        age_lane(&repo, "demo · implement", Duration::from_secs(700));
        run_pass(&repo, &mux);

        let task = reload(&path);
        let blocker = task.section("## Blocker").unwrap_or_default();
        assert!(
            blocker.contains("produced no output since its last reminder"),
            "{blocker}"
        );
    }

    /// The failure `checkout-line` hit for real: a lane's pane ends on
    /// claude's own usage-limit message, and — before this — was reminded,
    /// escalated to `blocked`, and immediately relaunched into the same
    /// wall, five times running with no work done. Parked here instead: the
    /// task never leaves `review`, and the pane — session and worktree with
    /// it — is left exactly as it is, because the agent resumes its own turn
    /// on its own clock; tearing it down would kill a session that was
    /// already coming back.
    ///
    /// Attended throughout — `fixture()`, not `unattended_fixture()` — on
    /// purpose: a spent quota is not a question for a person the way a dead
    /// launch is, so the park has to survive the very next pass whether or
    /// not anybody is watching, not only when there is nobody to escalate to.
    #[test]
    fn a_lane_ended_on_a_usage_limit_is_parked_rather_than_escalated() {
        let repo = fixture("usage-limit-hold");
        let path = add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            // What a real launch would already have set: the lane this pass
            // finds settled is its first, so `attempts` is 1 and
            // `launched_at` is now the moment it starts, before anything
            // about its own turn is known.
            f.attempts = 1;
            f.launched_at = Some(now_secs());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · review", LaneStatus::Done)]);
        mux.screen.borrow_mut().insert(
            "demo · review".to_string(),
            "  ⚠ Usage limit reached · continuing automatically at 9:50am · esc to cancel"
                .to_string(),
        );

        let before = now_secs();
        let report = run_pass(&repo, &mux);

        let task = reload(&path);
        assert_eq!(task.stage(), "review", "held, not moved to blocked");
        assert!(task.front.usage_limit_hold, "read by the next pass below");
        let until = task
            .front
            .parked_until
            .expect("parked_until must be set by the hold");
        assert!(
            until > before,
            "parked_until ({until}) must be in the future ({before})"
        );
        assert_eq!(
            task.front.attempts, 1,
            "the hold no longer touches `attempts` at all — it moved off that path onto \
             `parked_until`"
        );
        let status_log = task.section("## Status Log").unwrap_or_default();
        assert!(status_log.contains("usage limit"), "{status_log}");
        assert!(status_log.contains("parked until"), "{status_log}");
        assert!(
            task.section("## Blocker").is_none(),
            "not an escalation — nobody is being asked anything"
        );
        assert!(
            mux.did("stop").is_empty(),
            "the pane must be left running — the agent resumes its own turn"
        );
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("usage limit") && a.contains("parked until")),
            "{:?}",
            report.actions
        );

        // The lane is still there — nothing touched it — and this pass finds
        // the task parked before its own clock, whether or not anybody is
        // watching.
        let report = run_pass(&repo, &mux);

        let task = reload(&path);
        assert_eq!(
            task.stage(),
            "review",
            "still parked on the very next pass, attended or not"
        );
        assert!(
            task.section("## Blocker").is_none(),
            "attended must not have escalated it to blocked"
        );
        assert!(
            mux.did("start").is_empty(),
            "nothing relaunched into the wall"
        );
        assert!(
            mux.did("stop").is_empty(),
            "still not torn down on a later pass either"
        );
        assert!(
            report.actions.iter().any(|a| a.contains("parked until")),
            "{:?}",
            report.actions
        );
    }

    /// A usage-limit lane that has resumed its own turn — busy again, and no
    /// limit phrase on its pane any more — is out of the hold. The first
    /// pass that sees it working forgives the whole park through
    /// `launch_landed`, so `parked_at` stops and no growing "still parked"
    /// age sits on the board over a lane that is working.
    #[test]
    fn a_resumed_usage_limit_lane_has_its_parked_at_forgiven() {
        let repo = fixture("usage-limit-resumed");
        let path = add_task_with(&repo, "held", "review", |f| {
            f.usage_limit_hold = true;
            f.quota_retries = 2;
            f.parked_until = Some(now_secs() - 1);
            f.parked_window = crate::quota::Window::FiveHour.key().to_string();
            f.parked_at = Some(now_secs() - 3000);
        });

        // Busy again, and the pane carries no usage-limit phrase now.
        let mux = FakeMux::new(vec![lane(&repo, "held · review", LaneStatus::Working)]);

        run_pass(&repo, &mux);

        let task = reload(&path);
        assert_eq!(
            task.stage(),
            "review",
            "the lane is left to finish its turn"
        );
        assert_eq!(task.front.parked_at, None, "the hold is over");
        assert_eq!(task.front.parked_until, None);
        assert!(!task.front.usage_limit_hold);
        assert_eq!(task.front.quota_retries, 0);
        assert!(mux.did("stop").is_empty(), "the working lane is untouched");
        assert!(mux.did("start").is_empty());
    }

    /// The gap the mockup calls out by name: the old `check_unreported` hold
    /// only ever ran once a lane had *settled*, so a usage-limit surface that
    /// kept ticking — the agent's own "continuing automatically" redrawing
    /// the pane — read as `Working` forever and the hold never fired at all.
    /// `usage_limit_park` is checked ahead of the busy/settled match itself,
    /// so it catches the very same pane while the multiplexer still reports
    /// it `Working`.
    #[test]
    fn a_usage_limit_surface_is_caught_even_while_the_lane_still_reads_working() {
        let repo = fixture("usage-limit-hold-working");
        let path = add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.attempts = 1;
            f.launched_at = Some(now_secs());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · review", LaneStatus::Working)]);
        mux.screen.borrow_mut().insert(
            "demo · review".to_string(),
            "  ⚠ Usage limit reached · continuing automatically at 9:50am · esc to cancel"
                .to_string(),
        );

        let report = run_pass(&repo, &mux);

        let task = reload(&path);
        assert_eq!(task.stage(), "review", "held, not moved to blocked");
        assert!(
            task.front.parked_until.is_some(),
            "the hold must fire on a `Working` pane, not only a settled one"
        );
        assert!(
            mux.did("stop").is_empty(),
            "the pane must be left running even though it reads `Working`"
        );
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("usage limit") && a.contains("parked until")),
            "{:?}",
            report.actions
        );
    }

    /// A launch is a true exit from the hold: once the ceiling has dropped
    /// and the recheck lets the task through, the lane that starts clears
    /// the whole park off the file — deadline, window and `parked_at` — even
    /// though the task never changed step and so `set_stage` never ran.
    #[test]
    fn a_launch_out_of_a_park_clears_parked_at() {
        let mut repo = fixture("quota-launch-clears-parked-at");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 85;
        let path = add_task_with(&repo, "held", "review", |f| {
            f.parked_until = Some(now_secs() - 1);
            f.parked_window = crate::quota::Window::FiveHour.key().to_string();
            f.parked_at = Some(now_secs() - 2400);
        });

        // A reading well under the ceiling — the hold is over.
        let home = claude_quota_home(
            "launch-clears-parked-at",
            20,
            "2099-01-01T00:00:00Z",
            10,
            "2099-01-08T00:00:00Z",
        );
        let mux = FakeMux::new(vec![]);
        with_home(&home, || run_pass(&repo, &mux));
        std::fs::remove_dir_all(&home).ok();

        assert!(
            mux.did("start").iter().any(|c| c.contains("held")),
            "the task is picked up once the ceiling drops: {:?}",
            mux.did("start")
        );
        let task = reload(&path);
        assert_eq!(task.front.parked_at, None, "the hold is over");
        assert_eq!(task.front.parked_until, None);
        assert_eq!(task.front.parked_window, "");
    }

    /// A start that is refused is not an exit from the hold. The whole hold
    /// — all three park fields and the `quota_retries` streak — comes off
    /// only once `mux.start_lane` has actually started the lane, so a
    /// refused start leaves every one of them exactly as it was, whatever
    /// the task's placement looked like going in.
    ///
    /// This shape: a stale recorded worktree, so `ensure_workspace` re-cuts
    /// the placement and persists once before the refused start. That save
    /// must carry the hold through unchanged, not publish any part of a
    /// clear.
    #[test]
    fn a_refused_start_with_a_stale_workspace_keeps_the_whole_park() {
        let mut repo = fixture("quota-refused-start-stale-ws");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 85;
        let until = now_secs() - 1;
        let parked_at = now_secs() - 4000;
        let path = add_task_with(&repo, "held", "review", |f| {
            f.parked_until = Some(until);
            f.parked_window = crate::quota::Window::FiveHour.key().to_string();
            f.parked_at = Some(parked_at);
            f.quota_retries = 4;
            f.worktree_path = Some(repo.queue_dir().join("gone-worktree"));
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.tab_id = Some("w1:t1".into());
        });

        let home = claude_quota_home(
            "refused-start-stale-ws",
            20,
            "2099-01-01T00:00:00Z",
            10,
            "2099-01-08T00:00:00Z",
        );
        let mux = FakeMux::new(vec![]).refusing_to_start();
        let report = with_home(&home, || run_pass(&repo, &mux));
        std::fs::remove_dir_all(&home).ok();

        assert!(
            report.problems.iter().any(|p| p.contains("held")),
            "the launch was attempted and refused: {:?}",
            report.problems
        );
        let task = reload(&path);
        assert_eq!(
            task.front.parked_until,
            Some(until),
            "the park still stands"
        );
        assert_eq!(
            task.front.parked_window,
            crate::quota::Window::FiveHour.key()
        );
        assert_eq!(
            task.front.parked_at,
            Some(parked_at),
            "no lane started, so the age did not end"
        );
        assert_eq!(
            task.front.quota_retries, 4,
            "the retry streak is intact too — the fixup save did not flush a reset"
        );
    }

    /// The other placement shape: a workspace that is already valid, so
    /// `ensure_workspace` persists nothing at all and a refused start
    /// returns without writing the task. The whole hold — park fields and
    /// `quota_retries` — is still on disk because nothing touched it, and
    /// it matches the stale-workspace outcome field for field.
    #[test]
    fn a_refused_start_with_a_valid_workspace_keeps_the_whole_park() {
        let mut repo = fixture("quota-refused-start-valid-ws");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 85;
        let worktree = crate::scratch::root("quota-refused-start-valid-ws-worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let until = now_secs() - 1;
        let parked_at = now_secs() - 4000;
        let path = add_task_with(&repo, "held", "review", |f| {
            f.parked_until = Some(until);
            f.parked_window = crate::quota::Window::FiveHour.key().to_string();
            f.parked_at = Some(parked_at);
            f.quota_retries = 4;
            f.worktree_path = Some(worktree.clone());
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.tab_id = Some("w1:t1".into());
        });

        let home = claude_quota_home(
            "refused-start-valid-ws",
            20,
            "2099-01-01T00:00:00Z",
            10,
            "2099-01-08T00:00:00Z",
        );
        let before = std::fs::read(&path).unwrap();
        let mux = FakeMux::new(vec![]).refusing_to_start();
        let report = with_home(&home, || run_pass(&repo, &mux));
        std::fs::remove_dir_all(&home).ok();
        std::fs::remove_dir_all(&worktree).ok();

        assert!(
            report.problems.iter().any(|p| p.contains("held")),
            "the launch was attempted and refused: {:?}",
            report.problems
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a refused start on a valid placement writes the task file not at all"
        );
        let task = reload(&path);
        assert_eq!(
            task.front.parked_until,
            Some(until),
            "the park still stands"
        );
        assert_eq!(
            task.front.parked_window,
            crate::quota::Window::FiveHour.key()
        );
        assert_eq!(task.front.parked_at, Some(parked_at));
        assert_eq!(
            task.front.quota_retries, 4,
            "same final state as the stale-workspace shape — placement did not decide it"
        );
    }

    /// An unavailable park whose deadline runs out, then a valid reading
    /// that is over the ceiling. The stale recheck count is zeroed in
    /// memory and rides along with the one re-park write, rather than being
    /// persisted on its own with the run-out deadline still standing.
    #[test]
    fn an_expired_unavailable_park_that_reprobes_over_ceiling_saves_once() {
        let mut repo = fixture("quota-unavailable-then-ceiling");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 85;
        let parked_at = now_secs() - 6000;
        let path = add_task_with(&repo, "held", "review", |f| {
            f.parked_until = Some(now_secs() - 1);
            f.parked_window = "unknown".into();
            f.parked_at = Some(parked_at);
            f.quota_retries = 3;
        });

        let home = claude_quota_home(
            "unavailable-then-ceiling",
            90,
            "2099-01-01T14:00:00Z",
            10,
            "2099-01-08T00:00:00Z",
        );
        let mux = FakeMux::new(vec![]);
        let (_, saves) = saves_during(|| with_home(&home, || run_pass(&repo, &mux)));
        std::fs::remove_dir_all(&home).ok();

        // The name of this test, checked. One task in the queue and one write
        // for it: the pass resolved the whole park in a single save rather
        // than banking the recheck-count reset against the run-out deadline
        // first. The final fields below would look identical either way.
        assert_eq!(saves, 1, "the pass wrote the task exactly once");

        let task = reload(&path);
        assert_eq!(task.stage(), "review", "re-parked, not started");
        assert_eq!(
            task.front.quota_retries, 0,
            "the stale recheck count rode along with the re-park"
        );
        assert_eq!(
            task.front.parked_window, "five_hour",
            "the ceiling window replaced 'unknown'"
        );
        assert_eq!(
            task.front.parked_at,
            Some(parked_at),
            "one continuous hold — the age did not restart"
        );
        assert!(task.front.parked_until.unwrap() > now_secs());
        assert!(mux.did("start").iter().all(|c| !c.contains("held")));
    }

    /// An expired deadline is rechecked in memory, not saved cleared and
    /// then decided. When the pass reaches no fresh decision — here because
    /// the task is still waiting on a dependency — the park it arrived with
    /// is left on disk untouched, rather than a bare cleared park being
    /// written that no reader should ever see.
    #[test]
    fn an_expired_park_is_not_saved_cleared_before_the_pass_decides() {
        let repo = fixture("quota-expiry-no-decision");
        add_task(&repo, "first", "implement");
        let expired = now_secs() - 1;
        let parked_at = now_secs() - 1200;
        let path = add_task_with(&repo, "second", crate::pipeline::QUEUED, |f| {
            f.depends_on = vec!["first".into()];
            f.parked_until = Some(expired);
            f.parked_window = crate::quota::Window::FiveHour.key().to_string();
            f.parked_at = Some(parked_at);
        });

        let mux = FakeMux::new(Vec::new());
        run_pass(&repo, &mux);

        let task = reload(&path);
        assert_eq!(
            task.stage(),
            crate::pipeline::QUEUED,
            "still waiting on `first`"
        );
        assert_eq!(
            task.front.parked_until,
            Some(expired),
            "no intermediate cleared park was written — the pass reached no decision"
        );
        assert_eq!(
            task.front.parked_window,
            crate::quota::Window::FiveHour.key(),
            "the window it arrived with is untouched too"
        );
        assert_eq!(
            task.front.parked_at,
            Some(parked_at),
            "and `parked_at` is untouched — the whole park is left consistent"
        );
        assert!(
            mux.did("start").iter().all(|c| !c.contains("second")),
            "{:?}",
            mux.did("start")
        );
    }

    /// `Dispatcher::parked` never writes to the task it is handed. An
    /// expired park is left field-for-field as the file carries it, and the
    /// method only answers `false` so the task falls through. Nothing is
    /// cleared in memory, so an unrelated persist between here and the
    /// pass's real park decision — the successful-probe reset in
    /// `start_lanes`, a workspace re-check in `start_one` — has no
    /// half-cleared park to flush.
    #[test]
    fn parked_leaves_an_expired_park_untouched() {
        let repo = fixture("parked-no-mutation");
        let path = add_task_with(&repo, "demo", "review", |f| {
            f.parked_until = Some(now_secs() - 1);
            f.parked_window = crate::quota::Window::SevenDay.key().to_string();
            f.parked_at = Some(now_secs() - 5000);
            f.quota_retries = 3;
        });
        let mut task = reload(&path);
        let before = task.front.clone();

        let mux = FakeMux::new(vec![]);
        let pipelines = Pipelines::builtin();
        let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        let mut report = Report::default();

        let still_parked = dispatcher.parked(&mut task, &mut report).unwrap();

        assert!(!still_parked, "an expired deadline falls through");
        assert_eq!(task.front.parked_until, before.parked_until);
        assert_eq!(task.front.parked_window, before.parked_window);
        assert_eq!(task.front.parked_at, before.parked_at);
        assert_eq!(task.front.quota_retries, before.quota_retries);
    }

    /// A task parked while it was still sitting on `queued` stays parked,
    /// and costs the pass nothing while it waits.
    ///
    /// `route_reserved_stage` turns a `queued` task straight into a candidate
    /// and sends the loop on to the next task, so the park gate in
    /// `collect_candidates`'s own body never saw it — the park was not read
    /// at all on the one stage every task passes through. Two things came of
    /// that. A park the pass could not re-derive was ignored outright and the
    /// task started anyway, which is what this asserts first. And a park it
    /// could re-derive was re-probed, re-written and re-logged every pass: at
    /// the default ten second interval a seven-day-window park appended tens
    /// of thousands of `## Status Log` lines to one task file.
    #[test]
    fn a_task_parked_on_queued_is_not_re_probed_or_re_logged_every_pass() {
        let mut repo = fixture("quota-parked-on-queued");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 70;
        let until = now_secs() + 3600;
        let path = add_task_with(&repo, "demo", crate::pipeline::QUEUED, |f| {
            f.parked_until = Some(until);
            f.parked_window = crate::quota::Window::SevenDay.key().to_string();
        });

        let mux = FakeMux::new(Vec::new());
        let report = run_pass(&repo, &mux);

        let task = reload(&path);
        assert_eq!(
            task.stage(),
            crate::pipeline::QUEUED,
            "the park holds it where it is"
        );
        assert_eq!(
            task.front.parked_until,
            Some(until),
            "the deadline it arrived with, not one this pass wrote again"
        );
        assert!(
            mux.did("start").is_empty(),
            "a parked task must not take a worker slot"
        );
        assert!(
            report.actions.iter().any(|a| a.contains("parked until")),
            "the pass says why nothing moved: {:?}",
            report.actions
        );

        let before = task.section("## Status Log").unwrap_or_default();
        for _ in 0..3 {
            run_pass(&repo, &mux);
        }
        assert_eq!(
            reload(&path).section("## Status Log").unwrap_or_default(),
            before,
            "three more passes must not have written a single further line"
        );
    }

    #[test]
    fn unavailable_quota_holds_without_launching_and_backs_off_across_passes() {
        let mut repo = fixture("quota-unavailable");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 70;
        let path = add_task(&repo, "unknown", "review");
        let other = add_task(&repo, "local", "queued");
        let home = crate::scratch::root("quota-unavailable-home");
        std::fs::create_dir_all(&home).unwrap();
        let mux = FakeMux::new(vec![]);
        with_home(&home, || {
            run_pass(&repo, &mux);
            let mut task = reload(&path);
            assert_eq!(task.front.quota_retries, 1);
            assert_eq!(task.front.parked_window, "unknown");
            assert!(task.front.parked_until.unwrap() >= now_secs() + 59);
            let first_parked_at = task.front.parked_at.expect("stamped on the first hold");
            assert!(mux.did("start").iter().all(|s| !s.contains("unknown")));
            assert!(mux.did("start").iter().any(|s| s.contains("local")));
            // Backdate the start by an hour before the second pass. Both
            // passes run inside one second, so a start left at `now_secs()`
            // would read the same whether it was kept or re-stamped.
            let began = first_parked_at - 3600;
            task.front.parked_at = Some(began);
            task.front.parked_until = Some(now_secs() - 1);
            task.save().unwrap();
            run_pass(&repo, &mux);
            let mut task = reload(&path);
            assert_eq!(task.front.quota_retries, 2);
            assert!(task.front.parked_until.unwrap() >= now_secs() + 119);
            assert_eq!(
                task.front.parked_at,
                Some(began),
                "an unavailable-reading re-hold is the same continuous park"
            );
            assert_eq!(task.body.matches("quota unavailable:").count(), 1);
            // The agent has refreshed its cache: resume from the same stage.
            std::fs::write(
                home.join(".claude.json"),
                format!(
                    r#"{{"cachedUsageUtilization":{{
                "fetchedAtMs":{},"utilization":{{
                "five_hour":{{"utilization":30,"resets_at":"2099-01-01T00:00:00Z"}},
                "seven_day":{{"utilization":20,"resets_at":"2099-01-08T00:00:00Z"}}
            }}}}}}"#,
                    chrono::Utc::now().timestamp_millis()
                ),
            )
            .unwrap();
            task.front.parked_until = Some(now_secs() - 1);
            task.save().unwrap();
            run_pass(&repo, &mux);
            assert!(mux.did("start").iter().any(|s| s.contains("unknown")));
            let task = reload(&path);
            assert_eq!(task.front.quota_retries, 0);
            assert_eq!(task.front.parked_at, None, "a launch is a true exit");
        });
        assert_eq!(reload(&other).front.parked_until, None);
    }

    #[test]
    fn unavailable_quota_dry_run_does_not_persist_a_hold() {
        let mut repo = fixture("quota-unavailable-dry");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 70;
        let path = add_task(&repo, "unknown", "review");
        let before = std::fs::read(&path).unwrap();
        let home = crate::scratch::root("quota-unavailable-dry-home");
        std::fs::create_dir_all(&home).unwrap();
        let mux = FakeMux::new(vec![]);
        with_home(&home, || {
            let pipelines = Pipelines::builtin();
            let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, true);
            let report = dispatcher.pass().unwrap();
            assert!(
                report
                    .actions
                    .iter()
                    .any(|a| a.contains("would park unknown"))
            );
        });
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(mux.did("start").is_empty());
    }

    #[test]
    fn usage_limit_rechecks_back_off_without_duplicate_log_lines() {
        let repo = fixture("quota-held-rechecks");
        let path = add_task(&repo, "held", "review");
        let mux = FakeMux::new(vec![lane(&repo, "held · review", LaneStatus::Working)]);
        mux.screen
            .borrow_mut()
            .insert("held · review".into(), "Usage limit reached".into());
        let home = crate::scratch::root("quota-held-rechecks-home");
        std::fs::create_dir_all(&home).unwrap();
        with_home(&home, || {
            // A synthetic start, an hour behind the first pass, so a
            // re-stamp is visible: `now_secs()` on three passes inside the
            // same second would be indistinguishable from a fixed value.
            let began = now_secs() - 3600;
            for expected in 1..=3 {
                run_pass(&repo, &mux);
                let mut task = reload(&path);
                assert_eq!(task.front.quota_retries, expected);
                assert!(
                    task.front.parked_until.unwrap()
                        >= now_secs() + quota_backoff(expected - 1) - 1
                );
                assert_eq!(task.body.matches("hit its usage limit").count(), 1);
                // The deadline moves on every recheck; the age does not.
                // Passes two and three each walk in over an expired deadline
                // and must leave the backdated start exactly as it stands.
                if expected == 1 {
                    assert!(
                        task.front.parked_at.is_some(),
                        "the first hold stamps the start of the park"
                    );
                } else {
                    assert_eq!(
                        task.front.parked_at,
                        Some(began),
                        "pass {expected} re-parked the same hold — parked_at must not move"
                    );
                }
                task.front.parked_at = Some(began);
                task.front.parked_until = Some(now_secs() - 1);
                task.save().unwrap();
            }
            let mut task = reload(&path);
            task.set_stage("implement", None);
            assert_eq!(task.front.quota_retries, 0);
            assert!(!task.front.usage_limit_hold);
            assert_eq!(
                task.front.parked_at, None,
                "leaving the step is a true exit from the hold"
            );
        });
        assert!(mux.did("start").is_empty());
        assert!(mux.did("stop").is_empty());
        assert_eq!(quota_backoff(u32::MAX), 3600);
    }

    #[test]
    fn usage_limit_hold_uses_an_exhausted_weekly_window() {
        let repo = fixture("quota-weekly-hold");
        let path = add_task(&repo, "weekly", "review");
        let home = claude_quota_home(
            "weekly",
            20,
            "2099-01-01T00:00:00Z",
            100,
            "2099-01-08T00:00:00Z",
        );
        let mux = FakeMux::new(vec![lane(&repo, "weekly · review", LaneStatus::Working)]);
        mux.screen
            .borrow_mut()
            .insert("weekly · review".into(), "Usage limit reached".into());
        with_home(&home, || run_pass(&repo, &mux));
        let task = reload(&path);
        assert_eq!(task.front.parked_window, "seven_day");
        assert_eq!(
            task.front.parked_until,
            Some(
                chrono::DateTime::parse_from_rfc3339("2099-01-08T00:00:00Z")
                    .unwrap()
                    .timestamp()
            )
        );
        assert!(mux.did("stop").is_empty());
    }

    /// A scratch home carrying the supported flat Claude cache layout.
    fn claude_quota_home(
        name: &str,
        five_hour_pct: u8,
        five_hour_resets: &str,
        seven_day_pct: u8,
        seven_day_resets: &str,
    ) -> PathBuf {
        let root = crate::scratch::root(&format!("dispatch-quota-{name}"));
        std::fs::create_dir_all(&root).unwrap();
        let body = format!(
            r#"{{"cachedUsageUtilization": {{
                "fetchedAtMs": {},
                "five_hour": {{"utilization": {five_hour_pct}, "resets_at": "{five_hour_resets}"}},
                "seven_day": {{"utilization": {seven_day_pct}, "resets_at": "{seven_day_resets}"}}
            }}}}"#,
            chrono::Utc::now().timestamp_millis(),
        );
        std::fs::write(root.join(".claude.json"), body).unwrap();
        root
    }

    /// The pre-launch half of the feature: a quota probe already at or above
    /// its `quota_ceiling` refuses to start a new lane of that profile at
    /// all. The task never gets a lane in the first place — `parked_until`
    /// is written straight from the probe's own `resets_at` — and a task on
    /// a step naming a different profile (`implement`, staffed by `pi`,
    /// which carries no quota probe at all) is staffed in the same pass,
    /// unaffected.
    // covers: agents.<profile>.quota_ceiling — the pre-launch gate on a new lane
    #[test]
    fn a_quota_ceiling_over_its_limit_parks_a_new_lane_without_starting_one() {
        let mut repo = fixture("quota-ceiling");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 85;
        let parked_path = add_task(&repo, "over-ceiling", "review");
        let staffed_path = add_task(&repo, "different-profile", "queued");

        let home = claude_quota_home(
            "over-ceiling",
            88,
            "2099-01-01T14:00:00Z",
            10,
            "2099-01-08T00:00:00Z",
        );
        let mux = FakeMux::new(vec![]);
        let report = with_home(&home, || run_pass(&repo, &mux));
        std::fs::remove_dir_all(&home).ok();

        let task = reload(&parked_path);
        assert_eq!(task.stage(), "review", "parked, not started");
        let until = task
            .front
            .parked_until
            .expect("parked_until must be set by the ceiling");
        assert_eq!(
            until,
            chrono::DateTime::parse_from_rfc3339("2099-01-01T14:00:00Z")
                .unwrap()
                .timestamp(),
            "parked_until must come from the probe's own resets_at"
        );
        assert!(
            mux.did("start").iter().all(|c| !c.contains("over-ceiling")),
            "{:?}",
            mux.did("start")
        );
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("parked until") && a.contains("85")),
            "{:?}",
            report.actions
        );

        // The other task, on a step naming a different profile, is staffed
        // in the same pass — the ceiling on `claude` never touches `pi`.
        assert!(
            mux.did("start")
                .iter()
                .any(|c| c.contains("different-profile")),
            "{:?}",
            mux.did("start")
        );
        assert_eq!(reload(&staffed_path).front.parked_until, None);
    }

    /// `parked_at` is stamped once, when the ceiling first parks the task,
    /// and left exactly as it was when a later pass finds the deadline
    /// expired and re-parks against the same still-exhausted reading. The
    /// deadline moves; the start of the hold does not.
    #[test]
    fn a_repark_keeps_one_fixed_parked_at() {
        let mut repo = fixture("quota-repark-parked-at");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 85;
        let path = add_task(&repo, "held", "review");

        let home = claude_quota_home(
            "repark-parked-at",
            88,
            "2099-01-01T14:00:00Z",
            10,
            "2099-01-08T00:00:00Z",
        );
        let mux = FakeMux::new(vec![]);
        let before = now_secs();
        with_home(&home, || {
            run_pass(&repo, &mux);
            let first = reload(&path)
                .front
                .parked_at
                .expect("stamped on the first park");
            assert!(
                first >= before && first <= now_secs() + 1,
                "parked_at is the moment of the first park: {first}"
            );

            // Force the deadline into the past, the way its own clock running
            // out would, and pass again against the same 88% reading.
            // The start is backdated an hour at the same time: both passes
            // run inside one second, so only a synthetic start tells a fixed
            // `parked_at` apart from one re-stamped on every pass.
            let began = first - 3600;
            let mut task = reload(&path);
            task.front.parked_at = Some(began);
            task.front.parked_until = Some(now_secs() - 1);
            task.save().unwrap();
            run_pass(&repo, &mux);

            let after = reload(&path);
            assert_eq!(
                after.front.parked_at,
                Some(began),
                "a re-park is the same hold — parked_at must not move with the deadline"
            );
            assert!(
                after.front.parked_until.unwrap() > now_secs(),
                "the deadline itself was refreshed"
            );
        });
        std::fs::remove_dir_all(&home).ok();
        assert!(mux.did("start").iter().all(|c| !c.contains("held")));
    }

    /// A pipeline whose `review` step is staffed by `codex` rather than
    /// `claude` — the ceiling gate's own mechanism does not change per kind,
    /// so this is what lets the test below exercise codex's row through it
    /// rather than asserting on `quota::read` directly.
    fn codex_review_pipeline() -> Pipelines {
        let yaml = "steps:\n  \
             - id: implement\n    agent: pi\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: review\n  \
             - id: review\n    agent: codex\n    prompt: implementer\n    model: test-model\n\
             \x20   on_pass: handover\n  \
             - id: handover\n    end: true\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);
        pipelines
    }

    /// A scratch home holding a codex rollout shaped like a real
    /// ChatGPT-authed one — `used_percent` a float, `resets_at` epoch
    /// seconds — readable by [`crate::quota::read`] the way
    /// [`claude_quota_home`] is readable for claude. Written under the
    /// managed lane home `crate::quota::read`'s codex row actually reads —
    /// `<home>/.local/state/spoolway/codex/<session>/sessions/**` — never
    /// under `~/.codex`, which that row never looks at.
    fn codex_quota_home(
        name: &str,
        primary_pct: f64,
        primary_resets_at: i64,
        secondary_pct: f64,
        secondary_resets_at: i64,
    ) -> PathBuf {
        let root = crate::scratch::root(&format!("dispatch-quota-codex-{name}"));
        let dir = root.join(format!(
            ".local/state/spoolway/codex/{name}/sessions/2026/09/05"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let line = format!(
            r#"{{"timestamp":"{}","type":"event_msg","payload":{{"type":"token_count","info":{{}},"rate_limits":{{"limit_id":"codex","limit_name":null,"primary":{{"used_percent":{primary_pct},"window_minutes":300,"resets_at":{primary_resets_at}}},"secondary":{{"used_percent":{secondary_pct},"window_minutes":10080,"resets_at":{secondary_resets_at}}},"credits":null,"individual_limit":null,"spend_control_reached":null,"plan_type":"plus","rate_limit_reached_type":null}}}}}}"#,
            chrono::Utc::now().to_rfc3339(),
        );
        std::fs::write(dir.join("rollout-fixture.jsonl"), line).unwrap();
        root
    }

    /// codex's own row goes through the exact same pre-launch ceiling gate
    /// claude's does — see
    /// [`a_quota_ceiling_over_its_limit_parks_a_new_lane_without_starting_one`]
    /// — proven here against a fixture rollout shaped like a real
    /// ChatGPT-authed one rather than claude's single cache file, since that
    /// reading is what this task adds.
    // covers: agents.codex.quota_ceiling — the pre-launch gate on a new lane, off codex's own probe
    #[test]
    fn a_codex_quota_ceiling_over_its_limit_parks_a_new_lane_without_starting_one() {
        let mut repo = fixture("codex-quota-ceiling");
        repo.config.agents.get_mut("codex").unwrap().quota_ceiling = 85;
        let pipelines = codex_review_pipeline();
        let parked_path = add_task(&repo, "over-ceiling", "review");
        let staffed_path = add_task(&repo, "different-profile", "queued");

        let primary_resets = now_secs() + 3600;
        let secondary_resets = now_secs() + 6 * 86_400;
        let home = codex_quota_home("over-ceiling", 88.0, primary_resets, 10.0, secondary_resets);
        let mux = FakeMux::new(vec![]);
        let report = with_home(&home, || run_pass_with(&repo, &mux, &pipelines));
        std::fs::remove_dir_all(&home).ok();

        let task = reload(&parked_path);
        assert_eq!(task.stage(), "review", "parked, not started");
        let until = task
            .front
            .parked_until
            .expect("parked_until must be set by the ceiling");
        assert_eq!(
            until, primary_resets,
            "parked_until must come from the probe's own resets_at"
        );
        assert!(
            mux.did("start").iter().all(|c| !c.contains("over-ceiling")),
            "{:?}",
            mux.did("start")
        );
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("parked until") && a.contains("85")),
            "{:?}",
            report.actions
        );

        // The other task, on a step naming a different profile, is staffed
        // in the same pass — the ceiling on `codex` never touches `pi`.
        assert!(
            mux.did("start")
                .iter()
                .any(|c| c.contains("different-profile")),
            "{:?}",
            mux.did("start")
        );
        assert_eq!(reload(&staffed_path).front.parked_until, None);
    }

    /// The other side of the gate: a codex reading safely under its own
    /// ceiling staffs the lane exactly as if there were no probe at all —
    /// the above-ceiling test only proves a *different* profile (`pi`) is
    /// unaffected by codex's ceiling, never that codex's own lane starts
    /// when its own reading is fine.
    // covers: agents.codex.quota_ceiling — a reading under the ceiling staffs the same profile
    #[test]
    fn a_codex_quota_reading_under_its_ceiling_staffs_the_lane() {
        let mut repo = fixture("codex-quota-under-ceiling");
        repo.config.agents.get_mut("codex").unwrap().quota_ceiling = 85;
        let pipelines = codex_review_pipeline();
        let path = add_task(&repo, "under-ceiling", "review");

        let primary_resets = now_secs() + 3600;
        let secondary_resets = now_secs() + 6 * 86_400;
        let home = codex_quota_home(
            "under-ceiling",
            40.0,
            primary_resets,
            10.0,
            secondary_resets,
        );
        let mux = FakeMux::new(vec![]);
        with_home(&home, || run_pass_with(&repo, &mux, &pipelines));
        std::fs::remove_dir_all(&home).ok();

        assert!(
            mux.did("start").iter().any(|c| c.contains("under-ceiling")),
            "a reading safely under the ceiling must not hold the lane back: {:?}",
            mux.did("start")
        );
        assert_eq!(reload(&path).front.parked_until, None);
    }

    /// The park is written on the task file, not held anywhere in the
    /// dispatcher's own memory — so a dispatcher that never ran during the
    /// wait, or was stopped and started again from cold, still honours it,
    /// without ever taking a quota reading: the scratch home behind this
    /// pass holds no `~/.claude.json` at all, and a probe read against it
    /// would come back `Unreadable`. Once the clock has actually passed the
    /// very next pass staffs the task, again with no reading taken.
    // covers: parked_until — survives a restart, and needs no fresh quota reading to honour
    #[test]
    fn a_future_parked_until_survives_a_cold_restart_with_no_quota_reading_taken() {
        let repo = fixture("quota-cold-restart");
        let until = now_secs() + 3600;
        let path = add_task_with(&repo, "demo", "review", |f| {
            f.parked_until = Some(until);
        });

        let home = crate::scratch::root("dispatch-quota-cold-restart-empty-home");
        std::fs::create_dir_all(&home).unwrap();
        let mux = FakeMux::new(vec![]);
        let report = with_home(&home, || run_pass(&repo, &mux));

        let task = reload(&path);
        assert_eq!(task.front.parked_until, Some(until), "still parked");
        assert!(mux.did("start").is_empty(), "nothing started while parked");
        assert!(
            report.actions.iter().any(|a| a.contains("parked until")),
            "{:?}",
            report.actions
        );

        // The same effect as the clock actually running out, written by
        // hand rather than waited for.
        let mut task = reload(&path);
        task.front.parked_until = Some(now_secs() - 1);
        task.save().unwrap();

        let mux = FakeMux::new(vec![]);
        with_home(&home, || run_pass(&repo, &mux));
        std::fs::remove_dir_all(&home).ok();

        let task = reload(&path);
        assert_eq!(task.front.parked_until, None, "cleared once past");
        assert!(
            !mux.did("start").is_empty(),
            "picked up once the clock passed"
        );
    }

    /// `--dry-run` reports the ceiling park it would make, and writes
    /// nothing — `parked_until` must still be unset afterwards, the same
    /// guarantee every other dry-run path in this module gives.
    #[test]
    fn a_dry_run_reports_the_ceiling_park_it_would_make_and_writes_nothing() {
        let mut repo = fixture("quota-ceiling-dry");
        repo.config.agents.get_mut("claude").unwrap().quota_ceiling = 85;
        let path = add_task(&repo, "demo", "review");

        let home = claude_quota_home(
            "ceiling-dry",
            88,
            "2099-01-01T14:00:00Z",
            10,
            "2099-01-08T00:00:00Z",
        );
        let mux = FakeMux::new(vec![]);
        let report = with_home(&home, || {
            Dispatcher::new(&repo, &Pipelines::builtin(), &mux, true)
                .pass()
                .unwrap()
        });
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(
            reload(&path).front.parked_until,
            None,
            "a dry run writes nothing"
        );
        assert!(mux.did("start").is_empty());
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.starts_with("would park") && a.contains("parked until")),
            "{:?}",
            report.actions
        );
    }

    /// `--dry-run` against a task whose own `parked_until` has already
    /// passed reports it as picked up without clearing the field — the same
    /// "reports what a real pass would do, writes nothing" guarantee, on the
    /// other side of the clock.
    #[test]
    fn a_dry_run_against_an_expired_park_writes_nothing_either() {
        let repo = fixture("quota-park-expiry-dry");
        let expired = now_secs() - 1;
        let path = add_task_with(&repo, "demo", "review", |f| {
            f.parked_until = Some(expired);
        });

        let mux = FakeMux::new(vec![]);
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, true)
            .pass()
            .unwrap();

        assert_eq!(
            reload(&path).front.parked_until,
            Some(expired),
            "a dry run must not clear parked_until even once it is in the past"
        );
        assert!(mux.did("start").is_empty(), "a dry run starts nothing");
    }

    /// Attended, a lane that dies at launch parks the task, on the grounds that
    /// more launches do not fix a broken agent. Unattended there is nobody to
    /// park it in front of — so it is tried again, but not on the very next
    /// pass, which would be the busy loop the ceiling exists to prevent.
    #[test]
    fn an_unattended_dead_launch_waits_instead_of_parking_or_spinning() {
        let repo = unattended_fixture("unattended-backoff");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.attempts = MAX_LAUNCHES;
            // Launched a moment ago, so the wait is still running.
            f.launched_at = Some(now_secs());
        });

        let mux = FakeMux::new(vec![]);
        let report = Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .pass()
            .unwrap();

        let task = reload(&path);
        assert_eq!(task.stage(), "implement", "not parked");
        assert_eq!(
            task.front.attempts, MAX_LAUNCHES,
            "the counter has to survive, or every wait is the first wait"
        );
        assert!(
            report.actions.iter().any(|a| a.contains("trying again in")),
            "{:?}",
            report.actions
        );
        assert!(mux.did("start").is_empty(), "nothing was launched");
    }

    /// And once the wait is up, it really does try again.
    #[test]
    fn an_unattended_dead_launch_is_retried_once_its_wait_is_up() {
        let repo = unattended_fixture("unattended-backoff-elapsed");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
            f.attempts = MAX_LAUNCHES;
            f.launched_at = Some(now_secs() - 86_400);
        });

        let mux = FakeMux::new(vec![]);
        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .pass()
            .unwrap();

        assert!(!mux.did("start").is_empty(), "the retry never happened");
        assert!(
            reload(&path).front.attempts > MAX_LAUNCHES,
            "each try has to make the next wait longer"
        );
    }

    /// The wait grows, and stops growing. An hour is the point past which a
    /// wedged install costs nothing; a run left going over a weekend still has
    /// to recover on its own rather than settle into a wait measured in days.
    #[test]
    fn the_relaunch_wait_doubles_and_then_holds() {
        let interval = Duration::from_secs(10);
        assert_eq!(relaunch_backoff(interval, MAX_LAUNCHES), interval);
        assert_eq!(relaunch_backoff(interval, MAX_LAUNCHES + 1), interval * 2);
        assert_eq!(relaunch_backoff(interval, MAX_LAUNCHES + 3), interval * 8);
        assert_eq!(
            relaunch_backoff(interval, MAX_LAUNCHES + 40),
            Duration::from_secs(3600)
        );
        // Never shorter than a pass, whatever the arithmetic says.
        assert_eq!(relaunch_backoff(interval, 0), interval);
    }

    #[test]
    fn a_closeout_workspace_is_closed_and_never_removed_as_a_worktree() {
        let repo = fixture("closeout-cleanup");
        add_task_with(&repo, "plan-closeout", "done", |f| {
            f.borrowed = true;
            f.workspace_id = Some("w9".into());
            f.tab_id = Some("w9:t2".into());
            f.pane_id = Some("w9:p2".into());
            // What a borrowed workspace points at: the checkout itself.
            f.worktree_path = Some(a_checkout("dispatch-closeout-cleanup"));
        });

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        // Only the tab spoolway added: the workspace was already open on this
        // checkout and belongs to whoever opened it.
        assert_eq!(mux.did("close_tab"), ["close_tab w9:t2"]);
        assert!(
            mux.did("close_workspace").is_empty(),
            "the workspace was not ours"
        );
        assert!(
            mux.did("remove_workspace").is_empty(),
            "removing the worktree would be removing the person's own checkout"
        );
    }

    #[test]
    fn a_closeout_that_opened_its_own_workspace_takes_it_with_it() {
        let repo = fixture("closeout-own-workspace");
        add_task_with(&repo, "plan-closeout", "done", |f| {
            f.borrowed = true;
            f.workspace_id = Some("w9".into());
            f.tab_id = Some("w9:t1".into());
            f.pane_id = Some("w9:p1".into());
            f.worktree_path = Some(a_checkout("dispatch-closeout-own-workspace"));
        });

        // Nobody else had this checkout open, so spoolway made the workspace and
        // this is its only tab — which the multiplexer will not close.
        let mux = FakeMux::new(vec![]).with_last_tab("w9:t1");
        run_pass(&repo, &mux);

        assert_eq!(mux.did("close_workspace"), ["close_workspace w9"]);
        assert!(
            mux.did("remove_workspace").is_empty(),
            "there is no worktree under it — that is the person's own checkout"
        );
    }

    /// A task's row is named once, at creation, and never relabelled as it
    /// moves through its steps — under `split` it is fixed `spoolway/<task>`;
    /// under `grouped` there is no row of the task's own to rename at all.
    /// What tells one step from the next is the lane's own name, on its pane.
    #[test]
    fn a_tasks_row_is_never_relabelled_as_it_moves_steps() {
        let repo = fixture("labels");
        // A task already past its first step, in the workspace cut for it then.
        add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(a_checkout("dispatch-labels"));
        });

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert!(mux.did("tab").is_empty(), "{:?}", mux.calls());
        assert!(mux.did("workspace").is_empty(), "{:?}", mux.calls());
        assert_eq!(mux.did("pane"), ["pane demo · review"]);
    }

    #[test]
    fn another_projects_lane_is_not_this_dispatchers_to_touch() {
        let repo = fixture("someone-elses");
        add_task(&repo, "demo", "implement");

        // Same step, same task id, another checkout: two projects sharing a
        // multiplexer collide on names alone, and tearing this one down would
        // kill a pane the other dispatcher is mid-pass on.
        let elsewhere = crate::scratch::root("dispatch-another-project");
        let mux = FakeMux::new(vec![lane_at(
            &elsewhere,
            "demo · implement",
            LaneStatus::Done,
        )]);
        run_pass(&repo, &mux);

        assert!(
            mux.did("stop").is_empty(),
            "another project's lane was torn down"
        );
    }

    /// A lane that ends a turn without reporting has asked something — "I can
    /// do this three ways, which?" — and its pane is where the answer goes.
    /// Tearing it down destroys the question and starts the step over, which is
    /// the failure this whole branch exists to stop.
    #[test]
    fn a_lane_that_ends_its_turn_keeps_its_pane_and_waits() {
        let repo = fixture("waiting");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);

        assert!(mux.calls().is_empty(), "did: {:?}", mux.calls());

        let task = reload(&path);
        assert_eq!(task.stage(), "implement", "the step is not restarted");
        assert_eq!(
            task.front.attempts, 0,
            "nothing failed, so nothing was spent"
        );

        // Still waiting a pass later, and still nobody's pane has been closed.
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);
        assert!(mux.did("stop").is_empty());
        assert_eq!(reload(&path).stage(), "implement");
    }

    /// A pane nothing tears down is a pane nobody looks at, unless something
    /// says it is there — a row on the board naming the pane, marked once
    /// however many passes it waits.
    ///
    /// And unmarked again the moment the lane is working, because then it is
    /// holding no question: either somebody answered it, or the lane was only
    /// quiet between turns for long enough to look settled. A mark that stayed
    /// put had the board reading `● paused` at a pane with an agent
    /// visibly mid-turn in it, for the rest of that lane's life.
    ///
    /// The mark is earned by `dispatch.lane_quiet` of silence, not by one
    /// settled reading — so the lane is aged past it here, the same way a lane
    /// earning a reminder is.
    #[test]
    fn a_waiting_lane_is_announced_once_and_unmarked_when_it_works_again() {
        let repo = fixture("waiting-announced");
        add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        // The first pass adopts the lane; there is no record to age before it,
        // and a lane that has only just settled is holding nothing yet.
        run_pass(
            &repo,
            &FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]),
        );
        assert!(lanes_awaiting_a_person(&repo).is_empty());

        // One `lane_quiet` of silence later it is. Aged passes are counted
        // against `MAX_REMINDERS`, so this spends as few of them as the point
        // needs — a fourth would escalate the task off this step entirely and
        // take the lane being asserted on with it.
        age_lane(&repo, "demo · implement", Duration::from_secs(15));
        run_pass(
            &repo,
            &FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]),
        );
        assert!(lanes_awaiting_a_person(&repo).contains("demo · implement"));

        // Answered: the lane is working again, and the board stops offering
        // its pane as somewhere to go and reply.
        run_pass(
            &repo,
            &FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Working)]),
        );
        assert!(lanes_awaiting_a_person(&repo).is_empty());

        // Settled again, and quiet for less than `lane_quiet`: the mark does
        // not come straight back either. A lane between turns looks exactly
        // like this, and it is not asking anybody anything.
        //
        // The test stops here rather than aging once more to watch the mark
        // return. By this point the lane has spent a reminder and never
        // reported, so further passes are the escalation path's to decide, and
        // asserting on a lane that may be on its way to `blocked` would be
        // testing two things at once. That the mark returns is the same line
        // of code as the first time it was set.
        run_pass(
            &repo,
            &FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]),
        );
        assert!(
            lanes_awaiting_a_person(&repo).is_empty(),
            "a lane that only just stopped working is not asking anything yet"
        );
    }

    #[test]
    fn a_lane_waiting_on_a_person_is_left_completely_alone() {
        let repo = fixture("gate");
        let path = add_task_with(&repo, "demo", "document", |f| {
            f.pane_id = Some("w1:p1".into());
        });

        // `done` here means the lane asked its question and ended its turn.
        // Treating that as a silent exit would tear down the conversation the
        // human is about to answer.
        //
        // One mux across every pass below, not a fresh one per call: a real
        // pane's screen persists between passes, and `note_progress` reads it
        // to tell a lane that has written from one that has not. A fresh
        // `FakeMux` forgets the reminder it just wrote, so the third pass
        // would see the pane go back to empty and mistake that for fresh
        // output — see `FakeMux::clear_calls`.
        let mux = FakeMux::new(vec![lane(&repo, "demo · document", LaneStatus::Done)]);
        run_pass(&repo, &mux);

        assert!(mux.calls().is_empty(), "did: {:?}", mux.calls());
        let task = reload(&path);
        assert_eq!(task.stage(), "document");
        assert_eq!(task.front.attempts, 0);

        // A day later it is reminded rather than blocked outright — an
        // ungated step that ended its turn without reporting still gets the
        // same one round of patience any settled lane does.
        age_lane(&repo, "demo · document", Duration::from_secs(86_400));
        mux.clear_calls();
        run_pass(&repo, &mux);
        assert_eq!(reload(&path).stage(), "document", "reminded, not blocked");

        // Nothing follows the reminder, and there is no clock left to wait
        // out: the very next pass blocks it.
        mux.clear_calls();
        run_pass(&repo, &mux);
        assert_eq!(reload(&path).stage(), crate::pipeline::BLOCKED);
    }

    /// A gate used to exempt its lane from the silence clock, because the lane
    /// was the thing holding the question and a person answering it tomorrow
    /// morning was the feature. Nothing holds a question any more: the lane
    /// works, reports and ends, and the waiting happens after it on `paused`. So
    /// a lane still sitting on a gated step is a lane that never reported, and
    /// it is reminded and then blocked exactly like one on any other step.
    #[test]
    fn a_gated_lane_that_never_reported_is_clocked_like_any_other() {
        let repo = fixture("gate-forever");
        let pipelines = gated_pipeline();
        let path = add_task_with(&repo, "demo", "release", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        // One mux across every pass below, not a fresh one per call — see the
        // comment on the same pattern in
        // `a_lane_waiting_on_a_person_is_left_completely_alone`.
        let mux = FakeMux::new(vec![lane(&repo, "demo · release", LaneStatus::Done)]);
        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        // Freshly settled is not yet a fault — the report may have landed while
        // the pass was reading.
        assert_eq!(reload(&path).stage(), "release");

        age_lane(&repo, "demo · release", Duration::from_secs(86_400));
        mux.clear_calls();
        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        assert_eq!(reload(&path).stage(), "release", "reminded, not blocked");

        // Nothing follows the reminder, and there is no clock left to wait
        // out: the very next pass blocks it.
        mux.clear_calls();
        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        assert_eq!(reload(&path).stage(), crate::pipeline::BLOCKED);
    }

    /// The shape a project reaching for `gate:` writes: one step that changes
    /// something no pull request would show anybody first. Nothing shipped gates,
    /// so every gate test builds its own.
    fn gated_pipeline() -> Pipelines {
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse(
                "default",
                "steps:\n\
                 \x20 - id: release\n    agent: pi\n    prompt: implementer\n\
                 \x20   model: test-model\n    gate: true\n    on_pass: done\n",
            )
            .expect("hand-built pipeline"),
        );
        pipelines
    }

    /// The lane is told a person opens this pane, but that is all it is told:
    /// spoolway still holds the pass on its own, in `commands::report`, and
    /// nothing here asks the lane to hold it too.
    #[test]
    fn a_gated_step_tells_its_lane_a_person_opens_this_pane_and_parks_its_pass() {
        let repo = fixture("checkpoint-holds");
        let pipelines = gated_pipeline();
        let step = pipelines.get("default").unwrap().step("release").unwrap();
        let path = add_task_with(&repo, "demo", "release", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        let task = reload(&path);

        // The gate paragraph is in there, but not the paragraph the old
        // behaviour sent: nothing asks the lane to hold, pause, wait for, or
        // approve its own pass — that stayed `commands::report`'s alone to
        // decide.
        let pipeline = pipelines.get("default").unwrap();
        let sent = format!(
            "{}{}",
            crate::compose::system_prompt(&repo, &task, pipeline, step, "[prompt]").unwrap(),
            crate::compose::opening_prompt(&repo, &task, pipeline, step),
        );
        assert!(sent.contains("This step is gated"), "got: {sent}");
        assert!(sent.contains("a person opens this pane"), "got: {sent}");
        for word in [
            "hold your pass",
            "wait for a person",
            "ask before acting",
            "approve",
        ] {
            assert!(
                !sent.to_lowercase().contains(word),
                "the lane was asked to act on the gate itself: `{word}`\n{sent}"
            );
        }

        // And the pass it reports does not move it. The step passed, the work
        // stands, and the task waits on a person instead of going to `done`.
        let mut task = reload(&path);
        task.front.worktree_path = None;
        task.save().unwrap();
        // `report` now refuses a `--task` that disagrees with `SPOOLWAY_TASK`
        // (review finding 10). This whole suite runs inside a real lane whose
        // `SPOOLWAY_TASK` is its own; a report on `demo` carries `demo`.
        crate::platform::test_env::with_env(crate::commands::TASK_ENV, "demo", || {
            crate::commands::report(
                &repo,
                &pipelines,
                &crate::cli::ReportArgs {
                    task: Some("demo".into()),
                    pass: true,
                    fail: false,
                    block: false,
                    pause: false,
                    message: Some("released it".into()),
                    handoff: vec![],
                },
                Some("release"),
            )
        })
        .unwrap();

        let task = reload(&path);
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("release"));
    }

    /// The failure this bounds is a model that ends its turn without running
    /// `spoolway report`. The opening prompt asks for it in as many words; a
    /// model that ignores that used to leave its task sitting on the step for the
    /// rest of the day with a live session billing for nothing. Now it is sent
    /// the report contract again — as often as it takes — and only a lane that
    /// goes fully quiet *after* a reminder ever reaches `blocked`.
    // covers: dispatch.lane_quiet — how long a lane may say nothing before it is reminded, and marked as waiting
    #[test]
    fn a_lane_that_settles_without_reporting_is_reminded_then_blocked_once_it_goes_dead() {
        let repo = fixture("unreported");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        // One mux for the whole test: a real backend's pane persists between
        // passes, and the pane-hash fallback this lane falls back to (it names
        // no session) has to be measured against what is actually still on it,
        // reminder included, rather than a fresh empty one every pass.
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);

        // Freshly settled: this is what a lane that has just asked something
        // looks like too, and it is also what a lane that started a background
        // job and stopped talking looks like. Nothing can tell them apart this
        // early, so nothing happens — no reminder, which waits a full pass so
        // as not to race a report that may already be landing, and no mark on
        // the board either.
        run_pass(&repo, &mux);
        assert_eq!(reload(&path).stage(), "implement");
        assert!(mux.did("prompt").is_empty());
        assert!(mux.did("stop").is_empty());
        assert!(
            lanes_awaiting_a_person(&repo).is_empty(),
            "a lane settled for a moment is not yet holding a question"
        );
        mux.clear_calls();

        // A pass later, still settled: it is sent the report contract again,
        // and now it is quiet enough to be worth a person's attention too.
        age_lane(&repo, "demo · implement", Duration::from_secs(15));
        let report = run_pass(&repo, &mux);
        assert_eq!(
            reload(&path).stage(),
            "implement",
            "not escalated — reminded"
        );
        assert_eq!(mux.did("prompt"), vec!["prompt demo · implement"]);
        assert!(
            lanes_awaiting_a_person(&repo).contains("demo · implement"),
            "quiet for `lane_quiet` is what earns the mark, the same bar a reminder clears"
        );
        assert!(
            report
                .actions
                .iter()
                .any(|line| line.contains("reminded") && line.contains("implement")),
            "got {:?}",
            report.actions
        );
        assert!(
            lanes_awaiting_a_person(&repo).contains("demo · implement"),
            "still a pane worth a look, reminder or not"
        );
        mux.clear_calls();

        // Nothing written since that reminder: the dead session no reminder
        // can reach. One quiet pass is all it takes — there is no clock to
        // wait out any more.
        age_lane(&repo, "demo · implement", Duration::from_secs(700));
        let report = run_pass(&repo, &mux);

        let task = reload(&path);
        assert_eq!(task.stage(), "blocked");
        assert_eq!(
            task.front.blocked_from.as_deref(),
            Some("implement"),
            "unblocking has to resume the step that never reported"
        );
        assert!(
            task.section("## Blocker")
                .unwrap()
                .contains("produced no output since its last reminder"),
            "the blocker has to say what is actually wrong: {:?}",
            task.section("## Blocker")
        );
        assert!(
            mux.did("stop").is_empty(),
            "the session a person now has to read must still be there to read"
        );
        assert_eq!(
            mux.did("focus"),
            vec!["focus demo · implement"],
            "and it must be what they are looking at"
        );
        assert!(
            lanes_awaiting_a_person(&repo).is_empty(),
            "a blocked task is not a pane anybody is waiting on"
        );
        assert!(
            report.actions.iter().any(|line| line.contains("stuck at")),
            "got {:?}",
            report.actions
        );
    }

    /// A lane sitting in a long, quiet tool call and a lane genuinely idle at
    /// its prompt look identical to `note_progress`'s only signal today, the
    /// transcript: neither writes anything, so both read as silent. This
    /// lane never writes a line across three aged passes, yet it must not be
    /// reminded, because a process it started — `with_busy_child` stands in
    /// for a backend that can see this in the process table — is still
    /// running the whole time. See .spoolway/queue/lane-liveness.md, "Bug".
    #[test]
    fn a_lane_holding_a_process_it_started_is_never_reminded() {
        let repo = fixture("busy-child");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);
        mux.with_busy_child("demo · implement");

        for _ in 0..3 {
            age_lane(&repo, "demo · implement", Duration::from_secs(15));
            let report = run_pass(&repo, &mux);
            assert!(
                mux.did("prompt").is_empty(),
                "reminded a lane still holding a process it started: {:?}",
                report.actions
            );
        }
        assert_eq!(reload(&path).stage(), "implement", "never escalated either");
    }

    /// The excuse a live child gives is not unbounded: past
    /// `dispatch.lane_child_ceiling` the lane is escalated anyway, because a
    /// process that has run this long unsupervised is what the ceiling
    /// exists to catch. The reminder loop it bypasses never even runs — no
    /// `prompt` call at all — since a lane that never stops writing to its
    /// transcript would never reach it.
    #[test]
    fn a_process_held_open_past_the_ceiling_is_escalated_anyway() {
        let mut repo = fixture("busy-child-ceiling");
        repo.config.dispatch.lane_child_ceiling = Duration::from_secs(30);
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);
        mux.with_busy_child("demo · implement");

        // One pass to start the child's own clock ticking...
        run_pass(&repo, &mux);
        // ...then past the ceiling, still busy.
        age_lane(&repo, "demo · implement", Duration::from_secs(45));
        {
            let mut records = load_lane_records(&repo);
            let record = records.get_mut("demo · implement").unwrap();
            record.child_since = record.child_since.map(|at| at - 45);
            save_lane_records(&repo, &records).unwrap();
        }

        let report = run_pass(&repo, &mux);
        assert!(
            mux.did("prompt").is_empty(),
            "no reminder, straight to escalation"
        );

        let task = reload(&path);
        assert_eq!(task.stage(), "blocked");
        let blocker = task.section("## Blocker").unwrap();
        assert!(blocker.contains("held a process open"), "{blocker:?}");
        assert!(
            report.actions.iter().any(|line| line.contains("stuck at")),
            "got {:?}",
            report.actions
        );
    }

    /// The child going away restarts the excuse's own clock rather than
    /// leaving it running from a first, unrelated call — a lane that finishes
    /// one long tool call and starts a second gets the ceiling's full hour
    /// again, not whatever was left of the first one's.
    #[test]
    fn a_lane_whose_child_goes_and_comes_back_gets_a_fresh_ceiling() {
        let repo = fixture("busy-child-restart");
        add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);

        // Stand in for an old, already-cleared excuse — as if a first long
        // call had held this lane open a good while ago.
        {
            let mut records = load_lane_records(&repo);
            records.get_mut("demo · implement").unwrap().child_since = Some(now_secs() - 500);
            save_lane_records(&repo, &records).unwrap();
        }

        // The child is gone this pass — the transcript signal alone decides
        // — so the stale excuse is cleared rather than left to linger.
        run_pass(&repo, &mux);
        assert_eq!(
            load_lane_records(&repo)
                .get("demo · implement")
                .unwrap()
                .child_since,
            None,
            "cleared the moment the child went away"
        );

        // A second, unrelated call starts: the excuse is set again, and from
        // now — not from the 500-second-old mark this lane carried before.
        mux.with_busy_child("demo · implement");
        run_pass(&repo, &mux);
        let second = load_lane_records(&repo)
            .get("demo · implement")
            .unwrap()
            .child_since
            .expect("a live child sets the mark again");
        assert!(
            now_secs() - second < 5,
            "a fresh clock, not the 500-second-old one: {second}"
        );
    }

    /// The bound the plan's own mockup was drawn to hold, and the one figure
    /// on the page that was not captured from a real run: a person's own
    /// Escape reaches `● paused` within the pass that finds the lane settled,
    /// not after `dispatch.lane_quiet` nudges it once and `MAX_REMINDERS`
    /// escalates it. A single `pass()` call is the whole test — if this
    /// needed a second one to land on `paused`, it would already be too
    /// late, the same fifteen-minute road `a_lane_that_settles_without_reporting_is_reminded_then_blocked_once_it_goes_dead`
    /// walks for an ordinary settled lane right above.
    #[test]
    fn a_hand_interrupted_lane_reaches_paused_within_one_pass() {
        let repo = fixture("hand-interrupt");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        record_lane(
            &repo,
            "demo · implement",
            "aborted-session",
            &implement_kind(&repo),
        );

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        let home = pi_home_aborted("aborted-session");
        let report = with_home(&home, || run_pass(&repo, &mux));
        std::fs::remove_dir_all(&home).ok();

        let task = reload(&path);
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.parked_from.as_deref(), Some("implement"));
        assert!(
            mux.did("prompt").is_empty(),
            "a hand interrupt must never be nudged"
        );
        assert!(
            mux.did("stop").is_empty(),
            "and never escalated — the lane still holds whatever it was doing"
        );
        assert!(
            report
                .actions
                .iter()
                .any(|line| line.contains("demo") && line.contains("its own Escape")),
            "got {:?}",
            report.actions
        );
    }

    /// `--dry-run` reports the park it would make rather than the nudge it
    /// would otherwise send — and, being a dry run, writes nothing: the task
    /// must still be sitting on `implement` afterwards.
    #[test]
    fn a_dry_run_reports_the_park_it_would_make_not_the_nudge() {
        let repo = fixture("hand-interrupt-dry");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        record_lane(
            &repo,
            "demo · implement",
            "aborted-session",
            &implement_kind(&repo),
        );

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        let home = pi_home_aborted("aborted-session");
        let report = with_home(&home, || {
            Dispatcher::new(&repo, &Pipelines::builtin(), &mux, true).pass()
        });
        std::fs::remove_dir_all(&home).ok();
        let report = report.unwrap();

        assert_eq!(
            reload(&path).stage(),
            "implement",
            "a dry run writes nothing"
        );
        assert!(mux.did("prompt").is_empty());
        assert!(
            report
                .actions
                .iter()
                .any(|line| line.contains("would park")),
            "got {:?}",
            report.actions
        );
    }

    /// The other half of a hand interrupt: a settled lane with no aborted-turn
    /// record in its transcript takes exactly today's road, unaffected by the
    /// new branch ahead of it — reminded once here, the same outcome
    /// `a_lane_that_settles_without_reporting_is_reminded_then_blocked_once_it_goes_dead`
    /// already covers in full for the eventual escalation.
    #[test]
    fn a_settled_lane_with_no_abort_marker_is_still_reminded_not_parked() {
        let repo = fixture("hand-interrupt-none");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        record_lane(
            &repo,
            "demo · implement",
            "ordinary-session",
            &implement_kind(&repo),
        );

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        // No transcript at all under this home for `ordinary-session` —
        // `last_turn_aborted` answers "no" for a session it cannot find,
        // exactly as it would for a lane that simply has not written yet.
        let home = crate::scratch::root("dispatch-hand-interrupt-none-home");
        std::fs::create_dir_all(&home).unwrap();

        // A first, settling pass — the same reason
        // `a_lane_that_settles_without_reporting_is_reminded_then_blocked_once_it_goes_dead`
        // takes one before it ages the lane: this is what gives
        // `note_progress`'s pane-hash fallback a baseline to compare
        // against, so aging the record below is not immediately undone by
        // the fallback reading a changed hash as fresh progress.
        with_home(&home, || run_pass(&repo, &mux));
        mux.clear_calls();

        age_lane(&repo, "demo · implement", Duration::from_secs(15));
        let report = with_home(&home, || run_pass(&repo, &mux));
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(
            reload(&path).stage(),
            "implement",
            "no marker — reminded, not parked"
        );
        assert_eq!(mux.did("prompt"), vec!["prompt demo · implement"]);
        assert!(
            report
                .actions
                .iter()
                .any(|line| line.contains("reminded") && line.contains("implement")),
            "got {:?}",
            report.actions
        );
    }

    /// A lane that writes something new after a reminder — even without
    /// reporting — is reminded again rather than judged dead, up to the
    /// reminder ceiling; see `a_lane_reminded_three_times_is_escalated_on_the_fourth_due_reminder`
    /// for where that bites. This test's one reminder does not come close to
    /// A lane waiting on the run it was told to make is not a stuck lane.
    ///
    /// The `e2e` prompt is required to run the end-to-end `pr` tier and pass
    /// only once it has seen it green, and the shipped `suite` step allows its
    /// own run 45 minutes. An agent doing that ends its turn and waits on a
    /// background job, which from outside is indistinguishable from a lane
    /// that finished and forgot to report.
    ///
    /// The patience used to be `dispatch.interval` — how often a pass looks at
    /// a lane, which is not a statement about how long a lane may be quiet. At
    /// the shipped ten seconds, such a lane was nudged after ten seconds,
    /// burned a turn answering each of three reminders, and was escalated onto
    /// `blocked` inside a minute for doing exactly what it was told.
    #[test]
    fn the_shipped_patience_outlasts_a_lane_waiting_on_its_test_run() {
        let shipped = Config::default().dispatch;
        assert!(
            shipped.lane_quiet > shipped.interval,
            "patience is not the poll rate: {:?} vs {:?}",
            shipped.lane_quiet,
            shipped.interval
        );
        // Four of these is what it takes to escalate — three reminders and the
        // due one that replaces the fourth — so the round trip has to outlast
        // the 45 minutes the shipped `suite` step gives its own run.
        assert!(
            shipped.lane_quiet * (MAX_REMINDERS + 1) > Duration::from_secs(45 * 60),
            "a lane sitting out a `pr` tier would be escalated mid-run: {:?}",
            shipped.lane_quiet
        );

        // And the behaviour itself: a lane quiet for well over a poll interval,
        // but inside its patience, is left alone rather than prompted.
        let repo = {
            let mut repo = fixture("patience-outlasts-a-run");
            repo.config.dispatch.lane_quiet = shipped.lane_quiet;
            repo
        };
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);
        mux.clear_calls();

        // Minutes of silence — many poll intervals, and the whole of what the
        // old rule needed to nudge — while the run it is waiting on is still
        // going.
        age_lane(&repo, "demo · implement", Duration::from_secs(5 * 60));
        run_pass(&repo, &mux);

        assert!(
            mux.did("prompt").is_empty(),
            "a lane inside its patience must not be nudged: {:?}",
            mux.did("prompt")
        );
        assert_eq!(reload(&path).stage(), "implement", "and not escalated");
    }

    /// it.
    #[test]
    fn a_settled_lane_that_writes_after_a_reminder_is_reminded_again() {
        let repo = fixture("unreported-again");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);

        age_lane(&repo, "demo · implement", Duration::from_secs(15));
        run_pass(&repo, &mux);
        assert_eq!(mux.did("prompt"), vec!["prompt demo · implement"]);
        mux.clear_calls();

        // The lane wrote something after the reminder — a tool call, a false
        // start, anything — without ever running `spoolway report`. Modelled
        // directly on the record rather than through a real transcript, which
        // `note_progress` is unit-tested on its own. Aged first so there is
        // room between the reminder and the write to place it in — the
        // reminder's own `reminded_at` is baselined to when it landed, not to
        // whatever the record read before it was sent.
        age_lane(&repo, "demo · implement", Duration::from_secs(20));
        {
            let mut records = load_lane_records(&repo);
            let record = records.get_mut("demo · implement").unwrap();
            let reminded_at = record.reminded_at.expect("reminded on the pass above");
            record.last_progress = reminded_at + 8;
            save_lane_records(&repo, &records).unwrap();
        }

        // Long *aged* from the reminder, but only one pass past this fresh
        // write — so `due` reads the write, not the age, and it is reminded
        // again rather than blocked.
        let report = run_pass(&repo, &mux);

        assert_eq!(reload(&path).stage(), "implement", "reminded, not blocked");
        assert_eq!(mux.did("prompt"), vec!["prompt demo · implement"]);
        assert!(
            report.actions.iter().any(|line| line.contains("reminded")),
            "got {:?}",
            report.actions
        );
    }

    /// `free_finished_lanes` only drops a record once the multiplexer's own
    /// lane list shows the pane it belongs to. A pane that closed while the
    /// dispatcher was down is never in that list again, so a record like
    /// `ghost · implement` — whose task has since left the queue entirely —
    /// would sit in `lanes.json` forever without the prune this test proves.
    #[test]
    fn a_lane_record_for_a_task_no_longer_queued_is_pruned() {
        let repo = fixture("stale-lane-record");
        add_task(&repo, "demo", "implement");

        let mut records = HashMap::new();
        records.insert(
            "demo · implement".to_string(),
            LaneRecord::adopted(now_secs()),
        );
        records.insert(
            "ghost · implement".to_string(),
            LaneRecord::adopted(now_secs()),
        );
        save_lane_records(&repo, &records).unwrap();

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        let records = load_lane_records(&repo);
        assert!(
            records.contains_key("demo · implement"),
            "a task still in the queue keeps its lane record"
        );
        assert!(
            !records.contains_key("ghost · implement"),
            "a task that left the queue has its lane record pruned"
        );
    }

    /// Three reminders is the whole of the patience `check_unreported` has.
    /// A lane that is still writing something after the third does not get a
    /// fourth — it is escalated in its place, naming the pane a person has to
    /// go look at.
    #[test]
    fn a_lane_reminded_three_times_is_escalated_on_the_fourth_due_reminder() {
        let repo = fixture("reminder-ceiling");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        // Freshly settled: no reminder yet.
        run_pass(&repo, &mux);
        mux.clear_calls();

        for n in 1..=MAX_REMINDERS {
            age_lane(&repo, "demo · implement", Duration::from_secs(15));
            let report = run_pass(&repo, &mux);
            assert_eq!(
                mux.did("prompt"),
                vec!["prompt demo · implement"],
                "reminder {n}"
            );
            assert!(
                report
                    .actions
                    .iter()
                    .any(|line| line.contains(&format!("({n}/{MAX_REMINDERS})"))),
                "reminder {n}: got {:?}",
                report.actions
            );
            assert_eq!(reload(&path).stage(), "implement");
            mux.clear_calls();

            // The lane wrote something after the reminder, so the next pass
            // is due again with no clock to wait out — the same fixture
            // `a_settled_lane_that_writes_after_a_reminder_is_reminded_again`
            // drives once, driven here up to the ceiling.
            age_lane(&repo, "demo · implement", Duration::from_secs(20));
            let mut records = load_lane_records(&repo);
            let record = records.get_mut("demo · implement").unwrap();
            let reminded_at = record.reminded_at.expect("reminded on the pass above");
            record.last_progress = reminded_at + 8;
            save_lane_records(&repo, &records).unwrap();
        }

        // The fourth due reminder is not sent: the ceiling escalates instead.
        let report = run_pass(&repo, &mux);
        assert!(mux.did("prompt").is_empty(), "no fourth reminder");

        let task = reload(&path);
        assert_eq!(task.stage(), "blocked");
        let blocker = task.section("## Blocker").unwrap();
        assert!(
            blocker.contains(&format!("reminded {MAX_REMINDERS} times")),
            "{blocker:?}"
        );
        assert!(blocker.contains("w1:p1"), "names the pane: {blocker:?}");
        assert!(
            report.actions.iter().any(|line| line.contains("stuck at")),
            "got {:?}",
            report.actions
        );
    }

    /// `retire` has no next-step pane to hand its `PaneHandover` to within the
    /// same pass — the step starting again is this one's own name, and that
    /// only happens on a later pass. So a claude lane vacated here is written
    /// down on its own record instead, and the pass that finally starts
    /// `blocked` again finds no lane under that name but does find the pane
    /// its last round left standing — and starts in it rather than splitting
    /// one of its own, exactly as a cross-step handover would.
    #[test]
    fn a_self_routing_step_hands_its_pane_to_its_own_restart() {
        let repo = unattended_fixture("blocked-self-handover");
        let path = add_task_with(&repo, "demo", "blocked", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.blocked_from = Some("implement".into());
        });

        let started_at = now_secs() - 30;
        {
            let mut lanes = load_lane_records(&repo);
            lanes.insert(
                "demo · blocked".to_string(),
                LaneRecord {
                    started_at,
                    last_progress: started_at,
                    kind: "claude".into(),
                    ..LaneRecord::adopted(started_at)
                },
            );
            save_lane_records(&repo, &lanes).unwrap();
        }

        let mut task = reload(&path);
        task.front.last_report = Some(crate::task::LastReport {
            step: "blocked".into(),
            outcome: "block".into(),
            at: started_at + 5,
        });
        task.save().unwrap();

        let mux = FakeMux::new(vec![Lane {
            kind: "claude".into(),
            ..lane_in(&repo, "demo · blocked", LaneStatus::Done, "w1:p7")
        }]);
        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("vacate"),
            ["vacate demo · blocked"],
            "a claude lane is asked to leave, not closed"
        );
        assert!(
            mux.did("close_pane").is_empty(),
            "nothing is closed on the pass that retires it — {:?}",
            mux.did("close_pane")
        );

        // A later pass: the session really ended, so this backend no longer
        // reports a lane under this name at all — only `lanes.json`, on disk,
        // still knows the pane it left behind.
        let mux2 = FakeMux::new(vec![]);
        run_pass(&repo, &mux2);

        assert!(
            mux2.did("split_pane").is_empty(),
            "nothing is split: the pane retire vacated is the pane this restart uses — {:?}",
            mux2.did("split_pane")
        );
        assert_eq!(
            mux2.did("launch demo · blocked")[0],
            "launch demo · blocked pane=w1:p7",
            "the restart is launched into the pane its own last round handed forward"
        );
    }

    /// A settled lane on a step whose report routes back to itself —
    /// `blocked` reporting `--block` again, most often — has already done
    /// what stage movement does everywhere else: the task is exactly where
    /// its report sent it. Asking "did it report?" by stage movement alone
    /// would nudge this lane forever, since there is no movement to read; the
    /// identity check reads the report directly and retires the lane instead.
    #[test]
    fn a_settled_lane_that_reported_on_a_self_routing_step_is_retired_not_reminded() {
        let repo = unattended_fixture("blocked-self-route");
        let path = add_task_with(&repo, "demo", "blocked", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.blocked_from = Some("implement".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · blocked", LaneStatus::Done)]);

        // The lane's own record, as the dispatcher itself would have written
        // it at launch.
        let started_at = now_secs() - 30;
        {
            let mut lanes = load_lane_records(&repo);
            lanes.insert(
                "demo · blocked".to_string(),
                LaneRecord {
                    started_at,
                    last_progress: started_at,
                    ..LaneRecord::adopted(started_at)
                },
            );
            save_lane_records(&repo, &lanes).unwrap();
        }

        // It reported after its lane started — the only fact the identity
        // check reads.
        let mut task = reload(&path);
        task.front.last_report = Some(crate::task::LastReport {
            step: "blocked".into(),
            outcome: "block".into(),
            at: started_at + 5,
        });
        task.save().unwrap();

        let report = run_pass(&repo, &mux);

        assert!(
            mux.did("prompt").is_empty(),
            "a lane that already reported is not nudged"
        );
        assert_eq!(mux.did("stop"), vec!["stop demo · blocked"]);
        assert!(
            !load_lane_records(&repo).contains_key("demo · blocked"),
            "its bookkeeping ends with the round"
        );
        assert!(
            report
                .actions
                .iter()
                .any(|line| line.contains("demo · blocked") && line.contains("freed")),
            "got {:?}",
            report.actions
        );
    }

    /// The pane held for a person is a *pane*, and a backend without one has
    /// nothing to hold. Headless keeps a turn's process instead — a lane that
    /// hung, or a model mid-thought, left running with nobody able to look at
    /// it and nothing to reap it when the run ends. What a person reads there
    /// is the log, and the log deliberately outlives the lane either way.
    ///
    /// Same question `holds_a_slot` asks, and the same answer for the same
    /// reason: ask the backend whether a waiting lane is resident.
    #[test]
    fn a_block_on_a_detached_backend_stops_the_lane_rather_than_holding_it() {
        let repo = fixture("block-on-detached");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        // One instance for the whole test — see the equivalent comment on
        // `a_lane_that_settles_without_reporting_is_reminded_then_blocked_once_it_goes_dead`.
        let mux = FakeMux::new(vec![lane_in(
            &repo,
            "demo · implement",
            LaneStatus::Done,
            "w1:p2",
        )])
        .detached();

        run_pass(&repo, &mux);
        age_lane(&repo, "demo · implement", Duration::from_secs(15));
        run_pass(&repo, &mux);
        age_lane(&repo, "demo · implement", Duration::from_secs(700));

        run_pass(&repo, &mux);
        assert_eq!(reload(&path).stage(), "blocked");
        assert_eq!(
            mux.did("stop"),
            ["stop demo · implement"],
            "the turn was left running: {:?}",
            mux.calls()
        );
        assert!(
            mux.did("focus").is_empty(),
            "there is nothing to focus on a backend with no pane"
        );
    }

    /// A blocked task's pane is the only account of what the session was doing,
    /// so it is kept for as long as the block is — and no longer. The pass that
    /// finds the task unblocked closes it, and has to do so *before* the step
    /// is started again: a lane is named after its step and its task, so the
    /// pane held over from the block and the one about to replace it would be
    /// arguing over one name.
    #[test]
    fn a_pane_held_for_a_person_is_closed_once_the_block_is_cleared() {
        let repo = fixture("block-holds-its-pane");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
        });

        // A lane that settled, was reminded, and then wrote nothing further:
        // the escalation route that has a live pane to decide about. One mux
        // instance for these three passes, since a real
        // backend's pane persists between them and the reminder has to land
        // on the pane the next pass actually reads.
        let mux = FakeMux::new(vec![lane_in(
            &repo,
            "demo · implement",
            LaneStatus::Done,
            "w1:p2",
        )]);
        run_pass(&repo, &mux);
        age_lane(&repo, "demo · implement", Duration::from_secs(15));

        run_pass(&repo, &mux);
        age_lane(&repo, "demo · implement", Duration::from_secs(700));
        mux.clear_calls();

        run_pass(&repo, &mux);
        assert_eq!(reload(&path).stage(), "blocked");

        // Still blocked a pass later: held once, not focused again every pass
        // until somebody looks.
        let mux = FakeMux::new(vec![lane_in(
            &repo,
            "demo · implement",
            LaneStatus::Done,
            "w1:p2",
        )]);
        run_pass(&repo, &mux);
        assert!(mux.did("stop").is_empty(), "the pane was taken away");
        assert!(
            mux.did("focus").is_empty(),
            "a held pane is focused when it is held, not on every pass after"
        );

        // A person clears it. The pane goes, and the step starts again.
        let mut task = reload(&path);
        task.set_stage("implement", None);
        task.save().unwrap();
        let mux = FakeMux::new(vec![lane_in(
            &repo,
            "demo · implement",
            LaneStatus::Done,
            "w1:p2",
        )]);
        run_pass(&repo, &mux);
        assert_eq!(
            mux.did("close_pane"),
            vec!["close_pane w1:p2"],
            "the pane the block was read in has to go before its step runs again"
        );
        assert!(
            mux.did("launch")
                .iter()
                .any(|c| c.contains("demo · implement")),
            "and the step gets a lane of its own: {:?}",
            mux.calls()
        );
    }

    /// A held pane is a person's to type into, and the one thing worth
    /// noticing in it is a round: busy, then idle again. Seeded straight into
    /// `held_for_block` rather than driven there through a gate, the way
    /// `hold_for_block` itself would have left it — the point of this test is
    /// what happens *after* the hold, not the hold itself.
    #[test]
    fn a_persons_round_in_a_held_pane_is_committed_when_it_settles() {
        let repo = fixture("person-turn");
        let path = add_task_with(&repo, "demo", crate::pipeline::PAUSED, |f| {
            f.worktree_path = Some(repo.root.clone());
            f.workspace_id = Some("w1".into());
        });

        let started_at = crate::repo::run(&repo.root, "git", &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        {
            let mut lanes = load_lane_records(&repo);
            lanes.insert(
                "demo · implement".to_string(),
                LaneRecord {
                    head: started_at.clone(),
                    held_for_block: true,
                    ..LaneRecord::adopted(now_secs())
                },
            );
            save_lane_records(&repo, &lanes).unwrap();
        }

        // Busy: a person is mid-round. Nothing about the worktree is touched.
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Working)]);
        run_pass(&repo, &mux);
        assert_eq!(
            crate::repo::run(&repo.root, "git", &["rev-parse", "HEAD"])
                .unwrap()
                .trim(),
            started_at,
            "still mid-round, nothing to commit"
        );

        // The round leaves the worktree dirty, then settles.
        std::fs::write(repo.root.join("note.txt"), "left by a person\n").unwrap();
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);

        let head_after = crate::repo::run(&repo.root, "git", &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        assert_ne!(head_after, started_at, "the round's work is committed");

        let task = reload(&path);
        assert_eq!(
            task.stage(),
            crate::pipeline::PAUSED,
            "still parked afterwards — nobody answered anything"
        );
        let log = task.section("## Status Log").unwrap();
        assert!(
            log.contains("a person"),
            "status log should say a person drove the round: {log:?}"
        );

        // Settled and staying settled: the second idle pass in a row is not
        // mistaken for another round, and commits nothing further.
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);
        assert_eq!(
            crate::repo::run(&repo.root, "git", &["rev-parse", "HEAD"])
                .unwrap()
                .trim(),
            head_after,
            "idle after idle is not a second round"
        );
    }

    /// A held pane's lane is banked when the block clears and the pane is
    /// freed, so the rounds a person drove in it while it was held reach the
    /// ledger. `free_finished_lanes` used to skip `record_usage` for any
    /// `held_for_block` lane, on the belief that booking it a second time
    /// would double-count it — but `record_usage` diffs the transcript
    /// against what the hold-time line already banked, so only the person's
    /// own delta is appended (review finding 34).
    #[test]
    fn a_held_lane_banks_the_persons_rounds_when_its_pane_is_freed() {
        let mut repo = fixture("held-lane-banks");
        priced(&mut repo, "priced-model");
        let _task = reload(&add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        }));

        let session = "held-s34";
        let kind = local_kind(&repo);
        // The line banked when the lane was first held. The person's rounds
        // since are what is missing from it.
        write_entry(&repo, "demo", "implement", &kind, "priced-model", session);
        {
            let mut lanes = load_lane_records(&repo);
            lanes.insert(
                "demo · implement".into(),
                LaneRecord {
                    session: session.into(),
                    kind: kind.clone(),
                    agent: "pi".into(),
                    model: "priced-model".into(),
                    held_for_block: true,
                    ..LaneRecord::adopted(now_secs())
                },
            );
            save_lane_records(&repo, &lanes).unwrap();
        }

        // The block is cleared: the task is back on its step and its lane has
        // gone idle, so `free_finished_lanes` frees the held pane — and banks
        // it on the way.
        let home = pi_home_with(session, 8_400);
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        with_home(&home, || {
            Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
                .pass()
                .unwrap();
        });

        let banked = crate::usage::read(&repo).unwrap();
        let lines: Vec<_> = banked.iter().filter(|e| e.session == session).collect();
        assert_eq!(
            lines.len(),
            2,
            "the hold-time line and the freed lane's own bank: {banked:?}"
        );
        assert_eq!(
            lines[1].tokens.input, 8_400,
            "the transcript's spend since the hold, banked once the pane was freed"
        );
    }

    /// Backdate a lane's record so the clocks read as `how_long` having passed.
    /// The record is the dispatcher's own bookkeeping, so a test that wants a
    /// timeout to fire moves that rather than the wall clock.
    /// Push back the moment a lane's task first wanted its pane back, which is
    /// the clock `HANDOVER_WAIT` is measured against. Written by the pass that
    /// first waited, so this only ever runs after one.
    fn age_handover(repo: &Repo, lane: &str, how_long: Duration) {
        let mut records = load_lane_records(repo);
        let record = records
            .get_mut(lane)
            .unwrap_or_else(|| panic!("no record for `{lane}`: {:?}", lanes_path(repo)));
        let since = record
            .handing_over_since
            .as_mut()
            .unwrap_or_else(|| panic!("`{lane}` is not handing its pane over"));
        *since -= how_long.as_secs() as i64;
        save_lane_records(repo, &records).unwrap();
    }

    fn age_lane(repo: &Repo, lane: &str, how_long: Duration) {
        let mut records = load_lane_records(repo);
        let record = records
            .get_mut(lane)
            .unwrap_or_else(|| panic!("no record for `{lane}`: {:?}", lanes_path(repo)));
        record.started_at -= how_long.as_secs() as i64;
        record.last_progress -= how_long.as_secs() as i64;
        if let Some(at) = &mut record.reminded_at {
            *at -= how_long.as_secs() as i64;
        }
        save_lane_records(repo, &records).unwrap();
    }

    /// The serialisation this replaced: two tasks merging into one checkout was
    /// a race for one index, so the second was held for a pass. Nothing merges
    /// into a shared checkout any more — every lane works on its own branch and
    /// hands it over as a pull request — so two handovers run side by side.
    ///
    /// `handover` is a command step now, and starting one is a real run —
    /// see `only_the_top_of_a_chain_runs_a_last_step`.
    #[test]
    fn two_tasks_hand_over_at_the_same_time() {
        let repo = fixture("handover-parallel");
        for id in ["alpha", "beta"] {
            add_task_with(&repo, id, "handover", |f| {
                f.branch = Some(format!("task/{id}"));
                f.base = Some("work".into());
            });
        }

        let mux = FakeMux::new(vec![]);
        let report = Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .pass()
            .unwrap();

        // `handover` is a command step now — `run: spoolway stack` — which
        // takes no worker slot at all, so there is no contention left to
        // avoid here. Both run in the same pass regardless.
        assert_eq!(
            report
                .actions
                .iter()
                .filter(|line| line.contains("running `handover`"))
                .count(),
            2,
            "got {:?}",
            report.actions
        );
    }

    /// A `gate:` says a step is *expected* to stop and ask. It is not what makes
    /// a stopped lane a conversation: no shipped step gates, and a handover lane
    /// that ends its turn with the task still on its step is still one.
    #[test]
    fn a_settled_lane_waits_whether_or_not_its_step_declares_a_gate() {
        let repo = fixture("gate-off");
        let path = add_task_with(&repo, "demo", "document", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let mux = FakeMux::new(vec![lane(&repo, "demo · document", LaneStatus::Done)]);
        run_pass(&repo, &mux);

        assert!(mux.did("stop").is_empty(), "the question was torn down");
        assert_eq!(reload(&path).front.attempts, 0);

        // Quiet for `lane_quiet` is what puts it on the board, gate or no gate
        // — which is the point of the test: the gate changes nothing here.
        age_lane(&repo, "demo · document", Duration::from_secs(15));
        run_pass(&repo, &mux);
        assert!(lanes_awaiting_a_person(&repo).contains("demo · document"));
    }

    /// Every waiting lane gives its slot back, and a paused one gives it back
    /// too.
    ///
    /// A gate used to be the exception: its lane held the question, so answering
    /// resumed the work in that same lane and taking its slot away would leave a
    /// merge queued behind whatever started instead. Nothing holds a question
    /// now — the lane reported and ended, and the task waits on `paused` — so
    /// its pane is a pane being kept for somebody to read, exactly like a
    /// blocked one, and two of those are the whole of `[agents.pi]
    /// concurrency = 2`.
    #[test]
    fn a_pane_kept_for_a_person_holds_no_worker_slot() {
        let repo = fixture("waiting-slot");
        add_task_with(&repo, "asking", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        add_task(&repo, "next", "queued");
        add_task(&repo, "later", "queued");

        let mux = FakeMux::new(vec![lane(&repo, "asking · implement", LaneStatus::Done)]);
        run_pass(&repo, &mux);
        assert_eq!(mux.did("start").len(), 2, "started: {:?}", mux.did("start"));

        // And the same with a paused task holding the pane: both queued tasks
        // start, because nothing of this profile's is running.
        let repo = fixture("waiting-slot-paused");
        let pipelines = gated_pipeline();
        add_task_with(&repo, "gated", crate::pipeline::PAUSED, |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(repo.root.clone());
            f.paused_at = Some("release".into());
        });
        add_task(&repo, "next", crate::pipeline::QUEUED);
        add_task(&repo, "later", crate::pipeline::QUEUED);

        let mux = FakeMux::new(vec![lane(&repo, "gated · release", LaneStatus::Done)]);
        let report = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        assert_eq!(
            mux.did("start").len(),
            2,
            "started: {:?}, problems: {:?}",
            mux.did("start"),
            report.problems
        );
        // And its pane is still there to be read.
        assert!(mux.did("stop").is_empty(), "the pane was torn down");
    }

    /// The ordering this pins is the one thing about the start budget that is
    /// easy to get backwards. `set_stage` zeroes `attempts`, so a start banked
    /// before it is wiped by the very transition it belongs to, and the budget
    /// silently never bites.
    #[test]
    fn a_lane_start_is_counted_after_the_transition_that_zeroes_the_counter() {
        let repo = fixture("attempts-order");
        let path = add_task(&repo, "demo", "queued");

        run_pass(&repo, &FakeMux::new(vec![]));

        let task = reload(&path);
        assert_eq!(task.stage(), "implement");
        assert_eq!(
            task.front.attempts, 1,
            "the start that just happened has to survive its own transition"
        );
    }

    /// Nothing else bounds this. A lane whose agent dies at launch leaves no
    /// session behind, so the task looks unstarted again on the next pass and
    /// would be re-spawned for as long as the dispatcher runs.
    #[test]
    fn a_task_whose_lane_keeps_being_restarted_is_handed_to_a_person() {
        let repo = fixture("attempts-budget");
        let path = add_task(&repo, "demo", "queued");

        // `max_launches = 1` by default: launched once, and never relaunched.
        // A launch that left no session behind is a reason to fetch a person,
        // not a reason to spend another lane finding out the same thing.
        run_pass(&repo, &FakeMux::new(vec![]));
        assert_eq!(reload(&path).front.attempts, 1);

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert!(mux.did("start").is_empty(), "the budget was spent");
        let task = reload(&path);
        assert_eq!(task.stage(), "blocked");
        assert!(
            task.section("## Blocker").unwrap().contains("max_launches"),
            "the blocker has to name the setting that stopped it: {:?}",
            task.section("## Blocker")
        );
    }

    /// `MAX_LAUNCHES` distinguishes a step that failed from one this very
    /// dispatcher merely never watched happen. `lanes.json` is where that
    /// watching lives — a dispatcher that dies between launching a lane and
    /// finishing that pass never writes it — so a restarted dispatcher
    /// reaching a launch with no record of it at all gets a pass's grace
    /// rather than reading the silence as proof of death on the spot.
    #[test]
    fn a_launch_this_dispatcher_never_witnessed_gets_one_pass_before_a_person_is_asked() {
        let repo = fixture("unwitnessed-dead-launch");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.attempts = MAX_LAUNCHES;
            // Launched a moment ago — as recent as a restart racing its own
            // launch's own pass would leave it.
            f.launched_at = Some(now_secs());
        });
        assert!(
            load_lane_records(&repo).is_empty(),
            "this dispatcher must start with nothing on file for the lane"
        );

        let mux = FakeMux::new(vec![]);
        let report = Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .pass()
            .unwrap();

        let task = reload(&path);
        assert_eq!(
            task.stage(),
            "implement",
            "not escalated on the first sighting"
        );
        assert_eq!(
            task.front.attempts, MAX_LAUNCHES,
            "the count is kept, not spent again"
        );
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("never saw it launch")),
            "{:?}",
            report.actions
        );

        // The grace is durable, not timed: it marked the lane seen in
        // `lanes.json`, which the pass above wrote back — so the very next
        // pass, whenever it lands, reads it as witnessed and escalates like
        // any ordinary dead launch, exactly once past the one pass of grace.
        assert!(
            !load_lane_records(&repo).is_empty(),
            "the grace pass has to leave a record behind, or every pass would grace it again"
        );
        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);
        let task = reload(&path);
        assert_eq!(
            task.stage(),
            "blocked",
            "the second pass finds the same silence and this time it means what it says"
        );
    }

    /// The other half of the same guard: a launch this same dispatcher did
    /// watch happen — its own record is right there in `lanes.json` — is not
    /// given the grace above. Silence over a launch it saw for itself really
    /// is the dead-agent case `MAX_LAUNCHES` exists to catch.
    #[test]
    fn a_launch_this_dispatcher_watched_die_escalates_without_waiting() {
        let repo = fixture("witnessed-dead-launch");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.attempts = MAX_LAUNCHES;
            f.launched_at = Some(now_secs());
        });
        let mut records = HashMap::new();
        records.insert(
            lane_name("implement", "demo"),
            LaneRecord::adopted(now_secs()),
        );
        save_lane_records(&repo, &records).unwrap();

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(reload(&path).stage(), "blocked");
    }

    /// Unblocking pays for itself: it goes through `set_stage`, which is what
    /// zeroes the counter, so a task a person has looked at gets its full
    /// budget rather than one that is already spent.
    #[test]
    fn unblocking_a_task_gives_it_its_start_budget_back() {
        let repo = fixture("attempts-unblock");
        let path = add_task_with(&repo, "demo", "blocked", |f| {
            f.blocked_from = Some("implement".into());
            f.attempts = 7;
        });

        crate::commands::resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                reject: false,
                message: None,
            },
            false,
        )
        .unwrap();

        let task = reload(&path);
        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.attempts, 0);

        // And it really does start again, rather than escalating on sight.
        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);
        assert_eq!(mux.did("start"), ["start demo · implement"]);
    }

    /// The session id the lane that blocked was given, as the dispatcher's own
    /// bookkeeping leaves it behind.
    fn record_lane(repo: &Repo, lane: &str, session: &str, kind: &str) {
        let mut lanes = load_lane_records(repo);
        lanes.insert(
            lane.to_string(),
            LaneRecord {
                session: session.to_string(),
                kind: kind.to_string(),
                ..LaneRecord::adopted(now_secs())
            },
        );
        save_lane_records(repo, &lanes).unwrap();
    }

    /// The kind the shipped `implement` step actually runs on, read rather than
    /// written down twice: a resume is only offered to a kind whose resume flag
    /// spoolway has established.
    fn implement_kind(repo: &Repo) -> String {
        let agent = Pipelines::builtin()
            .get("default")
            .unwrap()
            .step("implement")
            .unwrap()
            .agent
            .clone()
            .unwrap();
        repo.config.agents.get(&agent).unwrap().kind.clone()
    }

    /// The whole point of the exercise. A block is something outside the lane's
    /// control, so that lane's work stands — often *all* of it, with only a
    /// report left to make. Continuing its session is what makes unblocking
    /// cost the difference rather than the entire task a second time.
    #[test]
    fn a_resumed_step_continues_the_session_its_lane_blocked_on() {
        let repo = fixture("resume-session");
        let path = add_task_with(&repo, "demo", "blocked", |f| {
            f.blocked_from = Some("implement".into());
        });
        record_lane(
            &repo,
            "demo · implement",
            "the-session-that-blocked",
            &implement_kind(&repo),
        );

        crate::commands::resume(
            &repo,
            &Pipelines::builtin(),
            &crate::cli::ResumeArgs {
                task: "demo".into(),
                stage: None,
                reject: false,
                message: None,
            },
            false,
        )
        .unwrap();

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("start"), ["start demo · implement"]);
        assert_eq!(
            load_lane_records(&repo)["demo · implement"].session,
            "the-session-that-blocked",
            "the lane has to come back as the session that stopped, or it \
             re-reads the task and redoes the work to reach the same place"
        );
        assert_eq!(
            reload(&path).front.resume,
            None,
            "one launch only — the next retry of this step is its own \
             conversation again"
        );
    }

    /// The rule the resume is carved out of, and the reason it is a one-shot: a
    /// step reached again through the pipeline is a retry, and a retry that
    /// reopened the failed attempt's conversation would argue with it rather
    /// than start over.
    #[test]
    fn an_ordinary_start_mints_a_session_of_its_own() {
        let repo = fixture("resume-only-once");
        add_task_with(&repo, "demo", "implement", |_| {});
        record_lane(
            &repo,
            "demo · implement",
            "some-earlier-session",
            &implement_kind(&repo),
        );

        run_pass(&repo, &FakeMux::new(vec![]));

        assert_ne!(
            load_lane_records(&repo)["demo · implement"].session,
            "some-earlier-session"
        );
    }

    /// The kind `agent: pi` actually runs, for the same reason
    /// [`implement_kind`] reads `implement`'s: a resume is only offered to a
    /// kind whose resume flag spoolway has established.
    fn local_kind(repo: &Repo) -> String {
        repo.config.agents.get("pi").unwrap().kind.clone()
    }

    /// One ledger line, standing in for a lane that has already finished.
    fn write_entry(repo: &Repo, task: &str, step: &str, kind: &str, model: &str, session: &str) {
        crate::usage::append(
            repo,
            &crate::usage::Entry {
                ts: chrono::Utc::now().to_rfc3339(),
                task: task.to_string(),
                plan: None,
                step: step.to_string(),
                pipeline: "default".to_string(),
                agent: "pi".to_string(),
                kind: kind.to_string(),
                model: model.to_string(),
                session: session.to_string(),
                round: 0,
                wall_s: 0,
                turns: 1,
                tokens: crate::usage::Tokens::default(),
                cost_usd: None,
                ctx_peak: None,
                version: None,
                commit: None,
                outcome: None,
                run: None,
                trial: None,
                skill: None,
                project: String::new(),
            },
        )
        .unwrap();
    }

    /// One ledger line with output tokens on it, and no task — which is what
    /// [`crate::usage::ambient_sessions`] banks for the operator's own session.
    fn write_spend(repo: &Repo, task: &str, output: u64) {
        crate::usage::append(
            repo,
            &crate::usage::Entry {
                ts: chrono::Utc::now().to_rfc3339(),
                task: task.to_string(),
                plan: None,
                step: String::new(),
                pipeline: String::new(),
                agent: String::new(),
                kind: "claude".to_string(),
                model: "claude-opus-5".to_string(),
                session: "s".to_string(),
                round: 0,
                wall_s: 0,
                turns: 1,
                tokens: crate::usage::Tokens {
                    output,
                    ..Default::default()
                },
                cost_usd: None,
                ctx_peak: None,
                version: None,
                commit: None,
                outcome: None,
                run: None,
                trial: None,
                skill: None,
                project: String::new(),
            },
        )
        .unwrap();
    }

    /// The one brake an unattended run has must measure the run, and the
    /// operator's own interactive session is not the run.
    ///
    /// `usage::bank_ambient` banks planning and queueing against the
    /// project, correctly — it is real spend on real work. Summed into the
    /// ceiling it meant that sitting in a Claude session *watching* an overnight
    /// run stopped the dispatcher starting any: 20 of the 38 ledger entries a
    /// plan run left behind were the session driving it, ~50k output tokens
    /// against lanes that spent nothing but GPU time.
    // covers: unattended.max_output_tokens — the ceiling counts what lanes spend, and stops the run starting more work
    #[test]
    fn the_output_ceiling_counts_lanes_and_not_the_person_watching() {
        let mut repo = unattended_fixture("ceiling-interactive");
        repo.config.unattended.max_output_tokens = 1_000;
        let pipelines = Pipelines::builtin();
        let mux = FakeMux::new(vec![]);

        // A person planning in their own session, expensively. No `task`.
        write_spend(&repo, "", 50_000);
        assert!(
            Dispatcher::new(&repo, &pipelines, &mux, false)
                .over_output_ceiling()
                .is_none(),
            "an interactive session spent the run's ceiling"
        );

        // And one lane, which is what the ceiling is for.
        write_spend(&repo, "demo", 1_500);
        let note = Dispatcher::new(&repo, &pipelines, &mux, false)
            .over_output_ceiling()
            .expect("the lane's own spend is over the ceiling");
        assert!(note.contains("1500 output tokens"), "{note}");
    }

    /// `max_cost_usd`'s own counterpart of the test above: off by default, so
    /// an existing config's runs are unaffected, and read from `cost_usd`
    /// rather than `tokens.output` once set.
    #[test]
    fn the_cost_ceiling_is_off_by_default_and_counts_lanes_once_set() {
        let mut repo = unattended_fixture("ceiling-cost");
        let pipelines = Pipelines::builtin();
        let mux = FakeMux::new(vec![]);

        crate::usage::append(
            &repo,
            &crate::usage::Entry {
                ts: chrono::Utc::now().to_rfc3339(),
                task: "demo".to_string(),
                plan: None,
                step: "implement".to_string(),
                pipeline: "default".to_string(),
                agent: "pi".to_string(),
                kind: "pi".to_string(),
                model: "priced-model".to_string(),
                session: "s".to_string(),
                round: 0,
                wall_s: 0,
                turns: 1,
                tokens: crate::usage::Tokens::default(),
                cost_usd: Some(7.50),
                ctx_peak: None,
                version: None,
                commit: None,
                outcome: None,
                run: None,
                trial: None,
                skill: None,
                project: String::new(),
            },
        )
        .unwrap();

        assert!(
            Dispatcher::new(&repo, &pipelines, &mux, false)
                .over_cost_ceiling()
                .is_none(),
            "max_cost_usd defaults to 0 — off"
        );

        repo.config.unattended.max_cost_usd = 5.0;
        let note = Dispatcher::new(&repo, &pipelines, &mux, false)
            .over_cost_ceiling()
            .expect("$7.50 spent is over a $5.00 ceiling");
        assert!(note.contains("$7.50"), "{note}");
        assert!(note.contains("$5.00"), "{note}");
    }

    /// Stopping a dispatcher must not block every task that was in flight.
    ///
    /// `attempts` counts launches that left nothing behind, and it was only ever
    /// zeroed by *arriving* at a step — so a lane torn down by the stop kept its
    /// count, and the very next launch was one too many. For an overnight run
    /// that was the difference between picking up where you left off and finding
    /// three blocked tasks in the morning.
    #[test]
    fn stopping_a_run_forgives_the_launch_counter_so_the_next_one_resumes() {
        let repo = fixture("stop-forgives");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(repo.root.clone());
            f.attempts = MAX_LAUNCHES;
            f.launched_at = Some(now_secs());
        });

        let pipelines = Pipelines::builtin();
        let mux = FakeMux::new(vec![]);
        let mut report = Report::default();
        Dispatcher::new(&repo, &pipelines, &mux, false)
            .sweep_on_stop(&mut report)
            .unwrap();

        let task = reload(&path);
        assert_eq!(task.front.attempts, 0);
        // `launched_at` is the board's clock now, not the counter's — it is
        // left behind here, harmlessly: nothing reads it once the lane it
        // timed is gone, and the next launch overwrites it anyway.
        assert!(task.front.launched_at.is_some());
        assert_eq!(task.stage(), "implement", "the task kept its place");

        // And the next run starts it rather than handing it to a person.
        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);
        assert_eq!(mux.did("start").len(), 1, "did: {:?}", mux.calls());
        assert_ne!(reload(&path).stage(), crate::pipeline::BLOCKED);
    }

    /// The other half of the same rule, and the one that covers a lane lost to
    /// an OOM kill or a reboot rather than to a person: a launch whose lane a
    /// pass sees *working* did not die at launch, whatever becomes of it
    /// afterwards.
    ///
    /// And the line the rule is drawn at. A *settled* lane that never reported
    /// is precisely what a dying agent binary looks like from here — so that one
    /// keeps its count, and if its pane goes too the guard is still there.
    #[test]
    fn a_working_lane_forgives_the_launch_counter_and_a_settled_one_does_not() {
        let repo = fixture("launch-landed");
        let working = add_task_with(&repo, "busy", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.attempts = MAX_LAUNCHES;
            f.launched_at = Some(now_secs());
        });
        let settled = add_task_with(&repo, "quiet", "implement", |f| {
            f.workspace_id = Some("w2".into());
            f.pane_id = Some("w2:p1".into());
            f.attempts = MAX_LAUNCHES;
            f.launched_at = Some(now_secs());
        });

        let mux = FakeMux::new(vec![
            lane(&repo, "busy · implement", LaneStatus::Working),
            lane(&repo, "quiet · implement", LaneStatus::Done),
        ]);
        run_pass(&repo, &mux);
        assert_eq!(reload(&working).front.attempts, 0);
        assert_eq!(reload(&settled).front.attempts, MAX_LAUNCHES);
    }

    /// An interrupted lane spent exactly as many tokens as one that finished,
    /// and they used to be discarded with the worktree — the ledger held nothing
    /// at all for a task the stop swept.
    #[test]
    fn stopping_a_run_banks_what_the_interrupted_lane_spent() {
        let repo = fixture("stop-banks");
        add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(repo.root.clone());
        });

        let session = "interrupted-session";
        let home = pi_home_with(session, 4_242);
        let mut records = HashMap::new();
        records.insert(
            lane_name("implement", "demo"),
            LaneRecord {
                session: session.to_string(),
                kind: local_kind(&repo),
                agent: "pi".into(),
                model: "priced-model".into(),
                ..LaneRecord::adopted(now_secs())
            },
        );
        save_lane_records(&repo, &records).unwrap();

        let pipelines = Pipelines::builtin();
        let mux = FakeMux::new(vec![]);
        let mut report = Report::default();
        with_home(&home, || {
            Dispatcher::new(&repo, &pipelines, &mux, false)
                .sweep_on_stop(&mut report)
                .unwrap();
        });

        let banked = crate::usage::read(&repo).unwrap();
        let entry = banked
            .iter()
            .find(|e| e.task == "demo")
            .expect("the interrupted lane banked nothing");
        assert_eq!(entry.step, "implement");
        assert_eq!(entry.tokens.input, 4_242);
        // And its record is gone, so a later dispatcher cannot bank it twice.
        assert!(!load_lane_records(&repo).contains_key("demo · implement"));
    }

    /// The ledger used to be parsed on hot paths every pass — the ceiling
    /// checks, `carried_session`, `readopted`, `lane_session` — whether or not
    /// the pass had any use for it (review findings 33 and the earlier lazy
    /// `usage_banked` work). An empty queue has nothing to bank, nothing to
    /// start and no attended ceiling to check, so both the ledger snapshot and
    /// the banked-totals fold must still be `None` once the pass is done.
    #[test]
    fn a_pass_that_banks_nothing_never_reads_the_ledger() {
        let repo = fixture("lazy-usage-banked");
        let mux = FakeMux::new(vec![]);
        let pipelines = Pipelines::builtin();
        let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        dispatcher.pass().unwrap();
        assert!(
            dispatcher.ledger.is_none(),
            "a pass with nothing to bank must never read the ledger at all"
        );
        assert!(dispatcher.usage_banked.is_none());
    }

    /// A lane re-adopted across a dispatcher restart still banks its tokens.
    ///
    /// `lanes.json` is what a dispatcher that dies mid-pass loses — see
    /// `LaneRecord::readopted` — so this seeds none at all, the same blank
    /// slate a restarted dispatcher's own `Dispatcher::new` would load. What
    /// survives the crash is the usage ledger's own earlier line for this
    /// exact lane, which is the one thing `readopted` has to work with: its
    /// `session`, `kind`, `agent` and `model`, recovered by lane name alone.
    #[test]
    fn a_lane_re_adopted_across_a_restart_still_banks_its_tokens() {
        let repo = fixture("readopted-banks");
        let task = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(repo.root.clone());
        });
        let task = reload(&task);

        let session = "readopted-session";
        write_entry(
            &repo,
            "demo",
            "implement",
            &local_kind(&repo),
            "priced-model",
            session,
        );
        // No `lanes.json` at all — `LaneRecord::readopted` has nothing but
        // the ledger line just written to go on.
        assert!(load_lane_records(&repo).is_empty());

        let home = pi_home_with(session, 4_242);
        let lane = lane(&repo, "demo · implement", LaneStatus::Blocked);
        let pipelines = Pipelines::builtin();
        let mux = FakeMux::new(vec![]);
        let mut report = Report::default();
        with_home(&home, || {
            Dispatcher::new(&repo, &pipelines, &mux, false).hold_for_block(
                &lane,
                "demo",
                "implement",
                Some(&task),
                &mut report,
            );
        });

        let banked = crate::usage::read(&repo).unwrap();
        assert_eq!(
            banked.iter().filter(|e| e.task == "demo").count(),
            2,
            "the seeded line and the readopted lane's own bank, not one swallowed for lack \
             of a `lanes.json` record: {banked:?}"
        );
        let entry = banked
            .iter()
            .rfind(|e| e.task == "demo")
            .expect("the readopted lane banked nothing");
        assert_eq!(entry.session, session);
        assert_eq!(entry.tokens.input, 4_242);
    }

    /// Silence is what the model has said, not what the screen is doing —
    /// still true with the busy-lane watchdog gone, since [`note_progress`]
    /// is unchanged and [`Dispatcher::check_unreported`] is its one remaining
    /// reader.
    ///
    /// The pane says nothing at all throughout (`FakeMux::read` is empty and
    /// unchanging, which is the *most* silent a pane can be), so a screen
    /// reading would judge this lane stuck from the first pass. Reading the
    /// transcript instead gives it the same patience any settled lane gets: a
    /// reminder once it is due, and only blocked once nothing follows the
    /// reminder either.
    #[test]
    fn silence_is_measured_from_the_transcript_and_not_from_the_pane() {
        let repo = fixture("silence-transcript");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });
        let session = "talking-session";
        let home = pi_home_with(session, 10);
        let transcript = home
            .join(".pi/agent/sessions/--home-someone-work--")
            .join(format!("2026-08-04T06-14-15-743Z_{session}.jsonl"));

        let record = LaneRecord {
            session: session.to_string(),
            kind: local_kind(&repo),
            agent: "pi".into(),
            model: "priced-model".into(),
            ..LaneRecord::adopted(now_secs())
        };
        let mut records = HashMap::new();
        records.insert(lane_name("implement", "demo"), record);
        save_lane_records(&repo, &records).unwrap();

        let pipelines = Pipelines::builtin();
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        with_home(&home, || {
            Dispatcher::new(&repo, &pipelines, &mux, false)
                .pass()
                .unwrap();
        });
        assert_eq!(
            reload(&path).stage(),
            "implement",
            "a lane whose transcript was written a moment ago is not judged silent"
        );

        // The transcript stops growing: now it is stuck, and the same clock
        // says so. `last_progress` only ever moves forward — the pass above
        // set it to the write it saw — so the wait has to be put back the
        // same way a real day of it would.
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(86_400);
        std::fs::File::options()
            .write(true)
            .open(&transcript)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        age_lane(&repo, "demo · implement", Duration::from_secs(86_400));

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        with_home(&home, || {
            Dispatcher::new(&repo, &pipelines, &mux, false)
                .pass()
                .unwrap();
        });
        assert_eq!(
            reload(&path).stage(),
            "implement",
            "due for the first time — reminded, not yet blocked"
        );

        // Nothing follows the reminder, and there is no further clock to wait
        // out: the very next pass blocks it.
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Done)]);
        with_home(&home, || {
            Dispatcher::new(&repo, &pipelines, &mux, false)
                .pass()
                .unwrap();
        });
        assert_eq!(reload(&path).stage(), crate::pipeline::BLOCKED);
    }

    /// A pipeline whose `implement` and `fix` share the prompt `implementer`
    /// — the shape the plan itself uses — with `fix` carrying `session:
    /// true`. `implement` names none of its own, so a ledger entry written
    /// under it is exactly what a real first visit would leave behind.
    fn session_pipelines() -> Pipelines {
        let yaml = "steps:\n  \
             - id: implement\n    agent: pi\n    prompt: implementer\n    \
               model: test-model\n    on_pass: fix\n  \
             - id: fix\n    agent: pi\n    prompt: implementer\n    \
               model: test-model\n    session: true\n    on_pass: closeout\n  \
             - id: closeout\n    end: true\n";
        let pipeline = crate::pipeline::Pipeline::parse("default", yaml).unwrap();
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert("default".into(), pipeline);
        pipelines
    }

    /// A scratch home with a pi transcript whose one turn sums to `size`
    /// tokens, laid out the way pi shards its sessions — the fixture the
    /// size check needs, since checking it is now unconditional on every
    /// carried session and so requires an actual transcript rather than a
    /// proxy for one.
    fn pi_home_with(session: &str, size: u64) -> PathBuf {
        let root = crate::scratch::root(&format!("dispatch-oversize-{session}"));
        let dir = root.join(".pi/agent/sessions/--home-someone-work--");
        std::fs::create_dir_all(&dir).unwrap();
        let line = format!(
            "{{\"type\":\"message\",\"id\":\"1\",\"message\":{{\"role\":\"assistant\",\
             \"content\":[],\"model\":\"priced-model\",\"usage\":{{\"input\":{size},\
             \"output\":1,\"cacheRead\":0,\"cacheWrite\":0}}}}}}\n"
        );
        std::fs::write(
            dir.join(format!("2026-08-04T06-14-15-743Z_{session}.jsonl")),
            line,
        )
        .unwrap();
        root
    }

    /// A scratch home with a pi transcript whose last record is the marker
    /// `abort_marker` established for pi — the exact record `usage.rs`'s own
    /// fixture tests use, quoted again here rather than shared, since a test
    /// module's fixtures are not each other's to import.
    fn pi_home_aborted(session: &str) -> PathBuf {
        let root = crate::scratch::root(&format!("dispatch-abort-{session}"));
        let dir = root.join(".pi/agent/sessions/--home-someone-work--");
        std::fs::create_dir_all(&dir).unwrap();
        let line = r#"{"type":"message","id":"u2","parentId":"1","timestamp":"2026-08-04T06:15:00.000Z","message":{"role":"assistant","content":[],"stopReason":"aborted","errorMessage":"Operation aborted"}}"#;
        std::fs::write(
            dir.join(format!("2026-08-04T06-14-15-743Z_{session}.jsonl")),
            format!("{line}\n"),
        )
        .unwrap();
        root
    }

    /// A scratch home with one claude transcript whose store has sat
    /// `elapsed` seconds since it was last touched — the fixture the `Stale`
    /// miss needs, since the horizon is now read off the store's own mtime
    /// rather than a timestamp inside the record.
    fn claude_home_with(session: &str, elapsed: i64, size: u64) -> PathBuf {
        let root = crate::scratch::root(&format!("dispatch-warmth-{session}"));
        let dir = root.join(".claude/projects/-home-someone-work");
        std::fs::create_dir_all(&dir).unwrap();
        let line = format!(
            "{{\"type\":\"assistant\",\"requestId\":\"r1\",\
             \"message\":{{\"model\":\"priced-model\",\"usage\":{{\"input_tokens\":{size},\
             \"output_tokens\":1,\"cache_read_input_tokens\":0,\
             \"cache_creation_input_tokens\":100,\"cache_creation\":{{\
             \"ephemeral_5m_input_tokens\":100,\"ephemeral_1h_input_tokens\":0}}}}}}}}\n"
        );
        let path = dir.join(format!("{session}.jsonl"));
        std::fs::write(&path, line).unwrap();
        let touched = std::time::SystemTime::now() - Duration::from_secs(elapsed.max(0) as u64);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(touched)
            .unwrap();
        root
    }

    /// Swap `$HOME` for the duration of `f`, so a transcript fixture written
    /// under a scratch home is the one the lookup under test actually finds.
    /// See [`crate::platform::test_home`] — one lock for every module that
    /// needs this, `$HOME` being process-global and tests running in
    /// parallel by default.
    use crate::platform::test_home::with_home;

    /// A model with a window wide enough that `size` tokens never comes near
    /// the explicit `session_reuse_ctx` used by size-bound tests, so a test
    /// can add a transcript fixture without also reasoning about that bound.
    fn priced(repo: &mut Repo, model: &str) {
        repo.config.models.insert(
            model.to_string(),
            crate::usage::ModelPrice {
                context_window: 1_000_000,
                ..Default::default()
            },
        );
    }

    /// `lane_session`'s usage-ledger fallback — for a lane whose own record a
    /// later pass has already retired — composes the entry's step and task
    /// with [`lane_name`] rather than a hand-rolled join, so it still matches
    /// a `<task> · <step>` lane name rather than silently finding nothing.
    #[test]
    fn a_retired_lanes_session_is_still_found_by_its_ledger_entry() {
        let repo = fixture("session-retired");
        write_entry(&repo, "demo", "fix", "claude", "test-model", "old-session");

        let found = lane_session(&repo, &lane_name("fix", "demo"));

        assert_eq!(
            found,
            Some(("claude".to_string(), "old-session".to_string()))
        );
    }

    /// The standing form: `fix` is the same prompt as `implement`, so its
    /// next visit is the same conversation rather than a fresh one. The
    /// default zero ceiling does not require a model window just to reuse it.
    #[test]
    fn a_zero_reuse_ceiling_resumes_its_prompts_earlier_conversation() {
        let repo = fixture("session-carry");
        assert_eq!(repo.config.agents["pi"].session_reuse_ctx, 0);
        add_task(&repo, "demo", "fix");
        let kind = local_kind(&repo);
        write_entry(
            &repo,
            "demo",
            "implement",
            &kind,
            "test-model",
            "carried-session",
        );

        let home = pi_home_with("carried-session", 10);
        with_home(&home, || {
            Dispatcher::new(&repo, &session_pipelines(), &FakeMux::new(vec![]), false)
                .pass()
                .unwrap();
        });
        std::fs::remove_dir_all(&home).ok();

        assert_eq!(
            load_lane_records(&repo)["demo · fix"].session,
            "carried-session"
        );
    }

    /// An enabled size bound needs a model with a known window to measure against —
    /// a model neither `[models]` nor either shared price table prices has nothing
    /// to compare the transcript to, so the step opens fresh rather than
    /// guessing. A zero bound is the explicit unbounded form and is covered
    /// separately above.
    // covers: agents.<profile>.session_reuse_ctx — how large a carried session may be before a fresh one opens instead
    // covers: models.<glob>.context_window — what one session of this model gets to work in
    #[test]
    fn a_session_key_falls_through_when_the_models_window_is_unset() {
        let mut repo = fixture("session-window-unset");
        // An enabled percentage needs a window to measure against. The
        // default zero does not, as the test above demonstrates.
        repo.config.agents.get_mut("pi").unwrap().session_reuse_ctx = 50;
        add_task(&repo, "demo", "fix");
        let kind = local_kind(&repo);
        write_entry(
            &repo,
            "demo",
            "implement",
            &kind,
            "test-model",
            "carried-session",
        );

        let report = Dispatcher::new(&repo, &session_pipelines(), &FakeMux::new(vec![]), false)
            .pass()
            .unwrap();

        assert_ne!(
            load_lane_records(&repo)["demo · fix"].session,
            "carried-session"
        );
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("context window is not set")),
            "{:?}",
            report.actions
        );

        // And it survives the pass. The action line is a log line under
        // `--plain` and a `RECENT` row under the board, gone within a pass or
        // two either way — so *why an expensive conversation was not reused* had
        // no answer available after the fact at all. Written to the task, it is
        // where somebody auditing one already looks.
        let log = repo
            .task("demo")
            .unwrap()
            .section("## Status Log")
            .unwrap_or_default()
            .to_string();
        assert!(log.contains("context window is not set"), "{log}");
        assert!(log.contains("`fix`"), "{log}");
    }

    /// No earlier ledger entry names this prompt on this task at all — the
    /// ordinary case for a task's first pass through a `session:` step.
    #[test]
    fn a_session_key_falls_through_when_no_earlier_session_is_found() {
        let repo = fixture("session-not-found");
        add_task(&repo, "demo", "fix");

        let report = Dispatcher::new(&repo, &session_pipelines(), &FakeMux::new(vec![]), false)
            .pass()
            .unwrap();

        assert!(!load_lane_records(&repo)["demo · fix"].session.is_empty());
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("no readable earlier `implementer` session")),
            "{:?}",
            report.actions
        );
    }

    /// A step that sets no `session:` of its own must not go asking by
    /// prompt just because an earlier step happens to share one — the whole
    /// of what criterion 5 asks for.
    #[test]
    fn a_step_with_no_session_key_ignores_an_earlier_prompt_match() {
        let repo = fixture("session-ordinary");
        add_task(&repo, "demo", "implement");
        let kind = local_kind(&repo);
        // `fix` is `implement`'s own prompt match, but `implement` itself
        // carries no `session:` in this pipeline.
        write_entry(&repo, "demo", "fix", &kind, "test-model", "carried-session");

        Dispatcher::new(&repo, &session_pipelines(), &FakeMux::new(vec![]), false)
            .pass()
            .unwrap();

        assert_ne!(
            load_lane_records(&repo)["demo · implement"].session,
            "carried-session"
        );
    }

    /// The one-shot `resume:` is the more specific instruction — a task sent
    /// back to an exact step it stopped on — and must win even when the
    /// step's own standing `session:` would have found a different session by
    /// prompt.
    #[test]
    fn the_one_shot_resume_flag_wins_over_a_standing_session() {
        let repo = fixture("session-one-shot-wins");
        let path = add_task_with(&repo, "demo", "fix", |f| {
            f.resume = Some("fix".into());
        });
        let kind = local_kind(&repo);
        record_lane(&repo, "demo · fix", "one-shot-session", &kind);
        write_entry(
            &repo,
            "demo",
            "implement",
            &kind,
            "test-model",
            "carried-session",
        );

        Dispatcher::new(&repo, &session_pipelines(), &FakeMux::new(vec![]), false)
            .pass()
            .unwrap();

        assert_eq!(
            load_lane_records(&repo)["demo · fix"].session,
            "one-shot-session",
            "the one-shot flag names an exact session; a prompt match found \
             along the way must not override it"
        );
        assert_eq!(reload(&path).front.resume, None);
    }

    /// Distinct wording for a distinct reason: nobody was in between for a
    /// carried session, which is the one thing [`carry_prompt`] says and
    /// [`resume_prompt`] never does.
    #[test]
    fn a_carried_session_reads_a_different_prompt_than_an_unblocked_one() {
        let repo = fixture("session-prompt");
        let task = reload(&add_task(&repo, "demo", "fix"));
        let pipelines = session_pipelines();
        let pipeline = pipelines.get("default").unwrap();

        let carried = crate::compose::carry_prompt(&repo, &task, pipeline);
        let unblocked = crate::compose::resume_prompt(&repo, &task, pipeline, false);

        assert!(carried.contains("nobody in between"), "{carried}");
        assert!(!carried.contains("unblocked"), "{carried}");
        assert!(
            unblocked.contains("a person has unblocked you"),
            "{unblocked}"
        );
        assert_ne!(carried, unblocked);
    }

    /// The one prompt that must never be shared between the two modes.
    ///
    /// An unattended lane is resumed by the run, not by a person, and nothing
    /// about its situation has changed while it waited. Told that somebody
    /// cleared its path it goes looking through the status log for a fix that
    /// was never made — and the cheapest thing it can then conclude is that
    /// whatever they did must have worked.
    #[test]
    fn an_unattended_resume_is_never_told_that_a_person_fixed_anything() {
        let repo = fixture("unattended-prompt");
        let task = reload(&add_task(&repo, "demo", "fix"));
        let pipelines = session_pipelines();
        let pipeline = pipelines.get("default").unwrap();

        let alone = crate::compose::resume_prompt(&repo, &task, pipeline, true);

        assert!(!alone.contains("a person has"), "{alone}");
        assert!(alone.contains("nobody is coming"), "{alone}");
        // And it carries the job a second, dedicated lane used to be sent to do.
        assert!(alone.contains("Reproduce it"), "{alone}");
        assert!(alone.contains("## Blocker"), "{alone}");
        assert_ne!(
            alone,
            crate::compose::resume_prompt(&repo, &task, pipeline, false)
        );
    }

    /// `resume_prompt`, `carry_prompt` and `park_prompt` no longer repeat the
    /// report contract or the `stage:` warning — both are already in
    /// `system_prompt`, which every resumed lane was also launched with.
    #[test]
    fn resumed_prompts_carry_no_report_form_and_no_stage_warning() {
        let repo = fixture("resumed-prompts-trimmed");
        let task = reload(&add_task(&repo, "demo", "fix"));
        let pipelines = session_pipelines();
        let pipeline = pipelines.get("default").unwrap();

        for prompt in [
            crate::compose::resume_prompt(&repo, &task, pipeline, false),
            crate::compose::resume_prompt(&repo, &task, pipeline, true),
            crate::compose::carry_prompt(&repo, &task, pipeline),
            crate::compose::park_prompt(&repo, &task, pipeline),
        ] {
            for gone in [
                "spoolway report --pass",
                "spoolway report --fail",
                "spoolway report --block",
                "`stage:`",
            ] {
                assert!(!prompt.contains(gone), "`{gone}` still in: {prompt}");
            }
        }
    }

    /// `park_prompt` never says the one thing that would send a continuing
    /// lane looking for an obstacle that was never there — see the note
    /// beside it on why `resume_prompt`'s unattended half must not reach a
    /// park.
    #[test]
    fn a_park_prompt_says_nothing_was_blocked() {
        let repo = fixture("park-prompt");
        let task = reload(&add_task(&repo, "demo", "fix"));
        let pipelines = session_pipelines();
        let pipeline = pipelines.get("default").unwrap();

        let parked = crate::compose::park_prompt(&repo, &task, pipeline);

        assert!(parked.contains("Nothing was blocked"), "{parked}");
        assert!(!parked.contains("## Blocker"), "{parked}");
        assert_ne!(
            parked,
            crate::compose::resume_prompt(&repo, &task, pipeline, false)
        );
        assert_ne!(parked, crate::compose::carry_prompt(&repo, &task, pipeline));
    }

    /// The other half of a park's own resume: an idle lane — none found at
    /// all under this name, exactly what a kept-but-since-closed pane looks
    /// like — still gets the launch, its carried session, and `park_prompt`
    /// on it.
    #[test]
    fn resuming_a_park_into_an_idle_lane_still_gets_park_prompt() {
        let repo = fixture("park-resume-idle");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.parked_from = Some("implement".into());
            f.resume = Some("implement".into());
        });
        record_lane(
            &repo,
            "demo · implement",
            "parked-session",
            &implement_kind(&repo),
        );

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("start"), ["start demo · implement"]);
        let sent = mux.read("demo · implement", 9999).unwrap();
        assert!(
            sent.contains("A person stopped your turn with a keypress"),
            "an idle park's resume must still read `park_prompt`: {sent}"
        );
        let task = reload(&path);
        assert_eq!(task.front.parked_from, None);
        assert_eq!(task.front.resume, None);
    }

    /// The failure `park_prompt`'s own doc warns against: a lane restarted
    /// by hand is not idle, and the report contract must never land on top
    /// of whatever a person just typed into it. The task is un-parked all
    /// the same — nothing is left waiting on a launch that will never
    /// happen — but no launch and no prompt are sent.
    #[test]
    fn resuming_a_park_into_a_busy_lane_sends_nothing() {
        let repo = fixture("park-resume-busy");
        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.parked_from = Some("implement".into());
            f.resume = Some("implement".into());
        });
        record_lane(
            &repo,
            "demo · implement",
            "parked-session",
            &implement_kind(&repo),
        );

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Working)]);
        let report = run_pass(&repo, &mux);

        assert!(
            mux.did("start").is_empty(),
            "a busy lane must not be relaunched"
        );
        assert!(
            mux.did("prompt").is_empty(),
            "and never sent a prompt on top of it"
        );
        let task = reload(&path);
        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.parked_from, None, "un-parked all the same");
        assert_eq!(task.front.resume, None);
        assert!(
            report
                .actions
                .iter()
                .any(|line| line.contains("demo") && line.contains("un-parked")),
            "got {:?}",
            report.actions
        );
    }

    #[test]
    fn exceeds_percent_is_a_bound_on_the_window_not_on_size_alone() {
        assert!(!exceeds_percent(1000, 60, 600));
        assert!(exceeds_percent(1000, 60, 601));
    }

    /// Several entries share the prompt; the newest one is the conversation
    /// worth continuing, not whichever happens to be first in the file.
    #[test]
    fn carried_session_prefers_the_newest_matching_entry() {
        let mut repo = fixture("session-newest");
        priced(&mut repo, "test-model");
        let task = reload(&add_task(&repo, "demo", "fix"));
        let pipelines = session_pipelines();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("fix").unwrap();
        let kind = local_kind(&repo);
        let profile = repo.config.agent("pi").unwrap().clone();

        write_entry(
            &repo,
            "demo",
            "implement",
            &kind,
            "test-model",
            "older-session",
        );
        write_entry(
            &repo,
            "demo",
            "implement",
            &kind,
            "test-model",
            "newer-session",
        );

        let home = pi_home_with("newer-session", 10);
        let (found_kind, session) = with_home(&home, || {
            carried_session(
                &repo,
                pipeline,
                &task,
                step,
                &profile,
                "test-model",
                &crate::usage::read(&repo).unwrap(),
            )
        })
        .expect("an earlier session for this prompt");
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(found_kind, kind);
        assert_eq!(session, "newer-session");
    }

    /// The four misses `carried_session` can report, each reached the way a
    /// real pass would reach it: no entry at all, an entry whose model prices
    /// nowhere, an entry whose model is priced but whose transcript is too
    /// big, and one that is well under size but whose store has sat past the
    /// model's `session_reuse_idle`.
    #[test]
    fn carried_session_reports_which_of_the_four_misses_it_was() {
        let mut repo = fixture("session-misses");
        // Priced separately from `test-model`, which the second assertion
        // below relies on staying unpriced.
        repo.config.models.insert(
            "priced-model".into(),
            crate::usage::ModelPrice {
                context_window: 1000,
                ..Default::default()
            },
        );
        let task = reload(&add_task(&repo, "demo", "fix"));
        let pipelines = session_pipelines();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("fix").unwrap();
        let mut profile = repo.config.agent("pi").unwrap().clone();
        profile.session_reuse_ctx = 60;

        assert_eq!(
            carried_session(
                &repo,
                pipeline,
                &task,
                step,
                &profile,
                "test-model",
                &crate::usage::read(&repo).unwrap()
            ),
            Err(SessionMiss::NotFound)
        );

        let kind = local_kind(&repo);
        write_entry(
            &repo,
            "demo",
            "implement",
            &kind,
            "test-model",
            "carried-session",
        );
        assert_eq!(
            carried_session(
                &repo,
                pipeline,
                &task,
                step,
                &profile,
                "test-model",
                &crate::usage::read(&repo).unwrap()
            ),
            Err(SessionMiss::WindowUnset),
            "`test-model` is priced by neither table, so there is nothing to \
             measure the transcript against"
        );

        // A newer entry under a model that *is* priced: found, and this time
        // there is a window to measure the transcript against — a transcript
        // whose last turn is deliberately past it.
        write_entry(
            &repo,
            "demo",
            "implement",
            &kind,
            "priced-model",
            "oversize-session",
        );
        let home = pi_home_with("oversize-session", 700);
        let result = with_home(&home, || {
            carried_session(
                &repo,
                pipeline,
                &task,
                step,
                &profile,
                "priced-model",
                &crate::usage::read(&repo).unwrap(),
            )
        });
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(
            result,
            Err(SessionMiss::OverSize),
            "700 tokens is past 60% of a 1000-token window"
        );

        // A newer entry still, well under size, but its store has sat 400
        // seconds without moving — past the five-minute `session_reuse_idle`
        // this model declares.
        repo.config
            .models
            .get_mut("priced-model")
            .unwrap()
            .session_reuse_idle = Some(Duration::from_secs(300));
        write_entry(
            &repo,
            "demo",
            "implement",
            "claude",
            "priced-model",
            "stale-session",
        );
        let home = claude_home_with("stale-session", 400, 10);
        let result = with_home(&home, || {
            carried_session(
                &repo,
                pipeline,
                &task,
                step,
                &profile,
                "priced-model",
                &crate::usage::read(&repo).unwrap(),
            )
        });
        assert_eq!(result, Err(SessionMiss::Stale));

        // Unset the horizon, and the same stale store resumes — the only way
        // left to say "carry this regardless of age."
        repo.config
            .models
            .get_mut("priced-model")
            .unwrap()
            .session_reuse_idle = None;
        let resumed = with_home(&home, || {
            carried_session(
                &repo,
                pipeline,
                &task,
                step,
                &profile,
                "priced-model",
                &crate::usage::read(&repo).unwrap(),
            )
        })
        .expect("a session with no idle horizon should resume however old its store");
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(resumed.1, "stale-session");
    }

    /// The end-to-end shape `session_blocked_ctx` exists for: a lane still
    /// running — busy in its pane, not settled — whose last completed turn
    /// has already read past the ceiling. The dispatcher stops it on the
    /// spot rather than wait for it to end its turn on its own, banks what
    /// it spent, and lands the task on `blocked` with a blocker line naming
    /// the reading and the ceiling.
    // covers: agents.<profile>.session_blocked_ctx — the ceiling on a running lane's size
    #[test]
    fn a_live_lane_past_its_ctx_ceiling_is_stopped_and_escalated() {
        let mut repo = fixture("ctx-ceiling-live");
        repo.config.models.insert(
            "test-model".into(),
            crate::usage::ModelPrice {
                context_window: 1000,
                ..Default::default()
            },
        );
        repo.config
            .agents
            .get_mut("pi")
            .unwrap()
            .session_blocked_ctx = 80;

        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        // A lane record with a real kind and session — what a real launch
        // would have written — is what `ctx_ceiling_hold` reads instead of
        // the ledger, since the lane it watches is still running and has not
        // reported anything yet.
        let kind = local_kind(&repo);
        {
            let mut record = LaneRecord::adopted(now_secs());
            record.kind = kind;
            record.session = "ceiling-session".to_string();
            let mut records = HashMap::new();
            records.insert("demo · implement".to_string(), record);
            save_lane_records(&repo, &records).unwrap();
        }

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Working)]);
        // 900 of a 1000-token window is 90%, past the 80% ceiling just set.
        let home = pi_home_with("ceiling-session", 900);
        let report = with_home(&home, || {
            Dispatcher::new(&repo, &session_pipelines(), &mux, false)
                .pass()
                .unwrap()
        });
        std::fs::remove_dir_all(&home).ok();

        let task = reload(&path);
        assert_eq!(task.stage(), "blocked", "stopped rather than left running");
        let blocker = task.section("## Blocker").unwrap_or_default();
        assert!(blocker.contains("90%"), "{blocker}");
        assert!(blocker.contains("session_blocked_ctx"), "{blocker}");
        assert!(blocker.contains("80%"), "{blocker}");
        assert!(blocker.contains("uncommitted"), "{blocker}");
        assert!(blocker.contains("conversation is gone"), "{blocker}");
        // Attended, so the pane is kept for a person to read rather than
        // closed — `tear_down_and_escalate`'s own `hold` — and the lane is
        // brought to the front instead of stopped. But it was busy, and
        // holding the pane must not mean leaving it running past the
        // ceiling: an interrupt ends its turn in place.
        assert!(
            mux.did("focus")
                .iter()
                .any(|c| c.contains("demo · implement")),
            "{:?}",
            mux.calls()
        );
        assert!(
            mux.did("interrupt")
                .iter()
                .any(|c| c.contains("demo · implement")),
            "a busy lane must be stopped even while its pane is held: {:?}",
            mux.calls()
        );
        assert!(
            crate::usage::read(&repo)
                .unwrap_or_default()
                .iter()
                .any(|e| e.session == "ceiling-session"),
            "its usage was banked on the way down"
        );
        assert!(
            report
                .actions
                .iter()
                .any(|line| line.contains("demo") && line.contains("implement")),
            "{:?}",
            report.actions
        );
    }

    /// The exclusion the review found missing: a lane that already reported
    /// is not "live" in the sense the ceiling means, even though its status
    /// still reads settled and its last turn is still over the ceiling.
    /// `blocked` reporting `--block` again is the shipped step that routes
    /// back to itself — the same lane name settled on the same step a pass
    /// later — and the ceiling must not preempt the retire that
    /// `reported_since` already sends it to: escalating here would replace
    /// a real report with a blocker claiming the conversation is gone, and
    /// that claim would be false.
    // covers: agents.<profile>.session_blocked_ctx — a reported lane is not preempted
    #[test]
    fn a_lane_that_already_reported_is_not_caught_by_the_ctx_ceiling() {
        // Unattended, and specifically for this: an attended run's `blocked`
        // is a pane parked in front of a person, which `free_finished_lanes`
        // holds through its own separate `hold_for_block` road before the
        // per-task loop this test means to reach is ever entered — see
        // `parked_for_a_person`. A staffed `blocked` settles like any other
        // step instead, which is the shape `retire` (and so this exclusion)
        // is for.
        let mut repo = unattended_fixture("ctx-ceiling-reported");
        // `blocked` is the shipped step that routes back to itself — a
        // `--block` report leaves the stage exactly where it found it — so
        // `Pipelines::builtin()` already carries the shape this test needs.
        // It assembles `blocked` from `Config::default()` rather than from
        // this fixture's own config, which is what fixes the profile at
        // `claude` and the model at `claude-opus-5` here rather than
        // naming a `test-model` of this test's own choosing.
        repo.config.models.insert(
            "claude-opus-5".into(),
            crate::usage::ModelPrice {
                context_window: 1000,
                ..Default::default()
            },
        );
        repo.config
            .agents
            .get_mut("claude")
            .unwrap()
            .session_blocked_ctx = 80;
        let pipelines = Pipelines::builtin();

        let started_at = now_secs() - 60;
        let path = add_task_with(&repo, "demo", crate::pipeline::BLOCKED, |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            // The report that just landed — after the lane started, which is
            // what tells a reported lane apart from a stuck one.
            f.last_report = Some(crate::task::LastReport {
                step: crate::pipeline::BLOCKED.into(),
                outcome: "block".into(),
                at: started_at + 30,
            });
        });

        {
            let mut record = LaneRecord::adopted(started_at);
            record.kind = "claude".to_string();
            record.session = "ceiling-session-reported".to_string();
            let mut records = HashMap::new();
            records.insert(lane_name(crate::pipeline::BLOCKED, "demo"), record);
            save_lane_records(&repo, &records).unwrap();
        }

        // Settled, not busy — the shape a lane that reported and ended its
        // turn actually has by the time the next pass looks.
        let mux = FakeMux::new(vec![lane(
            &repo,
            &lane_name(crate::pipeline::BLOCKED, "demo"),
            LaneStatus::Done,
        )]);
        // 800 input plus the fixture's own 100-token cache write is 900 of a
        // 1000-token window — 90%, past the 80% ceiling just set.
        let home = claude_home_with("ceiling-session-reported", 10, 800);
        let report = with_home(&home, || {
            Dispatcher::new(&repo, &pipelines, &mux, false)
                .pass()
                .unwrap()
        });
        std::fs::remove_dir_all(&home).ok();

        let task = reload(&path);
        assert_eq!(
            task.stage(),
            crate::pipeline::BLOCKED,
            "the report's own destination, untouched"
        );
        assert!(
            task.section("## Blocker").is_none(),
            "no escalation — the report already answered for this lane"
        );
        assert!(
            report.actions.iter().any(|line| line.contains("freed")),
            "retired like any other reported lane: {:?}",
            report.actions
        );
        assert!(
            !report.actions.iter().any(|line| line.contains("stuck at")),
            "must not be escalated: {:?}",
            report.actions
        );
    }

    /// The unattended path needs nothing extra for this: `escalate_clock`
    /// reaches the same `escalate` a dead lane or a spent reminder budget
    /// does, with no `unattended`-specific branch of its own — so a task the
    /// ceiling blocks reaches the unblocker exactly as any other block would.
    /// Driven on `Pipelines::builtin()`, whose `blocked` step
    /// `Pipelines::assemble` already staffs from `[unattended]`, since that
    /// staffing is the thing under test and a hand-rolled pipeline would only
    /// assert this test's own setup.
    // covers: agents.<profile>.session_blocked_ctx — the ceiling needs no unattended special case
    #[test]
    fn an_unattended_run_needs_no_special_case_for_the_ctx_ceiling() {
        let mut repo = unattended_fixture("ctx-ceiling-unattended");
        // `implement` in the shipped pipeline names the placeholder model —
        // priced here under its own name, standing in for the real local
        // model a project would have named instead.
        repo.config.models.insert(
            "your-local-model".into(),
            crate::usage::ModelPrice {
                context_window: 1000,
                ..Default::default()
            },
        );
        repo.config
            .agents
            .get_mut("pi")
            .unwrap()
            .session_blocked_ctx = 80;

        let path = add_task_with(&repo, "demo", "implement", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
        });

        let kind = local_kind(&repo);
        {
            let mut record = LaneRecord::adopted(now_secs());
            record.kind = kind;
            record.session = "ceiling-session-unattended".to_string();
            let mut records = HashMap::new();
            records.insert("demo · implement".to_string(), record);
            save_lane_records(&repo, &records).unwrap();
        }

        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Working)]);
        let home = pi_home_with("ceiling-session-unattended", 900);
        with_home(&home, || {
            run_pass(&repo, &mux);
        });
        std::fs::remove_dir_all(&home).ok();

        let task = reload(&path);
        assert_eq!(task.stage(), "blocked", "the ordinary road to `blocked`");
        assert_eq!(
            task.front.blocked_from.as_deref(),
            Some("implement"),
            "the step the ceiling stopped, for `resume_target` to carry it back to"
        );

        // A pass later, the dispatcher starts `blocked`'s own lane — the
        // unblocker prompt `[unattended]` staffs it with — the same as it
        // would for any other reason a task landed here. A fresh `FakeMux`
        // with no lanes at all, standing in for the real multiplexer once
        // the `stop_lane` the pass above just logged has actually closed
        // the pane the unattended escalation tore down: `FakeMux`'s own
        // lane list is fixed at construction and does not track its own
        // `stop_lane` calls the way a real backend's `list_lanes` would.
        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);
        assert!(
            mux.calls().iter().any(|call| call.contains("blocked")),
            "the unblocker was staffed: {:?}",
            mux.calls()
        );
        let blocked_lane = load_lane_records(&repo)["demo · blocked"].clone();
        assert_ne!(
            blocked_lane.session, "ceiling-session-unattended",
            "a fresh session, not the one that just blocked — there is no earlier \
             `unblocker` turn on this task for `carried_session` to find"
        );

        // What the unblocker's own `spoolway report --pass` reaches for is
        // `cleared_block_target` — the same call `spoolway resume` and the
        // board both make, and the one place `unattended.skip_blocked_lane`
        // is read. Called directly here rather than simulated through a
        // second lane's report, which is `commands::report`'s own test
        // surface: this is the proof that landing on `blocked` by way of the
        // ceiling put the task in exactly the state that call already knows
        // what to do with, no special case required.
        let pipeline = Pipelines::builtin().get("default").unwrap().clone();
        assert_eq!(
            crate::commands::cleared_block_target(
                &task,
                &pipeline,
                repo.config.unattended.skip_blocked_lane,
            ),
            "review",
            "`implement`'s own `on_pass` — one step past where the ceiling stopped it"
        );
    }

    /// A pull request is its own checkpoint, so nothing shipped stops to ask.
    /// `gate:` survives as a step-level key for a step that changes something no
    /// pull request would show anybody first — the two-level version, where a
    /// step said it *expected* to pause and the pipeline said whether pausing
    /// was policy, went with the second pipeline it existed to reconcile.
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

    /// Nothing says "run in place", and nothing ever should have. What decides
    /// it is git: a branch cannot be checked out twice, so a task whose branch
    /// somebody already has out borrows that checkout rather than being cut a
    /// second one git would refuse.
    ///
    /// This used to be how the plan closeout worked, deliberately. That is gone
    /// — the last task of a plan documents on its own branch — and what is left
    /// is the general case it was a special use of.
    #[test]
    fn a_task_whose_branch_is_already_checked_out_borrows_that_checkout() {
        let repo = fixture("in-place");
        // A document may not set its own `branch:` — see
        // `queue::RESERVED_KEYS` — and this fixture never turns on
        // `issue_tracking.key_in_names`, so the branch is the plain
        // `task/<id>` and the "already checked out" case is the fixture root
        // itself sitting on `task/plan-closeout`.
        crate::repo::run(
            &repo.root,
            "git",
            &["branch", "-m", "work", "task/plan-closeout"],
        )
        .unwrap();
        let path = add_task_with(&repo, "plan-closeout", crate::pipeline::QUEUED, |f| {
            f.branch = Some("task/plan-closeout".into());
        });

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        // No worktree: git already has this branch out somewhere.
        assert!(mux.did("create_workspace").is_empty());
        assert_eq!(
            mux.did("create_pane"),
            ["create_pane spoolway/plan-closeout"],
            "the lane borrows the checkout, in a row labelled like any other task's"
        );
        assert_eq!(mux.did("start"), ["start plan-closeout · implement"]);

        let task = reload(&path);
        assert_eq!(task.stage(), "implement");
        // Its workspace is recorded like any other task's, so cleanup closes
        // it — but as a workspace, never as a worktree: what it points at is
        // the person's own checkout.
        assert!(
            task.front.workspace_id.is_some(),
            "the borrowed workspace is tracked"
        );
        assert_eq!(
            task.front.worktree_path.as_deref().map(same_dir),
            Some(same_dir(&repo.root)),
            "the borrowed checkout is the one the person already had out"
        );
    }

    /// One directory, however the platform spelled it.
    ///
    /// `worktree_path` here is git's answer, and git writes a Windows path its
    /// own way: `C:/Users/runneradmin/…` where the fixture built
    /// `C:\Users\RUNNER~1\…` — forward slashes and the long name against
    /// backslashes and the 8.3 one. Both name the same directory, which is the
    /// claim being made, so comparing the strings tested the spelling instead.
    fn same_dir(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    #[test]
    fn a_task_waits_for_its_dependencies_to_finish() {
        let repo = fixture("deps");
        add_task(&repo, "first", "implement");
        add_task_with(&repo, "second", "queued", |f| {
            f.depends_on = vec!["first".into()];
        });

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        // `first` starts; `second` must not, since `first` is not terminal.
        assert_eq!(mux.did("start"), ["start first · implement"]);
    }

    #[test]
    fn an_archived_dependency_counts_as_finished() {
        let repo = fixture("deps-archived");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("first.md"),
            "---\nid: first\nstage: done\n---\n",
        )
        .unwrap();
        add_task_with(&repo, "second", "queued", |f| {
            f.depends_on = vec!["first".into()];
        });

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("start"), ["start second · implement"]);
    }

    #[test]
    fn the_group_with_least_left_to_do_is_started_first() {
        let mut repo = fixture("group-order");
        // `big-a` would win on task id alone. What decides it is that `small`
        // is one task away from reaching main and `big` is three.
        for id in ["big-a", "big-b", "big-c"] {
            add_task_with(&repo, id, "queued", |f| f.group = Some("big".into()));
        }
        add_task_with(&repo, "small", "queued", |f| f.group = Some("small".into()));

        // Only one slot free this pass.
        repo.config.agents.get_mut("pi").unwrap().concurrency = 1;

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("start"), ["start small · implement"]);
    }

    #[test]
    fn within_one_group_whatever_unblocks_the_most_goes_first() {
        let mut repo = fixture("dependents-order");
        // Same group and same step, so only the graph can separate them — and
        // `zulu` is the one that has to lose on task id to prove it did.
        add_task_with(&repo, "alpha", "queued", |f| f.group = Some("p".into()));
        add_task_with(&repo, "zulu", "queued", |f| f.group = Some("p".into()));
        add_task_with(&repo, "waiter", "queued", |f| {
            f.group = Some("p".into());
            f.depends_on = vec!["zulu".into()];
        });

        repo.config.agents.get_mut("pi").unwrap().concurrency = 1;

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("start"), ["start zulu · implement"]);
    }

    /// Under `dispatch.priority = "group"` — the shipped default — a group
    /// that has never run ranks behind another group that is still able to
    /// move: `dispatcher-ui` already has a task past `queued`, so it counts
    /// as open, and `slot-priority` loses the tie for the one free slot
    /// while `dispatcher-ui` can still land — the gate no longer drops it
    /// from the pass, it just sorts last.
    #[test]
    fn a_never_run_group_ranks_behind_an_open_group_that_can_still_move() {
        let mut repo = fixture("gate-hold");
        add_task_with(&repo, "billing", "implement", |f| {
            f.group = Some("dispatcher-ui".into());
        });
        add_task_with(&repo, "gate-filter", "queued", |f| {
            f.group = Some("slot-priority".into());
        });

        // Only one slot free this pass, so the gate's ranking is the only
        // thing that can decide which of the two starts.
        repo.config.agents.get_mut("pi").unwrap().concurrency = 1;

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(
            mux.did("start"),
            ["start billing · implement"],
            "gate-filter must rank behind dispatcher-ui while it can still move"
        );
    }

    /// The gate only ranks a candidate behind an open group while that open
    /// group has something free to move on — an open group whose only task
    /// is parked on `blocked` has nothing left to rank ahead of anything, so
    /// the untouched group's task sorts first and gets the slot.
    #[test]
    fn the_gate_has_nothing_to_rank_against_once_the_open_groups_only_task_is_blocked() {
        let repo = fixture("gate-release");
        add_task_with(&repo, "billing", crate::pipeline::BLOCKED, |f| {
            f.group = Some("dispatcher-ui".into());
        });
        add_task_with(&repo, "gate-filter", "queued", |f| {
            f.group = Some("slot-priority".into());
        });

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("start"), ["start gate-filter · implement"]);
    }

    /// A task with no `group:` has no siblings holding up a pull request, so
    /// the gate never ranks it behind anything — whether or not it is
    /// ranking something else behind an open group at the same moment.
    #[test]
    fn an_ungrouped_task_starts_whether_or_not_the_gate_is_ranking_anything() {
        // The gate is actively ranking `slot-priority` behind `dispatcher-ui`
        // here, and the ungrouped task starts anyway.
        let holding = fixture("gate-ungrouped-holding");
        add_task_with(&holding, "billing", "implement", |f| {
            f.group = Some("dispatcher-ui".into());
        });
        add_task_with(&holding, "gate-filter", "queued", |f| {
            f.group = Some("slot-priority".into());
        });
        add_task_with(&holding, "loner", "queued", |_| {});
        let mux = FakeMux::new(vec![]);
        run_pass(&holding, &mux);
        assert!(
            mux.did("start")
                .contains(&"start loner · implement".to_string()),
            "{:?}",
            mux.did("start")
        );

        // The gate has nothing to rank here — `dispatcher-ui`'s only task is
        // blocked — and the ungrouped task still starts exactly the same way.
        let released = fixture("gate-ungrouped-released");
        add_task_with(&released, "billing", crate::pipeline::BLOCKED, |f| {
            f.group = Some("dispatcher-ui".into());
        });
        add_task_with(&released, "loner", "queued", |_| {});
        let mux = FakeMux::new(vec![]);
        run_pass(&released, &mux);
        assert!(
            mux.did("start")
                .contains(&"start loner · implement".to_string()),
            "{:?}",
            mux.did("start")
        );
    }

    /// The change this task makes: an open group that has run out of ready
    /// work no longer holds the whole profile cap idle. One lane is already
    /// running for `dispatcher-ui`, mid-turn, and it has no further ready
    /// task of its own — so the two free slots the profile cap still has go
    /// to two groups that have never run, exactly what the old
    /// `candidates.retain` used to drop from the pass outright. This is the
    /// test that fails against that `retain`.
    #[test]
    fn an_open_groups_free_slots_go_to_never_run_groups_rather_than_idling() {
        let mut repo = fixture("gate-fills-idle-slots");
        // `dispatcher-ui` is open — a live lane of its own, mid-turn — but
        // has no further ready task, so nothing of its own group is left to
        // fill the two slots its one lane does not use.
        add_task_with(&repo, "billing", "implement", |f| {
            f.group = Some("dispatcher-ui".into());
        });
        add_task_with(&repo, "slot-priority-a", "queued", |f| {
            f.group = Some("slot-priority".into());
        });
        add_task_with(&repo, "ctx-ceiling-a", "queued", |f| {
            f.group = Some("ctx-ceiling".into());
        });

        repo.config.agents.get_mut("pi").unwrap().concurrency = 3;

        let mux = FakeMux::new(vec![lane(
            &repo,
            "billing · implement",
            LaneStatus::Working,
        )]);
        run_pass(&repo, &mux);

        let mut started = mux.did("start");
        started.sort();
        assert_eq!(
            started,
            [
                "start ctx-ceiling-a · implement",
                "start slot-priority-a · implement",
            ]
        );
    }

    #[test]
    fn a_dependency_that_can_never_finish_is_left_alone_rather_than_waited_on() {
        let repo = fixture("stranded");
        let root = add_task(&repo, "root", "blocked");
        let leaf = add_task_with(&repo, "leaf", "queued", |f| {
            f.depends_on = vec!["root".into()]
        });

        let mux = FakeMux::new(vec![]);
        let report = run_pass(&repo, &mux);

        // Not reported to the ticker: the leaf's own row already reads
        // `waiting on root, which is blocked — spoolway resume root` in the
        // board's NEXT column, through the same `dependency_note`, and a wait
        // that is still waiting next pass is not news.
        assert!(mux.did("start").is_empty());
        assert!(
            report.problems.is_empty(),
            "problems: {:?}",
            report.problems
        );
        // Neither file was rewritten: unblocking the root is meant to be all it
        // takes to release everything behind it.
        assert_eq!(reload(&root).stage(), "blocked");
        assert_eq!(reload(&leaf).stage(), "queued");
    }

    #[test]
    fn a_dependency_cycle_is_reported_rather_than_waited_on_forever() {
        let repo = fixture("cycle");
        add_task_with(&repo, "a", "queued", |f| f.depends_on = vec!["b".into()]);
        add_task_with(&repo, "b", "queued", |f| f.depends_on = vec!["a".into()]);

        let mux = FakeMux::new(vec![]);
        let report = run_pass(&repo, &mux);

        // Neither task starts — nothing in a cycle can ever be ready. Not
        // reported to the ticker either: both rows already read `Unreachable`
        // on the board, through the same `dependency_note` that names the
        // cycle, and a deadlock that is still a deadlock next pass is not
        // news.
        assert!(mux.did("start").is_empty());
        assert!(
            report.problems.is_empty(),
            "problems: {:?}",
            report.problems
        );
    }

    #[test]
    fn cutting_a_workspace_mints_a_run_and_pins_the_base_commit() {
        let repo = fixture("run-and-base-commit");
        let mut task = reload(&add_task(&repo, "demo", "implement"));
        let mux = FakeMux::new(vec![]);

        ensure_workspace(&repo, &mux, &mut task, &Default::default()).unwrap();

        assert!(
            task.front.run.is_some(),
            "a run is minted when a worktree is cut"
        );
        let expected = repo.git(&["rev-parse", "work"]).unwrap().trim().to_string();
        assert_eq!(task.front.base_commit.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn a_borrowed_checkout_mints_a_run_but_no_base_commit() {
        let repo = fixture("run-borrowed");
        let worktree = crate::scratch::root("dispatch-run-borrowed-wt");
        let _ = std::fs::remove_dir_all(&worktree);
        // A worktree already has `task/demo` out, which is what makes the
        // next call a borrow rather than a cut.
        repo.git(&[
            "worktree",
            "add",
            "-b",
            "task/demo",
            worktree.to_str().unwrap(),
        ])
        .unwrap();
        let mut task = reload(&add_task(&repo, "demo", "implement"));
        let mux = FakeMux::new(vec![]);

        ensure_workspace(&repo, &mux, &mut task, &Default::default()).unwrap();

        assert!(task.front.borrowed);
        assert!(
            task.front.run.is_some(),
            "a run is still minted on a borrow"
        );
        assert_eq!(
            task.front.base_commit, None,
            "nothing here witnessed where a borrowed checkout was cut from"
        );

        std::fs::remove_dir_all(&worktree).ok();
    }

    /// A multiplexer that restarted while the worktree survived — the whole
    /// point of this task. The directory is still there, so the existing
    /// missing-directory check never fires; only the recorded workspace is
    /// stale, and it is a fresh pane on the same worktree that fixes it, not
    /// a re-cut.
    #[test]
    fn a_forgotten_workspace_gets_a_fresh_pane_on_the_same_worktree() {
        let repo = fixture("forgotten-workspace");
        let worktree = crate::scratch::root("dispatch-forgotten-workspace-wt");
        let _ = std::fs::remove_dir_all(&worktree);
        std::fs::create_dir_all(&worktree).unwrap();

        let mut task = reload(&add_task_with(&repo, "demo", "implement", |f| {
            f.branch = Some("task/demo".into());
            f.base = Some("work".into());
            f.borrowed = false;
            f.worktree_path = Some(worktree.clone());
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.tab_id = Some("w1:t1".into());
        }));
        // Only the workspace, never the tab: `workspace_alive` still has to
        // answer `false` off the workspace half alone.
        let mux = FakeMux::new(vec![]).forgetting("w1");

        ensure_workspace(&repo, &mux, &mut task, &Default::default()).unwrap();

        assert_eq!(
            task.front.worktree_path,
            Some(worktree.clone()),
            "the worktree itself was never in question"
        );
        assert_eq!(
            task.front.branch.as_deref(),
            Some("task/demo"),
            "healing a stale workspace is not a re-cut, so the branch is untouched"
        );
        assert!(
            !task.front.borrowed,
            "a task that owns its worktree must not come out of healing looking borrowed"
        );
        assert_eq!(task.front.workspace_id.as_deref(), Some("w0"));
        assert_eq!(task.front.pane_id.as_deref(), Some("w0:p9"));
        assert_eq!(task.front.tab_id.as_deref(), Some("w0:t1"));
        assert_eq!(
            mux.did("create_pane"),
            vec!["create_pane spoolway/demo".to_string()],
            "a pane opened on the worktree that already exists, never a new checkout"
        );
        assert!(
            mux.did("create_workspace").is_empty(),
            "nothing here should ever cut a worktree that is still standing"
        );

        std::fs::remove_dir_all(&worktree).ok();
    }

    /// The other half of the same check: a workspace the mux still knows
    /// about is left exactly as it is, so a healthy pass never opens a second
    /// pane behind a task's back.
    #[test]
    fn a_workspace_the_mux_still_knows_is_left_alone() {
        let repo = fixture("live-workspace");
        let worktree = crate::scratch::root("dispatch-live-workspace-wt");
        let _ = std::fs::remove_dir_all(&worktree);
        std::fs::create_dir_all(&worktree).unwrap();

        let mut task = reload(&add_task_with(&repo, "demo", "implement", |f| {
            f.branch = Some("task/demo".into());
            f.base = Some("work".into());
            f.worktree_path = Some(worktree.clone());
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.tab_id = Some("w1:t1".into());
        }));
        let mux = FakeMux::new(vec![]);

        ensure_workspace(&repo, &mux, &mut task, &Default::default()).unwrap();

        assert_eq!(task.front.workspace_id.as_deref(), Some("w1"));
        assert_eq!(task.front.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(task.front.tab_id.as_deref(), Some("w1:t1"));
        assert!(
            mux.did("create_pane").is_empty() && mux.did("create_workspace").is_empty(),
            "a workspace still alive must never be re-opened, or every pass would cut a fresh \
             one: {:?}",
            mux.calls()
        );
        assert!(
            !mux.did("verify_workspace").is_empty(),
            "the check has to actually ask the multiplexer, not just assume"
        );

        std::fs::remove_dir_all(&worktree).ok();
    }

    #[test]
    fn a_bare_diff_reads_as_an_empty_patch() {
        assert_eq!(parse_shortstat(""), Some(crate::task::Patch::default()));
    }

    #[test]
    fn shortstat_is_parsed_however_many_of_its_three_clauses_are_present() {
        assert_eq!(
            parse_shortstat(" 4 files changed, 168 insertions(+), 44 deletions(-)"),
            Some(crate::task::Patch {
                files: 4,
                insertions: 168,
                deletions: 44
            })
        );
        assert_eq!(
            parse_shortstat(" 1 file changed, 3 insertions(+)"),
            Some(crate::task::Patch {
                files: 1,
                insertions: 3,
                deletions: 0
            })
        );
        assert_eq!(
            parse_shortstat(" 2 files changed, 5 deletions(-)"),
            Some(crate::task::Patch {
                files: 2,
                insertions: 0,
                deletions: 5
            })
        );
    }

    #[test]
    fn cleaning_up_measures_the_patch_against_base_commit_and_it_survives_into_the_archive() {
        let repo = fixture("patch-on-cleanup");
        // Beside the fixture root rather than inside it, so cleaning the
        // worktree up is not the same act as cleaning the repository up — and
        // by its own name, not a sibling of one, or two `cargo test`
        // processes would be adding the same worktree at once.
        let worktree = crate::scratch::root("dispatch-patch-on-cleanup-wt");
        let _ = std::fs::remove_dir_all(&worktree);
        let base_commit = repo.git(&["rev-parse", "work"]).unwrap().trim().to_string();
        repo.git(&[
            "worktree",
            "add",
            "-b",
            "task/demo",
            worktree.to_str().unwrap(),
        ])
        .unwrap();
        std::fs::write(worktree.join("new-file.txt"), "one\ntwo\nthree\n").unwrap();
        for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", "add a file"]] {
            crate::repo::run(&worktree, "git", &args).unwrap();
        }

        let path = add_task_with(&repo, "demo", "done", |f| {
            f.workspace_id = Some("w1".into());
            f.branch = Some("task/demo".into());
            f.base_commit = Some(base_commit);
            f.worktree_path = Some(worktree.clone());
        });

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        let archived = crate::task::Task::load(&repo.archive_dir().join("demo.md")).unwrap();
        assert_eq!(
            archived.front.patch,
            Some(crate::task::Patch {
                files: 1,
                insertions: 3,
                deletions: 0
            })
        );
        assert!(!path.exists());
        std::fs::remove_dir_all(&worktree).ok();
    }

    /// No fixture in this module may cut a worktree inside the person's own
    /// home directory.
    ///
    /// It used to. `crate::mux::worktree_root` falls back to
    /// `~/.spoolway/<basename>/worktrees` when a config names no root, and a
    /// fixture root's basename carries a fresh process id every run — so the
    /// fallback pointed somewhere new each time and no run could ever reuse or
    /// clean what the last one made. Two tests here reached that path and left
    /// 945 directories in one developer's home before anybody looked.
    ///
    /// `fixture` names a root of its own now. This is the test that fails if
    /// that line is ever removed, rather than the home filling up again in
    /// silence.
    #[test]
    fn no_fixture_cuts_a_worktree_in_the_real_home() {
        let repo = fixture("home-leak-guard");
        let cut = crate::mux::worktree_root(&repo.root, &repo.config.dispatch);

        let real_home = crate::mux::home();
        assert!(
            !cut.starts_with(real_home.join(".spoolway")),
            "a fixture would cut worktrees in the real home: {}",
            cut.display()
        );
        assert!(
            cut.starts_with(std::env::temp_dir()),
            "a fixture's worktree root belongs under the temporary directory, not {}",
            cut.display()
        );
        assert!(
            !cut.starts_with(&repo.root),
            "a cut checkout must stay outside the checkout it came from: {}",
            cut.display()
        );
    }

    /// A dependent's worktree is cut from its dependency's branch, not from
    /// `base:` — the whole point of this task. `base` keeps its own meaning
    /// regardless, and the new worktree already carries what the dependency
    /// left behind.
    #[test]
    fn a_dependent_is_cut_from_its_dependencys_branch_not_base() {
        let repo = fixture("cut-from-dependency");
        // Outside `repo.root`, because a checkout git treats as separate does
        // not belong inside the one it was cut from — `fixture` names the
        // sibling this lands in, and
        // `no_fixture_cuts_a_worktree_in_the_real_home` is what keeps it out
        // of the real `~/.spoolway/`.
        let worktree = crate::mux::worktree_root(&repo.root, &repo.config.dispatch).join("second");
        let _ = std::fs::remove_dir_all(&worktree);

        // A finished dependency's branch: real commits, exactly what `done`
        // leaves behind.
        repo.git(&["checkout", "-q", "-b", "task/first"]).unwrap();
        std::fs::write(repo.root.join("first.txt"), "from first\n").unwrap();
        crate::repo::run(&repo.root, "git", &["add", "first.txt"]).unwrap();
        repo.git(&["commit", "-q", "-m", "first's work"]).unwrap();
        repo.git(&["checkout", "-q", "work"]).unwrap();

        // The dependency's own task file: the cut reads its `branch:` rather
        // than rebuilding `task/first` from the id.
        add_task_with(&repo, "first", "done", |f| {
            f.branch = Some("task/first".into());
        });
        let mut task = reload(&add_task_with(&repo, "second", "implement", |f| {
            f.depends_on = vec!["first".into()];
        }));
        // A real cut, with git, rather than `FakeMux`'s stand-in workspace —
        // the point here is what the worktree actually contains.
        let mux = FakeMux::new(vec![]).tabs_in_one_workspace();

        ensure_workspace(&repo, &mux, &mut task, &Default::default()).unwrap();

        assert_eq!(task.front.cut_from.as_deref(), Some("task/first"));
        assert_eq!(
            task.front.base.as_deref(),
            Some("work"),
            "`base` still means where the plan lands, not what the worktree was cut from"
        );
        let expected = repo
            .git(&["rev-parse", "task/first"])
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(task.front.base_commit.as_deref(), Some(expected.as_str()));

        assert_eq!(
            task.front.worktree_path.as_deref(),
            Some(worktree.as_path())
        );
        assert!(
            worktree.join("first.txt").exists(),
            "the dependent's worktree already carries the dependency's work, ancestry rather than a later rebase"
        );

        std::fs::remove_dir_all(&worktree).ok();
    }

    /// The cut reads a dependency's branch from that task, not by formatting
    /// its id back into `task/<dep_id>`.
    ///
    /// When the dependency cannot be found at all, that has to surface as a
    /// named error about the dependency — not as a git failure over a branch
    /// name that was reconstructed from the id and never existed. Today the
    /// reconstructed `task/first` is handed straight to `cut_worktree`, which
    /// fails with `could not cut a worktree for ...`, saying nothing about
    /// the dependency being the problem.
    #[test]
    fn a_dependency_that_cannot_be_found_fails_by_name_not_at_the_worktree_cut() {
        let repo = fixture("dep-not-found");
        let worktree = crate::mux::worktree_root(&repo.root, &repo.config.dispatch).join("second");
        let _ = std::fs::remove_dir_all(&worktree);

        // `second` names `first` as a dependency, but no `first` task file
        // exists and no `task/first` branch was ever cut.
        let mut task = reload(&add_task_with(&repo, "second", "implement", |f| {
            f.depends_on = vec!["first".into()];
        }));
        let mux = FakeMux::new(vec![]).tabs_in_one_workspace();

        let err = ensure_workspace(&repo, &mux, &mut task, &Default::default())
            .expect_err("an unresolvable dependency must not silently reach the worktree cut");
        let err = format!("{err:#}");

        assert!(
            err.contains("first"),
            "the error should name the dependency `first`; got: {err}"
        );
        assert!(
            !err.contains("could not cut a worktree"),
            "the failure should land where the dependency is resolved, not at the \
             git worktree cut over a rebuilt `task/first`; got: {err}"
        );

        std::fs::remove_dir_all(&worktree).ok();
    }

    /// A task with no dependency is cut from `base:` exactly as it was
    /// before this task — `cut_from` and `base` agree.
    #[test]
    fn a_task_with_no_dependency_is_still_cut_from_base() {
        let repo = fixture("cut-from-base");
        let mut task = reload(&add_task(&repo, "demo", "implement"));
        let mux = FakeMux::new(vec![]);

        ensure_workspace(&repo, &mux, &mut task, &Default::default()).unwrap();

        assert_eq!(task.front.cut_from.as_deref(), Some("work"));
        assert_eq!(task.front.base.as_deref(), Some("work"));
    }

    /// The whole reason `base_commit` moves to `cut_from`: a queued
    /// dependent has not been cut yet, so the dependency's branch is still
    /// what its worktree has to start from. Deleting it here would leave
    /// that dependent with nothing to be cut from.
    #[test]
    fn a_finished_tasks_branch_survives_while_a_queued_dependent_still_names_it() {
        let repo = fixture("branch-survives-dependent");
        repo.git(&["checkout", "-q", "-b", "task/first"]).unwrap();
        repo.git(&["commit", "-q", "--allow-empty", "-m", "first"])
            .unwrap();
        repo.git(&["checkout", "-q", "work"]).unwrap();
        // Pushed already, so the survival this test asserts is the
        // dependency exemption's doing, not an unpushed branch being kept
        // for an unrelated reason.
        mark_pushed(&repo, "task/first");

        let first = add_task_with(&repo, "first", "done", |f| {
            f.branch = Some("task/first".into());
        });
        add_task_with(&repo, "second", "queued", |f| {
            f.depends_on = vec!["first".into()];
        });

        let mux = FakeMux::new(vec![]);
        let pipelines = Pipelines::builtin();
        let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        let mut report = Report::default();

        dispatcher
            .clean_up(&mut reload(&first), &[], &mut report)
            .unwrap();

        assert!(
            repo.git(&["rev-parse", "--verify", "--quiet", "task/first"])
                .is_ok(),
            "`second` is still queued and has not been cut from it yet"
        );
    }

    /// Once the last dependent naming it has itself been cut — here, cut and
    /// then finished — nothing queued needs the branch any more, and the
    /// next cleanup that reaches it is what frees it. Nothing revisits an
    /// already-archived task on its own, so this has to be some *other*
    /// task's cleanup.
    #[test]
    fn an_orphaned_branch_is_freed_by_the_next_cleanup_that_finds_it() {
        let repo = fixture("branch-freed-later");
        repo.git(&["checkout", "-q", "-b", "task/first"]).unwrap();
        repo.git(&["commit", "-q", "--allow-empty", "-m", "first"])
            .unwrap();
        repo.git(&["checkout", "-q", "work"]).unwrap();
        // Pushed already, so what frees it once it is orphaned is purely the
        // dependency going away — the thing this test is actually about.
        mark_pushed(&repo, "task/first");

        let first = add_task_with(&repo, "first", "done", |f| {
            f.branch = Some("task/first".into());
        });
        let second = add_task_with(&repo, "second", "done", |f| {
            f.branch = Some("task/second".into());
            f.depends_on = vec!["first".into()];
        });

        let mux = FakeMux::new(vec![]);
        let pipelines = Pipelines::builtin();
        let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        let mut report = Report::default();

        // `first` archives while `second` is still queued, naming it — kept
        // rather than deleted, as above.
        dispatcher
            .clean_up(&mut reload(&first), &[], &mut report)
            .unwrap();
        assert!(
            repo.git(&["rev-parse", "--verify", "--quiet", "task/first"])
                .is_ok()
        );

        // `second` reaches `done` next: nothing queued names `first` any
        // more, so `second`'s own cleanup is what frees the branch `first`
        // was retained for.
        dispatcher
            .clean_up(&mut reload(&second), &[], &mut report)
            .unwrap();
        assert!(
            repo.git(&["rev-parse", "--verify", "--quiet", "task/first"])
                .is_err(),
            "nothing queued needs it any more"
        );
    }

    /// The same sweep recovers a task id from a branch
    /// `issue_tracking.key_in_names` prefixed — `task/<slug>-<id>` — so a
    /// prefixed orphan branch is freed exactly as a bare one is.
    #[test]
    fn an_orphaned_prefixed_branch_is_recovered_and_freed() {
        let repo = fixture("prefixed-branch-freed");
        repo.git(&["checkout", "-q", "-b", "task/proj-12-auth-01"])
            .unwrap();
        repo.git(&["commit", "-q", "--allow-empty", "-m", "auth-01"])
            .unwrap();
        repo.git(&["checkout", "-q", "work"]).unwrap();
        mark_pushed(&repo, "task/proj-12-auth-01");

        let first = add_task_with(&repo, "auth-01", "done", |f| {
            f.branch = Some("task/proj-12-auth-01".into());
        });
        let second = add_task_with(&repo, "auth-02", "done", |f| {
            f.branch = Some("task/proj-12-auth-02".into());
            f.depends_on = vec!["auth-01".into()];
        });

        let mux = FakeMux::new(vec![]);
        let pipelines = Pipelines::builtin();
        let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        let mut report = Report::default();

        dispatcher
            .clean_up(&mut reload(&first), &[], &mut report)
            .unwrap();
        assert!(
            repo.git(&["rev-parse", "--verify", "--quiet", "task/proj-12-auth-01"])
                .is_ok(),
            "`auth-02` is still queued and names it"
        );

        dispatcher
            .clean_up(&mut reload(&second), &[], &mut report)
            .unwrap();
        assert!(
            repo.git(&["rev-parse", "--verify", "--quiet", "task/proj-12-auth-01"])
                .is_err(),
            "the prefixed orphan branch was not recovered and freed"
        );
    }

    /// The recovery matches the branch against each task's recorded `branch:`
    /// exactly, so a prefixed branch whose slug happens to contain another
    /// real task's id is still attributed to its own task: `task/proj-old-x`
    /// is `x` (slug `proj-old`), never the queued `old-x` whose own branch is
    /// something else — and `old-x` still on the queue must keep its branch.
    #[test]
    fn a_prefixed_branch_is_not_mis_attributed_to_a_task_its_slug_contains() {
        let repo = fixture("prefixed-branch-ambiguous");
        for branch in ["task/proj-old-x", "task/old-x"] {
            repo.git(&["checkout", "-q", "-b", branch]).unwrap();
            repo.git(&["commit", "-q", "--allow-empty", "-m", branch])
                .unwrap();
            repo.git(&["checkout", "-q", "work"]).unwrap();
        }
        mark_pushed(&repo, "task/proj-old-x");
        // Pushed too, so `old-x`'s survival below is the still-queued guard's
        // doing, not the push gate masking it.
        mark_pushed(&repo, "task/old-x");

        // `x` has finished; `old-x` is a different, still-queued task.
        let finished = add_task_with(&repo, "x", "done", |f| {
            f.branch = Some("task/proj-old-x".into());
        });
        add_task_with(&repo, "old-x", "queued", |f| {
            f.branch = Some("task/old-x".into());
        });

        let mux = FakeMux::new(vec![]);
        let pipelines = Pipelines::builtin();
        let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        let mut report = Report::default();
        dispatcher
            .clean_up(&mut reload(&finished), &[], &mut report)
            .unwrap();

        assert!(
            repo.git(&["rev-parse", "--verify", "--quiet", "task/proj-old-x"])
                .is_err(),
            "`x`'s own prefixed branch should have been freed"
        );
        assert!(
            repo.git(&["rev-parse", "--verify", "--quiet", "task/old-x"])
                .is_ok(),
            "the still-queued `old-x` must keep its branch"
        );
    }

    /// A task reaching a cleanup terminal with work its worktree holds that
    /// `auto_commit` cannot record — a rejecting `pre-commit` hook here — is
    /// held at `blocked` rather than archived, so the worktree is not torn
    /// down over a commit that never happened (review finding 4). This is the
    /// road every shipped pipeline takes to `done`: a command step, not an
    /// agent's `spoolway report`.
    #[cfg(unix)]
    #[test]
    fn a_cleanup_terminal_holds_a_task_whose_work_cannot_be_committed() {
        use std::os::unix::fs::PermissionsExt;

        let repo = fixture("cleanup-uncommittable");
        let worktree = crate::scratch::root("dispatch-cleanup-uncommittable-wt");
        let _ = std::fs::remove_dir_all(&worktree);
        std::fs::create_dir_all(&worktree).unwrap();
        crate::repo::run(&worktree, "git", &["init", "-q", "-b", "task/demo"]).unwrap();
        let hooks = worktree.join(".git/hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(
            hooks.join("pre-commit"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::write(worktree.join("left.txt"), "work that never got recorded").unwrap();

        let path = add_task_with(&repo, "demo", "done", |f| {
            f.workspace_id = Some("w1".into());
            f.branch = Some("task/demo".into());
            f.worktree_path = Some(worktree.clone());
            f.last_report = Some(crate::task::LastReport {
                step: "implement".into(),
                outcome: "pass".into(),
                at: 0,
            });
        });

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert!(
            path.exists(),
            "the task must stay in the queue, not be archived over lost work"
        );
        assert!(!repo.archive_dir().join("demo.md").exists());
        let task = reload(&path);
        assert_eq!(task.stage(), crate::pipeline::BLOCKED);
        assert_eq!(task.front.blocked_from.as_deref(), Some("implement"));
        assert!(
            mux.did("remove_workspace").is_empty(),
            "the worktree must not be torn down"
        );
        assert!(
            task.section("## Status Log")
                .unwrap_or_default()
                .contains("could not be committed"),
            "the record must say why"
        );
    }

    // covers: step.cleanup — a terminal step that cleans up takes the worktree, the branch and the task file
    #[test]
    fn reaching_a_cleanup_step_tears_down_and_archives() {
        let repo = fixture("cleanup");
        let path = add_task_with(&repo, "demo", "done", |f| {
            f.workspace_id = Some("w1".into());
            f.branch = Some("task/demo".into());
        });

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert_eq!(mux.did("remove_workspace"), ["remove_workspace w1"]);
        assert!(!path.exists(), "task file should have left the queue");
        assert!(repo.archive_dir().join("demo.md").exists());
    }

    /// Archiving a task takes its hook and command run files with it, so
    /// `tracking::failure_count` stops counting a long-gone task's failed
    /// hook (review finding 64). Another task's files are left untouched.
    #[test]
    fn cleanup_reclaims_a_tasks_tracking_and_command_run_files() {
        let repo = fixture("cleanup-reclaim");
        let path = add_task_with(&repo, "demo", "done", |f| {
            f.workspace_id = Some("w1".into());
            f.branch = Some("task/demo".into());
        });

        std::fs::create_dir_all(repo.tracking_dir()).unwrap();
        std::fs::create_dir_all(repo.commands_dir()).unwrap();
        std::fs::write(repo.tracking_dir().join("demo · queued.exit"), "1").unwrap();
        std::fs::write(repo.tracking_dir().join("demo · queued.log"), "boom").unwrap();
        std::fs::write(repo.commands_dir().join("demo · e2e.log"), "output").unwrap();
        std::fs::write(repo.tracking_dir().join("other · queued.exit"), "1").unwrap();

        let mux = FakeMux::new(vec![]);
        run_pass(&repo, &mux);

        assert!(!path.exists(), "task file should have left the queue");
        assert!(!repo.tracking_dir().join("demo · queued.exit").exists());
        assert!(!repo.tracking_dir().join("demo · queued.log").exists());
        assert!(!repo.commands_dir().join("demo · e2e.log").exists());
        assert!(
            repo.tracking_dir().join("other · queued.exit").exists(),
            "another task's run files must be left alone"
        );
    }

    /// The per-session agent home a self-id'ing kind was given is reclaimed
    /// once the lane is banked and the task archived (review finding 63) — a
    /// no-op for a kind that takes the id spoolway minted.
    #[test]
    fn reclaim_session_home_removes_a_self_id_kinds_home() {
        let home = crate::scratch::root("dispatch-session-home");
        let _ = std::fs::remove_dir_all(&home);
        crate::platform::test_home::with_home(&home, || {
            let session = "reclaim-sh-1";
            let dir = crate::agent::session_home("codex", session)
                .expect("codex pins by home, so it has one");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("auth.json"), "{}").unwrap();

            let record = LaneRecord {
                session: session.into(),
                kind: "codex".into(),
                ..LaneRecord::adopted(now_secs())
            };
            record.reclaim_session_home();
            assert!(!dir.exists(), "the per-session home was left behind");

            // A kind that takes the minted id has no home, so this does
            // nothing and cannot panic.
            LaneRecord {
                session: "s".into(),
                kind: "claude".into(),
                ..LaneRecord::adopted(now_secs())
            }
            .reclaim_session_home();
        });
        let _ = std::fs::remove_dir_all(&home);
    }

    /// `clean_up` can still turn back and hold the task at `blocked` when the
    /// worktree holds work `auto_commit` could not record. The per-session
    /// agent home must survive that: the task stays in the queue, and a
    /// `session:` step resuming it wants its transcript. Reclamation only
    /// runs after the archive rename, past every early return.
    #[test]
    fn a_cleanup_held_at_blocked_keeps_the_lanes_session_home() {
        let mut repo = fixture("cleanup-blocked-keeps-home");
        repo.config.dispatch.auto_commit = false;

        let worktree = crate::scratch::root("dispatch-cleanup-blocked-wt");
        let _ = std::fs::remove_dir_all(&worktree);
        repo.git(&[
            "worktree",
            "add",
            "-b",
            "task/demo",
            worktree.to_str().unwrap(),
        ])
        .unwrap();
        // Uncommitted work in the tree — with `auto_commit` off this is
        // `AutoCommit::Unrecorded`, which holds the cleanup at `blocked`.
        std::fs::write(worktree.join("scratch.txt"), "unsaved\n").unwrap();

        let path = add_task_with(&repo, "demo", "done", |f| {
            f.workspace_id = Some("w1".into());
            f.branch = Some("task/demo".into());
            f.worktree_path = Some(worktree.clone());
        });

        let home = crate::scratch::root("dispatch-cleanup-blocked-home");
        let _ = std::fs::remove_dir_all(&home);
        crate::platform::test_home::with_home(&home, || {
            let session = "cleanup-blocked-s1";
            let session_home = crate::agent::session_home("codex", session).unwrap();
            std::fs::create_dir_all(&session_home).unwrap();
            std::fs::write(session_home.join("auth.json"), "{}").unwrap();

            let mux = FakeMux::new(vec![]);
            let pipelines = Pipelines::builtin();
            let mut dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
            dispatcher.lanes.insert(
                "demo · implement".into(),
                LaneRecord {
                    session: session.into(),
                    kind: "codex".into(),
                    ..LaneRecord::adopted(now_secs())
                },
            );

            // A live owned lane for the task — the path the reclaim used to
            // run on before the early return.
            let owned_lane = lane(&repo, "demo · implement", LaneStatus::Working);
            let owned: Vec<(String, String, &Lane)> =
                vec![("implement".into(), "demo".into(), &owned_lane)];
            let mut report = Report::default();
            let archived = dispatcher
                .clean_up(&mut reload(&path), &owned, &mut report)
                .unwrap();

            assert!(!archived, "cleanup should have turned back to `blocked`");
            assert!(
                session_home.join("auth.json").exists(),
                "the owned lane's session home was reclaimed on a path that never archived the task"
            );
        });

        assert!(path.exists(), "a held task stays in the queue");
        assert_eq!(reload(&path).stage(), crate::pipeline::BLOCKED);

        repo.git(&["worktree", "remove", "--force", worktree.to_str().unwrap()])
            .ok();
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A lane still writing when its task reaches a cleaning terminal step is
    /// skipped by `free_finished_lanes` as busy and then killed by
    /// `clean_up`. `clean_up` used to drop its record without banking it, so
    /// its tokens were lost (review finding 14); now it banks each owned lane
    /// first, the way `sweep_on_stop` does.
    #[test]
    fn clean_up_banks_a_lane_still_running_when_the_task_reaches_done() {
        let mut repo = fixture("cleanup-banks");
        priced(&mut repo, "priced-model");
        let path = add_task_with(&repo, "demo", "done", |f| {
            f.workspace_id = Some("w1".into());
            f.branch = Some("task/demo".into());
        });

        let session = "cleanup-s14";
        let kind = local_kind(&repo);
        {
            let mut lanes = load_lane_records(&repo);
            lanes.insert(
                "demo · implement".into(),
                LaneRecord {
                    session: session.into(),
                    kind: kind.clone(),
                    agent: "pi".into(),
                    model: "priced-model".into(),
                    ..LaneRecord::adopted(now_secs())
                },
            );
            save_lane_records(&repo, &lanes).unwrap();
        }

        let home = pi_home_with(session, 5_000);
        let mux = FakeMux::new(vec![lane(&repo, "demo · implement", LaneStatus::Working)]);
        with_home(&home, || {
            Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
                .pass()
                .unwrap();
        });

        assert!(!path.exists(), "the task still reached the archive");
        let banked = crate::usage::read(&repo).unwrap();
        let line = banked
            .iter()
            .find(|e| e.session == session)
            .expect("clean_up killed the lane without banking it");
        assert_eq!(line.tokens.input, 5_000);
        // The lane's own step, not the terminal `done` the task now sits on:
        // `lane_name(&line.step, &line.task)` has to name a lane that existed.
        assert_eq!(line.step, "implement");
        assert_eq!(line.task, "demo");
    }

    #[test]
    fn a_task_on_an_unknown_step_is_reported_not_guessed_at() {
        let repo = fixture("unknown");
        add_task(&repo, "demo", "not-a-real-step");

        let mux = FakeMux::new(vec![]);
        let report = run_pass(&repo, &mux);

        assert!(mux.calls().is_empty());
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].contains("does not define"));
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let repo = fixture("dry");
        let path = add_task(&repo, "demo", "queued");

        let mux = FakeMux::new(vec![]);
        let report = Dispatcher::new(&repo, &Pipelines::builtin(), &mux, true)
            .pass()
            .unwrap();

        assert!(mux.calls().is_empty());
        assert_eq!(reload(&path).stage(), "queued");
        assert!(report.actions.iter().any(|a| a.starts_with("would start")));
    }

    // ------------------------------------------------------------ command steps
    //
    // POSIX-only, every one of them, and not for want of trying: the `run:`
    // lines below are sh idioms (`pwd >`, `exit 2` read back through a POSIX
    // wrapper) rather than the platform-neutral `sleep`/`exit`/`echo` the
    // handful of tests elsewhere use. `command_step` itself spawns and reads
    // liveness on either platform now — it is these lines that are Unix only,
    // not the spawn — so these are compiled where a POSIX shell can read them
    // rather than failing where there is not one.

    /// A pipeline whose `implement` step has been replaced by a command step
    /// running `run`, so the whole graph either side of it is the shipped one.
    #[cfg(unix)]
    fn pipelines_running(run: &str, background: bool) -> Pipelines {
        let mut pipelines = Pipelines::builtin();
        let name = pipelines.default.clone();
        let pipeline = pipelines.pipelines.get_mut(&name).unwrap();
        let step = pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap();
        step.run = Some(run.to_string());
        step.background = background;
        step.agent = None;
        step.prompt = None;
        // A command step starts no lane, so every key that configures one has to
        // go with the agent. `implement` carries a session now, for the round
        // trip back from `review`.
        step.session = false;
        if background {
            // Refused at load, and the fixture must not build a graph a project
            // could not actually write.
            step.on_fail = None;
        }
        pipelines
    }

    /// The same, with a timeout short enough for a test to reach.
    #[cfg(unix)]
    fn pipelines_running_for(run: &str, background: bool, timeout: Duration) -> Pipelines {
        let mut pipelines = pipelines_running(run, background);
        let name = pipelines.default.clone();
        let pipeline = pipelines.pipelines.get_mut(&name).unwrap();
        pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap()
            .timeout = Some(timeout);
        pipelines
    }

    /// A task whose workspace is already cut, at a directory that really
    /// exists.
    ///
    /// The fake multiplexer hands back a path and creates nothing, which is
    /// enough for every test about lanes — nothing is ever run in them. A
    /// command step runs a real process in that directory, so it needs one.
    #[cfg(unix)]
    fn add_task_with_worktree(repo: &Repo, id: &str, stage: &str) -> PathBuf {
        let worktree = repo.root.join(format!("wt-{id}"));
        std::fs::create_dir_all(&worktree).unwrap();
        add_task_with(repo, id, stage, |front| {
            front.branch = Some(format!("task/{id}"));
            front.base = Some("work".into());
            front.workspace_id = Some("w1".into());
            front.tab_id = Some("w1:t1".into());
            front.pane_id = Some("w1:p1".into());
            front.worktree_path = Some(worktree);
        })
    }

    /// Wait for the dispatcher to move the task off the command step, running
    /// passes rather than sleeping: what is being waited for is a process, and
    /// only a pass reads its exit code.
    #[cfg(unix)]
    fn drive(repo: &Repo, pipelines: &Pipelines, mux: &FakeMux, path: &Path, want: &str) -> Report {
        let started = std::time::Instant::now();
        loop {
            let report = Dispatcher::new(repo, pipelines, mux, false).pass().unwrap();
            if reload(path).stage() == want {
                return report;
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "task never reached `{want}` (it is on `{}`); last pass: {report:?}",
                reload(path).stage()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The blocking case, end to end through the dispatcher: the command runs
    /// in the task's own worktree, the pass waits for it, and a zero exit takes
    /// the `on_pass` route.
    #[cfg(unix)]
    #[test]
    fn a_command_step_runs_and_a_clean_exit_routes_on_pass() {
        let repo = fixture("command-pass");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        // Writes into the worktree it was given, which is what the assertion
        // below reads back — a command step that ran somewhere else would build
        // the wrong checkout and say nothing about it.
        let pipelines = pipelines_running("pwd > where-it-ran.txt", false);

        drive(&repo, &pipelines, &mux, &path, "review");

        let worktree = reload(&path).front.worktree_path.unwrap();
        let where_it_ran = std::fs::read_to_string(worktree.join("where-it-ran.txt")).unwrap();
        assert_eq!(
            where_it_ran.trim(),
            worktree.canonicalize().unwrap().display().to_string(),
            "the command ran outside the task's worktree"
        );
        // No lane, no slot, no model: the step started a process and nothing
        // else. The `review` lane the task went on to is the pipeline working —
        // what must not exist is a lane for the command step itself.
        assert!(
            !mux.did("start").iter().any(|s| s.contains("implement")),
            "a command step started a lane: {:?}",
            mux.did("start")
        );
    }

    /// The default now: a command step with no `headless:` key runs in a
    /// pane of its own, split off the task's own tab, and a passing exit
    /// closes it behind it.
    #[cfg(unix)]
    #[test]
    fn a_command_step_with_no_headless_key_runs_in_a_pane_and_closes_it() {
        let repo = fixture("command-pane-pass");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]).offering_panes();
        let pipelines = pipelines_running("echo paned", false);

        drive(&repo, &pipelines, &mux, &path, "review");

        assert!(
            mux.did("run_in_pane")
                .iter()
                .any(|call| call.contains("w1:t1") && call.contains("demo · implement")),
            "the step's own tab is where the pane split from: {:?}",
            mux.calls()
        );
        assert!(
            mux.did("close_pane")
                .iter()
                .any(|call| call.contains("w1:t1.s1")),
            "a passing command's pane closes behind it: {:?}",
            mux.calls()
        );
    }

    /// A herdr or tmux pane is spawned from the multiplexer server's own
    /// environment, not the dispatcher's — the dispatcher's own exports never
    /// reach it any other way. `start_command_in_pane` hands `run_in_pane`
    /// the dispatcher's own process environment as the layer the step's named
    /// map sits on top of, so a variable like `SPOOLWAY_GH` — set on the
    /// dispatcher, read nowhere a pane would otherwise see it — still reaches
    /// a paned command step the same way it already reaches a headless one by
    /// ordinary process inheritance.
    #[cfg(unix)]
    #[test]
    fn a_paned_commands_environment_carries_what_the_dispatcher_process_has() {
        let repo = fixture("command-pane-env");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]).offering_panes();
        let pipelines = pipelines_running("echo paned", false);

        crate::platform::set_test_env("SPOOLWAY_GH", "gh-forge-stub");
        drive(&repo, &pipelines, &mux, &path, "review");
        crate::platform::remove_test_env("SPOOLWAY_GH");

        let env_call = mux
            .did("run_in_pane env")
            .into_iter()
            .next()
            .expect("run_in_pane should have logged the environment it was handed");
        assert!(
            env_call.contains("SPOOLWAY_GH"),
            "a pane must see whatever the dispatcher's own process carries, \
             SPOOLWAY_GH among it: {env_call}"
        );
    }

    /// The one thing a pane must *not* inherit: where the dispatcher itself is
    /// standing.
    ///
    /// A pane is opened in the task's worktree, and the dispatcher runs from
    /// the main checkout. Handing its `PWD` over leaves the pane's shell
    /// pointing at two different directories at once — relative paths resolve
    /// against the worktree while `$PWD` names the main checkout — and a
    /// `run:` line that mixes the two silently runs against the wrong tree.
    /// `SPOOLWAY="$PWD/target/release/spoolway" scripts/e2e/run.sh` is the
    /// real one that did: this project's own end-to-end suites, read from the
    /// worktree, run against the main checkout's stale binary.
    #[cfg(unix)]
    #[test]
    fn a_paned_command_never_inherits_the_dispatchers_own_directory() {
        let repo = fixture("command-pane-pwd");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]).offering_panes();
        let pipelines = pipelines_running("echo paned", false);

        drive(&repo, &pipelines, &mux, &path, "review");

        let env_call = mux
            .did("run_in_pane env")
            .into_iter()
            .next()
            .expect("run_in_pane should have logged the environment it was handed");
        for name in ["PWD", "OLDPWD", "SHLVL"] {
            assert!(
                !env_call.split(' ').any(|word| word == name),
                "`{name}` is the shell's own, derived from the pane's cwd — \
                 the dispatcher's copy must not be handed over: {env_call}"
            );
        }
    }

    /// `headless: true` is the escape hatch back to today's silent, detached
    /// run — no pane is ever asked for.
    #[cfg(unix)]
    #[test]
    fn headless_true_never_asks_for_a_pane() {
        let repo = fixture("command-headless");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]).offering_panes();
        let mut pipelines = pipelines_running("echo hidden", false);
        pipelines
            .pipelines
            .get_mut(&pipelines.default.clone())
            .unwrap()
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap()
            .headless = true;

        drive(&repo, &pipelines, &mux, &path, "review");

        assert!(
            mux.did("run_in_pane").is_empty(),
            "a headless step must never split a pane: {:?}",
            mux.calls()
        );
    }

    /// A failing command's pane stands rather than closing, and the next
    /// arrival at the step replaces it instead of piling a second one on.
    #[cfg(unix)]
    #[test]
    fn a_failing_paned_commands_pane_stands_and_is_replaced_on_retry() {
        let repo = fixture("command-pane-fail");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]).offering_panes();
        let pipelines = pipelines_running("exit 1", false);

        // `implement` has no `on_fail` of its own in the shipped pipeline, so
        // a failing exit parks the task on `blocked`.
        drive(&repo, &pipelines, &mux, &path, crate::pipeline::BLOCKED);

        let key = crate::command_step::Runs::key("implement", "demo");
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        assert!(
            runs.pane(&key).is_some(),
            "a failing command's pane must stand until the step is retried"
        );
        assert!(
            mux.did("close_pane").is_empty(),
            "nothing should have closed it yet: {:?}",
            mux.calls()
        );

        // Sent back to `implement` by hand, the way a person clearing
        // `blocked` would — the retry is what proves the old pane is
        // replaced rather than left to accumulate.
        let mut reloaded = reload(&path);
        reloaded.set_stage("implement", None);
        reloaded.save().unwrap();
        mux.clear_calls();

        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();

        assert!(
            mux.did("close_pane")
                .iter()
                .any(|call| call.contains("w1:t1.s1")),
            "the old pane is closed before a new one is split: {:?}",
            mux.calls()
        );
        assert!(
            mux.did("run_in_pane")
                .iter()
                .any(|call| call.contains("w1:t1")),
            "a fresh arrival still gets a pane of its own: {:?}",
            mux.calls()
        );
    }

    /// A pipeline may open on a command step, and a task queued on one leaves
    /// `queued` on the pass its dependencies came in — not the pass after, and
    /// not never.
    ///
    /// Never is what it used to be: the queued arm handed the entry step to the
    /// slot allocator, which has nothing to allocate for a step that takes no
    /// slot and dropped it in silence, so the board said `nothing to do` for as
    /// long as the run lasted. Nothing about a `run:` needs a lane to have gone
    /// first — the command cuts the task's worktree itself.
    #[cfg(unix)]
    #[test]
    fn a_queued_task_whose_entry_is_a_command_step_starts_it() {
        let repo = fixture("command-entry");
        // The fake multiplexer hands back this path and creates nothing, which
        // is enough for every test about lanes. A command step runs a real
        // process in it.
        std::fs::create_dir_all("/tmp/spoolway-fake-worktree").unwrap();
        let path = add_task(&repo, "demo", crate::pipeline::QUEUED);
        let mux = FakeMux::new(vec![]);
        // `implement` is the shipped pipeline's entry, so replacing it with a
        // command step is a pipeline that opens on one.
        let ran = repo.root.join("the-entry-ran");
        let pipelines = pipelines_running(&format!("touch {}", ran.display()), false);

        let report = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        assert_eq!(
            reload(&path).stage(),
            "implement",
            "the task is still in `queued` after the pass that should have \
             started its entry: {report:?}"
        );

        // And the step it is on really is running: the pass that reads the exit
        // code routes it on.
        drive(&repo, &pipelines, &mux, &path, "review");
        assert!(ran.exists(), "the entry command never ran");
        assert!(
            !mux.did("start").iter().any(|s| s.contains("implement")),
            "a command step started a lane: {:?}",
            mux.did("start")
        );
    }

    /// The dry run of the same. `set_stage` writes the task file, and a dry run
    /// may not — so this arm says what it would start and stops there.
    #[cfg(unix)]
    #[test]
    fn a_dry_run_of_a_command_entry_says_so_and_writes_nothing() {
        let repo = fixture("command-entry-dry");
        let path = add_task(&repo, "demo", crate::pipeline::QUEUED);
        let mux = FakeMux::new(vec![]);
        let ran = repo.root.join("the-entry-ran");
        let pipelines = pipelines_running(&format!("touch {}", ran.display()), false);

        let report = Dispatcher::new(&repo, &pipelines, &mux, true)
            .pass()
            .unwrap();

        assert!(mux.calls().is_empty(), "{:?}", mux.calls());
        assert!(!ran.exists(), "a dry run ran the command");
        assert_eq!(reload(&path).stage(), crate::pipeline::QUEUED);
        assert!(
            report
                .actions
                .iter()
                .any(|a| a == "would start `implement` for demo"),
            "a dry run has to say what it would have started: {:?}",
            report.actions
        );
    }

    /// Every candidate is put there by an arm that has already asked what kind
    /// of step it is, so one with no `agent:` is a broken invariant rather than
    /// a step to pass over. Skipping it in silence is what made a pipeline
    /// opening on a `run:` step look like an empty queue.
    #[test]
    fn a_candidate_with_no_agent_is_reported_rather_than_skipped() {
        let repo = fixture("no-agent");
        let mut pipelines = Pipelines::builtin();
        let name = pipelines.default.clone();
        pipelines
            .pipelines
            .get_mut(&name)
            .unwrap()
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap()
            .agent = None;

        add_task(&repo, "demo", crate::pipeline::QUEUED);

        let mux = FakeMux::new(vec![]);
        let report = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();

        assert!(mux.did("start").is_empty(), "no lane should have started");
        assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        assert!(
            report.problems[0].contains("no `agent:`"),
            "the problem has to name what is missing: {}",
            report.problems[0]
        );
    }

    /// A non-zero exit is a failure, and it takes the step's own `on_fail`
    /// rather than the pipeline's blocked step — the whole point of putting a
    /// build in the graph is that a broken one routes to the fix step.
    #[cfg(unix)]
    // covers: step.on_fail — where a step's failure sends the task
    #[test]
    fn a_failing_command_routes_on_fail() {
        let repo = fixture("command-fail");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_running("exit 2", false);

        let report = drive(&repo, &pipelines, &mux, &path, "blocked");
        assert!(
            report.actions.iter().any(|a| a.contains("exited 2")),
            "the exit code is what the decision was made on, so it is said: {:?}",
            report.actions
        );
    }

    /// The exit code used to live only in this pass's own report, which the
    /// lane sent in to clear the block never reads. A non-zero exit now also
    /// writes the step, the code and the log path onto the task itself, so
    /// that lane can see what actually broke.
    #[cfg(unix)]
    // covers: the `run:` step exiting non-zero — what it leaves on the task
    #[test]
    fn a_failing_command_writes_the_step_code_and_log_onto_the_task() {
        let repo = fixture("command-fail-onto-task");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_running("exit 2", false);

        drive(&repo, &pipelines, &mux, &path, "blocked");

        let task = reload(&path);
        let log = task.section("## Status Log").unwrap_or_default();
        assert!(
            log.contains("`implement` exited 2"),
            "names the step and the code: {log}"
        );
        assert!(
            log.contains("demo · implement.log"),
            "names the log a person or lane would read: {log}"
        );
    }

    /// A clean exit writes nothing onto the task — there is no failure to
    /// carry forward, and every ordinary pass would otherwise grow the file.
    #[cfg(unix)]
    #[test]
    fn a_clean_command_exit_writes_nothing_onto_the_task() {
        let repo = fixture("command-pass-writes-nothing");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_running("exit 0", false);

        drive(&repo, &pipelines, &mux, &path, "review");

        let task = reload(&path);
        let log = task.section("## Status Log").unwrap_or_default();
        assert!(
            !log.contains("exited 0"),
            "a pass is not a failure to record: {log}"
        );
    }

    /// A pipeline whose command step fails straight to `blocked`, which is what
    /// a spent loop bound and a red mechanical gate both come down to.
    #[cfg(unix)]
    fn pipelines_failing_to_blocked() -> Pipelines {
        let mut pipelines = pipelines_running("exit 1", false);
        let name = pipelines.default.clone();
        let pipeline = pipelines.pipelines.get_mut(&name).unwrap();
        let step = pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap();
        step.on_fail = Some(crate::pipeline::BLOCKED.to_string());
        pipelines
    }

    /// An attended run parks on `blocked` and stays there. It is the pass's own
    /// routing that used to break this: the guard at the top of a pass leaves a
    /// task *sitting* on `blocked` alone, but one routed there mid-pass went
    /// straight into the slot allocator and had its unblocker started before
    /// any pass could look. The lane then reported, the task went back where it
    /// blocked from, and a gate that never turned green circled forever in the
    /// one kind of run that has a person to stop for.
    #[cfg(unix)]
    #[test]
    fn an_attended_run_parks_on_blocked_instead_of_staffing_it() {
        let repo = fixture("blocked-attended");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_failing_to_blocked();

        drive(&repo, &pipelines, &mux, &path, crate::pipeline::BLOCKED);

        // And it is still there a pass later, rather than having been sent back
        // round by a lane nobody should have started.
        let report = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        assert_eq!(
            reload(&path).stage(),
            crate::pipeline::BLOCKED,
            "an attended run waits for `spoolway resume`; last pass: {report:?}"
        );
        assert!(
            !mux.calls().iter().any(|call| call.contains("blocked")),
            "no unblocker lane belongs in an attended run: {:?}",
            mux.calls()
        );
    }

    /// A command step routed to `blocked` writes down where it came from.
    ///
    /// `report` does this on its own route there, and `escalate` on its own,
    /// but the dispatcher's command-step arm never did — so a task blocked by a
    /// red mechanical gate arrived with no `blocked_from`, and `resume_target`
    /// had no origin to carry it back to.
    #[cfg(unix)]
    #[test]
    fn a_command_step_blocking_records_the_step_it_blocked_on() {
        let repo = fixture("blocked-origin-direct");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_failing_to_blocked();

        drive(&repo, &pipelines, &mux, &path, crate::pipeline::BLOCKED);

        assert_eq!(
            reload(&path).front.blocked_from.as_deref(),
            Some("implement"),
            "a pass from `blocked` has to know which step's work was in the way"
        );
    }

    /// The same, by the road that actually bit: the command's `on_fail` is a
    /// step whose loop budget is already spent, so `apply_loop_budget`
    /// redirects it to `blocked` instead. The redirect is the only thing that
    /// changed about the destination, and it used to lose the origin with it.
    #[cfg(unix)]
    #[test]
    fn a_spent_loop_budget_redirecting_a_command_to_blocked_still_records_the_origin() {
        let repo = fixture("blocked-origin-budget");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);

        // `implement` fails back to `review`, and `review` allows one lap in
        // from `implement` before giving up to `blocked`.
        let mut pipelines = pipelines_running("exit 1", false);
        let name = pipelines.default.clone();
        let pipeline = pipelines.pipelines.get_mut(&name).unwrap();
        let implement = pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap();
        implement.on_fail = Some("review".to_string());
        let review = pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "review")
            .unwrap();
        review.r#loop = crate::pipeline::Loop::PerRoute(std::collections::BTreeMap::from([(
            "implement".to_string(),
            1,
        )]));
        review.on_loop_max = Some(crate::pipeline::BLOCKED.to_string());

        // That one lap already taken, so the failure below is the one over.
        let mut task = reload(&path);
        task.front.rounds.insert("implement->review".into(), 1);
        task.save().unwrap();

        drive(&repo, &pipelines, &mux, &path, crate::pipeline::BLOCKED);

        assert_eq!(
            reload(&path).front.blocked_from.as_deref(),
            Some("implement"),
            "the budget redirected the destination, not the question of where it came from"
        );
    }

    /// The other half, so the fix above is a distinction and not a blanket
    /// refusal: an unattended run has nobody to park in front of, so it staffs
    /// `blocked` exactly as it always did.
    #[cfg(unix)]
    #[test]
    fn an_unattended_run_still_staffs_blocked_on_arrival() {
        let repo = unattended_fixture("blocked-unattended");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_failing_to_blocked();

        drive(&repo, &pipelines, &mux, &path, crate::pipeline::BLOCKED);
        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();

        assert!(
            mux.calls().iter().any(|call| call.contains("blocked")),
            "an unattended run has no person to wait for: {:?}",
            mux.calls()
        );
    }

    /// While the command is running the task stays exactly where it is, and the
    /// pass does not wait on it — a four-minute build must not be four minutes
    /// in which no other task can move.
    #[cfg(unix)]
    #[test]
    fn a_pass_does_not_wait_for_a_running_command() {
        let repo = fixture("command-running");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_running("sleep 30", false);

        let started = std::time::Instant::now();
        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        let first = started.elapsed();
        // And a second pass, which finds it still going and leaves it alone.
        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the pass waited for the command: {first:?}"
        );
        assert_eq!(
            reload(&path).stage(),
            "implement",
            "the task left before its command finished"
        );

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("implement", "demo");
        assert_eq!(runs.state(&key), crate::command_step::RunState::Running);
        // Exactly one run was started, not one per pass.
        runs.stop(&key);
    }

    /// The background half: the task moves on the same pass the command starts,
    /// and the command is still running behind it.
    #[cfg(unix)]
    #[test]
    fn a_background_command_lets_the_task_move_on() {
        let repo = fixture("command-background");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_running("sleep 30", true);

        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();

        assert_eq!(
            reload(&path).stage(),
            "review",
            "a background command must not hold the task"
        );
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("implement", "demo");
        assert_eq!(
            runs.state(&key),
            crate::command_step::RunState::Running,
            "the task moved on but the command did not survive it"
        );
        runs.stop(&key);
    }

    /// A task standing on a background step whose run is already going moves
    /// on again, rather than waiting the run out. The pass that starts the
    /// run routes the task in its return value — and when that pass could
    /// not place the destination (a full model, a failed launch), the next
    /// pass finds the task still here, reads `Running`, and used to hold it
    /// against the command's own timeout: six hours behind an observer, for
    /// a placement that failed once.
    #[cfg(unix)]
    #[test]
    fn a_background_command_already_running_still_lets_the_task_move_on() {
        let repo = fixture("command-background-rearrival");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_running("sleep 30", true);

        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        assert_eq!(reload(&path).stage(), "review");

        // The destination could not be placed: the task is back on the
        // step, and the run it started is still going behind it.
        let mut task = reload(&path);
        task.set_stage("implement", None);
        task.save().unwrap();
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("implement", "demo");
        assert_eq!(runs.state(&key), crate::command_step::RunState::Running);

        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        assert_eq!(
            reload(&path).stage(),
            "review",
            "a running background command must not hold a re-arriving task"
        );
        runs.stop(&key);
    }

    /// Cleanup removes the task's worktree, and a background command is still
    /// running in it. Left alone it would spend the rest of its life writing
    /// into a directory that no longer exists.
    #[cfg(unix)]
    #[test]
    fn cleanup_stops_a_background_command_still_running() {
        let repo = fixture("command-cleanup");
        let path = add_task_with(&repo, "demo", "done", |front| {
            front.branch = Some("task/demo".into());
            front.workspace_id = Some("w1".into());
            front.worktree_path = Some(repo.root.clone());
        });
        let mux = FakeMux::new(vec![]);

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("bench", "demo");
        let pid = runs
            .start(&key, "sleep 60", &repo.root, &BTreeMap::new())
            .unwrap();

        run_pass(&repo, &mux);

        assert!(!path.exists(), "the task should have been archived");
        assert!(
            !crate::headless::alive(pid),
            "the background command outlived the worktree it was running in"
        );
    }

    /// The hang. A blocking command that never ends would park its task for as
    /// long as the dispatcher runs, and no other clock in a pass has an opinion
    /// about it — so the step's own timeout is the only thing that ends it.
    #[cfg(unix)]
    #[test]
    fn a_command_that_runs_past_its_timeout_is_stopped_and_routed() {
        let repo = fixture("command-timeout");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_running_for("sleep 300", false, Duration::from_millis(300));

        // The first pass starts it; the timeout has not passed yet, so the task
        // stays exactly where it is.
        let first = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        assert_eq!(reload(&path).stage(), "implement", "{first:?}");

        let key = crate::command_step::Runs::key("implement", "demo");
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let pid = runs.read_pid(&key).unwrap();

        std::thread::sleep(Duration::from_millis(400));
        let report = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();

        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("past its timeout")),
            "the timeout has to say so — a task that moved for no stated reason \
             is the thing this makes debuggable: {:?}",
            report.actions
        );
        // A timeout is a failure of the step, never a quiet pass: a build that
        // never finished did not succeed.
        assert_eq!(reload(&path).stage(), "blocked");
        assert!(
            !crate::headless::alive(pid),
            "the command was routed away from but left running"
        );
    }

    /// A background command is walked away from, so the step that started it
    /// never looks again — the sweep is the only thing that can stop it, and
    /// without it `background:` plus a task that blocks is a process nothing
    /// ever reaps.
    #[cfg(unix)]
    #[test]
    fn a_background_command_past_its_timeout_is_reaped() {
        let repo = fixture("command-timeout-background");
        let path = add_task_with_worktree(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_running_for("sleep 300", true, Duration::from_millis(300));

        Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();
        assert_ne!(
            reload(&path).stage(),
            "implement",
            "a background step does not hold its task"
        );

        let key = crate::command_step::Runs::key("implement", "demo");
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let pid = runs.read_pid(&key).unwrap();

        std::thread::sleep(Duration::from_millis(400));
        let report = Dispatcher::new(&repo, &pipelines, &mux, false)
            .pass()
            .unwrap();

        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("background") && a.contains("past its timeout")),
            "{:?}",
            report.actions
        );
        assert!(!crate::headless::alive(pid), "the background run survived");
    }

    /// A dry run says what it would start and starts nothing. A pass a person
    /// runs to look at the pipeline must not launch a deploy script.
    #[cfg(unix)]
    #[test]
    fn a_dry_run_starts_no_command() {
        let repo = fixture("command-dry");
        let path = add_task(&repo, "demo", "implement");
        let mux = FakeMux::new(vec![]);
        let pipelines = pipelines_running("touch it-ran.txt", false);

        let report = Dispatcher::new(&repo, &pipelines, &mux, true)
            .pass()
            .unwrap();

        assert!(
            report
                .actions
                .iter()
                .any(|a| a.contains("would run") && a.contains("touch it-ran.txt")),
            "{:?}",
            report.actions
        );
        assert_eq!(reload(&path).stage(), "implement");
        assert_eq!(
            crate::command_step::Runs::new(&repo.commands_dir())
                .state(&crate::command_step::Runs::key("implement", "demo")),
            crate::command_step::RunState::Fresh
        );
    }

    /// The step that hands the change over, and what the pipeline says about it.
    ///
    /// `handover` is a command step — `run: spoolway stack`, no model and no
    /// rebase — and a failure now routes straight to `blocked` rather than
    /// to a second, LLM-run escalation step: `spoolway stack` calls `gh pr
    /// view` first, so re-running `handover` from `blocked` is safe. Three
    /// tests used to live here, one per value of `dispatch.merge`, each
    /// checking which capabilities the setting produced; then one, checking
    /// the `handover:` and `credentials:` flags that replaced them. Those
    /// went the same way — a pipeline says which prompt runs where, and
    /// every statement it made about reaching a forge belongs to the
    /// prompt's own prose instead.
    #[test]
    fn handover_runs_spoolway_stack_and_falls_back_to_blocked() {
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();

        let handover = pipeline.step("handover").expect("a hand-off step");
        assert!(
            handover.run.is_some(),
            "`handover` should run `spoolway stack`, not a prompt"
        );
        assert_eq!(
            handover.on_fail.as_deref(),
            Some(crate::pipeline::BLOCKED),
            "a failed `spoolway stack` should park the task for a person, not hand off \
             to a second agent step"
        );
    }

    /// A gated step and the plain step beside it differ by exactly the gate
    /// paragraphs, and nothing else.
    ///
    /// The two used to be sent the same words — the paragraph asking a gated
    /// lane to stop and print its question was the whole enforcement, and a
    /// model that read it and reported a pass anyway walked straight through
    /// the gate, so it was cut down to nothing at all. It is back now, but as
    /// something the lane is told rather than something it is asked to act on:
    /// the mechanism is still outside the session, in `commands::report`.
    #[test]
    fn a_gated_step_and_the_step_beside_it_differ_by_the_gate_paragraphs_alone() {
        let repo = fixture("checkpoint-briefing");
        add_task(&repo, "t", "ask");
        let task = repo.task("t").unwrap();

        let pipeline = crate::pipeline::Pipeline::parse(
            "only",
            "steps:\n\
             \x20 - id: ask\n    agent: pi\n    gate: true\n    on_pass: deploy\n\
             \x20 - id: deploy\n    agent: pi\n    on_pass: done\n",
        )
        .expect("hand-built pipeline");

        let gated_step = pipeline.step("ask").unwrap();
        let plain_step = pipeline.step("deploy").unwrap();

        // `policy` differs only by a prefix: the gate paragraph, sent for the
        // gated step and nothing at all for the plain one.
        let gated_policy = crate::compose::policy(&repo, &task, &pipeline, gated_step);
        let plain_policy = crate::compose::policy(&repo, &task, &pipeline, plain_step);
        let gate_prefix = gated_policy
            .strip_suffix(plain_policy.as_str())
            .expect("the gated policy should be the plain one with a prefix added");
        assert!(
            gate_prefix.contains("This step is gated"),
            "got: {gate_prefix}"
        );

        // `situating` differs only by the step id and one bullet — the one
        // that says whether a person reads this pane once the lane reports.
        let gated_situating = crate::compose::situating(&pipeline, gated_step, &task, &repo)
            .unwrap()
            .replace("`ask`", "`X`");
        let plain_situating = crate::compose::situating(&pipeline, plain_step, &task, &repo)
            .unwrap()
            .replace("`deploy`", "`X`");
        let gated_lines: Vec<&str> = gated_situating.lines().collect();
        let plain_lines: Vec<&str> = plain_situating.lines().collect();
        assert_eq!(gated_lines.len(), plain_lines.len());
        let diffs: Vec<usize> = gated_lines
            .iter()
            .zip(&plain_lines)
            .enumerate()
            .filter(|(_, (g, p))| g != p)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            diffs,
            vec![
                gated_lines
                    .iter()
                    .position(|line| line.contains("this step is gated"))
                    .unwrap()
            ],
            "situating should differ in exactly the gated bullet:\n\
             gated: {gated_situating}\nplain: {plain_situating}"
        );
        assert!(gated_lines[diffs[0]].contains("a person opens this pane"));
    }

    /// A step with no `gate:` at all gets neither paragraph — there is nothing
    /// to say, and saying it would invite a lane to stop where it never should.
    #[test]
    fn an_ungated_step_is_told_nothing_about_gates() {
        let repo = fixture("gate-silent");
        add_task(&repo, "t", "implement");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();
        let task = repo.task("t").unwrap();

        assert!(!step.gate);
        let prompt = sent(&repo, &task, pipeline, step);
        assert!(!prompt.contains("gated"));
        assert!(!prompt.contains("dispatch.gates"));
    }

    /// The mechanics a prompt used to carry in a generated block are derived
    /// from the step instead, so they are sent exactly where they are true.
    ///
    /// The scratch space used to be a conditional paragraph naming
    /// `$SPOOLWAY_SCRATCH`, sent only to a step granted `rebase`. Every lane
    /// has one now — it is the lane's own directory, not a privilege — and it
    /// is a resolved path in `WHAT YOU HAVE` rather than a variable a prompt
    /// would have had to spell, so what a step is "told about it" means the
    /// path itself, not the name that resolves to it.
    #[test]
    fn the_opening_prompt_carries_only_the_mechanics_this_step_has() {
        let repo = fixture("prompt-mechanics");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let task = reload(&add_task(&repo, "demo", "review"));

        let sent_at = |step: &str| sent(&repo, &task, pipeline, pipeline.step(step).unwrap());

        for step in ["review", "handover", "implement"] {
            let prompt = sent_at(step);
            assert!(
                prompt.contains("scratch space"),
                "`{step}` has a scratch directory and is not told about it"
            );
            // And nothing spoolway sends still speaks the old vocabulary.
            for gone in ["SPOOLWAY_MERGE_INTO", "SPOOLWAY_ALLOW", "merge: git"] {
                assert!(
                    !prompt.contains(gone),
                    "`{gone}` reached `{step}`:\n{prompt}"
                );
            }
        }
    }

    /// The prompt contract's environment table is what somebody writing a
    /// prompt reads, and it is a separate list from the one a lane is
    /// actually started with. Nothing but this held them together, and they
    /// drifted: the contract went on offering `$SPOOLWAY_ALLOW` and
    /// `$SPOOLWAY_MERGE_INTO` for as long as it took somebody to notice, both
    /// of them gone with the capability system that set them.
    ///
    /// One direction only. A variable a lane sets and the contract does not
    /// mention is spoolway's own business — `$SPOOLWAY_HEAD`, which `report`
    /// reads to tell work from residue, is exactly that — but a variable the
    /// contract *promises* and no lane sets is a prompt written against
    /// something that will not be there.
    #[test]
    fn the_prompt_contract_promises_no_variable_a_lane_lacks() {
        let repo = fixture("contract-environment");
        add_task(&repo, "demo", "queued");
        let mux = FakeMux::new(vec![]);

        run_pass(&repo, &mux);

        let started = mux.did("env");
        let line = started.first().expect("no lane was started");
        for (name, _) in crate::prompt::ENVIRONMENT {
            assert!(
                line.split(' ').any(|set| set == *name),
                "the contract promises `{name}`, which no lane sets: {line}"
            );
        }
    }

    /// Every lane opens by being told what it is inside of. Without it a model
    /// reads a task file and a role and infers a solo assignment — so it widens
    /// its scope past the step it was given, asks a question into a pane nobody
    /// reads, or treats a boundary it was told about as an obstacle to work
    /// around rather than the edge of the step. Each
    /// of those is a lane that has to be fetched a person, and each is answered
    /// by one paragraph naming the step and the task it actually has.
    #[test]
    fn every_lane_is_told_it_is_one_step_of_one_task_in_spoolway() {
        let repo = fixture("prompt-situation");
        let pipelines = Pipelines::builtin();

        for (pipeline, step) in [
            ("default", "implement"),
            ("default", "handover"),
            ("default", "document"),
            ("bugfix", "reproduce"),
        ] {
            let pipeline = pipelines.get(pipeline).unwrap();
            let task = reload(&add_task(&repo, "demo", step));
            let prompt = sent(&repo, &task, pipeline, pipeline.step(step).unwrap());

            assert!(
                prompt.contains("spoolway runs one task at a time"),
                "{prompt}"
            );
            assert!(prompt.contains("You are a lane"), "{prompt}");
            // Named, not gestured at: "your step" is a phrase, `merge` is a
            // fact the lane can check itself against.
            assert!(prompt.contains(&format!("step `{step}`")), "{prompt}");
            assert!(prompt.contains("task `demo`"), "{prompt}");
            assert!(
                prompt.contains("Nobody reads your output as you produce it"),
                "{prompt}"
            );
            // And it is in the *system* prompt, not the typed message: the
            // whole point of composing is that a project cannot drop it by
            // writing its own prompt.
            assert!(
                crate::compose::system_prompt(
                    &repo,
                    &task,
                    pipeline,
                    pipeline.step(step).unwrap(),
                    "[prompt]"
                )
                .unwrap()
                .contains("spoolway runs one task at a time"),
                "the framing must survive in the system prompt alone"
            );
        }
    }

    /// A lane is told that nothing resumes a settled session — a lane that
    /// backgrounds a wait and ends its turn instead leaves a pane the board
    /// reports as a question.
    #[test]
    fn every_lane_is_told_that_nothing_will_wake_it() {
        let repo = fixture("prompt-no-wakeup");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("handover").unwrap();
        let task = reload(&add_task(&repo, "demo", "handover"));

        let prompt = sent(&repo, &task, pipeline, step);
        assert!(prompt.contains("Nothing will wake you"), "{prompt}");
        for gone in ["background job", "timer"] {
            assert!(prompt.contains(gone), "`{gone}` is not named:\n{prompt}");
        }
        assert!(prompt.contains("Poll anything you wait on"), "{prompt}");
    }

    /// `opening_prompt` used to carry the whole report contract, then just a
    /// pointer to it; now it carries neither. One sentence naming the task
    /// file, and nothing else — the report contract lives in the system
    /// prompt alone.
    #[test]
    fn the_opening_prompt_is_one_sentence_naming_the_task_file() {
        let repo = fixture("prompt-shrunk");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();
        let task = reload(&add_task(&repo, "demo", "implement"));

        let prompt = crate::compose::opening_prompt(&repo, &task, pipeline, step);
        assert_eq!(
            prompt,
            format!("Read {} before anything else.", task.path.display())
        );
        assert!(
            !prompt.contains("spoolway report"),
            "the report's own forms moved to the system prompt: {prompt}"
        );
    }

    /// A step naming `skills:` gets one `/name` line per skill, in
    /// declaration order, above everything else — a leading slash invocation
    /// only expands where it opens the message, so it cannot go after the
    /// briefing. The two paragraphs behind it are unchanged.
    // covers: step.skills — opening_prompt emits one leading `/name` per skill, in order
    #[test]
    fn the_opening_prompt_leads_with_one_slash_invocation_per_skill() {
        let repo = fixture("prompt-skills");
        let plain_pipeline = Pipelines::builtin().get("default").unwrap().clone();
        let plain_step = plain_pipeline.step("implement").unwrap();
        let task = reload(&add_task(&repo, "demo", "implement"));
        let plain_prompt =
            crate::compose::opening_prompt(&repo, &task, &plain_pipeline, plain_step);

        let mut pipeline = Pipelines::builtin().get("default").unwrap().clone();
        pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap()
            .skills = vec!["code-review".to_string(), "spoolway-doctor".to_string()];
        let step = pipeline.step("implement").unwrap();

        let prompt = crate::compose::opening_prompt(&repo, &task, &pipeline, step);
        assert!(
            prompt.starts_with("/code-review\n/spoolway-doctor\n\n"),
            "got: {prompt}"
        );
        assert_eq!(
            prompt
                .strip_prefix("/code-review\n/spoolway-doctor\n\n")
                .unwrap(),
            plain_prompt,
            "the two paragraphs after the skills should be untouched: {prompt}"
        );
    }

    /// The report contract moved to the end of the system prompt, after
    /// policy, and names `--handoff` — `--finding` is gone entirely.
    #[test]
    fn the_system_prompt_ends_with_the_report_contract() {
        let repo = fixture("prompt-contract");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();
        let task = reload(&add_task(&repo, "demo", "implement"));

        let prompt =
            crate::compose::system_prompt(&repo, &task, pipeline, step, "[prompt]").unwrap();
        assert!(
            prompt.trim_end().ends_with("`stage:` yourself."),
            "got: {prompt}"
        );
        assert!(prompt.contains("--handoff"), "got: {prompt}");
        assert!(!prompt.contains("--finding"), "got: {prompt}");
    }

    /// Acceptance criterion 2: `WHAT YOU WRITE DOWN` is in the composed
    /// system prompt, and it is placed directly after `WHAT YOU HAVE` — one
    /// blank line between them, the same gap every other heading in `YOUR
    /// LANE` keeps.
    #[test]
    fn what_you_write_down_follows_what_you_have_directly() {
        let repo = fixture("prompt-write-down-placement");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();
        let task = reload(&add_task(&repo, "demo", "implement"));

        let prompt =
            crate::compose::system_prompt(&repo, &task, pipeline, step, "[prompt]").unwrap();
        let have_at = prompt.find("WHAT YOU HAVE").expect("no WHAT YOU HAVE");
        let write_down_at = prompt
            .find("WHAT YOU WRITE DOWN")
            .expect("no WHAT YOU WRITE DOWN");
        assert!(write_down_at > have_at, "got: {prompt}");
        let between = &prompt[have_at..write_down_at];
        assert!(
            between.ends_with("\n\n"),
            "exactly one blank line should separate the two blocks: {between:?}"
        );
        assert!(
            !between.contains("YOUR ROLE"),
            "nothing else should sit between the two blocks: {between:?}"
        );
    }

    /// The three built-in headings, in `HEADINGS` order, each land as their
    /// own row.
    #[test]
    fn what_you_write_down_names_all_three_builtin_headings() {
        let repo = fixture("prompt-write-down-headings");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();
        let task = reload(&add_task(&repo, "demo", "implement"));

        let prompt =
            crate::compose::system_prompt(&repo, &task, pipeline, step, "[prompt]").unwrap();
        assert!(prompt.contains("Status Log"), "got: {prompt}");
        assert!(prompt.contains("Handoff"), "got: {prompt}");
        assert!(prompt.contains("Blocker"), "got: {prompt}");
    }

    /// `WHAT YOU WRITE DOWN`'s rows share `WHAT YOU HAVE`'s own label column
    /// — drawn like it, as the acceptance criterion says. Both blocks pad
    /// their label to the same width, so a value starts at the same column
    /// from the left margin whichever block's row it is.
    #[test]
    fn what_you_write_down_shares_what_you_haves_label_column() {
        let repo = fixture("prompt-write-down-column");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();
        let task = reload(&add_task(&repo, "demo", "implement"));

        let prompt =
            crate::compose::system_prompt(&repo, &task, pipeline, step, "[prompt]").unwrap();
        let column_of = |label: &str, first_value_word: &str| {
            let line = prompt
                .lines()
                .find(|line| line.trim_start().starts_with(label))
                .unwrap_or_else(|| panic!("no line starting with {label:?}: {prompt}"));
            line.find(first_value_word).unwrap()
        };
        assert_eq!(
            column_of("your change", "`git"),
            column_of("Status Log", "One"),
            "a row's value should start at the same column in both blocks: {prompt}"
        );
    }

    /// The one asymmetry acceptance criterion 1 calls out: a project whose
    /// `task-log.md` exists but leaves a heading out gets no row for it at
    /// all — not the built-in, unlike every other resolution chain in this
    /// codebase.
    #[test]
    fn a_heading_an_existing_task_log_omits_gets_no_row() {
        let repo = fixture("prompt-write-down-omitted-heading");
        std::fs::create_dir_all(repo.task_log_path().parent().unwrap()).unwrap();
        std::fs::write(
            repo.task_log_path(),
            "## Status Log\n\nOur own words.\n\n## Handoff\n\nOur own words too.\n",
        )
        .unwrap();
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();
        let task = reload(&add_task(&repo, "demo", "implement"));

        let prompt =
            crate::compose::system_prompt(&repo, &task, pipeline, step, "[prompt]").unwrap();
        assert!(prompt.contains("Our own words."), "got: {prompt}");
        assert!(prompt.contains("Our own words too."), "got: {prompt}");
        assert!(
            !prompt.contains("Blocker"),
            "task-log.md names no Blocker section, so no row should appear: {prompt}"
        );
    }

    /// Spoolway's own words in the composed system prompt — everything
    /// outside the prompt — stay under a budget for the plainest case a lane
    /// can land in: no dependency, no gate, no arrived-by-fail paragraph, no
    /// failed command. Raised from 275 when `WHAT YOU WRITE DOWN` joined
    /// `WHAT YOU HAVE` in `YOUR LANE` — the three headings' built-in wording
    /// costs around 60 words on its own.
    #[test]
    fn the_composed_prompt_is_under_the_word_budget_for_the_plain_case() {
        let repo = fixture("prompt-word-budget");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();
        let task = reload(&add_task(&repo, "demo", "implement"));
        let role = "[the project's prompt]";

        let prompt = crate::compose::system_prompt(&repo, &task, pipeline, step, role).unwrap();
        let words = prompt.replace(role, "").split_whitespace().count();
        assert!(words < 345, "got {words} words:\n{prompt}");
    }

    /// Every heading gap in the Mockup is exactly one blank line — never
    /// two. `situating`'s last bullet and `what_you_have`'s own leading
    /// blank line each supply one newline into that gap; a stray third would
    /// double it.
    #[test]
    fn no_heading_gap_is_more_than_one_blank_line() {
        let repo = fixture("prompt-no-double-blank");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();
        let task = reload(&add_task(&repo, "demo", "implement"));

        let prompt =
            crate::compose::system_prompt(&repo, &task, pipeline, step, "[prompt]").unwrap();
        assert!(!prompt.contains("\n\n\n"), "got: {prompt}");
    }

    /// `blocked`'s own branch of the report contract — acceptance criterion
    /// 5 — offers `--pass` and `--pause` only: no other step's system prompt
    /// should ever name `--pause`, and `blocked`'s should never name `--fail`
    /// or `--block`.
    #[test]
    fn the_blocked_steps_report_contract_offers_pass_and_pause_only() {
        let repo = fixture("prompt-contract-blocked");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step(crate::pipeline::BLOCKED).unwrap();
        let task = reload(&add_task(&repo, "demo", crate::pipeline::BLOCKED));

        let prompt =
            crate::compose::system_prompt(&repo, &task, pipeline, step, "[prompt]").unwrap();
        assert!(prompt.contains("--pause"), "got: {prompt}");
        assert!(!prompt.contains("--fail"), "got: {prompt}");
        assert!(!prompt.contains("--block"), "got: {prompt}");

        let implement = pipeline.step("implement").unwrap();
        let elsewhere =
            crate::compose::system_prompt(&repo, &task, pipeline, implement, "[prompt]").unwrap();
        assert!(
            !elsewhere.contains("--pause"),
            "no other step offers --pause: got: {elsewhere}"
        );
    }

    /// The `## arrived-by-fail` paragraph is sent only when this pass exists
    /// because the step this task arrived from failed it back here — its own
    /// `on_fail` naming this step, its `on_pass` naming somewhere else —
    /// which is what lets it open with "Fix pass" rather than a hedge.
    #[test]
    fn the_fix_pass_is_named_only_when_review_actually_failed_it_back() {
        let repo = fixture("prompt-findings");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("implement").unwrap();

        let mut task = reload(&add_task(&repo, "demo", "implement"));
        assert!(!sent(&repo, &task, pipeline, step).contains("Fix pass"));

        // `review`'s own `on_fail` is `implement`, and its `on_pass` is
        // `document` — the shape this paragraph exists for.
        task.front.arrived_from = Some("review".to_string());
        assert!(sent(&repo, &task, pipeline, step).contains("Fix pass"));
    }

    /// A command step reports nothing, so a lane its failure routes to used to
    /// arrive knowing only that it was there again. It redid the work, passed,
    /// and handed the same tree back for the same failure — a loop whose only
    /// exit was the round cap. The paragraph names the step and its log.
    #[test]
    fn a_lane_a_command_step_failed_into_is_told_which_one_and_where_to_read_it() {
        let repo = fixture("prompt-command-fail");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        // `handover` is the shipped pipeline's command step (`spoolway
        // stack`), and its `on_fail` now goes straight to `blocked` — the
        // shape this paragraph exists for.
        let step = pipeline.step("blocked").unwrap();

        let mut task = reload(&add_task(&repo, "demo", "blocked"));
        assert!(!sent(&repo, &task, pipeline, step).contains("command step"));

        task.front.arrived_from = Some("handover".to_string());
        let prompt = sent(&repo, &task, pipeline, step);
        assert!(
            prompt.contains("command step `handover` failed"),
            "{prompt}"
        );
        assert!(prompt.contains("demo · handover.log"), "{prompt}");
    }

    /// Arriving from an *agent* step is the ordinary case and says nothing:
    /// that step reported, and what it said is already in the task file.
    #[test]
    fn arriving_from_a_lane_is_not_a_failed_command() {
        let repo = fixture("prompt-command-lane");
        let pipelines = Pipelines::builtin();
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("review").unwrap();

        let mut task = reload(&add_task(&repo, "demo", "review"));
        task.front.arrived_from = Some("implement".to_string());
        assert!(!sent(&repo, &task, pipeline, step).contains("command step"));
    }

    /// A command step whose pass and fail both land here cannot say which
    /// happened: the exit code is read once and forgotten long before a prompt
    /// is built, and the route is all that is left to read. Guessing "failed"
    /// would send half of these lanes hunting a failure that never occurred,
    /// so the ambiguous case says nothing at all.
    #[test]
    fn a_command_step_routing_both_ways_to_one_lane_claims_nothing() {
        let repo = fixture("prompt-command-both");
        let mut pipelines = Pipelines::builtin();
        for pipeline in pipelines.pipelines.values_mut() {
            for step in &mut pipeline.steps {
                if step.id == "checks" {
                    step.on_pass = Some("handover".to_string());
                    step.on_fail = Some("handover".to_string());
                }
            }
        }
        let pipeline = pipelines.get("default").unwrap();
        let step = pipeline.step("handover").unwrap();

        let mut task = reload(&add_task(&repo, "demo", "handover"));
        task.front.arrived_from = Some("checks".to_string());
        assert!(!sent(&repo, &task, pipeline, step).contains("command step"));
    }

    /// The same gated lane as
    /// [`a_gate_keeps_its_slot_and_every_other_waiting_lane_gives_it_back`],
    /// on a backend where a waiting lane is not a running one.
    ///
    /// The slot is kept there because answering resumes that very session, so
    /// making the merge queue again would stall it behind whatever started
    /// instead. Headless there is no session: the turn exited, the answer will
    /// spawn a new one against the transcript on disk, and an approval sitting
    /// unread overnight would otherwise hold half of `concurrency = 2` for a
    /// process that is not there.
    #[test]
    fn a_gated_headless_lane_gives_its_slot_back() {
        let repo = fixture("waiting-slot-detached");
        add_task_with(&repo, "gated", "merge", |f| {
            f.workspace_id = Some("w1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(repo.root.clone());
        });
        add_task(&repo, "next", "queued");
        add_task(&repo, "later", "queued");

        let mux = FakeMux::new(vec![lane(&repo, "gated · merge", LaneStatus::Done)]).detached();
        run_pass(&repo, &mux);

        // One under a multiplexer, where the gate is still holding a session.
        assert_eq!(
            mux.did("start").len(),
            2,
            "a headless gate must free its slot: {:?}",
            mux.did("start")
        );
    }

    /// A step's model is exactly what it names — nothing about the task in
    /// front of it, and no fallback anywhere else, decides it any more.
    // covers: step.model — the model a step names is the model its lane runs
    #[test]
    fn a_steps_model_is_exactly_what_it_names() {
        let mut pipeline = Pipelines::builtin().get("default").unwrap().clone();

        let review = pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "review")
            .unwrap();
        review.model = Some("exactly-this".into());
        assert_eq!(
            resolve_model(pipeline.step("review").unwrap()),
            "exactly-this"
        );

        // A step with no model names none — there is nowhere left to fall
        // back to, and this is now the dispatcher's own backstop for it.
        let bare = pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap();
        bare.model = None;
        assert_eq!(resolve_model(pipeline.step("implement").unwrap()), "");
    }

    /// A step's `effort:` is handed straight to the flag its agent kind
    /// carries one on. The shipped `review` step already asks for one.
    // covers: step.effort — a step's effort reaches the lane on a kind whose argv has somewhere to put it
    #[test]
    fn a_steps_effort_is_rendered_on_a_kind_that_carries_the_flag() {
        let repo = fixture("effort-flag");
        add_task_with(&repo, "demo", "review", |f| {
            f.workspace_id = Some("w1".into());
            f.tab_id = Some("w1:t1".into());
            f.pane_id = Some("w1:p1".into());
            f.worktree_path = Some(a_checkout("dispatch-effort-flag"));
        });
        let mux = FakeMux::new(vec![]);

        run_pass(&repo, &mux);

        let args = mux.did("args demo · review");
        assert_eq!(
            args.len(),
            1,
            "the review lane did not start: {:?}",
            mux.calls()
        );
        let at = args[0]
            .split_whitespace()
            .position(|a| a == "--effort")
            .expect("claude carries an effort flag, so the step's `effort: high` must render");
        assert_eq!(args[0].split_whitespace().nth(at + 1), Some("high"));
    }

    // --------------------------------------------------------- issue_tracking
    //
    // POSIX-only, but not for the command steps above's reason — `fire`
    // (see `crate::tracking`) starts a hook through `command_step::Runs::start`
    // the same as any other run, and that spawns and reads liveness on either
    // platform now. What stays Unix-only is `write_hook`'s own fixture: a
    // `#!/bin/sh` script made executable by its file mode and then run as a
    // bare path, which relies on the shebang line to say what runs it — there
    // is no Windows equivalent of "an executable file names its own
    // interpreter", so these are compiled where that is true rather than
    // failing where it is not.
    /// Writes an executable `.spoolway/hooks/<name>` and points
    /// `[issue_tracking]` at it — the fixture every `issue_tracking` test
    /// below shares.
    #[cfg(unix)]
    fn write_hook(repo: &Repo, name: &str, script: &str) {
        let dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
    }

    /// Runs passes until `path`'s stage stops reading `from`, or panics —
    /// what every `issue_tracking` test below needs to observe a hook's exit
    /// code reaching a *later* pass, since the hook itself is spawned
    /// detached and no one pass waits on it.
    #[cfg(unix)]
    fn pass_until_settled(
        repo: &Repo,
        mux: &FakeMux,
        path: &std::path::Path,
        from: &str,
    ) -> String {
        for _ in 0..200 {
            run_pass(repo, mux);
            let stage = reload(path).stage().to_string();
            if stage != from {
                return stage;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("`{}` never left `{from}`", path.display());
    }

    /// Acceptance criterion: a non-zero hook exit under `on_fail = "pause"`
    /// lands the task on `paused` when the event was `queued` — caught
    /// before the dependency gate would otherwise have let it straight
    /// through, since this task has no dependency to wait on at all.
    #[cfg(unix)]
    #[test]
    fn a_failing_queued_hook_under_on_fail_pause_lands_the_task_on_paused() {
        let mut repo = fixture("hook-queued-pause");
        write_hook(&repo, "fail.sh", "exit 1");
        repo.config.issue_tracking.hook = "fail.sh".into();
        repo.config.issue_tracking.on_fail = "pause".into();
        let path = add_task(&repo, "demo", crate::pipeline::QUEUED);
        let mux = FakeMux::new(vec![]);

        let stage = pass_until_settled(&repo, &mux, &path, crate::pipeline::QUEUED);
        assert_eq!(stage, crate::pipeline::PAUSED);
        assert!(
            mux.did("start").is_empty(),
            "the task must never have started a lane: {:?}",
            mux.calls()
        );
    }

    /// The bug a first pass at this task left in: a hook slower than
    /// `Runs::start`'s own pid-await window (tens of milliseconds) must
    /// still hold the task — reacting only once a code is actually known,
    /// never merely because a pass happened not to see one yet. Without the
    /// fix, this task would have started a lane on the very first pass.
    #[cfg(unix)]
    #[test]
    fn a_slow_failing_queued_hook_under_on_fail_pause_never_starts_the_task() {
        let mut repo = fixture("hook-queued-slow-pause");
        write_hook(&repo, "slow-fail.sh", "sleep 0.3; exit 1");
        repo.config.issue_tracking.hook = "slow-fail.sh".into();
        repo.config.issue_tracking.on_fail = "pause".into();
        let path = add_task(&repo, "demo", crate::pipeline::QUEUED);
        let mux = FakeMux::new(vec![]);

        // Several passes land while the hook is still running — the task
        // must still be at `queued`, and no lane must have started, on every
        // one of them.
        for _ in 0..3 {
            run_pass(&repo, &mux);
            assert_eq!(reload(&path).stage(), crate::pipeline::QUEUED);
            assert!(mux.did("start").is_empty(), "{:?}", mux.calls());
            std::thread::sleep(Duration::from_millis(50));
        }

        let stage = pass_until_settled(&repo, &mux, &path, crate::pipeline::QUEUED);
        assert_eq!(stage, crate::pipeline::PAUSED);
        assert!(mux.did("start").is_empty(), "{:?}", mux.calls());
    }

    /// The bug the second pass at this task left in: `on_fail = "pause"`
    /// with no `hook` configured — or one that fails `is_bare_filename` —
    /// must change nothing at all, per "an empty hook produces no hook runs
    /// and no behaviour change". Gating the hold on `pauses_on_fail` alone
    /// would deadlock every queued task forever, since `fire` never starts
    /// anything for either shape and `exit_code` could only ever read `None`.
    #[cfg(unix)]
    #[test]
    fn on_fail_pause_with_no_hook_configured_never_holds_a_queued_task() {
        for hook in ["", "../escapes.sh"] {
            let mut repo = fixture(&format!("hook-queued-unconfigured-{}", hook.len()));
            repo.config.issue_tracking.hook = hook.into();
            repo.config.issue_tracking.on_fail = "pause".into();
            let path = add_task(&repo, "demo", crate::pipeline::QUEUED);
            let mux = FakeMux::new(vec![]);

            let stage = pass_until_settled(&repo, &mux, &path, crate::pipeline::QUEUED);
            assert_eq!(
                stage, "implement",
                "hook = {hook:?} must not deadlock the queue"
            );
        }
    }

    /// The other half of the same bug: a dry run has nothing to read either,
    /// since `fire` is deliberately skipped for it — so the hold must be
    /// skipped too, or `dispatch --dry-run` reports the opposite of what a
    /// real pass would do.
    #[cfg(unix)]
    #[test]
    fn a_dry_run_under_on_fail_pause_still_says_it_would_start_the_task() {
        let mut repo = fixture("hook-queued-dry-run");
        write_hook(&repo, "pass.sh", "exit 0");
        repo.config.issue_tracking.hook = "pass.sh".into();
        repo.config.issue_tracking.on_fail = "pause".into();
        let path = add_task(&repo, "demo", crate::pipeline::QUEUED);
        let mux = FakeMux::new(vec![]);

        let report = Dispatcher::new(&repo, &Pipelines::builtin(), &mux, true)
            .pass()
            .unwrap();
        assert!(
            report
                .actions
                .iter()
                .any(|a| a.starts_with("would start `implement` for demo")),
            "a dry run under on_fail=pause must still say what a real pass would do: {:?}",
            report.actions
        );
        assert_eq!(
            reload(&path).stage(),
            crate::pipeline::QUEUED,
            "a dry run writes nothing"
        );
    }

    /// A direct test of the two gates `Dispatcher::pass` wraps around every
    /// `issue_tracking` hook — `!self.dry_run` ahead of `RESERVED`'s own
    /// `crate::tracking::fire`, and the same `!self.dry_run` ahead of
    /// `holds_on_fail`'s read of `exit_code` — read off the hook's own run
    /// state rather than inferred from where the task ends up. A dry run
    /// must never start the hook at all: `exit_code` reads `None` before and
    /// after it, on a hook that would otherwise resolve in milliseconds. The
    /// very next pass, for real, is what actually starts it — proving the
    /// gate is the `dry_run` flag itself, not some other reason the hook
    /// never ran.
    #[cfg(unix)]
    #[test]
    fn dry_run_never_fires_the_tracking_hook_the_gate_is_checked_directly() {
        let mut repo = fixture("hook-gate-dry-run");
        write_hook(&repo, "pass.sh", "exit 0");
        repo.config.issue_tracking.hook = "pass.sh".into();
        let path = add_task(&repo, "demo", crate::pipeline::QUEUED);
        let mux = FakeMux::new(vec![]);

        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, true)
            .pass()
            .unwrap();
        assert_eq!(
            crate::tracking::exit_code(&repo, &reload(&path), crate::pipeline::QUEUED),
            None,
            "a dry run must never start the hook `fire` gates"
        );

        Dispatcher::new(&repo, &Pipelines::builtin(), &mux, false)
            .pass()
            .unwrap();
        for _ in 0..200 {
            if crate::tracking::exit_code(&repo, &reload(&path), crate::pipeline::QUEUED).is_some()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("a real pass never started the hook the dry run correctly skipped");
    }

    /// The same failure with `on_fail` left blank (`"ignore"`) changes
    /// nothing about where the task goes — it starts exactly as it would
    /// with no hook at all, and the failure is only ever recorded.
    #[cfg(unix)]
    #[test]
    fn a_failing_queued_hook_under_on_fail_ignore_still_starts_the_task() {
        let mut repo = fixture("hook-queued-ignore");
        write_hook(&repo, "fail.sh", "exit 1");
        repo.config.issue_tracking.hook = "fail.sh".into();
        let path = add_task(&repo, "demo", crate::pipeline::QUEUED);
        let mux = FakeMux::new(vec![]);

        let stage = pass_until_settled(&repo, &mux, &path, crate::pipeline::QUEUED);
        assert_eq!(stage, "implement");

        // The failure still shows up on the board — see `crate::tracking`.
        for _ in 0..200 {
            if crate::tracking::failure_count(&repo) > 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the failed hook was never recorded");
    }

    /// Acceptance criterion: a non-zero hook exit under `on_fail = "pause"`
    /// holds the task out of the archive when the event was `done`.
    #[cfg(unix)]
    #[test]
    fn a_failing_done_hook_under_on_fail_pause_holds_the_task_out_of_the_archive() {
        let mut repo = fixture("hook-done-pause");
        write_hook(&repo, "fail.sh", "exit 1");
        repo.config.issue_tracking.hook = "fail.sh".into();
        repo.config.issue_tracking.on_fail = "pause".into();
        let path = add_task(&repo, "demo", crate::pipeline::DONE);
        let mux = FakeMux::new(vec![]);

        // The hook is given time to actually exit; there is nothing here to
        // wait *until*, since a held task never changes stage — that is the
        // whole point of the hold.
        for _ in 0..50 {
            run_pass(&repo, &mux);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(path.exists(), "held tasks stay in the queue, not archived");
        assert_eq!(reload(&path).stage(), crate::pipeline::DONE);
        assert!(!repo.archive_dir().join("demo.md").exists());
    }

    /// The other bug a first pass left in: a `done` hold had no road out —
    /// once the hook had failed once, its stale exit code held the task
    /// forever, because nothing ever forgot the run so it could try again.
    /// The dispatcher must actually retry it, not just leave that
    /// possibility to `crate::tracking::retry_if_failed`'s own unit test.
    #[cfg(unix)]
    #[test]
    fn a_failing_done_hook_under_on_fail_pause_is_retried_not_stuck_forever() {
        let mut repo = fixture("hook-done-retry");
        write_hook(&repo, "fail.sh", "exit 1");
        repo.config.issue_tracking.hook = "fail.sh".into();
        repo.config.issue_tracking.on_fail = "pause".into();
        let path = add_task(&repo, "demo", crate::pipeline::DONE);
        let mux = FakeMux::new(vec![]);

        let key = crate::command_step::Runs::key(crate::pipeline::DONE, "demo");
        let runs = crate::command_step::Runs::new(&repo.tracking_dir());

        // A second attempt rolls the first one's log aside to `.prev.log` —
        // see `Runs::start` — so its existence is proof a retry actually
        // happened, without racing the exact moment either attempt exits.
        for _ in 0..300 {
            run_pass(&repo, &mux);
            if runs.prev_log_path(&key).exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            runs.prev_log_path(&key).exists(),
            "a second attempt never started — the hold has no road out"
        );
        assert!(
            path.exists(),
            "still held out of the archive while it retries"
        );
        assert_eq!(reload(&path).stage(), crate::pipeline::DONE);
    }

    /// The bug the third pass at this task left in: a `done` hold's own
    /// retry forgets the run — `.exit` file included — on every pass that
    /// still finds it failing, so the board's count must not go quiet the
    /// moment a retry begins. `failure_count` has to keep seeing this key as
    /// failing across many passes of retrying, not just the first one.
    #[cfg(unix)]
    #[test]
    fn the_board_keeps_counting_a_done_hold_while_it_retries() {
        let mut repo = fixture("hook-done-retry-visible");
        write_hook(&repo, "fail.sh", "exit 1");
        repo.config.issue_tracking.hook = "fail.sh".into();
        repo.config.issue_tracking.on_fail = "pause".into();
        add_task(&repo, "demo", crate::pipeline::DONE);
        let mux = FakeMux::new(vec![]);

        // Wait for the first attempt to actually fail before asserting
        // anything — there is nothing to count yet while it is still
        // starting.
        for _ in 0..200 {
            run_pass(&repo, &mux);
            if crate::tracking::failure_count(&repo) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            crate::tracking::failure_count(&repo),
            1,
            "the premise: the first attempt actually failed and was counted"
        );

        // Long enough to see several retries land — the count must never
        // drop to zero across any of them.
        for _ in 0..40 {
            run_pass(&repo, &mux);
            assert_eq!(
                crate::tracking::failure_count(&repo),
                1,
                "the board went quiet about a hook that is still failing"
            );
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    /// A hook failing on `blocked` or `paused` only ever records the
    /// failure — those two are already stopped for a person, so nothing
    /// about the task's stage moves, whatever `on_fail` says.
    #[cfg(unix)]
    #[test]
    fn a_failing_hook_on_blocked_only_records_the_failure() {
        let mut repo = fixture("hook-blocked-record");
        write_hook(&repo, "fail.sh", "exit 1");
        repo.config.issue_tracking.hook = "fail.sh".into();
        repo.config.issue_tracking.on_fail = "pause".into();
        let path = add_task_with(&repo, "demo", crate::pipeline::BLOCKED, |f| {
            f.blocked_from = Some("implement".into());
        });
        let mux = FakeMux::new(vec![]);

        for _ in 0..200 {
            run_pass(&repo, &mux);
            if crate::tracking::failure_count(&repo) > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            reload(&path).stage(),
            crate::pipeline::BLOCKED,
            "recording a failure must not move the task off `blocked`"
        );
    }

    /// `reclaim_scratch` matches a worktree it finds under `scratch` by
    /// string prefix, and `scratch` itself is reached through a symlink here
    /// — standing in for the ordinary case of a temp root that is one on the
    /// host. The worktree is added at the *real* path git resolves it to, so
    /// the two spellings of the same directory never agree without
    /// canonicalising first. Before this task's fix, the mismatch meant the
    /// branch this function exists to clean up was left behind.
    #[cfg(unix)]
    #[test]
    fn reclaim_scratch_matches_a_worktree_reached_through_a_symlink() {
        let repo = fixture("reclaim-scratch-symlink");
        let real = crate::scratch::root("reclaim-scratch-symlink-real");
        let _ = std::fs::remove_dir_all(&real);
        std::fs::create_dir_all(&real).unwrap();

        let scratch = repo.scratch_dir().join("demo");
        std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&real, &scratch).unwrap();

        let worktree = real.join("wt");
        repo.git(&[
            "worktree",
            "add",
            &worktree.display().to_string(),
            "-b",
            "demo-rebase",
        ])
        .unwrap();

        let mux = FakeMux::new(vec![]);
        let pipelines = Pipelines::builtin();
        let dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        dispatcher.reclaim_scratch("demo");

        let branches = repo.git(&["branch", "--list", "demo-rebase"]).unwrap();
        assert!(
            branches.trim().is_empty(),
            "the rebase worktree's branch must be cleaned up, not left behind: {branches:?}"
        );
    }

    /// The other side of canonicalising for the match: `git worktree list`
    /// still names a worktree whose checkout directory is already gone —
    /// exactly what it marks "prunable" — and `canonicalize` on a path that
    /// no longer exists fails outright. Falling back to `false` there, as a
    /// first version of this fix did, refuses the match and leaves the
    /// branch behind; the fallback has to be the raw path instead.
    #[test]
    fn reclaim_scratch_matches_a_worktree_whose_checkout_is_already_gone() {
        let repo = fixture("reclaim-scratch-gone");

        let scratch = repo.scratch_dir().join("demo");
        std::fs::create_dir_all(&scratch).unwrap();
        let worktree = scratch.join("wt");
        repo.git(&[
            "worktree",
            "add",
            &worktree.display().to_string(),
            "-b",
            "demo-rebase",
        ])
        .unwrap();

        // The checkout is gone, but git's own worktree list still carries
        // its `branch` line until something runs `worktree prune` — which
        // `reclaim_scratch` itself only does at the very end, after this
        // match already had to happen.
        std::fs::remove_dir_all(&worktree).unwrap();

        let mux = FakeMux::new(vec![]);
        let pipelines = Pipelines::builtin();
        let dispatcher = Dispatcher::new(&repo, &pipelines, &mux, false);
        dispatcher.reclaim_scratch("demo");

        let branches = repo.git(&["branch", "--list", "demo-rebase"]).unwrap();
        assert!(
            branches.trim().is_empty(),
            "a worktree whose checkout is already gone must still get its branch cleaned up: {branches:?}"
        );
    }
}
