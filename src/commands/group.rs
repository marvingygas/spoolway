//! `spoolway group list`: what the queue's own `group:` key answers.

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

    if groups.is_empty() {
        println!("No queued tasks belong to a group.");
        return Ok(());
    }

    for (group, mut open) in groups {
        open.sort();
        println!("{group:<28}  {} open — {}", open.len(), open.join(", "));
    }
    Ok(())
}
