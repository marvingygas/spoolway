//! The live board `spoolway dispatch` draws in its own terminal, between passes.
//!
//! A renderer, not a participant. Every frame is read from the same two sources
//! of truth a pass reconciles from — the task files and the live lane list —
//! plus the usage ledger for what the run has spent. It writes nothing and
//! decides nothing: a run with the board up and a `--plain` run take exactly
//! the same decisions in the same order.
//!
//! Split across three files: this one holds the board's data model — the
//! `Board` and `Row` themselves, their state transitions and key handling,
//! and everything that reads task files, the lane list and the ledger into
//! rows. [`view`] holds the pure rendering that turns that data into
//! terminal output — the table, the footer, the ticker, the masthead — and
//! nothing in it reads a task file or a lane list of its own.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
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

use view::{
    AMBER, DIM, GUTTER, RESET, RecentEvent, Style, Verdict, clamp_rows, footer, group_totals,
    masthead, pane_height, pane_width, pause_confirm_panel, resume_confirm_panel, spool_frame,
    strip_ansi, table, ticker, unqueue_all_confirm_panel, unqueue_confirm_panel,
};

/// How often the board re-reads the state while it waits for the next pass.
/// File reads and one multiplexer call — cheap enough that nothing needs to be
/// event-driven.
///
/// One second, not two, because it is also the rate the lockup's mark is
/// sampled at — see [`view::spool_frame`], which turns on every whole second. A
/// redraw slower than that turn would alias it: the board would sample the
/// same phase every time and the mark would sit still while the run moved.
pub const POLL: Duration = Duration::from_secs(1);

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
    /// A person is the one thing between this task and the rest of its
    /// pipeline, and nothing went wrong. Two situations reach it: a gated
    /// step that finished and is waiting to be let past, and a live pane
    /// that ended its turn on a question. The state does not tell the two
    /// apart — NEXT does, naming the resume for the first and the pane to
    /// look at for the second — because the reader's next move is the same
    /// either way: go and intervene. Next to `blocked` rather than inside
    /// it, since a paused step passed and a blocked one did not.
    Paused,
    /// Something is working the task right now: a live lane, or the run of a
    /// command step, which has no lane at all.
    Running,
    /// At the pipeline's blocked step, carrying its reason.
    Blocked,
    /// Waiting on a dependency that can never arrive.
    Unreachable,
    /// Held on a clock, not on a person. Read off
    /// [`crate::task::Frontmatter::parked_at`], the fixed start of a
    /// continuous hold, falling back to a future
    /// [`crate::task::Frontmatter::parked_until`] for a legacy park that has
    /// no `parked_at`. Next to `Queued` rather than `Paused`: nothing went
    /// wrong and nobody has anything to answer, it is simply not this task's
    /// turn yet.
    Parked,
    /// In the queue, waiting for a slot or a dependency.
    Queued,
    /// Archived — its pipeline finished and its file moved to the project's
    /// own `archive/`. Kept on the board, dimmed, for as long as its
    /// group still has a task in the queue: see [`rows_for_board`].
    Done,
}

/// What the run is doing at the moment a frame is drawn, which is the one
/// thing on the board that no task file records.
///
/// Only [`Phase::Stopping`] changes what is drawn, and it is the one state a
/// reader cannot infer from a frame sitting in front of them: a board between
/// passes and a board mid-pass look identical, and how long until the next
/// pass is not something a person watching can act on. The variants stay
/// apart anyway — the dispatcher knows which it is in, and a board that had to
/// guess would be the wrong shape for the next thing anyone wants to show.
#[derive(Clone, Copy)]
pub enum Phase {
    /// A pass is in flight right now.
    Passing,
    /// Between passes, with the next one due.
    Waiting,
    /// The last frame: the queue is empty and the run is ending.
    Stopping,
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
    /// Whether this task declared `parallel: true` — an overlap with another
    /// declared-parallel task of the same group is deliberate, not a missing
    /// `depends_on`. Marked on the row so a reader of the board sees the same
    /// thing `queue conflicts` reasons from.
    pub parallel: bool,
    pub stage: String,
    /// How many laps of this route the task has taken against the route's
    /// own budget, `(laps, limit)` — read off the step it is on now and the
    /// route named in `arrived_from`, the same pair `apply_loop_budget`
    /// compares before it lets a lap through. `None` wherever the step
    /// declares no `loop:` for that route, which draws as a bare step id.
    /// Shown on the STEP column from the first arrival: unlike the old NEXT
    /// suffix this replaced, there is no floor here, since a step's own row
    /// is where a reader would look to ask "is this looping" in the first
    /// place.
    pub step_loop: Option<(u32, u32)>,
    /// The pipeline this task resolves to, by name — what the `PIPELINE`
    /// column draws. A task with no `pipeline:` of its own names its
    /// project's configured default, exactly [`Pipelines::for_task`]'s own
    /// answer; an archived row whose named pipeline has since been deleted
    /// names it verbatim rather than erroring, since nothing here can
    /// resolve it any more.
    pub pipeline: String,
    pub state: State,
    /// How long a `Parked` row has been held, already formatted — `None` on
    /// every other state and when a legacy park has no `parked_at` to count
    /// from. Kept off [`State`] itself, which carries no data on any of its
    /// variants, and composed into the STATE cell by
    /// [`view::state_cell_text`] instead.
    pub parked_display: Option<String>,
    /// A board-only explanation that precedes the elapsed age on a `Parked`
    /// row. Kept separate so the machine-readable `parked_age` field remains
    /// a duration rather than sometimes becoming diagnostic prose.
    pub parked_reason: Option<String>,
    /// The formatted recheck clock historically exposed as `parked_until`
    /// by `queue list --json`. The board no longer draws this clock, but the
    /// machine-readable contract retains it beside the additive park age.
    pub parked_until_display: Option<String>,
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
    /// How long this task's lane has been open at the step it is on: `now -
    /// launched_at` while a lane is live, and the ledger's summed `wall_s` at
    /// that step once it is not — every round of it, the same as OUT and
    /// COST. `None` where neither answers: no live lane and nothing banked.
    pub lane_time: Option<i64>,
    /// The part of the three figures above that nothing has banked yet. See
    /// [`Unbanked`].
    pub unbanked: Unbanked,
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
/// run, so a frame is up exactly as long as the dispatcher is.
pub struct Board {
    /// Task id → the stage it was on at the last frame.
    stages: BTreeMap<String, String>,
    recent: VecDeque<RecentEvent>,
    /// Whether the queue as it stood at the first frame has been taken as the
    /// starting point. Without this every task already in flight is announced
    /// as news the moment the board opens.
    adopted: bool,
    /// The terminal, taken for as long as the board is up and given back on
    /// `Drop` — the cursor hidden and, on Unix, the tty deaf to whatever gets
    /// typed at it. Held here rather than beside it in the run loop so the
    /// two ways a run ends — the queue emptying and `ctrl-c` — both unwind
    /// through the same `Board` going out of scope and reach the same
    /// restore.
    _term: crate::platform::TermGuard,
    /// Whether this board is reading someone else's run rather than driving
    /// one of its own — see [`Board::watching`]. The only thing it changes is
    /// the header: everything below it is read fresh from the same task
    /// files and the same live lane list regardless of who is watching.
    watching: bool,
    /// The task id the cursor sits on, if it has been moved at all. By id
    /// rather than a plain row index, so a state change that resorts the
    /// board — a task passing its step, one landing on `paused` above it —
    /// never leaves the cursor pointing at a different task than the one a
    /// person last put it on.
    cursor: Option<String>,
    /// What a `p`, `P` or `R` keypress is waiting on, if anything — see
    /// [`BoardMode`]. `Browsing` on every other key, including the plain
    /// cursor moves and `r`, which never open a panel at all.
    mode: BoardMode,
    /// A one-minute memo for the "a job keeps this run resident" footer, so
    /// the per-second redraw does not re-scan the calendar for every enabled
    /// cron job — see [`crate::jobs::staying_up_cached`].
    jobs_next: Option<crate::jobs::StayingUpMemo>,
}

impl Board {
    pub fn new() -> Board {
        Board::with_term(crate::platform::TermGuard::new())
    }

    fn with_term(term: crate::platform::TermGuard) -> Board {
        Board {
            stages: BTreeMap::new(),
            recent: VecDeque::new(),
            adopted: false,
            _term: term,
            watching: false,
            cursor: None,
            mode: BoardMode::Browsing,
            jobs_next: None,
        }
    }

    /// A board whose terminal guard is inert — for tests, so parallel `Board`s
    /// do not take the process's real terminal raw and race on restore
    /// (finding 53).
    #[cfg(test)]
    pub fn for_test() -> Board {
        Board::with_term(crate::platform::TermGuard::inert())
    }

    /// [`Board::for_test`], watching rather than driving.
    #[cfg(test)]
    pub fn watching_for_test() -> Board {
        Board::watching_with_term(crate::platform::TermGuard::inert())
    }

    /// A watching board over a given terminal guard — the one place the
    /// watching board's shape is spelled, so [`Board::watching`] and
    /// [`Board::watching_for_test`] cannot drift.
    fn watching_with_term(term: crate::platform::TermGuard) -> Board {
        Board {
            watching: true,
            ..Board::with_term(term)
        }
    }

    /// A board for a process that holds no lock of its own: `spoolway
    /// dispatch` finding one already running. Draws the exact rows a driving
    /// board would — the same task files, the same [`Mux::list_lanes`] — but
    /// its header names whoever [`crate::lock::Lock::holder`] says holds the
    /// lock right now, read fresh every frame, rather than this process's own
    /// pid. A dispatcher that dies mid-watch is what turns that into "no
    /// dispatcher is running" on the very next redraw, with whatever it left
    /// running still on the board — the one thing a killed-rather-than-stopped
    /// run needs a person to see.
    pub fn watching() -> Board {
        Board::watching_with_term(crate::platform::TermGuard::new())
    }

    /// Draw one frame over whatever is on the terminal.
    ///
    /// `phase` is what the header reports the run is doing.
    ///
    /// The main screen buffer, not the alternate one: `ctrl-c` is how a run is
    /// stopped, and it kills this process without unwinding, which would leave
    /// a terminal stuck in a screen nothing is drawing to. Here the last frame
    /// simply stays where it is and the shell prompt appears under it.
    pub fn draw(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        phase: Phase,
        out: &mut impl Write,
    ) -> Result<()> {
        let frame = self.frame(repo, pipelines, phase)?;
        let _ = write!(out, "\x1b[2J\x1b[H{frame}");
        let _ = out.flush();
        Ok(())
    }

    /// One frame, built whole before anything is written so a slow read never
    /// leaves a half-drawn board on screen.
    fn frame(&mut self, repo: &Repo, pipelines: &Pipelines, phase: Phase) -> Result<String> {
        let frame = render(
            repo,
            pipelines,
            phase,
            self.watching,
            &mut self.stages,
            &mut self.recent,
            self.cursor.as_deref(),
            &mut self.jobs_next,
        )?;
        if !self.adopted {
            self.recent.clear();
            self.adopted = true;
        }
        // A confirm panel sits on top of the table it interrupted, the same
        // way `spoolway queue`'s own pickers do — see `crate::screen::overlay`.
        // Built from a `String` rather than the `Vec<String>` `overlay` wants,
        // because every other reader of `render`'s output — `draw`, the tests
        // below — wants one whole frame too, and a second return shape here
        // would be for this one caller alone.
        Ok(match self.mode.panel() {
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
                let stripped: Vec<String> = frame.lines().map(strip_ansi).collect();
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
                crate::screen::overlay(&mut lines, &panel);
                lines.join("\n")
            }
            None => frame,
        })
    }

