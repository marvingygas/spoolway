//! The live board bare `spoolway` draws on its dispatch tab, which runs no
//! pass itself and draws what the dispatcher it started writes (see
//! [`Phase::Watching`]). `spoolway dispatch` prints a line per pass instead
//! of drawing a board of its own.
//!
//! A renderer, not a participant. Every frame is read from the same two sources
//! of truth a pass reconciles from — the task files and the live lane list —
//! plus the usage ledger for what the run has spent. It writes nothing and
//! decides nothing: the board and the plain run take exactly the same
//! decisions in the same order.
//!
//! Split across three files: this one holds the board's data model — the
//! `Board` and `Row` themselves, their state transitions and key handling,
//! and everything that reads task files, the lane list and the ledger into
//! rows. [`view`] holds the pure rendering that turns that data into
//! terminal output — the table, the footer, the ticker, the masthead — and
//! nothing in it reads a task file or a lane list of its own.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};

use crate::graph::Graph;
use crate::pipeline::{Outcome, Pipelines};
use crate::repo::Repo;

#[cfg(test)]
pub mod testutil;
mod view;

pub(crate) use view::human_secs;
pub use view::{banner, plain_table};

// Re-exported, not just imported: `screen::key_hint` reads the same dim and
// gutter the board paints its own key line with, rather than spelling either
// out a second time — see `screen::key_hint` — and bare `spoolway`'s tab strip
// the same bold the wordmark is drawn in — see `screen::shell::strip_line`.
use view::{
    AMBER, Cause, Move, RecentEvent, Reported, Style, board_header, board_masthead, boxed,
    clamp_rows, first_name, footer, greeting, greeting_screen, group_totals, pane_height,
    pane_width, pause_confirm_panel, restart_confirm_panel, resume_picker_panel, spool_frame,
    table_laid_out, table_window, ticker, tint_available, unqueue_all_confirm_panel,
    unqueue_confirm_panel,
};
pub(crate) use view::{BOLD, DIM, GUTTER, RESET, strip_ansi};

/// The redraw rate every screen's own wait polls stdin at — the jobs screen,
/// the queue screen, and the dispatcher board, whose wait slices its whole
/// interval into stretches of this on every target. The board's wait also
/// watches the commands directory (see `crate::screen::DirWatch`), which
/// ends a slice early; it never lengthens one.
///
/// One second, not two, because it is also the rate the lockup's mark is
/// sampled at — see [`view::spool_frame`], which turns on every whole second. A
/// redraw slower than that turn would alias it: the screen would sample the
/// same phase every time and the mark would sit still while the run moved.
pub const POLL: Duration = Duration::from_secs(1);

/// How long the board keeps calling a task `Running` after its stage changes
/// with no lane up for it yet — the ordinary gap between `spoolway report`
/// writing the new stage and the dispatcher's next pass starting a lane
/// there. Past this, the row falls back to reading `Queued`, honestly.
///
/// Twenty seconds: two passes at the ten-second `dispatch.interval` a
/// project shipped with before this task removed it — the same figure
/// `crate::dispatch::PROBE_INTERVAL` fixes the poll rate at now, doubled. A
/// named constant of its own rather than the poll rate doubled at read time,
/// on purpose: the two agreeing today is a coincidence of the numbers this
/// task chose, not a fact about what this grace is for, so it must not start
/// moving with the poll rate if that ever changes.
pub(crate) const HANDOFF_GRACE: Duration = Duration::from_secs(20);

/// How often a live session's transcript may be re-read for the CTX column.
///
/// Ten times the redraw interval, because the two are paid for very
/// differently: a frame is a handful of small file reads, and a transcript is
/// the whole conversation, which runs to megabytes on a long session. The
/// figure is therefore up to this stale, which at a one-second redraw nobody
/// can see — a context window does not move fast enough for ten seconds to be
/// the difference between reading it and acting on it.
const CTX_REFRESH: Duration = Duration::from_secs(10);

/// The group a task with no `group:` falls into, and the one group whose
/// position is fixed rather than alphabetical. Never gets a band or a
/// closing total line — there is no group for the ledger to have grouped by.
const NO_GROUP: &str = "no group";

/// Transitions kept in the RECENT ticker.
const RECENT: usize = 6;

/// What one task is doing, reduced to the thing the board colors by.
#[derive(Clone, Copy, PartialEq)]
pub enum State {
    /// The task's own stage is `paused` — nothing else reaches this state
    /// any more. A person is the one thing between this task and the rest of
    /// its pipeline, and nothing went wrong: a gated step finished and is
    /// waiting to be let past. Next to `blocked` rather than inside it,
    /// since a paused step passed and a blocked one did not.
    Paused,
    /// Something is working the task right now: a live lane, or the run of a
    /// command step, which has no lane at all.
    Running,
    /// The dispatcher has claimed a slot for the task and is booting its
    /// lane, which herdr does not list until the boot is well along — read
    /// off the dispatcher's boot mark, see [`crate::claim`]. The row names
    /// the step being booted, which is not yet the task's own stage when
    /// it is coming off `queued`.
    Starting,
    /// A step whose agent has settled its turn, or whose command run has
    /// exited, while no dispatcher holds the lock — so nothing will route it
    /// until dispatching starts again. Only [`finish_settled`] reaches this
    /// state: with a dispatcher up, a finished step moves on within a pass
    /// and reads `Running` for that moment, exactly as it always has.
    Finished,
    /// At the pipeline's blocked step, carrying its reason.
    Blocked,
    /// On a stage this task's pipeline does not have, usually a hand edit
    /// of `stage:`. Not `Blocked`: that word is a stop a person clears with
    /// `[r]`, and nothing here offers the key.
    Unknown,
    /// A live lane's pane is holding a permission prompt — herdr's own
    /// read, off the lane list, fresh every redraw. The task's own stage has
    /// not moved and is not `paused`: this is a live turn waiting on a
    /// keystroke in its pane, not a stop, so nothing here is resumable.
    Prompt,
    /// In the queue, waiting for a slot or a dependency.
    Queued,
    /// On a `serial: true` command step another task's run still holds, with
    /// nothing of its own started yet. Apart from `Queued` because what it
    /// waits on is one named run, not a slot or a dependency — the NEXT
    /// column names that task.
    Waiting,
    /// Archived — its pipeline finished and its file moved to the project's
    /// own `archive/`. Kept on the board, dimmed, for as long as its
    /// group still has a task in the queue: see [`rows_for_board`].
    Done,
}

/// What the run is doing at the moment a frame is drawn, which is the one
/// thing on the board that no task file records.
///
/// A board is only ever drawn one way now: bare `spoolway`'s dispatch tab,
/// via [`Board::hosted_frame`]. `spoolway dispatch` prints a line per pass
/// instead of drawing a board of its own, so the phases that once told a
/// live pass, a wait between passes and the last frame of a stopped run
/// apart are gone with it — `Watching` is what is left standing.
#[derive(Clone, Copy)]
pub enum Phase {
    /// A board no dispatcher is drawing: bare `spoolway`'s dispatch tab,
    /// which runs no pass of its own. `holder` is the pid holding the
    /// dispatch lock, if anything does, so the header names the process
    /// actually dispatching — the child the tab started — rather than the
    /// screen's own, and says `dispatcher stopped`, with no pid at all, when
    /// nothing is. `dispatching` is whether the tab has a child up, which is
    /// what its `enter` would ask how to stop rather than start.
    Watching {
        holder: Option<u32>,
        dispatching: bool,
    },
}

pub struct Row {
    pub id: String,
    /// The group this task came from, verbatim. `None` is a task queued
    /// outside any group — see [`NO_GROUP`].
    pub group: Option<String>,
    /// This task's own issue URL, from its `url:` frontmatter. `key-in-names`
    /// owns setting it, off the tracker hook's answer, and stores it per task
    /// — there is no invariant that the tasks of a group all carry it or
    /// agree on it, only that the hook writes the same issue's url onto each
    /// task of a group it opens. `None` when this task has no `url:`. The
    /// board's group band takes the first row of the group that has one as
    /// its hyperlink target — see the `group_urls` map in [`view::table`].
    pub issue_url: Option<String>,
    /// The other group this row's own group stacks on, named on whichever
    /// row is that group's first task — the one whose `depends_on` crosses
    /// into another group's chain rather than staying inside its own. `None`
    /// on every other row, and on a group that stacks on nothing. The
    /// board's band reads the first `Some` any row of a group carries, the
    /// same way it already does for [`Self::issue_url`]'s `group_urls`.
    pub after: Option<String>,
    pub stage: String,
    /// How many times this task has arrived at the step it is on now —
    /// [`crate::task::Frontmatter::arrivals`]'s own entry for it,
    /// [`crate::task::Task::rounds_at`]'s own answer. Whichever route
    /// carried it there each time, and with no budget behind it: a step
    /// reached once is a bare id, since one visit is not yet worth a
    /// reader's notice, and every visit after that draws `↻ <n>` beside it.
    pub arrivals: u32,
    /// The pipeline this task resolves to, by name — what the `PIPELINE`
    /// column draws. A task with no `pipeline:` of its own names its
    /// project's configured default, exactly [`Pipelines::for_task`]'s own
    /// answer; an archived row whose named pipeline has since been deleted
    /// names it verbatim rather than erroring, since nothing here can
    /// resolve it any more.
    pub pipeline: String,
    pub state: State,
    /// How deep this task sits in the run — [`Graph::depth`] of its id. The
    /// first tier `Row::key` sorts a group's live rows by, once whether the
    /// row is done is settled: a dependency this deep below another belongs
    /// above it, whatever either one's state or pipeline says. Zero for an
    /// archived row — see [`done_rows`] — which never matters, since a done
    /// row's own tier already puts it last.
    pub depth: usize,
    /// Steps still ahead of this row in its own pipeline, mirroring
    /// [`crate::dispatch::Candidate::steps_left`] — the tier `Row::key` reads
    /// once two rows of a group tie on depth. Zero for an archived row.
    pub steps_left: i32,
    /// Tasks this one is holding up, transitively — [`Graph::dependents`] of
    /// its id. The last tie-break `Row::key` reads, most first, mirroring the
    /// dispatcher's own last candidate tier. Zero for an archived row.
    pub dependents: usize,
    /// How full the conversation this task's live lane is holding has got, as
    /// a percentage of its model's context window. The same reading
    /// an enabled `session_reuse_ctx` forks a fresh session on, so a row
    /// approaching that ceiling is a row about to lose its session.
    ///
    /// `None` wherever there is no honest answer: no live lane, a model whose
    /// window nothing resolves, or a session that has not taken a turn yet.
    pub ctx: Option<u64>,
    /// Output tokens the step it is on has produced. Read from the live lane's
    /// own transcript while there is one, and from the ledger once there is
    /// not: a step is banked when it settles, by which time the task has moved
    /// to the next one, so a running row asking the ledger would always read
    /// empty. Less whatever the session was already banked for, exactly as the
    /// COST beside it is — see [`live_spend`]. Output alone, of the four
    /// classes, for the reason the footer counts it — it tracks work done
    /// rather than context carried.
    pub out: Option<u64>,
    /// What the step this task is on has cost, read off the live lane's own
    /// transcript while there is one and from the ledger once there is not —
    /// the same two sources, in the same order, as the OUT beside it. The three
    /// figures on a row are then one story: this is what the conversation CTX
    /// measures has spent producing that OUT.
    ///
    /// The step, not the task. A task's whole bill is a different question, and
    /// `spoolway eval --by task` is the thing that answers it; carried here it
    /// meant a fresh session opened at a late step reported the spend of every
    /// session before it, on a row otherwise entirely about the new one.
    ///
    /// `None` where nothing that ran could be priced — a local worker's whole
    /// answer — or where the step has yet to spend anything at all.
    pub cost: Option<f64>,
    /// How long this task's lane has actually been busy: `now - launched_at`
    /// while a lane is live (the round in flight is presumed working, since
    /// nothing banked has settled it yet), and the ledger's summed `wall_s`
    /// — busy time only, banked as a delta the same way tokens are — at the
    /// step it is on once it is not live, every round of it, the same as OUT
    /// and COST. `None` where neither answers: no live lane and nothing
    /// banked. A [`State::Finished`] row holds the figure it read when the
    /// board first saw it finish instead — see [`finish_settled`].
    ///
    /// A paused row reads the ledger at [`ledger_stage`] rather than
    /// `task.stage()`: `paused` itself is not a step any pipeline declares
    /// and so never has a line of its own — the lane that paused it banked
    /// under the step it actually ran, named by `parked_from` or
    /// `paused_at`. Idle, parked and permission-prompt time bank nothing, so
    /// this is why a paused row's own figures stop moving the moment it
    /// lands there — see gh-378 / issue #380.
    pub lane_time: Option<i64>,
    /// What happens to this task next: the step it goes to, or — when it is
    /// stuck — what has to happen before it goes anywhere.
    pub next: String,
    /// Whether the board's resume key does anything on this row: a gated
    /// pass or a block, both waiting on `spoolway resume`, with every
    /// dependency finished and no lane of the task's own
    /// still mid-turn. `false` everywhere else, including a row that merely
    /// looks parked — `queue list` and every other reader ignores it, so
    /// nothing but the board's own key handling ever asks.
    pub resumable: bool,
}

impl Row {
    /// Where this row sorts. Group first and always: the outer order is the
    /// group's name, so a group stays where a reader last found it rather than
    /// moving up the board as its tasks get into trouble.
    ///
    /// Inside a group the row is placed by where its task stands in the run,
    /// not by its state: whether it is done, then depth, then steps left on
    /// its own pipeline, then how many tasks it holds up (most first), then
    /// task id. A row's state changes far more often than its place in the
    /// run does, so keying on the run order rather than the state is what
    /// keeps a row still while its run does one thing — see the module-level
    /// mockup in `docs/dispatcher.md`. `Done` is the one exception: an
    /// archived row is history, so it always sorts last regardless of depth.
    fn key(&self) -> (u8, &str, bool, usize, i32, std::cmp::Reverse<usize>, &str) {
        let done = self.state == State::Done;
        match &self.group {
            Some(group) => (
                0,
                group.as_str(),
                done,
                self.depth,
                self.steps_left,
                std::cmp::Reverse(self.dependents),
                self.id.as_str(),
            ),
            None => (
                1,
                NO_GROUP,
                done,
                self.depth,
                self.steps_left,
                std::cmp::Reverse(self.dependents),
                self.id.as_str(),
            ),
        }
    }

    /// The group block this row belongs under — what its band names and its
    /// total line closes.
    fn group(&self) -> &str {
        self.group.as_deref().unwrap_or(NO_GROUP)
    }
}

/// The board's memory between frames: what it last saw, so it can say what
/// changed, and whatever news the run handed it.
///
/// Held by the dispatch loop, which draws a frame before each pass and then
/// keeps drawing through the wait until the next one is due. There is no
/// separate process and no lock to poll — the thing rendering the run *is* the
/// run, so a frame is up exactly as long as the dispatcher is. The one
/// exception is bare `spoolway`'s dispatch tab, which holds a
/// [`Board::hosted`] of its own and reads the lock only to name who is
/// dispatching.
pub struct Board {
    /// The terminal, taken for as long as the board is up and given back on
    /// `Drop` — the cursor hidden and, on Unix, the tty deaf to whatever gets
    /// typed at it. Held here rather than beside it in the run loop so the
    /// two ways a run ends — the queue emptying and `ctrl-c` — both unwind
    /// through the same `Board` going out of scope and reach the same
    /// restore.
    _term: crate::platform::TermGuard,
    /// The task id the cursor sits on. Starts `None` only until the first
    /// reading lands, which lands it on the first row of the first group
    /// straight away rather than leaving a person's first `↑`/`↓` press go
    /// to discover that row — see [`Board::apply`]. By id rather than a
    /// plain row index, so a state change that resorts the board — a task
    /// passing its step, one landing on `paused` above it — never leaves the
    /// cursor pointing at a different task than the one a person last put it
    /// on. The same seeding re-lands it on the first row whenever the id it
    /// already holds falls out of a later reading's own rows entirely, such
    /// as a group finishing and taking the cursor's row with it — no key
    /// pressed. `None` again only once the board has nothing left to show at
    /// all.
    cursor: Option<String>,
    /// What a `p`, `r`, `s`, `u` or `U` keypress is waiting on, if anything — see
    /// [`BoardMode`]. `Browsing` on every other key, including the plain
    /// cursor moves, which never open a panel at all.
    mode: BoardMode,
    /// The id of every row the last reading composed, in the order it
    /// composed them — the live queue and the archived rows beside it,
    /// exactly as [`paint`] laid them out. A full board draws only a window
    /// of these rows, but the list holds them all, so `↑`/`↓` walk rows
    /// scrolled out of view too and the window follows the cursor. A cursor
    /// move is a step through a list already in memory rather than a fresh
    /// read of every task file: the board reads keys while a pass is rewriting
    /// and archiving those very files, and a read caught mid-write used to
    /// lose the keypress outright. The cost is that the marker can sit for a
    /// reading or two on a row the queue has already moved — the next one
    /// to land puts it right, the same way [`Board::apply`] already re-seeds
    /// a cursor whose row has left the board.
    ///
    /// Empty until the first reading.
    drawn: Vec<String>,
    /// The RECENT ticker's own lines as of the last reading this board has
    /// folded in — what [`paint`] draws under the table. Copied off each new
    /// [`Snapshot`] by [`Board::apply`], so it moves once per reading, the
    /// same as it once moved once per frame.
    recent: VecDeque<RecentEvent>,
    /// The last reading this board has folded into its own memory — compared
    /// by pointer against whatever [`Reader::latest`] or a synchronous
    /// [`build`] hands back, so [`Board::cursor`] and [`Board::drawn`] move
    /// forward exactly once per new reading, never once per frame — see
    /// [`Board::apply`]. `None` until the first.
    current: Option<Arc<Snapshot>>,
    /// The one reading a hosted frame no longer makes on the key thread
    /// itself — see [`Reader`]. `None` until the first hosted frame, which
    /// starts it — see [`Board::reader`]; a board whose own tests only ever call
    /// [`Board::frame`] never needs one at all, and never pays a thread for
    /// it.
    reader: Option<Reader<Snapshot>>,
    /// The first word of git's `user.name`, for the empty board's greeting —
    /// `None` until the first frame reads it, then `Some(None)` when git had
    /// no name to give. Read once per board and kept, so a changed name
    /// shows the next time the board opens. See [`git_first_name`].
    name: Option<Option<String>>,
    /// [`Reader`]'s own memory, kept here instead for a board whose tests
    /// call [`Board::frame`] directly: one synchronous [`build`] per call,
    /// with nothing asynchronous to own a thread of its own.
    #[cfg(test)]
    memory: Memory,
}

impl Board {
    fn with_term(term: crate::platform::TermGuard) -> Board {
        Board {
            _term: term,
            cursor: None,
            mode: BoardMode::Browsing,
            drawn: Vec::new(),
            recent: VecDeque::new(),
            current: None,
            reader: None,
            name: None,
            #[cfg(test)]
            memory: Memory::new(),
        }
    }

    /// A board whose terminal guard is inert — for tests, so parallel `Board`s
    /// do not take the process's real terminal raw and race on restore
    /// (finding 53).
    #[cfg(test)]
    pub fn for_test() -> Board {
        Board::with_term(crate::platform::TermGuard::inert())
    }

    /// A board drawn inside bare `spoolway`'s dispatch tab. Its guard is
    /// inert for the same reason [`Board::for_test`]'s is, from the other
    /// side: the screen already holds the terminal's one guard for every
    /// tab, and a second one restoring on this board's drop would hand the
    /// terminal back while the other tabs are still drawing on it.
    pub(crate) fn hosted() -> Board {
        Board::with_term(crate::platform::TermGuard::inert())
    }

    /// Whether no confirm panel is open — the one state in which `←`, `→`
    /// and `q` belong to the screen around the board rather than to the
    /// panel, which reads every key until it is answered.
    pub(crate) fn at_rest(&self) -> bool {
        matches!(self.mode, BoardMode::Browsing)
    }

    /// One frame for the dispatch tab: [`Board::frame`] under
    /// [`Phase::Watching`], naming whichever process holds the dispatch lock.
    /// A lock file that cannot be read reads as nobody holding it, since
    /// this must never name a live pid it did not see.
    ///
    /// Drawn inside an untitled box with the key line under it — see
    /// `paint` — so the tab reads like the three beside it.
    ///
    /// `popup` is the tab's own — a start gate, or why its dispatcher ended
    /// — drawn over the table the same way the board's confirm panels are.
    /// The board's own panel wins while one is open: the tab opens no popup
    /// of its own until the board is at rest.
    ///
    /// Waits for a reading that started after this call before it draws,
    /// the same full read every frame paid for before [`Reader`] existed.
    /// The dispatch tab asks for this only on the first frame each time it
    /// is entered, so the screen never opens on a reading left over from
    /// before the tab was last left. A reading that fails — the queue
    /// directory unreadable for an instant — is an error here, so the
    /// caller leaves the last frame on screen exactly as it always has.
    /// Every other frame comes from [`Board::hosted_frame_from_memory`].
    pub(crate) fn hosted_frame(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        dispatching: bool,
        popup: Option<&[String]>,
    ) -> Result<String> {
        // A reader started by this very call has just read on this thread —
        // see [`Reader::start`] — so a second reading straight after it
        // would only read the same files again.
        let started_here = self.reader.is_none();
        let reader = self.reader(repo, pipelines);
        let snapshot = if started_here {
            reader.last_reading()?
        } else {
            reader.wake_and_wait()?
        };
        Ok(self.paint_from(repo, pipelines, dispatching, popup, &snapshot))
    }

    /// [`Board::hosted_frame`], drawn from whatever reading [`Reader`] last
    /// landed instead of waiting for a new one, so it starts no process and
    /// reads no file. What a key and the dispatch tab's idle tick draw. It
    /// asks for no reading of its own — the caller wakes the reader through
    /// [`Board::wake_reader`] wherever it wants one, which a redraw for a
    /// reading that has just landed must not.
    pub(crate) fn hosted_frame_from_memory(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        dispatching: bool,
        popup: Option<&[String]>,
    ) -> String {
        let snapshot = self.reader(repo, pipelines).latest();
        self.paint_from(repo, pipelines, dispatching, popup, &snapshot)
    }

    /// Ask the reader for one more reading, without waiting for it. Every
    /// key on the dispatch tab asks, and so does its idle tick once a
    /// second; asks that arrive while a reading is running collapse into
    /// one more reading after it — see [`Reader::wake`]. Does nothing
    /// before the board's first hosted frame has started the reader.
    pub(crate) fn wake_reader(&self) {
        if let Some(reader) = &self.reader {
            reader.wake();
        }
    }

    /// Whether the reader has landed a reading this board has not drawn
    /// yet. The dispatch tab checks this between keys, so a reading a key
    /// asked for is drawn as soon as it lands rather than at the next tick.
    pub(crate) fn reading_landed(&self) -> bool {
        match (&self.reader, &self.current) {
            (Some(reader), Some(current)) => !Arc::ptr_eq(&reader.latest(), current),
            _ => false,
        }
    }

    /// The board's [`Reader`], started on its first hosted frame rather than
    /// in a constructor, so a board whose own tests never host anything
    /// never pays for a thread. Starting it reads once on this thread — see
    /// [`Reader::start`] — so the very first frame is never drawn from an
    /// empty reading.
    fn reader(&mut self, repo: &Repo, pipelines: &Pipelines) -> &Reader<Snapshot> {
        self.reader
            .get_or_insert_with(|| Reader::for_board(repo.clone(), pipelines.clone()))
    }

    /// One hosted frame painted from `snapshot`, after folding it into the
    /// board's memory if it is new — see [`Board::apply`].
    fn paint_from(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        dispatching: bool,
        popup: Option<&[String]>,
        snapshot: &Arc<Snapshot>,
    ) -> String {
        self.apply(snapshot);
        let name = self.greeting_name(repo);
        let frame = paint(
            repo,
            pipelines,
            dispatching,
            snapshot,
            self.cursor.as_deref(),
            &self.recent,
            name.as_deref(),
        );
        self.with_popup(frame, popup)
    }

    /// [`Board::name`], read off git on the first call and kept after.
    fn greeting_name(&mut self, repo: &Repo) -> Option<String> {
        self.name
            .get_or_insert_with(|| git_first_name(repo))
            .clone()
    }

    /// One frame, built whole before anything is written so a slow read never
    /// leaves a half-drawn board on screen.
    ///
    /// Only the tests below call this directly any more — production code
    /// always reaches [`Phase::Watching`] through [`Board::hosted_frame`] or
    /// [`Board::hosted_frame_from_memory`].
    /// Builds synchronously every call, against `phase`'s own `holder`
    /// rather than a real lock file, so a test can drive the "who is
    /// dispatching" display without needing one.
    #[cfg(test)]
    fn frame(&mut self, repo: &Repo, pipelines: &Pipelines, phase: Phase) -> Result<String> {
        self.frame_with(repo, pipelines, phase, None)
    }

    /// [`Board::frame`], with `popup` drawn over it wherever the board has
    /// no panel of its own open — see [`Board::hosted_frame`].
    #[cfg(test)]
    fn frame_with(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        phase: Phase,
        popup: Option<&[String]>,
    ) -> Result<String> {
        let Phase::Watching {
            holder,
            dispatching,
        } = phase;
        let snapshot = Arc::new(build(repo, pipelines, holder, &mut self.memory)?);
        self.apply(&snapshot);
        let name = self.greeting_name(repo);
        let frame = paint(
            repo,
            pipelines,
            dispatching,
            &snapshot,
            self.cursor.as_deref(),
            &self.recent,
            name.as_deref(),
        );
        Ok(self.with_popup(frame, popup))
    }

    /// Folds a new reading into the board's own memory — the cursor and
    /// [`Board::drawn`] — exactly once per reading, never once per frame: a
    /// second call with the very same [`Snapshot`] a key just redrew from is
    /// a no-op, compared by pointer rather than by content, since two
    /// readings over an unchanged queue are allowed to agree down to the
    /// byte without this treating them as the same one.
    fn apply(&mut self, snapshot: &Arc<Snapshot>) {
        if self
            .current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, snapshot))
        {
            return;
        }
        // Lands the cursor on the first row of the first group before the
        // very first reading this board ever draws is shown, rather than
        // leaving a person's first `↑`/`↓` press go to discover it — see
        // [`Board::cursor`]'s own doc comment. Re-seeded, not just seeded
        // once, because a group finishing can carry the row the cursor
        // named off the table between two readings with no key pressed —
        // the same "gone id" case `shift_cursor` already treats as no
        // cursor at all.
        let cursor_still_shown = self
            .cursor
            .as_deref()
            .is_some_and(|id| snapshot.rows.iter().any(|row| row.id == id));
        if !cursor_still_shown {
            self.cursor = snapshot.rows.first().map(|row| row.id.clone());
        }
        // What `↑`/`↓` will walk until the next reading replaces it — see
        // [`Board::drawn`].
        self.drawn = snapshot.rows.iter().map(|row| row.id.clone()).collect();
        self.recent = snapshot.recent.clone();
        self.current = Some(Arc::clone(snapshot));
    }

    /// `frame`, with `popup` drawn over it wherever the board has no panel
    /// of its own open — see [`Board::hosted_frame`].
    fn with_popup(&self, frame: String, popup: Option<&[String]>) -> String {
        match self.mode.panel().or_else(|| popup.map(<[String]>::to_vec)) {
            Some(panel) => {
                // `overlay` reads its target width off line zero and writes
                // each panel row by walking a target row's own characters —
                // right for `spoolway queue`'s own frame, whose every row is
                // one fixed-width pane with no colour under where a picker
                // lands. This board's rows carry colour throughout — the
                // state dot, the dimmed footer, the key hint — and `overlay`
                // counts an escape byte as a column exactly like a visible
                // one, so it writes at the wrong column and can slice a
                // `DIM`/`RESET` pair in two, printing what is left of the
                // code as stray text. A panel carries no colour of its own,
                // so the frame under one loses its for the frame this draws
                // — [`strip_ansi`] — and gets it back the moment the panel
                // closes and the next frame is read fresh.
                //
                // Also opens on a blank spacer line, and has other blank
                // rows through the ticker and the rule below it — a row
                // `overlay` cannot write into at all, since it only ever
                // replaces characters a row already has. Padding every row
                // out to the widest one first, never shorter than
                // `pane_width()`, gives every row the same floor to write
                // onto and never truncates anything that already reached it.
                let mut stripped: Vec<String> = frame.lines().map(strip_ansi).collect();
                // Hosted, the last row is the key line under the board's
                // box — see `paint` — and a panel lands on the box, never
                // on the key line: padded to one width with the box, a key
                // line wider than the terminal would widen every row of
                // the box past the terminal's last column along with it.
                let keys = match crate::screen::shell::hosted() {
                    Some(_) => stripped.pop(),
                    None => None,
                };
                let width = stripped
                    .iter()
                    .map(|line| line.chars().count())
                    .max()
                    .unwrap_or(0)
                    .max(pane_width());
                let mut lines: Vec<String> = stripped
                    .iter()
                    .map(|line| crate::screen::pad_to(line, width))
                    .collect();
                // `overlay` only writes into rows the frame already has,
                // so a panel taller than the frame under it — the
                // dispatch tab's warnings over an empty queue — would
                // lose its bottom rows, key line and all. Blank rows
                // under the frame give it somewhere to land, with one
                // row above and below it to spare.
                while lines.len() < panel.len() + 2 {
                    lines.push(" ".repeat(width));
                }
                crate::screen::overlay(&mut lines, &panel);
                lines.extend(keys);
                lines.join("\n")
            }
            None => frame,
        }
    }

    /// Apply one key read while the board is up.
    ///
    /// Browsing, `↑`/`↓` move the cursor; lowercase acts on the row it sits
    /// on and uppercase acts on the whole run — `r` opens a picker of the
    /// cursor's task's steps if its own resume key is live — see
    /// [`Board::resume_cursor`]; `p` pauses just the cursor's task — the run
    /// as a whole is stopped from the dispatch tab's own `enter`, not from
    /// here; `u` takes the
    /// cursor's task off the queue and back to pending if nothing has
    /// started for it and no still-queued task depends on it, `U` does the
    /// same for every task that has not started; `s` starts the cursor's
    /// task's step over with a fresh session. A run-wide key opens a
    /// confirm panel first wherever what it is about to do is not free to
    /// undo — `u`, `U` and `s` open one unconditionally, since writing a
    /// task back to pending and throwing a conversation away are exactly
    /// that — see [`BoardMode`]. With a panel already open every other key
    /// is read by that panel instead: `esc` cancels it on every panel the
    /// board draws, and `enter` confirms whatever it opened on every panel
    /// but the restart panel. The letter that opened a panel no longer
    /// answers it once it is — a `q` typed there, or any other key neither
    /// mode recognises, is ignored, the same as it is while browsing. The
    /// restart panel is the exception: it is confirmed by `s`, the letter
    /// that opened it, and `enter` there is ignored. A pause panel answers one key further: `s` leaves every named abort
    /// running and schedules its task's `gate_at` on the step it is on
    /// instead. The resume picker answers `↑`/`↓` too, which move its own
    /// cursor rather than the board's.
    ///
    /// Reads the queue fresh rather than trusting the last frame drawn: a key
    /// can land in the gap between two redraws, and moving the cursor — or
    /// acting on a row order that has since changed — would pick the wrong
    /// task.
    pub(crate) fn on_key(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        key: crate::screen::Key,
    ) -> Result<()> {
        // Taken rather than borrowed: every arm below either answers the
        // panel this was and moves on, or hands a fresh one straight back —
        // `BoardMode` holds no lock a mutable borrow would fight, so this is
        // only about letting each arm build its own replacement without
        // fighting the borrow checker over `self.mode` at the same time.
        match std::mem::take(&mut self.mode) {
            BoardMode::Browsing => self.on_key_browsing(repo, pipelines, key),
            BoardMode::ConfirmPause { aborts, id } => {
                self.on_key_pause_confirm(repo, aborts, id, key)
            }
            BoardMode::ResumePicker(picker) => {
                self.on_key_resume_picker(repo, pipelines, picker, key)
            }
            BoardMode::ConfirmUnqueue { chain, dir } => {
                self.on_key_unqueue_confirm(repo, pipelines, chain, dir, key)
            }
            BoardMode::ConfirmUnqueueAll(ids) => self.on_key_unqueue_all_confirm(repo, ids, key),
            BoardMode::ConfirmRestart(confirm) => {
                self.on_key_restart_confirm(repo, pipelines, confirm, key)
            }
        }
    }

    fn on_key_browsing(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            // Off the last frame's own rows, with no read of any task file
            // behind it — see [`Board::drawn`]. An arrow is the one key that
            // cannot fail, whatever a pass is doing to the queue right now.
            Key::Up => self.cursor = shift_cursor(&self.drawn, self.cursor.as_deref(), -1),
            Key::Down => self.cursor = shift_cursor(&self.drawn, self.cursor.as_deref(), 1),
            Key::Char('o') => self.open_cursor(repo)?,
            Key::Char('r') => self.resume_cursor(repo, pipelines)?,
            Key::Char('p') => self.begin_pause_cursor(repo, pipelines)?,
            Key::Char('s') => self.begin_restart_cursor(repo, pipelines)?,
            Key::Char('u') => self.begin_unqueue_cursor(repo)?,
            Key::Char('U') => self.begin_unqueue_all(repo)?,
            _ => {}
        }
        Ok(())
    }

    /// `o`: open the cursor's task file in an editor, in a pane the
    /// multiplexer opens — a no-op with no cursor or a cursor on a row the
    /// board no longer draws. `repo.task` reads both the queue and the
    /// archive, so this reaches a done row's task exactly as it does a
    /// live one — [`build`]'s own composed row list, which [`Board::drawn`]
    /// is taken from, is what lets the cursor land on that row in the first
    /// place. Never blocks: the pane runs the editor on its
    /// own, and the board keeps redrawing and the pass loop keeps running
    /// while it is open, exactly as if nothing had happened.
    ///
    /// A backend with no pane to open one in — headless, which refuses the
    /// way `Mux::open_command`'s default does — is best-effort like every
    /// other key here: the refusal is swallowed, exactly as an `Err` out of `on_key`
    /// already is by the dispatch loop that calls it, and the task file it
    /// would have opened is left exactly as it was.
    fn open_cursor(&mut self, repo: &Repo) -> Result<()> {
        let Some(id) = self.cursor.clone() else {
            return Ok(());
        };
        let Ok(task) = repo.task(&id) else {
            return Ok(());
        };
        let command = editor_command(&task.path);
        let mux = crate::mux::backend(repo)?;
        let _ = mux.open_command(&repo.root, &format!("{id} · edit"), &command);
        Ok(())
    }

    /// `r`: open [`BoardMode::ResumePicker`] on the highlighted row, so a
    /// person picks the step the task resumes at — preselected on the one a
    /// bare `spoolway resume` would send it to.
    ///
    /// A no-op wherever there is nothing to do: no cursor yet, a cursor
    /// sitting on a row the queue no longer has, or a row whose own
    /// `resumable` says the key does nothing here — the dependency or
    /// busy-lane rule that decided the NEXT column already decided this, for
    /// a row with a real step to check; a row parked off `queued` carries no
    /// such rule at all and reads `resumable` outright — see `build_rows`'s
    /// `paused` arm.
    ///
    /// Two roads skip the picker and resume at once, because neither has a
    /// step to choose: a task that never started goes back onto `queued`,
    /// and a task its `done` hook paused goes back onto `done`. `--stage`
    /// names neither, so a picker there could offer only the one answer.
    fn resume_cursor(&mut self, repo: &Repo, pipelines: &Pipelines) -> Result<()> {
        let Some(id) = self.cursor.clone() else {
            return Ok(());
        };
        let rows = rows(repo, pipelines)?;
        let Some(row) = rows.iter().find(|r| r.id == id) else {
            return Ok(());
        };
        if !row.resumable {
            return Ok(());
        }
        let Ok(task) = repo.task(&id) else {
            return Ok(());
        };
        if let Ok(crate::commands::ResumeRoad::Queued | crate::commands::ResumeRoad::HookDone) =
            crate::commands::resume_road(&task, pipelines)
        {
            return resume_task(repo, pipelines, &id);
        }
        // Read once, on the key: the picker is a snapshot, not redrawn from
        // the queue every frame.
        let dependents = crate::dispatch::queue_dependents(repo, &task)?;
        self.mode = BoardMode::ResumePicker(resume_picker(&task, pipelines, dependents)?);
        Ok(())
    }

    fn on_key_resume_picker(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        mut picker: ResumePicker,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            Key::Up => picker.cursor = picker.cursor.saturating_sub(1),
            Key::Down => {
                picker.cursor = (picker.cursor + 1).min(picker.rows.len().saturating_sub(1))
            }
            // Nothing was touched while the picker was open, so there is
            // nothing to undo.
            Key::Esc => return Ok(()),
            Key::Enter => {
                let Some(row) = picker.rows.get(picker.cursor) else {
                    return Ok(());
                };
                // The picker can sit open for minutes, and `resume_held_row`
                // skips `spoolway resume`'s not-stopped refusal on purpose —
                // see its own doc. So whether the task is still held the way
                // it was when `r` opened the picker is asked again here, at
                // the keypress: a task resumed from a shell or an unblocker
                // in the meantime would otherwise be rewound, or moved under
                // a lane still working it.
                if let Some(refusal) = picker_gone_stale(repo, pipelines, &picker) {
                    picker.error = Some(refusal);
                    self.mode = BoardMode::ResumePicker(picker);
                    return Ok(());
                }
                // The `(next)` row sends no stage at all, not its own step as
                // one: a gate, a park and a block each take their own road
                // out — see `resume_road` — and `--stage` would send every
                // one of them down the ordinary road onto that step instead.
                let resumed = match row.next {
                    true => resume_task(repo, pipelines, &picker.id),
                    false => routed_for(repo, pipelines, &picker.id).and_then(|routed| {
                        crate::commands::resume_held_row(
                            repo,
                            &routed,
                            &crate::cli::ResumeArgs {
                                task: picker.id.clone(),
                                stage: Some(row.step.clone()),
                                message: None,
                            },
                        )
                    }),
                };
                // The dispatch loop drops an `Err` out of `on_key` without a
                // word, so a refused pick would close the picker and leave
                // the person guessing. It stays open with the refusal under
                // the list instead.
                match resumed {
                    Ok(()) => return Ok(()),
                    Err(err) => picker.error = Some(format!("{err:#}")),
                }
            }
            _ => {}
        }
        self.mode = BoardMode::ResumePicker(picker);
        Ok(())
    }

    /// `p`: park the cursor's own task, opening [`BoardMode::ConfirmPause`]
    /// first — titled with its id — only when that task's own step has an
    /// agent turn or a command run live right now. A no-op with no cursor, a
    /// cursor on a row the queue no longer has, or a cursor on a `paused`
    /// row: that one is already stopped and already waiting on a person, so
    /// parking it again would only overwrite `parked_from` with the state it
    /// is already stuck in. A `blocked` row has no such guard — the unblocker
    /// can be mid-turn on it, and `p` reaches that turn exactly as it does
    /// any other live step.
    fn begin_pause_cursor(&mut self, repo: &Repo, pipelines: &Pipelines) -> Result<()> {
        let Some(id) = self.cursor.clone() else {
            return Ok(());
        };
        let tasks = repo.tasks()?;
        let Some(i) = tasks.iter().position(|t| t.id() == id) else {
            return Ok(());
        };
        if tasks[i].stage() == crate::pipeline::PAUSED {
            return Ok(());
        }
        let mux = crate::mux::backend(repo)?;
        let lanes = mux.list_lanes().unwrap_or_default();
        let aborts: Vec<Abort> = live_aborts(repo, &tasks, pipelines, &lanes)
            .into_iter()
            .filter(|a| a.task == id)
            .collect();
        if aborts.is_empty() {
            park_under_lock(repo, &id, ParkedBy::Board)?;
            return Ok(());
        }
        self.mode = BoardMode::ConfirmPause { aborts, id };
        Ok(())
    }

    fn on_key_pause_confirm(
        &mut self,
        repo: &Repo,
        aborts: Vec<Abort>,
        id: String,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            // The one thing the panel does: abort what it named — an agent
            // turn interrupted through `Mux::interrupt_lane`, a command run
            // stopped through `Runs::stop` — and park the task that named
            // abort belongs to.
            Key::Enter => {
                let mux = crate::mux::backend(repo)?;
                let runs = crate::command_step::Runs::new(&repo.commands_dir());
                for abort in &aborts {
                    // A command run is stopped after its task is parked: from
                    // the kill until its files are cleared the run reads as one
                    // that died without an exit code, and a dispatcher pass
                    // landing there with the task still on the step would log
                    // that it is running the command again. An agent turn is
                    // interrupted first, since its settling is what the park
                    // answers.
                    match abort.kind {
                        AbortKind::Agent => {
                            carry_out_abort(mux.as_ref(), &runs, abort);
                            park_under_lock(repo, &abort.task, ParkedBy::Board)?;
                        }
                        AbortKind::Command => {
                            park_under_lock(repo, &abort.task, ParkedBy::Board)?;
                            carry_out_abort(mux.as_ref(), &runs, abort);
                        }
                    }
                }
            }
            // `s`: leave every named abort running and write `gate_at` onto
            // its task instead — whatever that step reports, whenever it
            // reports it, is what parks it now. An agent step's report parks
            // it through `commands::report`, and a command step's exit
            // through the dispatcher's command arm; neither is limited to
            // the pass a pipeline's own `gate: true` catches. Toggled per task
            // rather than only ever set, so pressing `s` again on a row that
            // already carries a schedule for the step it is on clears it —
            // the mockup's "pressing `s` again... clears it".
            Key::Char('s') => {
                for abort in &aborts {
                    let mut task = repo.task(&abort.task)?;
                    task.front.gate_at = match task.front.gate_at.as_deref() {
                        Some(step) if step == abort.step => None,
                        _ => Some(abort.step.clone()),
                    };
                    task.save()?;
                }
            }
            // Backing out with `esc` leaves the task, every lane and every
            // run exactly as they were — nothing here to undo, since nothing
            // was touched while the panel was open.
            Key::Esc => {}
            _ => self.mode = BoardMode::ConfirmPause { aborts, id },
        }
        Ok(())
    }

    /// `s`: open [`BoardMode::ConfirmRestart`] on the cursor's task, naming
    /// the step a restart would start over and the session it would throw
    /// away.
    ///
    /// A no-op with no cursor, a cursor on a row the queue no longer has, or
    /// a task `spoolway restart` itself would refuse — one that never
    /// started, one that is done, one held by a hook or one on a command
    /// step. [`crate::commands::restart_step`] is asked rather than the row's
    /// state, so the panel only ever offers a restart the command carries
    /// out.
    fn begin_restart_cursor(&mut self, repo: &Repo, pipelines: &Pipelines) -> Result<()> {
        let Some(id) = self.cursor.clone() else {
            return Ok(());
        };
        let Ok(task) = repo.task(&id) else {
            return Ok(());
        };
        let Ok(step) = restartable_step(&task, pipelines) else {
            return Ok(());
        };
        self.mode = BoardMode::ConfirmRestart(RestartConfirm {
            session: crate::commands::abandoned_session(repo, &step, &id),
            id,
            step,
            error: None,
        });
        Ok(())
    }

    fn on_key_restart_confirm(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        mut confirm: RestartConfirm,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            // Nothing was touched while the panel was open.
            Key::Esc => return Ok(()),
            Key::Char('s') => {
                // The panel can sit open while the task moves on: a report
                // passing the step, or a resume from a shell. `restart`
                // reads the step afresh and would start over whichever one
                // the task is on now, so a step that is no longer the one
                // the panel named is refused here rather than restarted
                // unseen.
                let restarted = routed_for(repo, pipelines, &confirm.id).and_then(|routed| {
                    let now = repo
                        .task(&confirm.id)
                        .and_then(|task| restartable_step(&task, &routed));
                    match now {
                        Ok(step) if step == confirm.step => crate::commands::restart(
                            repo,
                            &routed,
                            &crate::cli::RestartArgs {
                                task: confirm.id.clone(),
                                message: None,
                            },
                            None,
                        ),
                        Ok(step) => Err(anyhow::anyhow!(
                            "task `{}` moved to `{step}` since this panel opened — press esc \
                             and `s` again to restart the step it is on now.",
                            confirm.id
                        )),
                        Err(err) => Err(err),
                    }
                });
                // The dispatch loop drops an `Err` out of `on_key` without a
                // word, so a refused restart would close the panel and look
                // like it happened. It stays open with the refusal in it.
                match restarted {
                    Ok(()) => return Ok(()),
                    Err(err) => confirm.error = Some(format!("{err:#}")),
                }
            }
            _ => {}
        }
        self.mode = BoardMode::ConfirmRestart(confirm);
        Ok(())
    }

    /// `u`: open [`BoardMode::ConfirmUnqueue`] for the cursor's task and
    /// everything that reaches it through `depends_on` — a no-op with no
    /// cursor, a cursor on a row the queue no longer has, or a task that has
    /// started ([`not_started`] says no). A task some other still-queued
    /// task names in its own `depends_on` is no longer refused outright: the
    /// panel lists that dependent alongside it instead — see
    /// [`unqueue_chain`].
    fn begin_unqueue_cursor(&mut self, repo: &Repo) -> Result<()> {
        let Some(id) = self.cursor.clone() else {
            return Ok(());
        };
        let tasks = repo.tasks()?;
        let Some(task) = tasks.iter().find(|t| t.id() == id) else {
            return Ok(());
        };
        if !not_started(task) {
            return Ok(());
        }
        self.mode = BoardMode::ConfirmUnqueue {
            chain: unqueue_chain(&tasks, &id),
            dir: repo.pending_dir(),
        };
        Ok(())
    }

    /// `U`: open [`BoardMode::ConfirmUnqueueAll`], naming every task that has
    /// not started — a no-op wherever there is not one. Unlike `u`, nothing
    /// here is refused for a still-queued dependent: whatever depends on a
    /// task in this set has not started either, so it is in the set too, and
    /// nothing is left stranded by carrying both back to pending together.
    fn begin_unqueue_all(&mut self, repo: &Repo) -> Result<()> {
        let ids: Vec<String> = repo
            .tasks()?
            .iter()
            .filter(|t| not_started(t))
            .map(|t| t.id().to_string())
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        self.mode = BoardMode::ConfirmUnqueueAll(ids);
        Ok(())
    }

    fn on_key_unqueue_confirm(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        chain: Vec<ChainEntry>,
        dir: PathBuf,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            Key::Enter => {
                // Every row the chain sits on is about to leave the table,
                // so asking where `↓` would go has to happen against the
                // rows as they stand right now — the same list every removed
                // row is still part of. `shift_cursor` already wraps and
                // already falls back to the first row, so this only adds
                // what it can't see on its own: a dependent removed in the
                // same batch is no row to land on either, and with nothing
                // else queued the walk comes back around to the chain's own
                // head, at which point the cursor clears instead of pointing
                // at a task the board no longer shows.
                let before: Vec<String> = rows(repo, pipelines)?
                    .into_iter()
                    .map(|row| row.id)
                    .collect();
                let next = next_cursor_after_chain(&before, &chain);
                for entry in &chain {
                    unqueue_task(repo, &entry.id)?;
                }
                self.cursor = next;
            }
            Key::Esc => {}
            _ => self.mode = BoardMode::ConfirmUnqueue { chain, dir },
        }
        Ok(())
    }

    fn on_key_unqueue_all_confirm(
        &mut self,
        repo: &Repo,
        ids: Vec<String>,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            Key::Enter => {
                for id in &ids {
                    unqueue_task(repo, id)?;
                }
            }
            Key::Esc => {}
            _ => self.mode = BoardMode::ConfirmUnqueueAll(ids),
        }
        Ok(())
    }
}

/// The command line `o` runs on a task, everywhere `o` appears.
///
/// The same resolution `spoolway config edit` already uses: `$VISUAL`, then
/// `$EDITOR`, then `vi`. Shared between [`Board::open_cursor`]
/// and the queue screen's own `o` — `crate::commands::queue::open_highlighted`
/// — so the two can never resolve to two different editors.
pub(crate) fn editor_command(path: &Path) -> String {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    format!("{editor} '{}'", path.display())
}

/// What a `p`, `r`, `s`, `u` or `U` keypress is waiting to be answered — a
/// panel drawn over the table, and the one thing standing between an
/// accidental press and the run it would otherwise change. `Default` is
/// `Browsing`, both for [`Board::with_term`] and for [`std::mem::take`]
/// inside [`Board::on_key`], which is what lets each key handler build the
/// mode's replacement without holding a borrow of `self.mode` open across it.
#[derive(Default)]
enum BoardMode {
    #[default]
    Browsing,
    /// `p` found an agent turn or a command run live on task `id`'s own
    /// step: what pausing would abort, named, and nothing acted on yet.
    /// `[enter]` carries the pause out, `[s]` leaves every named abort
    /// running and schedules its task's `gate_at` for the step it is on
    /// instead, and `[esc]` leaves the task, every lane and every run
    /// exactly as they were.
    ConfirmPause { aborts: Vec<Abort>, id: String },
    /// `r` on a held row: every step of its pipeline the task runs, with the
    /// one a bare resume goes to preselected — see [`ResumePicker`].
    ResumePicker(ResumePicker),
    /// `u` found a task that has not started, named along with every
    /// unstarted task that reaches it through `depends_on` — see
    /// [`unqueue_chain`] — and `dir`, this run's own [`Repo::pending_dir`],
    /// read once when the panel opened, so its panel can show where the
    /// tasks are about to land without a second lookup at answer time.
    /// `chain` still needs a fresh [`Repo::task`] per id to answer with, the
    /// same as every confirm panel here: a task a moment ago and the
    /// task now are not guaranteed to be the same file.
    ConfirmUnqueue {
        chain: Vec<ChainEntry>,
        dir: PathBuf,
    },
    /// `U`'s own version of the same panel, naming every task it would carry
    /// back to pending rather than just the one under the cursor.
    ConfirmUnqueueAll(Vec<String>),
    /// `s` on a task `spoolway restart` accepts: the step it would start
    /// over and the session it would abandon. `[s]` carries the restart out
    /// and `[esc]` leaves the task and its lane exactly as they were.
    ConfirmRestart(RestartConfirm),
}

impl BoardMode {
    /// The panel this mode draws over the table, if any.
    fn panel(&self) -> Option<Vec<String>> {
        match self {
            BoardMode::Browsing => None,
            BoardMode::ConfirmPause { aborts, id } => Some(pause_confirm_panel(id, &aborts[0])),
            BoardMode::ResumePicker(picker) => Some(resume_picker_panel(picker, pane_height())),
            BoardMode::ConfirmUnqueue { chain, dir } => Some(unqueue_confirm_panel(chain, dir)),
            BoardMode::ConfirmUnqueueAll(ids) => Some(unqueue_all_confirm_panel(ids)),
            BoardMode::ConfirmRestart(confirm) => Some(restart_confirm_panel(confirm)),
        }
    }
}

/// [`BoardMode::ResumePicker`]'s state: one held task, every step of its
/// pipeline it can be sent to, and which of them the cursor is on.
///
/// Built once when `r` opens it, not re-read per frame: the rows are a
/// snapshot of the pipeline and the task as they stood then. `enter` checks
/// that snapshot against the task as it stands now before acting on it —
/// see [`picker_gone_stale`].
struct ResumePicker {
    /// The task being resumed.
    id: String,
    /// The task's hold when the picker opened — see [`Hold`].
    hold: Hold,
    /// Where the task stopped, in words — the line above the list.
    header: String,
    /// The steps the task runs, in pipeline order, then the one pinned row a
    /// plain resume to `blocked` or `done` adds under them — see
    /// [`resume_picker`].
    rows: Vec<PickRow>,
    /// How many of `rows` are the pipeline's own steps — the part that
    /// scrolls. Any row past these is pinned under the list.
    listed: usize,
    /// The rows the list keeps in view while the cursor is on a pinned row:
    /// the stopped step and its `on_pass` and `on_fail` targets.
    labelled: Option<(usize, usize)>,
    cursor: usize,
    /// The last refusal `enter` met, printed under the list until `esc`.
    error: Option<String>,
}

/// [`BoardMode::ConfirmRestart`]'s state, read once when `s` opens it.
struct RestartConfirm {
    /// The task being restarted.
    id: String,
    /// The step it would start over — [`crate::commands::restart_step`]'s.
    step: String,
    /// The conversation the restart throws away, if one is on record.
    session: Option<crate::commands::AbandonedSession>,
    /// The last refusal `s` met, printed in the panel until `esc`.
    error: Option<String>,
}

/// The step `spoolway restart` would start over for `task`, or its refusal.
fn restartable_step(task: &crate::task::Task, pipelines: &Pipelines) -> Result<String> {
    crate::commands::restart_step(task, pipelines.for_task(task)?)
}

/// What a held task's stop is made of: its stage and the three fields that
/// say which road a resume takes out of it. Two readings that agree on all
/// four are the same stop; any difference means the task was resumed, moved
/// or stopped again somewhere else since the first.
#[derive(PartialEq, Eq)]
struct Hold {
    stage: String,
    paused_at: Option<String>,
    parked_from: Option<String>,
    blocked_from: Option<String>,
}

impl Hold {
    fn of(task: &crate::task::Task) -> Hold {
        Hold {
            stage: task.stage().to_string(),
            paused_at: task.front.paused_at.clone(),
            parked_from: task.front.parked_from.clone(),
            blocked_from: task.front.blocked_from.clone(),
        }
    }
}

/// Why `enter` must not act on `picker` any more, as the line printed under
/// its list — or `None` while the task is still held exactly as it was when
/// the picker opened, and its row still offers the resume key.
///
/// The same guard `r` itself applies — [`Board::resume_cursor`] reads the
/// row fresh and checks `resumable` — taken again, since the picker opened
/// on a reading that may be minutes old.
fn picker_gone_stale(repo: &Repo, pipelines: &Pipelines, picker: &ResumePicker) -> Option<String> {
    let id = &picker.id;
    let Ok(task) = repo.task(id) else {
        return Some(format!(
            "`{id}` is no longer in the queue, so nothing was resumed. Press esc to close this."
        ));
    };
    if Hold::of(&task) != picker.hold {
        return Some(format!(
            "`{id}` has moved on to `{}` since this opened, so nothing was resumed. Press esc \
             and press r again to see where it stands now.",
            task.stage()
        ));
    }
    let resumable = match rows(repo, pipelines) {
        Ok(rows) => rows.iter().any(|row| row.id == *id && row.resumable),
        Err(err) => return Some(format!("{err:#}")),
    };
    match resumable {
        true => None,
        false => Some(format!(
            "`{id}` cannot be resumed right now: a task it depends on has not finished, or its \
             own lane is still working. Nothing was resumed."
        )),
    }
}

/// One row of a [`ResumePicker`].
struct PickRow {
    step: String,
    /// What this step is to the stop: the row's state word on the step it
    /// stopped at, `on pass` and `on fail` on that step's own targets, and
    /// `(next)` on wherever a plain resume goes.
    note: String,
    /// Whether this is the `(next)` row, which resumes with no `--stage`.
    next: bool,
}

/// The step a held task stopped at, read off the field its stop recorded —
/// `None` only for a task held before it ever started, which `r` resumes
/// without a picker anyway.
///
/// A gate's `paused_at` first, then a park's `parked_from`, then a block's
/// `blocked_from` — the order `resume_road` tries its roads in. Each is taken
/// as recorded, even when the pipeline no longer has that step, so the
/// picker can name the missing step rather than whatever `resume_target`
/// would fall back to. A `p` on a blocked row writes `parked_from: blocked`,
/// which names no step, so its `blocked_from` answers instead.
fn stopped_at(task: &crate::task::Task, pipeline: &crate::pipeline::Pipeline) -> Option<String> {
    let front = &task.front;
    let real = |step: &Option<String>| step.clone().filter(|s| s != crate::pipeline::BLOCKED);
    real(&front.paused_at)
        .or_else(|| real(&front.parked_from))
        .or_else(|| real(&front.blocked_from))
        .or_else(|| {
            let target = crate::commands::resume_target(task, pipeline);
            (target != crate::pipeline::QUEUED).then_some(target)
        })
}

/// The line above the picker's list: the row's state word, the step it
/// stopped at, and what a gate caught there — the same reading
/// `resume_road` routes by, through `caught_at`.
fn picker_header(
    task: &crate::task::Task,
    pipeline: &crate::pipeline::Pipeline,
    stopped: Option<&str>,
) -> String {
    let word = task.stage();
    let Some(step) = stopped else {
        return word.to_string();
    };
    if pipeline.step(step).is_none() {
        return format!(
            "{word} at {step}, a step pipeline `{}` no longer has",
            pipeline.name
        );
    }
    if task.front.paused_at.is_some() {
        let caught = match crate::commands::caught_at(task, step) {
            Some(crate::commands::Caught::Pass) => " — it passed",
            Some(crate::commands::Caught::Fail) => " — it failed",
            Some(crate::commands::Caught::Blocked) => " — it blocked",
            None if task.front.blocked_from.as_deref() == Some(step) => " — its block was cleared",
            None => "",
        };
        return format!("{word} at {step}{caught}");
    }
    if task.front.parked_from.as_deref() == Some(crate::pipeline::BLOCKED) {
        return format!("{word} while blocked at {step}");
    }
    format!("{word} at {step}")
}

/// The picker `r` opens on `task`: every step of its pipeline but `blocked`
/// and `done`, which `--stage` does not accept, labelled against the step it
/// stopped at.
///
/// A step the task walks past is left out, since `resume --stage` refuses it.
/// Two are kept anyway. The step it stopped at is listed so the person sees
/// where the task is, even if that step has become hidden since; picking it
/// sends it as `--stage`, which refuses it, unless it is also the `(next)`
/// row. The step a plain resume lands on is listed so the `(next)` row is
/// never missing; that row sends no `--stage`, so picking it is accepted. It
/// is only ever hidden for a hidden step with no `on_pass` to carry the task
/// on.
/// `dependents` is [`crate::dispatch::same_group_dependents`]'s count.
///
/// The `(next)` row is placed from `resume_road`, the function a bare resume
/// acts on and `spoolway queue route` prints, landed past hidden steps the
/// way the resume lands it — see `ResumeRoad::landing` — so the preselected
/// row cannot name a step the resume would not go to. The `on pass` and
/// `on fail` labels are landed the same way. When that destination is
/// `blocked` or `done`, one pinned row under the steps names it, since
/// neither is a row anyone may pick on purpose. A stopped step the pipeline
/// no longer has leaves no `(next)` row at all: `resume_road` either refuses
/// it, sends the task back onto that same missing step, or falls back to a
/// step the task never stopped at. The cursor starts on the first step
/// instead, for a person to pick a real one.
fn resume_picker(
    task: &crate::task::Task,
    pipelines: &Pipelines,
    dependents: usize,
) -> Result<ResumePicker> {
    let pipeline = pipelines.for_task(task)?;
    let stopped = stopped_at(task, pipeline);
    let stopped_step = stopped.as_deref().and_then(|id| pipeline.step(id));
    let next = match (stopped.is_some(), stopped_step) {
        (true, None) => None,
        _ => crate::commands::resume_road(task, pipelines)
            .ok()
            .map(|road| road.landing(pipeline, task, dependents)),
    };
    let target = |outcome| {
        stopped_step
            .and_then(|step| step.destination(outcome))
            .map(|destination| landed(pipeline, task, destination.to_string(), dependents))
    };
    let on_pass = target(crate::pipeline::Outcome::Pass);
    let on_fail = target(crate::pipeline::Outcome::Fail);
    let (on_pass, on_fail) = (on_pass.as_deref(), on_fail.as_deref());

    let mut rows: Vec<PickRow> = Vec::new();
    for step in &pipeline.steps {
        let id = step.id.as_str();
        if id == crate::pipeline::BLOCKED || id == crate::pipeline::DONE {
            continue;
        }
        let kept = stopped.as_deref() == Some(id) || next.as_deref() == Some(id);
        if !kept && crate::dispatch::walk_past(step, task, dependents).is_some() {
            continue;
        }
        let mut notes: Vec<&str> = Vec::new();
        if stopped.as_deref() == Some(id) {
            notes.push(task.stage());
        }
        if on_pass == Some(id) {
            notes.push("on pass");
        }
        if on_fail == Some(id) {
            notes.push("on fail");
        }
        let is_next = next.as_deref() == Some(id);
        let mut note = notes.join(", ");
        if is_next {
            note = format!("{note} (next)").trim_start().to_string();
        }
        rows.push(PickRow {
            step: id.to_string(),
            note,
            next: is_next,
        });
    }
    let listed = rows.len();
    let labelled = {
        let marked: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| !row.note.is_empty())
            .map(|(i, _)| i)
            .collect();
        marked.first().zip(marked.last()).map(|(a, b)| (*a, *b))
    };
    if let Some(pinned) = next
        .as_deref()
        .filter(|step| *step == crate::pipeline::BLOCKED || *step == crate::pipeline::DONE)
    {
        rows.push(PickRow {
            step: pinned.to_string(),
            note: "(next)".to_string(),
            next: true,
        });
    }
    let cursor = rows.iter().position(|row| row.next).unwrap_or(0);
    Ok(ResumePicker {
        id: task.id().to_string(),
        hold: Hold::of(task),
        header: picker_header(task, pipeline, stopped.as_deref()),
        rows,
        listed,
        labelled,
        cursor,
        error: None,
    })
}

/// One command step `p` found running, named for
/// `commands::queue::queue_pause`'s own refusal — the task it belongs to,
/// the step, and how long it has been going, off the same clock the board's
/// own TIME column reads.
///
/// `pub(crate)`: `spoolway queue pause` reads [`running_command_steps`]
/// directly, since a running command step is the one case it cannot just act
/// on without a person there to answer a confirm panel — see
/// `commands::queue::queue_pause`. The board itself no longer holds a
/// `CommandRunning` list of its own past the moment a panel opens: see
/// [`Abort`], which folds this together with an agent turn into the one
/// shape [`BoardMode::ConfirmPause`] actually draws and answers.
pub(crate) struct CommandRunning {
    pub(crate) task: String,
    pub(crate) step: String,
    pub(crate) elapsed: Option<Duration>,
}

/// What pausing would cut short if it went ahead right now — an agent turn
/// mid-turn, or a command step's run in flight — named for
/// [`BoardMode::ConfirmPause`]'s panel and for carrying out the pause once
/// `enter` answers it. The one shape both kinds share, in place of the
/// pre-interrupted agent lane and the still-pending [`CommandRunning`] list
/// this task replaced: neither an agent lane nor a command run is touched
/// any more until the panel naming it is actually answered.
struct Abort {
    task: String,
    step: String,
    kind: AbortKind,
    /// How long this abort has been running, off the same two clocks the
    /// board's own TIME column reads: `now - launched_at` for a live agent
    /// lane, `Runs::elapsed` for a command run. `None` where neither answers.
    elapsed: Option<Duration>,
}

/// What kind of thing an [`Abort`] would cut short — read by the panel for
/// its own `agent`/`command` column, and by [`Board::on_key_pause_confirm`]
/// for which of `Mux::interrupt_lane` or `Runs::stop` carries the abort out.
#[derive(Clone, Copy)]
enum AbortKind {
    Agent,
    Command,
}

impl AbortKind {
    fn word(self) -> &'static str {
        match self {
            AbortKind::Agent => "agent",
            AbortKind::Command => "command",
        }
    }
}

/// Every live agent turn and running command step across `tasks`, one
/// [`Abort`] each — [`live_agent_lane_tasks`] and [`running_command_steps`]
/// folded into the one shape `p` draws a panel from, and the dispatch tab's
/// stop popup interrupts — see [`interrupt_for_stop`] — answer
/// against. Both of those already skip a `paused` task — the one state
/// nothing is ever live on — so there is nothing further to filter out here.
/// A `blocked` task is not skipped: the unblocker can be mid-turn on it, and
/// that turn is exactly what `p` and a stop's `i` reach.
fn live_aborts(
    repo: &Repo,
    tasks: &[crate::task::Task],
    pipelines: &Pipelines,
    lanes: &[crate::mux::Lane],
) -> Vec<Abort> {
    let mut out = Vec::new();
    for i in live_agent_lane_tasks(repo, tasks, pipelines, lanes) {
        let task = &tasks[i];
        // The same clock `build_rows` reads for a live row's own TIME column
        // — the round in flight's own launch, not banked yet.
        let elapsed = task.front.launched_at.map(|launched| {
            Duration::from_secs((chrono::Utc::now().timestamp() - launched).max(0) as u64)
        });
        out.push(Abort {
            task: task.id().to_string(),
            step: task.stage().to_string(),
            kind: AbortKind::Agent,
            elapsed,
        });
    }
    for cr in running_command_steps(repo, tasks, pipelines) {
        out.push(Abort {
            task: cr.task,
            step: cr.step,
            kind: AbortKind::Command,
            elapsed: cr.elapsed,
        });
    }
    out
}

/// Cut one named abort short — an agent turn interrupted through
/// `Mux::interrupt_lane`, a command run stopped through `Runs::stop`.
/// Best-effort: a lane that has already gone quiet on its own has nothing
/// left to interrupt, and one lane's failure here must never leave the rest
/// of what a panel or popup promised half kept. Shared by `p`'s panel and
/// [`interrupt_for_stop`], so the two cut a step short the same way.
fn carry_out_abort(mux: &dyn crate::mux::Mux, runs: &crate::command_step::Runs, abort: &Abort) {
    match abort.kind {
        AbortKind::Agent => {
            let name = crate::mux::lane_name(&abort.step, &abort.task);
            let _ = mux.interrupt_lane(&name);
        }
        AbortKind::Command => {
            runs.stop(&crate::command_step::Runs::key(&abort.step, &abort.task));
        }
    }
}

/// The dispatch tab's stop popup's `i`: interrupt every live agent turn and
/// kill every running command step, parking each of those tasks on `paused`
/// with [`crate::task::Frontmatter::parked_by_stop`] set, so the next start
/// can resume exactly these — see [`resume_stop_parked`]. Nothing else in
/// the queue is touched. The caller stops the dispatcher afterwards.
///
/// Parks before it aborts, for an agent turn as well as a command run —
/// `p`'s panel does that only for a command run — because the dispatcher is
/// still running while this does its work. Aborted first, a lane can settle
/// and be seen by a pass before the park lands — the pass then parks it as a
/// person's own Escape (`Dispatcher::park_after_interrupt`), and this park,
/// finding the task already on `paused`, would leave it unmarked and never
/// resumed. Parked first, the pass leaves a `paused` task alone, and any
/// stale copy it is holding fails `persist_task`'s fingerprint check.
pub(crate) fn interrupt_for_stop(repo: &Repo, pipelines: &Pipelines) -> Result<()> {
    let tasks = repo.tasks()?;
    let mux = crate::mux::backend(repo)?;
    let lanes = mux.list_lanes().unwrap_or_default();
    let runs = crate::command_step::Runs::new(&repo.commands_dir());
    for abort in live_aborts(repo, &tasks, pipelines, &lanes) {
        park_under_lock(repo, &abort.task, ParkedBy::Stop)?;
        carry_out_abort(mux.as_ref(), &runs, &abort);
    }
    Ok(())
}

/// Resume every task a stop's `i` parked — [`crate::task::Frontmatter::
/// parked_by_stop`] set and still on `paused` — through [`resume_task`],
/// the same code the board's `r` sends one row through, which spends the
/// mark. Called by `spoolway dispatch` once it holds the lock and before its
/// first pass, so a start from the dispatch tab and one typed at a terminal
/// both resume them. A task a person paused with `p`, or interrupted by hand
/// in its pane, carries no mark and stays where it is.
///
/// One task that will not resume does not hold the rest back, nor the start:
/// each failure comes back as a problem line for the caller to report, and
/// that task keeps its mark for the next start to try again.
pub(crate) fn resume_stop_parked(repo: &Repo, pipelines: &Pipelines) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for task in repo.tasks()? {
        if task.stage() != crate::pipeline::PAUSED || !task.front.parked_by_stop {
            continue;
        }
        if let Err(err) = resume_task(repo, pipelines, task.id()) {
            problems.push(format!(
                "{}: could not resume what stopping interrupted: {err:#}",
                task.id()
            ));
        }
    }
    Ok(problems)
}

/// Who is parking a task, which decides the Status Log line and whether the
/// park carries a stop's mark. See [`park_under_lock`].
#[derive(Clone, Copy)]
pub(crate) enum ParkedBy {
    /// A person's `p` on the board.
    Board,
    /// A dispatching stop, marked so the next start resumes the task — see
    /// [`interrupt_for_stop`].
    Stop,
    /// `spoolway queue pause`, for a person with no board in front of them.
    QueuePause,
}

/// Park one task by id: read, [`park`] and save under the same per-task
/// lock a lane's `spoolway report` and the dispatcher's own `persist` take,
/// so the park cannot land in the middle of either one's read-modify-write
/// and lose it — or be lost to it. Read fresh under the lock rather than
/// from the queue as the board last saw it, and silent about a task the
/// queue no longer has, or one already stopped on `paused`: a stop walks
/// every running step, and one row archived since it read the queue must
/// not leave the rest unparked (jobs review finding 5). A task already on
/// `paused` is left alone because [`park`] would overwrite `parked_from`
/// with `paused`, and `resume` could then only land back on `paused`.
/// Shared by the board's `p`, a stop and `spoolway queue pause`, so a rule
/// learned by one reaches the others; a caller that must refuse a task the
/// queue no longer has checks for it first, as `queue_pause` does.
/// `by` picks the log line and, for a stop, sets the mark.
pub(crate) fn park_under_lock(repo: &Repo, id: &str, by: ParkedBy) -> Result<()> {
    let _task_lock = crate::lock::TaskLock::acquire(&repo.task_lock_file(id));
    let Ok(mut task) = repo.task(id) else {
        return Ok(());
    };
    if task.stage() == crate::pipeline::PAUSED {
        return Ok(());
    }
    match by {
        ParkedBy::Stop => {
            park(&mut task, "interrupted when dispatching stopped", false);
            task.front.parked_by_stop = true;
        }
        ParkedBy::Board => park(&mut task, "paused from the board", false),
        ParkedBy::QueuePause => park(&mut task, "paused via `spoolway queue pause`", false),
    }
    task.save()
}

/// Send one task through `spoolway resume`'s body, as a person typing the
/// command would — [`crate::commands::resume_held_row`], not a second copy of
/// it. It differs in one way: a task on a live step is restarted rather than
/// refused, because a row holding an unanswered question stands on one.
/// Shared between the `(next)` row of `r`'s picker, `r` on a row it resumes
/// without a picker, and [`resume_stop_parked`], so none of them can resume
/// the same task a different way. `pub(crate)`: `spoolway queue resume` is
/// another caller, for a person with no board in front of them at all — see
/// `commands::queue::queue_resume`.
///
/// Silent about a task the queue no longer has — every caller has read it
/// fresh a moment before, so it knows the task is there, and racing a
/// second process that archived or removed it since is not this key's to
/// report.
/// The pipelines a key that moves task `id` routes on.
///
/// `pipelines` is the board's own, read from the files when the screen
/// opened. While a dispatcher runs, the task goes where that dispatcher's
/// copy says instead, the same as `spoolway resume` and `spoolway restart` —
/// or a pipeline edited since would send it to a step the run never loaded.
/// See `crate::pipeline_snapshot::for_task`.
fn routed_for<'a>(
    repo: &Repo,
    pipelines: &'a Pipelines,
    id: &str,
) -> Result<std::borrow::Cow<'a, Pipelines>> {
    Ok(
        match crate::pipeline_snapshot::for_task(repo, Some(id)).transpose()? {
            Some(routed) => std::borrow::Cow::Owned(routed),
            None => std::borrow::Cow::Borrowed(pipelines),
        },
    )
}

pub(crate) fn resume_task(repo: &Repo, pipelines: &Pipelines, id: &str) -> Result<()> {
    if repo.task(id).is_err() {
        return Ok(());
    }
    let routed = routed_for(repo, pipelines, id)?;
    let pipelines = &*routed;
    // `paused_at` is a gate passed, waiting to be sent on past it;
    // `parked_from` is a person's own interrupt, and `blocked_from` is a
    // real block — all three waiting to be sent back to where they stopped.
    // `parked_from` and `blocked_from` can both be set at once now: `p` on a
    // `blocked` row writes `parked_from: blocked` beside the `blocked_from`
    // already there, and `resume_road` checks `parked_from` first, so
    // it is the one that decides — see `unpark`, which never touches
    // `blocked_from` and leaves it to route the task again once cleared.
    //
    // There is no guard on any of that here, because neither the fields nor
    // the stage can refuse anything any more. Two ordinary rows carry none
    // of the three:
    //
    //   * a park off `queued`, which had no step to record — see `park`; it
    //     sits on `paused` with all three absent, so refusing on absence
    //     would refuse a genuine park;
    //   * a task holding a person-answered *question*, which never reaches
    //     `paused` at all and is still standing on its own live step, so
    //     refusing on `stage()` would refuse that one instead.
    //
    // The two are the same shape as the race this used to catch — another
    // process answering the row since it was built — and nothing readable
    // here tells them apart. Firing `commands::resume` on a raced row is a
    // repeat, not a wrong move: it sends the task to the step it is already
    // going to. Whether the key does anything at all is decided before this
    // is reached, by the row's own `resumable`, read fresh at the keypress —
    // see `resume_cursor`, and `picker_gone_stale` for the picker's `enter`.
    //
    // This goes through `resume_held_row`, not `spoolway resume`'s own entry:
    // that one refuses a task on a live step, which is exactly this question row.
    //
    // Nothing here has to say which of the three, if any, applies:
    // `commands::resume` reads `paused_at.is_some()` itself to route a gate
    // one way and everything else the other, so naming a step here would
    // only risk disagreeing with it — see `resume_road`, which finds
    // `parked_from` and `blocked_from` on its own, falls back to `queued`
    // when a task that never started leaves every field `resume_target`
    // reads unset, and handles the question-pane case through
    // `resume_target`'s `last_report.step`: pressing `r` there restarts the
    // step the pane was never answered on.
    crate::commands::resume_held_row(
        repo,
        pipelines,
        &crate::cli::ResumeArgs {
            task: id.to_string(),
            stage: None,
            message: None,
        },
    )
}

/// Whether `task` has not started at all — still sitting on the pipeline's
/// own `queued` step, with no worktree cut and no lane ever begun. This is
/// the one state `u`/`U` may act on: unqueuing anything further along would
/// mean tearing down a checkout, which is outside what either key reaches —
/// see this task's own non-goals.
pub(crate) fn not_started(task: &crate::task::Task) -> bool {
    task.stage() == crate::pipeline::QUEUED
}

/// The other not-started task that names `id` in its own `depends_on`, if
/// there is one — what `spoolway queue unqueue` still refuses outright,
/// since it has no panel to list a chain on. The board's own `u` no longer
/// reads this: it carries `id` and every such dependent back to pending
/// together instead — see [`unqueue_chain`].
///
/// Only a task that has not started can be waiting on `id` at all: `queued`
/// is the one step a dependency check gates, so nothing past it depends on a
/// task still active in the queue — see [`crate::graph::Graph::ready`]. The
/// check is still made explicit here, rather than assumed, so a caller never
/// has to trust that invariant to read this correctly.
///
/// `pub(crate)`: `spoolway queue unqueue` reads this for its own refusal —
/// see `commands::queue::queue_unqueue_one`. It returns the dependent itself
/// rather than a bare bool because the command names it in its message.
pub(crate) fn depended_on_by_queued<'a>(
    tasks: &'a [crate::task::Task],
    id: &str,
) -> Option<&'a crate::task::Task> {
    tasks
        .iter()
        .find(|t| t.id() != id && not_started(t) && t.front.depends_on.iter().any(|d| d == id))
}

/// One task in a `u`-panel's chain: its id, and — for everything but the
/// chain's own head — the ids elsewhere in the chain its own `depends_on`
/// names, for [`view::unqueue_confirm_panel`]'s "(depends on ...)"
/// annotation. Empty for the head, which by construction depends on nothing
/// else in its own chain — the chain is exactly what depends on *it*.
struct ChainEntry {
    id: String,
    depends_on: Vec<String>,
}

/// `id`, then every not-started task that reaches it through `depends_on`,
/// however many hops away — what `u`'s panel lists and carries back to
/// pending together. Breadth first, each new level sorted by id, so the
/// panel reads the way a person would explain the chain: the task under the
/// cursor, then what leans on it, then what leans on that.
///
/// Only a task that has not started can lean on a still-`queued` one at all
/// — see [`crate::graph::Graph::ready`] — so the walk never has to reason
/// about a task already running: everything it finds by chasing
/// `depends_on` backwards is itself fair game for the same carry.
fn unqueue_chain(tasks: &[crate::task::Task], id: &str) -> Vec<ChainEntry> {
    let mut ids: Vec<String> = vec![id.to_string()];
    let mut frontier: Vec<String> = vec![id.to_string()];
    while !frontier.is_empty() {
        let mut found: Vec<String> = tasks
            .iter()
            .filter(|t| not_started(t) && !ids.contains(&t.id().to_string()))
            .filter(|t| t.front.depends_on.iter().any(|d| frontier.contains(d)))
            .map(|t| t.id().to_string())
            .collect();
        if found.is_empty() {
            break;
        }
        found.sort();
        found.dedup();
        ids.extend(found.iter().cloned());
        frontier = found;
    }
    ids.iter()
        .map(|entry_id| {
            let depends_on = if entry_id == id {
                Vec::new()
            } else {
                tasks
                    .iter()
                    .find(|t| t.id() == entry_id)
                    .map(|t| {
                        t.front
                            .depends_on
                            .iter()
                            .filter(|d| ids.contains(d))
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default()
            };
            ChainEntry {
                id: entry_id.clone(),
                depends_on,
            }
        })
        .collect()
}

/// Move one task from the queue back to pending, dropping every
/// [`crate::commands::RESERVED_KEYS`] field from its frontmatter so `queue
/// add --from` accepts it exactly as it would a task that had never been
/// queued — the same fields `queue_add::parse_submission` refuses to see set
/// on a task coming in.
///
/// `Task::save` cannot do this alone: `stage` is a plain `String` with no
/// `skip_serializing_if`, so it always round-trips, and it is one of the
/// keys that has to disappear entirely rather than clear to empty. The
/// frontmatter goes through a bare YAML mapping instead, exactly as
/// `parse_submission` reads one coming in, so the same keys that task
/// path refuses are the ones this path removes.
///
/// Re-reads `id` fresh rather than trusting whatever `Task` a caller already
/// has — the same reason [`resume_task`] does — and checks `not_started`
/// again before touching anything: a task this key already refused a moment
/// ago, or one a second process moved on since the panel opened, is left
/// exactly where it is rather than risk carrying an archived task back
/// to pending.
///
/// `pub(crate)`: `spoolway queue unqueue` is the third caller, for a task on
/// `queued` with no `--force` — the same body a keypress and the command
/// share, exactly as [`resume_task`] is for `r` and `queue resume`. A
/// task that has already started is a different road: `queue unqueue
/// --force` goes around this function's own `not_started` gate and calls
/// [`carry_to_pending`] directly, once its own teardown has cleared the
/// checkout — see `commands::queue::queue_unqueue`.
pub(crate) fn unqueue_task(repo: &Repo, id: &str) -> Result<()> {
    // The same per-task lock the dispatcher and `spoolway report` take, so
    // this rename cannot land in the middle of one of their read-modify-
    // writes. See [`crate::lock::TaskLock`] and review finding 49.
    let _task_lock = crate::lock::TaskLock::acquire(&repo.task_lock_file(id));

    let Ok(task) = repo.task(id) else {
        return Ok(());
    };
    if !not_started(&task) {
        return Ok(());
    }
    // The reason goes to the problem log rather than breaking the board
    // loop this runs inside: the row simply stays queued.
    if carry_to_pending(repo, &task)?.is_none() {
        crate::problem_log::append(
            repo,
            &format!("did not unqueue `{id}`: a newer draft is already in the pending directory"),
        );
    }
    Ok(())
}

/// The move itself, shared with `spoolway queue unqueue`: `task`'s task
/// written to pending with every reserved key dropped, then its queue file
/// removed. The caller decides whether `task` may go — the board's `u` and
/// [`unqueue_task`] only carry a task that has not started, `queue unqueue
/// --force` one whose checkout it has just torn down — and holds the task's
/// lock while it does.
///
/// `None` when a task already sits in `pending/` under this id: that is
/// a newer draft — a producer re-ran over work already submitted — and
/// putting the queued copy back on top of it would silently lose that
/// draft. Nothing is touched in that case.
pub(crate) fn carry_to_pending(repo: &Repo, task: &crate::task::Task) -> Result<Option<PathBuf>> {
    let id = task.id();
    let dest = repo.pending_dir().join(format!("{id}.md"));
    if dest.exists() {
        return Ok(None);
    }
    let mut front = serde_norway::to_value(&task.front)
        .with_context(|| format!("serialising {id}'s frontmatter"))?;
    if let serde_norway::Value::Mapping(map) = &mut front {
        for key in crate::commands::RESERVED_KEYS {
            map.remove(*key);
        }
        // `starts_from:` is a person's to set, so it is not reserved, but once
        // a worktree has been cut spoolway has stamped it with the branch it
        // used and nothing tells that apart from a person's value. Carried
        // back, the stamp would outrank a `base:` or `depends_on:` the person
        // edits in the pending copy, and pause the task over a branch they
        // never chose. An uncut task has neither `run` nor `base_commit`, so a
        // value a person set before the cut survives.
        if task.front.run.is_some() || task.front.base_commit.is_some() {
            map.remove("starts_from");
        }
    }
    let yaml =
        serde_norway::to_string(&front).with_context(|| format!("rendering {id}'s frontmatter"))?;
    let rendered = format!("---\n{yaml}---\n{}", task.body);
    crate::task::write_atomic(&dest, &rendered)
        .with_context(|| format!("writing {}", dest.display()))?;
    // Only once the pending copy is safely on disk — a crash before this
    // leaves the queue file in place, so the task is still queued rather
    // than lost between the two directories.
    std::fs::remove_file(&task.path)
        .with_context(|| format!("removing {}", task.path.display()))?;
    Ok(Some(dest))
}

/// Park one task on `paused` with `parked_from` naming the step it was on —
/// the record `p` leaves behind, read back by [`resume_task`] exactly as a
/// block's own `blocked_from` is, but never confused with one: `blocked_from`
/// is left untouched, since nothing here failed the way a block does. Never
/// touches `paused_at` either: this task passed no gate, so there is no step
/// to release it past, only one to send it back to.
///
/// Sets no `parked_from` at all when the task is still on `queued`: `queued`
/// is not a step any pipeline declares, so a `parked_from: queued` would
/// never match the step a launch is starting and would never be spent by
/// `finish_launch_bookkeeping` — it would sit in the task for the rest of the
/// run. `resume_target` already answers `queued` itself when nothing names a
/// step, which is where a task that never started belongs — see
/// [`build_rows`]'s `paused` arm, which reads the same answer to skip the
/// dependency and lane check a real step still needs.
///
/// Goes through [`crate::task::Task::set_stage_unbanked`] rather than
/// `set_stage`: the task never left `implement` (or wherever it was), so
/// arriving at `paused` and leaving it again are not laps of anything, and
/// counting them would let a `loop:` budget see two moves nothing routed.
///
/// `pub(crate)`, and taking `message` rather than hard-coding one, so
/// `dispatch::Dispatcher` can write the same two fields the same way for a
/// person's own Escape in a pane, or a lane it gave up on — see
/// `Dispatcher::park_after_interrupt` and `Dispatcher::tear_down_and_escalate`
/// — with a `## Status Log` line that says why *that* park happened, which is
/// not "paused from the board". `escalated` is the one difference between
/// those callers: `false` for a person's own keypress or Escape, `true` for a
/// lane `escalate_clock` gave up on — see [`crate::task::Frontmatter::
/// escalated`], which this sets alongside `parked_from`.
///
/// `parked_from` is left unset for a task parked off `queued`: it had not
/// started, so there is no step to send it back to, and `resume_task` puts
/// it back on `queued` itself instead. `escalated` is written either way —
/// it says why the park happened, not where it happened from.
pub(crate) fn park(task: &mut crate::task::Task, message: &str, escalated: bool) {
    if task.stage() != crate::pipeline::QUEUED {
        task.front.parked_from = Some(task.stage().to_string());
    }
    task.front.escalated = escalated;
    // Any park is a fresh one: a stop's mark left from an earlier stop must
    // not make the next start resume a task a person has since parked.
    task.front.parked_by_stop = false;
    task.set_stage_unbanked(crate::pipeline::PAUSED, message);
}

/// Tasks (by index into `tasks`) with a live agent lane at the step they are
/// on right now — exactly what `p` sends an interrupt to.
///
/// Addressed by the task's own current step, the same way [`build_rows`]
/// finds a row's `live_lane`, rather than by parsing every lane name back
/// into a task and step the way [`slots_used`] does: a stale lane left over
/// from a step this task has since moved past must never be interrupted on
/// its behalf, and comparing against `task.stage()` directly is what rules
/// that out.
///
/// `pub(crate)`: `spoolway queue pause` reads this too, for the same
/// interrupt-before-park `p` does — see `commands::queue::queue_pause`.
pub(crate) fn live_agent_lane_tasks(
    repo: &Repo,
    tasks: &[crate::task::Task],
    pipelines: &Pipelines,
    lanes: &[crate::mux::Lane],
) -> Vec<usize> {
    let mine = crate::dispatch::our_checkouts(repo, tasks);
    let mut out = Vec::new();
    for (i, task) in tasks.iter().enumerate() {
        if task.stage() == crate::pipeline::PAUSED {
            continue;
        }
        let Ok(pipeline) = pipelines.for_task(task) else {
            continue;
        };
        let Some(step) = pipeline.step(task.stage()) else {
            continue;
        };
        if step.kind() != crate::pipeline::StepKind::Agent {
            continue;
        }
        let name = crate::mux::lane_name(task.stage(), task.id());
        if lanes
            .iter()
            .any(|l| l.name == name && crate::dispatch::owns_cwd(&mine, &l.cwd))
        {
            out.push(i);
        }
    }
    out
}

/// Every task on a command step whose run is still going — one half of what
/// [`live_aborts`] folds into its own list for the board's confirm panel,
/// the `AbortKind::Command` entries; the other half is [`live_agent_lane_tasks`].
///
/// `pub(crate)`: `spoolway queue pause` reads it directly rather than through
/// [`live_aborts`], since a running command step is the one case it cannot
/// just act on — killing one throws away whatever it was doing — so it
/// refuses instead of showing a panel nobody is there to answer; see
/// `commands::queue::queue_pause`.
pub(crate) fn running_command_steps(
    repo: &Repo,
    tasks: &[crate::task::Task],
    pipelines: &Pipelines,
) -> Vec<CommandRunning> {
    let runs = crate::command_step::Runs::new(&repo.commands_dir());
    let mut out = Vec::new();
    for task in tasks {
        if task.stage() == crate::pipeline::PAUSED {
            continue;
        }
        let Ok(pipeline) = pipelines.for_task(task) else {
            continue;
        };
        let Some(step) = pipeline.step(task.stage()) else {
            continue;
        };
        if step.kind() != crate::pipeline::StepKind::Command {
            continue;
        }
        let key = crate::command_step::Runs::key(task.stage(), task.id());
        if runs.state(&key) == crate::command_step::RunState::Running {
            out.push(CommandRunning {
                task: task.id().to_string(),
                step: task.stage().to_string(),
                elapsed: runs.elapsed(&key),
            });
        }
    }
    out
}

/// Where the cursor lands after moving `delta` rows from `current` among
/// `ids`, wrapping at either end. `None` only when there is nowhere to put
/// it — an empty board. Landing on the first row rather than nowhere both
/// when `current` is `None` — the cursor has never moved — and when it names
/// a task the board no longer shows, so a resort or a task's own state
/// change never strands the cursor on a row that is gone.
///
/// Row ids alone, rather than the rows themselves: a cursor move needs an
/// ordered list and nothing else, and the list it is given is the one the
/// last frame composed, rows scrolled out of view included — see
/// [`Board::drawn`].
fn shift_cursor(ids: &[String], current: Option<&str>, delta: i32) -> Option<String> {
    if ids.is_empty() {
        return None;
    }
    let next = match current.and_then(|id| ids.iter().position(|row| row == id)) {
        Some(at) => {
            let len = ids.len() as i32;
            (((at as i32 + delta) % len) + len) % len
        }
        None => 0,
    };
    Some(ids[next as usize].clone())
}

/// Where the cursor lands once a `u` chain leaves the table together — the
/// nearest row `↓` would reach that survives the whole carry, not just the
/// row directly beneath the chain's head. Walks [`shift_cursor`] forward
/// until it lands outside `chain`, or wraps back onto the chain's own head,
/// at which point nothing in the table survives and the cursor clears —
/// see [`Board::on_key_unqueue_confirm`].
fn next_cursor_after_chain(ids: &[String], chain: &[ChainEntry]) -> Option<String> {
    let head = chain.first()?.id.as_str();
    let removed: HashSet<&str> = chain.iter().map(|e| e.id.as_str()).collect();
    let mut cursor = head.to_string();
    loop {
        let next = shift_cursor(ids, Some(&cursor), 1)?;
        if !removed.contains(next.as_str()) {
            return Some(next);
        }
        if next == head {
            return None;
        }
        cursor = next;
    }
}

/// A row per task, whatever needs a person first.
///
/// The board's own reading of the queue, available to anything that wants the
/// same answer without the redraw — `spoolway queue list`, above all. Reads the
/// task files and the live lane list, and writes nothing.
pub fn rows(repo: &Repo, pipelines: &Pipelines) -> Result<Vec<Row>> {
    let tasks = repo.tasks()?;
    let graph = Graph::build(&tasks, &repo.archive_dir());
    let mux = crate::mux::backend(repo)?;
    let lanes = mux.list_lanes().unwrap_or_default();
    let ledger = crate::usage::read_cached(repo);
    // A plain snapshot, not the live board: it holds no memory of a task's
    // last stage between calls, so it has nothing to tell "just arrived"
    // apart from "genuinely queued" with, and keeps reading the latter —
    // see `Board::arrived`.
    build_rows(repo, &tasks, pipelines, &graph, &lanes, &ledger, None)
}

/// Everything [`build`] read and computed once, for [`paint`] to draw from
/// as many times as a key or an idle tick asks for a frame with no new
/// reading behind it — see the `board-reader-thread` decision this split
/// exists for. Carries the ticker's own RECENT lines forward too, since a
/// frame draws them from here rather than from a second place of its own.
struct Snapshot {
    holder: Option<u32>,
    rows: Vec<Row>,
    load_problems: Vec<crate::task::LoadProblem>,
    totals: BTreeMap<String, view::GroupTotal>,
    finishing: Option<usize>,
    used: BTreeMap<String, usize>,
    model_used: BTreeMap<String, usize>,
    agent_model: BTreeMap<String, Vec<String>>,
    active_jobs: Vec<crate::jobs::ActiveJob>,
    /// Every pipeline file edited since the running dispatcher loaded them —
    /// see [`crate::pipeline_snapshot::changed_since_start`]. Read here, off
    /// the key thread, rather than in the footer, since it reads every
    /// pipeline file.
    changed_pipelines: Vec<String>,
    recent: VecDeque<RecentEvent>,
}

impl Snapshot {
    /// What a board draws before its first real reading has ever landed, or
    /// in place of one a failed reading could not replace — the same
    /// tolerance a failed frame gets today.
    fn empty() -> Snapshot {
        Snapshot {
            holder: None,
            rows: Vec::new(),
            load_problems: Vec::new(),
            totals: BTreeMap::new(),
            finishing: None,
            used: BTreeMap::new(),
            model_used: BTreeMap::new(),
            agent_model: BTreeMap::new(),
            active_jobs: Vec::new(),
            changed_pipelines: Vec::new(),
            recent: VecDeque::new(),
        }
    }
}

/// [`Reader<Snapshot>`] needs this to fall back on when its lock is
/// poisoned, or when a reading fails before the very first one has ever
/// landed — see [`Reader::start`]. Identical to [`Snapshot::empty`], kept
/// as a trait impl only because the generic reader is written against it
/// rather than a `Snapshot`-specific method.
impl Default for Snapshot {
    fn default() -> Snapshot {
        Snapshot::empty()
    }
}

/// What [`build`] carries forward from one reading to the next — the
/// board's own memory, same as it always was, just no longer living on
/// `Board` itself. Owned by the [`Reader`] thread for a hosted board, so it
/// moves once per reading there rather than once per frame; owned by
/// `Board` itself for a board whose own tests call [`Board::frame`]
/// directly, with nothing asynchronous about it.
struct Memory {
    stages: BTreeMap<String, String>,
    arrived: BTreeMap<String, Instant>,
    frozen: BTreeMap<String, Option<i64>>,
    recent: VecDeque<RecentEvent>,
    /// Whether the queue as it stood at the first reading has been taken as
    /// the starting point. Without this every task already in flight is
    /// announced as news the moment the board opens.
    adopted: bool,
    jobs_next: Option<crate::jobs::ActiveJobsMemo>,
}

impl Memory {
    fn new() -> Memory {
        Memory {
            stages: BTreeMap::new(),
            arrived: BTreeMap::new(),
            frozen: BTreeMap::new(),
            recent: VecDeque::new(),
            adopted: false,
            jobs_next: None,
        }
    }
}

/// One reading: the task files, the lane list, the lock, the ledger, the
/// archive and the command runs — every one of them a dispatch-tab frame
/// used to pay for again on every key before this, now read once here and
/// drawn from by [`paint`] as many times as asked. `holder` is taken as a
/// plain argument rather than read in here with the rest, because a test
/// drives this against a `Phase` of its own choosing rather than a lock file
/// it would have to fake — see [`Board::frame`].
fn build(
    repo: &Repo,
    pipelines: &Pipelines,
    holder: Option<u32>,
    memory: &mut Memory,
) -> Result<Snapshot> {
    // `cached_queue`, not `repo.tasks_and_problems()`: the board's own
    // per-file byte cache, so a reading over an unchanged queue parses
    // nothing — see `cached_queue`. `tasks_and_problems` stays the
    // dispatcher pass's own uncached read (`dispatch.rs`), which must see a
    // just-written file immediately and runs far less often than a reading.
    let (tasks, load_problems, _parsed) = cached_queue(&repo.queue_dir())?;
    let graph = Graph::build(&tasks, &repo.archive_dir());
    let mux = crate::mux::backend(repo)?;
    let lanes = mux.list_lanes().unwrap_or_default();

    // The ticker sees the queue move: a stage that changed, or a task that
    // left the queue finished. A task entering the queue is already a new row
    // on the table above, so it earns no line here too.
    let now = chrono::Local::now().format("%H:%M").to_string();
    let mut current: BTreeMap<String, String> = BTreeMap::new();
    for task in &tasks {
        current.insert(task.id().to_string(), task.stage().to_string());
    }
    let queue = QueueView {
        repo,
        tasks: &tasks,
        graph: &graph,
        pipelines,
    };
    for (id, stage) in &current {
        // Names both the step the task left and the one it moved to — see
        // `arrival_event`. Clipped by `ticker` itself, so a long id or step
        // name ends in `…` rather than wrapping.
        if let Some(was) = memory.stages.get(id)
            && was != stage
        {
            push_recent(
                &mut memory.recent,
                arrival_event(&now, id, was, stage, &queue),
            );
            // Starts this row's grace clock: `build_rows`' state arm reads it
            // back to tell a task that just handed off, with no lane up for
            // it yet, apart from one genuinely out of workers.
            memory.arrived.insert(id.clone(), Instant::now());
        }
    }
    // Only for a task the last reading had and this one does not, so the
    // archive file is opened once per task that leaves, not once per reading.
    for (id, was) in &memory.stages {
        if !current.contains_key(id)
            && let Some(event) = finished_event(&now, id, was, &queue)
        {
            push_recent(&mut memory.recent, event);
        }
    }
    memory.stages = current;
    // Dropped alongside `stages` above rather than left to grow forever: a
    // task done or archived never clears its own entry, and the dispatcher
    // stays up for weeks (the same kind of leak `forget_dead_live_sessions`,
    // below, prunes for the session cache).
    let stages = &memory.stages;
    memory.arrived.retain(|id, _| stages.contains_key(id));

    // Read once and shared: the rows want it for the OUT column and the
    // footer wants it for the spend, and it is the largest file the board
    // opens.
    let ledger = crate::usage::read_cached(repo);
    let mut active_rows = build_rows(
        repo,
        &tasks,
        pipelines,
        &graph,
        &lanes,
        &ledger,
        Some(&memory.arrived),
    )?;
    // How many steps are still working with no dispatcher up to move them on
    // — `None` while one holds the lock, whose rows read exactly as they
    // always have. See `finish_settled`.
    let finishing = if holder.is_none() {
        Some(finish_settled(
            repo,
            &mut active_rows,
            &lanes,
            &mut memory.frozen,
        ))
    } else {
        memory.frozen.clear();
        None
    };

    // Archived tasks stay on the board, dimmed, only as long as their group
    // still has something in the queue — so the groups worth pulling from
    // the archive are exactly the ones already among the active rows.
    let active_groups: BTreeSet<String> =
        active_rows.iter().filter_map(|r| r.group.clone()).collect();
    let mut rows = active_rows;
    rows.extend(done_rows(repo, pipelines, &active_groups)?);
    rows.sort_by(|a, b| a.key().cmp(&b.key()));

    let totals = group_totals(&ledger, &rows);

    // Slots: how many live lanes each profile is paying for, against its cap
    // — or, where the model a profile's lanes are running has `slots` of its
    // own, how many that model is paying for against its own cap instead.
    // Owned as `String`s rather than the borrowed `&str`s `slots_used`
    // itself hands back, so a `Snapshot` can outlive the `tasks` and
    // `pipelines` those borrows are tied to.
    let SlotsUsed {
        agents,
        models,
        agent_model,
    } = slots_used(repo, &tasks, pipelines, &lanes);
    let used = agents
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let model_used = models
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let agent_model = agent_model
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.into_iter().map(str::to_string).collect()))
        .collect();

    // The job ledger's own rows — every enabled job, ordered by next firing
    // — memoised, so a reading over an unchanged calendar does not re-scan
    // it for every enabled job.
    let active_jobs = crate::jobs::active_jobs_cached(repo, &mut memory.jobs_next);
    let changed_pipelines = crate::pipeline_snapshot::changed_since_start(repo);

    // Taken before the clear below, which must not erase the very news this
    // reading is handing back — see `Memory::adopted`.
    let recent = memory.recent.clone();
    if !memory.adopted {
        memory.recent.clear();
        memory.adopted = true;
    }

    Ok(Snapshot {
        holder,
        rows,
        load_problems,
        totals,
        finishing,
        used,
        model_used,
        agent_model,
        active_jobs,
        changed_pipelines,
        recent,
    })
}

/// The reading [`build`] does, kept off the key thread by a thread of its
/// own — see the `board-reader-thread` decision. Before this, every key on
/// the dispatch tab waited for a full read of the project, two `git` and one
/// `herdr` process included, before its frame was drawn, and keys typed
/// behind a slow read queued up behind it. Started lazily on a board's first
/// hosted frame, which reads once before anything draws. From then on every
/// key and the dispatch tab's idle tick ask for one more reading, and a
/// frame draws from [`Reader::latest`] without waiting for it. A failed or
/// slow reading leaves [`Reader::latest`] holding whatever the last one
/// landed.
///
/// Generic over what a reading actually builds — the queue and routines
/// tabs read their own groups and branch the same way, through
/// `commands::queue`'s own `Reader<commands::queue::QueueSnapshot>`, rather
/// than a second copy of this thread. `T` only has to be `Default`, for the
/// empty value a poisoned lock or a reading that fails before the very
/// first one ever lands falls back to; everything else is read off
/// `build`, the one thing that differs between a board's own [`Snapshot`]
/// and the queue tab's.
pub(crate) struct Reader<T> {
    /// `None` once the owner has dropped this and the thread has gone with
    /// it — see [`Reader`]'s own `Drop`. Sending on a dropped receiver is
    /// also how the thread notices it is time to stop.
    wake: Option<mpsc::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// The last reading to land, paired with `generation` — the count of
    /// readings started so far as of the one that built it — so a caller
    /// that mutates its own copy of `T` directly between readings (the
    /// queue tab's own `finish_submit`, which edits `groups` the moment a
    /// submission lands) can tell a reading already in flight at that
    /// moment from one that actually started after it — see
    /// [`Reader::latest_with_generation`] and [`Reader::started_count`].
    snapshot: Arc<Mutex<(Arc<T>, u64)>>,
    /// How many readings have started and finished, and whether the last
    /// one to finish succeeded — see [`Readings`].
    readings: Arc<(Mutex<Readings>, std::sync::Condvar)>,
}

/// The reader thread's own count of its readings, kept so
/// [`Reader::wake_and_wait`] can wait for one that started after it was
/// called. Counting finished readings alone is not enough: a reading
/// already running when the call came in finishes next, and it may have
/// read the queue before whatever the caller is waiting to see was written.
struct Readings {
    started: u64,
    finished: u64,
    /// Whether the last reading to finish built a snapshot. A failed one
    /// leaves the snapshot before it standing.
    last_ok: bool,
}

impl<T: Send + Sync + Default + 'static> Reader<T> {
    /// Reads once, here, on the caller's own thread — so the very first
    /// hosted frame is never drawn from an empty reading, the same
    /// guarantee the inline call this replaces always gave, and with the
    /// same timing: a plain call, not a thread's first reading raced
    /// against whatever else on the machine is doing its own. `build` is
    /// called once right here for that reading, then moved onto the thread
    /// for every one after it — so whatever state it closes over (a board's
    /// own [`Memory`], say) carries over between readings exactly as a
    /// loop's own local would.
    pub(crate) fn start(mut build: impl FnMut() -> Option<T> + Send + 'static) -> Reader<T> {
        // Nothing stands in for an empty one on the very first reading —
        // there is no earlier one to keep instead.
        let initial = build();
        let last_ok = initial.is_some();
        let snapshot = Arc::new(Mutex::new((Arc::new(initial.unwrap_or_default()), 0)));
        let (wake_tx, wake_rx) = mpsc::channel::<()>();
        let readings = Arc::new((
            Mutex::new(Readings {
                started: 0,
                finished: 0,
                last_ok,
            }),
            std::sync::Condvar::new(),
        ));

        let thread_snapshot = Arc::clone(&snapshot);
        let thread_readings = Arc::clone(&readings);
        let handle = std::thread::spawn(move || {
            let (state, landed) = &*thread_readings;
            loop {
                if wake_rx.recv().is_err() {
                    // The owner dropped its sender: nothing will ever ask
                    // for another reading, so there is nothing left to wait
                    // for.
                    return;
                }
                // Keys typed while a reading was running collapse into the
                // one reading that follows it, never one per key.
                while wake_rx.try_recv().is_ok() {}
                let generation = if let Ok(mut state) = state.lock() {
                    state.started += 1;
                    state.started
                } else {
                    0
                };
                // A failed reading — the queue directory unreadable for an
                // instant — leaves the last one standing, the same
                // tolerance a failed frame always had.
                let fresh = build();
                let ok = fresh.is_some();
                if let Some(fresh) = fresh
                    && let Ok(mut guard) = thread_snapshot.lock()
                {
                    *guard = (Arc::new(fresh), generation);
                }
                if let Ok(mut state) = state.lock() {
                    state.finished += 1;
                    state.last_ok = ok;
                }
                landed.notify_all();
            }
        });

        Reader {
            wake: Some(wake_tx),
            handle: Some(handle),
            snapshot,
            readings,
        }
    }

    /// Ask for one more reading. Never blocks, and a wake with a reading
    /// already running is free: the thread drains every wake still waiting
    /// once that reading is done, rather than running one per wake.
    pub(crate) fn wake(&self) {
        if let Some(tx) = &self.wake {
            let _ = tx.send(());
        }
    }

    /// Whatever the last reading landed, cloned out from under the thread's
    /// own lock — cheap, since cloning an `Arc` is a refcount, not the rows
    /// behind it.
    pub(crate) fn latest(&self) -> Arc<T> {
        self.latest_with_generation().0
    }

    /// [`Reader::latest`], paired with the generation it landed on — see
    /// [`Reader`]'s own doc comment for what a caller needs that for.
    pub(crate) fn latest_with_generation(&self) -> (Arc<T>, u64) {
        self.snapshot
            .lock()
            .map(|guard| (Arc::clone(&guard.0), guard.1))
            .unwrap_or_else(|_| (Arc::new(T::default()), 0))
    }

    /// How many readings the thread has started so far, including one
    /// still running — what a caller compares a landed reading's own
    /// generation against to tell one that started before some edit of its
    /// own from one that actually started after it.
    pub(crate) fn started_count(&self) -> u64 {
        self.readings
            .0
            .lock()
            .map(|state| state.started)
            .unwrap_or(0)
    }

    /// Ask for one more reading and wait for a reading that started after
    /// this call to finish, rather than drawing from whatever is already
    /// there — [`Board::hosted_frame`]'s guarantee. An error when that
    /// reading failed, so the caller can leave its last frame on screen.
    pub(crate) fn wake_and_wait(&self) -> Result<Arc<T>> {
        let (state, landed) = &*self.readings;
        let poisoned = || anyhow::anyhow!("the reader thread panicked mid-reading");
        let guard = state.lock().map_err(|_| poisoned())?;
        // Every reading numbered above `started` begins after this line,
        // and they finish in order, so the first of them is done once
        // `finished` passes `started`.
        let wanted = guard.started + 1;
        drop(guard);
        self.wake();
        let guard = state.lock().map_err(|_| poisoned())?;
        drop(
            landed
                .wait_while(guard, |state| state.finished < wanted)
                .map_err(|_| poisoned())?,
        );
        self.last_reading()
    }

    /// [`Reader::latest`], or an error when the last reading to finish
    /// failed and left an older one standing — so [`Board::hosted_frame`]
    /// can leave its last frame on screen, as a failed frame always did.
    pub(crate) fn last_reading(&self) -> Result<Arc<T>> {
        let (state, _) = &*self.readings;
        let ok = state.lock().map(|state| state.last_ok).unwrap_or(false);
        if !ok {
            anyhow::bail!("the reader could not read just now; the last frame stays on screen");
        }
        Ok(self.latest())
    }
}

impl Reader<Snapshot> {
    /// [`Reader::start`], seeded the way the dispatch tab's board always
    /// has: a fresh [`Memory`], closed over so it carries its caches
    /// forward between readings the same way a loop's own local would, and
    /// [`build_now`] run against `repo` and `pipelines` for as long as this
    /// reader lives.
    fn for_board(repo: Repo, pipelines: Pipelines) -> Reader<Snapshot> {
        let mut memory = Memory::new();
        Reader::start(move || build_now(&repo, &pipelines, &mut memory))
    }
}

impl<T> Drop for Reader<T> {
    /// Drops the sending half first, so the thread's blocking `recv` wakes
    /// with an error and returns on its own — then waits for it to, so the
    /// thread never outlives the owner whose repo it was reading. Bounded by
    /// at most one reading already in flight: the thread checks for the drop
    /// only between readings, never partway through one.
    fn drop(&mut self) {
        self.wake.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// [`build`], reading the real lock file itself rather than taking `holder`
/// as an argument — the one caller that always wants it, both on
/// [`Reader::start`]'s first, synchronous reading and on every one its
/// thread does after. `None` on failure — the queue directory unreadable
/// for an instant — so a caller can leave the last good reading standing
/// rather than take the whole board down with an empty one, the same
/// tolerance a failed frame gets today.
fn build_now(repo: &Repo, pipelines: &Pipelines, memory: &mut Memory) -> Option<Snapshot> {
    let holder = crate::lock::Lock::holder(&repo.lock_file()).unwrap_or(None);
    build(repo, pipelines, holder, memory).ok()
}

/// Everything [`build`] already read and computed, painted as text — no
/// reading of its own, so a key or an idle tick can call this as many times
/// as it likes over the same [`Snapshot`], paying only for the string work.
/// `cursor` is live rather than carried in the snapshot, since moving it is
/// the one thing a key does that never needs a fresh reading behind it —
/// see [`Board::apply`]. `recent` is the board's own ticker, copied off the
/// snapshot once when the board folded it in.
fn paint(
    repo: &Repo,
    pipelines: &Pipelines,
    dispatching: bool,
    snapshot: &Snapshot,
    cursor: Option<&str>,
    recent: &VecDeque<RecentEvent>,
    name: Option<&str>,
) -> String {
    paint_at(
        repo,
        pipelines,
        dispatching,
        snapshot,
        cursor,
        recent,
        name,
        pane_height(),
    )
}

/// [`paint`], against a pane `height` rows tall rather than the terminal's
/// own — so a test can draw a full board in a pane of the height it names.
/// An empty snapshot is the exception: it goes to [`paint_empty`], which
/// still measures the terminal itself and ignores `height`.
#[allow(clippy::too_many_arguments)]
fn paint_at(
    repo: &Repo,
    pipelines: &Pipelines,
    dispatching: bool,
    snapshot: &Snapshot,
    cursor: Option<&str>,
    recent: &VecDeque<RecentEvent>,
    name: Option<&str>,
    height: Option<usize>,
) -> String {
    let phase = Phase::Watching {
        holder: snapshot.holder,
        dispatching,
    };
    // Borrowed back out to `&str` keys for `footer`, which wants the same
    // shape `slots_used` once handed this function directly — see `build`'s
    // own note on why the snapshot itself owns `String`s instead.
    let used: BTreeMap<&str, usize> = snapshot
        .used
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    let model_used: BTreeMap<&str, usize> = snapshot
        .model_used
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    let agent_model: BTreeMap<&str, Vec<&str>> = snapshot
        .agent_model
        .iter()
        .map(|(k, v)| (k.as_str(), v.iter().map(String::as_str).collect()))
        .collect();

    // ---- the frame ----
    let mut frame = String::new();
    // No pass clock, and no run clock either. Neither is a figure a person
    // can act on: when the next pass is due changes nothing they would do,
    // and how long the run has been up only ever said the board was alive —
    // which is not worth the room the version now takes. The spool beside
    // the wordmark turns while a lane is running or starting — with no
    // dispatcher up, while a step is still working; with nothing running the
    // board holds still, and a still board over a still queue is the truth
    // rather than something to animate over.
    let installed = crate::release::installed_newer();
    let published = published_newer(
        repo.config.housekeeping.update_check,
        std::env::var_os(crate::release::ENV_SKIP).is_some(),
        crate::release::published_newer,
    );
    let version = version_label(installed.as_deref(), published.as_deref());
    let available = available_label(installed.as_deref(), published.as_deref());
    let header = header_cells(phase, snapshot.finishing, version);
    let pane = pane_width();
    if snapshot.rows.is_empty() {
        return paint_empty(
            &header.join(" · "),
            available.as_deref(),
            pane,
            phase,
            recent,
            name,
        );
    }
    // One blank row before the lockup, so its ascenders have a margin to sit
    // in rather than landing flush on the pane's own top row. The row is
    // pushed here rather than by `board_masthead`, which draws only the
    // lockup and the header, as the `masthead` behind `init`'s banner does.
    // Inside bare `spoolway`'s dispatch tab the blank row under the tab strip
    // is that margin already, and a second one would push the board a row
    // lower than the mockup draws it.
    if crate::screen::shell::hosted().is_none() {
        frame.push('\n');
    }
    // The spool only turns while something on the board reads `Running` — a
    // lane, a command step, or a row still inside its own handoff grace
    // window — or `Starting`, a lane the dispatcher is booting, so a queue
    // with nothing to do, and nothing about to, prints the still mark
    // instead. A mid-handoff row turning the spool with
    // neither a lane nor a command step behind it is that third case working
    // as the mockup intends, not an animation with nothing behind it.
    //
    // With no dispatcher up it turns on the steps still working instead, the
    // same count the header gives, so it stops once the last of them does.
    let running = match snapshot.finishing {
        Some(n) => n > 0,
        None => logo_turns(&snapshot.rows),
    };
    frame.push_str(&tint_available(
        &board_masthead(&header.join(" · "), pane, spool_frame(running)),
        available.as_deref(),
    ));
    frame.push('\n');

    // Built before anything under it is pushed, although it is drawn first,
    // because how many rows it gets is what the rest of the frame leaves.
    let (table, layout) =
        table_laid_out(&snapshot.rows, Style::board(pane), &snapshot.totals, cursor);

    // A queue file that would not parse is skipped rather than freezing the
    // board — see [`crate::task::load_dir`] — and named here so the fix is
    // visible on the frame itself, not only in the log.
    let mut problems = String::new();
    for problem in &snapshot.load_problems {
        let name = problem
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("a queue file");
        problems.push_str(&format!(
            " {AMBER}⚠ {name} does not parse and was skipped{RESET}\n"
        ));
    }

    // Built before the table and the ticker although it is printed after
    // them, because the rows they share are what is left once this is counted.
    let mut tail = String::new();
    // The rule under the board, cut to the pane rather than wrapped: a rule
    // that wraps is two rules, and the second one lands where the footer goes.
    let rule = "─".repeat(60.min(pane.saturating_sub(1)));
    tail.push_str(&format!("\n {DIM}{rule}{RESET}\n"));
    for line in footer(
        repo,
        pipelines,
        &used,
        &model_used,
        &agent_model,
        &snapshot.active_jobs,
        &snapshot.changed_pipelines,
    ) {
        tail.push_str(&format!(" {line}\n"));
    }
    // The key hint, last of all. The row keys are drawn whenever the board
    // has a row, whether or not that row can use them this frame; it says
    // what the board can do, not what it would do now. A board with no row
    // drops them in `paint_empty`.
    //
    // Built by `crate::screen::key_hint` — see that function's own doc
    // comment for why this is the one place left to build it, rather than
    // spelling it out by hand — and, now that the cursor reaches the first
    // row on its own, without naming `↑↓`: every screen this project draws
    // leaves the arrows and `q` off its own key line, since a person reads
    // those the same way everywhere.
    //
    // `q` joins it only inside bare `spoolway`'s dispatch tab, the one place
    // it quits — see `crate::screen::shell::quit_hint` — and so does `enter`,
    // the one place it starts or stops dispatching.
    let keys = [
        &[enter_hint(phase)][..],
        [
            ("o", "open task"),
            ("p", "pause task"),
            ("r", "resume"),
            ("s", "restart"),
            ("u/U", "unqueue / all"),
        ]
        .as_slice(),
        crate::screen::shell::quit_hint(),
    ]
    .concat();
    let keys = crate::screen::key_hint(&keys);

    // The rows the table's body and RECENT share: everything under the
    // column header but the parse warnings, the footer, and the blank row
    // and key line pushed below. One more is held back for the reason
    // `clamp_rows` gives. Hosted, the count comes out the same: `boxed`
    // gives the body two rows fewer than `height`, the key line goes under
    // the box, and the blank row above it sits inside the box.
    let header_rows = frame.lines().count() + 1;
    let body_rows = table.lines().count() - 1;
    let rows = height.map(|height| {
        height
            .saturating_sub(1)
            .saturating_sub(header_rows)
            .saturating_sub(problems.lines().count())
            .saturating_sub(tail.lines().count())
            .saturating_sub(2)
    });
    // Only RECENT gives up rows to the table, and only the table scrolls:
    // the lockup, the slots and the jobs keep theirs. Only a pane too short
    // for them alone loses any, to `clamp_rows` below. RECENT
    // is drawn only when the whole table fits, in whatever rows it leaves.
    // Otherwise the table takes every row, its last one the marker saying
    // how many tasks are out of view.
    let ticker_rows = match rows {
        // Nothing to overflow: keep the ticker whole. See [`pane_height`].
        None => recent.len() + 2,
        Some(rows) => rows.saturating_sub(body_rows),
    };
    match rows {
        Some(rows) if body_rows > rows => {
            let at = cursor.and_then(|id| snapshot.rows.iter().position(|row| row.id == id));
            for line in table_window(&table, &layout, at, rows, pane) {
                frame.push_str(&line);
                frame.push('\n');
            }
        }
        _ => frame.push_str(&table),
    }
    frame.push_str(&problems);
    frame.push_str(&ticker(recent, pane, ticker_rows));
    frame.push_str(&tail);

    // Inside bare `spoolway`'s dispatch tab the board draws in an untitled box
    // the way the other three tabs draw theirs, with the key
    // line under the box rather than inside it. The blank row above the key
    // line stays inside, as the box's last row.
    if crate::screen::shell::hosted().is_some() {
        frame.push('\n');
        return boxed(&frame, &keys, pane, height);
    }
    frame.push_str(&format!("\n{keys}\n"));

    clamp_rows(&frame, height)
}

/// The key line's `enter` pair: what pressing it would do to the tab's own
/// dispatcher, given whether it has one up.
fn enter_hint(phase: Phase) -> (&'static str, &'static str) {
    match phase {
        Phase::Watching {
            dispatching: false, ..
        } => ("enter", "start dispatching"),
        Phase::Watching {
            dispatching: true, ..
        } => ("enter", "stop dispatching"),
    }
}

/// [`paint`]'s frame for a board with no rows: the header alone in the
/// top-right corner, then [`view::greeting_screen`] centred down the rest of
/// the pane, and a key line of `enter` and, inside the dispatch tab, `q`.
///
/// Everything else the busy board draws is left off, so an idle board reads
/// as idle at a glance: the rule, the slots lines, the jobs ledger, the
/// `pipelines` notice, hook failures and the parse warning. Each comes back
/// once the board has a row again. A queue file that fails to parse makes no
/// row, so a broken file on an otherwise empty queue is not named on the
/// board at all. The keys that act on a row go too, since there is no row to
/// act on.
///
/// RECENT is the one exception, and only while no dispatcher holds the lock
/// (`phase`'s `holder` is `None`): it is then the record of what the last
/// run did before it stopped.
///
/// The lockup holds frame 0: nothing on an empty board is running.
fn paint_empty(
    header: &str,
    available: Option<&str>,
    pane: usize,
    phase: Phase,
    recent: &VecDeque<RecentEvent>,
    name: Option<&str>,
) -> String {
    let hosted = crate::screen::shell::hosted().is_some();
    let height = pane_height();
    let mut frame = String::new();
    // The same top margin the busy board keeps — see `paint`.
    if !hosted {
        frame.push('\n');
    }
    frame.push_str(&tint_available(&board_header(header, pane), available));
    // The rows between the header and the key line. Hosted, `boxed` gives
    // the body `height - 2` rows, the header one of them. On its own the
    // frame also spends the top margin above, the key line, and the row
    // `clamp_rows` holds back so the cursor never scrolls the pane.
    let region = height.map(|height| match hosted {
        true => height.saturating_sub(3),
        false => height.saturating_sub(4),
    });
    let none = VecDeque::new();
    let recent = empty_board_recent(phase, recent, &none);
    let hour = chrono::Timelike::hour(&chrono::Local::now());
    frame.push_str(&greeting_screen(
        &greeting(hour, name),
        recent,
        pane,
        region,
    ));
    let keys = crate::screen::key_hint(
        &[&[enter_hint(phase)][..], crate::screen::shell::quit_hint()].concat(),
    );
    if hosted {
        return boxed(&frame, &keys, pane, height);
    }
    frame.push_str(&format!("{keys}\n"));
    clamp_rows(&frame, height)
}

/// The RECENT lines an empty board draws: `recent` while no dispatcher holds
/// the lock, and `none` while one does. See [`paint_empty`].
fn empty_board_recent<'a>(
    phase: Phase,
    recent: &'a VecDeque<RecentEvent>,
    none: &'a VecDeque<RecentEvent>,
) -> &'a VecDeque<RecentEvent> {
    match phase {
        Phase::Watching {
            holder: Some(_), ..
        } => none,
        Phase::Watching { holder: None, .. } => recent,
    }
}

/// The first word of git's `user.name` in `repo`, for an empty board's
/// greeting. Asked of git once per board — see [`Board::name`] — rather than
/// once per frame, which would start a process several times a second.
/// `git config` falls back to the global setting by itself; no git, a key
/// that is not set (git exits 1) or a blank value all read as no name.
fn git_first_name(repo: &Repo) -> Option<String> {
    repo.git(&["config", "user.name"])
        .ok()
        .and_then(|value| first_name(&value))
}

/// How many live lanes each agent profile is paying for, keyed by profile name.
///
/// A lane maps to a profile through the step that started it. Four things have
/// to hold before a session in the multiplexer is one of those lanes, and all
/// four are the pass's own tests — [`crate::dispatch::Dispatcher::pass`] counts
/// exactly this set against the cap, and a board that counted a different one
/// was describing a run that wasn't happening:
///
/// - It runs in a checkout of ours. A multiplexer is shared with the person
///   using it, and a lane name carries a `<task> · <step>` separator no task
///   or step id can hold, so a session they started in a worktree cut from
///   `fix/herdr-layout` is named after the branch — it neither parses as a
///   lane nor is meant to. Counting it anyway reads as `cloud slots 3/5` on a
///   run with one lane in flight.
/// - Its task is still queued. A pane can outlive the task it ran.
/// - Its task is not parked. A `paused` task's pane, or a `blocked` one whose
///   pipeline does not staff that step, is kept open for a person to read, and
///   the dispatcher gave the slot back when it did that — counted here it
///   would read as a full profile with nothing running in it, the board
///   saying no work can start while the dispatcher starts some. A staffed
///   `blocked` lane is not parked at all — see
///   [`crate::pipeline::Pipeline::blocked_is_staffed`] — and does count.
/// - It is on the task's own step. A lane whose task has moved on counts for
///   nothing, idle or `Working`: it is a finished session someone may still
///   type into, the same as one started by hand. The dispatcher applies the
///   same rule through [`crate::dispatch::lane_counts`], and so does this walk
///   for the parked check above.
///
/// `agent_model`, on its own, is wider than the other two: after the live
/// walk above it is widened again over every task's whole pipeline, so a
/// profile whose model carries its own `slots` gets a footer line as soon as
/// some task's pipeline names it, not only once a lane is actually open on
/// it. `agents` and `models` stay live-only — the footer's `used` and
/// `model_used` must keep agreeing with the dispatcher about what is
/// actually running.
///
/// A profile's entry holds every distinct model name its steps — live or
/// queued — name, in the order first seen, rather than only the first: a
/// pipeline may run two pooled models on the same profile's different agent
/// steps, and `footer` draws one line per name here, not one for the whole
/// profile.
fn slots_used<'a>(
    repo: &Repo,
    tasks: &[crate::task::Task],
    pipelines: &'a Pipelines,
    lanes: &[crate::mux::Lane],
) -> SlotsUsed<'a> {
    let step_ids = pipelines.all_step_ids();
    let mine = crate::dispatch::our_checkouts(repo, tasks);
    let mut out = SlotsUsed::default();
    for lane in lanes {
        if !crate::dispatch::owns_cwd(&mine, &lane.cwd) {
            continue;
        }
        let Some((step_id, task_id)) = crate::mux::parse_lane_name(&lane.name, &step_ids) else {
            continue;
        };
        let Some(task) = tasks.iter().find(|t| t.id() == task_id) else {
            continue;
        };
        let Ok(pipeline) = pipelines.for_task(task) else {
            continue;
        };
        if !crate::dispatch::lane_counts(task, pipeline, step_id, repo.unattended()) {
            continue;
        }
        let Some(step) = pipeline.step(step_id) else {
            continue;
        };
        if let Some(agent) = step.agent.as_deref() {
            *out.agents.entry(agent).or_default() += 1;
            // A model's own cap is counted separately from its profile's: a
            // server holding one set of weights at a time is the thing being
            // rationed, not the binary that talks to it.
            if let Some(model) = step.model.as_deref().filter(|m| !m.trim().is_empty()) {
                *out.models.entry(model).or_default() += 1;
                let models = out.agent_model.entry(agent).or_default();
                if !models.contains(&model) {
                    models.push(model);
                }
            }
        }
    }

    // Widen `agent_model` past the live lanes above with every agent step of
    // every task's own pipeline, whatever stage that task sits on. A profile
    // whose model carries its own `slots` is then a pool the footer can show
    // as soon as some task's pipeline routes onto it, rather than only once a
    // lane is actually open — see the footer section of `docs/dispatcher.md`
    // for what a slots line means. A model a live lane above already named is
    // skipped rather than pushed again — the live lane is the truth about
    // what is actually running, already first in the list, and a step that
    // has since moved the model on must not un-count it or duplicate its
    // line.
    for task in tasks {
        let Ok(pipeline) = pipelines.for_task(task) else {
            continue;
        };
        for step in &pipeline.steps {
            if step.kind() != crate::pipeline::StepKind::Agent {
                continue;
            }
            let Some(agent) = step.agent.as_deref() else {
                continue;
            };
            let Some(model) = step.model.as_deref().filter(|m| !m.trim().is_empty()) else {
                continue;
            };
            let models = out.agent_model.entry(agent).or_default();
            if !models.contains(&model) {
                models.push(model);
            }
        }
    }
    out
}

/// What the run's live lanes are consuming, counted two ways.
///
/// A lane maps to a profile, and to a model, through the step that started it.
/// Both counts come off the same walk because they must agree about which
/// lanes are ours and which are parked — counting them apart is how the board
/// ends up saying a profile is full while the dispatcher starts another lane.
#[derive(Debug, Default)]
struct SlotsUsed<'a> {
    /// Live lanes per agent profile, against its `concurrency`.
    agents: BTreeMap<&'a str, usize>,
    /// Live lanes per model, against that model's own `slots`.
    models: BTreeMap<&'a str, usize>,
    /// Every distinct model each profile is running or queued to run — a live
    /// lane's models first, in the order their lanes were found, then any
    /// further model named by some task's own pipeline for that profile's
    /// step. See [`slots_used`].
    agent_model: BTreeMap<&'a str, Vec<&'a str>>,
}

/// Whether the masthead's spool turns this frame: while any row reads
/// `Running`, or `Starting` — the dispatcher is working that task too, only
/// its agent is not up yet.
fn logo_turns(rows: &[Row]) -> bool {
    rows.iter()
        .any(|row| matches!(row.state, State::Running | State::Starting))
}

/// Where `task` goes once `step_id` passes, for the NEXT column of a row
/// working that step — a running one, or a starting one whose boot mark
/// names it.
///
/// One step ahead and no further. The whole remaining chain is a fact about
/// the pipeline, which does not change and is one `spoolway pipeline show`
/// away; what moves — and so what is worth a column that redraws every
/// second — is where this task goes when the step it is on passes.
///
/// A gated step used to read its lane's question out of `## Blocker` here.
/// There is no question: a gated lane works and reports like any other, and
/// the waiting happens after it, on `paused`, which has a row of its own.
///
/// A `gate_at` naming this step is `s`'s own schedule, still live: the step
/// it names is where it is headed regardless of what the pipeline's own
/// `on_pass` would otherwise carry it to, since the task is about to be
/// parked there the moment this step finishes — `commands::report` for an
/// agent step's report, the dispatcher for a command step's exit — whatever
/// the outcome, not only a pass.
///
/// Every step named here is the one the move lands on, past any step the
/// task walks past — see [`landed`]. `dependents` is
/// [`crate::dispatch::same_group_dependents`]'s count for `task`.
fn onward(
    task: &crate::task::Task,
    pipeline: &crate::pipeline::Pipeline,
    step_id: &str,
    dependents: usize,
) -> String {
    if task.front.gate_at.as_deref() == Some(step_id) {
        format!("→ paused after {step_id}")
    } else if step_id == crate::pipeline::BLOCKED {
        // `blocked` names no `on_pass` of its own, and its resumability is a
        // person's question, not a lane's — a staffed lane working it right
        // now reads the same destination a cleared block would, and nothing
        // else. Not resumable: a lane is already working this step, so there
        // is no `[r]` action to offer here.
        let target = crate::commands::cleared_block_target(task, pipeline, true);
        format!("→ {}", landed(pipeline, task, target, dependents))
    } else {
        match pipeline.next_running_step(step_id) {
            // Plain text, no colour: this string is clipped to the room the
            // pane has left, and a cut through an escape sequence dyes the
            // rest of the board. No arrival count here — see `arrivals` in
            // `build_rows`, which counts the step this task is *on* rather
            // than the one named here.
            Some(next) => format!("→ {}", landed(pipeline, task, next.to_string(), dependents)),
            None => "→ done".to_string(),
        }
    }
}

/// The step a move to `destination` writes as `task`'s stage: `destination`
/// itself, or the first step past it along `on_pass` that the task runs.
///
/// Every move that names a step for a task lands it there through
/// [`crate::dispatch::land_past_hidden`] — a report, a command step's exit,
/// a resume. A board that named the raw destination would point at a step
/// the task never stands on: a `last:` step below a chain's top, a `first:`
/// step off its root, or one its own `skip:` names.
fn landed(
    pipeline: &crate::pipeline::Pipeline,
    task: &crate::task::Task,
    destination: String,
    dependents: usize,
) -> String {
    crate::dispatch::land_past_hidden(pipeline, task, destination, dependents).destination
}

/// The NEXT column of a blocked row parked for a person, prefixed for the
/// column. Always `resume_target`, the step `spoolway resume` sends the task
/// back to, and the only way off a parked block — so the row names one step
/// whether or not the key is on offer yet.
///
/// The key, then the arrow, exactly like a paused row's own `next` below.
/// No command follows, because the key opens the step picker. The key shows
/// only when `resumable` actually offers it: a block still waiting on a
/// dependency or a busy lane of its own has no action here for `[r]` to
/// name, and gets the bare arrow to the same step.
///
/// A staffed block is not this row: an unblocker lane works it, and where its
/// pass goes is `cleared_block_target`, which [`onward`] draws.
///
/// The step named is where the resume lands, past any step the task walks
/// past — see [`landed`].
fn blocked_next(
    task: &crate::task::Task,
    pipeline: &crate::pipeline::Pipeline,
    resumable: bool,
    dependents: usize,
) -> String {
    let target = crate::commands::resume_target(task, pipeline);
    let target = landed(pipeline, task, target, dependents);
    match resumable {
        true => format!("[r] → {target}"),
        false => format!("→ {target}"),
    }
}

/// Where resuming a paused task would carry it, if there is an answer to
/// give — `None` only for a task hand-edited onto `paused` with neither
/// `paused_at` nor `parked_from` recorded, which is a step nothing here can
/// guess.
///
/// Two different roads out of `paused`, told apart the same way
/// `commands::report::resume_road` tells them apart, through
/// `commands::caught_at`: a pause raised from `blocked` itself resumes to
/// `cleared_block_target`, a caught block or loop-max (`Caught::Blocked`)
/// resumes straight to `blocked` — exactly where it would have landed
/// unheld — a command step's caught fail resumes by its own `on_fail`, and
/// everything else, a plain gated pass or a schedule's caught fail at an
/// agent step, resumes by the step's own `on_pass`. A `parked_from` with no
/// gate — a person's own keypress, or a lane `escalate_clock` gave up on —
/// names nothing to pass: `unpark` sends the task straight back onto that
/// exact step, so this names the step itself rather than whatever comes
/// after it.
///
/// A gate's resume lands past the steps the task walks past, so its target is
/// named where it lands — see [`landed`]. `unpark` lands nowhere but the step
/// it names, so a park's is not.
fn paused_next(
    task: &crate::task::Task,
    pipeline: &crate::pipeline::Pipeline,
    dependents: usize,
) -> Option<String> {
    if let Some(gated) = task.front.paused_at.as_deref() {
        let step = pipeline.step(gated)?;
        let caught = crate::commands::caught_at(task, gated);
        let target = match caught {
            None if task.front.blocked_from.as_deref() == Some(gated) => {
                crate::commands::cleared_block_target(task, pipeline, false)
            }
            Some(crate::commands::Caught::Blocked) => crate::pipeline::BLOCKED.to_string(),
            // A command step's held failure resumes down the `on_fail` its
            // exit code chose — see `resume_road`.
            Some(crate::commands::Caught::Fail)
                if step.kind() == crate::pipeline::StepKind::Command =>
            {
                step.destination(crate::pipeline::Outcome::Fail)
                    .map(str::to_string)?
            }
            _ => step
                .destination(crate::pipeline::Outcome::Pass)
                .map(str::to_string)?,
        };
        return Some(landed(pipeline, task, target, dependents));
    }
    task.front.parked_from.clone()
}

/// The word this pause caught, prefixed onto the arrow the NEXT column draws
/// in front of [`paused_next`]'s own target — `None` for a plain gated pass,
/// which reads exactly as it always has, and for a pause raised from
/// `blocked` itself, which is not a catch of anything to name.
fn paused_arrow(task: &crate::task::Task) -> Option<String> {
    let gated = task.front.paused_at.as_deref()?;
    match crate::commands::caught_at(task, gated)? {
        crate::commands::Caught::Fail => Some(format!("{gated} failed")),
        crate::commands::Caught::Blocked => Some(format!("{gated} {}", crate::pipeline::BLOCKED)),
        crate::commands::Caught::Pass => None,
    }
}

/// The step whose ledger lines answer OUT, COST and TIME for `task` — its
/// own `stage()` for every state but `paused`, which names no step any
/// pipeline declares and so matches no ledger line at all: `spent_at`,
/// `cost_at` and `lane_time_at` all filter on `entry.step`, and a lane is
/// banked under the real step it ran, never under `paused` itself.
///
/// Told apart the same two ways [`paused_next`] already reads: `paused_at`
/// for a gate, `parked_from` for a person's own keypress, an aborted
/// Escape, or a lane `escalate_clock` gave up on. `None` of either — a task
/// hand-edited onto `paused` — falls back to `stage()` unchanged, same as
/// every other state, which answers no lines and so no figures rather than
/// a wrong step's.
fn ledger_stage(task: &crate::task::Task) -> &str {
    if task.stage() != crate::pipeline::PAUSED {
        return task.stage();
    }
    task.front
        .paused_at
        .as_deref()
        .or(task.front.parked_from.as_deref())
        .unwrap_or_else(|| task.stage())
}

/// The rows themselves, from state already read. Split out so the board can
/// read the queue and the lane list once per frame and share both.
///
/// `grace` is [`Board`]'s per-task memory of when a row's stage last changed
/// — `Some` only from the live `Board::frame`, so a task fresh off a handoff
/// with no lane up yet reads `Running` rather than `Queued` for one ordinary
/// gap. `rows()`'s one-shot snapshot, and every test call here, pass `None`
/// and keep the plain `Queued` reading — there is no such memory to read.
///
/// Every argument is a distinct piece of state already read once per frame
/// — bundling them into a struct only to pass one reference would hide that.
#[allow(clippy::too_many_arguments)]
fn build_rows(
    repo: &Repo,
    tasks: &[crate::task::Task],
    pipelines: &Pipelines,
    graph: &Graph,
    lanes: &[crate::mux::Lane],
    ledger: &[crate::usage::Entry],
    grace: Option<&BTreeMap<String, Instant>>,
) -> Result<Vec<Row>> {
    // The dispatcher's own record of what it has started outside a lane. Built
    // once: it is a handle on a directory, and every task below asks it the
    // same question.
    let runs = crate::command_step::Runs::new(&repo.commands_dir());
    // Every step id any pipeline here declares, for parsing a lane name back
    // into the step and task it belongs to — the same lookup `slots_used`
    // and `spoolway resume` both already do, needed here for `lane_busy`.
    let step_ids = pipelines.all_step_ids();
    // Every session a live lane still backs this frame. `live_session` caches
    // one transcript reading per session process-wide, and nothing ever
    // dropped an entry once the lane behind it was gone — a dispatcher up for
    // weeks held thousands of dead ones (review finding 54). The set collected
    // here prunes the cache at the end of the reading.
    let mut live_sessions: HashSet<String> = HashSet::new();
    // Every task a live dispatcher is booting a lane for this frame, and the
    // step it is booting — empty with no dispatcher behind the lock, so a
    // mark a killed one left never pins a row on `starting`.
    let claims = crate::claim::live(repo);
    let mut rows: Vec<Row> = Vec::new();
    for task in tasks {
        let pipeline = pipelines.for_task(task)?;
        let step = pipeline.step(task.stage());
        // Read off the graph this frame already built, for every step NEXT
        // names: the `last:` rule needs the open tasks above this one.
        let dependents = crate::dispatch::same_group_dependents(repo, tasks, graph, task);
        // The step a boot mark names, when there is one. It is what STEP
        // reads for this row: a task coming off `queued` keeps that stage
        // on disk until its boot has returned, and the row should name the
        // step being booted rather than the queue it is leaving.
        let claimed = claims.get(task.id()).map(String::as_str);
        let shown_stage = claimed.unwrap_or(task.stage());
        // How many times the task has reached the step it is on, whichever
        // route carried it there each time. Computed once, ahead of the
        // match below, because it is a fact about the step a task sits on
        // and not about any one of the states that match branches out into.
        let arrivals = task.rounds_at(shown_stage);
        let lane = crate::mux::lane_name(task.stage(), task.id());
        // By name alone: a task's lane runs in its own worktree, so the
        // checkout path is no test of ownership here the way it is in a pass.
        let live_lane = lanes.iter().find(|l| l.name == lane);
        let live = live_lane.is_some();
        // One read of the transcript, for the three figures that come off it.
        let session = live
            .then(|| live_session(repo, ledger, &lane, &mut live_sessions))
            .flatten();

        // A command step's run is not a lane. It is a detached process the
        // dispatcher tracks under the project's own `commands/`, and the
        // multiplexer has never heard of it — so asking the lane list alone,
        // a task halfway through a three-quarter-hour `gate` read as
        // `queued`, the very word a task waiting for a worker slot gets. A
        // board of queued rows that do not move is a board saying the
        // dispatcher has stopped, which is the one thing it was not doing.
        //
        // `Runs::key` builds the string `mux::lane_name` builds, so `lane` is
        // already the key this asks about.
        let command_run = step
            .filter(|step| step.kind() == crate::pipeline::StepKind::Command)
            .filter(|_| runs.state(&lane) == crate::command_step::RunState::Running)
            .and(Some(&lane));

        // A `serial:` step this task has not started because another task's
        // run of it is still going — the same question
        // [`crate::dispatch::Dispatcher::run_command`]'s `Fresh` arm asks
        // before it will start one, over the same peers: every other task on
        // this pipeline, since a run's key names no pipeline of its own.
        let serial_holder = step
            .filter(|step| step.serial && step.kind() == crate::pipeline::StepKind::Command)
            .filter(|_| runs.state(&lane) == crate::command_step::RunState::Fresh)
            .and_then(|step| {
                runs.serial_holder(
                    &step.id,
                    tasks
                        .iter()
                        .filter(|peer| peer.id() != task.id())
                        .filter(|peer| {
                            pipelines
                                .for_task(peer)
                                .is_ok_and(|p| p.name == pipeline.name)
                        })
                        .map(|peer| peer.id()),
                )
            });

        // Parked in front of a person, rather than a lane spoolway is about to
        // start there — the one question that decides whether a task on
        // `blocked` reads as a row like any other running step or as a wait.
        let parked_on_blocked = task.stage() == crate::pipeline::BLOCKED
            && !pipeline.blocked_is_staffed(repo.unattended());

        // What happens to this task next. For one that is moving that is the
        // step it goes to; for one that is stuck it is whatever has to happen
        // before it moves at all, which is a person far more often than a step.
        let (state, next, resumable) = match step {
            // Ahead of every other arm, the task's own stage included: while
            // the mark stands the dispatcher is booting this step, whatever
            // the task file still says.
            _ if claimed.is_some() => (
                State::Starting,
                onward(task, pipeline, shown_stage, dependents),
                false,
            ),
            // The dispatcher's own states. `queued` names the dependency it is
            // held by, which is the only thing worth saying about a task there
            // — a fixed description said the same thing at every one of them.
            _ if task.stage() == crate::pipeline::QUEUED => {
                let dependency = crate::commands::dependency_note(graph, task.id());
                // Every task on `queued` reads `queued`, whatever it is
                // waiting for. A dependency that can never arrive — one
                // that is blocked, one that is paused, or a
                // cycle — used to be drawn apart, but the dispatcher has
                // never treated it apart: `graph.ready()` passes over any
                // task whose dependencies have not all finished, so a dead
                // wait and an ordinary one sit on the same step for the
                // same reason. The graph is not asked here at all, which
                // also retires the substring-matching that once decided
                // this — `starts_with("unreachable")`, `contains("cycle")`
                // over the rendered note made `waiting on: park-lifecycle`
                // report itself as a dependency cycle.
                //
                // The gate only ranks a candidate now, it does not drop one
                // — so a task it has ranked behind another group is not
                // held apart from every other task waiting on a worker
                // slot, and reads the same line the rest of them do.
                match dependency {
                    Some(d) => (State::Queued, d, false),
                    None => (
                        State::Queued,
                        "waiting for a worker slot to free up".to_string(),
                        false,
                    ),
                }
            }
            _ if parked_on_blocked => {
                // A block only offers the resume key once whatever put it
                // here is actually clear: every dependency finished, exactly
                // as `queued` demanded to let this task start at all, and no
                // lane of its own still mid-turn — resuming into a pane a
                // person or an agent is actively using would race it.
                let resumable = graph.ready(task.id()) && !lane_busy(lanes, &step_ids, task.id());
                (
                    State::Blocked,
                    blocked_next(task, pipeline, resumable, dependents),
                    resumable,
                )
            }
            // The step a pass would carry it to, not a description of what it
            // is waiting on — the same two conditions a block reads its
            // resumability by, dependencies and a busy lane, except for a
            // `p`/Escape park, whose own lane being busy does not count
            // against it below. Also except when nothing here names a real
            // step at all: no gate (`paused_at`) and no
            // `p`/`escalate_clock` park (`parked_from`) is exactly the shape
            // `park` leaves on a task still on `queued` — see `park`'s own
            // docs — and `resume_target` is the same answer `resume_road`
            // itself reads for that shape. Its `queued` means there is
            // no step of this task's own to check a dependency or a lane
            // against: `queued` is where that check belongs, and this row
            // goes straight back to it.
            None if task.stage() == crate::pipeline::PAUSED => {
                let never_started = task.front.paused_at.is_none()
                    && task.front.parked_from.is_none()
                    && crate::commands::resume_target(task, pipeline) == crate::pipeline::QUEUED;
                if never_started {
                    (State::Paused, "→ queued — [r] resumes it".to_string(), true)
                } else {
                    // A row parked by a person's own Escape or the board's
                    // `p` (`parked_from` set, `escalated: false`) is a
                    // different case from a gate: its own lane is expected
                    // to still be alive, typed into or working away, and `r`
                    // has to reach it anyway rather than wait for that lane
                    // to go quiet — see `dispatch::auto_restore_parked`,
                    // which does the same thing on its own once the lane is
                    // next seen `Working`. A gate (`paused_at` set, no
                    // `parked_from`) and an escalated park still follow the
                    // ordinary rule below unchanged — a person's own hand is
                    // expected to look at those, and `r` racing a lane still
                    // mid-turn there is exactly what `lane_busy` guards
                    // against.
                    let parked_by_a_person =
                        task.front.parked_from.is_some() && !task.front.escalated;
                    let resumable = graph.ready(task.id())
                        && (parked_by_a_person || !lane_busy(lanes, &step_ids, task.id()));
                    let target = paused_next(task, pipeline, dependents);
                    // The word this pause caught, ahead of the arrow — "review
                    // failed → e2e" rather than a bare "→ e2e" — so a caught
                    // fail or block never reads like the plain pass a gate
                    // always used to mean. `None` for that plain pass leaves the
                    // arrow exactly as it always drew.
                    let arrow = match paused_arrow(task) {
                        Some(label) => format!("{label} →"),
                        None => "→".to_string(),
                    };
                    let next = match (target, resumable) {
                        // A stop's own park: the next start resumes it on its
                        // own — see `resume_stop_parked` — so the row says
                        // that rather than offering a key. `r` still works.
                        (Some(step), _) if task.front.parked_by_stop => {
                            format!("{arrow} {step} — resumes when dispatching starts")
                        }
                        (Some(step), true) => format!("[r] {arrow} {step}"),
                        (Some(step), false) => format!("{arrow} {step}"),
                        // Nothing to name: `paused_at` names a step the
                        // pipeline no longer has, a gated step has no pass
                        // destination, or neither `paused_at` nor
                        // `parked_from` is set, as on a task an issue-tracking
                        // hook paused on `done`. The row must still say
                        // something, and `r` may resume at once with no picker.
                        (None, true) => "[r] resume".to_string(),
                        (None, false) => "no step named".to_string(),
                    };
                    (State::Paused, next, resumable)
                }
            }
            None if task.stage() == crate::pipeline::DONE => {
                (State::Queued, "finished".to_string(), false)
            }
            None => (
                State::Unknown,
                format!("unknown step `{}` — not in this pipeline", task.stage()),
                false,
            ),
            // A pane holding a permission prompt — herdr's own read of
            // `live_lane.status`, never inferred from silence and never
            // sticky: asked fresh every redraw, so the row flips back to
            // `Running` the instant the prompt is answered, with nothing
            // here to un-mark. Not resumable: the task has not stopped, and
            // the one thing to do about a live prompt is press a key in the
            // pane holding it, not reroute the task through `spoolway
            // resume`.
            Some(_) if live_lane.is_some_and(|l| l.status == crate::mux::LaneStatus::Blocked) => (
                State::Prompt,
                format!("press a key in pane `{lane}`"),
                false,
            ),
            Some(_) if serial_holder.is_some() => (
                State::Waiting,
                format!("serial: after {}", serial_holder.unwrap_or_default()),
                false,
            ),
            Some(step) => {
                // A handoff just landed and no lane is up for it yet — the
                // ordinary gap between `spoolway report` writing the new
                // stage and the dispatcher's next pass starting a lane
                // there. Indistinguishable on disk from a task genuinely out
                // of workers, so only the board's own memory of when this
                // row's stage last changed can tell the two apart; a task
                // still on `queued` has no such memory to consult in the
                // first place, since that arm above never reaches here.
                let mid_handoff = grace
                    .and_then(|arrived| arrived.get(task.id()))
                    .is_some_and(|since| since.elapsed() < HANDOFF_GRACE);
                let state = match live || command_run.is_some() || mid_handoff {
                    true => State::Running,
                    false => State::Queued,
                };
                (state, onward(task, pipeline, &step.id, dependents), false)
            }
        };
        // The lane's clock, which runs from the launch of the round in flight
        // — so it is the round's own time, and nothing has banked it yet.
        let elapsed = match live {
            true => task
                .front
                .launched_at
                .map(|launched| (chrono::Utc::now().timestamp() - launched).max(0)),
            false => None,
        };
        rows.push(Row {
            id: task.id().to_string(),
            group: task.front.group.clone(),
            issue_url: issue_url_of(task),
            after: stacked_after(repo, tasks, task),
            stage: shown_stage.to_string(),
            arrivals,
            pipeline: pipeline.name.clone(),
            state,
            depth: graph.depth(task.id()),
            // Mirrors `Candidate::steps_left`: the pipeline's own length less
            // one, less the step's raw index — comparable across pipelines of
            // different lengths, which a raw index alone is not.
            steps_left: pipeline.steps.len() as i32 - 1 - pipeline.priority(task.stage()),
            dependents: graph.dependents(task.id()),
            ctx: session
                .as_ref()
                .and_then(|session| percent_of(repo, session, step)),
            // While a lane is live the conversation itself is the better
            // answer, and the only one there is: the ledger banks a step when
            // it settles, by which time the task has moved on to the next one.
            out: match &session {
                Some(session) => Some(session.output),
                None => spent_at(ledger, task.id(), ledger_stage(task)),
            },
            // Same two sources as OUT, in the same order and for the same
            // reason: a running step is banked only once it settles, by which
            // time the task has moved on.
            cost: match &session {
                Some(session) => session.cost,
                None => cost_at(ledger, task.id(), ledger_stage(task)),
            },
            // Live goes by whether the lane itself is open, not by whether a
            // transcript could be read from it — a lane that has just
            // launched is live before it has written a byte, and its clock
            // still runs.
            // A command run's clock comes off its own pid file rather than
            // `launched_at`, which is the last *lane*'s launch and would date
            // a running `gate` to whatever agent step ran before it.
            lane_time: match (command_run, live) {
                (Some(key), _) => runs.elapsed(key).map(|ran| ran.as_secs() as i64),
                (None, true) => elapsed,
                (None, false) => lane_time_at(ledger, task.id(), ledger_stage(task)),
            },
            next,
            resumable,
        });
    }
    rows.sort_by(|a, b| a.key().cmp(&b.key()));

    forget_dead_live_sessions(&live_sessions);
    Ok(rows)
}

/// Whether any of `task_id`'s own lanes — wherever the pipeline last put one,
/// not necessarily the step it is parked on — is mid-turn right now.
///
/// A parked task's lane keeps the name of the step it paused or blocked at,
/// not `paused`/`blocked` itself: nothing ever starts a lane *at* either of
/// those, so building a lane name from the task's current stage — the way an
/// ordinary running row does — would look for a lane that can never exist.
/// Matched by task id alone instead, the same way `spoolway resume` frees a
/// stale one.
fn lane_busy(lanes: &[crate::mux::Lane], step_ids: &[&str], task_id: &str) -> bool {
    lanes.iter().any(|lane| {
        crate::mux::parse_lane_name(&lane.name, step_ids).is_some_and(|(_, id)| id == task_id)
            // Spelled out rather than `is_busy()`: resuming into a lane on a
            // permission prompt would race the prompt exactly as resuming
            // into a working one would race the turn, so `Blocked` keeps a
            // parked or paused task non-resumable too.
            && matches!(
                lane.status,
                crate::mux::LaneStatus::Working | crate::mux::LaneStatus::Blocked
            )
    })
}

/// The masthead's header cells, joined with ` · ` by [`paint`]: who is
/// dispatching and which build this is. `finishing` is [`finish_settled`]'s
/// count, `Some` exactly while no dispatcher holds the lock.
fn header_cells(phase: Phase, finishing: Option<usize>, version: String) -> Vec<String> {
    match phase {
        Phase::Watching {
            holder: Some(pid), ..
        } => {
            vec![
                "dispatcher running".to_string(),
                format!("pid {pid}"),
                version,
            ]
        }
        // Nothing is dispatching, so there is no pid to name — this
        // process's own would read as a dispatcher that is not there.
        Phase::Watching { holder: None, .. } => {
            let mut header = vec!["dispatcher stopped".to_string()];
            // Left out at zero: a stopped board with nothing still working
            // has nothing to count down.
            match finishing {
                Some(1) => header.push("1 step finishing".to_string()),
                Some(n) if n > 1 => header.push(format!("{n} steps finishing")),
                _ => {}
            }
            header.push(version);
            header
        }
    }
}

/// What a stopped board makes of the rows [`build_rows`] drew: every step
/// still working is counted, and every one that has finished reads
/// [`State::Finished`] instead. Returns the count, which is what the header's
/// `N steps finishing` and the spool's turning both read.
///
/// Only called while no dispatcher holds the lock. With one up a finished
/// step moves on within a pass, so the `Running` it reads for that moment is
/// honest; with none up it would read `Running` until someone started one,
/// with its clock still counting and the spool still turning over a board on
/// which nothing is working at all.
///
/// Working is herdr's own read of the lane — anything not settled, so
/// `Working`, a `Blocked` prompt and an `Unknown` all count — or a command
/// run still in flight. A lane that has settled, or a command run that has
/// exited, has finished: an exited run already reads `Queued` off
/// [`build_rows`], since no lane or running run backs it, so that state is
/// taken here too.
///
/// `frozen` is the board's memory of the TIME each finished step read the
/// first frame it was seen finished, by lane name, so its clock stops there
/// rather than going on counting from `launched_at` — the same kind of
/// memory `Board::arrived` keeps for the handoff grace. A lane that starts
/// working again, or leaves the board, drops its entry, so it counts live
/// once more.
fn finish_settled(
    repo: &Repo,
    rows: &mut [Row],
    lanes: &[crate::mux::Lane],
    frozen: &mut BTreeMap<String, Option<i64>>,
) -> usize {
    let runs = crate::command_step::Runs::new(&repo.commands_dir());
    let mut working = 0;
    let mut finished: BTreeSet<String> = BTreeSet::new();
    for row in rows.iter_mut() {
        if !matches!(row.state, State::Running | State::Prompt | State::Queued) {
            continue;
        }
        let lane = crate::mux::lane_name(&row.stage, &row.id);
        // The TIME this row reads if it has finished, before any frozen
        // figure: a lane's own `now - launched_at`, already on the row, or an
        // exited run's time since it started, which `build_rows` never put
        // there because it only times a run still in flight.
        let time = match lanes.iter().find(|l| l.name == lane) {
            Some(live) if live.status.is_settled() => row.lane_time,
            Some(_) => {
                working += 1;
                continue;
            }
            None => match runs.state(&lane) {
                crate::command_step::RunState::Running => {
                    working += 1;
                    continue;
                }
                crate::command_step::RunState::Exited(_) => {
                    runs.elapsed(&lane).map(|ran| ran.as_secs() as i64)
                }
                _ => continue,
            },
        };
        row.state = State::Finished;
        row.next = "moves on when dispatching starts".to_string();
        row.lane_time = *frozen.entry(lane.clone()).or_insert(time);
        finished.insert(lane);
    }
    frozen.retain(|lane, _| finished.contains(lane));
    working
}

/// What [`cached_archive`] last read, and the archive directory's own mtime
/// at that read — the one signal that changes when a task is filed into it,
/// whatever the platform.
struct ArchiveCache {
    dir: PathBuf,
    dir_mtime: SystemTime,
    tasks: Arc<Vec<crate::archive_index::Entry>>,
}

/// [`crate::archive_index::read_at`] over an archive directory, cached
/// process-wide and re-read only once the directory's own mtime moves.
///
/// A directory's mtime changes exactly when an entry is added to or removed
/// from it — a POSIX guarantee independent of any one file's own content —
/// and an archived task file is never rewritten in place once filed: nothing
/// edits a task already there, so that one signal is enough to know a re-read
/// is worth its cost. The re-read is of the archive index, one small file,
/// and never of the task files themselves, so a task being archived adds one
/// entry here however many are already held. Holding index entries rather
/// than whole tasks also keeps every archived task's full text out of memory.
/// This is the same shape [`crate::usage::read_cached`] takes for the ledger
/// beside it.
///
/// Returns a shared handle rather than an owned `Vec`, for the same reason
/// `read_cached` does: a warm cache the directory's mtime says is still
/// current hands back a clone of the `Arc` — a refcount bump — never a copy
/// of however many tasks are archived. Only the pass that finds the mtime
/// has moved pays to re-read the index, not every idle frame after.
fn cached_archive(dir: &Path) -> Result<Arc<Vec<crate::archive_index::Entry>>> {
    static CACHE: OnceLock<Mutex<Option<ArchiveCache>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let dir_mtime = std::fs::metadata(dir).and_then(|meta| meta.modified()).ok();

    if let Ok(mut guard) = cache.lock() {
        if let (Some(cached), Some(dir_mtime)) = (guard.as_ref(), dir_mtime)
            && cached.dir == dir
            && cached.dir_mtime == dir_mtime
        {
            return Ok(Arc::clone(&cached.tasks));
        }
        let tasks = Arc::new(crate::archive_index::read_at(dir)?);
        if let Some(dir_mtime) = dir_mtime {
            *guard = Some(ArchiveCache {
                dir: dir.to_path_buf(),
                dir_mtime,
                tasks: Arc::clone(&tasks),
            });
        }
        return Ok(tasks);
    }
    Ok(Arc::new(crate::archive_index::read_at(dir)?))
}

/// One queue file [`cached_queue`] has already parsed: the bytes it parsed
/// from, and what came of them — a [`Task`](crate::task::Task) on success, or
/// the message a [`LoadProblem`](crate::task::LoadProblem) would carry on
/// failure. Keeping the outcome rather than just the `Task` means a cache hit
/// on a file that does not parse still reports the same problem it did the
/// first time, instead of silently dropping it the second frame.
struct CachedQueueFile {
    bytes: String,
    outcome: Result<crate::task::Task, String>,
}

/// [`cached_queue`]'s own state between calls: the directory it last read,
/// and every file in it that is still on disk, keyed by path.
struct QueueCache {
    dir: PathBuf,
    files: HashMap<PathBuf, CachedQueueFile>,
}

/// Every queue file [`cached_queue_in`] has read the bytes of so far in this
/// process, with the thread that read it — test-only, the same shape as
/// [`crate::repo::runs_under`] and for the same reasons: scoped by path so
/// a test counts only its own queue, and by thread so it can tell its own
/// reads from the board's reader thread's.
#[cfg(test)]
static QUEUE_READS: Mutex<Vec<(PathBuf, std::thread::ThreadId)>> = Mutex::new(Vec::new());

/// How many files under `dir` [`cached_queue_in`] has read so far on the
/// calling thread — see [`QUEUE_READS`].
#[cfg(test)]
pub(crate) fn queue_reads_here_under(dir: &Path) -> usize {
    let here = std::thread::current().id();
    QUEUE_READS
        .lock()
        .map(|reads| {
            reads
                .iter()
                .filter(|(path, thread)| *thread == here && path.starts_with(dir))
                .count()
        })
        .unwrap_or(0)
}

/// [`crate::task::load_dir`] over the queue directory, parsing a file again
/// only when its bytes differ from the bytes this cached it from last — the
/// per-file sibling of [`cached_archive`]'s directory-wide one.
///
/// A queue file is rewritten in place on nearly every pass that touches
/// it — a stage move, a round banked, `touched` stamped — so, unlike the
/// archive, there is no single directory-mtime that tells "nothing in here
/// changed"; each file has to be compared on its own. The file is still read
/// in full on every call, same as
/// [`Repo::tasks_and_problems`](crate::repo::Repo::tasks_and_problems) does —
/// this only skips the YAML parse once the bytes just read are the ones
/// already parsed. That parse, not the read, is what scaled with the queue
/// and ran on every one-second frame.
///
/// A poisoned lock falls back to a plain, uncached [`crate::task::load_dir`]
/// and counts every file as freshly parsed, the same fallback shape
/// [`cached_archive`] takes — one frame paying full price rather than the
/// whole board failing to draw.
///
/// Returns the parsed tasks, the load problems for files that would not
/// parse — reported exactly as [`crate::task::load_dir`] reports them — and
/// how many files were freshly parsed this call, the count the acceptance
/// test reads rather than timing the call.
fn cached_queue(
    dir: &Path,
) -> Result<(Vec<crate::task::Task>, Vec<crate::task::LoadProblem>, usize)> {
    static CACHE: OnceLock<Mutex<Option<QueueCache>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));

    if let Ok(mut guard) = cache.lock() {
        let needs_reset = !matches!(guard.as_ref(), Some(cached) if cached.dir == dir);
        if needs_reset {
            *guard = Some(QueueCache {
                dir: dir.to_path_buf(),
                files: HashMap::new(),
            });
        }
        let state = guard.as_mut().expect("just set above");
        return cached_queue_in(dir, state);
    }
    let (tasks, problems) = crate::task::load_dir(dir)?;
    let parsed = tasks.len() + problems.len();
    Ok((tasks, problems, parsed))
}

/// [`cached_queue`]'s own logic, taking its cache state as a plain argument
/// rather than reaching into the process-wide static itself.
///
/// Split out so a test can drive it against a [`QueueCache`] of its own: the
/// static in `cached_queue` is one slot shared by every caller in the
/// process, and `Board::frame`'s own tests call `build` — so `cached_queue`
/// — from several tests that can run in parallel. One landing between two
/// calls of another resets that shared slot out from under it, which was
/// read as a reparse the test did not expect (review finding 1, caught by 40
/// runs of `status::` at `--test-threads=16` failing once).
fn cached_queue_in(
    dir: &Path,
    state: &mut QueueCache,
) -> Result<(Vec<crate::task::Task>, Vec<crate::task::LoadProblem>, usize)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // An empty queue is a normal state, not an error — matching
        // `load_dir`'s own handling of the same case.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            state.files.clear();
            return Ok((Vec::new(), Vec::new(), 0));
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };

    let mut seen = HashSet::new();
    let mut tasks = Vec::new();
    let mut problems = Vec::new();
    let mut parsed = 0usize;

    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        seen.insert(path.clone());

        // Read in full every call, same as `load_dir` — only the parse below
        // is conditional. A read error is wrapped and reported the same way
        // `Task::load` reports one, so `LoadProblem.error` reads identically
        // whichever of the two loaded the file. Counted here rather than
        // after the cache check below, since this is the one line that
        // actually touches the disk — see [`QUEUE_READS`].
        #[cfg(test)]
        if let Ok(mut reads) = QUEUE_READS.lock() {
            reads.push((path.clone(), std::thread::current().id()));
        }
        let bytes = match std::fs::read_to_string(&path)
            .with_context(|| format!("reading task file {}", path.display()))
        {
            Ok(bytes) => bytes,
            Err(e) => {
                problems.push(crate::task::LoadProblem {
                    path,
                    error: format!("{e:#}"),
                });
                continue;
            }
        };

        if let Some(cached) = state.files.get(&path)
            && cached.bytes == bytes
        {
            match &cached.outcome {
                Ok(task) => tasks.push(task.clone()),
                Err(error) => problems.push(crate::task::LoadProblem {
                    path: path.clone(),
                    error: error.clone(),
                }),
            }
            continue;
        }

        parsed += 1;
        let outcome = crate::task::Task::parse(path.clone(), &bytes)
            .with_context(|| format!("parsing task file {}", path.display()))
            .map_err(|e| format!("{e:#}"));
        match &outcome {
            Ok(task) => tasks.push(task.clone()),
            Err(error) => problems.push(crate::task::LoadProblem {
                path: path.clone(),
                error: error.clone(),
            }),
        }
        state.files.insert(path, CachedQueueFile { bytes, outcome });
    }

    // Files no longer on disk are dropped here rather than left to grow the
    // cache forever — the same pruning `forget_dead_live_sessions` does for
    // the session cache beside this one.
    state.files.retain(|path, _| seen.contains(path));

    tasks.sort_by(|a, b| a.front.id.cmp(&b.front.id));
    problems.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((tasks, problems, parsed))
}

/// The other group `task`'s own group stacks on, if any — the bare group its
/// `depends_on` crosses into, only ever set when `task` is its own group's
/// first task (no dependency inside its own group). `None` for a task with
/// no group, one with an in-group dependency (not first), or one whose
/// `depends_on` never leaves its own group — read by the board's group band
/// to draw `after <group>`, mirroring the shape `commands::queue::
/// check_dependencies_set` already refuses anything else than.
fn stacked_after(
    repo: &Repo,
    tasks: &[crate::task::Task],
    task: &crate::task::Task,
) -> Option<String> {
    let mine = crate::commands::bare_group(repo, task)?;
    let group_of = |id: &str| -> Option<String> {
        tasks
            .iter()
            .find(|t| t.id() == id)
            .and_then(|t| crate::commands::bare_group(repo, t))
    };
    let has_in_group_dep = task
        .front
        .depends_on
        .iter()
        .any(|dep| group_of(dep).is_some_and(|g| g == mine));
    if has_in_group_dep {
        return None;
    }
    task.front
        .depends_on
        .iter()
        .find_map(|dep| group_of(dep).filter(|g| g != &mine))
}

/// The issue URL a task carries as `url:`, or `None` when it carries none —
/// `key-in-names` owns storing it, off the tracker hook's `open` answer. The
/// board reads it only to point the group band's hyperlink somewhere.
fn issue_url_of(task: &crate::task::Task) -> Option<String> {
    let url = task.extra_str("url");
    (!url.is_empty()).then(|| url.to_string())
}

/// Every task whose file has moved to the project's own `archive/`, as
/// dimmed `● done` rows — but only for a group named among `active_groups`. A
/// group with nothing left in the queue is not on the board at all, so its
/// archived tasks are not either: this is what keeps a finished group from
/// lingering on the board forever.
fn done_rows(
    repo: &Repo,
    pipelines: &Pipelines,
    active_groups: &BTreeSet<String>,
) -> Result<Vec<Row>> {
    let archived = cached_archive(&repo.archive_dir())?;
    Ok(archived
        .iter()
        .filter_map(|entry| {
            let group = Some(entry.group.clone()).filter(|group| !group.is_empty())?;
            if !active_groups.contains(&group) {
                return None;
            }
            Some(Row {
                id: entry.id.clone(),
                group: Some(group),
                issue_url: entry.url.clone(),
                // An archived task's own group has already landed, so the
                // board has nothing left to say about what it stacked onto —
                // `stacked_after` is not even asked, since `done_rows` never
                // holds the live queue this would need to look a dependency
                // up in.
                after: None,
                stage: crate::pipeline::DONE.to_string(),
                // A task is archived at `done`, which is not a step, so no
                // launch is ever banked there and a done row draws no count.
                arrivals: 0,
                pipeline: archived_pipeline_name(
                    pipelines,
                    Some(entry.pipeline.as_str()).filter(|name| !name.is_empty()),
                ),
                state: State::Done,
                // An archived row's `Done` tier already puts it last within
                // its group — see `Row::key` — so none of the run-order tiers
                // beneath it are ever compared.
                depth: 0,
                steps_left: 0,
                dependents: 0,
                ctx: None,
                out: None,
                cost: None,
                lane_time: None,
                next: String::new(),
                resumable: false,
            })
        })
        .collect())
}

/// The name [`done_rows`] prints in its PIPELINE column: the pipeline an
/// archived task named, resolved the same way [`Pipelines::for_task`] would
/// — but never an error, and never a project default, since there is none
/// any more.
///
/// A live task's pipeline is read through `for_task`, which fails the whole
/// row-building pass if the name it carries is not one this project defines
/// any more — right for a task still running, since a pipeline it cannot be
/// read against is a project misconfigured, not a row that can be drawn
/// wrong. An archived task already finished under whatever pipeline it named,
/// possibly a run or two ago, so the same name going missing since is not a
/// misconfiguration to fail on — it is only a name nothing here can resolve,
/// and the row still has to print *something*, so it prints that name
/// verbatim instead. One predating `pipeline:` altogether named none at all,
/// and prints as much rather than a name it never carried.
fn archived_pipeline_name(pipelines: &Pipelines, declared: Option<&str>) -> String {
    match declared {
        Some(name) => pipelines
            .get(name)
            .map(|p| p.name.clone())
            .unwrap_or_else(|_| name.to_string()),
        None => "(none)".to_string(),
    }
}

/// `wall_s` banked for `task` at `stage`, every round of it summed — the same
/// shape as [`spent_at`] and [`cost_at`], for the same reason: a task that
/// came back round to a step has spent both rounds' time getting through it.
fn lane_time_at(ledger: &[crate::usage::Entry], task: &str, stage: &str) -> Option<i64> {
    let mut wall = None;
    for entry in ledger {
        if entry.task == task && entry.step == stage {
            *wall.get_or_insert(0) += entry.wall_s;
        }
    }
    wall
}

/// How full the conversation `session` holds has got, against the window of the
/// model `step` names.
///
/// The same arithmetic `dispatch::carried_session` decides on — a last turn
/// over the model's `context_window` — which is the whole point of the column:
/// the number on the board is the number the dispatcher will act on, so a row
/// nearing an enabled `session_reuse_ctx` is a row about to be given a fresh
/// session.
///
/// `None` where the model resolves to no window at all, which is a `[models]`
/// row missing rather than a session measured and found small.
fn percent_of(repo: &Repo, session: &Reading, step: Option<&crate::pipeline::Step>) -> Option<u64> {
    let window = crate::models::resolve(&repo.config.models, step?.model.as_deref()?)
        .price
        .map(|price| price.context_window)
        .filter(|window| *window > 0)?;
    Some(session.context * 100 / window as u64)
}

/// Output tokens banked for `task` at `stage`, every round of it summed, or
/// `None` where the step has never been paid for at all.
///
/// Not bounded to this run, unlike the footer: a task's spend at the step it
/// is sitting on is a fact about the task, and a run that picked the task up
/// again inherits what the last one already spent getting it there.
fn spent_at(ledger: &[crate::usage::Entry], task: &str, stage: &str) -> Option<u64> {
    let mut spent = None;
    for entry in ledger {
        if entry.task == task && entry.step == stage {
            *spent.get_or_insert(0) += entry.tokens.output;
        }
    }
    spent
}

/// What `task` has cost at `stage`, every round of it summed, or `None` where
/// nothing banked there carried a price at all.
///
/// Bounded to the step and not to the run, exactly as [`spent_at`] is: a task
/// that came back round to a step has spent both rounds getting through it, and
/// a run that picked the task up again inherits what the last one spent there.
/// An entry with no `cost_usd` is a lane nothing could price — a local worker —
/// and adds nothing rather than adding zero.
fn cost_at(ledger: &[crate::usage::Entry], task: &str, stage: &str) -> Option<f64> {
    let mut cost = None;
    for entry in ledger {
        if entry.task == task
            && entry.step == stage
            && let Some(usd) = entry.cost_usd
        {
            *cost.get_or_insert(0.0) += usd;
        }
    }
    cost
}

/// What the conversation `harvest` reads has spent that nothing has banked yet
/// — the step in flight's own output and its own bill, read before it settles.
///
/// The arithmetic is `dispatch::record_usage`'s, deliberately: what the board
/// shows while a step runs and what the ledger records when it ends are then
/// the same numbers, arrived at the same way, rather than figures that happen
/// to be close. Both rest on the ledger being the record of what was banked, so
/// it is also the check.
///
/// Subtracting is what makes a *reused* session honest here. A lane resumed on
/// the session its previous step ran under is reading a transcript that already
/// holds that step's turns, and reading the whole of it would put the earlier
/// step's output and bill on the running step's row — the same misreading at a
/// smaller scale as summing the task. A freshly minted session appears nowhere
/// in the ledger, so its delta is its total and this costs it nothing.
///
/// Both figures subtract the same banked total, so OUT and COST on a row are
/// one reading of one step rather than two windows onto it.
fn live_spend(
    ledger: &[crate::usage::Entry],
    session: &str,
    harvest: &crate::usage::Harvest,
) -> (u64, Option<f64>) {
    let mut banked = crate::usage::Tokens::default();
    let mut banked_cost = 0.0f64;
    for entry in ledger {
        if entry.session == session {
            banked.add(&entry.tokens);
            banked_cost += entry.cost_usd.unwrap_or(0.0);
        }
    }
    let unbanked = harvest.tokens.since(&banked);
    // The harvest priced each turn at its own tier, so what is left is the
    // part not banked before. Absent when some turn had no price, as the
    // ledger line banked for it will be.
    let cost = harvest.cost_usd.map(|total| (total - banked_cost).max(0.0));
    (unbanked.output, cost)
}

/// What one live lane's transcript says: how big its conversation has got, what
/// it has produced so far, and what that has cost.
#[derive(Clone, Copy)]
struct Reading {
    context: u64,
    /// Output tokens the step in flight has produced — what the transcript
    /// holds beyond what the ledger has already banked under this session, not
    /// the whole conversation's. See [`live_spend`].
    output: u64,
    /// `None` where the model that answered carries no price — a local worker,
    /// or a `[models]` row nobody has written.
    cost: Option<f64>,
}

/// What the board last read out of one session's transcript, and when.
struct Cached {
    /// Resolved once and kept: the lookup is a walk over every session the
    /// agent has ever written, which is far too much to pay per frame.
    path: Option<PathBuf>,
    mtime: Option<SystemTime>,
    reading: Option<Reading>,
    read_at: Instant,
}

/// What `lane`'s live conversation has come to, re-read at most every
/// [`CTX_REFRESH`] and only when the transcript has actually moved.
///
/// Both figures on one row come from this one read. Process-wide rather than
/// held by the [`Board`], because `queue list` asks the same question from the
/// same code and should not have to carry a cache to do it — a one-shot
/// command reads once and drops it with the process. A frame is drawn every
/// second and a transcript runs to megabytes; without this the board
/// would be the most expensive thing in a run that decides nothing.
///
/// `touched` collects every session id this frame still has a live lane for,
/// so [`forget_dead_live_sessions`] can drop the rest — a long-lived
/// dispatcher used to keep an entry for every lane it had ever seen (review
/// finding 54).
static LIVE_SESSION_CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();

fn live_session(
    repo: &Repo,
    ledger: &[crate::usage::Entry],
    lane: &str,
    touched: &mut HashSet<String>,
) -> Option<Reading> {
    // `lane_session_in`, not `lane_session`: the latter reads the whole
    // ledger fresh with no cache, which this call already holds as `ledger`
    // — see `build`'s own comment on why it is read once and shared.
    let (kind, session) = crate::dispatch::lane_session_in(repo, ledger, lane)?;
    touched.insert(session.clone());
    let cache = LIVE_SESSION_CACHE.get_or_init(Mutex::default);
    let mut cache = cache.lock().ok()?;

    let cached = cache.entry(session.clone()).or_insert_with(|| Cached {
        path: crate::usage::session_path(&kind, &session),
        mtime: None,
        reading: None,
        // Far enough back that the first ask is always a read.
        read_at: Instant::now() - CTX_REFRESH,
    });
    if cached.read_at.elapsed() < CTX_REFRESH {
        return cached.reading;
    }
    cached.read_at = Instant::now();

    // A path that did not resolve is looked for again, once per refresh: a
    // lane launched a moment ago has a session id before it has a transcript.
    if cached.path.is_none() {
        cached.path = crate::usage::session_path(&kind, &session);
    }
    let path = cached.path.clone()?;

    let own_subagents = crate::usage::lane_owns_subagents(ledger, &session);
    let mtime = if own_subagents {
        crate::usage::lane_touched_at(&path, &session)
    } else {
        crate::usage::touched_at(&path)
    };
    if mtime.is_some() && mtime == cached.mtime {
        return cached.reading;
    }
    cached.mtime = mtime;
    cached.reading =
        crate::usage::live_of(&kind, &session, own_subagents, &path, &repo.config.models).map(
            |live| {
                let (output, cost) = live_spend(ledger, &session, &live.harvest);
                Reading {
                    context: live.context,
                    output,
                    cost,
                }
            },
        );
    cached.reading
}

/// Drop every [`live_session`] cache entry whose session no longer backs a
/// live lane, called once at the end of each reading. Entries are small, but
/// nothing else ever removed one, so a dispatcher left up for weeks
/// accumulated one per lane it had ever drawn (review finding 54).
fn forget_dead_live_sessions(live: &HashSet<String>) {
    if let Some(cache) = LIVE_SESSION_CACHE.get()
        && let Ok(mut cache) = cache.lock()
    {
        cache.retain(|session, _| live.contains(session));
    }
}

/// What a RECENT line reads a move against: this reading's queue, its
/// dependency graph and the pipelines, exactly as [`build`] has them.
struct QueueView<'a> {
    repo: &'a Repo,
    tasks: &'a [crate::task::Task],
    graph: &'a Graph,
    pipelines: &'a Pipelines,
}

/// A RECENT event for a task that just moved from `was` to `stage`, worded
/// by [`view::sentence`] from what [`classify`] makes of the move.
fn arrival_event(now: &str, id: &str, was: &str, stage: &str, queue: &QueueView) -> RecentEvent {
    let change = match queue.tasks.iter().find(|t| t.id() == id) {
        Some(task) => classify(was, stage, task, queue),
        // `build` only asks about an id it read out of this same queue, so
        // this is not reached; a move with no task file to read is named
        // plainly rather than guessed at.
        None => Move::Left {
            from: was.to_string(),
            to: stage.to_string(),
        },
    };
    RecentEvent::Arrival {
        at: now.to_string(),
        id: id.to_string(),
        change,
    }
}

/// What a task did to move from `was` to `stage`, read off its task file and
/// pipeline as they stand once the move has landed.
///
/// The move is classified here, once, because which kind it is depends on
/// what the task file says at the moment the board sees it; [`view::sentence`]
/// only words the result. A cause is read only from a field that names the
/// step the move left, so a field left over from an earlier stop never names
/// this one's. Where the file does not say which road was taken — a field the
/// move itself cleared, or two roads that leave the same file behind — the
/// sentence goes without a cause rather than guess one.
///
/// A step's own word is `last_report`, and only while it names `was`: the one
/// slot is overwritten by every report, so a report naming any other step is
/// an earlier step's, and says nothing about this move. A report from an
/// earlier visit to `was` itself passes that test too. The file keeps no
/// arrival time to tell the two apart, so a lane that never reports on a
/// return visit is read by what it reported on the visit before.
fn classify(was: &str, stage: &str, task: &crate::task::Task, queue: &QueueView) -> Move {
    use crate::pipeline::{BLOCKED, PAUSED, QUEUED};

    let pipeline = queue.pipelines.for_task(task).ok();
    // An outcome this binary cannot parse is no word at all, and is read as
    // the absent report it then is.
    let report = task
        .front
        .last_report
        .as_ref()
        .filter(|r| r.step == was)
        .and_then(|r| r.outcome.parse::<Outcome>().ok());

    if stage == PAUSED {
        return into_paused(was, task, pipeline, report);
    }
    let to = stage.to_string();
    match (was, pipeline) {
        (QUEUED, _) => Move::Started { to },
        (PAUSED, _) => Move::Resumed { to },
        (BLOCKED, _) => Move::Unblocked {
            to,
            by_lane: report == Some(Outcome::Pass),
        },
        (_, Some(pipeline)) => off_a_step(was, stage, task, pipeline, report, queue),
        (_, None) => Move::Left {
            from: was.to_string(),
            to,
        },
    }
}

/// [`classify`] for a move into `paused`. The checks run in order, and the
/// first that matches wins.
fn into_paused(
    was: &str,
    task: &crate::task::Task,
    pipeline: Option<&crate::pipeline::Pipeline>,
    report: Option<Outcome>,
) -> Move {
    use crate::commands::Gate;
    use crate::pipeline::{BLOCKED, DONE, PAUSED, QUEUED, STARTED};

    let front = &task.front;
    let stopped = |cause| Move::Stopped {
        from: Some(was.to_string()),
        cause,
    };
    // `was`'s report, worded as its own verb: a gate or a schedule holds
    // whatever the step said, and a `--block` or `--fail` from `blocked`
    // parks the task the same way.
    let held = |cause: Option<Cause>| {
        let (from, to) = (was.to_string(), PAUSED.to_string());
        match report {
            Some(Outcome::Pass) => Move::Passed { from, to, cause },
            Some(Outcome::Fail) => Move::Failed { from, to, cause },
            Some(Outcome::Block) => Move::Reported {
                what: Reported::Block,
                from,
                to,
                cause,
            },
            _ => stopped(cause),
        }
    };

    // Nothing has started, so nothing but the dispatcher's own two checks
    // before a start, and a person's park, stops a task here. `started` is
    // a hook fired on the way out of `queued`, before the task moves.
    if was == QUEUED {
        let cause = if matches!(front.hook_paused.as_deref(), Some(QUEUED | STARTED)) {
            Cause::HookFailed
        } else if let Some(branch) = &front.missing_start_branch {
            Cause::BranchMissing(branch.clone())
        } else {
            Cause::Manually
        };
        return Move::Stopped {
            from: None,
            cause: Some(cause),
        };
    }

    // `done`'s own hook pauses a task already on `done`, so this move's own
    // `set_stage` wrote `arrived_from: done` — whether or not a reading ever
    // saw the task there.
    if front.hook_paused.as_deref() == Some(DONE) && front.arrived_from.as_deref() == Some(DONE) {
        return match finished_from(was, pipeline) {
            Some(step) => Move::Passed {
                from: step,
                to: PAUSED.to_string(),
                cause: Some(Cause::HookFailed),
            },
            None => stopped(Some(Cause::HookFailed)),
        };
    }

    // A gate names the step it held in `paused_at` — except a pass from
    // `blocked`, which names the step that pass stands in for instead.
    if let Some(by) = front.paused_by.as_deref()
        && (front.paused_at.as_deref() == Some(was)
            || (was == BLOCKED && report == Some(Outcome::Pass)))
    {
        let cause = if by == Gate::Step.as_str() {
            Some(Cause::Gate)
        } else if by == Gate::Schedule.as_str() {
            Some(Cause::Scheduled)
        } else {
            None
        };
        return held(cause);
    }

    if front.parked_from.as_deref() == Some(was) {
        let cause = if front.parked_by_stop {
            Cause::DispatchingStopped
        } else if front.escalated {
            Cause::Escalated
        } else {
            Cause::Manually
        };
        return stopped(Some(cause));
    }

    if was == BLOCKED {
        return match report {
            Some(Outcome::Pause) => Move::Reported {
                what: Reported::Pause,
                from: was.to_string(),
                to: PAUSED.to_string(),
                cause: None,
            },
            Some(Outcome::Block | Outcome::Fail) => held(None),
            // `Dispatcher::escalate`'s road off `blocked`, which reports
            // nothing and stamps the step to resume in `paused_at`.
            _ if front.paused_at.is_some() => stopped(Some(Cause::Escalated)),
            _ => stopped(None),
        };
    }

    stopped(None)
}

/// [`classify`] for a move off `was`, a step of `pipeline` — or `done`, for a
/// finished task held back from cleanup. The checks run in order, and the
/// first that matches wins.
fn off_a_step(
    was: &str,
    stage: &str,
    task: &crate::task::Task,
    pipeline: &crate::pipeline::Pipeline,
    report: Option<Outcome>,
    queue: &QueueView,
) -> Move {
    use crate::pipeline::{BLOCKED, DONE, StepKind};

    let front = &task.front;
    let (from, to) = (was.to_string(), stage.to_string());
    let spent = |destination| loop_spent(pipeline, task, destination);
    let into_blocked = stage == BLOCKED;

    // Cleanup's own hold: the task reached `done`, and the move off it wrote
    // `arrived_from: done`, whether or not a reading saw it there.
    if into_blocked
        && front.arrived_from.as_deref() == Some(DONE)
        && let Some(step) = finished_from(was, Some(pipeline))
    {
        return Move::Passed {
            from: step,
            to,
            cause: Some(Cause::UncommittedWork),
        };
    }
    let Some(step) = pipeline.step(was) else {
        return Move::Left { from, to };
    };
    // `tear_down_and_escalate`'s road into `blocked` marks the file
    // `escalated`, which no other road onto `blocked` does. A lane stopped
    // for going quiet leaves the same `blocked_from` and no report that a
    // lane dead at launch does, so without the mark it would read as a
    // launch that failed.
    if into_blocked && front.escalated {
        return Move::Escalated { from, to };
    }
    // Where each outcome lands the task, past the steps it walks past — the
    // stage a report or a command step's exit writes. Compared raw, a move
    // from `test` straight to `document` past a hidden `suite` matches
    // neither route and reads as a bare `left test`.
    let dependents =
        crate::dispatch::same_group_dependents(queue.repo, queue.tasks, queue.graph, task);
    let land = |outcome| {
        step.destination(outcome)
            .map(|destination| landed(pipeline, task, destination.to_string(), dependents))
    };
    let on_pass = land(Outcome::Pass);
    let on_fail = land(Outcome::Fail);
    let (on_pass, on_fail) = (on_pass.as_deref(), on_fail.as_deref());
    // The dispatcher's own walk-past still writes the raw `on_pass` — see
    // `crate::dispatch::fall_through` — and a failed lane start the raw
    // `on_fail` — see `Dispatcher::handle_boot_failure` — so a move either
    // made is read against that.
    let raw_pass = step.destination(Outcome::Pass);
    let raw_fail = step.destination(Outcome::Fail);

    // Ahead of the report: a launch refused this visit means no lane ran,
    // so a report naming `was` can only be an earlier visit's. The count
    // survives the move, since arriving clears only the step arrived at, and
    // stands below the ceiling only while a launch is still being retried.
    if let Some(&n) = front.launch_failures.get(was)
        && n >= crate::dispatch::MAX_LAUNCH_FAILURES
    {
        return Move::CouldNotLaunch {
            from,
            to,
            cause: Some(Cause::Attempts(n)),
        };
    }

    match report {
        Some(Outcome::Pass) => {
            return if on_pass == Some(stage) {
                Move::Passed {
                    from,
                    to,
                    cause: None,
                }
            } else if into_blocked && on_pass == Some(DONE) {
                // `report`'s own hold, for an agent step that routes
                // straight to `done`.
                Move::Passed {
                    from,
                    to,
                    cause: Some(Cause::UncommittedWork),
                }
            } else if into_blocked && let Some(limit) = spent(on_pass) {
                Move::Passed {
                    from,
                    to,
                    cause: Some(Cause::LoopLimit(limit)),
                }
            } else {
                Move::Left { from, to }
            };
        }
        Some(Outcome::Fail) => {
            return if on_fail == Some(stage) {
                Move::Failed {
                    from,
                    to,
                    cause: None,
                }
            } else if into_blocked && let Some(limit) = spent(on_fail) {
                Move::Failed {
                    from,
                    to,
                    cause: Some(Cause::LoopLimit(limit)),
                }
            } else {
                Move::Left { from, to }
            };
        }
        Some(Outcome::Block) if into_blocked => {
            return Move::Reported {
                what: Reported::Block,
                from,
                to,
                cause: None,
            };
        }
        Some(_) => return Move::Left { from, to },
        None => {}
    }

    // No report from `was`. The dispatcher's own walk-past first, asked of
    // the dispatcher itself, since a step walked past follows its `on_pass`
    // exactly as a pass does.
    if crate::dispatch::walks_past(queue.repo, queue.tasks, queue.graph, step, task) {
        return if raw_pass == Some(stage) {
            Move::Skipped {
                from,
                to,
                cause: None,
            }
        } else if into_blocked && let Some(limit) = spent(raw_pass) {
            Move::Skipped {
                from,
                to,
                cause: Some(Cause::LoopLimit(limit)),
            }
        } else {
            Move::Left { from, to }
        };
    }

    // A background run the task had already walked past, failing into its
    // own `on_fail` — named only when exactly one such step routes there,
    // and only when none of `was`'s own routes could have brought the task
    // here instead: the two roads leave the same file behind.
    let in_background =
        |s: &&crate::pipeline::Step| s.background && s.id != was && s.kind() == StepKind::Command;
    let mut background = pipeline
        .steps
        .iter()
        .filter(in_background)
        .filter(|s| s.on_fail.as_deref() == Some(stage));
    if let (Some(run), None) = (background.next(), background.next()) {
        let own = on_pass == Some(stage) || on_fail == Some(stage) || into_blocked;
        return match own {
            true => Move::Left { from, to },
            false => Move::FailedInBackground {
                from: run.id.clone(),
                to,
            },
        };
    }
    // `reap_stale_runs` sends a background failure through
    // `apply_loop_budget` too, so one whose `on_fail` has spent its `loop:`
    // lands on `blocked` instead — the same file a lane dead at launch or a
    // step's own spent loop leaves. Every cause below would be a guess.
    if into_blocked
        && pipeline
            .steps
            .iter()
            .filter(in_background)
            .any(|s| spent(s.on_fail.as_deref()).is_some())
    {
        return Move::Left { from, to };
    }

    // A command step never reports: its exit code routes it, and the route
    // it took is the only record of which way that went.
    if step.kind() == StepKind::Command {
        if on_pass == Some(stage) {
            return Move::Passed {
                from,
                to,
                cause: None,
            };
        }
        if on_fail == Some(stage) {
            return Move::Failed {
                from,
                to,
                cause: None,
            };
        }
        return match (into_blocked, spent(on_pass), spent(on_fail)) {
            (true, Some(limit), None) => Move::Passed {
                from,
                to,
                cause: Some(Cause::LoopLimit(limit)),
            },
            (true, None, Some(limit)) => Move::Failed {
                from,
                to,
                cause: Some(Cause::LoopLimit(limit)),
            },
            _ => Move::Left { from, to },
        };
    }

    // An agent step that sent no report: a pass needs one, so a move down
    // its `on_pass` is a step walked past — by the dispatcher, which writes
    // the raw `on_pass`.
    if raw_pass == Some(stage) {
        return Move::Skipped {
            from,
            to,
            cause: None,
        };
    }
    // A walk-past into a spent loop and a lane that never got going leave
    // the same file behind.
    if into_blocked && spent(raw_pass).is_some() {
        return Move::Left { from, to };
    }
    // Otherwise the lane never got going. Two roads lead here, and only one
    // leaves its mark: a pane that never reached its prompt clears its own
    // `launch_busy_since` on the way out, then takes `on_fail` as a refused
    // launch does; a lane launched and gone without a word is escalated
    // straight to `blocked` whatever `on_fail` says. So `blocked` names the
    // second road only where the first would have gone somewhere else.
    // The first writes the raw `on_fail`, unlanded, so that is the route
    // both checks read.
    if raw_fail == Some(stage) || into_blocked {
        let died = into_blocked
            && raw_fail.is_some_and(|step| step != BLOCKED)
            && spent(raw_fail).is_none();
        return Move::CouldNotLaunch {
            from,
            to,
            cause: died.then_some(Cause::LaneDiedAtLaunch),
        };
    }
    Move::Left { from, to }
}

/// `destination`, when it is a step whose `loop:` this task has already
/// spent — the step `commands::apply_loop_budget` refused a move to. The
/// refused move never arrived, so its count still stands where the refusal
/// found it.
fn loop_spent(
    pipeline: &crate::pipeline::Pipeline,
    task: &crate::task::Task,
    destination: Option<&str>,
) -> Option<String> {
    let destination = destination?;
    let limit = pipeline.step(destination)?.arrival_limit()?;
    (task.rounds_at(destination) >= limit).then(|| destination.to_string())
}

/// The step whose pass took the task to `done`, for a stop that happened
/// once it got there: `was` itself when its own `on_pass` is `done`, or, for
/// a task a reading already saw on `done`, the one step of `pipeline` whose
/// `on_pass` is. `None` when there is no such step, or more than one.
fn finished_from(was: &str, pipeline: Option<&crate::pipeline::Pipeline>) -> Option<String> {
    use crate::pipeline::DONE;

    let pipeline = pipeline?;
    if was != DONE {
        return (pipeline.step(was)?.on_pass.as_deref() == Some(DONE)).then(|| was.to_string());
    }
    let mut finishers = pipeline
        .steps
        .iter()
        .filter(|s| s.on_pass.as_deref() == Some(DONE));
    match (finishers.next(), finishers.next()) {
        (Some(step), None) => Some(step.id.clone()),
        _ => None,
    }
}

/// A RECENT event for a task the last reading had and this one does not, or
/// `None` when its leaving is no news.
///
/// A task leaves the queue three ways: it finishes and is archived, a person
/// takes it out with `u`, or its file is deleted. Only the first earns a
/// line, and only when the archive holds the task with stage `done` — a task
/// archived on any other stage did not finish. `was` is the step the board
/// last saw it on, classified against the archived file the way any other
/// move is. One the board already saw on `done` has had its line, and would
/// otherwise read as having passed `done` itself.
fn finished_event(now: &str, id: &str, was: &str, queue: &QueueView) -> Option<RecentEvent> {
    use crate::pipeline::DONE;

    if was == DONE {
        return None;
    }
    let archived =
        crate::task::Task::load(&queue.repo.archive_dir().join(format!("{id}.md"))).ok()?;
    (archived.stage() == DONE).then(|| RecentEvent::Arrival {
        at: now.to_string(),
        id: id.to_string(),
        change: classify(was, DONE, &archived, queue),
    })
}

/// Pushes one event onto the ticker's memory, keeping it to [`RECENT`] long.
///
/// Kept oldest first; [`view::ticker`] draws it newest on top. An arrival
/// first drops any earlier arrival for the same task already sitting in
/// `recent`: two moves of the same task inside the window are one row, not
/// two, and the later move is the one worth a task's single slot.
/// Dropping it before the ring-buffer trim below also means a coalesced
/// arrival never itself evicts another task's news just because a task
/// bounced between steps.
fn push_recent(recent: &mut VecDeque<RecentEvent>, event: RecentEvent) {
    let RecentEvent::Arrival { id, .. } = &event;
    recent.retain(|existing| {
        let RecentEvent::Arrival {
            id: existing_id, ..
        } = existing;
        existing_id != id
    });
    if recent.len() == RECENT {
        recent.pop_front();
    }
    recent.push_back(event);
}

/// The header's version cell: what this dispatcher is running, and a nudge to
/// restart it when a newer `spoolway` has been installed underneath it, or
/// else word that a newer one has been published.
///
/// The version is the running process's own, compiled in — not the
/// executable's on disk. An install swaps that file while a dispatcher goes on
/// running the code it started with, and naming that gap is the whole point of
/// the restart label; reading the version off disk would hide it.
///
/// `installed` is what the `spoolway` on `PATH` reports, already filtered to a
/// genuinely newer version by [`crate::release::installed_newer`] — so an
/// equal, older, unparseable or missing executable arrives here as `None` and
/// leaves the label off. `published` is [`published_newer`]'s answer, already
/// filtered the same way and already gated on the check being wanted. Both are
/// taken as arguments rather than read here so the wording is checked without
/// a test standing up an executable on `PATH` or an npm cache.
///
/// The restart nudge wins over the published version whatever npm says: a
/// newer binary on `PATH` is one restart from running, and naming a release
/// beside it would read as a second thing to do. The published version is
/// named with no command beside it, because the command to take it depends on
/// how this binary was installed and the header is the same for every install.
fn version_label(installed: Option<&str>, published: Option<&str>) -> String {
    let running = format!("v{}", crate::release::current());
    if installed.is_some() {
        return format!("{running} (restart to use latest installed version)");
    }
    match available_label(installed, published) {
        Some(available) => format!("{running} {available}"),
        None => running,
    }
}

/// The part of [`version_label`]'s cell the header paints yellow: the
/// bracketed published version, under the same rule that decides whether
/// the cell carries it — so the two can never disagree about it. `None`
/// whenever the cell has nothing to paint.
fn available_label(installed: Option<&str>, published: Option<&str>) -> Option<String> {
    match installed {
        Some(_) => None,
        None => published.map(|version| format!("({version} available)")),
    }
}

/// A published version newer than this build, when this project and this
/// machine want to hear about one.
///
/// `update_check` is `housekeeping.update_check` and `skipped` is whether
/// [`crate::release::ENV_SKIP`] is set — the two switches that silence the
/// header's version. Either one turned off means `read` is never called,
/// so a board told to keep quiet does not start a cache reading, or the
/// refresh child behind it, either. `read` is
/// [`crate::release::published_newer`] on the board, and a stub in tests.
fn published_newer(
    update_check: bool,
    skipped: bool,
    read: impl FnOnce() -> Option<String>,
) -> Option<String> {
    match update_check && !skipped {
        true => read(),
        false => None,
    }
}

/// When this run began: the moment the dispatcher wrote its lock, as an
/// RFC 3339 timestamp comparable with the ledger's own.
///
/// Shared with the dispatcher's own `max_output_tokens` and `max_cost_usd`
/// checks, so that what the footer reports the run has spent and what either
/// ceiling measures are the same figure over the same window — two readings
/// of "this run" that disagreed would be a board saying one thing while the
/// dispatcher acted on another.
pub fn run_start(repo: &Repo) -> Option<String> {
    let modified = std::fs::metadata(repo.lock_file())
        .and_then(|meta| meta.modified())
        .ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(modified).to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::testutil::*;
    use crate::status::view::{GUTTER, Move, OSC8, RecentEvent, ST, ticker};

    // ---- Reader: coalescing and shutdown ----

    /// A burst of wakes sent faster than any one reading could possibly
    /// finish must collapse into the one reading that follows, never one
    /// per wake — see [`Reader::wake`].
    #[test]
    fn a_burst_of_wakes_collapses_into_one_more_reading() {
        let (repo, _root_guard) = fixture("reader-coalesces-a-burst");
        let pipelines = Pipelines::builtin();
        let reader = Reader::for_board(repo.clone(), pipelines.clone());
        let before = crate::repo::runs_under(&repo.root);

        for _ in 0..50 {
            reader.wake();
        }
        // Waits for one more reading to land — whichever of the fifty wakes
        // above it answers, since every one of them that found a reading
        // already under way asked only to be covered by whatever follows
        // it, not to start a reading of its own.
        reader.wake_and_wait().unwrap();

        let after = crate::repo::runs_under(&repo.root);
        assert!(
            after > before,
            "the burst should have landed at least one more reading: \
             {before} -> {after}"
        );
        assert!(
            after - before < 50,
            "fifty wakes sent at once must not run fifty readings: \
             {before} -> {after}"
        );
    }

    /// A reading a key asked for is noticed once it lands and folded into
    /// the board by the next frame drawn from memory — and only once, so a
    /// second frame over the same reading has nothing new to draw. This is
    /// what lets the dispatch tab draw a reading when it lands.
    #[test]
    fn a_landed_reading_is_noticed_once_and_drawn_from_memory() {
        let (repo, _root_guard) = fixture("reader-landed-reading-is-noticed");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], None);

        let mut board = Board::for_test();
        board.hosted_frame(&repo, &pipelines, false, None).unwrap();
        assert!(!board.reading_landed(), "nothing new before any wake");

        board.wake_reader();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !board.reading_landed() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(board.reading_landed(), "the woken reading should land");

        board.hosted_frame_from_memory(&repo, &pipelines, false, None);
        assert!(
            !board.reading_landed(),
            "a frame drawn from memory takes the landed reading in"
        );
    }

    /// [`Board::hosted_frame`] waits for a reading that started after it was
    /// called, so a task written just before it is always on the frame —
    /// even with keys having woken the reader into readings of their own.
    #[test]
    fn hosted_frame_shows_a_task_written_just_before_it() {
        let (repo, _root_guard) = fixture("reader-hosted-frame-is-fresh");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], None);

        let mut board = Board::for_test();
        board.hosted_frame(&repo, &pipelines, false, None).unwrap();
        // Chained, since a group is one chain.
        let mut previous = "login".to_string();
        for round in 0..20 {
            let id = format!("late-{round}");
            board.wake_reader();
            add(&repo, &id, &[previous.as_str()], None);
            previous = id.clone();
            let frame = board.hosted_frame(&repo, &pipelines, false, None).unwrap();
            assert!(frame.contains(&id), "{id} missing from:\n{frame}");
        }
    }

    /// The reader's thread stops, and `Drop` waits for it to, rather than
    /// leaving it running past the board that started it — see [`Reader`]'s
    /// own `Drop`. A thread left running would make this loop leak one more
    /// each time round; fifty of them finishing at all is the proof.
    #[test]
    fn dropping_the_reader_stops_its_thread() {
        let (repo, _root_guard) = fixture("reader-drop-stops-its-thread");
        let pipelines = Pipelines::builtin();
        let start = std::time::Instant::now();
        for _ in 0..50 {
            drop(Reader::for_board(repo.clone(), pipelines.clone()));
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "dropping a reader should stop its thread promptly, not leak it"
        );
    }

    /// An archived task that named a pipeline no longer defined prints that
    /// name verbatim rather than failing the row, and one that predates
    /// `pipeline:` altogether — never having named one at all — prints
    /// `(none)` rather than a project default that no longer exists.
    #[test]
    fn archived_pipeline_name_covers_a_retired_name_and_a_task_predating_the_key() {
        let pipelines = Pipelines::builtin();
        assert_eq!(
            archived_pipeline_name(&pipelines, Some("default")),
            "default"
        );
        assert_eq!(
            archived_pipeline_name(&pipelines, Some("retired-pipeline")),
            "retired-pipeline"
        );
        assert_eq!(archived_pipeline_name(&pipelines, None), "(none)");
    }

    /// The footer's slot count is what says whether more work can start, so it
    /// has to count the lanes the dispatcher counts and nothing else. A
    /// multiplexer holds a person's own sessions too, and a lane name carries
    /// a `<task> · <step>` separator no task or step id can hold: a session
    /// in a worktree cut from `fix/herdr-layout` is named after the branch,
    /// and parses as no lane at all. Counting those had a run with one lane
    /// in flight reporting `cloud slots 3/5`.
    #[test]
    fn only_lanes_in_our_own_checkouts_take_a_slot() {
        let (repo, _root_guard) = fixture("slots-ownership");
        let pipelines = Pipelines::builtin();
        let worktree = repo.root.join("wt").join("page-and-skills");
        add(&repo, "page-and-skills", &[], Some("review"));
        let mut task = repo.task("page-and-skills").unwrap();
        task.front.worktree_path = Some(worktree.clone());
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let elsewhere = PathBuf::from("/home/someone/.herdr/worktrees/spoolway/fix-herdr-layout");
        let used = slots_used(
            &repo,
            &tasks,
            &pipelines,
            &[
                // Ours: the task's own worktree, at the step it is on.
                lane("page-and-skills · review", &worktree),
                // A person's own agent, in a checkout that is not ours. `fix`
                // is a step id and `herdr-layout` parses as a task, so the name
                // alone proves nothing.
                lane("herdr-layout · fix", &elsewhere),
                // A pane that outlived its task — archived, or never queued
                // here — even in a checkout of ours.
                lane("old-thing · review", &repo.root),
            ],
        );

        assert_eq!(used.agents.get("claude").copied(), Some(1), "{used:#?}");
        assert_eq!(used.agents.get("pi").copied(), None, "{used:#?}");
    }

    /// A profile whose model carries its own `slots` gets a footer entry
    /// straight off a queued task's own pipeline, with no live lane anywhere
    /// to have counted it instead — the walk this task widens `agent_model`
    /// with. The built-in `default` pipeline's `implement` step already names
    /// `pi` and the placeholder, so a sized placeholder and queuing a task
    /// onto it is the whole of the setup.
    #[test]
    fn agent_model_names_a_queued_pipelines_model_with_no_live_lane() {
        let (mut repo, _root_guard) = fixture("queued-agent-model");
        add(&repo, "login", &[], Some("implement"));
        let pipelines = Pipelines::builtin();
        let tasks = repo.tasks().unwrap();

        repo.config.models.insert(
            crate::models::PLACEHOLDER.to_string(),
            crate::usage::ModelPrice {
                slots: 3,
                ..Default::default()
            },
        );

        let used = slots_used(&repo, &tasks, &pipelines, &[]);

        assert_eq!(
            used.agent_model.get("pi").cloned(),
            Some(vec![crate::models::PLACEHOLDER]),
            "{:#?}",
            used.agent_model
        );
    }

    /// A profile whose steps name two different pooled models widens
    /// `agent_model` with both, in the order their steps are found, rather
    /// than keeping only the first — the fact `footer` draws its second pool
    /// line from.
    #[test]
    fn agent_model_widens_with_every_pooled_model_a_profiles_steps_name() {
        let (repo, _root_guard) = fixture("queued-agent-model-two-pools");
        let pipeline = crate::pipeline::Pipeline::parse(
            "impl_ui",
            "steps:\n\
             \x20 - id: implement\n\
             \x20   agent: pi\n\
             \x20   model: ornith/Ornith-1.5-35B-A3B\n\
             \x20   on_pass: review\n\
             \x20 - id: review\n\
             \x20   agent: pi\n\
             \x20   model: qwen/Qwen3.6-35B-A3B\n\
             \x20   on_pass: done\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("impl_ui".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };
        add(&repo, "login", &[], Some("implement"));
        let mut login = repo.task("login").unwrap();
        login.front.pipeline = Some("impl_ui".to_string());
        login.save().unwrap();
        let tasks = repo.tasks().unwrap();

        let used = slots_used(&repo, &tasks, &pipelines, &[]);

        assert_eq!(
            used.agent_model.get("pi").cloned(),
            Some(vec!["ornith/Ornith-1.5-35B-A3B", "qwen/Qwen3.6-35B-A3B"]),
            "{:#?}",
            used.agent_model
        );
    }

    /// A parked task's pane is kept open for a person to read, and the
    /// dispatcher hands the slot back the moment it does that. Counted here it
    /// would read as a profile with nothing running in it — the board saying no
    /// work can start while the dispatcher happily starts some.
    #[test]
    fn a_parked_lane_gives_its_slot_back() {
        let (repo, _root_guard) = fixture("slots-parked");
        let pipelines = Pipelines::builtin();
        for (id, stage) in [
            ("blocked-one", crate::pipeline::BLOCKED),
            ("paused-one", crate::pipeline::PAUSED),
        ] {
            // Each its own group: two tasks with no dependency between them
            // is now a two-root group, refused by `queue add` — these two
            // are independent tasks, not a chain.
            add_to(&repo, id, &[], Some(stage), Some(id));
        }

        let tasks = repo.tasks().unwrap();
        let used = slots_used(
            &repo,
            &tasks,
            &pipelines,
            &[
                lane("blocked-one · review", &repo.root),
                lane("paused-one · review", &repo.root),
            ],
        );

        assert!(used.agents.is_empty(), "{used:#?}");
    }

    /// A staffed `blocked` lane is not parked at all — it is a running lane
    /// like any other, and does count against its agent's cap. Only in an
    /// unattended run, on a pipeline that stages `blocked`, which the shipped
    /// pipelines now do.
    #[test]
    fn a_staffed_blocked_lane_keeps_its_slot() {
        let (mut repo, _root_guard) = fixture("slots-staffed-blocked");
        repo.config.unattended.enabled = true;
        let pipelines = Pipelines::builtin();
        add(&repo, "blocked-one", &[], Some(crate::pipeline::BLOCKED));

        let tasks = repo.tasks().unwrap();
        let used = slots_used(
            &repo,
            &tasks,
            &pipelines,
            &[lane("blocked-one · blocked", &repo.root)],
        );

        assert_eq!(used.agents.get("claude").copied(), Some(1), "{used:#?}");
    }

    /// A settled lane whose task has moved off the step that lane belongs to
    /// stays open, idle, until the task is done. `start_lanes` does not count
    /// it, and counting it here would read as a full profile while the
    /// dispatcher has a slot free for the next lane.
    ///
    /// A `Working` lane off its step counts for nothing either.
    /// `only_the_lane_on_the_tasks_current_step_counts` covers that one.
    #[test]
    fn a_finished_lane_still_open_gives_its_slot_back() {
        let (repo, _root_guard) = fixture("slots-finished-still-open");
        let pipelines = Pipelines::builtin();
        add(&repo, "moved-on", &[], Some("review"));

        let tasks = repo.tasks().unwrap();
        let mut stale = lane("moved-on · implement", &repo.root);
        stale.status = crate::mux::LaneStatus::Idle;
        let used = slots_used(&repo, &tasks, &pipelines, &[stale]);

        assert!(used.agents.is_empty(), "{used:#?}");
    }

    /// One lane on the task's current step and three on steps it has left, one
    /// of them `Working` because a person typed into it. The task sits on
    /// `document` of `bugfix`, with `reproduce`, `fix` and `review` behind it.
    /// The footer counts one. The dispatcher's start check is held to the same
    /// task by `only_the_lane_on_the_current_step_takes_a_slot`, and both go
    /// through `lane_counts`.
    #[test]
    fn only_the_lane_on_the_tasks_current_step_counts() {
        let (repo, _root_guard) = fixture("slots-current-step-only");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("document"));
        let mut login = repo.task("login").unwrap();
        login.front.pipeline = Some("bugfix".to_string());
        login.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let named = |name: &str, status| {
            let mut l = lane(name, &repo.root);
            l.status = status;
            l
        };
        use crate::mux::LaneStatus::{Idle, Working};
        let used = slots_used(
            &repo,
            &tasks,
            &pipelines,
            &[
                named("login · document", Idle),
                named("login · reproduce", Idle),
                named("login · fix", Working),
                named("login · review", Idle),
            ],
        );

        assert_eq!(used.agents.get("pi").copied(), Some(1), "{used:#?}");
        assert_eq!(used.agents.get("claude").copied(), None, "{used:#?}");
    }

    /// The whole point of the column: a task that is moving is described by
    /// where it goes, and a task that is stuck by what is holding it. Also
    /// where a blocked row, a paused row and a row with arrivals on file are
    /// checked, alongside the plain moving and stuck cases, since all of
    /// them share this one column — the arrival count itself belongs to the
    /// STEP column now, so it is read off `Row::arrivals`, not off `next`.
    #[test]
    fn the_next_column_names_a_step_for_a_moving_task_and_a_reason_for_a_stuck_one() {
        let (repo, _root_guard) = fixture("next-column");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        add(&repo, "sessions", &["login"], None);

        // Blocked, parked for a person: the step `spoolway resume` sends it
        // back to — `resume_target`'s own answer, the step it stopped on —
        // the key is offered, since every dependency is met and no lane of
        // its own is busy. Its own group: unrelated to `login`'s chain, and
        // a group is one chain now.
        add_to(&repo, "wall", &[], None, Some("wall"));
        let mut wall = repo.task("wall").unwrap();
        wall.front.blocked_from = Some("implement".into());
        wall.append_to_section("## Blocker", "waiting on a person\n");
        wall.set_stage(crate::pipeline::BLOCKED, None);
        wall.save().unwrap();

        // Paused at a gate: the step passing it would carry the task to,
        // with the resume hint after it.
        add_to(&repo, "ship", &[], None, Some("ship"));
        let mut ship = repo.task("ship").unwrap();
        ship.front.paused_at = Some("implement".into());
        ship.set_stage(crate::pipeline::PAUSED, None);
        ship.save().unwrap();

        // A second arrival at `review`. `add`'s own `set_stage` already
        // banked one; this stands in for a hand-edited second visit.
        add_to(&repo, "spinner", &[], Some("review"), Some("spinner"));
        let mut spinner = repo.task("spinner").unwrap();
        spinner.front.arrivals.insert("review".into(), 2);
        spinner.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap();

        // No lane is live in a fixture, so `login` reads as queued at its step —
        // what matters here is the column, which names the step it goes to and
        // not the whole chain behind it.
        assert_eq!(row("login").stage, "implement");
        assert_eq!(row("login").next, "→ review");
        assert!(
            row("sessions").next.contains("login"),
            "{}",
            row("sessions").next
        );

        assert_eq!(row("wall").next, "[r] → implement");
        assert_eq!(row("ship").next, "[r] → review");
        // No counter text on NEXT at all — it moved to the STEP column, read
        // off `Row::arrivals` instead.
        assert_eq!(row("spinner").next, "→ document");
        assert_eq!(row("spinner").arrivals, 2);
    }

    /// A paused row that caught something other than a plain pass carries the
    /// caught outcome ahead of the arrow — `review failed → document` and
    /// `review blocked → blocked` — so nobody resumes blind; a plain caught
    /// pass still reads exactly as it always has, with no word in front of
    /// the arrow at all.
    #[test]
    fn a_paused_rows_next_names_what_it_caught_when_it_was_not_a_pass() {
        let (repo, _root_guard) = fixture("paused-next-caught");
        let pipelines = Pipelines::builtin();

        add_to(&repo, "pause-reach", &[], None, Some("pause-reach"));
        let mut fail = repo.task("pause-reach").unwrap();
        fail.front.last_report = Some(crate::task::LastReport {
            step: "review".into(),
            outcome: "fail".into(),
            at: 0,
            blocked: false,
        });
        fail.front.paused_at = Some("review".into());
        fail.set_stage(crate::pipeline::PAUSED, None);
        fail.save().unwrap();

        add_to(&repo, "look-holds", &[], None, Some("look-holds"));
        let mut blocked = repo.task("look-holds").unwrap();
        blocked.front.last_report = Some(crate::task::LastReport {
            step: "review".into(),
            outcome: "block".into(),
            at: 0,
            blocked: false,
        });
        blocked.front.blocked_from = Some("review".into());
        blocked.front.paused_at = Some("review".into());
        blocked.set_stage(crate::pipeline::PAUSED, None);
        blocked.save().unwrap();

        add_to(&repo, "sweep-own-tabs", &[], None, Some("sweep-own-tabs"));
        let mut passed = repo.task("sweep-own-tabs").unwrap();
        passed.front.last_report = Some(crate::task::LastReport {
            step: "implement".into(),
            outcome: "pass".into(),
            at: 0,
            blocked: false,
        });
        passed.front.paused_at = Some("implement".into());
        passed.set_stage(crate::pipeline::PAUSED, None);
        passed.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap();

        assert_eq!(row("pause-reach").next, "[r] review failed → document");
        assert_eq!(row("look-holds").next, "[r] review blocked → blocked");
        assert_eq!(
            row("sweep-own-tabs").next,
            "[r] → review",
            "a caught pass reads exactly as an ordinary gate always has"
        );
    }

    /// The mockup `escalate_clock` draws: `parked_from` naming the step a
    /// lane stopped reporting at, with no `paused_at` beside it — there is no
    /// gate here, so `paused_next` must not read `None` and fall back to a
    /// bare `[r]`. `unpark` sends the task straight
    /// back onto `parked_from` itself, and the NEXT column has to name that
    /// same step.
    #[test]
    fn a_task_paused_for_going_quiet_names_the_step_its_resume_carries_it_back_to() {
        let (repo, _root_guard) = fixture("parked-from-next");
        let pipelines = Pipelines::builtin();
        add(&repo, "release-publishing", &[], None);
        let mut task = repo.task("release-publishing").unwrap();
        task.front.parked_from = Some("review".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = rows.iter().find(|r| r.id == "release-publishing").unwrap();
        assert!(matches!(row.state, State::Paused));
        assert_eq!(row.next, "[r] → review");
    }

    /// A task the gate has ranked behind another group is not held by the
    /// graph, so `dependency_note` reads `None` for it — the NEXT column
    /// falls through to the same "waiting for a worker slot" wording every
    /// other slot-starved queued row prints, since the gate only ranks a
    /// candidate now rather than reserving anything for the group ahead of
    /// it. Read through `rows`, the same reading the board and `spoolway
    /// queue list` both draw from.
    #[test]
    fn a_gate_ranked_row_waits_for_a_worker_slot_like_any_other() {
        let (repo, _root_guard) = fixture("gate-note");
        let pipelines = Pipelines::builtin();
        add_to(
            &repo,
            "billing",
            &[],
            Some("implement"),
            Some("dispatcher-ui"),
        );
        add_to(&repo, "gate-filter", &[], None, Some("slot-priority"));

        let rows = rows(&repo, &pipelines).unwrap();
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap();

        assert_eq!(
            row("gate-filter").next,
            "waiting for a worker slot to free up"
        );
    }

    /// `Row::arrivals` reads [`crate::task::Frontmatter::arrivals`]'s own
    /// entry for the step directly — no `loop:` bound involved at all, and,
    /// since this is its own map rather than a sum over `rounds`, no route
    /// behind it either: a task that reached a step several times over
    /// several different routes still banks one arrival count, not one per
    /// route.
    #[test]
    fn arrivals_read_the_step_s_own_entry_regardless_of_any_loop_bound() {
        let (repo, _root_guard) = fixture("arrival-counter");
        let pipelines = Pipelines::builtin();

        // A route the shipped pipeline does bound: the arrival count is
        // banked all the same, since it carries no budget of its own.
        add_to(&repo, "once", &[], Some("review"), Some("once"));

        // Several arrivals at `implement`, however many different routes
        // carried them — the shipped pipeline gives `implement` no `loop:`
        // of its own, so there is no budget behind this count either.
        add_to(&repo, "twice", &[], Some("implement"), Some("twice"));
        let mut twice = repo.task("twice").unwrap();
        twice.front.arrivals.insert("implement".into(), 5);
        twice.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap();

        assert_eq!(row("once").arrivals, 1);
        assert_eq!(row("twice").arrivals, 5);
    }

    /// The frame carries the run's own header, because whose process this is
    /// and which build it is running are the two things the task files cannot
    /// say. What it carries is no clock of either kind: when the next pass is
    /// due is not something a person can act on, and how long the run has been
    /// up only ever said the board was alive — neither earns the room the
    /// version now takes.
    #[test]
    fn a_frame_carries_the_run_and_its_version_but_no_clock() {
        let (repo, _root_guard) = fixture("frame-header");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let version = format!("v{}", crate::release::current());
        let mut board = Board::for_test();
        let running = board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: Some(1234),
                    dispatching: true,
                },
            )
            .unwrap();
        assert!(running.contains("dispatcher running · "), "{running}");
        assert!(running.contains(&format!("· {version}")), "{running}");
        assert!(running.contains("→ review"), "{running}");
        assert!(!running.contains("next pass"), "{running}");
        // The run clock the version replaced. `up ` rather than `up`, which
        // is a substring of ordinary words elsewhere on the frame.
        assert!(!running.contains("· up "), "{running}");

        // The one phase a frame in front of you cannot be read off the frame.
        let over = board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert!(over.contains("dispatcher stopped · "), "{over}");
        // A stopped board still says which build it was, which is what
        // somebody reads it for once it has stopped.
        assert!(over.contains(&format!("· {version}")), "{over}");
    }

    /// The restart hint is added to the version and nothing else — no second
    /// version printed, no wording that changes with what was installed. It
    /// is there exactly when a newer executable was found, and the running
    /// version stays the one the header names either way.
    #[test]
    fn the_version_cell_names_the_running_build_and_hints_only_at_a_newer_one() {
        let running = format!("v{}", crate::release::current());

        assert_eq!(version_label(None, None), running);
        assert_eq!(available_label(None, None), None);
        assert_eq!(
            version_label(Some("99.0.0"), None),
            format!("{running} (restart to use latest installed version)")
        );
        // What was installed is not printed: the header says what is running,
        // and a second version beside it would read as the running one.
        assert!(!version_label(Some("99.0.0"), None).contains("99.0.0"));
    }

    /// A newer published version is named beside the running one, bracketed,
    /// with no command in it — and the bracketed part is exactly what the
    /// header paints.
    #[test]
    fn the_version_cell_names_a_newer_published_version_without_a_command() {
        let running = format!("v{}", crate::release::current());

        let cell = version_label(None, Some("99.0.0"));
        assert_eq!(cell, format!("{running} (99.0.0 available)"));
        assert!(!cell.contains("spoolway"), "{cell}");
        assert_eq!(
            available_label(None, Some("99.0.0")).as_deref(),
            Some("(99.0.0 available)")
        );
    }

    /// A newer binary on `PATH` wins over anything npm says: the cell carries
    /// the restart hint alone, and nothing is left for the header to paint.
    #[test]
    fn the_restart_hint_wins_over_a_published_version() {
        let running = format!("v{}", crate::release::current());

        let cell = version_label(Some("98.0.0"), Some("99.0.0"));
        assert_eq!(
            cell,
            format!("{running} (restart to use latest installed version)")
        );
        assert!(!cell.contains("99.0.0"), "{cell}");
        assert_eq!(available_label(Some("98.0.0"), Some("99.0.0")), None);
    }

    /// `housekeeping.update_check = false` or `SPOOLWAY_SKIP_VERSION_CHECK`
    /// leaves the published version off — and without even asking for it, so
    /// a board told to keep quiet starts no reading behind the header.
    #[test]
    fn a_check_turned_off_never_asks_for_the_published_version() {
        let never = || -> Option<String> { panic!("the published version was read") };
        assert_eq!(published_newer(false, false, never), None);
        assert_eq!(published_newer(true, true, never), None);
        assert_eq!(published_newer(false, true, never), None);

        assert_eq!(
            published_newer(true, false, || Some("99.0.0".to_string())).as_deref(),
            Some("99.0.0")
        );
    }

    /// The notice is painted yellow in the laid-out header, at full strength
    /// rather than inside the header's dim, and the header's visible text and
    /// width are what they were before it was painted.
    #[test]
    fn the_available_notice_is_painted_yellow_in_the_masthead() {
        let header = "dispatcher running · pid 4120 · v0.9.0 (0.10.0 available)";
        let plain = board_masthead(header, 120, 0);
        let painted = tint_available(&plain, Some("(0.10.0 available)"));

        assert!(
            painted.contains(&format!("{RESET}{AMBER}(0.10.0 available){RESET}{DIM}")),
            "{painted:?}"
        );
        assert_eq!(strip_ansi(&painted), strip_ansi(&plain));
        assert_eq!(tint_available(&plain, None), plain);

        // The empty board's header row is painted the same way.
        let row = tint_available(&board_header(header, 120), Some("(0.10.0 available)"));
        assert!(
            row.contains(&format!("{AMBER}(0.10.0 available){RESET}")),
            "{row:?}"
        );
    }

    /// An empty board is the greeting and nothing else: no rule, no slots
    /// lines, no jobs ledger even with a job enabled, no `pipelines` notice
    /// and no parse warning — and only `enter` on the key line.
    #[test]
    fn an_empty_board_draws_only_its_greeting() {
        let (repo, _root_guard) = fixture("board-empty-greeting");
        let pipelines = Pipelines::builtin();
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::write(
            repo.user_jobs_file(),
            "[jobs.nightly]\nschedule = \"0 3 * * *\"\npipeline = \"default\"\nroutine = \"nightly\"\n",
        )
        .unwrap();

        let mut board = Board::for_test();
        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(frame.contains("Nothing queued"), "{frame}");
        assert!(!frame.contains("nothing queued"), "{frame}");
        assert!(frame.contains("dispatcher stopped"), "{frame}");
        // The fixture's own `user.name` is `spoolway t`.
        assert!(frame.contains(", spoolway."), "{frame}");
        assert!(!frame.contains("nightly"), "{frame}");
        assert!(!frame.contains("slots"), "{frame}");
        assert!(!frame.contains("────"), "{frame}");
        assert!(frame.contains("[enter] start dispatching"), "{frame}");
        assert!(!frame.contains("[o] open task"), "{frame}");

        // What only a reading can carry — a file that would not parse, an
        // edited pipeline — stays off an empty board too.
        let snapshot = Snapshot {
            load_problems: vec![crate::task::LoadProblem {
                path: "broken.md".into(),
                error: "no frontmatter".to_string(),
            }],
            changed_pipelines: vec!["default".to_string()],
            ..Snapshot::empty()
        };
        let frame = strip(&paint(
            &repo,
            &pipelines,
            false,
            &snapshot,
            None,
            &VecDeque::new(),
            None,
        ));
        assert!(!frame.contains("broken.md"), "{frame}");
        assert!(!frame.contains("default"), "{frame}");
        assert!(frame.contains("Nothing queued"), "{frame}");
    }

    /// RECENT stays on an empty board while no dispatcher is running, and
    /// goes while one is.
    ///
    /// Where RECENT lands, and how much of it a pane holds, depends on the
    /// pane's height, which `paint` reads from the real terminal. That layout
    /// is covered by `greeting_screen`'s own tests with a fixed region; this
    /// one checks only what holds at any height.
    #[test]
    fn an_empty_board_keeps_recent_only_while_no_dispatcher_runs() {
        let (repo, _root_guard) = fixture("board-empty-recent");
        let pipelines = Pipelines::builtin();
        let recent = arrivals(2);
        let none = VecDeque::new();
        let stopped = Phase::Watching {
            holder: None,
            dispatching: false,
        };
        let held = Phase::Watching {
            holder: Some(4242),
            dispatching: true,
        };
        assert_eq!(empty_board_recent(stopped, &recent, &none).len(), 2);
        assert!(empty_board_recent(held, &recent, &none).is_empty());

        let running = strip(&paint(
            &repo,
            &pipelines,
            true,
            &Snapshot {
                holder: Some(4242),
                ..Snapshot::empty()
            },
            None,
            &recent,
            Some("Marvin"),
        ));
        assert!(
            running.contains("dispatcher running · pid 4242"),
            "{running}"
        );
        assert!(running.contains("[enter] stop dispatching"), "{running}");
        assert!(!running.contains("RECENT"), "{running}");
        assert!(!running.contains("task-1"), "{running}");
    }

    /// The greeting's name is read off git once per board and kept: a name
    /// changed under an open board shows on the next board, not this one,
    /// and a blank `user.name` greets nobody — no comma, the full stop kept.
    #[test]
    fn the_greeting_reads_the_git_name_once_per_board() {
        let (repo, _root_guard) = fixture("board-greeting-name");
        let pipelines = Pipelines::builtin();
        let watching = Phase::Watching {
            holder: None,
            dispatching: false,
        };
        let greeting_of = |board: &mut Board| {
            let frame = strip(&board.frame(&repo, &pipelines, watching).unwrap());
            frame
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("Good ") || line.starts_with("Working late"))
                .unwrap_or_else(|| panic!("no greeting in {frame}"))
                .to_string()
        };

        let mut board = Board::for_test();
        assert!(greeting_of(&mut board).ends_with(", spoolway."));
        repo.git(&["config", "user.name", "Ada Lovelace"]).unwrap();
        assert!(greeting_of(&mut board).ends_with(", spoolway."));
        assert!(greeting_of(&mut Board::for_test()).ends_with(", Ada."));

        repo.git(&["config", "user.name", "  "]).unwrap();
        let nameless = greeting_of(&mut Board::for_test());
        assert!(!nameless.contains(','), "{nameless}");
        assert!(nameless.ends_with('.'), "{nameless}");
    }

    /// A task's `url:` frontmatter reaches the group band as a real OSC 8
    /// hyperlink in the frame the board hands back — the whole path from the
    /// saved task through `Row::issue_url` into the painted bytes, not
    /// just `view::table` exercised in isolation. Stripped, the band is still
    /// the plain heading a group search matches.
    #[test]
    fn a_saved_task_url_becomes_the_group_bands_hyperlink_in_the_frame() {
        let (repo, _root_guard) = fixture("band-url-frame");
        let pipelines = Pipelines::builtin();
        add_to(
            &repo,
            "auth-01",
            &[],
            Some("implement"),
            Some("proj-12-auth"),
        );

        let url = "https://acme.atlassian.net/browse/PROJ-12";
        let mut task = repo.task("auth-01").unwrap();
        task.set_extra_str("url", url);
        task.save().unwrap();

        let mut board = Board::for_test();
        let frame = board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert!(
            frame.contains(&format!("▌{OSC8}{url}{ST}proj-12-auth{OSC8}{ST}")),
            "the band should carry the issue hyperlink — {frame}"
        );
        assert!(
            strip(&frame)
                .lines()
                .any(|l| l.trim_end().ends_with("▌proj-12-auth")),
            "a group search still finds the band — {frame}"
        );
    }

    /// A board with a row on it says what every key does — the mockup's own
    /// last line — whether or not that row can use one: the hint says what
    /// the board can do, not what it would do this frame. An empty board has
    /// no row for those keys to act on, so its key line keeps only `enter`.
    #[test]
    fn the_row_keys_join_the_key_hint_once_a_row_is_on_the_board() {
        let (repo, _root_guard) = fixture("key-hint");
        let pipelines = Pipelines::builtin();

        let mut board = Board::for_test();
        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(frame.contains("[enter] start dispatching"), "{frame}");
        assert!(
            !frame.contains("[o] open task"),
            "an empty queue's frame has no row for these keys — {frame}"
        );

        add(&repo, "login", &[], Some("implement"));
        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(
            frame.contains(
                "[o] open task   [p] pause task   [r] resume   [s] restart   [u/U] unqueue / all"
            ),
            "{frame}"
        );
    }

    /// `paused` means the stage and nothing else now: a lane's own status —
    /// `Working`, or merely `Done`/settled with no report yet — never reads
    /// as `paused` on its own any more. Both draw `Running`, exactly as a
    /// task on a live step always has, because `live_lane` is present either
    /// way; only `LaneStatus::Blocked` (see the sibling test below) and the
    /// task's own stage move the state off `Running`.
    #[test]
    fn a_settled_lane_with_no_report_still_reads_as_running() {
        let (repo, _root_guard) = fixture("waiting-but-working");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());

        let mut working = lane("login · implement", &repo.root);
        working.status = crate::mux::LaneStatus::Working;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[working], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Running), "{}", row.next);

        let mut settled = lane("login · implement", &repo.root);
        settled.status = crate::mux::LaneStatus::Done;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[settled], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(
            matches!(row.state, State::Running),
            "settled-but-unreported is no longer a source of `paused`: {}",
            row.next
        );
    }

    /// The one live source of a lane-level stop the board still draws: herdr
    /// reading a permission prompt off the pane, this frame, with nothing
    /// remembered from the last one.
    #[test]
    fn a_blocked_lane_reads_as_prompt_live_and_never_sticky() {
        let (repo, _root_guard) = fixture("blocked-lane-reads-prompt");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());

        let mut prompting = lane("login · implement", &repo.root);
        prompting.status = crate::mux::LaneStatus::Blocked;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[prompting], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Prompt), "{}", row.next);
        assert_eq!(row.next, "press a key in pane `login · implement`");
        assert!(!row.resumable);

        // The very next frame, with the modal answered: nothing sticky left
        // to un-mark, the row is just `Running` again.
        let mut answered = lane("login · implement", &repo.root);
        answered.status = crate::mux::LaneStatus::Working;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[answered], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Running), "{}", row.next);
    }

    /// With no dispatcher up, a lane still in its turn counts — `Working`, a
    /// `Blocked` prompt and an `Unknown` alike — and one that has settled,
    /// `Done` or `Idle`, reads `✓ finished` with what moves it on.
    #[test]
    fn a_stopped_board_counts_working_lanes_and_finishes_settled_ones() {
        let (repo, _root_guard) = fixture("stopped-board-lanes");
        let pipelines = Pipelines::builtin();
        let statuses = [
            ("working", crate::mux::LaneStatus::Working),
            ("prompting", crate::mux::LaneStatus::Blocked),
            ("unknown", crate::mux::LaneStatus::Unknown),
            ("done", crate::mux::LaneStatus::Done),
            ("idle", crate::mux::LaneStatus::Idle),
        ];
        let mut lanes = Vec::new();
        for (id, status) in statuses {
            add_to(&repo, id, &[], Some("implement"), Some(id));
            let mut l = lane(&format!("{id} · implement"), &repo.root);
            l.status = status;
            lanes.push(l);
        }

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let mut rows = build_rows(&repo, &tasks, &pipelines, &graph, &lanes, &[], None).unwrap();
        let mut frozen = BTreeMap::new();
        let working = finish_settled(&repo, &mut rows, &lanes, &mut frozen);

        assert_eq!(working, 3);
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap();
        assert!(matches!(row("working").state, State::Running));
        assert!(matches!(row("prompting").state, State::Prompt));
        assert!(matches!(row("unknown").state, State::Running));
        for id in ["done", "idle"] {
            assert!(matches!(row(id).state, State::Finished), "{id}");
            assert_eq!(row(id).next, "moves on when dispatching starts");
        }
    }

    /// A finished step's TIME is the figure the board read the first frame
    /// it saw it finished, however long it then sits there — and it counts
    /// live again the moment the lane goes back to work.
    #[test]
    fn a_finished_steps_clock_stops_where_the_board_first_saw_it_settle() {
        let (repo, _root_guard) = fixture("stopped-board-clock");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let mut task = repo.task("login").unwrap();
        task.front.launched_at = Some(chrono::Utc::now().timestamp() - 90);
        task.save().unwrap();

        let mut settled = lane("login · implement", &repo.root);
        settled.status = crate::mux::LaneStatus::Done;
        let lanes = [settled];
        let mut frozen = BTreeMap::new();
        let frame = |frozen: &mut BTreeMap<String, Option<i64>>, lanes: &[crate::mux::Lane]| {
            let tasks = repo.tasks().unwrap();
            let graph = Graph::build(&tasks, &repo.archive_dir());
            let mut rows = build_rows(&repo, &tasks, &pipelines, &graph, lanes, &[], None).unwrap();
            let working = finish_settled(&repo, &mut rows, lanes, frozen);
            (working, rows.into_iter().find(|r| r.id == "login").unwrap())
        };

        let (working, first) = frame(&mut frozen, &lanes);
        assert_eq!(working, 0);
        let stopped_at = first.lane_time.unwrap();
        assert!((90..95).contains(&stopped_at), "{stopped_at}");

        // Later frames: the launch now reads ten minutes back, which a live
        // clock would count from. The finished row keeps its first figure.
        let mut task = repo.task("login").unwrap();
        task.front.launched_at = Some(chrono::Utc::now().timestamp() - 600);
        task.save().unwrap();
        let (_, later) = frame(&mut frozen, &lanes);
        assert!(matches!(later.state, State::Finished));
        assert_eq!(later.lane_time, Some(stopped_at));

        // Back to work: counted, `Running`, on its own live clock, and the
        // frozen figure forgotten.
        let mut busy = lanes[0].clone();
        busy.status = crate::mux::LaneStatus::Working;
        let (working, again) = frame(&mut frozen, &[busy]);
        assert_eq!(working, 1);
        assert!(matches!(again.state, State::Running));
        assert!(again.lane_time.unwrap() >= 600);
        assert!(frozen.is_empty());
    }

    /// A command step counts while its run is in flight and reads finished
    /// once the run has written its exit code — the row `build_rows` alone
    /// draws `queued`, since nothing running backs it any more.
    #[test]
    fn a_stopped_board_counts_a_running_command_and_finishes_an_exited_one() {
        let (repo, _root_guard) = fixture("stopped-board-command");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("handover"));

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        let dir = runs.log_path(&key).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{key}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let mut frozen = BTreeMap::new();
        let mut rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        assert_eq!(finish_settled(&repo, &mut rows, &[], &mut frozen), 1);
        assert!(matches!(rows[0].state, State::Running));

        std::fs::write(dir.join(format!("{key}.exit")), "0").unwrap();
        let mut rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        assert!(matches!(rows[0].state, State::Queued), "the premise");
        assert_eq!(finish_settled(&repo, &mut rows, &[], &mut frozen), 0);
        assert!(matches!(rows[0].state, State::Finished));
        assert_eq!(rows[0].next, "moves on when dispatching starts");
        assert!(rows[0].lane_time.is_some(), "an exited run keeps its time");
    }

    /// A stopped header counts the steps still working, singular for one and
    /// left out at none; a running one reads exactly as it did.
    #[test]
    fn a_stopped_header_counts_the_steps_still_finishing() {
        let stopped = Phase::Watching {
            holder: None,
            dispatching: false,
        };
        let header = |finishing| header_cells(stopped, finishing, "v1".to_string()).join(" · ");
        assert_eq!(header(Some(0)), "dispatcher stopped · v1");
        assert_eq!(
            header(Some(1)),
            "dispatcher stopped · 1 step finishing · v1"
        );
        assert_eq!(
            header(Some(3)),
            "dispatcher stopped · 3 steps finishing · v1"
        );

        let running = Phase::Watching {
            holder: Some(1234),
            dispatching: true,
        };
        assert_eq!(
            header_cells(running, None, "v1".to_string()).join(" · "),
            "dispatcher running · pid 1234 · v1"
        );
    }

    /// A dependency whose id happens to contain one of the words the state
    /// used to be sniffed out of is still an ordinary dependency.
    ///
    /// The `queued` state was once decided by substring-matching the
    /// rendered note: `contains("cycle")` over `waiting on: park-lifecycle`
    /// is true, so a task waiting on a perfectly healthy dependency was
    /// drawn in red as one caught in a dependency cycle. Nothing is read
    /// back out of the note now — a task on `queued` reads `queued` — and
    /// this holds that line against the id that first broke it.
    #[test]
    fn a_dependency_named_for_a_lifecycle_is_not_a_dependency_cycle() {
        let (repo, _root_guard) = fixture("cycle-in-the-name");
        let pipelines = Pipelines::builtin();
        add(&repo, "park-lifecycle", &[], Some(crate::pipeline::QUEUED));
        add(
            &repo,
            "paused-board",
            &["park-lifecycle"],
            Some(crate::pipeline::QUEUED),
        );

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();

        let row = rows.iter().find(|r| r.id == "paused-board").unwrap();
        assert!(
            matches!(row.state, State::Queued),
            "an unmet dependency is `queued`: {}",
            row.next
        );
        assert!(
            row.next.contains("waiting on: park-lifecycle"),
            "{}",
            row.next
        );
    }

    /// A wait that can never end is still a wait, and reads like one.
    ///
    /// `search-facets` depends on a task parked on `blocked`, so nothing
    /// will ever make it ready — the shape the board used to draw apart, in
    /// red, as `unreachable`. The dispatcher never told the two apart:
    /// `graph.ready()` passes over any task whose dependencies have not all
    /// finished, so a dead wait sits on `queued` exactly as an ordinary one
    /// does, and the NEXT column names the direct dependency rather than
    /// diagnosing it as stranded.
    #[test]
    fn a_dependency_that_can_never_arrive_still_reads_queued() {
        let (repo, _root_guard) = fixture("dead-dependency");
        let pipelines = Pipelines::builtin();
        add(&repo, "search-typo", &[], Some(crate::pipeline::BLOCKED));
        add(
            &repo,
            "search-facets",
            &["search-typo"],
            Some(crate::pipeline::QUEUED),
        );

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        // The premise: the dependency really is stuck — nothing will ever
        // ready it. The board draws `search-facets` `queued` anyway.
        assert!(!graph.ready("search-facets"));

        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "search-facets").unwrap();
        assert!(matches!(row.state, State::Queued), "{}", row.next);
        assert_eq!(row.state.word(), "○ queued");
        assert_eq!(row.next, "waiting on: search-typo");
    }

    /// A task the dispatcher is booting reads `◌ starting` on the step its
    /// boot mark names, with that step's next hop in NEXT — though the task
    /// file still says `queued` — and the logo turns for it.
    #[test]
    fn a_claimed_task_reads_starting_on_its_claimed_step() {
        let (repo, _root_guard) = fixture("claimed-starting");
        let pipelines = Pipelines::builtin();
        add_to(
            &repo,
            "login",
            &[],
            Some(crate::pipeline::QUEUED),
            Some("login"),
        );
        add_to(
            &repo,
            "profile",
            &[],
            Some(crate::pipeline::QUEUED),
            Some("profile"),
        );
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        let mut claims = crate::claim::Claims::new(&repo);
        claims.claim("login", "implement");

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();

        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Starting), "{}", row.next);
        assert_eq!(row.state.word(), "◌ starting");
        assert_eq!(row.stage, "implement");
        let pipeline = pipelines.get("default").unwrap();
        let hop = pipeline.next_running_step("implement").unwrap();
        assert_eq!(row.next, format!("→ {hop}"));
        let other = rows.iter().find(|r| r.id == "profile").unwrap();
        assert!(matches!(other.state, State::Queued), "{}", other.next);
        assert!(logo_turns(&rows));
        assert!(!logo_turns(std::slice::from_ref(other)));

        // The boot returned: the mark is gone and the row reads as before.
        claims.release("login");
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Queued), "{}", row.next);
        assert_eq!(row.stage, crate::pipeline::QUEUED);
    }

    /// A mark with no live dispatcher behind the lock is one a killed
    /// dispatcher left: the row reads `queued`, never `starting`.
    #[test]
    fn a_boot_mark_with_no_live_dispatcher_is_ignored() {
        let (repo, _root_guard) = fixture("claimed-stale");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some(crate::pipeline::QUEUED));
        let mut claims = crate::claim::Claims::new(&repo);
        claims.claim("login", "implement");

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Queued), "{}", row.next);
        assert_eq!(row.stage, crate::pipeline::QUEUED);
        assert!(!logo_turns(&rows));
    }

    /// A live lane's clock is its own: `now - launched_at`, not whatever the
    /// ledger has banked for a step still in flight.
    #[test]
    fn lane_time_reads_now_minus_launched_at_for_a_live_lane() {
        let (repo, _root_guard) = fixture("lane-time-live");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let mut task = repo.task("login").unwrap();
        task.front.launched_at = Some(chrono::Utc::now().timestamp() - 90);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let lanes = [lane("login · implement", &repo.root)];
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &lanes, &[], None).unwrap();

        let row = rows.iter().find(|r| r.id == "login").unwrap();
        let secs = row.lane_time.expect("a live lane answers");
        assert!((85..=95).contains(&secs), "{secs}");
    }

    /// A command step's run is a detached process, not a lane, so the
    /// multiplexer's list has nothing to say about it. Read from that list
    /// alone — as this was — a task halfway through a `gate` reads `queued`,
    /// the same word as a task waiting for a worker slot, and a board of
    /// queued rows that never move says the dispatcher has stopped.
    #[test]
    fn a_command_step_reads_as_running_while_its_own_run_is_in_flight() {
        let (repo, _root_guard) = fixture("command-step-running");
        let pipelines = Pipelines::builtin();
        // `handover` is the default pipeline's command step.
        add(&repo, "login", &[], Some("handover"));

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());

        // Nothing started yet: the step is where the task sits, not what it is
        // doing, so this half is what the running half below is measured
        // against.
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Queued));

        // The wrapper's own two files as `Runs::start` leaves them: a pid that
        // is alive — this test's own — and no exit code written yet.
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        let dir = runs.log_path(&key).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{key}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();

        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Running));
        // Its own clock, off the pid file, and not the last lane's
        // `launched_at` — which this task never set at all.
        assert!(row.lane_time.is_some(), "a run in flight answers with one");
    }

    /// A task held on a `serial:` step reads `○ waiting` and names the task
    /// whose run holds it — on the board and in `queue list`'s plain table,
    /// which both draw these rows — and falls back to an ordinary row the
    /// moment that run is no longer going.
    #[test]
    fn a_task_held_on_a_serial_step_reads_as_waiting_on_the_run_ahead() {
        let (repo, _root_guard) = fixture("command-step-serial");
        let mut pipelines = Pipelines::builtin();
        // `handover` is the default pipeline's command step.
        let default = pipelines.pipelines.get_mut("default").unwrap();
        let step = default
            .steps
            .iter_mut()
            .find(|s| s.id == "handover")
            .unwrap();
        step.serial = true;
        // Different groups, and named so neither's own band line contains
        // the other's id — `table.lines().find(|l| l.contains("b-export"))`
        // below would otherwise match `b-export`'s own group band instead of
        // its row.
        add_to(&repo, "a-login", &[], Some("handover"), Some("group-one"));
        add_to(&repo, "b-export", &[], Some("handover"), Some("group-two"));

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());

        // `a-login`'s run in flight, as `Runs::start` leaves it: a live pid
        // — this test's own — and no exit code yet.
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "a-login");
        let dir = runs.log_path(&key).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let pid = dir.join(format!("{key}.pid"));
        std::fs::write(&pid, std::process::id().to_string()).unwrap();

        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let login = rows.iter().find(|r| r.id == "a-login").unwrap();
        assert!(matches!(login.state, State::Running));
        let export = rows.iter().find(|r| r.id == "b-export").unwrap();
        assert!(matches!(export.state, State::Waiting), "{}", export.next);
        assert_eq!(export.next, "serial: after a-login");
        let table = plain_table(&rows);
        let line = table.lines().find(|l| l.contains("b-export")).unwrap();
        assert!(line.contains("○ waiting"), "{table}");
        assert!(line.contains("serial: after a-login"), "{table}");

        // The run ahead exited: nothing holds the step any more, and the
        // row reads as any other task a pass is about to start.
        std::fs::write(dir.join(format!("{key}.exit")), "0").unwrap();
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let export = rows.iter().find(|r| r.id == "b-export").unwrap();
        assert!(matches!(export.state, State::Queued), "{}", export.next);
    }

    /// A task fresh off a handoff, with no lane up for it yet, reads
    /// `Running` for the ordinary gap before the dispatcher's next pass
    /// starts one there — the same gap `spoolway report` opens by writing
    /// the new stage the instant a lane's turn ends. Once [`HANDOFF_GRACE`]
    /// has passed with still no lane, the row falls back to reading
    /// `Queued`, honestly. Faked forward by moving the clock in `grace` back
    /// rather than sleeping through it for real.
    #[test]
    fn a_stage_that_just_changed_reads_running_until_the_grace_window_passes() {
        let (repo, _root_guard) = fixture("mid-handoff-grace");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let window = HANDOFF_GRACE;

        // Just arrived: well inside the window, no lane anywhere.
        let mut grace = BTreeMap::new();
        grace.insert("login".to_string(), Instant::now());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], Some(&grace)).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Running), "{}", row.next);

        // The same row, the window already spent — still no lane.
        grace.insert(
            "login".to_string(),
            Instant::now() - window - Duration::from_secs(1),
        );
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], Some(&grace)).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Queued), "{}", row.next);
    }

    /// `launch_landed` forgives `attempts` the first pass that sees a lane
    /// busy — about two seconds after it started — but the clock is
    /// `launched_at`'s other reading, and it has to survive that forgiveness
    /// for the rest of the step: a `launch_landed` that cleared both, as it
    /// once did, made the board read a dash from the second frame onward.
    #[test]
    fn lane_time_survives_launch_landed() {
        let (repo, _root_guard) = fixture("lane-time-survives-launch-landed");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let mut task = repo.task("login").unwrap();
        task.front.launched_at = Some(chrono::Utc::now().timestamp() - 90);
        task.front.attempts = 1;
        // What the busy-lane branch in `Dispatcher::pass` does the first pass
        // that sees the lane working: forgive the counter, nothing else.
        assert!(task.launch_landed(), "attempts was set, so this counted");
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let lanes = [lane("login · implement", &repo.root)];
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &lanes, &[], None).unwrap();

        let row = rows.iter().find(|r| r.id == "login").unwrap();
        let secs = row
            .lane_time
            .expect("the clock still answers after launch_landed");
        assert!((85..=95).contains(&secs), "{secs}");
    }

    /// Once a lane is not live, TIME is the ledger's own summed `wall_s` at
    /// the step the task is on — every round of it, the same shape as
    /// [`spent_at`] and [`cost_at`].
    #[test]
    fn lane_time_sums_the_ledgers_wall_s_once_the_lane_is_not_live() {
        let entry = |wall_s: i64| {
            let mut entry = banked("login", "implement", "s1", None);
            entry.wall_s = wall_s;
            entry
        };
        assert_eq!(
            lane_time_at(&[entry(30), entry(45)], "login", "implement"),
            Some(75)
        );
        assert_eq!(lane_time_at(&[entry(30)], "login", "review"), None);
    }

    /// A paused task's own `stage()` is `paused` — not a step any pipeline
    /// declares, and so not the step its lane actually banked under. TIME
    /// (and OUT and COST beside it) must still read the ledger at the step
    /// named by `parked_from` or `paused_at`, or a paused row would show a
    /// dash for figures the task plainly has — and, worse, a row that
    /// *froze* on the wrong step's total would happen to read as "the same
    /// on every pass" for the wrong reason. See gh-378 / issue #380.
    #[test]
    fn a_paused_rows_time_reads_the_step_it_actually_paused_from() {
        let (repo, _root_guard) = fixture("paused-row-reads-its-own-step");
        let pipelines = Pipelines::builtin();
        add(&repo, "cart-totals", &[], Some("implement"));
        let mut task = repo.task("cart-totals").unwrap();
        task.front.parked_from = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut held = banked("cart-totals", "implement", "s1", Some(0.04));
        held.wall_s = 62;
        held.tokens.output = 2_100;
        let ledger = vec![held];

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());

        let first = build_rows(&repo, &tasks, &pipelines, &graph, &[], &ledger, None).unwrap();
        let row = first.iter().find(|r| r.id == "cart-totals").unwrap();
        assert!(matches!(row.state, State::Paused));
        assert_eq!(row.lane_time, Some(62), "the busy time it already banked");
        assert_eq!(row.out, Some(2_100));
        assert_eq!(row.cost, Some(0.04));

        // A later pass, nothing else banked in the meantime — a paused row
        // is never re-banked while it stays paused, so this reads the same
        // ledger and must draw the same figures.
        let second = build_rows(&repo, &tasks, &pipelines, &graph, &[], &ledger, None).unwrap();
        let row2 = second.iter().find(|r| r.id == "cart-totals").unwrap();
        assert_eq!(
            row2.lane_time, row.lane_time,
            "TIME never moves while paused"
        );
    }

    /// An archived task is only worth a row while its group still has
    /// something in the queue — a group whose last task has archived leaves
    /// the board entirely rather than lingering as a block of nothing but
    /// `● done` rows.
    #[test]
    fn done_rows_only_appear_for_a_group_still_in_the_queue() {
        let (repo, _root_guard) = fixture("done-rows");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "login", &[], Some("implement"), Some("auth"));

        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("signup.md"),
            "---\nid: signup\nstage: done\ngroup: auth\n---\n",
        )
        .unwrap();
        std::fs::write(
            repo.archive_dir().join("gone.md"),
            "---\nid: gone\nstage: done\ngroup: finished-group\n---\n",
        )
        .unwrap();

        let active = rows(&repo, &pipelines).unwrap();
        let active_groups: BTreeSet<String> =
            active.iter().filter_map(|r| r.group.clone()).collect();
        let done = done_rows(&repo, &pipelines, &active_groups).unwrap();

        let ids: Vec<&str> = done.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["signup"], "`finished-group` has nothing queued");
        assert!(matches!(done[0].state, State::Done));
        assert_eq!(done[0].group.as_deref(), Some("auth"));
    }

    /// `cached_archive` reads the directory once and again only past a
    /// change in its own mtime — the whole point of caching it rather than
    /// reparsing every file on every one-second frame.
    #[test]
    fn cached_archive_refreshes_once_the_directorys_mtime_moves() {
        let (repo, _root_guard) = fixture("archive-cache-refresh");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("first.md"),
            "---\nid: first\nstage: done\ngroup: g\n---\n",
        )
        .unwrap();

        let first = cached_archive(&repo.archive_dir()).unwrap();
        assert_eq!(first.len(), 1, "the first read finds the one archived task");

        std::fs::write(
            repo.archive_dir().join("second.md"),
            "---\nid: second\nstage: done\ngroup: g\n---\n",
        )
        .unwrap();
        // A new file bumps a directory's own mtime on any real filesystem —
        // pushed a few seconds ahead here so a coarse clock cannot make this
        // assertion flaky against the first read's own timestamp.
        let ahead = SystemTime::now() + Duration::from_secs(5);
        crate::scratch::set_mtime(&repo.archive_dir(), ahead);

        let second = cached_archive(&repo.archive_dir()).unwrap();
        assert_eq!(
            second.len(),
            2,
            "the cache must refresh once the directory's own mtime moves"
        );
    }

    /// The board's done rows come from the archive index: none of the three
    /// readings below opens a task file, a task archived under a warm cache
    /// adds its own entry and nothing else, and the rows are the ones the
    /// files give after adds, a sweep and a rebuild.
    #[test]
    fn done_rows_read_the_index_and_match_the_files() {
        use crate::archive_index::testutil::*;
        let (repo, _root_guard) = fixture("done-rows-index");
        let pipelines = Pipelines::builtin();
        let active: BTreeSet<String> = ["auth".to_string()].into();
        let url = "url: https://acme.example/1\n";
        let rows_now = || {
            let rows = done_rows(&repo, &pipelines, &active).unwrap();
            rows.iter()
                .map(|r| (r.id.clone(), r.issue_url.clone(), r.pipeline.clone()))
                .collect::<Vec<_>>()
        };
        // What the files say, read the way the board read them before.
        let from_files = || {
            let (tasks, _) = crate::task::load_dir(&repo.archive_dir()).unwrap();
            tasks
                .iter()
                .filter(|t| t.front.group.as_deref() == Some("auth"))
                .map(|t| {
                    (
                        t.id().to_string(),
                        issue_url_of(t),
                        archived_pipeline_name(&pipelines, t.front.pipeline.as_deref()),
                    )
                })
                .collect::<Vec<_>>()
        };

        archive(&repo, "a", &format!("group: auth\npipeline: impl\n{url}"));
        archive(&repo, "b", "group: auth\n");
        archive(&repo, "other", "group: elsewhere\n");
        assert_eq!(rows_now(), from_files());
        assert_eq!(rows_now().len(), 2, "the other group has nothing queued");

        // Warm now. With every file unreadable only the index can answer.
        reset_rebuilds();
        let before = cached_archive(&repo.archive_dir()).unwrap().len();
        let unreadable = with_unreadable_files(&repo, rows_now);
        assert_eq!(unreadable, from_files());
        assert_eq!(rebuilds(), 0, "a screen read opened task files");

        archive(&repo, "c", "group: auth\n");
        let after = cached_archive(&repo.archive_dir()).unwrap();
        assert_eq!(after.len(), before + 1, "the new task adds one entry");
        assert_eq!(rebuilds(), 0, "archiving one task opened another's file");
        assert_eq!(rows_now(), from_files());

        // A sweep and a rebuild agree with the files that remain.
        sweep(&repo, "a");
        assert_eq!(rows_now(), from_files());
        assert_eq!(rebuilds(), 0);
        lose_index(&repo);
        assert_eq!(rows_now(), from_files());
        assert_eq!(rows_now().len(), 2, "b and c remain");
    }

    /// `board-reads-changed-only`: `render` used to call
    /// `repo.tasks_and_problems()`, which parsed every queued task file on
    /// every frame even when none changed — the cost the board's own comment
    /// already called out for the archive, left unfixed there. A frame over
    /// unchanged files must parse none of them, and a frame after one file
    /// is rewritten in place must parse only that one — counted directly,
    /// not timed, so the assertion does not depend on how fast this machine
    /// happens to be (unlike `a_second_load_with_nothing_changed_rereads_no_transcript`
    /// in `src/eval.rs`, which times it). `cached_queue` is the per-file,
    /// byte-compared cache this task asked for, in the same shape
    /// `cached_archive` already takes for the directory beside it.
    ///
    /// Drives `cached_queue_in` directly, against a `QueueCache` this test
    /// owns, rather than `cached_queue` and its one process-wide static: that
    /// static is shared with every other test that calls `build` in this
    /// module, and one landing between two calls here was free to reset it
    /// out from under this test and make it parse again when nothing of its
    /// own had changed (review finding 1).
    #[test]
    fn cached_queue_reparses_only_a_file_whose_bytes_changed() {
        let (repo, _root_guard) = fixture("queue-cache-reparse");
        let dir = repo.queue_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.md"),
            "---\nid: a\nstage: implement\ngroup: g\n---\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.md"),
            "---\nid: b\nstage: implement\ngroup: g\n---\n",
        )
        .unwrap();

        let mut state = QueueCache {
            dir: dir.clone(),
            files: HashMap::new(),
        };

        let (_, _, parsed_first) = cached_queue_in(&dir, &mut state).unwrap();
        assert_eq!(parsed_first, 2, "both files are new, so both are parsed");

        let (tasks_second, _, parsed_second) = cached_queue_in(&dir, &mut state).unwrap();
        assert_eq!(
            parsed_second, 0,
            "nothing on disk changed, so a second reading parses nothing"
        );
        assert_eq!(tasks_second.len(), 2);

        // Rewritten in place, same byte length, new content.
        std::fs::write(
            dir.join("b.md"),
            "---\nid: b\nstage: implement\ngroup: h\n---\n",
        )
        .unwrap();

        let (tasks_third, _, parsed_third) = cached_queue_in(&dir, &mut state).unwrap();
        assert_eq!(
            parsed_third, 1,
            "only the file whose bytes changed is parsed again"
        );
        assert_eq!(
            tasks_third
                .iter()
                .find(|t| t.id() == "b")
                .unwrap()
                .front
                .group
                .as_deref(),
            Some("h"),
            "the board must show the rewritten file's new contents"
        );
    }

    /// `board-reads-changed-only`: `live_session` already held the ledger
    /// `build` reads once through `usage::read_cached`, as its own `ledger`
    /// parameter — but it used to resolve a lane's session through
    /// `crate::dispatch::lane_session`, which ignored that parameter and ran
    /// its own full, uncached `usage::read` instead. A frame with ten live
    /// lanes paid for the whole ledger ten times over. It now goes through
    /// `crate::dispatch::lane_session_in` with the `ledger` it was given.
    ///
    /// Proven here without a real transcript: the lane is on the ledger only
    /// (no `lanes.json` record), with a different session on disk than the
    /// one the in-memory `ledger` argument carries. `live_session` records
    /// whichever session it resolved in `touched` before it ever looks for a
    /// transcript, so which one lands there says which ledger it actually
    /// used.
    #[test]
    fn live_session_uses_the_ledger_render_already_holds() {
        let (repo, _root_guard) = fixture("live-session-ledger-reuse");
        let lane = crate::mux::lane_name("implement", "demo");

        crate::usage::append(
            &repo,
            &testutil::banked("demo", "implement", "disk-session", None),
        )
        .unwrap();
        let ledger = vec![testutil::banked(
            "demo",
            "implement",
            "memory-session",
            None,
        )];

        let mut touched = HashSet::new();
        live_session(&repo, &ledger, &lane, &mut touched);

        assert!(
            touched.contains("memory-session"),
            "live_session must resolve the lane from the ledger it was \
             already given: {touched:?}"
        );
        assert!(
            !touched.contains("disk-session"),
            "it must not fall back to its own fresh read of the ledger on \
             disk: {touched:?}"
        );
    }

    /// Every row names the pipeline its task resolves to: a task with no
    /// `pipeline:` of its own reads the project's configured default, one
    /// that names a real pipeline reads that name, and an archived task
    /// whose named pipeline has since been deleted reads that name verbatim
    /// rather than failing the whole board.
    #[test]
    fn every_row_names_the_pipeline_its_task_resolves_to() {
        let (repo, _root_guard) = fixture("pipeline-column-name");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "login", &[], Some("implement"), Some("auth"));
        // Its own group: `login` and `hotfix` have no dependency between
        // them, and a group is one chain now.
        add_to(&repo, "hotfix", &[], Some("implement"), Some("hotfix"));
        let mut hotfix = repo.task("hotfix").unwrap();
        hotfix.front.pipeline = Some("bugfix".into());
        hotfix.save().unwrap();

        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("signup.md"),
            // Same group as `login`, which stays active — `done_rows` only
            // shows an archived task whose group is still in the queue.
            "---\nid: signup\nstage: done\ngroup: auth\npipeline: retired\n---\n",
        )
        .unwrap();

        let active = rows(&repo, &pipelines).unwrap();
        let login = active.iter().find(|r| r.id == "login").unwrap();
        assert_eq!(login.pipeline, "default", "{}", login.pipeline);
        let hotfix_row = active.iter().find(|r| r.id == "hotfix").unwrap();
        assert_eq!(hotfix_row.pipeline, "bugfix");

        let active_groups: BTreeSet<String> =
            active.iter().filter_map(|r| r.group.clone()).collect();
        let done = done_rows(&repo, &pipelines, &active_groups).unwrap();
        assert_eq!(
            done[0].pipeline, "retired",
            "a deleted pipeline's name still prints — done_rows must never fail the board over it"
        );
    }

    /// The decision this whole change implements: a group orders by where its
    /// tasks stand in the run, not by steps left and not by id. `alpha` is
    /// alphabetically first, and its `default` pipeline leaves it fewer steps
    /// than `zeta`'s `bugfix` — both tiers a stale key would reach for before
    /// ever asking about the dependency. Only depth gets it right: `alpha`
    /// depends on `zeta`, so `zeta` runs first and belongs above it.
    #[test]
    fn a_dependency_sorts_above_its_dependent_despite_fewer_steps_left_and_an_earlier_id() {
        let (repo, _root_guard) = fixture("run-order-depth");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "zeta", &[], None, Some("pair"));
        add_to(&repo, "alpha", &["zeta"], None, Some("pair"));
        let mut alpha = repo.task("alpha").unwrap();
        alpha.front.pipeline = Some("default".into());
        alpha.save().unwrap();
        let mut zeta = repo.task("zeta").unwrap();
        zeta.front.pipeline = Some("bugfix".into());
        zeta.save().unwrap();

        let active = rows(&repo, &pipelines).unwrap();
        let ids: Vec<&str> = active.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["zeta", "alpha"],
            "the dependency belongs first, whatever the alphabet or the pipeline lengths say: {ids:?}"
        );
    }

    /// A row's place in its group is settled by the run, not by the state a
    /// pass happens to catch it in: the same pair, sorted once with the
    /// dependency `queued` and once with it `implement`ing, comes out in the
    /// same order both times — the mockup's "no row having moved" one pass
    /// later.
    #[test]
    fn a_groups_row_order_survives_a_dependency_moving_from_queued_to_running() {
        let (repo, _root_guard) = fixture("run-order-stable");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "gate-ranks", &[], None, Some("gate"));
        add_to(
            &repo,
            "gate-priority-docs",
            &["gate-ranks"],
            None,
            Some("gate"),
        );

        let before = rows(&repo, &pipelines).unwrap();
        let before_ids: Vec<&str> = before.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(before_ids, ["gate-ranks", "gate-priority-docs"]);

        let mut ranks = repo.task("gate-ranks").unwrap();
        ranks.set_stage("implement", None);
        ranks.save().unwrap();

        let after = rows(&repo, &pipelines).unwrap();
        let after_ids: Vec<&str> = after.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            before_ids, after_ids,
            "a state move must never reorder the group"
        );
    }

    /// The ledger is what says how much a step has cost so far, and every
    /// round of it counts — a task that went back through `fix` twice has
    /// spent both.
    #[test]
    fn the_out_column_sums_every_round_of_the_step_it_is_on() {
        let entry = |task: &str, step: &str, output: u64| {
            let mut entry = banked(task, step, "", None);
            entry.tokens.output = output;
            entry
        };
        let ledger = [
            entry("login", "implement", 1_200),
            entry("login", "implement", 800),
            entry("login", "review", 400),
            entry("billing", "implement", 9_000),
        ];

        assert_eq!(spent_at(&ledger, "login", "implement"), Some(2_000));
        assert_eq!(spent_at(&ledger, "login", "review"), Some(400));
        assert_eq!(spent_at(&ledger, "login", "handover"), None);
    }

    /// COST is the step's, not the task's. A task deep in a pipeline has spent
    /// most of its bill at steps it has already left, and putting that on the
    /// row said the step in flight had cost it — beside a CTX and an OUT that
    /// were only ever about that step.
    #[test]
    fn the_cost_column_is_the_step_it_is_on_and_not_the_whole_task() {
        let ledger = [
            banked("login", "implement", "s1", Some(10.0)),
            banked("login", "review", "s2", Some(4.0)),
            banked("login", "fix", "s1", Some(3.5)),
            banked("login", "review", "s2", Some(2.0)),
            banked("billing", "implement", "s3", Some(9.0)),
        ];

        assert_eq!(cost_at(&ledger, "login", "implement"), Some(10.0));
        // Both rounds of the step, as OUT sums both rounds of it.
        assert_eq!(cost_at(&ledger, "login", "review"), Some(6.0));
        // Not the $19.50 the task has run up getting here.
        assert_eq!(cost_at(&ledger, "login", "handover"), None);
    }

    /// A lane whose transcript reports its own cost — a local worker, or any
    /// agent that prices itself — is trusted over the price map, and what it
    /// has already been banked for comes off just the same. A cost the price
    /// table gave the harvest turn by turn is subtracted the same way.
    #[test]
    fn a_reported_cost_is_taken_at_its_word_less_whatever_is_banked() {
        let ledger = [banked("login", "implement", "s1", Some(1.5))];
        let harvest = |cost: f64| crate::usage::Harvest {
            tier_tokens: Default::default(),
            model: "claude-sonnet-5".into(),
            tokens: crate::usage::Tokens::default(),
            turns: 1,
            cost_usd: Some(cost),
            reported_usd: None,
            reported_models: Default::default(),
            ctx_peak: 0,
        };

        assert_eq!(live_spend(&ledger, "s1", &harvest(4.0)).1, Some(2.5));
        // A transcript that reads lower than the ledger is a torn read, not a
        // refund: nothing here ever hands back a negative.
        assert_eq!(live_spend(&ledger, "s1", &harvest(0.5)).1, Some(0.0));
        // A session nothing has banked keeps the whole of its own reading.
        assert_eq!(live_spend(&ledger, "s9", &harvest(4.0)).1, Some(4.0));
    }

    /// The reason the subtraction is there at all: a step resumed on the
    /// session its predecessor ran under reads a transcript that already holds
    /// the predecessor's turns, and showing the whole of it would put that
    /// step's bill on this one's row.
    #[test]
    fn a_reused_session_is_priced_from_what_it_has_spent_since_it_was_banked() {
        // The previous step banked 1M output ($15) against this session; the
        // transcript, cumulative, now reads 1.5M, priced turn by turn to $22.50.
        let mut entry = banked("login", "implement", "s1", Some(15.0));
        entry.tokens.output = 1_000_000;
        let harvest = crate::usage::Harvest {
            tier_tokens: Default::default(),
            model: "claude-sonnet-5".into(),
            tokens: crate::usage::Tokens {
                output: 1_500_000,
                ..Default::default()
            },
            turns: 4,
            cost_usd: Some(22.5),
            reported_usd: None,
            reported_models: Default::default(),
            ctx_peak: 0,
        };

        // The half-million this step has produced, at $15/M — not $22.50. OUT
        // is that same half-million, off the same subtraction: the two figures
        // beside each other on a row are one reading of one step.
        let (out, cost) = live_spend(&[entry], "s1", &harvest);
        assert_eq!(out, 500_000);
        assert_eq!(cost, Some(7.5));

        // And a fresh session, which is what a step ordinarily gets, is read
        // in full: its delta is its total.
        let (out, cost) = live_spend(&[], "s2", &harvest);
        assert_eq!(out, 1_500_000);
        assert_eq!(cost, Some(22.5));
    }

    /// A harvest with no cost has a turn nothing prices. The row shows no
    /// cost, as the ledger line banked for it will, rather than pricing the
    /// summed tokens at the last turn's model.
    #[test]
    fn a_harvest_with_no_cost_shows_no_cost() {
        let harvest = crate::usage::Harvest {
            tier_tokens: Default::default(),
            model: "claude-sonnet-5".into(),
            tokens: crate::usage::Tokens {
                output: 1_000_000,
                ..Default::default()
            },
            turns: 2,
            cost_usd: None,
            reported_usd: None,
            reported_models: Default::default(),
            ctx_peak: 0,
        };

        let (out, cost) = live_spend(&[], "s1", &harvest);
        assert_eq!(out, 1_000_000);
        assert_eq!(cost, None);
    }

    /// The STATE column is sized to the board, not to the widest word there
    /// is: a queue with nothing waiting on a person should not hold seven
    /// columns open in front of CTX for a state nothing is in. The gutter is
    /// what gives the columns room, and it is three.
    #[test]
    fn the_state_column_is_only_as_wide_as_the_states_on_the_board() {
        let (repo, _root_guard) = fixture("state-column");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let mut board = Board::for_test();
        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        let header = frame
            .lines()
            .find(|l| l.contains("STATE"))
            .expect("a header row");
        let row = frame.lines().find(|l| l.contains("login")).unwrap();

        // `○ queued` is the widest state here, so CTX starts right after it
        // rather than out where a longer state like `● running` would end.
        let ctx_at = header.find("CTX").unwrap() - header.find("STATE").unwrap();
        assert_eq!(
            ctx_at,
            "○ queued".chars().count() + GUTTER.len(),
            "{header}"
        );
        assert!(row.contains("○ queued"), "{row}");
    }

    /// A paused task has exactly one thing anybody can do about it, and the row
    /// is where they will be looking when they decide to — so the row carries
    /// the command rather than a description of the state.
    #[test]
    fn a_paused_row_names_the_resume_that_frees_it() {
        let (repo, _root_guard) = fixture("paused-row");
        let pipelines = Pipelines::builtin();
        add(&repo, "ship", &[], Some("handover"));
        let mut task = repo.task("ship").unwrap();
        task.front.paused_at = Some("handover".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = rows.iter().find(|r| r.id == "ship").unwrap();
        assert!(matches!(row.state, State::Paused));
        // Nothing holds this task back — no dependency, no lane of its own —
        // so the resume key is offered, then the arrow and the step passing
        // the gate would carry it to, `done` — `handover` is the last step
        // in the pipeline now.
        assert!(row.resumable);
        assert_eq!(row.next, "[r] → done");
    }

    /// Keeps only [`RECENT`] of them, oldest first out — each its own task, so
    /// this is the plain ring-buffer trim rather than the coalescing
    /// [`two_moves_of_the_same_task_coalesce_into_one_row_on_top`]
    /// covers.
    #[test]
    fn the_ticker_keeps_only_the_newest_transitions() {
        let mut recent = VecDeque::new();
        for i in 0..10 {
            push_recent(
                &mut recent,
                RecentEvent::Arrival {
                    at: "10:00".into(),
                    id: format!("task-{i}"),
                    change: Move::Started {
                        to: format!("line {i}"),
                    },
                },
            );
        }
        assert_eq!(recent.len(), RECENT);
        assert!(matches!(
            recent.front().unwrap(),
            RecentEvent::Arrival { change: Move::Started { to }, .. } if to == "line 4"
        ));
        assert!(matches!(
            recent.back().unwrap(),
            RecentEvent::Arrival { change: Move::Started { to }, .. } if to == "line 9"
        ));
    }

    /// Two moves of the same task inside the window are one row, not two: the
    /// later move replaces the earlier one, and it sits on top of the block —
    /// the newest news, drawn first.
    #[test]
    fn two_moves_of_the_same_task_coalesce_into_one_row_on_top() {
        let mut recent = VecDeque::new();
        push_recent(
            &mut recent,
            RecentEvent::Arrival {
                at: "14:20".into(),
                id: "other-task".into(),
                change: Move::Started {
                    to: "review".into(),
                },
            },
        );
        push_recent(
            &mut recent,
            RecentEvent::Arrival {
                at: "14:21".into(),
                id: "gate-board".into(),
                change: Move::Started { to: "task".into() },
            },
        );
        push_recent(
            &mut recent,
            RecentEvent::Arrival {
                at: "14:22".into(),
                id: "gate-board".into(),
                change: Move::Started {
                    to: "checks".into(),
                },
            },
        );

        assert_eq!(
            recent.len(),
            2,
            "the first gate-board move is replaced by the second, not kept alongside it"
        );

        let block = strip(&ticker(&recent, 120, 5));
        let gate_lines: Vec<&str> = block.lines().filter(|l| l.contains("gate-board")).collect();
        assert_eq!(gate_lines.len(), 1, "{block}");
        assert!(gate_lines[0].contains("checks"), "{block}");

        let other_row = block
            .lines()
            .position(|l| l.contains("other-task"))
            .unwrap();
        let gate_row = block
            .lines()
            .position(|l| l.contains("gate-board"))
            .unwrap();
        assert!(gate_row < other_row, "the later move sits on top: {block}");
    }

    /// Entering the queue is not a step change: it is already a new row on
    /// the table, so a line in the ticker too would only repeat it. Nor is a
    /// task file simply taken away, with nothing in the archive to say it
    /// finished. A real step change still reaches the ticker, so this is not
    /// just the ticker going silent altogether.
    #[test]
    fn a_task_entering_the_queue_or_vanishing_pushes_no_ticker_entry() {
        let (repo, _root_guard) = fixture("queue-and-archive-silent");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "steady", &[], Some("implement"), Some("steady"));

        let mut board = Board::for_test();
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert!(
            board.recent.is_empty(),
            "the board's first frame writes nothing to the ticker"
        );

        // A new task enters the queue.
        add_to(&repo, "newcomer", &[], None, Some("newcomer"));
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert!(
            board.recent.is_empty(),
            "a task entering the queue pushes no ticker entry"
        );

        // "steady" moves a step, so this is not the ticker going silent for
        // every kind of event.
        let mut task = repo.task("steady").unwrap();
        task.set_stage("review", None);
        task.save().unwrap();
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert!(
            !board.recent.is_empty(),
            "a real step change still reaches the ticker"
        );

        // "newcomer"'s file is deleted: it leaves the queue with no archived
        // copy behind it.
        std::fs::remove_file(repo.queue_dir().join("newcomer.md")).unwrap();
        let before = board.recent.len();
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert_eq!(
            board.recent.len(),
            before,
            "a task file deleted with no archived copy pushes no ticker entry"
        );
    }

    /// One reading of `board` over `repo`, for the tests below that drive a
    /// task through its moves and read what the ticker kept.
    fn read_once(board: &mut Board, repo: &Repo, pipelines: &Pipelines) {
        board
            .frame(
                repo,
                pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
    }

    /// The ticker's lines as drawn, newest first, with no colour in them.
    fn drawn(board: &Board) -> String {
        strip(&ticker(&board.recent, 120, 10))
    }

    /// `task`'s file moved out of the queue into the archive on `stage`, the
    /// same rename the teardown makes once a task is finished.
    fn archive_on(repo: &Repo, id: &str, stage: &str) {
        let mut task = repo.task(id).unwrap();
        task.set_stage(stage, None);
        task.save().unwrap();
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::rename(
            repo.queue_dir().join(format!("{id}.md")),
            repo.archive_dir().join(format!("{id}.md")),
        )
        .unwrap();
    }

    /// A task that leaves the queue into the archive as `done` gets a line
    /// naming the step the board last saw it on, even when the board never
    /// saw it reach `done` itself.
    #[test]
    fn a_task_archived_as_done_reads_passed_its_last_step_moved_to_done() {
        let (repo, _root_guard) = fixture("recent-finished");
        let pipelines = Pipelines::builtin();
        add(&repo, "board-tabs", &[], Some("handover"));

        let mut board = Board::for_test();
        read_once(&mut board, &repo, &pipelines);
        archive_on(&repo, "board-tabs", crate::pipeline::DONE);
        read_once(&mut board, &repo, &pipelines);

        let block = drawn(&board);
        assert!(
            block.contains("board-tabs   passed handover, moved to done"),
            "{block}"
        );
    }

    /// A finished task's move onto `done` is drawn once. The board that saw
    /// it on `done` before it was archived has already said so, and the
    /// archived copy must not add `passed done`.
    #[test]
    fn a_task_seen_on_done_before_archiving_keeps_its_one_line() {
        let (repo, _root_guard) = fixture("recent-finished-seen-done");
        let pipelines = Pipelines::builtin();
        add(&repo, "board-tabs", &[], Some("handover"));

        let mut board = Board::for_test();
        read_once(&mut board, &repo, &pipelines);
        let mut task = repo.task("board-tabs").unwrap();
        task.set_stage(crate::pipeline::DONE, None);
        task.save().unwrap();
        read_once(&mut board, &repo, &pipelines);
        archive_on(&repo, "board-tabs", crate::pipeline::DONE);
        read_once(&mut board, &repo, &pipelines);

        let block = drawn(&board);
        assert!(
            block.contains("board-tabs   passed handover, moved to done"),
            "{block}"
        );
        assert!(!block.contains("passed done"), "{block}");
    }

    /// A task taken out with `u` goes back to the pending directory, not the
    /// archive, so it did not finish and gets no line.
    #[test]
    fn a_task_taken_out_with_u_gets_no_line() {
        let (repo, _root_guard) = fixture("recent-unqueued");
        let pipelines = Pipelines::builtin();
        add(&repo, "board-tabs", &[], None);

        let mut board = Board::for_test();
        read_once(&mut board, &repo, &pipelines);
        unqueue_task(&repo, "board-tabs").unwrap();
        assert!(!repo.queue_dir().join("board-tabs.md").exists());
        read_once(&mut board, &repo, &pipelines);

        assert!(board.recent.is_empty(), "{}", drawn(&board));
    }

    /// The archive holding a task is not enough: one archived on any stage
    /// but `done` did not finish, and gets no line.
    #[test]
    fn a_task_archived_on_another_stage_gets_no_line() {
        let (repo, _root_guard) = fixture("recent-archived-blocked");
        let pipelines = Pipelines::builtin();
        add(&repo, "board-tabs", &[], Some("handover"));

        let mut board = Board::for_test();
        read_once(&mut board, &repo, &pipelines);
        archive_on(&repo, "board-tabs", crate::pipeline::BLOCKED);
        read_once(&mut board, &repo, &pipelines);

        assert!(board.recent.is_empty(), "{}", drawn(&board));
    }

    // ---- RECENT: the sentence each road off a step reads back as ----

    /// A pipeline with one of every road a RECENT sentence tells apart: an
    /// agent step with no `on_fail` (`implement`) and one with
    /// (`review`), three `loop:` budgets (`review`, `fix` and `e2e`, the
    /// background step's `on_fail`), a command step
    /// (`test`), a background one (`suite`), a gated step (`approve`), a
    /// `last:` step (`handover`) and a staffed `blocked`.
    const ROADS: &str = "steps:\n  \
        - id: implement\n    agent: pi\n    on_pass: review\n  \
        - id: review\n    agent: pi\n    loop: 2\n    on_pass: test\n    on_fail: fix\n  \
        - id: fix\n    agent: pi\n    loop: 1\n    on_pass: review\n  \
        - id: test\n    run: 'true'\n    on_pass: suite\n    on_fail: fix\n  \
        - id: suite\n    run: 'true'\n    background: true\n    on_pass: approve\n    on_fail: e2e\n  \
        - id: e2e\n    agent: pi\n    loop: 1\n    on_pass: approve\n  \
        - id: approve\n    agent: pi\n    gate: true\n    on_pass: handover\n  \
        - id: handover\n    run: 'true'\n    last: true\n    on_pass: done\n  \
        - id: blocked\n    agent: pi\n    session: true\n";

    fn roads() -> Pipelines {
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", ROADS).unwrap(),
        );
        pipelines
    }

    /// Walk `task` onto each of `steps` in turn, banking an arrival at every
    /// one the way any route does — the history a `loop:` budget counts.
    fn walk(task: &mut crate::task::Task, steps: &[&str]) {
        for step in steps {
            task.set_stage(step, None);
        }
    }

    /// The RECENT sentence for task `t`'s next move: `setup` writes the task
    /// as it stands before the move, a reading sees it there, `road` moves
    /// it, and a second reading sees where it went — the board's own two
    /// readings, and nothing in between.
    fn line_after(
        name: &str,
        stage: &str,
        setup: impl FnOnce(&mut crate::task::Task),
        road: impl FnOnce(&Repo, &Pipelines),
    ) -> String {
        let (repo, _root_guard) = fixture(name);
        let pipelines = roads();
        add(&repo, "t", &[], None);
        let mut task = repo.task("t").unwrap();
        if stage != crate::pipeline::QUEUED {
            task.set_stage(stage, None);
        }
        setup(&mut task);
        task.save().unwrap();

        let mut board = Board::for_test();
        read_once(&mut board, &repo, &pipelines);
        road(&repo, &pipelines);
        read_once(&mut board, &repo, &pipelines);
        let RecentEvent::Arrival { change, .. } = board.recent.back().expect("the move is seen");
        view::sentence(change)
    }

    /// A lane on `t` reporting `outcome`: routed by `commands::route`, the
    /// decision `spoolway report` makes, then written the way `report`
    /// writes it — the report first, then the arrival. `report` itself is
    /// not called, since it commits whatever worktree the lane environment
    /// of the process running these tests names.
    fn lane_reports(repo: &Repo, pipelines: &Pipelines, outcome: Outcome) {
        let mut task = repo.task("t").unwrap();
        let current = task.stage().to_string();
        let pipeline = pipelines.for_task(&task).unwrap().clone();
        let routed =
            crate::commands::route(&mut task, &pipeline, &current, outcome, false, None, 0)
                .unwrap();
        task.front.last_report = Some(crate::task::LastReport {
            step: current,
            outcome: outcome.as_str().to_string(),
            at: chrono::Utc::now().timestamp(),
            blocked: routed.destination == crate::pipeline::BLOCKED,
        });
        task.set_stage(&routed.destination, routed.pause_note.as_deref());
        task.save().unwrap();
    }

    /// `t` moved to `stage` with no report: a command step's exit, or one of
    /// the dispatcher's own roads, which write nothing more than this.
    fn moved(repo: &Repo, stage: &str) {
        let mut task = repo.task("t").unwrap();
        task.set_stage(stage, None);
        task.save().unwrap();
    }

    fn reported(task: &mut crate::task::Task, step: &str, outcome: Outcome) {
        task.front.last_report = Some(crate::task::LastReport {
            step: step.into(),
            outcome: outcome.as_str().into(),
            at: 1,
            blocked: false,
        });
    }

    #[test]
    fn leaving_queued_reads_started() {
        let line = line_after(
            "road-started",
            "queued",
            |_| {},
            |repo, _| moved(repo, "implement"),
        );
        assert_eq!(line, "started, moved to implement");
    }

    #[test]
    fn a_reported_pass_onto_on_pass_reads_passed() {
        let line = line_after(
            "road-passed",
            "implement",
            |_| {},
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Pass),
        );
        assert_eq!(line, "passed implement, moved to review");
    }

    #[test]
    fn a_reported_failure_onto_on_fail_reads_failed() {
        let line = line_after(
            "road-failed",
            "review",
            |_| {},
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Fail),
        );
        assert_eq!(line, "failed review, moved to fix");
    }

    /// `implement` declares no `on_fail`, so its failure lands on `blocked`.
    #[test]
    fn a_reported_failure_with_no_on_fail_reads_failed_into_blocked() {
        let line = line_after(
            "road-failed-blocked",
            "implement",
            |_| {},
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Fail),
        );
        assert_eq!(line, "failed implement, moved to blocked");
    }

    /// A command step never reports: its exit routes it, so a move down its
    /// `on_pass` is a pass, never a skip.
    #[test]
    fn a_command_step_exiting_onto_on_pass_reads_passed() {
        let line = line_after(
            "road-command-pass",
            "test",
            |_| {},
            |repo, _| moved(repo, "suite"),
        );
        assert_eq!(line, "passed test, moved to suite");
    }

    /// The report left standing names `implement`, two steps back; `e2e`
    /// itself sent none since the task arrived there, so its move down its
    /// own `on_pass` is not a pass.
    #[test]
    fn an_agent_step_with_no_report_since_it_arrived_reads_skipped() {
        let line = line_after(
            "road-skipped",
            "e2e",
            |task| reported(task, "implement", Outcome::Pass),
            |repo, _| moved(repo, "approve"),
        );
        assert_eq!(line, "skipped e2e, moved to approve");
    }

    /// The task's own `skip:`, walked past the way `dispatch::fall_through`
    /// does — through `apply_loop_budget`, then down `on_pass`.
    #[test]
    fn a_step_named_in_skip_reads_skipped() {
        let line = line_after(
            "road-skip-named",
            "test",
            |task| task.front.skip = vec!["test".into()],
            |repo, pipelines| {
                let mut task = repo.task("t").unwrap();
                let pipeline = pipelines.for_task(&task).unwrap().clone();
                let to = crate::commands::apply_loop_budget(
                    &pipeline,
                    &mut task,
                    "test",
                    "suite".into(),
                    false,
                );
                task.set_stage(&to, None);
                task.save().unwrap();
            },
        );
        assert_eq!(line, "skipped test, moved to suite");
    }

    /// A `last:` step is walked past while a task in its own group still
    /// depends on this one — `handover` is a command step, so without the
    /// walk-past it would read as passed.
    #[test]
    fn a_last_step_walked_past_for_a_dependent_reads_skipped() {
        let (repo, _root_guard) = fixture("road-skip-last");
        let pipelines = roads();
        add(&repo, "t", &[], Some("handover"));
        add(&repo, "after", &["t"], None);
        let mut board = Board::for_test();
        read_once(&mut board, &repo, &pipelines);
        moved(&repo, crate::pipeline::DONE);
        read_once(&mut board, &repo, &pipelines);
        let line = board
            .recent
            .iter()
            .find_map(|RecentEvent::Arrival { id, change, .. }| {
                (id == "t").then(|| view::sentence(change))
            })
            .expect("the move is seen");
        assert_eq!(line, "skipped handover, moved to done");
    }

    #[test]
    fn a_pass_held_by_the_steps_gate_reads_gate() {
        let line = line_after(
            "road-gate",
            "approve",
            |_| {},
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Pass),
        );
        assert_eq!(line, "passed approve, moved to paused (gate)");
    }

    #[test]
    fn a_pass_held_by_the_tasks_own_schedule_reads_scheduled() {
        let line = line_after(
            "road-scheduled",
            "implement",
            |task| task.front.gate_at = Some("implement".into()),
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Pass),
        );
        assert_eq!(line, "passed implement, moved to paused (scheduled)");
    }

    /// `done`'s hook, as `Dispatcher::pause_for_hook_failure` writes it, on
    /// a task the board last saw on `handover` and never saw on `done`.
    #[test]
    fn a_failed_done_hook_reads_hook_failed() {
        let line = line_after(
            "road-hook-done",
            "handover",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                task.set_stage(crate::pipeline::DONE, None);
                task.front.hook_paused = Some(crate::pipeline::DONE.into());
                task.set_stage(
                    crate::pipeline::PAUSED,
                    Some("issue_tracking hook exited 1"),
                );
                task.save().unwrap();
            },
        );
        assert_eq!(line, "passed handover, moved to paused (hook failed)");
    }

    /// The same pause, seen from `done`: the step that passed is the one
    /// step whose `on_pass` is `done`.
    #[test]
    fn a_failed_done_hook_seen_from_done_names_the_step_that_passed() {
        let line = line_after(
            "road-hook-done-seen",
            crate::pipeline::DONE,
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                task.front.hook_paused = Some(crate::pipeline::DONE.into());
                task.set_stage(
                    crate::pipeline::PAUSED,
                    Some("issue_tracking hook exited 1"),
                );
                task.save().unwrap();
            },
        );
        assert_eq!(line, "passed handover, moved to paused (hook failed)");
    }

    #[test]
    fn a_failed_queued_hook_reads_hook_failed_before_starting() {
        let line = line_after(
            "road-hook-queued",
            "queued",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                task.front.hook_paused = Some(crate::pipeline::QUEUED.into());
                task.set_stage(
                    crate::pipeline::PAUSED,
                    Some("issue_tracking hook exited 1"),
                );
                task.save().unwrap();
            },
        );
        assert_eq!(
            line,
            "stopped before starting, moved to paused (hook failed)"
        );
    }

    /// The board's `p`. An Escape in the pane parks through the same
    /// `park`, with the same `false`.
    #[test]
    fn a_park_from_the_board_reads_manually() {
        let line = line_after(
            "road-manual",
            "implement",
            |_| {},
            |repo, _| park_under_lock(repo, "t", ParkedBy::Board).unwrap(),
        );
        assert_eq!(line, "stopped on implement, moved to paused (manually)");
    }

    #[test]
    fn a_park_from_queued_reads_manually_before_starting() {
        let line = line_after(
            "road-manual-queued",
            "queued",
            |_| {},
            |repo, _| park_under_lock(repo, "t", ParkedBy::Board).unwrap(),
        );
        assert_eq!(line, "stopped before starting, moved to paused (manually)");
    }

    #[test]
    fn a_park_by_the_stop_popup_reads_dispatching_stopped() {
        let line = line_after(
            "road-stop",
            "implement",
            |_| {},
            |repo, _| park_under_lock(repo, "t", ParkedBy::Stop).unwrap(),
        );
        assert_eq!(
            line,
            "stopped on implement, moved to paused (dispatching stopped)"
        );
    }

    /// `Dispatcher::tear_down_and_escalate`'s own park.
    #[test]
    fn a_lane_the_dispatcher_gave_up_on_reads_escalated() {
        let line = line_after(
            "road-escalated",
            "implement",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                park(&mut task, "`implement` went quiet", true);
                task.save().unwrap();
            },
        );
        assert_eq!(line, "stopped on implement, moved to paused (escalated)");
    }

    /// As `Dispatcher::pause_for_missing_start_branch` writes it.
    #[test]
    fn a_missing_start_branch_reads_branch_missing() {
        let line = line_after(
            "road-branch",
            "queued",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                task.front.missing_start_branch = Some("feat/x".into());
                task.set_stage(
                    crate::pipeline::PAUSED,
                    Some("start branch `feat/x` does not exist"),
                );
                task.save().unwrap();
            },
        );
        assert_eq!(
            line,
            "stopped before starting, moved to paused (branch feat/x missing)"
        );
    }

    #[test]
    fn a_pause_reported_from_blocked_reads_reported() {
        let line = line_after(
            "road-blocked-pause",
            "blocked",
            |task| task.front.blocked_from = Some("review".into()),
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Pause),
        );
        assert_eq!(line, "reported a pause on blocked, moved to paused");
    }

    /// `Dispatcher::escalate` on a `blocked` lane that could not be started:
    /// no report, and the step to resume stamped in `paused_at`.
    #[test]
    fn a_blocked_lane_escalated_with_no_report_reads_escalated() {
        let line = line_after(
            "road-blocked-escalated",
            "blocked",
            |task| task.front.blocked_from = Some("review".into()),
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                task.reset_loop_counts();
                task.front.paused_at = task.front.blocked_from.clone();
                task.set_stage(crate::pipeline::PAUSED, Some("could not start `blocked`"));
                task.save().unwrap();
            },
        );
        assert_eq!(line, "stopped on blocked, moved to paused (escalated)");
    }

    #[test]
    fn a_block_reported_by_a_lane_reads_reported() {
        let line = line_after(
            "road-block",
            "review",
            |_| {},
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Block),
        );
        assert_eq!(line, "reported a block on review, moved to blocked");
    }

    /// `fix` has `loop: 1` and has had its one arrival, so `review`'s
    /// failure is refused by it and carried on to `blocked`.
    #[test]
    fn a_failure_refused_by_a_spent_loop_reads_loop_limit() {
        let line = line_after(
            "road-loop-fail",
            "review",
            |task| walk(task, &["fix", "review"]),
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Fail),
        );
        assert_eq!(line, "failed review, moved to blocked (loop limit on fix)");
    }

    /// `review` has `loop: 2` and has had both, so `fix`'s pass is refused.
    #[test]
    fn a_pass_refused_by_a_spent_loop_reads_loop_limit() {
        let line = line_after(
            "road-loop-pass",
            "review",
            |task| walk(task, &["fix", "review", "fix"]),
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Pass),
        );
        assert_eq!(line, "passed fix, moved to blocked (loop limit on review)");
    }

    /// Three refused launches, counted by `note_launch_failure` and routed
    /// by `handle_boot_failure`. Arriving at `blocked` clears only
    /// `blocked`'s own count, so `implement`'s survives the move.
    #[test]
    fn a_launch_refused_three_times_reads_attempts() {
        let line = line_after(
            "road-launch",
            "implement",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                for _ in 0..crate::dispatch::MAX_LAUNCH_FAILURES {
                    task.bump_launch_failures("implement");
                }
                crate::commands::set_blocked_from(&mut task, "implement");
                task.set_stage(crate::pipeline::BLOCKED, None);
                task.save().unwrap();
            },
        );
        assert_eq!(
            line,
            "could not launch implement, moved to blocked (3 attempts)"
        );
    }

    /// The cause is dropped here: `note_pane_busy` clears the step's
    /// `launch_busy_since` before it routes the task down `on_fail`, so
    /// nothing in the file says the pane never got ready. The sentence still
    /// says the lane never launched, since `review` sent no report.
    #[test]
    fn a_pane_that_never_got_ready_reads_could_not_launch_without_its_cause() {
        let line = line_after(
            "road-pane-busy",
            "review",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                task.stamp_launch_busy("review", 1);
                task.clear_launch_busy("review");
                task.set_stage("fix", None);
                task.save().unwrap();
            },
        );
        assert_eq!(line, "could not launch review, moved to fix");
    }

    /// `Dispatcher::escalate` for a lane launched and gone with no report.
    /// `review` routes a refused launch to `fix`, so `blocked` can only be
    /// this road. The report left standing is `implement`'s, from an
    /// earlier stop, and is not read as `review`'s block.
    #[test]
    fn a_lane_dead_at_launch_reads_lane_died_and_ignores_an_older_report() {
        let line = line_after(
            "road-died",
            "review",
            |task| reported(task, "implement", Outcome::Block),
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                crate::commands::set_blocked_from(&mut task, "review");
                task.set_stage(crate::pipeline::BLOCKED, Some("started 1 time(s)"));
                task.save().unwrap();
            },
        );
        assert_eq!(
            line,
            "could not launch review, moved to blocked (lane died at launch)"
        );
    }

    /// `Dispatcher::tear_down_and_escalate` in an unattended run: the file
    /// matches a lane dead at launch except for the `escalated` mark, and
    /// the line must say the lane was stopped, not that a launch failed.
    #[test]
    fn a_lane_the_dispatcher_stopped_into_blocked_reads_escalated() {
        let line = line_after(
            "road-stopped-into-blocked",
            "review",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                crate::commands::set_blocked_from(&mut task, "review");
                task.front.escalated = true;
                task.set_stage(crate::pipeline::BLOCKED, Some("went quiet"));
                task.save().unwrap();
            },
        );
        assert_eq!(line, "stopped on review, moved to blocked (escalated)");
    }

    /// The `escalated` mark belongs to the stop that set it. After the
    /// unblocker's pass carries the task off `blocked`, a later block on it
    /// is an ordinary reported block and must read as one.
    #[test]
    fn a_block_after_an_escalation_was_cleared_reads_as_a_reported_block() {
        let line = line_after(
            "road-block-after-escalation",
            "review",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                crate::commands::set_blocked_from(&mut task, "review");
                task.front.escalated = true;
                task.set_stage(crate::pipeline::BLOCKED, Some("went quiet"));
                task.set_stage("review", Some("unblocked"));
                reported(&mut task, "review", Outcome::Block);
                task.set_stage(crate::pipeline::BLOCKED, Some("needs a person"));
                task.save().unwrap();
            },
        );
        assert_eq!(line, "reported a block on review, moved to blocked");
    }

    /// The cause is dropped here: `implement` has no `on_fail`, so a pane
    /// that never got ready would land on `blocked` too, with the same file
    /// behind it.
    #[test]
    fn a_lane_dead_at_launch_with_no_on_fail_reads_could_not_launch_without_its_cause() {
        let line = line_after(
            "road-died-no-fail",
            "implement",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                crate::commands::set_blocked_from(&mut task, "implement");
                task.set_stage(crate::pipeline::BLOCKED, Some("started 1 time(s)"));
                task.save().unwrap();
            },
        );
        assert_eq!(line, "could not launch implement, moved to blocked");
    }

    /// Cleanup's hold, as `Dispatcher::clean_up` writes it, on a task that
    /// reached `done` with work it could not commit.
    #[test]
    fn a_pass_into_done_held_on_blocked_reads_uncommitted_work() {
        let line = line_after(
            "road-uncommitted",
            "handover",
            |_| {},
            |repo, _| {
                let mut task = repo.task("t").unwrap();
                task.set_stage(crate::pipeline::DONE, None);
                crate::commands::set_blocked_from(&mut task, "handover");
                task.set_stage(
                    crate::pipeline::BLOCKED,
                    Some("uncommitted work could not be recorded before cleanup"),
                );
                task.save().unwrap();
            },
        );
        assert_eq!(line, "passed handover, moved to blocked (uncommitted work)");
    }

    /// `suite` ran in the background while the task walked on to `approve`,
    /// then exited non-zero: `reap_stale_runs` moves the task down
    /// `suite`'s `on_fail` from wherever it is.
    #[test]
    fn a_background_run_failing_reads_failed_in_the_background() {
        let line = line_after(
            "road-background",
            "approve",
            |_| {},
            |repo, pipelines| {
                let mut task = repo.task("t").unwrap();
                let pipeline = pipelines.for_task(&task).unwrap().clone();
                let to = crate::commands::apply_loop_budget(
                    &pipeline,
                    &mut task,
                    "approve",
                    "e2e".into(),
                    false,
                );
                task.set_stage(&to, None);
                task.save().unwrap();
            },
        );
        assert_eq!(line, "failed suite in the background, moved to e2e");
    }

    /// The cause is dropped here: `suite` fails in the background while the
    /// task works `review`, and `reap_stale_runs` finds `e2e`'s `loop:`
    /// already spent, so the task lands on `blocked`. `review` sent no
    /// report and routes a refused launch to `fix`, so without the
    /// background check this would read as a lane dead at launch.
    #[test]
    fn a_background_run_failing_into_a_spent_loop_reads_left_without_a_cause() {
        let line = line_after(
            "road-background-spent",
            "review",
            |task| walk(task, &["e2e", "review"]),
            |repo, pipelines| {
                let mut task = repo.task("t").unwrap();
                let pipeline = pipelines.for_task(&task).unwrap().clone();
                let to = crate::commands::apply_loop_budget(
                    &pipeline,
                    &mut task,
                    "review",
                    "e2e".into(),
                    false,
                );
                assert_eq!(to, crate::pipeline::BLOCKED);
                crate::commands::set_blocked_from(&mut task, "review");
                task.set_stage(&to, None);
                task.save().unwrap();
            },
        );
        assert_eq!(line, "left review, moved to blocked");
    }

    /// A lane working `blocked` passes, and the task goes one step past
    /// where it stopped.
    #[test]
    fn a_pass_from_a_lane_on_blocked_reads_unblocked_by_its_lane() {
        let line = line_after(
            "road-unblocked-lane",
            "blocked",
            |task| task.front.blocked_from = Some("review".into()),
            |repo, pipelines| lane_reports(repo, pipelines, Outcome::Pass),
        );
        assert_eq!(line, "unblocked by its lane, moved to test");
    }

    /// A person's resume, with no report from `blocked`.
    #[test]
    fn a_resume_off_blocked_reads_unblocked() {
        let line = line_after(
            "road-unblocked",
            "blocked",
            |task| task.front.blocked_from = Some("review".into()),
            |repo, _| moved(repo, "review"),
        );
        assert_eq!(line, "unblocked, moved to review");
    }

    #[test]
    fn leaving_paused_reads_resumed() {
        let line = line_after(
            "road-resumed",
            "paused",
            |_| {},
            |repo, _| moved(repo, "implement"),
        );
        assert_eq!(line, "resumed, moved to implement");
    }

    /// `launch_failures` for `implement` outlives its block, since only
    /// arriving back at `implement` clears it. `review` reaching `blocked`
    /// later on its own lane's word must not be read as `implement`'s
    /// refused launches, and nor must the field be read for the step that
    /// was left.
    #[test]
    fn a_launch_count_left_from_an_earlier_stop_is_not_this_ones_cause() {
        let mut stale = None;
        let line = line_after(
            "road-stale",
            "review",
            |task| {
                task.front
                    .launch_failures
                    .insert("implement".into(), crate::dispatch::MAX_LAUNCH_FAILURES);
            },
            |repo, pipelines| {
                lane_reports(repo, pipelines, Outcome::Block);
                stale = repo
                    .task("t")
                    .unwrap()
                    .front
                    .launch_failures
                    .get("implement")
                    .copied();
            },
        );
        assert_eq!(stale, Some(3), "the old count is still in the file");
        assert_eq!(line, "reported a block on review, moved to blocked");
    }

    /// A move into `paused` that no field explains goes without a cause.
    #[test]
    fn a_stop_nothing_explains_reads_stopped_without_a_cause() {
        let line = line_after(
            "road-stopped",
            "review",
            |_| {},
            |repo, _| moved(repo, crate::pipeline::PAUSED),
        );
        assert_eq!(line, "stopped on review, moved to paused");
    }

    /// `implement` routes to `review` on a pass and `blocked` on a failure;
    /// `handover` is neither, so the move is named without a guess.
    #[test]
    fn a_move_matching_no_route_reads_left() {
        let line = line_after(
            "road-left",
            "implement",
            |task| reported(task, "implement", Outcome::Pass),
            |repo, _| moved(repo, "handover"),
        );
        assert_eq!(line, "left implement, moved to handover");
    }

    // ---- board-resume: cursor, resume key, forward-looking ticker ----

    /// The row ids of one drawn frame, in order — what [`shift_cursor`]
    /// walks. See [`Board::drawn`].
    fn ids<const N: usize>(names: [&str; N]) -> Vec<String> {
        names.iter().map(|id| id.to_string()).collect()
    }

    /// `↓` from no cursor at all lands on the first row rather than the
    /// second — there is nothing to move away from yet.
    #[test]
    fn moving_the_cursor_with_nothing_selected_lands_on_the_first_row() {
        let rows = ids(["a", "b", "c"]);
        assert_eq!(shift_cursor(&rows, None, 1), Some("a".to_string()));
        assert_eq!(shift_cursor(&rows, None, -1), Some("a".to_string()));
    }

    /// `↓` and `↑` step through the board's own row order and wrap at either
    /// end, rather than sticking or landing off the table.
    #[test]
    fn the_cursor_steps_through_rows_and_wraps_at_either_end() {
        let rows = ids(["a", "b", "c"]);
        assert_eq!(shift_cursor(&rows, Some("a"), 1), Some("b".to_string()));
        assert_eq!(shift_cursor(&rows, Some("c"), 1), Some("a".to_string()));
        assert_eq!(shift_cursor(&rows, Some("a"), -1), Some("c".to_string()));
    }

    /// A row the board no longer shows — its task passed its step, or
    /// vanished between two frames — must not strand the cursor: the next
    /// press lands on the first row still there rather than nowhere.
    #[test]
    fn the_cursor_resets_to_the_first_row_when_its_own_row_is_gone() {
        let rows = ids(["a", "b"]);
        assert_eq!(shift_cursor(&rows, Some("gone"), 1), Some("a".to_string()));
    }

    /// An empty board has nowhere for the cursor to sit.
    #[test]
    fn the_cursor_has_nowhere_to_go_on_an_empty_board() {
        assert_eq!(shift_cursor(&[], Some("a"), 1), None);
    }

    /// A fresh board already has its cursor on the first row before any key
    /// is read at all — `o`, `p` and the rest all act on it from the very
    /// first frame, rather than only after a first `↓` discovers it.
    #[test]
    fn a_fresh_board_starts_with_the_cursor_on_the_first_row() {
        let (repo, _root_guard) = fixture("cursor-starts-on-first-row");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let mut board = Board::for_test();
        assert_eq!(board.cursor, None, "nothing has drawn a frame yet");
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("login"));
    }

    /// `↑`/`↓` walk the last frame's own rows and read nothing off disk, so
    /// an arrow answers even while a pass is rewriting and archiving the
    /// very task files the cursor used to be worked out from. The queue
    /// emptying under the board here stands in for that window: the rows are
    /// gone from disk, and the marker still steps through the frame that is
    /// on screen.
    #[test]
    fn an_arrow_walks_the_last_frame_even_with_the_queue_gone_from_disk() {
        let (repo, _root_guard) = fixture("cursor-walks-the-drawn-frame");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "login", &[], Some("implement"), Some("login"));
        add_to(&repo, "signup", &[], Some("implement"), Some("signup"));

        let mut board = Board::for_test();
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert_eq!(board.drawn, vec!["login".to_string(), "signup".to_string()]);

        std::fs::remove_dir_all(repo.queue_dir()).unwrap();
        assert!(repo.tasks().unwrap().is_empty(), "the queue is gone");

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("signup"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Up)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("login"));
    }

    /// A group finishing takes its last row off the board entirely — see
    /// `done_rows_only_appear_for_a_group_still_in_the_queue` — and a cursor
    /// still naming that row must not strand there with nothing drawn under
    /// it. The very next frame, with no key pressed, lands it back on the
    /// first row still on the board.
    #[test]
    fn the_cursor_falls_back_to_the_first_row_when_its_group_finishes() {
        let (repo, _root_guard) = fixture("cursor-group-finishes");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "login", &[], Some("implement"), Some("auth"));
        add(&repo, "other", &[], None);

        let mut board = Board::for_test();
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board.cursor = Some("login".to_string());

        // "login" finishes and leaves the queue; nothing else in "auth" is
        // still queued, so the group drops off the board entirely rather
        // than lingering as a done row.
        std::fs::remove_file(repo.queue_dir().join("login.md")).unwrap();

        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert_eq!(
            board.cursor.as_deref(),
            Some("other"),
            "the cursor falls back to the first remaining row once its own row is gone"
        );
    }

    /// The cursor walks the archived rows the board draws too, not just the
    /// live queue: with a group holding one live task and one done one, `↓`
    /// from the live row reaches the done row, and `o` opens its task by
    /// way of `repo.task`'s own archive lookup rather than refusing because
    /// the queue no longer holds the file.
    #[test]
    fn the_cursor_reaches_a_done_row_and_o_opens_its_task() {
        let (mut repo, _root_guard) = fixture("cursor-reaches-done-row");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add_to(&repo, "login", &[], Some("implement"), Some("auth"));
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("signup.md"),
            "---\nid: signup\nstage: done\ngroup: auth\n---\n",
        )
        .unwrap();

        let mut board = Board::for_test();
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert_eq!(
            board.cursor.as_deref(),
            Some("login"),
            "the live row still sorts first within the group"
        );

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(
            board.cursor.as_deref(),
            Some("signup"),
            "`↓` walks onto the archived row"
        );

        // Headless refuses to open a pane at all, so this only proves `o`
        // never bails out before that — the archived task resolves through
        // `repo.task` rather than being treated as gone.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('o'))
            .unwrap();
    }

    /// A paused task offers the resume key only once its own dependencies
    /// have finished — the same rule that let it start at all, checked again
    /// because a board reads the graph fresh rather than trusting that a
    /// task already past `queued` must still pass it.
    #[test]
    fn a_paused_row_is_not_resumable_while_its_own_dependency_is_unfinished() {
        let (repo, _root_guard) = fixture("resume-deps");
        let pipelines = Pipelines::builtin();
        add(&repo, "blocker", &[], Some("implement"));
        add(&repo, "gate-board", &["blocker"], None);
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();

        assert!(!row.resumable, "{}", row.next);
        assert_eq!(row.next, "→ review");
    }

    /// A paused task whose `paused_at` names a step the pipeline does not
    /// have has nowhere to name. The row still says so rather than going
    /// blank, with the key when it is on offer and without it when not.
    #[test]
    fn a_paused_row_with_no_named_step_still_says_something() {
        let (repo, _root_guard) = fixture("paused-no-step");
        let pipelines = Pipelines::builtin();
        add(&repo, "blocker", &[], Some("implement"));
        add_to(&repo, "free", &[], None, Some("free"));
        add(&repo, "held", &["blocker"], None);
        for id in ["free", "held"] {
            let mut task = repo.task(id).unwrap();
            task.front.paused_at = Some("no-such-step".into());
            task.set_stage(crate::pipeline::PAUSED, None);
            task.save().unwrap();
        }

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let free = rows.iter().find(|r| r.id == "free").unwrap();
        assert!(free.resumable);
        assert_eq!(free.next, "[r] resume");
        let held = rows.iter().find(|r| r.id == "held").unwrap();
        assert!(!held.resumable);
        assert_eq!(held.next, "no step named");
    }

    /// A paused task offers the resume key only once no lane of its own is
    /// mid-turn — a held pane a person is actively typing into, or an agent
    /// still running in it. Settled again, the same row is resumable.
    #[test]
    fn a_paused_row_is_not_resumable_while_its_lane_is_busy() {
        let (repo, _root_guard) = fixture("resume-busy");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "gate-board", &[], None, Some("gate-board"));
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());

        // Held pane, still working — the lane keeps the name of the step it
        // paused at, not `paused` itself, which never starts a lane of its
        // own.
        let mut busy = lane("gate-board · implement", &repo.root);
        busy.status = crate::mux::LaneStatus::Working;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[busy], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();
        assert!(!row.resumable, "{}", row.next);
        assert_eq!(row.next, "→ review");

        // Same pane, settled: the round is over and the key comes back.
        let mut settled = lane("gate-board · implement", &repo.root);
        settled.status = crate::mux::LaneStatus::Done;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[settled], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();
        assert!(row.resumable, "{}", row.next);
        assert_eq!(row.next, "[r] → review");
    }

    /// A row parked by a person's own Escape or the board's `p`
    /// (`parked_from` set, `escalated: false`) is a different case from a
    /// gate: its own lane is expected to still be alive, typed into or
    /// working away, and `r` has to reach it anyway — bug gh group
    /// `parked-task-resume`'s "`r` does nothing" case. A gate's row still
    /// follows `a_paused_row_is_not_resumable_while_its_lane_is_busy`'s rule
    /// unchanged; this is the one shape that does not.
    #[test]
    fn a_parked_row_is_resumable_even_while_its_lane_is_working_or_blocked() {
        let (repo, _root_guard) = fixture("resume-parked-busy");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "gate-board", &[], None, Some("gate-board"));
        let mut task = repo.task("gate-board").unwrap();
        task.front.parked_from = Some("implement".into());
        task.front.escalated = false;
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());

        let mut working = lane("gate-board · implement", &repo.root);
        working.status = crate::mux::LaneStatus::Working;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[working], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();
        assert!(
            row.resumable,
            "a parked row's own lane being Working must not hide the key: {}",
            row.next
        );

        let mut blocked = lane("gate-board · implement", &repo.root);
        blocked.status = crate::mux::LaneStatus::Blocked;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[blocked], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();
        assert!(row.resumable, "nor Blocked: {}", row.next);
    }

    /// A blocked row that is actually parked for a person — nobody staffs
    /// that step — follows the same dependency and busy-lane rule as a
    /// paused one for whether the key does anything, and now says so the
    /// same way a paused row does: the key, then the arrow.
    #[test]
    fn a_parked_blocked_row_is_resumable_by_the_same_rule_as_a_paused_one() {
        let (repo, _root_guard) = fixture("resume-blocked");
        let pipelines = Pipelines::builtin();
        add(&repo, "wall", &[], None);
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "wall").unwrap();

        assert!(row.resumable, "{}", row.next);
        assert_eq!(row.next, "[r] → implement");
    }

    /// A task paused by a `--pause`, `--fail` or `--block` from `blocked` —
    /// told apart from an ordinary gate by `blocked_from` naming the same
    /// step as `paused_at` — reads its NEXT column as the step it blocked on,
    /// not past it: `paused_next`'s cleared-block branch now passes
    /// `takes_over: false` to `cleared_block_target`, the same rule
    /// `resume_road` resumes it by.
    #[test]
    fn a_cleared_block_row_names_the_step_it_blocked_on_not_past_it() {
        let (repo, _root_guard) = fixture("paused-cleared-block");
        let pipelines = Pipelines::builtin();
        add(&repo, "wall", &[], None);
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "wall").unwrap();

        assert_eq!(
            row.next, "[r] → implement",
            "never past `implement`, unlike an ordinary gate's own `on_pass`"
        );
    }

    /// `r` then `enter` on a paused row goes through exactly the code
    /// `spoolway release` runs: the task moves off `paused` onto its gate's
    /// `on_pass` destination, with `paused_at` cleared.
    #[test]
    fn r_on_a_resumable_paused_row_releases_it() {
        let (repo, _root_guard) = fixture("resume-key-release");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "gate-board", &[], None, Some("gate-board"));
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("gate-board"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();
        assert!(matches!(board.mode, BoardMode::ResumePicker(_)));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        let task = repo.task("gate-board").unwrap();
        assert_eq!(task.stage(), "review", "{}", task.stage());
        assert_eq!(task.front.paused_at, None);
    }

    /// A blocked row's `[r]` hint names the step `spoolway resume` actually
    /// sends the task to: the step it stopped on, not the one a pass from it
    /// would reach.
    #[test]
    fn a_blocked_rows_resume_hint_names_the_step_resume_goes_to() {
        let (repo, _root_guard) = fixture("blocked-hint-matches-resume");
        let pipelines = Pipelines::builtin();
        add(&repo, "wall", &[], None);
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "wall").unwrap();

        let pipeline = pipelines.pipelines.get("default").unwrap();
        let goes_to = crate::commands::resume_target(&task, pipeline);
        assert_eq!(
            row.next,
            format!("[r] → {goes_to}"),
            "the hint and `spoolway resume` must agree"
        );
    }

    /// A parked block still waiting on a dependency has no key to offer yet,
    /// and still names the step `spoolway resume` will send it to.
    #[test]
    fn a_parked_block_not_yet_resumable_names_the_same_step_as_when_ready() {
        let (repo, _root_guard) = fixture("blocked-not-ready");
        let pipelines = Pipelines::builtin();
        add(&repo, "base", &[], None);
        add(&repo, "wall", &["base"], None);
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = rows.iter().find(|r| r.id == "wall").unwrap();
        assert!(!row.resumable, "{}", row.next);
        assert_eq!(row.next, "→ implement");
    }

    /// A task on a stage its pipeline lacks reads as an unknown step, not as
    /// a blocked one: nothing on that row can be resumed.
    #[test]
    fn a_row_on_an_unknown_step_is_not_blocked() {
        let (repo, _root_guard) = fixture("unknown-step-row");
        let pipelines = Pipelines::builtin();
        add(&repo, "lost", &[], None);
        let mut task = repo.task("lost").unwrap();
        task.set_stage("nowhere", None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "lost").unwrap();
        assert!(matches!(row.state, State::Unknown));
        assert_eq!(row.state.word(), "● unknown");
        assert!(!row.resumable);
    }

    /// `r` then `enter` on a blocked row goes through exactly the code
    /// `spoolway unblock` runs: the task resumes at the step it stopped on,
    /// with `blocked_from` cleared.
    #[test]
    fn r_on_a_resumable_blocked_row_unblocks_it() {
        let (repo, _root_guard) = fixture("resume-key-unblock");
        let pipelines = Pipelines::builtin();
        add(&repo, "wall", &[], None);
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        let task = repo.task("wall").unwrap();
        assert_eq!(task.stage(), "implement", "{}", task.stage());
        assert_eq!(task.front.blocked_from, None);
    }

    /// `r` on a task parked before it ever started — no `paused_at`,
    /// `parked_from` or `blocked_from` at all, exactly what `park` leaves on
    /// a task still on `queued` — still resumes it, back onto `queued` where
    /// the dependency and hook gates apply again (jobs review finding 1):
    /// `resume_task`'s guard reads `stage()`, not the three fields, exactly
    /// so this case is never mistaken for "nothing to resume".
    #[test]
    fn r_on_a_task_parked_before_it_started_puts_it_back_on_queued() {
        let (repo, _root_guard) = fixture("resume-key-queued-park");
        let pipelines = Pipelines::builtin();
        add(&repo, "never-run", &[], None);
        let mut task = repo.task("never-run").unwrap();
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("never-run"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();

        let task = repo.task("never-run").unwrap();
        assert_eq!(task.stage(), crate::pipeline::QUEUED, "{}", task.stage());
        assert_eq!(task.front.parked_from, None);
        assert_eq!(task.front.resume, None);
    }

    /// A row parked off `queued` has no step of its own to check a
    /// dependency or a lane against — `resumable` reads `true` outright, and
    /// NEXT says so plainly, even while the dependency that would have gated
    /// it on `queued` is still unfinished. The complement of
    /// [`a_paused_row_is_not_resumable_while_its_own_dependency_is_unfinished`],
    /// which is about a row with a real step to check.
    #[test]
    fn a_row_parked_off_queued_is_resumable_however_its_dependency_stands() {
        let (repo, _root_guard) = fixture("resume-queued-park-deps");
        let pipelines = Pipelines::builtin();
        add(&repo, "blocker", &[], Some("implement"));
        add(&repo, "never-run", &["blocker"], None);
        let mut task = repo.task("never-run").unwrap();
        park(&mut task, "paused from the board", false);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "never-run").unwrap();

        assert!(row.resumable, "{}", row.next);
        assert_eq!(row.next, "→ queued — [r] resumes it");
    }

    /// `R` is no longer bound: a paused gate and a park beside it both stay
    /// exactly where they were, and no panel opens.
    #[test]
    fn shift_r_does_nothing() {
        let (repo, _root_guard) = fixture("shift-r-unbound");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "gate-board", &[], None, Some("gate-board"));
        let mut gated = repo.task("gate-board").unwrap();
        gated.front.paused_at = Some("implement".into());
        gated.set_stage(crate::pipeline::PAUSED, None);
        gated.save().unwrap();
        add_to(&repo, "quiet-pane", &[], None, Some("quiet-pane"));
        let mut parked = repo.task("quiet-pane").unwrap();
        parked.front.parked_from = Some("review".into());
        parked.set_stage(crate::pipeline::PAUSED, None);
        parked.save().unwrap();

        let mut board = Board::for_test();
        board.cursor = Some("gate-board".to_string());
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
            .unwrap();

        assert!(matches!(board.mode, BoardMode::Browsing));
        for id in ["gate-board", "quiet-pane"] {
            assert_eq!(repo.task(id).unwrap().stage(), crate::pipeline::PAUSED);
        }
    }

    /// `r` on a row whose own rule says it is not resumable does
    /// nothing: the task stays exactly where it was.
    #[test]
    fn r_on_a_row_that_is_not_resumable_does_nothing() {
        let (repo, _root_guard) = fixture("resume-key-refused");
        let pipelines = Pipelines::builtin();
        add(&repo, "blocker", &[], Some("implement"));
        add(&repo, "gate-board", &["blocker"], None);
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();

        assert!(matches!(board.mode, BoardMode::Browsing), "no picker opens");
        let task = repo.task("gate-board").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("implement"));
    }

    /// `enter` and `q` are read while browsing but do nothing: resuming now
    /// belongs to `r`, and quitting to `ctrl-c` alone.
    #[test]
    fn enter_and_q_are_ignored_while_browsing() {
        let (repo, _root_guard) = fixture("enter-and-q-ignored");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "gate-board", &[], None, Some("gate-board"));
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('q'))
            .unwrap();

        // Resumable and gated, so a `enter` or `q` that secretly still acted
        // would have moved it past `implement` or opened a panel over it.
        let task = repo.task("gate-board").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("implement"));
        let frame = board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert!(!frame.contains("┌─"), "{frame}");
    }

    /// A popup taller than the frame under it — the dispatch tab's warnings
    /// over an empty queue — still draws whole, down to its key line: the
    /// frame grows blank rows for it rather than cutting it off.
    #[test]
    fn a_popup_taller_than_the_frame_still_draws_its_key_line() {
        let (repo, _root_guard) = fixture("popup-taller-than-frame");
        let pipelines = Pipelines::builtin();
        let body: Vec<String> = (0..40).map(|i| format!("finding {i}")).collect();
        let panel = crate::screen::panel("before dispatching", &body, "[enter] its own key");

        let mut board = Board::for_test();
        let frame = board
            .hosted_frame(&repo, &pipelines, false, Some(&panel))
            .unwrap();
        assert!(frame.contains("finding 39"), "{frame}");
        assert!(frame.contains("[enter] its own key"), "{frame}");
    }

    /// Inside the dispatch tab the board draws in an untitled box,
    /// and a popup the tab opens still lands on it: every row of the box,
    /// the popup's included, ends in the box's right border, and the key
    /// line stays under the bottom border.
    #[test]
    fn a_hosted_board_draws_in_a_box_and_a_popup_lands_inside_it() {
        let (repo, _root_guard) = fixture("hosted-board-boxed");
        let pipelines = Pipelines::builtin();
        add(&repo, "boxed-row", &[], None);
        let _hosting = crate::screen::shell::Hosting::open(crate::screen::shell::Tab::Dispatch);
        let mut board = Board::for_test();
        let width = pane_width() + 2;
        for popup in [
            None,
            Some(crate::screen::panel(
                "a popup",
                &["over it".into()],
                &crate::screen::confirm(),
            )),
        ] {
            let frame = board
                .hosted_frame(&repo, &pipelines, false, popup.as_deref())
                .unwrap();
            let lines: Vec<String> = frame.lines().map(strip_ansi).collect();
            assert!(lines[0].starts_with("┌──"), "{frame}");
            let bottom = lines.iter().position(|l| l.starts_with('└')).unwrap();
            for line in &lines[..=bottom] {
                assert_eq!(line.chars().count(), width, "{line:?}");
            }
            for line in &lines[1..bottom] {
                assert!(line.starts_with('│') && line.ends_with('│'), "{line:?}");
            }
            assert!(
                lines[bottom + 1].contains("[enter] start dispatching"),
                "{frame}"
            );
            if popup.is_some() {
                assert!(frame.contains("over it"), "{frame}");
            }
        }
    }

    /// A confirm panel is drawn in plain text over a frame the table beneath
    /// it colours throughout — the state dot, the dimmed footer, the key
    /// hint. `overlay` counts an escape byte as a column exactly like a
    /// visible one, and a naive overlay wrote panel text at the wrong
    /// column for it, slicing a `DIM`/`RESET` pair in two and leaving what
    /// survived of the code sitting in the frame as stray text — this pins
    /// that every escape sequence in the raw, unstripped frame is still
    /// whole, an `ESC[` opened and a bare `m` closing it with nothing but
    /// digits and `;` between, panel open or not.
    #[test]
    fn a_confirm_panel_never_slices_a_coloured_rows_escape_sequence() {
        let (repo, _root_guard) = fixture("panel-colour-safe");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "gate-board", &[], None, Some("gate-board"));
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        board.cursor = Some("gate-board".to_string());
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();
        let frame = board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert!(
            frame.contains("resume gate-board"),
            "the panel itself must have opened: {frame}"
        );

        let mut chars = frame.chars();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                continue;
            }
            assert_eq!(
                chars.next(),
                Some('['),
                "an escape byte must open a CSI sequence, not sit on its own: {frame}"
            );
            let mut closed = false;
            for c in chars.by_ref() {
                if c == 'm' {
                    closed = true;
                    break;
                }
                assert!(
                    c.is_ascii_digit() || c == ';',
                    "a CSI sequence held something other than digits and `;`: {c:?} in {frame}"
                );
            }
            assert!(
                closed,
                "an escape sequence opened but never closed: {frame}"
            );
        }
    }

    /// A stop's `i` kills a running command step and parks its task with
    /// the stop's mark beside `parked_from` — and touches nothing that had
    /// nothing running: an idle task on a step stays exactly where it was.
    #[test]
    fn a_stop_interrupt_kills_a_running_command_step_and_marks_its_park() {
        let (repo, _root_guard) = fixture("stop-interrupt-command-step");
        let pipelines = Pipelines::builtin();
        // `handover` is the default pipeline's command step.
        add_to(&repo, "login", &[], Some("handover"), Some("login"));
        add_to(&repo, "idle", &[], Some("implement"), Some("idle"));

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        runs.start(&key, "sleep 30", &repo.root, &BTreeMap::new())
            .unwrap();

        interrupt_for_stop(&repo, &pipelines).unwrap();

        assert_ne!(runs.state(&key), crate::command_step::RunState::Running);
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.parked_from.as_deref(), Some("handover"));
        assert!(task.front.parked_by_stop);
        assert_eq!(task.front.blocked_from, None);
        assert_eq!(task.front.paused_at, None);

        let idle = repo.task("idle").unwrap();
        assert_eq!(idle.stage(), "implement");
        assert!(!idle.front.parked_by_stop);

        runs.stop(&key);
    }

    /// `p`, on the cursor's row, opens a panel scoped to just that task —
    /// titled with its id, and worded in the singular since killing it only
    /// ever stops this one task's step.
    #[test]
    fn pressing_p_on_the_cursor_with_a_command_step_running_opens_a_panel_scoped_to_it() {
        let (repo, _root_guard) = fixture("pause-cursor-command-step");
        let pipelines = Pipelines::builtin();
        // `handover` is the default pipeline's command step.
        add_to(&repo, "login", &[], Some("handover"), Some("login"));
        add_to(&repo, "other", &[], Some("implement"), Some("other"));

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        runs.start(&key, "sleep 30", &repo.root, &BTreeMap::new())
            .unwrap();

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("login"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(frame.contains("pause login"), "{frame}");
        assert!(frame.contains("handover"), "{frame}");
        assert!(frame.contains("command"), "{frame}");
        assert!(frame.contains("[enter] pause it"), "{frame}");

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();
        assert_ne!(runs.state(&key), crate::command_step::RunState::Running);
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.parked_from.as_deref(), Some("handover"));

        // The other task never entered the picture.
        assert_eq!(repo.task("other").unwrap().stage(), "implement");

        runs.stop(&key);
    }

    /// A stop's `i` interrupts a live agent lane this run owns and parks its
    /// task with the stop's mark, exercised against the headless backend,
    /// whose "interrupt" is ending the turn's process outright, the same as
    /// [`Mux::stop_lane`]. The next start's [`resume_stop_parked`] sends it
    /// back onto its step to continue its session, and spends the mark.
    #[test]
    fn a_stop_interrupt_ends_a_live_agent_turn_and_the_next_start_resumes_it() {
        let (mut repo, _root_guard) = fixture("stop-interrupt-live-lane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let (mux, name) = live_headless_lane(&repo);
        assert!(mux.list_lanes().unwrap().iter().any(|l| l.name == name));

        interrupt_for_stop(&repo, &pipelines).unwrap();

        assert!(
            mux.list_lanes().unwrap().iter().all(|l| l.name != name),
            "headless has no keyboard, so an interrupt ends the turn"
        );
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.parked_from.as_deref(), Some("implement"));
        assert!(task.front.parked_by_stop);

        let problems = resume_stop_parked(&repo, &pipelines).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.resume.as_deref(), Some("implement"));
        assert!(!task.front.parked_by_stop, "the resume spends the mark");
    }

    /// A start resumes only what a stop parked. A task a person paused with
    /// `p`, and one whose own Escape parked it (`park`, the same call
    /// `Dispatcher::park_after_interrupt` makes), carry no mark and stay on
    /// `paused`.
    #[test]
    fn a_start_resumes_only_the_tasks_a_stop_parked() {
        let (repo, _root_guard) = fixture("stop-resume-only-marked");
        let pipelines = Pipelines::builtin();
        for id in ["stopped", "by-hand", "escaped"] {
            add_to(&repo, id, &[], Some("implement"), Some(id));
        }
        park_under_lock(&repo, "stopped", ParkedBy::Stop).unwrap();
        park_under_lock(&repo, "by-hand", ParkedBy::Board).unwrap();
        let mut escaped = repo.task("escaped").unwrap();
        park(
            &mut escaped,
            "`implement` ended its turn on a person's own Escape",
            false,
        );
        escaped.save().unwrap();

        resume_stop_parked(&repo, &pipelines).unwrap();

        assert_eq!(repo.task("stopped").unwrap().stage(), "implement");
        for id in ["by-hand", "escaped"] {
            let task = repo.task(id).unwrap();
            assert_eq!(task.stage(), crate::pipeline::PAUSED, "{id}");
            assert!(!task.front.parked_by_stop, "{id}");
        }
    }

    /// The picker's reroute and the `s` restart route on what `routed_for`
    /// hands them: the running dispatcher's copy while one is up, the board's
    /// own otherwise — so a pipeline file broken since the run started fails
    /// neither key.
    #[test]
    fn a_key_moving_a_task_routes_on_the_running_dispatchers_pipelines() {
        let (repo, _root_guard) = fixture("routed-for-snapshot");
        add(&repo, "t1", &[], Some("implement"));
        // The board's graph: the files, with the pipeline renamed since the
        // run started.
        let mut board = Pipelines::builtin();
        let default = board.pipelines.remove("default").unwrap();
        board.pipelines.insert("edited".into(), default);

        assert!(
            routed_for(&repo, &board, "t1")
                .unwrap()
                .names()
                .contains(&"edited")
        );

        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        crate::pipeline_snapshot::write(&repo, &Pipelines::builtin()).unwrap();
        assert_eq!(
            routed_for(&repo, &board, "t1").unwrap().names(),
            vec!["default"]
        );
    }

    /// `r` on a gate sends the task where the running dispatcher's copy of
    /// its pipeline says, not where the board's own file-read graph says: a
    /// pipeline edited mid-run must not move a task onto a step the run
    /// never loaded. With no dispatcher running, the board's graph decides.
    #[test]
    fn r_on_a_gate_routes_on_the_running_dispatchers_pipelines() {
        let (repo, _root_guard) = fixture("resume-on-snapshot");
        let loaded = Pipelines::builtin();
        let loaded_next = loaded
            .get("default")
            .unwrap()
            .step("implement")
            .unwrap()
            .on_pass
            .clone()
            .unwrap();
        // The board's graph: the same file, edited so a gate passed at
        // `implement` skips straight to `document`.
        let mut edited = loaded.clone();
        let default = edited.pipelines.get_mut("default").unwrap();
        default
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap()
            .on_pass = Some("document".into());
        assert_ne!(loaded_next, "document", "the edit must change the route");

        let gate = |id: &str| {
            add_to(&repo, id, &[], Some("implement"), Some(id));
            let mut task = repo.task(id).unwrap();
            task.front.paused_at = Some("implement".into());
            task.set_stage(crate::pipeline::PAUSED, None);
            task.save().unwrap();
        };

        gate("unwatched");
        resume_task(&repo, &edited, "unwatched").unwrap();
        assert_eq!(repo.task("unwatched").unwrap().stage(), "document");

        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        crate::pipeline_snapshot::write(&repo, &loaded).unwrap();
        gate("running");
        resume_task(&repo, &edited, "running").unwrap();
        assert_eq!(repo.task("running").unwrap().stage(), loaded_next);
    }

    /// A task a stop parked and a person then resumed by hand, then parked
    /// again with `p`, is the person's park now: neither `r` nor `p` leaves
    /// the stop's mark behind for the next start to find.
    #[test]
    fn a_stop_mark_never_outlives_the_stop_that_wrote_it() {
        let (repo, _root_guard) = fixture("stop-mark-spent");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        park_under_lock(&repo, "login", ParkedBy::Stop).unwrap();
        assert!(repo.task("login").unwrap().front.parked_by_stop);

        resume_task(&repo, &pipelines, "login").unwrap();
        assert!(!repo.task("login").unwrap().front.parked_by_stop);

        // A mark somehow left on a task still at work is cleared by the
        // next park, whoever makes it.
        let mut task = repo.task("login").unwrap();
        task.front.parked_by_stop = true;
        task.save().unwrap();
        park_under_lock(&repo, "login", ParkedBy::Board).unwrap();
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert!(!task.front.parked_by_stop);
    }

    /// A stop-parked row's NEXT says the start will resume it, in place of
    /// the `[r]` offer a person's own park reads.
    #[test]
    fn a_stop_parked_row_says_it_resumes_when_dispatching_starts() {
        let (repo, _root_guard) = fixture("stop-parked-next");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "login", &[], Some("implement"), Some("login"));
        add_to(&repo, "by-hand", &[], Some("implement"), Some("by-hand"));
        park_under_lock(&repo, "login", ParkedBy::Stop).unwrap();
        park_under_lock(&repo, "by-hand", ParkedBy::Board).unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap();

        assert_eq!(
            row("login").next,
            "→ implement — resumes when dispatching starts"
        );
        assert!(row("login").resumable, "`r` still works on it");
        assert!(
            row("by-hand").next.starts_with("[r] → implement"),
            "{}",
            row("by-hand").next
        );
    }

    /// `P` is no longer a key: stopping the run is the dispatch tab's own
    /// `enter`. Pressed on the board it opens nothing and parks nothing.
    #[test]
    fn shift_p_is_no_longer_a_board_key() {
        let (repo, _root_guard) = fixture("shift-p-gone");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();

        assert!(matches!(board.mode, BoardMode::Browsing));
        assert_eq!(repo.task("login").unwrap().stage(), "implement");
    }

    /// `p` on the cursor's own live agent turn — the mockup's own panel —
    /// names the step, the word `agent`, an elapsed time, and the two lines
    /// that say the turn is interrupted rather than killed. Nothing is
    /// touched before `enter` answers it: the task file it would write is
    /// still exactly what it was when the panel opened.
    #[test]
    fn pressing_p_on_a_live_agent_lane_names_the_turn_and_says_it_is_only_interrupted() {
        let (mut repo, _root_guard) = fixture("pause-cursor-agent-lane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let mut task = repo.task("login").unwrap();
        task.front.launched_at = Some(chrono::Utc::now().timestamp() - 260);
        task.save().unwrap();
        let before = std::fs::read_to_string(&task.path).unwrap();

        let (_mux, _name) = live_headless_lane(&repo);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("login"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();

        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(frame.contains("pause login"), "{frame}");
        assert!(frame.contains("implement"), "{frame}");
        assert!(frame.contains("agent"), "{frame}");
        // ~4m20s, off `launched_at` set 260 seconds ago.
        assert!(frame.contains("4m"), "{frame}");
        assert!(
            frame.contains("The turn is interrupted, not killed."),
            "{frame}"
        );
        assert!(
            frame.contains("Resuming picks the session back up."),
            "{frame}"
        );

        // Nothing written until the panel is answered.
        let after = std::fs::read_to_string(&repo.task("login").unwrap().path).unwrap();
        assert_eq!(before, after);
        assert_eq!(repo.task("login").unwrap().stage(), "implement");
    }

    /// `board-reader-thread`'s own acceptance test: `p` must open the pause
    /// panel for a lane that started after the board's last reading, not
    /// park the task silently as if nothing were live on it —
    /// `begin_pause_cursor` reads the lane list itself, fresh, every time,
    /// rather than trusting whatever `status::Reader` already had on hand.
    #[test]
    fn p_opens_the_pause_panel_for_a_lane_that_started_after_the_last_reading() {
        let (mut repo, _root_guard) = fixture("pause-panel-after-stale-reading");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let mut board = Board::for_test();
        // Drawn through `hosted_frame`, not `Board::frame`: this is the one
        // path with a `status::Reader` of its own to go stale, and the
        // snapshot it reads here carries nothing live — no lane exists yet.
        board.hosted_frame(&repo, &pipelines, false, None).unwrap();

        // Only now does a lane start for it — after the reading above, with
        // nothing telling the board to read again before the key below.
        let (_mux, _name) = live_headless_lane(&repo);

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        assert!(
            matches!(board.mode, BoardMode::ConfirmPause { .. }),
            "`p` against a stale reading must still open the pause panel \
             for a lane that has since gone live, not park it silently"
        );
    }

    /// `s` on `p`'s own panel leaves the live turn running and writes
    /// `gate_at` naming the step instead of interrupting anything: the board
    /// answers back to `Browsing`, the task's stage never moves off
    /// `implement`, and the lane the panel named is still there afterwards.
    #[test]
    fn pressing_s_on_the_pause_panel_schedules_a_gate_and_leaves_the_lane_running() {
        let (mut repo, _root_guard) = fixture("schedule-pause-cursor");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let (mux, name) = live_headless_lane(&repo);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('s'))
            .unwrap();

        assert!(matches!(board.mode, BoardMode::Browsing));
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.gate_at.as_deref(), Some("implement"));
        assert_eq!(task.front.parked_from, None);
        assert!(mux.list_lanes().unwrap().iter().any(|l| l.name == name));
    }

    /// A board over one task, drawn once so the cursor has a row to sit on,
    /// with `keys` pressed in order — the shape every restart test below
    /// starts from.
    fn board_after(repo: &Repo, pipelines: &Pipelines, keys: &[crate::screen::Key]) -> Board {
        let mut board = Board::for_test();
        board
            .frame(
                repo,
                pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        for key in keys {
            board.on_key(repo, pipelines, *key).unwrap();
        }
        board
    }

    fn watching_frame(board: &mut Board, repo: &Repo, pipelines: &Pipelines) -> String {
        strip(
            &board
                .frame(
                    repo,
                    pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        )
    }

    /// `s` on the row list opens the restart panel, not the pause panel's
    /// schedule: nothing is written, `gate_at` stays unset, the lane keeps
    /// running, and the panel names the step and the lane it would end.
    #[test]
    fn pressing_s_on_the_row_list_opens_the_restart_panel() {
        use crate::screen::Key;
        let (mut repo, _root_guard) = fixture("restart-panel-opens");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let (mux, name) = live_headless_lane(&repo);
        let before = std::fs::read_to_string(&repo.task("login").unwrap().path).unwrap();

        let mut board = board_after(&repo, &pipelines, &[Key::Down, Key::Char('s')]);

        assert!(matches!(board.mode, BoardMode::ConfirmRestart(_)));
        let frame = watching_frame(&mut board, &repo, &pipelines);
        assert!(frame.contains("┌─ restart login ─"), "{frame}");
        assert!(frame.contains("step      implement"), "{frame}");
        assert!(frame.contains("lane      login · implement"), "{frame}");
        assert!(frame.contains("[s] restart   [esc] cancel"), "{frame}");
        let after = std::fs::read_to_string(&repo.task("login").unwrap().path).unwrap();
        assert_eq!(before, after);
        assert_eq!(repo.task("login").unwrap().front.gate_at, None);
        assert!(mux.list_lanes().unwrap().iter().any(|l| l.name == name));
    }

    /// `s` opens nothing on a row `spoolway restart` would refuse: a task
    /// still on `queued`, and one parked before it ever started a step.
    #[test]
    fn pressing_s_on_a_task_that_never_started_opens_nothing() {
        use crate::screen::Key;
        let (repo, _root_guard) = fixture("restart-panel-never-started");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], None);

        let board = board_after(&repo, &pipelines, &[Key::Down, Key::Char('s')]);
        assert!(matches!(board.mode, BoardMode::Browsing), "queued");

        let mut task = repo.task("login").unwrap();
        task.set_stage(crate::pipeline::PAUSED, Some("parked by hand"));
        task.save().unwrap();
        let board = board_after(&repo, &pipelines, &[Key::Down, Key::Char('s')]);
        assert!(
            matches!(board.mode, BoardMode::Browsing),
            "parked off queued"
        );
    }

    /// `esc` closes the restart panel and changes nothing; `enter`, which
    /// confirms every other panel, is ignored by this one.
    #[test]
    fn esc_on_the_restart_panel_changes_nothing() {
        use crate::screen::Key;
        let (mut repo, _root_guard) = fixture("restart-panel-esc");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let (mux, name) = live_headless_lane(&repo);
        let before = std::fs::read_to_string(&repo.task("login").unwrap().path).unwrap();

        let board = board_after(&repo, &pipelines, &[Key::Down, Key::Char('s'), Key::Enter]);
        assert!(matches!(board.mode, BoardMode::ConfirmRestart(_)), "enter");
        let board = board_after(&repo, &pipelines, &[Key::Down, Key::Char('s'), Key::Esc]);
        assert!(matches!(board.mode, BoardMode::Browsing));

        let after = std::fs::read_to_string(&repo.task("login").unwrap().path).unwrap();
        assert_eq!(before, after);
        assert!(mux.list_lanes().unwrap().iter().any(|l| l.name == name));
    }

    /// `s` on the restart panel carries the restart out: the task stays on
    /// its step with `restart:` naming it, and the lane is gone.
    #[test]
    fn pressing_s_twice_restarts_the_step() {
        use crate::screen::Key;
        let (mut repo, _root_guard) = fixture("restart-panel-confirm");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let (mux, name) = live_headless_lane(&repo);

        let board = board_after(
            &repo,
            &pipelines,
            &[Key::Down, Key::Char('s'), Key::Char('s')],
        );

        assert!(matches!(board.mode, BoardMode::Browsing));
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), "implement");
        assert_eq!(task.front.restart.as_deref(), Some("implement"));
        assert!(!mux.list_lanes().unwrap().iter().any(|l| l.name == name));
    }

    /// A task that moved to another step while the panel was open is not
    /// restarted on the step it is on now, which the panel never named: `s`
    /// refuses, the panel stays open saying where it went, and the task and
    /// the lane are left as they were.
    #[test]
    fn a_task_moved_to_another_step_is_not_restarted_from_a_stale_panel() {
        use crate::screen::Key;
        let (mut repo, _root_guard) = fixture("restart-panel-moved");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let (mux, name) = live_headless_lane(&repo);

        let mut board = board_after(&repo, &pipelines, &[Key::Down, Key::Char('s')]);
        let mut task = repo.task("login").unwrap();
        task.set_stage("review", Some("passed while the panel was open"));
        task.save().unwrap();
        board.on_key(&repo, &pipelines, Key::Char('s')).unwrap();

        let BoardMode::ConfirmRestart(confirm) = &board.mode else {
            panic!("the panel closed on a task that moved");
        };
        assert!(
            confirm
                .error
                .as_deref()
                .is_some_and(|e| e.contains("moved to `review` since this panel opened")),
            "{:?}",
            confirm.error
        );
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), "review");
        assert_eq!(task.front.restart, None);
        assert!(mux.list_lanes().unwrap().iter().any(|l| l.name == name));
    }

    /// A refused restart keeps the panel open with the refusal printed in
    /// it, rather than closing as if the restart had happened: here the
    /// task went back to `queued` while the panel was open.
    #[test]
    fn a_refused_restart_keeps_the_panel_open_with_the_error() {
        use crate::screen::Key;
        let (mut repo, _root_guard) = fixture("restart-panel-refused");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let (mux, name) = live_headless_lane(&repo);

        let mut board = board_after(&repo, &pipelines, &[Key::Down, Key::Char('s')]);
        let mut task = repo.task("login").unwrap();
        task.set_stage(crate::pipeline::QUEUED, Some("sent back by hand"));
        task.save().unwrap();
        board.on_key(&repo, &pipelines, Key::Char('s')).unwrap();

        let BoardMode::ConfirmRestart(confirm) = &board.mode else {
            panic!("the panel closed on a refused restart");
        };
        assert!(
            confirm
                .error
                .as_deref()
                .is_some_and(|e| e.contains("no step has started")),
            "{:?}",
            confirm.error
        );
        let frame = watching_frame(&mut board, &repo, &pipelines);
        assert!(frame.contains("no step has started"), "{frame}");
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::QUEUED);
        assert_eq!(task.front.restart, None);
        assert!(mux.list_lanes().unwrap().iter().any(|l| l.name == name));
    }

    /// Pressing `s` a second time, on a fresh panel over the same still-live
    /// step, clears the schedule it wrote rather than writing it again —
    /// the mockup's "pressing `s` again... clears it".
    #[test]
    fn pressing_s_again_clears_a_scheduled_pause() {
        let (mut repo, _root_guard) = fixture("schedule-pause-toggle");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let (_mux, _name) = live_headless_lane(&repo);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('s'))
            .unwrap();
        assert_eq!(
            repo.task("login").unwrap().front.gate_at.as_deref(),
            Some("implement")
        );

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('s'))
            .unwrap();
        assert_eq!(repo.task("login").unwrap().front.gate_at, None);
    }

    /// The NEXT column names a scheduled pause rather than the pipeline's
    /// own route once `gate_at` matches the step a running task sits on.
    #[test]
    fn the_next_column_names_a_scheduled_pause() {
        let (repo, _root_guard) = fixture("schedule-pause-next-column");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let mut task = repo.task("login").unwrap();
        task.front.gate_at = Some("implement".into());
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert_eq!(row.next, "→ paused after implement");
    }

    /// A command step's held failure resumes down its `on_fail`, so the NEXT
    /// column names that step, not `on_pass`; an agent step's still names
    /// `on_pass`.
    #[test]
    fn the_next_column_names_on_fail_for_a_held_command_failure() {
        let (repo, _root_guard) = fixture("held-command-failure-next-column");
        let mut pipelines = Pipelines::builtin();
        let pipeline = pipelines.pipelines.get_mut("default").unwrap();
        let step = pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap();
        step.run = Some("exit 2".into());
        step.agent = None;
        step.prompt = None;
        step.session = false;
        step.on_fail = Some("rebase".into());
        let on_pass = step.on_pass.clone().unwrap();
        add(&repo, "login", &[], Some("implement"));
        let mut task = repo.task("login").unwrap();
        task.front.paused_at = Some("implement".into());
        task.front.paused_by = Some("schedule".into());
        task.front.last_report = Some(crate::task::LastReport {
            step: "implement".into(),
            outcome: "fail".into(),
            at: 1,
            blocked: false,
        });
        let pipeline = pipelines.pipelines.get("default").unwrap();

        assert_eq!(paused_next(&task, pipeline, 0).as_deref(), Some("rebase"));

        let mut agent_pipelines = Pipelines::builtin();
        let agent = agent_pipelines.pipelines.get_mut("default").unwrap();
        agent
            .steps
            .iter_mut()
            .find(|s| s.id == "implement")
            .unwrap()
            .on_fail = Some("rebase".into());
        let agent = agent_pipelines.pipelines.get("default").unwrap();
        assert_eq!(
            paused_next(&task, agent, 0).as_deref(),
            Some(on_pass.as_str())
        );
    }

    /// `o` on a headless run has no pane to open an editor in — headless
    /// refuses it the way `Mux::open_command`'s default does — so the key is a no-op:
    /// best-effort, like every other key here, and the task file it would
    /// have opened is left exactly as it was.
    #[test]
    fn pressing_o_with_no_multiplexer_leaves_the_task_file_unchanged() {
        let (mut repo, _root_guard) = fixture("open-key-headless");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let before = std::fs::read_to_string(repo.task("login").unwrap().path).unwrap();

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("login"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('o'))
            .unwrap();

        let after = std::fs::read_to_string(repo.task("login").unwrap().path).unwrap();
        assert_eq!(before, after);
    }

    /// `p` then `r` `enter` parks a task and puts it straight back — the
    /// round trip the board leaves nothing behind for: `steps`, `rounds`,
    /// `arrivals` and `arrived_from` come back byte-for-byte,
    /// `rounds_via("implement", "paused")` stays zero, and the row's own NEXT
    /// column carries no loop counter across it.
    #[test]
    fn pressing_p_then_r_round_trips_a_task_without_banking_a_lap() {
        let (mut repo, _root_guard) = fixture("park-round-trip");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let before = repo.task("login").unwrap();
        let rounds_before = before.front.rounds.clone();
        let arrivals_before = before.front.arrivals.clone();
        let prompts_before = before.front.steps.clone();
        let arrived_from_before = before.front.arrived_from.clone();

        let (_mux, _name) = live_headless_lane(&repo);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        // A live agent lane opens a confirm panel, as a command run does.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();
        assert_eq!(repo.task("login").unwrap().stage(), crate::pipeline::PAUSED);

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        let after = repo.task("login").unwrap();
        assert_eq!(after.stage(), "implement");
        assert_eq!(after.front.rounds, rounds_before);
        assert_eq!(after.front.arrivals, arrivals_before);
        assert_eq!(after.front.steps, prompts_before);
        assert_eq!(after.front.arrived_from, arrived_from_before);
        assert_eq!(after.rounds_via("implement", crate::pipeline::PAUSED), 0);
        assert_eq!(after.rounds_via(crate::pipeline::PAUSED, "implement"), 0);

        let log = after.section("## Status Log").unwrap();
        assert!(log.contains("paused from the board"), "{log}");
        assert!(log.contains("put back from the board"), "{log}");
        assert!(!log.to_lowercase().contains("unblocked"), "{log}");

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(!row.next.contains("loop"), "{}", row.next);
    }

    /// `p` on a row with nothing live parks it straight to `paused` with no
    /// panel at all — from `queued`, and from a real step sitting in the gap
    /// between two lanes. `queued` is the one case that leaves no
    /// `parked_from` at all: it is not a step any pipeline declares, so a
    /// breadcrumb naming it would never be spent.
    ///
    /// A third state used to be walked here: a row parked on a quota clock.
    /// The quota gate is gone, and with it `parked_until` and
    /// `parked_window`, so there is no such row left to park.
    #[test]
    fn pressing_p_with_nothing_live_parks_at_once_from_every_such_state() {
        let (repo, _root_guard) = fixture("pause-nothing-live");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "queued-task", &[], None, Some("queued-task"));
        add_to(&repo, "gap-task", &[], Some("implement"), Some("gap-task"));

        let mut board = Board::for_test();
        let cases = [("queued-task", None), ("gap-task", Some("implement"))];
        for (id, from) in cases {
            board.cursor = Some(id.to_string());
            board
                .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
                .unwrap();
            // No panel opened — the mode never left `Browsing`.
            assert!(matches!(board.mode, BoardMode::Browsing), "pausing {id}");
            let task = repo.task(id).unwrap();
            assert_eq!(task.stage(), crate::pipeline::PAUSED, "pausing {id}");
            assert_eq!(task.front.parked_from.as_deref(), from, "pausing {id}");
        }
    }

    /// `p` is a no-op on a `paused` row — already stopped and already
    /// waiting on a person, not a state to park over again.
    #[test]
    fn pressing_p_on_a_paused_row_does_nothing() {
        let (repo, _root_guard) = fixture("pause-noop-states");
        let pipelines = Pipelines::builtin();
        add(&repo, "already-paused", &[], None);
        let mut paused = repo.task("already-paused").unwrap();
        paused.front.paused_at = Some("implement".into());
        paused.set_stage(crate::pipeline::PAUSED, None);
        paused.save().unwrap();

        let mut board = Board::for_test();
        let before = std::fs::read_to_string(repo.task("already-paused").unwrap().path).unwrap();
        board.cursor = Some("already-paused".to_string());
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        assert!(matches!(board.mode, BoardMode::Browsing));
        let after = std::fs::read_to_string(repo.task("already-paused").unwrap().path).unwrap();
        assert_eq!(before, after);
    }

    /// `p` on a `blocked` row with nothing live parks it at once, no panel —
    /// the same road `queued` and a real step in the gap between two lanes
    /// already take: a `blocked` row is not a state `p` skips any more, only
    /// `paused` is. `blocked_from` survives the park untouched, sitting
    /// beside the fresh `parked_from: blocked` it now carries too.
    #[test]
    fn pressing_p_on_a_blocked_row_with_nothing_live_parks_it_at_once() {
        let (repo, _root_guard) = fixture("pause-blocked-idle");
        let pipelines = Pipelines::builtin();
        add(&repo, "stuck", &[], Some(crate::pipeline::BLOCKED));
        let mut task = repo.task("stuck").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.save().unwrap();

        let mut board = Board::for_test();
        board.cursor = Some("stuck".to_string());
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();

        assert!(matches!(board.mode, BoardMode::Browsing));
        let task = repo.task("stuck").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(
            task.front.parked_from.as_deref(),
            Some(crate::pipeline::BLOCKED)
        );
        assert_eq!(task.front.blocked_from.as_deref(), Some("implement"));
    }

    /// `p` on a `blocked` row whose unblocker is mid-turn opens the same
    /// single-abort panel a live `implement` turn would, and `enter`
    /// interrupts that lane exactly as it would any other — the reach this
    /// task adds. `blocked_from` is left standing beside the `parked_from`
    /// the park writes, and `resume` afterwards carries the session back
    /// onto `blocked` rather than opening a fresh one.
    #[test]
    fn pressing_p_on_a_blocked_row_with_a_live_unblocker_interrupts_it_and_resumes_onto_blocked() {
        let (mut repo, _root_guard) = fixture("pause-blocked-live-lane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "stuck", &[], Some(crate::pipeline::BLOCKED));
        let mut task = repo.task("stuck").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.save().unwrap();

        let (mux, name) =
            live_headless_lane_at(&repo, "stuck", crate::pipeline::BLOCKED, "unblocker");

        let mut board = Board::for_test();
        board.cursor = Some("stuck".to_string());
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(frame.contains("pause stuck"), "{frame}");
        assert_eq!(
            repo.task("stuck").unwrap().stage(),
            crate::pipeline::BLOCKED
        );

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        assert!(
            mux.list_lanes().unwrap().iter().all(|l| l.name != name),
            "headless has no keyboard, so an interrupt ends the turn"
        );
        let task = repo.task("stuck").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(
            task.front.parked_from.as_deref(),
            Some(crate::pipeline::BLOCKED)
        );
        assert_eq!(task.front.blocked_from.as_deref(), Some("implement"));

        crate::status::resume_task(&repo, &pipelines, "stuck").unwrap();
        let task = repo.task("stuck").unwrap();
        assert_eq!(task.stage(), crate::pipeline::BLOCKED);
        assert_eq!(task.front.resume.as_deref(), Some(crate::pipeline::BLOCKED));
        assert_eq!(task.front.blocked_from.as_deref(), Some("implement"));
    }

    /// `u` on a queued row with nothing depending on it opens a panel naming
    /// the task and the pending path it would land at, and answering `u`
    /// moves the task there with every reserved key gone from its
    /// frontmatter — accepted unchanged by a fresh `queue add --from`.
    #[test]
    fn pressing_u_on_an_unstarted_task_moves_it_back_to_pending() {
        let (repo, _root_guard) = fixture("unqueue-cursor");
        let pipelines = Pipelines::builtin();
        add(&repo, "chain-refusals", &[], None);
        assert_eq!(repo.task("chain-refusals").unwrap().stage(), "queued");

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("chain-refusals"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();
        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(frame.contains("unqueue chain-refusals"), "{frame}");
        assert!(frame.contains("chain-refusals.md"), "{frame}");
        assert!(frame.contains("[enter] unqueue it"), "{frame}");
        // Still sitting in the queue — nothing moves until the panel is
        // answered.
        assert!(repo.task("chain-refusals").is_ok());

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        assert!(!repo.queue_dir().join("chain-refusals.md").exists());
        let pending_doc = repo.pending_dir().join("chain-refusals.md");
        assert!(pending_doc.exists());
        let raw = std::fs::read_to_string(&pending_doc).unwrap();
        for reserved in crate::commands::RESERVED_KEYS {
            assert!(
                !raw.contains(&format!("{reserved}:")),
                "{reserved}: leaked into {raw}"
            );
        }

        // `spoolway task contract --from` — the acceptance criterion's own
        // words — runs the same validation `queue add --from` does and
        // writes nothing; it has to accept the task exactly as it is.
        let contract_args = crate::cli::TaskContractArgs {
            from: vec![pending_doc.display().to_string()],
            base: None,
        };
        crate::commands::task_contract(&repo, &pipelines, &contract_args, &repo.root).unwrap();

        // And the task a fresh `queue add --from` accepts unchanged,
        // exactly as it did the first time — so it can be queued again
        // without editing it by hand.
        let queue_args = crate::cli::QueueAddArgs {
            from: vec![pending_doc.display().to_string()],
            base: Some("group/demo".to_string()),
            dry_run: false,
        };
        crate::commands::queue_add(&repo, &pipelines, &queue_args, &repo.root, false).unwrap();
        assert_eq!(repo.task("chain-refusals").unwrap().stage(), "queued");
    }

    /// `esc` on `u`'s panel leaves the queue exactly as it was.
    #[test]
    fn pressing_esc_on_the_unqueue_panel_moves_nothing() {
        let (repo, _root_guard) = fixture("unqueue-esc");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[], None);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Esc)
            .unwrap();

        assert_eq!(repo.task("solo").unwrap().stage(), "queued");
        assert!(!repo.pending_dir().join("solo.md").exists());
    }

    /// `u` does nothing on a row that has already started, however idle it
    /// looks between passes — unqueuing it would mean tearing down a
    /// checkout, which is outside what this key reaches.
    #[test]
    fn pressing_u_on_a_started_task_is_a_no_op() {
        let (repo, _root_guard) = fixture("unqueue-started");
        let pipelines = Pipelines::builtin();
        add(&repo, "under-way", &[], Some("implement"));

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();

        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(!frame.contains("unqueue under-way"), "{frame}");
        assert_eq!(repo.task("under-way").unwrap().stage(), "implement");
    }

    /// `u` on a task a still-queued task depends on opens a panel naming
    /// both, the dependent marked with what it depends on, and `enter`
    /// carries both back to pending, leaving neither in the queue.
    #[test]
    fn pressing_u_on_a_task_a_queued_dependent_names_carries_both() {
        let (repo, _root_guard) = fixture("unqueue-depended-on");
        let pipelines = Pipelines::builtin();
        add(&repo, "drop-walk", &[], None);
        add(&repo, "chain-refusals", &["drop-walk"], None);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        // `drop-walk` is the dependency, so it sorts first, and the frame
        // above already landed the cursor on it.
        assert_eq!(board.cursor.as_deref(), Some("drop-walk"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();

        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(frame.contains("unqueue drop-walk"), "{frame}");
        assert!(frame.contains("2 tasks go back to:"), "{frame}");
        assert!(frame.contains("drop-walk"), "{frame}");
        assert!(
            frame.contains("chain-refusals   (depends on drop-walk)"),
            "{frame}"
        );
        assert!(frame.contains("[enter] unqueue them"), "{frame}");
        // Still sitting in the queue — nothing moves until the panel is
        // answered.
        assert_eq!(repo.task("drop-walk").unwrap().stage(), "queued");
        assert_eq!(repo.task("chain-refusals").unwrap().stage(), "queued");

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        assert!(repo.pending_dir().join("drop-walk.md").exists());
        assert!(repo.pending_dir().join("chain-refusals.md").exists());
        assert!(!repo.queue_dir().join("drop-walk.md").exists());
        assert!(!repo.queue_dir().join("chain-refusals.md").exists());
        // Both rows left the table together, so the cursor clears rather
        // than pointing at either one.
        assert_eq!(board.cursor, None);
    }

    /// `u` walks a chain of more than one hop, carrying every unstarted task
    /// that reaches the cursor's own task through `depends_on` — not just
    /// its immediate dependent.
    #[test]
    fn pressing_u_carries_a_chain_two_hops_deep() {
        let (repo, _root_guard) = fixture("unqueue-chain-two-hops");
        let pipelines = Pipelines::builtin();
        add(&repo, "alpha", &[], None);
        add(&repo, "beta", &["alpha"], None);
        add(&repo, "gamma", &["beta"], None);
        add_to(&repo, "unrelated", &[], None, Some("unrelated"));

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("alpha"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();

        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(frame.contains("unqueue alpha"), "{frame}");
        assert!(frame.contains("3 tasks go back to:"), "{frame}");
        assert!(frame.contains("beta   (depends on alpha)"), "{frame}");
        assert!(frame.contains("gamma   (depends on beta)"), "{frame}");

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        assert!(repo.pending_dir().join("alpha.md").exists());
        assert!(repo.pending_dir().join("beta.md").exists());
        assert!(repo.pending_dir().join("gamma.md").exists());
        // `unrelated` names none of them in its own `depends_on`, so the
        // chain never reaches it — it stays queued and is where the cursor
        // lands once the three that did leave are gone.
        assert_eq!(repo.task("unrelated").unwrap().stage(), "queued");
        assert_eq!(board.cursor.as_deref(), Some("unrelated"));
    }

    /// `u` moves the cursor to the row underneath the one it just removed —
    /// the same row `↓` would have reached — rather than leaving it naming a
    /// task the board no longer shows.
    #[test]
    fn pressing_u_moves_the_cursor_to_the_row_underneath() {
        let (repo, _root_guard) = fixture("unqueue-cursor-advances");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "chain-refusals", &[], None, Some("chain-refusals"));
        add_to(&repo, "month-instant", &[], None, Some("month-instant"));

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("chain-refusals"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        assert_eq!(board.cursor.as_deref(), Some("month-instant"));
        assert!(!repo.queue_dir().join("chain-refusals.md").exists());
    }

    /// `u` on the only row left clears the cursor rather than pointing it at
    /// the very task it just removed — `shift_cursor` wrapping to a single
    /// row's own position is the one case [`Board::on_key_unqueue_confirm`]
    /// has to catch itself.
    #[test]
    fn pressing_u_on_the_last_row_clears_the_cursor() {
        let (repo, _root_guard) = fixture("unqueue-cursor-clears");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[], None);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board
            .frame(
                &repo,
                &pipelines,
                Phase::Watching {
                    holder: None,
                    dispatching: false,
                },
            )
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("solo"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        assert_eq!(board.cursor, None);
        assert!(!repo.queue_dir().join("solo.md").exists());
    }

    /// `U` opens a panel naming every task that has not started, and answering
    /// it carries each of them back to pending — a running task is never in
    /// that set, and never moves.
    #[test]
    fn pressing_shift_u_moves_every_unstarted_task_to_pending() {
        let (repo, _root_guard) = fixture("unqueue-all");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "chain-refusals", &[], None, Some("chain-refusals"));
        add_to(&repo, "month-instant", &[], None, Some("month-instant"));
        add(&repo, "already-running", &[], Some("implement"));

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('U'))
            .unwrap();
        let frame = strip(
            &board
                .frame(
                    &repo,
                    &pipelines,
                    Phase::Watching {
                        holder: None,
                        dispatching: false,
                    },
                )
                .unwrap(),
        );
        assert!(frame.contains("unqueue all"), "{frame}");
        assert!(frame.contains("2 tasks have not started"), "{frame}");
        // Both unstarted ids appear, one per line — the running task is not
        // among them, which the board's own table beneath the panel still
        // lists by name regardless, so checking for its absence from the
        // whole frame would prove nothing.
        assert!(frame.contains("chain-refusals"), "{frame}");
        assert!(frame.contains("month-instant"), "{frame}");
        assert!(frame.contains("[enter] unqueue them"), "{frame}");

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();

        assert!(repo.pending_dir().join("chain-refusals.md").exists());
        assert!(repo.pending_dir().join("month-instant.md").exists());
        assert!(!repo.queue_dir().join("chain-refusals.md").exists());
        assert!(!repo.queue_dir().join("month-instant.md").exists());
        // The running task was never in the set, and stays exactly where it
        // was.
        assert_eq!(repo.task("already-running").unwrap().stage(), "implement");
    }

    /// Unqueuing a row whose pending draft has been rewritten since it was
    /// queued leaves both files where they are, rather than dropping the
    /// stale queued copy on top of the newer draft. See review finding 49.
    #[test]
    fn unqueue_does_not_overwrite_a_newer_pending_draft() {
        let (repo, _root_guard) = fixture("unqueue-keeps-newer-draft");
        add(&repo, "solo", &[], None);

        let draft = repo.pending_dir().join("solo.md");
        std::fs::create_dir_all(draft.parent().unwrap()).unwrap();
        std::fs::write(
            &draft,
            "---\nid: solo\nstage: queued\ngroup: demo\n---\n## Goal\n\nthe newer draft\n",
        )
        .unwrap();

        unqueue_task(&repo, "solo").unwrap();

        assert!(
            repo.queue_dir().join("solo.md").exists(),
            "the queued copy is left in place"
        );
        assert!(
            std::fs::read_to_string(&draft)
                .unwrap()
                .contains("the newer draft"),
            "the pending draft is untouched"
        );
    }

    /// The `starts_from:` spoolway stamped at a cut does not travel back to
    /// pending, where it would outrank what the person edits there; the one a
    /// person set before any cut does.
    #[test]
    fn unqueue_drops_a_stamped_starts_from_but_keeps_one_set_before_the_cut() {
        let (repo, _root_guard) = fixture("unqueue-starts-from");
        add_to(&repo, "cut", &[], None, Some("cut"));
        let mut cut = repo.task("cut").unwrap();
        cut.front.starts_from = Some("task/old-run".into());
        cut.front.base_commit = Some("abc123".into());
        cut.save().unwrap();
        add_to(&repo, "uncut", &[], None, Some("uncut"));
        let mut uncut = repo.task("uncut").unwrap();
        uncut.front.starts_from = Some("main".into());
        uncut.save().unwrap();

        unqueue_task(&repo, "cut").unwrap();
        unqueue_task(&repo, "uncut").unwrap();

        let read = |id: &str| std::fs::read_to_string(repo.pending_dir().join(format!("{id}.md")));
        assert!(!read("cut").unwrap().contains("starts_from"));
        assert!(read("uncut").unwrap().contains("starts_from: main"));
    }

    /// The letter that opens the resume picker, the unqueue and the
    /// unqueue-all panels no longer answers them, the same as any other
    /// unrecognised key — `enter` is the only key that does now, proven on
    /// each panel by the tests above this one.
    #[test]
    fn the_old_confirming_letter_no_longer_answers_any_panel() {
        let (repo, _root_guard) = fixture("panels-ignore-their-own-letter");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "gate-board", &[], None, Some("gate-board"));
        let mut gated = repo.task("gate-board").unwrap();
        gated.front.paused_at = Some("implement".into());
        gated.set_stage(crate::pipeline::PAUSED, None);
        gated.save().unwrap();
        add_to(&repo, "solo", &[], None, Some("solo"));

        let mut board = Board::for_test();

        board.cursor = Some("gate-board".to_string());
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();
        assert!(!matches!(board.mode, BoardMode::Browsing), "resume picker");
        assert_eq!(
            repo.task("gate-board").unwrap().stage(),
            crate::pipeline::PAUSED
        );

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Esc)
            .unwrap();
        board.cursor = Some("solo".to_string());
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();
        assert!(!matches!(board.mode, BoardMode::Browsing), "unqueue");
        assert!(repo.task("solo").is_ok());

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Esc)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('U'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('U'))
            .unwrap();
        assert!(!matches!(board.mode, BoardMode::Browsing), "unqueue-all");
        assert!(repo.task("solo").is_ok());
    }

    /// Press each of `keys` on `board`, in order.
    fn press(board: &mut Board, repo: &Repo, pipelines: &Pipelines, keys: &[crate::screen::Key]) {
        for key in keys {
            board.on_key(repo, pipelines, *key).unwrap();
        }
    }

    /// The picker `board` has open, or a panic naming what it has instead.
    fn open_picker(board: &Board) -> &ResumePicker {
        match &board.mode {
            BoardMode::ResumePicker(picker) => picker,
            _ => panic!("no resume picker is open"),
        }
    }

    /// Put `id` on `paused`, held by a schedule that caught `outcome` at
    /// `step` — what `commands::report` leaves when a gate catches a report.
    fn caught_at_step(repo: &Repo, id: &str, step: &str, outcome: &str) {
        let mut task = repo.task(id).unwrap();
        task.front.last_report = Some(crate::task::LastReport {
            step: step.into(),
            outcome: outcome.into(),
            at: 0,
            blocked: outcome == "block",
        });
        task.front.paused_at = Some(step.into());
        task.front.paused_by = Some("schedule".into());
        task.front.blocked_from = (outcome == "block").then(|| step.into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();
    }

    /// `r` on a gate opens the picker as drawn: the header names the stop,
    /// every step of the pipeline is listed in order with the stopped step,
    /// its pass and fail targets labelled, and the cursor on `(next)`.
    #[test]
    fn r_opens_a_picker_listing_every_step_with_the_next_one_preselected() {
        let (repo, _root_guard) = fixture("picker-opens");
        let pipelines = Pipelines::builtin();
        add(&repo, "ship-login", &[], None);
        caught_at_step(&repo, "ship-login", "review", "pass");

        let mut board = Board::for_test();
        board.cursor = Some("ship-login".to_string());
        press(
            &mut board,
            &repo,
            &pipelines,
            &[crate::screen::Key::Char('r')],
        );

        let panel = resume_picker_panel(open_picker(&board), None);
        assert_eq!(
            panel,
            vec![
                "┌─ resume ship-login ─────────────────────────┐",
                "│                                             │",
                "│  paused at review — it passed               │",
                "│                                             │",
                "│    implement   on fail                      │",
                "│    review      paused                       │",
                "│  ▸ document    on pass (next)               │",
                "│    handover                                 │",
                "│                                             │",
                "│  [↑↓] pick   [enter] resume   [esc] cancel  │",
                "│                                             │",
                "└─────────────────────────────────────────────┘",
            ],
        );
        assert_eq!(
            repo.task("ship-login").unwrap().stage(),
            crate::pipeline::PAUSED,
            "opening the picker moves nothing"
        );

        // `esc` closes it and still moves nothing.
        press(&mut board, &repo, &pipelines, &[crate::screen::Key::Esc]);
        assert!(matches!(board.mode, BoardMode::Browsing));
        assert_eq!(
            repo.task("ship-login").unwrap().stage(),
            crate::pipeline::PAUSED
        );
    }

    /// `r` `enter` lands every kind of held row on its own `(next)` step —
    /// the one `resume_road` names: a gate on its `on_pass`, a `p` park back
    /// on the step it left, a block on the step it blocked at, and a caught
    /// fail at an agent step on its `on_pass`, never its `on_fail`.
    #[test]
    fn r_enter_lands_each_kind_of_hold_on_its_next_step() {
        let (repo, _root_guard) = fixture("picker-next-roads");
        let pipelines = Pipelines::builtin();

        add_to(&repo, "gate", &[], None, Some("gate"));
        caught_at_step(&repo, "gate", "review", "pass");

        add_to(&repo, "park", &[], Some("review"), Some("park"));
        let mut task = repo.task("park").unwrap();
        park(&mut task, "paused from the board", false);
        task.save().unwrap();

        add_to(&repo, "wall", &[], None, Some("wall"));
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("review".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        add_to(&repo, "caught-fail", &[], None, Some("caught-fail"));
        caught_at_step(&repo, "caught-fail", "review", "fail");

        for (id, lands) in [
            ("gate", "document"),
            ("park", "review"),
            ("wall", "review"),
            ("caught-fail", "document"),
        ] {
            let road = crate::commands::resume_road(&repo.task(id).unwrap(), &pipelines).unwrap();
            assert_eq!(road.destination(), lands, "{id}");

            let mut board = Board::for_test();
            board.cursor = Some(id.to_string());
            press(
                &mut board,
                &repo,
                &pipelines,
                &[crate::screen::Key::Char('r')],
            );
            let picker = open_picker(&board);
            let row = &picker.rows[picker.cursor];
            assert!(row.next, "{id}: the cursor starts on `(next)`");
            assert_eq!(row.step, lands, "{id}");

            press(&mut board, &repo, &pipelines, &[crate::screen::Key::Enter]);
            assert!(matches!(board.mode, BoardMode::Browsing), "{id}");
            assert_eq!(repo.task(id).unwrap().stage(), lands, "{id}");
        }
        // The park went back by `unpark`, not by a `--stage` onto the same
        // step: its own lane is marked to be continued.
        assert_eq!(
            repo.task("park").unwrap().front.resume.as_deref(),
            Some("review")
        );
    }

    /// A caught block resumes to `blocked`, which `--stage` does not accept:
    /// it is one pinned `blocked (next)` row under the steps, with the cursor
    /// on it, and `enter` there sends the task exactly where a plain resume
    /// does.
    #[test]
    fn a_caught_block_pins_one_blocked_next_row_under_the_steps() {
        let (repo, _root_guard) = fixture("picker-caught-block");
        let pipelines = Pipelines::builtin();
        add(&repo, "ship-login", &[], None);
        caught_at_step(&repo, "ship-login", "review", "block");

        let mut board = Board::for_test();
        board.cursor = Some("ship-login".to_string());
        press(
            &mut board,
            &repo,
            &pipelines,
            &[crate::screen::Key::Char('r')],
        );

        let picker = open_picker(&board);
        assert_eq!(picker.header, "paused at review — it blocked");
        let steps: Vec<&str> = picker.rows.iter().map(|r| r.step.as_str()).collect();
        assert_eq!(
            steps,
            ["implement", "review", "document", "handover", "blocked"]
        );
        assert_eq!(picker.listed, 4);
        assert_eq!(picker.rows[picker.cursor].step, "blocked");
        assert_eq!(picker.rows[picker.cursor].note, "(next)");
        assert_eq!(
            picker.rows.iter().filter(|r| r.next).count(),
            1,
            "only the pinned row is `(next)`"
        );

        press(&mut board, &repo, &pipelines, &[crate::screen::Key::Enter]);
        assert_eq!(
            repo.task("ship-login").unwrap().stage(),
            crate::pipeline::BLOCKED
        );
    }

    /// A stopped step the pipeline no longer defines has no road to
    /// preselect: no `(next)` row, the header names the missing step, and the
    /// cursor starts on the first step.
    #[test]
    fn a_stopped_step_the_pipeline_lacks_has_no_next_row() {
        let (repo, _root_guard) = fixture("picker-missing-step");
        let pipelines = Pipelines::builtin();
        add(&repo, "lost", &[], None);
        let mut task = repo.task("lost").unwrap();
        task.front.parked_from = Some("gone".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let picker = resume_picker(&repo.task("lost").unwrap(), &pipelines, 0).unwrap();
        assert_eq!(
            picker.header,
            "paused at gone, a step pipeline `default` no longer has"
        );
        assert!(picker.rows.iter().all(|r| !r.next && r.note.is_empty()));
        assert_eq!(picker.cursor, 0);
        assert_eq!(picker.rows[0].step, "implement");
    }

    /// `enter` on a row that can no longer be resumed keeps the picker open
    /// with the refusal under the list, where the dispatch loop would
    /// otherwise have dropped it without a word.
    #[test]
    fn a_refused_pick_keeps_the_picker_open_and_prints_the_refusal() {
        let (repo, _root_guard) = fixture("picker-refused");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "base", &[], None, Some("base"));
        add_to(&repo, "ship-login", &[], None, Some("ship-login"));
        caught_at_step(&repo, "ship-login", "review", "pass");

        let mut board = Board::for_test();
        board.cursor = Some("ship-login".to_string());
        press(
            &mut board,
            &repo,
            &pipelines,
            &[crate::screen::Key::Char('r'), crate::screen::Key::Down],
        );
        // A dependency added after the picker opened: the row stops offering
        // the resume key, and `enter` reads that fresh.
        let mut task = repo.task("ship-login").unwrap();
        task.front.depends_on = vec!["base".to_string()];
        task.save().unwrap();
        press(&mut board, &repo, &pipelines, &[crate::screen::Key::Enter]);

        let picker = open_picker(&board);
        assert_eq!(picker.rows[picker.cursor].step, "handover");
        let error = picker.error.as_deref().unwrap();
        assert!(error.contains("cannot be resumed right now"), "{error}");
        let panel = resume_picker_panel(picker, None).join("\n");
        assert!(panel.contains("cannot be resumed right now"), "{panel}");
        assert_eq!(
            repo.task("ship-login").unwrap().stage(),
            crate::pipeline::PAUSED
        );
    }

    /// A reroute off a `p` park goes by `--stage` onto the picked step, and
    /// leaves the lane the park left standing on the step it left. That lane
    /// is a finished one now: it keeps its pane until the task is done, and
    /// counts for nothing because the task is no longer on its step.
    #[test]
    fn a_reroute_off_a_park_keeps_the_parked_steps_lane() {
        let (mut repo, _root_guard) = fixture("picker-reroute-park");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let (mux, name) = settled_headless_lane(&repo);

        // `p`'s own park. Under herdr, `p` interrupts the turn and its lane
        // stays standing, settled, which is the lane this checks for.
        park_under_lock(&repo, "login", ParkedBy::Board).unwrap();
        let mut board = Board::for_test();
        board.cursor = Some("login".to_string());
        assert_eq!(
            repo.task("login").unwrap().front.parked_from.as_deref(),
            Some("implement")
        );
        assert!(mux.list_lanes().unwrap().iter().any(|l| l.name == name));

        press(
            &mut board,
            &repo,
            &pipelines,
            &[
                crate::screen::Key::Char('r'),
                crate::screen::Key::Down,
                crate::screen::Key::Enter,
            ],
        );

        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), "review");
        assert_eq!(task.front.parked_from, None);
        assert!(
            mux.list_lanes().unwrap().iter().any(|l| l.name == name),
            "the parked step's lane keeps its pane"
        );
    }

    /// A pipeline taller than the pane scrolls inside the picker: drawn in a
    /// 15-row pane, a 20-step list keeps the cursor's row and the key line,
    /// with `▲`/`▼` lines counting what it leaves out.
    #[test]
    fn a_long_pipeline_scrolls_inside_a_short_pane() {
        let (repo, _root_guard) = fixture("picker-scrolls");
        let mut yaml = String::from("steps:\n");
        for i in 1..=20 {
            let on_pass = match i {
                20 => "done".to_string(),
                _ => format!("s{:02}", i + 1),
            };
            yaml.push_str(&format!(
                "  - id: s{i:02}\n    agent: pi\n    model: m\n    on_pass: {on_pass}\n"
            ));
        }
        let pipeline = crate::pipeline::Pipeline::parse("long", &yaml).unwrap();
        let pipelines = Pipelines {
            pipelines: [("long".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };
        add(&repo, "tall", &[], Some("s12"));
        let mut task = repo.task("tall").unwrap();
        task.front.pipeline = Some("long".to_string());
        park(&mut task, "paused from the board", false);
        task.save().unwrap();

        let picker = resume_picker(&repo.task("tall").unwrap(), &pipelines, 0).unwrap();
        assert_eq!(picker.rows[picker.cursor].step, "s12");
        let panel = resume_picker_panel(&picker, Some(15));

        assert!(panel.len() < 15, "{}", panel.join("\n"));
        let has = |text: &str| panel.iter().any(|line| line.contains(text));
        assert!(has("▸ s12"), "{}", panel.join("\n"));
        assert!(
            has("[↑↓] pick   [enter] resume   [esc] cancel"),
            "{}",
            panel.join("\n")
        );
        assert!(has("▲ ") && has("▼ "), "{}", panel.join("\n"));
        let shown = panel
            .iter()
            .filter(|line| line.contains("  s") || line.contains("▸ s"))
            .count();
        let count = |mark: &str| -> usize {
            let line = panel.iter().find(|line| line.contains(mark)).unwrap();
            line.split_whitespace().nth(2).unwrap().parse().unwrap()
        };
        assert_eq!(count("▲") + shown + count("▼"), 20, "{}", panel.join("\n"));
    }

    /// A picker left open while the task is resumed somewhere else refuses
    /// `enter` on any row, rather than rewinding the task or moving it under
    /// the lane now working it, and says why under the list.
    #[test]
    fn enter_on_a_picker_whose_task_moved_on_refuses() {
        let (repo, _root_guard) = fixture("picker-stale");
        let pipelines = Pipelines::builtin();
        add(&repo, "wall", &[], None);
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("review".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        for keys in [
            vec![crate::screen::Key::Enter],
            vec![crate::screen::Key::Down, crate::screen::Key::Enter],
        ] {
            let mut board = Board::for_test();
            board.cursor = Some("wall".to_string());
            press(
                &mut board,
                &repo,
                &pipelines,
                &[crate::screen::Key::Char('r')],
            );
            // Resumed from a shell while the picker is open.
            resume_task(&repo, &pipelines, "wall").unwrap();
            let before = repo.task("wall").unwrap();
            assert_eq!(before.stage(), "review");

            press(&mut board, &repo, &pipelines, &keys);

            let picker = open_picker(&board);
            let error = picker.error.as_deref().unwrap();
            assert!(error.contains("has moved on to `review`"), "{error}");
            let after = repo.task("wall").unwrap();
            assert_eq!(after.stage(), "review");
            assert_eq!(after.front.resume, before.front.resume);

            // Put it back on the block for the next round.
            let mut task = repo.task("wall").unwrap();
            task.front.blocked_from = Some("review".into());
            task.front.resume = None;
            task.set_stage(crate::pipeline::BLOCKED, None);
            task.save().unwrap();
        }
    }

    /// `p` on a blocked row parks it with `parked_from: blocked`, which names
    /// no step: the picker reads the step it blocked at instead, says it is
    /// paused while blocked, and pins `blocked (next)` — the park's own road
    /// back, by `unpark`.
    #[test]
    fn a_park_on_a_blocked_row_resumes_back_onto_blocked() {
        let (repo, _root_guard) = fixture("picker-park-blocked");
        let pipelines = Pipelines::builtin();
        add(&repo, "wall", &[], None);
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("review".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        park(&mut task, "paused from the board", false);
        task.save().unwrap();
        assert_eq!(task.front.parked_from.as_deref(), Some("blocked"));

        let mut board = Board::for_test();
        board.cursor = Some("wall".to_string());
        press(
            &mut board,
            &repo,
            &pipelines,
            &[crate::screen::Key::Char('r')],
        );

        let picker = open_picker(&board);
        assert_eq!(picker.header, "paused while blocked at review");
        let review = picker.rows.iter().find(|r| r.step == "review").unwrap();
        assert_eq!(review.note, "paused");
        let row = &picker.rows[picker.cursor];
        assert_eq!(
            (row.step.as_str(), row.note.as_str()),
            ("blocked", "(next)")
        );
        assert_eq!(picker.cursor, picker.listed, "pinned under the steps");

        press(&mut board, &repo, &pipelines, &[crate::screen::Key::Enter]);
        let task = repo.task("wall").unwrap();
        assert_eq!(task.stage(), crate::pipeline::BLOCKED);
        assert_eq!(task.front.blocked_from.as_deref(), Some("review"));
    }

    /// A gate on the last step resumes to `done`, which `--stage` does not
    /// accept: one pinned `done (next)` row holds the cursor, so `r` `enter`
    /// still finishes the task as `r` alone did, rather than rerunning the
    /// pipeline from its first step.
    #[test]
    fn a_gate_whose_pass_finishes_pins_one_done_next_row() {
        let (repo, _root_guard) = fixture("picker-done-next");
        let pipelines = Pipelines::builtin();
        add(&repo, "ship-login", &[], None);
        caught_at_step(&repo, "ship-login", "handover", "pass");

        let mut board = Board::for_test();
        board.cursor = Some("ship-login".to_string());
        press(
            &mut board,
            &repo,
            &pipelines,
            &[crate::screen::Key::Char('r')],
        );

        let picker = open_picker(&board);
        let row = &picker.rows[picker.cursor];
        assert_eq!((row.step.as_str(), row.note.as_str()), ("done", "(next)"));
        assert_eq!(picker.cursor, picker.listed, "pinned under the steps");
        assert!(
            picker.rows[..picker.listed]
                .iter()
                .all(|r| r.step != "done"),
            "`done` is never a step to pick"
        );

        press(&mut board, &repo, &pipelines, &[crate::screen::Key::Enter]);
        assert_eq!(
            repo.task("ship-login").unwrap().stage(),
            crate::pipeline::DONE
        );
    }

    /// A long refusal in a short pane keeps its first line, cut with `…`,
    /// and the list folds to the cursor's row to make room: the panel still
    /// fits, with the cursor's row, the refusal and the key line drawn.
    #[test]
    fn a_long_refusal_never_pushes_the_key_line_off_a_short_pane() {
        let (repo, _root_guard) = fixture("picker-long-refusal");
        let pipelines = Pipelines::builtin();
        add(&repo, "ship-login", &[], None);
        caught_at_step(&repo, "ship-login", "review", "block");
        let mut picker = resume_picker(&repo.task("ship-login").unwrap(), &pipelines, 0).unwrap();
        picker.cursor = 1;
        picker.error = Some("one\ntwo\nthree\nfour\nfive".to_string());

        let panel = resume_picker_panel(&picker, Some(15));

        assert!(panel.len() <= 12, "{}", panel.join("\n"));
        let has = |text: &str| panel.iter().any(|line| line.contains(text));
        assert!(has("▸ review"), "{}", panel.join("\n"));
        assert!(has("[↑↓] pick"), "{}", panel.join("\n"));
        assert!(has("one …"), "{}", panel.join("\n"));
        assert!(!has("two"), "{}", panel.join("\n"));
    }

    /// The refusal `enter` meets on a task that moved on is drawn in the
    /// 15-row pane the picker is held to, with or without a pinned row.
    #[test]
    fn a_stale_refusal_is_drawn_in_a_fifteen_row_pane() {
        let (repo, _root_guard) = fixture("picker-stale-short");
        let pipelines = Pipelines::builtin();
        add(&repo, "ship-login", &[], None);
        for outcome in ["block", "pass"] {
            caught_at_step(&repo, "ship-login", "review", outcome);
            let mut board = Board::for_test();
            board.cursor = Some("ship-login".to_string());
            press(
                &mut board,
                &repo,
                &pipelines,
                &[crate::screen::Key::Char('r')],
            );
            resume_task(&repo, &pipelines, "ship-login").unwrap();
            press(&mut board, &repo, &pipelines, &[crate::screen::Key::Enter]);

            let picker = open_picker(&board);
            assert_eq!(picker.listed < picker.rows.len(), outcome == "block");
            let panel = resume_picker_panel(picker, Some(15));
            let text = panel.join("\n");
            assert!(panel.len() <= 12, "{outcome}: {text}");
            assert!(text.contains("has moved on"), "{outcome}: {text}");
            assert!(text.contains("[↑↓] pick"), "{outcome}: {text}");
            assert!(text.contains('▸'), "{outcome}: {text}");
        }
    }

    // ---- Steps a task walks past: never named on the board ----

    /// The three rules that hide `suite` from a task, each set up on task
    /// `t`: its own `skip:`, `suite` declared `last:` with a task in `t`'s
    /// group depending on it, and `suite` declared `first:` with `t`
    /// depending on a finished `base`.
    #[derive(Clone, Copy, Debug)]
    enum Rule {
        Skip,
        Last,
        First,
    }

    const RULES: [Rule; 3] = [Rule::Skip, Rule::Last, Rule::First];

    /// `test` passes to `suite`, which passes to `document`; both fail back
    /// to `fix`. `key` is spliced into `suite`, for the `last:` and `first:`
    /// rules.
    fn hides_suite(key: &str) -> Pipelines {
        let yaml = format!(
            "steps:\n  \
             - id: implement\n    agent: pi\n    on_pass: test\n  \
             - id: test\n    run: 'true'\n    on_pass: suite\n    on_fail: fix\n  \
             - id: fix\n    agent: pi\n    loop: 2\n    on_pass: test\n  \
             - id: suite\n    run: 'true'\n{key}    on_pass: document\n    on_fail: fix\n  \
             - id: document\n    agent: pi\n    on_pass: handover\n  \
             - id: handover\n    run: 'true'\n    on_pass: done\n  \
             - id: blocked\n    agent: pi\n    session: true\n"
        );
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", &yaml).unwrap(),
        );
        pipelines
    }

    /// A queue in which `rule` hides `suite` from task `t`, standing on
    /// `stage`, and the pipelines that go with it.
    fn hidden_suite(
        name: &str,
        rule: Rule,
        stage: &str,
    ) -> (Repo, crate::scratch::ScratchRoot, Pipelines) {
        let (repo, root_guard) = fixture(name);
        let pipelines = match rule {
            Rule::Skip => hides_suite(""),
            Rule::Last => hides_suite("    last: true\n"),
            Rule::First => hides_suite("    first: true\n"),
        };
        match rule {
            Rule::Skip | Rule::Last => add(&repo, "t", &[], Some(stage)),
            // `t` names `base` once it is archived: queueing it before would
            // look for `base`'s branch to start from.
            Rule::First => {
                add(&repo, "base", &[], None);
                archive_on(&repo, "base", crate::pipeline::DONE);
                add(&repo, "t", &[], Some(stage));
            }
        }
        let mut task = repo.task("t").unwrap();
        match rule {
            Rule::Skip => task.front.skip = vec!["suite".into()],
            Rule::Last => add(&repo, "after", &["t"], None),
            Rule::First => task.front.depends_on = vec!["base".into()],
        }
        task.save().unwrap();
        (repo, root_guard, pipelines)
    }

    /// NEXT on a row working `test` names `document`, the first step past it
    /// that the task runs, whichever rule hides `suite`.
    #[test]
    fn next_names_the_first_step_the_task_runs() {
        for rule in RULES {
            let (repo, _root_guard, pipelines) =
                hidden_suite(&format!("next-past-{rule:?}"), rule, "test");
            let rows = rows(&repo, &pipelines).unwrap();
            let row = rows.iter().find(|r| r.id == "t").unwrap();
            assert_eq!(row.next, "→ document", "{rule:?}");
        }
    }

    /// The same pipelines name `suite` for a task none of the rules touch: the
    /// top of a chain still runs its `last:` step, and a chain's root its
    /// `first:` one.
    #[test]
    fn next_still_names_a_step_the_task_runs() {
        for key in ["", "    last: true\n", "    first: true\n"] {
            let (repo, _root_guard) = fixture("next-runs-suite");
            let pipelines = hides_suite(key);
            add(&repo, "t", &[], Some("test"));
            let rows = rows(&repo, &pipelines).unwrap();
            let row = rows.iter().find(|r| r.id == "t").unwrap();
            assert_eq!(row.next, "→ suite", "{key:?}");
        }
    }

    /// A paused row's NEXT names where its resume lands, past `suite`.
    #[test]
    fn a_paused_rows_next_names_where_its_resume_lands() {
        for rule in RULES {
            let (repo, _root_guard, pipelines) =
                hidden_suite(&format!("next-paused-past-{rule:?}"), rule, "test");
            caught_at_step(&repo, "t", "test", "pass");
            let rows = rows(&repo, &pipelines).unwrap();
            let row = rows.iter().find(|r| r.id == "t").unwrap();
            assert_eq!(row.next, "[r] → document", "{rule:?}");
        }
    }

    /// A command step's exit from `test` straight onto `document`, past a
    /// hidden `suite`, reads as the pass it was.
    #[test]
    fn a_move_past_a_hidden_step_reads_passed() {
        for rule in RULES {
            let (repo, _root_guard, pipelines) =
                hidden_suite(&format!("recent-past-{rule:?}"), rule, "test");
            let mut board = Board::for_test();
            read_once(&mut board, &repo, &pipelines);
            moved(&repo, "document");
            read_once(&mut board, &repo, &pipelines);
            let line = board
                .recent
                .iter()
                .find_map(|RecentEvent::Arrival { id, change, .. }| {
                    (id == "t").then(|| view::sentence(change))
                })
                .expect("the move is seen");
            assert_eq!(line, "passed test, moved to document", "{rule:?}");
        }
    }

    /// A lane start refused on `document`, whose `on_fail` names the hidden
    /// `suite`, writes that raw `on_fail` — a failed start does not land
    /// past hidden steps — and still reads as the launch that failed.
    #[test]
    fn a_failed_start_onto_a_hidden_on_fail_reads_could_not_launch() {
        for rule in RULES {
            let (repo, _root_guard, mut pipelines) =
                hidden_suite(&format!("recent-launch-{rule:?}"), rule, "document");
            let pipeline = pipelines.pipelines.get_mut("default").unwrap();
            let document = pipeline.steps.iter_mut().find(|s| s.id == "document");
            document.unwrap().on_fail = Some("suite".into());
            let mut board = Board::for_test();
            read_once(&mut board, &repo, &pipelines);
            moved(&repo, "suite");
            read_once(&mut board, &repo, &pipelines);
            let line = board
                .recent
                .iter()
                .find_map(|RecentEvent::Arrival { id, change, .. }| {
                    (id == "t").then(|| view::sentence(change))
                })
                .expect("the move is seen");
            assert_eq!(
                line, "could not launch document, moved to suite",
                "{rule:?}"
            );
        }
    }

    /// `r` on a task held after `test` passed leaves `suite` out, puts
    /// `on pass (next)` on `document`, and `enter` lands the task there.
    #[test]
    fn the_picker_leaves_out_a_step_the_task_walks_past() {
        for rule in RULES {
            let (repo, _root_guard, pipelines) =
                hidden_suite(&format!("picker-past-{rule:?}"), rule, "test");
            caught_at_step(&repo, "t", "test", "pass");

            let mut board = Board::for_test();
            board.cursor = Some("t".to_string());
            press(
                &mut board,
                &repo,
                &pipelines,
                &[crate::screen::Key::Char('r')],
            );

            let picker = open_picker(&board);
            let rows: Vec<(&str, &str)> = picker
                .rows
                .iter()
                .map(|r| (r.step.as_str(), r.note.as_str()))
                .collect();
            assert_eq!(
                rows,
                [
                    ("implement", ""),
                    ("test", "paused"),
                    ("fix", "on fail"),
                    ("document", "on pass (next)"),
                    ("handover", ""),
                ],
                "{rule:?}"
            );
            assert_eq!(picker.rows[picker.cursor].step, "document", "{rule:?}");

            press(&mut board, &repo, &pipelines, &[crate::screen::Key::Enter]);
            assert_eq!(repo.task("t").unwrap().stage(), "document", "{rule:?}");
        }
    }

    /// A task stopped on a step it walks past is really there, so that step
    /// stays listed, and `(next)` sits on where the resume lands past it.
    #[test]
    fn the_picker_keeps_a_hidden_step_the_task_stopped_at() {
        for rule in RULES {
            let (repo, _root_guard, pipelines) =
                hidden_suite(&format!("picker-stopped-{rule:?}"), rule, "suite");
            caught_at_step(&repo, "t", "suite", "pass");

            let task = repo.task("t").unwrap();
            let dependents = crate::dispatch::queue_dependents(&repo, &task).unwrap();
            let picker = resume_picker(&task, &pipelines, dependents).unwrap();
            let rows: Vec<(&str, &str)> = picker
                .rows
                .iter()
                .map(|r| (r.step.as_str(), r.note.as_str()))
                .collect();
            assert_eq!(
                rows,
                [
                    ("implement", ""),
                    ("test", ""),
                    ("fix", "on fail"),
                    ("suite", "paused"),
                    ("document", "on pass (next)"),
                    ("handover", ""),
                ],
                "{rule:?}"
            );
            let road = crate::commands::resume_road(&task, &pipelines).unwrap();
            let pipeline = pipelines.for_task(&task).unwrap();
            assert_eq!(
                road.landing(pipeline, &task, dependents),
                "document",
                "{rule:?}"
            );
        }
    }

    // ---- a full board scrolls its task table ----

    /// The plan's full board: nine groups and thirty-one tasks, in the order
    /// the board draws them.
    const FULL_BOARD: [(&str, &[&str]); 9] = [
        (
            "cart",
            &["cart-empty-state", "cart-totals", "cart-discounts"],
        ),
        (
            "checkout",
            &["checkout-charge", "checkout-receipt", "checkout-refund"],
        ),
        (
            "billing",
            &["invoice-pdf", "invoice-email", "refunds", "tax-rates"],
        ),
        (
            "search",
            &["index-build", "facet-ui", "search-ranking", "search-cache"],
        ),
        (
            "auth",
            &["auth-login", "auth-logout", "auth-reset", "auth-mfa"],
        ),
        (
            "admin",
            &["admin-users", "admin-roles", "admin-audit", "admin-flags"],
        ),
        (
            "profile",
            &["profile-edit", "profile-avatar", "profile-export"],
        ),
        ("reports", &["report-weekly", "report-csv", "report-charts"]),
        ("infra", &["infra-logs", "infra-alerts", "infra-backup"]),
    ];

    /// [`FULL_BOARD`] as a reading, with the footer the plan draws under it:
    /// two agent profiles' slots and two jobs, eight rows with the rule —
    /// eighteen fixed rows in all with the lockup and the key line.
    fn full_board() -> Snapshot {
        let rows = FULL_BOARD
            .iter()
            .flat_map(|(group, ids)| {
                ids.iter().map(move |id| Row {
                    group: Some(group.to_string()),
                    ..row(id)
                })
            })
            .collect();
        let soon = chrono::Local::now() + chrono::TimeDelta::hours(6);
        let job = |name: &str| crate::jobs::ActiveJob {
            name: name.to_string(),
            next: Some(soon),
        };
        Snapshot {
            rows,
            used: [("claude".to_string(), 1), ("pi".to_string(), 1)].into(),
            active_jobs: vec![job("nightly-audit"), job("weekly-deps")],
            ..Snapshot::empty()
        }
    }

    /// The full board painted stripped in a pane `height` rows tall, the
    /// cursor on `cursor`, with RECENT holding lines it could draw.
    fn full_frame(repo: &Repo, cursor: &str, height: usize) -> Vec<String> {
        let frame = paint_at(
            repo,
            &Pipelines::builtin(),
            false,
            &full_board(),
            Some(cursor),
            &arrivals(3),
            None,
            Some(height),
        );
        strip(&frame).lines().map(str::to_string).collect()
    }

    /// The rows the table was given: every row from the column header down
    /// to the blank row over the rule, neither included.
    fn table_rows(frame: &[String]) -> Vec<String> {
        let header = frame
            .iter()
            .position(|line| line.trim_start().starts_with("TASK"))
            .expect("a column header");
        let rule = frame
            .iter()
            .position(|line| line.contains("────"))
            .expect("a rule");
        frame[header + 1..rule - 1].to_vec()
    }

    /// What a table row reads as: its band, its task id with the cursor's
    /// mark, `total` for a group's total line, or the marker itself.
    fn read_row(line: &str) -> String {
        let trimmed = line.trim();
        match line.chars().nth(3) {
            _ if trimmed.is_empty() => String::new(),
            _ if trimmed.starts_with('▌')
                || trimmed.starts_with('↑')
                || trimmed.starts_with('↓') =>
            {
                trimmed.to_string()
            }
            Some(' ') => "total".to_string(),
            _ => line[1..]
                .split("   ")
                .next()
                .unwrap()
                .trim_end()
                .to_string(),
        }
    }

    /// The four frames the plan draws for a 30-row pane and a 22-row one,
    /// and a view that starts on a group's total line:
    /// the table takes every row between the column header and the rule,
    /// the marker is its last, and the footer and the key line stay on
    /// screen while RECENT — which had lines to draw — gives way.
    #[test]
    fn a_full_board_scrolls_its_table_under_the_cursor() {
        let (repo, _root_guard) = fixture("board-full-scrolls");
        let cases: [(&str, usize, &[&str]); 5] = [
            (
                "cart-empty-state",
                30,
                &[
                    "",
                    "▌cart",
                    "▸ cart-empty-state",
                    "  cart-totals",
                    "  cart-discounts",
                    "total",
                    "",
                    "▌checkout",
                    "  checkout-charge",
                    "  checkout-receipt",
                    "↓ 26 tasks below",
                ],
            ),
            (
                "search-ranking",
                30,
                &[
                    "▌billing",
                    "  invoice-email",
                    "  refunds",
                    "  tax-rates",
                    "total",
                    "",
                    "▌search",
                    "  index-build",
                    "  facet-ui",
                    "▸ search-ranking",
                    "↑ 7 tasks above · ↓ 18 tasks below",
                ],
            ),
            (
                "infra-backup",
                30,
                &[
                    "▌reports",
                    "  report-csv",
                    "  report-charts",
                    "total",
                    "",
                    "▌infra",
                    "  infra-logs",
                    "  infra-alerts",
                    "▸ infra-backup",
                    "total",
                    "↑ 26 tasks above",
                ],
            ),
            (
                "cart-empty-state",
                22,
                &["▌cart", "▸ cart-empty-state", "↓ 30 tasks below"],
            ),
            // The view starts on cart's total line, below cart's band, so
            // the band is pinned in the total line's place.
            (
                "invoice-pdf",
                30,
                &[
                    "▌cart",
                    "",
                    "▌checkout",
                    "  checkout-charge",
                    "  checkout-receipt",
                    "  checkout-refund",
                    "total",
                    "",
                    "▌billing",
                    "▸ invoice-pdf",
                    "↑ 3 tasks above · ↓ 24 tasks below",
                ],
            ),
        ];
        for (cursor, height, expected) in cases {
            let frame = full_frame(&repo, cursor, height);
            let drawn: Vec<String> = table_rows(&frame).iter().map(|l| read_row(l)).collect();
            assert_eq!(
                drawn, expected,
                "cursor {cursor}, {height} rows: {frame:#?}"
            );
            assert_eq!(frame.len(), height - 1, "fills the pane: {frame:#?}");
            assert!(
                frame.last().unwrap().contains("[o] open task"),
                "the key line stays last: {frame:#?}"
            );
            assert!(
                frame.iter().any(|l| l.contains("slots"))
                    && frame.iter().any(|l| l.contains("nightly-audit")),
                "the slots and the jobs stay: {frame:#?}"
            );
            assert!(
                !frame.iter().any(|l| l.contains("RECENT")),
                "RECENT gives way to a table that does not fit: {frame:#?}"
            );
        }
    }

    /// At every cursor position and height, the cursor's row is drawn, each
    /// task row drawn has its group's band above it in the view wherever the
    /// table has a row to spare for one, and the
    /// marker counts exactly the task rows that are not drawn — never a
    /// band, a total line or a blank row.
    #[test]
    fn every_task_on_a_full_board_is_reached_and_counted() {
        let (repo, _root_guard) = fixture("board-full-counts");
        let ids: Vec<(&str, &str)> = FULL_BOARD
            .iter()
            .flat_map(|(group, ids)| ids.iter().map(move |id| (*group, *id)))
            .collect();
        for height in [21, 22, 25, 30, 40] {
            for (k, (_, cursor)) in ids.iter().enumerate() {
                let rows = table_rows(&full_frame(&repo, cursor, height));
                let drawn: Vec<String> = rows.iter().map(|l| read_row(l)).collect();
                let at = |id: &str| {
                    drawn
                        .iter()
                        .position(|r| r.trim_start_matches(['▸', ' ']) == id)
                };
                assert!(
                    drawn.contains(&format!("▸ {cursor}")),
                    "{height} rows, cursor {k}: {drawn:#?}"
                );
                let shown: Vec<usize> =
                    (0..ids.len()).filter(|&i| at(ids[i].1).is_some()).collect();
                // A total line is never drawn without its group's band
                // over it either.
                assert!(
                    rows.len() <= 2 || drawn.first().is_none_or(|first| first != "total"),
                    "{height} rows, cursor {k}: the view opens on a bare total line: {drawn:#?}"
                );
                // A table of two rows holds the cursor's task and the
                // marker, with no row left to pin a band in.
                for &i in shown.iter().filter(|_| rows.len() > 2) {
                    let band = format!("▌{}", ids[i].0);
                    let band_at = drawn.iter().position(|r| *r == band);
                    assert!(
                        band_at.is_some_and(|b| b < at(ids[i].1).unwrap()),
                        "{height} rows, cursor {k}: {} drawn without its band: {drawn:#?}",
                        ids[i].1
                    );
                }
                let above = (0..ids.len()).filter(|&i| i < shown[0]).count();
                let below = (0..ids.len())
                    .filter(|&i| i > *shown.last().unwrap())
                    .count();
                assert_eq!(
                    shown.len(),
                    shown.last().unwrap() - shown[0] + 1,
                    "the drawn tasks run unbroken: {drawn:#?}"
                );
                let marker = crate::screen::pane::marker_row(above, below, Some("task"), 200);
                match (above, below) {
                    (0, 0) => assert!(
                        !drawn.iter().any(|r| r.starts_with(['↑', '↓'])),
                        "no marker with nothing hidden: {drawn:#?}"
                    ),
                    _ => assert_eq!(
                        drawn.last().unwrap(),
                        &marker,
                        "{height} rows, cursor {k}: {drawn:#?}"
                    ),
                }
            }
        }
    }

    /// A pane no taller than the lockup, the footer and the key line leaves
    /// the table no rows at all, and the frame is still cut to the pane by
    /// `clamp_rows` rather than scrolling the terminal.
    #[test]
    fn a_pane_shorter_than_the_fixed_parts_gives_the_table_nothing() {
        let (repo, _root_guard) = fixture("board-full-short");
        // Eighteen fixed rows and the one `clamp_rows` holds back.
        for height in [19, 12, 3] {
            let frame = full_frame(&repo, "cart-empty-state", height);
            assert!(
                frame.len() < height,
                "{height} rows drew {} lines: {frame:#?}",
                frame.len()
            );
            assert!(
                !frame.iter().any(|l| l.contains("cart-") || l.contains('▌')),
                "no table row at {height} rows: {frame:#?}"
            );
        }
        let frame = full_frame(&repo, "cart-empty-state", 19);
        assert!(
            frame.last().unwrap().contains("[o] open task"),
            "{frame:#?}"
        );
    }

    /// The whole table fits, so nothing scrolls, no marker is drawn, and
    /// RECENT takes the rows the table leaves.
    #[test]
    fn a_board_that_fits_draws_no_marker_and_keeps_recent() {
        let (repo, _root_guard) = fixture("board-fits");
        let snapshot = Snapshot {
            rows: vec![row("alpha"), row("beta")],
            ..Snapshot::empty()
        };
        let frame = strip(&paint_at(
            &repo,
            &Pipelines::builtin(),
            false,
            &snapshot,
            Some("alpha"),
            &arrivals(3),
            None,
            Some(30),
        ));
        assert!(frame.contains("RECENT"), "{frame}");
        assert!(frame.contains("alpha") && frame.contains("beta"), "{frame}");
        assert!(!frame.contains('↓') && !frame.contains('↑'), "{frame}");
    }

    /// The dispatch tab scrolls the same table inside its box: the marker
    /// and the cursor's row are drawn above the bottom border, and the key
    /// line under it.
    #[test]
    fn the_dispatch_tab_scrolls_a_full_board_inside_its_box() {
        let (repo, _root_guard) = fixture("board-full-hosted");
        let _hosting = crate::screen::shell::Hosting::open(crate::screen::shell::Tab::Dispatch);
        // What `pane_height` leaves inside the box of a 35-row terminal: the
        // strip's three rows and the box's two borders come off first.
        let height = 30;
        let frame = full_frame(&repo, "search-ranking", height);
        assert_eq!(frame.len(), height + 1, "{frame:#?}");
        assert!(
            frame.last().unwrap().contains("[o] open task"),
            "{frame:#?}"
        );
        assert!(frame[frame.len() - 2].starts_with('└'), "{frame:#?}");
        let inside = |text: &str| frame.iter().any(|l| l.starts_with('│') && l.contains(text));
        assert!(inside("▸ search-ranking"), "{frame:#?}");
        assert!(inside("▌billing"), "{frame:#?}");
        // One row more than the 30-row pane `spoolway dispatch` draws in:
        // the box has no top margin over the lockup.
        assert!(inside("↑ 6 tasks above · ↓ 18 tasks below"), "{frame:#?}");
        assert!(inside("nightly-audit"), "{frame:#?}");
        assert!(!inside("RECENT"), "{frame:#?}");
    }
}
