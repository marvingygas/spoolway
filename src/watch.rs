//! Cheap answers to "has anything this reading looks at moved?", so an idle
//! screen tick can skip its work instead of repeating it.
//!
//! The queue tab reads every pending and queue file and asks git for the
//! branch once a second. Two checks keep that from costing anything while
//! nothing changes: a [`Stamp`] of the directories and files a reading
//! covers, and a [`BranchWatch`] that asks git only after `HEAD` moved.
//! Both compare inode, length and modified time together, the one
//! [`Identity`] `usage` also keeps for transcripts, and both refuse to trust a file written
//! within the last [`SETTLE`]: two writes inside one modified-time tick with
//! the same length look identical, so a file that young is read again.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

/// How old a modified time must be before an unchanged identity proves an
/// unchanged file. Filesystems that stamp whole seconds cannot tell two
/// writes inside one second apart, so anything younger is read again.
const SETTLE: Duration = Duration::from_secs(1);

/// What a file or directory looked like when it was last stat-ed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) inode: u64,
    pub(crate) len: u64,
    pub(crate) modified: Option<SystemTime>,
}

impl Identity {
    /// `None` when the path is not there.
    pub(crate) fn of(path: &Path) -> Option<Self> {
        std::fs::metadata(path)
            .ok()
            .map(|meta| Self::from_metadata(&meta))
    }

    pub(crate) fn from_metadata(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(meta);
        #[cfg(not(unix))]
        let inode = 0;
        Self {
            inode,
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }

    /// Whether the file was written within `window` of now. A file with no
    /// readable modified time is not.
    pub(crate) fn written_within(&self, window: Duration) -> bool {
        self.modified.is_some_and(|at| {
            SystemTime::now()
                .duration_since(at)
                .is_ok_and(|age| age <= window)
        })
    }

    /// Whether the modified time is old enough, at this moment, that a
    /// second write could no longer hide inside the same tick. A missing or
    /// future modified time is not settled.
    fn settled(&self) -> bool {
        self.settled_at(SystemTime::now())
    }

    /// [`Identity::settled`] as of `now`, so a caller that holds a clock of
    /// its own can judge against it.
    fn settled_at(&self, now: SystemTime) -> bool {
        self.modified
            .is_some_and(|at| now.duration_since(at).is_ok_and(|age| age >= SETTLE))
    }
}

/// The identities of everything one reading covers, taken before the
/// reading so a write that lands during it shows up as a change next tick.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    entries: Vec<(PathBuf, Option<Identity>)>,
    /// Whether every identity was settled when it was taken. Only a settled
    /// stamp can vouch for the files staying as they are.
    settled: bool,
}

impl Stamp {
    /// Stat each of `deep` and everything directly inside it, and each of
    /// `shallow` alone. A directory's own identity moves when a file is
    /// added or removed, and a file's moves when it is rewritten, so
    /// `shallow` suits a directory whose files are read through an index of
    /// their own, and a single file such as `HEAD`.
    pub(crate) fn capture(deep: &[PathBuf], shallow: &[PathBuf]) -> Self {
        let mut paths: Vec<PathBuf> = Vec::new();
        for dir in deep {
            paths.push(dir.clone());
            if let Ok(children) = std::fs::read_dir(dir) {
                paths.extend(children.filter_map(|child| child.ok()).map(|c| c.path()));
            }
        }
        paths.extend(shallow.iter().cloned());
        paths.sort();
        let entries: Vec<(PathBuf, Option<Identity>)> = paths
            .into_iter()
            .map(|path| {
                let identity = Identity::of(&path);
                (path, identity)
            })
            .collect();
        let settled = entries
            .iter()
            .all(|(_, identity)| identity.as_ref().is_none_or(Identity::settled));
        Self { entries, settled }
    }

    /// Whether a reading taken at `before` already saw everything this
    /// stamp sees. Never true after an unsettled stamp: it may have been
    /// taken between two writes that look alike.
    pub(crate) fn unchanged_since(&self, before: &Stamp) -> bool {
        before.settled && self.entries == before.entries
    }
}

