//! Installing pipeline skills into a coding agent's own convention.
//!
//! The skills are embedded in the binary, so `spoolway install` needs nothing on
//! disk to copy from. Every copy of every skill lives under
//! `assets/skills/<provider>/`, one directory per provider, and this file
//! embeds them from there. Nothing is read from an installed directory such as
//! `.claude/skills` or `.agents/skills`: those are output, and a source that
//! doubles as an install target drifts the moment somebody edits one and not
//! the other.
//!
//! A provider decides *where* files go, and which copy goes there. The three
//! copies differ only in how the procedure asks the person a question, because
//! that is the one thing the three agents genuinely do differently:
//!
//! - Claude calls its `AskUserQuestion` tool.
//! - Codex calls `request_user_input`, which is the same thing under another
//!   name — but only inside plan mode, so a copy whose question falls outside
//!   one prints it instead.
//! - pi has no dialog tool at all, and what a given install's plugins add is
//!   not something a skill can count on.
//!
//! A copy that cannot open a dialog asks all the same: it prints the question
//! as output and ends the turn, and the person answers in their next prompt.
//!
//! The layout follows the Agent Skills spec: a directory per skill with a
//! `SKILL.md` entrypoint, a `name` matching that directory, and static
//! resources under `assets/`. Every provider here follows it, which is why
//! [`Provider::plan`] is one function and not one per kind — what a provider
//! decides is a root, and [`Provider::skills_dir`] is the whole of that
//! decision, with the artifact each row was read off recorded beside it.
//!
//! Which of those resources ship beside the skill depends on whether the file
//! outlives the procedure that fills it. `spoolway-config` (renamed and
//! widened from `spoolway-pipeline`) ships none at all: every format it
//! routes to is printed by the binary itself — `spoolway pipeline contract`,
//! `prompt contract`, `config contract`, `override contract`, `template
//! contract` and `hook contract` — so the skill fetches them at runtime
//! instead of carrying a copy that can drift from the binary that actually
//! enforces the format.
//!
//! `spoolway-plan` is the exception that does carry assets: its skeleton and
//! the markup reference beside it, both from
//! `assets/skills/claude/spoolway-plan/assets/`. They ship rather than being
//! fetched because there is no command that prints them — a plan page is not
//! a format the binary enforces, so nothing can regenerate one at runtime.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::cli::Provider;
use crate::task::write_atomic;

/// One skill: its entrypoint per provider, and the static resources it brings
/// with it.
struct Skill {
    name: &'static str,
    /// The Claude copy, and the one the shape of a skill is written from.
    skill_md: &'static str,
    /// The same procedure for Codex: naming its `request_user_input` tool
    /// where the question lands inside plan mode, and printing the question
    /// where it does not.
    codex_skill_md: &'static str,
    /// The same procedure again, written for an agent with no dialog tool:
    /// questions are printed and the turn ends there.
    pi_skill_md: &'static str,
    /// Files landing under the skill's own `assets/`, by filename.
    assets: &'static [(&'static str, &'static str)],
}

#[cfg(test)]
impl Skill {
    /// Every provider's copy of this skill, by the name that provider is
    /// selected under. Tests walk this so a copy added here cannot skip the
    /// checks the others pass.
    fn copies(&self) -> [(&'static str, &'static str); 3] {
        [
            ("claude", self.skill_md),
            ("codex", self.codex_skill_md),
            ("pi", self.pi_skill_md),
        ]
    }
}

