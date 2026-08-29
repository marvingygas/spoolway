//! Spoolway's own block in the project's `.gitignore`, on its way out.
//!
//! Runtime state left the checkout for `~/.spoolway/<project>/` — see
//! [`crate::repo::Repo::home`] — so there is nothing left for an ignore rule
//! to match. What is left of this module is the half that finds a marked
//! block and takes it back out, for a project that was set up before the
//! move: `spoolway init` and `spoolway update` both call [`remove`] once and
//! write no rules of their own ever again.
//!
//! The markers are the whole of what makes that safe: everything between them
//! was ours, and only the lines between them are touched. Everything around
//! them is the project's and is copied through unread.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::assets;
use crate::task::write_atomic;

/// The project's `.gitignore` — the one at the root, whether or not it exists.
pub fn file(root: &Path) -> PathBuf {
    root.join(".gitignore")
}

/// What [`remove`] did, in the words `init` and `update` report it with.
pub enum Removed {
    /// The block was there, and is gone now.
    Gone,
    /// There was no `.gitignore`, or it had no block of ours.
    Absent,
    /// A start marker with no end marker: someone deleted half the fence, and
    /// the lines below it could be anyone's. Nothing was written.
    Unterminated,
}

/// Take spoolway's marked block back out of the project's `.gitignore`, if it
/// is there.
///
/// Never creates a `.gitignore` that was not there already, and never writes
/// one back only to have removed its own block: a project with no file, or one
/// whose file holds nothing of ours, is [`Removed::Absent`] and is left
/// untouched.
pub fn remove(root: &Path, dry_run: bool) -> Result<Removed> {
    let path = file(root);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Removed::Absent),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };

    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let (first, last) = match bounds(&lines) {
        None => return Ok(Removed::Absent),
        Some(Err(())) => return Ok(Removed::Unterminated),
        Some(Ok(span)) => span,
    };

    lines.drain(first..=last);
    if !dry_run {
        let mut out = lines.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        write_atomic(&path, &out)?;
    }
    Ok(Removed::Gone)
}

/// Our block's first and last line, `Err` when only the start marker is there.
fn bounds(lines: &[String]) -> Option<Result<(usize, usize), ()>> {
    let first = lines
        .iter()
        .position(|line| line.trim() == assets::IGNORE_BEGIN)?;
    let last = lines
        .iter()
        .skip(first)
        .position(|line| line.trim() == assets::IGNORE_END)
        .map(|offset| first + offset);
    Some(last.map(|last| (first, last)).ok_or(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let dir = crate::scratch::root(&format!("gitignore-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn block() -> String {
        format!(
            "{}\n.spoolway/queue/\n.spoolway/archive/\n{}",
            assets::IGNORE_BEGIN,
            assets::IGNORE_END
        )
    }

    /// No `.gitignore` at all is left exactly that way: nothing is removed,
    /// and nothing is created to remove it from.
    #[test]
    fn a_missing_gitignore_is_left_missing() {
        let root = root("missing");
        assert!(matches!(remove(&root, false).unwrap(), Removed::Absent));
        assert!(!file(&root).exists());
    }

    /// A `.gitignore` with no block of ours is untouched — including the file
    /// itself, which is not rewritten just to confirm there was nothing to do.
    #[test]
    fn a_gitignore_with_no_block_is_left_alone() {
        let root = root("no-block");
        std::fs::write(file(&root), "/target\n*.log\n").unwrap();
        assert!(matches!(remove(&root, false).unwrap(), Removed::Absent));
        assert_eq!(
            std::fs::read_to_string(file(&root)).unwrap(),
            "/target\n*.log\n"
        );
    }

    /// The point of the change: only the marked block goes, and every line
    /// the project wrote around it is copied through unread.
    #[test]
    fn only_the_marked_block_is_removed() {
        let root = root("remove");
        std::fs::write(
            file(&root),
            format!("/target\n{}\n\n# mine, below\n*.log\n", block()),
        )
        .unwrap();

        assert!(matches!(remove(&root, false).unwrap(), Removed::Gone));
        let text = std::fs::read_to_string(file(&root)).unwrap();
        assert!(!text.contains(assets::IGNORE_BEGIN));
        assert!(text.starts_with("/target\n"));
        assert!(text.ends_with("# mine, below\n*.log\n"));

        // Idempotent: a second run finds nothing left to remove.
        assert!(matches!(remove(&root, false).unwrap(), Removed::Absent));
    }

    /// Half a fence means the lines under it could be anyone's, so they are
    /// nobody's to rewrite.
    #[test]
    fn half_a_fence_is_left_alone() {
        let root = root("unterminated");
        let before = format!("{}\n.spoolway/queue/\n", assets::IGNORE_BEGIN);
        std::fs::write(file(&root), &before).unwrap();

        assert!(matches!(
            remove(&root, false).unwrap(),
            Removed::Unterminated
        ));
        assert_eq!(std::fs::read_to_string(file(&root)).unwrap(), before);
    }

    /// A dry run reports what would happen without touching the file.
    #[test]
    fn a_dry_run_reports_without_writing() {
        let root = root("dry-run");
        let before = format!("/target\n{}\n", block());
        std::fs::write(file(&root), &before).unwrap();

        assert!(matches!(remove(&root, true).unwrap(), Removed::Gone));
        assert_eq!(std::fs::read_to_string(file(&root)).unwrap(), before);
    }
}
