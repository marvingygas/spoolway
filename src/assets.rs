//! Files spoolway writes into a project. Embedded in the binary so that the CLI
//! is also the installer — `spoolway init` needs nothing on disk to work from.

/// One shipped prompt: a whole file, and not one byte spoolway parses.
///
/// A prompt is prose about a role — what it reads, what it judges, what this
/// project's bar is. The mechanics of a lane are not in here and never reach a
/// file a project also writes in: the dispatcher composes them around this prose
/// at launch — its framing before, this pass's policy after — into a system
/// prompt that is spoolway's outright and changes when spoolway changes. Which
/// is also what stops a project losing them by rewriting its prompts.
///
/// The consequence is deliberate: `spoolway update` never touches a prompt.
/// These are the defaults a project starts from, and after `init` they are the
/// project's — to sharpen, to rewrite, or to leave. Anyone wanting today's
/// defaults runs `init` in a scratch directory and copies what they like.
///
/// Each lives in a directory of its own, holding a `PROMPT.md` and — where the
/// role has belongings — an `assets/`, the way a skill is laid out. The
/// directory is what makes [`Prompt::assets`] possible at all: a role with a
/// skeleton to fill needs somewhere to keep it that is unmistakably *its*.
pub struct Prompt {
    pub name: &'static str,
    pub body: &'static str,
    /// Files written to the prompt's own `assets/`, by filename.
    ///
    /// These inherit the prompt's rules rather than a skeleton's: `init`
    /// writes them once, `update` never touches them, and after that they are
    /// the project's. A project restyles its pages by editing these in place,
    /// which is why there is no setting naming them — the prompt that fills
    /// them is the only thing that reads them, and it knows where its own
    /// assets are.
    pub assets: &'static [(&'static str, &'static str)],
}

/// The file a prompt's prose lives in, inside its directory.
pub const PROMPT_FILE: &str = "PROMPT.md";

/// The directory a prompt keeps its belongings in, inside its directory.
pub const PROMPT_ASSETS: &str = "assets";

/// Prompts, by directory name. A pipeline step's `prompt:` selects one.
pub const PROMPTS: &[Prompt] = &[
    Prompt {
        name: "implementer",
        body: include_str!("../assets/prompts/implementer/PROMPT.md"),
        assets: &[],
    },
    Prompt {
        name: "reviewer",
        body: include_str!("../assets/prompts/reviewer/PROMPT.md"),
        assets: &[],
    },
    // An `e2e` prompt shipped here once, named by no step since the default
    // pipeline dropped its e2e step. A role nothing runs is a role nothing
    // keeps honest: it was written against every project at once — hunt for
    // Playwright, or Cypress, or Detox — which is advice for a project spoolway
    // has never seen rather than a role. A project that adds the step back
    // writes the prompt beside it, against the harness it actually has.
    //
    // The bugfix pipeline's only new role. Runs twice — capture the bug as a
    // failing repro, then run that same repro after the fix.
    Prompt {
        name: "reproducer",
        body: include_str!("../assets/prompts/reproducer/PROMPT.md"),
        assets: &[],
    },
    // It writes pages, and a page needs a shape. One skeleton per kind of page,
    // because "one per domain" and "one per project" are different documents
    // with different rules, and a single skeleton serving both would state
    // neither.
    Prompt {
        name: "archivist",
        body: include_str!("../assets/prompts/archivist/PROMPT.md"),
        assets: &[
            (
                "document.md",
                include_str!("../assets/prompts/archivist/assets/document.md"),
            ),
            (
                "landing-page.md",
                include_str!("../assets/prompts/archivist/assets/landing-page.md"),
            ),
        ],
    },
    // A sample, not a mechanism: the binary knows the step id `blocked`, not
    // this prompt's name — nothing in `src/` outside this file spells
    // `unblocker`. Staffed only in an unattended run, on a pipeline that
    // declares `blocked` as a step; the shipped `default` and `bugfix`
    // pipelines both do, with `session: true`, so the same conversation that
    // hit a blocker is the one asked to clear it.
    Prompt {
        name: "unblocker",
        body: include_str!("../assets/prompts/unblocker/PROMPT.md"),
        assets: &[],
    },
];

pub fn prompt(name: &str) -> Option<&'static Prompt> {
    PROMPTS.iter().find(|prompt| prompt.name == name)
}

