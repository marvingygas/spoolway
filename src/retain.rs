//! Deletes byproducts old enough that losing them costs nothing but a log
//! nobody reads.
//!
//! A spoolway home mixes two kinds of directory. `queue/`, `pending/`,
//! `worktrees/`, `plans/` and `overrides/` hold work in flight — losing an
//! entry there loses work, and a patch under `overrides/` swept after
//! `retention.days` would silently change how a project runs rather than
//! merely lose a record of one. `system-prompts/`, `commands/`, `tracking/`,
//! `headless/` and `scratch/` hold what a pass left behind — a composed
//! prompt, a command step's log, a hook run, a headless lane's record and
//! log, a rebase's scratch worktree. Nobody reads any of those once the task
//! they belong to has left the queue, and nothing this binary does depends
//! on them still being there. This module ages them out under
//! `housekeeping.retention_days`; the first kind this never touches,
//! whatever its age — see [`crate::repo::Repo::byproduct_dirs`] for the one
//! place that split is written down.
//!
//! `archive/` is the third case. It holds tasks that reached `done`, it is
//! what `spoolway eval` and `queue add` read, and it is kept by default. It
//! ages out only under its own `housekeeping.archive_retention_days`, which
//! is `0` until a person sets it — `retention_days` never reaches it.
//!
//! Two of those carry state a task still in the queue needs.
//! `scratch/<id>` is what a lane is handed as `$SPOOLWAY_SCRATCH` — planner
//! output and all — and `headless/` holds the records a running lane is read
//! back through. An entry in either is spared for as long as its leading
//! task id names a file still in `queue/`, whatever stage that file sits on:
//! `paused` and `blocked` are stages a task rests on for longer than
//! `housekeeping.retention_days`, and a directory's own modification time does not move
//! while it only has files written *into* it. Only once the task is archived
//! do its scratch directory and its headless record age out like anything
//! else. See [`sweep_now`], which loads the queue once for this.
//!
//! `.spoolway/prompts/` in the checkout is not the `system-prompts/`
//! directory above, however alike the two names read. It holds the
//! project's tracked prompt templates, it is version-controlled, and
//! [`crate::version`] fingerprints it as part of how work is done. It was
//! in the swept list once, and that deleted checked-in files off any
//! working tree git had not rewritten inside `housekeeping.retention_days`.
//!
//! The shape is copied from [`crate::scratch`]'s own sweep of finished test
//! fixtures: a [`std::sync::Once`] so a pass runs at most once per process,
//! and [`SWEEP_LIMIT`] so a long-neglected home drains its backlog over
//! several runs rather than stalling the first command that happens to
//! trip it. What differs is the test an entry has to pass — `scratch`
//! checks whether the pid that named a directory is still alive, this
//! checks how old an entry's own modification time is — because a
//! byproduct's age is exactly the fact nobody else here needs, while a
//! finished worker's pid says nothing about how long its output is worth
//! keeping.
//!
//! `headless/logs/` is swept one file at a time and never as an entry of
//! `headless/`. Its own modification time moves whenever any lane starts, so
//! treated as one entry it would stay young while it lives and then be
//! deleted whole, fresh logs included.
//!
//! Sweeping `archive/` has one consequence worth stating plainly: `queue
//! add` resolves a `depends_on` against the queue and the archive, so an old
//! finished task that has aged out stops being nameable as a dependency —
//! see [`crate::commands::queue::check_dependencies_set`], which says so
//! when it refuses one. The sweep also removes a swept task's line from
//! `archive/index.jsonl` in the same pass, and never sweeps that file itself
//! — see [`crate::archive_index`].

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Once;
use std::time::{Duration, SystemTime};

use crate::repo::Repo;

/// Mirrors [`crate::scratch::SWEEP_LIMIT`] — see its own doc for why a
/// ceiling exists at all: deleting a large backlog takes real time, and a
/// sweep with no ceiling would spend it before the command that triggered
/// this ever ran, which reads as a hung command rather than as retention
/// working.
const SWEEP_LIMIT: usize = 2_000;

/// One second's worth of a day, for turning a retention key
/// (`housekeeping.retention_days` or `housekeeping.archive_retention_days`)
/// into a [`Duration`].
const SECS_PER_DAY: u64 = 86_400;

/// Sweep every byproduct directory in `repo`'s home, at most once per
/// process.
///
/// Called from every command's own startup rather than wired to one in
/// particular — the `Once` gate is what makes that safe: a `spoolway queue
/// list` right after a `spoolway report` in the same process sweeps nothing
/// twice.
pub fn sweep_once(repo: &Repo) {
    static DONE: Once = Once::new();
    DONE.call_once(|| sweep_now(repo));
}

