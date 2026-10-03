//! One line per archived task, in `archive/index.jsonl`, so a reader can list
//! the archive without opening every `<id>.md` in it.
//!
//! Teardown renames a finished task's file into `archive/` and nothing else
//! records that it happened, so every reader that wants the list of archived
//! tasks has had to parse the whole folder — a cost that grows for as long as
//! history is kept. The index is a copy of what those readers use: id, group,
//! title, pipeline, branch, `depends_on`, worktree path, base, whether the
//! task was a trial arm, issue slug and url, the file's creation time and the
//! archived time.
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
//! The board, the queue tab, `queue add` and eval read the archive through
//! [`read`] and [`read_at`]; none of them lists or parses the folder while
//! the index is current. A reader that needs more of one task than the index
//! holds opens that task's `<id>.md` by name.

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
    /// The branch the task's group lands on. `queue add` refuses a task
    /// whose `base:` differs from its dependency's, archived or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Whether the task was a trial arm. `queue add` leaves arms out of its
    /// one-chain check, archived ones included.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trial: bool,
    /// The task's issue-tracker slug, which `queue add` strips off its group
    /// name when it compares groups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// The issue URL the task carries as `url:`, which the board points a
    /// group band's hyperlink at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// When the task's file was created, in nanoseconds since the epoch: its
    /// birth time, or its modification time where the filesystem keeps no
    /// birth time. The queue tab orders groups by it, so it needs more
    /// precision than `archived_at` and a different meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_ns: Option<u64>,
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
            base: task.front.base.clone(),
            trial: task.front.trial.is_some(),
            slug: Some(task.extra_str("slug"))
                .filter(|slug| !slug.is_empty())
                .map(str::to_string),
            url: Some(task.extra_str("url"))
                .filter(|url| !url.is_empty())
                .map(str::to_string),
            created_ns: created_ns(&task.path),
            archived_at,
        }
    }

    /// A [`Task`] carrying only what the index holds, for a reader that
    /// wants archived tasks beside live ones without opening their files.
    ///
    /// `stage` is `done` because teardown archives a task only once it has
    /// reached it. The body is empty and the path is where the file lives, so
    /// a reader that needs more than the index holds opens that one file.
    pub fn to_task(&self, archive: &Path) -> Task {
        // Parsed once: only the id and the path differ between entries, and
        // a parse per entry would cost what the index exists to save.
        static BARE: std::sync::OnceLock<Task> = std::sync::OnceLock::new();
        let mut task = BARE
            .get_or_init(|| {
                Task::parse(
                    PathBuf::new(),
                    &format!("---\nid: bare\nstage: {}\n---\n", crate::pipeline::DONE),
                )
                .expect("a bare id and stage always parse")
            })
            .clone();
        task.path = archive.join(format!("{}.md", self.id));
        task.front.id = self.id.clone();
        task.front.title = self.title.clone();
        task.front.group = Some(self.group.clone()).filter(|group| !group.is_empty());
        task.front.pipeline = Some(self.pipeline.clone()).filter(|name| !name.is_empty());
        task.front.branch = Some(self.branch.clone()).filter(|branch| !branch.is_empty());
        task.front.depends_on = self.depends_on.clone();
        task.front.worktree_path = self.worktree_path.clone();
        task.front.base = self.base.clone();
        if self.trial {
            task.front.trial = Some(String::new());
        }
        for (key, value) in [("slug", &self.slug), ("url", &self.url)] {
            if let Some(value) = value {
                task.front
                    .extra
                    .insert(key.into(), serde_norway::Value::String(value.clone()));
            }
        }
        task
    }
}

/// When the file at `path` was created: its birth time, falling back to its
/// modification time where the platform or filesystem has no birth time to
/// give. The one definition, shared with the queue tab, which orders groups
/// by it whether the time comes from here or from the index.
pub fn created_at(path: &Path) -> Option<SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    meta.created().or_else(|_| meta.modified()).ok()
}

/// [`created_at`] in nanoseconds since the epoch — see [`Entry::created_ns`].
fn created_ns(path: &Path) -> Option<u64> {
    let at = created_at(path)?;
    Some(at.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_nanos() as u64)
}

