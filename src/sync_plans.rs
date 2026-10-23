//! The plan-page half of `spoolway sync`: move a home-mode clone's pages into
//! the workspace and point its tasks at them.
//!
//! Plan pages used to sit in each clone's own dispatcher folder. Screens now
//! read one `plans/` per workspace, so a page left in a clone's folder drops
//! off every screen. A task finds its page through the absolute path in
//! `plan:`, so moving a page alone would break every task that names it.
//!
//! Each page is therefore handled in this order: the tasks naming it are
//! rewritten first, and the page moves second. A crash between the two leaves
//! tasks naming the workspace path while the page is still in the clone's
//! folder, and the next sync moves it. A crash part-way through a page's
//! several tasks leaves the page in place too, so that run finishes the rest
//! the same way. A task that names the clone's path for a page that is already in the
//! workspace, because it came to name it after the page moved, is rewritten as
//! well.
//!
//! A lane does not do any of this. The pages and tasks are outside git, so a
//! lane's rewrite of them cannot be reviewed or undone, and it only reports
//! that a person runs `spoolway sync` in the main checkout.
//!
//! Nothing is overwritten. A page whose name is taken in the workspace folder
//! stays where it is and is reported with the tasks that name it, and the other
//! pages still move. Only the clone sync runs in is touched, only `.html`
//! files directly inside its `plans/` move (the folders there hold mockups that
//! tasks already link to), and archived tasks keep their old `plan:`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::commands::pending::{StampSource, front_str, md_files, missing_plans_since, plan_of};
use crate::repo::{Repo, shorten_home};

/// What a plan-page sync did, or with `--dry-run` would do.
pub(crate) struct Summary {
    /// Something was seeded, moved or rewritten.
    pub changed: bool,
    /// A page was left where it was because its name is taken.
    pub refused: bool,
}

/// One page to take into the workspace, with the task files that name it.
struct Page {
    from: PathBuf,
    to: PathBuf,
    /// False when the page is already in the workspace and only tasks
    /// still name the old path.
    move_file: bool,
    tasks: Vec<PathBuf>,
}

/// A page that cannot move because its name is taken in the workspace.
struct Clash {
    from: PathBuf,
    to: PathBuf,
    tasks: Vec<PathBuf>,
}

/// Move this clone's pages into the workspace, printing one line per change.
/// With `dry_run` nothing is written and the lines are in the conditional.
/// Repo mode has no workspace, so it does nothing.
pub(crate) fn run(repo: &Repo, dry_run: bool) -> Result<Summary> {
    let mut summary = Summary {
        changed: false,
        refused: false,
    };
    let Some(workspace) = repo.workspace_dir() else {
        return Ok(summary);
    };
    let own = repo.home().join(crate::config::PLANS_DIR);
    let shared = workspace.join(crate::config::PLANS_DIR);
    // Columns as the report is drawn: every word padded to the longest of its
    // set plus one space, so the path starts in the same column on every line
    // and the lines under an entry line up with it. A dry run's words are
    // longer.
    let (seeded, moved, rewrote, refused) = match dry_run {
        true => (
            "would seed    ",
            "would move    ",
            "would rewrite ",
            "refused       ",
        ),
        false => ("seeded   ", "moved    ", "rewrote  ", "refused  "),
    };
    let indent = " ".repeat(seeded.len());

    let (pages, clashes) = plan(repo, &own, &shared)?;

    if repo.in_lane_worktree() {
        let seed = missing_plans_since(repo, true).is_some();
        if seed || !pages.is_empty() || !clashes.is_empty() {
            println!(
                "plan pages: a lane does not move plan pages or rewrite task files, which sit \
                 outside git — a person runs `spoolway sync` in the main checkout to move them"
            );
        }
        return Ok(summary);
    }

    // Before any page moves: the stamp hides pages older than it, and a page
    // that arrived under a fresh one would read as already cut into tasks.
    let any_page = !pages.is_empty() || has_html(&shared);
    if let Some((since, source)) = missing_plans_since(repo, any_page) {
        let why = match source {
            StampSource::OldestClone => "the oldest clone stamp",
            StampSource::NoPage => "no page yet",
            StampSource::Now => "the time of this sync",
        };
        let file = repo.plans_since_file();
        if !dry_run {
            crate::task::write_atomic(&file, since.to_string())?;
        }
        println!("{seeded}{} ({since}, {why})", shorten_home(&file));
        summary.changed = true;
    }

    if !dry_run && !pages.is_empty() {
        std::fs::create_dir_all(&shared)
            .with_context(|| format!("creating {}", shared.display()))?;
    }
    for page in &pages {
        let mut lines = Vec::new();
        if page.move_file {
            lines.push(format!(
                "{moved}{}\n{indent}→ {}",
                shorten_home(&page.from),
                shorten_home(&page.to)
            ));
        }
        for task in &page.tasks {
            // A task that changed or left under us no longer names the page,
            // and has nothing to rewrite.
            if dry_run || rewrite_task(repo, task, &page.from, &page.to)? {
                lines.push(format!("{rewrote}{} (plan:)", shorten_home(task)));
            }
        }
        if page.move_file && !dry_run {
            move_page(&page.from, &page.to)?;
        }
        for line in lines {
            println!("{line}");
        }
        summary.changed = true;
    }

    for clash in &clashes {
        for line in refusal_lines(clash, refused) {
            println!("{line}");
        }
        summary.refused = true;
    }
    Ok(summary)
}