/// `days` as a [`Duration`], or `None` for `0`, which both retention keys
/// read as "keep everything forever" — what every install did before they
/// existed, and still the choice of neither deleting nor guessing an age
/// nobody asked for.
///
/// Saturating rather than a plain `*`: a retention key is a config value with
/// no upper bound, and an overflow here has to fail toward keeping
/// everything rather than toward deleting it. A `days` this large saturates
/// to `u64::MAX` seconds — hundreds of billions of years — which reads as
/// "never sweep", not as the wrapped-around tiny age a plain multiply would
/// panic on in a debug build or silently produce in a release one.
fn max_age(days: u64) -> Option<Duration> {
    (days > 0).then(|| Duration::from_secs(days.saturating_mul(SECS_PER_DAY)))
}

fn sweep_now(repo: &Repo) {
    let mut budget = SWEEP_LIMIT;
    if let Some(max_age) = max_age(repo.config.housekeeping.retention_days) {
        budget -= sweep_byproducts(repo, max_age, budget);
    }
    if budget > 0
        && let Some(max_age) = max_age(repo.config.housekeeping.archive_retention_days)
    {
        sweep_archive(repo, max_age, budget);
    }
}

/// The directories under `housekeeping.retention_days`, within `limit`
/// deletions in all. Returns how many went.
fn sweep_byproducts(repo: &Repo, max_age: Duration, limit: usize) -> usize {
    // `scratch/` and `headless/` hold a queued task's live state — see this
    // module's own doc. Read the queue once, here, and pass it only to those
    // two directories: an entry whose leading task id is still in the queue
    // is spared, whatever its age.
    let queued = repo.queued_ids();
    let scratch = repo.scratch_dir();
    let headless = repo.headless_dir();

    let mut budget = limit;
    for dir in repo.byproduct_dirs() {
        if budget == 0 {
            break;
        }
        let guard = (dir == scratch || dir == headless).then_some(&queued);
        let mut removed = sweep_dir(&dir, max_age, budget, guard);
        if dir == headless {
            removed += sweep_dir(
                &crate::headless::logs_dir_in(&dir),
                max_age,
                budget.saturating_sub(removed),
                guard,
            );
        }
        budget = budget.saturating_sub(removed);
    }
    limit - budget
}

/// [`sweep_dir`] over `archive/`, with the index kept in step.
///
/// A swept task must leave `archive/index.jsonl` in the same pass, or the
/// index lists tasks whose files are gone. The archive index lock is held
/// from the currency check, which must see the folder as it was before any
/// deletion, to the rewrite, so another process archiving a task cannot have
/// its change stamped as this sweep's. With no lock the files are swept
/// anyway and the index is left to rebuild.
fn sweep_archive(repo: &Repo, max_age: Duration, limit: usize) -> usize {
    let guard = crate::archive_index::lock(repo);
    let was_current = guard.is_some() && crate::archive_index::is_current(repo);
    let removed = sweep_dir(&repo.archive_dir(), max_age, limit, None);
    if removed > 0
        && let Some(guard) = &guard
    {
        crate::archive_index::forget_missing(repo, guard, was_current);
    }
    removed
}

