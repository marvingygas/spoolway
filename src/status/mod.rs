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

// Re-exported, not just imported: `screen::key_hint` reads the same dim and
// gutter the board paints its own key line with, rather than spelling either
// out a second time — see `screen::key_hint`.
use view::{
    AMBER, RecentEvent, Style, Verdict, clamp_rows, footer, group_totals, masthead, pane_height,
    pane_width, pause_confirm_panel, resume_confirm_panel, spool_frame, strip_ansi, table, ticker,
    unqueue_all_confirm_panel, unqueue_confirm_panel,
};
pub(crate) use view::{DIM, GUTTER, RESET};

/// The redraw rate every screen's own wait polls stdin at — the jobs screen,
/// the queue screen, and the dispatcher board, whose wait slices its whole
/// interval into stretches of this on every target. The board's wait also
/// watches the queue and commands directories (see
/// `crate::screen::DirWatch`), which ends a slice early; it never lengthens
/// one.
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
    /// At the pipeline's blocked step, carrying its reason.
    Blocked,
    /// A live lane's pane is holding a permission prompt — herdr's own
    /// read, off the lane list, fresh every redraw. The task's own stage has
    /// not moved and is not `paused`: this is a live turn waiting on a
    /// keystroke in its pane, not a stop, so nothing here is resumable.
    Prompt,
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
    /// How long this task's lane has been open at the step it is on: `now -
    /// launched_at` while a lane is live, and the ledger's summed `wall_s` at
    /// that step once it is not — every round of it, the same as OUT and
    /// COST. `None` where neither answers: no live lane and nothing banked.
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
/// run, so a frame is up exactly as long as the dispatcher is.
pub struct Board {
    /// Task id → the stage it was on at the last frame.
    stages: BTreeMap<String, String>,
    /// Task id → when its row's stage was last seen to change, alongside
    /// `stages` above. `build_rows`' state arm reads this back to tell a task
    /// that just handed off to a new step, with no lane up for it yet, apart
    /// from one that has genuinely run out of workers to pick it up: the two
    /// look identical on disk, and only this clock tells them apart.
    arrived: BTreeMap<String, Instant>,
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
    /// The task id the cursor sits on. Starts `None` only until [`render`]
    /// draws its first frame, which lands it on the first row of the first
    /// group straight away rather than leaving a person's first `↑`/`↓`
    /// press go to discover that row — see the seeding block inline in
    /// `render`'s own body. By id rather than a plain row index, so a state
    /// change that resorts the board — a task passing its step, one landing on
    /// `paused` above it — never leaves the cursor pointing at a different
    /// task than the one a person last put it on. The same seeding block
    /// re-lands it on the first row whenever the id it already holds falls
    /// out of the frame's own rows entirely, such as a group finishing and
    /// taking the cursor's row with it — no key pressed. `None` again only
    /// once the board has nothing left to show at all.
    cursor: Option<String>,
    /// What a `p`, `P` or `R` keypress is waiting on, if anything — see
    /// [`BoardMode`]. `Browsing` on every other key, including the plain
    /// cursor moves and `r`, which never open a panel at all.
    mode: BoardMode,
    /// A one-minute memo for the job ledger's own rows, so the per-second
    /// redraw does not re-scan the calendar for every enabled cron job — see
    /// [`crate::jobs::active_jobs_cached`].
    jobs_next: Option<crate::jobs::ActiveJobsMemo>,
    /// The id of every row the last frame drew, in the order it drew them —
    /// the live queue and the archived rows beside it, exactly as [`render`]
    /// composed them. This is what `↑`/`↓` walk, so a cursor move is a step
    /// through a list already in memory rather than a fresh read of every
    /// task file: the board reads keys while a pass is rewriting and
    /// archiving those very files, and a read caught mid-write used to lose
    /// the keypress outright. The cost is that the marker can sit for one
    /// frame on a row the queue has already moved — the next draw puts it
    /// right, the same way [`render`] already re-seeds a cursor whose row has
    /// left the board.
    ///
    /// Empty until the first frame, which is drawn before any key is read —
    /// see the dispatch loop's own draw ahead of its first pass.
    drawn: Vec<String>,
}

impl Board {
    pub fn new() -> Board {
        Board::with_term(crate::platform::TermGuard::new())
    }

    fn with_term(term: crate::platform::TermGuard) -> Board {
        Board {
            stages: BTreeMap::new(),
            arrived: BTreeMap::new(),
            recent: VecDeque::new(),
            adopted: false,
            _term: term,
            cursor: None,
            mode: BoardMode::Browsing,
            jobs_next: None,
            drawn: Vec::new(),
        }
    }

