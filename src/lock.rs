//! A single-holder lock for the dispatcher.
//!
//! Two dispatchers running against one repo would both see the same task at the
//! same step and both spawn a lane into its worktree — the concurrent-write
//! corruption that per-task worktrees exist to prevent in the first place. The
//! lock makes that impossible rather than unlikely.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const LOCK_FILE: &str = "dispatch.pid";

/// Beside `dispatch.pid`: how many starts in a row could not run at all, and
/// since when — see [`Restarts`].
pub const RESTART_FILE: &str = "dispatch.restarts";

/// The third line of the lock file: how the run answers to a task that cannot
/// go on. Words rather than a bare bool, because this file is read by people
/// looking for a wedged dispatcher and `true` on a line of its own answers a
/// question they cannot see.
const UNATTENDED: &str = "unattended";
const ATTENDED: &str = "attended";

/// Held for as long as a dispatcher is running; released on drop.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Take the lock, or fail naming the process that already holds it.
    ///
    /// `path` is the lock file itself — [`crate::repo::Repo::lock_file`] for
    /// every real caller, and a scratch path a test builds by hand.
    ///
    /// `unattended` is written down rather than kept in the dispatcher's memory
    /// because it is not only the dispatcher's business: every lane runs
    /// `spoolway report` as its own process, and that command decides for itself
    /// whether a block parks the task or resumes it. A `--unattended` that lived
    /// on the dispatcher alone would give a run whose dispatcher never stopped
    /// and whose lanes parked themselves anyway.
    pub fn acquire(path: &Path, unattended: bool) -> Result<Lock> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pid = std::process::id();
        let mode = match unattended {
            true => UNATTENDED,
            false => ATTENDED,
        };
        let contents = format!("{pid}\n{}\n{mode}\n", started_at(pid).unwrap_or_default());

        // Two attempts, not one: a `path` that is already there might be a
        // live holder's, which is refused below naming it, or one a crashed
        // process left behind, which is safe to clear and retry. Any number
        // of callers racing that removal still only ever gets one winner —
        // the rest see the link fail again on their retry and report
        // whoever that was.
        for _ in 0..2 {
            match link_into_place(path, &contents) {
                Ok(true) => {
                    return Ok(Lock {
                        path: path.to_path_buf(),
                    });
                }
                Ok(false) => match Lock::holder(path)? {
                    Some(pid) => bail!(
                        "a dispatcher is already running for this repo (pid {pid}). \
                         It re-reads the queue every pass, so anything just queued starts on its own — \
                         stop that one first if you really do want a different run. \
                         If that pid is not a dispatcher, delete {}.",
                        path.display()
                    ),
                    // Stale: nothing alive holds it. Best effort — if the
                    // removal itself loses a race to someone else clearing
                    // the same stale file, the retry below still lands on a
                    // fresh link either way.
                    None => {
                        let _ = std::fs::remove_file(path);
                    }
                },
                Err(e) => return Err(e).with_context(|| format!("writing {}", path.display())),
            }
        }

        bail!(
            "could not take the lock at {} — it kept being replaced out from under this attempt",
            path.display()
        )
    }

    /// Whether the run in progress stops for a person, or `None` when there is
    /// no run — no lock, a stale one, or one written before this line existed.
    ///
    /// The lock is the right place to ask because the question is about *this
    /// run*, not about the project: `spoolway dispatch --unattended` is a
    /// decision somebody made this afternoon, and config.toml never hears about
    /// it. A lane's `spoolway report` reads it here and gets the answer its own
    /// dispatcher started with, however the two disagree with the file.
    ///
    /// `None` from a stale lock is the honest answer rather than a cautious
    /// one: a person running `spoolway report` by hand in their own checkout
    /// with no dispatcher up is not in a run at all, and the project's own
    /// setting is what should speak for them.
    pub fn unattended(path: &Path) -> Option<bool> {
        // Through `holder`, so a lock file a crashed dispatcher left behind
        // cannot keep answering for a run that ended.
        Lock::holder(path).ok().flatten()?;
        let raw = std::fs::read_to_string(path).ok()?;
        match raw.lines().nth(2)?.trim() {
            UNATTENDED => Some(true),
            ATTENDED => Some(false),
            _ => None,
        }
    }

    /// The pid of a live dispatcher, if there is one.
    ///
    /// A lock file left behind by a crashed process is not a live holder, so a
    /// crash never wedges the pipeline until someone deletes a file by hand.
    ///
    /// Which is why the pid alone is not enough. A pid is a number the kernel
    /// hands back out, so a lock file that outlives its process eventually names
    /// something else entirely — and every check from then on says a dispatcher
    /// is running. The second line pins the answer to *that* process: a start
    /// time, which no reuse of the number can reproduce.
    pub fn holder(path: &Path) -> Result<Option<u32>> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };

        let mut lines = raw.lines();
        let pid: u32 = match lines.next().unwrap_or_default().trim().parse() {
            Ok(pid) => pid,
            // An unreadable lock file is stale by definition.
            Err(_) => return Ok(None),
        };

        if !is_running(pid) {
            return Ok(None);
        }

        // Both sides have to have an answer for the comparison to mean
        // anything. A file written before this line existed has none, and a
        // platform with no way to ask has none either — in both cases the pid
        // on its own is what there is, and it is what the check was before.
        let recorded = lines.next().unwrap_or_default().trim();
        match (recorded.is_empty(), started_at(pid)) {
            (false, Some(now)) if now != recorded => Ok(None),
            _ => Ok(Some(pid)),
        }
    }
}

