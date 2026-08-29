//! What the queue screen reads: task documents waiting in one flat
//! directory, gathered into the groups they name.
//!
//! spoolway has stopped knowing what a plan is. A producer — the shipped
//! `/spoolway-plan` skill, a Jira ticket, a GitHub issue, a script — writes
//! whole task documents into [`Repo::pending_dir`], and that directory is
//! the main thing the screen scans. [`Repo::queue_dir`] and
//! [`Repo::archive_dir`] are the other two: submitting a group deletes its
//! pending documents (`queue::finish_submit`), so a group already queued has
//! nothing left under `pending_dir` at all, and a task the pipeline ran to
//! the end moves out of the queue directory into the archive one
//! (`teardown`). Either way a row is built straight from whichever
//! directory still holds the task's document — see [`list_groups`]. Nothing
//! here parses a page, a card or a chip; a document is the unit, and its own
//! frontmatter is the whole of what this module reads.
//!
//! The reading is deliberately shallow. [`crate::commands::parse_submission`]
//! is what actually validates a document, once a person has chosen to queue
//! it; this only needs enough to draw two panes — which group a document
//! belongs to, what it is called, and what it says it is for. A document
//! that will be refused at submit time still lists here, and is refused
//! there, with the real error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;

/// Where a task's own document was read from, and what stage it has reached
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

/// One pending task document, as the screen shows it.
pub(crate) struct PendingTask {
    /// The file it was read from — deleted when its group is queued, and the
    /// name a submission failure reports this document by, so the two agree
    /// by construction.
    pub(crate) path: PathBuf,
    /// The document's own `id:`, which is what the queue will call this task.
    pub(crate) id: String,
    /// The document's own `title:` — the one sentence the tasks pane draws
    /// under what this task waits on. `None` for a document with no title to
    /// read, which the pane simply draws no line for rather than an empty
    /// one; `parse_submission` is what refuses it at submit time.
    pub(crate) description: Option<String>,
    /// The document itself, unread and unmodified, ready to hand to
    /// `validate_batch` exactly as a `--from` entry would.
    pub(crate) doc: String,
    /// Where this task's own id currently sits — see [`TaskState`].
    pub(crate) state: TaskState,
}

/// A group's own stage, folded from every task it holds — see
/// [`group_state`]. `Ord`ered in the order [`list_groups`] sorts by:
/// something still queueable first, then everything queued, then everything
/// done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum GroupState {
    /// At least one task's own id reads [`TaskState::Pending`] — the group
    /// is not every task queued yet, so it is the one `enter` may still
    /// submit.
    ///
    /// A pending document naming a task id the queue or the archive already
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
    /// Every task has reached [`Repo::archive_dir`]. `s` and `p` are the only
    /// doors out — see `queue::selectable`.
    Done,
}