/// Skills shipped for the planning and queueing half of the pipeline. The
/// running half is `spoolway dispatch`, which needs no skill at all.
const SKILLS: &[Skill] = &[
    Skill {
        name: "spoolway-plan",
        skill_md: include_str!("../assets/skills/claude/spoolway-plan/SKILL.md"),
        codex_skill_md: include_str!("../assets/skills/codex/spoolway-plan/SKILL.md"),
        pi_skill_md: include_str!("../assets/skills/pi/spoolway-plan/SKILL.md"),
        // The skeleton the procedure's step 4 copies, and the markup
        // reference it opens instead of reading the skeleton. Both are the
        // Claude copy: neither names a provider's tools.
        assets: &[
            (
                "template.html",
                include_str!("../assets/skills/claude/spoolway-plan/assets/template.html"),
            ),
            (
                "page.md",
                include_str!("../assets/skills/claude/spoolway-plan/assets/page.md"),
            ),
        ],
    },
    // The task-cutting procedure `spoolway-plan`'s step 7 invokes, and any
    // second caller doing the same breakdown later. It has to be reachable
    // from inside another skill's own procedure, which is why it carries no
    // `disable-model-invocation` — see the frontmatter test below, which
    // knows this one skill is the exception.
    Skill {
        name: "spoolway-tasks",
        skill_md: include_str!("../assets/skills/claude/spoolway-tasks/SKILL.md"),
        codex_skill_md: include_str!("../assets/skills/codex/spoolway-tasks/SKILL.md"),
        pi_skill_md: include_str!("../assets/skills/pi/spoolway-tasks/SKILL.md"),
        assets: &[],
    },
    // Reshaping the flow itself — the graph, and the prompts its agent steps
    // run — and, since it was renamed and widened from `spoolway-pipeline`,
    // routing over every other corner of the control plane: `config.toml`,
    // the override layer, the task and lane templates, and the
    // issue-tracking hooks. One skill rather than several, because a step and
    // the value that runs it (a model, an effort, a timeout, a concurrency)
    // are the same territory at different sizes. It carries no
    // `disable-model-invocation` either: `spoolway pipeline gen` reaches it
    // from another procedure. It ships no assets: every format it routes to is
    // printed by the binary itself — `spoolway pipeline contract`, `prompt
    // contract`, `config contract`, `override contract`, `template contract`
    // and `hook contract` — fetched at runtime instead of a copy that can
    // drift from the binary that enforces it.
    Skill {
        name: "spoolway-config",
        skill_md: include_str!("../assets/skills/claude/spoolway-config/SKILL.md"),
        codex_skill_md: include_str!("../assets/skills/codex/spoolway-config/SKILL.md"),
        pi_skill_md: include_str!("../assets/skills/pi/spoolway-config/SKILL.md"),
        assets: &[],
    },
    // Reading a pipeline's health is CLI calls too, and the same shape as the
    // others: what is wrong comes out of the binary, and what to do about
    // it is a person's decision. Shipped so that the answer to "is this thing
    // set up right" is not a session re-deriving `doctor`'s output every time.
    Skill {
        name: "spoolway-doctor",
        skill_md: include_str!("../assets/skills/claude/spoolway-doctor/SKILL.md"),
        codex_skill_md: include_str!("../assets/skills/codex/spoolway-doctor/SKILL.md"),
        pi_skill_md: include_str!("../assets/skills/pi/spoolway-doctor/SKILL.md"),
        assets: &[],
    },
    // Reads lane-written task records and step-level evaluation and spend data
    // back into the control plane that produced them. It uses both the agents'
    // own reports and the numbers, then applies the changes the person chooses.
    Skill {
        name: "spoolway-calibrate",
        skill_md: include_str!("../assets/skills/claude/spoolway-calibrate/SKILL.md"),
        codex_skill_md: include_str!("../assets/skills/codex/spoolway-calibrate/SKILL.md"),
        pi_skill_md: include_str!("../assets/skills/pi/spoolway-calibrate/SKILL.md"),
        assets: &[],
    },
];

/// Skill directories this project once shipped under a name it no longer
/// uses — a short, hand-written literal, and never derived from [`SKILLS`]:
/// a skill this binary actively ships must never appear here by
/// construction, or `spoolway update` would delete what `install` is about
/// to rewrite in the very same pass. `spoolway-pipeline` is the one entry
/// today, renamed and widened into `spoolway-config` — see [`SKILLS`]'s own
/// comment above.
pub const RETIRED_SKILLS: &[&str] = &["spoolway-pipeline"];

/// Templates under `.spoolway/templates/` this project once shipped and no
/// longer does, each with the reason `spoolway update` reports beside it — a
/// short, hand-written literal, and never derived from anything the binary
/// ships today, for the same reason [`RETIRED_SKILLS`] is: a template this
/// binary still ships must never appear here by construction, and a
/// project's own file under `.spoolway/templates/` — named by neither this
/// list nor the shapes `init` still places — is never touched. `task-log.md`
/// stopped being written when `assets::TASK_LOG`, `config::TASK_LOG_TEMPLATE`
/// and `src/task_log.rs` were removed; `pull-request.md` stopped being read
/// when `spoolway stack` started sending the task file's own body verbatim.
pub const RETIRED_TEMPLATES: &[(&str, &str)] = &[
    (
        ".spoolway/templates/task-log.md",
        "no longer written to a task file",
    ),
    (
        ".spoolway/templates/pull-request.md",
        "the pull request body is the task file itself",
    ),
];

