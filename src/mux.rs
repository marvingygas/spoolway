//! The terminal multiplexer spoolway drives.
//!
//! Every agent runs in a real pane, so any lane can be watched, attached to, or
//! taken over by hand. herdr is the only backend today; the operations below
//! are deliberately the small set a tmux backend could also implement.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::config::{DispatchConfig, MuxMode};
use crate::repo::run;

/// What a lane is doing right now, as the multiplexer sees it.
///
/// This answers "is a session alive and is it stuck" and nothing else. What has
/// actually *happened* to a task is always read from the task file's `stage:`,
/// which the lane itself writes via `spoolway report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaneStatus {
    /// Started but never prompted. Only seen before a lane's first turn.
    Idle,
    /// Mid-turn right now.
    Working,
    /// Waiting on a human.
    Blocked,
    /// Finished a turn and sitting there, alive and promptable.
    ///
    /// This — not `Idle` — is the resting state of every lane that has done any
    /// work at all. Treating it as "exited" strands every lane after its first
    /// turn, which is a mistake this pipeline has made before.
    Done,
    /// The multiplexer cannot tell.
    Unknown,
}

impl LaneStatus {
    /// Has the lane finished its turn and become promptable again?
    pub fn is_settled(self) -> bool {
        matches!(self, LaneStatus::Done | LaneStatus::Idle)
    }

    /// Is the lane still in flight and not to be disturbed?
    pub fn is_busy(self) -> bool {
        matches!(self, LaneStatus::Working | LaneStatus::Blocked)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LaneStatus::Idle => "idle",
            LaneStatus::Working => "working",
            LaneStatus::Blocked => "blocked",
            LaneStatus::Done => "done",
            LaneStatus::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for LaneStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A live agent session in a pane.
///
/// Mirrors the multiplexer's own payload, so it carries more than the
/// dispatcher currently reads — keeping the shape whole is what lets a new
/// decision use a field without another round trip.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Lane {
    /// The name spoolway assigned: `<task> · <step>` — see [`lane_name`].
    pub name: String,
    /// Agent kind, e.g. `pi` or `claude`.
    pub kind: String,
    pub status: LaneStatus,
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    pub cwd: PathBuf,
}

/// What [`Mux::vacate_lane`] found when it was done: the state the lane's pane
/// was actually left in.
///
/// Three states rather than a yes/no, because "the pane is not reusable"
/// covers two situations a caller has to handle differently — one where the
/// pane is gone and one where it is still there with an agent in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vacated {
    /// The session left and the pane is standing at its shell prompt, ready
    /// to be started in again.
    Shell,
    /// No gesture was tried — this backend has none, or this kind has no row
    /// — so the session was ended the only other way there is and the pane
    /// went with it. What every lane has always done.
    PaneClosed,
    /// The gesture was sent and the agent was still in the pane when the
    /// bound ran out. The pane is untouched and still occupied; closing it is
    /// the caller's decision, and one it must take *after* splitting whatever
    /// replaces it.
    StillOccupied,
}

/// A workspace created for one task: its checkout and its root pane.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub workspace_id: String,
    pub pane_id: String,
    pub tab_id: Option<String>,
    pub checkout_path: PathBuf,
}

/// How to start one agent in one pane.
#[derive(Debug, Clone)]
pub struct LaneSpec<'a> {
    /// `<task> · <step>` — see [`lane_name`] — the name every later call
    /// addresses it by.
    pub name: &'a str,
    /// What the pane is labelled. Purely for the person looking at it: the
    /// lane's identity is [`LaneSpec::name`], which is the multiplexer's own
    /// session name and is what [`Mux::list_lanes`] reads back. The tab already
    /// carries the task, so this carries the role.
    pub label: &'a str,
    /// herdr agent kind: `pi`, `claude`, `codex`, …
    pub kind: &'a str,
    pub pane_id: &'a str,
    /// Arguments passed through to the agent binary.
    pub args: &'a [String],
    /// Exported into the pane's shell before the agent starts, which is how a
    /// prompt's `spoolway report` knows which task it belongs to.
    pub env: &'a BTreeMap<String, String>,
    /// Put in front of the pane's `PATH`, so whatever lives there is what the
    /// multiplexer's `agent start` ends up running instead of the real agent
    /// binary. Every production caller passes `None`; tests use it to stand
    /// in a fake binary for the real one.
    pub path_prefix: Option<&'a Path>,
}

/// The operations spoolway needs from a multiplexer.
pub trait Mux {
    /// What to call this backend in a message a person reads.
    fn name(&self) -> &'static str;

    /// Can lanes be started here at all? Checked once before a dispatch pass so
    /// a missing backend is one clear message rather than one per lane.
    fn is_available(&self) -> bool;

    /// Why it is not, phrased as the thing to go and do about it.
    fn unavailable(&self) -> String;

    /// Does a lane that is waiting on a person keep a *process* alive while it
    /// waits?
    ///
    /// Under a multiplexer, yes: the session stays resident in its pane, which
    /// is what makes answering it resume the same conversation — and what makes
    /// it right for [`crate::dispatch`] to keep counting it against its
    /// profile's `concurrency`. Headless there is nothing resident to protect:
    /// a turn ends when its process exits, the next one is spawned against the
    /// transcript on disk, and a lane waiting overnight for an approval that
    /// still held a worker slot would be holding it for nothing.
    fn resident_while_waiting(&self) -> bool;

    /// Did this lane start a process that is still running, right now —
    /// independent of whatever [`LaneStatus`] the backend itself just
    /// reported for it?
    ///
    /// A lane mid-tool-call and a lane genuinely idle at its prompt can look
    /// identical on screen for as long as the call runs quietly, and a
    /// backend that reads the screen to tell "working" from "settled" apart
    /// is reading exactly the signal that cannot distinguish them. This is
    /// the other question — not "does it look busy" but "is anything of its
    /// own actually still running" — for a backend able to answer it from
    /// somewhere sturdier than the screen, such as a process table.
    ///
    /// `None`, the default, when the backend has no such way to check —
    /// [`crate::dispatch::Dispatcher::note_progress`] then trusts its other
    /// signal, the transcript, alone, exactly as it always has.
    fn lane_process_alive(&self, _name: &str) -> Option<bool> {
        None
    }

    fn list_lanes(&self) -> Result<Vec<Lane>>;

    /// Does the multiplexer still know the workspace and tab recorded against
    /// a task?
    ///
    /// Verified before either is reused — see
    /// [`crate::dispatch::ensure_workspace`] — because a multiplexer that
    /// restarts while the worktrees survive (a reboot, `tmux kill-server`,
    /// herdr being restarted) forgets every workspace and tab id it ever
    /// handed out. Reusing one blindly means `start_one` reads a `tab_id`
    /// nothing answers to any more, so the pane split fails on that pass and
    /// on every pass after it.
    ///
    /// `tab_id` is `None` for a backend that records none (headless); only
    /// the workspace is checked then.
    ///
    /// `true` by default, which is right for a backend with nothing that can
    /// go stale behind its back: headless derives a workspace id from the
    /// checkout path itself, so there is no separate registry for a restart
    /// to lose track of. Herdr and tmux — both fronted by a server that can
    /// restart out from under a surviving checkout — answer for real.
    fn workspace_alive(&self, _workspace_id: &str, _tab_id: Option<&str>) -> Result<bool> {
        Ok(true)
    }

    /// The one workspace every run of every project shares, found or opened.
    ///
    /// Fixed rather than named after a project — see
    /// [`dispatch_workspace_label`] — so that two projects dispatching at
    /// once are one row in the sidebar, not two: it holds no checkout of its
    /// own, and each project gets one tab of its own inside it.
    ///
    /// Found rather than made whenever one is already there, because that is
    /// what makes a second project's `dispatch` land in the workspace that
    /// already exists instead of opening another beside it.
    ///
    /// `create` says whether to open one that is not there yet. Cutting a
    /// worktree asks for it; the stop sweep asks without it, because a run
    /// that never opened a tab in it must not open the shared workspace on
    /// its way out just to close a tab in it.
    ///
    /// `None` from a backend with no such notion — headless has no workspaces
    /// at all — and under [`MuxMode::Split`], where every task is a group of
    /// its own and there is no shared workspace to open.
    fn dispatch_workspace(&self, _root: &Path, _create: bool) -> Result<Option<String>> {
        Ok(None)
    }

    /// A tab (herdr) or window (tmux) of `workspace_id`, opened on `cwd` with
    /// one pane, already sitting in `cwd` and free for the caller's own use —
    /// never a placeholder to be split from. Used to open a project's own tab
    /// in the run's shared workspace, on the worktree of whichever task is
    /// opening it, and — under [`MuxMode::Split`] — to give a task borrowing
    /// somebody else's checkout a home to run in.
    fn open_tab(&self, _workspace_id: &str, _cwd: &Path, _label: &str) -> Result<Workspace> {
        bail!("this backend has no tabs to open")
    }

    /// Open a pane on `cwd` and run `command` in it — not a bare shell for
    /// the caller to drive itself the way [`Mux::create_pane`] and
    /// [`Mux::open_tab`] do, but a foreground program already running by the
    /// time the pane appears. For the board's `o`: an editor is the person's
    /// own, not a lane, so nothing here waits for it to exit or reads
    /// anything back from it.
    ///
    /// The default refuses, the same as [`Mux::open_tab`]: a backend gets
    /// this only by implementing it. Headless inherits the refusal rather
    /// than overriding it — it has no pane to run anything in at all — and
    /// is the only backend left on it: herdr and tmux both implement this.
    fn open_command(&self, _cwd: &Path, _label: &str, _command: &str) -> Result<()> {
        bail!("this backend has no pane to open a command in")
    }

    /// The tab of `workspace_id` already labelled `label`, if there is one —
    /// a project's own tab in the shared workspace.
    ///
    /// Asked of the multiplexer rather than reconstructed from what the queue
    /// recorded, because the queue is not a complete record of them: a
    /// project whose tasks are all between steps holds no tab id anywhere.
    /// Found here, so a second dispatch pass joins the tab that already
    /// exists instead of opening another beside it.
    ///
    /// The label alone, with no pane or directory to check it against: since
    /// every pane a task's tab holds is now one of its lanes, sitting in that
    /// lane's own worktree, there is no anchor left standing anywhere to pick
    /// the right tab out with. Nor is there a same-named collision to guard
    /// against — [`crate::commands::init::claim`] refuses a second checkout
    /// that claims a project basename already pointed at another root, so a
    /// label is unique on this machine.
    ///
    /// `None` from a backend with no tabs, and whenever nothing matches.
    fn find_tab(&self, _workspace_id: &str, _label: &str) -> Result<Option<String>> {
        Ok(None)
    }

    /// Move the dispatcher's own pane into the run's workspace, so the board
    /// draws in the same group as the lanes it is drawing.
    ///
    /// Called once at the start of a run, after
    /// [`Mux::dispatch_workspace`] has found or made the workspace. A pane
    /// already in there is left where it is, which is what makes a dispatcher
    /// restarted in the pane a previous one moved a no-op rather than a second
    /// tab. Herdr never overrides this: the dispatcher's own pane stays where
    /// it was started under both layouts, and only tmux still moves its
    /// window into the shared session.
    ///
    /// `Ok(())` from a backend with no panes to move, and from a `dispatch`
    /// that is not running in one — a run started from an ordinary terminal
    /// against a reachable server still dispatches, it just draws where it was
    /// started.
    fn move_self_into(&self, _workspace_id: &str) -> Result<()> {
        Ok(())
    }

    /// Which workspace the dispatcher's own pane is in right now, if the
    /// backend has such a thing and this process is in one.
    ///
    /// Read live rather than remembered: the stop sweep is what asks, and by
    /// then the pane has already been moved once — see [`Mux::move_self_into`].
    fn own_workspace(&self) -> Option<String> {
        None
    }

    /// Is the workspace and tab recorded against a task the task's own, or
    /// ones the whole run shares?
    ///
    /// Its own under [`MuxMode::Split`], where every task cuts a workspace of
    /// its own; shared under [`MuxMode::Grouped`], where every task is a
    /// *pane* in the one tab its project shares. Two things turn on the
    /// answer and both would be disasters if it were assumed: tearing a task
    /// down must never close a tab or workspace that is not its own, and only
    /// a task that owns its row gets a fixed `spoolway/<task>` label at
    /// creation — a shared tab is never task's to rename.
    fn task_owns_workspace(&self) -> bool {
        true
    }

    /// Remove a checkout this backend cut outside the multiplexer's knowledge.
    ///
    /// Only ever called when [`Mux::task_owns_workspace`] is false: everywhere
    /// else the checkout goes with the workspace that owns it, and
    /// [`Mux::remove_workspace`] is what takes both. Never called for a
    /// borrowed checkout, which is a person's own.
    fn remove_checkout(&self, _path: &Path) -> Result<()> {
        Ok(())
    }

