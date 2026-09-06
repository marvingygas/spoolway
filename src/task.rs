//! The task file: a markdown document with YAML frontmatter that is the single
//! source of truth for one unit of pipeline work.
//!
//! Layout on disk:
//!
//! ```text
//! ---
//! id: my-task
//! stage: queued
//! ...
//! ---
//! ## Goal
//! ## Non-goals
//! ## Acceptance criteria
//! ## References
//! ## Status Log
//! ## Handoff
//! ## Blocker
//! ```
//!
//! The first four are written once, at queue time, and are the specification a
//! lane works from; the rest accumulate as it moves through the pipeline.
//!
//! The frontmatter is parsed into [`Frontmatter`]; the body after it is kept as an
//! opaque string so that rewriting a task never reformats prose we did not touch.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// What one lane said on its way out, kept until the dispatcher banks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastReport {
    /// The step the reporting lane was running.
    pub step: String,
    /// `pass`, `fail` or `block`.
    pub outcome: String,
    /// When `spoolway report` banked this, in epoch seconds — the same clock
    /// the dispatcher stamps a lane's own `started_at` from, which is what
    /// makes the two comparable at all.
    ///
    /// Absent on a task file written by an older spoolway, which reads as
    /// zero: older than any lane could have started, so it falls through to
    /// the reminder a report-less lane gets today rather than being read as
    /// one that reported before it ever ran.
    #[serde(default)]
    pub at: i64,
}

/// What a task's branch came to against the commit it started from — files,
/// insertions, deletions, and nothing finer. A replay's comparison is
/// mechanical on purpose; see `docs/eval.md`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    pub files: usize,
    pub insertions: usize,
    pub deletions: usize,
}

