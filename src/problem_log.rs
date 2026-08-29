//! One line per problem, per project, kept for the window a person could
//! plausibly still want it.
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

/// Where one project's problems accumulate: `~/.spoolway/logs/<basename of
/// the repo root>.log`.
///
/// Flat, and one file per project — unlike [`crate::mux::project_home`],
/// which nests a project's queue and archive under its own directory, this
/// sits beside every other project's log rather than inside any one of
/// them, since a problem worth keeping after a project's own home is wiped
/// by hand is exactly the kind this file is for.
pub fn path(repo: &Repo) -> PathBuf {
    crate::mux::home()
        .join(".spoolway")
        .join("logs")
        .join(format!("{}.log", crate::mux::project_label(&repo.root)))
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
/// — see both call sites in `src/commands/dispatch.rs`, which write here
/// whether or not a board is drawn and never let this stop a pass.
pub fn append(repo: &Repo, message: &str) {
    let _ = write_line(&path(repo), message);
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
    // fix and its reasoning in `usage::append`. Two calls to `dispatch.rs`
    // land in this file whether or not a board is drawn, so it is appended
    // to from more than one place in a single pass, let alone across runs.
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
}
