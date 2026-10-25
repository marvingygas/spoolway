//! The `spoolway dispatch` child bare `spoolway`'s dispatch tab starts on
//! `enter`, and stops on the next `enter` — once the popup it opens, when
//! there is one, is answered: [`stop_panel`] with a step running, or
//! [`stop_queued_panel`] with tasks queued.
//!
//! The screen never runs a pass itself. It starts a child with
//! `--from-screen`, and the child dispatches exactly as `spoolway dispatch`
//! would, into the same task files, lane list and ledger the board is drawn
//! from. That keeps one code path for dispatching. It also means the screen
//! owns nothing the run holds, so quitting it can leave lanes running the
//! same way stopping a dispatcher always has.
//!
//! The child owns no terminal. Its stdin and stdout go nowhere, and its
//! stderr is read back here: under `--from-screen`, stderr carries why the
//! run ended on its own — and little else, a config notice at load being the
//! rare exception — and that becomes the tab's popup. It runs in a
//! process group of its own, so a `ctrl-c` typed at the screen reaches the
//! screen alone. The screen then decides to stop the child, the same as `q`.
//!
//! Every exit of the child also leaves one line in the project's problem log,
//! written here: a killed child cannot write its own, and the screen is the
//! one that sees the exit status and the stderr. A screen that quits does not
//! wait for the exit, so its line is written as it asks, without a status.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;

use anyhow::{Context, Result};

/// A dispatcher the dispatch tab started.
pub(crate) struct Dispatcher {
    process: std::process::Child,
    /// The child's stderr, read to its end on a thread of its own. A pipe
    /// nobody drains fills, and a child blocked writing to a full pipe
    /// never exits. The thread's result is joined only once the child has
    /// exited.
    reason: Option<JoinHandle<String>>,
    /// Whether the child was ever seen holding the dispatch lock. This is
    /// what tells a run that stopped from a start that was refused before
    /// it got that far.
    ran: bool,
    /// The problem log this child's exit is written to — see
    /// [`crate::problem_log::path`]. Held as a path because the child
    /// outlives the call that started it.
    log: PathBuf,
    /// Who asked it to stop, once someone has. A child that exits after that
    /// did what it was told, and it gets no popup.
    asked: Option<Asked>,
    /// Whether its exit already has a line in the log. One exit, one line.
    logged: bool,
    /// The scratch directory a stand-in child's log sits in, kept alive as
    /// long as the child: a [`crate::scratch::ScratchRoot`] removes its
    /// directory when dropped, and the child writes there when it exits.
    #[cfg(test)]
    scratch: Option<crate::scratch::ScratchRoot>,
}

/// Who asked the child to stop, as the log line names it.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Asked {
    /// `enter` on the dispatch tab, or an answer to a popup it opened.
    Enter,
    /// The board itself ended.
    Quit,
}

impl Asked {
    fn said(self) -> &'static str {
        match self {
            Asked::Enter => "asked by the board (enter on the dispatch tab)",
            Asked::Quit => "asked by the board, which was quitting",
        }
    }
}

/// How many of the child's last stderr lines the log line keeps. A panic
/// with a backtrace is hundreds, and the log is read a line at a time.
const LOG_STDERR_LINES: usize = 5;

/// The longest a kept stderr line may be, in characters.
const LOG_STDERR_WIDTH: usize = 300;

/// The last lines of `text`, on one line: each trimmed and cut to
/// [`LOG_STDERR_WIDTH`], joined with ` / `. Empty when it said nothing.
fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let from = lines.len().saturating_sub(LOG_STDERR_LINES);
    lines[from..]
        .iter()
        .map(|line| line.chars().take(LOG_STDERR_WIDTH).collect::<String>())
        .collect::<Vec<_>>()
        .join(" / ")
}