/// The typed half of a task file's frontmatter.
///
/// Unrecognised keys are kept in `extra` and written back untouched, so a
/// hand-added field never disappears on the next rewrite.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frontmatter {
    pub id: String,

    /// A Conventional Commits line naming what this task does — a type, the
    /// area of code in parentheses, a colon, and one short present-tense
    /// sentence: `feat(queue): add a --dry-run flag`.
    ///
    /// No heading in the body can supply one, because the body's shape
    /// belongs to the project and a field there is never safe from a
    /// project's own template — the frontmatter is spoolway's, so this is.
    /// It is the subject of the squashed commit `spoolway stack` pushes,
    /// verbatim, and the pull request's title whenever no summary model is
    /// configured to write its own. Required: `queue_add::parse_submission`
    /// refuses a document that leaves it blank, naming the document and the
    /// field. The shape itself is not enforced anywhere — a task file older
    /// than this convention still lands, with its own line as the subject.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,

    /// The pipeline step this task is sitting on. Validated against the loaded
    /// pipeline rather than a fixed enum, so a custom pipeline's step ids are
    /// as valid as the built-in ones.
    pub stage: String,

    /// Glob patterns this task is expected to modify. Used for conflict
    /// detection at queue time and for reviewer-effort auto rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touches: Vec<String>,

    /// Task ids that must finish before this one may start.
    ///
    /// Every id named here has to belong to this task's own `group` — a
    /// chain does not cross a group — and, when there is more than one, the
    /// list has to start with whichever id's own history already reaches
    /// every other one named beside it: that is the id the worktree is cut
    /// from, so it is the only one that can carry the rest along for free.
    /// `queue add --from`'s `check_dependencies_set` refuses a batch that
    /// breaks either rule and reorders the rest, so a task already on disk
    /// is trusted to already have both right.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,

    /// Marks this task as meant to run beside the other declared-parallel
    /// tasks of the same `group:`, rather than as one somebody forgot a
    /// `depends_on` for. A document's own key, written by whoever produced
    /// it — the two planning skills write `parallel: true` — not a flag on
    /// any command.
    ///
    /// It does not excuse a `touches` overlap. Two declared-parallel tasks
    /// of one group that overlap are still reported by `queue conflicts`,
    /// worded as a mistake in how the group was cut rather than a missing
    /// edge: the pair said on purpose that they mean to run beside each
    /// other, and the overlap is what that choice costs.
    ///
    /// Read only by `queue conflicts`, `queue list`, and the two planning
    /// skills. Nothing that schedules or bases a task looks at it: the
    /// dispatcher already starts every ready task at once, so this enables
    /// nothing that was not already possible — it only tells the two queue
    /// commands and the two skills that an overlap was chosen, not forgotten.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parallel: bool,

    /// Whether this task's checkout was already there when its first lane
    /// started, rather than cut for it.
    ///
    /// Written once, when the lane is set up, because it cannot be recovered
    /// afterwards: a worktree spoolway cut and one it borrowed are the same
    /// thing to `git worktree list`. Cleanup reads it to decide whether the
    /// checkout and the branch are its to remove — and they are not, when
    /// somebody else's plan branch is what the task has been working on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub borrowed: bool,

    /// What the last lane to finish on this task reported, and which step it
    /// was running.
    ///
    /// Written by `spoolway report`, which is the one place every lane goes
    /// through, and read once by the dispatcher when it banks that lane's
    /// usage — the two run in different processes, so the task file is the
    /// only channel between them. The step is stored with the outcome because
    /// without it a lane that died silently would inherit the *previous*
    /// step's verdict, and a silent death recorded as a pass is worse than no
    /// record at all.
    ///
    /// One slot is enough because of the order a pass runs in: finished lanes
    /// are freed and banked *before* any new lane is started, so the next step
    /// cannot report until this one has been read. Reversing those two would
    /// silently start losing outcomes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report: Option<LastReport>,

    /// The step this task was on when it stopped.
    ///
    /// Recorded so that whatever picks the task back up — `spoolway resume` by
    /// hand, or an unattended run's own resume — puts it back where it stopped
    /// rather than at the start of its flow: a task that blocked at `handover`
    /// has a branch pushed and a pull request open, and re-running the steps
    /// that did that produces a second one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_from: Option<String>,

    /// The step a person interrupted this task from, with the board's `p`.
    ///
    /// `blocked_from`'s counterpart for a stop nothing reported: a lane never
    /// said it could not go on, a person's own keypress cut its turn short.
    /// Nothing else means that — a real block always carries `blocked_from`,
    /// and this and that are never both set on the same task. Read back by
    /// [`crate::commands::resume`], which puts the task back on this step
    /// rather than treating the stop as a block to clear, and cleared once
    /// the continuing lane has actually launched, in `Dispatcher::start_one`
    /// in `src/dispatch.rs` — the same moment `resume` is spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_from: Option<String>,

    /// The step whose lane is to be *continued* rather than started fresh, for
    /// exactly one launch.
    ///
    /// Written by every road back to a step a task stopped on: `spoolway
    /// unblock` by hand, and an unattended run resuming a block of its own
    /// accord. That step's lane
    /// had already read the task, walked the tree and, often enough, finished
    /// the work before something outside it got in the way; a new session pays
    /// for all of that a second time to arrive back where the last one already
    /// was.
    ///
    /// Consumed by the next launch of that step whatever comes of it, so a
    /// later retry of the same step is a fresh conversation again — which is
    /// what a retry is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,

    /// Pipeline this task runs on. Absent means the default one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,

    /// The chain of work this task belongs to — opaque, and never path-parsed.
    ///
    /// This is the grouping key every reader of the queue uses: the scheduler
    /// drains one group before spreading across several, the board and `queue
    /// list` print one block per group, and `handover`/`adopt` filter by it.
    /// Read verbatim everywhere, unlike the old grouping key this replaced —
    /// that one was a path, so a GitHub issue URL and a plan page with the
    /// same file stem collided under it. A task with no `group:` is a group
    /// of one, the same as a task with no grouping key used to be. Not to be
    /// confused with the unrelated [`Self::plan`] this struct carries today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,

    /// A reference to where this task came from — an issue URL, a plan page
    /// path, a bare name. Nothing in spoolway parses it; it is carried for a
    /// person to follow back, and for nothing else.
    ///
    /// The issue a planning skill read through `spoolway issue show`,
    /// whenever a task started at one — the issue a person filed stays
    /// theirs, so this is never overwritten with the enhanced version spoolway
    /// wrote from it. A page argued that shape too, in that case, and its own
    /// path moves to [`Self::plan`] instead of colliding with this one; with
    /// no issue behind it, a plan page's path stays here, exactly as before
    /// `plan:` existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// The plan page that argued this task's shape, as an absolute path —
    /// set only when a page was written *and* [`Self::source`] holds an
    /// issue instead of it. Nothing in spoolway parses this either; it is
    /// carried for a person to follow back, same as `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,

    /// A step this task pauses after, on the terms a step's own `gate: true`
    /// already sets — see `spoolway report`, where the pause is decided. Set
    /// by whoever wrote the task document, so a producer can hold work for a
    /// person without giving the task a pipeline of its own; a step's own
    /// `gate: true` still gates every task that reaches it, this or not.
    ///
    /// The step rather than a plain on-or-off for the same reason `paused_at`
    /// is: "the end" is a guess in a pipeline with more than one step routing
    /// to done, and naming it lets one task gate at a different step than
    /// another queued on the same pipeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_at: Option<String>,

    /// Branch created for this task. Set when its worktree is created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// Branch the plan lands in — the base of this task's own pull request,
    /// and every dependent's after it. Never the branch the worktree was
    /// actually cut from once there is a `depends_on`; see `cut_from` for
    /// that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,

    /// The run this task's lanes are banked under. Minted once, when the
    /// worktree is first set up, and copied onto every ledger line banked for
    /// it from then on — see [`crate::usage::Entry::run`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,

    /// What this task's worktree was actually cut from: the first
    /// dependency's own branch when `depends_on` names one — read from that
    /// task's `branch:` field, see [`crate::repo::Repo::dependency_branch`] —
    /// and `base` otherwise. Recorded separately from
    /// `base`, which keeps its own meaning — the branch the plan lands in —
    /// whether or not the two agree. Only set when a worktree is actually
    /// cut; a borrowed checkout was already there, cut from something at a
    /// moment nothing here witnessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_from: Option<String>,

    /// The commit `cut_from` pointed at when this task's worktree was cut.
    ///
    /// `cut_from` is a branch name, and branches move — by the time anyone
    /// wants to replay this task, it may be merged and deleted. This is what
    /// a replay pins to instead. Only recorded when a worktree is actually
    /// cut for the task; a borrowed checkout was already there, cut from
    /// something at a moment nothing here witnessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,

    /// What this task's branch came to against `base_commit`, measured at
    /// cleanup — the last instant the branch still exists to diff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<Patch>,

    /// Steps the dispatcher passes this task through without starting a lane —
    /// exactly the fall-through a false `when:` already takes, reached by a
    /// second route. `spoolway eval --replay` used to be the one writer of
    /// this field, letting a queued task walk past the steps that reach
    /// outside the worktree in one pass; that command is gone, and the field
    /// is kept only so an old task file written under it still parses and
    /// round-trips. Its one writer now is the queue screen's `p` picker,
    /// which stamps a trial arm's own ticked steps here so a throwaway run
    /// never pushes a branch or opens a pull request — see
    /// `commands::queue::build_trial_arm`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<String>,

    /// The trial this task is one arm of, minted once per trial and stamped
    /// on every arm the queue screen's `p` picker forks — see
    /// `commands::queue::begin_trial`. Absent on a task queued the ordinary
    /// way. What lets `spoolway eval --runs --trial <id>` find a trial's arms
    /// together in the ledger, since their ids otherwise share nothing but a
    /// minted-together prefix (`solo-1`, `solo-2`, …) that is not itself
    /// recorded anywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<String>,

    /// The run a task replayed, for a task `spoolway eval --replay` once
    /// wrote. Absent on every ordinary task. That command, and the `--show`
    /// that read this back to pair a replay's ledger lines with the run it
    /// answered, are both gone — the field is kept only so an old task file
    /// still parses and round-trips, and nothing writes or reads it any more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_of: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,

    /// Workspace holding the task's worktree, torn down at cleanup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,

    /// The pane the task's very first lane actually ran in — the one
    /// `Mux::create_workspace`, `Mux::create_pane` or `Mux::reopen_owned_pane`
    /// handed back when `ensure_workspace` set the task up, or, under
    /// `grouped`, the pane a project tab opened with if this task is the one
    /// that opened it. Most later steps keep running in that same pane —
    /// handed on from step to step rather than split fresh each time — so
    /// this is usually left as it was set rather than rewritten on every step
    /// change.
    ///
    /// Two things break that, and neither updates this field when they do: a
    /// kind with no quit gesture ends its session by closing the pane
    /// outright (`Vacated::PaneClosed`), and a lane that has not let go of it
    /// by `HANDOVER_WAIT` is stashed and replaced rather than waited on
    /// forever. Either way `start_one` splits a fresh pane for the next step,
    /// so from that point on this field names a pane that is gone.
    ///
    /// `None` under `grouped` for a task that joined a project tab an earlier
    /// task already opened: that tab's one existing pane belongs to somebody
    /// else's lane, so this task splits its own instead, and there is no
    /// pane of its own to record here yet.
    ///
    /// If the first launch out of this pane fails, it is closed — which,
    /// with nothing else yet in a task's own `split`-mode workspace, takes
    /// the multiplexer's workspace and tab down too, though the worktree on
    /// disk survives: `Mux::close_workspace`, what a workspace with no pane
    /// left closes to, leaves what it pointed at untouched.
    /// `ensure_workspace`'s `workspace_alive` check reads the now-gone
    /// workspace as stale on the next pass and reopens a pane on that same
    /// surviving worktree with `Mux::reopen_owned_pane`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,

    /// Tab holding that pane, relabelled on every step transition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,

    /// How many times a lane for this task has finished without reporting a
    /// stage. Drives the retry-then-escalate rule without any judgement call.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub attempts: u32,

    /// Whether this task is currently held for its agent kind's own
    /// usage-limit message rather than for a dead launch — set alongside
    /// [`Self::parked_until`] by `Dispatcher::usage_limit_hold` in
    /// `src/dispatch.rs`. Informational now that the hold itself lives on
    /// `parked_until` rather than on `attempts`/`relaunch_backoff`: nothing
    /// reads this to decide anything, but a person re-reading the document
    /// still wants to tell a quota hold apart from an ordinary park.
    ///
    /// Cleared wherever `attempts` is: by `set_stage`, `set_stage_unbanked`
    /// and `launch_landed`, because a task that has left the step it was
    /// held on — or landed a lane on it — has left the hold behind with it;
    /// and by `parse_submission`'s re-queue normalisation in
    /// `src/commands/queue.rs`, which clears both by hand for a document
    /// coming back through the queue rather than through either path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub usage_limit_hold: bool,

    /// A quota probe's or a usage-limit pane's own clock, in epoch seconds:
    /// no candidate of this task is offered a lane, and no reminder is sent
    /// to a lane already running one, before this passes.
    ///
    /// Written by two different dispatcher checks, both in `dispatch.rs`:
    /// `quota_over_ceiling`, ahead of a launch, from the probe's own
    /// `resets_at` for the window that tripped `agents.<profile>.
    /// quota_ceiling`; and `usage_limit_hold`, for a lane whose pane already
    /// carries its kind's usage-limit phrase, from the same probe when it is
    /// fresh or a doubling backoff when it is not. Either way the park is
    /// written here rather than held anywhere in the dispatcher's own
    /// memory, so a dispatcher that is stopped and restarted — or never
    /// running at all for as long as the wait takes — honours it without
    /// taking a fresh reading: see the top of a pass's own per-task loop,
    /// which reads this before it resolves a step or looks at a lane.
    ///
    /// Cleared by `set_stage` and `set_stage_unbanked`, the same as
    /// `usage_limit_hold` and `attempts` — a task that has moved on has
    /// left whatever parked it behind — and by the pass itself the moment it
    /// finds this in the past, so a task is never skipped a second time on
    /// a clock that has already run out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_until: Option<i64>,

    /// Which of a kind's two windows `parked_until` came from —
    /// [`crate::quota::Window::key`]'s own spelling, `"five_hour"` or
    /// `"seven_day"` — kept only so the clock draws the right way on a pass
    /// that did not just compute it.
    ///
    /// A five-hour park and a seven-day one are different enough in scale
    /// that one display would fail one of them: a bare `HH:MM` loses all
    /// sense of "how far" once the wait runs past today, and a full date
    /// with a duration next to it repeats what a same-day clock already
    /// said. So the shape is decided once, at park time, and remembered
    /// here rather than re-derived from how much of the wait is left —
    /// re-deriving it would have a seven-day park's own display quietly
    /// switch to the five-hour shape on its last day, which is exactly the
    /// day a reader most wants to see how far it has come.
    ///
    /// Empty for the mid-turn usage-limit hold, which is always the
    /// five-hour clock and needs no field to say so — see
    /// `Dispatcher::usage_limit_hold` in `dispatch.rs` — and for a
    /// `parked_until` no writer here classified, which reads the same as
    /// `"five_hour"`, the shorter and commoner shape.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parked_window: String,

    /// The gated step this task is paused on, waiting for a person to release
    /// it — see [`crate::pipeline::PAUSED`].
    ///
    /// The step rather than the destination, because the destination is the
    /// pipeline's to say and the pipeline may have been edited since. Released,
    /// the task goes wherever that step's `on_pass` points *now*; rejected, its
    /// `on_fail`. Cleared by the release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,

    /// When the last of those lanes was launched, in epoch seconds.
    ///
    /// Only an unattended run reads it for the backoff, and only for the one
    /// case it has no person to hand to: a lane that dies at launch, leaving
    /// no session and no pane, so the task looks unstarted again on the very
    /// next pass. Attended, `attempts` alone answers that — the task stops
    /// and waits for somebody. Unattended there is nobody, so the task is
    /// tried again, and this is what spaces the tries out.
    ///
    /// The board borrows it too, as a live lane's clock — `now - launched_at`
    /// for as long as the lane is running. That reading outlives the
    /// backoff's: `launch_landed` forgives the counter the moment a lane is
    /// seen busy, but this field runs on, because arriving somewhere is what
    /// `set_stage` clears it for, not the lane merely being seen. Any future
    /// reader of it, board or backoff, has to ask whether the lane is live
    /// first — this field is no longer self-evidently about one that exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launched_at: Option<i64>,

    /// How many lanes this task has started on each route, keyed `from->to`.
    ///
    /// What a person reads: "prompt 6" is six lanes launched at that step,
    /// however they got there and whether or not each one opened a
    /// conversation of its own. Retries count, because a retried lane is a
    /// second prompt and was paid for like one; [`Frontmatter::attempts`] goes
    /// on counting them separately.
    ///
    /// Per route rather than per step because a step two loops come back to is
    /// two loops. Banked at launch, in [`Task::bank_launch`], unlike
    /// [`Frontmatter::rounds`] beside it — that one is banked once per
    /// arrival, in [`Task::set_stage`], because a lap is a transition and a
    /// relaunch after a lane died before saying anything is a second prompt
    /// on the same lap rather than a second lap.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prompts: BTreeMap<String, u32>,

    /// How many times a task has *arrived* at each step, keyed the same way —
    /// one lap of the loop per arrival, whatever a lane there went on to do.
    /// This is what a step's `loop:` bounds.
    ///
    /// Banked by [`Task::set_stage`], not [`Task::bank_launch`]: a lap is a
    /// transition, and a retried launch at a step the task never left is a
    /// second prompt on the same lap rather than a second one. Whether a lane
    /// opened a fresh conversation or resumed one used to matter here and no
    /// longer does — see [`Frontmatter::prompts`] for what still counts that.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rounds: BTreeMap<String, u32>,

    /// The step this task last moved here from, written by [`Task::set_stage`]
    /// and read by the dispatcher to key the two counters above.
    ///
    /// The counters are banked at launch, and by then the stage is already the
    /// destination — so without this the route a launch belongs to would have
    /// to be guessed from `last_report`, which a lane that died without
    /// reporting leaves pointing at the step before the one that matters. It
    /// survives a retry on purpose: a second lane at the same step arrived by
    /// the same route, and is a second prompt on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrived_from: Option<String>,

    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_norway::Value>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// The `rounds` key for one route through the graph.
