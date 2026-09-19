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

/// One `kept`/`wrote`/`set` row, in the column every such row shares.
fn row(verb: &str, what: &str) -> String {
    format!("  {verb:<9}{what}")
}

/// Every path under `rel` (a `.spoolway/...` directory `init` populates),
/// relative to the project root, in the order a plain recursive walk of
/// disk finds them — not necessarily the order `init` itself considered
/// them in, which is why the tests below check each path's own row is
/// present rather than pinning the whole transcript's line order. What
/// paths actually exist is read off disk rather than duplicated here from
/// `assets::PROMPTS` and friends: this project's own prompt and hook lists
/// are not this test's to keep in sync by hand.
fn files_under(project: &Project, rel: &str) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read dir").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(&project.as_ref().join(rel), project.as_ref(), &mut out);
    out
}

/// Every file `init` places outside `.github/`, whatever tracker was
/// answered — the workflow is conditional on `github` and each call site
/// below adds it separately when it applies.
fn scaffold_paths(project: &Project) -> Vec<String> {
    let mut paths = vec![".spoolway/config.toml".to_string()];
    for dir in [
        ".spoolway/pipelines",
        ".spoolway/prompts",
        ".spoolway/templates/tasks",
        ".spoolway/templates/tracking",
        ".spoolway/hooks",
    ] {
        paths.extend(files_under(project, dir));
    }
    paths
}

/// Every one of `paths` has its own `verb` row somewhere in `stdout` —
/// acceptance criterion 3's "wrote or kept for every file it considered",
/// checked by membership rather than by line order, which nothing in that
/// criterion promises.
fn assert_report_rows(stdout: &str, verb: &str, paths: &[String]) {
    for path in paths {
        let needle = row(verb, path);
        assert!(stdout.contains(&needle), "missing `{needle}` in:\n{stdout}");
    }
}

