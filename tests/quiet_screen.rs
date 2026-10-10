//! The dispatcher bare `spoolway` starts, `dispatch --from-screen`, driven
//! through the real binary: it prints no config note, so the board's "the
//! dispatcher did not start" popup shows the refusal alone, while a fatal
//! error still reaches stderr as `spoolway: …`. Bare `spoolway` itself needs a
//! terminal to parse as the screen, so this child is the one the same switch
//! can be proved on without one.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "spoolway-quiet-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("project")).expect("create project");
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(root.join("project"))
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
        Self(root)
    }

    fn project(&self) -> PathBuf {
        self.0.join("project")
    }

    /// `HOME` and git's own global config both sit inside the scratch
    /// folder, so no git identity is set: `dispatch` refuses over it, which
    /// is the fatal error these tests need after the config has loaded. The
    /// identity variables a caller's shell may export are removed, and
    /// `user.useConfigOnly` stops git guessing one from the hostname: with
    /// any identity found, `dispatch --from-screen` passes the check and
    /// loops for good, so `output` never returns.
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(args)
            .current_dir(self.project())
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("home/.config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "user.useConfigOnly")
            .env("GIT_CONFIG_VALUE_0", "true")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .env_remove("EMAIL")
            .env("SPOOLWAY_SKIP_VERSION_CHECK", "1")
            .env_remove("SPOOLWAY_TASK")
            .output()
            .expect("run spoolway")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("output is utf-8")
}

fn config_path(project: &Path) -> PathBuf {
    project.join(".spoolway/config.toml")
}

#[test]
fn the_screens_dispatcher_prints_its_refusal_and_no_note() {
    let scratch = Scratch::new("refusal");
    let init = scratch.run(&["init", "--yes", "--provider", "claude", "--tracker", "none"]);
    assert!(init.status.success(), "{}", text(&init.stderr));
    let mut config = std::fs::read_to_string(config_path(&scratch.project())).unwrap();
    config.push_str("\n[unatended]\nx = 1\n");
    std::fs::write(config_path(&scratch.project()), config).unwrap();

    // Any other command still prints the note, so the config above earns one.
    let list = scratch.run(&["queue", "list"]);
    assert!(
        text(&list.stderr).contains("note: `unatended`"),
        "{}",
        text(&list.stderr)
    );

    let child = scratch.run(&["dispatch", "--from-screen"]);
    let stderr = text(&child.stderr);
    assert!(!child.status.success(), "{stderr}");
    assert!(stderr.starts_with("spoolway: "), "{stderr}");
    assert!(!stderr.contains("note:"), "{stderr}");
}

#[test]
fn the_screens_dispatcher_still_prints_a_missing_project() {
    let scratch = Scratch::new("no-project");
    let child = scratch.run(&["dispatch", "--from-screen"]);
    let stderr = text(&child.stderr);
    assert_eq!(child.status.code(), Some(1), "{stderr}");
    assert!(stderr.starts_with("spoolway: "), "{stderr}");
}
