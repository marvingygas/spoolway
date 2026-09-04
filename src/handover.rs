//! Handing in-flight work to someone else, and getting it off your laptop
//! first.
//!
//! Task files are deliberately untracked — they are runtime state, they change
//! on every transition, and a worktree carrying a copy of every one of them
//! would shadow the canonical queue (see `Repo::discover`). So they live on a
//! ref instead of in the tree: `refs/spoolway/<your git user.email>`, which
//! nobody but you writes.
//!
//! One writer per ref is the whole design. There is nothing to merge, no
//! conflict to resolve, no ownership field and no override for the person who
//! left without handing over — because a task file exists in exactly one queue,
//! and holding it is what owning the task means. `adopt` moves files from
//! someone else's ref into yours; their dispatcher stops seeing them because
//! they are no longer there.
//!
//! What is on the ref is task files, the task branches they name, and a snapshot
//! of any worktree with uncommitted changes. Not the project's code: that lives
//! on ordinary branches, pushed by the `handover` step, exactly as before.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::repo::Repo;
use crate::task::Task;

/// The namespace one person's mirror lives under.
///
/// Everything below it is a leaf: git refuses a ref that is a prefix of another
/// ref, so the queue cannot itself be `refs/spoolway/<who>` while the branches
/// hang off `refs/spoolway/<who>/branch/...`.
pub fn ref_for(who: &str) -> String {
    format!("refs/spoolway/{who}")
}

/// The queue itself.
fn queue_ref(who: &str) -> String {
    format!("{}/queue", ref_for(who))
}

/// One task branch, kept out of the repo's branch list until a lane with
/// `push` publishes it properly.
fn branch_ref(who: &str, branch: &str) -> String {
    format!("{}/branch/{branch}", ref_for(who))
}

/// Whose mirror this machine writes: the git identity already configured in
/// every repo anyone commits from, so there is nothing new to set up.
pub fn identity(repo: &Repo) -> Result<String> {
    let email = repo
        .git(&["config", "user.email"])
        .context("no git user.email is configured, so there is no mirror to write")?
        .trim()
        .to_string();
    if email.is_empty() {
        bail!("git user.email is empty, so there is no mirror to write");
    }
    Ok(email)
}

/// What one mirror pushed, for `spoolway handover`'s report.
#[derive(Debug, Default)]
pub struct Mirrored {
    pub tasks: usize,
    pub branches: usize,
    pub snapshots: usize,
    /// Tasks somebody else adopted, and this machine has just let go of.
    pub released: Vec<String>,
}

impl Mirrored {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.tasks == 0 && self.branches == 0 && self.snapshots == 0 && self.released.is_empty()
    }
}

/// Push this machine's queue, its task branches, and a snapshot of any dirty
/// worktree, to the mirror only this machine writes.
///
/// Never fatal to a pass: a mirror that cannot be written is worth reporting and
/// nothing more. The pipeline's job is to move work forward, and it does that
/// with or without a backup of it.
pub fn mirror(repo: &Repo, tasks: &[Task]) -> Result<Mirrored> {
    let who = identity(repo)?;

    // Someone may have adopted work away since the last pass. The mirror is
    // push-only, so this one read of *our own* ref is how this machine finds
    // out — without it, the task file would still be here, this dispatcher
    // would keep running it, and the very next push would put it back on the
    // mirror and undo the handover.
    let mut pushed = Mirrored {
        released: reap_adopted(repo, &who)?,
        ..Default::default()
    };

    let mut refspecs: Vec<String> = Vec::new();

    // The queue itself, as a commit whose tree is the queue directory. Built
    // through an index of its own so the checkout's real index is untouched —
    // a person may well be mid-`git add` while a pass runs.
    if let Some(commit) = queue_commit(repo, &who)? {
        repo.git(&["update-ref", &queue_ref(&who), &commit])?;
        refspecs.push(format!("+{0}:{0}", queue_ref(&who)));
        pushed.tasks = tasks.len();
    }

    for task in tasks {
        let Some(branch) = task.front.branch.as_deref() else {
            continue;
        };
        let Ok(tip) = repo.git(&["rev-parse", "--verify", "--quiet", branch]) else {
            continue;
        };
        let tip = tip.trim().to_string();

        // Namespaced, not pushed as an ordinary branch: unreviewed work has no
        // business in the repo's branch list until a lane with `push` puts it
        // there. It becomes a real branch when the `handover` step says so.
        //
        // Only when it has actually moved. The local copy of the mirror ref is
        // what it was last pushed at, so an idle task costs a `rev-parse` per
        // pass instead of a push every ten minutes for as long as it waits.
        let mirrored = branch_ref(&who, branch);
        if at(repo, &mirrored).as_deref() != Some(tip.as_str()) {
            repo.git(&["update-ref", &mirrored, &tip])?;
            refspecs.push(format!("+{0}:{0}", mirrored));
            pushed.branches += 1;
        }

        // Whatever the lane has not committed yet, as a commit object built
        // without touching the worktree — which is what makes it safe to do
        // underneath a lane that is still working.
        if let Some(worktree) = task.front.worktree_path.as_deref() {
            let wip = wip_ref(&who, branch);
            // Compared by tree, not by commit: `commit-tree` mints a new hash
            // whenever the clock has moved, so comparing commits would push a
            // fresh snapshot of identical content on every pass.
            let already = at(repo, &wip)
                .and_then(|sha| repo.git(&["rev-parse", &format!("{sha}^{{tree}}")]).ok())
                .map(|tree| tree.trim().to_string());
            if let Some((commit, tree)) = snapshot(worktree)
                && already.as_deref() != Some(tree.as_str())
            {
                repo.git(&["update-ref", &wip, &commit])?;
                refspecs.push(format!("+{0}:{0}", wip));
                pushed.snapshots += 1;
            }
        }
    }

    if refspecs.is_empty() || !repo.has_remote() {
        return Ok(pushed);
    }

    let mut args: Vec<&str> = vec!["push", "--quiet", "origin"];
    args.extend(refspecs.iter().map(String::as_str));
    repo.git(&args)?;
    Ok(pushed)
}

