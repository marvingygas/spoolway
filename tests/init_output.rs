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
        self.run_in(&self.0, args)
    }

    /// [`Project::run`], but from `cwd` rather than the project root — for a
    /// linked worktree, which still answers through this same project's
    /// `HOME` (it was bound to it by the `init` that created the worktree's
    /// commit in the first place).
    fn run_in(&self, cwd: &Path, args: &[&str]) -> Output {
        let home = self.0.join("home");
        let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(args)
            .current_dir(cwd)
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
        self.run(&["init", "--yes", "--provider", provider, "--tracker", "none"])
    }

    /// [`Project::run`], but without asserting success — for a command
    /// whose outcome, pass or fail, the test asserts on itself rather than
    /// have it swallowed by an `assert!` that only prints it and panics.
    fn run_allowing_failure(&self, args: &[&str]) -> Output {
        let home = self.0.join("home");
        Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(args)
            .current_dir(&self.0)
            .env("HOME", home)
            .output()
            .expect("run spoolway")
    }

    /// `init` with no `--yes` and nothing on stdin — the shape an agent with
    /// no terminal runs it in, and the one `crate::ask::confirm` cannot ask
    /// a question through.
    fn init_with_no_terminal_and_no_yes(&self) -> Output {
        use std::process::Stdio;
        let home = self.0.join("home");
        Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(["init"])
            .current_dir(&self.0)
            .env("HOME", home)
            .stdin(Stdio::null())
            .output()
            .expect("run spoolway")
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

/// One `kept`/`wrote`/`made`/`set` row, in the column every such row shares.
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
/// are not this test's to keep in sync by hand. A directory that does not
/// exist has no paths — `hooks/` is written only for a tracker.
fn files_under(project: &Project, rel: &str) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        if !dir.exists() {
            return;
        }
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

/// Every file `init` placed, read off disk: the config, the example setup,
/// and the hook scripts when a tracker was answered.
fn scaffold_paths(project: &Project) -> Vec<String> {
    let mut paths = vec![".spoolway/config.toml".to_string()];
    for dir in [
        ".spoolway/pipelines",
        ".spoolway/prompts",
        ".spoolway/templates/tasks",
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
        out.ends_with(&format!(
            "Skills installed successfully, into .claude/skills.\n{CLOSING_LINES}"
        )),
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
    let out = stdout(&project.run(&["init", "--yes", "--provider", "codex"]));

    // Nothing in the scaffold was missing, so every file gets a `kept` row
    // rather than a `wrote` one — no tracker flag was given, so
    // `[issue_tracking]` was never touched either. But switching providers
    // installs Codex's skills for the first time, so this run did write
    // something after all, and must not also close on "nothing to
    // install" right next to the line saying the opposite.
    assert_report_rows(&out, "kept", &paths);
    assert!(
        out.contains("Skills installed successfully, into .agents/skills.\n"),
        "{out}"
    );
    assert!(!out.trim_end().ends_with("nothing to install."), "{out}");
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

/// A plain repeat `init` with the same provider and nothing missing writes
/// nothing at all — not the scaffold, not the skills — so it must say so
/// once, not twice in a way that reads as contradicting itself: never both
/// "Skills installed successfully." (nothing was actually installed just
/// now) and "nothing to install." in the same run.
#[test]
fn a_plain_repeat_init_says_nothing_to_install_exactly_once() {
    let project = Project::new("plain-repeat");
    project.init("claude");

    let out = stdout(&project.run(&["init", "--yes", "--provider", "claude"]));

    assert!(!out.contains("Skills installed successfully."), "{out}");
    assert!(
        out.contains("Skills already installed, in .claude/skills.\n"),
        "{out}"
    );
    assert!(out.trim_end().ends_with("nothing to install."), "{out}");
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

    let out = stdout(&project.run(&["init", "--yes", "--provider", "claude"]));

    assert!(
        out.contains(&row(
            "wrote",
            ".spoolway/prompts/archivist/assets/document.md"
        )),
        "{out}"
    );
    assert_report_rows(&out, "kept", &kept);
    // Claude's skills were already installed by the first `init`, and this
    // run names the same provider with no `--force`, so nothing about them
    // was actually (re)written — "already", not "successfully".
    assert!(
        out.contains("Skills already installed, in .claude/skills.\n"),
        "{out}"
    );
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

    let out = stdout(&project.run(&[
        "init",
        "--yes",
        "--provider",
        "codex",
        "--project-key",
        "acme/app",
    ]));
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
/// writes the hook scripts, but no workflow under `.github/`.
#[test]
fn repeat_init_with_a_tracker_value_applies_it_to_an_existing_config() {
    let project = Project::new("tracker-apply");
    project.init("claude");

    let out = stdout(&project.run(&[
        "init",
        "--yes",
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
        out.contains(&row("wrote", ".spoolway/hooks/github.sh")),
        "{out}"
    );
    assert!(
        out.contains(&row("wrote", ".spoolway/hooks/jira.sh")),
        "{out}"
    );
    assert!(!out.contains("spoolway-issues.yml"), "{out}");
    assert!(out.trim_end().ends_with("issue tracking is on."), "{out}");
    assert!(!out.contains("Project initialized successfully."));

    let config = std::fs::read_to_string(project.as_ref().join(".spoolway/config.toml")).unwrap();
    assert!(config.contains("hook = \"github.sh\""), "{config}");
    assert!(config.contains("project_key = \"acme/app\""), "{config}");
    assert!(!project.as_ref().join(".github").exists());
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
    project.run(&[
        "init",
        "--yes",
        "--provider",
        "claude",
        "--tracker",
        "github",
    ]);
    let config_before = std::fs::read(project.as_ref().join(".spoolway/config.toml")).unwrap();

    let out = stdout(&project.run(&["init", "--yes", "--provider", "claude", "--tracker"]));
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

/// `init` no longer ships the close-on-merge workflow, and it does not
/// delete or rewrite one a project already has either: answering `github`
/// leaves the file exactly as the project left it and never names it in
/// the report.
#[test]
fn init_with_tracker_github_neither_writes_nor_touches_a_workflow_file() {
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
        "--yes",
        "--provider",
        "claude",
        "--tracker",
        "github",
        "--project-key",
        "acme/app",
    ]));

    assert!(!out.contains("spoolway-issues.yml"), "{out}");
    assert_eq!(std::fs::read_to_string(&workflow).unwrap(), mine);
}

/// A bare re-run with no `--tracker` at all never touches
/// `[issue_tracking]` (`tracker_touched` is false), so whether the hook
/// rows appear has to come from the tracker already on disk, not from an
/// answer this run gave. Nothing else in this file starts from a project
/// whose tracker is already `github`, so nothing else exercises that
/// fallback.
#[test]
fn a_bare_repeat_init_reports_the_hooks_kept_from_the_tracker_already_on_disk() {
    let project = Project::new("hooks-kept-from-disk");
    project.run(&[
        "init",
        "--yes",
        "--provider",
        "claude",
        "--tracker",
        "github",
        "--project-key",
        "acme/app",
    ]);

    let out = stdout(&project.run(&["init", "--yes", "--provider", "claude"]));
    assert!(
        out.contains(&row("kept", ".spoolway/hooks/github.sh")),
        "{out}"
    );
    assert!(
        out.contains(&row("kept", ".spoolway/hooks/jira.sh")),
        "{out}"
    );
}

/// With `none` there is no `hooks/` folder at all, and no hook row.
#[test]
fn a_fresh_init_with_no_tracker_writes_no_hooks_folder() {
    let project = Project::new("no-hooks");
    let out = stdout(&project.init("claude"));

    assert!(!project.as_ref().join(".spoolway/hooks").exists());
    assert!(!out.contains(".spoolway/hooks"), "{out}");
}

/// `--no-examples` writes the config and three empty folders, reports each
/// folder as `made`, and closes on the line naming the skill that writes a
/// pipeline rather than the model-and-effort line, which has no step to
/// point at.
#[test]
fn a_fresh_init_without_examples_makes_empty_folders_and_names_the_skill() {
    let project = Project::new("no-examples");
    let result = project.run(&[
        "init",
        "--yes",
        "--provider",
        "claude",
        "--no-examples",
        "--tracker",
        "none",
    ]);
    let out = stdout(&result);

    assert_eq!(stderr(&result), "");
    assert!(
        out.contains(&row("wrote", ".spoolway/config.toml")),
        "{out}"
    );
    for dir in ["pipelines", "prompts", "templates"] {
        let rel = format!(".spoolway/{dir}/");
        assert!(out.contains(&row("made", &rel)), "{out}");
        let on_disk = project.as_ref().join(".spoolway").join(dir);
        assert!(on_disk.is_dir(), "{rel} is not a folder");
        assert_eq!(std::fs::read_dir(&on_disk).unwrap().count(), 0, "{rel}");
    }
    assert!(!project.as_ref().join(".spoolway/hooks").exists());
    assert!(
        out.ends_with(
            "Skills installed successfully, into .claude/skills.\n\
             Project initialized successfully.\n\
             Use the spoolway-config skill to create pipelines.\n"
        ),
        "{out}"
    );

    // A repeat run keeps the choice: nothing brings the examples in behind
    // the person's back.
    let again = stdout(&project.run(&["init", "--yes", "--provider", "claude"]));
    assert!(
        again.contains(&row("kept", ".spoolway/pipelines/")),
        "{again}"
    );
    assert!(
        std::fs::read_dir(project.as_ref().join(".spoolway/pipelines"))
            .unwrap()
            .next()
            .is_none()
    );
}

/// `--examples` with `--tracker github` writes the example setup and the
/// hook scripts together, and closes on the model-and-effort line.
#[test]
fn a_fresh_init_with_examples_and_github_writes_both() {
    let project = Project::new("examples-github");
    let result = project.run(&[
        "init",
        "--yes",
        "--provider",
        "claude",
        "--examples",
        "--tracker",
        "github",
        "--project-key",
        "acme/app",
    ]);
    let out = stdout(&result);

    let paths = scaffold_paths(&project);
    assert!(paths.iter().any(|p| p == ".spoolway/pipelines/default.yml"));
    assert!(paths.iter().any(|p| p == ".spoolway/hooks/github.sh"));
    assert!(paths.iter().any(|p| p == ".spoolway/hooks/jira.sh"));
    assert_report_rows(&out, "wrote", &paths);
    assert!(!project.as_ref().join(".github").exists());
    assert!(
        out.ends_with(&format!(
            "Skills installed successfully, into .claude/skills.\n{CLOSING_LINES}"
        )),
        "{out}"
    );
}

#[test]
fn forced_init_reprints_every_wrote_row_and_closes_like_a_fresh_run() {
    let project = Project::new("force");
    project.init("claude");

    let result = project.run(&[
        "init",
        "--yes",
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
        out.ends_with(&format!(
            "Skills installed successfully, into .agents/skills.\n{CLOSING_LINES}"
        )),
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
fn standalone_install_names_where_it_installed() {
    let project = Project::new("install");
    project.init("claude");

    assert_eq!(
        stdout(&project.run(&["install", "codex"])),
        "Skills installed successfully, into .agents/skills.\n"
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
    assert_eq!(
        lines[at(&stamped) + 1],
        "Skills installed successfully, into .claude/skills."
    );
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
/// exists, but its own stamp has since moved on (a hand edit, or a hand
/// edit racing a crashed migration) — a stale copy taken before that still
/// carries the old id prints the same one line, not two.
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
    let label = std::fs::read_to_string(root.join(".git/spoolway-label"))
        .expect("read the original label")
        .trim()
        .to_string();

    // The original checkout's stamp moves on by hand — the home keyed on
    // `old_id` is now stale, though the checkout on record for it still
    // exists right where it was. A fresh home, keyed on the new id, is
    // written right alongside it: this is exactly what `spoolway init`
    // itself wrote the one time this checkout's id was first minted, just
    // done again by hand rather than by a command.
    let new_id = if old_id == "zzzzzz" {
        "yyyyyy"
    } else {
        "zzzzzz"
    };
    let new_home = home.join(".spoolway").join(format!("{label}-{new_id}"));
    std::fs::create_dir_all(&new_home).expect("create the fresh home");
    std::fs::write(
        new_home.join("project.toml"),
        format!("id = {new_id:?}\nroot = {root:?}\n"),
    )
    .expect("write the fresh home's binding");
    std::fs::write(root.join(".git/spoolway-id"), format!("{new_id}\n"))
        .expect("re-stamp the checkout by hand");

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

/// A pipeline edit made only in a linked worktree — never committed, never
/// synced to the project root — is one `prompt contract` can preview from
/// inside that worktree, printing the `checkout:` line first the same way
/// `pipeline show` does.
///
/// Runs the real binary rather than calling `prompt::contract` directly: a
/// unit test handed a `Pipelines` already loaded from the checkout would
/// pass whether or not `main.rs` actually chose that source, since
/// `contract` has always just taken whatever `Pipelines` it is given. This
/// is the one place `main.rs`'s own wiring — which copy it loads for this
/// command — is on the hook.
#[test]
fn prompt_contract_in_a_linked_worktree_reads_the_worktrees_own_pipeline() {
    let project = Project::new("prompt-contract-worktree");
    project.init("claude");

    let root = project.as_ref().to_path_buf();
    let status = Command::new("git")
        .args(["add", "-A"])
        .current_dir(&root)
        .status()
        .expect("git add");
    assert!(status.success());
    let status = Command::new("git")
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ])
        .current_dir(&root)
        .status()
        .expect("git commit");
    assert!(status.success());

    let wt = root.parent().unwrap().join(format!(
        "{}-wt",
        root.file_name().unwrap().to_string_lossy()
    ));
    let status = Command::new("git")
        .args([
            "worktree",
            "add",
            "-q",
            "-b",
            "task/upgrade-step",
            wt.to_str().unwrap(),
        ])
        .current_dir(&root)
        .status()
        .expect("git worktree add");
    assert!(status.success());

    // The worktree's own copy gains a step the project root never
    // committed — the edit `prompt contract` exists to preview before it
    // lands.
    let pipeline_path = wt.join(".spoolway/pipelines/default.yml");
    let mut pipeline = std::fs::read_to_string(&pipeline_path).expect("read worktree pipeline");
    pipeline.push_str(
        "\n  - id: upgrade\n    description: Added only in this worktree.\n    agent: claude\n    \
         prompt: implementer\n    model: \"\"\n    effort: \"\"\n    on_pass: done\n",
    );
    std::fs::write(&pipeline_path, pipeline).expect("write worktree pipeline");

    let out = stdout(&project.run_in(
        &wt,
        &[
            "prompt",
            "contract",
            "--pipeline",
            "default",
            "--step",
            "upgrade",
        ],
    ));

    assert!(out.starts_with("checkout: "), "{out}");
    assert!(
        out.contains(&wt.file_name().unwrap().to_string_lossy().to_string()),
        "{out}"
    );
    assert!(out.contains("task/upgrade-step"), "{out}");
    assert!(out.contains("For `default`/`upgrade`"), "{out}");

    let _ = Command::new("git")
        .args(["worktree", "remove", "--force", wt.to_str().unwrap()])
        .current_dir(&root)
        .status();
}

/// `init --no-examples` leaves `pipelines/` empty, and the skill's own next
/// step — `spoolway pipeline contract` and `spoolway prompt contract` — has
/// to work in exactly that project, since those are the two commands that
/// print the formats it writes the first pipeline from. Neither command's
/// own output depends on a project pipeline existing, so an empty
/// `pipelines/` directory must not fail them — see
/// `crate::pipeline::Pipelines::load_or_empty`.
#[test]
fn pipeline_contract_and_prompt_contract_succeed_with_no_pipelines_defined() {
    let project = Project::new("contract-no-pipelines");
    project.run(&[
        "init",
        "--yes",
        "--provider",
        "claude",
        "--no-examples",
        "--tracker",
        "none",
    ]);
    assert_eq!(
        std::fs::read_dir(project.as_ref().join(".spoolway/pipelines"))
            .unwrap()
            .count(),
        0,
        "fixture must start with no project pipelines defined"
    );

    let pipeline_out = project.run_allowing_failure(&["pipeline", "contract"]);
    assert!(
        pipeline_out.status.success(),
        "pipeline contract failed with no project pipelines: {}",
        stderr(&pipeline_out)
    );

    let prompt_out = project.run_allowing_failure(&["prompt", "contract"]);
    assert!(
        prompt_out.status.success(),
        "prompt contract failed with no project pipelines: {}",
        stderr(&prompt_out)
    );
}

/// A repeat `init` with no `--provider` and a missing `pipelines/` folder
/// restores it for the project's own configured provider, not the menu's
/// first entry (`claude`), and installs only that provider's skills.
#[test]
fn a_repeat_init_with_no_provider_restores_the_projects_own_provider() {
    let project = Project::new("repeat-no-provider");
    project.init("codex");
    std::fs::remove_dir_all(project.as_ref().join(".spoolway/pipelines")).unwrap();

    project.run(&["init", "--yes"]);

    let pipeline =
        std::fs::read_to_string(project.as_ref().join(".spoolway/pipelines/default.yml")).unwrap();
    assert!(pipeline.contains("agent: codex"), "{pipeline}");
    assert!(!pipeline.contains("agent: claude"), "{pipeline}");
    assert!(
        !project.as_ref().join(".claude/skills").exists(),
        "only the project's own provider's skills should be installed"
    );
    assert!(project.as_ref().join(".agents/skills").is_dir());
}

/// A fresh project has no existing key to fall back on, so choosing a
/// tracker with no `--project-key` and nobody to ask warns, naming
/// `--project-key`, and still writes the empty key rather than refusing.
#[test]
fn a_fresh_tracker_with_no_project_key_warns_and_writes_an_empty_key() {
    let project = Project::new("tracker-no-key-fresh");

    let out = stdout(&project.run(&[
        "init",
        "--yes",
        "--provider",
        "claude",
        "--tracker",
        "github",
    ]));

    assert!(out.contains("--project-key"), "{out}");
    let config = std::fs::read_to_string(project.as_ref().join(".spoolway/config.toml")).unwrap();
    assert!(config.contains("project_key = \"\""), "{config}");
}

/// The established-project half of the same bug: a repeat `--tracker` with
/// no `--project-key` and nobody to ask keeps the key the project already
/// has, and — since there is one to keep — prints no warning about it.
#[test]
fn a_repeat_tracker_with_no_project_key_keeps_the_existing_one_and_warns_only_when_none() {
    let project = Project::new("tracker-no-key-repeat");
    project.run(&[
        "init",
        "--yes",
        "--provider",
        "claude",
        "--tracker",
        "github",
        "--project-key",
        "acme/app",
    ]);

    let out = stdout(&project.run(&[
        "init",
        "--yes",
        "--provider",
        "claude",
        "--tracker",
        "github",
    ]));

    assert!(!out.contains("--project-key"), "{out}");
    let config = std::fs::read_to_string(project.as_ref().join(".spoolway/config.toml")).unwrap();
    assert!(config.contains("project_key = \"acme/app\""), "{config}");
}

/// `init --force` in a home-mode clone rewrites `config/`, shared by every
/// clone in that workspace — so it must name the others sharing it before
/// doing that, not reset a teammate's setup with no warning at all.
#[test]
fn force_in_a_home_mode_clone_names_the_other_clones_before_rewriting_their_config() {
    let base = std::env::temp_dir().join(format!(
        "spoolway-init-output-force-home-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = base.join("home");
    let first = base.join("first");
    let second = base.join("second");
    for root in [&first, &second] {
        std::fs::create_dir_all(root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }

    let run = |root: &Path, args: &[&str]| -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(args)
            .current_dir(root)
            .env("HOME", &home)
            .output()
            .expect("run spoolway");
        assert!(
            output.status.success(),
            "spoolway failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    run(
        &first,
        &[
            "init",
            "--yes",
            "--setup",
            "home",
            "--workspace",
            "new",
            "--provider",
            "claude",
            "--tracker",
            "none",
        ],
    );
    let workspace_name = std::fs::read_dir(home.join(".spoolway"))
        .expect("read ~/.spoolway")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("first-"))
        .expect("the fresh workspace folder, labelled after the first clone");
    run(
        &second,
        &[
            "init",
            "--yes",
            "--workspace",
            &workspace_name,
            "--provider",
            "claude",
            "--tracker",
            "none",
        ],
    );

    let out = stdout(&run(
        &second,
        &["init", "--force", "--yes", "--provider", "claude"],
    ));

    assert!(
        out.contains(&first.canonicalize().unwrap().display().to_string())
            || out.contains(&first.display().to_string()),
        "expected the first clone's path named before the shared config was rewritten:\n{out}"
    );

    std::fs::remove_dir_all(&base).ok();
}

/// A clone joining a workspace uses the setup already chosen there, so
/// `--tracker`, `--project-key` and `--examples` do nothing on that run —
/// but the run has to say so, rather than silently dropping them with no
/// trace beyond their absence from `config.toml`.
#[test]
fn joining_a_workspace_names_the_flags_it_ignored() {
    let base = std::env::temp_dir().join(format!(
        "spoolway-init-output-join-ignored-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = base.join("home");
    let first = base.join("first");
    let second = base.join("second");
    for root in [&first, &second] {
        std::fs::create_dir_all(root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "-b", "main"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }

    let run = |root: &Path, args: &[&str]| -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_spoolway"))
            .args(args)
            .current_dir(root)
            .env("HOME", &home)
            .output()
            .expect("run spoolway");
        assert!(
            output.status.success(),
            "spoolway failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    };

    run(
        &first,
        &[
            "init",
            "--yes",
            "--setup",
            "home",
            "--workspace",
            "new",
            "--provider",
            "claude",
            "--tracker",
            "none",
        ],
    );
    let workspace_name = std::fs::read_dir(home.join(".spoolway"))
        .expect("read ~/.spoolway")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .find(|name| name.starts_with("first-"))
        .expect("the fresh workspace folder, labelled after the first clone");

    let out = stdout(&run(
        &second,
        &[
            "init",
            "--yes",
            "--workspace",
            &workspace_name,
            "--provider",
            "claude",
            "--tracker",
            "github",
            "--project-key",
            "o/r",
            "--examples",
        ],
    ));

    assert!(out.contains("ignored"), "{out}");
    assert!(out.contains("--tracker"), "{out}");
    assert!(out.contains("--project-key"), "{out}");
    assert!(out.contains("--examples"), "{out}");
    let config = std::fs::read_to_string(
        home.join(".spoolway")
            .join(&workspace_name)
            .join("config")
            .join("config.toml"),
    )
    .unwrap();
    assert!(
        !config.contains("hook = \"github.sh\"") && !config.contains("project_key = \"o/r\""),
        "the joining run's --tracker must not have touched the shared config:\n{config}"
    );

    std::fs::remove_dir_all(&base).ok();
}

/// `init` with nobody to answer its confirmation writes nothing — the
/// declared-default path `crate::ask::confirm` takes when stdin is not a
/// terminal. An agent has no terminal either, so writing nothing has to
/// come with a word of explanation rather than pass for success: a line
/// saying nothing was written and that `--yes` is how to answer the
/// confirmation.
#[test]
fn init_with_no_terminal_and_no_yes_says_nothing_was_written() {
    let project = Project::new("no-terminal-no-yes");

    let result = project.init_with_no_terminal_and_no_yes();
    let out = stdout(&result);

    assert!(result.status.success(), "{}", stderr(&result));
    assert!(
        !project.as_ref().join(".spoolway").exists(),
        "nothing should have been written:\n{out}"
    );
    assert!(
        out.lines().any(|line| line.contains("--yes")
            && (line.contains("nothing") || line.contains("wrote nothing"))),
        "expected a line saying nothing was written and naming --yes, got:\n{out}"
    );
}

/// A workspace `project.toml` elsewhere on the machine, broken by a typo —
/// a `config/` folder beside it (so `all_workspaces` treats it as a real
/// workspace rather than skipping it silently) and unparseable TOML inside
/// it. Written directly under `project`'s own `HOME`, same as every other
/// workspace fixture in this file, so it sits beside — never inside — the
/// repo-mode project the test itself runs commands against.
fn write_broken_workspace_file(project: &Project) -> PathBuf {
    let broken = project.as_ref().join("home").join(".spoolway").join("a-1x");
    std::fs::create_dir_all(broken.join("config")).unwrap();
    std::fs::write(broken.join("project.toml"), "garbage = [\n").unwrap();
    broken.join("project.toml")
}

/// Acceptance criterion 2: an unrelated project — repo mode here, listed in
/// no workspace at all — prints exactly one note naming a broken workspace
/// file elsewhere on the machine, then carries on to its own normal output.
/// `bind` runs twice on an ordinary command (`main.rs`'s own update-check
/// notice resolves the project leniently before the command arm resolves it
/// again), so this is also the regression test for the note printing twice
/// in one run.
#[test]
fn queue_list_in_an_unrelated_project_notes_a_broken_workspace_file_once() {
    let project = Project::new("queue-list-broken-workspace");
    project.init("claude");
    let broken_file = write_broken_workspace_file(&project);

    let result = project.run(&["queue", "list"]);
    let out = stdout(&result);
    let err = stderr(&result);

    let note = format!("{} does not read as a workspace", broken_file.display());
    assert_eq!(
        err.matches(&note).count(),
        1,
        "expected exactly one note naming the broken file on stderr, got:\n{err}"
    );
    assert!(
        !out.contains("does not read as a workspace"),
        "the note belongs on stderr, not mixed into the command's own stdout:\n{out}"
    );
    assert!(
        out.contains("No tasks queued."),
        "the command's own normal output still follows the note:\n{out}"
    );
}

/// The same broken-file note, with `--json`: the note still goes to stderr,
/// once, and stdout is left as nothing but the parseable JSON `queue list
/// --json` always prints — a machine reader of stdout alone never sees it.
#[test]
fn queue_list_json_in_an_unrelated_project_keeps_the_note_off_stdout() {
    let project = Project::new("queue-list-broken-workspace-json");
    project.init("claude");
    let broken_file = write_broken_workspace_file(&project);

    let result = project.run(&["queue", "list", "--json"]);
    let out = stdout(&result);
    let err = stderr(&result);

    let note = format!("{} does not read as a workspace", broken_file.display());
    assert_eq!(
        err.matches(&note).count(),
        1,
        "expected exactly one note naming the broken file on stderr, got:\n{err}"
    );
    serde_json::from_str::<serde_json::Value>(&out)
        .unwrap_or_else(|err| panic!("--json stdout must parse as JSON, got {err}:\n{out}"));
}
