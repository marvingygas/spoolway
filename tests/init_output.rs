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

/// The two lines `init` closes a fresh run with. Not in the task mockup,
/// which is an excerpt of the run this task changes rather than a pin on
/// every line `init` prints — `docs/cli-reference.md` and
/// `docs/installation.md` both document these to a person.
const CLOSING_LINES: &str = "Project initialized successfully.\n\
     Set model and effort on every agent step in .spoolway/pipelines/*.yml before dispatching.\n";

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is utf-8")
}

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
fn fresh_init_prints_every_mockup_row_then_the_documented_closing_lines() {
    let project = Project::new("fresh");

    let result = project.init("claude");
    let id = std::fs::read_to_string(project.as_ref().join(".git/spoolway-id"))
        .expect("init stamps a project id");
    let id = id.trim();
    // The mockup's own counts (`6 pipelines`, `9 prompts`) are this
    // project's numbers as of an earlier draft, not a fixed contract — this
    // project ships two pipelines and five prompts. Printing the mockup's
    // literal digits regardless of what is actually on disk would be
    // printing something false; read the real counts instead and hold every
    // row's exact wording and column to the letter.
    let pipelines = std::fs::read_dir(project.as_ref().join(".spoolway/pipelines"))
        .expect("read pipeline dir")
        .count();
    let prompts = std::fs::read_dir(project.as_ref().join(".spoolway/prompts"))
        .expect("read prompt dir")
        .count();

    // The whole visible transcript, stdout and stderr both: every row the
    // mockup shows, in its order and column, and then the two closing lines
    // `docs/cli-reference.md` and `docs/installation.md` document. The
    // mockup stops at the skills line because it is an excerpt of the run
    // this task changes, not a pin on everything `init` prints — it elides
    // the tracker question the same way.
    assert_eq!(stderr(&result), "");
    assert_eq!(
        stdout(&result),
        format!(
            "  wrote    .spoolway/config.toml\n\
             {}\n\
             {}\n\
             \x20 stamped  .git/spoolway-id       {id}\n\
             Skills installed successfully.\n\
             {CLOSING_LINES}",
            wrote_row(".spoolway/pipelines/", pipelines, "pipeline"),
            wrote_row(".spoolway/prompts/", prompts, "prompt"),
        )
    );
    assert_scaffold(&project, "claude");
}