///
/// `->` and not a character that needs quoting: this is read in a task file by
/// whoever is working out why something escalated.
pub fn route_key(from: &str, to: &str) -> String {
    format!("{from}->{to}")
}

/// `until` — epoch seconds — in local time: a bare `HH:MM` on the day it
/// falls on `now`, or `YYYY-MM-DD HH:MM` otherwise. `dated` forces the long
/// form even on a day that now matches `now`'s own — see [`format_until`]'s
/// own doc for why a caller ever wants that.
///
/// Shared by every reader of [`Frontmatter::parked_until`] — the board, `queue
/// list` and the dispatcher's own report lines — so a park reads the same
/// clock everywhere it is printed. `now` is a parameter rather than read
/// here, so a test can hold it still.
pub fn format_instant(until: i64, now: i64, dated: bool) -> String {
    let (target, same_day) = local_instant(until, now);
    match dated || !same_day {
        true => target.format("%Y-%m-%d %H:%M").to_string(),
        false => target.format("%H:%M").to_string(),
    }
}

/// [`format_instant`], with the wait remaining appended in parentheses —
/// but only once the bare clock alone has stopped saying enough on its own:
/// a same-day reset already reads as "how soon", and a duration next to it
/// would repeat the answer rather than add one. A reset on a different day
/// carries no sense of scale by itself, which is exactly what the
/// parenthesised wait supplies — the mockup's own `14:00` for a five-hour
/// park against its `2026-09-11 04:00 (6d)` for a seven-day one, and the
/// same `(2h)` still there once that six-day wait has mostly run out.
///
/// `dated` is that last case's whole reason for existing: `now` and `until`
/// can end up on the same calendar day purely because most of a multi-day
/// wait has run out, at which point re-deriving the shape from the two
/// timestamps alone would have the display quietly drop back to the bare
/// five-hour shape on exactly the day a reader most wants to see how far
/// the wait has come. Callers who know which of a kind's windows this park
/// came from — [`Frontmatter::parked_window`] — pass `true` for
/// `"seven_day"` and `false` otherwise, so the shape decided when the park
/// was written survives however close `now` gets to it.
pub fn format_until(until: i64, now: i64, dated: bool) -> String {
    let (_, same_day) = local_instant(until, now);
    let bare = format_instant(until, now, dated);
    if same_day && !dated {
        return bare;
    }
    let remaining = (until - now).max(0) as u64;
    format!(
        "{bare} ({})",
        crate::config::human_duration::format(std::time::Duration::from_secs(remaining)),
    )
}

