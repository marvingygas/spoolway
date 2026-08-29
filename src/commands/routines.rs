//! Repeatable task documents kept in `.spoolway/routines/`, nested however a
//! project likes and tracked in git — the counterpart to [`super::pending`]'s
//! read of the single flat pending directory.
//!
//! Read only, and just as shallow as `pending.rs`'s own read: the queue
//! screen's `r` pane only needs enough to draw a folder tree and the
//! documents under it, and `super::queue::validate_batch` is still what
//! actually validates a document once a person queues one, exactly as it is
//! for a pending group.

use std::path::{Path, PathBuf};

use super::*;

/// One task document under a routines folder, read the same shallow way
/// [`super::pending::PendingTask`] is.
pub(crate) struct RoutineTask {
    /// The file it was read from, under `.spoolway/routines/` — never
    /// written to or removed by anything in this module; a routine's
    /// documents move only when a person edits them by hand.
    pub(crate) path: PathBuf,
    /// The document's own `id:`, before any minting a submission gives it.
    pub(crate) id: String,
    /// The document's own `title:`, the same optional one-line description
    /// [`super::pending::PendingTask::description`] is.
    pub(crate) description: Option<String>,
    /// The document itself, unread and unmodified.
    pub(crate) doc: String,
}

/// One folder under `.spoolway/routines/`, and everything nested under it.
pub(crate) struct RoutineFolder {
    /// This folder's own base name — what the left pane draws and what a
    /// selection is keyed by, alongside its own `path`.
    pub(crate) name: String,
    /// The folder's own absolute path, and the identity a selection is made
    /// under: two folders of the same name in different parents are two
    /// different rows.
    pub(crate) path: PathBuf,
    /// This folder's own immediate subfolders, name order — what `→`
    /// descends into.
    pub(crate) folders: Vec<RoutineFolder>,
    /// Every `*.md` at or below this folder — its own documents first, then
    /// each subfolder's, recursively depth-first. What the left pane's own
    /// "N tasks" tail counts, what the right pane lists for the highlighted
    /// folder, and what `enter` queues whole.
    pub(crate) tasks: Vec<RoutineTask>,
    /// How many of `tasks` sit directly in this folder, rather than folded
    /// in from a subfolder — the first `own` entries of `tasks`, by
    /// construction. What decides `→`'s own choice for a folder holding
    /// both: a document of its own to focus, or nothing here but subfolders
    /// left to descend into. See `queue::handle_routine_key`.
    pub(crate) own: usize,
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

/// One folder, read recursively: its own subfolders and its own documents,
/// then every document any of those subfolders hold, folded in after.
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
    for entry in entries {
        if entry.is_dir() {
            folders.push(read_folder(&entry)?);
        } else if entry.extension().and_then(|e| e.to_str()) == Some("md")
            && let Some(task) = read_task(&entry)
        {
            tasks.push(task);
        }
    }
    // Taken before folding a subfolder's documents in below, so `own` counts
    // only what actually sits directly in this folder.
    let own = tasks.len();
    // A subfolder's own documents are already gathered into its `tasks` by
    // this same recursion, so folding them in here — after this folder's
    // own — is what makes a parent's count and listing cover everything at
    // or below it, not just what sits directly inside.
    for sub in &folders {
        tasks.extend(sub.tasks.iter().map(clone_task));
    }

    Ok(RoutineFolder {
        name,
        path: path.to_path_buf(),
        folders,
        tasks,
        own,
    })
}

/// [`RoutineTask`] carries no `Clone` of its own — nothing else needs one,
/// and deriving it would suggest a document is ever duplicated for any
/// reason but folding a subfolder's list into its parent's, right here.
fn clone_task(task: &RoutineTask) -> RoutineTask {
    RoutineTask {
        path: task.path.clone(),
        id: task.id.clone(),
        description: task.description.clone(),
        doc: task.doc.clone(),
    }
}

/// One document, read the same tolerant way [`super::pending::list_groups`]
/// reads a pending one: no fence, no readable YAML or no `id:` at all is
/// `None` rather than an error, so one bad file does not take a whole
/// folder's listing down with it. `super::queue::validate_batch` is what
/// refuses it for real, once a person actually queues it.
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
        let repo = crate::commands::testutil::fixture("routines-none");
        assert!(list_routines(&repo).unwrap().is_empty());
    }

    /// A flat folder of documents: every one is this folder's own task, none
    /// of it a subfolder.
    #[test]
    fn a_flat_folder_lists_its_own_documents() {
        let repo = crate::commands::testutil::fixture("routines-flat");
        let dir = repo.routines_dir().join("nightly");
        write(&dir, "audit-deps.md", "audit-deps", "");
        write(&dir, "audit-docs.md", "audit-docs", "");

        let routines = list_routines(&repo).unwrap();
        assert_eq!(routines.len(), 1);
        assert_eq!(routines[0].name, "nightly");
        assert!(routines[0].folders.is_empty());
        let ids: Vec<&str> = routines[0].tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["audit-deps", "audit-docs"]);
    }

    /// A folder's own "at or below it" count and listing reach into every
    /// subfolder, recursively — not just what sits directly inside it.
    #[test]
    fn a_nested_folder_counts_every_document_below_it() {
        let repo = crate::commands::testutil::fixture("routines-nested");
        let top = repo.routines_dir().join("maintenance");
        write(&top, "sweep.md", "sweep", "");
        let sub = top.join("weekly");
        write(&sub, "rotate.md", "rotate", "");
        write(&sub, "prune.md", "prune", "");

        let routines = list_routines(&repo).unwrap();
        assert_eq!(routines.len(), 1);
        assert_eq!(routines[0].tasks.len(), 3, "one of its own, two nested");
        assert_eq!(routines[0].own, 1, "only `sweep` sits directly in it");
        assert_eq!(routines[0].folders.len(), 1);
        assert_eq!(routines[0].folders[0].name, "weekly");
        assert_eq!(routines[0].folders[0].tasks.len(), 2);
        assert_eq!(routines[0].folders[0].own, 2, "both of `weekly`'s own");
    }

    /// A document with no readable `id:` is skipped, the same tolerance
    /// [`super::pending::list_groups`] gives a document with no `group:`.
    #[test]
    fn a_document_with_no_readable_id_is_skipped() {
        let repo = crate::commands::testutil::fixture("routines-unreadable");
        let dir = repo.routines_dir().join("release");
        write(&dir, "good.md", "good", "");
        std::fs::write(dir.join("garbage.md"), "not a document\n").unwrap();

        let routines = list_routines(&repo).unwrap();
        assert_eq!(routines[0].tasks.len(), 1);
        assert_eq!(routines[0].tasks[0].id, "good");
    }
}
