//! Repeatable tasks kept in `.spoolway/routines/`, nested however a
//! project likes and tracked in git — the counterpart to [`super::pending`]'s
//! read of the single flat pending directory.
//!
//! Read only, and just as shallow as `pending.rs`'s own read: the routines
//! tab only needs enough to draw one row per routine and the
//! tasks under it, and `super::queue::validate_batch` is still what
//! actually validates a task once a person queues one, exactly as it is
//! for a pending group.

use std::path::{Path, PathBuf};

use super::*;

/// One task under a routines folder, read the same shallow way
/// [`super::pending::PendingTask`] is.
pub(crate) struct RoutineTask {
    /// The file it was read from, under `.spoolway/routines/` — never
    /// written to or removed by anything in this module; a routine's
    /// tasks move only when a person edits them by hand.
    pub(crate) path: PathBuf,
    /// The task's own `id:`, before any minting a submission gives it.
    pub(crate) id: String,
    /// The task's own `title:`, the same optional one-line description
    /// [`super::pending::PendingTask::description`] is.
    pub(crate) description: Option<String>,
    /// The task itself, unread and unmodified.
    pub(crate) doc: String,
}

/// One folder under `.spoolway/routines/`, and everything nested under it.
///
/// A routine is a top-level folder. A subfolder inside one has no row of its
/// own anywhere: it is read only so its tasks fold into [`Self::tasks`].
pub(crate) struct RoutineFolder {
    /// This folder's own base name — what the left pane draws and what a
    /// selection is keyed by, alongside its own `path`.
    pub(crate) name: String,
    /// The folder's own absolute path, and the identity a selection is made
    /// under.
    pub(crate) path: PathBuf,
    /// Every `*.md` at or below this folder — its own tasks first, then
    /// each subfolder's, recursively depth-first. What the left pane's own
    /// "N tasks" tail counts, what the right pane lists for the highlighted
    /// folder, and what `enter` queues whole.
    pub(crate) tasks: Vec<RoutineTask>,
    /// Every `*.md` at or below this folder that [`read_task`] could not
    /// read, in the same order as [`Self::tasks`]. Queueing the folder is
    /// refused while this is not empty — see [`Self::refusal`] — so a batch
    /// never goes in short of a task the person believes is part of it.
    pub(crate) unparseable: Vec<PathBuf>,
}

impl RoutineFolder {
    /// Why this folder cannot be queued, naming each file that will not
    /// parse — or `None` when every `*.md` in it does.
    pub(crate) fn refusal(&self) -> Option<String> {
        if self.unparseable.is_empty() {
            return None;
        }
        // One `task contract` line per file, not one command over all of
        // them: `task contract --from <folder>` reads only the folder's own
        // `*.md` files, so it reported "no problems" for a file nested below
        // it, and given several `--from` files it stops at the first that
        // fails and names only that one's reason.
        let files: Vec<String> = self
            .unparseable
            .iter()
            .map(|path| {
                format!(
                    "  {}\n    spoolway task contract --from \"{}\"",
                    path.display(),
                    path.display()
                )
            })
            .collect();
        let (them, it) = match self.unparseable.len() {
            1 => ("a task file".to_string(), "it"),
            n => (format!("{n} task files"), "them"),
        };
        Some(format!(
            "{} holds {them} that will not parse, so queueing it would leave {it} out:\n\n\
             {}\n\n\
             Run the command under each for its reason, fix or remove {it}, and queue the \
             folder again.",
            self.path.display(),
            files.join("\n"),
        ))
    }
}

/// Every top-level folder under [`Repo::routines_dir`], recursively read.
///
/// A missing directory is not an error — see that method's own doc comment
/// on why nothing creates it — it is simply nothing to list, the same way an
/// empty pending directory lists no groups.
pub(crate) fn list_routines(repo: &Repo) -> Result<Vec<RoutineFolder>> {
    let root = repo.routines_dir();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Ok(Vec::new());
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    paths.sort();

    paths.iter().map(|path| read_folder(path)).collect()
}