fn wip_ref(who: &str, branch: &str) -> String {
    format!("{}/wip/{branch}", ref_for(who))
}

/// Let go of tasks somebody has adopted away, and say which.
///
/// The one place this machine reads its own mirror. Comparing the ref as we last
/// pushed it against the ref as it now stands says exactly what was taken: a
/// task file that was on our mirror and is no longer there was moved into
/// somebody else's queue, and it is theirs from that moment.
///
/// A task never pushed yet — queued a minute ago — is not on either side of that
/// comparison and so is never touched. Nothing is deleted when the fetch fails,
/// because "the network is down" and "your colleague took this" must not look
/// the same.
fn reap_adopted(repo: &Repo, who: &str) -> Result<Vec<String>> {
    if !repo.has_remote() {
        return Ok(Vec::new());
    }
    let Some(ours) = at(repo, &queue_ref(who)) else {
        return Ok(Vec::new());
    };

    let probe = format!("{}/last-seen", ref_for(who));
    if repo
        .git(&[
            "fetch",
            "--quiet",
            "origin",
            &format!("+{}:{probe}", queue_ref(who)),
        ])
        .is_err()
    {
        return Ok(Vec::new());
    }
    let Some(theirs) = at(repo, &probe) else {
        return Ok(Vec::new());
    };
    if theirs == ours {
        return Ok(Vec::new());
    }

    let queue = crate::config::QUEUE_DIR;
    let listing = |commit: &str| -> Vec<String> {
        repo.git(&["ls-tree", "--name-only", &format!("{commit}:{queue}")])
            .map(|out| out.lines().map(str::trim).map(String::from).collect())
            .unwrap_or_default()
    };
    let still_there = listing(&theirs);

    // A lane still mid-turn on a task must not have its file yanked out from
    // under it: its own `spoolway report` would then fail with "no task" and
    // its usage go unbanked (review finding 61). The lane name is
    // `<task> · <step>`, so a key whose task half matches means one is live.
    // Such a task is kept — this machine's own `mirror` re-pushes it on the
    // same pass, so the race resolves to "the task belongs to the machine
    // whose lane is running it" rather than to a corrupted lane record. The
    // person who adopted in-flight work is left with a duplicate to sort out.
    let lanes = crate::dispatch::load_lane_records(repo);
    let a_lane_is_running = |id: &str| {
        lanes
            .keys()
            .filter_map(|name| name.split_once(" · "))
            .any(|(task, _)| task == id)
    };

    let mut released = Vec::new();
    for name in listing(&ours) {
        if name.is_empty() || still_there.contains(&name) {
            continue;
        }
        let path = repo.queue_dir().join(&name);
        if !path.exists() {
            continue;
        }
        let id = name.trim_end_matches(".md");
        if a_lane_is_running(id) {
            crate::problem_log::append(
                repo,
                &format!(
                    "{id}: adopted away elsewhere while a lane is running it here — kept, and \
                     re-mirrored; the other machine now has a duplicate"
                ),
            );
            continue;
        }
        std::fs::remove_file(&path).with_context(|| format!("letting go of {}", path.display()))?;
        released.push(id.to_string());
    }

    // Our mirror is now what the remote says it is, so the next pass compares
    // against the truth rather than reporting the same handover forever. A
    // task kept above is re-added to the ref by `mirror`'s own queue commit
    // right after this, so it does not read as taken again next pass.
    repo.git(&["update-ref", &queue_ref(who), &theirs])?;
    Ok(released)
}