impl Dispatcher {
    /// Start `spoolway dispatch --from-screen` in `cwd`, with the binary
    /// this screen is running. A binary on `PATH` could be another version.
    pub(crate) fn start(cwd: &Path, log: PathBuf) -> Result<Dispatcher> {
        let exe = std::env::current_exe().context("could not find the spoolway binary")?;
        let mut command = Command::new(runnable(&exe));
        // A child started through `/proc/self/exe` would otherwise call
        // itself `exe` in every usage line clap writes to the popup.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.arg0("spoolway");
        }
        command
            .arg("-C")
            .arg(cwd)
            .args(["dispatch", "--from-screen"]);
        Dispatcher::spawn(command, log).context("could not start spoolway dispatch")
    }

    /// Run `command` as the tab's dispatcher: no terminal, stderr kept.
    /// Apart from [`Dispatcher::start`] so a test can stand a plain shell
    /// command in for `spoolway dispatch` — under `cargo test`,
    /// `current_exe` is the test binary itself.
    pub(super) fn spawn(mut command: Command, log: PathBuf) -> Result<Dispatcher> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // A group of its own, so the terminal's `SIGINT` reaches only the
        // screen. Otherwise one `ctrl-c` would stop the child behind the
        // screen's back, before the screen had decided anything.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut process = command.spawn()?;
        let reason = process.stderr.take().map(|mut stderr| {
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                text
            })
        });
        Ok(Dispatcher {
            process,
            reason,
            ran: false,
            log,
            asked: None,
            logged: false,
            #[cfg(test)]
            scratch: None,
        })
    }

    /// A shell command standing in for `spoolway dispatch --from-screen`,
    /// for the dispatch tab's own tests — see [`Dispatcher::spawn`].
    #[cfg(test)]
    pub(crate) fn stand_in(script: &str) -> Dispatcher {
        let root = crate::scratch::root("dispatcher-stand-in-log");
        let mut child = Dispatcher::stand_in_logging(script, root.join("problems.log"));
        child.scratch = Some(root);
        child
    }

    /// [`Dispatcher::stand_in`], writing its exit to `log`.
    #[cfg(test)]
    pub(crate) fn stand_in_logging(script: &str, log: PathBuf) -> Dispatcher {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        Dispatcher::spawn(command, log).unwrap()
    }

    /// The child's process id, which the dispatcher writes into `first-pass`
    /// once its first pass is over.
    pub(crate) fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Ask the child to stop, with the signal `spoolway dispatch` already
    /// answers: it settles what it holds, tears nothing down, and exits,
    /// leaving every lane running. Asked once. A second `SIGINT` would find
    /// the default action restored and kill the child where it stands.
    pub(crate) fn stop(&mut self, by: Asked) {
        if self.asked.is_some() {
            return;
        }
        self.asked = Some(by);

        #[cfg(unix)]
        // SAFETY: `kill` takes no pointers. A child that has exited but not
        // been reaped is a zombie, and signalling it succeeds and does
        // nothing; the next `ended` reads its exit either way.
        unsafe {
            libc::kill(self.process.id() as i32, libc::SIGINT);
        }
        #[cfg(not(unix))]
        let _ = self.process.kill();
    }

    /// Whether the screen has already asked this child to stop. A child
    /// still settling after that has nothing left to ask, so the tab's
    /// `enter` does nothing over it rather than opening the stop popup again.
    pub(crate) fn stopping(&self) -> bool {
        self.asked.is_some()
    }

    /// `None` while the child is still running. Once it has exited, why,
    /// or `Some(None)` for a child that stopped because it was asked to.
    ///
    /// Whether it ever ran decides the popup's title, and two things say
    /// so. A clean exit is one: under `--from-screen` every refusal before
    /// the lock is an error or a non-zero code — no gate is asked and an
    /// empty queue is waited on — so `0` only ever comes from a run that
    /// held the lock and then stopped on its own, a spend ceiling. The
    /// other is having seen the child hold the lock, noted here on every
    /// call. That is only while the dispatch tab is open, which is why it
    /// is not the only witness: it catches the rarer run that held the
    /// lock and then failed, provided the tab was open to see it.
    pub(crate) fn ended(&mut self, lock_file: &Path) -> Option<Option<Ended>> {
        let pid = self.process.id();
        if !self.ran && crate::lock::Lock::holder(lock_file).ok().flatten() == Some(pid) {
            self.ran = true;
        }
        let status = self.process.try_wait().ok()??;
        self.ran |= status.success();
        let text = self.log_exit(status);
        if self.asked.is_some() {
            return Some(None);
        }
        // `main` puts the binary's name in front of every error. Inside the
        // screen that name is noise: the popup's title already says who
        // spoke.
        let text = text.trim();
        let text = text.strip_prefix("spoolway: ").unwrap_or(text);
        let reason = match text.is_empty() {
            true => format!("it exited with {status} and said nothing."),
            false => text.to_string(),
        };
        Some(Some(Ended {
            ran: self.ran,
            reason,
        }))
    }
}

