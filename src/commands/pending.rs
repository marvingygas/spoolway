//! What the queue screen reads: tasks waiting in one flat
//! directory, gathered into the groups they name.
//!
//! spoolway has stopped knowing what a plan is. A producer — the shipped
//! `/spoolway-plan` skill, a Jira ticket, a GitHub issue, a script — writes
//! whole tasks into [`Repo::pending_dir`], and that directory is
//! the main thing the screen scans. [`Repo::queue_dir`] and
//! [`Repo::archive_dir`] are the other two: submitting a group deletes its
//! pending tasks (`queue::finish_submit`), so a group already queued has
//! nothing left under `pending_dir` at all, and a task the pipeline ran to
//! the end moves out of the queue directory into the archive one
//! (`teardown`). Either way a row is built straight from whichever
//! directory still holds the task — see [`list_groups_and_skipped`]. Nothing
//! here parses a page, a card or a chip; a task is the unit, and its own
//! frontmatter is the whole of what this module reads.
//!
//! The reading is deliberately shallow. [`crate::commands::parse_submission`]
//! is what actually validates a task, once a person has chosen to queue
//! it; this only needs enough to draw two panes — which group a task
//! belongs to, what it is called, and what it says it is for. A task
//! that will be refused at submit time still lists here, and is refused
//! there, with the real error.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use super::*;

/// Where a task's own file was read from, and what stage it has reached
/// — one word per task, since [`Group::state`] can no longer speak for every
/// task it holds: a chain half archived and half still queued carries both
/// at once, and this is what the tasks pane draws beside each task's own id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskState {
    /// Still in [`Repo::pending_dir`], waiting to be queued at all.
    Pending,
    /// In [`Repo::queue_dir`] — submitted, not yet finished.
    Queued,
    /// In [`Repo::archive_dir`] — the pipeline ran it to the end.
    Done,
}

/// One pending task, as the screen shows it.
///
/// `Clone`: [`archive_tasks`] caches a whole `Vec` of these, keyed on the
/// archive directory's own mtime, and hands back a clone of it on every
/// call that finds the mtime unmoved.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PendingTask {
    /// The file it was read from — deleted when its group is queued, and the
    /// name a submission failure reports this task by, so the two agree
    /// by construction.
    pub(crate) path: PathBuf,
    /// The task's own `id:`, which is what the queue will call this task.
    pub(crate) id: String,
    /// The task's own `title:` — the one sentence the tasks pane draws
    /// under what this task waits on. `None` for a task with no title to
    /// read, which the pane simply draws no line for rather than an empty
    /// one; `parse_submission` is what refuses it at submit time.
    pub(crate) description: Option<String>,
    /// The task itself, unread and unmodified, ready to hand to
    /// `validate_batch` exactly as a `--from` entry would. Empty for an
    /// archived task, which is listed from the archive index without opening
    /// its file; read it through [`PendingTask::text`].
    pub(crate) doc: String,
    /// Where this task's own id currently sits — see [`TaskState`].
    pub(crate) state: TaskState,
    /// This task's own `depends_on:` list, read once while [`list_groups_and_skipped`]
    /// parses the task's front matter and kept here so [`in_reading_order`]
    /// can sort by it, and so [`super::queue::tasks_pane_lines`] can draw
    /// its `Depends on:` row, without parsing the same doc a second time.
    pub(crate) depends_on: Vec<String>,
    /// This task's own `pipeline:` key, read the same one time `depends_on`
    /// is — what [`super::queue::tasks_pane_lines`]'s `Pipeline:` row draws.
    /// `None` for a task naming no pipeline of its own, which that row
    /// reads as unassigned rather than refusing to draw.
    pub(crate) pipeline: Option<String>,
    /// This task's own `base:` key, read the same way — what
    /// [`super::queue::tasks_pane_lines`]'s `Lands in:` row draws.
    pub(crate) base: Option<String>,
    /// This task's own `starts_from:` key, read the same way — what
    /// [`super::queue::tasks_pane_lines`]'s `Starts from:` row draws when
    /// it is set. See [`front_starts_from`].
    pub(crate) starts_from: Option<String>,
}

impl PendingTask {
    /// The task's text: [`PendingTask::doc`], or for an archived task, whose
    /// `doc` is empty, the one file it names. That read fails if retention
    /// swept the file since the listing was made, and the caller says so
    /// rather than treating the task as empty.
    pub(crate) fn text(&self) -> std::io::Result<std::borrow::Cow<'_, str>> {
        if self.doc.is_empty() && self.state == TaskState::Done {
            return std::fs::read_to_string(&self.path).map(Into::into);
        }
        Ok(std::borrow::Cow::Borrowed(&self.doc))
    }
}

/// A group's own stage, folded from every task it holds — see
/// [`group_state`]. `Ord`ered in the order [`list_groups_and_skipped`] sorts by:
/// something still queueable first, then everything queued, then everything
/// done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum GroupState {
    /// At least one task's own id reads [`TaskState::Pending`] — the group
    /// is not every task queued yet, so it is the one `enter` may still
    /// submit.
    ///
    /// A pending task naming a task id the queue or the archive already
    /// holds — a re-run of a producer over work already submitted, which
    /// cannot be queued again since `validate_batch` would refuse it — is
    /// *not* this case: that task's own state reads `Queued` or `Done`, not
    /// `Pending`, so it does not keep the group `Queueable` on its own.
    Queueable,
    /// No task is still pending, but not every one has finished either — the
    /// ordinary shape a group takes the moment it is submitted, and the shape
    /// a chain keeps for as long as any of its tasks is still mid-run: half
    /// archived and half still queued folds to this, the same as a group
    /// wholly in the queue.
    Queued,
    /// Every task has reached [`Repo::archive_dir`]. `s` and `t` are the only
    /// doors out — see `queue::selectable`.
    Done,
}

/// One `group:` across the pending tasks: the left pane's unit.
///
/// A group is what a person selects and submits, whole. Its tasks are a
/// chain — cut together, ordered by `depends_on` — and half a chain in the
/// queue is a task waiting on a dependency nobody queued.
///
/// `Clone`: the queue tab's reader thread (`commands::queue`'s own
/// `QueueReader`) hands a freshly read `Vec<Group>` back across threads
/// inside an `Arc`, and the key thread clones it out into its own owned
/// list so the rest of the screen can keep mutating that list in place —
/// `finish_submit`'s `groups.retain` the one place that still does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Group {
    /// The `group:` value, verbatim. Never path-parsed and never split: two
    /// tasks group together only when they name exactly the same string,
    /// the same rule `spoolway group list` reads the queue by.
    pub(crate) name: String,
    /// Its tasks, dependencies before dependents — see [`in_reading_order`].
    pub(crate) tasks: Vec<PendingTask>,
    /// This group's own stage — see [`GroupState`] and [`group_state`], which
    /// is what [`list_groups_and_skipped`] computes this from once every task has been
    /// read.
    pub(crate) state: GroupState,
    /// The newest of its tasks' own birth times, which is what
    /// [`list_groups_and_skipped`] sorts on: the group somebody just wrote is the one
    /// under the cursor on the first frame. `None` only when no task's
    /// time could be read at all, which sorts the group to the end rather
    /// than refusing the whole listing over one stat failure.
    pub(crate) created: Option<std::time::SystemTime>,
}

/// A group's own [`GroupState`], folded from its tasks' individual
/// [`TaskState`]s: queueable if any task is still pending, done if every task
/// is, queued otherwise — which is also what a group with two states at
/// once (some archived, some still queued, none pending) folds to, since
/// there is no fourth bucket for it and it is not yet wholly finished.
///
/// An empty group cannot happen through [`list_groups_and_skipped`] — a group only
/// exists because a task named it — but reads `Queueable` rather than
/// vacuously `Done` if that ever changes, the same way the bare `bool` this
/// replaced started its own fold at `true` and was corrected the same way.
fn group_state(tasks: &[PendingTask]) -> GroupState {
    if tasks.is_empty() || tasks.iter().any(|t| t.state == TaskState::Pending) {
        GroupState::Queueable
    } else if tasks.iter().all(|t| t.state == TaskState::Done) {
        GroupState::Done
    } else {
        GroupState::Queued
    }
}

