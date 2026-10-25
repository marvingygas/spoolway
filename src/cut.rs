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
//!   The stop happens in [`stop_if_overdue`], which every pass runs over every
//!   cut of a task still in the queue, so a task that is not visited this
//!   pass — one that cannot get a worker slot, or that a person paused — does
//!   not hold a hung hook past its limit. The stop leaves its error in a note
//!   beside the run, and the next visit to the task reads it as that launch's
//!   failure.
//!
//! A task that leaves the queue while its cut is going — unqueued, discarded
//! or cleaned up — is no longer in that sweep. [`stop_and_reclaim`] ends its
//! cut first, so the hook does not run on unwatched, and a checkout the cut
//! finished after the task was gone does not stay behind for a later queue of
//! the same task to trip over.
//!
//! A run that vanished without an exit code is not a verdict. The machine went
//! down, or something killed the wrapper, so what it left running is stopped
//! and the cut starts again without spending a launch attempt — up to
//! [`MAX_CUT_KILLS`] times in a row, so a cut something kills every time is
//! still a failure in the end. The folder it may have left behind is cleared
//! by [`begin`], or adopted by the caller when git had finished writing it.

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

/// The note kept beside a run holding the `git worktree add` it runs, as it is
/// shown in an error.
fn shown_note(runs: &Runs, key: &str) -> std::path::PathBuf {
    runs.log_path(key).with_extension("shown")
}

/// The note kept beside a run holding the branch on its first line and the
/// checkout's path after it, so [`stop_and_reclaim`] can take the checkout
/// away without the caller knowing which backend cut it. It exists from the
/// start of a cut until the run's answer is read or the run is found
/// vanished, and is cleared with the other notes then.
fn where_note(runs: &Runs, key: &str) -> std::path::PathBuf {
    runs.log_path(key).with_extension("where")
}

/// Remove every note kept beside a run, the answer of an earlier run
/// included. A stopped task's `.stopped` note left behind would be read as
/// the failure of the cut that follows it.
fn clear_notes(runs: &Runs, key: &str) {
    for note in [fetch_note, made_note, shown_note, where_note, stopped_note] {
        let _ = std::fs::remove_file(note(runs, key));
    }
}

