//! A temporary directory no other caller of [`root`] can be standing in.
//!
//! Every fixture in this crate builds its world under a directory in
//! `/tmp` and wipes that directory on the way in. Naming
//! one after the test that wanted it — `spoolway-dispatch-warmth` — reads
//! well and is wrong in exactly one situation, which used to be the normal
//! one here: **two `cargo test` processes at once**.
//!
//! The dispatcher runs one `test` step per task lane, and several lanes run
//! together, so two copies of this binary routinely walk the same suite side
//! by side. Sharing a name there means sharing a directory: one run's
//! `remove_dir_all` deletes the repository the other has just `git init`ed,
//! and the failures land as `not a git repository`, `Unable to read current
//! working directory`, or a detached lane that looks finished because the
//! other run's shell wrote the exit code. Fifty-odd tests failed that way in
//! one pass, none of them for a reason in the code they cover.
//!
//! So a root carries the process that asked for it and a number that never
//! repeats within that process. The name still leads, because a directory
//! left behind by a crashed run is only useful if you can tell what wrote it.
//!
//! That leaves nobody to delete a finished run's directory, since no later run
//! will ever ask for the same name — see [`reclaim_finished_runs`], which is
//! what stops them accumulating without number.
//!
//! [`root`] is not test-only any more: `spoolway doctor`'s own live pane
//! check opens one on a real machine, to run its trivial command in — see
//! `commands::doctor::live_pane`. That means an ordinary `spoolway doctor`
//! now pays [`reclaim_finished_runs`]'s cost too: the first `root` of any
//! process sweeps up to [`SWEEP_LIMIT`] finished runs' directories out of
//! the system temporary directory, the same side effect `cargo test` always
//! had, on a machine that also runs the test suite a lot.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicU64, Ordering};

/// Never repeats within one process, which is what tells apart two fixtures
/// built back to back under the same name — a name is usually just what the
/// test calls itself, so reuse is common.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// A path under the system temporary directory that belongs to this call
/// alone. Nothing is created here; the caller does that, as it did before.
pub(crate) fn root(name: &str) -> PathBuf {
    reclaim_finished_runs();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("spoolway-{name}-{}-{seq}", std::process::id()))
}

/// Set a path's modification time, file or directory, on either platform.
///
/// A test that dates an entry cannot just `File::options().write(true)` its
/// way there: a directory cannot be opened for writing, and the read-only
/// handle that works for one on Unix is refused outright on Windows, where
/// opening a directory at all takes `FILE_FLAG_BACKUP_SEMANTICS` and
/// `set_modified` needs `FILE_WRITE_ATTRIBUTES` on the handle whichever kind
/// of path it is.
#[cfg(test)]
pub(crate) fn set_mtime(path: &Path, to: std::time::SystemTime) {
    let mut options = std::fs::File::options();
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES, then FILE_FLAG_BACKUP_SEMANTICS — neither is
        // exported by windows-sys' std-adjacent modules, and both are fixed
        // parts of the Win32 ABI, written out the way `headless::JOB_OBJECT_TERMINATE`
        // is.
        options.access_mode(0x0100).custom_flags(0x0200_0000);
    }
    #[cfg(not(windows))]
    match path.is_dir() {
        true => options.read(true),
        false => options.write(true),
    };
    options.open(path).unwrap().set_modified(to).unwrap();
}

/// Delete what runs that are over left behind, once per process.
///
/// The naming above is what makes this necessary. A root carries the process
/// that asked for it and a number that never repeats, so two `cargo test`
/// processes never share a directory — and so no run ever picks a name an
/// earlier run used. A fixture wipes its own root on the way in, and there is
/// no way in for a name nobody will ask for again. Nothing reclaimed them, and
/// nothing ever would: one machine reached 351,766 of these directories.
///
/// So the reclaiming happens here, on the first root of each process. A
/// directory named `spoolway-<name>-<pid>-<seq>` whose pid is not a live
/// process belongs to a run that has finished, whether it finished cleanly or
/// crashed. A live pid is left alone whatever the directory's age, which is
/// the property the whole naming scheme exists to give — a neighbouring test
/// process mid-run must never have its fixtures deleted underneath it.
///
/// Under the temporary directory only, and only for the `spoolway-` prefix
/// with that exact trailing shape. A name that does not parse is somebody
/// else's and is skipped.
fn reclaim_finished_runs() {
    static DONE: Once = Once::new();
    DONE.call_once(sweep_now);
}

/// How many directories one process will delete before it gets on with the
/// tests.
///
/// A suite leaves a few hundred roots, so an ordinary run clears its whole
/// backlog and this ceiling is never reached. It is here for the machine that
/// has been running the suite for weeks without one: deleting a large backlog
/// takes minutes, and a sweep with no ceiling would spend them before the
/// first test ran, which reads as a hung suite and is a good way to get the
/// sweep deleted instead. Bounded, a backlog drains over the next few runs and
/// no single run pays for it.
const SWEEP_LIMIT: usize = 2_000;

