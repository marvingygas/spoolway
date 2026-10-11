//! A test-only hook that kills `spoolway dispatch` at a named moment.
//!
//! The end-to-end harness needs to stop the dispatcher inside a step that is
//! over in milliseconds on a small fixture: while `git worktree add` is still
//! writing a checkout, or `git worktree remove` is still deleting one. An
//! outside `kill` cannot land there, so the dispatcher shoots itself.
//!
//! `SPOOLWAY_TEST_KILL_AT=<point>` names the moment. The variable is read only
//! when `SPOOLWAY_TEST_BACKEND` is also set, which is the gate the headless
//! backend already has, and only in a process that called [`arm`], which only
//! `spoolway dispatch` does. Any other command, or a dispatcher run without
//! the test marker, never looks at it.
//!
//! The harness's supervisor starts `spoolway dispatch` again with the same
//! environment after a signal exit. A hook that fired on every start would
//! kill every restart too, so the first kill leaves a marker file inside the
//! repository's `.git` directory and later starts find it and carry on.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

/// The environment variable that names the kill point.
pub(crate) const KILL_AT_ENV: &str = "SPOOLWAY_TEST_KILL_AT";

/// After the task file records `run` and `base_commit`, before the worktree
/// is cut.
pub(crate) const CUT_BEFORE_ADD: &str = "cut-before-add";
/// While `git worktree add` is still writing the checkout.
pub(crate) const CUT_DURING_ADD: &str = "cut-during-add";
/// After the worktree is cut, before the task file records its workspace.
pub(crate) const CUT_AFTER_ADD: &str = "cut-after-add";
/// After the final auto-commit, before the checkout is torn down.
pub(crate) const TEARDOWN_AFTER_COMMIT: &str = "teardown-after-commit";
/// While `git worktree remove` is still deleting the checkout.
pub(crate) const TEARDOWN_DURING_REMOVE: &str = "teardown-during-remove";
/// After the checkout is torn down, before the task file is archived.
pub(crate) const TEARDOWN_BEFORE_ARCHIVE: &str = "teardown-before-archive";

const POINTS: [&str; 6] = [
    CUT_BEFORE_ADD,
    CUT_DURING_ADD,
    CUT_AFTER_ADD,
    TEARDOWN_AFTER_COMMIT,
    TEARDOWN_DURING_REMOVE,
    TEARDOWN_BEFORE_ARCHIVE,
];

/// How long a `during` point watches the git child for the checkout to start
/// changing. Past this the child is simply waited for: a command that has not
/// touched its directory by now will not, and holding the dispatcher on it
/// longer helps no test.
const DURING_WAIT: Duration = Duration::from_secs(20);

/// Set by [`arm`], so a call from any process that is not `spoolway dispatch`
/// does nothing.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The point this environment asks for, if the test marker is set and the
/// variable is set. An unknown name is an error here rather than a silent
/// no-op: a typo would otherwise leave a fault test passing with nothing
/// injected.
fn requested() -> Result<Option<String>> {
    if crate::platform::env_var(crate::headless::TEST_BACKEND_ENV).is_err() {
        return Ok(None);
    }
    let Ok(point) = crate::platform::env_var(KILL_AT_ENV) else {
        return Ok(None);
    };
    if !POINTS.contains(&point.as_str()) {
        bail!(
            "{KILL_AT_ENV}={point} names no kill point — it must be one of {}. \
             Unset {KILL_AT_ENV}, or set it to one of those.",
            POINTS.join(", ")
        );
    }
    Ok(Some(point))
}

/// Called once by `spoolway dispatch` at start. Refuses an unknown point name,
/// and turns the hook on for this process.
pub(crate) fn arm() -> Result<()> {
    if requested()?.is_some() {
        ARMED.store(true, Ordering::SeqCst);
    }
    Ok(())
}

