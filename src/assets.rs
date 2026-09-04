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
    // `spoolway stack`'s optional summary model. Named by `[stack.summary]`
    // rather than by a pipeline step — there is no worker slot and no lane
    // here, just one turn read back for its title line and the text below
    // it. It does nothing else: no git, no `gh`, not even the task's diff.
    Prompt {
        name: "summariser",
        body: include_str!("../assets/prompts/summariser/PROMPT.md"),
        assets: &[],
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

/// Read one of [`TRACKING_TEMPLATES`] by name. Not reached outside a test:
/// `init` places the pair by iterating the slice above directly, and
/// `resolve_tracking` never falls back to either by name — this is only
/// what a test compares a seeded file's contents against.
#[allow(dead_code)]
pub fn tracking_template(name: &str) -> Option<&'static str> {
    TRACKING_TEMPLATES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, text)| *text)
}

/// The four hook scripts `spoolway init` writes into `.spoolway/hooks/` — a
/// `.sh`/`.ps1` pair per tracker, each calling the tracker's own
/// command-line tool (`gh` or `acli`) rather than any tracker's HTTP API
/// directly. Every one is written whatever a project answered the tracker
/// question, or none at all: switching trackers later is a
/// `spoolway config set issue_tracking.hook` away, not a second `init`.
///
/// Only one pair of these ever runs on a given platform — `.sh` wherever
/// [`crate::platform::shell_command`] reaches for `sh -c`, `.ps1` wherever it
/// reaches for PowerShell instead — so `init` writes only the pair its own
/// platform can run; see `commands::init`. `spoolway update` never touches a
/// hook it has already written, the same rule a prompt or a task skeleton
/// already follows once a project has made a file its own.
///
/// `github.sh`'s `open`, `done`, `blocked` and `paused` branches were run
/// against a real repository: the issues they create, the `sub_issues` link,
/// the comment and the close all landed. Its `fetch` branch, and every branch
/// of the Jira pair, are written the same careful way but have not been run
/// against a live tracker — `jira.sh`'s `acli` commands were checked flag by
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
pub const HOOK_SCRIPTS: &[(&str, &str)] = &[
    ("github.sh", include_str!("../assets/hooks/github.sh")),
    ("jira.sh", include_str!("../assets/hooks/jira.sh")),
    ("github.ps1", include_str!("../assets/hooks/github.ps1")),
    ("jira.ps1", include_str!("../assets/hooks/jira.ps1")),
];

/// The shape `spoolway stack`'s summary prompt fills in — see
/// [`crate::config::PULL_REQUEST_TEMPLATE`] for where it installs.
///
/// A top-level asset rather than a prompt's own: the versioner used to keep
/// a private copy at `assets/prompts/versioner/assets/pull-request.md`, and
/// this is that file moved up a level and shared, before the versioner
/// prompt it was shared with was deleted outright — git up to the pull
/// request is `spoolway stack`'s job now, and this is the one piece of that
/// prompt still worth keeping. Restorable the way a prompt or a task skeleton is: written once
/// by `init`, left alone by an ordinary `spoolway update`, and brought back
/// to this text only by `spoolway update --replace
/// .spoolway/templates/pull-request.md`.
pub const PULL_REQUEST_TEMPLATE: &str = include_str!("../assets/pull-request.md");

/// The six typed messages a lane's pane receives — see
/// [`crate::lane_prompts`]. Written whole by `init`, left alone by an
/// ordinary `spoolway update`, and brought back to this text only by
/// `spoolway update --replace .spoolway/templates/lane-prompts.md`, the same
/// bargain every other file in this module keeps.
pub const LANE_PROMPTS: &str = include_str!("../assets/lane-prompts.md");

/// What belongs under each heading spoolway appends to a task file — see
/// [`crate::task_log`]. Written whole by `init`, left alone by an ordinary
/// `spoolway update`, and brought back to this text only by `spoolway
/// update --replace .spoolway/templates/task-log.md`, the same bargain
/// every other file in this module keeps.
pub const TASK_LOG: &str = include_str!("../assets/task-log.md");

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
    /// repository's own build. `spoolway pipeline check` runs the same check
    /// through `shipped_run_names_a_repo_local_path`, but only over the files
    /// `BUILTIN_PIPELINES` loads — this one reads every `*.yml` on disk, so a
    /// file added under `assets/pipelines/` without a table row is still held to
    /// it. A `local.yml` that shipped `run: ./target/debug/spoolway stack`
    /// behind the table's back is what this closes.
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
    /// (the shape is `<name>/PROMPT.md`), no `gates` (gone) — finding 29.
    #[test]
    fn the_shipped_doctor_skill_names_nothing_retired() {
        let skill = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/skills/claude/spoolway-doctor/SKILL.md"),
        )
        .unwrap();

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
    }
}