/// A short-lived advisory lock over one task file's read-modify-write.
///
/// The dispatcher reads a task at the top of a pass, mutates the copy in
/// memory and writes it back from many places for the rest of that pass;
/// `spoolway report` does the same read-modify-write from another process,
/// mid-turn. With nothing between them the later `rename` wins outright, so a
/// pass can silently overwrite a report that landed while it was working —
/// review finding 2. This serialises the two.
///
/// It is a *task* lock, not [`Lock`], the dispatcher's single-holder one:
/// many are held at once, one per task, and — like any lock a pass takes —
/// it must never be held across a multiplexer call. Staleness is
/// [`Lock::holder`]'s: a holder that crashed mid-write left a file naming a
/// dead pid (or a live pid whose start time is not the recorded one), and
/// the next acquire reaps it.
pub struct TaskLock {
    path: PathBuf,
}

impl TaskLock {
    /// How long to wait on a lock a live process holds before giving up. A
    /// task read-modify-write is a render and a rename — a holder still in
    /// one after this long has stalled, and the caller proceeds without the
    /// lock rather than failing a whole dispatch pass for one task. A
    /// *crashed* holder does not wait this out: its file is stale by
    /// [`Lock::holder`] and is cleared on the first retry.
    const WAIT: Duration = Duration::from_secs(3);