    /// Cut a task's own worktree, with git, and open its own workspace on it.
    /// Only ever called when [`Mux::task_owns_workspace`] is true: under
    /// [`MuxMode::Grouped`] a task's checkout is cut the same way but opens no
    /// workspace of its own — its lane runs in a pane of its project's shared
    /// tab instead. The checkout lands at [`worktree_root`] under a directory
    /// named after `branch`, flattened by [`branch_slug`] — the one rule every
    /// backend now shares.
    fn create_workspace(
        &self,
        cwd: &Path,
        branch: &str,
        base: &str,
        label: &str,
    ) -> Result<Workspace>;
    /// Remove a task's worktree, which takes its workspace with it. Never call
    /// this for a workspace that only borrowed an existing checkout: the
    /// checkout it would be asked to remove is the person's own.
    fn remove_workspace(&self, workspace_id: &str) -> Result<()>;
    /// Close a workspace that owns no worktree, leaving what it pointed at.
    fn close_workspace(&self, workspace_id: &str) -> Result<()>;
    /// Close one tab, leaving the workspace it lived in alone.
    fn close_tab(&self, tab_id: &str) -> Result<()>;
    /// A bare pane in an existing checkout, for a lane that needs no worktree
    /// of its own. Only ever called when [`Mux::task_owns_workspace`] is
    /// true, for the same reason [`Mux::create_workspace`] is.
    fn create_pane(&self, cwd: &Path, label: &str) -> Result<Workspace>;
    /// The same, but for a checkout this task cut for itself rather than
    /// borrowed — [`crate::dispatch::ensure_workspace`]'s heal path, the only
    /// caller, reaches for this instead of [`Mux::create_pane`] whenever
    /// `!task.front.borrowed`.
    ///
    /// A backend that marks ownership on the workspace or session itself
    /// rather than asking the multiplexer directly — tmux's
    /// `@spoolway_checkout`, headless's own workspace id — has to re-apply
    /// that mark here, or the pane this opens answers to
    /// [`Mux::remove_workspace`] as a borrowed one and cleanup silently
    /// leaves the worktree behind. [`Mux::create_pane`] by default, which is
    /// right for a backend like herdr whose removal asks the multiplexer for
    /// the workspace's own checkout rather than trusting a mark spoolway
    /// wrote down itself.
    fn reopen_owned_pane(&self, cwd: &Path, label: &str) -> Result<Workspace> {
        self.create_pane(cwd, label)
    }
    /// A fresh pane in `tab_id`, which is where every lane is started. A new
    /// pane is a bare shell by construction, so nothing has to be waited for
    /// and nothing has to be detected.
    ///
    /// Which pane in the tab actually gets split is this call's own decision,
    /// not the caller's: herdr has no rebalance command, so spoolway chooses
    /// the biggest one, ties to the newest, along its longer side, every
    /// time. tmux needs none of that — `select-layout tiled` retiles the
    /// whole window after every split.
    fn split_pane(&self, tab_id: &str, cwd: &Path) -> Result<String>;