/// The branch checked out in a directory, asked of git only after `HEAD`
/// changed.
///
/// A checkout, `git switch -c` and a rebase all rewrite `HEAD`
/// through a lock file and a rename, so its inode, length or modified time
/// moves. Between such moves the answer cannot differ, and starting `git
/// branch --show-current` once a second to confirm it was most of an idle
/// tick's cost.
#[derive(Default)]
pub(crate) struct BranchWatch {
    /// Where `HEAD` lives. In a linked worktree `.git` is a file naming the
    /// real git directory, so this is resolved by asking [`crate::repo`]
    /// once, rather than assumed to sit under the checkout.
    head: Option<PathBuf>,
    resolved: bool,
    seen: Option<Seen>,
    /// A clock for a test to move while `run` is in flight, which a file
    /// write cannot do without also changing the identity under test.
    #[cfg(test)]
    clock: Option<fn() -> SystemTime>,
}

struct Seen {
    identity: Identity,
    settled: bool,
    branch: String,
}

impl BranchWatch {
    /// A watch on an explicit `HEAD` path, for a test that supplies its own.
    #[cfg(test)]
    fn with_head(head: PathBuf) -> Self {
        Self {
            head: Some(head),
            resolved: true,
            seen: None,
            clock: None,
        }
    }

    fn now(&self) -> SystemTime {
        #[cfg(test)]
        if let Some(clock) = self.clock {
            return clock();
        }
        SystemTime::now()
    }

    /// Where `dir`'s `HEAD` file is, or `None` when it cannot be found, in
    /// which case [`BranchWatch::read`] asks git every time.
    pub(crate) fn head_path(&mut self, dir: &Path) -> Option<PathBuf> {
        if !self.resolved {
            self.resolved = true;
            self.head = crate::repo::worktree_git_dir(dir)
                .ok()
                .map(|git| git.join("HEAD"));
        }
        self.head.clone()
    }