/// One pass of the sweep, without the once-per-process gate.
///
/// Separate so the tests below can drive a pass whether or not they happen to
/// be the call that opened the gate. Nothing else should reach for it: a sweep
/// per fixture would read the whole temporary directory hundreds of times a
/// run for nothing.
fn sweep_now() {
    sweep_up_to(SWEEP_LIMIT);
}

/// Delete at most `limit` finished runs' directories. Returns how many went,
/// which is what lets a test see the ceiling hold.
fn sweep_up_to(limit: usize) -> usize {
    sweep_dir(&std::env::temp_dir(), limit)
}

/// The sweep over one named directory.
///
/// `dir` is a parameter so the tests below can sweep a directory of their own
/// making rather than the shared temporary one. Sweeping the real one in a test
/// would make the result depend on what every other process on the machine
/// happened to leave there.
fn sweep_dir(dir: &Path, limit: usize) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    // One liveness lookup per pid rather than per directory: a single run
    // leaves hundreds, and they all carry the same pid.
    let mut checked: HashMap<u32, bool> = HashMap::new();
    let mut removed = 0;
    for entry in entries.flatten() {
        if removed >= limit {
            break;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid) = finished_run_pid(name) else {
            continue;
        };
        let live = *checked
            .entry(pid)
            .or_insert_with(|| crate::lock::is_running(pid));
        if !live {
            // Counted whether or not the removal succeeded: a directory that
            // cannot be deleted is exactly the one a retry would spend the
            // rest of the budget on.
            let _ = std::fs::remove_dir_all(entry.path());
            removed += 1;
        }
    }
    removed
}

/// The pid in `spoolway-<name>-<pid>-<seq>`, or `None` for anything that is
/// not one of our roots.
///
/// `<name>` may itself contain digits and dashes — `dispatch-cut-from-dependency`
/// does — so the shape is read from the right rather than the left, and a
/// directory a person made called `spoolway-notes` or `spoolway-1` is left
/// alone.
///
/// A trailing word after the two numbers is allowed, and matters more than it
/// looks. Fixtures routinely want a second directory beside the root — a
/// scratch `$HOME` in `crate::commands::init`, a worktree root in
/// `crate::dispatch` — and they name it by appending to the root's own
/// basename, giving `spoolway-init-twice-3361287-23-home`. Those are roots too,
/// they outnumbered the plain ones on the machine this was written for, and a
/// sweep that only understood the bare shape would have left every one of them.
fn finished_run_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("spoolway-")?;
    let mut parts: Vec<&str> = rest.split('-').collect();

    // Drop a suffix like `home` or `worktrees`. Only non-numeric words go, so
    // the two numbers this is looking for are never mistaken for one.
    while parts
        .last()
        .is_some_and(|last| last.parse::<u64>().is_err())
    {
        parts.pop();
    }

    // What must remain: at least one word of name, then the pid, then the
    // sequence number.
    let seq = parts.pop()?;
    let pid = parts.pop()?;
    // `concat`, not `is_empty`: `spoolway--4242-7` leaves one empty word,
    // which is a name in the shape of one and not a name.
    if parts.concat().is_empty() {
        return None;
    }
    seq.parse::<u64>().ok()?;
    pid.parse::<u32>().ok()
}