/// What a ref points at here, or `None` if it names nothing.
fn at(repo: &Repo, reference: &str) -> Option<String> {
    repo.git(&["rev-parse", "--verify", "--quiet", reference])
        .ok()
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
}

/// A commit object for whatever is uncommitted in `worktree`, or `None` when it
/// is clean. Touches neither the worktree nor its index.
///
/// Not `git stash create`, which is the obvious call and the wrong one: it
/// captures modifications to tracked files and silently leaves untracked ones
/// behind — and a brand new source file the implementer just wrote is exactly
/// that. An index of our own with `add -A` takes both, and still honours
/// `.gitignore` so build output stays out.
fn snapshot(worktree: &Path) -> Option<(String, String)> {
    if !worktree.is_dir() {
        return None;
    }

    // Kept outside the worktree: a stray index file left by a killed process
    // would otherwise be swept into the next `git add -A` and committed.
    let index =
        std::env::temp_dir().join(format!("spoolway-snapshot-{}.index", std::process::id()));
    let _ = std::fs::remove_file(&index);
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(worktree)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    let head = crate::repo::run(worktree, "git", &["rev-parse", "HEAD"])
        .ok()?
        .trim()
        .to_string();
    git(&["read-tree", "HEAD"])?;
    git(&["add", "-A"])?;
    let tree = git(&["write-tree"])?;
    let _ = std::fs::remove_file(&index);
    if tree.is_empty() {
        return None;
    }

    // Identical to what is already committed: nothing to snapshot.
    let committed = crate::repo::run(worktree, "git", &["rev-parse", "HEAD^{tree}"]).ok()?;
    if committed.trim() == tree {
        return None;
    }

    let commit = crate::repo::run(
        worktree,
        "git",
        &["commit-tree", &tree, "-p", &head, "-m", "spoolway snapshot"],
    )
    .ok()
    .map(|sha| sha.trim().to_string())
    .filter(|sha| !sha.is_empty())?;
    Some((commit, tree))
}