/// The lines of one refused page, led by `word` and indented under it.
fn refusal_lines(clash: &Clash, word: &str) -> Vec<String> {
    let indent = " ".repeat(word.len());
    let mut lines = vec![
        format!("{word}{}", shorten_home(&clash.from)),
        format!("{indent}— {} is already there", shorten_home(&clash.to)),
    ];
    for task in &clash.tasks {
        lines.push(format!(
            "{indent}this task names it: {}",
            shorten_home(task)
        ));
    }
    lines.push(format!(
        "{indent}rename the page and that task's plan: line to a free name, then run \
         spoolway sync again"
    ));
    lines
}

/// Whether `dir` holds an `.html` file directly inside it.
fn has_html(dir: &Path) -> bool {
    html_files(dir).is_ok_and(|pages| !pages.is_empty())
}

/// The `.html` files directly inside `dir`, by name; none when it is missing.
fn html_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    let mut pages: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("html"))
        .collect();
    pages.sort();
    Ok(pages)
}

/// Work out which pages move, which clash, and which tasks name each.
fn plan(repo: &Repo, own: &Path, shared: &Path) -> Result<(Vec<Page>, Vec<Clash>)> {
    // A clone folder that is a link to the workspace's has its pages there
    // already, and "moving" one would remove the only name it has.
    if same_folder(own, shared) {
        return Ok((Vec::new(), Vec::new()));
    }
    // Old path -> the task files whose `plan:` is exactly that path.
    let mut naming: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for dir in [repo.pending_dir(), repo.queue_dir()] {
        for task in md_files(&dir)? {
            let Some(plan) = plan_of(&task) else {
                continue;
            };
            let plan = PathBuf::from(plan);
            if plan.parent() == Some(own)
                && plan.extension().and_then(|e| e.to_str()) == Some("html")
            {
                naming.entry(plan).or_default().push(task);
            }
        }
    }

    let mut olds: Vec<PathBuf> = html_files(own)?;
    for old in naming.keys() {
        if !olds.contains(old) {
            olds.push(old.clone());
        }
    }
    olds.sort();

    let mut pages = Vec::new();
    let mut clashes = Vec::new();
    for from in olds {
        let Some(name) = from.file_name() else {
            continue;
        };
        let to = shared.join(name);
        let tasks = naming.remove(&from).unwrap_or_default();
        let taken = std::fs::symlink_metadata(&to).is_ok();
        // A link is not a page: only a regular file moves, as in `html_files`.
        if std::fs::symlink_metadata(&from).is_ok_and(|meta| meta.is_file()) {
            // A page that is the very same file as the one in the workspace is
            // what a crash between linking and unlinking leaves. Nothing is
            // lost by finishing that move.
            if taken && !same_file(&from, &to) {
                clashes.push(Clash { from, to, tasks });
            } else {
                pages.push(Page {
                    from,
                    to,
                    move_file: true,
                    tasks,
                });
            }
        } else if taken && !tasks.is_empty() {
            pages.push(Page {
                from,
                to,
                move_file: false,
                tasks,
            });
        }
    }
    Ok((pages, clashes))
}

/// Whether two folders are one folder, by where each really is. A missing one
/// is nothing in common with the other.
fn same_folder(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether two paths are one file. Only Unix can tell; elsewhere a clash is
/// always refused, which is the safe answer.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        false
    }
}

