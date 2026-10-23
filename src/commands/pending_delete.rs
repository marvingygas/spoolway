//! The rules behind deleting pending work from the queue screen: which
//! files a delete takes, and how they are removed.
//!
//! The pending directory sits under the project home, outside git, so a
//! removed file cannot be brought back. A task whose `depends_on` names a
//! deleted task can never be queued either, because queueing refuses an
//! unknown dependency. So a delete takes its target and every pending task
//! that depends on it, in any group, and nothing else: the tasks a target
//! depends on stay, and queued or done tasks are never touched.
//!
//! Nothing here draws or reads keys. [`delete_set`] works out what a delete
//! would take, for the confirm popup, and [`delete_pending`] removes it
//! after checking that the pending directory still says the same thing.
//!
//! A plan row on the same screen is deleted here too, by [`delete_plan`]:
//! its one page under `plans/`, and only while no task names that page.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::pending::{
    Group, PendingTask, TaskState, list_groups_and_skipped, plans_named_by_other_clones,
};
use super::*;

/// What a delete was asked to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeleteTarget {
    /// A whole group, by its `group:` name.
    Group(String),
    /// One task, by the path of its file.
    Task(PathBuf),
}

/// One task in a delete set, as the confirm popup lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeleteEntry {
    pub(crate) path: PathBuf,
    pub(crate) id: String,
    /// The ids in this task's `depends_on` that are also in the set.
    pub(crate) depends_on: Vec<String>,
}

/// The part of a delete set that sits in one group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeleteGroup {
    pub(crate) name: String,
    pub(crate) tasks: Vec<DeleteEntry>,
}

/// How [`delete_pending`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeleteOutcome {
    Removed,
    Changed,
    Failed {
        /// What went wrong, and nothing about what to do next: the popup
        /// that shows it ends on the remedy, chosen by `kind`.
        error: String,
        /// The kind of the error removing a file gave, so the popup can
        /// name the remedy for it. `None` when no file removal failed: the
        /// re-read of the directory, or a path refused before removing.
        kind: Option<std::io::ErrorKind>,
        removed: Vec<String>,
        left: Vec<String>,
    },
}

/// The tasks of `group` that are still files in the pending directory.
fn pending(group: &Group) -> Vec<&PendingTask> {
    group
        .tasks
        .iter()
        .filter(|t| t.state == TaskState::Pending)
        .collect()
}

/// Every pending task a delete of `target` takes, grouped for display.
///
/// `groups` is every group the screen read, never the filtered list it
/// draws: a dependent in a group the filter or hide scope hides still loses
/// its dependency, so the walk has to reach it.
///
/// The target's own group comes first, then the others in the order given.
/// Inside a group, tasks keep that group's reading order. A target that is
/// not a pending task, or a group with no pending task, gives an empty set.
pub(crate) fn delete_set(groups: &[Group], target: &DeleteTarget) -> Vec<DeleteGroup> {
    let all: Vec<&PendingTask> = groups.iter().flat_map(pending).collect();

    let (mut taken, first): (BTreeSet<&Path>, Option<&str>) = match target {
        DeleteTarget::Task(path) => (
            all.iter()
                .filter(|t| t.path == *path)
                .map(|t| t.path.as_path())
                .collect(),
            groups
                .iter()
                .find(|g| pending(g).iter().any(|t| t.path == *path))
                .map(|g| g.name.as_str()),
        ),
        DeleteTarget::Group(name) => (
            groups
                .iter()
                .filter(|g| g.name == *name)
                .flat_map(pending)
                .map(|t| t.path.as_path())
                .collect(),
            Some(name.as_str()),
        ),
    };

    // Breadth first over dependents. `taken` doubles as the visited set, so
    // a `depends_on` loop ends the walk instead of feeding it forever.
    let mut frontier: Vec<&PendingTask> = all
        .iter()
        .filter(|t| taken.contains(t.path.as_path()))
        .copied()
        .collect();
    while !frontier.is_empty() {
        let ids: BTreeSet<&str> = frontier.iter().map(|t| t.id.as_str()).collect();
        frontier = all
            .iter()
            .filter(|t| !taken.contains(t.path.as_path()))
            .filter(|t| t.depends_on.iter().any(|dep| ids.contains(dep.as_str())))
            .copied()
            .collect();
        taken.extend(frontier.iter().map(|t| t.path.as_path()));
    }

    let in_set: BTreeSet<&str> = all
        .iter()
        .filter(|t| taken.contains(t.path.as_path()))
        .map(|t| t.id.as_str())
        .collect();
    let mut ordered: Vec<&Group> = groups.iter().collect();
    // Stable, so the other groups keep the screen's order.
    ordered.sort_by_key(|g| Some(g.name.as_str()) != first);
    ordered
        .into_iter()
        .filter_map(|group| {
            let tasks: Vec<DeleteEntry> = pending(group)
                .into_iter()
                .filter(|t| taken.contains(t.path.as_path()))
                .map(|t| DeleteEntry {
                    path: t.path.clone(),
                    id: t.id.clone(),
                    depends_on: t
                        .depends_on
                        .iter()
                        .filter(|dep| in_set.contains(dep.as_str()))
                        .cloned()
                        .collect(),
                })
                .collect();
            (!tasks.is_empty()).then(|| DeleteGroup {
                name: group.name.clone(),
                tasks,
            })
        })
        .collect()
}