/// Why a child ended without being asked to: what it said, and whether it
/// ever ran, which picks the title of the popup that says so.
pub(crate) struct Ended {
    ran: bool,
    reason: String,
}

impl Ended {
    /// The popup saying why — see [`popup`].
    pub(crate) fn panel(&self) -> Vec<String> {
        popup(self.ran, &self.reason)
    }

    /// The same popup with `more` as a paragraph of its own under the
    /// reason: what else the child's going left undone.
    pub(crate) fn panel_with(&self, more: &str) -> Vec<String> {
        popup(self.ran, &format!("{}\n\n{more}", self.reason))
    }
}

impl Dispatcher {
    /// Write the problem-log line for a child that has exited with
    /// `status`, and give back everything it said on stderr. The line names
    /// the stop `enter` asked for when there was one, and otherwise the
    /// status and the last of what it said.
    fn log_exit(&mut self, status: std::process::ExitStatus) -> String {
        let pid = self.process.id();
        let text = self
            .reason
            .take()
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        let kept = last_lines(&text);
        let line = match self.asked {
            Some(by) => {
                let tail = match kept.is_empty() {
                    true => String::new(),
                    false => format!(": {kept}"),
                };
                format!(
                    "dispatcher {pid} stopped: {}; it exited with {status}{tail}",
                    by.said()
                )
            }
            None if kept.is_empty() => {
                format!("dispatcher {pid} exited with {status} and said nothing")
            }
            None => format!("dispatcher {pid} exited with {status}, last said: {kept}"),
        };
        self.log_line(&line);
        text
    }

    /// Write `line` to the problem log, once for this child: a second
    /// exit line for the same child would say the same thing twice.
    fn log_line(&mut self, line: &str) {
        if !std::mem::replace(&mut self.logged, true) {
            crate::problem_log::append_at(&self.log, line);
        }
    }
}

impl Drop for Dispatcher {
    /// However the screen ends — `q`, `ctrl-c`, or an error on its way out
    /// — dispatching stops with it. The child is not waited on: it settles
    /// what it holds and exits on its own, and the screen does not keep
    /// the person waiting on a pass that is still finishing.
    fn drop(&mut self) {
        match self.process.try_wait() {
            // A child that already exited unseen, with the dispatch tab
            // closed, is logged here with its status, as `ended` would have.
            Ok(Some(status)) => {
                if !self.logged {
                    self.log_exit(status);
                }
            }
            Ok(None) => {
                self.stop(Asked::Quit);
                // The exit is never seen, so the line is written now, naming
                // whoever asked first: a stop `enter` asked for a moment ago
                // is still the reason, and its exit has not been reaped.
                if let Some(by) = self.asked {
                    self.log_line(&format!(
                        "dispatcher {} stopped: {}; the board left before it exited",
                        self.process.id(),
                        by.said()
                    ));
                }
            }
            Err(_) => {}
        }
    }
}

