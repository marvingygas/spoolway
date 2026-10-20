//! Cutting a task's worktree beside the dispatcher's pass.
//!
//! `git worktree add` runs the repository's `post-checkout` hook, and a hook
//! may take minutes or never exit. A pass that waited for the cut waited for
//! every task in the project with it, three times over, because a failed cut
//! is retried at once. So the cut runs as a detached process, the way a
//! command step does, and only the task being cut waits.
//!
//! ## A cut is a run, watched across passes
//!
//! The run is a [`crate::command_step::Runs`] under the project's `cuts/`
//! folder, keyed on the task. Its pid and exit files are the whole record,
//! so a dispatcher that is stopped, killed, or started again finds the cut
//! where it was: still going (and waits for it), finished (and reads its
//! answer), or gone without an answer (and cuts again). A task has one key,
//! so two cuts for one task cannot run beside each other.
//!
//! The run is given [`QUICK_CUT`] to finish before the pass moves on. A
//! repository with no slow hook cuts in milliseconds, and the task then
//! starts in the same pass it did before. Only a cut that is still going after
//! that is left to a later pass, and the task reads as `cutting worktree`.
//!
//! ## The three ways a cut ends
//!
//! - It exits `0`: the checkout is complete. The caller opens its workspace
//!   on it, and [`crate::mux::cut_worktree`] recognises it as already cut.
//! - It exits with anything else: an ordinary failed launch. The caller counts
//!   it against [`crate::dispatch::MAX_LAUNCH_FAILURES`], and the next pass
//!   tries again, so a cut that always fails still blocks the task on the
//!   third failure.
//! - It is still going at [`crate::repo::GIT_HOOK_TIMEOUT`]: the run's whole
//!   process group is stopped, so the hook and everything it started go with
//!   it, the half checkout is taken away, and that is a failed launch too.
//!
//! A run that vanished without an exit code is not a verdict. The machine went
//! down, or something killed the wrapper, so the half checkout is cleaned up
//! and the cut starts again without spending a launch attempt — up to
//! [`MAX_CUT_KILLS`] times in a row, so a cut something kills every time is
//! still a failure in the end.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::command_step::{RunState, Runs};
use crate::repo::Repo;

/// Where a cut's pid, exit code and log live, under the project's home — see
/// [`crate::repo::Repo::cuts_dir`].
pub(crate) const CUTS_DIR: &str = "cuts";

/// The step name a cut's run key is built with. It lives in a folder of its
/// own, so it cannot meet a pipeline step that happens to share the name.
const KEY_STEP: &str = "cut";

/// How long the pass waits for a cut it has just started. Long enough for a
/// repository with no slow hook, short enough that a hung one costs the pass
/// only this much, once.
const QUICK_CUT: Duration = Duration::from_millis(1500);

/// How many runs of one cut may vanish without an exit code, in a row, before
/// that counts as a failed launch.
const MAX_CUT_KILLS: u32 = 3;

/// How often the pass looks at a cut it has just started.
const QUICK_POLL: Duration = Duration::from_millis(20);

/// The answer for a task whose cut is still going. Not a failure: no launch
/// attempt is spent, and the task is looked at again on the next pass.
#[derive(Debug)]
pub(crate) struct CutPending {
    pub task: String,
    pub ran: Duration,
}

impl std::fmt::Display for CutPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: its worktree is still being cut ({} so far, stopped at {})",
            self.task,
            crate::tracking::minutes(self.ran),
            crate::tracking::minutes(crate::repo::GIT_HOOK_TIMEOUT),
        )
    }
}

impl std::error::Error for CutPending {}

/// What looking in on a task's cut found, when it did not fail.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Poll {
    /// No cut is running, and none has an answer waiting.
    Idle,
    /// A cut ended with `0` and its checkout is complete.
    Finished,
}

fn runs(repo: &Repo) -> Runs {
    Runs::new(&repo.cuts_dir())
}

fn key(task_id: &str) -> String {
    Runs::key(KEY_STEP, task_id)
}

/// The note kept beside a run that its fetch of the start point was stopped,
/// holding the start point, for the failure to name.
fn fetch_note(runs: &Runs, key: &str) -> std::path::PathBuf {
    runs.log_path(key).with_extension("fetch")
}

