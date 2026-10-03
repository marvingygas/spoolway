//! One line per archived task, in `archive/index.jsonl`, so a reader can list
//! the archive without opening every `<id>.md` in it.
//!
//! Teardown renames a finished task's file into `archive/` and nothing else
//! records that it happened, so every reader that wants the list of archived
//! tasks has had to parse the whole folder — a cost that grows for as long as
//! history is kept. The index is a copy of what those readers use: id, group,
//! title, pipeline, branch, `depends_on`, worktree path and archived time.
//! The folder of `<id>.md` files stays exactly as it was and is still the
//! source of truth; the index can always be rebuilt from it.
//!
//! The index says whether it is current by its own modification time. Every
//! write here ends by setting that time to the folder's modification time, so
//! the two are equal when nothing has been added to or removed from
//! `archive/` since the index was last made to match it. That holds only if
//! no other process changes the folder between a writer's currency check and
//! its stamp, so every writer holds a [`Guard`] — the archive index lock —
//! across both, and so does everything else that changes what is in the
//! folder: it waits for the lock however long a rebuild takes. Only a pure
//! reader gives up on it, and then reads the task files without writing the
//! index. Any other change —
//! a hand edit, a file dropped in, an older install that archived without
//! writing a line — moves the folder's time away from the index's, and the
//! next [`read`] rebuilds. A line that does not parse (a write torn by a
//! crash) forces a rebuild for the same reason.
//!
//! Nothing reads the index yet beyond this module; readers move onto
//! [`read`] separately.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::lock::ArchiveIndexLock;
use crate::repo::Repo;
use crate::task::{self, Task};

/// The index's file name inside `archive/`. The retention sweep spares this
/// name, because the index is a map of the folder rather than a task in it
/// and its own age says nothing about whether it is still wanted.
pub const FILE_NAME: &str = "index.jsonl";

/// What a reader uses of an archived task, one per index line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub pipeline: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    pub archived_at: i64,
}

impl Entry {
    /// The entry for a parsed task, archived at `archived_at` (Unix seconds).
    pub fn from_task(task: &Task, archived_at: i64) -> Entry {
        Entry {
            id: task.front.id.clone(),
            group: task.front.group.clone().unwrap_or_default(),
            title: task.front.title.clone(),
            pipeline: task.front.pipeline.clone().unwrap_or_default(),
            branch: task.front.branch.clone().unwrap_or_default(),
            depends_on: task.front.depends_on.clone(),
            worktree_path: task.front.worktree_path.clone(),
            archived_at,
        }
    }
}