/// One file an install would write.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub path: PathBuf,
    pub contents: &'static str,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Pi => "pi",
        }
    }

    /// The directory this provider scans for project skills, under `root`.
    ///
    /// Every row was read off the binary installed on the machine this was
    /// written on, never from memory, and the note says which artifact said so.
    /// Three providers converged on the same layout — one directory per skill,
    /// holding a `SKILL.md` — so the root is the whole of the difference, and
    /// `plan` below is written once rather than three times.
    ///
    /// Public because it is also the answer to "what does choosing this one
    /// mean", which is what `init`'s menu shows beside each name.
    pub fn skills_dir(self, root: &Path) -> PathBuf {
        let [parent, dir] = self.segments();
        root.join(parent).join(dir)
    }

    /// The two path segments of [`skills_dir`](Self::skills_dir), which is the
    /// whole of what a provider decides.
    fn segments(self) -> [&'static str; 2] {
        match self {
            // Claude Code's own convention, and the one the spec is written
            // from. The others are all reachable from it.
            Provider::Claude => [".claude", "skills"],
            // Codex's repository scope from its current skills documentation.
            // `.codex/skills` was accepted by older builds, but current Codex
            // scans `.agents/skills` from the working directory to the repo
            // root; installing under the old root leaves `/skills` empty.
            Provider::Codex => [".agents", "skills"],
            // `<cwd>/.pi/skills`, joined from `CONFIG_DIR_NAME` — which is
            // `.pi` unless a fork's package.json overrides it. Guarded by
            // project trust: pi collects these only inside `if
            // (projectTrusted)`, so the files are correct from the moment they
            // are written and load once the project has been trusted. `install`
            // says so rather than leaving it to be discovered.
            Provider::Pi => [".pi", "skills"],
        }
    }

    /// Where this provider expects skills to live, and under what filename.
    pub fn plan(self, root: &Path) -> Vec<Planned> {
        let skills = self.skills_dir(root);
        SKILLS
            .iter()
            .flat_map(|skill| {
                let dir = skills.join(skill.name);
                let skill_md = match self {
                    Provider::Claude => skill.skill_md,
                    Provider::Codex => skill.codex_skill_md,
                    Provider::Pi => skill.pi_skill_md,
                };
                let mut files = vec![Planned {
                    path: dir.join("SKILL.md"),
                    contents: skill_md,
                }];
                files.extend(skill.assets.iter().map(|(file, contents)| Planned {
                    path: dir.join("assets").join(file),
                    contents,
                }));
                files
            })
            .collect()
    }

    /// What is true of this provider that the file list does not say, or
    /// nothing.
    ///
    /// One line, printed after an install. This exists for a kind that loads
    /// project skills only once a person has trusted the project — and
    /// having a place to put that beats an install that writes four correct
    /// files and leaves a person wondering why the agent cannot see them.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            Provider::Pi => Some(
                "pi loads a project's skills only once the project is trusted — answer its \
                 trust prompt, or start it with `--approve`, or they stay invisible",
            ),
            Provider::Claude | Provider::Codex => None,
        }
    }
}

/// The facts a command may report after the skill files are safely in place.
pub struct Outcome {
    caveat: Option<&'static str>,
}

/// Write the provider's skill files, skipping any that already exist unless
/// `force`. Rendering is left to [`report`], so a caller embedding the install
/// does not inherit a nested file-by-file transcript.
pub fn install(root: &Path, provider: Provider, force: bool) -> Result<Outcome> {
    let planned = provider.plan(root);

    for file in &planned {
        if file.path.exists() && !force {
            continue;
        }
        write_atomic(&file.path, file.contents)?;
    }

    Ok(Outcome {
        caveat: provider.caveat(),
    })
}