/// `git init` in `root`, with an identity pinned into that repository's own
/// config.
///
/// The identity is the point. `git commit` with none configured locally falls
/// back to `$HOME/.gitconfig`, and a subprocess reads the real `$HOME`
/// whatever this process's tests are doing with theirs. Pinning the identity
/// into the repository takes the fixture off that dependency entirely: it
/// commits the same on a machine whose git has no global identity configured
/// at all, which is what CI usually is.
///
/// `args` are appended to `git init -q`, for the `-b <branch>` or `--bare` a
/// caller wants.
#[cfg(test)]
pub(crate) fn git_init(root: &Path, args: &[&str]) {
    let mut init = vec!["init", "-q"];
    init.extend_from_slice(args);
    crate::repo::run(root, "git", &init).unwrap();
    for (key, value) in [("user.email", "t@example.com"), ("user.name", "spoolway t")] {
        crate::repo::run(root, "git", &["config", key, value]).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two things a root has to survive: another process running the same
    /// suite, and the same name being asked for twice in this one.
    #[test]
    fn no_two_roots_are_ever_the_same_directory() {
        let a = root("same-name");
        let b = root("same-name");
        assert_ne!(a, b);
        for path in [&a, &b] {
            assert!(
                path.display()
                    .to_string()
                    .contains(&std::process::id().to_string()),
                "a root another test process would also compute: {}",
                path.display()
            );
        }
    }

    /// Only our own roots are recognised, and only in the exact shape `root`
    /// writes. Anything else under the temporary directory belongs to somebody
    /// else and must be invisible to the sweep.
    #[test]
    fn only_a_roots_own_shape_offers_up_a_pid() {
        assert_eq!(
            finished_run_pid("spoolway-dispatch-warmth-4242-7"),
            Some(4242)
        );
        // A name with its own digits and dashes still reads from the right.
        assert_eq!(
            finished_run_pid("spoolway-dispatch-cut-from-dependency-1000502-150"),
            Some(1000502)
        );

        // The sibling directories fixtures name after their own root. These
        // outnumbered the plain roots on the machine this sweep was written
        // for, so missing them would have missed most of the leak.
        assert_eq!(
            finished_run_pid("spoolway-init-twice-3361287-23-home"),
            Some(3361287)
        );
        assert_eq!(
            finished_run_pid("spoolway-dispatch-warmth-4242-7-worktrees"),
            Some(4242)
        );

        for other in [
            "spoolway",                   // no trailing fields at all
            "spoolway-notes",             // a person's directory
            "spoolway-1",                 // one field, not two
            "spoolway-warmth-4242",       // a pid but no sequence
            "spoolway--4242-7",           // no name left between the dashes
            "spoolway-warmth-abc-7",      // pid is not a number
            "spoolway-warmth-4242-xyz",   // sequence is not a number
            "spoolway-4242-7-home",       // a suffix, but no name before them
            "spoolway-warmth-4242-home",  // a suffix eats the sequence number
            "herdr-warmth-4242-7",        // not ours
            "tmp-spoolway-warmth-4242-7", // ours only in the middle
        ] {
            assert_eq!(finished_run_pid(other), None, "claimed {other}");
        }
    }

    /// The property the naming scheme exists for, kept by the sweep: a
    /// directory belonging to a process that is still running is never
    /// removed, however old it looks. This process is the live one to hand.
    #[test]
    fn a_live_processs_own_root_survives_the_sweep() {
        let mine = root("sweep-must-not-take-this");
        std::fs::create_dir_all(mine.join("deep")).unwrap();
        std::fs::write(mine.join("deep/file"), "still in use").unwrap();

        // A full pass, driven directly so this test does not depend on being
        // the call that opened the once-per-process gate.
        sweep_now();

        assert!(
            mine.join("deep/file").exists(),
            "the sweep took a live process's fixture: {}",
            mine.display()
        );
        std::fs::remove_dir_all(&mine).ok();
    }

    /// A finished run's directory does go, which is the whole point. A pid
    /// that cannot be running stands in for a run that has ended.
    #[test]
    fn a_finished_runs_root_is_reclaimed() {
        // `/proc/0` does not exist and pid 0 is never a process a person can
        // see — `crate::lock::is_running` settles that case deliberately, and
        // its own tests pin it.
        assert!(!crate::lock::is_running(0));

        let dir = root("reclaim");
        let dead = dir.join("spoolway-a-run-that-ended-0-999999");
        // The shape the leak actually left: a worktree registration inside,
        // pointing at a repository that has gone.
        std::fs::create_dir_all(dead.join("worktrees/second")).unwrap();
        std::fs::write(dead.join("worktrees/second/.git"), "gitdir: nowhere").unwrap();

        assert_eq!(sweep_dir(&dir, SWEEP_LIMIT), 1);

        assert!(
            !dead.exists(),
            "a directory whose process is long gone was kept: {}",
            dead.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The ceiling holds, so a machine with a huge backlog spends a bounded
    /// amount of time on it rather than stalling the suite before its first
    /// test. What is left over is the next run's to take.
    #[test]
    fn a_sweep_stops_at_its_ceiling_and_the_rest_keeps() {
        let dir = root("ceiling");
        std::fs::create_dir_all(&dir).unwrap();
        for n in 0..5 {
            // Pid 0, so every one of them reads as a run that has ended.
            std::fs::create_dir_all(dir.join(format!("spoolway-ceiling-{n}-0-7"))).unwrap();
        }
        // Not ours, and it must survive every pass.
        std::fs::create_dir_all(dir.join("someone-elses-directory")).unwrap();

        assert_eq!(sweep_dir(&dir, 2), 2, "a pass ignored its ceiling");
        assert_eq!(
            count(&dir),
            4,
            "3 finished runs and one bystander should remain"
        );

        assert_eq!(sweep_dir(&dir, 2), 2);
        assert_eq!(sweep_dir(&dir, 2), 1, "the last one is all that was left");
        assert_eq!(sweep_dir(&dir, 2), 0, "nothing left to take");

        assert!(
            dir.join("someone-elses-directory").exists(),
            "the sweep took a directory that was never ours"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn count(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }
}