/// `until` and `now`, both in local time, and whether they fall on the same
/// calendar day — the one question [`format_instant`] and [`format_until`]
/// both have to ask before they can decide their own shape.
///
/// `pub(crate)` rather than private: `spoolway agent verify`'s own quota
/// clause asks the same same-day question, for a shorter display of its
/// own (`MM-DD HH:MM`, no year — see `commands::agent::quota_clause`)
/// rather than either of the two shapes here.
pub(crate) fn local_instant(until: i64, now: i64) -> (DateTime<chrono::Local>, bool) {
    let target = DateTime::from_timestamp(until, 0)
        .unwrap_or_else(Utc::now)
        .with_timezone(&chrono::Local);
    let today = DateTime::from_timestamp(now, 0)
        .unwrap_or_else(Utc::now)
        .with_timezone(&chrono::Local);
    (target, target.date_naive() == today.date_naive())
}

/// A task file as loaded from disk: typed frontmatter plus the untouched body.
#[derive(Debug, Clone)]
pub struct Task {
    pub path: PathBuf,
    pub front: Frontmatter,
    pub body: String,
}

impl Task {
    pub fn id(&self) -> &str {
        &self.front.id
    }

    pub fn stage(&self) -> &str {
        &self.front.stage
    }

    /// How many lanes this task has started at `step`, by any route.
    ///
    /// What a person reads and what the ledger records — "prompt 3 of review"
    /// is about the step, however it got there. The limit is a route's
    /// business; see [`Task::rounds_via`].
    pub fn prompts_at(&self, step: &str) -> u32 {
        let suffix = format!("->{step}");
        self.front
            .prompts
            .iter()
            .filter(|(key, _)| key.as_str() == step || key.ends_with(&suffix))
            .map(|(_, n)| *n)
            .sum()
    }

    /// How many times this task has arrived at `to` from `from` — laps of
    /// that route through the loop. This is what a step's `loop:` bounds.
    pub fn rounds_via(&self, from: &str, to: &str) -> u32 {
        self.front
            .rounds
            .get(&route_key(from, to))
            .copied()
            .unwrap_or(0)
    }

    /// Bank one lane launch at `to`, arriving from `from`.
    ///
    /// The dispatcher calls this at launch. Nothing else may: a counter
    /// banked from two places is one that double-counts the moment the two
    /// disagree about what a launch is.
    ///
    /// Whether the lane opened a conversation of its own or resumed one used
    /// to matter here and no longer does — see [`Frontmatter::rounds`], which
    /// counts arrivals rather than conversations and is banked in
    /// [`Task::set_stage`] instead, once per lap rather than once per launch.
    pub fn bank_launch(&mut self, from: &str, to: &str) {
        let key = route_key(from, to);
        *self.front.prompts.entry(key).or_insert(0) += 1;
    }

