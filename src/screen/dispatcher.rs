//! The `spoolway dispatch` child bare `spoolway`'s dispatch tab starts on
//! `enter` and stops on the next one.
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

use std::io::Read;
use std::path::Path;
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
    /// Whether the screen asked it to stop. A child that exits after that
    /// did what it was told, and it gets no popup.
    stopping: bool,
    /// Whether the child was ever seen holding the dispatch lock. This is
    /// what tells a run that stopped from a start that was refused before
    /// it got that far.
    ran: bool,
}

impl Dispatcher {
    /// Start `spoolway dispatch --from-screen` in `cwd`, with the binary
    /// this screen is running. A binary on `PATH` could be another version.
    pub(crate) fn start(cwd: &Path) -> Result<Dispatcher> {
        let exe = std::env::current_exe().context("could not find the spoolway binary")?;
        let mut command = Command::new(exe);
        command
            .arg("-C")
            .arg(cwd)
            .args(["dispatch", "--from-screen"]);
        Dispatcher::spawn(command).context("could not start spoolway dispatch")
    }

    /// Run `command` as the tab's dispatcher: no terminal, stderr kept.
    /// Apart from [`Dispatcher::start`] so a test can stand a plain shell
    /// command in for `spoolway dispatch` — under `cargo test`,
    /// `current_exe` is the test binary itself.
    pub(super) fn spawn(mut command: Command) -> Result<Dispatcher> {
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
            stopping: false,
            ran: false,
        })
    }

    /// Ask the child to stop, with the signal `spoolway dispatch` already
    /// answers: it settles what it holds, tears nothing down, and exits,
    /// leaving every lane running. Asked once. A second `SIGINT` would find
    /// the default action restored and kill the child where it stands.
    pub(crate) fn stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        #[cfg(unix)]
        // SAFETY: `kill` takes no pointers; a child that has already exited
        // earns ESRCH, which is the answer the next `ended` reads anyway.
        unsafe {
            libc::kill(self.process.id() as i32, libc::SIGINT);
        }
        #[cfg(not(unix))]
        let _ = self.process.kill();
    }

    /// `None` while the child is still running. Once it has exited, the
    /// popup that says why, or `Some(None)` for a child that stopped because
    /// it was asked to.
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
    pub(crate) fn ended(&mut self, lock_file: &Path) -> Option<Option<Vec<String>>> {
        let pid = self.process.id();
        if !self.ran && crate::lock::Lock::holder(lock_file).ok().flatten() == Some(pid) {
            self.ran = true;
        }
        let status = self.process.try_wait().ok()??;
        self.ran |= status.success();
        let text = self
            .reason
            .take()
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        if self.stopping {
            return Some(None);
        }
        // `main` puts the binary's name in front of every error. Inside the
        // screen that name is noise: the popup's title already says who
        // spoke.
        let text = text.trim();
        let text = text.strip_prefix("spoolway: ").unwrap_or(text);
        Some(Some(match text.is_empty() {
            true => popup(
                self.ran,
                &format!("It exited with {status} and said nothing."),
            ),
            false => popup(self.ran, text),
        }))
    }
}

impl Drop for Dispatcher {
    /// However the screen ends — `q`, `ctrl-c`, or an error on its way out
    /// — dispatching stops with it. The child is not waited on: it settles
    /// what it holds and exits on its own, and the screen does not keep
    /// the person waiting on a pass that is still finishing.
    fn drop(&mut self) {
        if matches!(self.process.try_wait(), Ok(None)) {
            self.stop();
        }
    }
}

/// The popup for a child that ended without being asked to, or a start
/// that never got as far as a child: `reason`, wrapped, under a title that
/// says whether it ever ran.
pub(crate) fn popup(ran: bool, reason: &str) -> Vec<String> {
    let title = match ran {
        true => "the dispatcher stopped",
        false => "the dispatcher did not start",
    };
    super::notice(title, reason, "[enter] close", super::NOTICE_WRAP)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            panel[0].starts_with("┌─ the dispatcher did not start "),
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
                "[enter] close"
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
            panel[0].starts_with("┌─ the dispatcher stopped "),
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

    /// A shell command standing in for `spoolway dispatch --from-screen`.
    fn child(script: &str) -> Dispatcher {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        Dispatcher::spawn(command).unwrap()
    }

    /// Poll [`Dispatcher::ended`] until the child has exited. The lock file
    /// named is one nothing holds, so only the exit status says it ran.
    fn wait_ended(child: &mut Dispatcher) -> Option<Vec<String>> {
        let lock = crate::scratch::root("dispatcher-no-lock").join("dispatch.pid");
        for _ in 0..500 {
            if let Some(ended) = child.ended(&lock) {
                return ended;
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
            panel[0].starts_with("┌─ the dispatcher did not start "),
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
            panel[0].starts_with("┌─ the dispatcher stopped "),
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
                .any(|line| line.contains("It exited with exit status: 4 and said nothing.")),
            "{panel:?}"
        );
    }

    /// A child the screen asked to stop ends with no popup at all.
    #[cfg(unix)]
    #[test]
    fn a_child_asked_to_stop_ends_without_a_popup() {
        let mut child = child("exec sleep 30");
        child.stop();
        assert!(child.stopping);
        assert_eq!(wait_ended(&mut child), None);
    }
}
