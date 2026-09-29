//! Bare `spoolway`: one screen holding four tabs — dispatch, queue, jobs and
//! eval, in that order — with one terminal guard, one key reader and one quit.
//!
//! A tab carries no logic of its own. Each one calls the code its CLI command
//! already draws with — `commands::queue_tab`, `commands::jobs_tab`,
//! `eval::tab` and the board's own [`crate::status::Board`] — and this module
//! adds only the strip across the top and the keys that move between tabs.
//! The one exception is the dispatch tab's `enter`, which starts a
//! `spoolway dispatch` child ([`super::dispatcher`]) behind the gates that
//! command asks, and stops it behind a popup asking whether running steps
//! finish or are interrupted — each drawn here as a popup over the board —
//! and, once it has started, a keyless popup over the dispatcher's own first
//! checks, which hands every key through to the board and the shell as if it
//! were not up.
//!
//! A tab's screen keeps its own loop. When it reads `←`, `→` or `q` with no
//! popup or sub-mode of its own open — [`leave_on`] — it hands a [`Leave`]
//! back here and returns, and this loop opens the neighbouring tab. That is
//! what lets every key the tab's screen already reads, inside its filters or
//! its routines view, keep its own meaning without this module knowing what
//! any of them are. A sub-mode that reads nothing of its own on a key may
//! still hand that one key over — the queue tab's routines view does for
//! `q`, and its key line says so.
//!
//! What the screen has to say the moment it opens — the sync notice, any
//! override the load left out and the update notice, which every other
//! command prints ahead of itself — is handed to the queue tab, the one the
//! screen opens on, to show as popups over it: printed ahead of the screen,
//! the first frame would wipe it
//! before anybody could read it. See [`OnOpen`].
//!
//! Which tab is open lives in a thread-local rather than being threaded
//! through every screen's own `render`: the strip and the three rows it takes
//! off the terminal's height are read deep inside each screen's own layout
//! code — `commands::queue::layout`, `eval::frame_rows`, the board's own
//! `pane_height` — and a screen drawn with no shell around it, from its CLI
//! command or a unit test, must draw exactly what it drew before. Thread-local
//! rather than process-global, so a test that hosts a screen on its own thread
//! never leaks a strip into another test's frame running beside it.

use std::cell::Cell;
use std::io::Write;
use std::path::Path;

use anyhow::Result;

use super::{Key, PollableRead, RawStdin, read_key};
use crate::pipeline::Pipelines;
use crate::repo::Repo;

/// What the screen shows the moment it opens, over the queue tab — see the
/// module doc. Each is `None` when there is nothing to say.
#[derive(Debug, Default)]
pub(crate) struct OnOpen {
    /// The sync notice's popup — [`crate::gate::sync_popup`].
    pub(crate) sync: Option<Vec<String>>,
    /// The "override ignored" popup — [`crate::commands::ignored_popup`].
    pub(crate) ignored: Option<crate::commands::IgnoredPopup>,
    /// The update notice's line — [`crate::release::notice`].
    pub(crate) update: Option<String>,
}

/// One tab of the shell, in strip order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Dispatch,
    Queue,
    Jobs,
    Eval,
}

/// Every tab, left to right — the order the strip draws them and `←`/`→`
/// walk them in.
const TABS: [Tab; 4] = [Tab::Dispatch, Tab::Queue, Tab::Jobs, Tab::Eval];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Dispatch => "dispatch",
            Tab::Queue => "queue",
            Tab::Jobs => "jobs",
            Tab::Eval => "eval",
        }
    }

    /// The tab beside this one, or this one again at either end: `←` on
    /// dispatch and `→` on eval have no neighbour to reach, and wrapping
    /// round would put the two furthest tabs one key apart.
    fn toward(self, way: Toward) -> Tab {
        let at = TABS.iter().position(|tab| *tab == self).unwrap_or(0);
        match way {
            Toward::Left => TABS[at.saturating_sub(1)],
            Toward::Right => TABS[(at + 1).min(TABS.len() - 1)],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Toward {
    Left,
    Right,
}

/// Why a tab's screen handed control back to the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leave {
    /// `q`, `ctrl-c`, or the input running out: the whole screen ends.
    Quit,
    /// `←` or `→`: open the neighbouring tab.
    Switch(Toward),
}

thread_local! {
    /// The tab the shell has open on this thread, or `None` wherever a
    /// screen is drawn on its own — see the module doc.
    static HOSTED: Cell<Option<Tab>> = const { Cell::new(None) };
}

/// Marks `tab` as the one open for as long as this lives, and clears it on
/// drop — so an early `?` out of a tab's screen can never leave a stale tab
/// behind for whatever draws next on this thread.
pub(crate) struct Hosting;

impl Hosting {
    pub(crate) fn open(tab: Tab) -> Hosting {
        HOSTED.with(|cell| cell.set(Some(tab)));
        Hosting
    }
}

impl Drop for Hosting {
    fn drop(&mut self) {
        HOSTED.with(|cell| cell.set(None));
    }
}

/// The tab open on this thread, if a shell is hosting one.
pub(crate) fn hosted() -> Option<Tab> {
    HOSTED.with(Cell::get)
}

/// The rows the strip takes off the top of the terminal: one blank row above
/// it, the strip itself and one blank row under it, as the mockup draws —
/// always [`strip`]'s own length. Zero with no shell around the screen, so a
/// screen drawn by its own CLI command keeps every row it had.
pub(crate) fn strip_rows() -> usize {
    match hosted() {
        Some(_) => 3,
        None => 0,
    }
}

/// What `key` means to the shell, read by a tab's screen only while nothing
/// of its own is open — a popup, a filter, a picker — or for a key the open
/// sub-mode reads nothing of its own on, so every key a sub-mode reads keeps
/// its meaning there. `None` with no shell around the screen at all — a
/// standalone `run_screen` driven directly by a test has no neighbouring tab
/// to go to, and `q` there keeps whatever meaning it already had.
pub(crate) fn leave_on(key: Key) -> Option<Leave> {
    hosted()?;
    match key {
        Key::Left => Some(Leave::Switch(Toward::Left)),
        Key::Right => Some(Leave::Switch(Toward::Right)),
        Key::Char('q') => Some(Leave::Quit),
        _ => None,
    }
}

/// The key line's own `q` pair, for a screen to append while it is hosted —
/// empty otherwise, since a screen drawn on its own does not read `q` as quit.
pub(crate) fn quit_hint() -> &'static [(&'static str, &'static str)] {
    match hosted() {
        Some(_) => &[("q", "quit")],
        None => &[],
    }
}

/// How wide the strip is drawn with no terminal to measure: the width every
/// mockup of this screen is drawn to.
const FALLBACK_WIDTH: usize = 100;

/// The gap between two neighbouring tab slots on the strip. A slot is a
/// label with one column either side for the open tab's brackets, so with
/// these six spaces every label lands in the column it held when the strip
/// marked the open tab by colour and spaced its labels eight apart.
const SLOT_GAP: &str = "      ";

/// The strip with one blank row above it and one under it, for a screen to
/// put above its own frame — or nothing at all with no shell hosting one. The
/// row above keeps the strip off the terminal's top edge.
///
/// Three rows, the count [`strip_rows`] takes off every tab's height: a line
/// added or dropped here without it pushes each tab's frame past the
/// terminal's last row, or leaves a row unused under it.
pub(crate) fn strip() -> Vec<String> {
    match hosted() {
        Some(open) => {
            let width = terminal_size::terminal_size()
                .map(|(w, _)| w.0 as usize)
                .unwrap_or(FALLBACK_WIDTH);
            vec![String::new(), strip_line(open, width), String::new()]
        }
        None => Vec::new(),
    }
}