/// The note a stop leaves beside the run it stopped, holding the error the
/// task's next visit reports. Read before the run's exit code, because a hook
/// that caught the signal and exited `0` would otherwise read as a cut that
/// finished.
fn stopped_note(runs: &Runs, key: &str) -> std::path::PathBuf {
    runs.log_path(key).with_extension("stopped")
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
    // A cut past its limit is stopped first, if the pass's sweep has not
    // already. Once a stop has left its note, that error is this poll's
    // answer, whatever the run's files now say.
    stop_if_overdue_within(repo, task_id, path, branch, limit);
    let state = runs.state(&key);
    if state != RunState::Running
        && let Some(stopped) = take_stopped(&runs, &key)
    {
        return Err(stopped);
    }
    match state {
        RunState::Fresh => Ok(Poll::Idle),
        RunState::Running => Err(CutPending {
            task: task_id.to_string(),
            ran: runs.elapsed(&key).unwrap_or_default(),
        }
        .into()),
        RunState::Exited(0) => {
            clear_notes(&runs, &key);
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
                 `{branch}`. remove it with `git worktree remove --force --force {}`, then \
                 resume the task",
                path.display(),
                path.display()
            )
        }
        RunState::Exited(code) => {
            let base = std::fs::read_to_string(fetch_note(&runs, &key)).ok();
            let said = failure_text(&runs, &key);
            clear_notes(&runs, &key);
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
            // The run has no answer, but its notes are over with it. Left,
            // they would tell [`stop_and_reclaim`] this task's checkout is
            // still half cut, when a cut killed during its hook has left a
            // complete one that the caller adopts.
            clear_notes(&runs, &key);
            if kills >= MAX_CUT_KILLS {
                runs.forget(&key)?;
                bail!(
                    "{task_id}: the cut of its worktree was killed {kills} times in a row \
                     without finishing. see {} for what it printed, find what keeps ending it, \
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

/// Stop the task's cut if it is still going at its limit. Does nothing for a
/// cut that is under it, finished, or not there.
///
/// Called for every task in each pass, and again by [`poll`]: a cut must not
/// outlive its limit because its task was not visited.
pub(crate) fn stop_if_overdue(repo: &Repo, task_id: &str, path: &Path, branch: &str) {
    stop_if_overdue_within(repo, task_id, path, branch, crate::repo::GIT_HOOK_TIMEOUT)
}

fn stop_if_overdue_within(repo: &Repo, task_id: &str, path: &Path, branch: &str, limit: Duration) {
    let runs = runs(repo);
    let key = key(task_id);
    if runs.state(&key) != RunState::Running || stopped_note(&runs, &key).exists() {
        return;
    }
    if runs.elapsed(&key).unwrap_or_default() < limit {
        return;
    }
    // Looked at before the group is signalled: the hook is gone afterwards.
    let seen = runs
        .read_pid(&key)
        .and_then(|group| crate::repo::running_hook_name(&repo.root, group, &["post-checkout"]));
    let made = made_note(&runs, &key).exists();
    let base = std::fs::read_to_string(fetch_note(&runs, &key)).ok();
    let shown = std::fs::read_to_string(shown_note(&runs, &key))
        .unwrap_or_else(|_| format!("git worktree add {}", path.display()));
    runs.stop_the_group(&key);
    let stopped = crate::repo::hook_stopped_message(&shown, limit, seen, &["post-checkout"]);
    // The text is built as the synchronous cut built it: the stop with the
    // cut's context in front, or, when the half checkout could not be taken
    // away, the removal's own message alone with the command to run by hand.
    let text = match crate::mux::undo_stopped_cut(
        &repo.root,
        path,
        branch,
        !made,
        &stopped,
        "resume the task",
    ) {
        Ok(()) => format!(
            "{}: {stopped}",
            crate::mux::cut_failure(
                &format!(
                    "could not cut a worktree for `{branch}` at {}",
                    path.display()
                ),
                base.as_deref().unwrap_or_default(),
                base.is_some(),
            )
        ),
        Err(removal) => format!("{removal:#}"),
    };
    // Nothing to do if the note cannot be written: the run then reads as
    // vanished and the cut starts again, which is the stop's own cost.
    let _ = std::fs::write(stopped_note(&runs, &key), text);
}

/// The error a stop left for the task's next visit, read once: the run, its
/// notes and its log are cleared, so the visit after starts clean.
fn take_stopped(runs: &Runs, key: &str) -> Option<anyhow::Error> {
    let text = std::fs::read_to_string(stopped_note(runs, key)).ok()?;
    clear_notes(runs, key);
    let _ = runs.forget(key);
    Some(anyhow::anyhow!(text))
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
    // or that fails has them to read. Cleared first, whatever an earlier run
    // left: the answer of one cut must never be read as another's.
    clear_notes(&runs, &key);
    std::fs::create_dir_all(repo.cuts_dir())?;
    let _ = std::fs::write(
        where_note(&runs, &key),
        format!("{branch}\n{}", path.display()),
    );
    if command.fetch_timed_out {
        let _ = std::fs::write(fetch_note(&runs, &key), base);
    }
    if !command.branch_existed {
        let _ = std::fs::write(made_note(&runs, &key), "");
    }
    let _ = std::fs::write(
        shown_note(&runs, &key),
        format!("git {}", command.args.join(" ")),
    );
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

/// End this task's cut because the task is leaving the queue, and take away
/// what the cut made or half made: the checkout, and the branch if the cut
/// created it.
///
/// Acts only on a run that is going or has an answer nobody has read yet. A
/// task with no run on record has nothing of the cut's to remove: its
/// checkout is the task's own, whether the cut was answered, was adopted
/// after the run vanished, or was never needed, and its teardown removes it.
///
/// `next` finishes the advice "Run `<command>`, then ..." in the error, so
/// each caller says what its person does after removing the checkout. The
/// error is the removal's own and names the command to remove the checkout by
/// hand. The checkout is then locked as `initializing`, so a later queue of
/// the same task removes it, or says it cannot, instead of running in it.
pub(crate) fn stop_and_reclaim(repo: &Repo, task_id: &str, next: &str) -> Result<()> {
    let runs = runs(repo);
    let key = key(task_id);
    let cut = std::fs::read_to_string(where_note(&runs, &key)).ok();
    let made = made_note(&runs, &key).exists();
    let answered_or_none = runs.state(&key) == RunState::Fresh;
    // The group is signalled and waited for here, before the checkout is
    // taken away: a hook still running would write into a folder that is
    // being removed.
    runs.stop(&key);
    let undone = match cut.as_deref().and_then(|text| text.split_once('\n')) {
        Some((branch, path)) if !answered_or_none => crate::mux::undo_stopped_cut(
            &repo.root,
            Path::new(path),
            branch,
            !made,
            &format!(
                "{task_id}: the cut of its worktree is stopped because the task is leaving the queue."
            ),
            next,
        ),
        _ => Ok(()),
    };
    clear_notes(&runs, &key);
    reclaim(repo, task_id);
    undone
}

/// Drop what this task's cut left under `cuts/`. Nothing reads a finished
/// cut's log once the task is gone.
fn reclaim(repo: &Repo, task_id: &str) {
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

        let text = format!("{err:#}");
        assert!(text.contains("post-checkout"), "{text}");
        assert!(
            text.starts_with("could not cut a worktree for `task/t1` at "),
            "{text}"
        );
        assert!(
            text.contains("`git worktree add --no-track -b task/t1 "),
            "{text}"
        );
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

    /// A stop does not need its task to be visited: the sweep stops the cut
    /// at its limit, and the visit that comes later reads the stop as the
    /// failed launch it is, instead of finding nothing and cutting again with
    /// no strike spent.
    #[test]
    fn a_cut_stopped_by_the_sweep_is_a_failure_for_the_next_visit() {
        let (repo, _guard) = fixture("swept");
        let child = repo.root.join("child.pid");
        hook(
            &repo,
            &format!(
                "trap 'exit 0' TERM; sleep 300 & echo $! >'{}'; wait",
                child.display()
            ),
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

        stop_if_overdue_within(&repo, "t1", &path, "task/t1", Duration::ZERO);

        assert_eq!(
            running(&repo, "t1"),
            None,
            "the stopped cut still reads as going"
        );
        let err = poll_within(&repo, "t1", &path, "task/t1", Duration::from_secs(60))
            .expect_err("a hook that exited 0 after the stop was read as a finished cut");
        let text = format!("{err:#}");
        assert!(text.contains("was stopped"), "{text}");
        // Naming the hook reads `/proc`, which only Linux has; elsewhere the
        // message lists the hooks it could have been.
        #[cfg(target_os = "linux")]
        assert!(text.contains("was stopped after"), "{text}");
        assert!(!path.exists(), "the half checkout was left standing");
        assert_eq!(
            poll_within(&repo, "t1", &path, "task/t1", Duration::from_secs(60)).unwrap(),
            Poll::Idle,
            "the stop was reported twice"
        );
    }

    /// Whether `branch` exists in the repository.
    fn branch_exists(repo: &Repo, branch: &str) -> bool {
        crate::repo::run(
            &repo.root,
            "git",
            &["rev-parse", "--verify", "--quiet", branch],
        )
        .is_ok()
    }

    /// A task that leaves the queue while its hook is running takes the cut
    /// with it: the hook and its children are stopped, and neither the
    /// half checkout nor the branch the cut made is left for a later queue
    /// of the same task to trip over.
    #[test]
    fn a_task_that_leaves_mid_cut_has_its_hook_stopped_and_nothing_left_behind() {
        let (repo, _guard) = fixture("leaves");
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
        assert!(running(&repo, "t1").is_some());

        stop_and_reclaim(&repo, "t1", "go on").unwrap();

        let child: u32 = std::fs::read_to_string(&child)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!crate::headless::alive(child), "the hook's child lived on");
        assert_eq!(running(&repo, "t1"), None);
        assert!(!path.exists(), "the half checkout was left standing");
        assert!(
            !branch_exists(&repo, "task/t1"),
            "the branch the cut made was left behind"
        );
        let left = std::fs::read_dir(repo.cuts_dir())
            .map(|d| d.flatten().count())
            .unwrap_or(0);
        assert_eq!(left, 0, "files of the cut were left under cuts/");
    }

    /// A cut that finished on its own after nobody was looking leaves a
    /// complete checkout and a branch that belong to no task once the task is
    /// gone. They are taken away as well.
    #[test]
    fn a_cut_that_finished_unread_is_taken_away_when_its_task_leaves() {
        let (repo, _guard) = fixture("finished-unread");
        // Slow enough that the cut is still going when the pass moves on, so
        // nothing reads its answer.
        hook(&repo, "sleep 1");
        let path = target(&repo);
        // No wait at all: the pass moves on and the cut finishes unread.
        let _ = begin_within(
            &repo,
            "t1",
            &path,
            "task/t1",
            "main",
            Duration::from_secs(60),
            Duration::ZERO,
        );
        settle(&repo);

        stop_and_reclaim(&repo, "t1", "go on").unwrap();

        assert!(!path.exists(), "the finished checkout was left standing");
        assert!(!branch_exists(&repo, "task/t1"), "the branch was left");
    }

    /// A branch that was there before the cut is the task's own work and
    /// stays, whatever happens to the checkout.
    #[test]
    fn a_branch_the_cut_did_not_make_is_kept_when_its_task_leaves() {
        let (repo, _guard) = fixture("keeps-branch");
        // Slow enough that the cut is still going when the pass moves on, so
        // nothing reads its answer.
        hook(&repo, "sleep 1");
        crate::repo::run(&repo.root, "git", &["branch", "task/t1", "main"]).unwrap();
        let path = target(&repo);
        let _ = begin_within(
            &repo,
            "t1",
            &path,
            "task/t1",
            "main",
            Duration::from_secs(60),
            Duration::ZERO,
        );
        settle(&repo);

        stop_and_reclaim(&repo, "t1", "go on").unwrap();

        assert!(!path.exists());
        assert!(
            branch_exists(&repo, "task/t1"),
            "a branch of the task's was deleted"
        );
    }

    /// A cut killed while its hook runs leaves a finished checkout that the
    /// caller adopts as the task's own. When the task later leaves, that
    /// checkout and its branch are the task's work and must not be taken away
    /// as if the cut were still going.
    #[test]
    fn a_checkout_adopted_after_a_vanished_run_survives_its_task_leaving() {
        let (repo, _guard) = fixture("adopted");
        hook(
            &repo,
            "echo started >>\"$(dirname \"$0\")/../../hook-ran\"; sleep 30",
        );
        let path = target(&repo);
        let _ = begin_within(
            &repo,
            "t1",
            &path,
            "task/t1",
            "main",
            Duration::from_secs(60),
            Duration::ZERO,
        );
        let began = Instant::now();
        while !repo.root.join("hook-ran").exists() {
            assert!(began.elapsed() < Duration::from_secs(20));
            std::thread::sleep(Duration::from_millis(20));
        }
        // The whole group at once, wrapper included, so the run leaves no
        // exit code: the machine going down looks the same.
        let runs = runs(&repo);
        let group = runs.read_pid(&key("t1")).expect("the run has a pid");
        // SAFETY: a plain signal to the process group this test started.
        unsafe { libc::kill(-(group as libc::pid_t), libc::SIGKILL) };
        while runs.state(&key("t1")) != RunState::Interrupted {
            assert!(began.elapsed() < Duration::from_secs(20));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(poll(&repo, "t1", &path, "task/t1").unwrap(), Poll::Idle);
        assert!(crate::mux::cut_is_finished(&repo.root, &path, "task/t1"));

        stop_and_reclaim(&repo, "t1", "go on").unwrap();

        assert!(
            path.join(".git").exists(),
            "the adopted checkout was removed"
        );
        assert!(
            branch_exists(&repo, "task/t1"),
            "the adopted checkout's branch was deleted"
        );
    }

    /// A task with no cut has nothing to stop, and asking twice is harmless.
    #[test]
    fn stopping_the_cut_of_a_task_that_never_had_one_does_nothing() {
        let (repo, _guard) = fixture("never");

        stop_and_reclaim(&repo, "t1", "go on").unwrap();
        stop_and_reclaim(&repo, "t1", "go on").unwrap();
    }

    /// A stop note an earlier queue of the task left behind is not the failure
    /// of the cut that follows it.
    #[test]
    fn a_stale_stop_note_is_not_read_as_the_next_cuts_failure() {
        let (repo, _guard) = fixture("stale-stop");
        let path = target(&repo);
        std::fs::create_dir_all(repo.cuts_dir()).unwrap();
        std::fs::write(
            stopped_note(&runs(&repo), &key("t1")),
            "an earlier queue's stop",
        )
        .unwrap();

        begin(&repo, "t1", &path, "task/t1", "main").unwrap();

        assert!(crate::mux::cut_is_finished(&repo.root, &path, "task/t1"));
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