/// The set's files, every dependent before any task it depends on.
///
/// If removal stops part way, a task left behind still has its dependencies.
/// Repeatedly takes a task that nobody remaining depends on. That
/// eligibility check is what keeps dependents first; nothing else does.
/// Among the eligible tasks it takes the last in display order, so a
/// dependent in a later group goes before one in the target's own group,
/// which is listed first. In a loop every task has a dependent, so the
/// first one remaining goes, which keeps the order stable instead of
/// looping forever.
fn removal_order(set: &[DeleteGroup]) -> Vec<&DeleteEntry> {
    let mut rest: Vec<&DeleteEntry> = set.iter().flat_map(|g| g.tasks.iter()).collect();
    let mut order = Vec::with_capacity(rest.len());
    while !rest.is_empty() {
        let next = rest
            .iter()
            .rposition(|t| {
                !rest
                    .iter()
                    .any(|other| other.path != t.path && other.depends_on.contains(&t.id))
            })
            .unwrap_or(0);
        order.push(rest.remove(next));
    }
    order
}

/// A file name for the popup's removed and left lists.
fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into(),
    )
}

/// Removes `order` one file at a time through `remove`, stopping at the
/// first failure. A file already gone counts as removed, the same as
/// `delete_routine` treats a missing folder.
///
/// Refuses, before removing anything, any path that is not directly inside
/// `pending_dir`: a queued or archived file must never go this way.
fn remove_in_order(
    order: &[&DeleteEntry],
    pending_dir: &Path,
    remove: impl Fn(&Path) -> std::io::Result<()>,
) -> DeleteOutcome {
    let names: Vec<String> = order.iter().map(|t| file_name(&t.path)).collect();
    if let Some(bad) = order.iter().find(|t| t.path.parent() != Some(pending_dir)) {
        return DeleteOutcome::Failed {
            error: format!(
                "{} is not in the pending directory, so nothing was removed",
                bad.path.display()
            ),
            kind: None,
            removed: Vec::new(),
            left: names,
        };
    }
    for (done, task) in order.iter().enumerate() {
        match remove(&task.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return DeleteOutcome::Failed {
                    error: format!("{}: {err}", names[done]),
                    kind: Some(err.kind()),
                    removed: names[..done].to_vec(),
                    left: names[done..].to_vec(),
                };
            }
        }
    }
    DeleteOutcome::Removed
}