/// The note kept beside a run whose `git worktree add` made the branch, so
/// that stopping the run takes the branch away again.
fn made_note(runs: &Runs, key: &str) -> std::path::PathBuf {
    runs.log_path(key).with_extension("made")
}

/// How long this task's cut has been going, when one is.
pub(crate) fn running(repo: &Repo, task_id: &str) -> Option<Duration> {
    let runs = runs(repo);
    let key = key(task_id);
    (runs.state(&key) == RunState::Running).then(|| runs.elapsed(&key).unwrap_or_default())
}

/// Look in on the task's cut, reading its answer when it has one.
///
/// `Err` carries [`CutPending`] while the cut is going, and an ordinary error
/// for a cut that failed or was stopped, after which the next call starts
/// clean.
pub(crate) fn poll(repo: &Repo, task_id: &str, path: &Path, branch: &str) -> Result<Poll> {
    poll_within(repo, task_id, path, branch, crate::repo::GIT_HOOK_TIMEOUT)
}

/// [`poll`] with the limit passed in, so a test can prove the stop without
/// waiting out the real one.
fn poll_within(
    repo: &Repo,
    task_id: &str,
    path: &Path,
    branch: &str,
    limit: Duration,
) -> Result<Poll> {
    let runs = runs(repo);
    let key = key(task_id);
    match runs.state(&key) {
        RunState::Fresh => Ok(Poll::Idle),
        RunState::Running => {
            let ran = runs.elapsed(&key).unwrap_or_default();
            if ran < limit {
                return Err(CutPending {
                    task: task_id.to_string(),
                    ran,
                }
                .into());
            }
            Err(stop_overdue(repo, &runs, &key, path, branch, limit))
        }
        RunState::Exited(0) => {
            let _ = std::fs::remove_file(fetch_note(&runs, &key));
            let _ = std::fs::remove_file(made_note(&runs, &key));
            runs.forget(&key)?;
            // `git worktree add` can only exit `0` with the checkout in
            // place. Checked all the same, because the next step opens a
            // workspace on it and a cut that lied would put a pane in a
            // folder with nothing in it.
            if crate::mux::cut_is_finished(&repo.root, path, branch) {
                return Ok(Poll::Finished);
            }
            bail!(
                "{task_id}: `git worktree add` exited 0 but {} is not a finished checkout of \
                 `{branch}`. Remove it with `git worktree remove --force --force {}`, then \
                 resume the task",
                path.display(),
                path.display()
            )
        }
        RunState::Exited(code) => {
            let base = std::fs::read_to_string(fetch_note(&runs, &key)).ok();
            let said = failure_text(&runs, &key);
            let _ = std::fs::remove_file(fetch_note(&runs, &key));
            let _ = std::fs::remove_file(made_note(&runs, &key));
            runs.forget(&key)?;
            Err(anyhow::anyhow!(crate::mux::cut_failure(
                &format!(
                    "could not cut a worktree for `{branch}` at {}: `git worktree add` failed \
                     (exit status: {code}): {said}",
                    path.display()
                ),
                base.as_deref().unwrap_or_default(),
                base.is_some(),
            )))
        }
        RunState::Interrupted => {
            let kills = runs.note_kill(&key)?;
            // Stops what the wrapper left running and clears its files, and
            // keeps the count: `forget` would reset it.
            runs.discard(&key)?;
            if kills >= MAX_CUT_KILLS {
                runs.forget(&key)?;
                bail!(
                    "{task_id}: the cut of its worktree was killed {kills} times in a row \
                     without finishing. See {} for what it printed, find what keeps ending it, \
                     then resume the task",
                    runs.log_path(&key).display()
                );
            }
            Ok(Poll::Idle)
        }
    }
}

/// The last of what a failed cut printed, for the error to carry: git's own
/// reason is in it.
fn failure_text(runs: &Runs, key: &str) -> String {
    let log = std::fs::read_to_string(runs.log_path(key)).unwrap_or_default();
    let lines: Vec<&str> = log.lines().rev().take(8).collect();
    let tail: Vec<&str> = lines.into_iter().rev().collect();
    match tail.join(" ").trim() {
        "" => format!("it printed nothing; see {}", runs.log_path(key).display()),
        said => said.to_string(),
    }
}

