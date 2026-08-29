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

    /// The exit file's content, parsed as a code — `None` only if the file
    /// does not exist at all. Content that is there but unreadable as a
    /// number reads as `1`: a wrapper that wrote something is a wrapper that
    /// ran, and that is the safer reading over reporting a run that plainly
    /// finished as still going.
    pub(crate) fn read_exit_code(&self, key: &str) -> Option<i32> {
        let raw = std::fs::read_to_string(self.exit_path(key)).ok()?;
        Some(raw.trim().parse().unwrap_or(1))
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
