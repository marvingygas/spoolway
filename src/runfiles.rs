//! The on-disk protocol behind a detached process's pid and exit files.
//!
//! [`crate::headless`] and [`crate::command_step`] each watch a detached
//! process the same way, across dispatch passes that cannot afford to wait on
//! it: a wrapper shell writes its own pid before starting whatever it runs,
//! and the exit code that command ended with after it. Both backends read
//! that same pair of files back to answer "is it still going, and what did it
//! end with" — this module is the one place that reads and writes them.
//!
//! What a caller does with the two facts is still theirs, because a lane and
//! a command step attach different meaning to the same pair: a command step
//! reports the exit code it finds, where a lane never looks past whether the
//! file exists, and a lane distinguishes "never started" from "gone without a
//! code" using bookkeeping this module knows nothing about (see
//! [`crate::headless::Headless::status`] and [`crate::command_step::Runs::state`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The pid and exit files kept for every key under one directory.
#[derive(Debug, Clone)]
pub(crate) struct RunFiles {
    dir: PathBuf,
}

impl RunFiles {
    pub(crate) fn new(dir: PathBuf) -> RunFiles {
        RunFiles { dir }
    }

    /// Written by the wrapper as its first act, so the filesystem already
    /// knows when the run began and there is no second place for that to
    /// disagree.
    pub(crate) fn pid_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.pid"))
    }

    /// Written by the wrapper as its last act, so its presence means the run
    /// is over however it ended. It is also what makes liveness safe against
    /// pid reuse: a recycled pid would read as alive, but a finished run has
    /// already said otherwise here.
    pub(crate) fn exit_path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.exit"))
    }

    /// The wrapper's own pid, which is the process group everything it
    /// started belongs to.
    pub(crate) fn read_pid(&self, key: &str) -> Option<u32> {
        std::fs::read_to_string(self.pid_path(key))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The exit file's content, parsed as a code — `None` while there is
    /// nothing in it to read yet. Content that is there but unreadable as a
    /// number reads as `1`: a wrapper that wrote something is a wrapper that
    /// ran, and that is the safer reading over reporting a run that plainly
    /// finished as still going.
    ///
    /// An *empty* file is not a wrapper that wrote something, and reading it
    /// as one was a bug that reached the dispatcher. Writing a file is two
    /// steps — create it, then put something in it — so a reader polling at
    /// this one's pace catches it between them, and every one of those reads
    /// used to come back as exit 1. A command step that had succeeded was
    /// routed down `on_fail`, at random, on about one Windows run in three;
    /// PowerShell's `Set-Content` leaves a wider gap between the two steps
    /// than `sh`'s redirect does, which is why that platform saw it and this
    /// one mostly did not.
    ///
    /// `None` puts the question back to the pid, which is the one thing that
    /// can tell the two cases apart: a wrapper still running is a run still
    /// going, and a wrapper already gone left no code to route on — see
    /// [`crate::command_step::Runs::state`], which reads that as
    /// `Interrupted` and runs the command again rather than calling it
    /// failed.
    pub(crate) fn read_exit_code(&self, key: &str) -> Option<i32> {
        let raw = std::fs::read_to_string(self.exit_path(key)).ok()?;
        let text = raw.trim();
        if text.is_empty() {
            return None;
        }
        Some(text.parse().unwrap_or(1))
    }

    /// Make room for a fresh run: drop whatever an earlier arrival at this key
    /// left behind, so it cannot make the run about to start look finished
    /// before it has written a line.
    ///
    /// The result is worth reading. This used to be two deletions with both
    /// errors thrown away, and on Windows a deletion that failed was the
    /// ordinary case rather than an exotic one — see [`disown`]. A caller
    /// that carried on regardless left the previous arrival's exit code
    /// sitting where the next read would find it, and
    /// [`crate::command_step::Runs::state`] reads the exit code before the
    /// pid, so that stale file decided the answer and nothing later
    /// corrected it. A step re-run at a key it had been at before was routed
    /// on the last attempt's result without having run at all.
    pub(crate) fn clear(&self, key: &str) -> Result<()> {
        disown(&self.exit_path(key))?;
        disown(&self.pid_path(key))?;
        Ok(())
    }
}