    /// A board whose terminal guard is inert — for tests, so parallel `Board`s
    /// do not take the process's real terminal raw and race on restore
    /// (finding 53).
    #[cfg(test)]
    pub fn for_test() -> Board {
        Board::with_term(crate::platform::TermGuard::inert())
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
        // `render` seeds `self.cursor` itself, from the very rows it composes
        // to draw the table — see the seeding block inline in its own body
        // for why — rather than this reading the queue a second time first.
        let frame = render(
            repo,
            pipelines,
            phase,
            &mut self.stages,
            &mut self.arrived,
            &mut self.recent,
            &mut self.cursor,
            &mut self.jobs_next,
            &mut self.drawn,
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
    /// already open every other key is read by that panel instead: `enter`
    /// confirms whatever it opened and `esc` cancels it, on every panel the
    /// board draws, and the letter that opened the panel no longer answers
    /// it once it is — a `q` typed there, or any other key neither mode
    /// recognises, is ignored, the same as it is while browsing. A pause
    /// panel answers one key further: `s` leaves every named abort running
    /// and schedules its task's `gate_at` on the step it is on instead.
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
            BoardMode::ConfirmPause { aborts, scope } => {
                self.on_key_pause_confirm(repo, aborts, scope, key)
            }
            BoardMode::ConfirmResume(gated) => {
                self.on_key_resume_confirm(repo, pipelines, gated, key)
            }
            BoardMode::ConfirmUnqueue { chain, dir } => {
                self.on_key_unqueue_confirm(repo, pipelines, chain, dir, key)
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
            // Off the last frame's own rows, with no read of any task file
            // behind it — see [`Board::drawn`]. An arrow is the one key that
            // cannot fail, whatever a pass is doing to the queue right now.
            Key::Up => self.cursor = shift_cursor(&self.drawn, self.cursor.as_deref(), -1),
            Key::Down => self.cursor = shift_cursor(&self.drawn, self.cursor.as_deref(), 1),
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

    /// `o`: open the cursor's task file in an editor, in a pane the
    /// multiplexer opens — a no-op with no cursor or a cursor on a row the
    /// board no longer draws. `repo.task` reads both the queue and the
    /// archive, so this reaches a done row's document exactly as it does a
    /// live one — [`render`]'s own composed row list, which [`Board::drawn`]
    /// is taken from, is what lets the cursor land on that row in the first
    /// place. Never blocks: the pane runs the editor on its
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
        let mux = crate::mux::backend(repo)?;
        let _ = mux.open_command(&repo.root, &format!("{id} · edit"), &command);
        Ok(())
    }

    /// Send the highlighted row through `spoolway resume`, exactly as a
    /// person typing the command would.
    ///
    /// A no-op wherever there is nothing to do: no cursor yet, a cursor
    /// sitting on a row the queue no longer has, or a row whose own
    /// `resumable` says the key does nothing here — the dependency or
    /// busy-lane rule that decided the NEXT column already decided this, for
    /// a row with a real step to check; a row parked off `queued` carries no
    /// such rule at all and reads `resumable` outright — see `build_rows`'s
    /// `paused` arm.
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

    /// `P`: park every task in the run, opening [`BoardMode::ConfirmPause`]
    /// first — scoped to the whole run, and naming every live agent turn and
    /// command run it would abort — whenever anything is live anywhere in
    /// the queue. Nothing is touched until that panel is answered: unlike the
    /// old shape, an agent lane is no longer interrupted ahead of the panel,
    /// since `esc` has to be able to leave the *whole* keypress undone, not
    /// just the half of it a command step's kill would have covered.
    fn begin_pause_all(&mut self, repo: &Repo, pipelines: &Pipelines) -> Result<()> {
        let tasks = repo.tasks()?;
        let mux = crate::mux::backend(repo)?;
        let lanes = mux.list_lanes().unwrap_or_default();
        let aborts = live_aborts(repo, &tasks, pipelines, &lanes);
        if aborts.is_empty() {
            park_every_pausable(repo, tasks)?;
            return Ok(());
        }
        self.mode = BoardMode::ConfirmPause {
            aborts,
            scope: PauseScope::All,
        };
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
            park_under_lock(repo, &id)?;
            return Ok(());
        }
        self.mode = BoardMode::ConfirmPause {
            aborts,
            scope: PauseScope::Cursor(id),
        };
        Ok(())
    }

    fn on_key_pause_confirm(
        &mut self,
        repo: &Repo,
        aborts: Vec<Abort>,
        scope: PauseScope,
        key: crate::screen::Key,
    ) -> Result<()> {
        use crate::screen::Key;
        match key {
            // The one thing either panel does: abort what it named — an
            // agent turn interrupted through `Mux::interrupt_lane`, a
            // command run stopped through `Runs::stop` — and park every task
            // that named abort belongs to. `P`'s panel then goes on to park
            // whatever else in the run had nothing live to abort, whether or
            // not the panel named it.
            Key::Enter => {
                let mux = crate::mux::backend(repo)?;
                let runs = crate::command_step::Runs::new(&repo.commands_dir());
                for abort in &aborts {
                    match abort.kind {
                        // Best-effort: a lane that has already gone quiet on
                        // its own has nothing left to interrupt, and one
                        // lane's failure here must never leave the rest of
                        // the panel's own promise half kept.
                        AbortKind::Agent => {
                            let name = crate::mux::lane_name(&abort.step, &abort.task);
                            let _ = mux.interrupt_lane(&name);
                        }
                        AbortKind::Command => {
                            runs.stop(&crate::command_step::Runs::key(&abort.step, &abort.task));
                        }
                    }
                    park_under_lock(repo, &abort.task)?;
                }
                if matches!(scope, PauseScope::All) {
                    // Read fresh rather than trusting the queue as it stood
                    // when the panel opened: the same reason every other
                    // confirm panel here re-reads at answer time, and the
                    // loop above may itself have just moved some of these
                    // tasks off their own step and onto `paused`.
                    let rest: Vec<crate::task::Task> = repo
                        .tasks()?
                        .into_iter()
                        .filter(|t| !aborts.iter().any(|a| a.task == t.id()))
                        .collect();
                    park_every_pausable(repo, rest)?;
                }
            }
            // `s`: leave every named abort running and write `gate_at` onto
            // its task instead — whatever that step reports, whenever it
            // reports it, is what parks it now, on its own road through
            // `commands::report` rather than the pass-only one a pipeline's
            // own `gate: true` reads. Toggled per task
            // rather than only ever set, so pressing `s` again on a row that
            // already carries a schedule for the step it is on clears it —
            // the mockup's "pressing `s` again... clears it". `P`'s panel
            // still parks every task with nothing live to wait out, exactly
            // as `enter` does: there is no step in flight to schedule for
            // those.
            Key::Char('s') => {
                for abort in &aborts {
                    let mut task = repo.task(&abort.task)?;
                    task.front.gate_at = match task.front.gate_at.as_deref() {
                        Some(step) if step == abort.step => None,
                        _ => Some(abort.step.clone()),
                    };
                    task.save()?;
                }
                if matches!(scope, PauseScope::All) {
                    let rest: Vec<crate::task::Task> = repo
                        .tasks()?
                        .into_iter()
                        .filter(|t| !aborts.iter().any(|a| a.task == t.id()))
                        .collect();
                    park_every_pausable(repo, rest)?;
                }
            }
            // Backing out with `esc` leaves the task, every lane and every
            // run exactly as they were — nothing here to undo, since nothing
            // was touched while the panel was open.
            Key::Esc => {}
            _ => self.mode = BoardMode::ConfirmPause { aborts, scope },
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
            Key::Enter => self.resume_all(repo, pipelines)?,
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

/// The command line `o` runs on a document, everywhere `o` appears.
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
    /// `p` or `P` found an agent turn or a command run live for its scope:
    /// what pausing would abort, named, and nothing acted on yet. `[enter]`
    /// carries the pause out — every named abort, plus, for `P`, every other
    /// task in the run that has nothing live to abort — `[s]` leaves every
    /// named abort running and schedules its task's `gate_at` for the step
    /// it is on instead, still parking the rest of `P`'s run outright, and
    /// `[esc]` leaves the task, every lane and every run exactly as they
    /// were. `scope` says whether this is `P`'s whole-run panel or `p`'s,
    /// narrowed to one task.
    ConfirmPause {
        aborts: Vec<Abort>,
        scope: PauseScope,
    },
    /// `R` found a paused task still waiting at a gate, named so that
    /// carrying it past that gate is never the accidental half of a
    /// keypress meant for a plain interrupted one beside it.
    ConfirmResume(Vec<String>),
    /// `u` found a task that has not started, named along with every
    /// unstarted task that reaches it through `depends_on` — see
    /// [`unqueue_chain`] — and `dir`, this run's own [`Repo::pending_dir`],
    /// read once when the panel opened, so its panel can show where the
    /// documents are about to land without a second lookup at answer time.
    /// `chain` still needs a fresh [`Repo::task`] per id to answer with, the
    /// same as every confirm panel here: a document a moment ago and the
    /// document now are not guaranteed to be the same file.
    ConfirmUnqueue {
        chain: Vec<ChainEntry>,
        dir: PathBuf,
    },
    /// `U`'s own version of the same panel, naming every task it would carry
    /// back to pending rather than just the one under the cursor.
    ConfirmUnqueueAll(Vec<String>),
}

impl BoardMode {
    /// The panel this mode draws over the table, if any.
    fn panel(&self) -> Option<Vec<String>> {
        match self {
            BoardMode::Browsing => None,
            BoardMode::ConfirmPause { aborts, scope } => Some(pause_confirm_panel(aborts, scope)),
            BoardMode::ConfirmResume(gated) => Some(resume_confirm_panel(gated)),
            BoardMode::ConfirmUnqueue { chain, dir } => Some(unqueue_confirm_panel(chain, dir)),
            BoardMode::ConfirmUnqueueAll(ids) => Some(unqueue_all_confirm_panel(ids)),
        }
    }
}

/// Who a [`BoardMode::ConfirmPause`] panel is about — the whole run for `P`,
/// or one task for `p`. Decides which of `view`'s two panel builders draws
/// the panel — `all_pause_panel`'s multi-line list against
/// `cursor_pause_panel`'s single step.
enum PauseScope {
    All,
    Cursor(String),
}

/// One command step `p` or `P` found running, named for
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
/// folded into the one shape `p` and `P` both draw a panel from and answer
/// against. Both of those already skip a `paused` task — the one state
/// nothing is ever live on — so there is nothing further to filter out here.
/// A `blocked` task is not skipped: the unblocker can be mid-turn on it, and
/// that turn is exactly what `p` and `P` reach.
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

/// Whether `task` is one `P` would park — everything but a `paused` row,
/// already stopped and already waiting on a person rather than a state to
/// park. A `blocked` row is pausable like any other: the unblocker may be
/// mid-turn on it, which is what makes it worth reaching in the first place.
/// The one predicate [`park_every_pausable`] reads.
fn is_pausable(task: &crate::task::Task) -> bool {
    task.stage() != crate::pipeline::PAUSED
}

/// Park every task [`is_pausable`] selects, straight onto `paused` with no
/// panel — what `P` does outright when nothing in the run is live, and what
/// it does to the rest of the run once its own panel, if one opened, is
/// answered.
fn park_every_pausable(repo: &Repo, tasks: Vec<crate::task::Task>) -> Result<()> {
    for task in tasks {
        if !is_pausable(&task) {
            continue;
        }
        park_under_lock(repo, task.id())?;
    }
    Ok(())
}

/// Park one task by id: read, [`park`] and save under the same per-task
/// lock a lane's `spoolway report` and the dispatcher's own `persist` take,
/// so the park cannot land in the middle of either one's read-modify-write
/// and lose it — or be lost to it. Read fresh under the lock rather than
/// from the queue as the board last saw it, and silent about a task the
/// queue no longer has, or one already stopped: `P` walks the whole run,
/// and one row archived since the panel opened must not leave the rest of
/// it unparked (jobs review finding 5).
fn park_under_lock(repo: &Repo, id: &str) -> Result<()> {
    let _task_lock = crate::lock::TaskLock::acquire(&repo.task_lock_file(id));
    let Ok(mut task) = repo.task(id) else {
        return Ok(());
    };
    if !is_pausable(&task) {
        return Ok(());
    }
    park(&mut task, "paused from the board", false);
    task.save()
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
    if repo.task(id).is_err() {
        return Ok(());
    }
    // `paused_at` is a gate passed, waiting to be sent on past it;
    // `parked_from` is a person's own interrupt, and `blocked_from` is a
    // real block — all three waiting to be sent back to where they stopped.
    // `parked_from` and `blocked_from` can both be set at once now: `p` on a
    // `blocked` row writes `parked_from: blocked` beside the `blocked_from`
    // already there, and `back_onto_its_step` checks `parked_from` first, so
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
    // is reached, by the row's own `resumable` — see `resume_cursor`.
    //
    // Nothing here has to say which of the three, if any, applies:
    // `commands::resume` reads `paused_at.is_some()` itself to route a gate
    // one way and everything else the other, so naming a step here would
    // only risk disagreeing with it — see `back_onto_its_step`, which finds
    // `parked_from` and `blocked_from` on its own, falls back to `queued`
    // when a task that never started leaves every field `resume_target`
    // reads unset, and handles the question-pane case through
    // `resume_target`'s `last_report.step`: pressing `r` there restarts the
    // step the pane was never answered on.
    crate::commands::resume(
        repo,
        pipelines,
        &crate::cli::ResumeArgs {
            task: id.to_string(),
            stage: None,
            message: None,
        },
        None,
    )?;
    Ok(())
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
///
/// `pub(crate)`: `spoolway queue unqueue` is the third caller, for a task on
/// `queued` with no `--force` — the same body a keypress and the command
/// share, exactly as [`resume_task`] is for `r`/`R` and `queue resume`. A
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

/// The move itself, shared with `spoolway queue unqueue`: `task`'s document
/// written to pending with every reserved key dropped, then its queue file
/// removed. The caller decides whether `task` may go — the board's `u` and
/// [`unqueue_task`] only carry a task that has not started, `queue unqueue
/// --force` one whose checkout it has just torn down — and holds the task's
/// lock while it does.
///
/// `None` when a document already sits in `pending/` under this id: that is
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
/// `Dispatcher::start_one` — it would sit in the document for the rest of the
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
/// last frame drew — see [`Board::drawn`].
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

// Every argument is a distinct piece of the board's own state that `frame`
// holds and this builds one frame from; bundling them into a struct just to
// pass one reference would hide that. The same call the codebase's other
// frame builders make.
#[allow(clippy::too_many_arguments)]
fn render(
    repo: &Repo,
    pipelines: &Pipelines,
    phase: Phase,
    stages: &mut BTreeMap<String, String>,
    arrived: &mut BTreeMap<String, Instant>,
    recent: &mut VecDeque<RecentEvent>,
    cursor: &mut Option<String>,
    jobs_next: &mut Option<crate::jobs::ActiveJobsMemo>,
    drawn: &mut Vec<String>,
) -> Result<String> {
    let (tasks, load_problems) = repo.tasks_and_problems()?;
    let graph = Graph::build(&tasks, &repo.archive_dir());
    let mux = crate::mux::backend(repo)?;
    // Read once and passed down: this is a call out to the multiplexer, and
    // `render` is already the one place `draw` makes it from.
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
            // Starts this row's grace clock: `build_rows`' state arm reads it
            // back to tell a task that just handed off, with no lane up for
            // it yet, apart from one genuinely out of workers.
            arrived.insert(id.clone(), Instant::now());
        }
    }
    *stages = current;
    // Dropped alongside `stages` above rather than left to grow forever: a
    // task done or archived never clears its own entry, and the dispatcher
    // stays up for weeks (the same kind of leak `forget_dead_live_sessions`,
    // below, prunes for the session cache).
    arrived.retain(|id, _| stages.contains_key(id));

    // Read once and shared: the rows want it for the OUT column and the footer
    // wants it for the spend, and it is the largest file the board opens.
    let ledger = crate::usage::read_cached(repo);
    let active_rows = build_rows(
        repo,
        &tasks,
        pipelines,
        &graph,
        &lanes,
        &ledger,
        Some(&*arrived),
    )?;

    // Archived tasks stay on the board, dimmed, only as long as their group
    // still has something in the queue — so the groups worth pulling from the
    // archive are exactly the ones already among the active rows.
    let active_groups: BTreeSet<String> =
        active_rows.iter().filter_map(|r| r.group.clone()).collect();
    let mut rows = active_rows;
    rows.extend(done_rows(repo, pipelines, &active_groups)?);
    rows.sort_by(|a, b| a.key().cmp(&b.key()));

    // Lands the cursor on the first row of the first group before the very
    // first frame this board draws is ever shown, rather than leaving a
    // person's first `↑`/`↓` press go to discover it — see `Board::cursor`'s
    // own doc comment. Seeded from `rows`, the same composed list `table`
    // draws below, rather than a second read of the same task files, graph
    // and lane list this function already just did. Re-seeded, not just
    // seeded once, because a group finishing can carry the row the cursor
    // named off the table between two frames with no key pressed — the same
    // "gone id" case `shift_cursor` already treats as no cursor at all.
    let cursor_still_shown = cursor
        .as_deref()
        .is_some_and(|id| rows.iter().any(|row| row.id == id));
    if !cursor_still_shown {
        *cursor = rows.first().map(|row| row.id.clone());
    }
    // What `↑`/`↓` will walk until the next frame replaces it — see
    // [`Board::drawn`]. Taken here, after the sort and from the same composed
    // list `table` draws below, so the order on screen and the order a cursor
    // move steps through are the same list by construction.
    *drawn = rows.iter().map(|row| row.id.clone()).collect();
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
    // No pass clock: `up` already ticks on every redraw, so a board that has
    // not changed in a while is visibly alive without a second clock
    // counting the other way.
    let mut header = vec![
        match phase {
            Phase::Stopping => "dispatcher stopped".to_string(),
            _ => "dispatcher running".to_string(),
        },
        format!("pid {}", std::process::id()),
    ];
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
    // The spool only turns while something on the board reads `Running` — a
    // lane, a command step, or a row still inside its own handoff grace
    // window — so a queue with nothing to do, and nothing about to, prints
    // the still mark instead. A mid-handoff row turning the spool with
    // neither a lane nor a command step behind it is that third case working
    // as the mockup intends, not an animation with nothing behind it.
    let running = rows.iter().any(|row| row.state == State::Running);
    frame.push_str(&masthead(&header.join(" · "), pane, spool_frame(running)));
    frame.push('\n');

    // The job ledger's own rows — every enabled job, ordered by next firing —
    // read once and shared between the empty-queue copy below and the
    // footer's own ledger block. Memoised: this redraws every second and the
    // calendar scan behind it is not cheap per enabled job.
    let active_jobs = crate::jobs::active_jobs_cached(repo, jobs_next);

    if rows.is_empty() {
        // A cron job keeps the dispatcher resident on an empty queue, so the
        // board says why it is still up rather than "nothing queued" — the
        // same opening and closing the plain run prints, in the board's own
        // dim style. The "next: ..." line the plain run inserts between them
        // is left out here: the job ledger in the footer below already names
        // every enabled job's own next firing, and printing it twice would
        // read as two answers to the same question.
        if !active_jobs.is_empty() {
            for line in crate::jobs::staying_up_resident_lines(active_jobs.len()) {
                frame.push_str(&format!(" {DIM}{line}{RESET}\n"));
            }
        } else {
            frame.push_str(&format!(" {DIM}nothing queued{RESET}\n"));
        }
    } else {
        frame.push_str(&table(
            &rows,
            Style::board(pane),
            &totals,
            cursor.as_deref(),
        ));
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
    for line in footer(
        repo,
        pipelines,
        &used,
        &model_used,
        &agent_model,
        &active_jobs,
    ) {
        tail.push_str(&format!(" {line}\n"));
    }
    // The key hint, last of all. Nothing about it depends on whether any row
    // can use it right now; it says what the board can do, not what it would
    // do this frame.
    //
    // Built by `crate::screen::key_hint` — see that function's own doc
    // comment for why this is the one place left to build it, rather than
    // spelling it out by hand — and, now that the cursor reaches the first
    // row on its own, without naming `↑↓`: every screen this project draws
    // leaves the arrows and `q` off its own key line, since a person reads
    // those the same way everywhere.
    tail.push_str(&format!(
        "\n{}\n",
        crate::screen::key_hint(&[
            ("o", "open task"),
            ("r/R", "resume / all"),
            ("p/P", "pause / all"),
            ("u/U", "unqueue / all"),
        ])
    ));

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
/// - It is still the task's own step, or mid-turn. A lane whose task has
///   already moved on and that is not currently `Working` or `Blocked` is a
///   finished pane the multiplexer has not yet reported closed —
///   `Dispatcher::free_finished_lanes`'s own `still_current` check, mirrored
///   here read-only. Counted here it would read as an occupied slot for a
///   pass after the one where `start_lanes`' own `in_flight` already
///   started another lane in its place.
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
        // A lane whose task has already moved off the step this lane's own
        // name carries, and is not mid-turn, is a finished pane the
        // dispatcher's own `free_finished_lanes` would close and stop
        // counting on this very pass — `still_current` there, mirrored here
        // without touching a pane or a task file: reading the board must
        // never do either. Left out, a lane that finished a while ago but
        // whose pane the multiplexer has not yet reported closed reads as an
        // occupied slot here while `start_lanes`' own `in_flight` has
        // already stopped counting it and starts another lane in its place.
        let live = task.stage() == step_id
            || matches!(
                lane.status,
                crate::mux::LaneStatus::Working | crate::mux::LaneStatus::Blocked
            );
        if !live {
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

/// Where a pass out of `blocked` would carry this task, prefixed for the NEXT
/// column — `cleared_block_target` itself, so the board can never name a
/// destination the dispatcher would not actually take it to. Read for both a
/// parked block and a staffed one: the row differs in state and colour, not
/// in where the arrow points.
///
/// Key first, then the command it fires, exactly like a paused row's own
/// `next` below — but only when `resumable` actually offers it: a staffed
/// block clears on its own, and a block still waiting on a dependency or a
/// busy lane of its own has no action here for `[r]` to name.
fn blocked_next(
    task: &crate::task::Task,
    pipeline: &crate::pipeline::Pipeline,
    resumable: bool,
) -> String {
    let target = crate::commands::cleared_block_target(task, pipeline, true);
    match resumable {
        true => format!("[r] → {target} — `spoolway resume {}`", task.id()),
        false => format!("→ {target}"),
    }
}

/// Where resuming a paused task would carry it, if there is an answer to
/// give — `None` only for a task hand-edited onto `paused` with neither
/// `paused_at` nor `parked_from` recorded, which is a step nothing here can
/// guess.
///
/// Two different roads out of `paused`, told apart the same way
/// `commands::report::past_the_gate` tells them apart, through
/// `commands::caught_at`: a pause raised from `blocked` itself resumes to
/// `cleared_block_target`, a caught block or loop-max (`Caught::Blocked`)
/// resumes straight to `blocked` — exactly where it would have landed
/// unheld — and everything else, a plain gated pass or a schedule's caught
/// fail alike, resumes by the step's own `on_pass`. A `parked_from` with no
/// gate — a person's own keypress, or a lane `escalate_clock` gave up on —
/// names nothing to pass: `unpark` sends the task straight back onto that
/// exact step, so this names the step itself rather than whatever comes
/// after it.
fn paused_next(task: &crate::task::Task, pipeline: &crate::pipeline::Pipeline) -> Option<String> {
    if let Some(gated) = task.front.paused_at.as_deref() {
        let step = pipeline.step(gated)?;
        let caught = crate::commands::caught_at(task, gated);
        return Some(match caught {
            None if task.front.blocked_from.as_deref() == Some(gated) => {
                crate::commands::cleared_block_target(task, pipeline, false)
            }
            Some(crate::commands::Caught::Blocked) => crate::pipeline::BLOCKED.to_string(),
            _ => step
                .destination(crate::pipeline::Outcome::Pass)
                .map(str::to_string)?,
        });
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
/// — the same reasoning `render`'s own `too_many_arguments` allow gives.
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
    // here prunes the cache at the end of the render.
    let mut live_sessions: HashSet<String> = HashSet::new();
    let mut rows: Vec<Row> = Vec::new();
    for task in tasks {
        let pipeline = pipelines.for_task(task)?;
        let step = pipeline.step(task.stage());
        // How many times the task has reached the step it is on, whichever
        // route carried it there each time. Computed once, ahead of the
        // match below, because it is a fact about the step a task sits on
        // and not about any one of the states that match branches out into.
        let arrivals = task.rounds_at(task.stage());
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

        // Parked in front of a person, rather than a lane spoolway is about to
        // start there — the one question that decides whether a task on
        // `blocked` reads as a row like any other running step or as a wait.
        let parked_on_blocked = task.stage() == crate::pipeline::BLOCKED
            && !pipeline.blocked_is_staffed(repo.unattended());

        // What happens to this task next. For one that is moving that is the
        // step it goes to; for one that is stuck it is whatever has to happen
        // before it moves at all, which is a person far more often than a step.
        let (state, next, resumable) = match step {
            // The dispatcher's own states. `queued` names the dependency it is
            // held by, which is the only thing worth saying about a task there
            // — a fixed description said the same thing at every one of them.
            _ if task.stage() == crate::pipeline::QUEUED => {
                let dependency = crate::commands::dependency_note(graph, task.id());
                // Every task on `queued` reads `queued`, whatever it is
                // waiting for. A dependency that can never arrive — one
                // that is blocked, one that ended at a terminal step, or a
                // cycle — used to be drawn apart, but the dispatcher has
                // never treated it apart: `graph.ready()` passes over any
                // task whose dependencies are not all `done`, so a dead
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
                    blocked_next(task, pipeline, resumable),
                    resumable,
                )
            }
            // The step a pass would carry it to, not a description of what it
            // is waiting on — the same rule a block reads its resumability
            // by, on exactly the same two conditions. Except when nothing
            // here names a real step at all: no gate (`paused_at`) and no
            // `p`/`escalate_clock` park (`parked_from`) is exactly the shape
            // `park` leaves on a task still on `queued` — see `park`'s own
            // docs — and `resume_target` is the same answer `back_onto_its_step`
            // itself would act on for that shape. Its `queued` means there is
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
                    let resumable =
                        graph.ready(task.id()) && !lane_busy(lanes, &step_ids, task.id());
                    let target = paused_next(task, pipeline);
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
                        (Some(step), true) => {
                            format!("[r] {arrow} {step} — `spoolway resume {}`", task.id())
                        }
                        (Some(step), false) => {
                            format!("{arrow} {step} — `spoolway resume {}`", task.id())
                        }
                        (None, true) => format!("[r] `spoolway resume {}`", task.id()),
                        (None, false) => format!("`spoolway resume {}`", task.id()),
                    };
                    (State::Paused, next, resumable)
                }
            }
            None if task.stage() == crate::pipeline::DONE => {
                (State::Queued, "finished".to_string(), false)
            }
            None => (
                State::Blocked,
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
                //
                // A `gate_at` naming the step this task is on right now is
                // `s`'s own schedule, still live: the step it names is where
                // it is headed regardless of what the pipeline's own
                // `on_pass` would otherwise carry it to, since
                // `commands::report` is about to park it there the moment
                // this step reports at all — whatever it reports, not only a
                // pass.
                let next = if task.front.gate_at.as_deref() == Some(step.id.as_str()) {
                    format!("→ paused after {}", step.id)
                } else if step.id == crate::pipeline::BLOCKED {
                    // `blocked` names no `on_pass` of its own, and its
                    // resumability is a person's question, not a lane's — a
                    // staffed lane working it right now reads the same
                    // destination a cleared block would, and nothing else:
                    // acceptance criterion 1. Not resumable: a lane is
                    // already working this step, so there is no `[r]` action
                    // to offer here.
                    blocked_next(task, pipeline, false)
                } else {
                    match pipeline.next_running_step(&step.id) {
                        // Plain text, no colour: this string is clipped to the
                        // room the pane has left, and a cut through an escape
                        // sequence dyes the rest of the board. No arrival
                        // count here — see `arrivals`, above, which counts
                        // the step this task is *on* rather than the one
                        // named here.
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
            // a running `gate` to whatever agent step ran before it.
            lane_time: match (command_run, live) {
                (Some(key), _) => runs.elapsed(key).map(|ran| ran.as_secs() as i64),
                (None, true) => elapsed,
                (None, false) => lane_time_at(ledger, task.id(), task.stage()),
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
                // Read the same way a live row's is: an archived task's
                // `rounds` are on file same as any other, and `step_text`
                // still draws this count for a done row — only the paint
                // branch in `step_cell` is skipped for one.
                arrivals: task.rounds_at(task.stage()),
                pipeline: archived_pipeline_name(pipelines, task.front.pipeline.as_deref()),
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
/// `paused_at`, the gate that passed; `parked_from`, the step a person
/// interrupted; or `blocked_from`, the step the task stopped on and will
/// return to — the three fields `spoolway resume` reads to tell one stop
/// from the other. Everywhere else, `was` is the step that reported, and its
/// own [`Step::destination`] against `stage` is what tells a pass from a
/// fail — see [`Verdict`].
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
        // `paused_at` names a gate; `parked_from` names a person's own
        // interrupt (see the two fields' own comments in `src/task.rs`).
        // Never both set, so falling back to the second whenever the first
        // is absent picks up the step a `p`-park actually left — the common
        // case now that `p` works from every state, not the rare one it was
        // while `paused_at` alone was ever worth reading here.
        crate::pipeline::PAUSED => (
            task.and_then(|t| {
                t.front
                    .paused_at
                    .clone()
                    .or_else(|| t.front.parked_from.clone())
            }),
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
    // task `p`-parked before it ever started is the real case that reaches
    // this: it was on `queued`, which `park` never records into
    // `parked_from` because no pipeline declares it as a step, so there is
    // no step here to name or score, only the bare word `paused` and a `—`
    // for its position.
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
            pipelines: [("impl_ui".to_string(), pipeline)].into_iter().collect(),
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

    /// A lane whose pane the multiplexer has not yet reported closed, but
    /// whose task has already moved off the step that lane belongs to and is
    /// not mid-turn, is exactly the pane `Dispatcher::free_finished_lanes`
    /// closes on this very pass — `start_lanes`' own `in_flight` has already
    /// stopped counting it before the footer is ever drawn. Counting it here
    /// too would read as a full profile for a pass where the dispatcher
    /// starts another lane in its place.
    #[test]
    fn a_finished_lane_not_yet_closed_gives_its_slot_back() {
        let repo = fixture("slots-finished-not-closed");
        let pipelines = Pipelines::builtin();
        add(&repo, "moved-on", &[], Some("review"));

        let tasks = repo.tasks().unwrap();
        let mut stale = lane("moved-on · implement", &repo.root);
        stale.status = crate::mux::LaneStatus::Idle;
        let used = slots_used(&repo, &tasks, &pipelines, &[stale]);

        assert!(used.agents.is_empty(), "{used:#?}");
    }

    /// The other half of the same check: a lane still `Working` or
    /// `Blocked` counts even once its task has moved off the step that lane
    /// belongs to — a lane runs `spoolway report` mid-turn, so the stage
    /// moves on the spot while the lane keeps talking, and
    /// `free_finished_lanes` leaves a busy lane alone whatever step its task
    /// now reads. Dropping this half and keeping only the idle one would
    /// still pass `a_finished_lane_not_yet_closed_gives_its_slot_back`
    /// above, so it needs its own case.
    #[test]
    fn a_lane_still_mid_turn_counts_even_once_its_task_has_moved_on() {
        let repo = fixture("slots-busy-not-current");
        let pipelines = Pipelines::builtin();
        add(&repo, "moved-on", &[], Some("review"));

        let tasks = repo.tasks().unwrap();
        let mut busy = lane("moved-on · implement", &repo.root);
        busy.status = crate::mux::LaneStatus::Working;
        let used = slots_used(&repo, &tasks, &pipelines, &[busy]);

        // `Pipelines::builtin`'s own test hydration gives every agent step
        // but `review` the `pi` profile — see its own doc comment.
        assert_eq!(used.agents.get("pi").copied(), Some(1), "{used:#?}");
    }

    /// The whole point of the column: a task that is moving is described by
    /// where it goes, and a task that is stuck by what is holding it. Also
    /// where a blocked row, a paused row and a row with arrivals on file are
    /// checked, alongside the plain moving and stuck cases, since all of
    /// them share this one column — the arrival count itself belongs to the
    /// STEP column now, so it is read off `Row::arrivals`, not off `next`.
    #[test]
    fn the_next_column_names_a_step_for_a_moving_task_and_a_reason_for_a_stuck_one() {
        let repo = fixture("next-column");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        add(&repo, "sessions", &["login"], None);

        // Blocked, parked for a person: the step a pass out of `blocked`
        // would actually carry it to — `cleared_block_target`'s own answer —
        // key first, then the command, since every dependency is met and no
        // lane of its own is busy.
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

        // A second arrival at `review`. `add`'s own `set_stage` already
        // banked one; this stands in for a hand-edited second visit.
        add(&repo, "spinner", &[], Some("review"));
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

        assert_eq!(row("wall").next, "[r] → review — `spoolway resume wall`");
        assert_eq!(row("ship").next, "[r] → review — `spoolway resume ship`");
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
        let repo = fixture("paused-next-caught");
        let pipelines = Pipelines::builtin();

        add(&repo, "pause-reach", &[], None);
        let mut fail = repo.task("pause-reach").unwrap();
        fail.front.last_report = Some(crate::task::LastReport {
            step: "review".into(),
            outcome: "fail".into(),
            at: 0,
        });
        fail.front.paused_at = Some("review".into());
        fail.set_stage(crate::pipeline::PAUSED, None);
        fail.save().unwrap();

        add(&repo, "look-holds", &[], None);
        let mut blocked = repo.task("look-holds").unwrap();
        blocked.front.last_report = Some(crate::task::LastReport {
            step: "review".into(),
            outcome: "block".into(),
            at: 0,
        });
        blocked.front.blocked_from = Some("review".into());
        blocked.front.paused_at = Some("review".into());
        blocked.set_stage(crate::pipeline::PAUSED, None);
        blocked.save().unwrap();

        add(&repo, "sweep-own-tabs", &[], None);
        let mut passed = repo.task("sweep-own-tabs").unwrap();
        passed.front.last_report = Some(crate::task::LastReport {
            step: "implement".into(),
            outcome: "pass".into(),
            at: 0,
        });
        passed.front.paused_at = Some("implement".into());
        passed.set_stage(crate::pipeline::PAUSED, None);
        passed.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap();

        assert_eq!(
            row("pause-reach").next,
            "[r] review failed → document — `spoolway resume pause-reach`"
        );
        assert_eq!(
            row("look-holds").next,
            "[r] review blocked → blocked — `spoolway resume look-holds`"
        );
        assert_eq!(
            row("sweep-own-tabs").next,
            "[r] → review — `spoolway resume sweep-own-tabs`",
            "a caught pass reads exactly as an ordinary gate always has"
        );
    }

    /// The mockup `escalate_clock` draws: `parked_from` naming the step a
    /// lane stopped reporting at, with no `paused_at` beside it — there is no
    /// gate here, so `paused_next` must not read `None` and fall back to a
    /// bare `[r] \`spoolway resume <id>\``. `unpark` sends the task straight
    /// back onto `parked_from` itself, and the NEXT column has to name that
    /// same step.
    #[test]
    fn a_task_paused_for_going_quiet_names_the_step_its_resume_carries_it_back_to() {
        let repo = fixture("parked-from-next");
        let pipelines = Pipelines::builtin();
        add(&repo, "release-publishing", &[], None);
        let mut task = repo.task("release-publishing").unwrap();
        task.front.parked_from = Some("review".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = rows.iter().find(|r| r.id == "release-publishing").unwrap();
        assert!(matches!(row.state, State::Paused));
        assert_eq!(
            row.next,
            "[r] → review — `spoolway resume release-publishing`"
        );
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

    /// `Row::arrivals` reads [`crate::task::Frontmatter::arrivals`]'s own
    /// entry for the step directly — no `loop:` bound involved at all, and,
    /// since this is its own map rather than a sum over `rounds`, no route
    /// behind it either: a task that reached a step several times over
    /// several different routes still banks one arrival count, not one per
    /// route.
    #[test]
    fn arrivals_read_the_step_s_own_entry_regardless_of_any_loop_bound() {
        let repo = fixture("arrival-counter");
        let pipelines = Pipelines::builtin();

        // A route the shipped pipeline does bound: the arrival count is
        // banked all the same, since it carries no budget of its own.
        add(&repo, "once", &[], Some("review"));

        // Several arrivals at `implement`, however many different routes
        // carried them — the shipped pipeline gives `implement` no `loop:`
        // of its own, so there is no budget behind this count either.
        add(&repo, "twice", &[], Some("implement"));
        let mut twice = repo.task("twice").unwrap();
        twice.front.arrivals.insert("implement".into(), 5);
        twice.save().unwrap();

        let rows = rows(&repo, &pipelines).unwrap();
        let row = |id: &str| rows.iter().find(|r| r.id == id).unwrap();

        assert_eq!(row("once").arrivals, 1);
        assert_eq!(row("twice").arrivals, 5);
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
        assert!(!frame.contains("nothing queued"), "{frame}");
        // The next firing is not repeated here — the job ledger in the
        // footer below already names it once, for `nightly` itself.
        assert!(!frame.contains("next: nightly,"), "{frame}");
        assert!(
            frame.contains("jobs") && frame.contains("1 active"),
            "{frame}"
        );
        assert!(frame.contains("nightly"), "{frame}");
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

    /// A board says what its keys do — the mockup's own last line — on every
    /// frame it draws, not only once something on the board can use one: the
    /// hint says what the board can do, not what it would do this frame.
    /// There is one board per run now, and it is the one reading keys, so
    /// the hint is never conditional.
    #[test]
    fn the_key_hint_is_drawn_on_every_frame() {
        let repo = fixture("key-hint");
        let pipelines = Pipelines::builtin();

        let mut board = Board::for_test();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(
            frame.contains(
                "[o] open task   [r/R] resume / all   [p/P] pause / all   [u/U] unqueue / all"
            ),
            "an empty queue's own frame should still carry the hint — {frame}"
        );

        add(&repo, "login", &[], Some("implement"));
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(
            frame.contains(
                "[o] open task   [r/R] resume / all   [p/P] pause / all   [u/U] unqueue / all"
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
        let repo = fixture("waiting-but-working");
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
        let repo = fixture("blocked-lane-reads-prompt");
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
    /// `graph.ready()` passes over any task whose dependencies are not all
    /// `done`, so a dead wait sits on `queued` exactly as an ordinary one
    /// does, and the NEXT column names the direct dependency rather than
    /// diagnosing it as stranded.
    #[test]
    fn a_dependency_that_can_never_arrive_still_reads_queued() {
        let repo = fixture("dead-dependency");
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
        let repo = fixture("command-step-running");
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

    /// A task fresh off a handoff, with no lane up for it yet, reads
    /// `Running` for the ordinary gap before the dispatcher's next pass
    /// starts one there — the same gap `spoolway report` opens by writing
    /// the new stage the instant a lane's turn ends. Once [`HANDOFF_GRACE`]
    /// has passed with still no lane, the row falls back to reading
    /// `Queued`, honestly. Faked forward by moving the clock in `grace` back
    /// rather than sleeping through it for real.
    #[test]
    fn a_stage_that_just_changed_reads_running_until_the_grace_window_passes() {
        let repo = fixture("mid-handoff-grace");
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
        // so the resume key is offered, key first, then the command that
        // does the same thing, and the step passing the gate would carry it
        // to, `done` — `handover` is the last step in the pipeline now.
        assert!(row.resumable);
        assert_eq!(row.next, "[r] → done — `spoolway resume ship`");
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
        let repo = fixture("cursor-starts-on-first-row");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let mut board = Board::for_test();
        assert_eq!(board.cursor, None, "nothing has drawn a frame yet");
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        let repo = fixture("cursor-walks-the-drawn-frame");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        add(&repo, "signup", &[], Some("implement"));

        let mut board = Board::for_test();
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        let repo = fixture("cursor-group-finishes");
        let pipelines = Pipelines::builtin();
        add_to(&repo, "login", &[], Some("implement"), Some("auth"));
        add(&repo, "other", &[], None);

        let mut board = Board::for_test();
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        board.cursor = Some("login".to_string());

        // "login" finishes and leaves the queue; nothing else in "auth" is
        // still queued, so the group drops off the board entirely rather
        // than lingering as a done row.
        std::fs::remove_file(repo.queue_dir().join("login.md")).unwrap();

        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert_eq!(
            board.cursor.as_deref(),
            Some("other"),
            "the cursor falls back to the first remaining row once its own row is gone"
        );
    }

    /// The cursor walks the archived rows the board draws too, not just the
    /// live queue: with a group holding one live task and one done one, `↓`
    /// from the live row reaches the done row, and `o` opens its document by
    /// way of `repo.task`'s own archive lookup rather than refusing because
    /// the queue no longer holds the file.
    #[test]
    fn the_cursor_reaches_a_done_row_and_o_opens_its_document() {
        let mut repo = fixture("cursor-reaches-done-row");
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
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        let repo = fixture("resume-deps");
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
        let graph = Graph::build(&tasks, &repo.archive_dir());

        // Held pane, still working — the lane keeps the name of the step it
        // paused at, not `paused` itself, which never starts a lane of its
        // own.
        let mut busy = lane("gate-board · implement", &repo.root);
        busy.status = crate::mux::LaneStatus::Working;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[busy], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();
        assert!(!row.resumable, "{}", row.next);
        assert!(row.next.contains("spoolway resume"), "{}", row.next);

        // Same pane, settled: the round is over and the key comes back.
        let mut settled = lane("gate-board · implement", &repo.root);
        settled.status = crate::mux::LaneStatus::Done;
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[settled], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "gate-board").unwrap();
        assert!(row.resumable, "{}", row.next);
        assert!(row.next.starts_with("[r] "), "{}", row.next);
        assert!(row.next.contains("spoolway resume"), "{}", row.next);
    }

    /// A blocked row that is actually parked for a person — nobody staffs
    /// that step — follows the same dependency and busy-lane rule as a
    /// paused one for whether the key does anything, and now says so the
    /// same way a paused row does: key first, then the command.
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
        let graph = Graph::build(&tasks, &repo.archive_dir());
        let rows = build_rows(&repo, &tasks, &pipelines, &graph, &[], &[], None).unwrap();
        let row = rows.iter().find(|r| r.id == "wall").unwrap();

        assert!(row.resumable, "{}", row.next);
        assert_eq!(row.next, "[r] → review — `spoolway resume wall`");
    }

    /// A task paused by a `--pause`, `--fail` or `--block` from `blocked` —
    /// told apart from an ordinary gate by `blocked_from` naming the same
    /// step as `paused_at` — reads its NEXT column as the step it blocked on,
    /// not past it: `paused_next`'s cleared-block branch now passes
    /// `takes_over: false` to `cleared_block_target`, the same rule
    /// `past_the_gate` resumes it by.
    #[test]
    fn a_cleared_block_row_names_the_step_it_blocked_on_not_past_it() {
        let repo = fixture("paused-cleared-block");
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
            row.next, "[r] → implement — `spoolway resume wall`",
            "never past `implement`, unlike an ordinary gate's own `on_pass`"
        );
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
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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

    /// `r` on a task parked before it ever started — no `paused_at`,
    /// `parked_from` or `blocked_from` at all, exactly what `park` leaves on
    /// a task still on `queued` — still resumes it, back onto `queued` where
    /// the dependency and hook gates apply again (jobs review finding 1):
    /// `resume_task`'s guard reads `stage()`, not the three fields, exactly
    /// so this case is never mistaken for "nothing to resume".
    #[test]
    fn r_on_a_task_parked_before_it_started_puts_it_back_on_queued() {
        let repo = fixture("resume-key-queued-park");
        let pipelines = Pipelines::builtin();
        add(&repo, "never-run", &[], None);
        let mut task = repo.task("never-run").unwrap();
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        let repo = fixture("resume-queued-park-deps");
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

    /// `R` over a run `P` parked leaves nothing stranded on `paused` that
    /// had not started: every row parked off `queued` — even one still
    /// waiting on an unfinished dependency of its own — goes straight back
    /// to `queued`, where that dependency is checked the ordinary way.
    #[test]
    fn shift_r_sends_every_queued_park_back_to_queued_whatever_its_dependency() {
        let repo = fixture("resume-all-queued-parks");
        let pipelines = Pipelines::builtin();
        add(&repo, "blocker", &[], Some("implement"));
        add(&repo, "never-run", &["blocker"], None);
        let mut task = repo.task("never-run").unwrap();
        park(&mut task, "paused from the board", false);
        task.save().unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
            .unwrap();

        let task = repo.task("never-run").unwrap();
        assert_eq!(task.stage(), crate::pipeline::QUEUED, "{}", task.stage());
        assert_eq!(task.front.parked_from, None);
        assert!(task.front.rounds.is_empty(), "{:?}", task.front.rounds);
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
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        // `handover` is the default pipeline's command step.
        add(&repo, "login", &[], Some("handover"));

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        runs.start(&key, "sleep 30", &repo.root, &BTreeMap::new())
            .unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("pause all"), "{frame}");
        assert!(frame.contains("login · handover"), "{frame}");
        assert!(frame.contains("command"), "{frame}");

        // Declining: nothing about the run or the task changes.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Esc)
            .unwrap();
        assert_eq!(runs.state(&key), crate::command_step::RunState::Running);
        assert_eq!(repo.task("login").unwrap().stage(), "handover");

        // Asking again and confirming this time kills the run and parks the
        // task on `parked_from`, the same record an interrupt leaves.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();
        assert_ne!(runs.state(&key), crate::command_step::RunState::Running);
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.parked_from.as_deref(), Some("handover"));
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
        // `handover` is the default pipeline's command step.
        add(&repo, "login", &[], Some("handover"));
        add(&repo, "other", &[], Some("implement"));

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        runs.start(&key, "sleep 30", &repo.root, &BTreeMap::new())
            .unwrap();

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert_eq!(board.cursor.as_deref(), Some("login"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("pause login"), "{frame}");
        assert!(!frame.contains("pause all"), "{frame}");
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

    /// `P` opens a confirm panel over a live agent lane this run owns —
    /// naming its turn as an abort just as it would a command run — and only
    /// `enter` interrupts it and parks its task, exercised against the
    /// headless backend, whose "interrupt" is ending the turn's process
    /// outright, the same as [`Mux::stop_lane`]. `esc` leaves the lane
    /// running untouched, which a plain lane list check alone would not
    /// catch: the old shape interrupted the lane *before* this panel ever
    /// opened, so this is also what pins that it no longer does.
    #[test]
    fn pressing_shift_p_opens_a_panel_over_a_live_headless_lane_and_only_enter_interrupts_it() {
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
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("pause all"), "{frame}");
        assert!(frame.contains("login · implement"), "{frame}");
        assert!(frame.contains("agent"), "{frame}");

        // `esc` first: the lane is still live and the task untouched.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Esc)
            .unwrap();
        assert!(mux.list_lanes().unwrap().iter().any(|l| l.name == name));
        assert_eq!(repo.task("login").unwrap().stage(), "implement");

        // Asking again and confirming this time interrupts the lane.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
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

    /// `p` on the cursor's own live agent turn — the mockup's own panel —
    /// names the step, the word `agent`, an elapsed time, and the two lines
    /// that say the turn is interrupted rather than killed. Nothing is
    /// touched before `enter` answers it: the task file it would write is
    /// still exactly what it was when the panel opened.
    #[test]
    fn pressing_p_on_a_live_agent_lane_names_the_turn_and_says_it_is_only_interrupted() {
        let mut repo = fixture("pause-cursor-agent-lane");
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
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        assert_eq!(board.cursor.as_deref(), Some("login"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();

        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
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

    /// `s` on `p`'s own panel leaves the live turn running and writes
    /// `gate_at` naming the step instead of interrupting anything: the board
    /// answers back to `Browsing`, the task's stage never moves off
    /// `implement`, and the lane the panel named is still there afterwards.
    #[test]
    fn pressing_s_on_the_pause_panel_schedules_a_gate_and_leaves_the_lane_running() {
        let mut repo = fixture("schedule-pause-cursor");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));

        let (mux, name) = live_headless_lane(&repo);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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

    /// Pressing `s` a second time, on a fresh panel over the same still-live
    /// step, clears the schedule it wrote rather than writing it again —
    /// the mockup's "pressing `s` again... clears it".
    #[test]
    fn pressing_s_again_clears_a_scheduled_pause() {
        let mut repo = fixture("schedule-pause-toggle");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("implement"));
        let (_mux, _name) = live_headless_lane(&repo);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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

    /// `s` on `P`'s panel schedules every task it named an abort for and
    /// parks the rest of the run at once, exactly as `enter` would — there
    /// is no step in flight to wait out for those.
    #[test]
    fn pressing_s_on_shift_p_schedules_the_live_task_and_parks_the_rest() {
        let repo = fixture("schedule-pause-all");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("handover"));
        add(&repo, "idle", &[], Some("implement"));

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        runs.start(&key, "sleep 30", &repo.root, &BTreeMap::new())
            .unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('s'))
            .unwrap();

        assert!(matches!(board.mode, BoardMode::Browsing));
        assert_eq!(runs.state(&key), crate::command_step::RunState::Running);
        let login = repo.task("login").unwrap();
        assert_eq!(login.stage(), "handover");
        assert_eq!(login.front.gate_at.as_deref(), Some("handover"));
        assert_eq!(repo.task("idle").unwrap().stage(), crate::pipeline::PAUSED);

        runs.stop(&key);
    }

    /// The NEXT column names a scheduled pause rather than the pipeline's
    /// own route once `gate_at` matches the step a running task sits on.
    #[test]
    fn the_next_column_names_a_scheduled_pause() {
        let repo = fixture("schedule-pause-next-column");
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
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
    /// the board leaves nothing behind for: `steps`, `rounds`, `arrivals`
    /// and `arrived_from` come back byte-for-byte, `rounds_via("implement",
    /// "paused")` stays zero, and the row's own NEXT column carries no loop
    /// counter across it.
    #[test]
    fn pressing_p_then_shift_r_round_trips_a_task_without_banking_a_lap() {
        let mut repo = fixture("park-round-trip");
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
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Down)
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('p'))
            .unwrap();
        // A live agent lane now opens a confirm panel too, same as `P`.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();
        assert_eq!(repo.task("login").unwrap().stage(), crate::pipeline::PAUSED);

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
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
        let repo = fixture("pause-nothing-live");
        let pipelines = Pipelines::builtin();
        add(&repo, "queued-task", &[], None);
        add(&repo, "gap-task", &[], Some("implement"));

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

    /// `p` and `P` are both no-ops on a `paused` row — already stopped and
    /// already waiting on a person, not a state to park over again.
    #[test]
    fn pressing_p_or_shift_p_on_a_paused_row_does_nothing() {
        let repo = fixture("pause-noop-states");
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

        // `P` across the run leaves it alone too: the stage survives whole.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();
        assert_eq!(
            repo.task("already-paused").unwrap().stage(),
            crate::pipeline::PAUSED
        );
    }

    /// `p` on a `blocked` row with nothing live parks it at once, no panel —
    /// the same road `queued` and a real step in the gap between two lanes
    /// already take: a `blocked` row is not a state `p` skips any more, only
    /// `paused` is. `blocked_from` survives the park untouched, sitting
    /// beside the fresh `parked_from: blocked` it now carries too.
    #[test]
    fn pressing_p_on_a_blocked_row_with_nothing_live_parks_it_at_once() {
        let repo = fixture("pause-blocked-idle");
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

    /// `P` widens with `p`: an idle `blocked` row is one more thing `P`
    /// parks outright, through the same [`is_pausable`] `park_every_pausable`
    /// reads — not a hole `p`'s own tests leave open on their own, since `p`
    /// already goes through the same predicate, but the acceptance criterion
    /// names `P` by itself too.
    #[test]
    fn pressing_shift_p_parks_an_idle_blocked_row_too() {
        let repo = fixture("pause-all-blocked-idle");
        let pipelines = Pipelines::builtin();
        add(&repo, "stuck", &[], Some(crate::pipeline::BLOCKED));
        let mut task = repo.task("stuck").unwrap();
        task.front.blocked_from = Some("implement".into());
        task.save().unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
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
        let mut repo = fixture("pause-blocked-live-lane");
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
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
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

    /// `P` with one thing live and one thing not: the panel names only the
    /// one abort — no count of the rest of the run any more — and answering
    /// `enter` parks both, the live one through its own abort, the idle one
    /// straight onto `paused`.
    #[test]
    fn pressing_shift_p_with_one_live_and_one_idle_task_parks_both() {
        let repo = fixture("pause-all-mixed");
        let pipelines = Pipelines::builtin();
        add(&repo, "login", &[], Some("handover"));
        add(&repo, "idle", &[], Some("implement"));

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        runs.start(&key, "sleep 30", &repo.root, &BTreeMap::new())
            .unwrap();

        let mut board = Board::for_test();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('P'))
            .unwrap();
        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("login · handover"), "{frame}");
        assert!(!frame.contains("more task"), "{frame}");
        assert!(!frame.contains("nothing"), "{frame}");

        // A stray key neither `enter`, `s` nor `esc` leaves the panel open
        // and nothing acted on.
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('x'))
            .unwrap();
        assert!(!matches!(board.mode, BoardMode::Browsing));
        assert_eq!(runs.state(&key), crate::command_step::RunState::Running);

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
            .unwrap();
        assert_ne!(runs.state(&key), crate::command_step::RunState::Running);
        assert_eq!(repo.task("login").unwrap().stage(), crate::pipeline::PAUSED);
        assert_eq!(repo.task("idle").unwrap().stage(), crate::pipeline::PAUSED);

        runs.stop(&key);
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
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        // writes nothing; it has to accept the document exactly as it is.
        let contract_args = crate::cli::TaskContractArgs {
            from: vec![pending_doc.display().to_string()],
            base: None,
        };
        crate::commands::task_contract(&repo, &pipelines, &contract_args, &repo.root).unwrap();

        // And the document a fresh `queue add --from` accepts unchanged,
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
        let repo = fixture("unqueue-esc");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[], None);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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

    /// `u` on a task a still-queued task depends on opens a panel naming
    /// both, the dependent marked with what it depends on, and `enter`
    /// carries both back to pending, leaving neither in the queue.
    #[test]
    fn pressing_u_on_a_task_a_queued_dependent_names_carries_both() {
        let repo = fixture("unqueue-depended-on");
        let pipelines = Pipelines::builtin();
        add(&repo, "drop-walk", &[], None);
        add(&repo, "chain-refusals", &["drop-walk"], None);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        // `drop-walk` is the dependency, so it sorts first, and the frame
        // above already landed the cursor on it.
        assert_eq!(board.cursor.as_deref(), Some("drop-walk"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();

        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("unqueue drop-walk"), "{frame}");
        assert!(frame.contains("2 documents go back to:"), "{frame}");
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
        let repo = fixture("unqueue-chain-two-hops");
        let pipelines = Pipelines::builtin();
        add(&repo, "alpha", &[], None);
        add(&repo, "beta", &["alpha"], None);
        add(&repo, "gamma", &["beta"], None);
        add(&repo, "unrelated", &[], None);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
        assert_eq!(board.cursor.as_deref(), Some("alpha"));
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('u'))
            .unwrap();

        let frame = strip(&board.frame(&repo, &pipelines, Phase::Waiting).unwrap());
        assert!(frame.contains("unqueue alpha"), "{frame}");
        assert!(frame.contains("3 documents go back to:"), "{frame}");
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
        let repo = fixture("unqueue-cursor-advances");
        let pipelines = Pipelines::builtin();
        add(&repo, "chain-refusals", &[], None);
        add(&repo, "month-instant", &[], None);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
        let repo = fixture("unqueue-cursor-clears");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[], None);

        let mut board = Board::for_test();
        // The cursor walks the last frame's own rows, so the board has to
        // have drawn one — the dispatch loop draws before it reads a key.
        board.frame(&repo, &pipelines, Phase::Waiting).unwrap();
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
            .on_key(&repo, &pipelines, crate::screen::Key::Enter)
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

    /// The letter that opens the resume-all, unqueue and unqueue-all panels
    /// no longer answers them, the same as any other unrecognised key —
    /// `enter` is the only key that does now, proven on each panel by the
    /// tests above this one.
    #[test]
    fn the_old_confirming_letter_no_longer_answers_any_panel() {
        let repo = fixture("panels-ignore-their-own-letter");
        let pipelines = Pipelines::builtin();
        add(&repo, "gate-board", &[], None);
        let mut gated = repo.task("gate-board").unwrap();
        gated.front.paused_at = Some("implement".into());
        gated.set_stage(crate::pipeline::PAUSED, None);
        gated.save().unwrap();
        add(&repo, "solo", &[], None);

        let mut board = Board::for_test();

        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
            .unwrap();
        board
            .on_key(&repo, &pipelines, crate::screen::Key::Char('R'))
            .unwrap();
        assert!(!matches!(board.mode, BoardMode::Browsing), "resume-all");
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
}
