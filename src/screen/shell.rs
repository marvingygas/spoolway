//! Bare `spoolway`: one screen holding four tabs — dispatch, queue, jobs and
//! eval, in that order — with one terminal guard, one key reader and one quit.
//!
//! A tab carries no logic of its own. Each one calls the code its CLI command
//! already draws with — `commands::queue_tab`, `commands::jobs_tab`,
//! `eval::tab` and the board's own [`crate::status::Board`] — and this module
//! adds only the strip on the top row and the keys that move between tabs.
//! The one exception is the dispatch tab's `enter`, which starts and stops a
//! `spoolway dispatch` child ([`super::dispatcher`]) behind the gates that
//! command asks, drawn here as popups over the board.
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
//! What the screen has to say the moment it opens — the sync gate and the
//! update notice, which every other command prints ahead of itself — is
//! handed to the queue tab, the one the screen opens on, to show as popups
//! over it: printed ahead of the screen, the first frame would wipe it
//! before anybody could read it. See [`OnOpen`].
//!
//! Which tab is open lives in a thread-local rather than being threaded
//! through every screen's own `render`: the strip and the two rows it takes
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
    /// The sync gate's popup — [`crate::gate::sync_popup`].
    pub(crate) sync: Option<Vec<String>>,
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

/// The rows the strip takes off the top of the terminal: the strip itself and
/// one blank row under it, as the mockup draws. Zero with no shell around the
/// screen, so a screen drawn by its own CLI command keeps every row it had.
pub(crate) fn strip_rows() -> usize {
    match hosted() {
        Some(_) => 2,
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

/// The strip and the blank row under it, for a screen to put above its own
/// frame — or nothing at all with no shell hosting one.
pub(crate) fn strip() -> Vec<String> {
    match hosted() {
        Some(open) => {
            let width = terminal_size::terminal_size()
                .map(|(w, _)| w.0 as usize)
                .unwrap_or(FALLBACK_WIDTH);
            vec![strip_line(open, width), String::new()]
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
/// every other slot leaves them blank. Nothing is coloured or dim — the
/// brackets alone say which tab is open.
///
/// At least two columns stay before `←` however narrow the terminal, and
/// nothing is written past the `→`: a row that reaches the terminal's last
/// column is followed by a newline the terminal has already wrapped for, and
/// the frame under it comes out one row lower than it was measured for.
fn strip_line(open: Tab, width: usize) -> String {
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
    format!("{}← {} →", " ".repeat(start - 2), slots.join(SLOT_GAP))
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
/// nothing left to restore it. Opens on the queue tab, with the sync gate
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
    // way the printed gate reports it, as this command's own error.
    let on_open = OnOpen {
        sync: crate::gate::sync_popup(repo)?,
        update,
    };

    crate::platform::stop::catch_interrupt();
    let _term = crate::platform::TermGuard::new();
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

    loop {
        let leave = {
            let _hosting = Hosting::open(tab);
            match tab {
                Tab::Dispatch => {
                    dispatch_tab(repo, pipelines, cwd, &mut board, &mut dispatch, input, out)?
                }
                // Taken on the first visit, so a notice is shown once and
                // not again on every return to the tab.
                Tab::Queue => crate::commands::queue_tab(
                    repo,
                    pipelines,
                    cwd,
                    std::mem::take(&mut on_open),
                    input,
                    out,
                )?,
                Tab::Jobs => crate::commands::jobs_tab(repo, pipelines, cwd, input, out)?,
                Tab::Eval => crate::eval::tab(repo, pipelines, input, out)?,
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

/// A popup the dispatch tab draws over the board. Each one reads every key
/// until it is answered, the same as the board's own confirm panels.
enum Popup {
    /// The overrides gate `enter` asks first, when there is a layer to name.
    Overrides(crate::commands::GatePopup),
    /// Doctor's cheap findings, asked after the overrides gate.
    Warnings(crate::commands::GatePopup),
    /// Why the dispatcher ended, or never started. `enter` closes it.
    Ended(Vec<String>),
}

impl Popup {
    fn panel(&self) -> &[String] {
        match self {
            Popup::Overrides(gate) | Popup::Warnings(gate) => &gate.panel,
            Popup::Ended(panel) => panel,
        }
    }
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
    /// ended without being asked to.
    fn reap(&mut self, repo: &Repo) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        if let Some(ended) = child.ended(&repo.lock_file()) {
            self.child = None;
            if let Some(panel) = ended {
                self.popup = Some(Popup::Ended(panel));
            }
        }
    }

    /// `enter` with no popup open: stop the child straight away, with no
    /// question, or — with none running — ask the start gates, each only
    /// when it has something to say, and start one. A child already asked
    /// to stop is left to finish going: `stop` asks once.
    fn enter(&mut self, repo: &Repo, pipelines: &Pipelines, cwd: &Path) {
        match self.child.as_mut() {
            Some(child) => child.stop(),
            None => self.overrides_then_start(repo, pipelines, cwd),
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
        match super::dispatcher::Dispatcher::start(cwd) {
            Ok(child) => self.child = Some(child),
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
    /// nothing. Every other key is ignored.
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
            (Popup::Overrides(_) | Popup::Warnings(_), Key::Esc)
            | (Popup::Ended(_), Key::Enter) => {}
            (popup, _) => self.popup = Some(popup),
        }
    }
}

/// The dispatch tab: the board, drawn from the task files, the lane list and
/// the ledger exactly as `spoolway dispatch` draws it, with the board's own
/// keys. `enter` starts dispatching — a `spoolway dispatch` child, see
/// [`super::dispatcher`] — and `enter` again stops it. The board is drawn
/// from what that child writes; this tab runs no pass itself. Redrawn every
/// [`crate::status::POLL`] while no key is typed, the same wait the board
/// keeps under a dispatcher, and that redraw is also where a child that has
/// exited is noticed.
fn dispatch_tab(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &Path,
    board: &mut crate::status::Board,
    tab: &mut DispatchTab,
    input: &mut impl PollableRead,
    out: &mut impl Write,
) -> Result<Leave> {
    loop {
        tab.reap(repo);
        draw_board(repo, pipelines, board, tab, out);
        let Some(key) = wait_key(input, || {
            tab.reap(repo);
            draw_board(repo, pipelines, board, tab, out)
        }) else {
            return Ok(Leave::Quit);
        };
        // The board's own panel is drawn over the tab's popup — see
        // `Board::hosted_frame` — so it answers first. A child that exits
        // while a pause panel is open waits its turn rather than taking
        // keys meant for the panel on screen.
        if board.at_rest() && tab.popup.is_some() {
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
fn draw_board(
    repo: &Repo,
    pipelines: &Pipelines,
    board: &mut crate::status::Board,
    tab: &DispatchTab,
    out: &mut impl Write,
) {
    let popup = tab.popup.as_ref().map(Popup::panel);
    let Ok(frame) = board.hosted_frame(repo, pipelines, tab.dispatching(), popup) else {
        return;
    };
    let _ = write!(out, "\x1b[2J\x1b[H");
    for line in strip() {
        let _ = writeln!(out, "{line}");
    }
    let _ = write!(out, "{frame}");
    let _ = out.flush();
}

/// A tab that has nothing to draw but a message — a ledger eval could not
/// read — held on screen under the strip until a key moves on. Every key but
/// `←`, `→` and `q` is ignored: there is nothing else here to act on.
pub(crate) fn message_tab(
    message: &str,
    input: &mut impl PollableRead,
    out: &mut impl Write,
) -> Leave {
    message_frame(message, out);
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
pub(crate) fn message_frame(message: &str, out: &mut impl Write) {
    let _ = write!(out, "\x1b[2J\x1b[H");
    for line in strip() {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, " {message}");
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", super::key_hint(&[("q", "quit")]));
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `line` with every `\x1b[...` code taken out — the colour a tab's own
    /// frame paints under the strip, which itself paints none, and the
    /// screen clear ahead of it. Ends a code on any letter, not only `m`: the
    /// clear's `J` and `H` would otherwise run the skip on into the strip.
    fn plain(line: &str) -> String {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(char::is_ascii_alphabetic);
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
    // for column: every label in the column it held before the brackets.
    #[test]
    fn the_strip_lands_every_label_where_the_mockup_draws_it() {
        let lines = TABS.map(|tab| strip_line(tab, 100));
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

    // Brackets are the only mark: no label or arrow carries a colour code,
    // open or closed.
    #[test]
    fn the_strip_draws_no_colour_code_on_any_tab() {
        for tab in TABS {
            let line = strip_line(tab, 100);
            assert!(!line.contains('\x1b'), "{line:?}");
        }
    }

    #[test]
    fn a_narrow_terminal_still_keeps_a_space_between_each_arrow_and_the_labels() {
        let line = strip_line(Tab::Queue, 20);
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
        drawn.split("\x1b[2J\x1b[H").skip(1).map(plain).collect()
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
                "[enter] start dispatching   [o] open task   [r/R] resume / all   \
                 [p/P] pause / all   [u/U] unqueue / all   [q] quit"
            ),
            "{last}"
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
            assert_eq!(strip_rows(), 2);
            assert_eq!(strip().len(), 2);
        }
        assert_eq!(hosted(), None);
    }
}