/// One `group:` across the pending documents: the left pane's unit.
///
/// A group is what a person selects and submits, whole. Its tasks are a
/// chain — cut together, ordered by `depends_on` — and half a chain in the
/// queue is a task waiting on a dependency nobody queued.
pub(crate) struct Group {
    /// The `group:` value, verbatim. Never path-parsed and never split: two
    /// documents group together only when they name exactly the same string,
    /// the same rule `spoolway group list` reads the queue by.
    pub(crate) name: String,
    /// Its documents, dependencies before dependents — see [`in_reading_order`].
    pub(crate) tasks: Vec<PendingTask>,
    /// This group's own stage — see [`GroupState`] and [`group_state`], which
    /// is what [`list_groups`] computes this from once every task has been
    /// read.
    pub(crate) state: GroupState,
    /// The newest of its documents' own birth times, which is what
    /// [`list_groups`] sorts on: the group somebody just wrote is the one
    /// under the cursor on the first frame. `None` only when no document's
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
/// An empty group cannot happen through [`list_groups`] — a group only
/// exists because a document named it — but reads `Queueable` rather than
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

/// The order [`list_groups`] sorts by within a queued/unqueued half: newer
/// before older, and a group whose instant could not be read after every one
/// that could — a stat that failed on one file is not a reason to refuse the
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

/// A document's own birth time, falling back to its modification time where
/// the platform or filesystem has no birth time to give.
fn created_time(path: &Path) -> Option<std::time::SystemTime> {
    let meta = std::fs::metadata(path).ok()?;
    meta.created().or_else(|_| meta.modified()).ok()
}

/// One string off a document's frontmatter, without any of
/// `parse_submission`'s validation.
///
/// The screen has to draw a document before anyone has decided to submit it,
/// so a malformed one must come back as "nothing to show" rather than an
/// error that would take the whole listing down with it. Every field below
/// is read this way for that reason. `pub(crate)` rather than private: a
/// routines folder is read the same shallow way — see
/// `super::routines::read_task`.
pub(crate) fn front_str(yaml: &serde_norway::Value, key: &str) -> Option<String> {
    let text = yaml.as_mapping()?.get(key)?.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Every `.md` file directly inside `dir`, filename order — the listing
/// [`list_groups`] runs the same way over all three of its own sources.
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
/// A document with no readable `group:` is skipped, not shown: there is no
/// row for it to be one of, since a row *is* a `group:` value. It is not
/// lost — `queue add --from <pending dir>` still reads the whole directory
/// and refuses that document by name, with the real reason.
///
/// No "does not exist yet" case to handle: [`Repo::pending_dir`],
/// [`Repo::queue_dir`] and [`Repo::archive_dir`] all create their directory
/// silently the moment they are asked for, so a fresh project that has
/// planned nothing still gets a real, empty directory to read.
pub(crate) fn list_groups(repo: &Repo) -> Result<Vec<Group>> {
    let dir = repo.pending_dir();
    let queue_dir = repo.queue_dir();
    let archive_dir = repo.archive_dir();

    // Grouped by `group:` verbatim, in a map ordered by that string, so a
    // group's own identity never depends on which document happened to be
    // read first.
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    // Every id read out of the pending directory below, plus every one read
    // out of the queue directory after it. The archive loop, last, skips any
    // id already in here: that task already spoke for itself in whichever of
    // the first two loops read it.
    let mut spoken_for: std::collections::BTreeSet<String> = Default::default();

    for path in md_files(&dir)? {
        let doc = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let Ok((yaml, _)) = crate::task::split_fence(&doc) else {
            continue;
        };
        let Ok(front) = serde_norway::from_str::<serde_norway::Value>(yaml) else {
            continue;
        };
        let Some(group) = front_str(&front, "group") else {
            continue;
        };
        let Some(id) = front_str(&front, "id") else {
            continue;
        };

        // A document still sitting in `pending/` ordinarily has no stamp
        // anywhere else — but a re-run of a producer over work already
        // submitted names an id the queue, or even the archive, already
        // holds, and that document's own state has to say so rather than
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
            path,
            id,
            doc,
            state,
        });
    }