/// A cut still going at its limit: stop its whole process group, take the half
/// checkout away, and say so. The error is a failed launch like any other, so
/// the task is tried again on a later pass and blocked on the third.
fn stop_overdue(
    repo: &Repo,
    runs: &Runs,
    key: &str,
    path: &Path,
    branch: &str,
    limit: Duration,
) -> anyhow::Error {
    // Looked at before the group is signalled: the hook is gone afterwards.
    let seen = runs
        .read_pid(key)
        .and_then(|group| crate::repo::running_hook_name(&repo.root, group, &["post-checkout"]));
    let made = made_note(runs, key).exists();
    let _ = std::fs::remove_file(fetch_note(runs, key));
    let _ = std::fs::remove_file(made_note(runs, key));
    // `stop` signals the group and clears the files, and the name is read
    // first. A run that cannot be cleared is still stopped.
    runs.stop(key);
    let stopped = crate::repo::hook_stopped_message(
        &format!("git worktree add {}", path.display()),
        limit,
        seen,
        &["post-checkout"],
    );
    match crate::mux::undo_stopped_cut(&repo.root, path, branch, !made, &stopped) {
        Ok(()) => anyhow::anyhow!(stopped),
        Err(removal) => removal,
    }
}

/// Start the cut and give it [`QUICK_CUT`] to finish.
///
/// `base` is where the branch starts, as `Mux::create_workspace` would have
/// been given it. Answers like [`poll`]: `Ok` only when the checkout is
/// complete, [`CutPending`] when it is still going.
pub(crate) fn begin(
    repo: &Repo,
    task_id: &str,
    path: &Path,
    branch: &str,
    base: &str,
) -> Result<()> {
    begin_within(
        repo,
        task_id,
        path,
        branch,
        base,
        crate::repo::GIT_HOOK_TIMEOUT,
        QUICK_CUT,
    )
}