    /// Take the lock, waiting out a live holder up to [`TaskLock::WAIT`] and
    /// reaping a crashed holder's file on the way. Returns `Err` when a live
    /// process still holds it after the wait — callers treat that as "the
    /// read-modify-write is not serialised this time, proceed unlocked"
    /// rather than an error to propagate, because failing a whole dispatch
    /// pass for one contended task file is the worse outcome.
    pub fn acquire(path: &Path) -> Result<TaskLock> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pid = std::process::id();
        let contents = format!("{pid}\n{}\n", started_at(pid).unwrap_or_default());
        let deadline = std::time::Instant::now() + Self::WAIT;
        loop {
            match link_into_place(path, &contents) {
                Ok(true) => {
                    return Ok(TaskLock {
                        path: path.to_path_buf(),
                    });
                }
                Ok(false) => match Lock::holder(path)? {
                    // A live holder: wait a moment and try again, up to the
                    // deadline, then return `Err` — the caller reads that as
                    // "proceed without the lock" (see `acquire`'s own doc).
                    Some(pid) => {
                        if std::time::Instant::now() >= deadline {
                            bail!(
                                "task lock at {} is still held by pid {pid} after {:?}",
                                path.display(),
                                Self::WAIT
                            );
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    // Stale — nothing alive holds it. Clear and retry; a
                    // caller that loses that removal race still lands on a
                    // fresh link on the next turn of the loop.
                    None => {
                        let _ = std::fs::remove_file(path);
                    }
                },
                Err(e) => return Err(e).with_context(|| format!("writing {}", path.display())),
            }
        }
    }
}

impl Drop for TaskLock {
    fn drop(&mut self) {
        // Only if it still names this process — a lock judged stale and
        // replaced out from under a slow holder must not have that holder's
        // own drop delete the new owner's file. Same reasoning as [`Lock`].
        if let Ok(Some(holder)) = Lock::holder(&self.path)
            && holder == std::process::id()
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// How many consecutive starts have failed to run at all, and why the last
/// one did — the state a caller restarting the engine in a tight loop is
/// finally refused by.
///
/// A cycle inside a pipeline is refused by `pipeline check` because a graph
/// with no way out is a defect the engine can see on its own. A caller
/// restarting a dispatcher that can never run is the same shape one level
/// up — nothing inside a single pass loops, but the process outside it does
/// — and the engine has just as little business trusting that it will stop
/// on its own. This is what counts it.
///
/// Only a start that *could not run at all* counts — the lock already held
/// by another dispatcher, today. An empty queue is an ordinary ending, not a
/// failure to run, and never touches this file. A start that does run, or
/// one made with `--force`, clears it: the guard is for a restart storm
/// against a repo that can never move, not for the ordinary idle stretches
/// between real runs.
pub struct Restarts;

impl Restarts {
    /// Record one start that could not run, for `reason`. Returns the count
    /// now standing — restarted at 1 if the *previous* refusal fell more
    /// than `window` ago, since a caller that gave up for a while and tried
    /// again is not the tight loop this guards against.
    ///
    /// The window is judged from that previous refusal, not from the first
    /// one in the run: a fixed start would mean a caller restarting every
    /// `window`-minus-a-second, forever, eventually ages out of its own
    /// window and is let through, which is exactly the storm this exists to
    /// catch. Sliding it to the most recent refusal instead means the count
    /// only ever resets on an actual gap — the caller genuinely stopping
    /// and trying again later, not just the clock outrunning where the
    /// window happened to start.
    pub fn note_refusal(path: &Path, reason: &str, window: Duration) -> Result<u32> {
        let now = now_secs();
        let count = match Self::read(path)? {
            Some((count, last, _)) if now - last <= window.as_secs() as i64 => count,
            // No file, an unreadable one, or one whose last refusal is
            // further back than `window`: a fresh count starts here.
            _ => 0,
        };
        let count = count + 1;
        Self::write(path, count, now, reason)?;
        Ok(count)
    }

    /// The count and last reason, if a caller asking right now would be
    /// refused: at least `threshold` refusals, the most recent inside
    /// `window`.
    pub fn status(path: &Path, threshold: u32, window: Duration) -> Result<Option<(u32, String)>> {
        let now = now_secs();
        Ok(match Self::read(path)? {
            Some((count, last, reason))
                if count >= threshold && now - last <= window.as_secs() as i64 =>
            {
                Some((count, reason))
            }
            _ => None,
        })
    }

    /// A start that ran, or `--force`: the storm is over, whichever it was.
    pub fn clear(path: &Path) -> Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    /// `count\nlast\nreason`, where `last` is when the most recent refusal
    /// landed. Unreadable, short or otherwise corrupt is no different from
    /// absent — a file mangled by a crash or a hand edit must not itself
    /// refuse every start from then on; see [`note_refusal`](Self::note_refusal)
    /// and [`status`](Self::status), which both fall back to "no storm
    /// standing" the same way an absent file does.
    fn read(path: &Path) -> Result<Option<(u32, i64, String)>> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut lines = raw.lines();
        let count = lines.next().and_then(|l| l.trim().parse().ok());
        let last = lines.next().and_then(|l| l.trim().parse().ok());
        let reason = lines.next().unwrap_or_default().to_string();
        Ok(match (count, last) {
            (Some(count), Some(last)) => Some((count, last, reason)),
            _ => None,
        })
    }

    fn write(path: &Path, count: u32, last: i64, reason: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // One line: a reason with a newline in it would shift the fields
        // after it on the next read.
        let reason = reason.replace('\n', " ");
        std::fs::write(path, format!("{count}\n{last}\n{reason}\n"))
            .with_context(|| format!("writing {}", path.display()))
    }
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Puts `contents` at `path`, but only if nothing is there yet — `true` on
/// success, `false` if `path` was already taken.
///
/// Not `File::create_new` followed by a write: that is two syscalls, and the
/// file `create_new` makes is visible, empty, to anyone else racing the same
/// path in the gap between them. A reader who lands there — this function's
/// own caller, retrying after losing a race — reads an empty file, which
/// `Lock::holder` cannot parse a pid out of and so calls stale, and deletes
/// out from under a write that was never incomplete, only unlucky in its
/// timing. Writing the whole, already-assembled `contents` to a temp file
/// first and hard-linking that into `path` closes the gap: the link either
/// succeeds and `path` is the temp file's complete content in one atomic
/// step, or fails with nothing at `path` touched at all.
fn link_into_place(path: &Path, contents: &str) -> std::io::Result<bool> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{unique}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    let linked = std::fs::hard_link(&tmp, path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// Something about a process that a later process with the same pid cannot have.
///
/// The value is opaque and only ever compared with itself, so each platform
/// answers in whatever unit it already keeps — this is never parsed, formatted
/// or shown to anyone.
#[cfg(target_os = "linux")]
fn started_at(pid: u32) -> Option<String> {
    // Field 22 of /proc/<pid>/stat, in clock ticks since boot. Counted from the
    // *last* `)` rather than split from the left, because field 2 is the
    // executable name and a process is free to have a space or a bracket in it.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(windows)]
fn started_at(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: a failed open returns null and is checked; the handle is closed
    // on every path out, and the four FILETIMEs are owned by this frame.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        // Same reasoning as `is_running`: a process owned by another user
        // exists but will not open. Without a start time the pid stands alone,
        // which is the conservative answer for a lock.
        return None;
    }

    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    unsafe { CloseHandle(handle) };

    (ok != 0).then(|| {
        format!(
            "{}",
            (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
        )
    })
}

/// No portable way to ask, so the pid stands alone — exactly as it did before.
#[cfg(all(unix, not(target_os = "linux")))]
fn started_at(_pid: u32) -> Option<String> {
    None
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Only remove the file if it still names this process. A dispatcher
        // that hangs long enough to be judged dead by a second one has its
        // stale lock cleared and replaced out from under it — see the retry
        // in `acquire` — and this process's own eventual drop must not then
        // delete the second dispatcher's lock in turn. Best effort either
        // way: if this fails the next acquire sees a dead pid and treats the
        // file as stale anyway.
        if let Ok(Some(holder)) = Lock::holder(&self.path)
            && holder == std::process::id()
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

// `pub(crate)` for [`crate::headless::alive`], which needs the same answer on
// the platforms that have no `/proc` to read it from.
#[cfg(target_os = "linux")]
pub(crate) fn is_running(pid: u32) -> bool {
    // The same distinction `headless::alive` draws: a zombie is still in
    // `/proc` but has already stopped running, and counting it as a live
    // holder wedges the lock until something reaps it — which, under a
    // supervisor that never does, is never.
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|state| state != "Z")
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn is_running(pid: u32) -> bool {
    // `kill -0` reports whether a signal could be delivered, without sending
    // one. Absent /proc this is the portable equivalent.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Windows has neither `/proc` nor `kill`.
///
/// This used to fall through to the `kill -0` arm, where the missing binary
/// made `.status()` an error and `unwrap_or(false)` reported *every* holder as
/// dead. A lock that always reads stale is not a lock: two dispatchers would
/// both take it and both drive lanes into the same worktrees, which is the
/// concurrent-write corruption this module exists to make impossible.
#[cfg(windows)]
pub(crate) fn is_running(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };

    // One of the standard access rights every kernel object shares, and a
    // fixed part of the Win32 ABI. Written out because windows-sys re-exports
    // it only as a `FILE_ACCESS_RIGHTS` under `Win32::Storage::FileSystem`,
    // and pulling in a filesystem module to name a right being asked for on a
    // *process* would be the more confusing of the two.
    const SYNCHRONIZE: u32 = 0x0010_0000;

    // Pid 0 is the System Idle Process, never a dispatcher. Windows is
    // inconsistent about which error `OpenProcess` reports for it, and one of
    // the candidates is the access-denied that means "exists" below — so it is
    // settled here rather than left to the platform. `/proc/0` does not exist,
    // so this is also what Linux already answers.
    if pid == 0 {
        return false;
    }

    // SYNCHRONIZE is not optional: without it the handle opens fine and the
    // wait below fails with WAIT_FAILED, which reads as "not running" and
    // makes every lock look stale — the exact bug this function was written to
    // fix, reintroduced one access right further down.
    //
    // SAFETY: a failed open returns null and is checked; the handle is closed
    // on every path out.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        // A process owned by another user exists but will not open. Reporting
        // it as running is the conservative direction for a lock — it refuses
        // to start a second dispatcher rather than allowing one — and it
        // matches Linux, where `/proc/<pid>` is visible whoever owns it.
        return unsafe { windows_sys::Win32::Foundation::GetLastError() } == ERROR_ACCESS_DENIED;
    }

    // Windows keeps a pid alive for as long as anything holds a handle to it,
    // so an open handle does not by itself mean the process still runs. The
    // wait distinguishes them: a live process never signals, an exited one
    // signals immediately. `GetExitCodeProcess` would be the usual reflex and
    // is ambiguous — a process that genuinely exits with 259 is
    // indistinguishable from STILL_ACTIVE.
    let alive = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    unsafe { CloseHandle(handle) };
    alive
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock file's own path, in a scratch directory of its own — standing
    /// in for [`crate::repo::Repo::lock_file`], which every real caller uses.
    fn scratch(name: &str) -> PathBuf {
        let dir = crate::scratch::root(&format!("lock-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(LOCK_FILE)
    }

    /// The property every other test here rests on: this very process, which
    /// is indisputably running, has to be reported as running. Asserted
    /// directly because when it is wrong the symptom is a *lock that silently
    /// never holds*, and the tests below then fail in a way that points at
    /// locking rather than at process detection.
    #[test]
    fn the_running_process_is_detected_as_running() {
        assert!(is_running(std::process::id()));
    }

    /// The pid in a stale lock file is eventually handed to something else, and
    /// from that moment a pid-only check says a dispatcher is running forever.
    /// Stand in for the reuse by writing this process's pid against a start
    /// time that is not this process's — which is what a reused pid looks like.
    #[test]
    fn a_pid_reused_by_another_process_is_not_a_holder() {
        let path = scratch("reused");

        std::fs::write(&path, format!("{}\nnot-this-process\n", std::process::id())).unwrap();
        assert_eq!(Lock::holder(&path).unwrap(), None);
        // And so the lock is takeable, rather than wedged until someone finds
        // the file and deletes it.
        assert!(Lock::acquire(&path, false).is_ok());
    }

    /// Written by a version that recorded only the pid, or on a platform with
    /// no way to ask for a start time. The pid alone still has to be honoured.
    #[test]
    fn a_lock_file_with_no_start_time_still_names_its_holder() {
        let path = scratch("pid-only");

        std::fs::write(&path, format!("{}\n", std::process::id())).unwrap();
        assert_eq!(Lock::holder(&path).unwrap(), Some(std::process::id()));
    }

    #[test]
    fn a_start_time_is_stable_and_tells_this_process_from_its_pid() {
        // Nothing to compare against on a platform that cannot answer, but the
        // call still has to be harmless there.
        if let Some(first) = started_at(std::process::id()) {
            assert!(!first.is_empty());
            assert_eq!(
                started_at(std::process::id()).as_deref(),
                Some(first.as_str())
            );
        }
    }

    #[test]
    fn a_second_acquire_is_refused_while_the_first_is_held() {
        let path = scratch("held");
        let _first = Lock::acquire(&path, false).unwrap();
        assert!(Lock::acquire(&path, false).is_err());
    }

    #[test]
    fn the_lock_is_released_on_drop() {
        let path = scratch("drop");
        {
            let _lock = Lock::acquire(&path, false).unwrap();
            assert!(Lock::holder(&path).unwrap().is_some());
        }
        assert!(Lock::holder(&path).unwrap().is_none());
        Lock::acquire(&path, false).unwrap();
    }

    /// The run's mode is what a lane's own `spoolway report` reads to decide
    /// whether a block parks or resumes, so it has to survive the trip through
    /// the file — and has to stop being an answer the moment the run is over.
    #[test]
    fn the_lock_carries_the_runs_mode_and_only_while_the_run_is_live() {
        let path = scratch("mode");
        {
            let _lock = Lock::acquire(&path, true).unwrap();
            assert_eq!(Lock::unattended(&path), Some(true));
        }
        // Released: there is no run to have a mode, so the project's own
        // setting is what speaks.
        assert_eq!(Lock::unattended(&path), None);

        let _attended = Lock::acquire(&path, false).unwrap();
        assert_eq!(Lock::unattended(&path), Some(false));
    }

    /// A lock written before the third line existed still names its holder, and
    /// simply has nothing to say about the mode.
    #[test]
    fn a_lock_file_with_no_mode_line_is_still_a_holder() {
        let path = scratch("pid-and-time-only");

        let pid = std::process::id();
        std::fs::write(
            &path,
            format!("{pid}\n{}\n", started_at(pid).unwrap_or_default()),
        )
        .unwrap();
        assert_eq!(Lock::holder(&path).unwrap(), Some(pid));
        assert_eq!(Lock::unattended(&path), None);
    }

    /// The bug this task closes: `holder()` then a plain `write` left a
    /// window between the check and the write that a second caller could
    /// land in and take the lock right alongside the first. Real OS threads
    /// exercise the same `hard_link` two dispatcher processes racing the same
    /// lock file would — the atomicity `link_into_place` relies on is the
    /// kernel's, not anything specific to being a separate process — so this
    /// is a faithful stand-in for two dispatchers starting at once.
    #[test]
    fn two_contenders_for_one_lock_leave_exactly_one_holder() {
        let path = scratch("contend");
        let contenders = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(contenders));
        let handles: Vec<_> = (0..contenders)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Lock::acquire(&path, false)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            results.iter().filter(|r| r.is_ok()).count(),
            1,
            "exactly one of the racing acquires should win the lock"
        );
    }

    #[test]
    fn a_lock_file_from_a_dead_process_is_stale() {
        let path = scratch("stale");
        // Pid 0 is never a live user process, so this stands in for the file a
        // crashed dispatcher leaves behind.
        std::fs::write(&path, "0\n").unwrap();
        assert!(Lock::holder(&path).unwrap().is_none());
        Lock::acquire(&path, false).unwrap();
    }

    /// The per-task lock releases on drop, and a second acquire then
    /// succeeds — the read-modify-write it guards is short, and one held
    /// forever would wedge every pass and every `spoolway report` for that
    /// task.
    #[test]
    fn a_task_lock_is_taken_then_released_on_drop() {
        let path = scratch("task-lock").with_file_name("demo.lock");
        {
            let _held = TaskLock::acquire(&path).unwrap();
            assert!(Lock::holder(&path).unwrap().is_some());
        }
        assert!(Lock::holder(&path).unwrap().is_none());
        // And re-takeable.
        let _again = TaskLock::acquire(&path).unwrap();
    }

    /// A task lock file a crashed holder left behind names a dead pid, so
    /// [`Lock::holder`] reads it as stale and the next acquire reaps it
    /// rather than waiting the whole [`TaskLock::WAIT`] out.
    #[test]
    fn a_stale_task_lock_is_reaped_at_once() {
        let path = scratch("task-lock-stale").with_file_name("demo.lock");
        std::fs::write(&path, "0\n").unwrap();
        let started = std::time::Instant::now();
        let _lock = TaskLock::acquire(&path).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "a dead holder is cleared immediately, not waited out"
        );
    }

    /// [`Restarts`]' own path, beside the lock file `scratch` returns.
    fn restarts_scratch(name: &str) -> PathBuf {
        scratch(name).with_file_name(RESTART_FILE)
    }

    /// Below the threshold, nothing is refused — a caller that has only just
    /// started failing to run has not yet shown a pattern worth stopping.
    #[test]
    fn fewer_refusals_than_the_threshold_are_not_refused() {
        let path = restarts_scratch("below-threshold");
        for _ in 0..3 {
            Restarts::note_refusal(&path, "reason", Duration::from_secs(30)).unwrap();
        }
        assert_eq!(
            Restarts::status(&path, 4, Duration::from_secs(30)).unwrap(),
            None
        );
    }

    /// At the threshold, inside the window, a caller is refused — and told
    /// the count and the last of the reasons it was refused for.
    #[test]
    fn enough_refusals_inside_the_window_are_refused_with_the_count_and_reason() {
        let path = restarts_scratch("at-threshold");
        for reason in [
            "first reason",
            "second reason",
            "third reason",
            "fourth reason",
        ] {
            Restarts::note_refusal(&path, reason, Duration::from_secs(30)).unwrap();
        }
        let (count, reason) = Restarts::status(&path, 4, Duration::from_secs(30))
            .unwrap()
            .expect("four refusals inside the window should refuse the fifth start");
        assert_eq!(count, 4);
        assert_eq!(reason, "fourth reason");
    }

    /// A refusal outside the window does not accumulate onto an older one —
    /// a caller that gave up for a while and tried again starts its own
    /// count fresh, rather than inheriting a storm from an hour ago.
    #[test]
    fn a_refusal_outside_the_window_restarts_the_count() {
        let path = restarts_scratch("stale-window");
        // Written directly with an hour-old `last`, standing in for a
        // caller that gave up and tried again later — cheaper than a test
        // that actually sleeps out a window.
        Restarts::write(&path, 3, now_secs() - 3600, "old reason").unwrap();
        let count = Restarts::note_refusal(&path, "new reason", Duration::from_secs(30)).unwrap();
        assert_eq!(count, 1);
    }

    /// A refusal slides the window forward onto itself. Judging it from the
    /// first refusal in a run instead let a caller restarting every few
    /// seconds age out of a window anchored where it started, and be let
    /// through on the fifth try.
    ///
    /// Goes through `note_refusal` rather than hand-writing the final
    /// count, unlike the test this replaced — the bug lived entirely in
    /// which timestamp `note_refusal` carries forward, so a test built only
    /// from `Restarts::write` and `Restarts::status` cannot tell the
    /// anchored version from the sliding one; both read back whatever was
    /// written.
    #[test]
    fn a_refusal_slides_the_window_onto_itself() {
        let path = restarts_scratch("window-slides");
        // One refusal 20 seconds back, then a second one right now.
        Restarts::write(&path, 1, now_secs() - 20, "first").unwrap();
        let count = Restarts::note_refusal(&path, "second", Duration::from_secs(30)).unwrap();
        assert_eq!(count, 2);
        // Ten seconds is narrower than the 20-second gap to the first
        // refusal and wider than the zero-second gap to the second, so
        // this holds only if the window is judged from the most recent
        // one.
        assert!(
            Restarts::status(&path, 2, Duration::from_secs(10))
                .unwrap()
                .is_some()
        );
    }

    /// `clear` is what a start that actually runs, or `--force`, calls — and
    /// it has to make the file behave exactly as if it had never existed.
    #[test]
    fn clearing_the_counter_lets_a_fresh_storm_start_from_one() {
        let path = restarts_scratch("cleared");
        for _ in 0..4 {
            Restarts::note_refusal(&path, "reason", Duration::from_secs(30)).unwrap();
        }
        assert!(
            Restarts::status(&path, 4, Duration::from_secs(30))
                .unwrap()
                .is_some()
        );

        Restarts::clear(&path).unwrap();
        assert_eq!(
            Restarts::status(&path, 4, Duration::from_secs(30)).unwrap(),
            None
        );

        let count = Restarts::note_refusal(&path, "reason", Duration::from_secs(30)).unwrap();
        assert_eq!(count, 1);
    }

    /// A counter file that is absent, short or corrupt is treated as
    /// absent — a mangled file must not itself refuse every start.
    #[test]
    fn a_corrupt_counter_file_is_treated_as_absent() {
        let path = restarts_scratch("corrupt");
        std::fs::write(&path, "not-a-number\n").unwrap();
        assert_eq!(
            Restarts::status(&path, 4, Duration::from_secs(30)).unwrap(),
            None
        );
        // And still writable from here, rather than wedged.
        let count = Restarts::note_refusal(&path, "reason", Duration::from_secs(30)).unwrap();
        assert_eq!(count, 1);
    }
}