    /// Run a shell script in a fresh pane of `tab_id`, labelled for a person,
    /// and answer the pane it landed in. `None` from a backend with no pane
    /// to run it in — headless — which is the caller's signal to fall back to
    /// a detached run.
    ///
    /// For [`crate::command_step`]'s own use: a command step's blocking run,
    /// given a pane rather than the `setsid`-detached process it used to be
    /// spawned as. The script is the caller's whole wrapper — pid file, exit
    /// trap, the `run:` line itself — piped to `tee` so the pane shows every
    /// line while the log gets the same text; this call only has to land it
    /// somewhere a person can watch.
    ///
    /// `env` is not the step's own small named map alone: the caller —
    /// [`crate::dispatch::Dispatcher::start_command_in_pane`] — hands this
    /// the dispatcher's own process environment as an inherited layer, with
    /// the step's named map merged on top, same key winning to the named
    /// value. A herdr or tmux pane starts life with the multiplexer server's
    /// own environment, not the dispatcher's, and would otherwise never see
    /// anything the dispatcher was started with — `SPOOLWAY_GH`, an e2e
    /// suite's own forge stub, among it — the way a headless run's child
    /// process does by ordinary inheritance. Applied by the backend before
    /// the script runs, and both backends have to write it out rather than
    /// hand it across some other way: written to a file beside the run's
    /// other bookkeeping and sourced with one `.` command under herdr — see
    /// [`Herdr::run_in_pane`] — and as `-e KEY=VALUE` flags on the respawn
    /// itself under tmux. A person watching a herdr pane sees the `.`
    /// command land, not the environment itself; typing the whole thing in,
    /// as one `export` line, is exactly what this is avoiding — herdr cuts
    /// that line mid-value past some length and the pane's shell then waits
    /// forever on the unterminated quote it left behind.
    ///
    /// `script` itself is not built from this full map:
    /// [`crate::command_step::script_for_pane`] writes only the step's own
    /// named map into the script text, since that is the small, declared
    /// contract a step actually asks for, not everything this process
    /// happens to be carrying.
    ///
    /// The default refuses nothing, the same shape [`Mux::open_tab`] takes:
    /// it declines outright rather than half-answering. headless inherits it
    /// unchanged — its panes are not real, so there is nothing to split.
    fn run_in_pane(
        &self,
        _tab_id: &str,
        _cwd: &Path,
        _label: &str,
        _script: &str,
        _env: &BTreeMap<String, String>,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    /// Close a pane, killing whatever is in it whatever kind of agent that is.
    ///
    /// Never call this for a task's only pane without meaning to take its tab
    /// with it: a tab whose last pane closes goes with it, and a workspace
    /// whose last tab closes goes with that.
    fn close_pane(&self, pane_id: &str) -> Result<()>;
    fn start_lane(&self, spec: &LaneSpec<'_>) -> Result<()>;
    fn prompt(&self, name: &str, text: &str) -> Result<()>;
    fn read(&self, name: &str, lines: usize) -> Result<String>;
    /// Stop a lane's turn in place, without ending the lane itself.
    ///
    /// Under a multiplexer this is a keystroke — Escape, sent the same way
    /// [`Mux::prompt`] sends `Enter` — which every agent already treats as
    /// "stop where you are". The session survives it: the pane looks exactly
    /// as it did mid-turn, so a later resume finds the same conversation
    /// there and sends it nothing, the same as a lane a person answered by
    /// hand. Headless has no keyboard to reach a running turn with, so there
    /// stopping the turn's process is the only interrupt there is — see the
    /// headless impl, which does exactly what [`Mux::stop_lane`] does.
    ///
    /// Addressed by lane name alone, like [`Mux::prompt`]: a herdr or tmux
    /// backend resolves its own pane from it, and there is no pane to close
    /// here the way [`Mux::stop_lane`] needs one for.
    fn interrupt_lane(&self, name: &str) -> Result<()>;
    /// End the agent session by closing the pane it is in. The pane is the
    /// lane's alone — its task's next step gets a new one — so there is no
    /// keystroke to guess at and no exit to wait for.
    fn stop_lane(&self, name: &str, pane_id: &str) -> Result<()>;

    /// End the agent session in a pane and hand the pane itself back, empty,
    /// at its shell prompt — the weaker thing [`Mux::stop_lane`] has never
    /// offered.
    ///
    /// This exists so a task's steps can share one pane instead of each
    /// splitting a new one and closing it again — see
    /// `Dispatcher::free_finished_lanes`, which is what calls it.
    ///
    /// What actually makes a session leave is per *kind*, not per backend —
    /// see [`crate::agent::Quit`] — so a backend answering for real reads the
    /// kind's row and does nothing at all when there is none.
    ///
    /// The answer says which of three things happened, because a caller
    /// cannot tell them apart afterwards and all three need different
    /// handling:
    ///
    /// - [`Vacated::Shell`] — the pane is standing, empty, and can be started
    ///   in again.
    /// - [`Vacated::PaneClosed`] — nothing was tried, the session was ended
    ///   the old way, and the pane is gone. Exactly today's behaviour.
    /// - [`Vacated::StillOccupied`] — the gesture was sent and the agent was
    ///   still in the pane when the bound ran out. The pane is left alone,
    ///   agent and all, rather than closed: a caller that wants it gone has
    ///   to split its replacement *first*, or a tab whose last pane this was
    ///   goes with it.
    ///
    /// Never assumed to have worked. A pane reported as empty that is not is
    /// the one failure that costs a task its pane: the next step's `agent
    /// start` would be typed into whatever is still sitting there.
    ///
    /// The default closes the pane, by deferring to [`Mux::stop_lane`], which
    /// is the right answer for every backend that has nothing to type at —
    /// headless, which has no panes at all, and tmux, whose pane *is* the
    /// agent process.
    fn vacate_lane(&self, name: &str, _kind: &str, pane_id: &str) -> Result<Vacated> {
        self.stop_lane(name, pane_id)?;
        Ok(Vacated::PaneClosed)
    }
    /// Bring a lane's pane back in front of the person sitting there.
    ///
    /// Called for exactly one thing: a task that has landed on `blocked`. What
    /// a person needs then is not the stage change and not the fifteen lines
    /// the notification carries, but the session as it stood when it stopped —
    /// and that session is still in its pane, in a workspace nobody is looking
    /// at. Focusing is the whole gesture; the pane is left alone otherwise.
    fn focus_lane(&self, name: &str) -> Result<()>;
    /// Unused by the dispatcher itself now that a row's label is fixed at
    /// creation and never changed underneath it — a task's tab and workspace
    /// are named once, either `spoolway/<task>` under `split` or the
    /// project's own name under `grouped`, and neither is renamed as a lane
    /// moves through steps the way it used to be. Kept on the trait, and
    /// exercised by the live tmux integration tests, rather than removed:
    /// deleting it would ripple into `headless.rs`, outside this task.
    #[allow(dead_code)]
    fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()>;
    /// See [`Mux::rename_tab`] — unused for the same reason.
    #[allow(dead_code)]
    fn rename_workspace(&self, workspace_id: &str, label: &str) -> Result<()>;
    fn rename_pane(&self, pane_id: &str, label: &str) -> Result<()>;
}

/// The backend this project dispatches through.
///
/// The one place a concrete backend is named. Everything downstream — the
/// dispatcher, `doctor` — works through [`Mux`], which is what
/// makes `backend = "headless"` a config edit rather than a code path.
pub fn backend(repo: &crate::repo::Repo) -> Box<dyn Mux> {
    let root = &repo.root;
    let config = &repo.config;
    match config.dispatch.backend {
        crate::config::Backend::Herdr => Box::new(Herdr::new(root, &config.dispatch)),
        crate::config::Backend::Headless => Box::new(crate::headless::Headless::new(
            root,
            &config.dispatch,
            repo.headless_dir(),
        )),
        crate::config::Backend::Tmux => Box::new(crate::tmux::Tmux::new(root, &config.dispatch)),
    }
}

/// How long a prompt is given to actually start a turn before spoolway assumes
/// it was typed but never submitted, and presses Enter itself. Also the bound
/// on re-verifying after that Enter, with `herdr agent wait`.
///
/// Short on purpose: paid once or twice per lane start, and the thing it is
/// waiting for — the agent going from settled to working — happens immediately
/// or not at all. It also doubles as the stall threshold: herdr reports a
/// plain `timeout` error at exactly this bound, and spoolway reads that the
/// same as its own `agent_prompt_stalled`.
const PROMPT_SUBMIT_TIMEOUT_MS: &str = "5000";

/// How long [`Mux::vacate_lane`] gives a session to actually leave its pane
/// after the gesture has been typed and submitted.
///
/// Bounded rather than open-ended because an agent can simply refuse to go —
/// the probe reproduced it, with a modal sitting in the pane waiting for an
/// answer nobody was there to give. A task whose one pane is waited on for
/// ever would never run another step, which is worse than the pane churn this
/// is meant to remove.
///
/// Ten seconds, which is generous for what it is measuring. Driven against a
/// real Claude Code in a real herdr pane: `/exit` submitted at a settled
/// prompt took it out of `herdr agent list` in about 750ms, the pane was
/// still standing at its shell, and `herdr agent start` on that same pane id
/// launched the next session in it. Everything past a second here is a
/// machine under load rather than an agent thinking it over, and the bound is
/// paid in full only by a session that was never going to leave.
const VACATE_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the pane is checked while [`VACATE_TIMEOUT`] runs down. One
/// `herdr agent list` per tick, so short enough to hand a settled pane back
/// promptly and long enough not to spin on the socket.
const VACATE_POLL: Duration = Duration::from_millis(250);

/// The herdr backend. Shells out to the `herdr` CLI, which speaks to its server
/// over a socket and answers in JSON.
#[derive(Debug, Clone)]
pub struct Herdr {
    /// Where `herdr` is invoked from, and the repository every worktree of a
    /// `MuxMode::Split` task is cut from: the project root.
    pub cwd: PathBuf,

    /// The main checkout of the repository `cwd` belongs to — a third value,
    /// never [`crate::repo::Repo::root`] or [`crate::repo::Repo::checkout`]
    /// under a new name. Equal to `cwd` in the ordinary case; only differs
    /// when this project's `.spoolway/` sits inside a linked worktree, where
    /// `Repo::root`'s own ancestor search finds the worktree itself rather
    /// than the main checkout. Given to herdr as the `--cwd` of every
    /// `worktree open` — see [`Herdr::open_worktree_workspace`] — because
    /// that is the one thing herdr resolves the repository a row nests under
    /// from, and it refuses a `--cwd` that is itself a linked worktree with
    /// `linked_worktree_source`.
    project_root: PathBuf,

    /// How this run is laid out — see [`MuxMode`], which is the one thing
    /// deciding whether a task cuts a workspace of its own or shares its
    /// project's pane in the run's one shared workspace.
    mode: MuxMode,

    /// Where this run's checkouts are cut, under [`MuxMode::Split`] — see
    /// [`worktree_root`]. Never where the shared workspace itself is opened;
    /// that is fixed, at [`dispatch_home`].
    worktree_root: PathBuf,

    /// The tab `workspace create` opened the shared workspace with, recorded
    /// only when *this* process is the one that opened it.
    ///
    /// A workspace cannot be created without a pane, and that pane is a bare
    /// shell nobody asked for. [`Herdr::open_tab`] closes it once a project's
    /// own tab exists to replace it, and remembering it rather than guessing
    /// is what keeps a second project — or a second run of this one — from
    /// closing a tab it never opened.
    root_tab: RefCell<Option<String>>,
}

impl Herdr {
    pub fn new(cwd: &Path, config: &DispatchConfig) -> Herdr {
        Herdr {
            cwd: cwd.to_path_buf(),
            project_root: crate::repo::main_checkout(cwd).unwrap_or_else(|| cwd.to_path_buf()),
            mode: config.herdr_mode,
            worktree_root: worktree_root(cwd, config),
            root_tab: RefCell::new(None),
        }
    }

    fn call<T: for<'de> Deserialize<'de>>(&self, args: &[&str]) -> Result<T> {
        let raw = run(&self.cwd, "herdr", args)?;
        let envelope: Envelope<T> = serde_json::from_str(&raw).with_context(|| {
            format!(
                "herdr {} returned output that is not the expected JSON envelope: {}",
                args.join(" "),
                raw.chars().take(200).collect::<String>()
            )
        })?;

        match (envelope.result, envelope.error) {
            (Some(result), _) => Ok(result),
            (None, Some(error)) => bail!("herdr {}: {}", args.join(" "), error.message),
            (None, None) => bail!(
                "herdr {} returned neither a result nor an error",
                args.join(" ")
            ),
        }
    }

    fn call_ignoring_result(&self, args: &[&str]) -> Result<()> {
        run(&self.cwd, "herdr", args)?;
        Ok(())
    }

    /// Run a herdr call whose failure might be a *reported* stall rather than
    /// a real one, and say which it was.
    ///
    /// `repo::run` folds stderr into a formatted `anyhow` message on a non-zero
    /// exit, which is enough for every other caller here but throws away the
    /// one thing this call needs: herdr's own JSON error envelope, written to
    /// stderr on failure, carrying a `code`. So this runs `herdr` itself and
    /// reads that envelope directly, rather than pattern-matching the text
    /// `run` would have produced.
    fn call_watching_for_stall(&self, args: &[&str]) -> PromptOutcome {
        let output = match std::process::Command::new("herdr")
            .args(args)
            .current_dir(&self.cwd)
            .output()
        {
            Ok(output) => output,
            Err(err) => {
                return PromptOutcome::Failed(
                    anyhow::Error::new(err).context(format!("running `herdr {}`", args.join(" "))),
                );
            }
        };

        if output.status.success() {
            return PromptOutcome::Started;
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if let Ok(envelope) = serde_json::from_str::<Envelope<serde_json::Value>>(&stderr)
            && let Some(error) = envelope.error
        {
            if matches!(
                error.code.as_deref(),
                Some("agent_prompt_stalled") | Some("timeout")
            ) {
                return PromptOutcome::Stalled;
            }
            return PromptOutcome::Failed(anyhow!("herdr {}: {}", args.join(" "), error.message));
        }

        PromptOutcome::Failed(anyhow!(
            "`herdr {}` failed ({}): {}",
            args.join(" "),
            output.status,
            stderr.trim()
        ))
    }

    /// The shared dispatch workspace, if this multiplexer already has one.
    ///
    /// Identified by its label *and* by holding no checkout, and both halves
    /// are load-bearing. The label is fixed — [`DISPATCH_WORKSPACE_LABEL`] —
    /// and the absent worktree is the other half, only true because the
    /// workspace is opened on [`dispatch_home`], which is not a checkout: a
    /// workspace opened anywhere inside a repository comes back bound to it,
    /// which is what made this test match nothing and open a new workspace on
    /// every single run.
    ///
    /// Verified against a live pair of calls, not assumed: `workspace create`
    /// on a directory that is not a checkout answers with `worktree: null`.
    fn dispatch_workspace_id(&self) -> Result<Option<String>> {
        let list: WorkspaceList = self.call(&["workspace", "list"])?;
        Ok(list
            .workspaces
            .into_iter()
            .find(|w| w.label == DISPATCH_WORKSPACE_LABEL && w.worktree.is_none())
            .map(|w| w.workspace_id))
    }

    /// Cut a task's own worktree with git, and say where it landed. Only ever
    /// under [`MuxMode::Split`] — see [`Mux::create_workspace`]. The directory
    /// is named after the branch slug, the rule every backend now shares — see
    /// [`branch_slug`].
    fn cut_task_worktree(&self, branch: &str, base: &str) -> Result<PathBuf> {
        let path = self.worktree_root.join(branch_slug(branch));
        cut_worktree(&self.cwd, &path, branch, base)?;
        Ok(path)
    }

    /// The pane this process is running in, if it is running in one.
    ///
    /// Asked of herdr rather than read out of `HERDR_PANE_ID`, because the
    /// environment a pane's shell was started with is fixed at that moment,
    /// and herdr's own bookkeeping is what stays current if a pane is ever
    /// moved elsewhere.
    ///
    /// `None` when there is no pane to speak of: `dispatch` run from an
    /// ordinary terminal that can still reach the server.
    fn own_pane(&self) -> Option<CurrentPane> {
        self.call::<PaneCurrent>(&["pane", "current"])
            .ok()
            .map(|c| c.pane)
    }

    /// git, in the repository this backend was built on.
    fn git(&self, args: &[&str]) -> Result<String> {
        run(&self.cwd, "git", args)
    }

    /// The workspace already open on this checkout, if one is.
    fn workspace_holding(&self, cwd: &Path) -> Result<Option<String>> {
        let list: WorkspaceList = self.call(&["workspace", "list"])?;
        Ok(list
            .workspaces
            .into_iter()
            .find(|w| w.worktree.as_ref().is_some_and(|t| t.checkout_path == cwd))
            .map(|w| w.workspace_id))
    }

    /// Which pane in `tab_id` the next lane should split off, and along which
    /// side: the biggest pane with no agent running in it, falling back to
    /// the biggest overall once every pane holds one, split `down` while
    /// both halves would keep at least [`MIN_ROWS_AFTER_SPLIT`] rows and
    /// `right` below that — herdr has no rebalance command, so a tab left to
    /// grow by always splitting the newest pane degenerates into slivers,
    /// and this is spoolway's decision to make every time.
    ///
    /// Three calls. `pane list` names a pane in the tab, because `pane
    /// layout` is addressed by pane rather than by tab and the layout that
    /// pane is in is the whole tab's, rects and all — asking `pane layout`
    /// for a tab directly is a usage error herdr answers on stderr, which
    /// arrives here as an unparseable envelope and reads, one layer up, as a
    /// tab this multiplexer has never heard of. `agent list` then says which
    /// of those panes are free to give up their space: a command step's pane
    /// runs a shell script rather than an agent, so it reads as agentless and
    /// is a valid target, same as any pane whose agent has already exited.
    ///
    /// Also answers the tab's own pane ids, read off this same `pane
    /// layout` call rather than a fourth one — see [`Herdr::split_pane`],
    /// which needs exactly this set, from exactly this moment, to tell
    /// which pane the split actually made.
    fn pane_to_split(&self, tab_id: &str) -> Result<(SplitTarget, HashSet<String>)> {
        let list: PaneList = self.call(&["pane", "list"])?;
        // Read off this same `pane list` reply, before it is consumed below
        // — see [`Herdr::split_pane`], which needs this tab's pane ids from
        // *this* moment. Taken from `pane list` rather than the `pane
        // layout` call two lines down: the two are different herdr
        // commands, and a before/after comparison is only meaningful when
        // both sides come from the same one.
        let before = pane_ids_in_tab(&list, tab_id);
        let any = list
            .panes
            .into_iter()
            .find(|p| p.tab_id.as_deref() == Some(tab_id))
            .with_context(|| format!("tab `{tab_id}` has no panes to split"))?;
        let layout: PaneLayout = self.call(&["pane", "layout", "--pane", &any.pane_id])?;
        let agents: AgentList = self.call(&["agent", "list"])?;
        let agent_panes: HashSet<String> = agents
            .agents
            .into_iter()
            .filter(|raw| raw.agent.is_some())
            .map(|raw| raw.pane_id)
            .collect();
        let target =
            choose_split(layout.layout, &agent_panes).context("this tab has no panes to split")?;
        Ok((target, before))
    }

    /// Open a workspace on a checkout this project owns, bound to the project
    /// it was cut from.
    ///
    /// `worktree open`, never `workspace create`: herdr resolves the
    /// repository a row nests under from `--cwd`, so `--cwd` is the project
    /// root and `--path` is the checkout. That is what puts the row under the
    /// project's own row in the sidebar instead of flat beside it, and what
    /// leaves `worktree remove` something to remove later.
    ///
    /// Shared by the first cut ([`Mux::create_workspace`]) and the heal path
    /// ([`Mux::reopen_owned_pane`]), because a checkout that is reopened after
    /// herdr forgot it belongs in exactly the same place it did the first
    /// time.
    fn open_worktree_workspace(&self, checkout: &Path, label: &str) -> Result<Workspace> {
        let path = checkout.display().to_string();
        // The main checkout, not `self.cwd` outright: see [`Herdr::project_root`]'s
        // own doc for the one case they differ, which is exactly the case
        // this call exists to get right.
        let root = self.project_root.display().to_string();
        let argv = worktree_open_argv(&root, &path, label);
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let created: WorkspaceCreated = self.call(&args)?;
        Ok(Workspace {
            workspace_id: created.workspace.workspace_id,
            pane_id: created.root_pane.pane_id,
            tab_id: created.root_pane.tab_id,
            checkout_path: checkout.to_path_buf(),
        })
    }

    /// A tab of `workspace`, opened on `cwd` and labelled `label`.
    ///
    /// Unlike [`Mux::open_tab`] this closes no root tab: it is for a workspace
    /// that already holds a checkout, which never has a bare shell of this
    /// process's making standing in it.
    fn tab_on(&self, workspace: &str, cwd: &Path, label: &str) -> Result<Workspace> {
        let path = cwd.display().to_string();
        let created: TabCreated = self.call(&[
            "tab",
            "create",
            "--workspace",
            workspace,
            "--cwd",
            &path,
            "--label",
            label,
            "--no-focus",
        ])?;
        Ok(Workspace {
            workspace_id: workspace.to_string(),
            pane_id: created.root_pane.pane_id,
            tab_id: Some(created.tab.tab_id),
            checkout_path: cwd.to_path_buf(),
        })
    }

    /// Is there still an agent session in this pane?
    ///
    /// Asked of `agent list` rather than of the pane, because leaving is
    /// exactly what herdr records there: a session that has ended stops being
    /// listed against its pane, and the pane itself carries on. Established
    /// alongside the `/exit` gesture, against a real Claude Code in a real
    /// pane.
    ///
    /// Unlike [`Mux::list_lanes`] this counts *any* agent, named or not. The
    /// question here is not "is this one of ours" but "is this pane free to
    /// start in", and a session herdr did not name is just as much in the way.
    fn pane_has_agent(&self, pane_id: &str) -> Result<bool> {
        let list: AgentList = self.call(&["agent", "list"])?;
        Ok(list
            .agents
            .iter()
            .any(|raw| raw.pane_id == pane_id && raw.agent.is_some()))
    }

    /// Write `env` to `<project home>/<dir_name>/<key>.<ext>` and answer the
    /// one `.` command that sources it — the file [`Mux::run_in_pane`] and
    /// [`Herdr::start_lane`] hand a pane instead of typing the environment in
    /// directly.
    ///
    /// `dir_name` puts the file beside whichever bookkeeping the caller
    /// already keeps: [`crate::command_step::RUN_DIR`] for a command step's
    /// run, [`crate::dispatch::SYSTEM_PROMPTS_DIR`] for a lane, next to its
    /// own composed system prompt. Overwritten on every call rather than
    /// rolled aside the way a log is — see
    /// [`crate::command_step::Runs::prev_log_path`] — because nothing here
    /// is ever read a second time: the one `.` command that follows is the
    /// file's only reader, and a fresh pane always wants the current
    /// environment, never a generation back.
    ///
    /// Written one assignment per line — [`crate::platform::Shell::env_export_lines`],
    /// not the single-line `env_export` a pane is typed — matching the
    /// task's own mockup, and named with [`crate::platform::Shell::source_extension`]'s
    /// own answer for the dialect: PowerShell refuses to dot-source a file
    /// not named `.ps1`.
    ///
    /// This is the dispatcher's whole inherited environment for a command
    /// step — see [`crate::dispatch::Dispatcher::start_command_in_pane`] —
    /// so whatever secret it was started with (a forge token, an API key)
    /// lands in this file too. Created `0600` on Unix, right after writing
    /// it, so a shared machine's other users see a directory listing and
    /// nothing more; Windows ACLs already restrict a user's own `~` to
    /// itself, which is the platform's own answer to the same question.
    fn hand_environment(
        &self,
        dir_name: &str,
        key: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<String> {
        let dir = project_home(&self.cwd).join(dir_name);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let sh = crate::platform::Shell::CURRENT;
        let path = dir.join(format!("{key}.{}", sh.source_extension()));
        std::fs::write(&path, format!("{}\n", sh.env_export_lines(env)))
            .with_context(|| format!("writing {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("restricting {}", path.display()))?;
        }
        Ok(sh.source_command(&path))
    }
}

/// The fewest rows a split-off half may be left with. Below this a pane is
/// too short to read comfortably, so the split goes `right` instead.
///
/// A fixed constant and not a `dispatch.*` setting: herdr's rects are in
/// terminal cells, and this is a property of what a person can read, not of
/// a project's preference.
const MIN_ROWS_AFTER_SPLIT: i64 = 20;

/// The pane in a tab's layout to split, and the side to halve it along.
///
/// Its own function so the rule can be read against a captured `pane layout`
/// payload — the shape of that payload is the whole of what went wrong here
/// once already.
///
/// `agent_panes` is the set of pane ids `agent list` says hold a live agent.
/// The target is the biggest pane outside that set — an idle root shell, say,
/// or a command step's pane, which runs a shell script and so never appears
/// in it — falling back to the biggest pane overall once every pane holds an
/// agent, so a split still has somewhere to go.
fn choose_split(layout: RawLayout, agent_panes: &HashSet<String>) -> Option<SplitTarget> {
    // `max_by_key` returns the *last* of several equally maximum elements,
    // which is exactly "ties to the newest" as long as `pane layout` lists
    // panes in the order they were made — the same order a fresh split is
    // appended in.
    let agentless: Vec<RawPaneRect> = layout
        .panes
        .iter()
        .filter(|p| !agent_panes.contains(&p.pane_id))
        .cloned()
        .collect();
    let chosen = agentless
        .into_iter()
        .max_by_key(|p| p.rect.width * p.rect.height)
        .or_else(|| {
            layout
                .panes
                .into_iter()
                .max_by_key(|p| p.rect.width * p.rect.height)
        })?;
    // A terminal cell is roughly twice as tall as it is wide, so the choice
    // is made on rows alone rather than by comparing width against height:
    // stack panes top/bottom for as long as each half still has room to
    // read, and only then start giving up columns.
    let direction = match chosen.rect.height / 2 >= MIN_ROWS_AFTER_SPLIT {
        true => "down",
        false => "right",
    };
    Some(SplitTarget {
        pane_id: chosen.pane_id,
        direction,
    })
}

/// The id of the tab labelled `label`, if the workspace has one.
///
/// Its own function, like [`choose_split`], so the rule can be read against a
/// captured `tab list` payload rather than only against a live herdr.
///
/// The label — always [`project_label`] — is the whole of the rule now.
/// Once a project's tab holds nothing but lanes, each sitting in its own
/// task's worktree, there is no anchor pane left standing in the project
/// root to pick the tab out with, and no directory to match against. Nor
/// can two tabs share the label and mean different projects:
/// [`crate::commands::init::claim`] refuses a second checkout that claims a
/// project basename already pointed at another root.
fn find_tab_id(tabs: TabList, label: &str) -> Option<String> {
    tabs.tabs
        .into_iter()
        .find(|t| t.label.as_deref() == Some(label))
        .map(|t| t.tab_id)
}

/// Every pane `pane list` says sits in `tab_id`, right now — the account
/// [`Herdr::split_pane`] takes both before and after a split, so the pane
/// that *appeared* between the two readings is the one it trusts, rather
/// than the split's own JSON reply. Its own function, like [`choose_split`]
/// and [`find_tab_id`], so the rule can be read against a captured `pane
/// list` payload rather than only against a live herdr.
fn pane_ids_in_tab(list: &PaneList, tab_id: &str) -> HashSet<String> {
    list.panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(tab_id))
        .map(|p| p.pane_id.clone())
        .collect()
}

/// Which pane a split actually made, confirmed against the multiplexer's
/// own account rather than trusted from the split's own reply.
///
/// `before` and `after` are [`pane_ids_in_tab`]'s answer for the same tab,
/// read from two separate `pane list` calls straddling the split; `reported`
/// is the pane id the split's own JSON reply claimed. Its own function, like
/// [`choose_split`] and [`find_tab_id`], so each of the three ways this can
/// refuse — nothing new appeared, more than one pane appeared, or the one
/// that did disagrees with the reply — has a test of its own, rather than
/// living only inline in [`Herdr::split_pane`] where nothing exercises the
/// refusals directly.
fn confirm_split(
    before: &HashSet<String>,
    after: &HashSet<String>,
    reported: &str,
) -> Result<String> {
    let mut appeared = after.difference(before);
    let confirmed = match (appeared.next(), appeared.next()) {
        (Some(only), None) => only.clone(),
        (None, _) => bail!(
            "herdr pane split reported pane `{reported}`, but no new pane appeared — \
             refusing to record a pane id that might not exist"
        ),
        (Some(_), Some(_)) => bail!(
            "herdr pane split left more than one new pane behind — refusing to guess \
             which one is `{reported}`"
        ),
    };
    if confirmed != reported {
        bail!(
            "herdr pane split reported pane `{reported}`, but the pane that actually \
             appeared is `{confirmed}` — refusing to record a pane id that disagrees with \
             the multiplexer's own pane list"
        );
    }
    Ok(confirmed)
}

/// Which pane [`Herdr::split_pane`] should split, and along which side.
struct SplitTarget {
    pane_id: String,
    direction: &'static str,
}

#[derive(Debug, Deserialize)]
struct PaneList {
    panes: Vec<RawPaneRow>,
}

/// One row of `pane list`. `tab_id` is optional because a backend without
/// tabs answers without one, the same way [`Workspace::tab_id`] is optional.
#[derive(Debug, Deserialize)]
struct RawPaneRow {
    pane_id: String,
    #[serde(default)]
    tab_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TabList {
    tabs: Vec<RawTabRow>,
}

/// One row of `tab list`. A tab opened by hand carries no label at all, which
/// is why this is optional rather than an empty string.
#[derive(Debug, Deserialize)]
struct RawTabRow {
    tab_id: String,
    #[serde(default)]
    label: Option<String>,
}

/// `pane layout` answers about the whole tab the pane it was given sits in,
/// under one `layout` key.
#[derive(Debug, Deserialize)]
struct PaneLayout {
    layout: RawLayout,
}

#[derive(Debug, Deserialize)]
struct RawLayout {
    panes: Vec<RawPaneRect>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawPaneRect {
    pane_id: String,
    rect: RawRect,
}

#[derive(Debug, Clone, Deserialize)]
struct RawRect {
    width: i64,
    height: i64,
}

#[derive(Debug, Deserialize)]
struct Envelope<T> {
    result: Option<T>,
    error: Option<EnvelopeError>,
}

#[derive(Debug, Deserialize)]
struct EnvelopeError {
    /// Absent on an envelope with nothing machine-readable to say — an
    /// ordinary usage error, say. Present and equal to `agent_prompt_stalled`
    /// or `timeout` is the one thing [`Herdr::call_watching_for_stall`] acts
    /// on; anything else is just folded into the message.
    #[serde(default)]
    code: Option<String>,
    message: String,
}

/// What one herdr call made toward starting a lane's turn: the turn began,
/// herdr itself reported the submission stalled, or something else went
/// wrong.
enum PromptOutcome {
    Started,
    Stalled,
    Failed(anyhow::Error),
}

#[derive(Debug, Deserialize)]
struct AgentList {
    agents: Vec<RawAgent>,
}

#[derive(Debug, Deserialize)]
struct RawAgent {
    /// The name the session was started under. Null for any session not
    /// started via `herdr agent start <NAME>` — that is, every session spoolway
    /// did not create, which is exactly what makes it a safe ownership test.
    #[serde(default)]
    name: Option<String>,
    /// Agent kind. Absent on a pane that has no agent in it.
    agent: Option<String>,
    #[serde(default = "unknown_status")]
    agent_status: LaneStatus,
    pane_id: String,
    #[serde(default)]
    tab_id: String,
    #[serde(default)]
    workspace_id: String,
    #[serde(default)]
    cwd: PathBuf,
}

fn unknown_status() -> LaneStatus {
    LaneStatus::Unknown
}

#[derive(Debug, Deserialize)]
struct RawWorkspace {
    workspace_id: String,
}

#[derive(Debug, Deserialize)]
struct RawPane {
    pane_id: String,
    #[serde(default)]
    tab_id: Option<String>,
}

/// What `pane current` answers with: the pane the calling process is in.
#[derive(Debug, Deserialize)]
struct PaneCurrent {
    pane: CurrentPane,
}

#[derive(Debug, Deserialize)]
struct CurrentPane {
    #[serde(default)]
    workspace_id: String,
}

/// `workspace create` returns this shape. `worktree open` returns the same
/// `workspace` and `root_pane` plus a `worktree` object this struct does not
/// name — nothing here needs it, and an undeclared field is dropped rather
/// than rejected.
#[derive(Debug, Deserialize)]
struct WorkspaceCreated {
    workspace: RawWorkspace,
    root_pane: RawPane,
}

#[derive(Debug, Deserialize)]
struct TabCreated {
    tab: RawTab,
    root_pane: RawPane,
}

#[derive(Debug, Deserialize)]
struct RawTab {
    tab_id: String,
}

#[derive(Debug, Deserialize)]
struct WorkspaceList {
    workspaces: Vec<RawWorkspaceEntry>,
}

#[derive(Debug, Deserialize)]
struct RawWorkspaceEntry {
    workspace_id: String,
    #[serde(default)]
    label: String,
    /// Absent for a workspace that is not a checkout of anything.
    ///
    /// This absence is load-bearing: it is what tells the dispatch workspace
    /// apart from the workspace herdr already labels after the repository
    /// directory, which for this project is the same word. See
    /// [`Herdr::dispatch_workspace`].
    #[serde(default)]
    worktree: Option<RawWorkspaceWorktree>,
}

#[derive(Debug, Deserialize)]
struct RawWorkspaceWorktree {
    checkout_path: PathBuf,
}

/// What `pane split` answers with: the pane it made.
#[derive(Debug, Deserialize)]
struct PaneSplit {
    pane: RawPane,
}

impl Mux for Herdr {
    fn name(&self) -> &'static str {
        "herdr"
    }

    fn is_available(&self) -> bool {
        run(&self.cwd, "herdr", &["agent", "list"]).is_ok()
    }

    fn unavailable(&self) -> String {
        "no herdr server is reachable — every lane runs in a pane, so start herdr first \
         (`herdr`) and run this from inside it. To dispatch through tmux instead, set \
         `dispatch.backend = \"tmux\"`; with no multiplexer at all, `\"headless\"`."
            .to_string()
    }

    fn resident_while_waiting(&self) -> bool {
        true
    }

    fn dispatch_workspace(&self, _root: &Path, create: bool) -> Result<Option<String>> {
        // Nothing to find and nothing to open: `split` puts every task in the
        // project's own group and leaves the dispatcher where it was started,
        // which is what it is for.
        if self.mode == MuxMode::Split {
            return Ok(None);
        }
        if let Some(existing) = self.dispatch_workspace_id()? {
            return Ok(Some(existing));
        }
        if !create {
            return Ok(None);
        }

        // Opened on the fixed dispatch home, shared by every project, and
        // never on a repository. A workspace opened inside a checkout comes
        // back bound to it, which files the run inside that project's own
        // group and makes it unfindable next time; this directory is not a
        // checkout, so herdr binds nothing to it.
        let home_dir = dispatch_home();
        std::fs::create_dir_all(&home_dir)
            .with_context(|| format!("creating {}", home_dir.display()))?;
        let home = home_dir.display().to_string();
        let created: WorkspaceCreated = self.call(&[
            "workspace",
            "create",
            "--cwd",
            &home,
            "--label",
            DISPATCH_WORKSPACE_LABEL,
            // Never steal focus: `dispatch` is usually started from a pane the
            // person is watching, and a project's own tab is about to draw
            // beside it.
            "--no-focus",
        ])?;
        // The bare shell it had to be created with. Remembered so
        // [`Herdr::open_tab`] can close exactly that tab, once a project's own
        // tab replaces it, and no other.
        *self.root_tab.borrow_mut() = created.root_pane.tab_id.clone();
        Ok(Some(created.workspace.workspace_id))
    }

    fn open_tab(&self, workspace_id: &str, cwd: &Path, label: &str) -> Result<Workspace> {
        let path = cwd.display().to_string();
        let created: TabCreated = self.call(&[
            "tab",
            "create",
            "--workspace",
            workspace_id,
            "--cwd",
            &path,
            "--label",
            label,
            "--no-focus",
        ])?;

        // Now that there is a tab beyond the bare shell `workspace create` had
        // to open with, that shell can go — which is what makes a project's
        // own tab the first one rather than the one after an empty prompt
        // nobody typed into. Only ever the tab *this instance* opened: a
        // second project finding the workspace already there recorded no
        // root tab and closes nothing of another project's.
        if let Some(tab) = self.root_tab.borrow_mut().take() {
            let _ = self.close_tab(&tab);
        }

        Ok(Workspace {
            workspace_id: workspace_id.to_string(),
            pane_id: created.root_pane.pane_id,
            tab_id: Some(created.tab.tab_id),
            checkout_path: cwd.to_path_buf(),
        })
    }

    fn open_command(&self, cwd: &Path, label: &str, command: &str) -> Result<()> {
        // A pane exactly the way `create_pane` gives a lane one — under the
        // checkout's own workspace if it already has one open, or a fresh
        // workspace of its own otherwise — and then the command is typed
        // into it and submitted, the same way `start_lane` types a lane's
        // own environment into a fresh pane before the agent starts.
        let workspace = self.create_pane(cwd, label)?;
        self.call_ignoring_result(&["pane", "run", &workspace.pane_id, command])
    }

    fn find_tab(&self, workspace_id: &str, label: &str) -> Result<Option<String>> {
        let tabs: TabList = self.call(&["tab", "list", "--workspace", workspace_id])?;
        Ok(find_tab_id(tabs, label))
    }

    fn task_owns_workspace(&self) -> bool {
        // Under `split` every task cuts a workspace of its own; under
        // `grouped` every task is a pane in the one tab its project shares.
        self.mode == MuxMode::Split
    }

    fn remove_checkout(&self, path: &Path) -> Result<()> {
        self.git(&["worktree", "remove", "--force", &path.display().to_string()])?;
        Ok(())
    }

    fn own_workspace(&self) -> Option<String> {
        self.own_pane().map(|pane| pane.workspace_id)
    }

    fn list_lanes(&self) -> Result<Vec<Lane>> {
        let list: AgentList = self.call(&["agent", "list"])?;

        Ok(list
            .agents
            .into_iter()
            .filter_map(|raw| {
                // A pane with no agent in it is not a lane, and an unnamed
                // session is someone else's — a human's own pi or claude
                // window in the same multiplexer. Never count it, never
                // prompt it, and above all never close its pane.
                let kind = raw.agent?;
                let name = from_agent_name(&raw.name?);

                Some(Lane {
                    name,
                    kind,
                    status: raw.agent_status,
                    pane_id: raw.pane_id,
                    tab_id: raw.tab_id,
                    workspace_id: raw.workspace_id,
                    cwd: raw.cwd,
                })
            })
            .collect())
    }

    /// Asked of herdr's own listings rather than tried-and-caught: a call
    /// against an id herdr has never heard of answers with an ordinary
    /// envelope error, indistinguishable from any other failure, and this
    /// wants a plain "no" rather than something [`Herdr::call`] would bail
    /// out of `ensure_workspace` with.
    fn workspace_alive(&self, workspace_id: &str, tab_id: Option<&str>) -> Result<bool> {
        let workspaces: WorkspaceList = self.call(&["workspace", "list"])?;
        if !workspaces
            .workspaces
            .iter()
            .any(|w| w.workspace_id == workspace_id)
        {
            return Ok(false);
        }
        let Some(tab_id) = tab_id else {
            return Ok(true);
        };
        let tabs: TabList = self.call(&["tab", "list", "--workspace", workspace_id])?;
        Ok(tabs.tabs.iter().any(|t| t.tab_id == tab_id))
    }

    fn create_workspace(
        &self,
        _cwd: &Path,
        branch: &str,
        base: &str,
        label: &str,
    ) -> Result<Workspace> {
        // Cut with git, never with `herdr worktree create --cwd <repo>`: that
        // call also registered the repository itself as a workspace of its
        // own, a stray row this used to leave behind on every single task.
        //
        // The workspace is opened with `worktree open`, not `workspace
        // create`: herdr resolves the repository a checkout nests under from
        // `--cwd`, so `--cwd` is the project root and `--path` is the
        // checkout just cut. That is what makes the row land under the
        // project's own row instead of sitting flat in the sidebar, and what
        // makes `worktree remove` later find something to remove.
        let checkout = self.cut_task_worktree(branch, base)?;
        self.open_worktree_workspace(&checkout, label)
    }

    fn remove_workspace(&self, workspace_id: &str) -> Result<()> {
        self.call_ignoring_result(&["worktree", "remove", "--workspace", workspace_id, "--force"])
    }

    fn close_workspace(&self, workspace_id: &str) -> Result<()> {
        self.call_ignoring_result(&["workspace", "close", workspace_id])
    }

    fn close_tab(&self, tab_id: &str) -> Result<()> {
        self.call_ignoring_result(&["tab", "close", tab_id])
    }

    fn create_pane(&self, cwd: &Path, label: &str) -> Result<Workspace> {
        let path = cwd.display().to_string();

        // A tab in the workspace that already holds this checkout, so a closeout
        // appears under the plan it belongs to rather than as a stray workspace
        // beside it. Found by checkout path, never by focus: `--current` would
        // resolve to whatever pane the dispatcher sits in, or to someone else's
        // when the dispatcher is not in one at all.
        if let Some(workspace) = self.workspace_holding(cwd)? {
            return self.tab_on(&workspace, cwd, label);
        }

        // Nobody has this checkout open: it needs a home of its own, and how
        // that home is opened decides two things well past this call.
        //
        // `worktree open` first, exactly as [`Mux::create_workspace`] opens a
        // checkout spoolway cut itself. herdr binds a worktree to a workspace
        // only when the workspace was opened *onto* it that way; a workspace
        // created on the checkout instead comes back with no binding at all,
        // however plainly the path sits inside the repository. An unbound row
        // is the one that files itself flat in the sidebar rather than under
        // its project, and the one `worktree remove --workspace` later refuses
        // with `not_linked_worktree` — so both halves of the bug a person sees
        // at the end of a run start here.
        //
        // `workspace create` is kept as the fallback for the case the first
        // call is right to refuse: a directory that is no checkout of this
        // repository at all.
        if let Ok(workspace) = self.open_worktree_workspace(cwd, label) {
            return Ok(workspace);
        }

        let created: WorkspaceCreated = self.call(&[
            "workspace",
            "create",
            "--cwd",
            &path,
            "--label",
            label,
            "--no-focus",
        ])?;
        Ok(Workspace {
            workspace_id: created.workspace.workspace_id,
            pane_id: created.root_pane.pane_id,
            tab_id: created.root_pane.tab_id,
            checkout_path: cwd.to_path_buf(),
        })
    }

    // No `reopen_owned_pane` of its own any more. It existed to reopen a
    // resumed task's checkout with `worktree open` while the plain
    // `create_pane` still used `workspace create`, and the two now do the same
    // thing — `create_pane` reaches for `worktree open` first whatever brought
    // it there. tmux keeps its own override because it has a mark to re-apply;
    // herdr has none, and asks the multiplexer for the binding instead.

    fn split_pane(&self, tab_id: &str, cwd: &Path) -> Result<String> {
        let path = cwd.display().to_string();
        let (target, before) = self.pane_to_split(tab_id)?;
        // `--pane`, never the positional `pane split <id>`: the positional form
        // splits the *focused* pane and ignores the one it was given, which
        // silently splits a pane in whatever workspace a person is looking at.
        let created: PaneSplit = self.call(&[
            "pane",
            "split",
            "--pane",
            &target.pane_id,
            "--direction",
            target.direction,
            "--cwd",
            &path,
            "--no-focus",
        ])?;
        let reported = created.pane.pane_id;
        // Read back from `pane list` rather than trusted outright: a `.pane`
        // file once held `w8:p4` while herdr had `w8:p5`, because the split's
        // own JSON reply had already drifted from the multiplexer's own
        // bookkeeping by the time this ran. `confirm_split` takes it from
        // there — see its own doc for what it checks and why it is a
        // function of its own rather than living inline here.
        let list: PaneList = self.call(&["pane", "list"])?;
        let after = pane_ids_in_tab(&list, tab_id);
        confirm_split(&before, &after, &reported)
    }

    fn run_in_pane(
        &self,
        tab_id: &str,
        cwd: &Path,
        label: &str,
        script: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<Option<String>> {
        // Split exactly the way a lane's pane is, then hand the whole script
        // to it as one shell command — the same `pane run` route
        // [`Herdr::open_command`] takes for the board's own `o`, just against
        // an existing tab instead of a fresh pane of its own.
        let pane = self.split_pane(tab_id, cwd)?;
        self.rename_pane(&pane, label)?;
        // Written to a file and sourced, rather than typed ahead of the
        // script as one `export` line — see [`hand_environment`]'s own doc
        // for why: this map is the dispatcher's whole inherited environment,
        // large enough that a herdr pane has cut it mid-value before, and a
        // shell left waiting on the unterminated quote that leaves behind
        // never gets as far as the script line below it.
        if !env.is_empty() {
            let key = format!("{} · handover", lane_task(label));
            let source = self.hand_environment(crate::command_step::RUN_DIR, &key, env)?;
            self.call_ignoring_result(&["pane", "run", &pane, &source])?;
        }
        self.call_ignoring_result(&["pane", "run", &pane, script])?;
        Ok(Some(pane))
    }

    fn close_pane(&self, pane_id: &str) -> Result<()> {
        self.call_ignoring_result(&["pane", "close", pane_id])
    }

    fn start_lane(&self, spec: &LaneSpec<'_>) -> Result<()> {
        // Everything below is typed at a shell prompt, and lands in an agent's
        // chat input instead if anything is running in the pane. Nothing checks
        // for that here, because both panes the dispatcher ever hands in are
        // already known to be bare shells: one it has just split, with no
        // previous occupant to leave, and one it inherited from the step
        // before — offered only after `Mux::vacate_lane` confirmed that
        // session left and the pane came back to its prompt.
        //
        // A PATH prefix goes on first, because everything after it — including
        // the agent `agent start` is about to launch — is resolved through this
        // PATH. Nothing sets it in a real run; the tests use it to put a
        // stand-in agent in front of the real one.
        if let Some(prefix) = spec.path_prefix {
            let export = crate::platform::Shell::CURRENT.path_export(prefix);
            self.call_ignoring_result(&["pane", "run", spec.pane_id, &export])?;
        }

        // The lane's environment reaches the pane's shell first, so the agent
        // inherits it at startup — which is how `spoolway report` inside the
        // lane knows which task it is working on without being told in the
        // prompt. Written to a file beside this lane's own composed system
        // prompt and sourced, rather than typed in as one `export` line — see
        // [`hand_environment`]'s own doc for why: a herdr pane cuts that line
        // mid-value past some length, and the pane's shell then waits
        // forever on the unterminated quote it left behind, with no agent
        // ever started to notice.
        if !spec.env.is_empty() {
            let source =
                self.hand_environment(crate::dispatch::SYSTEM_PROMPTS_DIR, spec.name, spec.env)?;
            self.call_ignoring_result(&["pane", "run", spec.pane_id, &source])?;
        }

        let handle = to_agent_name(spec.name);
        let mut args: Vec<&str> = vec![
            "agent",
            "start",
            &handle,
            "--kind",
            spec.kind,
            "--pane",
            spec.pane_id,
            // herdr's default 30s readiness wait reads a slow cold start as a
            // dead agent: a node CLI in a fresh pane on a loaded machine (WSL,
            // first run after checkout churn) can take longer than that to
            // reach its prompt, and the timeout kills a lane that was seconds
            // from ready — then every retry pays the same toll. Waiting longer
            // costs nothing when the agent really is dead: the failure just
            // arrives late once.
            "--timeout",
            "120000",
            "--",
        ];
        args.extend(spec.args.iter().map(String::as_str));
        self.call_ignoring_result(&args)?;

        // The label is cosmetic — `list_lanes` finds a lane by the session name
        // `agent start` was given above, never by what the pane is called.
        self.rename_pane(spec.pane_id, spec.label)
    }

    fn prompt(&self, lane: &str, text: &str) -> Result<()> {
        let name = &to_agent_name(lane);
        // Bounded, and only long enough to see the turn *start*. Not a wait for
        // the lane to finish — that would serialise every lane behind whichever
        // was prompted first, which is why this used to pass no `--wait` at all.
        //
        // It has to see the start, though, because a prompt that is typed and
        // never submitted is indistinguishable afterwards from a lane that
        // answered and is waiting for a person: both are settled with an
        // unchanged stage. Observed against Claude Code — the text landed in the
        // input box, no turn began, and the task sat on `review` until somebody
        // looked at the pane.
        //
        // herdr already tells us which of the two happened: a stalled
        // submission exits non-zero with `agent_prompt_stalled` in its error
        // envelope, and the 5000ms timeout spoolway passes sits exactly on
        // herdr's own stall threshold, so a plain `timeout` means the same
        // thing. Anything else is a real failure and is never papered over
        // with an Enter — a healthy lane that answered normally must never
        // receive a stray keystroke.
        let wait = [
            "agent",
            "prompt",
            name,
            text,
            "--wait",
            "--until",
            "working",
            "--timeout",
            PROMPT_SUBMIT_TIMEOUT_MS,
        ];
        match self.call_watching_for_stall(&wait) {
            PromptOutcome::Started => return Ok(()),
            PromptOutcome::Failed(err) => return Err(err),
            PromptOutcome::Stalled => {}
        }

        // Stalled: the text is sitting in the input box, unsent. One Enter
        // submits it — and only it gets one, never a lane that is genuinely
        // mid-turn or one that already answered.
        let _ = self.call_ignoring_result(&["agent", "send-keys", name, "Enter"]);

        // Re-verify with herdr's own wait rather than a single immediate status
        // read: the Enter still has to land and the turn still has to start,
        // and a lane status read the instant after pressing it can catch the
        // lane before either has happened.
        match self.call_watching_for_stall(&[
            "agent",
            "wait",
            name,
            "--until",
            "working",
            "--timeout",
            PROMPT_SUBMIT_TIMEOUT_MS,
        ]) {
            PromptOutcome::Started => Ok(()),
            PromptOutcome::Stalled => {
                // The lane's own name, not the wire spelling: this reaches a
                // person, and a person addresses a lane as `<task> · <step>`.
                bail!("herdr agent prompt {lane}: submission stalled even after Enter")
            }
            PromptOutcome::Failed(err) => Err(err),
        }
    }

    /// A pane's recent output, as text.
    ///
    /// The one herdr verb that answers with the thing itself instead of a JSON
    /// envelope — `--format text` means the output *is* text, not that a `text`
    /// field carries it. Putting this through `call` therefore failed to parse
    /// every single time, and both callers swallow the error with
    /// `unwrap_or_default`: the dispatcher's own progress hash saw an empty
    /// string every pass, so no lane ever looked like it was making progress,
    /// and an escalation's "last 15 lines" were fifteen lines of nothing.
    fn read(&self, lane: &str, lines: usize) -> Result<String> {
        let name = &to_agent_name(lane);
        let lines = lines.to_string();
        run(
            &self.cwd,
            "herdr",
            &["agent", "read", name, "--lines", &lines, "--format", "text"],
        )
    }

    fn interrupt_lane(&self, name: &str) -> Result<()> {
        self.call_ignoring_result(&["agent", "send-keys", &to_agent_name(name), "Escape"])
    }

    fn stop_lane(&self, _name: &str, pane_id: &str) -> Result<()> {
        // No agent CLI has a "quit" verb, so asking one to leave used to mean
        // guessing its keystroke — `ctrl+d` ends a `pi` and is ignored outright
        // by Claude Code — and then waiting to find out whether it worked.
        // Closing the pane needs to know neither. The pane is this lane's own,
        // so nothing of the task's is in it.
        self.close_pane(pane_id)
    }

    fn vacate_lane(&self, lane: &str, kind: &str, pane_id: &str) -> Result<Vacated> {
        // No row for this kind means nobody has watched this binary leave a
        // pane, and a guessed gesture is worse than the churn it would save:
        // it either does nothing, or it lands as text in somebody's
        // conversation. Fall back to what every lane has always done.
        let Some(quit) = crate::agent::adapter(kind).and_then(|a| a.quit.as_ref()) else {
            self.stop_lane(lane, pane_id)?;
            return Ok(Vacated::PaneClosed);
        };

        // Typed at the *pane*, not through `agent prompt`: the gesture is a
        // slash command the agent acts on itself, not a turn to wait on, and
        // `agent prompt --wait --until working` would sit out its whole bound
        // waiting for a turn that is never going to start. Text first, then
        // Enter as its own call — the same two-step `Herdr::start_lane`
        // already uses to type at a pane.
        self.call_ignoring_result(&["pane", "send-text", pane_id, quit.line])?;
        self.call_ignoring_result(&["pane", "send-keys", pane_id, "enter"])?;

        // Then watch for the agent to actually go, rather than sleeping once
        // and assuming. A pane reported as empty that still has an agent in
        // it is the expensive failure here: the next step's `agent start`
        // would be typed straight into the conversation still sitting there.
        let deadline = Instant::now() + VACATE_TIMEOUT;
        loop {
            if !self.pane_has_agent(pane_id)? {
                return Ok(Vacated::Shell);
            }
            if Instant::now() >= deadline {
                // Left exactly as it was found. Closing it here would be the
                // one thing a caller cannot undo, and under a shared tab a
                // pane closed without a replacement beside it takes the tab
                // with it.
                return Ok(Vacated::StillOccupied);
            }
            std::thread::sleep(VACATE_POLL);
        }
    }

    fn focus_lane(&self, lane: &str) -> Result<()> {
        let name = &to_agent_name(lane);
        // By agent name rather than pane id: `agent focus` walks the workspace
        // and tab the pane lives in, which is the difference between the pane
        // being focused somewhere off-screen and it being what you are looking
        // at.
        self.call_ignoring_result(&["agent", "focus", name])
    }

    fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()> {
        self.call_ignoring_result(&["tab", "rename", tab_id, label])
    }

    fn rename_workspace(&self, workspace_id: &str, label: &str) -> Result<()> {
        self.call_ignoring_result(&["workspace", "rename", workspace_id, label])
    }

    fn rename_pane(&self, pane_id: &str, label: &str) -> Result<()> {
        self.call_ignoring_result(&["pane", "rename", pane_id, label])
    }
}

/// A lane's name — `<task> · <step>` — is the one thing spoolway matches a
/// session on: what the pane header shows, what `spoolway lane` lists, and
/// the argument `spoolway lane -m` takes. One name, not a display name over a
/// hidden key, so every caller that matches a lane moves with it. Never touch
/// a session whose name does not parse — see [`parse_lane_name`] — since that
/// is an unrelated agent a human happens to have open in the same
/// multiplexer.
pub fn lane_name(step: &str, task: &str) -> String {
    tab_label(task, step)
}

/// What every run's one shared workspace is called, in every project.
///
/// Fixed, rather than named after a project: the one thing every dispatched
/// project has in common is that spoolway is running, and a workspace named
/// after each project separately never said so. Two projects dispatching at
/// once now share this one row in the sidebar, each holding a tab of its
/// own — see [`Mux::open_tab`] — rather than a row each.
pub const DISPATCH_WORKSPACE_LABEL: &str = "spoolway-dispatcher";

/// The same label, as a function — kept so a caller written against the old,
/// per-project signature still compiles against a fixed one.
pub fn dispatch_workspace_label(_root: &Path) -> String {
    DISPATCH_WORKSPACE_LABEL.to_string()
}

/// The name reserved for [`dispatch_home`], under `~/.spoolway/`. No checkout
/// may claim it as its own directory name — `spoolway init` refuses it the
/// same way it refuses a name another checkout already holds.
pub const DISPATCH_HOME_NAME: &str = ".dispatcher";

/// The directory the shared dispatch workspace is opened on.
///
/// Not a checkout of anything, which is what lets [`Herdr::dispatch_workspace_id`]
/// find the workspace again by its label alone: a workspace opened inside a
/// repository comes back bound to it, and this directory holds none. Named
/// with a leading dot so that it sorts apart from every project directory
/// beside it under `~/.spoolway/` — see [`project_home`] — and so that a name
/// a checkout could plausibly have can never collide with it.
pub fn dispatch_home() -> PathBuf {
    home().join(".spoolway").join(DISPATCH_HOME_NAME)
}

/// A project's own directory name, which is what its tab in the shared
/// dispatch workspace is labelled, what names its rows under
/// [`MuxMode::Split`], and what [`project_home`] resolves under `~/.spoolway/`.
pub fn project_label(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string())
}

/// Where a project's own runtime state lives: `~/.spoolway/<basename>/`.
///
/// The one directory a project's queue, archive, plans, lane bookkeeping and
/// dispatched worktrees all sit under — see [`crate::repo::Repo::home`] for
/// the rest of it, and [`worktree_root`] for the one piece that lives here
/// too but does not go through `Repo`. Two checkouts sharing a basename share
/// this directory, which is exactly the clash `spoolway init` refuses —
/// nothing here resolves that; it only names where the pointer file lives.
pub fn project_home(root: &Path) -> PathBuf {
    home().join(".spoolway").join(project_label(root))
}

/// Where dispatched checkouts are cut, for both backends and every layout.
///
/// Outside the project checkout, always: a worktree under `.spoolway/` would
/// sit beside the prompts every lane already reads, and a lane building
/// there could rewrite any other task's checkout by name.
///
/// Outside `~/.herdr/` too, which it did not used to be. That directory is
/// herdr's, and the checkouts spoolway cuts are ones herdr never hears about
/// until it is pointed at one — filing them there invited the multiplexer to
/// make sense of directories it did not make. One root, one setting
/// (`dispatch.worktree_root`), one answer to where a dispatched checkout
/// lives — nested under [`project_home`] by default, beside the same
/// project's queue and archive, so a worktree cut here never registers as a
/// workspace of its own the way one cut at a repository's root did.
///
/// The directory *under* the root is the branch slug — see [`branch_slug`] —
/// for every backend now: one flat entry per task that `git worktree list`
/// and a person both read by the branch, and the same name whichever
/// multiplexer cut it. A tracker slug on the branch rides into that directory
/// name for free.
pub fn worktree_root(root: &Path, config: &DispatchConfig) -> PathBuf {
    let configured = config.worktree_root.trim();
    if !configured.is_empty() {
        return PathBuf::from(shellexpand_home(configured));
    }
    project_home(root).join("worktrees")
}

/// Where `~` and the default worktree root resolve to.
///
/// [`crate::platform::home_dir`] rather than `$HOME`, which is the whole point
/// of that helper: `HOME` is a POSIX convention, and reading only it is the bug
/// that stopped a lane running natively on Windows at all.
pub fn home() -> PathBuf {
    crate::platform::home_dir().unwrap_or_else(std::env::temp_dir)
}

/// `~` at the front of a configured path, and nowhere else — a project that
/// wrote one is naming its own home, not asking for shell expansion.
fn shellexpand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => home().join(rest).display().to_string(),
        None => path.to_string(),
    }
}

/// The argv of the `herdr worktree open` call that binds a task's checkout to
/// its project. `root` is the project's own directory — never the checkout —
/// because herdr resolves the repository a row nests under from `--cwd`, and
/// `path` is the checkout that call binds.
///
/// Pulled out on its own so the shape of the call is checkable without a real
/// herdr to send it to.
fn worktree_open_argv(root: &str, path: &str, label: &str) -> Vec<String> {
    vec![
        "worktree".to_string(),
        "open".to_string(),
        "--cwd".to_string(),
        root.to_string(),
        "--path".to_string(),
        path.to_string(),
        "--label".to_string(),
        label.to_string(),
        "--no-focus".to_string(),
    ]
}

/// A branch name reduced to one directory component: `task/add-endpoint`
/// becomes `task-add-endpoint`. Every backend names a task's worktree
/// directory this way, so herdr, tmux and headless agree — and a slug the
/// tracker prefixed onto the branch (`task/proj-12-add-endpoint`) rides into
/// the directory name for free. Without it a `/` in the branch would nest
/// every worktree under a shared `task/` directory that nothing owns or
/// cleans up.
pub(crate) fn branch_slug(branch: &str) -> String {
    branch.replace('/', "-")
}

/// Cut a worktree with git, at exactly the path asked for.
///
/// Shared by the headless backend, which has never had a multiplexer to ask,
/// and by herdr and tmux both — neither hands its own worktree-cutting
/// machinery a repository any more; every checkout is cut here, with git, and
/// the multiplexer is only ever pointed at what already exists.
///
/// A branch left behind by an earlier run is reused rather than fought with:
/// `-b` on a branch that already exists fails outright, and the task it belongs
/// to would never start again.
///
/// The task id is safe to join onto the root, and [`crate::config::check_id`]
/// is what makes it so — at `queue add` and again whenever a task file is read,
/// which is the half that matters: a file nobody queued is still a file the
/// dispatcher will pick up.
pub fn cut_worktree(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let path_arg = path.display().to_string();
    let exists = run(repo, "git", &["rev-parse", "--verify", "--quiet", branch]).is_ok();
    let args: Vec<&str> = match exists {
        true => vec!["worktree", "add", &path_arg, branch],
        false => vec!["worktree", "add", "-b", branch, &path_arg, base],
    };
    run(repo, "git", &args)
        .with_context(|| format!("could not cut a worktree for `{branch}` at {path_arg}"))?;
    Ok(())
}

/// herdr's own bound on an agent name: 1–32 characters, refused outright by
/// `agent start` with `invalid_agent_name` (herdr 0.8.2). It is not spoolway's
/// choice and cannot be argued with, so [`LANE_NAME_MAX`] is derived from it
/// rather than picked.
const AGENT_NAME_MAX: usize = 32;

/// What the wire spelling saves against a lane's own name: `" · "` is four
/// bytes and [`AGENT_NAME_SEPARATOR`] is two.
const AGENT_NAME_SAVING: usize = LANE_NAME_SEPARATOR.len() - AGENT_NAME_SEPARATOR.len();

/// The bound on a lane's own name, `<task> · <step>`, checked when the task is
/// queued — because the alternative is a lane that refuses to start somewhere
/// in the middle of a pipeline, after earlier steps have already done their
/// work.
///
/// It is [`AGENT_NAME_MAX`] and not a round number of spoolway's own choosing.
/// A lane crosses the wire as [`to_agent_name`] spells it, which is this name
/// with its separator swapped, so the wire name is always [`AGENT_NAME_SAVING`]
/// bytes shorter. Every id a task is allowed to carry therefore names an agent
/// herdr will accept, with nothing spare. Both counts agree here: a task id and
/// a step id are ASCII (see [`crate::config::check_id`]), so the wire name's
/// bytes are its characters, and the only multi-byte character in the lane name
/// is the `·` this arithmetic removes.
///
/// Raising it means raising what herdr accepts first. There is no spelling that
/// buys more room: at anything above this, a task queues and then fails to
/// start a lane mid-pipeline, which is the failure the queue-time check exists
/// to prevent.
pub const LANE_NAME_MAX: usize = AGENT_NAME_MAX + AGENT_NAME_SAVING;

/// Is `id` usable as a task id for a pipeline whose longest step is
/// `longest_step`? Returns the reason it is not, so the caller can say which of
/// the two constraints was missed.
pub fn check_task_id(id: &str, longest_step: &str) -> Result<()> {
    // The character rule is not the multiplexer's alone — it is what keeps every
    // id a single path component — so it is stated once, in
    // [`crate::config::check_id`], and applied here as well as when a task file
    // is read. What is only true here is the length, below.
    crate::config::check_id("task id", id)?;

    let longest = lane_name(longest_step, id).len();
    if longest > LANE_NAME_MAX {
        bail!(
            "task id `{id}` is {} characters too long: its lane at the `{longest_step}` step \
             would be `{}`, and a lane name stops at {LANE_NAME_MAX}",
            longest - LANE_NAME_MAX,
            lane_name(longest_step, id)
        );
    }
    Ok(())
}

/// Split a lane name back into its step and task, if it is one of ours.
///
/// `" · "` is unambiguous as the separator: a task id and a step id are both
/// checked by [`crate::config::check_id`] to hold nothing but lowercase
/// letters, digits and hyphens, so neither can ever contain it.
pub fn parse_lane_name<'a>(name: &'a str, steps: &[&str]) -> Option<(&'a str, &'a str)> {
    let (task, step) = name.split_once(LANE_NAME_SEPARATOR)?;
    if task.is_empty() || !steps.contains(&step) {
        return None;
    }
    Some((step, task))
}

/// The task id half of a `<task> · <step>` name — everything before the
/// separator, or the whole string when there is none.
///
/// Unlike [`parse_lane_name`] this needs no list of valid steps: the callers
/// that reach for it — [`crate::retain`]'s sweep and
/// [`crate::tracking::failure_count`] — only want to know *which task* a
/// scratch directory, headless record or hook run file belongs to, and do
/// not care whether the step half is one a pipeline still defines. A scratch
/// entry carries no separator at all and is named for its task outright, so
/// the whole name is the id there. A task id holds no spaces (see
/// [`crate::config::check_id`]), so `" · "` stays unambiguous.
pub fn lane_task(name: &str) -> &str {
    name.split_once(LANE_NAME_SEPARATOR)
        .map_or(name, |(task, _)| task)
}

/// What spoolway writes between a lane's two halves, everywhere except the
/// wire — see [`AGENT_NAME_SEPARATOR`] for what herdr gets instead.
///
/// Named rather than spelled out at each use, because [`LANE_NAME_MAX`] does
/// arithmetic on its length and a literal there would be a number nobody could
/// check.
const LANE_NAME_SEPARATOR: &str = " · ";

/// The label a lane's own name is built from: task first, since a lane's pane
/// sits in its project's tab, where the task is what tells one apart from
/// another and the step is what changes as it moves.
pub fn tab_label(task: &str, step: &str) -> String {
    format!("{task}{LANE_NAME_SEPARATOR}{step}")
}

/// What herdr writes between a lane's two halves, in place of [`lane_name`]'s
/// own separator.
///
/// herdr takes an agent name of a lowercase letter followed by lowercase
/// letters, digits, `-` or `_`, and refuses anything else outright — `agent
/// start` on a `<task> · <step>` answers `invalid_agent_name`, which is a lane
/// that never starts. So the separator is spelled differently on the wire and
/// nowhere else: spoolway's own name stays `<task> · <step>` in every file
/// path, every message and every command a person types.
///
/// `__` and not `-`, because the translation has to come back: both halves may
/// hold hyphens, and neither can hold an underscore — [`crate::config::check_id`]
/// allows lowercase letters, digits and hyphens and nothing else — so this is
/// the one spelling that maps both ways without knowing the pipeline's steps.
/// It is also shorter than what it replaces, so [`LANE_NAME_MAX`] still bounds
/// it.
const AGENT_NAME_SEPARATOR: &str = "__";

/// A lane's name as herdr will accept it — see [`AGENT_NAME_SEPARATOR`].
fn to_agent_name(lane: &str) -> String {
    lane.replace(LANE_NAME_SEPARATOR, AGENT_NAME_SEPARATOR)
}

/// And back, for a name read out of `agent list`. A name with no separator in
/// it is returned untouched: it is somebody else's session, and
/// [`parse_lane_name`] is what refuses it.
fn from_agent_name(name: &str) -> String {
    name.replace(AGENT_NAME_SEPARATOR, LANE_NAME_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from `herdr pane layout --pane w5S:p19` against herdr as it
    /// ships: the rects live under `layout.panes[].rect`, and a reader that
    /// expects them flat parses nothing at all — which is how a tab that
    /// exists came to be reported as one this multiplexer had never heard of.
    #[test]
    fn the_biggest_pane_is_read_out_of_a_real_layout_payload() {
        let payload = r#"{
          "layout": {
            "area": {"height": 50, "width": 173, "x": 36, "y": 1},
            "focused_pane_id": "w5S:p1E",
            "panes": [
              {"focused": false, "pane_id": "w5S:p19",
               "rect": {"height": 25, "width": 89, "x": 36, "y": 1}},
              {"focused": true, "pane_id": "w5S:p1E",
               "rect": {"height": 25, "width": 89, "x": 36, "y": 26}},
              {"focused": false, "pane_id": "w5S:p1C",
               "rect": {"height": 50, "width": 84, "x": 125, "y": 1}}
            ],
            "splits": [],
            "tab_id": "w5S:t6",
            "workspace_id": "w5S",
            "zoomed": false
          },
          "type": "pane_layout"
        }"#;
        let layout: PaneLayout = serde_json::from_str(payload).expect("a live pane layout parses");
        let chosen = choose_split(layout.layout, &HashSet::new())
            .expect("a tab with panes has one to split");
        // 84 × 50 = 4,200 beats both 89 × 25 = 2,225 halves. Halving the
        // chosen pane along rows leaves 25 on each side, at or above the
        // 20-row floor, so it is halved top/bottom rather than left/right —
        // not "wider than tall" the way comparing width against height
        // directly used to read it, since a terminal cell is roughly twice
        // as tall as it is wide.
        assert_eq!(chosen.pane_id, "w5S:p1C");
        assert_eq!(chosen.direction, "down");
    }

    /// The failure this check exists for: a `.pane` file once held `w8:p4`
    /// while herdr had `w8:p5`. `pane list` is the multiplexer's own account,
    /// so a pane split's reply is trusted only once it agrees with it.
    #[test]
    fn a_split_reply_is_confirmed_against_the_multiplexers_own_pane_list() {
        let before: HashSet<String> = ["w8:p1".to_string()].into_iter().collect();

        let after: PaneList = serde_json::from_str(
            r#"{"panes": [
                {"pane_id": "w8:p1", "tab_id": "w8:t1"},
                {"pane_id": "w8:p5", "tab_id": "w8:t1"},
                {"pane_id": "w8:p9", "tab_id": "w8:t2"}
            ]}"#,
        )
        .expect("a live pane list parses");
        // Filtered by tab first — `w8:p9` belongs to a different tab and
        // must never count as this split's new pane, whatever appeared
        // there in the meantime.
        let after = pane_ids_in_tab(&after, "w8:t1");

        assert_eq!(
            confirm_split(&before, &after, "w8:p5").unwrap(),
            "w8:p5",
            "the pane that appeared agrees with the reply, so it is confirmed"
        );
    }

