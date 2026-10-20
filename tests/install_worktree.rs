use std::path::{Path, PathBuf};
use std::process::Command;

/// A project with a committed `init` scaffold, in a directory of its own.
struct Project(PathBuf);

impl Project {
    fn new(label: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "spoolway-install-worktree-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create project");
        let project = Self(root);
        project.git(&project.0, &["init", "-q", "-b", "main"]);
        project.spoolway(
            &project.0,
            &["init", "--yes", "--provider", "claude", "--tracker", "none"],
        );
        project.git(&project.0, &["add", "-A"]);
        project.git(
            &project.0,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        project
    }

    fn git(&self, cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn spoolway(&self, cwd: &Path, args: &[&str]) {
        let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(args)
            .current_dir(cwd)
            .env("HOME", self.0.join("home"))
            .env("SPOOLWAY_SKIP_VERSION_CHECK", "1")
            .output()
            .expect("run spoolway");
        assert!(
            output.status.success(),
            "spoolway {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A linked worktree on its own branch, with its Claude skill folder
    /// removed so that anything found there afterwards was written by the
    /// command under test.
    fn worktree_without_skills(&self, label: &str, borrowed: bool) -> PathBuf {
        let wt = self.0.parent().unwrap().join(format!(
            "{}-{label}",
            self.0.file_name().unwrap().to_string_lossy()
        ));
        self.git(
            &self.0,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("task/{label}"),
                wt.to_str().unwrap(),
            ],
        );
        std::fs::remove_dir_all(wt.join(".claude/skills")).expect("remove worktree skills");
        if borrowed {
            std::fs::remove_dir_all(wt.join(".spoolway")).expect("remove worktree .spoolway");
        }
        wt
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(files_under(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// What the main checkout holds, with one shipped skill file changed by hand
/// so an install that reaches it either overwrites the edit or refuses.
const EDITED: &str = "edited in the main checkout\n";

fn edit_main_skill(project: &Project) -> PathBuf {
    let skill = files_under(&project.0.join(".claude/skills"))
        .into_iter()
        .next()
        .expect("init wrote a skill");
    std::fs::write(&skill, EDITED).expect("edit main skill");
    skill
}

/// `spoolway install` run inside a linked worktree that carries its own
/// `.spoolway/` writes that worktree's skill folder, and leaves the main
/// checkout's skill files and version marker as they were.
#[test]
fn install_in_a_linked_worktree_with_its_own_setup_writes_that_worktree() {
    let project = Project::new("own");
    let edited = edit_main_skill(&project);
    let main_marker = project.0.join(".spoolway/spoolway-version");
    std::fs::write(&main_marker, "0.10.0\n").unwrap();
    let wt = project.worktree_without_skills("own-wt", false);
    let wt_marker = wt.join(".spoolway/spoolway-version");
    std::fs::write(&wt_marker, "0.10.0\n").unwrap();

    project.spoolway(&wt, &["install", "claude"]);

    assert!(
        !files_under(&wt.join(".claude/skills")).is_empty(),
        "the worktree's skill folder was not written"
    );
    assert_eq!(std::fs::read_to_string(&edited).unwrap(), EDITED);
    assert!(
        main_marker.exists(),
        "the main checkout's marker was removed"
    );
    assert!(!wt_marker.exists(), "the worktree's marker was left behind");

    project.git(
        &project.0,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
}

/// `spoolway install` run inside a linked worktree that borrows the main
/// checkout's `.spoolway/` still writes that worktree's skill folder, and
/// leaves the main checkout's skill files as they were.
#[test]
fn install_in_a_borrowed_linked_worktree_writes_that_worktree() {
    let project = Project::new("borrowed");
    let edited = edit_main_skill(&project);
    let wt = project.worktree_without_skills("borrowed-wt", true);

    project.spoolway(&wt, &["install", "claude"]);

    assert!(
        !files_under(&wt.join(".claude/skills")).is_empty(),
        "the worktree's skill folder was not written"
    );
    assert_eq!(std::fs::read_to_string(&edited).unwrap(), EDITED);

    project.git(
        &project.0,
        &["worktree", "remove", "--force", wt.to_str().unwrap()],
    );
}