/// `frame` with the strip on top of it — see [`strip`].
pub(crate) fn under_strip(frame: Vec<String>) -> Vec<String> {
    let mut lines = strip();
    lines.extend(frame);
    lines
}

/// The strip itself, `width` columns wide: the four tabs centred, each in a
/// slot one column wider than its label on either side, and `←` and `→`
/// one space outside the first and last slot. The open tab fills its slot's
/// spare columns with `[` and `]`, the same mark the key line gives a key;
/// every other slot leaves them blank. The whole line is bold, the weight the
/// wordmark is drawn in, and reset on the same line so the bold never runs
/// into the row under it. Nothing is coloured or dim, and the open tab is not
/// bolder than the rest — the brackets alone say which tab is open.
///
/// At least two columns stay before `←` however narrow the terminal, and
/// nothing is written past the `→`: a row that reaches the terminal's last
/// column is followed by a newline the terminal has already wrapped for, and
/// the frame under it comes out one row lower than it was measured for. The
/// bold and reset codes take no column, so they are left out of every width
/// measured here.
fn strip_line(open: Tab, width: usize) -> String {
    use crate::status::{BOLD, RESET};

    let slots: Vec<String> = TABS
        .iter()
        .map(|tab| match *tab == open {
            true => format!("[{}]", tab.label()),
            false => format!(" {} ", tab.label()),
        })
        .collect();
    let span =
        slots.iter().map(|s| s.chars().count()).sum::<usize>() + SLOT_GAP.len() * (slots.len() - 1);
    // Two columns of margin, the arrow, and one space before the first slot.
    let start = (width.saturating_sub(span) / 2).max(4);
    format!(
        "{BOLD}{}← {} →{RESET}",
        " ".repeat(start - 2),
        slots.join(SLOT_GAP)
    )
}

/// Open the screen: bare `spoolway` in a terminal.
///
/// One `spoolway` at a time per project: refuses with exactly `Dispatcher
/// already running` while [`crate::repo::Repo::screen_lock_file`] or
/// [`crate::repo::Repo::lock_file`] names a live process — a second screen
/// in this project, or a dispatcher already running from the CLI. The lock
/// this screen takes on the way in is what a second `spoolway` or
/// `spoolway dispatch` sees; it is released on drop, whenever and however
/// this returns, so it is gone the moment the screen quits.
///
/// Holds the one [`crate::platform::TermGuard`] every tab draws under and
/// installs the one `ctrl-c` handler ahead of it: a `ctrl-c` between the two
/// would otherwise kill the process with the terminal already raw and
/// nothing left to restore it. Opens on the queue tab, with the sync notice
/// and `update` — the update notice's line, which `main` holds back from
/// stderr for this — as popups over it.
///
/// A dispatcher the dispatch tab started stops when this returns, however it
/// returns — see [`super::dispatcher::Dispatcher`]'s own `Drop`.
pub(crate) fn run(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &Path,
    update: Option<String>,
) -> Result<()> {
    // `false`: bare `spoolway` is never the `--from-screen` child, so both
    // locks are checked, the same as a typed `spoolway dispatch`. Shared
    // with `commands::dispatch` rather than written out a second time here,
    // so the one line either refusal prints — [`crate::commands::ALREADY_RUNNING`]
    // — cannot drift between the two.
    if crate::commands::already_running(repo, false)? {
        println!("{}", crate::commands::ALREADY_RUNNING);
        return Ok(());
    }
    let _lock = crate::lock::Lock::acquire(&repo.screen_lock_file(), false, None)?;
    // Asked before the terminal is taken: a scan that fails is reported the
    // way the printed notice reports it, as this command's own error.
    let on_open = OnOpen {
        sync: crate::gate::sync_popup(repo)?,
        ignored: crate::commands::ignored_popup(repo)?,
        update,
    };

    crate::platform::stop::catch_interrupt();
    let _term = crate::platform::TermGuard::screen();
    let mut stdin = RawStdin;
    let mut stdout = std::io::stdout();
    host(repo, pipelines, cwd, on_open, &mut stdin, &mut stdout)
}

/// The tab loop, apart from the terminal it runs on so a test can drive it
/// over a scripted input.
fn host(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &Path,
    mut on_open: OnOpen,
    input: &mut impl PollableRead,
    out: &mut impl Write,
) -> Result<()> {
    // Built once and kept across visits, so the board's RECENT ticker and
    // its cursor survive a trip to another tab and back — and so does the
    // dispatcher the tab started, which keeps running while another tab is
    // open. An inert guard: the terminal is already this screen's, taken
    // once above.
    let mut board = crate::status::Board::hosted();
    let mut dispatch = DispatchTab::default();
    let mut tab = Tab::Queue;
    // One writer for the whole hosted session, not one per tab, so the idle
    // skip it gives every screen (see `frame_writer`'s own doc) also covers
    // the moment of switching between them: whichever tab is about to
    // redraw calls `forget` below on every entry, so its own first frame is
    // never skipped as "already on screen" against whatever a different
    // tab, or a one-shot popup drawn outside this writer entirely, last
    // left there instead.
    let mut writer = crate::screen::frame_writer::FrameWriter::new();

    loop {
        let leave = {
            let _hosting = Hosting::open(tab);
            writer.forget();
            match tab {
                Tab::Dispatch => dispatch_tab(
                    repo,
                    pipelines,
                    cwd,
                    &mut board,
                    &mut dispatch,
                    &mut writer,
                    input,
                    out,
                )?,
                // Taken on the first visit, so a notice is shown once and
                // not again on every return to the tab.
                Tab::Queue => crate::commands::queue_tab(
                    repo,
                    pipelines,
                    cwd,
                    std::mem::take(&mut on_open),
                    &mut writer,
                    input,
                    out,
                )?,
                Tab::Jobs => {
                    crate::commands::jobs_tab(repo, pipelines, cwd, &mut writer, input, out)?
                }
                Tab::Eval => crate::eval::tab(repo, pipelines, &mut writer, input, out)?,
            }
        };
        // A caught `ctrl-c` ends every tab's own wait with `None` — see
        // `wait_key` — which each reads as `Quit` already; checked here too
        // so a tab that answered with a switch in the same instant cannot
        // open another one.
        if crate::platform::stop::asked() {
            return Ok(());
        }
        match leave {
            Leave::Quit => return Ok(()),
            Leave::Switch(way) => tab = tab.toward(way),
        }
    }
}

/// What the dispatch tab keeps between visits beyond the board itself: the
/// dispatcher it started, if any, and the popup it has open.
#[derive(Default)]
struct DispatchTab {
    child: Option<super::dispatcher::Dispatcher>,
    popup: Option<Popup>,
}

/// A popup the dispatch tab draws over the board. Each one but `Starting`
/// reads every key until it is answered, the same as the board's own
/// confirm panels.
enum Popup {
    /// The overrides gate `enter` asks first, when there is a layer to name.
    Overrides(crate::commands::GatePopup),
    /// Doctor's cheap findings, asked after the overrides gate.
    Warnings(crate::commands::GatePopup),
    /// Why the dispatcher ended, or never started. `enter` closes it.
    Ended(Vec<String>),
    /// The dispatcher just started and has not yet claimed anything: its own
    /// checks — herdr, git, the queue — run before its first pass, and the
    /// board has nothing to show for them. Takes no key and names none;
    /// [`DispatchTab::reap`] closes it once the first pass has claimed a
    /// slot or found nothing to claim. `since` is when the child was
    /// started, which is what that pass's files are timed against.
    Starting {
        panel: Vec<String>,
        since: std::time::SystemTime,
    },
    /// How to stop the running dispatcher — [`super::dispatcher::stop_panel`].
    Stop(Vec<String>),
}