    pub fn load(path: &Path) -> Result<Task> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading task file {}", path.display()))?;
        Task::parse(path.to_path_buf(), &raw)
            .with_context(|| format!("parsing task file {}", path.display()))
    }

    /// Split `---\n<yaml>\n---\n<body>` into its two halves.
    pub fn parse(path: PathBuf, raw: &str) -> Result<Task> {
        let (yaml, body) = split_fence(raw)?;
        let body = body.to_string();

        let front: Frontmatter =
            serde_norway::from_str(yaml).context("frontmatter is not valid task YAML")?;

        // `queue add` checks this too, and is not the only way a file gets here:
        // a person edits one, a plan writes several. The id names this task's
        // worktree and every file a lane of it writes under the project's
        // home directory, so it is checked on the way in rather than trusted
        // because of where it came from. See [`crate::config::check_id`].
        crate::config::check_id("task id", &front.id)?;

        // `branch` is spoolway's alone — see `queue::RESERVED_KEYS`. `queue add`
        // stamps `task/<id>`, or `task/<slug>-<id>` when
        // `issue_tracking.key_in_names` prefixed it, and nothing else ever
        // should. A hand-edited file dropped in `queue/`, or one `handover
        // adopt` pulled off a mirror, never passes `queue add`. `spoolway
        // stack` force-pushes a squashed commit onto whatever this says and
        // passes it to `gh pr view` as a positional argument, so a
        // body-authored value is refused here rather than acted on.
        if let Some(branch) = &front.branch
            && !branch_belongs_to(branch, &front.id)
        {
            bail!(
                "task `{}` sets `branch: {branch}`, but spoolway owns that field — \
                 it must be `task/{0}`, or `task/<slug>-{0}` with a slug of lowercase \
                 letters, digits and hyphens, or absent",
                front.id
            );
        }

        Ok(Task { path, front, body })
    }

    /// Serialise back to the on-disk form.
    pub fn render(&self) -> Result<String> {
        let yaml = serde_norway::to_string(&self.front).context("serialising frontmatter")?;
        Ok(format!("---\n{yaml}---\n{}", self.body))
    }

    pub fn save(&self) -> Result<()> {
        let rendered = self.render()?;
        write_atomic(&self.path, &rendered)
            .with_context(|| format!("writing task file {}", self.path.display()))
    }

    /// Move to a new step, bank one round on the route this arrival took, and
    /// append a `## Status Log` line.
    ///
    /// This is the only supported way to change a stage — prompts reach it
    /// through `spoolway report`, the dispatcher calls it directly.
    ///
    /// A lap is a transition, so this is where it is banked — once per
    /// arrival, whatever a lane at the destination goes on to do there.
    /// [`Task::bank_launch`] banks `prompts` separately, once per launch: a
    /// relaunch after a lane died before saying anything calls that again
    /// without calling this again, and costs a prompt rather than a round.
    pub fn set_stage(&mut self, stage: &str, message: Option<&str>) {
        let from = std::mem::replace(&mut self.front.stage, stage.to_string());
        *self
            .front
            .rounds
            .entry(route_key(&from, stage))
            .or_insert(0) += 1;
        self.front.arrived_from = Some(from);
        self.front.attempts = 0;
        self.front.usage_limit_hold = false;
        self.front.parked_until = None;
        self.front.parked_window = String::new();
        self.front.launched_at = None;

        let stamp: DateTime<Utc> = Utc::now();
        let line = match message {
            Some(m) if !m.trim().is_empty() => format!(
                "- {} → `{}`: {}\n",
                stamp.to_rfc3339(),
                stage,
                m.trim().replace('\n', " ")
            ),
            _ => format!("- {} → `{}`\n", stamp.to_rfc3339(), stage),
        };
        self.append_to_section("## Status Log", &line);
    }

    /// Move to `stage` and log why, without banking a lap.
    ///
    /// For exactly one round trip: a person parking a task from the board
    /// with `p`, and putting it back afterwards. Neither direction is a lap
    /// of anything — the task never left the step it was on, so counting one
    /// here would leave a `loop:` budget seeing an arrival no pipeline
    /// routed. See [`Task::set_stage`], which this deliberately does not
    /// call: `rounds` and `arrived_from` are left exactly as they were, and
    /// `attempts`, `usage_limit_hold`, `parked_until`, `parked_window` and
    /// `launched_at` are reset the same way `set_stage` resets them, since
    /// neither a parked task nor the lane it is handed back to has anything
    /// of those left to mean.
    pub fn set_stage_unbanked(&mut self, stage: &str, message: &str) {
        self.front.stage = stage.to_string();
        self.front.attempts = 0;
        self.front.usage_limit_hold = false;
        self.front.parked_until = None;
        self.front.parked_window = String::new();
        self.front.launched_at = None;

        let stamp: DateTime<Utc> = Utc::now();
        let line = format!(
            "- {} → `{}`: {}\n",
            stamp.to_rfc3339(),
            stage,
            message.trim().replace('\n', " ")
        );
        self.append_to_section("## Status Log", &line);
    }

    /// Whether this task's `last_report` was banked after `since` — a lane's
    /// own `started_at`, in the same epoch-seconds clock.
    ///
    /// A report always moves the stage now — no step routes back to itself
    /// any more — but a report and a reminder can still race on the same
    /// pass: the report is what the dispatcher reads, rather than inferring
    /// an answer from movement it might not have seen land yet.
    pub fn reported_since(&self, since: i64) -> bool {
        self.front
            .last_report
            .as_ref()
            .is_some_and(|report| report.at > since)
    }

    /// Forgive the launches counted against this step: one of them left
    /// something behind, so none of them was the failure `attempts` counts.
    ///
    /// [`Frontmatter::attempts`] exists for one case — an agent binary that
    /// dies at launch, leaving no session and no pane, so the task looks
    /// unstarted on the next pass and would be re-spawned forever. It cannot
    /// tell that case from any other reason a lane is not there any more, and
    /// the commonest of those is a person pressing ctrl-c: the stop sweep takes
    /// the worktree back without the task ever changing stage, so nothing zeroes
    /// the counter and the next run blocks the task instead of resuming it.
    ///
    /// So the counter is *spent* at launch and *forgiven* here, wherever
    /// something the launch produced is in evidence — a lane the pass can see, a
    /// session, a worktree the sweep is taking back. That is what "launches that
    /// left nothing behind" always meant, decided where the answer is known
    /// rather than assumed at the moment of asking.
    ///
    /// Forgives `attempts` and `usage_limit_hold` — a launch landing means
    /// whatever `attempts` was counting is over, hold or dead launch alike —
    /// but not `launched_at`. That is the board's clock for a live lane and
    /// outlives the forgiveness — `set_stage` is what clears it, because
    /// arriving somewhere is what restarts it. Folding it in here, as this
    /// once did, forgave the clock about two seconds after every lane
    /// started, and the board read `None` — a dash — for the rest of the
    /// step.
    ///
    /// Answers whether anything changed, so a caller reading every task on
    /// every pass writes only the file that moved.
    pub fn launch_landed(&mut self) -> bool {
        let counted = self.front.attempts > 0;
        self.front.attempts = 0;
        self.front.usage_limit_hold = false;
        counted
    }

    /// Append `text` under `heading`, creating the section if it is absent.
    ///
    /// Sections are appended to rather than replaced so that no writer can
    /// clobber another's section — the dispatcher owns `## Blocker`, every
    /// step's own findings and notes go to `## Handoff`, everyone appends to
    /// `## Status Log`.
    pub fn append_to_section(&mut self, heading: &str, text: &str) {
        match self.find_section(heading) {
            Some((_, end)) => {
                let mut insert_at = end;
                // Back up over trailing blank lines so the new entry sits
                // directly under the existing ones.
                while insert_at > 0 && self.body[..insert_at].ends_with('\n') {
                    let trimmed = self.body[..insert_at].trim_end_matches('\n');
                    if insert_at - trimmed.len() <= 1 {
                        break;
                    }
                    insert_at -= 1;
                }
                self.body.insert_str(insert_at, text);
            }
            None => {
                if !self.body.ends_with('\n') && !self.body.is_empty() {
                    self.body.push('\n');
                }
                if !self.body.ends_with("\n\n") && !self.body.is_empty() {
                    self.body.push('\n');
                }
                self.body.push_str(heading);
                self.body.push('\n');
                self.body.push_str(text);
            }
        }
    }

    /// Byte range of a section's content (after the heading line, up to the
    /// next heading of the same or higher level, or end of body).
    fn find_section(&self, heading: &str) -> Option<(usize, usize)> {
        let level = heading.chars().take_while(|c| *c == '#').count();
        let mut content_start = None;
        let mut offset = 0usize;

        for line in self.body.split_inclusive('\n') {
            let trimmed = line.trim_end();
            match content_start {
                None => {
                    if trimmed.eq_ignore_ascii_case(heading) {
                        content_start = Some(offset + line.len());
                    }
                }
                Some(start) => {
                    let this_level = trimmed.chars().take_while(|c| *c == '#').count();
                    if this_level > 0 && this_level <= level {
                        return Some((start, offset));
                    }
                }
            }
            offset += line.len();
        }

        content_start.map(|start| (start, self.body.len()))
    }

    /// Read a section's content, if present.
    ///
    /// `#[cfg(test)]` only: spoolway writes a task's body and never reads it
    /// back — nothing in `src/` outside a test asks a task file what it
    /// says any more. Kept for what a test still needs to assert on: that a
    /// writer put the right words under the right heading.
    #[cfg(test)]
    pub fn section(&self, heading: &str) -> Option<&str> {
        self.find_section(heading)
            .map(|(start, end)| self.body[start..end].trim())
    }

    /// One extra frontmatter key, read as a plain string — blank when it is
    /// absent or not a string.
    ///
    /// The `[issue_tracking]` `open` hook's four answers — `epic:`, `ticket:`,
    /// `slug:` and `url:` — are the callers. None carries a typed field on
    /// [`Frontmatter`] — see [`Task::set_extra_str`]'s own doc comment for
    /// why — so all four are read back through the same `extra` catch-all any
    /// other unknown key already round-trips through.
    pub fn extra_str(&self, key: &str) -> &str {
        match self.front.extra.get(key) {
            Some(serde_norway::Value::String(s)) => s.as_str(),
            _ => "",
        }
    }

    /// Set one extra frontmatter key to a plain string.
    ///
    /// No typed field on [`Frontmatter`] for the `open` hook's four answers
    /// on purpose. `epic:` and `ticket:` are opaque strings spoolway never
    /// parses — written and read verbatim, exactly the way `group:` and
    /// `source:` already are. `slug:` and `url:` are checked once, at
    /// `queue add` time, before they are set here — a slug against
    /// [`crate::config::check_id`]'s alphabet, a url for an absolute
    /// `http`/`https` scheme — and then carried the same verbatim way. The
    /// catch-all every other hand-added key survives a rewrite through is
    /// enough for all four.
    pub fn set_extra_str(&mut self, key: &str, value: &str) {
        self.front.extra.insert(
            key.to_string(),
            serde_norway::Value::String(value.to_string()),
        );
    }
}

/// Split `---\n<yaml>\n---\n<body>` into its two halves, without deciding
/// anything about what the yaml half means.
///
/// Shared by [`Task::parse`], which trusts every key the yaml half sets, and
/// by `queue::parse_submission`, which trusts none of them until it has
/// checked which ones a document may set at all — both need the same two
/// fences found the same way, and a submitted document is this same shape
/// before it is anything spoolway's.
pub fn split_fence(raw: &str) -> Result<(&str, &str)> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .context("task file does not start with a `---` frontmatter fence")?;

    // The closing fence is a line that is exactly `---`.
    let mut yaml_end = None;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            yaml_end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let (yaml_end, body_start) =
        yaml_end.context("task file has no closing `---` frontmatter fence")?;

    Ok((&rest[..yaml_end], &rest[body_start..]))
}

/// Write via a temporary file + rename so a crash never leaves a half-written
/// task file. Task files are the pipeline's only durable state.
///
/// The dispatcher and a lane's own mid-turn `spoolway report` both write the
/// same task file from different processes — see [`Task::save`] and
/// `Dispatcher::pass`. A temp name that is a pure function of the
/// destination, as this used to be, is the same name for both of them: they
/// share one fd, and one's `truncate`+`write` interleaves with the other's
/// before either gets to `rename`, splicing two writers' bytes into the file
/// that lands. Naming the temp file after this write instead — a pid a
/// second process never shares, paired with a counter so two writes from two
/// threads of *one* process don't share it either — gives every writer its
/// own file to write whole, so the only thing two racing writers can do to
/// each other is have the second `rename` win outright.
///
/// The temp file's data is flushed to disk with `sync_all` *before* the
/// rename, and the parent directory is synced after it on Unix. Without the
/// first sync, a filesystem that persists the rename before the bytes it
/// points at can, after a power loss, leave an empty `<id>.md` — which
/// [`load_dir`] would then have to report as a parse failure for the whole
/// queue. A rename is atomic against a crash without either sync; it is not
/// atomic against the power going out mid-write.
pub fn write_atomic(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let pid = std::process::id();
    let call = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.{pid}-{call}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("spoolway")
    ));
    {
        let mut file = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut file, contents.as_ref())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // The rename itself is a directory-entry change; sync the directory so it
    // survives a power loss too. Best effort — not every platform lets a
    // directory be opened as a file, and a failure here does not mean the
    // rename did not land.
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// One `*.md` file in a task directory that would not load, with the reason
/// it did not — a broken frontmatter fence, an `id:` that fails `check_id`,
/// a stray note that is not a task at all.
#[derive(Debug, Clone)]
pub struct LoadProblem {
    pub path: PathBuf,
    pub error: String,
}