/// The skeletons a task file is written from, by pipeline name.
///
/// Only the markdown half is here; the block documenting what spoolway will do
/// to a task later is rendered from [`crate::task_template`] when the file is
/// written, so there is one copy of that text in the tree and no asset that can
/// fall behind the code generating it.
///
/// `default` answers for any pipeline with no file of its own, which is what
/// makes adding a pipeline cost no configuration: write `<pipeline>.md` beside
/// these to give it a shape, or write nothing and take this one.
pub const TASK_TEMPLATES: &[(&str, &str)] = &[
    ("default", include_str!("../assets/tasks/default.md")),
    // A bug is reported, not designed: what goes wrong and how to see it, so
    // the reproduce step has something to start from.
    ("bugfix", include_str!("../assets/tasks/bugfix.md")),
];

pub fn task_template(name: &str) -> Option<&'static str> {
    TASK_TEMPLATES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, text)| *text)
}

/// The two ticket-body templates a project's `.spoolway/templates/tracking/`
/// starts from, by name — seeded by `spoolway init` the way a task
/// skeleton's own shipped default is seeded into `.spoolway/templates/tasks/`.
/// `spoolway update` never touches either, once `init` has written them: the
/// same rule a task skeleton or a prompt already follows.
///
/// [`crate::task_template::resolve_tracking`] never falls back to these at
/// render time — a project with neither file written gets a single line
/// naming the task instead, never this prose silently standing in for its
/// own. Neither carries the word "spoolway": the rendered body is the
/// project's own ticket, not a spoolway one — see this module's own test.
pub const TRACKING_TEMPLATES: &[(&str, &str)] = &[
    ("epic", include_str!("../assets/tracking/epic.md")),
    ("ticket", include_str!("../assets/tracking/ticket.md")),
];

/// Read one of [`TRACKING_TEMPLATES`] by name — what `crate::update`'s own
/// `shipped_for` hands `--replace` for a tracking template, the same way it
/// already does for a task skeleton and a prompt; `init` itself still places
/// the pair by iterating the slice above directly, and `resolve_tracking`
/// never falls back to either by name.
pub fn tracking_template(name: &str) -> Option<&'static str> {
    TRACKING_TEMPLATES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, text)| *text)
}

/// The two hook scripts `spoolway init` writes into `.spoolway/hooks/`, one
/// per tracker, each calling the tracker's own command-line tool (`gh` or
/// `acli`) rather than any tracker's HTTP API directly. Both are written
/// whatever a project answered the tracker question, or none at all:
/// switching trackers later is a `spoolway config set issue_tracking.hook`
/// away, not a second `init`.
///
/// `spoolway update` never touches a hook it has already written, the same
/// rule a prompt or a task skeleton already follows once a project has made
/// a file its own.
///
/// `github.sh` treats GitHub as a mirror: `open` opens the group's epic (if
/// any) and the task's own ticket, `queued` labels the ticket in progress,
/// `blocked`/`paused` comment a snapshot of the task file's own status log
/// and handoff, and `done` — reached when `spoolway stack` has already
/// opened this task's own pull request, not when it merges — leaves a
/// `<!-- spoolway-issue: URL -->` marker comment on that pull request and
/// relabels the ticket for review. Closing the ticket itself is left to
/// [`GITHUB_ISSUE_WORKFLOW`], the workflow a project writes alongside this
/// hook: it trusts only a marker left by an owner, member or collaborator,
/// reads the pull request's `merged` event, and closes the ticket — and,
/// once every child of a group has closed, the group's own epic — from
/// there. This whole design, `open` through `done`, was run against a real
/// repository: the issues it creates, the parent link, the labels, the
/// marker comment and the workflow's own close all landed, four times over,
/// closing a real group and the tasks inside it. The `fetch` branch, plus
/// every branch of the Jira pair, are written the same careful way but have
/// not been run against a live tracker — `jira.sh`'s `acli` commands were
/// checked flag by
/// flag against acli 1.3.30-stable, and its header names what still wants a
/// project's own site to confirm: the field `workitem create --json` puts the
/// new key in and `workitem view --json` puts an issue's own fields in,
/// whether a project spells its link type `Blocks` and its status `Done`, and
/// the link type `fetch`'s own `hang_under` guesses at, `Relates`. `jq` is a
/// hard dependency of the Jira pair alongside `acli` itself — the only way
/// `workitem create --json` hands a ticket's key back — and a create call
/// that comes back with no key exits loudly rather than writing an empty
/// `epic=`/`ticket=` line.
///
/// The Jira pair does not send the task file. Jira's REST API would take it as
/// a real attachment, but only against a site, an account email and an API
/// token, and spoolway holds no credentials of its own — `gh` and `acli` each
/// carry their own. `acli` cannot upload one either: its `workitem attachment`
/// group only lists and deletes. So the Jira comment names the task and leaves
/// the file in the queue, while the GitHub pair, which needs nothing beyond
/// the `gh` login a project already has, still folds it into the comment.
///
/// `github.sh` carries one line `.spoolway/hooks/github.sh` (the file this
/// repository actually runs, and what `github.sh` here is a byte-for-byte
/// port of) does not: `# spoolway-requires: gh >= 2.97.0`, restored because
/// the marker-and-label design otherwise ships with no machine-readable
/// version floor at all — `spoolway doctor` and the submit-time gate
/// (`scripts/e2e/suites/commands.sh`'s "gh below the floor" block) both read
/// it, and read nothing back once it is gone, which stops warning silently
/// rather than failing loudly. `.spoolway/hooks/github.sh` itself still wants
/// this line added to stay in step.
pub const HOOK_SCRIPTS: &[(&str, &str)] = &[
    ("github.sh", include_str!("../assets/hooks/github.sh")),
    ("jira.sh", include_str!("../assets/hooks/jira.sh")),
];

