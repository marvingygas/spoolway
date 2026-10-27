//! One line per problem, and one per dispatcher stop, per project, kept for
//! the window a person could plausibly still want it.
//!
//! The board used to carry a pass's own trouble as an amber ticker line —
//! see the removed `RecentEvent::Note` in `src/status.rs`. That line pushed
//! whatever the queue was actually doing off the six rows a person has to
//! read at a glance, and it did so with the same string every pass until
//! somebody cleared it. This is where that trouble goes instead: appended
//! here, in full, and left off the board entirely.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::repo::Repo;

/// How long a line survives before [`open`] drops it.
const WINDOW_DAYS: i64 = 30;

/// Where one project's problems accumulate: `<repo.home's own
/// parent>/logs/<home's own directory name>.log` — `~/.spoolway/logs/…` for
/// a real project, since `repo.home` there is `~/.spoolway/<label>-<id>`.
///
/// Flat, and one file per project — unlike [`crate::mux::project_home`],
/// which nests a project's queue and archive under its own directory, this
/// sits beside every other project's log rather than inside any one of
/// them, since a problem worth keeping after a project's own home is wiped
/// by hand is exactly the kind this file is for.
///
/// Resolved against `repo.home`'s own parent, not `mux::home()` — a fixture
/// whose `home` sits entirely under a scratch directory has no `~/.spoolway`
/// to leak into, and the parent of whatever `home` it was given is always
/// the directory that one actually sits beside. See issue #188.
///
/// Named off `repo.home`'s own basename (`<label>-<id>`), not
/// `repo.root`'s — the label is frozen at a checkout's first stamp and the
/// id never changes under an ordinary rename, so a renamed checkout keeps
/// writing to the one log file it always has, rather than starting a
/// second one under its new basename. See the `binding-record` task.
pub fn path(repo: &Repo) -> PathBuf {
    let (logs, name) = logs_and_name(repo);
    logs.join(format!("{name}.log"))
}

/// The `logs/` folder beside the project's home, and the home's own name.
fn logs_and_name(repo: &Repo) -> (PathBuf, String) {
    let name = repo
        .home
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| crate::mux::project_label(&repo.root));
    // `repo.home` has no parent only for `/` or an empty path — never a real
    // `home`, which is always at least one component under something. Kept
    // rather than `.unwrap()`'d so a `repo.home` built by hand for a test
    // that skips that convention cannot panic this; the real `~/.spoolway`
    // is the nearest thing to a sane answer left.
    let state_root = repo
        .home
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(crate::mux::state_root);
    (state_root.join("logs"), name)
}

/// Where a failed command step's log is kept: `logs/<home's name>/`, beside
/// the problem log [`path`] names and named the same way.
///
/// Outside the home on purpose. Archiving a task deletes its `commands/`
/// files, and a later run of the same step truncates the log, so the path a
/// status line named there stops opening. A copy here outlives both, until
/// the retention sweep in `src/retain.rs` ages it out.
pub fn kept_dir(repo: &Repo) -> PathBuf {
    let (logs, name) = logs_and_name(repo);
    logs.join(name)
}