    /// The failure the check above exists for: herdr's own reply named
    /// `w8:p4`, but the pane that actually appeared in the tab is `w8:p5` —
    /// `confirm_split` has to notice the two disagree rather than recording
    /// the reply outright.
    #[test]
    fn a_disagreeing_reply_is_refused_rather_than_recorded() {
        let before: HashSet<String> = ["w8:p1".to_string()].into_iter().collect();
        let after: HashSet<String> = ["w8:p1".to_string(), "w8:p5".to_string()]
            .into_iter()
            .collect();

        let err = confirm_split(&before, &after, "w8:p4").unwrap_err();
        assert!(
            format!("{err:#}").contains("w8:p5"),
            "the error names the pane that actually appeared: {err:#}"
        );
    }

    /// The split's reply named a pane, but `pane list` shows nothing new in
    /// the tab at all — herdr answered, and the multiplexer's own account
    /// disagrees about whether anything happened.
    #[test]
    fn no_new_pane_is_refused_rather_than_guessed_at() {
        let before: HashSet<String> = ["w8:p1".to_string()].into_iter().collect();
        let after = before.clone();

        assert!(confirm_split(&before, &after, "w8:p4").is_err());
    }

    /// Two panes appeared between the two readings — a person split the
    /// same tab by hand in the moment between them, say — and there is no
    /// safe way to guess which one this split actually made.
    #[test]
    fn more_than_one_new_pane_is_refused_rather_than_guessed_at() {
        let before: HashSet<String> = ["w8:p1".to_string()].into_iter().collect();
        let after: HashSet<String> = [
            "w8:p1".to_string(),
            "w8:p5".to_string(),
            "w8:p6".to_string(),
        ]
        .into_iter()
        .collect();

        assert!(confirm_split(&before, &after, "w8:p5").is_err());
    }