/// Load every `*.md` task file in a directory, sorted by id for stable
/// output, alongside the list of files that would not parse.
///
/// One malformed `*.md` in `queue/` used to fail this call outright, and
/// with it every dispatcher pass, every lane's `spoolway report`, every
/// board frame and every pending listing that reads the queue — a single
/// hand-edit left without its closing `---` froze the whole pipeline with
/// the only trace in `~/.spoolway/logs/<project>.log`. The bad file is now
/// skipped and returned in the second half of the pair, for the caller to
/// name where a person will see it.
pub fn load_dir(dir: &Path) -> Result<(Vec<Task>, Vec<LoadProblem>)> {
    let mut tasks = Vec::new();
    let mut problems = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // An empty queue is a normal state, not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((tasks, problems)),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };

    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        match Task::load(&path) {
            Ok(task) => tasks.push(task),
            Err(e) => problems.push(LoadProblem {
                path,
                error: format!("{e:#}"),
            }),
        }
    }

    tasks.sort_by(|a, b| a.front.id.cmp(&b.front.id));
    problems.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((tasks, problems))
}

/// The unprefixed branch name for a task: `task/<id>`. The one place this
/// shape is written outside `queue add` itself, so a caller that needs the
/// branch of a task whose file records none — a file hand-dropped in `queue/`
/// that never passed `queue add` — reconstructs it here rather than spelling
/// `format!("task/{id}")` out again at each site.
///
/// A task whose file *does* record a `branch:` is read straight off that
/// field — see [`crate::repo::Repo::dependency_branch`] — because
/// `issue_tracking.key_in_names` can make `queue add` stamp a prefixed
/// `task/<slug>-<id>` that this fallback would not reproduce.
/// [`branch_belongs_to`] is the invariant [`Task::parse`] checks a recorded
/// field against.
pub fn default_branch(id: &str) -> String {
    format!("task/{id}")
}

/// Whether `branch` is a branch `queue add` could have stamped for a task
/// named `id`: `task/<id>` plainly, or `task/<slug>-<id>` when
/// `issue_tracking.key_in_names` prefixed it with a tracker slug. The slug
/// itself is opaque — only its alphabet is checked, the same
/// [`crate::config::check_id`] one every id already uses — so this refuses a
/// branch that is neither shape while accepting the prefixed one.
pub fn branch_belongs_to(branch: &str, id: &str) -> bool {
    if branch == default_branch(id) {
        return true;
    }
    let Some(rest) = branch.strip_prefix("task/") else {
        return false;
    };
    let Some(slug) = rest.strip_suffix(&format!("-{id}")) else {
        return false;
    };
    !slug.is_empty() && crate::config::check_id("issue_tracking slug", slug).is_ok()
}