/// Removes what `shown` listed for `target`, if the pending directory still
/// says the same thing.
///
/// Reads the directory again and works the set out again. If the ids differ
/// from `shown`, a task was written or removed since the popup opened, and
/// removing now could leave a task depending on a deleted file, so nothing
/// is removed and the answer is [`DeleteOutcome::Changed`].
pub(crate) fn delete_pending(
    repo: &Repo,
    target: &DeleteTarget,
    shown: &[DeleteGroup],
) -> DeleteOutcome {
    let (groups, _) = match list_groups_and_skipped(repo) {
        Ok(read) => read,
        Err(err) => {
            return DeleteOutcome::Failed {
                error: format!("{err:#}"),
                kind: None,
                removed: Vec::new(),
                left: removal_order(shown)
                    .iter()
                    .map(|t| file_name(&t.path))
                    .collect(),
            };
        }
    };
    let now = delete_set(&groups, target);
    let ids = |set: &[DeleteGroup]| -> BTreeSet<String> {
        set.iter()
            .flat_map(|g| g.tasks.iter().map(|t| t.id.clone()))
            .collect()
    };
    if ids(&now) != ids(shown) {
        return DeleteOutcome::Changed;
    }
    remove_in_order(&removal_order(&now), &repo.pending_dir(), |path| {
        std::fs::remove_file(path)
    })
}

/// How [`delete_plan`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanDeleteOutcome {
    Removed,
    /// A task names the page in `plan:` now, so it was left alone.
    Named,
    Failed {
        /// What went wrong, with no remedy: the popup showing it adds one,
        /// chosen by `kind`.
        error: String,
        /// The kind of the error removing the page gave. `None` when the
        /// removal was never tried: a read of the task folders that failed,
        /// or a path that is not a plan page.
        kind: Option<std::io::ErrorKind>,
    },
}