/// Copies a command step's `log` into [`kept_dir`] as `<task> · <step> ·
/// <local time as YYYY-MM-DDTHH-MM>.log` and returns the copy's path.
///
/// A name already taken gets ` · 2`, ` · 3` and so on before `.log`: two
/// failures of one step in the same minute would otherwise overwrite each
/// other. The copy is created with `create_new`, so the check and the claim
/// are one step and a second process cannot take the name in between.
///
/// An error, not a swallowed failure, because the caller must then keep the
/// status line pointing at the `commands/` path rather than at a copy that
/// is not there.
pub fn keep_command_log(repo: &Repo, task: &str, step: &str, log: &Path) -> Result<PathBuf> {
    let dir = kept_dir(repo);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut source =
        std::fs::File::open(log).with_context(|| format!("opening {}", log.display()))?;
    let stamp = chrono::Local::now().format("%Y-%m-%dT%H-%M");
    let sep = crate::mux::LANE_NAME_SEPARATOR;
    for attempt in 1u32.. {
        let suffix = match attempt {
            1 => String::new(),
            n => format!("{sep}{n}"),
        };
        let target = dir.join(format!("{task}{sep}{step}{sep}{stamp}{suffix}.log"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(mut copy) => {
                std::io::copy(&mut source, &mut copy).with_context(|| {
                    format!("copying {} to {}", log.display(), target.display())
                })?;
                return Ok(target);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", target.display()));
            }
        }
    }
    unreachable!("the attempt counter does not end")
}

/// Trims a project's log to the last thirty days. Called once, at the start
/// of a run — never on a write, so a pass that hits a dozen problems is a
/// dozen appends, not a dozen rewrites of everything already there.
///
/// A write that fails is swallowed: a run that could not tidy its own
/// postmortem is not a run that should stop for it.
pub fn open(repo: &Repo) {
    let _ = trim(&path(repo));
}

/// Appends one problem, timestamped now. Swallowed the same way [`open`] is
/// — see the call sites in `src/commands/dispatch.rs`, none of which ever
/// let this stop a pass. A pass's own trouble is written here whether or not
/// a board is drawn; `drain_keys`' failed keypress only where one is, since
/// there are no keys to read without a board to read them for.
pub fn append(repo: &Repo, message: &str) {
    let _ = write_line(&path(repo), message);
}

/// [`append`], for a caller that holds the log's path but not the `Repo` —
/// the dispatch tab's child, which outlives the call that started it.
pub fn append_at(path: &Path, message: &str) {
    let _ = write_line(path, message);
}

fn write_line(path: &Path, message: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let now = chrono::Local::now().to_rfc3339();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    // One `write_all` of an assembled buffer, not `writeln!` — see the same
    // fix and its reasoning in `usage::append`. Several calls in `dispatch.rs`
    // land in this file, so it is appended to from more than one place in a
    // single pass, let alone across runs.
    let line = format!("{now}  {message}\n");
    file.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

/// Drops every line whose own timestamp is more than [`WINDOW_DAYS`] old,
/// leaving everything else byte for byte — including a line whose timestamp
/// cannot be parsed, which is kept rather than guessed away: a parser a
/// future version gets wrong must never be what deletes someone's history.
///
/// A no-op, not an error, when there is nothing to trim yet.
fn trim(path: &Path) -> Result<()> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let cutoff = chrono::Local::now().timestamp() - WINDOW_DAYS * 24 * 3600;
    let lines: Vec<&str> = raw.lines().collect();
    let kept: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| match line.split_once("  ") {
            Some((ts, _)) => chrono::DateTime::parse_from_rfc3339(ts)
                .map(|parsed| parsed.timestamp() >= cutoff)
                .unwrap_or(true),
            None => true,
        })
        .collect();
    if kept.len() == lines.len() {
        return Ok(());
    }
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    std::fs::write(path, out).with_context(|| format!("trimming {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::repo::Repo;

    /// Two failures of one step inside the same minute leave two files, the
    /// second named with ` · 2`, and the first is not overwritten.
    #[test]
    fn a_taken_name_gets_a_number_instead_of_overwriting() {
        let base = crate::scratch::root("problem-log-kept-twice");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let repo = Repo {
            borrowed: false,
            root: base.to_path_buf(),
            checkout: base.to_path_buf(),
            config: Config::default(),
            home: base.join("proj-abc"),
        };
        let log = base.join("run.log");
        std::fs::write(&log, "first").unwrap();
        let one = keep_command_log(&repo, "t", "test", &log).unwrap();
        std::fs::write(&log, "second").unwrap();
        let two = keep_command_log(&repo, "t", "test", &log).unwrap();
        let name = two.file_name().unwrap().to_string_lossy().into_owned();
        // Both calls may straddle a minute; a differing stamp proves the
        // same thing, that nothing was overwritten.
        assert_ne!(one, two);
        assert!(name.ends_with(" · 2.log") || name != one.file_name().unwrap().to_string_lossy());
        assert_eq!(std::fs::read_to_string(&one).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&two).unwrap(), "second");
        assert_eq!(one.parent().unwrap(), base.join("logs").join("proj-abc"));
        std::fs::remove_dir_all(&base).ok();
    }

    /// A renamed checkout keeps writing to the one log file it always had —
    /// `repo.home`'s own basename is frozen at the checkout's first stamp
    /// and does not change with an ordinary rename, unlike `repo.root`'s.
    /// See the `binding-record` task's problem-log criterion.
    #[test]
    fn a_renamed_checkout_keeps_the_same_log_file() {
        let home = crate::scratch::root("problem-log-home-k7f2q9");
        let before = Repo {
            borrowed: false,
            root: crate::scratch::root("problem-log-before").to_path_buf(),
            checkout: crate::scratch::root("problem-log-before").to_path_buf(),
            config: Config::default(),
            home: home.to_path_buf(),
        };
        let after = Repo {
            borrowed: false,
            root: crate::scratch::root("problem-log-after-renamed").to_path_buf(),
            checkout: crate::scratch::root("problem-log-after-renamed").to_path_buf(),
            config: Config::default(),
            home: home.to_path_buf(),
        };

        assert_eq!(
            path(&before),
            path(&after),
            "the log path must follow the home, not the checkout's own basename"
        );
    }

    /// A file spanning both sides of the thirty-day window: two lines older
    /// than it, one right at the edge, one well inside it, and one whose
    /// timestamp cannot be parsed at all. Only the two old ones go.
    #[test]
    fn trim_drops_only_what_is_older_than_the_window() {
        let dir = crate::scratch::root("problem-log");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proj.log");

        let now = chrono::Local::now();
        let old = (now - chrono::Duration::days(45)).to_rfc3339();
        let edge = (now - chrono::Duration::days(31)).to_rfc3339();
        let fresh = (now - chrono::Duration::days(1)).to_rfc3339();
        let unparsable = "not-a-timestamp";

        let seed = format!(
            "{old}  ancient problem\n{edge}  just past the edge\n{fresh}  still recent\n{unparsable}  kept, unreadable\n"
        );
        std::fs::write(&path, &seed).unwrap();

        trim(&path).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(!after.contains("ancient problem"), "{after}");
        assert!(!after.contains("just past the edge"), "{after}");
        assert!(after.contains("still recent"), "{after}");
        assert!(after.contains("kept, unreadable"), "{after}");
    }

    /// No file yet is not an error — a fresh project's first problem should
    /// not have to survive a spurious failure on the way in.
    #[test]
    fn trim_is_a_no_op_when_there_is_nothing_to_trim() {
        let dir = crate::scratch::root("problem-log-missing");
        let path = dir.join("proj.log");
        assert!(trim(&path).is_ok());
    }

    /// Issue #188: `path` names the right basename but joins it onto
    /// `mux::home()` — the real `~/.spoolway` — instead of onto `repo.home`'s
    /// own parent. A fixture whose `home` sits entirely under a scratch
    /// directory should never have its problem log land anywhere outside
    /// that scratch directory.
    #[test]
    fn a_fixtures_problem_log_stays_under_the_fixtures_own_scratch_home() {
        let fixture_root = crate::scratch::root("problem-log-leak-home");
        let repo = Repo {
            borrowed: false,
            root: fixture_root.to_path_buf(),
            checkout: fixture_root.to_path_buf(),
            config: Config::default(),
            home: fixture_root.join("leak-id"),
        };

        let log_path = path(&repo);
        assert!(
            log_path.starts_with(&fixture_root),
            "the problem log escaped its fixture's scratch home: {} is not under {}",
            log_path.display(),
            fixture_root.display()
        );
    }
}