#[test]
fn fresh_init_prints_every_mockup_row_then_the_documented_closing_lines() {
    let project = Project::new("fresh");

    let result = project.init("claude");
    let id = std::fs::read_to_string(project.as_ref().join(".git/spoolway-id"))
        .expect("init stamps a project id");
    let id = id.trim();
    let out = stdout(&result);

    // Every file a fresh run placed gets its own `wrote` row — the mockup's
    // own point — followed by the `stamped` row and the two closing lines
    // `docs/cli-reference.md` and `docs/installation.md` document.
    assert_eq!(stderr(&result), "");
    assert_report_rows(&out, "wrote", &scaffold_paths(&project));
    assert!(
        out.contains(&format!("  stamped  .git/spoolway-id       {id}")),
        "{out}"
    );
    assert!(
        out.ends_with(&format!("Skills installed successfully.\n{CLOSING_LINES}")),
        "{out}"
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

    let paths = scaffold_paths(&project);
    let out = stdout(&project.run(&["init", "--provider", "codex"]));

    // Nothing was missing, so every file gets a `kept` row rather than a
    // `wrote` one, and the run closes by saying so — no tracker flag was
    // given, so `[issue_tracking]` was never touched either.
    assert_report_rows(&out, "kept", &paths);
    assert!(out.contains("Skills installed successfully.\n"), "{out}");
    assert!(out.trim_end().ends_with("nothing to install."), "{out}");
    assert!(!out.contains("Project initialized successfully."));
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
/// `archivist`'s `document.md`, never its own `PROMPT.md` — reports that one
/// path `wrote`, and everything else it considered `kept`.
#[test]
fn repeat_init_that_restores_only_a_missing_prompt_asset_writes_that_one_path_and_keeps_the_rest() {
    let project = Project::new("asset-only");
    project.init("claude");
    let asset = project
        .as_ref()
        .join(".spoolway/prompts/archivist/assets/document.md");
    std::fs::remove_file(&asset).expect("remove the archivist prompt's document.md");

    let mut kept = scaffold_paths(&project);
    kept.retain(|p| p != ".spoolway/prompts/archivist/assets/document.md");

    let out = stdout(&project.run(&["init", "--provider", "claude"]));

    assert!(
        out.contains(&row(
            "wrote",
            ".spoolway/prompts/archivist/assets/document.md"
        )),
        "{out}"
    );
    assert_report_rows(&out, "kept", &kept);
    assert!(out.contains("Skills installed successfully.\n"), "{out}");
    assert!(
        asset.exists(),
        "the missing asset must actually be restored"
    );
}

/// `--project-key` given alone, with no `--tracker`, is still dropped on an
/// established project — the one refusal this task's own acceptance
/// criteria leaves alone.
#[test]
fn repeat_init_still_refuses_a_bare_project_key() {
    let project = Project::new("bare-project-key");
    project.init("claude");

    let out = stdout(&project.run(&["init", "--provider", "codex", "--project-key", "acme/app"]));
    assert!(
        out.contains("--project-key was not applied without --tracker"),
        "{out}"
    );
    let config = std::fs::read_to_string(project.as_ref().join(".spoolway/config.toml")).unwrap();
    assert!(!config.contains("acme/app"), "{config}");
}

/// Acceptance criterion 4: `--tracker` with a value answers `[issue_tracking]`
/// outright on an established project, editing the two keys into
/// `config.toml` in place rather than refusing — and answering `github`
/// also writes the workflow that closes a mirrored issue.
#[test]
fn repeat_init_with_a_tracker_value_applies_it_to_an_existing_config() {
    let project = Project::new("tracker-apply");
    project.init("claude");

    let out = stdout(&project.run(&[
        "init",
        "--provider",
        "codex",
        "--tracker",
        "github",
        "--project-key",
        "acme/app",
    ]));

    assert!(out.contains(&row("kept", ".spoolway/config.toml")), "{out}");
    assert!(
        out.contains(&row("set", "issue_tracking.hook = github.sh")),
        "{out}"
    );
    assert!(
        out.contains(&row("set", "issue_tracking.project_key = acme/app")),
        "{out}"
    );
    assert!(
        out.contains(&row("wrote", ".github/workflows/spoolway-issues.yml")),
        "{out}"
    );
    assert!(out.trim_end().ends_with("issue tracking is on."), "{out}");
    assert!(!out.contains("Project initialized successfully."));

    let config = std::fs::read_to_string(project.as_ref().join(".spoolway/config.toml")).unwrap();
    assert!(config.contains("hook = \"github.sh\""), "{config}");
    assert!(config.contains("project_key = \"acme/app\""), "{config}");
    assert!(
        project
            .as_ref()
            .join(".github/workflows/spoolway-issues.yml")
            .exists()
    );
}

/// Review finding 4: `--tracker` with no value opens the picker whatever
/// the project's age, but a script running unattended — the shape every
/// test in this file runs under, with no terminal for `crate::ask::
/// interactive` to find — has nobody to answer that picker. Answering
/// `none` on the person's behalf would silently clear a tracker the
/// project already had, so an established project's `[issue_tracking]` is
/// left exactly as it is instead, with a note explaining why, and no `set`
/// row at all.
#[test]
fn repeat_init_with_a_bare_tracker_flag_and_nobody_to_ask_leaves_an_established_config_alone() {
    let project = Project::new("tracker-bare");
    project.run(&["init", "--provider", "claude", "--tracker", "github"]);
    let config_before = std::fs::read(project.as_ref().join(".spoolway/config.toml")).unwrap();

    let out = stdout(&project.run(&["init", "--provider", "claude", "--tracker"]));
    assert!(
        out.contains("nobody to answer its picker") && out.contains("[issue_tracking] was left"),
        "{out}"
    );
    assert!(!out.contains("issue_tracking.hook ="), "{out}");
    assert_eq!(
        std::fs::read(project.as_ref().join(".spoolway/config.toml")).unwrap(),
        config_before
    );
}

/// Review finding 2: `spoolway init` answering `github` must leave an
/// existing `.github/workflows/spoolway-issues.yml` exactly as a project
/// left it — the same rule a hook or a tracking template already follows —
/// proven with sentinel bytes a shipped workflow would never itself
/// contain.
#[test]
fn repeat_init_with_tracker_github_never_overwrites_an_existing_workflow_file() {
    let project = Project::new("workflow-untouched");
    project.init("claude");
    let workflow = project
        .as_ref()
        .join(".github/workflows/spoolway-issues.yml");
    std::fs::create_dir_all(workflow.parent().unwrap()).unwrap();
    let mine = "name: mine\n# not the shipped workflow\n";
    std::fs::write(&workflow, mine).unwrap();

    let out = stdout(&project.run(&[
        "init",
        "--provider",
        "claude",
        "--tracker",
        "github",
        "--project-key",
        "acme/app",
    ]));

    assert!(
        out.contains(&row("kept", ".github/workflows/spoolway-issues.yml")),
        "{out}"
    );
    assert_eq!(std::fs::read_to_string(&workflow).unwrap(), mine);
}

/// Review finding 3: a bare re-run with no `--tracker` at all never touches
/// `[issue_tracking]` (`tracker_touched` is false), so whether the
/// workflow row reads `kept` has to come from the tracker already on disk
/// — `Config::load_tracked` in `github_in_force` — not from an answer this
/// run gave. Nothing else in this file starts from a project whose tracker
/// is already `github`, so nothing else exercises that fallback.
#[test]
fn a_bare_repeat_init_reports_the_workflow_kept_from_the_tracker_already_on_disk() {
    let project = Project::new("workflow-kept-from-disk");
    project.run(&[
        "init",
        "--provider",
        "claude",
        "--tracker",
        "github",
        "--project-key",
        "acme/app",
    ]);

    let out = stdout(&project.run(&["init", "--provider", "claude"]));
    assert!(
        out.contains(&row("kept", ".github/workflows/spoolway-issues.yml")),
        "{out}"
    );
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
    let out = stdout(&result);

    // `--force` rewrites the scaffold, so every row fires `wrote` again —
    // but the id is already stamped from the first `init`, and `--force`
    // does not re-stamp it, so there is no `stamped` row here at all.
    assert_report_rows(&out, "wrote", &scaffold_paths(&project));
    assert!(!out.contains("stamped"), "{out}");
    assert!(
        out.ends_with(&format!("Skills installed successfully.\n{CLOSING_LINES}")),
        "{out}"
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