/// Removes the plan page at `page`, if it is still a plan no task names.
///
/// The pending, queue and archive folders are read again first, by the
/// rule the queue screen lists plans by, since a task cut from the plan
/// after its row was drawn would be left linking to a page that is gone.
/// In home mode that is every clone's folders, since the page is the
/// workspace's: one clone must not remove a page another clone's tasks link
/// to. A page some task now names answers [`PlanDeleteOutcome::Named`] with
/// nothing removed, and a folder that will not read answers
/// [`PlanDeleteOutcome::Failed`] naming it, with nothing removed either.
///
/// Only an `.html` file directly inside `plans/` is ever removed, never a
/// folder there: those hold mockups that tasks link to. A page already gone
/// counts as removed, the same as [`delete_pending`] treats a missing file.
pub(crate) fn delete_plan(repo: &Repo, page: &Path) -> PlanDeleteOutcome {
    let plans = repo.plans_dir();
    if page.parent() != Some(plans.as_path())
        || page.extension().and_then(|e| e.to_str()) != Some("html")
    {
        return PlanDeleteOutcome::Failed {
            error: format!(
                "{} is not a plan page in {}, so nothing was removed",
                page.display(),
                plans.display()
            ),
            kind: None,
        };
    }
    let groups = match list_groups_and_skipped(repo) {
        Ok((groups, _)) => groups,
        Err(err) => {
            return PlanDeleteOutcome::Failed {
                error: format!("{err:#}"),
                kind: None,
            };
        }
    };
    let elsewhere = match plans_named_by_other_clones(repo) {
        Ok(named) => named,
        Err(err) => {
            return PlanDeleteOutcome::Failed {
                error: format!("{err:#}"),
                kind: None,
            };
        }
    };
    let path = page.to_string_lossy();
    if elsewhere.contains(path.as_ref())
        || groups
            .iter()
            .flat_map(|group| &group.tasks)
            .any(|task| task.plan.as_deref() == Some(path.as_ref()))
    {
        return PlanDeleteOutcome::Named;
    }
    match std::fs::remove_file(page) {
        Ok(()) => PlanDeleteOutcome::Removed,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => PlanDeleteOutcome::Removed,
        Err(err) => PlanDeleteOutcome::Failed {
            error: err.to_string(),
            kind: Some(err.kind()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::pending::list_groups_and_skipped;
    use super::*;

    fn doc(id: &str, group: &str, extra: &str) -> String {
        format!(
            "---\nid: {id}\ntitle: {id}, done\ngroup: {group}\n{extra}---\n\
             ## Goal\n\nDo the thing.\n"
        )
    }

    fn write(repo: &Repo, id: &str, group: &str, deps: &[&str]) -> PathBuf {
        let extra = if deps.is_empty() {
            String::new()
        } else {
            format!("depends_on: [{}]\n", deps.join(", "))
        };
        let path = repo.pending_dir().join(format!("{id}.md"));
        std::fs::write(&path, doc(id, group, &extra)).unwrap();
        path
    }

    fn groups(repo: &Repo) -> Vec<Group> {
        list_groups_and_skipped(repo).unwrap().0
    }

    fn ids(set: &[DeleteGroup]) -> Vec<(String, Vec<String>)> {
        set.iter()
            .map(|g| {
                (
                    g.name.clone(),
                    g.tasks.iter().map(|t| t.id.clone()).collect(),
                )
            })
            .collect()
    }

    fn flat(set: &[DeleteGroup]) -> Vec<String> {
        let mut all: Vec<String> = set
            .iter()
            .flat_map(|g| g.tasks.iter().map(|t| t.id.clone()))
            .collect();
        all.sort();
        all
    }

    fn strs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    /// A cart: `cart-discounts` (same group) and
    /// `checkout-total` (another group) both depend on `cart-totals`, which
    /// itself depends on `cart-model` — a task the delete must leave alone.
    fn cart(repo: &Repo) -> PathBuf {
        write(repo, "cart-model", "cart", &[]);
        let totals = write(repo, "cart-totals", "cart", &["cart-model"]);
        write(repo, "cart-discounts", "cart", &["cart-totals"]);
        write(repo, "checkout-total", "checkout", &["cart-totals"]);
        write(repo, "unrelated", "checkout", &[]);
        totals
    }

    #[test]
    fn a_task_target_takes_its_dependents_in_any_group_and_never_its_dependencies() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-task");
        let totals = cart(&repo);

        let set = delete_set(&groups(&repo), &DeleteTarget::Task(totals));

        assert_eq!(
            flat(&set),
            strs(&["cart-discounts", "cart-totals", "checkout-total"])
        );
    }

    #[test]
    fn dependents_are_followed_any_number_of_hops() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-hops");
        let a = write(&repo, "a", "g1", &[]);
        write(&repo, "b", "g2", &["a"]);
        write(&repo, "c", "g3", &["b"]);
        write(&repo, "d", "g1", &["c"]);

        let set = delete_set(&groups(&repo), &DeleteTarget::Task(a));

        assert_eq!(flat(&set), strs(&["a", "b", "c", "d"]));
    }

    #[test]
    fn a_group_target_takes_its_pending_tasks_and_their_dependents_elsewhere() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-group");
        cart(&repo);

        let set = delete_set(&groups(&repo), &DeleteTarget::Group("cart".into()));

        assert_eq!(
            flat(&set),
            strs(&[
                "cart-discounts",
                "cart-model",
                "cart-totals",
                "checkout-total"
            ])
        );
    }

    #[test]
    fn a_group_target_leaves_queued_and_done_tasks_out() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-states");
        write(&repo, "pend", "mixed", &[]);
        write(&repo, "queued", "mixed", &[]);
        write(&repo, "archived", "mixed", &[]);
        // An id the queue or archive already holds reads as that state.
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(repo.queue_dir().join("queued.md"), "x").unwrap();
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(repo.archive_dir().join("archived.md"), "x").unwrap();

        let set = delete_set(&groups(&repo), &DeleteTarget::Group("mixed".into()));

        assert_eq!(flat(&set), strs(&["pend"]));
    }

    #[test]
    fn a_group_with_no_pending_task_gives_an_empty_set() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-empty");
        write(&repo, "queued", "sent", &[]);
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(repo.queue_dir().join("queued.md"), "x").unwrap();

        let set = delete_set(&groups(&repo), &DeleteTarget::Group("sent".into()));
        assert!(set.is_empty(), "{set:?}");
        let none = delete_set(&groups(&repo), &DeleteTarget::Group("absent".into()));
        assert!(none.is_empty(), "{none:?}");
    }

    #[test]
    fn the_listing_puts_the_targets_group_first_and_keeps_reading_order() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-order");
        // `cart-discounts` sorts before `cart-totals` by name, but reads after it.
        let totals = cart(&repo);
        let all = groups(&repo);

        let set = delete_set(&all, &DeleteTarget::Task(totals));
        assert_eq!(
            ids(&set),
            vec![
                ("cart".to_string(), strs(&["cart-totals", "cart-discounts"])),
                ("checkout".to_string(), strs(&["checkout-total"])),
            ]
        );

        // Targeting from the other side puts that group first instead.
        let checkout = write(&repo, "checkout-root", "checkout", &[]);
        write(&repo, "cart-late", "cart", &["checkout-root"]);
        let set = delete_set(&groups(&repo), &DeleteTarget::Task(checkout));
        assert_eq!(set[0].name, "checkout");
        assert_eq!(set[1].name, "cart");
    }

    #[test]
    fn the_other_groups_follow_the_screens_order() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-screen-order");
        // The target's group sorts last by name, so both the screen's
        // newest-first order and its name-order fallback put it last
        // unless the set moves it to the front.
        let root = write(&repo, "root", "zz", &[]);
        write(&repo, "x", "aa", &["root"]);
        write(&repo, "y", "bb", &["root"]);
        let all = groups(&repo);
        let screen: Vec<String> = all.iter().map(|g| g.name.clone()).collect();

        let set = delete_set(&all, &DeleteTarget::Task(root));

        let mut expect: Vec<String> = vec!["zz".into()];
        expect.extend(screen.into_iter().filter(|n| n != "zz"));
        let got: Vec<String> = set.iter().map(|g| g.name.clone()).collect();
        assert_eq!(got, expect);
    }

    #[test]
    fn each_entry_names_only_the_dependencies_that_are_in_the_set() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-notes");
        let totals = cart(&repo);

        let set = delete_set(&groups(&repo), &DeleteTarget::Task(totals));

        let note = |id: &str| -> Vec<String> {
            set.iter()
                .flat_map(|g| g.tasks.iter())
                .find(|t| t.id == id)
                .unwrap()
                .depends_on
                .clone()
        };
        // `cart-model` is a dependency outside the set, so it is not named.
        assert!(note("cart-totals").is_empty());
        assert_eq!(note("cart-discounts"), strs(&["cart-totals"]));
        assert_eq!(note("checkout-total"), strs(&["cart-totals"]));
    }

    /// The screen's filter and hide scope shorten the list it draws, but
    /// the walk is handed every group, so a dependent in a hidden group is
    /// still found.
    #[test]
    fn a_dependent_in_a_group_the_screen_would_hide_is_still_found() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-hidden");
        let root = write(&repo, "root", "shown", &[]);
        write(&repo, "hidden-child", "hidden-by-filter", &["root"]);

        let set = delete_set(&groups(&repo), &DeleteTarget::Task(root));

        assert_eq!(flat(&set), strs(&["hidden-child", "root"]));
    }

    #[test]
    fn a_dependency_loop_ends_and_takes_both_tasks() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-loop");
        let a = write(&repo, "a", "loop", &["b"]);
        write(&repo, "b", "loop", &["a"]);

        let all = groups(&repo);
        let set = delete_set(&all, &DeleteTarget::Task(a.clone()));
        assert_eq!(flat(&set), strs(&["a", "b"]));

        let outcome = delete_pending(&repo, &DeleteTarget::Task(a), &set);
        assert_eq!(outcome, DeleteOutcome::Removed);
        assert!(groups(&repo).is_empty());
    }

    #[test]
    fn removal_deletes_the_set_and_nothing_else() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-remove");
        let totals = cart(&repo);
        let target = DeleteTarget::Task(totals);
        let shown = delete_set(&groups(&repo), &target);

        assert_eq!(
            delete_pending(&repo, &target, &shown),
            DeleteOutcome::Removed
        );

        let left: Vec<String> = groups(&repo)
            .iter()
            .flat_map(|g| g.tasks.iter().map(|t| t.id.clone()))
            .collect();
        let mut left = left;
        left.sort();
        assert_eq!(left, strs(&["cart-model", "unrelated"]));
    }

    #[test]
    fn removal_answers_changed_and_removes_nothing_when_a_dependent_appeared() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-changed");
        let totals = cart(&repo);
        let target = DeleteTarget::Task(totals);
        let shown = delete_set(&groups(&repo), &target);
        write(&repo, "late-arrival", "cart", &["cart-discounts"]);

        assert_eq!(
            delete_pending(&repo, &target, &shown),
            DeleteOutcome::Changed
        );
        assert_eq!(flat(&delete_set(&groups(&repo), &target)).len(), 4);
        assert!(repo.pending_dir().join("cart-totals.md").exists());
    }

    #[test]
    fn removal_answers_changed_when_a_shown_task_vanished() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-vanished");
        let totals = cart(&repo);
        let target = DeleteTarget::Task(totals);
        let shown = delete_set(&groups(&repo), &target);
        std::fs::remove_file(repo.pending_dir().join("checkout-total.md")).unwrap();

        assert_eq!(
            delete_pending(&repo, &target, &shown),
            DeleteOutcome::Changed
        );
        assert!(repo.pending_dir().join("cart-discounts.md").exists());
    }

    /// A pending directory that cannot be read is not "changed": nothing was
    /// removed, and the error is the read's own, naming the directory. It
    /// carries no remedy of its own, since the popup showing it adds one,
    /// and never `spoolway doctor`, which does not look at the pending
    /// directory.
    #[test]
    fn an_unreadable_pending_directory_removes_nothing_and_names_the_error() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-unreadable");
        let a = write(&repo, "a", "solo", &[]);
        let target = DeleteTarget::Task(a);
        let shown = delete_set(&groups(&repo), &target);
        // `pending_dir` recreates a missing directory, so take the path once.
        let dir = repo.pending_dir();
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::write(&dir, "not a directory").unwrap();

        let outcome = delete_pending(&repo, &target, &shown);

        let DeleteOutcome::Failed {
            error,
            removed,
            left,
            ..
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert!(removed.is_empty());
        assert_eq!(left, strs(&["a.md"]));
        assert!(error.contains(&dir.display().to_string()), "{error}");
        assert!(!error.contains("pick delete again"), "{error}");
        assert!(!error.contains("doctor"), "{error}");
    }

    /// A plan page in `plans/`, as the queue screen lists one.
    fn plan(repo: &Repo, file: &str) -> PathBuf {
        let path = repo.plans_dir().join(file);
        std::fs::write(&path, "<html></html>").unwrap();
        path
    }

    #[test]
    fn a_plan_no_task_names_is_removed_and_nothing_else_is() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-plan");
        let page = plan(&repo, "2026-10-21-search.html");
        let other = plan(&repo, "2026-10-20-onboarding.html");
        let mockups = repo.plans_dir().join("2026-10-21-search");
        std::fs::create_dir(&mockups).unwrap();

        assert_eq!(delete_plan(&repo, &page), PlanDeleteOutcome::Removed);

        assert!(!page.exists());
        assert!(other.exists());
        assert!(mockups.is_dir(), "a folder under plans/ is never touched");
        // Gone already: the disk says what the delete was asked for.
        assert_eq!(delete_plan(&repo, &page), PlanDeleteOutcome::Removed);
    }

    #[test]
    fn a_plan_a_task_names_now_is_left_alone() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-plan-named");
        let page = plan(&repo, "2026-10-21-search.html");
        let extra = format!("plan: {}\n", page.display());
        std::fs::write(
            repo.pending_dir().join("search-index.md"),
            doc("search-index", "search", &extra),
        )
        .unwrap();

        assert_eq!(delete_plan(&repo, &page), PlanDeleteOutcome::Named);
        assert!(page.exists());
    }

    /// In a workspace, a task in another clone naming the page keeps it:
    /// `shop-2` cannot delete what `shop`'s task links to.
    #[test]
    fn a_plan_another_clones_task_names_is_left_alone() {
        let (shop, shop_2, _root) = crate::commands::testutil::two_clones("pdel-plan-clones");
        let page = plan(&shop, "2026-10-21-search.html");
        let extra = format!("plan: {}\n", page.display());
        std::fs::write(
            shop.pending_dir().join("search-index.md"),
            doc("search-index", "search", &extra),
        )
        .unwrap();

        assert_eq!(delete_plan(&shop_2, &page), PlanDeleteOutcome::Named);
        assert!(page.exists());
    }

    /// Another clone's task folder that will not list removes nothing, and
    /// the error names that folder.
    #[test]
    fn another_clones_unreadable_folder_removes_no_plan() {
        let (shop, shop_2, _root) =
            crate::commands::testutil::two_clones("pdel-plan-clones-broken");
        let page = plan(&shop, "2026-10-21-search.html");
        let queue = shop.home.join(crate::config::QUEUE_DIR);
        // A file where the folder should be, which `read_dir` fails on.
        std::fs::write(&queue, "").unwrap();

        let outcome = delete_plan(&shop_2, &page);
        let PlanDeleteOutcome::Failed { error, kind } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(kind, None);
        assert!(error.contains(&queue.display().to_string()), "{error}");
        assert!(page.exists());
    }

    /// Only an `.html` file directly in `plans/` goes: another kind of file,
    /// or a page anywhere else, is refused before anything is removed. A
    /// folder named like a page passes that check, but fails to remove and
    /// stays.
    #[test]
    fn anything_but_a_page_in_plans_is_refused() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-plan-refused");
        let folder = repo.plans_dir().join("mockups.html");
        std::fs::create_dir(&folder).unwrap();
        let note = repo.plans_dir().join("notes.md");
        std::fs::write(&note, "x").unwrap();
        let task = write(&repo, "wire", "cart", &[]);

        let outcome = delete_plan(&repo, &note);
        let PlanDeleteOutcome::Failed { error, kind } = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(kind, None);
        assert_eq!(
            error,
            format!(
                "{} is not a plan page in {}, so nothing was removed",
                note.display(),
                repo.plans_dir().display()
            )
        );
        assert!(matches!(
            delete_plan(&repo, &task),
            PlanDeleteOutcome::Failed { kind: None, .. }
        ));
        assert!(note.exists());
        assert!(task.exists());

        // Named like a page, but a folder: removing it fails, and it stays.
        let outcome = delete_plan(&repo, &folder);
        assert!(
            matches!(outcome, PlanDeleteOutcome::Failed { kind: Some(_), .. }),
            "{outcome:?}"
        );
        assert!(folder.is_dir());
    }

    fn entry(dir: &Path, id: &str, deps: &[&str]) -> DeleteEntry {
        DeleteEntry {
            path: dir.join(format!("{id}.md")),
            id: id.into(),
            depends_on: strs(deps),
        }
    }

    /// One group holding a chain `root` <- `mid` <- `leaf`, listed in
    /// reading order, as the popup would show it.
    fn chain_set(dir: &Path) -> Vec<DeleteGroup> {
        vec![DeleteGroup {
            name: "chain".into(),
            tasks: vec![
                entry(dir, "root", &[]),
                entry(dir, "mid", &["root"]),
                entry(dir, "leaf", &["mid"]),
            ],
        }]
    }

    #[test]
    fn every_dependent_is_removed_before_what_it_depends_on() {
        let dir = PathBuf::from("/p");
        let set = chain_set(&dir);
        let order: Vec<&str> = removal_order(&set).iter().map(|t| t.id.as_str()).collect();
        assert_eq!(order, vec!["leaf", "mid", "root"]);
    }

    /// A dependent in a later group goes before one in the target's own
    /// group, though the target's group is listed first.
    #[test]
    fn a_cross_group_chain_is_removed_from_the_later_group_first() {
        let (repo, _guard) = crate::commands::testutil::fixture("pdel-cross-order");
        let totals = cart(&repo);
        let set = delete_set(&groups(&repo), &DeleteTarget::Task(totals));

        let order: Vec<String> = removal_order(&set)
            .iter()
            .map(|t| file_name(&t.path))
            .collect();

        assert_eq!(
            order,
            strs(&["checkout-total.md", "cart-discounts.md", "cart-totals.md"])
        );
    }

    #[test]
    fn a_loop_still_has_a_removal_order() {
        let dir = PathBuf::from("/p");
        let set = vec![DeleteGroup {
            name: "loop".into(),
            tasks: vec![entry(&dir, "a", &["b"]), entry(&dir, "b", &["a"])],
        }];
        let order: Vec<&str> = removal_order(&set).iter().map(|t| t.id.as_str()).collect();
        assert_eq!(order, vec!["a", "b"]);
    }

    #[test]
    fn a_file_already_gone_counts_as_removed() {
        let dir = PathBuf::from("/p");
        let set = chain_set(&dir);
        let outcome = remove_in_order(&removal_order(&set), &dir, |_| {
            Err(std::io::ErrorKind::NotFound.into())
        });
        assert_eq!(outcome, DeleteOutcome::Removed);
    }

    #[test]
    fn a_failed_removal_stops_and_names_removed_and_left_files_in_removal_order() {
        let dir = PathBuf::from("/p");
        let set = chain_set(&dir);
        let seen = std::cell::RefCell::new(Vec::new());
        let outcome = remove_in_order(&removal_order(&set), &dir, |path| {
            seen.borrow_mut().push(file_name(path));
            if path.ends_with("mid.md") {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            Ok(())
        });

        let DeleteOutcome::Failed {
            error,
            kind,
            removed,
            left,
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert!(error.starts_with("mid.md: "), "{error}");
        assert_eq!(kind, Some(std::io::ErrorKind::PermissionDenied));
        assert_eq!(removed, strs(&["leaf.md"]));
        assert_eq!(left, strs(&["mid.md", "root.md"]));
        // Stopped at the failure: `root.md` was never attempted.
        assert_eq!(*seen.borrow(), strs(&["leaf.md", "mid.md"]));
    }

    #[test]
    fn a_path_outside_the_pending_directory_is_refused_with_nothing_removed() {
        let pending = PathBuf::from("/p");
        let mut set = chain_set(&pending);
        set[0].tasks[1].path = PathBuf::from("/q/mid.md");
        let called = std::cell::Cell::new(false);
        let outcome = remove_in_order(&removal_order(&set), &pending, |_| {
            called.set(true);
            Ok(())
        });

        let DeleteOutcome::Failed {
            error,
            removed,
            left,
            ..
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert!(removed.is_empty());
        assert_eq!(left, strs(&["leaf.md", "mid.md", "root.md"]));
        assert!(!called.get(), "a refused set must remove nothing");
        assert_eq!(
            error, "/q/mid.md is not in the pending directory, so nothing was removed",
            "the popup showing it adds the remedy"
        );
    }
}