/// The mockup's `wrote` row: the same left column every `wrote`/`stamped`
/// row shares, followed by a count and the plural noun it counts.
fn wrote_row(path: &str, count: usize, noun: &str) -> String {
    format!(
        "  wrote    {path:<width$}{count} {noun}{plural}",
        width = 23,
        plural = if count == 1 { "" } else { "s" }
    )
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

/// A repeat `init` that restores nothing but a missing prompt *asset* —
/// `archivist`'s `document.md`, never its own `PROMPT.md` — must still say
/// it wrote to `.spoolway/prompts/`: the row counts prompt directories a
/// run touched at all, not only the ones whose own `PROMPT.md` was missing.
#[test]
fn repeat_init_that_restores_only_a_missing_prompt_asset_reports_the_prompts_row() {
    let project = Project::new("asset-only");
    project.init("claude");
    std::fs::remove_file(
        project
            .as_ref()
            .join(".spoolway/prompts/archivist/assets/document.md"),
    )
    .expect("remove the archivist prompt's document.md");

    let output = stdout(&project.run(&["init", "--provider", "claude"]));

    assert_eq!(
        output,
        format!(
            "{}\nSkills installed successfully.\n",
            wrote_row(".spoolway/prompts/", 1, "prompt")
        )
    );
    assert!(
        project
            .as_ref()
            .join(".spoolway/prompts/archivist/assets/document.md")
            .exists(),
        "the missing asset must actually be restored"
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
fn forced_init_reprints_every_wrote_row_and_closes_like_a_fresh_run() {
    let project = Project::new("force");
    project.init("claude");

    let result = project.run(&[
        "init",
        "--force",
        "--provider",
        "codex",
        "--tracker",
        "none",
    ]);
    let pipelines = std::fs::read_dir(project.as_ref().join(".spoolway/pipelines"))
        .expect("read pipeline dir")
        .count();
    let prompts = std::fs::read_dir(project.as_ref().join(".spoolway/prompts"))
        .expect("read prompt dir")
        .count();

    // `--force` rewrites the scaffold, so every `wrote` row fires again —
    // but the id is already stamped from the first `init`, and `--force`
    // does not re-stamp it, so there is no `stamped` row here at all.
    assert_eq!(
        stdout(&result),
        format!(
            "  wrote    .spoolway/config.toml\n\
             {}\n\
             {}\n\
             Skills installed successfully.\n\
             {CLOSING_LINES}",
            wrote_row(".spoolway/pipelines/", pipelines, "pipeline"),
            wrote_row(".spoolway/prompts/", prompts, "prompt"),
        )
    );
    assert_eq!(stderr(&result), "");
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

/// Where the `stamped` line sits among everything else a fresh `init`
/// prints. The mockup puts it last of the indented report rows — after the
/// provider prompt and every `wrote`/`removed` row — and immediately before
/// the skills result, not first, before `init` has done any of the work a
/// person actually asked for. A project carrying spoolway's old
/// `.gitignore` block is the one fresh run that prints a row above it, so
/// it is the only way to pin that order from the outside.
#[test]
fn a_fresh_init_prints_the_stamped_line_below_its_other_rows() {
    let project = Project::new("ordering");
    std::fs::write(
        project.as_ref().join(".gitignore"),
        "# >>> spoolway >>>\n.spoolway/queue/\n# <<< spoolway <<<\n",
    )
    .expect("write a legacy ignore block");

    let output = stdout(&project.init("claude"));
    let id = std::fs::read_to_string(project.as_ref().join(".git/spoolway-id"))
        .expect("init stamps a project id");
    let id = id.trim();

    let removed = "  removed .gitignore (spoolway's rules are gone)";
    let stamped = format!("  stamped  .git/spoolway-id       {id}");
    let lines: Vec<&str> = output.lines().collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|line| *line == needle)
            .unwrap_or_else(|| panic!("{needle:?} is not in {output:?}"))
    };
    assert!(at(removed) < at(&stamped), "{output:?}");
    assert_eq!(lines[at(&stamped) + 1], "Skills installed successfully.");
}

/// Every file and directory under `from`, copied to the same relative path
/// under `to` — a real `cp -r`, `.git` included, which is what lets a copy
/// carry the same stamp its original had. Used only to build the "still
/// exists, no longer carries the id" fixture below.
fn copy_dir_all(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create destination");
    for entry in std::fs::read_dir(from).expect("read source dir").flatten() {
        let dest = to.join(entry.file_name());
        if entry.file_type().expect("file type").is_dir() {
            copy_dir_all(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), &dest).expect("copy file");
        }
    }
}

/// Acceptance criterion 2 of `binding-record`, its "gone" cause, at the
/// level a unit test cannot reach: the one line a real command prints when
/// it moves a stale record, and only one — never once per file the check
/// touches, never once for every accessor a lane's worth of commands might
/// call.
#[test]
fn a_moved_checkout_prints_exactly_one_line_when_the_old_one_is_gone() {
    let project = Project::new("moved-gone");
    project.init("claude");
    let old = project.as_ref().to_path_buf();
    let renamed = old.parent().unwrap().join(format!(
        "{}-renamed",
        old.file_name().unwrap().to_string_lossy()
    ));
    std::fs::rename(&old, &renamed).expect("rename the checkout");
    let home = renamed.join("home"); // moved along with everything else.

    let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
        .args(["queue", "list"])
        .current_dir(&renamed)
        .env("HOME", &home)
        .output()
        .expect("run spoolway");
    assert!(
        output.status.success(),
        "spoolway failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = String::from_utf8_lossy(&output.stdout);
    let moves: Vec<&str> = out
        .lines()
        .filter(|line| line.contains("now records"))
        .collect();
    assert_eq!(moves.len(), 1, "expected exactly one move notice: {out:?}");

    std::fs::remove_dir_all(&renamed).ok();
}

/// The same criterion's other cause: a home's recorded checkout still
/// exists, but its own stamp has since moved on (`--new-id`, or a hand
/// edit) — a stale copy taken before that still carries the old id prints
/// the same one line, not two.
#[test]
fn a_re_stamped_checkout_prints_exactly_one_line_when_the_old_one_moved_on() {
    let project = Project::new("moved-restamped");
    project.init("claude");
    let root = project.as_ref().to_path_buf();
    let home = root.join("home");
    let old_id = std::fs::read_to_string(root.join(".git/spoolway-id"))
        .expect("read the original stamp")
        .trim()
        .to_string();

    // The original checkout mints itself a fresh id — the home keyed on
    // `old_id` is now stale, though the checkout on record for it still
    // exists right where it was.
    let restamp = Command::new(env!("CARGO_BIN_EXE_spoolway"))
        .args(["init", "--new-id"])
        .current_dir(&root)
        .env("HOME", &home)
        .output()
        .expect("run spoolway");
    assert!(
        restamp.status.success(),
        "spoolway failed: {}",
        String::from_utf8_lossy(&restamp.stderr)
    );

    // A second checkout — a full copy of the first, taken before the
    // restamp — still carries `old_id`.
    let copy = root.parent().unwrap().join("moved-restamped-copy");
    copy_dir_all(&root, &copy);
    std::fs::write(copy.join(".git/spoolway-id"), format!("{old_id}\n"))
        .expect("restore the old stamp on the copy");

    let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
        .args(["queue", "list"])
        .current_dir(&copy)
        .env("HOME", &home) // the same home both checkouts share.
        .output()
        .expect("run spoolway");
    assert!(
        output.status.success(),
        "spoolway failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let out = String::from_utf8_lossy(&output.stdout);
    let moves: Vec<&str> = out
        .lines()
        .filter(|line| line.contains("now records"))
        .collect();
    assert_eq!(moves.len(), 1, "expected exactly one move notice: {out:?}");

    std::fs::remove_dir_all(&copy).ok();
}