/// The GitHub Actions workflow that closes a mirrored issue once its pull
/// request actually merges — `.github/workflows/spoolway-issues.yml` in
/// this repository, unchanged, and proven there: it closed #159 through
/// #162 and their group #158 on four real merges. `spoolway init` writes it
/// into a project's own `.github/workflows/` only when `[issue_tracking]`
/// names `github`, and — like a hook script — never touches one already
/// there. It reads the `<!-- spoolway-issue: URL -->` marker `github.sh`'s
/// `done` branch leaves on the pull request, but only from a comment whose
/// author is the repository's own owner, a member or a collaborator, so a
/// marker pasted into a comment by anyone else cannot close a ticket it
/// does not belong to.
pub const GITHUB_ISSUE_WORKFLOW: &str = include_str!("../assets/tracking/spoolway-issues.yml");

/// The markers around spoolway's rules in the project's `.gitignore`.
///
/// A project already has a `.gitignore`, or wants one at its root — so the rules
/// go there rather than into a second file two directories down that nobody
/// remembers is there. The markers are what make that possible without merging:
/// [`crate::gitignore`] rewrites what is between them and never reads a line the
/// project wrote around them.
pub const IGNORE_BEGIN: &str = "# >>> spoolway >>>";
pub const IGNORE_END: &str = "# <<< spoolway <<<";

/// The markers around the key reference in a pipeline file.
///
/// The same pair, deliberately: both fence a block spoolway rewrites inside a
/// file the project owns the rest of, and both are `#` comments in a format
/// that has no element to close. A second spelling would be a second thing to
/// recognise for one idea — see [`crate::skeleton::Region::Comment`].
pub const PIPELINE_KEYS_BEGIN: &str = IGNORE_BEGIN;
pub const PIPELINE_KEYS_END: &str = IGNORE_END;

#[cfg(test)]
mod tests {
    use super::*;

