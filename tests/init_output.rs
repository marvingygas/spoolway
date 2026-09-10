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
        self.run(&["init", "--provider", provider, "--tracker", "none"])
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

const CONFIGURE_STEPS: &str =
    "Set model and effort on every agent step in .spoolway/pipelines/*.yml before dispatching.\n";

fn assert_scaffold(project: &Project, agent: &str) {
    let config = std::fs::read_to_string(project.as_ref().join(".spoolway/config.toml"))
        .expect("read config");
    let profile_headers: Vec<&str> = config
        .lines()
        .filter(|line| line.starts_with("[agents."))
        .collect();
    let expected_header = format!("[agents.{agent}]");
    assert_eq!(profile_headers, [expected_header.as_str()]);
    assert!(
        config.contains(&format!("blocked_agent = \"{agent}\"")),
        "{config}"
    );
    assert!(
        config.contains(&format!("pipeline_agent = \"{agent}\"")),
        "{config}"
    );
    assert!(config.contains("blocked_model = \"\""), "{config}");
    assert!(config.contains("blocked_effort = \"\""), "{config}");

    for entry in
        std::fs::read_dir(project.as_ref().join(".spoolway/pipelines")).expect("read pipeline dir")
    {
        let pipeline =
            std::fs::read_to_string(entry.expect("pipeline entry").path()).expect("read pipeline");
        for line in pipeline
            .lines()
            .filter(|line| line.trim_start().starts_with("agent:"))
        {
            assert_eq!(line.trim(), format!("agent: {agent}"), "{pipeline}");
        }
        let agent_steps = pipeline.matches("    agent:").count();
        assert_eq!(
            pipeline.matches("    model: \"\"").count(),
            agent_steps,
            "{pipeline}"
        );
        assert_eq!(
            pipeline.matches("    effort: \"\"").count(),
            agent_steps,
            "{pipeline}"
        );
    }
}

#[test]
fn fresh_init_prints_success_and_the_single_configuration_instruction() {
    let project = Project::new("fresh");

    assert_eq!(
        stdout(&project.init("claude")),
        format!(
            "Skills installed successfully.\nProject initialized successfully.\n{CONFIGURE_STEPS}"
        )
    );
    assert_scaffold(&project, "claude");
}

#[test]
fn repeat_init_that_adds_skills_omits_project_success() {
    let project = Project::new("repeat");
    project.init("claude");
    let config_before = std::fs::read(project.as_ref().join(".spoolway/config.toml")).unwrap();
    let pipeline_before =
        std::fs::read(project.as_ref().join(".spoolway/pipelines/default.yml")).unwrap();

    assert_eq!(
        stdout(&project.run(&["init", "--provider", "codex"])),
        "Skills installed successfully.\n"
    );
    assert!(project.as_ref().join(".claude/skills").is_dir());
    let codex_plan = project
        .as_ref()
        .join(".agents/skills/spoolway-plan/SKILL.md");
    let codex_plan = std::fs::read_to_string(codex_plan).expect("read installed Codex plan skill");
    assert!(codex_plan.contains("request_user_input"));
    assert!(!codex_plan.contains("AskUserQuestion"));
    assert_eq!(
        std::fs::read(project.as_ref().join(".spoolway/config.toml")).unwrap(),
        config_before
    );
    assert_eq!(
        std::fs::read(project.as_ref().join(".spoolway/pipelines/default.yml")).unwrap(),
        pipeline_before
    );
}

#[test]
fn repeat_init_keeps_actionable_tracker_warnings() {
    let project = Project::new("warning");
    project.init("claude");

    let output = stdout(&project.run(&["init", "--provider", "codex", "--tracker", "github"]));
    assert!(output.contains("--tracker/--project-key were not applied"));
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
            "codex",
            "--tracker",
            "none",
        ])),
        format!(
            "Skills installed successfully.\nProject initialized successfully.\n{CONFIGURE_STEPS}"
        )
    );
    assert_scaffold(&project, "codex");
}

#[test]
fn init_rejects_the_removed_agent_and_model_flags() {
    let project = Project::new("removed-flags");
    for flag in ["--agent", "--model"] {
        let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(["init", flag, "old-answer"])
            .current_dir(project.as_ref())
            .env("HOME", project.as_ref().join("home"))
            .output()
            .expect("run spoolway");
        assert!(!output.status.success(), "{flag} was still accepted");
    }
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