/// The path to start this same binary from, given what `current_exe` said.
///
/// An install that lands while the screen is open — `spoolway update`,
/// `cargo install`, npm — replaces the file at `exe`. On Linux,
/// `current_exe` then names the old inode as `<path> (deleted)`, a file
/// that does not exist, and every start fails with `No such file or
/// directory`. `/proc/self/exe` still reaches the binary this screen is
/// running, so the child is the same version the screen is.
fn runnable(exe: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    if !exe.exists() {
        return PathBuf::from("/proc/self/exe");
    }
    exe.to_path_buf()
}

/// The popup for a child that ended without being asked to, or a start
/// that never got as far as a child: `reason`, wrapped, under a title that
/// says whether it ever ran.
pub(crate) fn popup(ran: bool, reason: &str) -> Vec<String> {
    let title = match ran {
        true => "the dispatcher stopped",
        false => "the dispatcher did not start",
    };
    super::notice(title, reason, &super::confirm(), super::NOTICE_WRAP)
}

/// The stop popup's answers, in the order [`stop_panel`] lists them: the
/// first leaves running steps to finish, the second interrupts them.
pub(crate) const STOP_ANSWERS: [&str; 2] = ["let running steps finish", "interrupt them now"];

/// The popup `enter` opens over a running dispatcher: whether its running
/// steps finish, or are interrupted now, with the cursor on the
/// `cursor`th of [`STOP_ANSWERS`]. Asked only while an agent lane or command
/// step is running — with none, the two answers would do the same thing, and
/// `enter` asks [`stop_queued_panel`] instead when tasks are queued, or stops
/// the dispatcher without asking when none are. The choice is the person's
/// each time, never remembered. Laid out line by line rather than wrapped, so
/// the explanation keeps the line breaks it was written with.
pub(crate) fn stop_panel(cursor: usize) -> Vec<String> {
    let body = [
        "",
        "no new steps will be started.",
        "interrupting stops agents and commands. when resumed, agents pick",
        "up where they left off and commands restart.",
    ];
    super::ask(
        "stop dispatching",
        &body.map(str::to_string),
        &STOP_ANSWERS.map(str::to_string),
        cursor,
    )
}

/// The answers of the popup `enter` opens over a dispatcher with no step
/// running and tasks queued, in the order [`stop_queued_panel`] lists them:
/// the first stops the dispatcher, the second leaves it running.
pub(crate) const STOP_QUEUED_ANSWERS: [&str; 2] = ["stop dispatching", "keep it running"];