    /// The bug the task's own context names: a project whose `.spoolway/`
    /// sits inside a linked worktree finds that worktree, not the main
    /// checkout, when [`crate::repo::Repo::root`]'s own ancestor search runs
    /// — and handing that straight to herdr's `worktree open --cwd` gets
    /// refused with `linked_worktree_source`. `Herdr::new` has to resolve
    /// past it on its own; real git, since this is exactly the case a
    /// hand-rolled `.git`-ancestor walk gets wrong.
    #[test]
    fn herdr_resolves_its_project_root_to_the_main_checkout() {
        let base = crate::scratch::root("mux-test-project-root");
        let _ = std::fs::remove_dir_all(&base);
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &Path, args: &[&str]| crate::repo::run(dir, "git", args).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "t"]);
        std::fs::write(work.join("README"), "hi\n").unwrap();
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "root"]);

        let wt = base.join("wt");
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/a",
                wt.to_str().unwrap(),
            ],
        );

        let config = crate::config::DispatchConfig::default();
        let ordinary = Herdr::new(&work, &config);
        assert_eq!(
            ordinary.project_root.canonicalize().unwrap(),
            work.canonicalize().unwrap(),
            "the main checkout resolves to itself"
        );

        let from_worktree = Herdr::new(&wt, &config);
        assert_eq!(
            from_worktree.project_root.canonicalize().unwrap(),
            work.canonicalize().unwrap(),
            "a linked worktree resolves to the main checkout, not itself"
        );

        git(
            &work,
            &["worktree", "remove", "--force", wt.to_str().unwrap()],
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// The file [`Mux::run_in_pane`] and [`Herdr::start_lane`] hand a pane
    /// instead of typing the environment in directly: written where it says,
    /// and answering with a `.` command short enough that herdr never has a
    /// value long enough to cut mid-quote.
    #[test]
    fn hand_environment_writes_the_file_and_answers_a_short_source_command() {
        let base = crate::scratch::root("mux-test-hand-environment");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // `hand_environment` writes under `project_home`, which reads off
        // this thread's own home — see [`crate::platform::test_home`] — so
        // this stands in for `~` rather than actually writing there.
        crate::platform::test_home::with_home(&base, || {
            let config = crate::config::DispatchConfig::default();
            let herdr = Herdr::new(&base, &config);
            let sh = crate::platform::Shell::CURRENT;
            let env = BTreeMap::from([
                ("SPOOLWAY_TASK".to_string(), "demo".to_string()),
                ("SPOOLWAY_STEP".to_string(), "implement".to_string()),
            ]);

            let source = herdr
                .hand_environment("system-prompts", "demo · implement", &env)
                .unwrap();

            // Named with the dialect's own extension, not a plain `.env` —
            // PowerShell refuses to dot-source anything else.
            let path = project_home(&base)
                .join("system-prompts")
                .join(format!("demo · implement.{}", sh.source_extension()));
            assert_eq!(
                source,
                sh.source_command(&path),
                "the pane is told to source exactly the file that was written"
            );
            let written = std::fs::read_to_string(&path).unwrap();
            assert_eq!(
                written,
                format!("{}\n", sh.env_export_lines(&env)),
                "one assignment per line, as the mockup draws it — not env_export's \
                 single-line pane form, which nothing here needs to race against"
            );

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "the file carries this process's own secrets");
            }
        });

        std::fs::remove_dir_all(&base).ok();
    }

    /// A full-screen tab's first split: one pane, 173x50, halves to two
    /// 173x25 rows since 25 still clears the floor. This is the mockup's
    /// first drawing.
    #[test]
    fn a_full_tab_splits_down_on_its_first_split() {
        let layout: RawLayout = serde_json::from_str(
            r#"{"panes": [{"pane_id": "w6A:p1", "rect": {"height": 50, "width": 173}}]}"#,
        )
        .expect("a layout with one pane parses");
        let chosen = choose_split(layout, &HashSet::new()).expect("one pane is one to split");
        assert_eq!(chosen.pane_id, "w6A:p1");
        assert_eq!(chosen.direction, "down");
    }

    /// A pane already only 25 rows tall halves to 12 on each side, below the
    /// floor, so the split falls back to `right` instead of shrinking it
    /// further. The mockup's second split, against the idle shell pane.
    #[test]
    fn a_pane_too_short_to_halve_again_splits_right() {
        let layout: RawLayout = serde_json::from_str(
            r#"{"panes": [{"pane_id": "w6A:p1", "rect": {"height": 25, "width": 86}}]}"#,
        )
        .expect("a layout with one pane parses");
        let chosen = choose_split(layout, &HashSet::new()).expect("one pane is one to split");
        assert_eq!(chosen.pane_id, "w6A:p1");
        assert_eq!(chosen.direction, "right");
    }

    /// The idle shell is smaller than the lane pane but is the only one with
    /// no agent in it, so it is the one that gives up its space rather than
    /// the running lane.
    #[test]
    fn the_agentless_pane_is_chosen_over_a_bigger_one_holding_an_agent() {
        let layout: RawLayout = serde_json::from_str(
            r#"{"panes": [
              {"pane_id": "w6A:shell", "rect": {"height": 25, "width": 86}},
              {"pane_id": "w6A:lane",  "rect": {"height": 25, "width": 173}}
            ]}"#,
        )
        .expect("a layout with two panes parses");
        let agent_panes: HashSet<String> = ["w6A:lane".to_string()].into_iter().collect();
        let chosen = choose_split(layout, &agent_panes).expect("an agentless pane is available");
        assert_eq!(chosen.pane_id, "w6A:shell");
    }

    /// Every pane holds an agent, so there is no agentless pane to prefer —
    /// the choice falls back to the biggest pane overall, same as before the
    /// agent-aware rule existed.
    #[test]
    fn the_biggest_pane_is_chosen_when_every_pane_holds_an_agent() {
        let layout: RawLayout = serde_json::from_str(
            r#"{"panes": [
              {"pane_id": "w6A:small", "rect": {"height": 25, "width": 86}},
              {"pane_id": "w6A:big",   "rect": {"height": 50, "width": 173}}
            ]}"#,
        )
        .expect("a layout with two panes parses");
        let agent_panes: HashSet<String> = ["w6A:small".to_string(), "w6A:big".to_string()]
            .into_iter()
            .collect();
        let chosen = choose_split(layout, &agent_panes)
            .expect("every pane holding an agent still leaves one to split");
        assert_eq!(chosen.pane_id, "w6A:big");
    }

    /// The other half of the same drift: `pane layout` is addressed by pane,
    /// so a tab id has to be turned into one of its panes first.
    #[test]
    fn a_pane_in_the_tab_is_found_by_its_tab_id() {
        let payload = r#"{"panes": [
          {"pane_id": "w5S:p19", "tab_id": "w5S:t6", "workspace_id": "w5S"},
          {"pane_id": "w65:p2",  "tab_id": "w65:t2", "workspace_id": "w65"}
        ]}"#;
        let list: PaneList = serde_json::from_str(payload).expect("a live pane list parses");
        let found = list
            .panes
            .into_iter()
            .find(|p| p.tab_id.as_deref() == Some("w65:t2"))
            .expect("the tab has a pane");
        assert_eq!(found.pane_id, "w65:p2");
    }

    /// A project's tab is found by its label alone, read against the same
    /// `tab list` payload herdr actually answers with — no pane, and no
    /// directory, involved.
    ///
    /// Before `pane-per-task` this also had to pick the anchor's tab out from
    /// a same-named one belonging to a different project, by the directory
    /// its anchor stood in. That guard is gone along with the anchor: a
    /// second checkout claiming a project basename already pointed at
    /// another root is refused at `init` time, so a label collision between
    /// two different projects cannot happen on one machine to begin with.
    #[test]
    fn a_projects_tab_is_found_by_its_label_alone() {
        let tabs: TabList = serde_json::from_str(
            r#"{"tabs": [
              {"tab_id": "w66:t2", "label": "otherapp", "workspace_id": "w66"},
              {"tab_id": "w66:t9", "label": "spoolway",  "workspace_id": "w66"}
            ]}"#,
        )
        .expect("a live tab list parses");

        assert_eq!(find_tab_id(tabs, "spoolway"), Some("w66:t9".to_string()));
    }

    /// A label nothing carries is a tab to open, not one to guess at: a
    /// project whose tab is not there yet must come back as nothing at all.
    #[test]
    fn a_tab_nobody_has_opened_is_not_found() {
        let tabs: TabList = serde_json::from_str(
            r#"{"tabs": [{"tab_id": "w66:t2", "label": "otherapp", "workspace_id": "w66"}]}"#,
        )
        .unwrap();
        assert_eq!(find_tab_id(tabs, "spoolway"), None);
    }

    /// herdr refuses an agent name holding anything but a lowercase letter
    /// first and lowercase letters, digits, `-` or `_` after it, up to 32
    /// characters — `agent start "demo · plan"` answers `invalid_agent_name`
    /// and the lane never starts. So the name goes over the wire spelled
    /// differently, and has to come back.
    #[test]
    fn a_lane_name_crosses_the_wire_and_comes_back() {
        let lane = lane_name("pr-review", "add-health-endpoint");
        let wire = to_agent_name(&lane);
        assert_eq!(wire, "add-health-endpoint__pr-review");
        assert_eq!(from_agent_name(&wire), lane);

        // Both halves may hold hyphens and neither may hold an underscore, so
        // the round trip needs no knowledge of the pipeline's steps.
        assert_eq!(
            parse_lane_name(&from_agent_name(&wire), &["pr-review"]),
            Some(("pr-review", "add-health-endpoint"))
        );

        assert!(wire.chars().count() <= AGENT_NAME_MAX);
        let mut chars = wire.chars();
        assert!(chars.next().is_some_and(|c| c.is_ascii_lowercase()));
        assert!(
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "`{wire}` holds something herdr refuses"
        );
    }

    /// A person's own session in the same multiplexer has no separator in it,
    /// so it crosses back untouched — and is refused by `parse_lane_name`,
    /// which is the only thing that decides what is ours.
    #[test]
    fn someone_elses_agent_name_survives_the_trip_unchanged() {
        assert_eq!(from_agent_name("my-scratch"), "my-scratch");
        assert_eq!(parse_lane_name("my-scratch", &["implement"]), None);
    }

    #[test]
    fn done_is_a_settled_state_not_an_exited_one() {
        assert!(LaneStatus::Done.is_settled());
        assert!(LaneStatus::Idle.is_settled());
        assert!(!LaneStatus::Done.is_busy());
        assert!(LaneStatus::Working.is_busy());
        assert!(LaneStatus::Blocked.is_busy());
    }

    #[test]
    fn lane_names_round_trip_with_hyphenated_ids() {
        let steps = ["implement", "pr", "e2e"];
        let name = lane_name("implement", "add-health-endpoint");
        assert_eq!(name, "add-health-endpoint · implement");
        assert_eq!(
            parse_lane_name(&name, &steps),
            Some(("implement", "add-health-endpoint"))
        );
    }

    #[test]
    fn a_hyphenated_step_id_still_round_trips() {
        // Splitting on `" · "` rather than matching a step prefix means a
        // step id holding hyphens needs no special casing at all.
        let steps = ["pr", "pr-review"];
        let name = lane_name("pr-review", "thing");
        assert_eq!(name, "thing · pr-review");
        assert_eq!(parse_lane_name(&name, &steps), Some(("pr-review", "thing")));
    }

    #[test]
    fn someone_elses_session_is_not_a_lane() {
        let steps = ["implement", "review"];
        assert_eq!(parse_lane_name("my-scratch-session", &steps), None);
        assert_eq!(parse_lane_name("implement", &steps), None);
        assert_eq!(parse_lane_name("implement-add-auth", &steps), None);
        // A separator with no known step on the far side of it, or nothing
        // ahead of it, is not a lane either.
        assert_eq!(parse_lane_name("add-auth · unknown-step", &steps), None);
        assert_eq!(parse_lane_name(" · implement", &steps), None);
    }

    /// The bound exists to keep every id herdr will ever be handed inside its
    /// own 1–32 character rule. So the longest id the queue accepts must still
    /// name an agent herdr takes — with nothing to spare, or the bound is
    /// costing ids room for no reason.
    #[test]
    fn the_longest_id_the_queue_accepts_still_names_an_agent_herdr_takes() {
        for step in ["pr", "implement", "review", "look", "blocked"] {
            let longest = "a".repeat(LANE_NAME_MAX - lane_name(step, "").len());
            assert!(check_task_id(&longest, step).is_ok());

            let wire = to_agent_name(&lane_name(step, &longest));
            assert_eq!(
                wire.chars().count(),
                AGENT_NAME_MAX,
                "`{wire}` should sit exactly on herdr's cap"
            );
        }
    }

    #[test]
    fn a_task_id_is_refused_when_its_lane_could_never_be_named() {
        assert!(check_task_id("slug-subcommand", "implement").is_ok());
        // One character past the limit, once its lane at `implement` is built.
        let over = "a".repeat(LANE_NAME_MAX - lane_name("implement", "").len() + 1);
        assert!(check_task_id(&over, "implement").is_err());
        // The same id is fine on a pipeline whose longest agent step is shorter.
        let short_step = "a".repeat(LANE_NAME_MAX - lane_name("pr", "").len());
        assert!(check_task_id(&short_step, "pr").is_ok());
        assert!(check_task_id("Capitalised", "implement").is_err());
        assert!(check_task_id("3rd-task", "implement").is_err());
        assert!(check_task_id("has_underscore", "implement").is_err());
        assert!(check_task_id("", "implement").is_err());
    }

    /// The `code` a stalled submission is told apart by has to come from the
    /// envelope's own field, not from matching text inside a formatted error
    /// — and has to survive an envelope that carries no `code` at all, which
    /// every error herdr writes for reasons other than a stall does.
    #[test]
    fn envelope_error_code_is_read_from_the_field_not_matched_as_text() {
        let stalled: Envelope<serde_json::Value> = serde_json::from_str(
            r#"{"error":{"code":"agent_prompt_stalled","message":"submission stalled"}}"#,
        )
        .unwrap();
        assert_eq!(
            stalled.error.unwrap().code.as_deref(),
            Some("agent_prompt_stalled")
        );

        let no_code: Envelope<serde_json::Value> =
            serde_json::from_str(r#"{"error":{"message":"pane not found"}}"#).unwrap();
        assert_eq!(no_code.error.unwrap().code, None);
    }

    /// The point of naming the path at all: one directory holds every worktree
    /// spoolway cut, each named after its branch slug — see [`branch_slug`],
    /// the rule every backend shares. Nested under the project's own home,
    /// beside its queue and archive, never under `~/.herdr`, which is the
    /// multiplexer's own directory.
    #[test]
    fn every_task_worktree_lands_under_the_projects_own_home() {
        let root = home().join("dev").join("spoolway");
        let path = worktree_root(&root, &DispatchConfig::default()).join("session-key");
        assert!(
            path.ends_with(Path::new("worktrees/session-key")),
            "{path:?}"
        );
        assert!(path.starts_with(project_home(&root)), "{path:?}");
        assert!(!path.starts_with(home().join(".herdr")), "{path:?}");
    }

    /// A branch is a path with a `/` in it, and every backend flattens it to
    /// one directory component the same way — so a tracker slug on the branch
    /// (`task/proj-12-add-endpoint`) rides into the worktree directory name
    /// without any backend doing anything special.
    #[test]
    fn a_branch_becomes_one_directory() {
        assert_eq!(branch_slug("task/add-endpoint"), "task-add-endpoint");
        assert_eq!(
            branch_slug("task/proj-12-add-endpoint"),
            "task-proj-12-add-endpoint"
        );
        assert_eq!(branch_slug("plan/a/b"), "plan-a-b");
    }

    /// A backend that overrides nothing has no gesture to try, so
    /// `Mux::vacate_lane`'s own default is the only thing under test here: it
    /// has to fall back to exactly what `Mux::stop_lane` already does — close
    /// the pane — and say so truthfully, rather than claim the pane came back
    /// to a shell when nothing made that happen.
    struct BareMux {
        closed: RefCell<Vec<String>>,
    }

    impl Mux for BareMux {
        fn name(&self) -> &'static str {
            "bare"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable(&self) -> String {
            String::new()
        }
        fn resident_while_waiting(&self) -> bool {
            true
        }
        fn list_lanes(&self) -> Result<Vec<Lane>> {
            Ok(Vec::new())
        }
        fn create_workspace(
            &self,
            _cwd: &Path,
            _branch: &str,
            _base: &str,
            _label: &str,
        ) -> Result<Workspace> {
            unimplemented!()
        }
        fn remove_workspace(&self, _workspace_id: &str) -> Result<()> {
            Ok(())
        }
        fn close_workspace(&self, _workspace_id: &str) -> Result<()> {
            Ok(())
        }
        fn close_tab(&self, _tab_id: &str) -> Result<()> {
            Ok(())
        }
        fn create_pane(&self, _cwd: &Path, _label: &str) -> Result<Workspace> {
            unimplemented!()
        }
        fn split_pane(&self, _tab_id: &str, _cwd: &Path) -> Result<String> {
            unimplemented!()
        }
        fn close_pane(&self, pane_id: &str) -> Result<()> {
            self.closed.borrow_mut().push(pane_id.to_string());
            Ok(())
        }
        fn start_lane(&self, _spec: &LaneSpec<'_>) -> Result<()> {
            unimplemented!()
        }
        fn prompt(&self, _name: &str, _text: &str) -> Result<()> {
            unimplemented!()
        }
        fn read(&self, _name: &str, _lines: usize) -> Result<String> {
            unimplemented!()
        }
        fn interrupt_lane(&self, _name: &str) -> Result<()> {
            unimplemented!()
        }
        fn stop_lane(&self, _name: &str, pane_id: &str) -> Result<()> {
            self.close_pane(pane_id)
        }
        fn focus_lane(&self, _name: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_tab(&self, _tab_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_workspace(&self, _workspace_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_pane(&self, _pane_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
    }

    #[test]
    fn a_backend_with_no_gesture_still_closes_the_pane_when_asked_to_vacate() {
        let mux = BareMux {
            closed: RefCell::new(Vec::new()),
        };

        let left = mux
            .vacate_lane("demo · implement", "no-such-kind", "pane-1")
            .expect("the default falls back to closing the pane, which never fails here");

        assert_eq!(
            left,
            Vacated::PaneClosed,
            "nothing typed a gesture into the pane, so it cannot have come back to a shell"
        );
        assert_eq!(
            mux.closed.borrow().as_slice(),
            ["pane-1"],
            "the default has to close the pane exactly as `stop_lane` does"
        );
    }

    /// The gesture lives on the *kind*, but sending it is the backend's job,
    /// so a backend that overrides nothing must ignore the kind's row
    /// entirely rather than half-honour it. `claude` is the one kind that
    /// carries a gesture, and here it has to end up in exactly the same place
    /// a kind with no row does: pane closed, and said so.
    #[test]
    fn a_kind_with_a_gesture_gains_nothing_from_a_backend_that_cannot_send_it() {
        let mux = BareMux {
            closed: RefCell::new(Vec::new()),
        };

        let left = mux
            .vacate_lane("demo · implement", "claude", "pane-1")
            .expect("the default falls back to closing the pane, which never fails here");

        assert_eq!(
            left,
            Vacated::PaneClosed,
            "a backend with nothing to type at cannot use a gesture, whatever kind carries one"
        );
        assert_eq!(mux.closed.borrow().as_slice(), ["pane-1"]);
    }

    /// Fixed, and the same for every project: two projects dispatching at
    /// once share one row in the sidebar rather than opening one each.
    #[test]
    fn the_dispatch_workspace_is_shared_by_every_project() {
        assert_eq!(
            dispatch_workspace_label(Path::new("/home/x/dev/spoolway")),
            "spoolway-dispatcher"
        );
        assert_eq!(
            dispatch_workspace_label(Path::new("/home/x/dev/some-other-app")),
            "spoolway-dispatcher"
        );
    }

    /// The call that binds a task's checkout to its project: `--cwd` is the
    /// project root, never the checkout, because that is what herdr resolves
    /// the repository from — and `--path` is the checkout itself.
    #[test]
    fn worktree_open_binds_the_checkout_under_the_project_root() {
        assert_eq!(
            worktree_open_argv(
                "/home/dev/spoolway",
                "/home/dev/spoolway/.worktrees/demo",
                "spoolway/demo"
            ),
            vec![
                "worktree",
                "open",
                "--cwd",
                "/home/dev/spoolway",
                "--path",
                "/home/dev/spoolway/.worktrees/demo",
                "--label",
                "spoolway/demo",
                "--no-focus",
            ]
        );
    }

    /// `grouped` puts every task in a pane of its project's shared tab, so
    /// nothing may tear that tab or workspace down on a task's behalf;
    /// `split` gives each task a workspace of its own, which is the task's to
    /// remove.
    #[test]
    fn only_a_task_with_a_workspace_of_its_own_owns_one() {
        let mut config = DispatchConfig::default();
        let root = Path::new("/repo");

        config.herdr_mode = MuxMode::Grouped;
        assert!(!Herdr::new(root, &config).task_owns_workspace());

        config.herdr_mode = MuxMode::Split;
        assert!(Herdr::new(root, &config).task_owns_workspace());
    }
}
