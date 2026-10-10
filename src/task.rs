//! The task file: markdown with YAML frontmatter that is the single
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
use chrono::Local;
use serde::{Deserialize, Serialize};

/// What opens the line [`Task::mark_blocker_cleared`] adds, and what it looks
/// for to avoid adding a second.
const BLOCKER_CLEARED: &str = "- Cleared";

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
    /// Whether this report left the task on `blocked`, whatever the lane
    /// said: a `--block` does, and so does a pass or fail whose next step has
    /// spent its `loop:`, a `--fail` from a step that declares no `on_fail`,
    /// or a pass whose worktree could not be committed. The
    /// outcome alone cannot say so — it stays `pass` or `fail`, which is what
    /// the pass rate reads — so `spoolway eval` counts blocks from this.
    ///
    /// Absent on a task file written by an older spoolway, which reads as
    /// `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blocked: bool,
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

/// Frontmatter keys of the quota-and-usage-limit park that no `Frontmatter`
/// field answers to any more — see the comment above where the five used to
/// live. Stripped out of `extra` in [`Task::parse`] so a task file still
/// carrying one from before this shipped drops it on its next save rather
/// than round-tripping it forever, the way an ordinary unrecognised key
/// would. `pub(crate)` so `commands::queue::parse_submission`, which
/// deserialises a submitted task's own frontmatter directly rather than
/// through [`Task::parse`], strips the same keys from its own `extra`.
pub(crate) const RETIRED_PARK_KEYS: &[&str] = &[
    "usage_limit_hold",
    "quota_retries",
    "parked_until",
    "parked_window",
    "parked_at",
];

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
    /// verbatim, and the pull request's title. Required:
    /// `queue_add::parse_submission` refuses a task that leaves it
    /// blank, naming the task and the field. The shape itself is not
    /// enforced anywhere — a task file older than this convention still
    /// lands, with its own line as the subject.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,

    /// The pipeline step this task is sitting on. Validated against the loaded
    /// pipeline rather than a fixed enum, so a custom pipeline's step ids are
    /// as valid as the built-in ones.
    pub stage: String,

    /// Task ids that must finish before this one may start.
    ///
    /// A group is one chain: every id named here has to belong to this
    /// task's own `group`, with one exception — a group's first task (the
    /// one with no dependency inside its own group) may instead name
    /// exactly one other group's own last task, stacking this group onto
    /// that one. When there is more than one id and they are all in this
    /// task's own group, the list has to start with whichever id's own
    /// history already reaches every other one named beside it: that is the
    /// id the worktree is cut from, so it is the only one that can carry the
    /// rest along for free. `queue add --from`'s `check_dependencies_set`
    /// refuses a batch that breaks any of these rules and reorders the
    /// rest, so a task already on disk is trusted to already have them
    /// right.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,

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
    /// The dispatcher also writes it for a command step whose exit a gate
    /// held, so `caught_at` can tell a held pass from a held fail on resume.
    /// That is safe for the banking, which only reads a report whose step
    /// matches the lane's own step, and a command step has no lane.
    ///
    /// One slot is enough because of the order a pass runs in: finished lanes
    /// are finished and banked *before* any new lane is started, so the next step
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

    /// The step this task stopped on without failing a check, for `spoolway
    /// resume` to carry it straight back to.
    ///
    /// `blocked_from`'s counterpart for a stop nothing reported — but the two
    /// can both be set at once: parking a `blocked` row (the board's `p`, or
    /// `spoolway queue pause`) writes `parked_from: blocked` beside whatever
    /// `blocked_from` the block already carried, and leaves that field alone.
    /// `resume_road` in `src/commands/report.rs` checks `parked_from`
    /// first, so it is the one that decides where a resume goes when both are
    /// set. Three gestures set it — a person's own keypress (the
    /// board's `p`), a person's own Escape typed into the pane (see
    /// `Dispatcher::park_after_interrupt`), and a lane `Dispatcher::
    /// escalate_clock` gave up on for going quiet — and [`Self::escalated`]
    /// is what tells the last of those apart from the first two once a lane
    /// resumes here. Read back by [`crate::commands::resume`], which puts the
    /// task back on this step rather than treating the stop as a block to
    /// clear, and cleared once the continuing lane has actually launched, in
    /// `finish_launch_bookkeeping` in `src/dispatch.rs` — the same moment
    /// `resume` is spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_from: Option<String>,

    /// Whether the dispatcher gave up on a lane, rather than a person
    /// stopping it. The mark has two meanings.
    ///
    /// Beside [`Self::parked_from`], it says that step is one `escalate_clock`
    /// gave up on, not a person's own keypress or Escape. It is the one fact
    /// `prepare_boot` cannot otherwise recover once a lane resumes:
    /// `parked_from` alone reads the same for all three gestures that set it.
    /// A person's interrupt genuinely changed nothing, and the resumed lane
    /// is told so by `park_prompt`; an escalation reminded the lane three
    /// times, tore its pane down and wrote a `## Status Log` line saying why
    /// — telling it nothing changed would be false. This meaning is spent
    /// with `parked_from`, in the same places and at the same moment.
    ///
    /// Beside [`Self::blocked_from`], with no `parked_from`, it says an
    /// unattended run stopped the lane and sent the task to `blocked`, so the
    /// board's RECENT line can tell a stopped lane from one dead at launch.
    /// That meaning is cleared by `Task::set_stage` when the task leaves
    /// `blocked`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub escalated: bool,

    /// Whether this task was parked by the dispatch tab's stop popup — its
    /// `i`, which interrupts every running step as dispatching stops — rather
    /// than by a person's own `p`, Escape in the pane, or an escalation.
    ///
    /// The one thing that sets a stop's parks apart from every other park:
    /// all of them write the same `parked_from`, and a restarted dispatcher
    /// reads an interrupted turn exactly as a person's own Escape. Read once,
    /// by `spoolway dispatch` as it starts — see
    /// `crate::status::resume_stop_parked` — which resumes every task still
    /// carrying it. Cleared by every road back out of `paused`
    /// (`back_onto_its_step` in `src/commands/report.rs`), and by any later
    /// park, so a mark can never outlive the stop that wrote it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub parked_by_stop: bool,

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

    /// The step to start over on, for exactly one launch: a fresh
    /// conversation even where the step declares `session: true`.
    ///
    /// Every other road onto a step continues a conversation the step
    /// already has, so a conversation that cannot work is walked back into
    /// by resume and by the next retry alike. This one ends it. `prepare_boot`
    /// reads it as `restarting` when it names the step being started, skips
    /// both session lookups, and briefs the lane as if the step had never
    /// run, plus a sentence saying the worktree is not clean.
    /// `finish_launch_bookkeeping` spends it in the same pass as `resume` and
    /// `parked_from`, so the launch after a restart is an ordinary retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart: Option<String>,

    /// Pipeline this task runs on — the only routing source spoolway reads
    /// any more; there is no project default to fall back to. Still
    /// `Option` rather than required at this layer, on purpose: a legacy or
    /// hand-edited file predating that rule can still reach the live queue,
    /// and something has to be able to parse it far enough to name the
    /// problem — `queue add`'s own submission path is what actually refuses
    /// a task naming none, and `spoolway dispatch`'s own start preflight
    /// refuses the whole run over any live task still missing one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,

    /// The chain of work this task belongs to — opaque, and never path-parsed.
    ///
    /// This is the grouping key every reader of the queue uses: the scheduler
    /// drains one group before spreading across several, and the board and
    /// `queue list` print one block per group. Read verbatim everywhere, unlike
    /// the old grouping key this replaced —
    /// that one was a path, so a GitHub issue URL and a plan page with the
    /// same file stem collided under it. A task with no `group:` is a group
    /// of one, the same as a task with no grouping key used to be. Not to be
    /// confused with the unrelated [`Self::plan`] this struct carries today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,

    /// The group's own words for the issue a mirror opens above it —
    /// carried verbatim into `SPOOLWAY_GROUP_DESCRIPTION` on the `open`
    /// event, and into nothing else spoolway does. Only one task of a
    /// group needs to set it; `queue add`'s own `validate_batch` looks at
    /// every task of a submission's group together, not this one field
    /// alone, and refuses the whole batch when a hook is configured and none
    /// of them carry it — an issue with a hook but no group description
    /// would have nothing of its own to say. Never required when no hook is
    /// configured: a project with no `issue_tracking.hook` set pays for none
    /// of this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_description: Option<String>,

    /// Plain words carried onto this task's own tracker issue, and its
    /// group's, beside whatever `spoolway:*` labels the hook adds itself —
    /// see `assets/hooks/github.sh`'s own `open` branch. `queue add`'s
    /// `parse_submission` refuses a batch where any one of them holds
    /// whitespace or a comma, naming the task and the label: a comma is the
    /// join character [`crate::tracking::build_env`] and [`crate::tracking::
    /// open_env`] hand a hook in `SPOOLWAY_LABELS`, and Jira's own labels
    /// cannot hold a space at all, so neither is ever safe inside one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,

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

    /// A step this task pauses after, whatever it reports or exits with —
    /// see `spoolway report`, where an agent step's pause is decided, and the
    /// dispatcher's command arm, where a command step's is. Set by whoever wrote
    /// the task, so a producer can hold work for a person without
    /// giving the task a pipeline of its own; a step's own `gate: true`
    /// still gates every task that reaches it, this or not, but on
    /// narrower terms than this field's: it only ever catches a pass, and
    /// only one whose destination is not already `blocked`.
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
    /// actually cut from once there is a `depends_on`; see `starts_from` for
    /// that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,

    /// The run this task's lanes are banked under. Minted once, when the
    /// worktree is first set up, and copied onto every ledger line banked for
    /// it from then on — see [`crate::usage::Entry::run`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,

    /// The branch this task's worktree starts from: its own value when a
    /// person set one before the cut, else the first dependency's own branch
    /// when `depends_on` names one — read from that task's `branch:` field,
    /// see [`crate::repo::Repo::dependency_branch`] — and `base` otherwise.
    /// Recorded separately from `base`, which keeps its own meaning — the
    /// branch the plan lands in — whether or not the two agree.
    ///
    /// Two writers: a person, before the cut, for a start branch the
    /// dispatcher cannot work out (a dependency whose branch was deleted when
    /// its pull request merged), and spoolway at the cut, which stamps the
    /// branch it used. So a value here does not mean the task has started —
    /// its recorded `worktree_path` does. A borrowed checkout is never
    /// stamped: it was already there, cut from something at a moment nothing
    /// here witnessed. Task files written when the field was called
    /// `cut_from` still read.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "cut_from")]
    pub starts_from: Option<String>,

    /// The commit `starts_from` pointed at when this task's worktree was cut.
    ///
    /// `starts_from` is a branch name, and branches move — by the time anyone
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
    /// round-trips. Its one writer now is the queue screen's `t` picker,
    /// which stamps a trial arm's own ticked steps here so a throwaway run
    /// never pushes a branch or opens a pull request — see
    /// `commands::queue::build_trial_arm`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip: Vec<String>,

    /// The trial this task is one arm of, minted once per trial and stamped
    /// on every arm the queue screen's `t` picker forks — see
    /// `commands::queue::begin_trial`. Absent on a task queued the ordinary
    /// way. What lets `spoolway eval --by task --trial <id>` find a trial's arms
    /// together in the ledger: a trial forks a whole group once per ticked
    /// pipeline, one arm per source task in each copy (`alpha-1`,
    /// `beta-1`, `alpha-2`, …), each copy in a group of its own, so those
    /// arms come from different source tasks and different groups and share
    /// nothing else — not even an id prefix — to group them by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial: Option<String>,

    /// The group a trial arm was forked from, stamped beside [`Self::trial`]
    /// on every arm by `commands::queue::begin_trial`. Absent on a task
    /// queued the ordinary way. An arm runs in a group of its own,
    /// `<group>-<pipeline>`, so its own `group:` no longer names the group
    /// a person tried; and once a trial settles its arms are deleted, so
    /// this, carried onto every ledger line the arm banks, is the only
    /// record left of which group the trial compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trial_group: Option<String>,

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
    /// `Mux::create_workspace`, `Mux::create_pane` or
    /// `Mux::reopen_owned_pane` handed back when `ensure_workspace` set the
    /// task up. Every later step splits a pane of its own and leaves this
    /// field as it was set, so it is the first step's pane and no other.
    ///
    /// A step that comes back replaces its own pane, and when that step is
    /// the first one the pane this field names is closed and the field is not
    /// updated. From then on it names a pane that is gone.
    ///
    /// If the first launch out of this pane fails, it is closed — which,
    /// with nothing else yet in a task's own workspace, takes
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

    // `usage_limit_hold`, `quota_retries`, `parked_until`, `parked_window`
    // and `parked_at` — the quota-and-usage-limit park's own five fields —
    // lived here through the gate and the detector that wrote them, and
    // then a while longer as inert fields kept only for a task file that
    // still carried one from before either was deleted. Both are long gone,
    // and so now are they: a task file still setting any of the five parses
    // (`RETIRED_PARK_KEYS` in [`Task::parse`] strips them from `extra`
    // rather than let them round-trip forever) and drops them on its next
    // save. The board's only clock-bearing state was `Parked`, which went
    // with them; a lane that stops reporting now waits on a person, on
    // `paused`, like every other stop.
    /// The gated step this task is paused on, waiting for a person to release
    /// it — see [`crate::pipeline::PAUSED`].
    ///
    /// The step rather than the destination, because the destination is the
    /// pipeline's to say and the pipeline may have been edited since. Released,
    /// the task goes wherever that step's `on_pass` points *now*; rejected, its
    /// `on_fail`. A command step's held failure is the exception: its release
    /// takes the `on_fail` its exit code chose. Cleared by the release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,

    /// Which road caught this pause — `"gate"` for a step's own
    /// `gate: true`, `"schedule"` for a task's own `gate_at`. `gate_at`
    /// clears itself the moment it fires (see `commands::report` and the
    /// dispatcher's command arm), so this
    /// is the one thing left to say which of the two ever held the task;
    /// `paused_at` alone cannot, since both roads set it the same way.
    /// Absent for a pause raised from `blocked` itself, which is not a catch
    /// of either gate. Cleared the moment `paused_at` is, on the same
    /// arrival, so it never outlives the pause it describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_by: Option<String>,

    /// `"queued"` or `"done"` — which reserved stage's issue-tracking hook
    /// paused this task, set the moment [`crate::dispatch::Dispatcher::
    /// tracking_gate`] finds one exited non-zero, or killed
    /// [`crate::tracking::MAX_HOOK_KILLS`] times in a row without leaving an
    /// exit code at all. Unlike [`paused_by`] this
    /// is not one of a gate's two roads — a hook pause answers no question a
    /// person releases past, it stops the task until the hook itself is
    /// made to run again — so it needs its own field rather than a third
    /// value squeezed into that one.
    ///
    /// `spoolway resume` reads this twice. For `"done"`,
    /// `commands::report::resume_road` decides the task goes straight back to
    /// `done` rather than through [`crate::commands::resume_target`]'s
    /// ordinary step-shaped roads, neither of which knows a name that is not
    /// a pipeline step at all. Then `commands::report::back_onto_its_step`
    /// moves it, and takes this field as it forgets the failed run (see
    /// [`crate::tracking::forget`]), so the next pass's `fire` starts it over
    /// and a later, ordinary pause never inherits it. `resume_road` and
    /// `spoolway queue route` only read it, and leave it set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hook_paused: Option<String>,

    /// The start branch that was missing when the dispatcher paused this task
    /// instead of cutting its worktree — the pause's own reason, which
    /// `queue show` prints with the rest of the front matter.
    ///
    /// Written together with the move to `paused` by
    /// `Dispatcher::pause_for_missing_start_branch`, and cleared by
    /// `spoolway resume`, which puts the task back on `queued` for the next
    /// pass to look again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missing_start_branch: Option<String>,

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

    /// How many launches this task has started on each route, keyed
    /// `from->to` — an agent lane's own conversation, and, since this
    /// change, a command step's `run:` process too (see
    /// [`Task::bank_launch`]'s own doc for the second writer that adds).
    /// Neither is a "prompt" in the model sense any more, which is why this
    /// is named `steps:` on disk rather than `prompts:` — see that field
    /// name's own note just below.
    ///
    /// What a person reads: "6 at that step" is six launches there, however
    /// they got there and whichever kind of step it is. Retries count,
    /// because a retried lane or a rerun command is a second launch and was
    /// paid for like one; [`Frontmatter::attempts`] goes on counting agent
    /// retries separately.
    ///
    /// Per route rather than per step because a step two loops come back to is
    /// two loops. Banked at launch, in [`Task::bank_launch`], unlike
    /// [`Frontmatter::rounds`] beside it — that one is banked once per
    /// arrival, in [`Task::set_stage`], because a lap is a transition and a
    /// relaunch after a lane died before saying anything is a second launch
    /// on the same lap rather than a second lap.
    ///
    /// Named `steps:` on disk, not `prompts:` — a command step banks a
    /// launch here too (see `dispatch::run_command`'s `Fresh` arm), and
    /// nothing about running a `run:` command is a prompt. `alias =
    /// "prompts"` reads a task file an older spoolway already wrote, or one
    /// mid-flight when this shipped, unchanged; every save from here on
    /// writes `steps:` instead, and nothing ever writes `prompts:` again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty", alias = "prompts")]
    pub steps: BTreeMap<String, u32>,

    /// How many times a task has taken each route, keyed the same way — one
    /// lap of the loop per move, whatever a lane at either end went on to do.
    /// Kept as history now that a step's `loop:` reads [`Frontmatter::
    /// arrivals`] instead: nothing routes on a single route's own count any
    /// more. [`Task::reset_loop_counts`] wipes it, with `arrivals`, whenever a
    /// task leaves `blocked`. Not dead weight, though — [`Task::parse`]'s own backfill still
    /// sums this map, in production, to give a task file written before
    /// `arrivals:` existed its own arrival count the first time it is read
    /// under this shipped version; [`Task::rounds_via`] is the one *test*
    /// reader left.
    ///
    /// Banked by [`Task::set_stage`], not [`Task::bank_launch`]: a lap is a
    /// transition, and a retried launch at a step the task never left is a
    /// second prompt on the same lap rather than a second one. Whether a lane
    /// opened a fresh conversation or resumed one used to matter here and no
    /// longer does — see [`Frontmatter::steps`] for what still counts that.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rounds: BTreeMap<String, u32>,

    /// How many times this task has arrived at each step, keyed by the step
    /// itself rather than by route — what the board's STEP column draws as
    /// `↻<n>` once it reaches [`crate::status::view::ARRIVAL_FLOOR`].
    ///
    /// Its own map rather than a sum over [`Self::rounds`], which
    /// [`Task::rounds_at`] used to compute: a step's `loop:` now reads this
    /// map directly, as the whole of its own budget, and `rounds` is kept
    /// only as per-route history — see its own doc. Keeping the two apart
    /// survives a change to either: even if some later road removed a
    /// `rounds` entry on its own, this map would still answer for a step
    /// actually visited. Banked separately, in [`Task::set_stage`], and only
    /// [`Task::reset_loop_counts`] — a task leaving `blocked` — ever removes
    /// or lowers an entry in it.
    ///
    /// Empty on a task file written before this field existed, which
    /// [`Task::parse`] backfills from `rounds` the moment such a file is
    /// read — see its own doc for why an empty map here is safe to treat as
    /// "not yet backfilled" rather than "genuinely never arrived".
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub arrivals: BTreeMap<String, u32>,

    /// How many consecutive launches at each step could not even start —
    /// never a lane or a command process that ran and failed, only one that
    /// [`crate::mux::Mux::start_lane`], [`crate::command_step::Runs::start`]
    /// or `start_command_in_pane` refused outright. Keyed by step rather than
    /// by route, unlike [`Self::rounds`] and [`Self::steps`] beside it: a
    /// launch that never started never had a route to be counted against.
    ///
    /// Bumped by `Dispatcher::note_launch_failure` in `src/dispatch.rs`,
    /// shared by both roads a launch takes: an agent lane's start in
    /// `start_lanes`, and a command step's `Fresh` arm in `run_command`.
    ///
    /// Cleared two ways, for two different moments a stale count would
    /// otherwise survive. [`Task::clear_launch_failures`] forgives it the
    /// instant a launch of the same step actually starts — a launch that
    /// gets as far as running is not the failure this counts, whatever it
    /// goes on to do, and this is the only writer for a step the task never
    /// actually leaves (a command step whose process just spawned, still
    /// sitting on that very step waiting for its exit code). [`Task::
    /// set_stage`] and [`Task::set_stage_unbanked`] forgive it the other
    /// way, on arrival: a step's own count means nothing once the task has
    /// moved off it and come back, so a *later* visit starts counting from
    /// zero rather than inheriting whatever an earlier one left behind —
    /// without this, a step whose `on_fail` loops back to the step behind
    /// it (`review`'s own `on_fail: implement` in the shipped pipeline)
    /// would ceiling on its first failure the second time around.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub launch_failures: BTreeMap<String, u32>,

    /// When a step's launch was first refused with herdr's `agent_pane_busy`
    /// — the pane it was asked to start in has not yet reached its shell
    /// prompt — in epoch seconds. Keyed by step, like [`Self::launch_failures`]
    /// beside it, whose lifecycle this copies: a busy-pane refusal is
    /// transient by construction and must not spend one of that field's
    /// strikes, but three passes in a row of nothing else still has to
    /// resolve somewhere, ten minutes after the *first* one — not the most
    /// recent — which is why only the first refusal writes anything here.
    ///
    /// Cleared the same two ways `launch_failures` is: [`Task::set_stage`]
    /// and [`Task::set_stage_unbanked`] on arrival, and
    /// [`Task::clear_launch_busy`] the moment a launch of the step actually
    /// starts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub launch_busy_since: BTreeMap<String, i64>,

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

