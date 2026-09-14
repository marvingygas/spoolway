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

use std::path::PathBuf;

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
    pub(crate) fn clear(&self, key: &str) {
        let _ = std::fs::remove_file(self.exit_path(key));
        let _ = std::fs::remove_file(self.pid_path(key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        files.clear("demo");
        assert_eq!(files.read_pid("demo"), None);
        assert_eq!(files.read_exit_code("demo"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