    /// Apply one key read while the board is up.
    ///
    /// Browsing, `↑`/`↓` move the cursor; lowercase acts on the row it sits
    /// on and uppercase acts on the whole run — `r` resumes the cursor's row
    /// if its own resume key is live, `R` resumes every paused row that is;
    /// `p` pauses just the cursor's task, `P` pauses the run; `u` takes the
    /// cursor's task off the queue and back to pending if nothing has
    /// started for it and no still-queued task depends on it, `U` does the
    /// same for every task that has not started. A run-wide key opens a
    /// confirm panel first wherever what it is about to do is not free to
    /// undo — `u` and `U` open one unconditionally, since writing a document
    /// back to pending is exactly that — see [`BoardMode`]. With a panel
    /// already open every other key is read by that panel instead, and a
    /// key neither mode recognises is ignored — `enter` and `q` included,
    /// since resuming and quitting now belong to `r` and `ctrl-c` — the
    /// board answers a gate and the run around it, and nothing past that;
    /// see the task's own non-goals for the keys this deliberately does not
    /// add.
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
            BoardMode::ConfirmPause { running, scope } => {
                self.on_key_pause_confirm(repo, running, scope, key)
            }
            BoardMode::ConfirmResume(gated) => {
                self.on_key_resume_confirm(repo, pipelines, gated, key)
            }
            BoardMode::ConfirmUnqueue { id, dir } => {
                self.on_key_unqueue_confirm(repo, pipelines, id, dir, key)
            }
            BoardMode::ConfirmUnqueueAll(ids) => self.on_key_unqueue_all_confirm(repo, ids, key),
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
            Key::Up => {
                self.cursor = shift_cursor(&rows(repo, pipelines)?, self.cursor.as_deref(), -1)
            }
            Key::Down => {
                self.cursor = shift_cursor(&rows(repo, pipelines)?, self.cursor.as_deref(), 1)
            }
            Key::Char('o') => self.open_cursor(repo)?,
            Key::Char('r') => self.resume_cursor(repo, pipelines)?,
            Key::Char('R') => self.begin_resume_all(repo, pipelines)?,
            Key::Char('p') => self.begin_pause_cursor(repo, pipelines)?,
            Key::Char('P') => self.begin_pause_all(repo, pipelines)?,
            Key::Char('u') => self.begin_unqueue_cursor(repo)?,
            Key::Char('U') => self.begin_unqueue_all(repo)?,
            _ => {}
        }
        Ok(())
    }

    /// `o`: open the cursor's queued task file in an editor, in a pane the
    /// multiplexer opens — a no-op with no cursor or a cursor on a row the
    /// queue no longer has. Never blocks: the pane runs the editor on its
    /// own, and the board keeps redrawing and the pass loop keeps running
    /// while it is open, exactly as if nothing had happened.
    ///
    /// A backend with no pane to open one in — headless, which refuses the
    /// way `open_tab` already does — is best-effort like every other key
    /// here: the refusal is swallowed, exactly as an `Err` out of `on_key`
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
        let mux = crate::mux::backend(repo);
        let _ = mux.open_command(&repo.root, &format!("{id} · edit"), &command);
        Ok(())
    }

    /// Send the highlighted row through `spoolway resume`, exactly as a
    /// person typing the command would.
    ///
    /// A no-op wherever there is nothing to do: no cursor yet, a cursor
    /// sitting on a row the queue no longer has, or a row whose own
    /// `resumable` says the key does nothing here — the dependency or
    /// busy-lane rule that decided the NEXT column already decided this.
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
        resume_task(repo, pipelines, &id)
    }

    /// `P`: interrupt every live agent lane this run owns and park each of
    /// their tasks, then — only if a command step is running somewhere in
    /// the queue — open [`BoardMode::ConfirmPause`], scoped to the whole
    /// run, to ask what to do with it. The agent lanes are parked whether or
    /// not that panel ends up opening: nothing about resuming a command step
    /// later is reversible the way an interrupted turn is, so only the
    /// irreversible half waits on a person.
    fn begin_pause_all(&mut self, repo: &Repo, pipelines: &Pipelines) -> Result<()> {
        let mut tasks = repo.tasks()?;
        let mux = crate::mux::backend(repo);
        let lanes = mux.list_lanes().unwrap_or_default();
        for i in live_agent_lane_tasks(repo, &tasks, pipelines, &lanes) {
            let name = crate::mux::lane_name(tasks[i].stage(), tasks[i].id());
            // Best-effort: a lane that has already gone quiet on its own has
            // nothing left to interrupt, and one lane's failure here must
            // never stop the rest of the run from parking.
            let _ = mux.interrupt_lane(&name);
            park(&mut tasks[i], "paused from the board");
            tasks[i].save()?;
        }
        let running = running_command_steps(repo, &tasks, pipelines);
        if !running.is_empty() {
            self.mode = BoardMode::ConfirmPause {
                running,
                scope: PauseScope::All,
            };
        }
        Ok(())
    }

    /// `p`: the same interrupt-and-park `P` does, narrowed to the cursor's
    /// own task — a no-op with no cursor, or with the cursor on a row this
    /// run owns no live agent lane for. Opens [`BoardMode::ConfirmPause`],
    /// titled with the task's id, only if that task's own step is itself a
    /// running command step — the case a plain interrupt cannot reach,
    /// exactly as `P` reaches it for the whole run.
    fn begin_pause_cursor(&mut self, repo: &Repo, pipelines: &Pipelines) -> Result<()> {
        let Some(id) = self.cursor.clone() else {
            return Ok(());
        };
        let mut tasks = repo.tasks()?;
        let Some(i) = tasks.iter().position(|t| t.id() == id) else {
            return Ok(());
        };
        let mux = crate::mux::backend(repo);
        let lanes = mux.list_lanes().unwrap_or_default();
        if live_agent_lane_tasks(repo, &tasks, pipelines, &lanes).contains(&i) {
            let name = crate::mux::lane_name(tasks[i].stage(), tasks[i].id());
            let _ = mux.interrupt_lane(&name);
            park(&mut tasks[i], "paused from the board");
            tasks[i].save()?;
        }
        let running: Vec<CommandRunning> = running_command_steps(repo, &tasks, pipelines)
            .into_iter()
            .filter(|cr| cr.task == id)
            .collect();
        if !running.is_empty() {
            self.mode = BoardMode::ConfirmPause {
                running,
                scope: PauseScope::Cursor(id),
            };
        }
        Ok(())
    }

    fn on_key_pause_confirm(
        &mut self,
        repo: &Repo,
        running: Vec<CommandRunning>,
        scope: PauseScope,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            // Killing them is what stops the run for good: their tasks are
            // parked exactly as the agent lanes already were, `parked_from`
            // naming the step whose run this took down.
            Key::Char('k') => {
                let runs = crate::command_step::Runs::new(&repo.commands_dir());
                for cr in &running {
                    runs.stop(&crate::command_step::Runs::key(&cr.step, &cr.task));
                    let mut task = repo.task(&cr.task)?;
                    park(&mut task, "paused from the board");
                    task.save()?;
                }
            }
            // Declining, or backing out with `esc`, leaves every one of them
            // exactly as it was — nothing here to undo, since nothing was
            // touched while the panel was open.
            Key::Char('l') | Key::Esc => {}
            _ => self.mode = BoardMode::ConfirmPause { running, scope },
        }
        Ok(())
    }

    /// `R`: resume every paused task, opening [`BoardMode::ConfirmResume`]
    /// first — and naming the tasks it would carry past a gate — whenever
    /// any of them is a genuine gate rather than a park `p` or an interrupt
    /// left behind.
    fn begin_resume_all(&mut self, repo: &Repo, pipelines: &Pipelines) -> Result<()> {
        let tasks = repo.tasks()?;
        let paused: Vec<&crate::task::Task> = tasks
            .iter()
            .filter(|t| t.stage() == crate::pipeline::PAUSED)
            .collect();
        if paused.is_empty() {
            return Ok(());
        }
        let gated: Vec<String> = paused
            .iter()
            .filter(|t| t.front.paused_at.is_some())
            .map(|t| t.id().to_string())
            .collect();
        if gated.is_empty() {
            self.resume_all(repo, pipelines)?;
        } else {
            self.mode = BoardMode::ConfirmResume(gated);
        }
        Ok(())
    }

    fn on_key_resume_confirm(
        &mut self,
        repo: &Repo,
        pipelines: &Pipelines,
        gated: Vec<String>,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            Key::Char('R') => self.resume_all(repo, pipelines)?,
            Key::Esc => {}
            _ => self.mode = BoardMode::ConfirmResume(gated),
        }
        Ok(())
    }

    /// Every paused row whose own resume key is live, sent through
    /// [`resume_task`] — the same rule and the same code `r` uses on
    /// one row, just walked over all of them. A row not yet resumable —
    /// a dependency still running, a lane of its own still mid-turn — is
    /// left exactly where it is; the next `R`, or its own `r`, catches
    /// it once it is.
    fn resume_all(&mut self, repo: &Repo, pipelines: &Pipelines) -> Result<()> {
        for row in rows(repo, pipelines)?
            .iter()
            .filter(|r| r.state == State::Paused && r.resumable)
        {
            resume_task(repo, pipelines, &row.id)?;
        }
        Ok(())
    }

    /// `u`: open [`BoardMode::ConfirmUnqueue`] for the cursor's task — a
    /// no-op with no cursor, a cursor on a row the queue no longer has, a
    /// task that has started ([`not_started`] says no), or a task some other
    /// still-queued task names in its own `depends_on` — unqueuing it would
    /// strand that dependent on a dependency the board no longer shows it.
    fn begin_unqueue_cursor(&mut self, repo: &Repo) -> Result<()> {
        let Some(id) = self.cursor.clone() else {
            return Ok(());
        };
        let tasks = repo.tasks()?;
        let Some(task) = tasks.iter().find(|t| t.id() == id) else {
            return Ok(());
        };
        if !not_started(task) || depended_on_by_queued(&tasks, &id) {
            return Ok(());
        }
        self.mode = BoardMode::ConfirmUnqueue {
            id,
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
        id: String,
        dir: PathBuf,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            Key::Char('u') => {
                // The row this task sits on is about to leave the table, so
                // asking where `↓` would go has to happen against the rows
                // as they stand right now — the same list the removed row is
                // still part of. `shift_cursor` already wraps and already
                // falls back to the first row, so the only case this adds is
                // the one it can't see: with nothing else queued, the "next"
                // row it finds is the one about to vanish, and the cursor
                // clears instead of pointing at a task the board no longer
                // shows.
                let before = rows(repo, pipelines)?;
                let next = shift_cursor(&before, Some(&id), 1).filter(|next_id| *next_id != id);
                unqueue_task(repo, &id)?;
                self.cursor = next;
            }
            Key::Esc => {}
            _ => self.mode = BoardMode::ConfirmUnqueue { id, dir },
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
            Key::Char('U') => {
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

/// The command line `o` runs on a document, everywhere `o` appears.
///
/// The same resolution `spoolway config edit` already uses: `$VISUAL`, then
/// `$EDITOR`, then a platform default. Shared between [`Board::open_cursor`]
/// and the queue screen's own `o` — `crate::commands::queue::open_highlighted`
/// — so the two can never resolve to two different editors.
pub(crate) fn editor_command(path: &Path) -> String {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| if cfg!(windows) { "notepad" } else { "vi" }.to_string());
    format!("{editor} '{}'", path.display())
}

/// What a `p`, `P`, `R`, `u` or `U` keypress is waiting to be answered — a
/// panel drawn over the table, and the one thing standing between an
/// accidental press and the run it would otherwise change. `Default` is
/// `Browsing`, both for [`Board::new`] and for [`std::mem::take`] inside
/// [`Board::on_key`], which is what lets each key handler build the mode's
/// replacement without holding a borrow of `self.mode` open across it.
#[derive(Default)]
enum BoardMode {
    #[default]
    Browsing,
    /// `p` or `P` found a command step running: what it would kill, named,
    /// and nothing acted on yet. `[k]` stops them and parks their tasks too;
    /// `[l]` and `[esc]` both leave them exactly as they are — whatever
    /// agent lane this run owns for that scope was already interrupted and
    /// parked before this panel ever opens, so there is nothing about it
    /// left to decide. `scope` says whether this is `P`'s whole-run panel or
    /// `p`'s, narrowed to one task.
    ConfirmPause {
        running: Vec<CommandRunning>,
        scope: PauseScope,
    },
    /// `R` found a paused task still waiting at a gate, named so that
    /// carrying it past that gate is never the accidental half of a
    /// keypress meant for a plain interrupted one beside it.
    ConfirmResume(Vec<String>),
    /// `u` found a task that has not started, named along with `dir` — this
    /// run's own [`Repo::pending_dir`], read once when the panel opened — so
    /// its panel can show where the document is about to land without a
    /// second lookup at answer time. `id` still needs a fresh
    /// [`Repo::task`] to answer with, the same as every confirm panel here:
    /// the document a moment ago and the document now are not guaranteed to
    /// be the same file.
    ConfirmUnqueue { id: String, dir: PathBuf },
    /// `U`'s own version of the same panel, naming every task it would carry
    /// back to pending rather than just the one under the cursor.
    ConfirmUnqueueAll(Vec<String>),
}

impl BoardMode {
    /// The panel this mode draws over the table, if any.
    fn panel(&self) -> Option<Vec<String>> {
        match self {
            BoardMode::Browsing => None,
            BoardMode::ConfirmPause { running, scope } => Some(pause_confirm_panel(running, scope)),
            BoardMode::ConfirmResume(gated) => Some(resume_confirm_panel(gated)),
            BoardMode::ConfirmUnqueue { id, dir } => Some(unqueue_confirm_panel(id, dir)),
            BoardMode::ConfirmUnqueueAll(ids) => Some(unqueue_all_confirm_panel(ids)),
        }
    }
}

/// Who a [`BoardMode::ConfirmPause`] panel is about — the whole run for `P`,
/// or one task for `p` — which decides both the panel's title and whether
/// its body reads "it"/"this task" or "them"/"the run".
enum PauseScope {
    All,
    Cursor(String),
}

/// One command step `p` or `P` found running, named for
/// [`BoardMode::ConfirmPause`]'s panel — the task it belongs to, the step,
/// and how long it has been going, off the same clock the board's own TIME
/// column reads.
///
/// `pub(crate)`: `spoolway queue pause` reads the same list, since a running
/// command step is the one case it cannot just act on without a person
/// there to answer the board's own confirm panel — see
/// `commands::queue::queue_pause`.
pub(crate) struct CommandRunning {
    pub(crate) task: String,
    pub(crate) step: String,
    pub(crate) elapsed: Option<Duration>,
}

/// Send one task through `spoolway resume`, exactly as a person typing the
/// command would — this is the same [`crate::commands::report::resume`], not
/// a second copy of what it does. Shared between `r` on one row and `R`
/// walking every paused row at once, so the two can never resume the same
/// task two different ways. `pub(crate)`: `spoolway queue resume` is the
/// third caller, for a person with no board in front of them at all — see
/// `commands::queue::queue_resume`.
///
/// Silent about a task the queue no longer has — read fresh a moment before
/// this is called, both callers already know it is there, and racing a
/// second process that archived or removed it since is not this key's to
/// report.
pub(crate) fn resume_task(repo: &Repo, pipelines: &Pipelines, id: &str) -> Result<()> {
    let Ok(task) = repo.task(id) else {
        return Ok(());
    };
    // `paused_at` is a gate passed, waiting to be sent on past it;
    // `parked_from` is a person's own interrupt, and `blocked_from` is a
    // real block — both waiting to be sent back to where they stopped. Only
    // ever one of the three, and never none, on a task genuinely standing on
    // `paused` — so a task there with none of them set is a race: some other
    // process already answered it since the row was built, and there is
    // nothing left here for this key to do.
    //
    // A task holding a person-answered *question* never reaches `paused` at
    // all — it is still standing on its own live step, and none of the three
    // fields is ever set for it — so this guard only reads them on the stage
    // where their absence is actually a race rather than the ordinary shape
    // of that row.
    //
    // Nothing here has to say which of the three: `commands::resume` reads
    // `paused_at.is_some()` itself to route a gate one way and everything
    // else the other, so naming a step here would only risk disagreeing
    // with it — see `back_onto_its_step`, which finds `parked_from` and
    // `blocked_from` on its own, and which also handles the question-pane
    // case by falling back to `resume_target`'s `last_report.step`: pressing
    // `r` there restarts the step the pane's question was never answered on.
    if task.stage() == crate::pipeline::PAUSED
        && task.front.paused_at.is_none()
        && task.front.parked_from.is_none()
        && task.front.blocked_from.is_none()
    {
        return Ok(());
    }
    crate::commands::resume(
        repo,
        pipelines,
        &crate::cli::ResumeArgs {
            task: id.to_string(),
            stage: None,
            reject: false,
            message: None,
        },
        false,
    )?;
    Ok(())
}

/// Whether `task` has not started at all — still sitting on the pipeline's
/// own `queued` step, with no worktree cut and no lane ever begun. This is
/// the one state `u`/`U` may act on: unqueuing anything further along would
/// mean tearing down a checkout, which is outside what either key reaches —
/// see this task's own non-goals.
fn not_started(task: &crate::task::Task) -> bool {
    task.stage() == crate::pipeline::QUEUED
}

/// Whether some other task that has not started itself names `id` in its own
/// `depends_on` — the one thing `u` refuses that `U` does not, since carrying
/// `id` back to pending alone would leave that dependent waiting on a
/// dependency the queue no longer shows it.
///
/// Only a task that has not started can be waiting on `id` at all: `queued`
/// is the one step a dependency check gates, so nothing past it depends on a
/// task still active in the queue — see [`crate::graph::Graph::ready`]. The
/// check is still made explicit here, rather than assumed, so a caller never
/// has to trust that invariant to read this correctly.
fn depended_on_by_queued(tasks: &[crate::task::Task], id: &str) -> bool {
    tasks
        .iter()
        .any(|t| t.id() != id && not_started(t) && t.front.depends_on.iter().any(|d| d == id))
}

/// Move one task's document from the queue back to pending, dropping every
/// [`crate::commands::RESERVED_KEYS`] field from its frontmatter so `queue
/// add --from` accepts it exactly as it would a document that had never been
/// queued — the same fields `queue_add::parse_submission` refuses to see set
/// on a document coming in.
///
/// `Task::save` cannot do this alone: `stage` is a plain `String` with no
/// `skip_serializing_if`, so it always round-trips, and it is one of the
/// keys that has to disappear entirely rather than clear to empty. The
/// frontmatter goes through a bare YAML mapping instead, exactly as
/// `parse_submission` reads one coming in, so the same keys that document
/// path refuses are the ones this path removes.
///
/// Re-reads `id` fresh rather than trusting whatever `Task` a caller already
/// has — the same reason [`resume_task`] does — and checks `not_started`
/// again before touching anything: a task this key already refused a moment
/// ago, or one a second process moved on since the panel opened, is left
/// exactly where it is rather than risk carrying an archived document back
/// to pending.
fn unqueue_task(repo: &Repo, id: &str) -> Result<()> {
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
    let dest = repo.pending_dir().join(format!("{id}.md"));
    // A document already sitting in `pending/` is a newer draft — a producer
    // re-ran over work already submitted — and putting the queued copy back
    // on top of it would silently lose that draft. Leave everything where it
    // is: the row stays queued, and the reason goes to the problem log
    // rather than breaking the board loop this runs inside.
    if dest.exists() {
        crate::problem_log::append(
            repo,
            &format!("did not unqueue `{id}`: a newer draft is already in the pending directory"),
        );
        return Ok(());
    }
    let mut front = serde_norway::to_value(&task.front)
        .with_context(|| format!("serialising {id}'s frontmatter"))?;
    if let serde_norway::Value::Mapping(map) = &mut front {
        for key in crate::commands::RESERVED_KEYS {
            map.remove(*key);
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
    Ok(())
}

/// Park one task on `paused` with `parked_from` naming the step it was on —
/// the record `p` leaves behind, read back by [`resume_task`] exactly as a
/// block's own `blocked_from` is, but never confused with one: `blocked_from`
/// is left untouched, since nothing here failed the way a block does. Never
/// touches `paused_at` either: this task passed no gate, so there is no step
/// to release it past, only one to send it back to.
///
/// Goes through [`crate::task::Task::set_stage_unbanked`] rather than
/// `set_stage`: the task never left `implement` (or wherever it was), so
/// arriving at `paused` and leaving it again are not laps of anything, and
/// counting them would let a `loop:` budget see two arrivals nothing routed.
///
/// `pub(crate)`, and taking `message` rather than hard-coding one, so
/// `dispatch::Dispatcher` can write the same two fields the same way for a
/// person's own Escape in a pane — see `Dispatcher::park_after_interrupt` —
/// with a `## Status Log` line that says why *that* park happened, which is
/// not "paused from the board".
pub(crate) fn park(task: &mut crate::task::Task, message: &str) {
    task.front.parked_from = Some(task.stage().to_string());
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
        if task.stage() == crate::pipeline::PAUSED || task.stage() == crate::pipeline::BLOCKED {
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

/// Every task on a command step whose run is still going — what `p` shows a
/// panel about rather than acting on outright, since killing one throws its
/// work away where interrupting an agent lane does not.
///
/// `pub(crate)`: `spoolway queue pause` reads it for the same reason, and
/// refuses rather than showing a panel nobody is there to answer — see
/// `commands::queue::queue_pause`.
pub(crate) fn running_command_steps(
    repo: &Repo,
    tasks: &[crate::task::Task],
    pipelines: &Pipelines,
) -> Vec<CommandRunning> {
    let runs = crate::command_step::Runs::new(&repo.commands_dir());
    let mut out = Vec::new();
    for task in tasks {
        if task.stage() == crate::pipeline::PAUSED || task.stage() == crate::pipeline::BLOCKED {
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
/// `rows`, wrapping at either end. `None` only when there is nowhere to put
/// it — an empty board. Landing on the first row rather than nowhere both
/// when `current` is `None` — the cursor has never moved — and when it names
/// a task the board no longer shows, so a resort or a task's own state
/// change never strands the cursor on a row that is gone.
fn shift_cursor(rows: &[Row], current: Option<&str>, delta: i32) -> Option<String> {
    if rows.is_empty() {
        return None;
    }
    let next = match current.and_then(|id| rows.iter().position(|r| r.id == id)) {
        Some(at) => {
            let len = rows.len() as i32;
            (((at as i32 + delta) % len) + len) % len
        }
        None => 0,
    };
    Some(rows[next as usize].id.clone())
}

impl Default for Board {
    fn default() -> Board {
        Board::new()
    }
}

/// A row per task, whatever needs a person first.
///
/// The board's own reading of the queue, available to anything that wants the
/// same answer without the redraw — `spoolway queue list`, above all. Reads the
/// task files and the live lane list, and writes nothing.
pub fn rows(repo: &Repo, pipelines: &Pipelines) -> Result<Vec<Row>> {
    let tasks = repo.tasks()?;
    let graph = Graph::build_for_run(&tasks, pipelines, &repo.archive_dir(), repo.unattended());
    let waiting = crate::dispatch::lanes_awaiting_a_person(repo);
    let mux = crate::mux::backend(repo);
    let lanes = mux.list_lanes().unwrap_or_default();
    let ledger = crate::usage::read_cached(repo);
    build_rows(repo, &tasks, pipelines, &graph, &waiting, &lanes, &ledger)
}

// Every argument is a distinct piece of the board's own state that `frame`
// holds and this builds one frame from; bundling them into a struct just to
// pass one reference would hide that. The same call the codebase's other
// frame builders make.
#[allow(clippy::too_many_arguments)]
fn render(
    repo: &Repo,
    pipelines: &Pipelines,
    phase: Phase,
    watching: bool,
    stages: &mut BTreeMap<String, String>,
    recent: &mut VecDeque<RecentEvent>,
    cursor: Option<&str>,
    jobs_next: &mut Option<crate::jobs::StayingUpMemo>,
) -> Result<String> {
    let (tasks, load_problems) = repo.tasks_and_problems()?;
    let graph = Graph::build_for_run(&tasks, pipelines, &repo.archive_dir(), repo.unattended());
    let waiting = crate::dispatch::lanes_awaiting_a_person(repo);
    let mux = crate::mux::backend(repo);
    // Read once and passed down: this is a call out to the multiplexer, and the
    // board makes it about once a second already.
    let lanes = mux.list_lanes().unwrap_or_default();

    // The ticker sees the queue move: a stage that changed. A task entering
    // the queue is already a new row on the table above, and one archiving
    // just dims in place there — neither is a lane reporting anything, so
    // neither earns a line here too.
    let now = chrono::Local::now().format("%H:%M").to_string();
    let mut current: BTreeMap<String, String> = BTreeMap::new();
    for task in &tasks {
        current.insert(task.id().to_string(), task.stage().to_string());
    }
    for (id, stage) in &current {
        // Names the step behind the move, not the one arrived at: `stage` is
        // where the destination decides pass or fail, but `was` is the step
        // that actually reported — see `arrival_event`. Clipped by `ticker`
        // itself, the same as every other recent line, so a long id or step
        // name ends in `…` rather than wrapping.
        if let Some(was) = stages.get(id)
            && was != stage
        {
            push_recent(
                recent,
                arrival_event(&now, id, was, stage, &tasks, pipelines),
            );
        }
    }
    *stages = current;

    // Read once and shared: the rows want it for the OUT column and the footer
    // wants it for the spend, and it is the largest file the board opens.
    let ledger = crate::usage::read_cached(repo);
    let active_rows = build_rows(repo, &tasks, pipelines, &graph, &waiting, &lanes, &ledger)?;

    // Archived tasks stay on the board, dimmed, only as long as their group
    // still has something in the queue — so the groups worth pulling from the
    // archive are exactly the ones already among the active rows.
    let active_groups: BTreeSet<String> =
        active_rows.iter().filter_map(|r| r.group.clone()).collect();
    let mut rows = active_rows;
    rows.extend(done_rows(repo, pipelines, &active_groups)?);
    rows.sort_by(|a, b| a.key().cmp(&b.key()));
    let totals = group_totals(&ledger, &rows);

    // Slots: how many live lanes each profile is paying for, against its cap
    // — or, where the model a profile's lanes are running has `slots` of its
    // own, how many that model is paying for against its own cap instead.
    let SlotsUsed {
        agents: used,
        models: model_used,
        agent_model,
    } = slots_used(repo, &tasks, pipelines, &lanes);

    // ---- the frame ----
    let mut frame = String::new();
    // The dispatcher is whoever is drawing this — ourselves, when this board
    // is driving a run, or whoever the lock names, when it is only watching
    // one. No pass clock: `up` already ticks on every redraw, so a board that
    // has not changed in a while is visibly alive without a second clock
    // counting the other way — and when the next pass is due is not something
    // a person watching can do anything about.
    let mut header = match watching {
        // Read fresh every frame, never cached: a watcher that opened on a
        // live dispatcher and stays up after that dispatcher dies is exactly
        // what turns this into "no dispatcher is running" without a second
        // process having to notice and say so.
        true => match crate::lock::Lock::holder(&repo.lock_file())? {
            Some(pid) => vec!["watching dispatcher".to_string(), format!("pid {pid}")],
            None => vec!["no dispatcher is running".to_string()],
        },
        false => vec![
            match phase {
                Phase::Stopping => "dispatcher stopped".to_string(),
                _ => "dispatcher running".to_string(),
            },
            format!("pid {}", std::process::id()),
        ],
    };
    // Absent only if the lock file has gone missing under a live run, which is
    // a thing to leave out rather than a thing to print an empty figure for.
    if let Some(up) = run_elapsed(repo).map(human_secs) {
        header.push(format!("up {up}"));
    }
    let pane = pane_width();
    // One blank row before the lockup, so its ascenders have a margin to sit
    // in rather than landing flush on the pane's own top row. `masthead`
    // stays untouched: `init` prints its banner through the same function and
    // must not gain a line it never asked for.
    frame.push('\n');
    // The spool only turns while something on the board is actually running —
    // a lane or a command step — so a queue with nothing to do prints the
    // still mark rather than an animation with nothing behind it.
    let running = rows.iter().any(|row| row.state == State::Running);
    frame.push_str(&masthead(&header.join(" · "), pane, spool_frame(running)));
    frame.push('\n');

    if rows.is_empty() {
        // A cron job keeps the dispatcher resident on an empty queue, so the
        // board says why it is still up rather than "nothing queued" — the
        // same facts the plain run prints, in the board's own dim style.
        // Memoised: this redraws every second and the scan behind it is not
        // cheap per enabled job.
        let jobs = crate::jobs::staying_up_cached(repo, jobs_next);
        if jobs.enabled > 0 {
            for line in crate::jobs::staying_up_lines(&jobs) {
                frame.push_str(&format!(" {DIM}{line}{RESET}\n"));
            }
        } else {
            frame.push_str(&format!(" {DIM}nothing queued{RESET}\n"));
        }
    } else {
        frame.push_str(&table(&rows, Style::board(pane), &totals, cursor));
    }

    // A queue file that would not parse is skipped rather than freezing the
    // board — see [`crate::task::load_dir`] — and named here so the fix is
    // visible on the frame itself, not only in the log.
    for problem in &load_problems {
        let name = problem
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("a queue file");
        frame.push_str(&format!(
            " {AMBER}⚠ {name} does not parse and was skipped{RESET}\n"
        ));
    }

    // Built before the ticker although it is printed after it, because how
    // many rows are left for the ticker is what is left once this is counted.
    let mut tail = String::new();
    // The rule under the board, cut to the pane rather than wrapped: a rule
    // that wraps is two rules, and the second one lands where the footer goes.
    let rule = "─".repeat(60.min(pane.saturating_sub(1)));
    tail.push_str(&format!("\n {DIM}{rule}{RESET}\n"));
    let local_notice = queued_local_models(repo, &tasks, pipelines);
    for line in footer(
        repo,
        pipelines,
        &used,
        &model_used,
        &agent_model,
        &local_notice,
    ) {
        tail.push_str(&format!(" {line}\n"));
    }
    // The key hint, last of all — only for a board actually driving a run:
    // [`Board::watching`] never reads a key, and a hint under it would tell
    // somebody watching another process's board that pressing r or p does
    // something here. Nothing about the hint depends on whether any row can
    // use it right now; it says what the board can do, not what it would do
    // this frame.
    if !watching {
        tail.push_str(&format!(
            "\n {DIM}[↑↓] row{GUTTER}[o] open{GUTTER}[r/R] resume / all{GUTTER}[p/P] pause / all{GUTTER}\
             [u/U] unqueue / all{RESET}\n"
        ));
    }

    let height = pane_height();
    let rows = match height {
        // Nothing to overflow: keep the ticker whole. See [`pane_height`].
        None => recent.len() + 2,
        Some(height) => height
            .saturating_sub(1)
            .saturating_sub(frame.lines().count())
            .saturating_sub(tail.lines().count()),
    };
    frame.push_str(&ticker(recent, pane, rows));
    frame.push_str(&tail);

    Ok(clamp_rows(&frame, height))
}

/// How many live lanes each agent profile is paying for, keyed by profile name.
///
/// A lane maps to a profile through the step that started it. Three things have
/// to hold before a session in the multiplexer is one of those lanes, and all
/// three are the pass's own tests — [`crate::dispatch::Dispatcher::pass`] counts
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
        let parked = task.stage() == crate::pipeline::PAUSED
            || (task.stage() == crate::pipeline::BLOCKED
                && !pipeline.blocked_is_staffed(repo.unattended()));
        if parked {
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
    // every task's own pipeline, whatever stage that task sits on — the same
    // whole-pipeline walk [`queued_local_models`] already makes. A profile
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

/// The `local` models a task in the queue will run: any model an agent step
/// of a queued task's pipeline names and that `[models]` flags `local =
/// true`, deduped and in name order.
///
/// The whole pipeline's steps, not only the one a task sits on: a task that
/// will reach a local step later is already a reason to keep the card clear.
/// The footer draws one line per name this returns — the plain's own
/// `d-notice-not-gate`. A model configured `local` but named by no queued
/// task yields nothing here, which is why this project's board carries no
/// such line: every pipeline it ships names cloud models.
fn queued_local_models(
    repo: &Repo,
    tasks: &[crate::task::Task],
    pipelines: &Pipelines,
) -> Vec<String> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for task in tasks {
        let Ok(pipeline) = pipelines.for_task(task) else {
            continue;
        };
        for step in &pipeline.steps {
            if step.kind() != crate::pipeline::StepKind::Agent {
                continue;
            }
            let Some(model) = step.model.as_deref().filter(|m| !m.trim().is_empty()) else {
                continue;
            };
            if crate::models::resolve(&repo.config.models, model)
                .price
                .is_some_and(|price| price.local)
            {
                seen.insert(model.to_string());
            }
        }
    }
    seen.into_iter().collect()
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

/// What a row shows that the ledger does not hold yet: the spend of the step
/// in flight, which is banked only when that step settles.
///
/// Carried on the row rather than read a second time, because reading it is a
/// pass over a live transcript that [`build_rows`] has already made. A group's
/// total adds this to the ledger's own sum; without it a running lane's whole
/// bill is missing from the line that closes its group, and the total reads
/// smaller than the row right above it.
#[derive(Default)]
pub struct Unbanked {
    /// Output tokens the live lane has produced at this step, `0` where there
    /// is no live reading to take.
    pub out: u64,
    /// What those tokens have cost, `None` where nothing could be priced.
    pub cost: Option<f64>,
    /// How long the live lane has been open at this step. Unbanked for the
    /// same reason the rest is: the ledger learns a round's `wall_s` when the
    /// round ends.
    pub lane_time: Option<i64>,
}

/// Where a pass out of `blocked` would carry this task, prefixed for the NEXT
/// column — `cleared_block_target` itself, so the board can never name a
/// destination the dispatcher would not actually take it to. Read for both a
/// parked block and a staffed one: the row differs in state and colour, not
/// in where the arrow points.
fn blocked_next(
    repo: &Repo,
    task: &crate::task::Task,
    pipeline: &crate::pipeline::Pipeline,
) -> String {
    format!(
        "→ {}",
        crate::commands::cleared_block_target(
            task,
            pipeline,
            repo.config.unattended.skip_blocked_lane
        )
    )
}

/// Where letting a paused task past its gate would carry it, if the pipeline
/// still has an answer — `None` only for a task hand-edited onto `paused`
/// with no `paused_at` recorded, which is a step nothing here can guess.
///
/// A block reported as `--pause` is told apart from an ordinary gate by the
/// same `blocked_from` naming `paused_at` that `past_the_gate` reads it by,
/// and takes the same `cleared_block_target` a pass from `blocked` would —
/// an ordinary gate has no such thing to take over, so it reads its step's
/// own `on_pass` instead.
fn paused_next(
    repo: &Repo,
    task: &crate::task::Task,
    pipeline: &crate::pipeline::Pipeline,
) -> Option<String> {
    let gated = task.front.paused_at.as_deref()?;
    let step = pipeline.step(gated)?;
    if task.front.blocked_from.as_deref() == Some(gated) {
        Some(crate::commands::cleared_block_target(
            task,
            pipeline,
            repo.config.unattended.skip_blocked_lane,
        ))
    } else {
        step.destination(crate::pipeline::Outcome::Pass)
            .map(str::to_string)
    }
}

/// The rows themselves, from state already read. Split out so the board can
/// read the queue and the lane list once per frame and share both.
fn build_rows(
    repo: &Repo,
    tasks: &[crate::task::Task],
    pipelines: &Pipelines,
    graph: &Graph,
    waiting: &std::collections::BTreeSet<String>,
    lanes: &[crate::mux::Lane],
    ledger: &[crate::usage::Entry],
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
    // here prunes the cache at the end of the render.
    let mut live_sessions: HashSet<String> = HashSet::new();
    let mut rows: Vec<Row> = Vec::new();
    for task in tasks {
        let pipeline = pipelines.for_task(task)?;
        let step = pipeline.step(task.stage());
        // `(N/M)`: how many laps of *this* route the task has taken against
        // the route's own budget, read off the step it is on right now and
        // the route it arrived by — the same pair `apply_loop_budget`
        // compares before it lets a lap through. Computed once, ahead of the
        // match below, because it is a fact about the step a task sits on
        // and not about any one of the states that match branches out into.
        let step_loop = step.and_then(|step| {
            task.front.arrived_from.as_deref().and_then(|from| {
                step.round_limit(from)
                    .map(|limit| (task.rounds_via(from, &step.id), limit))
            })
        });
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

        // Mid-turn right now, as the multiplexer sees it this second.
        //
        // The board redraws about once a second and a pass runs every
        // `interval`, so between the two the lane list is the fresher answer
        // about a lane that has gone back to work — and when no dispatcher is
        // running at all it is the only one. Only `Working` counts:
        // `LaneStatus::Blocked` is busy too and means a lane waiting on a
        // person, which is the state below, and `Unknown` is the multiplexer
        // declining to say rather than saying no.
        let working = live_lane.is_some_and(|l| l.status == crate::mux::LaneStatus::Working);

        // Parked in front of a person, rather than a lane spoolway is about to
        // start there — the one question that decides whether a task on
        // `blocked` reads as a row like any other running step or as a wait.
        let parked_on_blocked = task.stage() == crate::pipeline::BLOCKED
            && !pipeline.blocked_is_staffed(repo.unattended());

        // A park recorded on the task's own file — nothing in this binary
        // writes one any more, but a row here still honours one already on
        // disk. `park-lifecycle` stamps `parked_at` once when a
        // continuous hold begins and clears it only on a true exit — a
        // launch, a stage move, a re-queue — so it stays set across a
        // re-probe whose `parked_until` deadline has already run out. Reading
        // the hold off `parked_at`, rather than off `parked_until > now`,
        // keeps the row on `Parked` through that gap instead of flashing back
        // to `running` for the one frame between the deadline passing and the
        // next pass re-parking — the transient unpark frame this task
        // removes. A legacy park written before `parked_at` existed has only
        // the deadline to go on, and no age to show.
        let now = chrono::Utc::now().timestamp();
        let parked_at = task.front.parked_at;
        let parked_hold =
            parked_at.is_some() || task.front.parked_until.is_some_and(|until| until > now);

        // What happens to this task next. For one that is moving that is the
        // step it goes to; for one that is stuck it is whatever has to happen
        // before it moves at all, which is a person far more often than a step.
        let (state, next, resumable) = match step {
            // The dispatcher's own states. `queued` names the dependency it is
            // held by, which is the only thing worth saying about a task there
            // — a fixed description said the same thing at every one of them.
            _ if task.stage() == crate::pipeline::QUEUED => {
                let dependency = crate::commands::dependency_note(graph, task.id());
                // Asked of the graph, never read back out of the note's own
                // English. This used to substring-match the rendered
                // sentence — `starts_with("unreachable")`,
                // `contains("cycle")`, `contains("itself")` — which made
                // every task id containing one of those words its own bug
                // report: `waiting on: park-lifecycle` contains "cycle", so
                // an ordinary unmet dependency was drawn in red as a
                // dependency cycle. The graph is asked the same two
                // questions `dependency_note` asks it, and answers about the
                // shape of the queue rather than about the spelling of a
                // task's name.
                let state = match graph.cycle_with(task.id()).is_some()
                    || graph.unreachable(task.id()).is_some()
                {
                    true => State::Unreachable,
                    false => State::Queued,
                };
                // The gate only ranks a candidate now, it does not drop one
                // — so a task it has ranked behind another group is not
                // held apart from every other task waiting on a worker
                // slot, and reads the same line the rest of them do.
                //
                // A park outranks that plain queued read, the same way it
                // does on a real step below. A row that read `queued ·
                // waiting for a worker slot` right through a park was
                // describing a dispatcher that had stopped, which is the one
                // thing it was not doing.
                //
                // Only when nothing else holds it. A dependency it is still
                // waiting on is the harder hold and the more useful thing to
                // say, so that wording keeps the row whether or not a park
                // is also running.
                match dependency {
                    Some(d) => (state, d, false),
                    None if parked_hold => {
                        (State::Parked, format!("→ {}", pipeline.entry()), false)
                    }
                    None => (
                        state,
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
                    blocked_next(repo, task, pipeline),
                    resumable,
                )
            }
            // The step a pass would carry it to, not a description of what it
            // is waiting on — the same rule a block reads its resumability
            // by, on exactly the same two conditions.
            None if task.stage() == crate::pipeline::PAUSED => {
                let resumable = graph.ready(task.id()) && !lane_busy(lanes, &step_ids, task.id());
                let target = paused_next(repo, task, pipeline);
                let next = match (target, resumable) {
                    (Some(step), true) => format!("→ {step} — [r] resumes it"),
                    (Some(step), false) => {
                        format!("→ {step} — `spoolway resume {}`", task.id())
                    }
                    (None, true) => "[r] resumes it".to_string(),
                    (None, false) => format!("`spoolway resume {}`", task.id()),
                };
                (State::Paused, next, resumable)
            }
            None if task.stage() == crate::pipeline::DONE => {
                (State::Queued, "finished".to_string(), false)
            }
            None => (
                State::Blocked,
                format!("unknown step `{}` — not in this pipeline", task.stage()),
                false,
            ),
            // Parked ahead of the ordinary running/queued read below: a
            // parked task is still sitting on a real step, which would
            // otherwise read as `Running` (a live lane) or `Queued` (no lane
            // yet) with nothing on the row saying why nothing is happening.
            Some(step) if parked_hold => {
                let next = match pipeline.next_running_step(&step.id) {
                    Some(next) => format!("→ {next}"),
                    None => "→ done".to_string(),
                };
                (State::Parked, next, false)
            }
            // A pane holding a question, unless the lane in it is visibly
            // working — in which case the question was answered, or the lane
            // only looked settled for a moment between turns, and the row
            // reads as the running step it is. `Paused`, the same state a
            // gated task on `paused` gets: both ask the reader to intervene,
            // and NEXT carries the difference — the pane to look at here, the
            // resume there. Resumable like a gate too, though `r` cannot
            // answer the question for a person: it restarts the step the
            // pane never got an answer on, exactly as `resume_task` does for
            // any other row this key fires on — see its own comment for why
            // that fallback lands there rather than doing nothing.
            Some(_) if waiting.contains(&lane) && !working => (
                State::Paused,
                format!("look at pane `{lane}` — [r] resumes it"),
                true,
            ),
            Some(step) => {
                let state = match live || command_run.is_some() {
                    true => State::Running,
                    false => State::Queued,
                };
                // One step ahead and no further. The whole remaining chain is a
                // fact about the pipeline, which does not change and is one
                // `spoolway pipeline show` away; what moves — and so what is
                // worth a column that redraws every second — is where this
                // task goes when the step it is on passes.
                //
                // A gated step used to read its lane's question out of
                // `## Blocker` here. There is no question: a gated lane works
                // and reports like any other, and the waiting happens after it,
                // on `paused`, which has a row of its own above.
                let next = if step.id == crate::pipeline::BLOCKED {
                    // `blocked` names no `on_pass` of its own, and its
                    // resumability is a person's question, not a lane's — a
                    // staffed lane working it right now reads the same
                    // destination a cleared block would, and nothing else:
                    // acceptance criterion 1.
                    blocked_next(repo, task, pipeline)
                } else {
                    match pipeline.next_running_step(&step.id) {
                        // Plain text, no colour: this string is clipped to the
                        // room the pane has left, and a cut through an escape
                        // sequence dyes the rest of the board. No loop count
                        // here any more — see `step_loop`, above, which reads
                        // the same pair against the step this task is *on*
                        // rather than the one named here.
                        Some(next) => format!("→ {next}"),
                        None => "→ done".to_string(),
                    }
                };
                (state, next, false)
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
            parallel: task.front.parallel,
            stage: task.stage().to_string(),
            step_loop,
            pipeline: pipeline.name.clone(),
            state,
            // The park's elapsed age, `now - parked_at`, drawn onto the STATE
            // cell as `● parked · 8m` — how long this has been held, not
            // when the next probe falls. `parked_at` is fixed for the whole
            // continuous hold, so this counts up smoothly across every
            // re-probe rather than jumping when a deadline moves. Only on a
            // row that actually reads `Parked`, and only when `parked_at` is
            // set: a legacy park has no start to count from, so it supplies
            // no age. Its separate unavailable-reading reason, when present,
            // remains visible below.
            parked_display: (state == State::Parked)
                .then(|| parked_at.map(|at| view::park_age((now - at).max(0))))
                .flatten(),
            // Keep an unavailable reading visible without putting prose in
            // the additive JSON age field. `parked_window == "unknown"` was
            // the spelling the deleted quota gate wrote for a probe it could
            // not read; nothing writes it any more, but a park already on a
            // task file from before this shipped still carries it, and this
            // still preserves its diagnosis for a hold whose missing
            // `parked_at` leaves no age to draw beside it.
            parked_reason: (state == State::Parked && task.front.parked_window == "unknown")
                .then(|| "quota unavailable".to_string()),
            // Keep the pre-existing JSON clock independent of the board's
            // age: consumers of `parked_until` must not silently receive a
            // duration with different semantics under the old field name. An
            // `unknown`-window park still names no reset time here either —
            // `parked_until` was the deleted quota gate's own retry deadline
            // in that case, drifting outward every pass it rechecked, and
            // printing it would put back the exact clock this task's
            // `parked_display` fix removed from the board (a board reading
            // 22:05, 22:13 and 22:29 in one evening while usage never
            // moved), just under the JSON field instead.
            parked_until_display: task.front.parked_until.filter(|&until| until > now).map(
                |until| {
                    if task.front.parked_window == "unknown" {
                        return "quota unavailable".to_string();
                    }
                    let dated = task.front.parked_window == "seven_day";
                    crate::task::format_instant(until, now, dated)
                },
            ),
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
                None => spent_at(ledger, task.id(), task.stage()),
            },
            // Same two sources as OUT, in the same order and for the same
            // reason: a running step is banked only once it settles, by which
            // time the task has moved on.
            cost: match &session {
                Some(session) => session.cost,
                None => cost_at(ledger, task.id(), task.stage()),
            },
            // Live goes by whether the lane itself is open, not by whether a
            // transcript could be read from it — a lane that has just
            // launched is live before it has written a byte, and its clock
            // still runs.
            // A command run's clock comes off its own pid file rather than
            // `launched_at`, which is the last *lane*'s launch and would date
            // a running `gate` to whatever agent step ran before it. It stays
            // out of `unbanked` below: the ledger records lanes, so a
            // command's time is in no total either before or after it ends,
            // and adding it here alone would have a group's total shrink the
            // moment the command finished.
            lane_time: match (command_run, live) {
                (Some(key), _) => runs.elapsed(key).map(|ran| ran.as_secs() as i64),
                (None, true) => elapsed,
                (None, false) => lane_time_at(ledger, task.id(), task.stage()),
            },
            // Exactly the figures above that came off the live lane rather
            // than the ledger — a row falling back to the ledger has nothing
            // here, because the ledger's own sum already holds it.
            unbanked: Unbanked {
                out: session.map(|session| session.output).unwrap_or(0),
                cost: session.and_then(|session| session.cost),
                lane_time: elapsed,
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
            && lane.status.is_busy()
    })
}

/// What [`cached_archive`] last read, and the archive directory's own mtime
/// at that read — the one signal that changes when a task is filed into it,
/// whatever the platform.
struct ArchiveCache {
    dir: PathBuf,
    dir_mtime: SystemTime,
    tasks: Arc<Vec<crate::task::Task>>,
}

/// [`crate::task::load_dir`] over an archive directory, cached process-wide
/// and re-read only once the directory's own mtime moves.
///
/// A directory's mtime changes exactly when an entry is added to or removed
/// from it — a POSIX guarantee independent of any one file's own content —
/// and an archived task file is never rewritten in place once filed: nothing
/// deletes from `archive/` and nothing edits a task already there, so that
/// one signal is enough to know a re-read is worth its cost. Parsing every
/// file in a 1500-task archive on every one-second frame was the board's own
/// comment calling itself the most expensive thing in a pass that decides
/// nothing; this is the fix, in the same shape [`crate::usage::read_cached`]
/// already takes for the ledger beside it.
///
/// Returns a shared handle rather than an owned `Vec`, for the same reason
/// `read_cached` does: a warm cache the directory's mtime says is still
/// current hands back a clone of the `Arc` — a refcount bump — never a copy
/// of however many tasks are archived. Only the pass that finds the mtime
/// has moved pays to re-read the directory, and that is proportional to what
/// is actually in it, not paid again by every idle frame after.
fn cached_archive(dir: &Path) -> Result<Arc<Vec<crate::task::Task>>> {
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
        let tasks = Arc::new(crate::task::load_dir(dir)?.0);
        if let Some(dir_mtime) = dir_mtime {
            *guard = Some(ArchiveCache {
                dir: dir.to_path_buf(),
                dir_mtime,
                tasks: Arc::clone(&tasks),
            });
        }
        return Ok(tasks);
    }
    Ok(Arc::new(crate::task::load_dir(dir)?.0))
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
        .filter_map(|task| {
            let group = task.front.group.clone()?;
            if !active_groups.contains(&group) {
                return None;
            }
            Some(Row {
                id: task.id().to_string(),
                group: Some(group),
                issue_url: issue_url_of(task),
                parallel: task.front.parallel,
                stage: task.stage().to_string(),
                // An archived task has no live step to read a loop count off
                // — its row is history, not a lap in progress.
                step_loop: None,
                pipeline: archived_pipeline_name(pipelines, task.front.pipeline.as_deref()),
                state: State::Done,
                parked_display: None,
                parked_reason: None,
                parked_until_display: None,
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
                unbanked: Unbanked::default(),
                next: String::new(),
                resumable: false,
            })
        })
        .collect())
}

/// The name [`done_rows`] prints in its PIPELINE column: the pipeline an
/// archived task named, resolved the same way [`Pipelines::for_task`] would
/// — or the project's default where it named none — but never an error.
///
/// A live task's pipeline is read through `for_task`, which fails the whole
/// row-building pass if the name it carries is not one this project defines
/// any more — right for a task still running, since a pipeline it cannot be
/// read against is a project misconfigured, not a row that can be drawn
/// wrong. An archived task already finished under whatever pipeline it named,
/// possibly a run or two ago, so the same name going missing since is not a
/// misconfiguration to fail on — it is only a name nothing here can resolve,
/// and the row still has to print *something*, so it prints that name
/// verbatim instead.
fn archived_pipeline_name(pipelines: &Pipelines, declared: Option<&str>) -> String {
    match declared {
        Some(name) => pipelines
            .get(name)
            .map(|p| p.name.clone())
            .unwrap_or_else(|_| name.to_string()),
        None => pipelines.default.clone(),
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
    repo: &Repo,
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
    // Prices are linear in tokens, so pricing the delta is the same as
    // differencing two priced totals — and it stays right when the agent
    // reports its own cost instead.
    let cost = match harvest.cost_usd {
        Some(total) => Some((total - banked_cost).max(0.0)),
        None => crate::usage::price(&repo.config.models, &harvest.model, &unbanked),
    };
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
    let (kind, session) = crate::dispatch::lane_session(repo, lane)?;
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

    let mtime = crate::usage::touched_at(&path);
    if mtime.is_some() && mtime == cached.mtime {
        return cached.reading;
    }
    cached.mtime = mtime;
    cached.reading = crate::usage::live_of(&kind, &path).map(|live| {
        let (output, cost) = live_spend(repo, ledger, &session, &live.harvest);
        Reading {
            context: live.context,
            output,
            cost,
        }
    });
    cached.reading
}

/// Drop every [`live_session`] cache entry whose session no longer backs a
/// live lane, called once at the end of each render. Entries are small, but
/// nothing else ever removed one, so a dispatcher left up for weeks
/// accumulated one per lane it had ever drawn (review finding 54).
fn forget_dead_live_sessions(live: &HashSet<String>) {
    if let Some(cache) = LIVE_SESSION_CACHE.get()
        && let Ok(mut cache) = cache.lock()
    {
        cache.retain(|session, _| live.contains(session));
    }
}

/// A RECENT event for a task that just moved from `was` to `stage`.
///
/// `paused` and `blocked` are not steps a pipeline declares — they are
/// dispatcher states no `on_pass`/`on_fail` ever names, so the step worth
/// naming and scoring is not `was` but the *real* step behind the wait:
/// `paused_at`, the gate that passed, or `blocked_from`, the step the task
/// stopped on and will return to — exactly the two fields `spoolway resume`
/// reads to tell one stop from the other. Everywhere else, `was` is the step
/// that reported, and its own [`Step::destination`] against `stage` is what
/// tells a pass from a fail — see [`Verdict`].
///
/// Both the step name and the position are worked out here, once, because
/// they are facts about the pipeline the task is on at the moment it
/// arrives; [`view::ticker`] only turns the result into a line, against the pane
/// and the block around it.
///
/// [`Step::destination`]: crate::pipeline::Step::destination
fn arrival_event(
    now: &str,
    id: &str,
    was: &str,
    stage: &str,
    tasks: &[crate::task::Task],
    pipelines: &Pipelines,
) -> RecentEvent {
    let task = tasks.iter().find(|t| t.id() == id);
    let pipeline = task.and_then(|t| pipelines.for_task(t).ok());

    let (named, verdict) = match stage {
        crate::pipeline::PAUSED => (
            task.and_then(|t| t.front.paused_at.clone()),
            Verdict::Paused,
        ),
        crate::pipeline::BLOCKED => (
            task.and_then(|t| t.front.blocked_from.clone()),
            Verdict::Blocked,
        ),
        _ => {
            let verdict = match pipeline.and_then(|p| p.step(was)) {
                Some(step) if step.destination(Outcome::Pass) == Some(stage) => Verdict::Pass,
                Some(step) if step.destination(Outcome::Fail) == Some(stage) => Verdict::Fail,
                _ => Verdict::None,
            };
            (Some(was.to_string()), verdict)
        }
    };

    // Falls back to the stage itself when there is no step to name — a
    // paused/blocked task with no `paused_at`/`blocked_from` recorded, which
    // `spoolway resume` never leaves behind but an edited task file could.
    let step = named.clone().unwrap_or_else(|| stage.to_string());
    // The named step's own position in its pipeline's walk: its index plus
    // one — the steps up to and including it — over that plus the length of
    // `pass_chain` from it — the steps still ahead. `None` wherever the
    // named step or its pipeline can't be resolved, which `ticker` draws as
    // `—` rather than a wrong number.
    let position = named.zip(pipeline).and_then(|(step, pipeline)| {
        let numerator = pipeline.steps.iter().position(|s| s.id == step)? + 1;
        let denominator = numerator + pipeline.pass_chain(&step).len();
        Some((numerator, denominator))
    });

    RecentEvent::Arrival {
        at: now.to_string(),
        id: id.to_string(),
        step,
        verdict,
        position,
    }
}

/// Pushes one event onto the ticker's memory, keeping it to [`RECENT`] long.
///
/// An arrival first drops any earlier arrival for the same task already
/// sitting in `recent`: two moves of the same task inside the window are one
/// row, not two, and the later move is the one worth a task's single slot.
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

/// How long the run has been going, in whole seconds, from the same clock.
fn run_elapsed(repo: &Repo) -> Option<i64> {
    let modified = std::fs::metadata(repo.lock_file())
        .and_then(|meta| meta.modified())
        .ok()?;
    modified.elapsed().ok().map(|d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::testutil::*;
    use crate::status::view::{GUTTER, OSC8, RecentEvent, ST, Verdict, ticker};

    /// The footer's slot count is what says whether more work can start, so it
    /// has to count the lanes the dispatcher counts and nothing else. A
    /// multiplexer holds a person's own sessions too, and a lane name carries
    /// a `<task> · <step>` separator no task or step id can hold: a session
    /// in a worktree cut from `fix/herdr-layout` is named after the branch,
    /// and parses as no lane at all. Counting those had a run with one lane
    /// in flight reporting `cloud slots 3/5`.
    #[test]
    fn only_lanes_in_our_own_checkouts_take_a_slot() {
        let repo = fixture("slots-ownership");
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
        let mut repo = fixture("queued-agent-model");
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
        let repo = fixture("queued-agent-model-two-pools");
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
            default: "impl_ui".to_string(),
            pipelines: [("impl_ui".to_string(), pipeline)].into_iter().collect(),
        };
        add(&repo, "login", &[], Some("implement"));
        let tasks = repo.tasks().unwrap();

        let used = slots_used(&repo, &tasks, &pipelines, &[]);

        assert_eq!(
            used.agent_model.get("pi").cloned(),
            Some(vec!["ornith/Ornith-1.5-35B-A3B", "qwen/Qwen3.6-35B-A3B"]),
            "{:#?}",
            used.agent_model
        );
    }

    /// A queued task whose pipeline names a `local` model puts that model's
    /// name on the list the footer draws a line from; a model in `[models]`
    /// that has not set `local` puts nothing there, however it is sized.
    #[test]
    fn queued_local_models_names_a_local_model_a_queued_task_will_run() {
        let mut repo = fixture("queued-local-models");
        add(&repo, "login", &[], Some("implement"));
        let mut pipelines = Pipelines::builtin();
        let tasks = repo.tasks().unwrap();

        // Fresh shipped steps are blank. Give this fixture the local model a
        // configured project would have chosen before asking what the footer
        // reports about it.
        pipelines
            .pipelines
            .get_mut("default")
            .unwrap()
            .steps
            .iter_mut()
            .find(|step| step.id == "implement")
            .unwrap()
            .model = Some(crate::models::PLACEHOLDER.to_string());
        repo.config.models.insert(
            crate::models::PLACEHOLDER.to_string(),
            crate::usage::ModelPrice {
                local: true,
                ..Default::default()
            },
        );
        assert_eq!(
            queued_local_models(&repo, &tasks, &pipelines),
            vec![crate::models::PLACEHOLDER.to_string()]
        );

        // Sized but silent on `local`: nothing to warn about.
        repo.config.models.insert(
            crate::models::PLACEHOLDER.to_string(),
            crate::usage::ModelPrice {
                slots: 3,
                exclusive: true,
                ..Default::default()
            },
        );
        assert!(queued_local_models(&repo, &tasks, &pipelines).is_empty());
    }

    /// A parked task's pane is kept open for a person to read, and the
    /// dispatcher hands the slot back the moment it does that. Counted here it
    /// would read as a profile with nothing running in it — the board saying no
    /// work can start while the dispatcher happily starts some.
    #[test]
    fn a_parked_lane_gives_its_slot_back() {
        let repo = fixture("slots-parked");
        let pipelines = Pipelines::builtin();
        for (id, stage) in [
            ("blocked-one", crate::pipeline::BLOCKED),
            ("paused-one", crate::pipeline::PAUSED),
        ] {
            add(&repo, id, &[], Some(stage));
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
        let mut repo = fixture("slots-staffed-blocked");
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

    /// The whole point of the column: a task that is moving is described by
    /// where it goes, and a task that is stuck by what is holding it. Also
    /// where a blocked row, a paused row and a looping row's `step_loop` are
    /// checked, alongside the plain moving and stuck cases, since all of
    /// them share this one column — the loop count itself belongs to the
    /// STEP column now, so it is read off `Row::step_loop`, not off `next`.
    #[test]
    fn the_next_column_names_a_step_for_a_moving_task_and_a_reason_for_a_stuck_one() {
        let repo = fixture("next-column");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        add(&repo, "sessions", &["login"], None);

        // Blocked, parked for a person: the step a pass out of `blocked`
        // would actually carry it to — `cleared_block_target`'s own answer —
        // and nothing else. No blocker reason, no resumability hint: the
        // state dot already says this is a block, and the key-hint line
        // already says whether `r` does anything.
        add(&repo, "wall", &[], None);
        let mut wall = repo.task("wall").unwrap();
        wall.front.blocked_from = Some("implement".into());
        wall.append_to_section("## Blocker", "waiting on a person\n");
        wall.set_stage(crate::pipeline::BLOCKED, None);
        wall.save().unwrap();

        // Paused at a gate: the step passing it would carry the task to,
        // with the resume hint after it.
        add(&repo, "ship", &[], None);
        let mut ship = repo.task("ship").unwrap();
        ship.front.paused_at = Some("implement".into());
        ship.set_stage(crate::pipeline::PAUSED, None);
        ship.save().unwrap();

        // A second lap of `implement -> review`: the shipped default
        // pipeline bounds that route at 2, so the third round would spend
        // it — this is the last lap before the route escalates.
        add(&repo, "spinner", &[], Some("review"));
        let mut spinner = repo.task("spinner").unwrap();
        spinner.front.arrived_from = Some("implement".into());
        spinner
            .front
            .rounds
            .insert(crate::task::route_key("implement", "review"), 2);
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

        assert_eq!(row("wall").next, "→ review");
        assert_eq!(row("ship").next, "→ review — [r] resumes it");
        // No loop text on NEXT at all — it moved to the STEP column, read
        // off `step_loop` instead, on the last lap before the route
        // escalates.
        assert_eq!(row("spinner").next, "→ document");
        assert_eq!(row("spinner").step_loop, Some((2, 2)));
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
        let repo = fixture("gate-note");
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

    /// The STEP counter shows from the first arrival — there is no floor any
    /// more, unlike the NEXT suffix it replaced — but only where the step
    /// itself declares a `loop:` for the route named in `arrived_from`. A
    /// route nothing bounds has no ceiling to read a lap count against,
    /// however many rounds are on file, so it stays a bare step id.
    #[test]
    fn the_step_counter_shows_from_first_arrival_and_is_omitted_on_an_unbounded_route() {
        let repo = fixture("loop-suffix-floor");
        let pipelines = Pipelines::builtin();

        // One lap of a bounded route: shown, unlike the old NEXT suffix,
        // which said nothing below two laps.
        add(&repo, "first-lap", &[], Some("review"));
        let mut first_lap = repo.task("first-lap").unwrap();
        first_lap.front.arrived_from = Some("implement".into());
        first_lap
            .front
            .rounds
            .insert(crate::task::route_key("implement", "review"), 1);
        first_lap.save().unwrap();

        // Arrived at `implement` from `review` — a route the shipped
        // pipeline's `implement` step names no `loop:` limit for — so there
        // is no budget to read a lap count against, however many rounds are
        // on file.
        add(&repo, "unbounded", &[], Some("implement"));
        let mut unbounded = repo.task("unbounded").unwrap();
        unbounded.front.arrived_from = Some("review".into());
        unbounded
            .front
            .rounds
            .insert(crate::task::route_key("review", "implement"), 5);
        unbounded.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap();

        assert_eq!(row("first-lap").next, "→ document");
        assert_eq!(row("first-lap").step_loop, Some((1, 2)));
        assert_eq!(row("unbounded").next, "→ review");
        assert_eq!(row("unbounded").step_loop, None);
    }

    /// The frame carries the run's own header, because whose process this is
    /// and how long it has been going are the two things the task files cannot
    /// say. What it no longer carries is a pass clock: a person watching cannot
    /// act on when the next pass is due, and `up` already proves the board is
    /// alive by moving on every redraw.
    #[test]
    fn a_frame_carries_the_run_but_no_pass_clock() {
        let repo = fixture("frame-header");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let mut board = Board::for_test();
        let waiting = board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert!(waiting.contains("dispatcher running · "), "{waiting}");
        assert!(waiting.contains("→ review"), "{waiting}");
        assert!(!waiting.contains("next pass"), "{waiting}");

        let working = board.frame(&repo, &pipelines, Phase::Passing).unwrap();
        assert!(!working.contains("pass running"), "{working}");
        assert!(!working.contains("queue empty"), "{working}");

        // The one phase a frame in front of you cannot be read off the frame.
        let over = board.frame(&repo, &pipelines, Phase::Stopping).unwrap();
        assert!(over.contains("dispatcher stopped · "), "{over}");
    }

    /// An empty queue with a cron job enabled is why a dispatcher is still
    /// resident, so the board says so — the same queue-empty / job-count /
    /// next-fire facts the plain run prints — rather than "nothing queued".
    #[test]
    fn an_empty_queue_with_a_job_enabled_says_the_job_is_holding_the_run_up() {
        let repo = fixture("board-jobs-resident");
        let pipelines = Pipelines::builtin();
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::write(
            repo.user_jobs_file(),
            "[jobs.nightly]\nschedule = \"0 3 * * *\"\npipeline = \"default\"\nroutine = \"nightly\"\n",
        )
        .unwrap();

        let mut board = Board::for_test();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("1 job enabled"), "{frame}");
        assert!(frame.contains("staying up"), "{frame}");
        assert!(frame.contains("next: nightly,"), "{frame}");
        assert!(!frame.contains("nothing queued"), "{frame}");
    }

    /// A task's `url:` frontmatter reaches the group band as a real OSC 8
    /// hyperlink in the frame the board hands back — the whole path from the
    /// saved document through `Row::issue_url` into the painted bytes, not
    /// just `view::table` exercised in isolation. Stripped, the band is still
    /// the plain heading a group search matches.
    #[test]
    fn a_saved_task_url_becomes_the_group_bands_hyperlink_in_the_frame() {
        let repo = fixture("band-url-frame");
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
        let frame = board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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

    /// A driving board says what the keys it now reads do — the mockup's own
    /// last line — but a board only watching someone else's run never reads
    /// one, so the hint would be a lie there and is left off.
    #[test]
    fn a_driving_board_carries_the_key_hint_but_a_watching_one_does_not() {
        let repo = fixture("key-hint");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let mut driving = Board::for_test();
        let frame = strip(&driving.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(
            frame.contains(
                "[↑↓] row   [o] open   [r/R] resume / all   [p/P] pause / all   [u/U] unqueue / all"
            ),
            "{frame}"
        );

        let mut watching = Board::watching_for_test();
        let frame = strip(&watching.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(!frame.contains("[r/R] resume / all"), "{frame}");
    }

    /// The lane list wins over the mark when the two disagree.
    ///
    /// A lane that settled once carries the mark until a pass takes it back,
    /// and a pass runs every `interval` while the board redraws every couple
    /// of seconds. In between — and for good, with no dispatcher running —
    /// the board would tell a person to go and answer a pane with an agent
    /// visibly mid-turn in it. A working lane reads as the running step it is,
    /// and settles back to `paused` the moment it stops.
    #[test]
    fn a_marked_lane_that_is_working_reads_as_running() {
        let repo = fixture("waiting-but-working");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let waiting = BTreeSet::from(["login · implement".to_string()]);

        let mut working = lane("login · implement", &repo.root);
        working.status = crate::mux::LaneStatus::Working;
        let rows =
            build_rows(&repo, &tasks, &pipelines, &graph, &waiting, &[working], &[]).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Running), "{}", row.next);

        // Quiet again: the pane really is holding the question. The row reads
        // `paused`, the same state a gated task gets, and NEXT names both the
        // pane to look at and the same `[r]` a gate would offer.
        let mut settled = lane("login · implement", &repo.root);
        settled.status = crate::mux::LaneStatus::Done;
        let rows =
            build_rows(&repo, &tasks, &pipelines, &graph, &waiting, &[settled], &[]).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Paused));
        assert!(row.resumable);
        assert_eq!(
            row.next,
            "look at pane `login · implement` — [r] resumes it"
        );
    }

    /// A task the dispatcher parked for its quota reads `Parked` on the
    /// board, with the park's elapsed age — `now - parked_at` — drawn on the
    /// STATE cell itself. `queue list` and the board share this one table, so
    /// both read it the same way. A `parked_until` already in the past does
    /// not end the hold: `parked_at` is what the row is read off, so it still
    /// reads `Parked` with a growing age until the dispatcher resolves it.
    #[test]
    fn a_task_parked_for_its_quota_reads_parked_with_its_elapsed_age() {
        let repo = fixture("quota-parked-row");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("review"));
        let mut task = repo.task("login").unwrap();
        // An hour-plus age so `park_age` is stable across the sub-second
        // gap between this save and the assertion — its `h` branch drops the
        // seconds, where a bare-minutes age would tick mid-test.
        task.front.parked_at = Some(chrono::Utc::now().timestamp() - 3840);
        // The recheck deadline has already run out; the hold has not.
        task.front.parked_until = Some(chrono::Utc::now().timestamp() - 60);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[],
            &[],
        )
        .unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Parked), "{}", row.next);
        let display = row
            .parked_display
            .as_deref()
            .expect("a parked row carries its elapsed age");
        assert_eq!(display, "1h 04m");
        assert_eq!(
            row.parked_until_display, None,
            "the compatible JSON clock expires even while parked_at keeps the hold visible"
        );
        assert!(view::plain_table(&rows).contains("parked · 1h 04m"));
    }

    /// A park written by an older spoolway carries `parked_until` but no
    /// `parked_at`. The row still reads `Parked` off the future deadline, but
    /// there is no start to count an age from, so the STATE cell is a bare
    /// `● parked` with nothing invented after it.
    #[test]
    fn a_legacy_park_with_no_parked_at_shows_no_age() {
        let repo = fixture("legacy-park-row");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("review"));
        let mut task = repo.task("login").unwrap();
        let until = chrono::Utc::now().timestamp() + 3600;
        task.front.parked_until = Some(until);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[],
            &[],
        )
        .unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Parked), "{}", row.next);
        assert_eq!(row.parked_display, None);
        assert_eq!(
            row.parked_until_display,
            Some(crate::task::format_instant(
                until,
                chrono::Utc::now().timestamp(),
                false
            )),
            "legacy JSON consumers keep receiving the recheck clock"
        );
        let table = view::plain_table(&rows);
        assert!(table.contains("● parked"), "{table}");
        assert!(!table.contains("parked · "), "{table}");
    }

    /// A park with no real window to name carries its diagnosis on
    /// `parked_reason`, not on the age itself. `parked_window = "unknown"`
    /// with `parked_until` pushed out by a retry backoff is the shape the
    /// deleted quota gate wrote when it could not produce a reading at all —
    /// nothing writes it now, but a park already on a task file from before
    /// this shipped may still carry it. That instant was a retry deadline,
    /// not a quota reset, and drifted outward every pass it rechecked, so
    /// the row must not draw it in the position a real window's reset time
    /// occupies — and this task has no `parked_at` either, so
    /// `parked_display` invents no age for it.
    ///
    /// The compatible JSON clock, `parked_until_display`, must not leak that
    /// drifting deadline back out either: it is the same bare `quota
    /// unavailable` a board reading it would draw, not `quota unavailable ·
    /// <clock>` — printing the clock there would reintroduce, under the old
    /// field's name, the exact "22:05, 22:13, 22:29 in one evening while
    /// usage never moved" defect this task's `parked_display` fix removed
    /// from the board.
    #[test]
    fn an_unknown_window_park_prints_quota_unavailable_with_no_clock() {
        let repo = fixture("quota-unknown-window-no-clock");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("review"));
        let mut task = repo.task("login").unwrap();
        task.front.parked_until = Some(chrono::Utc::now().timestamp() + 3600);
        task.front.parked_window = "unknown".to_string();
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[],
            &[],
        )
        .unwrap();

        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Parked), "{}", row.next);
        assert_eq!(
            row.parked_reason.as_deref(),
            Some("quota unavailable"),
            "an `unknown`-window park names no reset time, but still says why it is held"
        );
        assert_eq!(
            row.parked_until_display.as_deref(),
            Some("quota unavailable"),
            "the compatible JSON clock names no reset time either, with no drifting deadline appended"
        );
        assert_eq!(
            row.parked_display, None,
            "no `parked_at` means no invented age"
        );
    }

    /// A dependency whose id happens to contain one of the words the state
    /// used to be sniffed out of is still an ordinary dependency.
    ///
    /// The `queued` state was decided by substring-matching the rendered
    /// note: `contains("cycle")` over `waiting on: park-lifecycle` is true,
    /// so a task waiting on a perfectly healthy dependency was drawn in red
    /// as one caught in a dependency cycle. Real trouble is a question for
    /// the graph, and this asks it there.
    #[test]
    fn a_dependency_named_for_a_lifecycle_is_not_a_dependency_cycle() {
        let repo = fixture("cycle-in-the-name");
        let pipelines = Pipelines::builtin();
        add(&repo, "park-lifecycle", &[], Some(crate::pipeline::QUEUED));
        add(
            &repo,
            "paused-board",
            &["park-lifecycle"],
            Some(crate::pipeline::QUEUED),
        );

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[],
            &[],
        )
        .unwrap();

        let row = rows.iter().find(|r| r.id == "paused-board").unwrap();
        assert!(
            matches!(row.state, State::Queued),
            "an unmet dependency is `queued`, not `unreachable`: {}",
            row.next
        );
        assert!(
            row.next.contains("waiting on: park-lifecycle"),
            "{}",
            row.next
        );
    }

    /// The same park, read on a task that never left `queued`. An unavailable
    /// quota reading keeps its diagnosis alongside the elapsed age; replacing
    /// the recheck clock must not erase why the task is held.
    ///
    /// A `queued` task is parked by exactly the same gate a task on a real
    /// step is — the quota check runs on candidates, and `queued` is where
    /// candidates come from — but the `queued` arm of the match answered
    /// ahead of the park arm and never consulted it. The row read
    /// `queued · waiting for a worker slot to free up` for the whole park,
    /// which describes a dispatcher that has stopped rather than one holding
    /// a task on a clock.
    #[test]
    fn a_queued_task_parked_for_its_quota_also_reads_parked() {
        let repo = fixture("quota-parked-queued-row");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some(crate::pipeline::QUEUED));
        let mut task = repo.task("login").unwrap();
        task.front.parked_at = Some(chrono::Utc::now().timestamp() - 3840);
        task.front.parked_until = Some(chrono::Utc::now().timestamp() + 3600);
        task.front.parked_window = "unknown".into();
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[],
            &[],
        )
        .unwrap();

        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Parked), "{}", row.next);
        assert!(
            !row.next.contains("worker slot"),
            "a park is not a queue for a slot: {}",
            row.next
        );
        let display = row
            .parked_display
            .as_deref()
            .expect("a parked row carries its elapsed age");
        assert_eq!(display, "1h 04m");
        assert_eq!(row.parked_reason.as_deref(), Some("quota unavailable"));
        assert!(view::plain_table(&rows).contains("parked · quota unavailable · 1h 04m"));
    }

    /// A dependency it cannot pass is the harder hold, and keeps the row even
    /// when a park is running underneath it — the park says when a slot could
    /// be taken, the dependency says whether there is anything to take one
    /// for.
    #[test]
    fn a_dependency_outranks_a_park_on_a_queued_row() {
        let repo = fixture("quota-parked-queued-dependency");
        let pipelines = Pipelines::builtin();
        add(&repo, "api", &[], Some("implement"));
        add(&repo, "login", &["api"], Some(crate::pipeline::QUEUED));
        let mut task = repo.task("login").unwrap();
        task.front.parked_until = Some(chrono::Utc::now().timestamp() + 3600);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[],
            &[],
        )
        .unwrap();

        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Queued), "{}", row.next);
        assert!(row.next.contains("waiting on: api"), "{}", row.next);
    }

    /// A live lane's clock is its own: `now - launched_at`, not whatever the
    /// ledger has banked for a step still in flight.
    #[test]
    fn lane_time_reads_now_minus_launched_at_for_a_live_lane() {
        let repo = fixture("lane-time-live");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let mut task = repo.task("login").unwrap();
        task.front.launched_at = Some(chrono::Utc::now().timestamp() - 90);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let waiting = BTreeSet::new();
        let lanes = [lane("login · implement", &repo.root)];
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &waiting, &lanes, &[]).unwrap();

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
        let repo = fixture("command-step-running");
        let pipelines = Pipelines::builtin();
        // `checks` is the default pipeline's command step.
        add(&repo, "login", &[], Some("checks"));

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let waiting = BTreeSet::new();

        // Nothing started yet: the step is where the task sits, not what it is
        // doing, so this half is what the running half below is measured
        // against.
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &waiting, &[], &[]).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Queued));

        // The wrapper's own two files as `Runs::start` leaves them: a pid that
        // is alive — this test's own — and no exit code written yet.
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("checks", "login");
        let dir = runs.log_path(&key).parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{key}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();

        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &waiting, &[], &[]).unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(matches!(row.state, State::Running));
        // Its own clock, off the pid file, and not the last lane's
        // `launched_at` — which this task never set at all.
        assert!(row.lane_time.is_some(), "a run in flight answers with one");
    }

    /// `launch_landed` forgives `attempts` the first pass that sees a lane
    /// busy — about two seconds after it started — but the clock is
    /// `launched_at`'s other reading, and it has to survive that forgiveness
    /// for the rest of the step: a `launch_landed` that cleared both, as it
    /// once did, made the board read a dash from the second frame onward.
    #[test]
    fn lane_time_survives_launch_landed() {
        let repo = fixture("lane-time-survives-launch-landed");
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
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let waiting = BTreeSet::new();
        let lanes = [lane("login · implement", &repo.root)];
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &waiting, &lanes, &[]).unwrap();

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

    /// An archived task is only worth a row while its group still has
    /// something in the queue — a group whose last task has archived leaves
    /// the board entirely rather than lingering as a block of nothing but
    /// `● done` rows.
    #[test]
    fn done_rows_only_appear_for_a_group_still_in_the_queue() {
        let repo = fixture("done-rows");
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
        let repo = fixture("archive-cache-refresh");
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

    /// Every row names the pipeline its task resolves to: a task with no
    /// `pipeline:` of its own reads the project's configured default, one
    /// that names a real pipeline reads that name, and an archived task
    /// whose named pipeline has since been deleted reads that name verbatim
    /// rather than failing the whole board.
    #[test]
    fn every_row_names_the_pipeline_its_task_resolves_to() {
        let repo = fixture("pipeline-column-name");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "login", &[], Some("implement"), Some("auth"));
        add_to(&repo, "hotfix", &[], Some("implement"), Some("auth"));
        let mut hotfix = repo.task("hotfix").unwrap();
        hotfix.front.pipeline = Some("bugfix".into());
        hotfix.save().unwrap();

        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("signup.md"),
            "---\nid: signup\nstage: done\ngroup: auth\npipeline: retired\n---\n",
        )
        .unwrap();

        let active = rows(&repo, &pipelines).unwrap();
        let login = active.iter().find(|r| r.id == "login").unwrap();
        assert_eq!(login.pipeline, pipelines.default, "{}", login.pipeline);
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
        let repo = fixture("run-order-depth");
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
        let repo = fixture("run-order-stable");
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
    /// has already been banked for comes off just the same.
    #[test]
    fn a_reported_cost_is_taken_at_its_word_less_whatever_is_banked() {
        let repo = fixture("live-cost-reported");
        let ledger = [banked("login", "implement", "s1", Some(1.5))];
        let harvest = |cost: f64| crate::usage::Harvest {
            model: "claude-sonnet-5".into(),
            tokens: crate::usage::Tokens::default(),
            turns: 1,
            cost_usd: Some(cost),
            ctx_peak: 0,
        };

        assert_eq!(live_spend(&repo, &ledger, "s1", &harvest(4.0)).1, Some(2.5));
        // A transcript that reads lower than the ledger is a torn read, not a
        // refund: nothing here ever hands back a negative.
        assert_eq!(live_spend(&repo, &ledger, "s1", &harvest(0.5)).1, Some(0.0));
        // A session nothing has banked keeps the whole of its own reading.
        assert_eq!(live_spend(&repo, &ledger, "s9", &harvest(4.0)).1, Some(4.0));
    }

    /// The reason the subtraction is there at all: a step resumed on the
    /// session its predecessor ran under reads a transcript that already holds
    /// the predecessor's turns, and pricing the whole of it would put that
    /// step's bill on this one's row.
    #[test]
    fn a_reused_session_is_priced_from_what_it_has_spent_since_it_was_banked() {
        let mut repo = fixture("live-cost-reuse");
        repo.config.models = std::collections::BTreeMap::from([(
            "claude-*".to_string(),
            crate::usage::ModelPrice {
                context_window: 200_000,
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write_5m: 3.75,
                cache_write_1h: 6.0,
                session_reuse_idle: None,
                slots: 0,
                exclusive: false,
                local: false,
            },
        )]);

        // The previous step banked 1M output against this session; the
        // transcript, cumulative, now reads 1.5M.
        let mut entry = banked("login", "implement", "s1", None);
        entry.tokens.output = 1_000_000;
        let harvest = crate::usage::Harvest {
            model: "claude-sonnet-5".into(),
            tokens: crate::usage::Tokens {
                output: 1_500_000,
                ..Default::default()
            },
            turns: 4,
            // Claude Code records no cost, so the price map answers.
            cost_usd: None,
            ctx_peak: 0,
        };

        // The half-million this step has produced, at $15/M — not $22.50. OUT
        // is that same half-million, off the same subtraction: the two figures
        // beside each other on a row are one reading of one step.
        let (out, cost) = live_spend(&repo, &[entry], "s1", &harvest);
        let cost = cost.expect("nothing priced");
        assert_eq!(out, 500_000);
        assert!((cost - 7.5).abs() < 1e-9, "{cost}");

        // And a fresh session, which is what a step ordinarily gets, is read
        // in full: its delta is its total.
        let (out, cost) = live_spend(&repo, &[], "s2", &harvest);
        let cost = cost.expect("nothing priced");
        assert_eq!(out, 1_500_000);
        assert!((cost - 22.5).abs() < 1e-9, "{cost}");
    }

    /// The STATE column is sized to the board, not to the widest word there
    /// is: a queue with nothing waiting on a person should not hold seven
    /// columns open in front of CTX for a state nothing is in. The gutter is
    /// what gives the columns room, and it is three.
    #[test]
    fn the_state_column_is_only_as_wide_as_the_states_on_the_board() {
        let repo = fixture("state-column");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let mut board = Board::for_test();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Passing).unwrap());
        let header = frame
            .lines()
            .find(|l| l.contains("STATE"))
            .expect("a header row");
        let row = frame.lines().find(|l| l.contains("login")).unwrap();

        // `○ queued` is the widest state here, so CTX starts right after it
        // rather than out where a longer state like `● unreachable` would end.
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
        let repo = fixture("paused-row");
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
        // so the resume key does the job, and the NEXT column names the step
        // passing the gate would carry it to, `checks`, rather than the
        // command that names the same thing.
        assert!(row.resumable);
        assert_eq!(row.next, "→ checks — [r] resumes it");
    }

    /// Keeps only [`RECENT`] of them, oldest first out — each its own task, so
    /// this is the plain ring-buffer trim rather than the coalescing
    /// [`two_moves_of_the_same_task_coalesce_into_one_row_at_the_bottom`]
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
                    step: format!("line {i}"),
                    verdict: Verdict::None,
                    position: None,
                },
            );
        }
        assert_eq!(recent.len(), RECENT);
        assert!(matches!(
            recent.front().unwrap(),
            RecentEvent::Arrival { step, .. } if step == "line 4"
        ));
        assert!(matches!(
            recent.back().unwrap(),
            RecentEvent::Arrival { step, .. } if step == "line 9"
        ));
    }

    /// Two moves of the same task inside the window are one row, not two: the
    /// later move replaces the earlier one, and it sits at the bottom of the
    /// block — the newest news, where a reader's eye already goes.
    #[test]
    fn two_moves_of_the_same_task_coalesce_into_one_row_at_the_bottom() {
        let mut recent = VecDeque::new();
        push_recent(
            &mut recent,
            RecentEvent::Arrival {
                at: "14:20".into(),
                id: "other-task".into(),
                step: "review".into(),
                verdict: Verdict::None,
                position: None,
            },
        );
        push_recent(
            &mut recent,
            RecentEvent::Arrival {
                at: "14:21".into(),
                id: "gate-board".into(),
                step: "document".into(),
                verdict: Verdict::None,
                position: None,
            },
        );
        push_recent(
            &mut recent,
            RecentEvent::Arrival {
                at: "14:22".into(),
                id: "gate-board".into(),
                step: "checks".into(),
                verdict: Verdict::None,
                position: None,
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
        assert!(
            gate_row > other_row,
            "the later move sits at the bottom: {block}"
        );
    }

    /// Neither end of a task's life on the board is a step change: entering
    /// the queue is already a new row on the table, and archiving just dims
    /// one in place there, so pushing either into the ticker too would only
    /// repeat what the table already says. A real step change still reaches
    /// it, so this is not just the ticker going silent altogether.
    #[test]
    fn a_task_entering_the_queue_or_archiving_pushes_no_ticker_entry() {
        let repo = fixture("queue-and-archive-silent");
        let pipelines = Pipelines::builtin();
        add(&repo, "steady", &[], Some("implement"));

        let mut board = Board::for_test();
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert!(
            board.recent.is_empty(),
            "the board's first frame writes nothing to the ticker"
        );

        // A new task enters the queue.
        add(&repo, "newcomer", &[], None);
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert!(
            board.recent.is_empty(),
            "a task entering the queue pushes no ticker entry"
        );

        // "steady" moves a step, so this is not the ticker going silent for
        // every kind of event.
        let mut task = repo.task("steady").unwrap();
        task.set_stage("review", None);
        task.save().unwrap();
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert!(
            !board.recent.is_empty(),
            "a real step change still reaches the ticker"
        );

        // "newcomer" archives: its task file leaves the queue directory the
        // same way the archive step leaves it, taken out from under the
        // board rather than moved through a stage this board would see.
        std::fs::remove_file(repo.queue_dir().join("newcomer.md")).unwrap();
        let before = board.recent.len();
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert_eq!(
            board.recent.len(),
            before,
            "a task archiving pushes no ticker entry"
        );
    }

    // ---- board-resume: cursor, resume key, forward-looking ticker ----

    /// `↓` from no cursor at all lands on the first row rather than the
    /// second — there is nothing to move away from yet.
    #[test]
    fn moving_the_cursor_with_nothing_selected_lands_on_the_first_row() {
        let rows = vec![row("a"), row("b"), row("c")];
        assert_eq!(shift_cursor(&rows, None, 1), Some("a".to_string()));
        assert_eq!(shift_cursor(&rows, None, -1), Some("a".to_string()));
    }

    /// `↓` and `↑` step through the board's own row order and wrap at either
    /// end, rather than sticking or landing off the table.
    #[test]
    fn the_cursor_steps_through_rows_and_wraps_at_either_end() {
        let rows = vec![row("a"), row("b"), row("c")];
        assert_eq!(shift_cursor(&rows, Some("a"), 1), Some("b".to_string()));
        assert_eq!(shift_cursor(&rows, Some("c"), 1), Some("a".to_string()));
        assert_eq!(shift_cursor(&rows, Some("a"), -1), Some("c".to_string()));
    }

    /// A row the board no longer shows — its task passed its step, or
    /// vanished between two frames — must not strand the cursor: the next
    /// press lands on the first row still there rather than nowhere.
    #[test]
    fn the_cursor_resets_to_the_first_row_when_its_own_row_is_gone() {
        let rows = vec![row("a"), row("b")];
        assert_eq!(shift_cursor(&rows, Some("gone"), 1), Some("a".to_string()));
    }

    /// An empty board has nowhere for the cursor to sit.
    #[test]
    fn the_cursor_has_nowhere_to_go_on_an_empty_board() {
        assert_eq!(shift_cursor(&[], Some("a"), 1), None);
    }

    /// A paused task offers the resume key only once its own dependencies
    /// have finished — the same rule that let it start at all, checked again
    /// because a board reads the graph fresh rather than trusting that a
    /// task already past `queued` must still pass it.
    #[test]
    fn a_paused_row_is_not_resumable_while_its_own_dependency_is_unfinished() {
        let repo = fixture("resume-deps");
        let pipelines = Pipelines::builtin();
        add(&repo, "blocker", &[], Some("implement"));
        add(&repo, "gate-board", &["blocker"], None);
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[],
            &[],
        )
        .unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();

        assert!(!row.resumable, "{}", row.next);
        assert!(row.next.contains("spoolway resume"), "{}", row.next);
        assert!(!row.next.contains("[r]"), "{}", row.next);
    }

    /// A paused task offers the resume key only once no lane of its own is
    /// mid-turn — a held pane a person is actively typing into, or an agent
    /// still running in it. Settled again, the same row is resumable.
    #[test]
    fn a_paused_row_is_not_resumable_while_its_lane_is_busy() {
        let repo = fixture("resume-busy");
        let pipelines = Pipelines::builtin();
        add(&repo, "gate-board", &[], None);
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());

        // Held pane, still working — the lane keeps the name of the step it
        // paused at, not `paused` itself, which never starts a lane of its
        // own.
        let mut busy = lane("gate-board · implement", &repo.root);
        busy.status = crate::mux::LaneStatus::Working;
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[busy],
            &[],
        )
        .unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();
        assert!(!row.resumable, "{}", row.next);
        assert!(row.next.contains("spoolway resume"), "{}", row.next);

        // Same pane, settled: the round is over and the key comes back.
        let mut settled = lane("gate-board · implement", &repo.root);
        settled.status = crate::mux::LaneStatus::Done;
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[settled],
            &[],
        )
        .unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();
        assert!(row.resumable, "{}", row.next);
        assert!(row.next.contains("[r] resumes it"), "{}", row.next);
        assert!(!row.next.contains("spoolway resume"), "{}", row.next);
    }

    /// A blocked row that is actually parked for a person — nobody staffs
    /// that step — follows the same dependency and busy-lane rule as a
    /// paused one for whether the key does anything, but its NEXT column
    /// never says so: it reads only the step a pass would carry the task
    /// to, `cleared_block_target`'s own answer, and nothing else.
    #[test]
    fn a_parked_blocked_row_is_resumable_by_the_same_rule_as_a_paused_one() {
        let repo = fixture("resume-blocked");
        let pipelines = Pipelines::builtin();
        add(&repo, "wall", &[], None);
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[],
            &[],
        )
        .unwrap();
        let row = rows.iter().find(|r| r.id == "wall").unwrap();

        assert!(row.resumable, "{}", row.next);
        assert_eq!(row.next, "→ review");
    }

    /// `r` on a paused row goes through exactly the code `spoolway
    /// release` runs: the task moves off `paused` onto its gate's `on_pass`
    /// destination, with `paused_at` cleared.
    #[test]
    fn r_on_a_resumable_paused_row_releases_it() {
        let repo = fixture("resume-key-release");
        let pipelines = Pipelines::builtin();
        add(&repo, "gate-board", &[], None);
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("gate-board"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();

        let task = repo.task("gate-board").unwrap();
        assert_eq!(task.stage(), "review", "{}", task.stage());
        assert_eq!(task.front.paused_at, None);
    }

    /// `r` on a blocked row goes through exactly the code `spoolway
    /// unblock` runs: the task resumes at the step it stopped on, with
    /// `blocked_from` cleared.
    #[test]
    fn r_on_a_resumable_blocked_row_unblocks_it() {
        let repo = fixture("resume-key-unblock");
        let pipelines = Pipelines::builtin();
        add(&repo, "wall", &[], None);
        let mut task = repo.task("wall").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();

        let task = repo.task("wall").unwrap();
        assert_eq!(task.stage(), "implement", "{}", task.stage());
        assert_eq!(task.front.blocked_from, None);
    }

    /// `r` on a row whose own rule says it is not resumable does
    /// nothing: the task stays exactly where it was.
    #[test]
    fn r_on_a_row_that_is_not_resumable_does_nothing() {
        let repo = fixture("resume-key-refused");
        let pipelines = Pipelines::builtin();
        add(&repo, "blocker", &[], Some("implement"));
        add(&repo, "gate-board", &["blocker"], None);
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('r'))
            .unwrap();

        let task = repo.task("gate-board").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.paused_at.as_deref(), Some("implement"));
    }

    /// `enter` and `q` are read while browsing but do nothing: resuming now
    /// belongs to `r`, and quitting to `ctrl-c` alone.
    #[test]
    fn enter_and_q_are_ignored_while_browsing() {
        let repo = fixture("enter-and-q-ignored");
        let pipelines = Pipelines::builtin();
        add(&repo, "gate-board", &[], None);
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
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
        let frame = board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert!(!frame.contains("┌─"), "{frame}");
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
        let repo = fixture("panel-colour-safe");
        let pipelines = Pipelines::builtin();
        add(&repo, "gate-board", &[], None);
        let mut task = repo.task("gate-board").unwrap();
        task.front.paused_at = Some("implement".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
            .unwrap();
        let frame = board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert!(
            frame.contains("resume all"),
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

    /// `P` with a command step running parks nothing about it until the
    /// panel it opens is answered: `l` leaves the run and the task exactly
    /// as they were, and a later `P`/`k` stops the run and parks the task,
    /// `parked_from` naming the step whose run it took down.
    #[test]
    fn pressing_shift_p_with_a_command_step_running_opens_a_kill_or_leave_panel() {
        let repo = fixture("pause-command-step");
        let pipelines = Pipelines::builtin();
        // `checks` is the default pipeline's command step.
        add(&repo, "login", &[], Some("checks"));

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("checks", "login");
        runs.start(&key, "sleep 30", &repo.root, &BTreeMap::new())
            .unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("pause all"), "{frame}");
        assert!(frame.contains("login · checks"), "{frame}");

        // Declining: nothing about the run or the task changes.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('l'))
            .unwrap();
        assert_eq!(runs.state(&key), crate::command_step::RunState::Running);
        assert_eq!(repo.task("login").unwrap().stage(), "checks");

        // Asking again and confirming this time kills the run and parks the
        // task on `parked_from`, the same record an interrupt leaves.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('k'))
            .unwrap();
        assert_ne!(runs.state(&key), crate::command_step::RunState::Running);
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.parked_from.as_deref(), Some("checks"));
        assert_eq!(task.front.blocked_from, None);
        assert_eq!(task.front.paused_at, None);

        runs.stop(&key);
    }

    /// `p`, on the cursor's row, opens the same kind of panel as `P` but
    /// scoped to just that task — titled with its id, and worded in the
    /// singular since killing it only ever stops this one task's step.
    #[test]
    fn pressing_p_on_the_cursor_with_a_command_step_running_opens_a_panel_scoped_to_it() {
        let repo = fixture("pause-cursor-command-step");
        let pipelines = Pipelines::builtin();
        // `checks` is the default pipeline's command step.
        add(&repo, "login", &[], Some("checks"));
        add(&repo, "other", &[], Some("implement"));

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("checks", "login");
        runs.start(&key, "sleep 30", &repo.root, &BTreeMap::new())
            .unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("login"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("pause login"), "{frame}");
        assert!(!frame.contains("pause all"), "{frame}");
        assert!(frame.contains("login · checks"), "{frame}");
        assert!(frame.contains("[k] kill it"), "{frame}");

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('k'))
            .unwrap();
        assert_ne!(runs.state(&key), crate::command_step::RunState::Running);
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.parked_from.as_deref(), Some("checks"));

        // The other task never entered the picture.
        assert_eq!(repo.task("other").unwrap().stage(), "implement");

        runs.stop(&key);
    }

    /// `P` interrupts a live agent lane this run owns and parks its task —
    /// exercised against the headless backend, whose "interrupt" is ending
    /// the turn's process outright, the same as [`Mux::stop_lane`].
    #[test]
    #[cfg(unix)]
    fn pressing_shift_p_interrupts_a_live_headless_lane_and_parks_its_task() {
        let mut repo = fixture("pause-live-lane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let (mux, name) = live_headless_lane(&repo);
        assert!(mux.list_lanes().unwrap().iter().any(|l| l.name == name));

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();

        assert!(
            mux.list_lanes().unwrap().iter().all(|l| l.name != name),
            "headless has no keyboard, so an interrupt ends the turn"
        );
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.parked_from.as_deref(), Some("implement"));
        assert_eq!(task.front.blocked_from, None);
    }

    /// `o` on a headless run has no pane to open an editor in — headless
    /// refuses it the way `open_tab` already does — so the key is a no-op:
    /// best-effort, like every other key here, and the task file it would
    /// have opened is left exactly as it was.
    #[test]
    fn pressing_o_with_no_multiplexer_leaves_the_task_file_unchanged() {
        let mut repo = fixture("open-key-headless");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let before = std::fs::read_to_string(repo.task("login").unwrap().path).unwrap();

        let mut board = Board::for_test();
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

    /// `p` then `R` parks a task and puts it straight back — the round trip
    /// the board leaves nothing behind for: `prompts`, `rounds` and
    /// `arrived_from` come back byte-for-byte, `rounds_via("implement",
    /// "paused")` stays zero, and the row's own NEXT column carries no loop
    /// counter across it.
    #[test]
    #[cfg(unix)]
    fn pressing_p_then_shift_r_round_trips_a_task_without_banking_a_lap() {
        let mut repo = fixture("park-round-trip");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let before = repo.task("login").unwrap();
        let rounds_before = before.front.rounds.clone();
        let prompts_before = before.front.prompts.clone();
        let arrived_from_before = before.front.arrived_from.clone();

        let (_mux, _name) = live_headless_lane(&repo);

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        assert_eq!(repo.task("login").unwrap().stage(), crate::pipeline::PAUSED);

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
            .unwrap();

        let after = repo.task("login").unwrap();
        assert_eq!(after.stage(), "implement");
        assert_eq!(after.front.rounds, rounds_before);
        assert_eq!(after.front.prompts, prompts_before);
        assert_eq!(after.front.arrived_from, arrived_from_before);
        assert_eq!(after.rounds_via("implement", crate::pipeline::PAUSED), 0);
        assert_eq!(after.rounds_via(crate::pipeline::PAUSED, "implement"), 0);

        let log = after.section("## Status Log").unwrap();
        assert!(log.contains("paused from the board"), "{log}");
        assert!(log.contains("put back from the board"), "{log}");
        assert!(!log.to_lowercase().contains("unblocked"), "{log}");

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &pipelines, &repo.archive_dir());
        let rows = build_rows(
            &repo,
            &tasks,
            &pipelines,
            &graph,
            &BTreeSet::new(),
            &[],
            &[],
        )
        .unwrap();
        let row = rows.iter().find(|r| r.id == "login").unwrap();
        assert!(!row.next.contains("loop"), "{}", row.next);
    }

    /// `u` on a queued row with nothing depending on it opens a panel naming
    /// the task and the pending path it would land at, and answering `u`
    /// moves the document there with every reserved key gone from its
    /// frontmatter — accepted unchanged by a fresh `queue add --from`.
    #[test]
    fn pressing_u_on_an_unstarted_task_moves_it_back_to_pending() {
        let repo = fixture("unqueue-cursor");
        let pipelines = Pipelines::builtin();
        add(&repo, "chain-refusals", &[], None);
        assert_eq!(repo.task("chain-refusals").unwrap().stage(), "queued");

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("chain-refusals"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("unqueue chain-refusals"), "{frame}");
        assert!(frame.contains("chain-refusals.md"), "{frame}");
        assert!(frame.contains("[u] unqueue it"), "{frame}");
        // Still sitting in the queue — nothing moves until the panel is
        // answered.
        assert!(repo.task("chain-refusals").is_ok());

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
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
        // writes nothing; it has to accept the document exactly as it is.
        let contract_args = crate::cli::TaskContractArgs {
            from: vec![pending_doc.display().to_string()],
        };
        crate::commands::task_contract(&repo, &pipelines, &contract_args, &repo.root).unwrap();

        // And the document a fresh `queue add --from` accepts unchanged,
        // exactly as it did the first time — so it can be queued again
        // without editing it by hand.
        let queue_args = crate::cli::QueueAddArgs {
            from: vec![pending_doc.display().to_string()],
        };
        crate::commands::queue_add(&repo, &pipelines, &queue_args, &repo.root, false).unwrap();
        assert_eq!(repo.task("chain-refusals").unwrap().stage(), "queued");
    }

    /// `esc` on `u`'s panel leaves the queue exactly as it was.
    #[test]
    fn pressing_esc_on_the_unqueue_panel_moves_nothing() {
        let repo = fixture("unqueue-esc");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[], None);

        let mut board = Board::for_test();
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
        let repo = fixture("unqueue-started");
        let pipelines = Pipelines::builtin();
        add(&repo, "under-way", &[], Some("implement"));

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();

        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(!frame.contains("unqueue under-way"), "{frame}");
        assert_eq!(repo.task("under-way").unwrap().stage(), "implement");
    }

    /// `u` refuses a queued task that a still-queued task depends on —
    /// unqueuing it would leave the dependent waiting on a dependency the
    /// board no longer shows.
    #[test]
    fn pressing_u_on_a_task_a_queued_dependent_names_is_a_no_op() {
        let repo = fixture("unqueue-depended-on");
        let pipelines = Pipelines::builtin();
        add(&repo, "drop-walk", &[], None);
        add(&repo, "chain-refusals", &["drop-walk"], None);

        let mut board = Board::for_test();
        // `drop-walk` is the dependency, so it sorts first — one `Down` from
        // no cursor at all reaches it directly.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("drop-walk"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();

        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(!frame.contains("unqueue drop-walk"), "{frame}");
        assert_eq!(repo.task("drop-walk").unwrap().stage(), "queued");
    }

    /// `u` moves the cursor to the row underneath the one it just removed —
    /// the same row `↓` would have reached — rather than leaving it naming a
    /// task the board no longer shows.
    #[test]
    fn pressing_u_moves_the_cursor_to_the_row_underneath() {
        let repo = fixture("unqueue-cursor-advances");
        let pipelines = Pipelines::builtin();
        add(&repo, "chain-refusals", &[], None);
        add(&repo, "month-instant", &[], None);

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("chain-refusals"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
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
        let repo = fixture("unqueue-cursor-clears");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[], None);

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("solo"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();

        assert_eq!(board.cursor, None);
        assert!(!repo.queue_dir().join("solo.md").exists());
    }

    /// `U` opens a panel naming every task that has not started, and answering
    /// it carries each of them back to pending — a running task is never in
    /// that set, and never moves.
    #[test]
    fn pressing_shift_u_moves_every_unstarted_task_to_pending() {
        let repo = fixture("unqueue-all");
        let pipelines = Pipelines::builtin();
        add(&repo, "chain-refusals", &[], None);
        add(&repo, "month-instant", &[], None);
        add(&repo, "already-running", &[], Some("implement"));

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('U'))
            .unwrap();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("unqueue all"), "{frame}");
        assert!(frame.contains("2 tasks have not started"), "{frame}");
        // Both unstarted ids appear, one per line — the running task is not
        // among them, which the board's own table beneath the panel still
        // lists by name regardless, so checking for its absence from the
        // whole frame would prove nothing.
        assert!(frame.contains("chain-refusals"), "{frame}");
        assert!(frame.contains("month-instant"), "{frame}");
        assert!(frame.contains("[U] unqueue them"), "{frame}");

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('U'))
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
        let repo = fixture("unqueue-keeps-newer-draft");
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

    /// `R` resumes every paused task, but only after a panel naming the
    /// gated ones among them whenever there is at least one — a plain park
    /// left by `p`, or an interrupt, carries no `paused_at` and never holds
    /// `R` up on its own.
    #[test]
    fn pressing_shift_r_gates_on_a_paused_at_but_resumes_a_plain_park_freely() {
        let repo = fixture("resume-all");
        let pipelines = Pipelines::builtin();
        add(&repo, "gate-board", &[], None);
        let mut gated = repo.task("gate-board").unwrap();
        gated.front.paused_at = Some("implement".into());
        gated.set_stage(crate::pipeline::PAUSED, None);
        gated.save().unwrap();

        add(&repo, "quiet-pane", &[], None);
        let mut parked = repo.task("quiet-pane").unwrap();
        parked.front.parked_from = Some("review".into());
        parked.set_stage(crate::pipeline::PAUSED, None);
        parked.save().unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
            .unwrap();

        // A gate is among them, so nothing moved yet — a panel names it.
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("resume all"), "{frame}");
        assert!(frame.contains("gate-board"), "{frame}");
        assert_eq!(
            repo.task("gate-board").unwrap().stage(),
            crate::pipeline::PAUSED
        );
        assert_eq!(
            repo.task("quiet-pane").unwrap().stage(),
            crate::pipeline::PAUSED
        );

        // Confirmed: both go, the gate released past `implement` and the
        // park sent back to the step it stopped on.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
            .unwrap();
        assert_eq!(repo.task("gate-board").unwrap().stage(), "review");
        assert_eq!(repo.task("quiet-pane").unwrap().stage(), "review");
    }

    /// A paused queue with no gate among it needs no panel at all: `R`
    /// resumes it on the spot.
    #[test]
    fn pressing_shift_r_with_no_gate_among_them_resumes_at_once() {
        let repo = fixture("resume-all-no-gate");
        let pipelines = Pipelines::builtin();
        add(&repo, "quiet-pane", &[], None);
        let mut parked = repo.task("quiet-pane").unwrap();
        parked.front.parked_from = Some("review".into());
        parked.set_stage(crate::pipeline::PAUSED, None);
        parked.save().unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
            .unwrap();

        assert_eq!(repo.task("quiet-pane").unwrap().stage(), "review");
    }
}