#[cfg(test)]
thread_local! {
    static REBUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// What the reader tests share: archiving a task the way teardown does,
/// counting the times a task file was opened, and making the files unreadable
/// so a reader that opened one would be caught.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;

    /// How many times this thread has parsed the task files to rebuild.
    pub(crate) fn rebuilds() -> usize {
        REBUILDS.with(std::cell::Cell::get)
    }

    pub(crate) fn reset_rebuilds() {
        REBUILDS.with(|c| c.set(0));
    }

    /// Write `archive/<id>.md` with `front` as extra front matter lines, and
    /// return it parsed. The folder is left looking changed, as any new file
    /// leaves it, but nothing is recorded in the index.
    pub(crate) fn drop_in(repo: &Repo, id: &str, front: &str) -> Task {
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        let path = repo.archive_dir().join(format!("{id}.md"));
        std::fs::write(
            &path,
            format!("---\nid: {id}\ntitle: t {id}\nstage: done\n{front}---\nbody\n"),
        )
        .unwrap();
        Task::load(&path).unwrap()
    }

    /// Archive `id` as teardown does: bring the index up to date, add the
    /// file, record it. The folder's time is pushed ahead first so a coarse
    /// clock cannot make a reader's cache think nothing changed.
    pub(crate) fn archive(repo: &Repo, id: &str, front: &str) {
        let guard = lock(repo).unwrap();
        ensure_current(repo, &guard);
        let was_current = is_current(repo);
        let task = drop_in(repo, id, front);
        // Strictly later every call, so no two archivings share a folder time.
        static BUMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(5);
        let ahead = BUMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::scratch::set_mtime(
            &repo.archive_dir(),
            SystemTime::now() + std::time::Duration::from_secs(ahead),
        );
        record(repo, &guard, was_current, &task, 1_790_000_000).unwrap();
    }

    /// Run `read` with every task file replaced by junk, keeping the
    /// folder's time, so only the index can still say what was archived; the
    /// files are put back afterwards.
    pub(crate) fn with_unreadable_files<T>(repo: &Repo, read: impl FnOnce() -> T) -> T {
        let dir = repo.archive_dir();
        let folder = modified(&dir).unwrap();
        let saved: Vec<(PathBuf, String)> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
            .map(|path| {
                let text = std::fs::read_to_string(&path).unwrap();
                std::fs::write(&path, "not a task").unwrap();
                (path, text)
            })
            .collect();
        crate::scratch::set_mtime(&dir, folder);
        let out = read();
        for (path, text) in saved {
            std::fs::write(path, text).unwrap();
        }
        crate::scratch::set_mtime(&dir, folder);
        out
    }

    /// Sweep `id` out of the archive the way retention does.
    pub(crate) fn sweep(repo: &Repo, id: &str) {
        let guard = lock(repo).unwrap();
        let was_current = is_current(repo);
        std::fs::remove_file(repo.archive_dir().join(format!("{id}.md"))).unwrap();
        forget_missing(repo, &guard, was_current);
    }

    /// Delete the index, so the next read rebuilds it from the files.
    pub(crate) fn lose_index(repo: &Repo) {
        std::fs::remove_file(index_path(repo)).unwrap();
    }
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

/// The lock file for the archive folder `archive`: beside it, in the home,
/// and never inside it, because a file created in `archive/` would move the
/// folder's modification time that the index is compared against.
pub fn lock_file_for(archive: &Path) -> PathBuf {
    archive
        .parent()
        .unwrap_or(archive)
        .join("archive-index.lock")
}

fn index_path(repo: &Repo) -> PathBuf {
    index_at(&repo.archive_dir())
}

fn index_at(archive: &Path) -> PathBuf {
    archive.join(FILE_NAME)
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Whether the index exists and was last made to match the folder as it is
/// now. Says nothing about whether every line parses; [`read`] checks that.
pub fn is_current(repo: &Repo) -> bool {
    is_current_at(&repo.archive_dir())
}

fn is_current_at(archive: &Path) -> bool {
    match (modified(&index_at(archive)), modified(archive)) {
        (Some(index), Some(folder)) => index == folder,
        _ => false,
    }
}

/// Make the index's modification time equal the folder's, which is what
/// [`is_current`] compares. Best effort: a failure leaves the index looking
/// stale, and the next read rebuilds it.
fn stamp(archive: &Path) {
    let Some(folder) = modified(archive) else {
        return;
    };
    if let Ok(file) = std::fs::File::options().write(true).open(index_at(archive)) {
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
/// [`read_at`] without the error: a folder that cannot be listed reads as
/// holding nothing.
pub fn read(repo: &Repo) -> Vec<Entry> {
    read_at(&repo.archive_dir()).unwrap_or_default()
}

/// [`read`] over the archive folder `archive`, for a caller that holds a
/// folder rather than a [`Repo`]. An error only when the folder exists but
/// cannot be listed, which a screen keeps showing what it had through rather
/// than drawing an archive that looks empty.
///
/// A current index is read without the lock. A rebuild takes it and checks
/// again, because another process may have repaired the index while this one
/// waited. If the lock cannot be had the entries are still returned, read
/// from the files, and the index is left alone.
pub fn read_at(archive: &Path) -> Result<Vec<Entry>> {
    if let Some(entries) = read_current(archive) {
        return Ok(entries);
    }
    // Only a rebuild lists the folder, so only it can find the folder
    // unlistable; a missing one is an empty archive, as it always was.
    if let Err(err) = std::fs::read_dir(archive)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        return Err(err).with_context(|| format!("reading {}", archive.display()));
    }
    let lock = ArchiveIndexLock::acquire(&lock_file_for(archive), ArchiveIndexLock::READ_WAIT)
        .ok()
        .map(|lock| Guard { _lock: lock });
    Ok(match lock {
        Some(guard) => read_locked(archive, &guard, false),
        None => load_entries(archive),
    })
}

fn read_current(archive: &Path) -> Option<Vec<Entry>> {
    if !is_current_at(archive) {
        return None;
    }
    parse_lines(&std::fs::read_to_string(index_at(archive)).ok()?)
}

/// The current index, or the entries rebuilt from the files. A rebuild of an
/// empty archive is written only if `write_empty`: a reader that writes a
/// file into a project that archived nothing leaves a mark, which
/// `queue add --dry-run` promises not to. A writer that is about to add to
/// the index needs the file to exist, so it passes `true`.
fn read_locked(archive: &Path, guard: &Guard, write_empty: bool) -> Vec<Entry> {
    read_current(archive).unwrap_or_else(|| rebuild(archive, guard, write_empty))
}

/// One archived file as a [`Task`], or `None` if it will not parse.
///
/// A file with no `stage:` is read as `done`, which is where every archived
/// task is. The queue tab always listed such a file, because it reads only a
/// task's `group:`, `id:` and the like; refusing it here would drop it from
/// that screen.
fn load_lenient(path: &Path) -> Option<Task> {
    let raw = std::fs::read_to_string(path).ok()?;
    Task::parse(path.to_path_buf(), &raw).ok().or_else(|| {
        let rest = raw.strip_prefix("---\n")?;
        Task::parse(
            path.to_path_buf(),
            &format!("---\nstage: {}\n{rest}", crate::pipeline::DONE),
        )
        .ok()
    })
}

/// Parse the `<id>.md` files into entries.
///
/// A task's archived time is its file's modification time: teardown renames
/// the file in without touching it, so that is the moment it was last
/// written before archiving. A file that will not parse is left out.
fn load_entries(archive: &Path) -> Vec<Entry> {
    // The only place the task files are opened, so a test counts these to
    // prove a current index is read without them.
    #[cfg(test)]
    REBUILDS.with(|c| c.set(c.get() + 1));
    let mut tasks: Vec<Task> = std::fs::read_dir(archive)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter_map(|path| load_lenient(&path))
        .collect();
    tasks.sort_by(|a, b| a.front.id.cmp(&b.front.id));
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
fn rebuild(archive: &Path, guard: &Guard, write_empty: bool) -> Vec<Entry> {
    let entries = load_entries(archive);
    if write_empty || !entries.is_empty() {
        let _ = write_all(archive, guard, &entries);
    }
    entries
}

fn write_all(archive: &Path, _guard: &Guard, entries: &[Entry]) -> Result<()> {
    let mut out = String::new();
    for entry in entries {
        out.push_str(&serde_json::to_string(entry)?);
        out.push('\n');
    }
    task::write_atomic(&index_at(archive), out)?;
    stamp(archive);
    Ok(())
}

/// Bring the index up to date before a task is renamed into `archive/`, so
/// that [`record`] has a current index to append to. The guard must be held
/// from here until the append, so no other process's change lands between.
pub fn ensure_current(repo: &Repo, guard: &Guard) {
    let _ = read_locked(&repo.archive_dir(), guard, true);
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
    // `task` still names the queue file the rename moved, so the time comes
    // from where the file is now.
    let mut entry = Entry::from_task(task, archived_at);
    entry.created_ns = created_ns(&repo.archive_dir().join(format!("{}.md", entry.id)));
    let mut line = serde_json::to_string(&entry)?;
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
    stamp(&repo.archive_dir());
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
    let _ = write_all(&dir, guard, &kept);
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
        stamp(&repo.archive_dir());
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