/// The order [`list_groups_and_skipped`] sorts by within a queued/unqueued
/// half: newer before older, and a group whose instant could not be read
/// after every one that could — a stat that failed on one file is not a reason to refuse the
/// listing, so that group is still shown, just at the bottom.
///
/// Pure, and taking the two instants rather than the two groups, so a test
/// can check every branch — both readable, either missing, both missing —
/// without touching a filesystem at all.
fn newest_first(
    a: Option<std::time::SystemTime>,
    b: Option<std::time::SystemTime>,
) -> std::cmp::Ordering {
    match (a, b) {
        (Some(a), Some(b)) => b.cmp(&a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// A task's own birth time, falling back to its modification time where
/// the platform or filesystem has no birth time to give.
fn created_time(path: &Path) -> Option<std::time::SystemTime> {
    crate::archive_index::created_at(path)
}

/// One string off a task's frontmatter, without any of
/// `parse_submission`'s validation.
///
/// The screen has to draw a task before anyone has decided to submit it,
/// so a malformed one must come back as "nothing to show" rather than an
/// error that would take the whole listing down with it. Every field below
/// is read this way for that reason. `pub(crate)` rather than private: a
/// routines folder is read the same shallow way — see
/// `super::routines::read_task`.
pub(crate) fn front_str(yaml: &serde_norway::Value, key: &str) -> Option<String> {
    let text = yaml.as_mapping()?.get(key)?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// A task's `starts_from:`, or the `cut_from:` a task file written before
/// the field was renamed holds instead — the same alias
/// [`crate::task::Frontmatter::starts_from`] reads, so a queued or archived
/// task stamped under the old name still shows where it started.
fn front_starts_from(yaml: &serde_norway::Value) -> Option<String> {
    front_str(yaml, "starts_from").or_else(|| front_str(yaml, "cut_from"))
}

/// Every `.md` file directly inside `dir`, filename order — the listing
/// [`list_groups_and_skipped`] runs the same way over all three of its own sources.
fn md_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();
    Ok(paths)
}

/// Every group across the pending directory, the queue directory and the
/// archive directory — queueable groups before queued ones before done ones,
/// and each third newest-written first — the order the left pane draws, so
/// the group somebody just wrote is already under the cursor and one that
/// can no longer be queued is never on top of one that can.
///
/// Two groups written in the same instant fall back to name order, so the
/// list does not reshuffle between two draws that land in the same second.
///
/// A task with no readable `group:` is skipped, not shown: there is no
/// row for it to be one of, since a row *is* a `group:` value. It is not
/// lost — [`list_groups_and_skipped`] hands it back for the queue tab to
/// name, and `queue add --from <pending dir>` still reads the whole
/// directory and refuses that task by name, with the real reason.
///
/// No "does not exist yet" case to handle: [`Repo::pending_dir`],
/// [`Repo::queue_dir`] and [`Repo::archive_dir`] all create their directory
/// silently the moment they are asked for, so a fresh project that has
/// planned nothing still gets a real, empty directory to read.
///
/// The groups come with the pending files that no group could be built from
/// — see [`list_groups_in`] for what counts as that. The queue tab reads both
/// from this one call, so naming a broken file costs no second parse.
///
/// A thin wrapper over [`list_groups_in`], which takes a [`ListFrontCache`]
/// and a parse counter as plain arguments instead of reaching into a
/// static for either. Production reaches into one static cache here, once,
/// so every real reload in the same process shares it; a test calls
/// `list_groups_in` directly, with a cache of its own, so it can both avoid
/// colliding with every other test's fixture in this binary and read back
/// how many files were actually parsed.
pub(crate) fn list_groups_and_skipped(repo: &Repo) -> Result<(Vec<Group>, Vec<PathBuf>)> {
    static CACHE: OnceLock<Mutex<ListFrontCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    let Ok(mut guard) = cache.lock() else {
        // A poisoned lock falls back to an unshared, uncached-across-calls
        // cache for this one call — one reload paying full price rather
        // than the whole screen failing to draw.
        let mut local = HashMap::new();
        let mut parsed = 0;
        return list_groups_in(repo, &mut local, &mut parsed);
    };
    let mut parsed = 0; // Nothing in production reads this; see `list_groups_in`.
    list_groups_in(repo, &mut guard, &mut parsed)
}

/// Every file [`list_groups_in`] has read the bytes of so far in this
/// process, with the thread that read it — test-only, the same shape as
/// [`crate::status::QUEUE_READS`] and [`crate::repo::PROCESS_RUNS`]: scoped
/// by path so a test counts only its own fixture, and by thread so it can
/// tell a key's own thread's reads from a reader thread's.
#[cfg(test)]
static PENDING_READS: Mutex<Vec<(PathBuf, std::thread::ThreadId)>> = Mutex::new(Vec::new());

/// How many files under `dir` [`list_groups_in`] has read so far on the
/// calling thread — see [`PENDING_READS`].
#[cfg(test)]
pub(crate) fn pending_reads_here_under(dir: &Path) -> usize {
    let here = std::thread::current().id();
    PENDING_READS
        .lock()
        .map(|reads| {
            reads
                .iter()
                .filter(|(path, thread)| *thread == here && path.starts_with(dir))
                .count()
        })
        .unwrap_or(0)
}

/// [`list_groups_and_skipped`]'s own logic, taking its [`list_front_in`]
/// cache and a freshly-parsed-file counter as plain arguments instead of
/// reaching into a process-wide static for either — the same split
/// [`super::status::cached_queue`] and `cached_queue_in` already make, and
/// for the same two reasons: a test can drive this directly against a
/// cache of its own, with nothing shared with any other test running in
/// parallel to race against; and the same call can report back how many
/// files it actually parsed, which is what the bug this closes is
/// measured by rather than by timing a reload.
///
/// The second half of the answer is the `.md` files under the pending
/// directory that no group could be built from, in filename order: one that
/// will not read, has no fence or readable YAML, or lacks a `group:` or an
/// `id:`. They are collected in this same pass, over the same cached parse,
/// so telling the person about them never opens a file a second time — the
/// reload this runs on every second parses nothing for a file whose bytes
/// have not moved, broken or not.
pub(crate) fn list_groups_in(
    repo: &Repo,
    front_cache: &mut ListFrontCache,
    parsed: &mut usize,
) -> Result<(Vec<Group>, Vec<PathBuf>)> {
    let dir = repo.pending_dir();
    let queue_dir = repo.queue_dir();
    let archive_dir = repo.archive_dir();

    // Grouped by `group:` verbatim, in a map ordered by that string, so a
    // group's own identity never depends on which task happened to be
    // read first.
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    // Every id read out of the pending directory below, plus every one read
    // out of the queue directory after it. The archive loop, last, skips any
    // id already in here: that task already spoke for itself in whichever of
    // the first two loops read it.
    let mut spoken_for: std::collections::BTreeSet<String> = Default::default();
    // Every path read out of the pending and queue directories this call —
    // what `front_cache` is pruned down to below, so a file removed or
    // queued away does not sit in the cache forever. The archive has no
    // entry here: it keeps its own, separately-pruned cache — see
    // `archive_tasks`, which reads the archive index instead.
    let mut seen_paths: std::collections::HashSet<PathBuf> = Default::default();
    // The pending files below that no group could be built from — the
    // second half of what this returns.
    let mut skipped: Vec<PathBuf> = Vec::new();

    for path in md_files(&dir)? {
        // A file that will not read — held open under an exclusive lock on
        // Windows, a broken symlink — is skipped like an unparsable one, not
        // propagated: one bad task must not fail the whole listing. It goes
        // into `skipped`, which is how the queue tab names it.
        //
        // Counted here, the one line that actually touches the disk — see
        // [`PENDING_READS`].
        #[cfg(test)]
        if let Ok(mut reads) = PENDING_READS.lock() {
            reads.push((path.clone(), std::thread::current().id()));
        }
        let Ok(doc) = std::fs::read_to_string(&path) else {
            skipped.push(path);
            continue;
        };
        seen_paths.insert(path.clone());
        // `list_front_in`, not a direct split-and-parse: this runs on
        // every one-second reload, and a file whose bytes have not moved
        // since the last call has nothing left to parse — see
        // `list_front_in`.
        let Some(front) = list_front_in(&path, &doc, front_cache, parsed) else {
            skipped.push(path);
            continue;
        };
        let (Some(group), Some(id)) = (front_str(&front, "group"), front_str(&front, "id")) else {
            skipped.push(path);
            continue;
        };

        // A task still sitting in `pending/` ordinarily has no stamp
        // anywhere else — but a re-run of a producer over work already
        // submitted names an id the queue, or even the archive, already
        // holds, and that task's own state has to say so rather than
        // read as still-queueable: `validate_batch` would refuse queueing it
        // again either way.
        let state = if archive_stamp(&archive_dir, &id) {
            TaskState::Done
        } else if queue_stamp(&queue_dir, &id) {
            TaskState::Queued
        } else {
            TaskState::Pending
        };
        let created = created_time(&path);
        let entry = groups.entry(group.clone()).or_insert_with(|| Group {
            name: group,
            tasks: Vec::new(),
            // Overwritten by `group_state` once every task is in; a fold
            // needs no seed here, unlike the bare `bool` this replaced.
            state: GroupState::Queueable,
            created: None,
        });
        if newest_first(created, entry.created) == std::cmp::Ordering::Less {
            entry.created = created;
        }
        spoken_for.insert(id.clone());
        entry.tasks.push(PendingTask {
            description: front_str(&front, "title"),
            depends_on: front_depends_on(&front),
            pipeline: front_pipeline_name(&front),
            base: front_str(&front, "base"),
            starts_from: front_starts_from(&front),
            path,
            id,
            doc,
            state,
        });
    }

    // A second source: tasks that live only in the queue directory.
    // Submitting a group deletes its pending tasks in the same act that
    // writes its queue ones (`queue::finish_submit`), so a group already
    // fully queued has nothing left under `dir` at all — this is the only
    // way such a group still gets a row, built from its queue tasks and
    // marked `Queued` by construction: nothing here reads as anything else.
    for path in md_files(&queue_dir)? {
        // The queue file's own name is what `queue_stamp` above already keys
        // on, so this uses the same identity rather than trusting the
        // task's own `id:` to agree with the name it was saved under.
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if spoken_for.contains(id) {
            continue;
        }
        let id = id.to_string();

        // Counted here, the one line that actually touches the disk — see
        // [`PENDING_READS`].
        #[cfg(test)]
        if let Ok(mut reads) = PENDING_READS.lock() {
            reads.push((path.clone(), std::thread::current().id()));
        }
        let Ok(doc) = std::fs::read_to_string(&path) else {
            continue;
        };
        seen_paths.insert(path.clone());
        // `list_front_in`, not a direct split-and-parse: this runs on
        // every one-second reload, and a file whose bytes have not moved
        // since the last call has nothing left to parse — see
        // `list_front_in`.
        let Some(front) = list_front_in(&path, &doc, front_cache, parsed) else {
            continue;
        };
        let Some(group) = front_str(&front, "group") else {
            continue;
        };

        let created = created_time(&path);
        let entry = groups.entry(group.clone()).or_insert_with(|| Group {
            name: group,
            tasks: Vec::new(),
            state: GroupState::Queueable,
            created: None,
        });
        if newest_first(created, entry.created) == std::cmp::Ordering::Less {
            entry.created = created;
        }
        spoken_for.insert(id.clone());
        entry.tasks.push(PendingTask {
            description: front_str(&front, "title"),
            depends_on: front_depends_on(&front),
            pipeline: front_pipeline_name(&front),
            base: front_str(&front, "base"),
            starts_from: front_starts_from(&front),
            path,
            id,
            doc,
            state: TaskState::Queued,
        });
    }

    // A path no longer in either directory is dropped here rather than
    // left to grow `front_cache` forever — the same pruning
    // `cached_queue_in` does for its own map.
    front_cache.retain(|path, _| seen_paths.contains(path));

    // A third source: tasks the pipeline already ran to the end. Read
    // last and skipped for any id the first two loops already claimed, so a
    // group half archived and half still queued lists its whole chain rather
    // than only the half still in the queue — the gap this whole feature
    // exists to close. `archive_tasks`, not a fourth copy of the loop
    // above: it lists the archive from its index, and only when the
    // archive directory's mtime moves — see that function's own comment —
    // rather than on every one-second reload the way the pending and queue
    // directories still are, since pending and queue files are rewritten in
    // place and archived ones never are.
    for (group_name, task, created) in archive_tasks(&archive_dir)? {
        if spoken_for.contains(&task.id) {
            continue;
        }
        let entry = groups.entry(group_name.clone()).or_insert_with(|| Group {
            name: group_name,
            tasks: Vec::new(),
            state: GroupState::Queueable,
            created: None,
        });
        if newest_first(created, entry.created) == std::cmp::Ordering::Less {
            entry.created = created;
        }
        entry.tasks.push(task);
    }

    let mut groups: Vec<Group> = groups
        .into_values()
        .map(|mut group| {
            group.tasks = in_reading_order(group.tasks);
            group.state = group_state(&group.tasks);
            group
        })
        .collect();

    groups.sort_by(|a, b| {
        a.state
            .cmp(&b.state)
            .then_with(|| newest_first(a.created, b.created))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok((groups, skipped))
}

/// The groups of [`list_groups_and_skipped`] alone, for the tests that only
/// want that half — production reads both halves from the one call.
#[cfg(test)]
pub(crate) fn list_groups(repo: &Repo) -> Result<Vec<Group>> {
    list_groups_and_skipped(repo).map(|(groups, _)| groups)
}

/// The pending files [`list_groups_and_skipped`] passed over, for the tests
/// that only want that half.
#[cfg(test)]
pub(crate) fn unreadable(repo: &Repo) -> Vec<PathBuf> {
    list_groups_and_skipped(repo)
        .map(|(_, skipped)| skipped)
        .unwrap_or_default()
}

/// Whether the queue already holds a task with this id — see [`TaskState`]
/// for what the screen does with the answer.
fn queue_stamp(queue_dir: &Path, id: &str) -> bool {
    queue_dir.join(format!("{id}.md")).exists()
}

/// Whether the archive already holds a task with this id — the same check
/// [`queue_stamp`] makes, one directory over.
fn archive_stamp(archive_dir: &Path, id: &str) -> bool {
    archive_dir.join(format!("{id}.md")).exists()
}

/// A group's tasks, dependencies before dependents.
///
/// The pane is read top to bottom, and a chain read in the order it will run
/// is the one a person can follow: the task that waits on nothing first, then
/// whatever waits on it. Filename order alone would not give that — a
/// task's name is its task id, and ids are not written to sort.
///
/// A stable selection sort over the in-group edges only: at each step the
/// first remaining task whose in-group dependencies have all been emitted.
/// Dependencies on tasks outside this group are ignored, since nothing here
/// can order against them. A cycle — which `check_dependencies_set` refuses
/// at submit time, not here — leaves some task always ineligible, so the
/// first one remaining is taken anyway rather than looping forever.
fn in_reading_order(mut tasks: Vec<PendingTask>) -> Vec<PendingTask> {
    let ids: std::collections::BTreeSet<String> = tasks.iter().map(|t| t.id.clone()).collect();
    let mut done: std::collections::BTreeSet<String> = Default::default();
    let mut ordered = Vec::with_capacity(tasks.len());

    while !tasks.is_empty() {
        let next = tasks
            .iter()
            .position(|task| {
                // `task.depends_on`, not `depends_on(&task.doc)`: `list_groups`
                // already parsed this exact doc once, just above, to read its
                // `group:` and `id:` — see `PendingTask::depends_on`.
                task.depends_on
                    .iter()
                    .all(|dep| !ids.contains(dep) || done.contains(dep))
            })
            .unwrap_or(0);
        let task = tasks.remove(next);
        done.insert(task.id.clone());
        ordered.push(task);
    }
    ordered
}

/// [`crate::task::split_fence`] plus a YAML parse, the one actual parse
/// [`list_front_in`] caches in front of.
fn parse_front(doc: &str) -> Option<serde_norway::Value> {
    let (yaml, _) = crate::task::split_fence(doc).ok()?;
    serde_norway::from_str(yaml).ok()
}

/// One path's own last-seen bytes and what they parsed to — [`list_front_in`]'s
/// own cache shape, named so a caller outside this module (a test driving
/// [`list_groups_in`] directly) can hold one of its own without reaching into
/// the function's internals.
pub(crate) type ListFrontCache = HashMap<PathBuf, (String, Option<serde_norway::Value>)>;

/// One task file's own front matter, parsed again only when its bytes have
/// moved since the last call that read this same path — what
/// [`list_groups_in`]'s pending and queue loops read a task's `group:`,
/// `id:`, `title:`, `depends_on:`, `pipeline:`, `base:` and
/// `starts_from:` off.
///
/// [`list_groups_in`] calls this with the cache its own caller handed it,
/// which is how [`list_groups_and_skipped`] shares one cache across every
/// real reload while a test can hand in one of its own instead — see its own
/// comment for why that split exists.
///
/// Keyed on the path *and* the bytes, the same pair
/// [`super::status::cached_queue`]'s own `CachedQueueFile` keys on: content
/// alone would let two different tests' byte-identical fixtures (several
/// write the exact same `wire`/`group: one` task text) collide on one
/// cache entry and on each other's state. A reload over a pending or queue
/// file whose bytes have not moved since the path was last read finds this
/// entry and parses nothing; a file rewritten with new content of the same
/// length is a different value at the same key and is parsed fresh,
/// because the comparison is the bytes themselves, not their length or
/// the file's mtime.
///
/// `None`, cached the same as a successful parse, for a doc with no fence
/// or unparsable YAML — a malformed task is not retried on every reload.
/// `parsed` is incremented only on an actual cache miss that goes on to
/// call [`parse_front`], successful or not — what the tests count to prove
/// a reload over unchanged files parses nothing, rather than timing one.
///
/// One entry per path still present the last time [`list_groups_in`]
/// walked the pending and queue directories is what this is pruned down
/// to there, the same pruning `cached_queue_in` does for its own map — see
/// that function's own comment.
fn list_front_in(
    path: &Path,
    doc: &str,
    cache: &mut ListFrontCache,
    parsed: &mut usize,
) -> Option<serde_norway::Value> {
    if let Some((bytes, value)) = cache.get(path)
        && bytes == doc
    {
        return value.clone();
    }
    *parsed += 1;
    let value = parse_front(doc);
    cache.insert(path.to_path_buf(), (doc.to_string(), value.clone()));
    value
}

/// A task's own `depends_on:` list, read straight off an already-parsed
/// [`serde_norway::Value`] — what [`list_groups_in`] and [`depends_on`] both
/// want from one parse, each already holding its own `Value` by the time it
/// asks.
pub(crate) fn front_depends_on(front: &serde_norway::Value) -> Vec<String> {
    front
        .as_mapping()
        .and_then(|m| m.get("depends_on"))
        .and_then(|v| v.as_sequence())
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// A task's own `pipeline:` key, read straight off an already-parsed
/// [`serde_norway::Value`] — the same shallow read
/// [`super::queue::doc_pipeline_name`] makes starting from a raw doc
/// string, kept as its own function so [`list_groups_in`] can fill
/// [`PendingTask::pipeline`] from a `Value` it already holds rather than handing the doc back to be split and parsed
/// again. `None` for a task naming no pipeline of its own; unlike
/// [`front_str`], an empty `pipeline: ""` reads as `Some(String::new())`
/// rather than `None`, matching `doc_pipeline_name`'s own reading exactly.
pub(crate) fn front_pipeline_name(front: &serde_norway::Value) -> Option<String> {
    front
        .as_mapping()?
        .get("pipeline")?
        .as_str()
        .map(str::to_string)
}

/// A peek at a task's own `depends_on` list, unvalidated. The screen only
/// has to show what a task claims; `parse_submission` is what checks the
/// claim.
///
/// [`in_reading_order`] and [`super::queue::tasks_pane_lines`] do not call
/// this: they read [`PendingTask::depends_on`] instead, which
/// [`list_groups_in`] already filled in from its own parse — see that
/// field's own comment. What remains is `super::queue::mint_routine_batch`
/// and `super::queue::routine_task_lines`, which both run over a
/// `super::routines::RoutineTask` rather than a `PendingTask`, and so have
/// no parsed [`serde_norway::Value`] of their own lying around to read
/// this off; each still parses its own doc here.
pub(crate) fn depends_on(doc: &str) -> Vec<String> {
    let Ok((yaml, _)) = crate::task::split_fence(doc) else {
        return Vec::new();
    };
    let Ok(value) = serde_norway::from_str::<serde_norway::Value>(yaml) else {
        return Vec::new();
    };
    front_depends_on(&value)
}

/// One archived task, as [`list_groups_in`]'s third source wants it — the
/// task's own `group:` paired with the rest of it already built into a
/// [`PendingTask`] marked [`TaskState::Done`], and the time it counts as
/// created for ordering its group.
///
/// Built from `archive/index.jsonl` through [`crate::archive_index`], not
/// from the task files, so a one-second reload costs one small file however
/// many tasks have finished. The task's text is left out of the
/// [`PendingTask`] and is read from its file by [`PendingTask::text`] when a
/// key needs that one task.
///
/// Read again only when the archive directory's own modified time has moved
/// since the last call: the index is stamped with that time whenever it
/// matches the folder, so a task filed in or swept out always moves it.
/// Keyed by the directory path and its mtime together, the same pair
/// [`super::status::cached_archive`] keys its own single-slot cache by, for
/// the same reason: two different tests' archive directories never collide,
/// since each is a distinct path under its own fixture root.
///
/// A poisoned lock falls back to a plain, uncached read, the same
/// fallback shape `cached_archive` and `cached_queue` take.
///
/// A directory that will not list is an error: [`list_groups_and_skipped`]
/// fails and `reload` keeps the groups already on screen. Swallowing it into an empty
/// listing instead would drop every archive-only group from the screen, and
/// caching that empty listing under the directory's mtime would keep them
/// dropped until a task was next filed or swept.
fn archive_tasks(dir: &Path) -> Result<Vec<ArchivedTask>> {
    fn fresh(dir: &Path) -> Result<Vec<ArchivedTask>> {
        let mut out = Vec::new();
        for entry in crate::archive_index::read_at(dir)? {
            let group = entry.group.trim();
            if group.is_empty() {
                continue;
            }
            let trimmed = |text: &str| Some(text.trim().to_string()).filter(|t| !t.is_empty());
            out.push((
                group.to_string(),
                PendingTask {
                    description: trimmed(&entry.title),
                    depends_on: entry.depends_on.clone(),
                    pipeline: Some(entry.pipeline.clone()).filter(|name| !name.is_empty()),
                    base: entry.base.as_deref().and_then(trimmed),
                    starts_from: entry.starts_from.as_deref().and_then(trimmed),
                    path: dir.join(format!("{}.md", entry.id)),
                    id: entry.id.clone(),
                    doc: String::new(),
                    state: TaskState::Done,
                },
                // The file's creation time, as `created_time` read it off the
                // file itself; the archived time only for an entry written
                // before the index held it.
                std::time::UNIX_EPOCH.checked_add(match entry.created_ns {
                    Some(ns) => std::time::Duration::from_nanos(ns),
                    None => std::time::Duration::from_secs(entry.archived_at.max(0) as u64),
                }),
            ));
        }
        // The folder listing's order, which tasks of one group reach
        // `in_reading_order` in.
        out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        Ok(out)
    }

    struct ArchiveCache {
        dir: PathBuf,
        mtime: std::time::SystemTime,
        tasks: Vec<ArchivedTask>,
    }

    static CACHE: OnceLock<Mutex<Option<ArchiveCache>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let dir_mtime = std::fs::metadata(dir).and_then(|meta| meta.modified()).ok();

    let Ok(mut guard) = cache.lock() else {
        return fresh(dir);
    };
    if let (Some(cached), Some(dir_mtime)) = (guard.as_ref(), dir_mtime)
        && cached.dir == dir
        && cached.mtime == dir_mtime
    {
        return Ok(cached.tasks.clone());
    }
    let tasks = fresh(dir)?;
    if let Some(dir_mtime) = dir_mtime {
        *guard = Some(ArchiveCache {
            dir: dir.to_path_buf(),
            mtime: dir_mtime,
            tasks: tasks.clone(),
        });
    }
    Ok(tasks)
}

/// What [`archive_tasks`] hands [`list_groups_in`] for one archived task: its
/// group, the row, and the time that orders the group.
type ArchivedTask = (String, PendingTask, Option<std::time::SystemTime>);

// ===================== The `/` filter's own scorer =========================
//
// What follows scores one query against one field of a group: the fuzzy
// match `spoolway queue`'s own filter mode narrows the pending list by. It
// lives here rather than in `super::queue` because every field it reads —
// the group's name, a task's id, a task's title — is something this module
// already owns the shape of; `super::queue::group_score` is the one place
// that walks a `Group` and adds these fields' own weights.

/// Below this score a group is judged too weak a match to list at all, even
/// though something in it did match — usually a short query smeared across a
/// long sentence, which a bare subsequence search would otherwise treat the
/// same as a tight, meaningful hit.
pub(crate) const MATCH_FLOOR: i64 = 60;

/// Below this many characters in the query, only a group's name and each
/// task's id are searched — every task's title stays out of it. A one- or
/// two-character query is exactly where a subsequence search is most likely
/// to land inside a long sentence by accident, and the short, name-shaped
/// fields are the ones a query that short is actually useful against.
pub(crate) const RICH_FIELD_MIN_QUERY: usize = 3;

/// What a query of more than one character earns when every one of its
/// characters lands on a word start in the window found — see [`score`]'s
/// own doc comment on step 3 of the scoring rule. Flat rather than
/// slack-penalized: `qcp` landing on `q`, `c` and `p` in
/// `queue-conflict-picker` is exactly the kind of acronym match this filter
/// exists to reward, however far apart its letters actually sit.
const WORD_START_SCORE: i64 = 150;

/// What an ordinary match starts from before [`SLACK_PENALTY`] is taken off
/// per character of slack — see [`score`]'s step 3.
const BASE_SCORE: i64 = 100;

/// Points taken off [`BASE_SCORE`] for every character of slack — the
/// matched window's own length beyond the query's — an ordinary match
/// carries. The wider the window a query's letters had to spread across to
/// match, the weaker the match.
const SLACK_PENALTY: i64 = 12;

/// How much a window entirely on word starts is preferred over one that
/// merely has the same span, when [`score`] is choosing the least-cost
/// window in step 1 of the scoring rule — see [`min_window`].
const WORD_START_WINDOW_DISCOUNT: i64 = 60;

/// Added when the chosen window itself starts on a word start — step 4 of
/// the scoring rule — whether or not every character inside it does.
const WINDOW_START_BONUS: i64 = 20;

/// The divisor [`score`]'s step 5 spends a field's own length against: the
/// same match costs less the longer the field it was found buried in.
const LENGTH_PENALTY_DIVISOR: i64 = 20;

/// Field weights — step 6 of the scoring rule. A group's name is what a
/// person is looking for it by, so a match there counts for the most; a
/// task's id is a name too, just one level down; a task's title is a full
/// sentence, worth a match on its own but not worth more than that.
pub(crate) const GROUP_WEIGHT: i64 = 30;
pub(crate) const TASK_ID_WEIGHT: i64 = 20;
pub(crate) const DESCRIPTION_WEIGHT: i64 = 0;

/// Whether `field[i]` is a word start: the field's first character, or one
/// whose predecessor is not alphanumeric — step 1 of the scoring rule's own
/// definition.
fn is_word_start(field: &[char], i: usize) -> bool {
    i == 0 || !field[i - 1].is_alphanumeric()
}

/// The least-span window of `field` (already case-folded) holding `query`
/// (already case-folded) as a subsequence, considering only the positions
/// `allowed` admits — every position for an unrestricted search, or only the
/// word starts for the discounted search [`score`] runs beside it. Returns
/// the window as inclusive `(start, end)` indices into `field`, or `None`
/// when no such window exists at all.
///
/// The classic minimum-window-subsequence technique, in one pass: a forward
/// scan from `i` finds *a* window ending as early as possible, then a
/// backward scan from that same end finds how late its start can be pushed
/// up without losing the match — turning "a window that works" into "the
/// least-span window ending here". Restarting just past the previous
/// window's own start, rather than at `i + 1`, is what keeps this to one
/// pass over `field` instead of one per candidate window.
fn min_window(
    field: &[char],
    query: &[char],
    allowed: impl Fn(usize) -> bool,
) -> Option<(usize, usize)> {
    if query.is_empty() {
        return None;
    }
    let mut best: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < field.len() {
        let mut qi = 0;
        let mut k = i;
        while k < field.len() && qi < query.len() {
            if allowed(k) && field[k] == query[qi] {
                qi += 1;
            }
            k += 1;
        }
        if qi < query.len() {
            // Nothing left in `field` can complete the match.
            break;
        }
        let end = k - 1;

        // Walk backward from `end`, matching the query from its own last
        // character, to find how late this window's start can be.
        let mut qi = query.len();
        let mut k = end + 1;
        loop {
            k -= 1;
            if allowed(k) && field[k] == query[qi - 1] {
                qi -= 1;
                if qi == 0 {
                    break;
                }
            }
        }
        let start = k;

        if best.is_none_or(|(bs, be)| end - start < be - bs) {
            best = Some((start, end));
        }
        i = start + 1;
    }
    best
}

/// One query against one field, under the scoring rule: the least-cost
/// window holding the query as a subsequence (step 1-2), scored (step 3),
/// bonused for starting on a word start (step 4) and penalized for the rest
/// of the field around it (step 5) — everything but the field's own weight
/// (step 6), which only [`super::queue::group_score`] knows since it differs
/// by which field this was. `None` when the query does not occur in the
/// field as a subsequence at all.
///
/// Case-insensitive by lower-casing both strings first — ASCII-only, same as
/// every field this runs against (group names, task ids, English prose) — so
/// [`is_word_start`]'s own alphanumeric check still lines up character for
/// character with the lower-cased text [`min_window`] matches against.
pub(crate) fn score(query: &str, field: &str) -> Option<i64> {
    let query_chars: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let field_chars: Vec<char> = field.chars().collect();
    let field_lower: Vec<char> = field_chars.iter().map(|c| c.to_ascii_lowercase()).collect();

    let starts: Vec<bool> = (0..field_chars.len())
        .map(|i| is_word_start(&field_chars, i))
        .collect();

    let any = min_window(&field_lower, &query_chars, |_| true);
    let word = min_window(&field_lower, &query_chars, |i| starts[i]);

    // The least-cost window: an unrestricted span costs itself, a
    // word-start-only span costs itself minus the discount — see
    // `WORD_START_WINDOW_DISCOUNT`. Whichever is lower wins; a tie prefers
    // the word-start window, which can only tie by being the very same span
    // the unrestricted search already found.
    let any_cost = any.map(|(s, e)| (e - s) as i64);
    let word_cost = word.map(|(s, e)| (e - s) as i64 - WORD_START_WINDOW_DISCOUNT);
    let (window, all_word_start) = match (any_cost, word_cost) {
        (None, None) => return None,
        (Some(_), None) => (any.unwrap(), false),
        (None, Some(_)) => (word.unwrap(), true),
        (Some(ac), Some(wc)) if wc <= ac => (word.unwrap(), true),
        (Some(_), Some(_)) => (any.unwrap(), false),
    };

    let (start, end) = window;
    let window_len = (end - start + 1) as i64;
    let query_len = query_chars.len() as i64;
    let slack = window_len - query_len;

    let mut total = if query_len > 1 && all_word_start {
        WORD_START_SCORE
    } else {
        BASE_SCORE - slack * SLACK_PENALTY
    };
    if starts[start] {
        total += WINDOW_START_BONUS;
    }
    total -= (field_chars.len() as i64 - window_len) / LENGTH_PENALTY_DIVISOR;
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole task, in the shape `--from` accepts.
    fn doc(id: &str, group: &str, extra: &str) -> String {
        format!(
            "---\nid: {id}\ntitle: {id}, done\ngroup: {group}\n{extra}---\n\
             ## Goal\n\nDo the thing.\n"
        )
    }

    fn write(repo: &Repo, name: &str, text: &str) -> PathBuf {
        let path = repo.pending_dir().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// A fresh project has planned nothing, and that is not an error — the
    /// screen has to open onto an empty list rather than refuse to start.
    #[test]
    fn a_project_with_no_pending_directory_lists_none() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-none");
        assert!(list_groups(&repo).unwrap().is_empty());
    }

    /// The comparator alone, with no filesystem involved — every branch
    /// [`list_groups`] relies on, including both instants missing at once.
    #[test]
    fn newest_first_sorts_missing_instants_last() {
        use std::cmp::Ordering;
        use std::time::{Duration, SystemTime};

        let older = SystemTime::UNIX_EPOCH;
        let newer = SystemTime::UNIX_EPOCH + Duration::from_secs(60);

        assert_eq!(newest_first(Some(newer), Some(older)), Ordering::Less);
        assert_eq!(newest_first(Some(older), Some(newer)), Ordering::Greater);
        assert_eq!(newest_first(Some(older), None), Ordering::Less);
        assert_eq!(newest_first(None, Some(newer)), Ordering::Greater);
        assert_eq!(newest_first(None, None), Ordering::Equal);
    }

    /// The left pane's whole unit: one row per distinct `group:`, however
    /// many tasks named it, and each row carrying every one of them.
    #[test]
    fn tasks_gather_into_one_row_per_distinct_group() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-groups");
        write(
            &repo,
            "split-fields.md",
            &doc("split-fields", "issue-42", ""),
        );
        write(
            &repo,
            "scan-pending.md",
            &doc("scan-pending", "issue-42", ""),
        );
        write(&repo, "quiet.md", &doc("quiet", "quiet-doctor", ""));

        let groups = list_groups(&repo).unwrap();
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&"issue-42"), "{names:?}");
        assert!(names.contains(&"quiet-doctor"), "{names:?}");

        let issue = groups.iter().find(|g| g.name == "issue-42").unwrap();
        assert_eq!(issue.tasks.len(), 2);
    }

    /// `group:` is read verbatim, never path-parsed — two strings that only
    /// look alike are two groups, the same rule `spoolway group list` reads
    /// the queue by.
    #[test]
    fn a_group_is_the_string_verbatim() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-verbatim");
        write(&repo, "a.md", &doc("a", "plans/auth", ""));
        write(&repo, "b.md", &doc("b", "auth", ""));

        assert_eq!(list_groups(&repo).unwrap().len(), 2);
    }

    /// A chain reads in the order it will run, not in the order its files
    /// happen to sort: `scan-pending` waits on `split-fields`, and sorts
    /// before it by filename, so filename order alone would draw the pane
    /// backwards.
    #[test]
    fn a_groups_tasks_are_ordered_dependencies_first() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-order");
        write(
            &repo,
            "scan-pending.md",
            &doc("scan-pending", "issue-42", "depends_on: [split-fields]\n"),
        );
        write(
            &repo,
            "split-fields.md",
            &doc("split-fields", "issue-42", ""),
        );

        let groups = list_groups(&repo).unwrap();
        let ids: Vec<&str> = groups[0].tasks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["split-fields", "scan-pending"]);
    }

    /// A `depends_on` naming a task of some other group orders nothing here
    /// — there is no sibling in this group to sort behind — and above all
    /// must not drop the task out of the pane.
    #[test]
    fn a_dependency_outside_the_group_orders_nothing() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-order-outside");
        write(
            &repo,
            "one.md",
            &doc("one", "issue-42", "depends_on: [elsewhere]\n"),
        );

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups[0].tasks.len(), 1);
        assert_eq!(groups[0].tasks[0].id, "one");
    }

    /// A cycle is refused at submit time, not here — so the pane still draws
    /// every task rather than looping for want of an eligible one.
    #[test]
    fn a_cycle_still_lists_every_task() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-cycle");
        write(&repo, "a.md", &doc("a", "loop", "depends_on: [b]\n"));
        write(&repo, "b.md", &doc("b", "loop", "depends_on: [a]\n"));

        assert_eq!(list_groups(&repo).unwrap()[0].tasks.len(), 2);
    }

    /// A task that names no `group:`, or whose frontmatter will not
    /// parse at all, has no row to be one of — a row *is* a `group:` value.
    /// It is skipped rather than shown, and `queue add --from` is what
    /// refuses it by name. [`list_groups_and_skipped`] hands back the same
    /// set, for the one case the screen has to say so out loud.
    #[test]
    fn a_task_with_no_readable_group_is_skipped() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-ungrouped");
        write(&repo, "good.md", &doc("good", "issue-42", ""));
        std::fs::write(
            repo.pending_dir().join("no-group.md"),
            "---\nid: stray\ntitle: stray\n---\n## Goal\n\nx\n",
        )
        .unwrap();
        std::fs::write(repo.pending_dir().join("garbage.md"), "not a task\n").unwrap();

        let (groups, skipped) = list_groups_and_skipped(&repo).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "issue-42");

        let names: Vec<String> = skipped
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["garbage.md", "no-group.md"], "{names:?}");
    }

    /// The screen only says anything about skipped tasks when there is
    /// nothing else to list, so the skipped list must come back empty on a
    /// directory where every task reads — otherwise the ordinary case
    /// would be paying for a listing nobody looks at.
    #[test]
    fn unreadable_is_empty_when_every_task_reads() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-all-readable");
        write(&repo, "a.md", &doc("a", "issue-42", ""));
        write(&repo, "b.md", &doc("b", "issue-42", ""));

        assert!(list_groups_and_skipped(&repo).unwrap().1.is_empty());
    }

    /// Only `.md` is read. Anything else in the directory is not a task,
    /// and above all no page is scanned for one.
    #[test]
    fn only_markdown_tasks_are_read() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-md-only");
        write(&repo, "a.md", &doc("a", "issue-42", ""));
        std::fs::write(repo.pending_dir().join("a.html"), "<html></html>").unwrap();

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tasks.len(), 1);
    }

    /// A group every one of whose tasks the queue already holds cannot be
    /// queued again — `validate_batch` would refuse it — so it is marked and
    /// sorted behind the groups that can be. The queue screen leaves it off its
    /// list altogether.
    #[test]
    fn a_group_already_in_the_queue_is_marked_and_sorted_last() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-already-queued");
        write(&repo, "done.md", &doc("done", "old-group", ""));
        write(&repo, "fresh.md", &doc("fresh", "new-group", ""));
        std::fs::write(
            repo.queue_dir().join("done.md"),
            "---\nid: done\ntitle: done\nstage: queued\n---\nbody\n",
        )
        .unwrap();

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups[0].name, "new-group", "queueable groups come first");
        assert_eq!(groups[0].state, GroupState::Queueable);
        assert_eq!(groups[1].state, GroupState::Queued);
    }

    /// One of a group's tasks already queued does not make the group queued:
    /// the rest still have to be submitted, and hiding the row would be
    /// hiding the work that is left.
    #[test]
    fn a_group_only_partly_queued_is_still_queueable() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-part-queued");
        write(&repo, "one.md", &doc("one", "issue-42", ""));
        write(&repo, "two.md", &doc("two", "issue-42", ""));
        std::fs::write(
            repo.queue_dir().join("one.md"),
            "---\nid: one\ntitle: one\nstage: queued\n---\nbody\n",
        )
        .unwrap();

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups[0].state, GroupState::Queueable);
    }

    /// Submitting a group deletes its pending tasks (`finish_submit`),
    /// so a group already queued in full has nothing left under the pending
    /// directory at all — this is the one case that still has to produce a
    /// row, built entirely from the queue directory instead.
    #[test]
    fn a_group_with_no_pending_tasks_still_lists_from_the_queue() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-queue-only");
        std::fs::write(
            repo.queue_dir().join("shipped.md"),
            "---\nid: shipped\ntitle: shipped, done\ngroup: already-gone\nstage: queued\n\
             ---\nbody\n",
        )
        .unwrap();

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "already-gone");
        assert_eq!(groups[0].state, GroupState::Queued);
        assert_eq!(groups[0].tasks.len(), 1);
        assert_eq!(groups[0].tasks[0].id, "shipped");
        assert_eq!(
            groups[0].tasks[0].description.as_deref(),
            Some("shipped, done")
        );
    }

    /// A group wholly in the archive gets a row too, built from its archived
    /// tasks the same way a wholly-queued group is built from its queue
    /// ones — the third source this whole feature adds.
    #[test]
    fn a_group_wholly_in_the_archive_still_lists() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-archive-only");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("finished.md"),
            "---\nid: finished\ntitle: finished, done\ngroup: wholly-done\nstage: done\n\
             ---\nbody\n",
        )
        .unwrap();

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "wholly-done");
        assert_eq!(groups[0].state, GroupState::Done);
        assert_eq!(groups[0].tasks[0].state, TaskState::Done);
    }

    /// The constraint the context notes call out by name: a chain half
    /// archived and half still queued lists every one of its tasks, each
    /// carrying its own state, and the group as a whole reads `Queued` —
    /// not `Done`, since one of its tasks has not reached the archive yet.
    #[test]
    fn a_group_half_archived_and_half_queued_lists_its_whole_chain() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-mixed-states");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("first.md"),
            "---\nid: first\ntitle: first, done\ngroup: chain\nstage: done\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            repo.queue_dir().join("second.md"),
            "---\nid: second\ntitle: second, done\ngroup: chain\nstage: queued\n\
             depends_on: [first]\n---\nbody\n",
        )
        .unwrap();

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tasks.len(), 2, "the whole chain, not just half");
        assert_eq!(groups[0].state, GroupState::Queued);

        let first = groups[0].tasks.iter().find(|t| t.id == "first").unwrap();
        let second = groups[0].tasks.iter().find(|t| t.id == "second").unwrap();
        assert_eq!(first.state, TaskState::Done);
        assert_eq!(second.state, TaskState::Queued);
    }

    /// A pending task naming an id the archive already holds — a re-run
    /// of a producer over work already finished — reads that task as `Done`,
    /// not `Pending`: `validate_batch` would refuse queueing it again either
    /// way, and the group must not read as still-queueable over a task that
    /// is not.
    #[test]
    fn a_pending_task_already_archived_reads_as_done() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-already-archived");
        write(&repo, "done.md", &doc("done", "old-group", ""));
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("done.md"),
            "---\nid: done\ntitle: done\nstage: done\n---\nbody\n",
        )
        .unwrap();

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups[0].state, GroupState::Done);
        assert_eq!(groups[0].tasks[0].state, TaskState::Done);
    }

    /// The tasks pane draws a task's own `title:` under what it waits
    /// on, and `waits on` reads `depends_on` off the same task without
    /// validating it.
    #[test]
    fn a_task_carries_its_title_and_its_waits_on() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-fields");
        write(
            &repo,
            "b.md",
            &doc("b", "issue-42", "touches: [src/a.rs]\ndepends_on: [a]\n"),
        );
        write(&repo, "a.md", &doc("a", "issue-42", ""));

        let groups = list_groups(&repo).unwrap();
        let b = groups[0].tasks.iter().find(|t| t.id == "b").unwrap();
        assert_eq!(b.description.as_deref(), Some("b, done"));
        assert_eq!(depends_on(&b.doc), vec!["a"]);
    }

    /// A task carrying no `depends_on` — or no fence at all — reads as
    /// an empty list rather than an error: the pane shows "waits on nothing"
    /// for the first and never panics on either.
    #[test]
    fn depends_on_defaults_to_an_empty_list() {
        assert_eq!(depends_on(&doc("a", "g", "")), Vec::<String>::new());
        assert_eq!(depends_on("not a task"), Vec::<String>::new());
    }

    /// `queue-tab-reads-changed-only`: `list_front_in`'s own per-path cache
    /// must never change what `list_groups_in` reports for the files
    /// actually on disk — checked by comparing it field by field (`Group`
    /// and `PendingTask` both derive `PartialEq` for exactly this) against
    /// a cold read of the same files, built with an empty cache of its own
    /// rather than the one `b.md`'s now-stale bytes and `a.md`'s previous
    /// content are still sitting in. That covers every field the
    /// acceptance criterion names — groups, their order, their tasks, and
    /// each one's own `created` — rather than only the ids this test
    /// checked before review (finding 5).
    ///
    /// One file is added, a second removed, and a third rewritten in
    /// place with new content of the *same byte length* — the one case a
    /// cache keyed on length or mtime alone would wrongly call unchanged.
    #[test]
    fn list_groups_matches_a_fresh_read_after_files_are_added_removed_and_rewritten() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-cache-equivalence");
        write(&repo, "a.md", &doc("a", "g", ""));
        write(&repo, "b.md", &doc("b", "g", ""));

        let mut warm_cache = HashMap::new();
        let mut parsed = 0;
        let (warm, _) = list_groups_in(&repo, &mut warm_cache, &mut parsed).unwrap();
        assert_eq!(warm.len(), 1, "one group, `g`, holding both tasks");
        assert_eq!(warm[0].tasks.len(), 2);

        std::fs::remove_file(repo.pending_dir().join("b.md")).unwrap();
        write(&repo, "c.md", &doc("c", "g", ""));
        // `g` and `h` are both one byte long: this rewrite changes `a`'s
        // own group without changing `a.md`'s length at all.
        write(&repo, "a.md", &doc("a", "h", ""));

        let mut parsed_warm = 0;
        let (warm_after, _) = list_groups_in(&repo, &mut warm_cache, &mut parsed_warm).unwrap();

        let mut cold_cache = HashMap::new();
        let mut parsed_cold = 0;
        let (cold_after, _) = list_groups_in(&repo, &mut cold_cache, &mut parsed_cold).unwrap();

        assert_eq!(
            warm_after, cold_after,
            "a cache that still remembers b.md's old bytes and a.md's previous \
             content must report exactly the same groups, order, tasks and \
             created times a cold read of the files actually on disk would"
        );

        let g = warm_after.iter().find(|group| group.name == "g").unwrap();
        assert_eq!(
            g.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["c"],
            "`b` is gone and `a` moved out, leaving only the task just added"
        );
        let h = warm_after.iter().find(|group| group.name == "h").unwrap();
        assert_eq!(
            h.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            vec!["a"],
            "`a.md`'s new content, same length as its old content, must still be read fresh"
        );
    }

    /// `queue-tab-reads-changed-only`: a second `list_groups_in` call over
    /// files whose bytes have not moved parses none of them; rewriting one
    /// of them forces exactly that one to be parsed again, not the other —
    /// the per-file byte cache this task closes "reload reparses
    /// everything, every second" with, counted directly rather than timed
    /// (review finding 4: nothing had covered this before).
    #[test]
    fn list_groups_in_parses_only_a_file_whose_bytes_changed() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-cache-parse-count");
        write(&repo, "a.md", &doc("a", "g", ""));
        write(&repo, "b.md", &doc("b", "g", ""));

        let mut cache = HashMap::new();
        let mut parsed = 0;
        let _ = list_groups_in(&repo, &mut cache, &mut parsed).unwrap();
        assert_eq!(parsed, 2, "both files are new, so both are parsed");

        let mut parsed_again = 0;
        let _ = list_groups_in(&repo, &mut cache, &mut parsed_again).unwrap();
        assert_eq!(
            parsed_again, 0,
            "nothing on disk changed, so a second reading parses nothing"
        );

        write(&repo, "a.md", &doc("a", "h", ""));
        let mut parsed_third = 0;
        let _ = list_groups_in(&repo, &mut cache, &mut parsed_third).unwrap();
        assert_eq!(
            parsed_third, 1,
            "only the file whose bytes changed is parsed again"
        );
    }

    /// The files `list_groups_in` passes over come from the same cached
    /// pass as the groups: a broken file is named on a second reading
    /// without being parsed again, and a repaired one stops being named.
    /// Before, a separate scan re-read and re-parsed every pending file on
    /// each reload, so the second reading here would have counted them.
    #[test]
    fn list_groups_in_names_a_broken_file_without_parsing_it_again() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-skipped-cached");
        write(&repo, "a.md", &doc("a", "g", ""));
        std::fs::write(repo.pending_dir().join("torn.md"), "no fence here\n").unwrap();

        let mut cache = HashMap::new();
        let mut parsed = 0;
        let (_, skipped) = list_groups_in(&repo, &mut cache, &mut parsed).unwrap();
        assert_eq!(skipped, vec![repo.pending_dir().join("torn.md")]);
        assert_eq!(parsed, 2, "both files are new, so both are parsed");

        let mut parsed_again = 0;
        let (_, skipped) = list_groups_in(&repo, &mut cache, &mut parsed_again).unwrap();
        assert_eq!(skipped, vec![repo.pending_dir().join("torn.md")]);
        assert_eq!(
            parsed_again, 0,
            "a broken file is cached like any other, not retried on every reading"
        );

        write(&repo, "torn.md", &doc("torn", "g", ""));
        let (groups, skipped) = list_groups_in(&repo, &mut cache, &mut parsed_again).unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(groups[0].tasks.len(), 2);
    }

    /// `queue-tab-reads-changed-only`: the archive is read again, not from
    /// any stale cache, once a task that was queued moves into it — the
    /// same lifecycle `teardown` drives: a queue file deleted and the same
    /// task written under the archive directory instead.
    #[test]
    fn list_groups_rereads_the_archive_once_a_task_moves_into_it() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-archive-reread");
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(repo.queue_dir().join("a.md"), doc("a", "g", "")).unwrap();

        let before = list_groups(&repo).unwrap();
        assert_eq!(before[0].tasks[0].state, TaskState::Queued);

        std::fs::remove_file(repo.queue_dir().join("a.md")).unwrap();
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(repo.archive_dir().join("a.md"), doc("a", "g", "")).unwrap();

        let after = list_groups(&repo).unwrap();
        assert_eq!(
            after[0].tasks[0].state,
            TaskState::Done,
            "a second list_groups call must see the task that just moved into \
             the archive, not the queued state the first call cached"
        );
    }

    /// `queue-tab-reads-changed-only`: an archive directory that will not
    /// list is an error, the way the uncached loop's `md_files(..)?` made
    /// it, not an empty listing. An empty listing would drop every
    /// archive-only group from the screen, where an error keeps the groups
    /// `reload` already has.
    #[test]
    fn an_archive_directory_that_will_not_list_is_an_error_not_an_empty_listing() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-archive-unlistable");
        let not_a_dir = repo.archive_dir().join("not-a-directory");
        std::fs::write(&not_a_dir, "").unwrap();

        assert!(archive_tasks(&not_a_dir).is_err());
        assert!(
            archive_tasks(&not_a_dir).is_err(),
            "a failed listing must not be cached as an empty one"
        );
    }

    /// The queue tab lists archived tasks from the archive index: nothing
    /// below opens a task file, a task archived under a warm cache adds its
    /// own row and nothing else, and the listing is what the files give after
    /// adds, a sweep and a rebuild. The task's text is read from its own
    /// file, and only when asked for.
    #[test]
    fn list_groups_reads_archived_tasks_from_the_index_and_matches_the_files() {
        use crate::archive_index::testutil::*;
        let (repo, _root_guard) = crate::commands::testutil::fixture("pending-archive-index");
        let front = "group: g\npipeline: impl\nbase: plan/demo\n";
        archive(&repo, "a", front);
        archive(
            &repo,
            "b",
            &format!("{front}depends_on: [a]\ncut_from: task/a\n"),
        );
        archive(&repo, "bare", "group: other\n");
        // Created a moment apart, the later one's file modified earliest, so
        // an order taken from modification or archiving time would differ.
        let nudge = |id: &str, secs: u64| {
            crate::scratch::set_mtime(
                &repo.archive_dir().join(format!("{id}.md")),
                std::time::SystemTime::now() + std::time::Duration::from_secs(secs),
            );
        };
        // Only where files have a birth time to tell apart from the
        // modification time; elsewhere both read the same and nothing is
        // being distinguished.
        if std::fs::metadata(repo.archive_dir().join("a.md"))
            .unwrap()
            .created()
            .is_ok()
        {
            nudge("a", 50);
            nudge("bare", 10);
        }

        // The listing the folder gives, field by field.
        let from_files = || {
            let mut rows = Vec::new();
            for path in md_files(&repo.archive_dir()).unwrap() {
                let doc = std::fs::read_to_string(&path).unwrap();
                let front = parse_front(&doc).unwrap();
                rows.push((
                    front_str(&front, "group").unwrap(),
                    front_str(&front, "id").unwrap(),
                    front_str(&front, "title"),
                    front_depends_on(&front),
                    front_pipeline_name(&front),
                    front_str(&front, "base"),
                    front_starts_from(&front),
                ));
            }
            rows
        };
        // Group order as `created_time` of each group's newest file gives it,
        // names breaking ties, which is what the folder-reading listing used.
        let group_order = |repo: &Repo| -> Vec<String> {
            list_groups(repo)
                .unwrap()
                .into_iter()
                .map(|g| g.name)
                .collect()
        };
        let order_from_files = |repo: &Repo| -> Vec<String> {
            let mut groups: BTreeMap<String, Option<std::time::SystemTime>> = BTreeMap::new();
            for path in md_files(&repo.archive_dir()).unwrap() {
                let doc = std::fs::read_to_string(&path).unwrap();
                let group = front_str(&parse_front(&doc).unwrap(), "group").unwrap();
                let created = created_time(&path);
                let entry = groups.entry(group).or_insert(None);
                if newest_first(created, *entry) == std::cmp::Ordering::Less {
                    *entry = created;
                }
            }
            let mut order: Vec<_> = groups.into_iter().collect();
            order.sort_by(|a, b| newest_first(a.1, b.1).then_with(|| a.0.cmp(&b.0)));
            order.into_iter().map(|(name, _)| name).collect()
        };
        let from_index = |repo: &Repo| {
            let mut rows = Vec::new();
            for group in list_groups(repo).unwrap() {
                for t in group.tasks {
                    assert_eq!(t.state, TaskState::Done);
                    rows.push((
                        group.name.clone(),
                        t.id,
                        t.description,
                        t.depends_on,
                        t.pipeline,
                        t.base,
                        t.starts_from,
                    ));
                }
            }
            rows.sort_by(|a, b| a.1.cmp(&b.1));
            rows
        };
        assert_eq!(from_index(&repo), from_files());
        assert_eq!(group_order(&repo), order_from_files(&repo));

        reset_rebuilds();
        let unreadable = with_unreadable_files(&repo, || from_index(&repo));
        assert_eq!(unreadable.len(), 3);
        assert_eq!(rebuilds(), 0, "a screen read opened task files");

        archive(
            &repo,
            "c",
            &format!("{front}depends_on: [b]\nstarts_from: task/b\n"),
        );
        assert_eq!(from_index(&repo), from_files());
        assert_eq!(rebuilds(), 0, "archiving one task opened another's file");

        assert_eq!(group_order(&repo), order_from_files(&repo));
        sweep(&repo, "a");
        assert_eq!(from_index(&repo), from_files());
        assert_eq!(group_order(&repo), order_from_files(&repo));
        lose_index(&repo);
        assert_eq!(from_index(&repo), from_files());
        assert_eq!(group_order(&repo), order_from_files(&repo));

        let groups = list_groups(&repo).unwrap();
        let task = &groups.iter().find(|g| g.name == "g").unwrap().tasks[0];
        assert!(task.doc.is_empty(), "the listing carries no task text");
        assert_eq!(
            task.text().unwrap(),
            std::fs::read_to_string(&task.path).unwrap(),
            "the text is the file's own"
        );
    }

    /// The three worked examples in the scoring rule, tested directly
    /// against the pure function rather than through a group.
    #[test]
    fn score_matches_the_three_worked_examples() {
        assert_eq!(score("ctx", "ctx-peak"), Some(120));
        assert_eq!(score("qcp", "queue-conflict-picker"), Some(170));
        assert_eq!(score("hs", "home-state"), Some(170));
    }

    /// A short query smeared across a long sentence has to fall under
    /// `MATCH_FLOOR` once the field's own weight (0 for a title) is added,
    /// not merely score lower than a tighter match.
    #[test]
    fn a_query_smeared_across_a_sentence_falls_under_the_floor() {
        let field = "Coax our nested flags into the tail, per contract, over time.";
        let s = score("conflict", field).expect("subsequence does occur");
        assert!(
            s + DESCRIPTION_WEIGHT < MATCH_FLOOR,
            "expected a smeared match under the floor, got {s}"
        );
    }

    /// No occurrence of the query as a subsequence at all — not even a bad
    /// one — is `None`, not a very negative score.
    #[test]
    fn a_query_with_no_subsequence_at_all_does_not_match() {
        assert_eq!(score("xyz", "queue-browse"), None);
    }

    /// A query longer than one character whose every letter lands on a word
    /// start scores the flat 150, regardless of how far apart those letters
    /// actually sit — the acronym case the discount in step 1 exists for.
    #[test]
    fn every_letter_on_a_word_start_scores_flat() {
        let s = score("qb", "queue-browse").unwrap();
        assert_eq!(s, 150 + 20 /* window itself starts on a word start */);
    }
}