impl Popup {
    fn panel(&self) -> &[String] {
        match self {
            Popup::Overrides(gate) | Popup::Warnings(gate) => &gate.panel,
            Popup::Ended(panel) | Popup::Stop(panel) | Popup::Starting { panel, .. } => panel,
        }
    }

    /// Whether this popup reads keys. `Starting` does not: the board's
    /// keys, `←`, `→` and `q` all work under it, and it closes on its own.
    fn reads_keys(&self) -> bool {
        !matches!(self, Popup::Starting { .. })
    }
}

/// The popup [`Popup::Starting`] draws: a title and one line, and no key
/// line, since nothing answers it.
fn starting_panel() -> Vec<String> {
    super::boxed(
        "Starting dispatcher",
        &["checking herdr, git, queue…".to_string()],
    )
}

/// Whether the dispatcher started at `since` has finished its first pass's
/// claims: a boot mark written since — see [`crate::claim`] — or, for a pass
/// that found nothing to claim, `lanes.json`, which every pass writes on its
/// way out. The child's stdout goes nowhere, so its files are all the tab
/// has to go on. Both are written well after `since`, past the child's own
/// start checks, so a filesystem clock a few milliseconds coarse cannot
/// date either ahead of it.
fn first_pass_seen(repo: &Repo, since: std::time::SystemTime) -> bool {
    crate::claim::marked_since(repo, since)
        || std::fs::metadata(repo.lanes_file())
            .and_then(|meta| meta.modified())
            .is_ok_and(|at| at >= since)
}

impl DispatchTab {
    /// Whether the key line offers to stop rather than start. A child
    /// already asked to stop still counts until it has exited: offering to
    /// start one then would be a promise `enter` cannot keep, since the
    /// lock is still its.
    fn dispatching(&self) -> bool {
        self.child.is_some()
    }

    /// Forget a child that has exited, and open the popup saying why if it
    /// ended without being asked to — over `Starting`, if the child ended
    /// before its first pass. A child still running closes `Starting` once
    /// its first pass has claimed.
    fn reap(&mut self, repo: &Repo) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if let Some(ended) = child.ended(&repo.lock_file()) {
            self.child = None;
            match ended {
                Some(panel) => self.popup = Some(Popup::Ended(panel)),
                // Stopped from the stop popup before its first pass:
                // nothing is starting any more.
                None => {
                    if matches!(self.popup, Some(Popup::Starting { .. })) {
                        self.popup = None;
                    }
                }
            }
            return;
        }
        if let Some(Popup::Starting { since, .. }) = &self.popup
            && first_pass_seen(repo, *since)
        {
            self.popup = None;
        }
    }

    /// `enter` with no key-reading popup open: over a running child, ask how
    /// to stop it — every time, even with nothing running, and in place of
    /// the keyless `Starting` popup if that is still up; with none running,
    /// ask the start gates, each only when it has something to say, and
    /// start one. A child already asked to stop is left to finish going, and
    /// `enter` over it does nothing: there is nothing left to ask. Stopped
    /// before its first pass, the child is gone with nothing starting, and
    /// [`DispatchTab::reap`] clears any `Starting` popup once it has exited.
    fn enter(&mut self, repo: &Repo, pipelines: &Pipelines, cwd: &Path) {
        match self.child.as_ref() {
            Some(child) if child.stopping() => {}
            Some(_) => self.popup = Some(Popup::Stop(super::dispatcher::stop_panel())),
            None => self.overrides_then_start(repo, pipelines, cwd),
        }
    }

    /// Stop the child, if one is still up — the stop popup's answer either
    /// way. A child that exited while the popup was open has already been
    /// reaped, and its own popup replaced this one.
    fn stop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            child.stop();
        }
    }

    fn overrides_then_start(&mut self, repo: &Repo, pipelines: &Pipelines, cwd: &Path) {
        match crate::commands::overrides_popup(repo) {
            Ok(Some(gate)) => self.popup = Some(Popup::Overrides(gate)),
            Ok(None) => self.warnings_then_start(repo, pipelines, cwd),
            Err(err) => {
                self.popup = Some(Popup::Ended(super::dispatcher::popup(
                    false,
                    &format!("{err:#}"),
                )))
            }
        }
    }

    fn warnings_then_start(&mut self, repo: &Repo, pipelines: &Pipelines, cwd: &Path) {
        match crate::commands::warnings_popup(repo, pipelines) {
            Some(gate) => self.popup = Some(Popup::Warnings(gate)),
            None => self.start(cwd),
        }
    }

    fn start(&mut self, cwd: &Path) {
        let since = std::time::SystemTime::now();
        self.started(super::dispatcher::Dispatcher::start(cwd), since);
    }

    /// What [`DispatchTab::start`] got back from starting the child at
    /// `since`: the child and `Starting` over the board — the popup is up on
    /// the very frame after the keypress, before the child has done
    /// anything — or the popup saying why it did not start. Apart from
    /// `start` so a test can hand it a stand-in child.
    fn started(
        &mut self,
        child: Result<super::dispatcher::Dispatcher>,
        since: std::time::SystemTime,
    ) {
        match child {
            Ok(child) => {
                self.child = Some(child);
                self.popup = Some(Popup::Starting {
                    panel: starting_panel(),
                    since,
                });
            }
            Err(err) => {
                self.popup = Some(Popup::Ended(super::dispatcher::popup(
                    false,
                    &format!("{err:#}"),
                )))
            }
        }
    }

    /// A key read while a popup is open. `enter` on a gate goes on to the
    /// next one, or starts; `x` hides the gate until what it names changes
    /// and then goes on the same way; `esc` backs out having started
    /// nothing. On the stop popup, `enter` stops the child and leaves every
    /// running step to finish, `i` interrupts them first, and `esc` leaves
    /// the dispatcher running. Every other key is ignored.
    fn answer(&mut self, repo: &Repo, pipelines: &Pipelines, cwd: &Path, key: Key) {
        let Some(popup) = self.popup.take() else {
            return;
        };
        match (popup, key) {
            (Popup::Overrides(gate), Key::Char('x' | 'X')) => {
                // Best-effort: an acknowledgement that could not be written
                // only means the gate asks again next time.
                let _ = gate.hide(repo);
                self.warnings_then_start(repo, pipelines, cwd);
            }
            (Popup::Overrides(_), Key::Enter) => self.warnings_then_start(repo, pipelines, cwd),
            (Popup::Warnings(gate), Key::Char('x' | 'X')) => {
                let _ = gate.hide(repo);
                self.start(cwd);
            }
            (Popup::Warnings(_), Key::Enter) => self.start(cwd),
            (Popup::Stop(_), Key::Enter) => self.stop(),
            (Popup::Stop(_), Key::Char('i' | 'I')) => {
                // Stopped whatever came of the interrupt: stopping is what
                // the person asked for. A failure is said once it is.
                let interrupted = crate::status::interrupt_for_stop(repo, pipelines);
                self.stop();
                if let Err(err) = interrupted {
                    self.popup = Some(Popup::Ended(super::notice(
                        "stop dispatching",
                        &format!("Not every running step could be interrupted: {err:#}"),
                        "[enter] close",
                        super::NOTICE_WRAP,
                    )));
                }
            }
            (Popup::Overrides(_) | Popup::Warnings(_) | Popup::Stop(_), Key::Esc)
            | (Popup::Ended(_), Key::Enter) => {}
            (popup, _) => self.popup = Some(popup),
        }
    }
}