    /// One of [`HOOK_SCRIPTS`] by name, panicking on a typo rather than
    /// silently comparing against nothing.
    fn hook_script(name: &str) -> &'static str {
        HOOK_SCRIPTS
            .iter()
            .find(|(known, _)| *known == name)
            .unwrap_or_else(|| panic!("no shipped hook script named {name}"))
            .1
    }

    /// The GitHub hook hands a ticket to its own pull request at `done`
    /// rather than closing anything — see the `done` branch of the script.
    /// A `gh issue close` anywhere in it would silently undo that, so it is
    /// refused outright rather than left to a live-repo run nobody in CI
    /// can make.
    #[test]
    fn shipped_github_hook_never_closes_an_issue() {
        let script = hook_script("github.sh");
        assert!(
            !script.contains("issue close"),
            "github.sh still closes a GitHub issue"
        );
    }

    /// `spoolway doctor` and the submit-time version gate both read this
    /// declaration back through `crate::tracking::required_tools` — see
    /// [`HOOK_SCRIPTS`]'s own doc for why it is restored here even though
    /// `.spoolway/hooks/github.sh` does not carry it (yet).
    #[test]
    fn shipped_github_hook_declares_its_gh_version_floor() {
        let script = hook_script("github.sh");
        assert!(
            script.contains("# spoolway-requires: gh >= 2.97.0"),
            "github.sh no longer declares the gh version its own commands were checked against"
        );
    }

    /// `hand_off_for_review` — the `done` branch — leaves the marker comment
    /// [`GITHUB_ISSUE_WORKFLOW`] trusts on the pull request, relabels the
    /// ticket for review, and tells it where to find the pull request,
    /// checking every `gh` call for failure rather than treating a failed
    /// lookup, comment or edit as nothing to react to. Static substring
    /// checks can only prove these markers are present, not that the
    /// script's logic is correct on its own — see `tracking::tests` for
    /// execution-level proof of `github.sh`'s `done` behavior.
    #[test]
    fn github_sh_hands_off_to_a_pull_request_for_review() {
        let sh = hook_script("github.sh");
        for marker in [
            "spoolway-issue:",
            "gh pr view",
            "gh pr comment",
            "gh issue edit",
            "--remove-label spoolway:in-progress",
            "--add-label spoolway:review",
            "gh issue comment",
            "ready for review",
            "no pull request found",
        ] {
            assert!(sh.contains(marker), "hook script drops `{marker}`");
        }
        // The branch lookup, the marker comment and the label swap are each
        // checked with `|| exit $?` — a failed lookup, comment or edit must
        // stop the handoff rather than let a later step run against a
        // pull request or ticket the earlier one never actually reached.
        // The closing "ready for review" comment needs no `|| exit $?` of
        // its own: it is the function's last command, so its own exit
        // status is already `hand_off_for_review`'s. Scoped to that
        // function's own body — `open`'s `epic=$(...)`/`ticket=$(...)` use
        // the same guard for the same reason and would otherwise inflate
        // the count without proving anything about `done`.
        let done_branch = sh
            .split("hand_off_for_review() {")
            .nth(1)
            .expect("github.sh still defines hand_off_for_review");
        let done_branch = &done_branch[..done_branch.find("\n}\n").unwrap_or(done_branch.len())];
        assert_eq!(
            done_branch.matches("|| exit $?").count(),
            3,
            "github.sh no longer checks all three of the branch lookup, the marker comment \
             and the label swap in its done branch"
        );
        // The branch lookup can also succeed with nothing to report — a
        // `done` this hook fires for always has a pull request behind it by
        // then, so that has to fail too rather than read as "nothing to do".
        assert!(
            sh.contains("[ -n \"$pr\" ]"),
            "github.sh no longer treats a missing pull request as a failure"
        );
        // `done` here is the hand-off, not a merge — GitHub itself is what
        // closes the ticket, once the pull request the marker names
        // actually merges.
        assert!(
            sh.contains("GitHub will close this issue after the pull request merges"),
            "github.sh's own comment no longer explains that GitHub closes the ticket, not \
             this hook"
        );
    }

    /// This task's non-goals rule out touching Jira's own `done` behaviour:
    /// the shipped Jira hook still transitions the epic to `Done` once a
    /// group's last task settles there, unlike the GitHub one this task
    /// changed.
    #[test]
    fn shipped_jira_hook_keeps_transitioning_the_epic_on_done() {
        let script = hook_script("jira.sh");
        assert!(
            script.contains("workitem transition") && script.contains("--status Done"),
            "jira.sh no longer transitions the epic to Done"
        );
    }

    /// Nothing parses a prompt any more, which makes one thing worth asserting
    /// instead: that every shipped prompt is prose and carries no leftover
    /// marker from the format that used to partition these files. A stray
    /// marker would be read by nothing and quietly puzzle whoever opened it.
    #[test]
    fn no_shipped_prompt_carries_a_marker() {
        for prompt in PROMPTS {
            assert!(
                !prompt.body.contains("<!-- spoolway:"),
                "{} still carries a spoolway marker",
                prompt.name
            );
            assert!(!prompt.body.trim().is_empty(), "{} is empty", prompt.name);
        }
    }

    /// The rendered body of an epic or a ticket is the project's own, opened
    /// on its own tracker — not a spoolway one. Neither shipped template may
    /// say the brand's name in its own prose, so a hand-off between
    /// projects never reads as spoolway's ticket rather than theirs.
    ///
    /// The `${SPOOLWAY_*}` placeholders themselves are stripped first: those
    /// name the variables the hook's own environment already carries, the
    /// mechanism rather than the brand, and every one of them necessarily
    /// spells the word.
    #[test]
    fn no_shipped_tracking_template_names_the_brand() {
        for (name, body) in TRACKING_TEMPLATES {
            let mut prose = String::new();
            let mut rest = *body;
            while let Some(start) = rest.find("${") {
                prose.push_str(&rest[..start]);
                rest = match rest[start + 2..].find('}') {
                    Some(end) => &rest[start + 2 + end + 1..],
                    None => "",
                };
            }
            prose.push_str(rest);

            assert!(
                !prose.to_lowercase().contains("spoolway"),
                "{name}.md names spoolway outside its `${{SPOOLWAY_*}}` placeholders"
            );
        }
    }

    /// Every directory under `assets/prompts/` is named by `PROMPTS`, and every
    /// file under `assets/pipelines/` by `BUILTIN_PIPELINES`. A file named by
    /// neither is shipped in the binary's `include_str!` closure only if a table
    /// row references it, so an unreferenced one is dead weight: never written by
    /// `init`, never reachable by `update --replace`, never validated. A stray
    /// `local.yml` pipeline with a repo-local `run:` line and six orphan prompt
    /// directories had accumulated this way before this test existed. Wire a new
    /// asset into its table, or delete it.
    #[test]
    fn every_shipped_asset_is_named_by_its_table() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

        let prompt_dir = root.join("assets/prompts");
        let mut orphans = Vec::new();
        for entry in std::fs::read_dir(&prompt_dir).expect("reading assets/prompts") {
            let path = entry.expect("prompt entry").path();
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if !PROMPTS.iter().any(|p| p.name == name) {
                orphans.push(format!("assets/prompts/{name}"));
            }
        }

        let pipeline_dir = root.join("assets/pipelines");
        for entry in std::fs::read_dir(&pipeline_dir).expect("reading assets/pipelines") {
            let path = entry.expect("pipeline entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_string_lossy().to_string();
            if !crate::pipeline::BUILTIN_PIPELINES
                .iter()
                .any(|(name, _)| *name == stem)
            {
                orphans.push(format!("assets/pipelines/{stem}.yml"));
            }
        }

        assert!(
            orphans.is_empty(),
            "shipped asset(s) named by no table: {orphans:?}"
        );
    }

    /// No shipped pipeline file names a path that only resolves inside this
    /// repository's own build. `spoolway pipeline check` validates only a
    /// project's own loaded pipelines now — this is the release-time proof
    /// that the bundled samples stay clean, and it reads every `*.yml` on
    /// disk directly, so a file added under `assets/pipelines/` without a
    /// table row is still held to it. A `local.yml` that shipped `run:
    /// ./target/debug/spoolway stack` behind the table's back is what this
    /// closes.
    #[test]
    fn no_shipped_pipeline_file_names_a_repo_local_path() {
        let pipeline_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/pipelines");
        let mut offenders = Vec::new();
        for entry in std::fs::read_dir(&pipeline_dir).expect("reading assets/pipelines") {
            let path = entry.expect("pipeline entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("yml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("reading a pipeline file");
            for (number, line) in text.lines().enumerate() {
                let trimmed = line.trim_start();
                if trimmed.starts_with('#') {
                    continue;
                }
                if trimmed.contains("run:") && line.contains("./target/") {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.file_name().unwrap().to_string_lossy(),
                        number + 1,
                        trimmed
                    ));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "shipped pipeline `run:` names a repo-local path: {offenders:?}"
        );
    }

    /// Release-time proof that `assets/pipelines/*.yml` still parses and
    /// stays neutral, now that `spoolway pipeline check` derives every
    /// finding from a project's own loaded set and never opens these files
    /// itself. `Pipelines::shipped` parses, assembles and runs
    /// `Pipelines::validate` over both — the same structural rules `pipeline
    /// check` holds a project's own files to: every id unique, every
    /// transition naming a real step, every cycle bounded. Neutrality is
    /// checked directly per agent step written in the file: no assignment to
    /// a `pi` profile (spoolway's own retired special case) and no fixed
    /// `model:`/`effort:` — every choice is left an explicit blank for a
    /// project's own `init` to fill in. `blocked` is skipped: `assemble`
    /// materialises it whole from `[unattended]`'s own defaults rather than
    /// from anything either file writes, so it carries no neutrality promise
    /// of its own.
    ///
    /// The blank `model:`/`effort:` half of this overlaps with
    /// `crate::models::tests::shipped_pipelines_name_no_model`, which checks
    /// the same files at the text level — kept apart rather than merged,
    /// since that one lives beside `resolve`'s own model-pricing tests. This
    /// one adds what that one cannot: parse, assemble and validate against
    /// [`crate::pipeline::Pipelines::validate`], and the `pi`-assignment
    /// check.
    #[test]
    fn bundled_pipelines_parse_and_stay_agent_neutral() {
        let shipped = crate::pipeline::Pipelines::shipped(&crate::config::Config::default())
            .expect("assets/pipelines/*.yml must parse and validate structurally");

        for pipeline in shipped.pipelines.values() {
            for step in &pipeline.steps {
                if step.kind() != crate::pipeline::StepKind::Agent
                    || step.id == crate::pipeline::BLOCKED
                {
                    continue;
                }
                assert_ne!(
                    step.agent.as_deref(),
                    Some("pi"),
                    "`{}`/`{}` assigns the retired `pi` profile directly",
                    pipeline.name,
                    step.id
                );
                assert!(
                    step.model.as_deref().is_none_or(|m| m.trim().is_empty()),
                    "`{}`/`{}` fixes a model choice: {:?}",
                    pipeline.name,
                    step.id,
                    step.model
                );
                assert!(
                    step.effort.as_deref().is_none_or(|e| e.trim().is_empty()),
                    "`{}`/`{}` fixes an effort choice: {:?}",
                    pipeline.name,
                    step.id,
                    step.effort
                );
            }
        }
    }

    /// The crate description npm and crates.io display is a sentence from the
    /// README, not a flow (`implement -> review -> e2e -> PR -> merge`) that no
    /// shipped pipeline runs (finding 73).
    #[test]
    fn the_crate_description_is_the_readme_tagline() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cargo_toml = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        let readme = std::fs::read_to_string(root.join("README.md")).unwrap();

        let description = cargo_toml
            .lines()
            .find_map(|line| line.trim().strip_prefix("description = "))
            .map(|value| value.trim().trim_matches('"'))
            .expect("Cargo.toml has a description");

        assert!(
            readme.contains(description),
            "crate description is not a phrase from README.md: {description:?}"
        );
        for stale in ["e2e", "-> merge", "-> PR"] {
            assert!(
                !description.contains(stale),
                "crate description still names `{stale}`"
            );
        }
    }

    /// The shipped `spoolway-doctor` skill proposes only commands and keys this
    /// binary accepts — no `config set agents.*.model` (retired, refused), no
    /// "the profile's `args`" (retired), no flat `.spoolway/prompts/<name>.md`
    /// (the shape is `<name>/PROMPT.md`), no `gates` (gone), and no retired
    /// `prompt`-reading subcommand that `pipeline check` replaced — finding 29.
    /// (That banned phrase is built at runtime below, not spelled out here or
    /// in the literal list, so this file does not itself fail the repo-wide
    /// grep for it.)
    #[test]
    fn the_shipped_doctor_skill_names_nothing_retired() {
        // Every provider's copy, not just Claude's: they are three separate
        // files, and a retired command left behind in one of them is shipped to
        // that provider's users all the same.
        let skill: String = ["claude", "codex", "pi"]
            .iter()
            .map(|provider| {
                std::fs::read_to_string(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join(format!("assets/skills/{provider}/spoolway-doctor/SKILL.md")),
                )
                .unwrap()
            })
            .collect();

        for banned in [
            "config set agents.x.model",
            "config set agents.pi.model",
            "the profile's `args`",
            ".spoolway/prompts/<name>.md",
            "`gates`",
            "max_launches (default 1",
        ] {
            assert!(
                !skill.contains(banned),
                "spoolway-doctor skill still names `{banned}`"
            );
        }

        // Built from two halves rather than written as one literal, so this
        // guard's own source does not fail the same repo-wide grep it enforces.
        let retired_subcommand = ["prompt", "check"].join(" ");
        assert!(
            !skill.contains(&retired_subcommand),
            "spoolway-doctor skill still names the retired `{retired_subcommand}` subcommand"
        );
    }
}