#[cfg(test)]
thread_local! {
    static REBUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Proof that the archive index lock is held, required by every function that
/// writes the index so none can be called without it.
pub struct Guard {
    /// Held for its `Drop`, which releases the lock.
    _lock: ArchiveIndexLock,
}

/// Take the archive index lock, waiting up to `wait`. `None` means the lock
/// was not taken: either someone else still held it when `wait` ran out, or
/// the lock file could not be created or read at all (for example a read-only
/// or full home). The two are not told apart, so a caller must not read `None`
/// as contention worth retrying; it carries on without writing the index.
pub fn lock_within(repo: &Repo, wait: std::time::Duration) -> Option<Guard> {
    ArchiveIndexLock::acquire(&repo.archive_index_lock_file(), wait)
        .ok()
        .map(|lock| Guard { _lock: lock })
}

/// The lock for a caller that changes `archive/`, which waits as long as it
/// takes — see [`ArchiveIndexLock::MUTATE_WAIT`] for why it cannot give up
/// early. `None` is a holder wedged for that whole wait, or a lock file that
/// could not be written; either way the caller carries on without the index,
/// which then reads as stale and rebuilds.
pub fn lock(repo: &Repo) -> Option<Guard> {
    lock_within(repo, ArchiveIndexLock::MUTATE_WAIT)
}

fn index_path(repo: &Repo) -> PathBuf {
    repo.archive_dir().join(FILE_NAME)
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Whether the index exists and was last made to match the folder as it is
/// now. Says nothing about whether every line parses; [`read`] checks that.
pub fn is_current(repo: &Repo) -> bool {
    let dir = repo.archive_dir();
    match (modified(&dir.join(FILE_NAME)), modified(&dir)) {
        (Some(index), Some(folder)) => index == folder,
        _ => false,
    }
}

/// Make the index's modification time equal the folder's, which is what
/// [`is_current`] compares. Best effort: a failure leaves the index looking
/// stale, and the next read rebuilds it.
fn stamp(repo: &Repo) {
    let dir = repo.archive_dir();
    let Some(folder) = modified(&dir) else {
        return;
    };
    if let Ok(file) = std::fs::File::options()
        .write(true)
        .open(dir.join(FILE_NAME))
    {
        let _ = file.set_modified(folder);
    }
}

/// Parse every line, or `None` if any non-blank line does not parse.
fn parse_lines(raw: &str) -> Option<Vec<Entry>> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Every archived task's entry, rebuilding the index first if it is missing,
/// holds a line that does not parse, or no longer matches the folder.
///
/// A current index is read without the lock. A rebuild takes it and checks
/// again, because another process may have repaired the index while this one
/// waited. If the lock cannot be had the entries are still returned, read
/// from the files, and the index is left alone.
// No reader of the archive calls this yet outside the tests; the board, queue
// tab and `queue add` are the intended callers.
#[allow(dead_code)]
pub fn read(repo: &Repo) -> Vec<Entry> {
    if let Some(entries) = read_current(repo) {
        return entries;
    }
    match lock_within(repo, ArchiveIndexLock::READ_WAIT) {
        Some(guard) => read_locked(repo, &guard),
        None => load_entries(repo),
    }
}

fn read_current(repo: &Repo) -> Option<Vec<Entry>> {
    if !is_current(repo) {
        return None;
    }
    parse_lines(&std::fs::read_to_string(index_path(repo)).ok()?)
}

fn read_locked(repo: &Repo, guard: &Guard) -> Vec<Entry> {
    read_current(repo).unwrap_or_else(|| rebuild(repo, guard))
}

/// Parse the `<id>.md` files into entries.
///
/// A task's archived time is its file's modification time: teardown renames
/// the file in without touching it, so that is the moment it was last
/// written before archiving. A file that will not parse is left out, as
/// every reader of the folder leaves it out today.
fn load_entries(repo: &Repo) -> Vec<Entry> {
    // The only place the task files are opened, so a test counts these to
    // prove a current index is read without them.
    #[cfg(test)]
    REBUILDS.with(|c| c.set(c.get() + 1));
    let (tasks, _problems) = task::load_dir(&repo.archive_dir()).unwrap_or_default();
    tasks
        .iter()
        .map(|task| {
            let archived_at = modified(&task.path)
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            Entry::from_task(task, archived_at)
        })
        .collect()
}

/// Rebuild the index from the `<id>.md` files and return what it now holds.
/// If the index cannot be written the entries are still returned, so a
/// read-only home reads slowly rather than wrongly.
fn rebuild(repo: &Repo, guard: &Guard) -> Vec<Entry> {
    let entries = load_entries(repo);
    let _ = write_all(repo, guard, &entries);
    entries
}

fn write_all(repo: &Repo, _guard: &Guard, entries: &[Entry]) -> Result<()> {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&serde_json::to_string(entry)?);
        out.push('\n');
    }
    task::write_atomic(&index_path(repo), out)?;
    stamp(repo);
    Ok(())
}

/// Bring the index up to date before a task is renamed into `archive/`, so
/// that [`record`] has a current index to append to. The guard must be held
/// from here until the append, so no other process's change lands between.
pub fn ensure_current(repo: &Repo, guard: &Guard) {
    let _ = read_locked(repo, guard);
}