/// The dispatch tab: the board, drawn from the task files, the lane list and
/// the ledger exactly as `spoolway dispatch` draws it, with the board's own
/// keys. `enter` starts dispatching — a `spoolway dispatch` child, see
/// [`super::dispatcher`] — and `enter` again asks how to stop it. The board is drawn
/// from what that child writes; this tab runs no pass itself. Redrawn every
/// [`crate::status::POLL`] while no key is typed, the same wait the board
/// keeps under a dispatcher, and that redraw is also where a child that has
/// exited is noticed.
///
/// `writer` pushes this past clippy's default argument count, but every
/// argument here is a distinct piece of the tab's own state — see
/// `status::mod`'s own `too_many_arguments` allow for the same reasoning.
#[allow(clippy::too_many_arguments)]
fn dispatch_tab(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &Path,
    board: &mut crate::status::Board,
    tab: &mut DispatchTab,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl Write,
) -> Result<Leave> {
    loop {
        tab.reap(repo);
        draw_board(
            repo,
            pipelines,
            board,
            tab,
            crate::screen::pane_size(),
            writer,
            out,
        );
        let Some(key) = wait_key(input, || {
            tab.reap(repo);
            draw_board(
                repo,
                pipelines,
                board,
                tab,
                crate::screen::pane_size(),
                writer,
                out,
            )
        }) else {
            return Ok(Leave::Quit);
        };
        // The board's own panel is drawn over the tab's popup — see
        // `Board::hosted_frame` — so it answers first. A child that exits
        // while a pause panel is open waits its turn rather than taking
        // keys meant for the panel on screen. A popup that reads no key —
        // `Starting` — leaves every key to the board and the shell below.
        if board.at_rest() && tab.popup.as_ref().is_some_and(Popup::reads_keys) {
            tab.answer(repo, pipelines, cwd, key);
            continue;
        }
        if board.at_rest() {
            if let Some(leave) = leave_on(key) {
                return Ok(leave);
            }
            if key == Key::Enter {
                tab.enter(repo, pipelines, cwd);
                continue;
            }
        }
        // Best-effort, the same as the dispatch loop's own `on_key`: a key
        // that failed against a queue being rewritten under it is lost, not a
        // reason to close the screen.
        let _ = board.on_key(repo, pipelines, key);
    }
}

/// Wait for a key, calling `idle` on every [`crate::status::POLL`] slice with
/// nothing typed — the same shape as the queue screen's own `wait_for_key`,
/// and for the same reasons. `None` once the input runs out or a `ctrl-c` has
/// been caught.
///
/// Every tab waits through this or through a loop of the same shape, never a
/// bare [`read_key`]: [`crate::platform::stop::catch_interrupt`] installs its
/// handler with `SA_RESTART`, so a blocking read is never interrupted by a
/// `ctrl-c`. A tab blocked in one would swallow the first press, and the
/// second would kill the process with the terminal still raw — the default
/// action the handler hands back after one catch.
pub(crate) fn wait_key(input: &mut impl PollableRead, mut idle: impl FnMut()) -> Option<Key> {
    loop {
        if crate::platform::stop::asked() {
            return None;
        }
        if !cfg!(unix) || input.byte_pending(crate::status::POLL) {
            return read_key(input);
        }
        idle();
    }
}

/// One board frame under the strip. A frame that fails to build — the queue
/// directory unreadable for an instant — leaves the last one on screen, the
/// same tolerance the dispatch loop's own `board.draw` gets.
///
/// `pane_size` is a parameter rather than read here off the real terminal —
/// see [`crate::screen::pane_size`] — so a test can hold the board's own
/// frame steady and vary only the pane size, to prove a resize alone still
/// forces the idle tab to repaint.
fn draw_board(
    repo: &Repo,
    pipelines: &Pipelines,
    board: &mut crate::status::Board,
    tab: &DispatchTab,
    pane_size: (usize, usize),
    writer: &mut crate::screen::frame_writer::FrameWriter,
    out: &mut impl Write,
) {
    if let Some(rows) = board_frame_rows(repo, pipelines, board, tab) {
        writer.write_frame(&rows, pane_size, out);
    }
}

/// The rows [`draw_board`] paints, split out so a test can play the very
/// same rows through today's frozen write and through
/// [`crate::screen::frame_writer`] and require the same picture. `None`
/// when the board's own frame fails to build — the queue directory
/// unreadable for an instant — leaving the last frame on screen, the same
/// tolerance the dispatch loop's own `board.draw` gets.
fn board_frame_rows(
    repo: &Repo,
    pipelines: &Pipelines,
    board: &mut crate::status::Board,
    tab: &DispatchTab,
) -> Option<Vec<String>> {
    let popup = tab.popup.as_ref().map(Popup::panel);
    let frame = board
        .hosted_frame(repo, pipelines, tab.dispatching(), popup)
        .ok()?;
    let mut rows = strip();
    rows.extend(frame.lines().map(str::to_string));
    Some(rows)
}

/// A tab that has nothing to draw but a message — a ledger eval could not
/// read — held on screen under the strip until a key moves on. Every key but
/// `←`, `→` and `q` is ignored: there is nothing else here to act on.
pub(crate) fn message_tab(
    message: &str,
    input: &mut impl PollableRead,
    out: &mut impl Write,
) -> Leave {
    // A writer of its own: this is a fresh visit to a screen that has
    // nothing but this one message to show, not a continuation of whatever
    // `eval`'s own writer last painted before handing off here.
    let mut writer = crate::screen::frame_writer::FrameWriter::new();
    message_frame(message, &mut writer, out);
    loop {
        let Some(key) = wait_key(input, || {}) else {
            return Leave::Quit;
        };
        if let Some(leave) = leave_on(key) {
            return leave;
        }
    }
}

/// [`message_tab`]'s one frame, on its own: eval draws it from inside a
/// `wait_key` idle callback, where a load that failed on its thread is
/// noticed, and that callback cannot also take the keys.
pub(crate) fn message_frame(
    message: &str,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    out: &mut impl Write,
) {
    writer.write_frame(
        &message_frame_rows(message),
        crate::screen::pane_size(),
        out,
    );
}