/// One folder at a known path, read the same recursive way [`list_routines`]
/// reads a top-level one. For a caller that already has the folder it wants —
/// a job pointing at `routines/nightly/` — rather than the whole tree. The
/// path must be a directory under [`Repo::routines_dir`]; a missing one is an
/// error here, since a job named it explicitly.
pub(crate) fn read_folder_at(path: &Path) -> Result<RoutineFolder> {
    if !path.is_dir() {
        anyhow::bail!("{} is not a folder", path.display());
    }
    read_folder(path)
}

/// One task at a known path, read the same tolerant way [`read_task`]
/// reads one found by walking a folder — but an error rather than `None`
/// when it will not parse, since a job named this file directly and a silent
/// skip would look like the job fired nothing.
pub(crate) fn read_task_at(path: &Path) -> Result<RoutineTask> {
    read_task(path).with_context(|| {
        format!(
            "{} is not a readable task (needs a `---` fence and an `id:`)",
            path.display()
        )
    })
}

/// One folder, read recursively: its own tasks, then every task any of its
/// subfolders hold, folded in after.
fn read_folder(path: &Path) -> Result<RoutineFolder> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
        .with_context(|| format!("reading {}", path.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    entries.sort();

    let mut folders = Vec::new();
    let mut tasks = Vec::new();
    let mut unparseable = Vec::new();
    for entry in entries {
        if entry.is_dir() {
            folders.push(read_folder(&entry)?);
        } else if entry.extension().and_then(|e| e.to_str()) == Some("md") {
            match read_task(&entry) {
                Some(task) => tasks.push(task),
                None => unparseable.push(entry),
            }
        }
    }
    // A subfolder's own tasks are already gathered into its `tasks` by
    // this same recursion, so folding them in here — after this folder's
    // own — is what makes a parent's count and listing cover everything at
    // or below it, not just what sits directly inside.
    for sub in &folders {
        tasks.extend(sub.tasks.iter().map(clone_task));
        unparseable.extend(sub.unparseable.iter().cloned());
    }

    Ok(RoutineFolder {
        name,
        path: path.to_path_buf(),
        tasks,
        unparseable,
    })
}

/// [`RoutineTask`] carries no `Clone` of its own — nothing else needs one,
/// and deriving it would suggest a task is ever duplicated for any
/// reason but folding a subfolder's list into its parent's, right here.
fn clone_task(task: &RoutineTask) -> RoutineTask {
    RoutineTask {
        path: task.path.clone(),
        id: task.id.clone(),
        description: task.description.clone(),
        doc: task.doc.clone(),
    }
}

/// One task, read the same tolerant way [`super::pending::list_groups`]
/// reads a pending one: no fence, no readable YAML or no `id:` at all is
/// `None` rather than an error, so one bad file does not take a whole
/// folder's listing down with it. [`read_folder`] keeps the path of every
/// file that comes back `None`, and [`RoutineFolder::refusal`] is what
/// refuses queueing the folder over them.
fn read_task(path: &Path) -> Option<RoutineTask> {
    let doc = std::fs::read_to_string(path).ok()?;
    let (yaml, _) = crate::task::split_fence(&doc).ok()?;
    let front: serde_norway::Value = serde_norway::from_str(yaml).ok()?;
    let id = super::pending::front_str(&front, "id")?;
    Some(RoutineTask {
        path: path.to_path_buf(),
        description: super::pending::front_str(&front, "title"),
        id,
        doc,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, id: &str, extra: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(name),
            format!(
                "---\nid: {id}\ntitle: {id}, done\ngroup: nightly\n{extra}---\n\
                 ## Goal\n\nDo the thing.\n"
            ),
        )
        .unwrap();
    }

    /// No `.spoolway/routines/` on disk at all lists nothing, and is not an
    /// error — the same tolerance an empty pending directory gets.
    #[test]
    fn a_project_with_no_routines_directory_lists_none() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("routines-none");
        assert!(list_routines(&repo).unwrap().is_empty());
    }

    /// A flat folder of tasks: every one is this folder's own task, none
    /// of it a subfolder.
    #[test]
    fn a_flat_folder_lists_its_own_tasks() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("routines-flat");
        let dir = repo.routines_dir().join("nightly");
        write(&dir, "audit-deps.md", "audit-deps", "");
        write(&dir, "audit-docs.md", "audit-docs", "");

        let routines = list_routines(&repo).unwrap();
        assert_eq!(routines.len(), 1);
        assert_eq!(routines[0].name, "nightly");
        let ids: Vec<&str> = routines[0].tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["audit-deps", "audit-docs"]);
    }

    /// A file that will not parse is no task of the folder's, but it is not
    /// dropped without a word either: the folder keeps its path, a nested
    /// one's included, and refuses to be queued over it.
    #[test]
    fn a_file_that_will_not_parse_is_named_by_its_folder_and_refuses_it() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("routines-unparseable");
        let top = repo.routines_dir().join("nightly");
        write(&top, "good.md", "good", "");
        std::fs::write(top.join("bad.md"), "title: no fence at all\n").unwrap();
        let sub = top.join("weekly");
        write(&sub, "fine.md", "fine", "");
        std::fs::write(sub.join("worse.md"), "---\nid: [unclosed\n---\nx\n").unwrap();

        let routines = list_routines(&repo).unwrap();
        let names: Vec<String> = routines[0]
            .unparseable
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["bad.md", "worse.md"], "{names:?}");
        let ids: Vec<&str> = routines[0].tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["good", "fine"]);

        let refusal = routines[0]
            .refusal()
            .expect("a folder with bad files is refused");
        assert!(refusal.contains("bad.md"), "{refusal}");
        assert!(refusal.contains("worse.md"), "{refusal}");
        // The remedy has to reach the nested file: `task contract --from` on
        // the top folder does not look into `weekly/`.
        // One command per file, since `task contract` given several stops at
        // the first failure and explains only that one.
        for file in [top.join("bad.md"), sub.join("worse.md")] {
            let line = format!("spoolway task contract --from \"{}\"", file.display());
            assert_eq!(refusal.matches(&line).count(), 1, "{refusal}");
        }
        assert_eq!(
            refusal.matches("spoolway task contract").count(),
            2,
            "{refusal}"
        );
        let top_only = format!("--from {}", top.display());
        assert!(!refusal.contains(&top_only), "{refusal}");
    }

    /// A folder whose every file parses has nothing to refuse.
    #[test]
    fn a_folder_that_reads_cleanly_has_no_refusal() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("routines-clean");
        write(&repo.routines_dir().join("nightly"), "a.md", "a", "");

        assert!(list_routines(&repo).unwrap()[0].refusal().is_none());
    }

    /// A folder's own "at or below it" count and listing reach into every
    /// subfolder, recursively — not just what sits directly inside it.
    #[test]
    fn a_nested_folder_counts_every_task_below_it() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("routines-nested");
        let top = repo.routines_dir().join("maintenance");
        write(&top, "sweep.md", "sweep", "");
        let sub = top.join("weekly");
        write(&sub, "rotate.md", "rotate", "");
        write(&sub, "prune.md", "prune", "");

        let routines = list_routines(&repo).unwrap();
        assert_eq!(routines.len(), 1, "`weekly` is no routine of its own");
        let ids: Vec<&str> = routines[0].tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["sweep", "prune", "rotate"],
            "its own first, then the nested ones"
        );
    }

    /// A task with no readable `id:` is skipped, the same tolerance
    /// [`super::pending::list_groups`] gives a task with no `group:`.
    #[test]
    fn a_task_with_no_readable_id_is_skipped() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("routines-unreadable");
        let dir = repo.routines_dir().join("release");
        write(&dir, "good.md", "good", "");
        std::fs::write(dir.join("garbage.md"), "not a task\n").unwrap();

        let routines = list_routines(&repo).unwrap();
        assert_eq!(routines[0].tasks.len(), 1);
        assert_eq!(routines[0].tasks[0].id, "good");
    }
}