/// Find one task by id across the active queue and the merged archive.
pub fn find(dirs: &[&Path], id: &str) -> Result<Task> {
    for dir in dirs {
        let candidate = dir.join(format!("{id}.md"));
        if candidate.exists() {
            return Task::load(&candidate);
        }
    }
    bail!(
        "no task `{id}` found in {}",
        dirs.iter()
            .map(|d| d.display().to_string())
            .collect::<Vec<_>>()
            .join(" or ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nid: demo\nstage: queued\ntouches: [src/**]\n---\n## Goal\nDo a thing.\n\n## Status Log\n- earlier entry\n";

    /// The mockup's own five-hour park: same local day as `now`, so the bare
    /// clock already says how soon — `format_until` must not repeat that as
    /// a duration in parentheses. `dated` is `false` throughout, the way a
    /// five-hour park's own `parked_window` reads.
    ///
    /// Built through `chrono::Local` directly rather than parsed off a UTC
    /// string, so the assertion holds whatever this machine's own timezone
    /// is — `format_instant`/`format_until` compare local calendar days, and
    /// a fixed UTC instant lands on a different local day depending on the
    /// offset the test happens to run under.
    #[test]
    fn a_same_day_park_carries_no_duration() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 9, 4, 12, 0, 0)
            .unwrap()
            .timestamp();
        let until = chrono::Local
            .with_ymd_and_hms(2026, 9, 4, 14, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(format_instant(until, now, false), "14:00");
        assert_eq!(format_until(until, now, false), "14:00");
    }

    /// The mockup's own six-day park, and the same figure days later with
    /// most of the wait spent: a reset on a different day carries no sense
    /// of scale on its own, which is exactly what the duration supplies.
    /// `dated` is `true` throughout, the way a seven-day park's own
    /// `parked_window` reads — pinned at park time rather than re-derived,
    /// so the shape survives even once `now` has caught up to `until`'s own
    /// calendar day.
    #[test]
    fn a_cross_day_park_carries_its_duration() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 9, 5, 4, 0, 0)
            .unwrap()
            .timestamp();
        let until = chrono::Local
            .with_ymd_and_hms(2026, 9, 11, 4, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(format_instant(until, now, true), "2026-09-11 04:00");
        assert_eq!(format_until(until, now, true), "2026-09-11 04:00 (6d)");

        // Days later, most of the wait spent — now on the same calendar day
        // as `until` itself: still the dated shape and a duration, now
        // shorter, because `dated` still says so.
        let almost_there = chrono::Local
            .with_ymd_and_hms(2026, 9, 11, 2, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(
            format_until(until, almost_there, true),
            "2026-09-11 04:00 (2h)"
        );
    }

    #[test]
    fn parses_and_round_trips() {
        let task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert_eq!(task.id(), "demo");
        assert_eq!(task.stage(), "queued");
        assert_eq!(task.front.touches, vec!["src/**"]);
        assert!(task.body.starts_with("## Goal"));

        let reparsed = Task::parse(PathBuf::from("demo.md"), &task.render().unwrap()).unwrap();
        assert_eq!(reparsed.stage(), "queued");
        assert_eq!(reparsed.body, task.body);
    }

    #[test]
    fn set_stage_appends_to_existing_status_log() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.set_stage("review", Some("done"));

        assert_eq!(task.stage(), "review");
        let log = task.section("## Status Log").unwrap();
        assert!(log.contains("earlier entry"));
        assert!(log.contains("→ `review`: done"));
        // Exactly one Status Log heading — we appended, not duplicated.
        assert_eq!(task.body.matches("## Status Log").count(), 1);
    }

    /// A park-and-resume round trip through `set_stage_unbanked` banks
    /// nothing: `prompts`, `rounds` and `arrived_from` come out byte-for-byte
    /// as they went in, which is exactly what a person interrupting a turn
    /// and putting it back must look like — the task never left the step.
    #[test]
    fn set_stage_unbanked_round_trip_banks_nothing() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.set_stage("implement", Some("moved on"));
        let prompts_before = task.front.prompts.clone();
        let rounds_before = task.front.rounds.clone();
        let arrived_from_before = task.front.arrived_from.clone();

        task.set_stage_unbanked("paused", "paused from the board");
        task.set_stage_unbanked("implement", "put back from the board");

        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.prompts, prompts_before);
        assert_eq!(task.front.rounds, rounds_before);
        assert_eq!(task.front.arrived_from, arrived_from_before);
        let log = task.section("## Status Log").unwrap();
        assert!(log.contains("→ `paused`: paused from the board"));
        assert!(log.contains("→ `implement`: put back from the board"));
        assert!(!log.to_lowercase().contains("unblocked"));
    }

    /// `launch_landed` forgives the launch counter but leaves `launched_at`
    /// running — that field is the board's clock for a live lane now, and
    /// only `set_stage` clears it, because arriving somewhere is what
    /// restarts the clock, not a pass merely seeing the lane busy.
    #[test]
    fn launch_landed_clears_attempts_only() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.front.attempts = 2;
        task.front.launched_at = Some(1_000);

        assert!(task.launch_landed(), "attempts was set, so this counted");
        assert_eq!(task.front.attempts, 0);
        assert_eq!(task.front.launched_at, Some(1_000));

        // A second call with nothing left to forgive answers `false`, and
        // still leaves the clock alone.
        assert!(!task.launch_landed());
        assert_eq!(task.front.launched_at, Some(1_000));

        // `set_stage` is what clears it — arriving somewhere restarts the
        // clock for the step just reached.
        task.set_stage("fix", None);
        assert_eq!(task.front.launched_at, None);
    }

    /// A stage change is an arrival, and a lap is a transition: `set_stage`
    /// banks the round on its own, before any lane has even started. Launching
    /// a lane there is a separate cost, banked separately.
    #[test]
    fn a_transition_banks_a_round_on_its_own() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.set_stage("fix", None);
        task.set_stage("review", None);

        assert_eq!(task.prompts_at("fix"), 0);
        assert_eq!(task.rounds_via("queued", "fix"), 1);
        assert_eq!(task.rounds_via("fix", "review"), 1);
        // But the route is recorded, which is what a launch banks against.
        assert_eq!(task.front.arrived_from.as_deref(), Some("fix"));
    }

    /// A retried launch at a step the task never left is a second prompt, not
    /// a second lap — the lap was already banked on arrival, once, by
    /// `set_stage`.
    #[test]
    fn a_retried_launch_is_a_prompt_but_not_a_second_round() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.set_stage("fix", None);
        task.bank_launch("review", "fix");
        task.bank_launch("review", "fix");
        task.bank_launch("review", "fix");

        assert_eq!(task.prompts_at("fix"), 3);
        assert_eq!(
            task.rounds_via("review", "fix"),
            0,
            "`set_stage` never ran again"
        );
    }

    /// The gap this counting exists to close: `review` and `e2e` both send
    /// failures to `fix`, and they are two loops. What one spends must leave
    /// the other's budget alone.
    #[test]
    fn each_route_into_a_step_counts_separately() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.set_stage("review", None);
        task.set_stage("fix", None);
        task.set_stage("review", None);
        task.set_stage("e2e", None);
        task.set_stage("fix", None);

        assert_eq!(task.rounds_via("review", "fix"), 1);
        assert_eq!(task.rounds_via("e2e", "fix"), 1);
        assert_eq!(task.rounds_via("merge", "fix"), 0);
    }

    /// A task already in flight when this shipped carries `new_sessions:`,
    /// which nothing reads any more. It lands in `extra` and is written back
    /// untouched, and `rounds:` starts empty — so the task's next loop gets a
    /// full budget rather than being escalated on arrival by a number that
    /// was counting conversations, not laps.
    #[test]
    fn an_older_tasks_new_sessions_key_bounds_nothing_and_survives_a_rewrite() {
        let raw = "---\nid: demo\nstage: fix\nnew_sessions:\n  review->fix: 9\n---\nbody\n";
        let task = Task::parse(PathBuf::from("demo.md"), raw).unwrap();

        assert_eq!(task.rounds_via("review", "fix"), 0);
        assert!(task.render().unwrap().contains("review->fix: 9"));
    }

    #[test]
    fn creates_missing_section() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.append_to_section("## Blocker", "- llama-server unreachable\n");
        assert_eq!(
            task.section("## Blocker").unwrap(),
            "- llama-server unreachable"
        );
        // The pre-existing section is untouched.
        assert!(task.section("## Status Log").unwrap().contains("earlier"));
    }

    #[test]
    fn run_base_commit_and_patch_round_trip_and_stay_out_when_unset() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert!(!task.render().unwrap().contains("run:"));
        assert!(!task.render().unwrap().contains("base_commit:"));
        assert!(!task.render().unwrap().contains("patch:"));

        task.front.run = Some("r00001".into());
        task.front.base_commit = Some("4f1e9a2".into());
        task.front.patch = Some(Patch {
            files: 4,
            insertions: 168,
            deletions: 44,
        });
        let rendered = task.render().unwrap();
        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert_eq!(reparsed.front.run.as_deref(), Some("r00001"));
        assert_eq!(reparsed.front.base_commit.as_deref(), Some("4f1e9a2"));
        assert_eq!(
            reparsed.front.patch,
            Some(Patch {
                files: 4,
                insertions: 168,
                deletions: 44
            })
        );
    }

    /// The `branch:` invariant accepts the two shapes `queue add` stamps —
    /// `task/<id>` and, when `issue_tracking.key_in_names` prefixed it,
    /// `task/<slug>-<id>` — and refuses anything else.
    #[test]
    fn branch_invariant_accepts_the_prefixed_form_and_refuses_the_rest() {
        assert!(branch_belongs_to("task/auth-01", "auth-01"));
        assert!(branch_belongs_to("task/proj-12-auth-01", "auth-01"));
        assert!(branch_belongs_to("task/p-auth-01", "auth-01"));

        // A prefix that is not a valid id, a different id, a different
        // namespace, no prefix at all.
        assert!(!branch_belongs_to("task/PROJ-12-auth-01", "auth-01"));
        assert!(!branch_belongs_to("task/proj-12-auth-02", "auth-01"));
        assert!(!branch_belongs_to("feature/auth-01", "auth-01"));
        assert!(!branch_belongs_to("task/-auth-01", "auth-01"));
        assert!(!branch_belongs_to("auth-01", "auth-01"));

        let raw = "---\nid: auth-01\nstage: implement\nbranch: task/proj-12-auth-01\n---\nbody\n";
        let task = Task::parse(PathBuf::from("auth-01.md"), raw).unwrap();
        assert_eq!(task.front.branch.as_deref(), Some("task/proj-12-auth-01"));

        let bad = "---\nid: auth-01\nstage: implement\nbranch: task/whatever\n---\nbody\n";
        let err = Task::parse(PathBuf::from("auth-01.md"), bad).unwrap_err();
        assert!(format!("{err:#}").contains("spoolway owns that field"));
    }

    /// `cut_from` is what `spoolway queue show` prints for what the worktree
    /// was actually cut from — a line of its own, distinct from `base`, which
    /// keeps meaning the branch the plan lands in.
    #[test]
    fn cut_from_round_trips_and_stays_out_when_unset_and_stands_apart_from_base() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert!(!task.render().unwrap().contains("cut_from:"));

        task.front.base = Some("main".into());
        task.front.cut_from = Some("task/dependency".into());
        let rendered = task.render().unwrap();
        assert!(rendered.contains("base: main"));
        assert!(rendered.contains("cut_from: task/dependency"));

        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert_eq!(reparsed.front.base.as_deref(), Some("main"));
        assert_eq!(reparsed.front.cut_from.as_deref(), Some("task/dependency"));
    }

    /// `at` round-trips like any other field, and a task file written before
    /// it existed parses with it read as zero rather than refusing to load.
    #[test]
    fn last_report_at_round_trips_and_an_absent_one_reads_as_zero() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.front.last_report = Some(LastReport {
            step: "implement".into(),
            outcome: "pass".into(),
            at: 1_786_900_000,
        });
        let rendered = task.render().unwrap();
        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert_eq!(
            reparsed.front.last_report.unwrap().at,
            1_786_900_000,
            "{rendered}"
        );

        // The shape an older spoolway left behind: `last_report` with no `at`
        // at all.
        let older = "---\nid: demo\nstage: blocked\nlast_report:\n  step: implement\n  \
                      outcome: block\n---\n";
        let task = Task::parse(PathBuf::from("demo.md"), older).unwrap();
        assert_eq!(task.front.last_report.unwrap().at, 0);
    }

    /// The comparison the settled arm decides a self-routing step by: newer
    /// than the lane's own start is a report banked this round, and anything
    /// else — no report, or one left over from the round before — is not.
    #[test]
    fn reported_since_reads_the_reports_own_clock_against_the_lanes_start() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert!(
            !task.reported_since(1_000),
            "no report at all is not a report"
        );

        task.front.last_report = Some(LastReport {
            step: "blocked".into(),
            outcome: "block".into(),
            at: 1_000,
        });
        assert!(
            !task.reported_since(1_000),
            "a report banked before the lane started is the round before's"
        );
        assert!(task.reported_since(999), "banked after the lane started");
    }

    #[test]
    fn parallel_round_trips_and_stays_out_of_the_file_when_false() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert!(!task.render().unwrap().contains("parallel:"));

        task.front.parallel = true;
        let rendered = task.render().unwrap();
        assert!(rendered.contains("parallel: true"));
        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert!(reparsed.front.parallel);
    }

    /// `title:` round-trips like any other plain key, and an older task file
    /// written before this field existed still parses — it reads as empty
    /// rather than refusing the file.
    #[test]
    fn title_round_trips_and_an_older_task_file_reads_it_as_empty() {
        let task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert_eq!(task.front.title, "");
        assert!(!task.render().unwrap().contains("title:"));

        let mut task = task;
        task.front.title = "cut a dependent's worktree from its dependency".into();
        let rendered = task.render().unwrap();
        assert!(rendered.contains("title: cut a dependent's worktree from its dependency"));
        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert_eq!(
            reparsed.front.title,
            "cut a dependent's worktree from its dependency"
        );
    }

    /// `gate_at` is a plain frontmatter key, set by whoever wrote the
    /// document — it round-trips like any other, and stays out of the file
    /// entirely when a task never named one.
    #[test]
    fn gate_at_round_trips_and_stays_out_when_unset() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert!(!task.render().unwrap().contains("gate_at:"));

        task.front.gate_at = Some("handover".into());
        let rendered = task.render().unwrap();
        assert!(rendered.contains("gate_at: handover"));
        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert_eq!(reparsed.front.gate_at.as_deref(), Some("handover"));
    }

    #[test]
    fn plan_round_trips_and_stays_out_when_unset() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert!(!task.render().unwrap().contains("plan:"));

        task.front.plan = Some("/plans/2026-08-28-session-store.html".into());
        let rendered = task.render().unwrap();
        assert!(rendered.contains("plan: /plans/2026-08-28-session-store.html"));
        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert_eq!(
            reparsed.front.plan.as_deref(),
            Some("/plans/2026-08-28-session-store.html")
        );
    }

    #[test]
    fn skip_and_replay_of_round_trip_and_stay_out_when_unset() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert!(!task.render().unwrap().contains("skip:"));
        assert!(!task.render().unwrap().contains("replay_of:"));

        task.front.skip = vec!["handover".into(), "land".into()];
        task.front.replay_of = Some("r2c904".into());
        let rendered = task.render().unwrap();
        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert_eq!(reparsed.front.skip, vec!["handover", "land"]);
        assert_eq!(reparsed.front.replay_of.as_deref(), Some("r2c904"));
    }

    #[test]
    fn unknown_frontmatter_keys_survive_a_rewrite() {
        let raw = "---\nid: demo\nstage: queued\nmy_custom: hello\n---\nbody\n";
        let task = Task::parse(PathBuf::from("demo.md"), raw).unwrap();
        assert!(task.render().unwrap().contains("my_custom: hello"));
    }

    /// `epic:` and `ticket:` round-trip through `extra` like any other
    /// unknown key, opaque and unparsed — `extra_str` reads blank until
    /// `set_extra_str` writes one, and the write survives a save/reparse.
    #[test]
    fn epic_and_ticket_round_trip_through_the_extra_map() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert_eq!(task.extra_str("epic"), "");
        assert_eq!(task.extra_str("ticket"), "");

        task.set_extra_str("epic", "https://github.com/acme/app/issues/42");
        task.set_extra_str("ticket", "https://github.com/acme/app/issues/43");
        let rendered = task.render().unwrap();
        assert!(rendered.contains("epic: https://github.com/acme/app/issues/42"));
        assert!(rendered.contains("ticket: https://github.com/acme/app/issues/43"));

        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert_eq!(
            reparsed.extra_str("epic"),
            "https://github.com/acme/app/issues/42"
        );
        assert_eq!(
            reparsed.extra_str("ticket"),
            "https://github.com/acme/app/issues/43"
        );
    }

    /// One unparsable `*.md` in the directory is skipped and named, and every
    /// other file still loads — the freeze this closes was `load_dir`
    /// returning the first parse error and nothing else.
    #[test]
    fn load_dir_skips_the_bad_file_and_names_it() {
        let dir = crate::scratch::root("load-dir-bad-file");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("good.md"),
            "---\nid: good\nstage: queued\n---\n## Goal\nok\n",
        )
        .unwrap();
        std::fs::write(dir.join("broken.md"), "---\nid: broken\nstage: queued\n").unwrap();

        let (tasks, problems) = load_dir(&dir).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id(), "good");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].path.ends_with("broken.md"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_file_with_no_frontmatter() {
        assert!(Task::parse(PathBuf::from("demo.md"), "## Goal\nno fence\n").is_err());
        assert!(Task::parse(PathBuf::from("demo.md"), "---\nid: demo\n").is_err());
    }

    /// `queue add` is not the only way a task file appears: a person writes one,
    /// a plan writes several. The id names the task's worktree and every file a
    /// lane of it writes under the project's home directory, so it is checked
    /// on the way in — a file that traverses out of the queue never becomes a
    /// `Task` at all.
    #[test]
    fn a_task_file_whose_id_escapes_the_queue_does_not_parse() {
        let raw = "---\nid: ../../pwn\nstage: queued\n---\nbody\n";
        assert!(Task::parse(PathBuf::from("pwn.md"), raw).is_err());
    }

    /// The dispatcher and a lane's own `spoolway report` both call
    /// `write_atomic` on the same task file — see `Dispatcher::pass`.
    /// Real OS threads exercise the same `create`/`write`/`rename` syscalls
    /// two processes racing the same destination would; the kernel's
    /// atomicity guarantee on each is not process-specific, so this is a
    /// faithful stand-in for two dispatchers writing the same file at once.
    /// Before this task's fix, both writers shared one temp file name and
    /// could interleave into it; every racing write below has to land whole.
    #[test]
    fn two_writers_racing_write_atomic_never_splice_the_destination() {
        let dir = crate::scratch::root("write-atomic-race");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("task.md");

        let writers = 16;
        let handles: Vec<_> = (0..writers)
            .map(|i| {
                let path = path.clone();
                // Long enough, and different enough per writer, that a
                // splice of any two would fail this shape check below.
                let contents = format!("writer-{i}:{}", "x".repeat(200 + i));
                std::thread::spawn(move || write_atomic(&path, contents.clone()).map(|_| contents))
            })
            .collect();
        let contents: Vec<String> = handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect();

        let landed = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains(&landed),
            "the file on disk must be exactly one writer's content, not a splice of two"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