/// Move `from` to `to` without ever replacing a file at `to`.
///
/// A rename would silently replace a page another process wrote there since
/// the plan was made. A hard link fails instead, and the old name is removed
/// only once the new one is in place, so the page is never missing from both.
/// A filesystem that cannot hard link gets a copy that also refuses an existing
/// name. Only a hard link keeps the page's creation time, which the plan list
/// compares with `plans-since`, so the copy is the fallback and not the rule.
fn move_page(from: &Path, to: &Path) -> Result<()> {
    let taken = |to: &Path| {
        anyhow::anyhow!(
            "{} appeared while sync ran, so {} was not moved\n  rename the page to a free name \
             and fix the plan: line of any task that names it, then run spoolway sync again",
            shorten_home(to),
            shorten_home(from)
        )
    };
    match std::fs::hard_link(from, to) {
        Ok(()) => {}
        // Plan-time checks keep the two names in different folders, so one
        // inode under both is what a crash between linking and removing leaves.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && same_file(from, to) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Err(taken(to)),
        Err(_) => match copy_no_clobber(from, to) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(taken(to));
            }
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "moving {} to {}: this folder cannot hard link and copying failed\n  \
                         check that {} is writable and has room, then run spoolway sync again",
                        shorten_home(from),
                        shorten_home(to),
                        shorten_home(to.parent().unwrap_or(to))
                    )
                });
            }
        },
    }
    std::fs::remove_file(from).with_context(|| format!("removing {}", shorten_home(from)))
}

/// Copy `from` to a `to` that must not exist yet. A partial copy is removed,
/// so a failed one never leaves half a page under the final name.
fn copy_no_clobber(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut src = std::fs::File::open(from)?;
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)?;
    let copied = std::io::copy(&mut src, &mut dst).and_then(|_| dst.sync_all());
    if copied.is_err() {
        let _ = std::fs::remove_file(to);
    }
    copied
}

/// Point one task's `plan:` at `to`, and say whether it was changed.
///
/// Read, rewritten and written under the task's own lock, as the queue's edits
/// are, so the dispatcher's write to the same file is not lost. Only the
/// `plan:` line's value changes; every other byte is copied through.
fn rewrite_task(repo: &Repo, task: &Path, from: &Path, to: &Path) -> Result<bool> {
    let id = task
        .file_stem()
        .and_then(|stem| stem.to_str())
        .with_context(|| format!("{} has no usable file name", task.display()))?;
    let _lock = crate::lock::TaskLock::acquire(&repo.task_lock_file(id)).with_context(|| {
        format!(
            "{} is being written by another process\n  wait a moment, or stop the dispatcher, \
             then run spoolway sync again; pages already moved stay moved",
            shorten_home(task)
        )
    })?;
    // Listed before the lock was held, so the task may have left the folder
    // since: queued, archived or deleted. It then has nothing to rewrite here.
    let doc = match std::fs::read_to_string(task) {
        Ok(doc) => doc,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(err).with_context(|| {
                format!(
                    "reading {}\n  check that the file is readable, then run spoolway sync again",
                    shorten_home(task)
                )
            });
        }
    };
    let Some(new) = with_plan(&doc, from, to) else {
        return Ok(false);
    };
    crate::task::write_atomic(task, new)?;
    Ok(true)
}