    /// The branch `run` reports, calling it only when `HEAD` is not the
    /// settled file the last answer came from. A failed `run`, which is how
    /// a detached head reads, is not remembered: it is asked again next
    /// time rather than pinning a transient failure until `HEAD` moves.
    pub(crate) fn read(
        &mut self,
        dir: &Path,
        mut run: impl FnMut() -> Result<String>,
    ) -> Option<String> {
        let identity = self.head_path(dir).and_then(|head| Identity::of(&head));
        // Judged now, before `run`: how long git takes must not age a
        // modified time that was still inside the settle window when taken.
        let now = self.now();
        let settled = identity.as_ref().is_some_and(|id| id.settled_at(now));
        if let (Some(now), Some(seen)) = (&identity, &self.seen)
            && seen.settled
            && seen.identity == *now
        {
            return Some(seen.branch.clone());
        }
        let branch = run().ok();
        self.seen = match (identity, &branch) {
            (Some(identity), Some(branch)) => Some(Seen {
                settled,
                identity,
                branch: branch.clone(),
            }),
            _ => None,
        };
        branch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::set_mtime;
    use std::cell::Cell;

    /// A directory that removes itself when the test ends.
    fn scratch(name: &str) -> crate::scratch::ScratchRoot {
        let dir = crate::scratch::root(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Back-date `path` so its identity counts as settled.
    fn age(path: &Path) {
        set_mtime(path, SystemTime::now() - Duration::from_secs(60));
    }

    /// Date `path` in the future, which is never settled however long the
    /// test takes to run.
    fn unsettle(path: &Path) {
        set_mtime(path, SystemTime::now() + Duration::from_secs(3600));
    }

    /// `git` runs on the first read and again only once `HEAD` is replaced.
    /// Before the cache existed it ran on every read.
    #[test]
    fn branch_watch_asks_git_only_when_head_changes() {
        let dir = scratch("watch-branch");
        let head = dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/one\n").unwrap();
        age(&head);
        let mut watch = BranchWatch::with_head(head.clone());
        let runs = Cell::new(0);
        let read = |watch: &mut BranchWatch, answer: &'static str| {
            watch.read(&dir, || {
                runs.set(runs.get() + 1);
                Ok(answer.to_string())
            })
        };

        assert_eq!(read(&mut watch, "one").as_deref(), Some("one"));
        assert_eq!(read(&mut watch, "wrong").as_deref(), Some("one"));
        assert_eq!(read(&mut watch, "wrong").as_deref(), Some("one"));
        assert_eq!(runs.get(), 1, "an unchanged HEAD must not start git");

        std::fs::write(&head, "ref: refs/heads/another\n").unwrap();
        age(&head);
        assert_eq!(read(&mut watch, "another").as_deref(), Some("another"));
        assert_eq!(runs.get(), 2, "a changed HEAD must start git once");
    }

    /// A `HEAD` written within the last second is read again, since a second
    /// write of the same length could land in the same modified-time tick.
    #[test]
    fn branch_watch_does_not_trust_a_head_written_this_second() {
        let dir = scratch("watch-young");
        let head = dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/one\n").unwrap();
        unsettle(&head);
        let mut watch = BranchWatch::with_head(head);
        let runs = Cell::new(0);
        for _ in 0..3 {
            watch.read(&dir, || {
                runs.set(runs.get() + 1);
                Ok("one".to_string())
            });
        }
        assert_eq!(runs.get(), 3);
    }

    /// The clock [`branch_watch_judges_head_before_git_runs`] moves forward
    /// while git is running.
    static SKEW_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn skewed_now() -> SystemTime {
        SystemTime::now() + Duration::from_secs(SKEW_SECS.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Whether `HEAD` was settled is judged when its identity was taken, not
    /// after git has finished: time passing during a slow `run` must not make
    /// a young `HEAD` look old. The clock jumps past the settle window inside
    /// `run` while `HEAD` itself stays untouched, so the second read sees the
    /// same identity and only the settled flag decides whether git runs
    /// again. Judged after `run`, it was recorded as settled and git did not.
    #[test]
    fn branch_watch_judges_head_before_git_runs() {
        let dir = scratch("watch-slow-git");
        let head = dir.join("HEAD");
        std::fs::write(&head, "ref: refs/heads/one\n").unwrap();
        let mut watch = BranchWatch::with_head(head);
        watch.clock = Some(skewed_now);
        watch.read(&dir, || {
            SKEW_SECS.store(10, std::sync::atomic::Ordering::SeqCst);
            Ok("one".to_string())
        });
        let runs = Cell::new(0);
        watch.read(&dir, || {
            runs.set(runs.get() + 1);
            Ok("one".to_string())
        });
        SKEW_SECS.store(0, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(runs.get(), 1, "the young HEAD must be asked about again");
    }

    /// A failed `run` — a detached head — is asked again, not remembered.
    #[test]
    fn branch_watch_does_not_remember_a_failed_answer() {
        let dir = scratch("watch-detached");
        let head = dir.join("HEAD");
        std::fs::write(&head, "0123\n").unwrap();
        age(&head);
        let mut watch = BranchWatch::with_head(head);
        let runs = Cell::new(0);
        for _ in 0..2 {
            let branch = watch.read(&dir, || {
                runs.set(runs.get() + 1);
                anyhow::bail!("detached")
            });
            assert_eq!(branch, None);
        }
        assert_eq!(runs.get(), 2);
    }

    /// A stamp of settled files is unchanged until one is rewritten, added
    /// or removed, and a stamp of a young file never vouches for anything.
    #[test]
    fn stamp_notices_edits_additions_and_young_files() {
        let dir = scratch("watch-stamp");
        let file = dir.join("a.md");
        std::fs::write(&file, "one").unwrap();
        age(&file);
        age(&dir);
        let deep = [dir.to_path_buf()];
        let first = Stamp::capture(&deep, &[]);
        assert!(first.settled);
        assert!(Stamp::capture(&deep, &[]).unchanged_since(&first));

        std::fs::write(&file, "two!").unwrap();
        age(&file);
        age(&dir);
        let edited = Stamp::capture(&deep, &[]);
        assert!(!edited.unchanged_since(&first), "a longer file is a change");

        let added_file = dir.join("b.md");
        std::fs::write(&added_file, "x").unwrap();
        unsettle(&added_file);
        age(&dir);
        let added = Stamp::capture(&deep, &[]);
        assert!(!added.unchanged_since(&edited), "a new file is a change");
        assert!(
            !Stamp::capture(&deep, &[]).unchanged_since(&added),
            "a stamp holding a young file is never trusted"
        );
    }
}
