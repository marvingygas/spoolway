//! The layer's fingerprint — what the override consent gate checks.
//!
//! Every ledger line used to also carry a fingerprint of the whole tracked
//! configuration and the commit that last touched it, so runs could be
//! grouped by what they ran under. That is gone: a pipeline now carries its
//! own `version:`, a person's own note recorded straight onto the ledger
//! line — see [`crate::pipeline::Pipeline::version`] and
//! [`crate::dispatch::LaneRecord`]. What is left here is narrower: whether a
//! patch layer (see [`crate::overrides`]) is active, and which one, so
//! `dispatch`'s acknowledgement gate can tell a layer moved without needing
//! an opinion about anything else in `.spoolway/`.

use std::path::{Path, PathBuf};

use crate::repo::Repo;

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
/// in `files`, concatenated — so a rename or a genuinely different byte is a
/// new value either way.
///
/// Best-effort: a file that will not read is skipped rather than failing the
/// whole hash, because this is a record of the layer, not a part of it.
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

/// Fingerprint of the layer alone, `None` when there is nothing overridden.
pub fn layer_fingerprint(repo: &Repo) -> Option<String> {
    let mut files = Vec::new();
    files_under(&repo.overrides_dir(), &mut files);
    if files.is_empty() {
        return None;
    }
    Some(crate::skeleton::fingerprint(&material(&files, &repo.home)))
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
    fn different_layers_are_different_fingerprints() {
        let dir = tempdir();
        let repo = project(dir.path());
        std::fs::create_dir_all(dir.path().join(".home/overrides")).unwrap();
        std::fs::write(dir.path().join(".home/overrides/config.toml"), "a = 1\n").unwrap();
        let a = layer_fingerprint(&repo);
        std::fs::write(dir.path().join(".home/overrides/config.toml"), "a = 2\n").unwrap();
        let b = layer_fingerprint(&repo);
        assert_ne!(a, b, "two layers must carry different fingerprints");
    }

    #[test]
    fn no_layer_at_all_is_none() {
        let dir = tempdir();
        let repo = project(dir.path());
        assert!(
            layer_fingerprint(&repo).is_none(),
            "no layer directory at all"
        );

        std::fs::create_dir_all(repo.overrides_dir()).unwrap();
        assert!(
            layer_fingerprint(&repo).is_none(),
            "an overrides directory that holds nothing must read exactly like no directory"
        );
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