/// The rows [`message_frame`] paints, split out for the same reason as
/// [`board_frame_rows`]: a test needs them on their own to prove the shared
/// writer paints this frame exactly as today's frozen write did.
fn message_frame_rows(message: &str) -> Vec<String> {
    let mut rows = strip();
    rows.push(format!(" {message}"));
    rows.push(String::new());
    rows.push(super::key_hint(&[("q", "quit")]));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `line` with every escape code taken out — the colour a tab's own
    /// frame paints under the strip, the strip's own bold, and the frame
    /// writer's own codes around it. A CSI code (`ESC [ ... letter`) ends on
    /// any letter, not only `m`: the frame writer's `H`, `J` and `K` would
    /// otherwise run the skip on into real text. `ESC7`/`ESC8` (DECSC/DECRC,
    /// the frame writer's save and restore around each clear — see
    /// [`super::frame_writer::CLEAR_TO_END`]) are not CSI at all: `7` and
    /// `8` are not letters, so hunting for one the way a CSI code is skipped
    /// would eat every character up to the next stray letter in the frame's
    /// own text instead of stopping after the one byte the code actually
    /// spends.
    fn plain(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                match chars.next() {
                    Some('[') => {
                        chars.by_ref().find(char::is_ascii_alphabetic);
                    }
                    _ => continue,
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// A live `dispatch.pid` — a dispatcher started from the CLI — refuses
    /// bare `spoolway` exactly as a second screen would. Against
    /// [`crate::commands::already_running`] itself, the one check `run`
    /// shares with `commands::dispatch` rather than a copy of its own — see
    /// that call site's own comment.
    #[test]
    fn a_live_dispatcher_lock_refuses_the_screen() {
        let repo = crate::status::testutil::fixture("shell-already-running-dispatch");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        assert!(crate::commands::already_running(&repo, false).unwrap());
    }

    /// A live `spoolway.pid` — another screen already open in this project
    /// — refuses too.
    #[test]
    fn a_live_screen_lock_refuses_a_second_screen() {
        let repo = crate::status::testutil::fixture("shell-already-running-screen");
        let _lock = crate::lock::Lock::acquire(&repo.screen_lock_file(), false, None).unwrap();
        assert!(crate::commands::already_running(&repo, false).unwrap());
    }

    /// Neither lock naming a live process — the ordinary case — lets the
    /// screen open.
    #[test]
    fn no_live_lock_lets_the_screen_open() {
        let repo = crate::status::testutil::fixture("shell-already-running-clear");
        assert!(!crate::commands::already_running(&repo, false).unwrap());
    }

    // The mockup's own strip for each open tab, drawn to 100 columns, column
    // for column: every label in the column it held before the brackets. The
    // bold takes no column, so it is taken out before the columns are read.
    #[test]
    fn the_strip_lands_every_label_where_the_mockup_draws_it() {
        let lines = TABS.map(|tab| plain(&strip_line(tab, 100)));
        assert_eq!(
            lines,
            [
                "                        ← [dispatch]       queue        jobs        eval  →",
                "                        ←  dispatch       [queue]       jobs        eval  →",
                "                        ←  dispatch        queue       [jobs]       eval  →",
                "                        ←  dispatch        queue        jobs       [eval] →",
            ]
        );
    }

    // The whole line is bold and reset on the same line, and nothing else:
    // no colour, no dim, and no second bold marking the open tab — brackets
    // are still the only mark of which one is open.
    #[test]
    fn the_strip_is_bold_from_its_first_column_to_its_last_and_nothing_else() {
        for tab in TABS {
            let line = strip_line(tab, 100);
            let inner = line
                .strip_prefix("\x1b[1m")
                .and_then(|l| l.strip_suffix("\x1b[0m"))
                .unwrap_or_else(|| panic!("not wrapped in bold and reset: {line:?}"));
            assert!(!inner.contains('\x1b'), "{line:?}");
        }
    }

    #[test]
    fn a_narrow_terminal_still_keeps_a_space_between_each_arrow_and_the_labels() {
        let line = plain(&strip_line(Tab::Queue, 20));
        assert!(line.starts_with("  ←  dispatch"), "{line:?}");
        assert!(line.ends_with("eval  →"), "{line:?}");
    }

    #[test]
    fn left_and_right_walk_the_tabs_in_strip_order_and_stop_at_either_end() {
        assert_eq!(Tab::Queue.toward(Toward::Left), Tab::Dispatch);
        assert_eq!(Tab::Queue.toward(Toward::Right), Tab::Jobs);
        assert_eq!(Tab::Jobs.toward(Toward::Right), Tab::Eval);
        assert_eq!(Tab::Dispatch.toward(Toward::Left), Tab::Dispatch);
        assert_eq!(Tab::Eval.toward(Toward::Right), Tab::Eval);
    }

    /// Drive [`host`] over `input`, handing back every frame it drew, colour
    /// codes taken out.
    ///
    /// No case here may press `enter` on the dispatch tab with no gate to
    /// ask: that starts a child from `current_exe`, which under `cargo test`
    /// is the test binary itself. The e2e suite `screen` covers the start.
    fn drive_host(repo: &Repo, input: &str) -> Vec<String> {
        let mut input = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        host(
            repo,
            &Pipelines::builtin(),
            &repo.root,
            OnOpen::default(),
            &mut input,
            &mut out,
        )
        .unwrap();
        let drawn = String::from_utf8(out).unwrap();
        drawn
            .split("\x1b[?2026h\x1b[H")
            .skip(1)
            .map(plain)
            .collect()
    }

    // The e2e suite's own gesture, at unit level: the screen opens on the
    // queue tab under the strip, and `←` from there draws the board with no
    // dispatcher behind it, offering `enter` to start one.
    #[test]
    fn host_opens_on_the_queue_tab_and_left_reaches_the_board() {
        let repo = crate::status::testutil::fixture("shell-host-left");
        let frames = drive_host(&repo, "\x1b[D");
        let first = &frames[0];
        assert!(
            first.contains("dispatch       [queue]       jobs        eval"),
            "{first}"
        );
        assert!(first.contains("─ groups"), "{first}");
        let last = frames.last().unwrap();
        assert!(last.contains("dispatcher stopped"), "{last}");
        assert!(last.contains("┌─ dispatch ─"), "{last}");
        assert!(!last.contains("─ groups"), "{last}");
        assert!(
            last.contains(
                "[enter] start dispatching   [o] open task   [p] pause task   \
                 [r/R] resume / all   [u/U] unqueue / all   [q] quit"
            ),
            "{last}"
        );
    }

    /// [`drive_host`], keeping each frame's own bytes whole — colour codes,
    /// cursor moves and all — with its own start code put back on the front
    /// (`split` throws the delimiter it matched away). A frame's own bytes
    /// can never hold another frame's text — a fresh write replaces the
    /// whole picture — so a check for stale text has to read it off the
    /// *screen* an emulator paints these bytes onto instead, the same way a
    /// person watching a real terminal would see whatever a skipped write
    /// left behind.
    fn drive_host_raw_frames(repo: &Repo, input: &str) -> Vec<String> {
        let mut input = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        host(
            repo,
            &Pipelines::builtin(),
            &repo.root,
            OnOpen::default(),
            &mut input,
            &mut out,
        )
        .unwrap();
        let drawn = String::from_utf8(out).unwrap();
        drawn
            .split("\x1b[?2026h\x1b[H")
            .skip(1)
            .map(|body| format!("\x1b[?2026h\x1b[H{body}"))
            .collect()
    }

    /// Round every tab both ways and require that, after each switch, no
    /// marker of the tab just left is still on the *screen* — not merely
    /// absent from the new frame's own bytes, which a fresh write never
    /// carries regardless. Every frame is played into one `vt100::Parser`
    /// kept across the whole walk, exactly as a real terminal accumulates
    /// what is actually written to it, and each check reads the emulator's
    /// own screen contents rather than the frame's raw bytes.
    ///
    /// This does not prove `host` needs its `forget` on tab entry: with one
    /// shared writer the last frame is always the previous tab's, whose
    /// strip marks a different tab, so no entry frame is ever skipped here
    /// and this test passes with that `forget` removed. The proof that
    /// `forget` is needed is `frame_writer::tab_switch_leaves_nothing_behind`.
    #[test]
    fn every_tab_forgets_the_last_one_going_round_and_back() {
        let repo = crate::status::testutil::fixture("shell-host-tab-cycle");
        // Queue (open) → Dispatch → Queue → Jobs → Eval → Jobs → Queue →
        // Dispatch → Queue: forward through all four, then back through all
        // four, both with a real switch behind every arrow.
        let frames =
            drive_host_raw_frames(&repo, "\x1b[D\x1b[C\x1b[C\x1b[C\x1b[D\x1b[D\x1b[D\x1b[C");
        const DISPATCH: &str = "┌─ dispatch ─";
        const QUEUE: &str = "─ groups";
        const JOBS: &str = "no jobs yet";
        const EVAL: &str = "┌─ eval ·";
        let visited = [
            QUEUE, DISPATCH, QUEUE, JOBS, EVAL, JOBS, QUEUE, DISPATCH, QUEUE,
        ];
        assert_eq!(frames.len(), visited.len(), "{frames:?}");

        let (width, height) = crate::screen::pane_size();
        let mut parser = vt100::Parser::new(height as u16, width as u16, 0);
        for (i, marker) in visited.iter().enumerate() {
            parser.process(&crate::screen::frame_writer::as_terminal_would_receive(
                frames[i].as_bytes(),
            ));
            let screen = parser.screen().contents();
            assert!(
                screen.contains(marker),
                "frame {i} should show {marker:?} on the screen:\n{screen}"
            );
            if i > 0 && visited[i - 1] != *marker {
                assert!(
                    !screen.contains(visited[i - 1]),
                    "frame {i} still leaves the previous tab's {:?} on the screen:\n{screen}",
                    visited[i - 1]
                );
            }
        }
    }

    /// [`draw_board`] with a fresh board and dispatch tab over `repo`, and
    /// a fixed pane size so a test can hold it steady or change it on
    /// purpose rather than depending on whatever terminal `cargo test`
    /// happens to run under.
    fn draw_idle_board(
        repo: &Repo,
        pipelines: &Pipelines,
        board: &mut crate::status::Board,
        writer: &mut crate::screen::frame_writer::FrameWriter,
        pane_size: (usize, usize),
        out: &mut Vec<u8>,
    ) {
        draw_board(
            repo,
            pipelines,
            board,
            &DispatchTab::default(),
            pane_size,
            writer,
            out,
        );
    }

    /// The acceptance criterion this task exists for: the dispatch tab's
    /// own board frame, painted through the shared
    /// [`crate::screen::frame_writer`], must look exactly as it did through
    /// today's frozen erase-then-write — cell for cell, in text, colour and
    /// bold.
    #[test]
    fn dispatch_board_paints_as_before() {
        let repo = crate::status::testutil::fixture("dispatch-board-paints-as-before");
        crate::status::testutil::add(&repo, "wire", &[], None);
        let pipelines = Pipelines::builtin();
        let mut board = crate::status::Board::hosted();
        let frame = board_frame_rows(&repo, &pipelines, &mut board, &DispatchTab::default())
            .expect("the board's own frame must build");
        // Wide enough that no row here reaches the pane's own edge.
        let pane_size = (200, 60);

        let mut old = Vec::new();
        crate::screen::frame_writer::todays_write(&frame, &mut old);

        let mut new = Vec::new();
        crate::screen::frame_writer::FrameWriter::new().write_frame(&frame, pane_size, &mut new);

        crate::screen::frame_writer::assert_same_picture(&old, &new, pane_size);
    }

    /// The same acceptance criterion for `message_frame`.
    #[test]
    fn message_frame_paints_as_before() {
        let frame = message_frame_rows("spoolway eval: no ledger");
        let pane_size = (200, 60);

        let mut old = Vec::new();
        crate::screen::frame_writer::todays_write(&frame, &mut old);

        let mut new = Vec::new();
        crate::screen::frame_writer::FrameWriter::new().write_frame(&frame, pane_size, &mut new);

        crate::screen::frame_writer::assert_same_picture(&old, &new, pane_size);
    }

    /// The acceptance criterion for the dispatch tab's own idle skip: two
    /// calls over a board nothing has touched write the same bytes once.
    #[test]
    fn an_idle_board_writes_nothing_on_a_second_draw() {
        let repo = crate::status::testutil::fixture("shell-draw-board-idle");
        crate::status::testutil::add(&repo, "wire", &[], None);
        let pipelines = Pipelines::builtin();
        let mut board = crate::status::Board::hosted();
        let mut writer = crate::screen::frame_writer::FrameWriter::new();
        let mut out = Vec::new();
        draw_idle_board(
            &repo,
            &pipelines,
            &mut board,
            &mut writer,
            (100, 30),
            &mut out,
        );
        let after_first = out.len();
        assert!(after_first > 0, "the first draw must write the frame");
        draw_idle_board(
            &repo,
            &pipelines,
            &mut board,
            &mut writer,
            (100, 30),
            &mut out,
        );
        assert_eq!(
            out.len(),
            after_first,
            "an unchanged board must not write anything more"
        );
    }

    /// The other half of the same criterion: a task moving to a new stage
    /// changes the board's own frame, so the tab must repaint even though
    /// nothing asked it to and the pane never moved.
    #[test]
    fn a_moved_task_forces_the_idle_board_to_repaint() {
        let repo = crate::status::testutil::fixture("shell-draw-board-task-moves");
        crate::status::testutil::add(&repo, "wire", &[], None);
        let pipelines = Pipelines::builtin();
        let mut board = crate::status::Board::hosted();
        let mut writer = crate::screen::frame_writer::FrameWriter::new();
        let mut out = Vec::new();
        draw_idle_board(
            &repo,
            &pipelines,
            &mut board,
            &mut writer,
            (100, 30),
            &mut out,
        );
        let after_first = out.len();

        let mut task = repo.task("wire").unwrap();
        task.set_stage("implement", None);
        task.save().unwrap();

        draw_idle_board(
            &repo,
            &pipelines,
            &mut board,
            &mut writer,
            (100, 30),
            &mut out,
        );
        assert!(
            out.len() > after_first,
            "a moved task must repaint the idle board"
        );
    }

    /// The pane-size half of the same criterion: an unchanged board still
    /// repaints once the terminal itself has resized, since a row's own
    /// clearing and the frame writer's skip both depend on the pane size it
    /// was drawn for.
    #[test]
    fn a_resize_forces_the_idle_board_to_repaint_even_with_the_same_content() {
        let repo = crate::status::testutil::fixture("shell-draw-board-resize");
        crate::status::testutil::add(&repo, "wire", &[], None);
        let pipelines = Pipelines::builtin();
        let mut board = crate::status::Board::hosted();
        let mut writer = crate::screen::frame_writer::FrameWriter::new();
        let mut out = Vec::new();
        draw_idle_board(
            &repo,
            &pipelines,
            &mut board,
            &mut writer,
            (100, 30),
            &mut out,
        );
        let after_first = out.len();
        draw_idle_board(
            &repo,
            &pipelines,
            &mut board,
            &mut writer,
            (110, 30),
            &mut out,
        );
        assert!(
            out.len() > after_first,
            "a pane resize must repaint an unchanged board"
        );
    }

    #[test]
    fn q_ends_the_screen() {
        let repo = crate::status::testutil::fixture("shell-host-q");
        let frames = drive_host(&repo, "q");
        assert_eq!(frames.len(), 1, "one frame, then q ended it: {frames:?}");
    }

    /// A project with an override layer nobody has acknowledged.
    fn fixture_with_layer(name: &str) -> Repo {
        let repo = crate::status::testutil::fixture(name);
        std::fs::create_dir_all(repo.overrides_dir().join("pipelines")).unwrap();
        std::fs::write(
            repo.overrides_dir().join("pipelines/default.yml"),
            "steps:\n  implement:\n    model: fake-opus\n",
        )
        .unwrap();
        repo
    }

    // `enter` asks the overrides gate first, as a popup over the board, and
    // the popup reads every key: `→` does not leave the tab while it is up.
    #[test]
    fn enter_opens_the_overrides_popup_and_it_keeps_the_arrows() {
        let repo = fixture_with_layer("shell-host-overrides");
        let frames = drive_host(&repo, "\x1b[D\r\x1b[C");
        let last = frames.last().unwrap();
        assert!(
            last.contains("┌─ overrides are active for this project "),
            "{last}"
        );
        assert!(last.contains("pipelines/default.yml"), "{last}");
        assert!(
            last.contains(
                "[enter] start dispatching   [esc] back   [x] don't ask again until this changes"
            ),
            "{last}"
        );
        assert!(last.contains("dispatcher stopped"), "{last}");
        assert!(!last.contains("─ groups"), "{last}");
    }

    // `esc` backs out of the gate having started nothing: the board is back
    // at rest, still offering to start dispatching.
    #[test]
    fn esc_off_the_overrides_popup_starts_nothing() {
        let repo = fixture_with_layer("shell-host-overrides-esc");
        let frames = drive_host(&repo, "\x1b[D\r\x1b");
        let last = frames.last().unwrap();
        assert!(!last.contains("overrides are active"), "{last}");
        assert!(last.contains("[enter] start dispatching"), "{last}");
        assert!(last.contains("dispatcher stopped"), "{last}");
        assert_eq!(crate::lock::Lock::holder(&repo.lock_file()).unwrap(), None);
    }

    // `x` on the overrides popup hides it and goes on to the warnings gate,
    // just as `enter` would — and the overrides gate does not ask again.
    // `unattended.enabled` guarantees the warnings gate has something to
    // say, so nothing here reaches a start.
    #[test]
    fn x_on_the_overrides_popup_hides_it_and_asks_the_warnings_next() {
        let mut repo = fixture_with_layer("shell-host-overrides-x");
        repo.config.unattended.enabled = true;
        let pipelines = Pipelines::builtin();
        let mut tab = DispatchTab::default();
        tab.enter(&repo, &pipelines, &repo.root);
        assert!(matches!(tab.popup, Some(Popup::Overrides(_))));
        tab.answer(&repo, &pipelines, &repo.root, Key::Char('x'));
        assert!(matches!(tab.popup, Some(Popup::Warnings(_))));
        assert!(tab.child.is_none());
        assert!(crate::commands::overrides_popup(&repo).unwrap().is_none());

        // `esc` off the warnings backs out with nothing started, and the
        // next `enter` skips the hidden overrides gate.
        tab.answer(&repo, &pipelines, &repo.root, Key::Esc);
        assert!(tab.popup.is_none());
        tab.enter(&repo, &pipelines, &repo.root);
        assert!(matches!(tab.popup, Some(Popup::Warnings(_))));
    }

    // A dispatcher that ended on its own leaves its reason up until `enter`
    // closes it, and with no child left `enter` is back to starting one.
    #[test]
    fn an_ended_popup_closes_on_enter() {
        let repo = crate::status::testutil::fixture("shell-host-ended");
        let mut tab = DispatchTab {
            child: None,
            popup: Some(Popup::Ended(super::super::dispatcher::popup(
                true,
                "unattended.max_cost_usd reached",
            ))),
        };
        let pipelines = Pipelines::builtin();
        tab.answer(&repo, &pipelines, &repo.root, Key::Char('x'));
        assert!(tab.popup.is_some(), "only enter closes it");
        tab.answer(&repo, &pipelines, &repo.root, Key::Enter);
        assert!(tab.popup.is_none());
        assert!(!tab.dispatching());
    }

    /// How long a stand-in first pass waits after the start before writing
    /// anything. A real one comes after the child's own start checks; one
    /// written the same instant can be dated a few milliseconds *before*
    /// `since`, by the kernel's coarse file clock, and would not count.
    const FIRST_PASS_GAP: std::time::Duration = std::time::Duration::from_millis(50);

    /// A shell command standing in for the tab's `spoolway dispatch` child.
    fn stand_in(script: &str) -> super::super::dispatcher::Dispatcher {
        super::super::dispatcher::Dispatcher::stand_in(script)
    }

    // A started child puts the keyless `Starting dispatcher` popup up at
    // once, drawn as the mockup draws it, and it reads no key.
    #[test]
    fn a_start_opens_the_keyless_starting_popup() {
        let repo = crate::status::testutil::fixture("shell-starting-opens");
        let mut tab = DispatchTab::default();
        tab.started(Ok(stand_in("exec sleep 30")), std::time::SystemTime::now());
        let popup = tab.popup.as_ref().expect("the popup is up");
        assert!(matches!(popup, Popup::Starting { .. }));
        assert!(!popup.reads_keys());
        assert_eq!(
            popup.panel(),
            [
                "┌─ Starting dispatcher ─────────┐",
                "│  checking herdr, git, queue…  │",
                "└───────────────────────────────┘",
            ]
        );
        // Nothing has claimed yet, so a redraw leaves it up.
        tab.reap(&repo);
        assert!(matches!(tab.popup, Some(Popup::Starting { .. })));
    }

    // The first pass claiming a slot closes the popup, and the child keeps
    // running under the board.
    #[test]
    fn the_starting_popup_closes_on_the_first_claim() {
        let repo = crate::status::testutil::fixture("shell-starting-claim");
        let mut tab = DispatchTab::default();
        tab.started(Ok(stand_in("exec sleep 30")), std::time::SystemTime::now());
        std::thread::sleep(FIRST_PASS_GAP);
        let mut claims = crate::claim::Claims::new(&repo);
        claims.claim("wire", "implement");
        tab.reap(&repo);
        assert!(tab.popup.is_none());
        assert!(tab.dispatching());
    }

    // A first pass with nothing to claim still writes `lanes.json` on its
    // way out, and that closes the popup too. A mark or a `lanes.json` from
    // before the start does not.
    #[test]
    fn the_starting_popup_closes_on_a_first_pass_that_claimed_nothing() {
        let repo = crate::status::testutil::fixture("shell-starting-empty");
        crate::task::write_atomic(&repo.lanes_file(), "{}").unwrap();
        crate::task::write_atomic(&repo.claims_dir().join("gone"), "review").unwrap();
        std::thread::sleep(FIRST_PASS_GAP);
        let mut tab = DispatchTab::default();
        tab.started(Ok(stand_in("exec sleep 30")), std::time::SystemTime::now());
        tab.reap(&repo);
        assert!(matches!(tab.popup, Some(Popup::Starting { .. })));

        std::thread::sleep(FIRST_PASS_GAP);
        crate::task::write_atomic(&repo.lanes_file(), "{}").unwrap();
        tab.reap(&repo);
        assert!(tab.popup.is_none());
    }

    // A child that ends before its first pass puts `Ended` in the popup's
    // place; one stopped from the stop popup just takes the popup away.
    #[test]
    fn a_child_ending_first_replaces_the_starting_popup() {
        let repo = crate::status::testutil::fixture("shell-starting-ended");
        let mut tab = DispatchTab::default();
        tab.started(
            Ok(stand_in("echo refused >&2; exit 1")),
            std::time::SystemTime::now(),
        );
        for _ in 0..500 {
            tab.reap(&repo);
            if tab.child.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(matches!(tab.popup, Some(Popup::Ended(_))));

        let mut tab = DispatchTab::default();
        tab.started(Ok(stand_in("exec sleep 30")), std::time::SystemTime::now());
        let pipelines = Pipelines::builtin();
        tab.enter(&repo, &pipelines, &repo.root);
        assert!(matches!(tab.popup, Some(Popup::Stop(_))));
        tab.answer(&repo, &pipelines, &repo.root, Key::Enter);
        for _ in 0..500 {
            tab.reap(&repo);
            if tab.child.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(tab.child.is_none());
        assert!(tab.popup.is_none());
    }

    /// A dispatch tab with a stand-in child running, as `enter` would have
    /// left it — see [`super::super::dispatcher::Dispatcher::stand_in`].
    fn running_tab() -> DispatchTab {
        DispatchTab {
            child: Some(super::super::dispatcher::Dispatcher::stand_in(
                "exec sleep 30",
            )),
            popup: None,
        }
    }

    fn stopping(tab: &DispatchTab) -> bool {
        tab.child.as_ref().is_some_and(|child| child.stopping())
    }

    // `enter` over a running dispatcher asks how to stop it — even with
    // nothing running — and `esc` leaves it running.
    #[cfg(unix)]
    #[test]
    fn enter_over_a_running_dispatcher_asks_and_esc_leaves_it_running() {
        let repo = crate::status::testutil::fixture("shell-stop-esc");
        let pipelines = Pipelines::builtin();
        let mut tab = running_tab();
        tab.enter(&repo, &pipelines, &repo.root);
        assert!(matches!(tab.popup, Some(Popup::Stop(_))));
        assert!(tab.popup.as_ref().unwrap().panel()[0].starts_with("┌─ stop dispatching "));
        assert!(!stopping(&tab), "nothing is stopped before it is answered");

        // A key the popup does not read leaves it up.
        tab.answer(&repo, &pipelines, &repo.root, Key::Char('q'));
        assert!(matches!(tab.popup, Some(Popup::Stop(_))));

        tab.answer(&repo, &pipelines, &repo.root, Key::Esc);
        assert!(tab.popup.is_none());
        assert!(!stopping(&tab));
        assert!(tab.dispatching());
    }

    // `enter` on the popup stops the child and touches no task; a second
    // `enter` while that stop is under way does nothing.
    #[cfg(unix)]
    #[test]
    fn enter_on_the_stop_popup_stops_the_child_and_changes_no_task() {
        let repo = crate::status::testutil::fixture("shell-stop-enter");
        crate::status::testutil::add(&repo, "login", &[], Some("handover"));
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        runs.start(
            &key,
            "sleep 30",
            &repo.root,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        let pipelines = Pipelines::builtin();
        let mut tab = running_tab();
        tab.enter(&repo, &pipelines, &repo.root);
        tab.answer(&repo, &pipelines, &repo.root, Key::Enter);
        assert!(tab.popup.is_none());
        assert!(stopping(&tab));
        assert_eq!(runs.state(&key), crate::command_step::RunState::Running);
        assert_eq!(repo.task("login").unwrap().stage(), "handover");

        tab.enter(&repo, &pipelines, &repo.root);
        assert!(tab.popup.is_none(), "a stop under way asks nothing again");
        runs.stop(&key);
    }

    // `i` interrupts what is running, parks it with the stop's mark, then
    // stops the child.
    #[cfg(unix)]
    #[test]
    fn i_on_the_stop_popup_interrupts_parks_and_stops() {
        let repo = crate::status::testutil::fixture("shell-stop-i");
        crate::status::testutil::add(&repo, "login", &[], Some("handover"));
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "login");
        runs.start(
            &key,
            "sleep 30",
            &repo.root,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        let pipelines = Pipelines::builtin();
        let mut tab = running_tab();
        tab.enter(&repo, &pipelines, &repo.root);
        tab.answer(&repo, &pipelines, &repo.root, Key::Char('i'));
        assert!(
            tab.popup.is_none(),
            "{:?}",
            tab.popup.as_ref().map(Popup::panel)
        );
        assert!(stopping(&tab));
        assert_ne!(runs.state(&key), crate::command_step::RunState::Running);
        let task = repo.task("login").unwrap();
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert!(task.front.parked_by_stop);
        runs.stop(&key);
    }

    // While a board confirm panel is open it reads every key: `→` there is
    // the panel's to ignore, not the shell's, so the screen stays on the
    // board rather than opening the queue tab.
    #[test]
    fn an_open_board_panel_keeps_the_arrows_from_the_shell() {
        let repo = crate::status::testutil::fixture("shell-host-panel");
        crate::status::testutil::add(&repo, "wire", &[], None);
        let frames = drive_host(&repo, "\x1b[DU\x1b[C");
        let last = frames.last().unwrap();
        assert!(
            last.contains("not started"),
            "the panel is still up: {last}"
        );
        assert!(!last.contains("─ groups"), "{last}");
    }

    // Once the panel is answered — `enter`, which carries the unqueue out —
    // the board is at rest again and `→` is the shell's: it reaches the
    // queue tab.
    #[test]
    fn an_answered_board_panel_gives_the_arrows_back() {
        let repo = crate::status::testutil::fixture("shell-host-panel-answered");
        crate::status::testutil::add(&repo, "wire", &[], None);
        let frames = drive_host(&repo, "\x1b[DU\r\x1b[C");
        let last = frames.last().unwrap();
        assert!(last.contains("─ groups"), "{last}");
    }

    // A tab with only a message to show keeps it under the strip, ignores
    // every key but the shell's own, and leaves on `←`.
    #[test]
    fn message_tab_holds_its_message_until_a_shell_key() {
        let _hosting = Hosting::open(Tab::Eval);
        let mut input = std::io::Cursor::new(b"xj\x1b[D".to_vec());
        let mut out = Vec::new();
        let leave = message_tab("spoolway eval: no ledger", &mut input, &mut out);
        assert_eq!(leave, Leave::Switch(Toward::Left));
        let drawn = plain(&String::from_utf8(out).unwrap());
        assert!(drawn.contains("dispatch        queue"), "{drawn}");
        assert!(drawn.contains("spoolway eval: no ledger"), "{drawn}");
    }

    #[test]
    fn with_no_shell_hosting_nothing_leaves_and_nothing_is_drawn() {
        assert_eq!(leave_on(Key::Left), None);
        assert_eq!(leave_on(Key::Char('q')), None);
        assert!(strip().is_empty());
        assert_eq!(strip_rows(), 0);
        assert!(quit_hint().is_empty());
    }

    #[test]
    fn hosting_a_tab_makes_the_arrows_and_q_leave_until_it_is_dropped() {
        {
            let _hosting = Hosting::open(Tab::Jobs);
            assert_eq!(leave_on(Key::Left), Some(Leave::Switch(Toward::Left)));
            assert_eq!(leave_on(Key::Right), Some(Leave::Switch(Toward::Right)));
            assert_eq!(leave_on(Key::Char('q')), Some(Leave::Quit));
            assert_eq!(leave_on(Key::Tab), None);
            assert_eq!(strip_rows(), 3);
            let strip = strip();
            assert_eq!(strip.len(), 3);
            assert_eq!(strip[0], "");
            assert!(plain(&strip[1]).contains("[jobs]"), "{:?}", strip[1]);
            assert_eq!(strip[2], "");
        }
        assert_eq!(hosted(), None);
    }
}
