use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Project(PathBuf);

impl Project {
    fn new(label: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "spoolway-init-output-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create project");
        let status = Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&root)
            .status()
            .expect("run git init");
        assert!(status.success(), "git init failed");
        Self(root)
    }

    fn run(&self, args: &[&str]) -> Output {
        let home = self.0.join("home");
        let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(args)
            .current_dir(&self.0)
            .env("HOME", home)
            .output()
            .expect("run spoolway");
        assert!(
            output.status.success(),
            "spoolway failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn init(&self, provider: &str) -> Output {
        self.run(&[
            "init",
            "--provider",
            provider,
            "--tracker",
            "none",
            "--agent",
            "pi",
            "--model",
            "test-model",
        ])
    }
}

impl AsRef<Path> for Project {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is utf-8")
}

#[test]
fn fresh_init_prints_only_both_success_messages() {
    let project = Project::new("fresh");

    assert_eq!(
        stdout(&project.init("claude")),
        "Skills installed successfully.\nProject initialized successfully.\n"
    );
}

#[test]
fn repeat_init_that_adds_skills_omits_project_success() {
    let project = Project::new("repeat");
    project.init("claude");

    assert_eq!(
        stdout(&project.run(&["init", "--provider", "codex"])),
        "Skills installed successfully.\n"
    );
    assert!(project.as_ref().join(".claude/skills").is_dir());
    assert!(project.as_ref().join(".codex/skills").is_dir());
}

#[test]
fn repeat_init_keeps_actionable_warnings() {
    let project = Project::new("warning");
    project.init("claude");

    let output = stdout(&project.run(&["init", "--provider", "codex", "--agent", "codex"]));
    assert!(output.contains("--agent/--model/--tracker/--project-key were not applied"));
    assert!(output.ends_with("Skills installed successfully.\n"));
    assert!(!output.contains("Project initialized successfully."));
}

#[test]
fn forced_init_reports_that_the_project_was_initialized_again() {
    let project = Project::new("force");
    project.init("claude");

    assert_eq!(
        stdout(&project.run(&[
            "init",
            "--force",
            "--provider",
            "claude",
            "--tracker",
            "none",
            "--agent",
            "pi",
            "--model",
            "test-model",
        ])),
        "Skills installed successfully.\nProject initialized successfully.\n"
    );
}

#[test]
fn standalone_install_uses_the_concise_success_message() {
    let project = Project::new("install");
    project.init("claude");

    assert_eq!(
        stdout(&project.run(&["install", "codex"])),
        "Skills installed successfully.\n"
    );
}
