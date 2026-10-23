//! `spoolway group list`: what the queue's own `group:` key answers, and the
//! plan pages no task names yet.

use super::*;

/// Groups that have tasks in the queue, and which of their tasks are still
/// open.
///
/// One line per group, and that shape is load-bearing: it is how a person
/// sees what a group has left before landing its stack. See the doc comment
/// on [`crate::cli::GroupCommand::List`], which says the same thing to
/// whoever reaches for `--help` before reflowing it.
///
/// `repo.tasks()` is already exactly the open set — a task that reaches `done`
/// is archived out of it — so counting them is the whole computation. A group
/// every one of whose tasks has been archived has no line here at all, which
/// is how a finished group reads. `group:` is read verbatim — never
/// path-parsed — so two tasks group together only when they name exactly the
/// same string.
///
/// After the group lines come the plan pages that no pending, queued or
/// archived task with a `group:` names, newest first, each as
/// `<name>  0 open — plan <path>`. The fixed `0 open — plan` lets a script
/// that counts groups tell a plan line from a group line. The pages are the
/// ones the queue screen lists, under the same `plans-since` rule.
///
/// The plan side can fail without the queue being at fault, for instance when
/// `plans/` is not a folder, or when `pending/` or the archive index, which
/// the exclusion reads too, cannot be read. The group lines still print then,
/// and a notice on stderr says the plan lines are missing and names the path
/// that failed, the way the queue screen keeps its groups when its plan list
/// fails.
pub fn group_list(repo: &Repo) -> Result<()> {
    let tasks = repo.tasks()?;
    let mut groups: std::collections::BTreeMap<String, Vec<String>> = Default::default();

    for task in &tasks {
        let Some(group) = task.front.group.as_deref() else {
            continue;
        };
        groups
            .entry(group.to_string())
            .or_default()
            .push(task.id().to_string());
    }

    // The queue alone is not enough here: a plan a pending task names is
    // not waiting to be cut, and only the pending-aware listing knows that.
    let plans = super::pending::list_groups_and_skipped(repo)
        .and_then(|(listed, _)| super::pending::plans_without_tasks(repo, &listed))
        .unwrap_or_else(|err| {
            eprintln!(
                "Plans with no tasks are not listed, because they could not be read: {err:#}. \
                 Check that the path named above is one you can read, or repair \
                 it if it will not parse, then run `spoolway group list` again."
            );
            Vec::new()
        });

    if groups.is_empty() && plans.is_empty() {
        println!("No queued tasks belong to a group.");
        return Ok(());
    }

    for (group, mut open) in groups {
        open.sort();
        println!("{group:<28}  {} open — {}", open.len(), open.join(", "));
    }
    for page in plans {
        println!("{:<28}  0 open — plan {}", page.name, page.path.display());
    }
    Ok(())
}
