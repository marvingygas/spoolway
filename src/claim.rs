//! The mark a dispatcher leaves on a task for as long as it is booting that
//! task's lane, so the board can say `◌ starting` rather than `○ queued`.
//!
//! A booting task has nothing else on disk to say so. Its stage moves off
//! `queued` only once `Mux::start_lane` has returned — see
//! `finish_launch_bookkeeping` in [`crate::dispatch`], which is where that
//! stays — and herdr lists no lane for it until then either, so for the
//! whole of an `agent start` the row read `waiting for a worker slot to free
//! up`: the opposite of what was happening. The board runs in another
//! process from the dispatcher (the dispatch tab's child writes, the tab
//! reads), so the mark has to be a file.
//!
//! One file per task under [`crate::repo::Repo::claims_dir`], holding the id
//! of the step being booted, rather than one shared file: the dispatcher
//! sets and clears a mark per lane, and a file of its own is written and
//! removed whole, with nothing to read back and merge.
//!
//! A mark outlives its dispatcher only when that dispatcher is killed
//! mid-boot. Two things keep that from pinning a row on `starting`: [`live`]
//! reads nothing unless the dispatch lock names a live process, and every
//! pass starts with [`clear`], so the next dispatcher's first pass sweeps
//! whatever the last one left.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::repo::Repo;

/// The directory [`crate::repo::Repo::claims_dir`] names, under the
/// project's home.
pub(crate) const CLAIMS_DIR: &str = "claims";

/// The marks one pass's boots hold, cleared as each boot returns — and,
/// through `Drop`, whatever a `?` out of the boot's own bookkeeping left
/// set, so no return path of a pass leaves a mark behind a live dispatcher.
pub(crate) struct Claims<'a> {
    repo: &'a Repo,
    held: Vec<String>,
}

impl<'a> Claims<'a> {
    pub(crate) fn new(repo: &'a Repo) -> Claims<'a> {
        Claims {
            repo,
            held: Vec::new(),
        }
    }

    /// Mark `task` as booting `step`. Best effort: the mark only changes
    /// what the board draws, and a boot is never refused over a word on a
    /// screen.
    pub(crate) fn claim(&mut self, task: &str, step: &str) {
        let _ = crate::task::write_atomic(&path(self.repo, task), step);
        if !self.held.iter().any(|held| held == task) {
            self.held.push(task.to_string());
        }
    }

    /// `task`'s boot has returned, whatever came of it.
    pub(crate) fn release(&mut self, task: &str) {
        self.held.retain(|held| held != task);
        let _ = std::fs::remove_file(path(self.repo, task));
    }
}

impl Drop for Claims<'_> {
    fn drop(&mut self) {
        for task in std::mem::take(&mut self.held) {
            let _ = std::fs::remove_file(path(self.repo, &task));
        }
    }
}

/// Remove every mark on disk — the first thing a pass does, for the marks
/// a dispatcher killed mid-boot left behind.
pub(crate) fn clear(repo: &Repo) {
    let _ = std::fs::remove_dir_all(repo.claims_dir());
}

/// Every task a live dispatcher is booting, and the step it is booting it
/// on. Empty unless the dispatch lock names a live process: a mark with no
/// dispatcher behind it is one a killed dispatcher never got to clear.
pub(crate) fn live(repo: &Repo) -> BTreeMap<String, String> {
    if !matches!(crate::lock::Lock::holder(&repo.lock_file()), Ok(Some(_))) {
        return BTreeMap::new();
    }
    all(repo)
}

/// Whether any mark on disk was written at or after `since` — the dispatch
/// tab's word that the dispatcher it started at `since` has claimed a slot.
/// Timed rather than merely present, since a mark a killed dispatcher left
/// stands until the new one's first pass sweeps it.
pub(crate) fn marked_since(repo: &Repo, since: std::time::SystemTime) -> bool {
    let Ok(entries) = std::fs::read_dir(repo.claims_dir()) else {
        return false;
    };
    entries.flatten().any(|entry| {
        !entry.file_name().to_string_lossy().starts_with('.')
            && entry
                .metadata()
                .and_then(|meta| meta.modified())
                .is_ok_and(|at| at >= since)
    })
}

/// Every mark on disk, live dispatcher or not.
fn all(repo: &Repo) -> BTreeMap<String, String> {
    let Ok(entries) = std::fs::read_dir(repo.claims_dir()) else {
        return BTreeMap::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let task = entry.file_name().into_string().ok()?;
            // `write_atomic`'s own temporary, caught mid-rename.
            if task.starts_with('.') {
                return None;
            }
            let step = std::fs::read_to_string(entry.path()).ok()?;
            let step = step.trim();
            (!step.is_empty()).then(|| (task, step.to_string()))
        })
        .collect()
}

fn path(repo: &Repo, task: &str) -> PathBuf {
    repo.claims_dir().join(task)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mark reads back only while a dispatcher holds the lock, and a
    /// release takes it off.
    #[test]
    fn a_mark_reads_only_under_a_live_lock_and_goes_on_release() {
        let (repo, _root_guard) = crate::status::testutil::fixture("claim-live-lock");
        let mut claims = Claims::new(&repo);
        claims.claim("t1", "implement");
        assert!(live(&repo).is_empty(), "no lock, no mark");

        let lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        assert_eq!(live(&repo).get("t1").map(String::as_str), Some("implement"));
        claims.release("t1");
        assert!(live(&repo).is_empty());
        drop(lock);
    }

    /// A mark still held when its `Claims` goes — a `?` out of the boot's
    /// bookkeeping — is cleared on the way out.
    #[test]
    fn a_held_mark_is_cleared_on_drop() {
        let (repo, _root_guard) = crate::status::testutil::fixture("claim-drop");
        {
            let mut claims = Claims::new(&repo);
            claims.claim("t1", "implement");
            claims.claim("t2", "review");
            assert_eq!(all(&repo).len(), 2);
        }
        assert!(all(&repo).is_empty());
    }

    /// A mark left by a dispatcher killed mid-boot is swept by `clear`.
    #[test]
    fn clear_sweeps_a_mark_left_behind() {
        let (repo, _root_guard) = crate::status::testutil::fixture("claim-clear");
        let mut claims = Claims::new(&repo);
        claims.claim("t1", "implement");
        std::mem::forget(claims);
        assert_eq!(all(&repo).len(), 1);
        clear(&repo);
        assert!(all(&repo).is_empty());
    }
}