/// Whether `point` is the requested one and has not fired yet. Claims the
/// marker file as it answers, so the answer is `true` once across restarts.
fn claim(repo_root: &Path, point: &str) -> bool {
    if requested().ok().flatten().as_deref() != Some(point) {
        return false;
    }
    let git_dir = repo_root.join(".git");
    // A linked worktree has a `.git` file, not a directory, so there is
    // nowhere to put the marker; the hook stays off there rather than fire
    // on every restart. The harness's fixtures are plain repositories.
    if !git_dir.is_dir() {
        return false;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(git_dir.join(format!("spoolway-test-kill-{point}")))
        .is_ok()
}

fn die() -> ! {
    // SAFETY: `kill` on our own pid has no memory-safety preconditions.
    unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    // SIGKILL cannot be caught; this only satisfies the return type.
    std::process::abort()
}

/// Kill this dispatcher with `SIGKILL` if `point` is the requested one and
/// has not fired. Returns normally otherwise.
pub(crate) fn kill_at(repo_root: &Path, point: &str) {
    if ARMED.load(Ordering::SeqCst) && claim(repo_root, point) {
        die();
    }
}

/// Whether `point` is the one this process is asked to fire at, without
/// claiming it. Lets a caller keep its ordinary code path when no fault is
/// asked for.
pub(crate) fn wants(point: &str) -> bool {
    ARMED.load(Ordering::SeqCst) && requested().ok().flatten().as_deref() == Some(point)
}

/// [`crate::repo::run`], but when `point` fires the git command's whole
/// process group and then the dispatcher are killed with `SIGKILL` as soon as
/// `moved` says the command has started changing the checkout. Used for the
/// `during` points, where the kill has to land inside the git command.
///
/// The command runs in a process group of its own. `git worktree add` writes
/// the checkout through a child, `git reset --hard`, so killing only the pid
/// spawned here would leave that child to finish the checkout on its own and
/// no half state would be left behind. The dispatcher's own group is not
/// signalled: under the harness's supervisor it is shared with the loop that
/// restarts the dispatcher.
///
/// A child that finishes before `moved` is ever true is not an error here:
/// nothing half done is left behind, and the end-to-end case that asked for
/// the kill checks for that half state and fails on its absence.
pub(crate) fn run_killing_at(
    repo_root: &Path,
    point: &str,
    cwd: &Path,
    args: &[&str],
    moved: impl Fn() -> bool,
) -> Result<String> {
    use anyhow::Context;
    if !claim(repo_root, point) {
        return crate::repo::run(cwd, "git", args);
    }
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .process_group(0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if moved() {
            // SAFETY: `kill` on a process group has no memory-safety
            // preconditions; the group id is the child's pid because it was
            // started with `process_group(0)`.
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            die();
        }
        if started.elapsed() > DURING_WAIT {
            break child.wait()?;
        }
        std::thread::sleep(Duration::from_micros(200));
    };
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut stderr);
        }
        bail!(
            "`git {}` failed ({status}): {}",
            args.join(" "),
            stderr.trim()
        );
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::test_env::with_env;

    fn repo(name: &str) -> crate::scratch::ScratchRoot {
        let dir = crate::scratch::root(name);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    /// The marker the dispatcher already has to export for the headless
    /// backend is the gate: without it the hook is inert even when a point
    /// is named.
    #[test]
    fn a_named_point_is_inert_without_the_test_backend() {
        let dir = repo("fault-inert");
        with_env(KILL_AT_ENV, CUT_BEFORE_ADD, || {
            assert!(!claim(&dir, CUT_BEFORE_ADD));
            assert!(requested().unwrap().is_none());
        });
    }

    /// A restart under the same environment must not be shot again.
    #[test]
    fn a_point_fires_at_most_once() {
        let dir = repo("fault-once");
        with_env(crate::headless::TEST_BACKEND_ENV, "1", || {
            with_env(KILL_AT_ENV, CUT_AFTER_ADD, || {
                assert!(!claim(&dir, CUT_BEFORE_ADD), "another point");
                assert!(claim(&dir, CUT_AFTER_ADD));
                assert!(!claim(&dir, CUT_AFTER_ADD), "second start");
            });
        });
    }

    #[test]
    fn an_unknown_point_is_refused_by_name() {
        with_env(crate::headless::TEST_BACKEND_ENV, "1", || {
            with_env(KILL_AT_ENV, "cut-sideways", || {
                let err = format!("{:#}", requested().unwrap_err());
                assert!(err.contains("cut-sideways") && err.contains("teardown-before-archive"));
                assert!(arm().is_err());
            });
        });
    }

    /// With no marker the same unknown name is ignored: the variable is not
    /// read at all.
    #[test]
    fn an_unknown_point_without_the_test_backend_is_ignored() {
        with_env(KILL_AT_ENV, "cut-sideways", || {
            assert!(requested().unwrap().is_none());
            assert!(arm().is_ok());
        });
    }
}