/// Append the line for a task that has just been renamed into `archive/`.
///
/// One `write` call carries the whole line, so two archivers appending at
/// once cannot interleave half-lines. [`ensure_current`] must have run before
/// the rename: if the index was already stale then, the folder's change is
/// not this task's alone, so the index is left stale for the next read to
/// rebuild rather than stamped as current with lines missing.
pub fn record(
    repo: &Repo,
    _guard: &Guard,
    was_current: bool,
    task: &Task,
    archived_at: i64,
) -> Result<()> {
    if !was_current {
        return Ok(());
    }
    let mut line = serde_json::to_string(&Entry::from_task(task, archived_at))?;
    line.push('\n');
    let path = index_path(repo);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    drop(file);
    stamp(repo);
    Ok(())
}

/// Drop the line of every task whose `<id>.md` is no longer in `archive/`.
///
/// For the retention sweep, which deletes archive files without knowing which
/// ids they were. `was_current` is whether [`is_current`] held *before* the
/// deletions: only then is the folder's new modification time wholly
/// explained by them, and so safe to stamp onto the index. Otherwise the
/// index is left stale and the next read rebuilds it from what remains.
pub fn forget_missing(repo: &Repo, guard: &Guard, was_current: bool) {
    if !was_current {
        return;
    }
    let dir = repo.archive_dir();
    let Ok(raw) = std::fs::read_to_string(index_path(repo)) else {
        return;
    };
    let Some(entries) = parse_lines(&raw) else {
        return;
    };
    let kept: Vec<Entry> = entries
        .into_iter()
        .filter(|entry| dir.join(format!("{}.md", entry.id)).exists())
        .collect();
    let _ = write_all(repo, guard, &kept);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::{root, set_mtime};
    use std::time::Duration;

    fn repo_in(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let base = root(name);
        let _ = std::fs::remove_dir_all(&base);
        let repo = Repo {
            checkout: base.to_path_buf(),
            root: base.to_path_buf(),
            config: crate::config::Config::default(),
            home: base.join(".home"),
        };
        (repo, base)
    }

    fn archive_file(repo: &Repo, id: &str, depends_on: &str) -> Task {
        let path = repo.archive_dir().join(format!("{id}.md"));
        std::fs::write(
            &path,
            format!(
                "---\nid: {id}\ntitle: 'perf: {id}'\nstage: done\npipeline: impl\n\
                 group: g\nbranch: task/{id}\ndepends_on: [{depends_on}]\n\
                 worktree_path: /w/{id}\n---\n## Context\n"
            ),
        )
        .unwrap();
        Task::load(&path).unwrap()
    }

    fn parsed(repo: &Repo) -> Vec<Entry> {
        let (tasks, _) = task::load_dir(&repo.archive_dir()).unwrap();
        tasks
            .iter()
            .map(|t| {
                let at = modified(&t.path)
                    .unwrap()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;
                Entry::from_task(t, at)
            })
            .collect()
    }

    #[test]
    fn a_missing_index_is_rebuilt_from_the_files() {
        let (repo, base) = repo_in("aidx-missing");
        archive_file(&repo, "a", "");
        archive_file(&repo, "b", "a");
        assert!(!index_path(&repo).exists());

        let entries = read(&repo);

        assert_eq!(entries, parsed(&repo));
        assert_eq!(entries.len(), 2);
        assert!(index_path(&repo).exists(), "the rebuild was not written");
        assert!(is_current(&repo));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_line_that_does_not_parse_forces_a_rebuild() {
        let (repo, base) = repo_in("aidx-torn");
        archive_file(&repo, "a", "");
        read(&repo);
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(index_path(&repo))
            .unwrap();
        file.write_all(b"{\"id\":\"torn\",\"gro").unwrap();
        drop(file);
        stamp(&repo);
        assert!(is_current(&repo), "the torn line must be the only fault");

        let entries = read(&repo);

        assert_eq!(entries, parsed(&repo));
        assert!(
            !std::fs::read_to_string(index_path(&repo))
                .unwrap()
                .contains("torn")
        );
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_folder_changed_since_the_index_was_checked_forces_a_rebuild() {
        let (repo, base) = repo_in("aidx-stale");
        archive_file(&repo, "a", "");
        read(&repo);
        // A file dropped in by hand: the folder's time moves, the index's
        // does not.
        archive_file(&repo, "b", "a");
        set_mtime(
            &repo.archive_dir(),
            SystemTime::now() + Duration::from_secs(5),
        );
        assert!(!is_current(&repo));

        let entries = read(&repo);

        assert_eq!(entries, parsed(&repo));
        assert_eq!(entries.len(), 2);
        assert!(is_current(&repo));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn archiving_appends_exactly_one_line() {
        let (repo, base) = repo_in("aidx-append");
        archive_file(&repo, "a", "");
        read(&repo);
        let guard = lock(&repo).unwrap();
        let was_current = is_current(&repo);
        let task = archive_file(&repo, "b", "a");

        record(&repo, &guard, was_current, &task, 1_790_000_000).unwrap();

        let raw = std::fs::read_to_string(index_path(&repo)).unwrap();
        assert_eq!(raw.lines().count(), 2);
        let last: Entry = serde_json::from_str(raw.lines().last().unwrap()).unwrap();
        assert_eq!(last.id, "b");
        assert_eq!(last.depends_on, ["a"]);
        assert_eq!(last.worktree_path, Some(PathBuf::from("/w/b")));
        assert_eq!(last.archived_at, 1_790_000_000);
        assert!(is_current(&repo), "a recorded line must leave it current");
        std::fs::remove_dir_all(&base).ok();
    }

    /// A read of a current index never opens a task file. The files are
    /// replaced with junk first, so a read that opened them would also come
    /// back wrong, and the rebuild counter — the only code path that opens
    /// them — must not move.
    #[test]
    fn a_current_index_is_read_without_opening_any_task_file() {
        let (repo, base) = repo_in("aidx-no-reads");
        archive_file(&repo, "a", "");
        archive_file(&repo, "b", "a");
        let built = read(&repo);

        // Make every task file unreadable without touching the folder's own
        // modification time, which is what keeps the index current.
        let folder = modified(&repo.archive_dir()).unwrap();
        for id in ["a", "b"] {
            std::fs::write(repo.archive_dir().join(format!("{id}.md")), "not a task").unwrap();
        }
        set_mtime(&repo.archive_dir(), folder);
        assert!(is_current(&repo));
        REBUILDS.with(|c| c.set(0));

        let again = read(&repo);

        assert_eq!(again, built);
        assert_eq!(REBUILDS.with(std::cell::Cell::get), 0);
        // And the counter is not vacuous: a stale read does parse them.
        set_mtime(
            &repo.archive_dir(),
            SystemTime::now() + Duration::from_secs(5),
        );
        read(&repo);
        assert_eq!(REBUILDS.with(std::cell::Cell::get), 1);
        std::fs::remove_dir_all(&base).ok();
    }

    /// While another process holds the lock, a stale index is still read
    /// correctly from the files, and the held-off writer leaves the index
    /// alone rather than stamping a folder time it cannot vouch for.
    #[test]
    fn a_held_lock_keeps_a_reader_from_writing_the_index() {
        let (repo, base) = repo_in("aidx-held");
        archive_file(&repo, "a", "");
        let guard = lock(&repo).unwrap();
        assert!(
            lock_within(&repo, std::time::Duration::from_millis(60)).is_none(),
            "a second holder got the lock"
        );

        let entries = read(&repo);

        assert_eq!(entries, parsed(&repo));
        assert!(
            !index_path(&repo).exists(),
            "a reader wrote without the lock"
        );
        drop(guard);
        assert!(lock(&repo).is_some(), "the lock was not released on drop");
        std::fs::remove_dir_all(&base).ok();
    }
}