/// `doc` with its frontmatter's `plan:` line pointing at `to`, or `None` when
/// that line does not name `from`.
fn with_plan(doc: &str, from: &Path, to: &Path) -> Option<String> {
    let mut offset = 0usize;
    let mut lines = doc.split_inclusive('\n');
    // The opening fence, as `split_fence` reads it.
    if lines.next()?.trim_end() != "---" {
        return None;
    }
    offset += doc.split_inclusive('\n').next()?.len();
    for line in lines {
        let content = line.trim_end_matches(['\r', '\n']);
        if content == "---" {
            return None;
        }
        if content.starts_with("plan:") {
            let value: serde_norway::Value = serde_norway::from_str(content).ok()?;
            if front_str(&value, "plan")? != from.to_str()? {
                return None;
            }
            let new =
                serde_norway::to_string(&serde_norway::Value::String(to.to_str()?.to_string()))
                    .ok()?;
            let ending = &line[content.len()..];
            return Some(format!(
                "{}plan: {}{ending}{}",
                &doc[..offset],
                new.trim_end(),
                &doc[offset + line.len()..]
            ));
        }
        offset += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::two_clones;

    fn task(repo: &Repo, dir: &str, id: &str, plan: &Path) -> PathBuf {
        let path = repo.home().join(dir).join(format!("{id}.md"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "---\nid: {id}\ngroup: g\nplan: {}\ntitle: 'keep: me'\n---\n\nbody plan: x\n",
                plan.display()
            ),
        )
        .unwrap();
        path
    }

    fn page(dir: &Path, name: &str, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn own_plans(repo: &Repo) -> PathBuf {
        repo.home().join("plans")
    }

    fn shared_plans(repo: &Repo) -> PathBuf {
        repo.workspace_dir().unwrap().join("plans")
    }

    /// Fails before the change: `sync` moved nothing, so the page stayed in
    /// the clone and the task kept naming it.
    #[test]
    fn moves_a_page_and_relinks_pending_and_queued_tasks() {
        let (shop, _shop_2, _root) = two_clones("sync-plans-move");
        let old = page(&own_plans(&shop), "2026-10-12-a.html", "A");
        let pending = task(&shop, "pending", "p", &old);
        let queued = task(&shop, "queue", "q", &old);
        let new = shared_plans(&shop).join("2026-10-12-a.html");

        let summary = run(&shop, false).unwrap();

        assert!(summary.changed && !summary.refused);
        assert!(!old.exists());
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "A");
        for path in [pending, queued] {
            let doc = std::fs::read_to_string(path).unwrap();
            assert!(doc.contains(&format!("plan: {}\n", new.display())), "{doc}");
            // Every other byte is copied through.
            assert!(doc.contains("title: 'keep: me'\n---\n\nbody plan: x\n"));
        }
    }

    /// Fails before the change: nothing refused a clash, so there was no
    /// check that a page in the workspace folder survives.
    #[test]
    fn a_taken_name_is_refused_and_the_rest_still_move() {
        let (shop, _shop_2, _root) = two_clones("sync-plans-clash");
        let taken = page(&shared_plans(&shop), "b.html", "WORKSPACE");
        let clash = page(&own_plans(&shop), "b.html", "CLONE");
        let free = page(&own_plans(&shop), "c.html", "C");
        let naming = task(&shop, "pending", "p", &clash);

        let summary = run(&shop, false).unwrap();

        assert!(summary.refused);
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "WORKSPACE");
        assert_eq!(std::fs::read_to_string(&clash).unwrap(), "CLONE");
        assert!(!free.exists());
        assert!(shared_plans(&shop).join("c.html").exists());
        let doc = std::fs::read_to_string(naming).unwrap();
        assert!(doc.contains(&format!("plan: {}\n", clash.display())));
    }

    /// Fails before the change: a dry run had no `would` entries to print and
    /// no way to promise it wrote nothing.
    #[test]
    fn a_dry_run_writes_nothing() {
        let (shop, _shop_2, _root) = two_clones("sync-plans-dry");
        std::fs::remove_file(shop.plans_since_file()).unwrap();
        let old = page(&own_plans(&shop), "d.html", "D");
        let pending = task(&shop, "pending", "p", &old);
        let before = std::fs::read_to_string(&pending).unwrap();

        let summary = run(&shop, true).unwrap();

        assert!(summary.changed);
        assert!(old.exists());
        assert!(!shared_plans(&shop).join("d.html").exists());
        assert!(!shop.plans_since_file().exists());
        assert_eq!(std::fs::read_to_string(&pending).unwrap(), before);
    }

    /// Fails before the change: the stamp was only ever written by a screen
    /// reload, after the pages had arrived.
    #[test]
    fn a_missing_stamp_is_seeded_from_the_oldest_clone() {
        let (shop, shop_2, _root) = two_clones("sync-plans-seed");
        std::fs::remove_file(shop.plans_since_file()).unwrap();
        std::fs::write(shop_2.home().join("plans-since"), "7").unwrap();
        page(&own_plans(&shop), "e.html", "E");

        run(&shop, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(shop.plans_since_file()).unwrap(),
            "7"
        );
        assert!(shared_plans(&shop).join("e.html").exists());
    }

    /// Fails before the change: no sync step relinked a task that names a
    /// clone path whose page is already in the workspace.
    #[test]
    fn a_second_run_rewrites_a_task_whose_page_already_moved() {
        let (shop, _shop_2, _root) = two_clones("sync-plans-resume");
        let old = own_plans(&shop).join("f.html");
        let new = page(&shared_plans(&shop), "f.html", "F");
        let pending = task(&shop, "pending", "p", &old);

        run(&shop, false).unwrap();

        let doc = std::fs::read_to_string(pending).unwrap();
        assert!(doc.contains(&format!("plan: {}\n", new.display())));
        let again = run(&shop, false).unwrap();
        assert!(!again.changed);
    }

    /// Fails before the change: mockup folders, other clones' pages and
    /// archived tasks had no rule saying sync leaves them alone.
    #[test]
    fn folders_other_clones_and_archived_tasks_stay_put() {
        let (shop, shop_2, _root) = two_clones("sync-plans-scope");
        let folder = own_plans(&shop).join("mock");
        std::fs::create_dir_all(&folder).unwrap();
        let theirs = page(&own_plans(&shop_2), "g.html", "G");
        let moving = page(&own_plans(&shop), "h.html", "H");
        let archived = task(&shop, "archive", "a", &moving);
        let before = std::fs::read_to_string(&archived).unwrap();

        run(&shop, false).unwrap();

        assert!(folder.is_dir());
        assert!(theirs.exists());
        assert!(shared_plans(&shop).join("h.html").exists());
        assert_eq!(std::fs::read_to_string(&archived).unwrap(), before);
    }

    /// Fails before the change: nothing stopped a lane from moving pages and
    /// rewriting tasks that sit outside git.
    #[test]
    fn a_lane_moves_nothing() {
        let (mut shop, _shop_2, _root) = two_clones("sync-plans-lane");
        let old = page(&own_plans(&shop), "i.html", "I");
        let pending = task(&shop, "pending", "p", &old);
        let before = std::fs::read_to_string(&pending).unwrap();
        shop.checkout = shop.worktree_root().join("some-task");

        let summary = run(&shop, false).unwrap();

        assert!(!summary.changed && !summary.refused);
        assert!(old.exists());
        assert_eq!(std::fs::read_to_string(&pending).unwrap(), before);
    }

    /// Fails before the change: a clone folder linked to the workspace's had
    /// its only copy of a page removed by the finishing step of a move.
    #[cfg(unix)]
    #[test]
    fn a_plans_folder_linked_to_the_workspace_loses_nothing() {
        let (shop, _shop_2, _root) = two_clones("sync-plans-linked");
        let kept = page(&shared_plans(&shop), "j.html", "J");
        std::os::unix::fs::symlink(shared_plans(&shop), own_plans(&shop)).unwrap();
        let pending = task(&shop, "pending", "p", &own_plans(&shop).join("j.html"));
        let before = std::fs::read_to_string(&pending).unwrap();

        let summary = run(&shop, false).unwrap();

        assert!(!summary.refused);
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "J");
        assert_eq!(std::fs::read_to_string(&pending).unwrap(), before);
    }

    /// Fails before the change: a task that left its folder between the
    /// listing and the rewrite aborted the whole sync with a bare read error.
    #[test]
    fn a_task_that_left_its_folder_is_not_an_error() {
        let (shop, _shop_2, _root) = two_clones("sync-plans-gone");
        let old = own_plans(&shop).join("k.html");
        let gone = task(&shop, "pending", "p", &old);
        std::fs::remove_file(&gone).unwrap();

        let changed = rewrite_task(&shop, &gone, &old, &shared_plans(&shop).join("k.html"));

        assert!(!changed.unwrap());
    }

    /// Fails before the change: the refused entry's wording and columns had
    /// no test, so any edit to them passed.
    #[test]
    fn a_refusal_names_both_paths_each_task_and_the_remedy() {
        let clash = Clash {
            from: PathBuf::from("/nowhere/own/plans/a.html"),
            to: PathBuf::from("/nowhere/plans/a.html"),
            tasks: vec![PathBuf::from("/nowhere/own/pending/t.md")],
        };

        assert_eq!(
            refusal_lines(&clash, "refused  "),
            [
                "refused  /nowhere/own/plans/a.html",
                "         — /nowhere/plans/a.html is already there",
                "         this task names it: /nowhere/own/pending/t.md",
                "         rename the page and that task's plan: line to a free name, then run \
                 spoolway sync again",
            ]
        );
    }

    /// Fails before the change: there was no copy to fall back on when a
    /// filesystem cannot hard link, and nothing checked it refuses a taken
    /// name.
    #[test]
    fn the_copy_fallback_never_replaces_a_page() {
        let (shop, _shop_2, _root) = two_clones("sync-plans-copy");
        let from = page(&own_plans(&shop), "l.html", "NEW");
        let to = shared_plans(&shop).join("l.html");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();

        copy_no_clobber(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "NEW");

        std::fs::write(&to, "OLD").unwrap();
        let err = copy_no_clobber(&from, &to).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&to).unwrap(), "OLD");
    }
}