/// Print the deliberately small successful-install report.
pub fn report(outcome: Outcome) {
    // Reported whether or not anything was written: a project that installed
    // these last week and has never seen them load wants this warning too.
    if let Some(caveat) = outcome.caveat {
        println!("  note  {caveat}");
    }
    println!("Skills installed successfully.");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every provider's root, spelled out. The one test here that is allowed to
    /// repeat what the code says, because "which directory" is the whole of
    /// what a provider is and a silent change to one is a project whose skills
    /// stop loading with nothing failing.
    fn root_of(provider: Provider) -> [&'static str; 2] {
        match provider {
            Provider::Claude => [".claude", "skills"],
            Provider::Codex => [".agents", "skills"],
            Provider::Pi => [".pi", "skills"],
        }
    }

    #[test]
    fn every_provider_installs_the_same_skills_under_its_own_root() {
        let root = Path::new("/repo");
        for provider in <Provider as clap::ValueEnum>::value_variants() {
            let paths: Vec<PathBuf> = provider
                .plan(root)
                .iter()
                .map(|planned| planned.path.clone())
                .collect();

            // Compared as paths, not as strings: the separator is the
            // platform's, and what this test is about is the shape of the
            // layout — one directory per skill, holding a SKILL.md and an
            // `assets/` beside it — not which slash Windows happens to join
            // with.
            let [parent, dir] = root_of(*provider);
            let skills = root.join(parent).join(dir);
            let expected = vec![
                skills.join("spoolway-plan").join("SKILL.md"),
                skills
                    .join("spoolway-plan")
                    .join("assets")
                    .join("template.html"),
                skills.join("spoolway-plan").join("assets").join("page.md"),
                skills.join("spoolway-tasks").join("SKILL.md"),
                skills.join("spoolway-config").join("SKILL.md"),
                skills.join("spoolway-doctor").join("SKILL.md"),
                skills.join("spoolway-calibrate").join("SKILL.md"),
            ];

            assert_eq!(paths, expected, "{}", provider.name());
        }
    }

    /// Two providers writing to one directory would each report having
    /// installed, and the second would silently be the only one that had — or,
    /// with files already there, neither. Nothing else in this module would
    /// notice.
    #[test]
    fn no_two_providers_claim_the_same_directory() {
        let root = Path::new("/repo");
        let mut seen: Vec<(PathBuf, &str)> = Vec::new();
        for provider in <Provider as clap::ValueEnum>::value_variants() {
            let dir = provider.skills_dir(root);
            if let Some((_, other)) = seen.iter().find(|(path, _)| *path == dir) {
                panic!(
                    "{} and {other} both install into {}",
                    provider.name(),
                    dir.display()
                );
            }
            seen.push((dir, provider.name()));
        }
    }

    /// Provider-specific copies may differ only where the agent's tool really
    /// has a different name, or where the agent cannot reach one. A Claude tool
    /// name in a Codex skill reads like an instruction to call something that
    /// does not exist, which can turn a required question into prose or stop
    /// the workflow entirely.
    #[test]
    fn codex_skills_never_name_claudes_question_tool() {
        for skill in SKILLS {
            assert!(
                !skill.codex_skill_md.contains("AskUserQuestion"),
                "{} still names Claude's question tool in its Codex copy",
                skill.name
            );
        }
    }

    /// pi has no dialog tool. Naming either of the other two in its copy tells
    /// the agent to call something that does not exist, and the question it was
    /// meant to ask is then simply not asked.
    #[test]
    fn pi_skills_name_no_dialog_tool_at_all() {
        for skill in SKILLS {
            for tool in ["AskUserQuestion", "request_user_input"] {
                assert!(
                    !skill.pi_skill_md.contains(tool),
                    "{} names `{tool}` in its pi copy, and pi has no such tool",
                    skill.name
                );
            }
        }
    }

    /// A question dropped in translation is the failure this whole split
    /// exists to avoid: the procedure carries on past a decision the person was
    /// supposed to make. Where Claude's copy asks, every other copy has to ask
    /// too — with that agent's own dialog tool, or, where it has none it can
    /// reach, by saying both that the question is printed and that the turn
    /// ends on it. Printing a question and continuing answers it on the
    /// person's behalf.
    #[test]
    fn a_copy_with_no_dialog_prints_the_question_and_stops() {
        for skill in SKILLS {
            if !skill.skill_md.contains("AskUserQuestion") {
                continue;
            }
            let others = [("codex", skill.codex_skill_md), ("pi", skill.pi_skill_md)];
            for (provider, skill_md) in others {
                if skill_md.contains("request_user_input") {
                    continue;
                }
                let copy = skill_md.to_lowercase();
                assert!(
                    copy.contains("print"),
                    "{}'s {provider} copy asks a question without saying it is printed",
                    skill.name
                );
                assert!(
                    copy.contains("end the turn") || copy.contains("ends there"),
                    "{}'s {provider} copy never says the turn ends on the question",
                    skill.name
                );
                assert!(
                    copy.contains("next prompt"),
                    "{}'s {provider} copy never says where the answer comes back",
                    skill.name
                );
            }
        }
    }

    /// The name a provider prints is the name `--provider` takes. They are
    /// written in two places — `Provider::name` and clap's derive — and a
    /// summary line naming something the flag will not accept is a paste that
    /// fails.
    #[test]
    fn every_providers_name_is_the_one_its_flag_takes() {
        for provider in <Provider as clap::ValueEnum>::value_variants() {
            let spelling = clap::ValueEnum::to_possible_value(provider)
                .expect("every provider is selectable")
                .get_name()
                .to_string();
            assert_eq!(provider.name(), spelling);
        }
    }

    /// The one thing a skill cannot do for itself: a template it tells the
    /// agent to copy has to actually be installed, or the procedure names a
    /// file that is not there.
    #[test]
    fn a_skill_that_names_a_template_ships_it() {
        for skill in SKILLS {
            for (file, _) in skill.assets {
                assert!(
                    skill.skill_md.contains(file),
                    "{} installs {file} and never mentions it",
                    skill.name
                );
            }
            if skill.skill_md.contains("assets/template.yml") {
                assert!(
                    skill.assets.iter().any(|(file, _)| *file == "template.yml"),
                    "{} tells the agent to copy assets/template.yml but ships no such file",
                    skill.name
                );
            }
            if skill.skill_md.contains("assets/template.html") {
                assert!(
                    skill
                        .assets
                        .iter()
                        .any(|(file, _)| *file == "template.html"),
                    "{} tells the agent to copy assets/template.html but ships no such file",
                    skill.name
                );
            }
            if skill.skill_md.contains("assets/page.md") {
                assert!(
                    skill.assets.iter().any(|(file, _)| *file == "page.md"),
                    "{} tells the agent to open assets/page.md but ships no such file",
                    skill.name
                );
            }
        }
    }

    #[test]
    fn every_skill_carries_frontmatter_a_provider_can_read() {
        for skill in SKILLS {
            let name = skill.name;
            for (provider, skill_md) in skill.copies() {
                assert!(
                    skill_md.starts_with("---\n"),
                    "{name}'s {provider} copy has no frontmatter"
                );
                assert!(
                    skill_md.contains(&format!("name: {name}")),
                    "{name}'s {provider} copy has a frontmatter name that does not match its file"
                );
                // Every skill here is human-triggered, except spoolway-tasks
                // and spoolway-config: both are called from inside another
                // procedure (spoolway-plan's step 7 and `spoolway pipeline
                // gen` for spoolway-config),
                // and `disable-model-invocation: true` would make a skill
                // unreachable from there.
                if matches!(name, "spoolway-tasks" | "spoolway-config") {
                    assert!(
                        !skill_md.contains("disable-model-invocation"),
                        "{name}'s {provider} copy must stay reachable from another skill's own \
                         procedure"
                    );
                } else {
                    assert!(
                        skill_md.contains("disable-model-invocation: true"),
                        "{name}'s {provider} copy should be human-invoked only"
                    );
                }
            }
        }
    }

    /// The Agent Skills spec's hard limits, asserted rather than assumed —
    /// these are what a validator rejects a skill for, and every one of them is
    /// a thing a person editing prose could break without noticing.
    #[test]
    fn every_skill_is_valid_by_the_agent_skills_spec() {
        for skill in SKILLS {
            let name = skill.name;
            // `name`: 1–64 chars, lowercase alphanumeric and hyphens, no
            // leading, trailing or doubled hyphen — and it must equal the
            // directory, which here is the name it is installed under.
            assert!((1..=64).contains(&name.len()), "{name} is not 1-64 chars");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{name} has characters the spec does not allow"
            );
            assert!(
                !name.starts_with('-') && !name.ends_with('-') && !name.contains("--"),
                "{name} misuses hyphens"
            );

            // Every provider's copy is a skill in its own right, and a
            // validator will reject it on its own terms — so each one is
            // checked, not just the Claude copy the others are written from.
            for (provider, contents) in skill.copies() {
                let front = contents
                    .split("---\n")
                    .nth(1)
                    .unwrap_or_else(|| panic!("{name}'s {provider} copy has no frontmatter block"));
                let description = front
                    .lines()
                    .find_map(|line| line.strip_prefix("description:"))
                    .unwrap_or_else(|| panic!("{name}'s {provider} copy has no description"))
                    .trim();

                // `description`: non-empty, and 1024 is the spec's ceiling.
                // Claude Code truncates the listing at 1536 including
                // `when_to_use`, so the spec's limit is the binding one either
                // way.
                assert!(
                    !description.is_empty(),
                    "{name}'s {provider} description is empty"
                );
                assert!(
                    description.len() <= 1024,
                    "{name}'s {provider} description is {} chars, past the spec's 1024",
                    description.len()
                );

                // "Keep your main SKILL.md under 500 lines." Past that the body
                // is meant to be split into files loaded on demand.
                let lines = contents.lines().count();
                assert!(
                    lines < 500,
                    "{name}'s {provider} copy is {lines} lines; the spec says under 500"
                );
            }
        }
    }
}