/// The step a task was moved onto and has not yet been saved at, shared by
/// reference so [`Task::save`], which takes `&self`, can use it up.
///
/// A mutex rather than a `Cell` because a `Task` sits in a `static` in
/// `archive_index`, which needs it to be `Sync`.
#[derive(Debug, Default)]
pub(crate) struct Arrival(std::sync::Mutex<Option<String>>);

impl Arrival {
    fn set(&self, step: &str) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(step.to_string());
    }

    fn take(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl Clone for Arrival {
    fn clone(&self) -> Self {
        Arrival(std::sync::Mutex::new(
            self.0.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        ))
    }
}

/// A task file as loaded from disk: typed frontmatter plus the untouched body.
#[derive(Debug, Clone)]
pub struct Task {
    pub path: PathBuf,
    pub front: Frontmatter,
    pub body: String,
    /// The step [`Task::set_stage`] moved this task onto since the last
    /// [`Task::save`], which is where an arrival at a command step starts
    /// that step's run from nothing. Never loaded from disk, so a pass that
    /// only reads the queue cannot set it.
    pub(crate) arrived_at: Arrival,
}

impl Task {
    pub fn id(&self) -> &str {
        &self.front.id
    }

    pub fn stage(&self) -> &str {
        &self.front.stage
    }

    /// How many launches this task has started at `step`, by any route —
    /// an agent lane's own conversation or a command step's `run:` process,
    /// whichever this step is.
    ///
    /// What a person reads and what the ledger records — "3 launches of
    /// review" is about the step, however it got there. The limit a `loop:`
    /// sets is the step's own business now; see [`Task::rounds_at`].
    pub fn steps_at(&self, step: &str) -> u32 {
        let suffix = format!("->{step}");
        self.front
            .steps
            .iter()
            .filter(|(key, _)| key.as_str() == step || key.ends_with(&suffix))
            .map(|(_, n)| *n)
            .sum()
    }

    /// How many times this task has moved from `from` to `to` — laps of that
    /// one route through the loop, kept as history now that a `loop:` limit
    /// reads [`Task::rounds_at`] instead. Test-only: nothing routes on a
    /// single route's own count any more, so no production caller is left.
    #[cfg(test)]
    pub fn rounds_via(&self, from: &str, to: &str) -> u32 {
        self.front
            .rounds
            .get(&route_key(from, to))
            .copied()
            .unwrap_or(0)
    }

    /// How many times this task has arrived at `step`, whichever route
    /// carried it there each time — [`Frontmatter::arrivals`]'s own entry for
    /// it. Read by two very different callers for the same number: the
    /// board's STEP column, to say how many times a task has stood there, and
    /// `apply_loop_budget` in `src/commands/report.rs`, as the step's own
    /// `loop:` budget — a fact about the step itself, unlike [`Self::
    /// rounds_via`]'s test-only answer about one route into it.
    pub fn rounds_at(&self, step: &str) -> u32 {
        self.front.arrivals.get(step).copied().unwrap_or(0)
    }

    /// Start every step's `loop:` count again from zero.
    ///
    /// Both maps are emptied, not just [`Frontmatter::arrivals`]: [`Task::parse`]
    /// backfills an empty `arrivals` from `rounds`, so a reset that left
    /// `rounds` behind would bring the old counts back the next time the file
    /// is read. `steps` (launch counts) and `launch_failures` are a different
    /// ledger and stay.
    pub fn reset_loop_counts(&mut self) {
        self.front.rounds.clear();
        self.front.arrivals.clear();
    }

    /// Bank one launch at `to`, arriving from `from` — an agent lane's own
    /// conversation, or a command step's `run:` process starting.
    ///
    /// The dispatcher calls this at launch, from exactly two places: an
    /// agent lane's own `finish_launch_bookkeeping`, and a command step's `Fresh` arm in
    /// `run_command`. Nothing else may — a counter banked from a third
    /// place is one that can double-count the moment it disagrees with
    /// these two about what a launch is — and the two that do call it
    /// cannot disagree with each other: an agent step and a command step
    /// are never the same step, so at most one of them ever calls this for
    /// a given `to` on a given arrival.
    ///
    /// Whether the lane opened a conversation of its own or resumed one used
    /// to matter here and no longer does — see [`Frontmatter::rounds`], which
    /// counts arrivals rather than conversations and is banked in
    /// [`Task::set_stage`] instead, once per lap rather than once per launch.
    pub fn bank_launch(&mut self, from: &str, to: &str) {
        let key = route_key(from, to);
        *self.front.steps.entry(key).or_insert(0) += 1;
    }

    /// Count one more launch of `step` that could not even start, and return
    /// the new count — see [`Frontmatter::launch_failures`].
    pub fn bump_launch_failures(&mut self, step: &str) -> u32 {
        let count = self
            .front
            .launch_failures
            .entry(step.to_string())
            .or_insert(0);
        *count += 1;
        *count
    }

    /// Forgive `step`'s launch-failure count: a launch of it just started.
    /// Answers whether there was anything to forgive, so a caller that reads
    /// this on every successful launch writes only the file that changed.
    pub fn clear_launch_failures(&mut self, step: &str) -> bool {
        self.front.launch_failures.remove(step).is_some()
    }

    /// Stamp `step`'s first `agent_pane_busy` refusal, or answer the stamp
    /// already there — see [`Frontmatter::launch_busy_since`]. Only the
    /// first refusal writes anything: the wait is measured from when the
    /// pane first went busy, not reset on every pass that still finds it so.
    pub fn stamp_launch_busy(&mut self, step: &str, now: i64) -> i64 {
        *self
            .front
            .launch_busy_since
            .entry(step.to_string())
            .or_insert(now)
    }

    /// Forgive `step`'s pane-busy stamp: a launch of it just started, or the
    /// wait ran out and the step is moving on. Answers whether there was
    /// anything to forgive, matching [`Task::clear_launch_failures`].
    pub fn clear_launch_busy(&mut self, step: &str) -> bool {
        self.front.launch_busy_since.remove(step).is_some()
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

        let mut front: Frontmatter =
            serde_norway::from_str(yaml).context("frontmatter is not valid task YAML")?;

        // The quota-and-usage-limit park's five fields have no struct home
        // any more, so a file that still carries one lands here — in
        // `extra`, the catch-all every other unrecognised key survives a
        // rewrite through. These do not get that mercy: they are retired,
        // not merely unread, so a save must drop them rather than round-trip
        // them forever.
        for key in RETIRED_PARK_KEYS {
            front.extra.remove(*key);
        }

        // A task file from before `arrivals:` existed carries `rounds:` but
        // no `arrivals:` of its own — every route this task ever took is
        // still there, only not yet re-keyed by destination. Backfilled by
        // the sum [`Task::rounds_at`] itself used to compute, the one time
        // this can still be told apart from a task that has genuinely never
        // arrived anywhere: after this shipped, [`Task::set_stage`] banks
        // both maps on every arrival, so a task with laps on file but no
        // `arrivals:` can only be one still carrying the old shape. Without
        // this, every task already in a queue would read as never having
        // arrived anywhere the moment this shipped — exactly the count this
        // field exists to keep from dropping. Same precedent as
        // `an_old_prompts_key_is_read_unchanged_and_saved_as_steps`, for the
        // shape that field's own alias could not cover: an old file's laps
        // are keyed by route already, this only sums them per destination.
        if front.arrivals.is_empty() {
            for (route, n) in &front.rounds {
                if let Some((_, to)) = route.split_once("->") {
                    *front.arrivals.entry(to.to_string()).or_insert(0) += n;
                }
            }
        }

        // `queue add` checks this too, and is not the only way a file gets here:
        // a person edits one, a plan writes several. The id names this task's
        // worktree and every file a lane of it writes under the project's
        // home directory, so it is checked on the way in rather than trusted
        // because of where it came from. See [`crate::config::check_task_id`].
        crate::config::check_task_id(&front.id)?;

        // `branch` is spoolway's alone — see `queue::RESERVED_KEYS`. `queue add`
        // stamps `task/<id>`, or `task/<slug>-<id>` when
        // `issue_tracking.key_in_names` prefixed it, and nothing else ever
        // should. A hand-edited file dropped in `queue/` never passes `queue
        // add`. `spoolway stack` force-pushes a squashed commit onto whatever
        // this says and
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

        Ok(Task {
            path,
            front,
            body,
            arrived_at: Default::default(),
        })
    }

    /// Serialise back to the on-disk form.
    pub fn render(&self) -> Result<String> {
        let yaml = serde_norway::to_string(&self.front).context("serialising frontmatter")?;
        Ok(format!("---\n{yaml}---\n{}", self.body))
    }

    /// Write the task file. If [`Task::set_stage`] moved it onto a step since
    /// the last save, that step's old command run is cleared first.
    ///
    /// The clearing lives in the write, not in the dispatcher, because a task
    /// reaches a command step by many roads: a dispatcher pass, a lane's
    /// `spoolway report`, a resume from the board. All of them save through
    /// here, so none can leave the old exit code for the next visit to route
    /// on. A task only *loaded* never clears anything, which is why a
    /// restarted dispatcher still adopts the run it left behind.
    ///
    /// The run goes before the file is written. A failed write then leaves a
    /// task that is still where it was with a step's run missing, which costs
    /// one re-run. The other order could leave a task on the step with the old
    /// code still beside it.
    pub fn save(&self) -> Result<()> {
        if let Some(step) = self.arrived_at.take() {
            self.clear_old_run(&step)?;
        }
        let rendered = self.render()?;
        write_atomic(&self.path, &rendered)
            .with_context(|| format!("writing task file {}", self.path.display()))
    }

    /// Stop and forget whatever run `step` left for this task, if the task
    /// file sits in a project's queue and so has a `commands/` beside it.
    fn clear_old_run(&self, step: &str) -> Result<()> {
        let Some(home) = self
            .path
            .parent()
            .filter(|queue| {
                queue
                    .file_name()
                    .is_some_and(|n| n == crate::config::QUEUE_DIR)
            })
            .and_then(Path::parent)
        else {
            return Ok(());
        };
        let dir = home.join(crate::command_step::RUN_DIR);
        if !dir.is_dir() {
            return Ok(());
        }
        let key = crate::command_step::Runs::key(step, self.id());
        crate::command_step::Runs::new(&dir)
            .begin_visit(&key)
            .with_context(|| format!("clearing the old run of `{step}` for {}", self.id()))
    }

    /// Move to a new step, bank one round on the route this arrival took, and
    /// append a `## Status Log` line.
    ///
    /// This is the only supported way to change a stage — prompts reach it
    /// through `spoolway report`, the dispatcher calls it directly.
    ///
    /// A lap is a transition, so this is where it is banked — once per
    /// arrival, whatever a lane at the destination goes on to do there.
    /// [`Task::bank_launch`] banks `steps` separately, once per launch: a
    /// relaunch after a lane died before saying anything calls that again
    /// without calling this again, and costs a prompt rather than a round.
    pub fn set_stage(&mut self, stage: &str, message: Option<&str>) {
        let from = std::mem::replace(&mut self.front.stage, stage.to_string());
        *self
            .front
            .rounds
            .entry(route_key(&from, stage))
            .or_insert(0) += 1;
        *self.front.arrivals.entry(stage.to_string()).or_insert(0) += 1;
        // `Dispatcher::tear_down_and_escalate` marks a task it sent to
        // `blocked` as `escalated`. The mark describes that one stop, so it
        // ends when the task leaves `blocked`; left set, the next block on
        // the task would read on the board as another escalation.
        if from == crate::pipeline::BLOCKED {
            self.front.escalated = false;
        }
        self.front.arrived_from = Some(from);
        self.front.attempts = 0;
        self.front.launched_at = None;
        // A fresh arrival at `stage` has nothing to do with whatever a much
        // earlier visit there once counted against `Self::launch_failures` —
        // see [`Frontmatter::launch_failures`]'s own doc for the loop this
        // closes: a step whose `on_fail` routes back to the very step behind
        // it (`review`'s own `on_fail: implement` in the shipped pipeline)
        // would otherwise ceiling on its *first* failure the second time
        // around, having inherited a count nothing had reset.
        self.front.launch_failures.remove(stage);
        self.front.launch_busy_since.remove(stage);
        // A new visit to `stage`: the next `save` clears its old run. See
        // [`Task::save`].
        self.arrived_at.set(stage);

        let line = match message {
            Some(m) if !m.trim().is_empty() => {
                format!("→ `{}`: {}", stage, m.trim().replace('\n', " "))
            }
            _ => format!("→ `{stage}`"),
        };
        self.log_status(&line);
    }

    /// Move to `stage` and log why, without banking a lap.
    ///
    /// For exactly one round trip: a person parking a task from the board
    /// with `p`, and putting it back afterwards. Neither direction is a lap
    /// of anything — the task never left the step it was on, so counting one
    /// here would leave a `loop:` budget seeing an arrival no pipeline
    /// routed. See [`Task::set_stage`], which this deliberately does not
    /// call: `rounds` and `arrived_from` are left exactly as they were, and
    /// `attempts`, `launched_at` and `stage`'s own entry in
    /// `launch_failures` are reset the same way `set_stage` resets them,
    /// since neither a parked task nor the lane it is handed back to has
    /// anything of those left to mean.
    pub fn set_stage_unbanked(&mut self, stage: &str, message: &str) {
        self.front.stage = stage.to_string();
        self.front.attempts = 0;
        self.front.launched_at = None;
        self.front.launch_failures.remove(stage);
        self.front.launch_busy_since.remove(stage);

        let line = format!("→ `{}`: {}", stage, message.trim().replace('\n', " "));
        self.log_status(&line);
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
    /// Forgives `attempts` — a launch landing means whatever it was counting
    /// is over — but not `launched_at`. That is the board's clock for a live
    /// lane and outlives the forgiveness — `set_stage` is what clears it,
    /// because arriving somewhere is what restarts it. Folding it in here, as
    /// this once did, forgave the clock about two seconds after every lane
    /// started, and the board read `None` — a dash — for the rest of the
    /// step.
    ///
    /// Answers whether anything changed, so a caller reading every task on
    /// every pass writes only the file that moved.
    pub fn launch_landed(&mut self) -> bool {
        let counted = self.front.attempts > 0;
        self.front.attempts = 0;
        counted
    }

    /// Append `text` to `## Status Log`, stamped with the local wall clock to
    /// the minute — the one door every status-log writer goes through, so no
    /// caller can leave a line unstamped by forgetting to.
    ///
    /// Nothing in spoolway reads a status-log line back (`Task::section` is
    /// `#[cfg(test)]`), so the stamp is a display choice: local time reads
    /// the way a person on the board would say it, the way
    /// `problem_log.rs` and `eval.rs` already do, rather than the `Utc`
    /// used for machine-read ledger entries in `usage.rs`.
    pub fn log_status(&mut self, text: &str) {
        let stamp = Local::now().format("%Y-%m-%d %H:%M");
        self.append_to_section("## Status Log", &format!("- {stamp} {text}\n"));
    }

    /// Mark everything now under `## Blocker` as a stop that has been cleared.
    ///
    /// The dispatcher appends to `## Blocker`, so the entries a stopped lane
    /// left stay in the file after the task is put back, and a later lane
    /// reading them cannot tell they are over. This adds a dated line after
    /// them instead of touching what the earlier writer put there. A person
    /// or lane can still rewrite the whole section with `spoolway task edit`,
    /// which drops the line along with the entries.
    ///
    /// A stop that comes after the line appends below it, so the newest entry
    /// is the one with no mark under it. A section that is absent or empty,
    /// or already ends in this line, is left alone: putting a task back twice
    /// does not stack marks.
    pub fn mark_blocker_cleared(&mut self) {
        let Some((start, end)) = self.find_section("## Blocker") else {
            return;
        };
        let last = self.body[start..end]
            .trim_end()
            .lines()
            .last()
            .unwrap_or("");
        if last.is_empty() || last.starts_with(BLOCKER_CLEARED) {
            return;
        }
        let stamp = Local::now().format("%Y-%m-%d %H:%M");
        self.append_to_section(
            "## Blocker",
            &format!(
                "{BLOCKER_CLEARED} {stamp}: the task was put back; the entries above are \
                 past.\n"
            ),
        );
    }

    /// Append `text` under `heading`, creating the section if it is absent.
    ///
    /// Sections are appended to rather than replaced so that no writer can
    /// clobber another's section — the dispatcher owns `## Blocker`, every
    /// step's own findings and notes go to `## Handoff`, everyone appends to
    /// `## Status Log`. [`Task::mark_blocker_cleared`] also appends a line to
    /// `## Blocker`.
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

    /// Replace a section's whole content with `text` — the road `spoolway
    /// task edit` uses to hand a stopped task to whoever is
    /// reading its pane.
    ///
    /// Unlike [`Task::append_to_section`], which creates a heading it does
    /// not find, this refuses one the body does not already have: an edit
    /// names a section the task's own template put there, not an author
    /// free to invent a heading that will read as ordinary spoolway output.
    ///
    /// `text` is trimmed and re-wrapped in the one blank line every other
    /// section already carries above its own content and below it, rather
    /// than spliced in as given: a `--from` file with no trailing newline —
    /// the ordinary shape a person's own editor leaves — would otherwise glue
    /// the next heading onto its last line, and `## Non-goals` stops being a
    /// heading `find_section` can see at all. The trailing blank line is
    /// dropped for the body's own last section, matching the single newline
    /// [`Task::render`] already ends every task with.
    pub fn replace_section(&mut self, heading: &str, text: &str) -> Result<()> {
        let (start, end) = self
            .find_section(heading)
            .with_context(|| format!("no `{heading}` section in this task's body"))?;
        let trimmed = text.trim();
        let replacement = match end == self.body.len() {
            true => format!("\n{trimmed}\n"),
            false => format!("\n{trimmed}\n\n"),
        };
        self.body.replace_range(start..end, &replacement);
        Ok(())
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
    /// `slug:` and `url:` — are the main callers, alongside [`Task::
    /// tracking_off`] reading `tracking:` the same way. None carries a typed
    /// field on [`Frontmatter`] — see [`Task::set_extra_str`]'s own doc
    /// comment for why — so all five are read back through the same `extra`
    /// catch-all any other unknown key already round-trips through.
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
    /// enough for all four — and for the fifth caller,
    /// `commands::queue::open_and_prefix`, which sets `tracking:` to `off`
    /// on every task in a batch that declined issue creation.
    pub fn set_extra_str(&mut self, key: &str, value: &str) {
        self.front.extra.insert(
            key.to_string(),
            serde_norway::Value::String(value.to_string()),
        );
    }

    /// Whether this task was queued — or hand-written — with `tracking:
    /// off`. `commands::queue::open_and_prefix` stamps this on every task
    /// in a batch whose issue creation was declined, at the queue screen's
    /// `n` or the tool-requirements gate's `enter`; a person may also write
    /// it by hand, since `tracking` is one of `OPTIONAL_KEYS`.
    /// `dispatch::route_reserved_stage` and `tracking_gate` both read this the
    /// same way they already read [`Frontmatter::trial`], so a task with
    /// tracking off starts no hook run and is never held waiting on one.
    pub fn tracking_off(&self) -> bool {
        self.extra_str("tracking") == "off"
    }
}

/// Split `---\n<yaml>\n---\n<body>` into its two halves, without deciding
/// anything about what the yaml half means.
///
/// Shared by [`Task::parse`], which trusts every key the yaml half sets, and
/// by `queue::parse_submission`, which trusts none of them until it has
/// checked which ones a task may set at all — both need the same two
/// fences found the same way, and a submitted task is this same shape
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
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

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
        // Every error here is named by `path`, never `tmp` — `tmp` is a
        // hidden, pid-and-counter-named file nobody asked for and nothing
        // reads back, and naming it would send a person to a file that is
        // never there.
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("writing {}", path.display()))?;
        std::io::Write::write_all(&mut file, contents.as_ref())
            .with_context(|| format!("writing {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("writing {}", path.display()))?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
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

/// Remove the `.<name>.<pid>-<n>.tmp` files in `dir` whose writer is gone.
///
/// [`write_atomic`] writes through one of these and renames it into place. A
/// writer killed between the two (`kill -9`, power loss) leaves the temp file
/// behind, and nothing else ever names it, so without this it stays for good.
/// One whose pid is still alive may be a write in flight and is kept; a pid
/// recycled by an unrelated process keeps its file too, which is a leak
/// rather than a loss. Best effort: a file that cannot be removed is tried
/// again on the next pass.
pub fn sweep_stale_tmp(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(writer_pid) else {
            continue;
        };
        if !crate::headless::alive(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The pid in a `.<name>.<pid>-<n>.tmp` file name, `None` for any other name.
fn writer_pid(file_name: &str) -> Option<u32> {
    let stem = file_name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let (_, tail) = stem.rsplit_once('.')?;
    let (pid, call) = tail.split_once('-')?;
    call.parse::<u64>().ok()?;
    pid.parse().ok()
}

/// One `*.md` file in a task directory that would not load, with the reason
/// it did not — a broken frontmatter fence, an `id:` that fails `check_task_id`,
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

    // `touches:` is no longer a typed field, but a task that still sets
    // it must keep loading and round-tripping — the key survives as
    // passthrough in `extra`, the same as any other key spoolway does not
    // name.
    const SAMPLE: &str = "---\nid: demo\nstage: queued\ntouches: [src/**]\n---\n## Goal\nDo a thing.\n\n## Status Log\n- earlier entry\n";

    /// `write_atomic` names the path it was trying to create a directory
    /// for when that fails — the "`local` is a file" case a copy or promote
    /// hits when something that should be a directory is a plain file
    /// instead. Before this fix the bare `std::io::Error` carried no path at
    /// all.
    #[test]
    fn write_atomic_names_the_path_when_the_parent_cannot_be_created() {
        let root = crate::scratch::root("write-atomic-parent-is-a-file");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let blocker = root.join("local");
        std::fs::write(&blocker, "not a directory").unwrap();

        let target = blocker.join("pipelines").join("foo.yml");
        let err = write_atomic(&target, "steps: []\n").unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(&blocker.join("pipelines").display().to_string()),
            "{message}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// `write_atomic` names the destination `path`, never the hidden
    /// `.<name>.<pid>-<n>.tmp` it actually wrote to, when the write itself
    /// fails — a name too long for the filesystem is the OS error
    /// acceptance criterion 4 names for this: the temp file's own longer
    /// name (prefixed with `.` and suffixed with a pid and a counter) fails
    /// `File::create` at the same limit a plain `path` this long would.
    #[test]
    fn write_atomic_names_the_destination_not_the_hidden_temp_file_when_the_name_is_too_long() {
        let root = crate::scratch::root("write-atomic-name-too-long");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // 255 bytes is the usual per-component limit on Linux/macOS; `tmp`'s
        // own name adds a `.` prefix and a `.<pid>-<n>.tmp` suffix on top of
        // this, so 250 `x`s is already too long for the temp file even
        // though a plain file of that name would just barely fit.
        let long_name = format!("{}.yml", "x".repeat(250));
        let target = root.join(&long_name);

        let err = write_atomic(&target, "steps: []\n").unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(&target.display().to_string()),
            "the error must name the destination path, not the hidden temp file: {message}"
        );
        assert!(
            !message.contains(".tmp"),
            "the error must not name the hidden temp file: {message}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A temp file whose writer is gone is swept; one whose writer is alive,
    /// and anything not shaped like a temp file, stays.
    #[test]
    fn sweep_stale_tmp_removes_only_files_whose_writer_is_gone() {
        let root = crate::scratch::root("sweep-stale-tmp");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // A pid above `pid_max` on every platform this runs on, so nothing
        // is ever alive at it.
        let gone = root.join(".a.md.4294967290-0.tmp");
        let alive = root.join(format!(".b.md.{}-3.tmp", std::process::id()));
        let task = root.join("a.md");
        let other = root.join(".hidden.tmp");
        for path in [&gone, &alive, &task, &other] {
            std::fs::write(path, "x").unwrap();
        }

        sweep_stale_tmp(&root);

        assert!(!gone.exists(), "a dead writer's temp file must go");
        assert!(alive.exists(), "a live writer's temp file must stay");
        assert!(task.exists(), "a task file must stay");
        assert!(
            other.exists(),
            "a file not named like a temp write must stay"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parses_and_round_trips() {
        let task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert_eq!(task.id(), "demo");
        assert_eq!(task.stage(), "queued");
        assert_eq!(
            task.front.extra.get("touches"),
            Some(&serde_norway::Value::Sequence(vec![
                serde_norway::Value::String("src/**".to_string())
            ]))
        );
        assert!(task.body.starts_with("## Goal"));

        let reparsed = Task::parse(PathBuf::from("demo.md"), &task.render().unwrap()).unwrap();
        assert_eq!(reparsed.stage(), "queued");
        assert_eq!(reparsed.body, task.body);
    }

    /// A task file already on disk carrying the old `prompts:` key — written
    /// before this field was renamed — is read unchanged, and every save
    /// from here on writes `steps:` instead: nothing ever writes `prompts:`
    /// again.
    #[test]
    fn an_old_prompts_key_is_read_unchanged_and_saved_as_steps() {
        let raw = "---\nid: demo\nstage: queued\nprompts:\n  queued->implement: 2\n---\n## Goal\nDo a thing.\n";
        let task = Task::parse(PathBuf::from("demo.md"), raw).unwrap();
        assert_eq!(task.steps_at("implement"), 2);

        let rendered = task.render().unwrap();
        assert!(rendered.contains("steps:"), "{rendered}");
        assert!(!rendered.contains("prompts:"), "{rendered}");
    }

    /// A task file already on disk carrying `rounds:` but no `arrivals:` of
    /// its own — written before this field existed — reads its arrival
    /// count backfilled from the same sum [`Task::rounds_at`] used to
    /// compute before `arrivals:` had a map of its own: every task already
    /// in a queue the moment this ships keeps its counter rather than
    /// reading as never having arrived anywhere.
    #[test]
    fn a_task_with_rounds_but_no_arrivals_key_backfills_its_count() {
        let raw = "---\nid: demo\nstage: implement\nrounds:\n  \
                   queued->implement: 1\n  review->implement: 1\n  implement->review: 2\n\
                   ---\n## Goal\nDo a thing.\n";
        let task = Task::parse(PathBuf::from("demo.md"), raw).unwrap();
        assert_eq!(task.rounds_at("implement"), 2);
        assert_eq!(task.rounds_at("review"), 2);

        // The backfill writes `arrivals:` back out, so a re-save never
        // repeats it.
        let rendered = task.render().unwrap();
        assert!(rendered.contains("arrivals:"), "{rendered}");
    }

    /// A task that already carries `arrivals:` of its own is read exactly as
    /// written — the backfill above only ever fills a genuinely empty map,
    /// never adds to one a real `set_stage` already banked.
    #[test]
    fn a_task_with_its_own_arrivals_key_is_never_backfilled_over() {
        let raw = "---\nid: demo\nstage: implement\nrounds:\n  queued->implement: 5\n\
                   arrivals:\n  implement: 1\n---\n## Goal\nDo a thing.\n";
        let task = Task::parse(PathBuf::from("demo.md"), raw).unwrap();
        assert_eq!(task.rounds_at("implement"), 1);
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

    /// Every line the format documents — a transition through `set_stage`
    /// and a bare note through `log_status` directly — opens with a local
    /// `YYYY-MM-DD HH:MM ` stamp, not the RFC 3339 timestamp this replaced.
    #[test]
    fn log_status_stamps_transitions_and_notes() {
        fn starts_with_minute_stamp(line: &str) -> bool {
            let Some(rest) = line.strip_prefix("- ") else {
                return false;
            };
            let bytes = rest.as_bytes();
            bytes.len() >= 16
                && bytes[0..4].iter().all(u8::is_ascii_digit)
                && bytes[4] == b'-'
                && bytes[5..7].iter().all(u8::is_ascii_digit)
                && bytes[7] == b'-'
                && bytes[8..10].iter().all(u8::is_ascii_digit)
                && bytes[10] == b' '
                && bytes[11..13].iter().all(u8::is_ascii_digit)
                && bytes[13] == b':'
                && bytes[14..16].iter().all(u8::is_ascii_digit)
                && bytes[16] == b' '
        }

        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.set_stage("review", Some("done"));
        task.log_status("a bare note with no step attached");

        let log = task.section("## Status Log").unwrap();
        let lines: Vec<&str> = log
            .lines()
            .filter(|l| l.contains("done") || l.contains("bare note"))
            .collect();
        assert_eq!(lines.len(), 2, "{log:?}");
        for line in lines {
            assert!(starts_with_minute_stamp(line), "{line:?}");
        }
        assert!(
            !log.contains("+00:00"),
            "no RFC 3339 offset should remain: {log:?}"
        );
    }

    /// A park-and-resume round trip through `set_stage_unbanked` banks
    /// nothing: `steps`, `rounds`, `arrivals` and `arrived_from` come out
    /// byte-for-byte as they went in, which is exactly what a person
    /// interrupting a turn and putting it back must look like — the task
    /// never left the step.
    #[test]
    fn set_stage_unbanked_round_trip_banks_nothing() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.set_stage("implement", Some("moved on"));
        let prompts_before = task.front.steps.clone();
        let rounds_before = task.front.rounds.clone();
        let arrivals_before = task.front.arrivals.clone();
        let arrived_from_before = task.front.arrived_from.clone();

        task.set_stage_unbanked("paused", "paused from the board");
        task.set_stage_unbanked("implement", "put back from the board");

        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.steps, prompts_before);
        assert_eq!(task.front.rounds, rounds_before);
        assert_eq!(task.front.arrivals, arrivals_before);
        assert_eq!(task.front.arrived_from, arrived_from_before);
        let log = task.section("## Status Log").unwrap();
        assert!(log.contains("→ `paused`: paused from the board"));
        assert!(log.contains("→ `implement`: put back from the board"));
        assert!(!log.to_lowercase().contains("unblocked"));
    }

    /// A step whose `on_fail` loops back to the step behind it — `review`'s
    /// own `on_fail: implement` in the shipped pipeline — sends a ceilinged
    /// task away and, sooner or later, some other route sends it back. That
    /// second visit must not inherit the first one's spent count: arriving
    /// fresh clears it, the same way `set_stage` already clears `attempts`
    /// and the rest of a step's launch state on every arrival — cleared for
    /// the step just *reached*, not the one just left, since a route away
    /// from a ceilinged step leaves that step's count exactly where a later
    /// return visit needs to find it: spent, until arriving there again is
    /// itself what clears it.
    #[test]
    fn set_stage_clears_the_arriving_steps_launch_failures() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.front.launch_failures.insert("review".into(), 3);

        // Routed away from `review` after it ceilinged. Nothing arrived at
        // `review` this move, so its count rides through untouched — this is
        // the state a later return visit has to find it in.
        task.set_stage("implement", None);
        assert_eq!(task.front.launch_failures.get("review"), Some(&3));

        // Later, `review` is reached again — this arrival is what clears it,
        // so a first failure this time around is attempt one, not four.
        task.set_stage("review", None);
        assert_eq!(task.front.launch_failures.get("review"), None);
    }

    /// `set_stage_unbanked`'s own round trip — a person parking a task from
    /// the board and releasing it — forgives the same way, for the same
    /// reason: the lane handed back has nothing left to mean by a count from
    /// before the interruption.
    #[test]
    fn set_stage_unbanked_clears_the_arriving_steps_launch_failures() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.set_stage("implement", None);
        task.front.launch_failures.insert("implement".into(), 2);

        task.set_stage_unbanked("paused", "paused from the board");
        task.set_stage_unbanked("implement", "put back from the board");

        assert_eq!(task.front.launch_failures.get("implement"), None);
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

    /// The quota-and-usage-limit park's five fields have no struct home any
    /// more, so a task file still carrying one from before this shipped
    /// lands in `extra` on parse — and unlike an ordinary unrecognised key,
    /// which `extra` writes back untouched, these are dropped: the field
    /// they belonged to is retired, not merely a hand-added key nothing
    /// reads, so a rewrite must not preserve it.
    #[test]
    fn a_legacy_park_field_parses_and_is_dropped_on_the_next_save() {
        let raw = "---\nid: demo\nstage: review\nparked_until: 1788801180\n\
                    parked_window: five_hour\nparked_at: 1788793980\n\
                    quota_retries: 2\nusage_limit_hold: true\n---\nbody\n";
        let task = Task::parse(PathBuf::from("demo.md"), raw).unwrap();
        let rendered = task.render().unwrap();
        for key in [
            "parked_until",
            "parked_window",
            "parked_at",
            "quota_retries",
            "usage_limit_hold",
        ] {
            assert!(!rendered.contains(key), "{key} survived a save: {rendered}");
        }
    }

    /// A stage change is an arrival, and a lap is a transition: `set_stage`
    /// banks the round on its own, before any lane has even started. Launching
    /// a lane there is a separate cost, banked separately.
    #[test]
    fn a_transition_banks_a_round_on_its_own() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.set_stage("fix", None);
        task.set_stage("review", None);

        assert_eq!(task.steps_at("fix"), 0);
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

        assert_eq!(task.steps_at("fix"), 3);
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

    /// The mark is added once per stop: a second put-back with nothing new
    /// under `## Blocker` leaves the section as it was, and a section with no
    /// entries gets none.
    #[test]
    fn marking_a_blocker_cleared_twice_adds_one_line_and_an_empty_section_none() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.append_to_section("## Blocker", "");
        task.mark_blocker_cleared();
        assert_eq!(task.section("## Blocker").unwrap(), "");

        task.append_to_section("## Blocker", "- llama-server unreachable\n");
        task.mark_blocker_cleared();
        let once = task.section("## Blocker").unwrap().to_string();
        task.mark_blocker_cleared();

        assert_eq!(task.section("## Blocker").unwrap(), once);
        assert_eq!(once.matches("- Cleared").count(), 1, "{once}");
        assert!(once.starts_with("- llama-server unreachable\n"), "{once}");
    }

    /// `--from` content with no trailing newline — the ordinary shape a
    /// person's own editor leaves a file in — must not glue the next
    /// heading onto the replacement's last line. Review finding: an earlier
    /// version spliced `text` in verbatim and left `## Non-goals` unreadable
    /// as a heading at all.
    #[test]
    fn replace_section_normalises_a_replacement_with_no_trailing_newline() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.replace_section("## Goal", "line one\nline two")
            .unwrap();

        assert_eq!(task.section("## Goal").unwrap(), "line one\nline two");
        // The next heading must still read as a heading, with the blank
        // line every other section carries above its own content.
        assert!(
            task.body.contains("line two\n\n## Status Log\n"),
            "{}",
            task.body
        );
        assert_eq!(
            task.section("## Status Log").unwrap(),
            "- earlier entry",
            "a section this edit did not name is untouched"
        );
    }

    /// The body's own last section gets no trailing blank line — just the
    /// one newline [`Task::render`] already ends every task with — so a
    /// repeated edit never grows a longer and longer gap at the end of the
    /// file.
    #[test]
    fn replace_section_on_the_last_section_adds_no_trailing_blank_line() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        task.replace_section("## Status Log", "- rewritten\n\n\n")
            .unwrap();

        assert!(task.body.ends_with("- rewritten\n"), "{:?}", task.body);
        assert!(
            !task.body.ends_with("- rewritten\n\n"),
            "no trailing blank line on the last section: {:?}",
            task.body
        );
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

    /// `starts_from` is what `spoolway queue show` prints for what the worktree
    /// was actually cut from — a line of its own, distinct from `base`, which
    /// keeps meaning the branch the plan lands in.
    #[test]
    fn starts_from_round_trips_and_stays_out_when_unset_and_stands_apart_from_base() {
        let mut task = Task::parse(PathBuf::from("demo.md"), SAMPLE).unwrap();
        assert!(!task.render().unwrap().contains("starts_from:"));

        task.front.base = Some("main".into());
        task.front.starts_from = Some("task/dependency".into());
        let rendered = task.render().unwrap();
        assert!(rendered.contains("base: main"));
        assert!(rendered.contains("starts_from: task/dependency"));

        let reparsed = Task::parse(PathBuf::from("demo.md"), &rendered).unwrap();
        assert_eq!(reparsed.front.base.as_deref(), Some("main"));
        assert_eq!(
            reparsed.front.starts_from.as_deref(),
            Some("task/dependency")
        );
    }

    /// A task file written when the field was called `cut_from` still reads,
    /// and is written back under the new name.
    #[test]
    fn a_task_file_that_says_cut_from_reads_as_starts_from() {
        let old = "---\nid: demo\nstage: done\ncut_from: task/old\n---\n";
        let task = Task::parse(PathBuf::from("demo.md"), old).unwrap();
        assert_eq!(task.front.starts_from.as_deref(), Some("task/old"));
        let rendered = task.render().unwrap();
        assert!(rendered.contains("starts_from: task/old"), "{rendered}");
        assert!(!rendered.contains("cut_from"), "{rendered}");
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
            blocked: false,
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
            blocked: false,
        });
        assert!(
            !task.reported_since(1_000),
            "a report banked before the lane started is the round before's"
        );
        assert!(task.reported_since(999), "banked after the lane started");
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
    /// task — it round-trips like any other, and stays out of the file
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
