//! What the pipeline *was* when a lane ran.
//!
//! Comparing what two versions of a setup cost only means something if every
//! ledger line says which setup it ran under. That is this module: a
//! fingerprint of the tracked configuration, taken when a lane settles and
//! written into its ledger line.
//!
//! Two values, because they answer different questions.
//!
//! The **fingerprint** is content, so it groups runs correctly even when the
//! edit that produced them was never committed. It is the identity: two lanes
//! share a version exactly when the files they ran under were byte-identical.
//!
//! The **commit** is a pointer into git, and it is the only one of the two
//! that a person could turn back into a diff by hand: `git log`, or `git show`
//! on the tracked parts a version covers.
//!
//! Neither alone does both jobs, which is why both are recorded. Where the
//! working tree has edits git has not seen, the commit is suffixed `+dirty`:
//! that version cannot be reconstructed from history, and saying so is better
//! than implying it can. Where a patch layer (see [`crate::overrides`]) is
//! also active, `+ovr` is suffixed too — composed with `+dirty` rather than
//! replacing it, since the two say different things and a run can be either,
//! neither or both. The layer's files fold into the fingerprint alongside
//! the tracked ones for the same reason the tracked ones are there at all: a
//! layer that changes what ran without changing what is stamped would make
//! two runs under different layers share one version.

use std::path::{Path, PathBuf};

use crate::repo::Repo;

/// What one lane's ledger line records about the setup it ran under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stamp {
    /// Eight hex digits over the tracked config's content.
    pub version: String,
    /// Short commit that last touched it, `+dirty` where the working tree has
    /// since diverged. Absent outside a git repository.
    pub commit: Option<String>,
}

/// The parts of `.spoolway/` that decide how work is done.
///
/// Deliberately not the whole directory. `queue/` and `archive/` are the work
/// itself rather than the setup, and they are untracked for the same reason —
/// folding them in would mint a new version on every task.
fn tracked_parts(repo: &Repo) -> Vec<PathBuf> {
    let state = repo.root.join(crate::config::STATE_DIR);
    vec![
        state.join(crate::config::CONFIG_FILE),
        state.join(crate::pipeline::PIPELINE_DIR),
        repo.prompts_dir(),
    ]
}

/// Every file under `path`, or `path` itself when it is a file, sorted.
///
/// Sorted because a fingerprint that depends on the order the filesystem
/// happened to hand back directory entries is a fingerprint that changes on a
/// different machine for no reason.
fn files_under(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    let mut found: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    found.sort();
    for entry in found {
        files_under(&entry, out);
    }
}