/// The popup `enter` opens over a running dispatcher when no step is running
/// but `queued` tasks are waiting, with the cursor on the `cursor`th of
/// [`STOP_QUEUED_ANSWERS`]. A stop then is silent and those tasks just wait,
/// so the count is said before the one key that does it. Not asked with an
/// empty queue, where `enter` stops at once, nor with a step running, where
/// [`stop_panel`] asks instead.
pub(crate) fn stop_queued_panel(queued: usize, cursor: usize) -> Vec<String> {
    let verb = match queued {
        1 => "is",
        _ => "are",
    };
    let body = [
        String::new(),
        format!(
            "{} {verb} queued and will not start.",
            super::plural(queued, "task")
        ),
    ];
    super::ask(
        "stop dispatching",
        &body,
        &STOP_QUEUED_ANSWERS.map(str::to_string),
        cursor,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::corner::{BOTTOM_LEFT, BOTTOM_RIGHT, TOP_LEFT, TOP_RIGHT};

    /// The refusal `spoolway dispatch` gives outside a herdr pane, as the
    /// popup for step 28 draws it: the two commands indented under the
    /// sentence, and only `enter` to close.
    #[test]
    fn a_refused_start_names_the_way_in_under_its_own_title() {
        let panel = popup(
            false,
            "Open herdr and start spoolway there:\n\n  herdr\n  spoolway",
        );
        let body: Vec<&str> = panel
            .iter()
            .map(|line| line.trim_matches(['│', ' ']))
            .collect();
        assert!(
            panel[0].starts_with(&format!("{TOP_LEFT}─ the dispatcher did not start ")),
            "{panel:?}"
        );
        assert_eq!(
            body[1..panel.len() - 1],
            [
                "",
                "Open herdr and start spoolway there:",
                "",
                "herdr",
                "spoolway",
                "",
                "[enter] confirm"
            ],
            "{panel:?}"
        );
        assert!(
            panel.iter().any(|line| line.starts_with("│    herdr ")),
            "{panel:?}"
        );
    }

    /// A run that took the lock and then ended on its own — a spend ceiling
    /// — is a dispatcher that stopped, as step 30 draws it.
    #[test]
    fn a_run_that_ended_on_its_own_says_it_stopped() {
        let panel = popup(
            true,
            "unattended.max_cost_usd reached: $5.02 of $5.00\n\
             Every task is where its last lane left it.",
        );
        assert!(
            panel[0].starts_with(&format!("{TOP_LEFT}─ the dispatcher stopped ")),
            "{panel:?}"
        );
        assert!(
            panel[2].contains("unattended.max_cost_usd reached: $5.02 of $5.00"),
            "{panel:?}"
        );
        assert!(
            panel[3].contains("Every task is where its last lane left it."),
            "{panel:?}"
        );
    }

    /// The stop popup, border and all: the explanation, then its two
    /// answers with the cursor on the gentler one.
    #[test]
    fn the_stop_popup_lists_its_answers_under_the_explanation() {
        assert_eq!(
            stop_panel(0),
            [
                format!("{TOP_LEFT}─ stop dispatching ──────────────────────────────────────────────────{TOP_RIGHT}").as_str(),
                "│                                                                     │",
                "│  no new steps will be started.                                      │",
                "│  interrupting stops agents and commands. when resumed, agents pick  │",
                "│  up where they left off and commands restart.                       │",
                "│                                                                     │",
                "│  > let running steps finish                                         │",
                "│    interrupt them now                                               │",
                "│                                                                     │",
                "│  [enter] confirm   [esc] close                                      │",
                format!("{BOTTOM_LEFT}─────────────────────────────────────────────────────────────────────{BOTTOM_RIGHT}").as_str(),
            ]
        );
    }

    /// The popup over a dispatcher with tasks queued and no step running,
    /// border and all: the count, then its two answers with the cursor on
    /// the stop.
    #[test]
    fn the_queued_stop_popup_names_the_count_over_its_answers() {
        assert_eq!(
            stop_queued_panel(1, 0),
            [
                format!("{TOP_LEFT}─ stop dispatching ─────────────────────{TOP_RIGHT}").as_str(),
                "│                                        │",
                "│  1 task is queued and will not start.  │",
                "│                                        │",
                "│  > stop dispatching                    │",
                "│    keep it running                     │",
                "│                                        │",
                "│  [enter] confirm   [esc] close         │",
                format!("{BOTTOM_LEFT}────────────────────────────────────────{BOTTOM_RIGHT}")
                    .as_str(),
            ]
        );
        let many = stop_queued_panel(3, 1).join("\n");
        assert!(
            many.contains("3 tasks are queued and will not start."),
            "{many}"
        );
        assert!(many.contains("> keep it running"), "{many}");
    }

    fn child(script: &str) -> Dispatcher {
        Dispatcher::stand_in(script)
    }

    /// Poll [`Dispatcher::ended`] until the child has exited. The lock file
    /// named is one nothing holds, so only the exit status says it ran.
    fn wait_ended(child: &mut Dispatcher) -> Option<Vec<String>> {
        let lock = crate::scratch::root("dispatcher-no-lock").join("dispatch.pid");
        for _ in 0..500 {
            if let Some(ended) = child.ended(&lock) {
                return ended.map(|ended| ended.panel());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the child never exited");
    }

    /// A refusal: a non-zero exit, its stderr the popup's body with
    /// `main`'s `spoolway: ` prefix taken off.
    #[test]
    fn a_child_refused_before_it_ran_says_it_did_not_start() {
        let mut child = child(
            "printf 'spoolway: Open herdr and start spoolway there:\\n\\n  herdr\\n  spoolway\\n' >&2; \
             exit 1",
        );
        let panel = wait_ended(&mut child).expect("an unasked exit gets a popup");
        assert!(
            panel[0].starts_with(&format!("{TOP_LEFT}─ the dispatcher did not start ")),
            "{panel:?}"
        );
        assert!(
            panel[2].starts_with("│  Open herdr and start spoolway there:"),
            "{panel:?}"
        );
        assert!(
            !panel.iter().any(|line| line.contains("spoolway: ")),
            "{panel:?}"
        );
    }

    /// A clean exit the screen did not ask for is a run that stopped on its
    /// own, whether or not the tab was open to see it hold the lock.
    #[test]
    fn a_clean_exit_it_was_not_asked_for_says_it_stopped() {
        let mut child = child(
            "printf 'unattended.max_cost_usd reached: $5.02 of $5.00\\n\\
             Every task is where its last lane left it.\\n' >&2",
        );
        let panel = wait_ended(&mut child).expect("an unasked exit gets a popup");
        assert!(
            panel[0].starts_with(&format!("{TOP_LEFT}─ the dispatcher stopped ")),
            "{panel:?}"
        );
        assert!(
            panel[2].contains("unattended.max_cost_usd reached: $5.02 of $5.00"),
            "{panel:?}"
        );
    }

    /// A child that said nothing still gets a reason: its exit status.
    #[test]
    fn a_silent_exit_names_its_status() {
        let mut child = child("exit 4");
        let panel = wait_ended(&mut child).expect("an unasked exit gets a popup");
        assert!(
            panel
                .iter()
                .any(|line| line.contains("it exited with exit status: 4 and said nothing.")),
            "{panel:?}"
        );
    }

    /// A binary still where it was is started from that path.
    #[test]
    fn a_binary_still_in_place_is_started_from_its_own_path() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(runnable(&exe), exe);
    }

    /// A binary replaced while the screen ran is started from the inode
    /// this process still holds, which runs: the reported path does not.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_replaced_binary_is_started_from_the_running_one() {
        let gone = Path::new("/nonexistent/spoolway (deleted)");
        let path = runnable(gone);
        assert_eq!(path, Path::new("/proc/self/exe"));
        let mut command = Command::new(path);
        command.arg("--list");
        let output = command.output().expect("the running binary starts");
        assert!(output.status.success(), "{output:?}");
    }

    /// A child the screen asked to stop ends with no popup at all.
    #[cfg(unix)]
    #[test]
    fn a_child_asked_to_stop_ends_without_a_popup() {
        let mut child = child("exec sleep 30");
        assert!(!child.stopping());
        child.stop(Asked::Enter);
        assert!(child.stopping());
        assert_eq!(wait_ended(&mut child), None);
    }

    /// A fresh scratch directory and a log path inside it. The directory is
    /// removed when the first is dropped, so each test holds it to the end.
    fn log_in(name: &str) -> (crate::scratch::ScratchRoot, PathBuf) {
        let root = crate::scratch::root(name);
        let log = root.join("logs").join("p.log");
        (root, log)
    }

    fn logged(log: &Path) -> String {
        std::fs::read_to_string(log).unwrap_or_default()
    }

    /// An exit nobody asked for leaves one line naming the pid, the status
    /// and the last of what it said.
    #[test]
    fn an_unasked_exit_is_logged_with_its_status_and_stderr() {
        let (_root, log) = log_in("dispatcher-log-unasked");
        let mut child = Dispatcher::stand_in_logging("echo boom >&2; exit 4", log.clone());
        let pid = child.pid();
        wait_ended(&mut child).expect("an unasked exit gets a popup");
        let text = logged(&log);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains(&format!(
                "dispatcher {pid} exited with exit status: 4, last said: boom"
            )),
            "{text}"
        );
    }

    /// A child that said nothing is logged as having said nothing.
    #[test]
    fn a_silent_exit_is_logged_as_saying_nothing() {
        let (_root, log) = log_in("dispatcher-log-silent");
        let mut child = Dispatcher::stand_in_logging("exit 0", log.clone());
        wait_ended(&mut child);
        assert!(
            logged(&log).contains("exit status: 0 and said nothing"),
            "{}",
            logged(&log)
        );
    }

    /// A panic's worth of stderr keeps only its last lines, on one line.
    #[test]
    fn a_long_stderr_is_cut_to_its_last_lines() {
        let (_root, log) = log_in("dispatcher-log-long");
        let mut child = Dispatcher::stand_in_logging(
            "i=1; while [ $i -le 40 ]; do echo line$i >&2; i=$((i+1)); done; exit 1",
            log.clone(),
        );
        wait_ended(&mut child);
        let text = logged(&log);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains("line36 / line37 / line38 / line39 / line40"),
            "{text}"
        );
        assert!(!text.contains("line35"), "{text}");
    }

    /// A stop `enter` asked for names the board and `enter` as the reason,
    /// and still shows no popup.
    #[cfg(unix)]
    #[test]
    fn a_stop_enter_asked_for_is_logged_with_its_reason() {
        let (_root, log) = log_in("dispatcher-log-enter");
        let mut child = Dispatcher::stand_in_logging("exec sleep 30", log.clone());
        let pid = child.pid();
        child.stop(Asked::Enter);
        assert_eq!(wait_ended(&mut child), None);
        let text = logged(&log);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains(&format!(
                "dispatcher {pid} stopped: asked by the board (enter on the dispatch tab); \
                 it exited with signal: 2 (SIGINT)"
            )),
            "{text}"
        );
    }

    /// A board that quits does not wait for the exit, so the line is
    /// written as the stop is asked, without a status.
    #[cfg(unix)]
    #[test]
    fn a_board_that_quits_logs_the_stop_it_asked_for() {
        let (_root, log) = log_in("dispatcher-log-quit");
        let child = Dispatcher::stand_in_logging("exec sleep 30", log.clone());
        let pid = child.pid();
        drop(child);
        let text = logged(&log);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains(&format!(
                "dispatcher {pid} stopped: asked by the board, which was quitting; \
                 the board left before it exited"
            )),
            "{text}"
        );
    }

    /// A stop `enter` asked for that the board quits over still gets its
    /// line, naming `enter`, and only one.
    #[cfg(unix)]
    #[test]
    fn a_quit_over_a_stop_enter_asked_for_keeps_that_reason() {
        let (_root, log) = log_in("dispatcher-log-enter-then-quit");
        let mut child = Dispatcher::stand_in_logging("exec sleep 30", log.clone());
        child.stop(Asked::Enter);
        drop(child);
        let text = logged(&log);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains("(enter on the dispatch tab); the board left"),
            "{text}"
        );
    }

    /// A child that dies unseen, while the dispatch tab is closed, still
    /// gets its line when the board quits: with its status, not as a stop
    /// the board asked for.
    #[cfg(unix)]
    #[test]
    fn an_exit_never_reaped_is_logged_when_the_board_quits() {
        let (_root, log) = log_in("dispatcher-log-unreaped");
        let mut child = Dispatcher::stand_in_logging("echo gone >&2; exit 7", log.clone());
        let pid = child.pid();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(child.process.try_wait(), Ok(None)) {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never exited"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(child);
        let text = logged(&log);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains(&format!(
                "dispatcher {pid} exited with exit status: 7, last said: gone"
            )),
            "{text}"
        );
    }

    /// An exit the tab already reaped and logged is not logged again when
    /// the board quits.
    #[test]
    fn a_reaped_exit_is_not_logged_again_on_quit() {
        let (_root, log) = log_in("dispatcher-log-reaped-then-quit");
        let mut child = Dispatcher::stand_in_logging("exit 3", log.clone());
        wait_ended(&mut child);
        drop(child);
        let text = logged(&log);
        assert_eq!(text.lines().count(), 1, "{text}");
    }
}