    // A second source: documents that live only in the queue directory.
    // Submitting a group deletes its pending documents in the same act that
    // writes its queue ones (`queue::finish_submit`), so a group already
    // fully queued has nothing left under `dir` at all — this is the only
    // way such a group still gets a row, built from its queue documents and
    // marked `Queued` by construction: nothing here reads as anything else.
    for path in md_files(&queue_dir)? {
        // The queue file's own name is what `queue_stamp` above already keys
        // on, so this uses the same identity rather than trusting the
        // document's own `id:` to agree with the name it was saved under.
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if spoken_for.contains(id) {
            continue;
        }
        let id = id.to_string();

        let Ok(doc) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok((yaml, _)) = crate::task::split_fence(&doc) else {
            continue;
        };
        let Ok(front) = serde_norway::from_str::<serde_norway::Value>(yaml) else {
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
            path,
            id,
            doc,
            state: TaskState::Queued,
        });
    }

    // A third source: documents the pipeline already ran to the end. Read
    // last and skipped for any id the first two loops already claimed, so a
    // group half archived and half still queued lists its whole chain rather
    // than only the half still in the queue — the gap this whole feature
    // exists to close.
    for path in md_files(&archive_dir)? {
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if spoken_for.contains(id) {
            continue;
        }
        let id = id.to_string();

        let Ok(doc) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok((yaml, _)) = crate::task::split_fence(&doc) else {
            continue;
        };
        let Ok(front) = serde_norway::from_str::<serde_norway::Value>(yaml) else {
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
        entry.tasks.push(PendingTask {
            description: front_str(&front, "title"),
            path,
            id,
            doc,
            state: TaskState::Done,
        });
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

    Ok(groups)
}

/// The `.md` files in the pending directory that [`list_groups`] could not
/// file under any group: no fence, no readable YAML, or no `group:` at all.
///
/// Read separately rather than carried back out of `list_groups`, because
/// there is exactly one caller and one moment it matters — `queue::
/// opening_message`, and only when there is no group a person can actually
/// queue: every one already `queued`, which is also true of an empty list.
/// A person is otherwise looking at a pane with nothing selectable in it and
/// a file sitting right there in the directory, unaccounted for. Whenever a
/// real, queueable group exists too, the document is one row's worth of
/// nothing among rows that do exist, and `queue add --from <pending dir>` is
/// what names it and says why.
pub(crate) fn unreadable(repo: &Repo) -> Vec<PathBuf> {
    let dir = repo.pending_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut skipped: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
        .filter(|path| {
            let Ok(doc) = std::fs::read_to_string(path) else {
                return true;
            };
            let readable = crate::task::split_fence(&doc)
                .ok()
                .and_then(|(yaml, _)| serde_norway::from_str::<serde_norway::Value>(yaml).ok())
                .and_then(|front| front_str(&front, "group"));
            readable.is_none()
        })
        .collect();
    skipped.sort();
    skipped
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
/// document's name is its task id, and ids are not written to sort.
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
                depends_on(&task.doc)
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

/// A peek at a document's own `depends_on` list, unvalidated — what the
/// tasks pane's `waits on` line draws, and what [`in_reading_order`] sorts
/// by. The screen only has to show what a document claims;
/// `parse_submission` is what checks the claim.
///
/// A document's `touches` is deliberately not read back: the globs are the
/// widest thing it carries and no pane shows them.
pub(crate) fn depends_on(doc: &str) -> Vec<String> {
    let Ok((yaml, _)) = crate::task::split_fence(doc) else {
        return Vec::new();
    };
    let Ok(value) = serde_norway::from_str::<serde_norway::Value>(yaml) else {
        return Vec::new();
    };
    value
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

    /// A whole task document, in the shape `--from` accepts.
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
        let repo = crate::commands::testutil::fixture("pending-none");
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
    /// many documents named it, and each row carrying every one of them.
    #[test]
    fn documents_gather_into_one_row_per_distinct_group() {
        let repo = crate::commands::testutil::fixture("pending-groups");
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
        let repo = crate::commands::testutil::fixture("pending-verbatim");
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
        let repo = crate::commands::testutil::fixture("pending-order");
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
        let repo = crate::commands::testutil::fixture("pending-order-outside");
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
        let repo = crate::commands::testutil::fixture("pending-cycle");
        write(&repo, "a.md", &doc("a", "loop", "depends_on: [b]\n"));
        write(&repo, "b.md", &doc("b", "loop", "depends_on: [a]\n"));

        assert_eq!(list_groups(&repo).unwrap()[0].tasks.len(), 2);
    }

    /// A document that names no `group:`, or whose frontmatter will not
    /// parse at all, has no row to be one of — a row *is* a `group:` value.
    /// It is skipped rather than shown, and `queue add --from` is what
    /// refuses it by name. [`unreadable`] is what finds the same set again,
    /// for the one case the screen has to say so out loud.
    #[test]
    fn a_document_with_no_readable_group_is_skipped() {
        let repo = crate::commands::testutil::fixture("pending-ungrouped");
        write(&repo, "good.md", &doc("good", "issue-42", ""));
        std::fs::write(
            repo.pending_dir().join("no-group.md"),
            "---\nid: stray\ntitle: stray\n---\n## Goal\n\nx\n",
        )
        .unwrap();
        std::fs::write(repo.pending_dir().join("garbage.md"), "not a document\n").unwrap();

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "issue-42");

        let skipped = unreadable(&repo);
        let names: Vec<String> = skipped
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["garbage.md", "no-group.md"], "{names:?}");
    }

    /// The screen only says anything about skipped documents when there is
    /// nothing else to list, so `unreadable` must come back empty on a
    /// directory where every document reads — otherwise the ordinary case
    /// would be paying for a listing nobody looks at.
    #[test]
    fn unreadable_is_empty_when_every_document_reads() {
        let repo = crate::commands::testutil::fixture("pending-all-readable");
        write(&repo, "a.md", &doc("a", "issue-42", ""));
        write(&repo, "b.md", &doc("b", "issue-42", ""));

        assert!(unreadable(&repo).is_empty());
    }

    /// Only `.md` is read. Anything else in the directory is not a task
    /// document, and above all no page is scanned for one.
    #[test]
    fn only_markdown_documents_are_read() {
        let repo = crate::commands::testutil::fixture("pending-md-only");
        write(&repo, "a.md", &doc("a", "issue-42", ""));
        std::fs::write(repo.pending_dir().join("a.html"), "<html></html>").unwrap();

        let groups = list_groups(&repo).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tasks.len(), 1);
    }

    /// A group every one of whose tasks the queue already holds cannot be
    /// queued again — `validate_batch` would refuse it — so it is marked and
    /// sorted behind the groups that can be, the same way `h` hides it.
    #[test]
    fn a_group_already_in_the_queue_is_marked_and_sorted_last() {
        let repo = crate::commands::testutil::fixture("pending-already-queued");
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
        let repo = crate::commands::testutil::fixture("pending-part-queued");
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

    /// Submitting a group deletes its pending documents (`finish_submit`),
    /// so a group already queued in full has nothing left under the pending
    /// directory at all — this is the one case that still has to produce a
    /// row, built entirely from the queue directory instead.
    #[test]
    fn a_group_with_no_pending_documents_still_lists_from_the_queue() {
        let repo = crate::commands::testutil::fixture("pending-queue-only");
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
    /// documents the same way a wholly-queued group is built from its queue
    /// ones — the third source this whole feature adds.
    #[test]
    fn a_group_wholly_in_the_archive_still_lists() {
        let repo = crate::commands::testutil::fixture("pending-archive-only");
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
        let repo = crate::commands::testutil::fixture("pending-mixed-states");
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

    /// A pending document naming an id the archive already holds — a re-run
    /// of a producer over work already finished — reads that task as `Done`,
    /// not `Pending`: `validate_batch` would refuse queueing it again either
    /// way, and the group must not read as still-queueable over a task that
    /// is not.
    #[test]
    fn a_pending_document_already_archived_reads_as_done() {
        let repo = crate::commands::testutil::fixture("pending-already-archived");
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

    /// The tasks pane draws a document's own `title:` under what it waits
    /// on, and `waits on` reads `depends_on` off the same document without
    /// validating it.
    #[test]
    fn a_task_carries_its_title_and_its_waits_on() {
        let repo = crate::commands::testutil::fixture("pending-fields");
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

    /// A document carrying no `depends_on` — or no fence at all — reads as
    /// an empty list rather than an error: the pane shows "waits on nothing"
    /// for the first and never panics on either.
    #[test]
    fn depends_on_defaults_to_an_empty_list() {
        assert_eq!(depends_on(&doc("a", "g", "")), Vec::<String>::new());
        assert_eq!(depends_on("not a document"), Vec::<String>::new());
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