fn begin_within(
    repo: &Repo,
    task_id: &str,
    path: &Path,
    branch: &str,
    base: &str,
    limit: Duration,
    quick: Duration,
) -> Result<()> {
    let runs = runs(repo);
    let key = key(task_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    tidy(repo, task_id, path)?;
    let command = crate::mux::cut_command(&repo.root, path, branch, base);
    // The notes are written before the run starts so a run that is stopped
    // or that fails has them to read. A stale one from an earlier run was
    // removed when that run's answer was read.
    let _ = std::fs::remove_file(fetch_note(&runs, &key));
    let _ = std::fs::remove_file(made_note(&runs, &key));
    std::fs::create_dir_all(repo.cuts_dir())?;
    if command.fetch_timed_out {
        let _ = std::fs::write(fetch_note(&runs, &key), base);
    }
    if !command.branch_existed {
        let _ = std::fs::write(made_note(&runs, &key), "");
    }
    let line = std::iter::once("git".to_string())
        .chain(command.args.iter().map(|a| crate::platform::quote(a)))
        .collect::<Vec<_>>()
        .join(" ");
    // `LC_ALL=C`: see [`crate::mux::CUT_ENV`], which says why the cut runs
    // in English.
    let env = crate::mux::CUT_ENV
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let pid = runs.start(&key, &line, &repo.root, &env)?;
    // The test hook that shoots the dispatcher while git is still writing the
    // checkout. Inert unless a fault is asked for.
    crate::fault::kill_run_at(&repo.root, crate::fault::CUT_DURING_ADD, pid, || {
        std::fs::read_dir(path)
            .is_ok_and(|mut entries| entries.any(|e| e.is_ok_and(|e| e.file_name() != ".git")))
    });

    let began = Instant::now();
    while runs.state(&key) == RunState::Running && began.elapsed() < quick {
        std::thread::sleep(QUICK_POLL);
    }
    match poll_within(repo, task_id, path, branch, limit)? {
        Poll::Finished => Ok(()),
        // The run vanished in the first moments, so there is no answer. The
        // next pass starts it again.
        Poll::Idle => Err(CutPending {
            task: task_id.to_string(),
            ran: Duration::ZERO,
        }
        .into()),
    }
}

/// Clear away a folder at `path` that no checkout owns, left by a cut that was
/// killed before git recorded it. `git worktree add` refuses to land in a
/// folder that is not empty, and that refusal is what used to spend a launch
/// attempt after a dispatcher was killed mid-cut.
///
/// A checkout git lists and has finished is somebody's work and is left alone;
/// the folder is then in the way, and the cut that follows says so.
fn tidy(repo: &Repo, task_id: &str, path: &Path) -> Result<()> {
    let listed = repo
        .git(&["worktree", "list", "--porcelain"])?
        .lines()
        .any(|line| {
            line.strip_prefix("worktree ").is_some_and(|rest| {
                crate::platform::resolve_through_existing(Path::new(rest))
                    == crate::platform::resolve_through_existing(path)
            })
        });
    if !path.exists() && !listed {
        return Ok(());
    }
    if listed && !repo.worktree_initializing(path)? {
        return Ok(());
    }
    crate::mux::remove_worktree(&repo.root, &repo.worktree_root(), path).with_context(|| {
        format!(
            "{task_id}: a folder is in the way of its worktree at {} and could not be removed; \
             run `{}`, then resume the task",
            path.display(),
            crate::mux::remove_by_hand_command(&repo.root, path)
        )
    })
}

/// Drop what this task's cut left under `cuts/`, once the task has been
/// archived. Nothing reads a finished cut's log after that.
pub(crate) fn reclaim(repo: &Repo, task_id: &str) {
    runs(repo).reclaim_task(task_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::os::unix::fs::PermissionsExt;

    /// A real repository with one commit on `main`, and a `Repo` over it.
    fn fixture(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let root = crate::scratch::root(&format!("cut-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        crate::repo::run(
            &root,
            "git",
            &["commit", "--allow-empty", "-q", "-m", "seed"],
        )
        .unwrap();
        (
            Repo {
                borrowed: false,
                checkout: root.to_path_buf(),
                root: root.to_path_buf(),
                config: Config::default(),
                home: root.join(".home"),
            },
            root,
        )
    }

    /// A `post-checkout` hook with `body`, the one `git worktree add` runs.
    fn hook(repo: &Repo, body: &str) {
        let hook = repo.root.join(".git/hooks/post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn target(repo: &Repo) -> std::path::PathBuf {
        repo.worktree_root().join("task-t1")
    }

    /// Wait for the run's wrapper to have ended, so its answer can be read.
    fn settle(repo: &Repo) {
        let runs = runs(repo);
        let key = key("t1");
        let began = Instant::now();
        while runs.state(&key) == RunState::Running {
            assert!(
                began.elapsed() < Duration::from_secs(30),
                "the cut never ended"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A cut with no slow hook finishes inside the quick wait, so the pass
    /// sees a finished checkout at once and the task starts in the same pass.
    #[test]
    fn a_quick_cut_is_finished_before_the_pass_moves_on() {
        let (repo, _guard) = fixture("quick");
        let path = target(&repo);

        begin(&repo, "t1", &path, "task/t1", "main").unwrap();

        assert!(crate::mux::cut_is_finished(&repo.root, &path, "task/t1"));
        assert_eq!(
            running(&repo, "t1"),
            None,
            "the finished run was not forgotten"
        );
        // The backend's own cut now adopts it instead of running git again.
        crate::mux::cut_worktree(&repo.root, &path, "task/t1", "main").unwrap();
    }

    /// The quick wait is bounded: a hook that is still going hands the pass
    /// back with the cut running, and a later look finds the same run
    /// instead of starting a second.
    #[test]
    fn a_slow_cut_leaves_the_pass_and_is_adopted_not_started_twice() {
        let (repo, _guard) = fixture("slow");
        hook(
            &repo,
            "echo started >>\"$(dirname \"$0\")/../../hook-ran\"; sleep 4",
        );
        let path = target(&repo);

        let began = Instant::now();
        let err = begin_within(
            &repo,
            "t1",
            &path,
            "task/t1",
            "main",
            Duration::from_secs(60),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert!(err.downcast_ref::<CutPending>().is_some(), "{err:#}");
        assert!(
            began.elapsed() < Duration::from_secs(3),
            "the pass waited for the hook"
        );
        assert!(running(&repo, "t1").is_some());

        let again = poll_within(&repo, "t1", &path, "task/t1", Duration::from_secs(60));
        assert!(again.unwrap_err().downcast_ref::<CutPending>().is_some());

        settle(&repo);
        assert_eq!(
            poll_within(&repo, "t1", &path, "task/t1", Duration::from_secs(60)).unwrap(),
            Poll::Finished
        );
        let ran = std::fs::read_to_string(repo.root.join("hook-ran")).unwrap();
        assert_eq!(
            ran.lines().count(),
            1,
            "the hook ran more than once: {ran:?}"
        );
    }

    /// A hook still going at the limit is stopped with everything it started,
    /// and the half checkout and the branch the cut made are taken away.
    #[test]
    fn a_cut_past_its_limit_is_stopped_with_its_children_and_cleaned_up() {
        let (repo, _guard) = fixture("overdue");
        let child = repo.root.join("child.pid");
        hook(
            &repo,
            &format!("sleep 300 & echo $! >'{}'; wait", child.display()),
        );
        let path = target(&repo);
        let _ = begin_within(
            &repo,
            "t1",
            &path,
            "task/t1",
            "main",
            Duration::from_secs(60),
            Duration::from_millis(100),
        );
        let began = Instant::now();
        while !child.exists() {
            assert!(began.elapsed() < Duration::from_secs(20));
            std::thread::sleep(Duration::from_millis(20));
        }

        let err = poll_within(&repo, "t1", &path, "task/t1", Duration::ZERO).unwrap_err();

        assert!(format!("{err:#}").contains("post-checkout"), "{err:#}");
        let child: u32 = std::fs::read_to_string(&child)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            !crate::headless::alive(child),
            "the hook's child outlived the stop"
        );
        assert!(!path.exists(), "the half checkout was left standing");
        assert!(
            crate::repo::run(
                &repo.root,
                "git",
                &["rev-parse", "--verify", "--quiet", "task/t1"]
            )
            .is_err(),
            "the branch this cut made was left behind"
        );
        // The run is gone, so the next attempt starts clean.
        assert_eq!(
            poll_within(&repo, "t1", &path, "task/t1", Duration::ZERO).unwrap(),
            Poll::Idle
        );
    }

    /// A cut that fails is an ordinary error carrying git's reason, and it is
    /// tried again from nothing on the next call.
    #[test]
    fn a_failed_cut_reports_gits_reason_and_the_next_call_starts_clean() {
        let (repo, _guard) = fixture("failed");
        let path = target(&repo);

        let err = begin(&repo, "t1", &path, "task/t1", "no-such-base").unwrap_err();

        let text = format!("{err:#}");
        assert!(err.downcast_ref::<CutPending>().is_none(), "{text}");
        assert!(
            text.contains("could not cut a worktree for `task/t1`"),
            "{text}"
        );
        assert!(text.contains("no-such-base"), "{text}");
        assert_eq!(
            poll(&repo, "t1", &path, "task/t1").unwrap(),
            Poll::Idle,
            "a failed run was not forgotten"
        );
    }

    /// A folder git has no record of, left by a cut killed before it was
    /// registered, is cleared away, not spent as a failed launch on
    /// `already exists`.
    #[test]
    fn a_folder_left_by_a_killed_cut_does_not_fail_the_next_one() {
        let (repo, _guard) = fixture("leftover");
        let path = target(&repo);
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::write(path.join("src/half.txt"), "x").unwrap();

        begin(&repo, "t1", &path, "task/t1", "main").unwrap();

        assert!(crate::mux::cut_is_finished(&repo.root, &path, "task/t1"));
        assert!(!path.join("src/half.txt").exists());
    }

    /// A run that vanished without an exit code is not a verdict: the cut
    /// starts again, and only the third such loss in a row is an error.
    #[test]
    fn a_run_that_vanished_is_cut_again_and_only_the_third_loss_is_an_error() {
        let (repo, _guard) = fixture("vanished");
        let path = target(&repo);
        let runs = runs(&repo);
        let key = key("t1");
        for lost in 1..=MAX_CUT_KILLS {
            // A pid file for a process that is not there, and no exit file.
            std::fs::create_dir_all(repo.cuts_dir()).unwrap();
            std::fs::write(runs.log_path(&key).with_extension("pid"), "999999\n").unwrap();
            let polled = poll(&repo, "t1", &path, "task/t1");
            match lost < MAX_CUT_KILLS {
                true => assert_eq!(polled.unwrap(), Poll::Idle),
                false => assert!(
                    format!("{:#}", polled.unwrap_err()).contains("killed 3 times"),
                    "the third loss was not an error"
                ),
            }
        }
    }
}