/// A commit whose tree is the queue directory, parented on the last mirror so
/// the ref has a history rather than being rewritten from nothing each time.
fn queue_commit(repo: &Repo, who: &str) -> Result<Option<String>> {
    let queue = crate::config::QUEUE_DIR;
    // The field, not `Repo::home`: a queue that has never existed must read as
    // "nothing to mirror" below, and the accessor would create an empty one
    // just by being asked.
    if !repo.home.join(queue).is_dir() {
        return Ok(None);
    }

    let index = repo.mirror_index_file();
    let _ = std::fs::remove_file(&index);

    // The queue moved out of the checkout, so every call here points git's
    // work tree at the project's home explicitly — `add` and `write-tree`
    // both read the filesystem, unlike the plain object-store reads
    // elsewhere in this file, which take `repo.root` as cwd and never see the
    // queue's new home at all.
    let git = |args: &[&str]| -> Result<String> {
        let mut command = std::process::Command::new("git");
        command
            .arg("--work-tree")
            .arg(repo.home())
            .args(args)
            .current_dir(&repo.root)
            .env("GIT_INDEX_FILE", &index);
        let out = command
            .output()
            .with_context(|| format!("running `git {}`", args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };

    // `--force`: the queue is in `.gitignore` in a project set up before it
    // moved out of the checkout, which is exactly why it needs a ref of its
    // own rather than a place in the tree. An absolute path, because a
    // pathspec is read against the current directory rather than against
    // `--work-tree` — `repo.root`, here — and has to resolve to somewhere
    // inside the work tree either way.
    let queue_path = repo.home().join(queue);
    git(&["add", "--force", "--", &queue_path.display().to_string()])?;
    let tree = git(&["write-tree"])?.trim().to_string();
    let _ = std::fs::remove_file(&index);
    if tree.is_empty() {
        return Ok(None);
    }

    let parent = repo
        .git(&["rev-parse", "--verify", "--quiet", &queue_ref(who)])
        .ok()
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty());

    // Nothing changed since the last mirror: no commit, no push, no noise.
    if let Some(parent) = &parent
        && let Ok(previous) = repo.git(&["rev-parse", &format!("{parent}^{{tree}}")])
        && previous.trim() == tree
    {
        return Ok(None);
    }

    let message = "spoolway queue";
    let mut args: Vec<String> = vec!["commit-tree".into(), tree, "-m".into(), message.into()];
    if let Some(parent) = parent {
        args.push("-p".into());
        args.push(parent);
    }
    let commit = repo.git(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    Ok(Some(commit.trim().to_string()))
}

/// One task file as it exists on somebody's mirror.
struct Carried {
    name: String,
    task: Task,
}

/// Take tasks from someone else's mirror into this queue.
///
/// A move, not a copy: the files are removed from their mirror in the same
/// breath, so the machine that had them stops seeing them and no two
/// dispatchers can ever run one task. Runs entirely from this side, which is the
/// case that actually happens — the other person is on a beach.
pub fn adopt(repo: &Repo, from: &str, group: Option<&str>, only: &[String]) -> Result<Vec<String>> {
    if !repo.has_remote() {
        bail!("this repo has no remote, so there is no mirror to adopt from");
    }
    let source = queue_ref(from);
    repo.git(&["fetch", "--quiet", "origin", &format!("+{source}:{source}")])
        .with_context(|| format!("no mirror for `{from}` on origin"))?;

    let queue = crate::config::QUEUE_DIR;
    let listing = repo
        .git(&["ls-tree", "--name-only", &format!("{source}:{queue}")])
        .with_context(|| format!("`{from}`'s mirror has no queue in it"))?;

    let mut carried: Vec<Carried> = Vec::new();
    for name in listing
        .lines()
        .map(str::trim)
        .filter(|n| n.ends_with(".md"))
    {
        let body = repo.git(&["show", &format!("{source}:{queue}/{name}")])?;
        let task = Task::parse(repo.queue_dir().join(name), &body)
            .with_context(|| format!("reading `{name}` off {from}'s mirror"))?;

        let wanted = match (group, only.is_empty()) {
            (Some(group), _) => task.front.group.as_deref() == Some(group),
            (None, false) => only.iter().any(|id| id == task.id()),
            (None, true) => true,
        };
        if wanted {
            carried.push(Carried {
                name: name.to_string(),
                task,
            });
        }
    }

    if carried.is_empty() {
        bail!("nothing on `{from}`'s mirror matches");
    }

    // Every collision, before anything is written. Refusing halfway through
    // would leave the tasks already taken sitting in both queues, with the
    // source mirror unpruned — two dispatchers on one task, which is the single
    // thing this design exists to make impossible.
    let clashes: Vec<&str> = carried
        .iter()
        .filter(|c| repo.queue_dir().join(&c.name).exists())
        .map(|c| c.task.id())
        .collect();
    if !clashes.is_empty() {
        bail!(
            "already in this queue, and not overwritten: {} — rename or archive \
             {} before adopting",
            clashes.join(", "),
            if clashes.len() == 1 { "it" } else { "them" }
        );
    }

    std::fs::create_dir_all(repo.queue_dir())?;
    let mut adopted = Vec::new();
    for Carried { name, task } in &carried {
        let destination = repo.queue_dir().join(name);

        // The branch, and the placement that came with it. The worktree path on
        // the file is the other machine's; clearing it here saves the first pass
        // from having to notice, and means the board never shows a directory
        // that does not exist.
        let mut task = task.clone();
        if let Some(branch) = task.front.branch.clone() {
            fetch_branch(repo, from, &branch)?;
        }
        task.front.worktree_path = None;
        task.front.workspace_id = None;
        task.front.pane_id = None;
        task.front.tab_id = None;
        task.path = destination;
        task.save()?;
        adopted.push(task.id().to_string());
    }

    // And drop them from the mirror they came from, so it stops offering work
    // that is now somebody else's.
    prune_mirror(
        repo,
        from,
        &carried.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
    )?;
    Ok(adopted)
}

/// Bring one task branch across, preferring the real branch on origin and
/// falling back to the mirror's copy of work that was never published.
///
/// The local branch has to be created explicitly: a worktree cut for a branch
/// that exists only as a remote-tracking ref is cut *from the base* instead,
/// silently, taking none of the work with it.
fn fetch_branch(repo: &Repo, from: &str, branch: &str) -> Result<()> {
    if repo
        .git(&["rev-parse", "--verify", "--quiet", branch])
        .is_ok()
    {
        return Ok(());
    }

    let mirrored = branch_ref(from, branch);
    let published = repo
        .git(&[
            "fetch",
            "--quiet",
            "origin",
            &format!("+refs/heads/{branch}:{branch}"),
        ])
        .is_ok();
    if published {
        let _ = repo.git(&[
            "branch",
            "--set-upstream-to",
            &format!("origin/{branch}"),
            branch,
        ]);
        return Ok(());
    }

    if repo
        .git(&[
            "fetch",
            "--quiet",
            "origin",
            &format!("+{mirrored}:{branch}"),
        ])
        .is_ok()
    {
        return Ok(());
    }

    // Neither: the work only ever existed on a machine that is not here. The
    // task file's goal and acceptance criteria are intact, which is the part
    // that matters, so it is re-done rather than lost.
    Ok(())
}

/// Rewrite someone's mirror without the task files that have been taken.
fn prune_mirror(repo: &Repo, from: &str, taken: &[String]) -> Result<()> {
    let source = queue_ref(from);
    let queue = crate::config::QUEUE_DIR;
    let index = repo.prune_index_file();
    let _ = std::fs::remove_file(&index);

    let git = |args: &[&str]| -> Result<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo.root)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .with_context(|| format!("running `git {}`", args.join(" ")))?;
        if !out.status.success() {
            bail!(
                "`git {}` failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };

    git(&["read-tree", &source])?;
    for name in taken {
        let path = format!("{queue}/{name}");
        git(&["update-index", "--force-remove", "--", &path])?;
    }
    let tree = git(&["write-tree"])?.trim().to_string();
    let _ = std::fs::remove_file(&index);

    let parent = repo.git(&["rev-parse", &source])?.trim().to_string();
    let commit = repo.git(&["commit-tree", &tree, "-p", &parent, "-m", "spoolway adopt"])?;
    let commit = commit.trim().to_string();
    repo.git(&["update-ref", &source, &commit])?;
    // `--force-with-lease` against the sha this rewrite was built on, not a
    // blind `--force`: `source` is someone else's ref, and their own in-flight
    // `handover` pushes `+queue:queue` to it too. A blind force here would
    // overwrite that push and leave the task on both machines (review finding
    // 59). A stale lease fails the adopt loudly instead, for the operator to
    // retry.
    repo.git(&[
        "push",
        "--quiet",
        &format!("--force-with-lease={source}:{parent}"),
        "origin",
        &format!("{source}:{source}"),
    ])?;
    Ok(())
}

/// The before-you-leave half: mirror now, and offer to send back work that is
/// not worth carrying across.
///
/// Optional by construction — `adopt` works without it, at the cost of whatever
/// the last mirror pass did not catch.
pub fn handover(repo: &Repo, group: Option<&str>, reset: bool) -> Result<BTreeMap<String, String>> {
    let mut tasks = repo.tasks()?;
    if let Some(group) = group {
        tasks.retain(|t| t.front.group.as_deref() == Some(group));
    }
    if tasks.is_empty() {
        bail!("no tasks to hand over");
    }

    let mut outcome: BTreeMap<String, String> = BTreeMap::new();

    if reset {
        // A task whose branch is nowhere but this machine has its commits
        // nowhere but this machine. Its goal and acceptance criteria are the
        // real source of truth, so it is cheaper to redo than to inherit.
        //
        // Asked of the remote rather than inferred from the step it is on: step
        // ids are a project's to rename, and a list of them here would silently
        // stop matching the day someone did.
        let pipelines = crate::pipeline::Pipelines::load(&repo.root, &repo.config)?;
        for task in &mut tasks {
            let Ok(pipeline) = pipelines.for_task(task) else {
                continue;
            };
            let published = task.front.branch.as_deref().is_some_and(|branch| {
                repo.git(&["ls-remote", "--heads", "origin", branch])
                    .is_ok_and(|out| !out.trim().is_empty())
            });
            if published {
                continue;
            }
            task.set_stage(
                pipeline.entry(),
                Some("handed over: to be redone from the plan"),
            );
            // "Redone from the plan" has to mean it. The branch, the point it
            // was cut from, and the diff measured at its last cleanup all
            // describe the abandoned attempt; left on the file, the adopter's
            // next worktree is cut on the existing `task/<id>` on top of those
            // commits, with no status-log line saying why (review finding 60).
            // Cleared, the adopter cuts fresh from `base` and the branch is
            // not mirrored below.
            task.front.branch = None;
            task.front.cut_from = None;
            task.front.base_commit = None;
            task.save()?;
            outcome.insert(
                task.id().to_string(),
                format!("reset to `{}`", pipeline.entry()),
            );
        }
    }

    let pushed = mirror(repo, &tasks)?;
    outcome.insert(
        "mirror".to_string(),
        format!(
            "{} task(s), {} branch(es), {} snapshot(s) on {}",
            pushed.tasks,
            pushed.branches,
            pushed.snapshots,
            ref_for(&identity(repo)?)
        ),
    );
    // The report is the only place a person hears that the mirror pass let a
    // task go, and letting go is the half of adoption that happens here.
    if !pushed.released.is_empty() {
        outcome.insert(
            "released".to_string(),
            format!("adopted elsewhere: {}", pushed.released.join(", ")),
        );
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn git(dir: &Path, args: &[&str]) -> String {
        crate::repo::run(dir, "git", args)
            .unwrap_or_else(|e| panic!("git {args:?} in {dir:?}: {e:#}"))
    }

    /// One bare origin and two checkouts of it — yours and a colleague's. Real
    /// git throughout, because every behaviour here is about what git actually
    /// does with a ref nobody has checked out.
    fn two_machines(name: &str) -> (Repo, Repo) {
        let base = crate::scratch::root(&format!("handover-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let origin = base.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "plan/x"]);

        let make = |dir: PathBuf, email: &str| -> Repo {
            std::fs::create_dir_all(&dir).unwrap();
            git(&dir, &["init", "-q", "-b", "plan/x"]);
            git(&dir, &["config", "user.email", email]);
            git(&dir, &["config", "user.name", "t"]);
            git(&dir, &["remote", "add", "origin", origin.to_str().unwrap()]);
            std::fs::write(dir.join("README"), "hello\n").unwrap();
            git(&dir, &["add", "-A"]);
            git(&dir, &["commit", "-q", "-m", "root"]);
            let home = dir.join(".home");
            Repo {
                checkout: dir.clone(),
                root: dir,
                config: crate::config::Config::default(),
                home,
            }
        };

        let mine = make(base.join("mine"), "me@corp.com");
        git(&mine.root, &["push", "-q", "-u", "origin", "plan/x"]);
        let theirs = make(base.join("theirs"), "you@corp.com");
        git(&theirs.root, &["fetch", "-q", "origin"]);
        (mine, theirs)
    }

    fn queue(repo: &Repo, id: &str, stage: &str, branch: Option<&str>) -> Task {
        let raw = format!("---\nid: {id}\nstage: {stage}\ngroup: p1\n---\n## Goal\nA thing.\n");
        let mut task = Task::parse(repo.queue_dir().join(format!("{id}.md")), &raw).unwrap();
        task.front.branch = branch.map(str::to_string);
        task.front.base = Some("plan/x".into());
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        task.save().unwrap();
        task
    }

    /// Work committed on a task branch, exactly as a lane leaves it.
    fn work_on(repo: &Repo, branch: &str, file: &str) {
        git(&repo.root, &["checkout", "-q", "-b", branch]);
        std::fs::write(repo.root.join(file), "the work\n").unwrap();
        git(&repo.root, &["add", "--", file]);
        git(&repo.root, &["commit", "-q", "-m", "work"]);
        git(&repo.root, &["checkout", "-q", "plan/x"]);
    }

    #[test]
    fn a_mirror_carries_the_task_files_and_no_project_code() {
        let (mine, _) = two_machines("carries");
        let task = queue(&mine, "one", "implement", None);

        let pushed = mirror(&mine, &[task]).unwrap();
        assert_eq!(pushed.tasks, 1);

        let listing = git(
            &mine.root,
            &["ls-tree", "-r", "--name-only", &queue_ref("me@corp.com")],
        );
        assert!(listing.contains("queue/one.md"), "got: {listing}");
        assert!(
            !listing.contains("README"),
            "the mirror carried project files: {listing}"
        );
    }

    /// Nothing changed, so nothing is pushed: a pass every ten minutes must not
    /// write a commit every ten minutes.
    #[test]
    fn an_unchanged_queue_is_not_mirrored_again() {
        let (mine, _) = two_machines("unchanged");
        let task = queue(&mine, "one", "implement", None);

        assert_eq!(mirror(&mine, std::slice::from_ref(&task)).unwrap().tasks, 1);
        assert!(mirror(&mine, &[task]).unwrap().is_empty());
    }

    /// The heart of it: a colleague takes the task, and it leaves your queue in
    /// the same breath. Two dispatchers can never run one task because the file
    /// is only ever in one place.
    #[test]
    fn adopting_takes_the_task_out_of_the_queue_it_came_from() {
        let (mine, theirs) = two_machines("moves");
        let task = queue(&mine, "one", "implement", Some("task/one"));
        work_on(&mine, "task/one", "one.txt");
        mirror(&mine, &[task]).unwrap();

        let adopted = adopt(&theirs, "me@corp.com", Some("p1"), &[]).unwrap();
        assert_eq!(adopted, vec!["one".to_string()]);
        assert!(theirs.queue_dir().join("one.md").exists());

        // And it is gone from the mirror it came from, so a second adopt finds
        // nothing rather than handing the same task to two people.
        let again = adopt(&theirs, "me@corp.com", Some("p1"), &[]);
        assert!(again.is_err(), "the task was still on offer");
    }

    /// The failure this exists to prevent, found by probing the multiplexer: a
    /// worktree cut for a branch that exists only as a remote-tracking ref is
    /// cut from the *base* instead, silently, with none of the work on it. So
    /// adopt creates the local branch itself.
    #[test]
    fn an_adopted_task_gets_a_local_branch_with_the_work_on_it() {
        let (mine, theirs) = two_machines("branch");
        let task = queue(&mine, "one", "implement", Some("task/one"));
        work_on(&mine, "task/one", "one.txt");
        mirror(&mine, &[task]).unwrap();

        adopt(&theirs, "me@corp.com", None, &[]).unwrap();

        let head = git(&theirs.root, &["rev-parse", "task/one"]);
        let files = git(&theirs.root, &["ls-tree", "--name-only", "task/one"]);
        assert!(!head.trim().is_empty(), "no local branch was created");
        assert!(files.contains("one.txt"), "the work is missing: {files}");
    }

    /// Placement is the one part of a task file that is about a machine rather
    /// than about the work, so it does not travel.
    #[test]
    fn an_adopted_task_forgets_the_other_machines_placement() {
        let (mine, theirs) = two_machines("placement");
        let mut task = queue(&mine, "one", "implement", Some("task/one"));
        task.front.worktree_path = Some(PathBuf::from("/home/someone-else/worktrees/one"));
        task.front.workspace_id = Some("w7".into());
        task.front.pane_id = Some("w7:p1".into());
        task.save().unwrap();
        work_on(&mine, "task/one", "one.txt");
        mirror(&mine, &[task]).unwrap();

        adopt(&theirs, "me@corp.com", None, &[]).unwrap();

        let taken = Task::load(&theirs.queue_dir().join("one.md")).unwrap();
        assert_eq!(taken.front.worktree_path, None);
        assert_eq!(taken.front.workspace_id, None);
        assert_eq!(taken.front.pane_id, None);
        // What the work *is* survives untouched.
        assert_eq!(taken.front.branch.as_deref(), Some("task/one"));
        assert_eq!(taken.stage(), "implement");
    }

    /// The other half of the move, and the one that is easy to miss: the machine
    /// the task was taken *from* has to let go of it.
    ///
    /// Found end to end against a real forge. `adopt` removes the file from the
    /// mirror, but the mirror is push-only — so without this the source machine
    /// still had the task file, would keep dispatching it, and its very next
    /// mirror pass would put it back and undo the handover.
    #[test]
    fn the_machine_a_task_was_taken_from_lets_go_of_it() {
        let (mine, theirs) = two_machines("release");
        let one = queue(&mine, "one", "implement", None);
        let two = queue(&mine, "two", "implement", None);
        mirror(&mine, &[one, two]).unwrap();

        adopt(&theirs, "me@corp.com", None, &["one".to_string()]).unwrap();
        assert!(
            mine.queue_dir().join("one.md").exists(),
            "still mine for now"
        );

        let pushed = mirror(&mine, &mine.tasks().unwrap()).unwrap();

        assert_eq!(pushed.released, vec!["one".to_string()]);
        assert!(
            !mine.queue_dir().join("one.md").exists(),
            "it was not let go of"
        );
        assert!(
            mine.queue_dir().join("two.md").exists(),
            "the wrong task was dropped"
        );

        // And it stays gone: a second pass has nothing more to report.
        assert!(
            mirror(&mine, &mine.tasks().unwrap())
                .unwrap()
                .released
                .is_empty()
        );
    }

    /// A task adopted away while a lane on this machine is still running it is
    /// not yanked out from under that lane — its `spoolway report` would fail
    /// with "no task" and its usage go unbanked (review finding 61). It is
    /// kept and re-mirrored, so this machine holds on to the work its lane is
    /// doing; only the untouched sibling is let go.
    #[test]
    fn a_task_with_a_live_lane_is_kept_when_adopted_away() {
        let (mine, theirs) = two_machines("reap-running");
        let one = queue(&mine, "one", "implement", None);
        let two = queue(&mine, "two", "implement", None);
        mirror(&mine, &[one, two]).unwrap();

        let mut lanes: std::collections::HashMap<String, crate::dispatch::LaneRecord> =
            std::collections::HashMap::new();
        lanes.insert(
            "one · implement".to_string(),
            crate::dispatch::LaneRecord::readopted("one · implement", 0, &[]),
        );
        crate::dispatch::save_lane_records(&mine, &lanes).unwrap();

        adopt(
            &theirs,
            "me@corp.com",
            None,
            &["one".to_string(), "two".to_string()],
        )
        .unwrap();

        let pushed = mirror(&mine, &mine.tasks().unwrap()).unwrap();
        assert_eq!(pushed.released, vec!["two".to_string()]);
        assert!(
            mine.queue_dir().join("one.md").exists(),
            "a task with a live lane must not be released out from under it"
        );
        assert!(!mine.queue_dir().join("two.md").exists());

        // And it stays: re-mirrored, it never reads as taken again.
        assert!(
            mirror(&mine, &mine.tasks().unwrap())
                .unwrap()
                .released
                .is_empty()
        );
        assert!(mine.queue_dir().join("one.md").exists());
    }

    /// `handover --reset` says the task will be redone from the plan, so it
    /// clears the abandoned attempt's `branch`, `cut_from` and `base_commit`
    /// — otherwise the adopter's next worktree is cut on the old branch on
    /// top of the old commits (review finding 60).
    #[test]
    fn reset_clears_the_abandoned_attempts_branch_and_cut_point() {
        let (mine, _) = two_machines("reset");
        let mut task = queue(&mine, "one", "implement", Some("task/one"));
        task.front.cut_from = Some("task/dep".into());
        task.front.base_commit = Some("deadbeef".into());
        task.save().unwrap();
        work_on(&mine, "task/one", "one.txt");

        handover(&mine, None, true).unwrap();

        let after = Task::load(&mine.queue_dir().join("one.md")).unwrap();
        assert_eq!(after.front.branch, None);
        assert_eq!(after.front.cut_from, None);
        assert_eq!(after.front.base_commit, None);
        assert!(
            after
                .section("## Status Log")
                .unwrap_or_default()
                .contains("redone from the plan"),
            "the reset must leave a trace of itself"
        );
    }

    /// A task queued a moment ago is on neither side of that comparison, and
    /// must never be mistaken for one that was taken away.
    #[test]
    fn a_task_queued_since_the_last_mirror_is_never_reaped() {
        let (mine, _) = two_machines("fresh");
        let one = queue(&mine, "one", "implement", None);
        mirror(&mine, &[one]).unwrap();

        queue(&mine, "two", "implement", None);
        let pushed = mirror(&mine, &mine.tasks().unwrap()).unwrap();

        assert!(
            pushed.released.is_empty(),
            "released: {:?}",
            pushed.released
        );
        assert!(mine.queue_dir().join("two.md").exists());
    }

    /// A task id already here is a collision, not something to silently
    /// overwrite: both files describe real work.
    #[test]
    fn adopting_never_overwrites_a_task_already_in_this_queue() {
        let (mine, theirs) = two_machines("collision");
        let task = queue(&mine, "one", "implement", None);
        mirror(&mine, &[task]).unwrap();
        queue(&theirs, "one", "review", None);

        let err = adopt(&theirs, "me@corp.com", None, &[]).unwrap_err();
        assert!(err.to_string().contains("already in this queue"), "{err:#}");
        // And the local one is untouched.
        assert_eq!(
            Task::load(&theirs.queue_dir().join("one.md"))
                .unwrap()
                .stage(),
            "review"
        );
    }

    /// Work a lane never committed still leaves the laptop: the mirror snapshots
    /// the worktree, without putting anything on the task branch.
    #[test]
    fn uncommitted_work_is_snapshotted_onto_the_mirror() {
        let (mine, _) = two_machines("wip");
        work_on(&mine, "task/one", "one.txt");

        // A worktree of the task branch with something uncommitted in it.
        let worktree = mine.root.parent().unwrap().join("wt-one");
        git(
            &mine.root,
            &[
                "worktree",
                "add",
                "-q",
                worktree.to_str().unwrap(),
                "task/one",
            ],
        );
        std::fs::write(worktree.join("scratch.txt"), "not committed\n").unwrap();

        let mut task = queue(&mine, "one", "implement", Some("task/one"));
        task.front.worktree_path = Some(worktree.clone());
        task.save().unwrap();

        let pushed = mirror(&mine, &[task]).unwrap();
        assert_eq!(pushed.snapshots, 1, "nothing was snapshotted");

        let files = git(
            &mine.root,
            &[
                "ls-tree",
                "-r",
                "--name-only",
                &wip_ref("me@corp.com", "task/one"),
            ],
        );
        assert!(files.contains("scratch.txt"), "got: {files}");
        // The branch itself is untouched by the snapshot.
        let on_branch = git(&mine.root, &["ls-tree", "-r", "--name-only", "task/one"]);
        assert!(!on_branch.contains("scratch.txt"));
    }
}