/// Leave nothing at `path` that a later read could believe.
///
/// Deleting it is the whole intent, and on Unix the first line is the end of
/// the story. Windows refuses to delete a file anything still holds open, and
/// something reliably does: the wrapper writes its exit code one statement
/// before the process carrying that handle goes away, so the moment a run is
/// read as finished is the same moment its files are hardest to remove. This
/// is the exact asymmetry [`crate::command_step::Runs::roll_log_aside`] was
/// written for, one call later in the same function — it waits the handle out
/// while the deletion beside it did not.
///
/// So *empty* it when it will not go. That is a plain write, which the same
/// lingering handle does allow where a delete is refused, and it settles the
/// question rather than waiting on it: an empty file carries no pid and no
/// exit code, and both readers here already answer `None` for one. The file
/// itself is tidied on the second attempt if it can be, and left harmless if
/// it cannot.
///
/// An error means neither was possible, which is a filesystem a run has no
/// business being started against — see [`RunFiles::clear`]'s own doc.
fn disown(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    std::fs::File::create(path).with_context(|| {
        format!(
            "could not remove or empty {} — a stale run file a new run would be read by",
            path.display()
        )
    })?;
    let _ = std::fs::remove_file(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `clear` has to leave nothing a later read could believe, and deleting
    /// is only its first choice — see [`disown`] for the platform that
    /// refuses it. Emptying is the fallback, and it settles the same question
    /// because neither reader here believes an empty file.
    ///
    /// What is asserted is the contract, not the branch: after this, neither
    /// file answers. Which of the two ways got there is the platform's to
    /// decide, and only Windows ever takes the second — there is no portable
    /// way to arrange a file that refuses to be deleted but agrees to be
    /// written, so that arm is covered by the Windows job rather than here.
    #[test]
    fn clearing_leaves_nothing_a_later_read_can_believe() {
        let dir = crate::scratch::root("runfiles-cleared");
        std::fs::create_dir_all(&dir).unwrap();
        let files = RunFiles::new(dir.clone());

        std::fs::write(files.pid_path("demo"), "4242").unwrap();
        std::fs::write(files.exit_path("demo"), "3").unwrap();
        files.clear("demo").unwrap();
        assert_eq!(files.read_pid("demo"), None);
        assert_eq!(files.read_exit_code("demo"), None);

        // Clearing a key that was never used is not a failure.
        files.clear("never-ran").unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The wrapper creates the exit file and then writes to it, so a reader
    /// can arrive in between — see [`RunFiles::read_exit_code`] for what
    /// calling that moment "exit 1" did to a command step that had succeeded.
    #[test]
    fn an_exit_file_with_nothing_in_it_yet_is_not_a_code() {
        let dir = crate::scratch::root("runfiles-half-written");
        std::fs::create_dir_all(&dir).unwrap();
        let files = RunFiles::new(dir.clone());

        std::fs::write(files.exit_path("demo"), "").unwrap();
        assert_eq!(files.read_exit_code("demo"), None);
        std::fs::write(files.exit_path("demo"), "  \n").unwrap();
        assert_eq!(files.read_exit_code("demo"), None);

        // Something that is not a number still reads as a failure, which is
        // the reading that doc asks for and this change leaves alone.
        std::fs::write(files.exit_path("demo"), "not-a-code").unwrap();
        assert_eq!(files.read_exit_code("demo"), Some(1));
        std::fs::write(files.exit_path("demo"), "0").unwrap();
        assert_eq!(files.read_exit_code("demo"), Some(0));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_key_with_no_files_reads_as_nothing() {
        let dir = crate::scratch::root("runfiles-empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = RunFiles::new(dir.clone());

        assert_eq!(files.read_pid("demo"), None);
        assert_eq!(files.read_exit_code("demo"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_pid_and_exit_files_round_trip() {
        let dir = crate::scratch::root("runfiles-round-trip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = RunFiles::new(dir.clone());

        std::fs::write(files.pid_path("demo"), "1234\n").unwrap();
        assert_eq!(files.read_pid("demo"), Some(1234));

        std::fs::write(files.exit_path("demo"), "3\n").unwrap();
        assert_eq!(files.read_exit_code("demo"), Some(3));

        // Unreadable content is a wrapper that ran — read as a failure, not
        // as still going.
        std::fs::write(files.exit_path("demo"), "not-a-number").unwrap();
        assert_eq!(files.read_exit_code("demo"), Some(1));

        files.clear("demo").unwrap();
        assert_eq!(files.read_pid("demo"), None);
        assert_eq!(files.read_exit_code("demo"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