/// One pass over one directory: delete a top-level entry whose own
/// modification time is `max_age` or older, up to `limit` deletions.
/// Returns how many went, which is what lets the tests below see the
/// ceiling hold and lets [`sweep_now`] divide one budget across the
/// directories.
///
/// `queued` is `Some` only for `scratch/` and `headless/` (and its `logs/`) —
/// an entry whose leading task id ([`crate::mux::lane_task`]) is in that set
/// is a live task's and is never swept, however old it reads. `None`
/// everywhere else.
///
/// Not gated by `days == 0` itself — that check belongs to the one caller
/// that means it as "retention is off"; a test driving this directly passes
/// whatever age it wants to see swept.
fn sweep_dir(
    dir: &Path,
    max_age: Duration,
    limit: usize,
    queued: Option<&BTreeSet<String>>,
) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries.flatten() {
        if removed >= limit {
            break;
        }
        // A scratch directory or headless record whose task is still in the
        // queue is spared before its age is ever looked at — `spoolway
        // resume` continues a lane whose bookkeeping this would otherwise
        // have deleted.
        if let Some(queued) = queued
            && queued.contains(crate::mux::lane_task(
                entry.file_name().to_string_lossy().as_ref(),
            ))
        {
            continue;
        }
        // `headless/logs/` is swept by its own pass over its contents — see
        // this module's doc for why it is never one entry here.
        if entry.file_name() == crate::headless::LOGS_DIR
            && dir.ends_with(crate::headless::LANE_DIR)
        {
            continue;
        }
        // `archive/index.jsonl` is a map of the folder, not a task in it, and
        // its own age says nothing about whether it is wanted. Deleting it
        // would only force a rebuild that re-reads every surviving task.
        if entry.file_name() == crate::archive_index::FILE_NAME {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        // `Err` here means `modified` is in the future relative to `now` —
        // clock skew, or a file just written — and an entry that young is
        // kept, not swept, so this is folded into "too young to go" rather
        // than treated as an error.
        let age = now.duration_since(modified).unwrap_or_default();
        if age < max_age {
            continue;
        }
        let path = entry.path();
        // Counted whether or not the removal succeeded: a directory that
        // cannot be deleted is exactly the one a retry would spend the rest
        // of the budget on, the same reasoning `scratch::sweep_dir` uses.
        let result = if metadata.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        let _ = result;
        removed += 1;
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep dates a top-level directory by its own modification time
    /// exactly as it dates a file — and opening a directory to move its clock
    /// is platform-shaped, which is `scratch::set_mtime`'s to know.
    fn age(path: &Path, secs_ago: u64) {
        crate::scratch::set_mtime(path, SystemTime::now() - Duration::from_secs(secs_ago));
    }

    fn count(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().count()
    }

    /// The two entries the age check is actually for: one old enough to go,
    /// one not.
    #[test]
    fn an_entry_past_the_age_goes_and_one_inside_it_stays() {
        let dir = crate::scratch::root("retain-age");
        std::fs::create_dir_all(&dir).unwrap();

        let old = dir.join("old.log");
        std::fs::write(&old, "stale").unwrap();
        age(&old, 31 * SECS_PER_DAY);

        let fresh = dir.join("fresh.log");
        std::fs::write(&fresh, "still warm").unwrap();
        age(&fresh, 2 * SECS_PER_DAY);

        let removed = sweep_dir(
            &dir,
            Duration::from_secs(30 * SECS_PER_DAY),
            SWEEP_LIMIT,
            None,
        );

        assert_eq!(removed, 1);
        assert!(!old.exists(), "an entry past the age was kept");
        assert!(fresh.exists(), "an entry inside the age was swept");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `days = 0` means nothing is ever deleted, checked at the level that
    /// actually reads the config key rather than at `sweep_dir` — see its
    /// own doc for why the zero check does not live there. Each key's zero
    /// guards its own folders only.
    #[test]
    fn days_zero_sweeps_nothing() {
        let base = crate::scratch::root("retain-zero");
        let _ = std::fs::remove_dir_all(&base);
        let mut config = crate::config::Config::default();
        config.housekeeping.retention_days = 0;
        config.housekeeping.archive_retention_days = 0;
        let repo = Repo {
            checkout: base.to_path_buf(),
            root: base.to_path_buf(),
            config,
            home: base.join(".home"),
        };

        // Real directories, not one of the test's own invention: `sweep_now`
        // reads `retention_days` for `repo.byproduct_dirs()` and
        // `archive_retention_days` for `archive/`, so each key's zero is
        // checked against a file in a folder it governs.
        let old = repo.archive_dir().join("ancient.md");
        std::fs::write(&old, "done").unwrap();
        age(&old, 365 * SECS_PER_DAY);
        let prompt = repo.system_prompts_dir().join("ancient \u{b7} step.md");
        std::fs::write(&prompt, "composed").unwrap();
        age(&prompt, 365 * SECS_PER_DAY);

        sweep_now(&repo);
        assert!(
            old.exists(),
            "archive_retention_days = 0 must not be reached by the sweep at all"
        );
        assert!(
            prompt.exists(),
            "retention_days = 0 must not be reached by the sweep at all"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// `retention_days` no longer reaches `archive/`: an archive file far past
    /// it, and its index line, stay while `archive_retention_days` is 0, and
    /// both go once that key is set.
    #[test]
    fn retention_days_keeps_the_archive_until_its_own_key_is_set() {
        let base = crate::scratch::root("retain-archive-key");
        let _ = std::fs::remove_dir_all(&base);
        let mut config = crate::config::Config::default();
        config.housekeeping.retention_days = 30;
        assert_eq!(config.housekeeping.archive_retention_days, 0);
        let mut repo = Repo {
            checkout: base.to_path_buf(),
            root: base.to_path_buf(),
            config,
            home: base.join(".home"),
        };
        let file = repo.archive_dir().join("old.md");
        std::fs::write(&file, "---\nid: old\nstage: done\n---\n").unwrap();
        assert_eq!(crate::archive_index::read(&repo).len(), 1);
        age(&file, 400 * SECS_PER_DAY);
        let index = repo.archive_dir().join("index.jsonl");

        sweep_now(&repo);
        assert!(file.exists(), "retention_days swept the archive");
        assert!(
            std::fs::read_to_string(&index).unwrap().contains("\"old\""),
            "the index line went with nothing swept"
        );

        repo.config.housekeeping.archive_retention_days = 30;
        sweep_now(&repo);
        assert!(!file.exists(), "archive_retention_days did not sweep");
        assert!(
            !std::fs::read_to_string(&index).unwrap().contains("\"old\""),
            "the swept task's index line stayed"
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// An aged log inside `headless/logs/` goes by itself: the folder stays,
    /// and so does a fresh log beside it.
    #[test]
    fn an_aged_headless_log_goes_alone_and_the_logs_folder_stays() {
        let base = crate::scratch::root("retain-headless-logs");
        let _ = std::fs::remove_dir_all(&base);
        let mut config = crate::config::Config::default();
        config.housekeeping.retention_days = 30;
        let repo = Repo {
            checkout: base.to_path_buf(),
            root: base.to_path_buf(),
            config,
            home: base.join(".home"),
        };
        let logs = crate::headless::logs_dir_in(&repo.headless_dir());
        std::fs::create_dir_all(&logs).unwrap();
        let old = logs.join("gone \u{b7} implement.log");
        let fresh = logs.join("recent \u{b7} implement.log");
        std::fs::write(&old, "stale").unwrap();
        std::fs::write(&fresh, "warm").unwrap();
        age(&old, 60 * SECS_PER_DAY);
        // The folder reads as ancient too, as a quiet one would: swept as an
        // entry of `headless/` it would take the fresh log with it.
        age(&logs, 60 * SECS_PER_DAY);

        sweep_now(&repo);

        assert!(!old.exists(), "the aged log was kept");
        assert!(fresh.exists(), "a fresh log went with it");
        assert!(logs.exists(), "the logs folder was swept whole");
        std::fs::remove_dir_all(&base).ok();
    }

    /// The ceiling holds, the same property `scratch::sweep_dir` proves for
    /// its own sweep: a machine with a large backlog spends a bounded
    /// amount of time on it, and what is left over is the next pass's to
    /// take.
    #[test]
    fn a_sweep_stops_at_its_ceiling_and_the_rest_keeps() {
        let dir = crate::scratch::root("retain-ceiling");
        std::fs::create_dir_all(&dir).unwrap();
        for n in 0..5 {
            let path = dir.join(format!("old-{n}.log"));
            std::fs::write(&path, "stale").unwrap();
            age(&path, 31 * SECS_PER_DAY);
        }

        let max_age = Duration::from_secs(30 * SECS_PER_DAY);
        assert_eq!(
            sweep_dir(&dir, max_age, 2, None),
            2,
            "a pass ignored its ceiling"
        );
        assert_eq!(count(&dir), 3, "3 of 5 should remain");

        assert_eq!(sweep_dir(&dir, max_age, 2, None), 2);
        assert_eq!(
            sweep_dir(&dir, max_age, 2, None),
            1,
            "the last one is all that was left"
        );
        assert_eq!(sweep_dir(&dir, max_age, 2, None), 0, "nothing left to take");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Sweeping an aged archive file takes its line out of the index in the
    /// same pass, keeps every other line as it was, and spares the index file
    /// itself however old it reads.
    #[test]
    fn sweeping_an_archive_file_removes_only_its_index_line() {
        let base = crate::scratch::root("retain-index");
        let _ = std::fs::remove_dir_all(&base);
        let mut config = crate::config::Config::default();
        config.housekeeping.archive_retention_days = 30;
        let repo = Repo {
            checkout: base.to_path_buf(),
            root: base.to_path_buf(),
            config,
            home: base.join(".home"),
        };
        for id in ["old", "new"] {
            std::fs::write(
                repo.archive_dir().join(format!("{id}.md")),
                format!("---\nid: {id}\nstage: done\n---\n"),
            )
            .unwrap();
        }
        let before = crate::archive_index::read(&repo);
        assert_eq!(before.len(), 2);
        assert!(crate::archive_index::is_current(&repo));
        let kept_line = std::fs::read_to_string(repo.archive_dir().join("index.jsonl"))
            .unwrap()
            .lines()
            .find(|l| l.contains("\"new\""))
            .unwrap()
            .to_string();
        age(&repo.archive_dir().join("old.md"), 31 * SECS_PER_DAY);
        // A quiet archive is old all the way through: the folder and the
        // index (which carries the folder's time) both read as ancient, and
        // the index must still be spared and still count as current.
        let ancient = SystemTime::now() - Duration::from_secs(400 * SECS_PER_DAY);
        crate::scratch::set_mtime(&repo.archive_dir(), ancient);
        crate::scratch::set_mtime(&repo.archive_dir().join("index.jsonl"), ancient);
        assert!(crate::archive_index::is_current(&repo));

        sweep_now(&repo);

        assert!(!repo.archive_dir().join("old.md").exists());
        let raw = std::fs::read_to_string(repo.archive_dir().join("index.jsonl")).unwrap();
        assert_eq!(raw.lines().collect::<Vec<_>>(), [kept_line.as_str()]);
        assert!(crate::archive_index::is_current(&repo));
        std::fs::remove_dir_all(&base).ok();
    }

    /// A task paused or blocked for longer than `housekeeping.retention_days` keeps its
    /// scratch directory and its headless record, so `spoolway resume`
    /// continues a lane whose planner output and bookkeeping are intact. An
    /// entry whose task has left the queue ages out exactly as before.
    #[test]
    fn the_sweep_spares_a_queued_tasks_scratch_and_headless_state() {
        let base = crate::scratch::root("retain-live-task");
        let _ = std::fs::remove_dir_all(&base);
        let mut config = crate::config::Config::default();
        config.housekeeping.retention_days = 30;
        let repo = Repo {
            checkout: base.to_path_buf(),
            root: base.to_path_buf(),
            config,
            home: base.join(".home"),
        };

        // `held` is still in the queue — the stage on its file does not
        // matter to the sweep. `gone` was archived long ago and only its
        // leftovers are left on disk.
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        std::fs::write(
            repo.queue_dir().join("held.md"),
            "---\nid: held\nstage: paused\n---\n",
        )
        .unwrap();

        let held_scratch = repo.scratch_dir().join("held");
        std::fs::create_dir_all(&held_scratch).unwrap();
        std::fs::write(held_scratch.join("plan.md"), "planner output").unwrap();
        let held_record = repo.headless_dir().join("held · implement.json");
        std::fs::write(&held_record, "{}").unwrap();

        let gone_scratch = repo.scratch_dir().join("gone");
        std::fs::create_dir_all(&gone_scratch).unwrap();
        let gone_record = repo.headless_dir().join("gone · implement.json");
        std::fs::write(&gone_record, "{}").unwrap();

        for path in [&held_scratch, &held_record, &gone_scratch, &gone_record] {
            age(path, 60 * SECS_PER_DAY);
        }

        sweep_now(&repo);

        assert!(
            held_scratch.join("plan.md").exists(),
            "a queued task's scratch space was swept"
        );
        assert!(
            held_record.exists(),
            "a queued task's headless record was swept"
        );
        assert!(
            !gone_scratch.exists(),
            "an archived task's scratch space should have gone"
        );
        assert!(
            !gone_record.exists(),
            "an archived task's headless record should have gone"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    /// The sweep reaches the composed prompt a lane was handed and never the
    /// tracked `.spoolway/prompts/` a person wrote. The two accessors read
    /// alike, and the one time they were confused the sweep was pointed at
    /// committed source — a deletion no retention setting is meant to make.
    #[test]
    fn the_sweep_takes_composed_prompts_and_never_the_tracked_ones() {
        let base = crate::scratch::root("retain-prompt-source");
        let _ = std::fs::remove_dir_all(&base);
        let mut config = crate::config::Config::default();
        config.housekeeping.retention_days = 1;
        let repo = Repo {
            checkout: base.to_path_buf(),
            root: base.to_path_buf(),
            config,
            home: base.join(".home"),
        };

        let composed = repo.system_prompts_dir().join("aged \u{b7} step.md");
        std::fs::write(&composed, "a prompt nobody will read again").unwrap();
        age(&composed, 2 * SECS_PER_DAY);

        // Through `path_for` rather than by joining onto the prompts
        // directory here — see `commands::tests`, which holds that rule over
        // the whole tree. The sweep dates the entry directly under the
        // prompts directory, which is this file's parent.
        let source = crate::prompt::path_for(&repo, "implementer");
        let tracked = source.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&tracked).unwrap();
        std::fs::write(&source, "the prompt a person wrote").unwrap();
        age(&tracked, 2 * SECS_PER_DAY);

        sweep_now(&repo);

        assert!(!composed.exists(), "the composed prompt should have gone");
        assert!(
            source.exists(),
            "the tracked prompt is source, not a byproduct"
        );

        std::fs::remove_dir_all(&base).ok();
    }
}