/// The path (relative to `root`, forward-slashed) and content of every file
/// in `files`, concatenated — shared by [`stamp`] and [`layer_fingerprint`]
/// so a rename or a genuinely different byte is a new value either way,
/// whichever of the two roots the files actually sit under.
///
/// Best-effort: a file that will not read is skipped rather than failing the
/// whole hash, because this is a record of the setup, not a part of it. The
/// worst case is a lane banked without a version, which reads as
/// `unversioned` and is honest about what is not known.
fn material(files: &[PathBuf], root: &Path) -> String {
    // The path goes into the hash alongside the content, so that renaming a
    // prompt is a new version even when nothing inside it changed — the step
    // that reads `reviewer.md` cares which file it is.
    let mut material = String::new();
    for file in files {
        let name = file
            .strip_prefix(root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        material.push_str(&name);
        material.push('\n');
        material.push_str(&content);
        material.push('\n');
    }
    material
}

/// Fingerprint and commit for the setup as it stands right now.
///
/// Best-effort throughout: a missing prompts directory, a repo git cannot be
/// asked about, a file that will not read — none of them are worth failing a
/// dispatch pass over, because this is a record of the pipeline, not a part of
/// it. The worst case is a lane banked without a version, which reads as
/// `unversioned` and is honest about what is not known.
pub fn stamp(repo: &Repo) -> Stamp {
    let parts = tracked_parts(repo);
    let mut tracked_files = Vec::new();
    for part in &parts {
        files_under(part, &mut tracked_files);
    }

    // The layer's own directory — absent entirely for a project that has
    // overridden nothing, which is how `files_under` already reads "off":
    // an empty `Vec` and no change to the material below.
    let mut layer_files = Vec::new();
    files_under(&repo.overrides_dir(), &mut layer_files);

    // Tracked files are named relative to `repo.root`; the layer lives under
    // `repo.home` instead — a different tree entirely on a real project — so
    // each gets its own root rather than one that would leave every layer
    // file's name as an unhelpful absolute path.
    let mut whole = material(&tracked_files, &repo.root);
    whole.push_str(&material(&layer_files, &repo.home));

    Stamp {
        version: crate::skeleton::fingerprint(&whole),
        commit: commit_of(repo, &parts, !layer_files.is_empty()),
    }
}

/// Fingerprint of the layer alone, `None` when there is nothing overridden —
/// isolated from [`stamp`]'s own so `dispatch`'s acknowledgement gate can
/// tell the layer itself moved without also tripping on an edit to the
/// tracked files beside it, which [`stamp`]'s fingerprint folds in too.
pub fn layer_fingerprint(repo: &Repo) -> Option<String> {
    let mut files = Vec::new();
    files_under(&repo.overrides_dir(), &mut files);
    if files.is_empty() {
        return None;
    }
    Some(crate::skeleton::fingerprint(&material(&files, &repo.home)))
}

/// The last commit that touched any of `parts`, marked `+dirty` when the
/// working tree has since moved on and `+ovr` when a patch layer is active —
/// composed rather than either replacing the other, since a run can be
/// dirty, layered, both or neither.
fn commit_of(repo: &Repo, parts: &[PathBuf], layered: bool) -> Option<String> {
    let relative: Vec<String> = parts
        .iter()
        .map(|p| {
            p.strip_prefix(&repo.root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    let mut log = vec!["log", "-1", "--format=%h", "--"];
    log.extend(relative.iter().map(String::as_str));
    let commit = repo.git(&log).ok()?;
    let commit = commit.trim();
    if commit.is_empty() {
        return None;
    }

    // Staged or unstaged, both count: what ran is what was on disk, and
    // neither state is in the commit this points at.
    let mut status = vec!["status", "--porcelain", "--"];
    status.extend(relative.iter().map(String::as_str));
    let dirty = repo
        .git(&status)
        .map(|out| !out.trim().is_empty())
        .unwrap_or(false);

    let mut suffixed = commit.to_string();
    if dirty {
        suffixed.push_str("+dirty");
    }
    if layered {
        suffixed.push_str("+ovr");
    }
    Some(suffixed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(dir: &Path) -> Repo {
        std::fs::create_dir_all(dir.join(".spoolway/pipelines")).unwrap();
        std::fs::create_dir_all(dir.join(".spoolway/prompts")).unwrap();
        std::fs::write(dir.join(".spoolway/config.toml"), "[dispatch]\n").unwrap();
        std::fs::write(
            dir.join(".spoolway/pipelines/default.yml"),
            "steps:\n  - id: implement\n    agent: pi\n    on_pass: done\n",
        )
        .unwrap();
        std::fs::write(dir.join(".spoolway/prompts/reviewer.md"), "review it\n").unwrap();
        Repo {
            root: dir.to_path_buf(),
            checkout: dir.to_path_buf(),
            config: crate::config::Config::default(),
            home: dir.join(".home"),
        }
    }

    #[test]
    fn the_same_files_fingerprint_the_same() {
        let a = tempdir();
        let b = tempdir();
        assert_eq!(
            stamp(&project(a.path())).version,
            stamp(&project(b.path())).version
        );
    }

    #[test]
    fn editing_a_prompt_is_a_new_version() {
        let dir = tempdir();
        let repo = project(dir.path());
        let before = stamp(&repo).version;
        std::fs::write(
            dir.path().join(".spoolway/prompts/reviewer.md"),
            "review it, briefly\n",
        )
        .unwrap();
        assert_ne!(before, stamp(&repo).version);
    }

    #[test]
    fn editing_a_pipeline_is_a_new_version() {
        let dir = tempdir();
        let repo = project(dir.path());
        let before = stamp(&repo).version;
        std::fs::write(
            dir.path().join(".spoolway/pipelines/default.yml"),
            "steps:\n  - id: build\n    agent: pi\n    on_pass: done\n",
        )
        .unwrap();
        assert_ne!(before, stamp(&repo).version);
    }

    #[test]
    fn renaming_a_prompt_is_a_new_version() {
        let dir = tempdir();
        let repo = project(dir.path());
        let before = stamp(&repo).version;
        std::fs::rename(
            dir.path().join(".spoolway/prompts/reviewer.md"),
            dir.path().join(".spoolway/prompts/auditor.md"),
        )
        .unwrap();
        assert_ne!(before, stamp(&repo).version);
    }

    #[test]
    fn different_layers_over_the_same_tracked_files_are_different_versions() {
        let dir = tempdir();
        let repo = project(dir.path());
        std::fs::create_dir_all(dir.path().join(".home/overrides")).unwrap();
        std::fs::write(dir.path().join(".home/overrides/config.toml"), "a = 1\n").unwrap();
        let a = stamp(&repo).version;
        std::fs::write(dir.path().join(".home/overrides/config.toml"), "a = 2\n").unwrap();
        let b = stamp(&repo).version;
        assert_ne!(
            a, b,
            "two layers over the same tracked files must carry different versions"
        );
    }

    /// The third case the criterion asks for: with no layer at all — not
    /// even an empty `overrides/` directory left behind by a promote that
    /// just cleared its last entry — the fingerprint reads exactly as it did
    /// before this change ever folded a layer in.
    #[test]
    fn the_fingerprint_is_unchanged_when_no_layer_is_present() {
        let dir = tempdir();
        let repo = project(dir.path());
        let before = stamp(&repo).version;
        assert_eq!(before, stamp(&repo).version, "no layer directory at all");

        std::fs::create_dir_all(repo.overrides_dir()).unwrap();
        assert_eq!(
            before,
            stamp(&repo).version,
            "an overrides directory that holds nothing must read exactly like no directory"
        );
        assert!(layer_fingerprint(&repo).is_none());
    }

    #[test]
    fn the_queue_is_not_part_of_the_version() {
        let dir = tempdir();
        let repo = project(dir.path());
        let before = stamp(&repo).version;
        std::fs::create_dir_all(dir.path().join(".spoolway/queue")).unwrap();
        std::fs::write(dir.path().join(".spoolway/queue/login.md"), "---\nid: x\n").unwrap();
        assert_eq!(
            before,
            stamp(&repo).version,
            "queueing work must not mint a new version of the setup"
        );
    }

    /// `+dirty` and `+ovr` compose rather than one replacing the other: a
    /// clean, layered run gets `+ovr` alone; the same tree with an uncommitted
    /// edit to a tracked file gets both, `+dirty` first as it already did
    /// before a layer existed at all.
    #[test]
    fn dirty_and_ovr_suffixes_compose() {
        let dir = tempdir();
        let repo = project(dir.path());
        crate::scratch::git_init(dir.path(), &[]);
        repo.git(&["add", "-A"]).unwrap();
        repo.git(&["commit", "-qm", "initial"]).unwrap();

        let clean = stamp(&repo).commit.unwrap();
        assert!(!clean.ends_with("+dirty"), "{clean}");
        assert!(!clean.ends_with("+ovr"), "{clean}");

        std::fs::create_dir_all(dir.path().join(".home/overrides")).unwrap();
        std::fs::write(dir.path().join(".home/overrides/config.toml"), "a = 1\n").unwrap();
        let layered = stamp(&repo).commit.unwrap();
        assert_eq!(layered, format!("{clean}+ovr"));

        std::fs::write(
            dir.path().join(".spoolway/prompts/reviewer.md"),
            "an uncommitted edit\n",
        )
        .unwrap();
        let both = stamp(&repo).commit.unwrap();
        assert_eq!(both, format!("{clean}+dirty+ovr"));
    }

    /// A scratch directory that removes itself, without a dev-dependency.
    struct TempDir(PathBuf);
    impl TempDir {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir() -> TempDir {
        let base = std::env::temp_dir().join(format!(
            "spoolway-version-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        TempDir(base)
    }
}
