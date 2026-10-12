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
//! `spoolway-plan` and `spoolway-calibrate` are the exceptions that do carry
//! assets: the plan skeleton and the markup reference beside it, and the
//! calibration report's skeleton, each from its skill's own
//! `assets/skills/claude/<skill>/assets/`. They ship rather than being fetched
//! because there is no command that prints them — a plan page or a report is
//! not a format the binary enforces, so nothing can regenerate one at runtime.

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

impl Skill {
    /// Every provider's copy of this skill. Walked by the tests and by
    /// [`shipped_skill_files`], so a copy added here cannot skip the checks
    /// the others pass or the list of known copies.
    fn copies(&self) -> [(Provider, &'static str); 3] {
        [
            (Provider::Claude, self.skill_md),
            (Provider::Codex, self.codex_skill_md),
            (Provider::Pi, self.pi_skill_md),
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
    // `disable-model-invocation` either — that stayed exempt so `spoolway
    // pipeline gen` could reach it from another procedure; that command is
    // gone now, and nothing currently reaches this skill from another
    // procedure's own turn. It ships no assets: every format it routes to is
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
    // Reads lane-written task records and step-level evaluation and spend data
    // back into whatever produced them — the control plane, and the scripts,
    // skills and source the runs depend on. It uses both the agents' own
    // reports and the numbers, then applies the changes the person chooses.
    Skill {
        name: "spoolway-calibrate",
        skill_md: include_str!("../assets/skills/claude/spoolway-calibrate/SKILL.md"),
        codex_skill_md: include_str!("../assets/skills/codex/spoolway-calibrate/SKILL.md"),
        pi_skill_md: include_str!("../assets/skills/pi/spoolway-calibrate/SKILL.md"),
        // The report every run fills, so its findings table looks the same
        // each time. The Claude copy: it names no provider's tools.
        assets: &[(
            "report.md",
            include_str!("../assets/skills/claude/spoolway-calibrate/assets/report.md"),
        )],
    },
];

/// One skill file this binary ships, as [`crate::known_copies`] names it.
pub struct ShippedSkillFile {
    pub provider: Provider,
    pub skill: &'static str,
    /// Relative to the skill's directory: `SKILL.md` or `assets/<file>`.
    pub file: String,
    pub text: &'static str,
}

/// Every skill file this binary ships: each provider's `SKILL.md`, and each
/// asset once, as the Claude copy that every provider is handed.
pub fn shipped_skill_files() -> Vec<ShippedSkillFile> {
    let mut files = Vec::new();
    for skill in SKILLS {
        for (provider, text) in skill.copies() {
            files.push(ShippedSkillFile {
                provider,
                skill: skill.name,
                file: "SKILL.md".to_string(),
                text,
            });
        }
        for (file, text) in skill.assets {
            files.push(ShippedSkillFile {
                provider: Provider::Claude,
                skill: skill.name,
                file: format!("assets/{file}"),
                text,
            });
        }
    }
    files
}

/// Skill directories this project once shipped under a name it no longer
/// uses, each with the reason `spoolway sync` reports beside it — a short,
/// hand-written literal, and never derived from [`SKILLS`]: a skill this
/// binary actively ships must never appear here by construction, or
/// `spoolway sync` would delete what `install` is about to rewrite in the
/// very same pass. `spoolway-pipeline` was renamed and widened into
/// `spoolway-config` — see [`SKILLS`]'s own comment above. `spoolway-doctor`
/// is retired outright: repair moved into `spoolway-config`, which reads
/// `spoolway doctor --json` itself rather than the read-only skill that used
/// to wrap it.
pub const RETIRED_SKILLS: &[(&str, &str)] = &[
    ("spoolway-pipeline", "renamed to spoolway-config"),
    (
        "spoolway-doctor",
        "retired: /spoolway-config repairs a project now",
    ),
];

/// Templates under `.spoolway/templates/` this project once shipped and no
/// longer does, each with the reason `spoolway sync` reports beside it — a
/// short, hand-written literal, and never derived from anything the binary
/// ships today, for the same reason [`RETIRED_SKILLS`] is: a template this
/// binary still ships must never appear here by construction, and a
/// project's own file under `.spoolway/templates/` — named by neither this
/// list nor the shapes `init` still places — is never touched. `task-log.md`
/// stopped being written when `assets::TASK_LOG`, `config::TASK_LOG_TEMPLATE`
/// and `src/task_log.rs` were removed; `pull-request.md` stopped being read
/// when `spoolway stack` started sending the task file's own body verbatim;
/// `lane-prompts.md` stopped being read when the seven typed messages a
/// lane's pane receives became spoolway's own, with no project override left
/// to resolve against them. `tracking/epic.md` and `ticket.md` stopped being
/// read when the `open` hook started building the whole issue body itself,
/// from the task file and the group description.
pub const RETIRED_TEMPLATES: &[(&str, &str)] = &[
    (
        ".spoolway/templates/task-log.md",
        "no longer written to a task file",
    ),
    (
        ".spoolway/templates/pull-request.md",
        "the pull request body is the task file itself",
    ),
    (
        ".spoolway/templates/lane-prompts.md",
        "the lane messages are spoolway's own",
    ),
    (
        ".spoolway/templates/tracking/epic.md",
        "the issue body is the hook's own",
    ),
    (
        ".spoolway/templates/tracking/ticket.md",
        "the issue body is the hook's own",
    ),
];

/// One file an install would write.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub path: PathBuf,
    pub contents: &'static str,
    /// Which shipped file this is, so a file already on disk can be sorted
    /// into this binary's copy, an older one, or one nobody shipped.
    pub shipped: crate::known_copies::Shipped,
}

impl Planned {
    /// What the file already at this path is, or `None` when there is none to
    /// judge. A file that is not text is a hand edit, so it is unknown; a read
    /// that fails for any other reason is left to the write to report.
    pub(crate) fn on_disk(&self) -> Option<crate::known_copies::Known> {
        use crate::known_copies::{Known, lookup};
        match std::fs::read_to_string(&self.path) {
            Ok(text) => Some(lookup(&self.shipped, &text)),
            Err(err) if err.kind() == std::io::ErrorKind::InvalidData => Some(Known::Unknown),
            Err(_) => None,
        }
    }
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

    /// The directory this provider scans for a person's own skills, the ones
    /// it loads in every project — under `home`, the user's home directory.
    /// Where a home-mode `init` and `spoolway install --user` put skills,
    /// because a home-mode project promises to write nothing into the
    /// checkout, and a project skill folder sits inside it.
    ///
    /// Each row was read off that agent's own documentation, as
    /// [`skills_dir`](Self::skills_dir)'s rows were:
    ///
    /// - Claude Code 2.1.286 names `~/.claude/skills/` in its own help text,
    ///   as the folder "for skills that work in any project".
    /// - Codex's skills documentation (developers.openai.com/codex/skills)
    ///   gives its USER scope as `$HOME/.agents/skills`.
    /// - pi 0.85.1's `docs/skills.md` lists `~/.pi/agent/skills/` as a
    ///   global skill folder, `skills/` under `getAgentDir()`. It also reads
    ///   `~/.agents/skills/`, so a person who installs both Codex and pi at
    ///   user level shows pi each skill twice; the pi copy is the one worded
    ///   for an agent with no dialog tool, which is why pi keeps a folder of
    ///   its own here rather than sharing Codex's.
    pub fn user_skills_dir(self, home: &Path) -> PathBuf {
        match self {
            Provider::Claude => home.join(".claude").join("skills"),
            Provider::Codex => home.join(".agents").join("skills"),
            Provider::Pi => home.join(".pi").join("agent").join("skills"),
        }
    }

    /// Where this provider expects skills to live, and under what filename.
    pub fn plan(self, root: &Path) -> Vec<Planned> {
        self.plan_at(&self.skills_dir(root))
    }

    /// [`plan`](Self::plan), for the user-level folder under `home` — see
    /// [`user_skills_dir`](Self::user_skills_dir).
    pub fn plan_user(self, home: &Path) -> Vec<Planned> {
        self.plan_at(&self.user_skills_dir(home))
    }

    /// Every file this provider's copy of [`SKILLS`] lands as, under
    /// `skills`: the one layout both the project and the user folder take.
    fn plan_at(self, skills: &Path) -> Vec<Planned> {
        SKILLS
            .iter()
            .flat_map(|skill| {
                let dir = skills.join(skill.name);
                let skill_md = match self {
                    Provider::Claude => skill.skill_md,
                    Provider::Codex => skill.codex_skill_md,
                    Provider::Pi => skill.pi_skill_md,
                };
                let shipped = |file: String| crate::known_copies::Shipped::Skill {
                    provider: self,
                    skill: skill.name.to_string(),
                    file,
                };
                let mut files = vec![Planned {
                    path: dir.join("SKILL.md"),
                    contents: skill_md,
                    shipped: shipped("SKILL.md".to_string()),
                }];
                files.extend(skill.assets.iter().map(|(file, contents)| Planned {
                    path: dir.join("assets").join(file),
                    contents,
                    shipped: shipped(format!("assets/{file}")),
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
    /// Where the files landed, shown the way a person would type it back —
    /// relative to the project for [`install`], `~`-shortened for
    /// [`install_user`] — so [`report`] can say where it installed rather
    /// than only that it did.
    dest: String,
    /// Whether any file was actually written, as opposed to every planned
    /// one already sitting there unchanged. `init` folds this into its own
    /// "did this run write anything at all" tally, so a repeat run that
    /// skipped every scaffold file but picked up a provider's skills for
    /// the first time does not also claim there was "nothing to install" —
    /// see the `home-mode-messages` task.
    pub(crate) wrote: bool,
    /// Retired version markers this run deleted, as a person would type them
    /// back. The project's marker was a tracked file, so [`report`] gives each
    /// removed marker its own `removed` row.
    removed_markers: Vec<String>,
}

/// Fail when a skill file `init` would write is none of the copies spoolway
/// has shipped.
///
/// Asked by `init` before it writes anything: it lays down a scaffold ahead of
/// the skills, and a refusal arriving with the skills would come after the
/// scaffold was already down.
pub(crate) fn ensure_installable(root: &Path, provider: Provider, user_level: bool) -> Result<()> {
    // A user-level install writes nothing into the checkout, so the project's
    // skill files are not its business. With no home directory it checks
    // nothing at all: `install_user` gives the no-home error itself.
    match (user_level, user_home()) {
        (true, Some(home)) => ensure_known(&provider.plan_user(&home), crate::repo::shorten_home),
        (true, None) => Ok(()),
        (false, _) => ensure_known(&provider.plan(root), |path| {
            crate::fmt::relative(root, path)
        }),
    }
}

/// Fail, writing nothing, when any of `planned` already exists as a file that
/// is neither this binary's copy nor an older one it shipped.
///
/// Such a file was written by a newer spoolway or edited by hand, and
/// installing over it would roll it back or discard the edit. Every offending
/// file is named, each with the reason and the remedy `sync` gives, so one run
/// shows the whole set rather than one file per attempt. `shown` spells a path
/// the way a person would type it back to `spoolway sync --replace`.
fn ensure_known(planned: &[Planned], shown: impl Fn(&Path) -> String) -> Result<()> {
    let refused: Vec<String> = planned
        .iter()
        .filter(|file| file.on_disk() == Some(crate::known_copies::Known::Unknown))
        .map(|file| {
            let shown = shown(&file.path);
            format!("refused {shown} — {}", crate::known_copies::refusal(&shown))
        })
        .collect();
    match refused.is_empty() {
        true => Ok(()),
        false => anyhow::bail!("{}", refused.join("\n")),
    }
}

/// Write the provider's skill files: a missing one, and one that is an older
/// copy spoolway shipped. One matching this binary's copy is left alone unless
/// `force`, and one that is neither refuses the install before anything is
/// written. Rendering is left to [`report`], so a caller embedding the install
/// does not inherit a nested file-by-file transcript.
///
/// `spoolway sync` makes the same call per file, so a skill is brought forward
/// by whichever command runs first. A file no spoolway shipped stops the
/// install, `force` or not: `force` means "write files that already match",
/// and an unknown file is a statement about the file, not about this run.
///
/// Also deletes the project's retired version marker, as `sync` does.
pub fn install(root: &Path, provider: Provider, force: bool) -> Result<Outcome> {
    let dest = crate::fmt::relative(root, &provider.skills_dir(root));
    let planned = provider.plan(root);
    let mut outcome = write_planned(&planned, provider.caveat(), force, dest, |path| {
        crate::fmt::relative(root, path)
    })?;
    outcome
        .removed_markers
        .extend(crate::sync::remove_marker(crate::sync::project_marker(
            root,
        ))?);
    Ok(outcome)
}

/// [`install`], into the provider's user-level folder rather than a
/// project's — a home-mode `init`, and `spoolway install --user`. Refused
/// when there is no home directory to install under, rather than writing a
/// `.claude/` into whatever directory a relative path would land in. Also
/// deletes the folder's retired version marker.
pub fn install_user(provider: Provider, force: bool) -> Result<Outcome> {
    let Some(home) = user_home() else {
        anyhow::bail!(
            "cannot install {} skills at user level: no home directory is set ($HOME is empty)\n  \
             set $HOME and run this again, or run `spoolway install {}` without --user inside \
             a repo-mode project",
            provider.name(),
            provider.name()
        );
    };
    // No caveat: pi's is about trusting a project before it loads that
    // project's skills, and a user folder is loaded without asking.
    let dest = crate::repo::shorten_home(&provider.user_skills_dir(&home));
    let mut outcome = write_planned(
        &provider.plan_user(&home),
        None,
        force,
        dest,
        crate::repo::shorten_home,
    )?;
    outcome
        .removed_markers
        .extend(crate::sync::remove_marker(crate::sync::user_marker(
            &provider.user_skills_dir(&home),
        ))?);
    Ok(outcome)
}

/// The home directory user-level skills are installed under and synced in.
///
/// Inside the test binary this is only ever a scratch home a test set with
/// `crate::platform::test_home::with_home`, never the real `$HOME`: every
/// sync test scans user-level folders too, and one run on a machine whose
/// person installed spoolway's skills at user level would otherwise rewrite
/// that person's real `~/.claude/skills/` from whatever branch was under test.
#[cfg(not(test))]
pub(crate) fn user_home() -> Option<PathBuf> {
    crate::platform::home_dir()
}

/// See the non-test [`user_home`].
#[cfg(test)]
pub(crate) fn user_home() -> Option<PathBuf> {
    crate::platform::test_home::current()
}

/// Write each planned file, and carry `caveat` and `dest` on to [`report`].
///
/// A file matching the shipped copy is left alone unless `force`, the same
/// "nothing to do" `sync` would itself report. A stale one left by an older
/// release is rewritten, `force` or not: a repeat `init` that found only stale
/// copies must say it wrote them, never "Skills already installed" beside
/// content that still differs from what this binary ships. A file that is
/// none of the copies spoolway shipped is refused first, by [`ensure_known`],
/// so no skill is rewritten in a set that holds one.
fn write_planned(
    planned: &[Planned],
    caveat: Option<&'static str>,
    force: bool,
    dest: String,
    shown: impl Fn(&Path) -> String,
) -> Result<Outcome> {
    ensure_known(planned, shown)?;
    let mut wrote = false;
    for file in planned {
        if file.path.exists()
            && !force
            && let Ok(on_disk) = std::fs::read_to_string(&file.path)
            && on_disk == file.contents
        {
            continue;
        }
        write_atomic(&file.path, file.contents)?;
        wrote = true;
    }

    Ok(Outcome {
        caveat,
        dest,
        wrote,
        removed_markers: Vec::new(),
    })
}

/// Print the deliberately small successful-install report.
///
/// Says "already" rather than "successfully" when nothing was actually
/// written — every planned file already sat there, unforced — so a plain
/// repeat `init` does not claim to have just installed these skills right
/// next to its own "nothing to install" line for the very same run; see the
/// `home-mode-messages` task's review finding 1.
pub fn report(outcome: Outcome) {
    // Reported whether or not anything was written: a project that installed
    // these last week and has never seen them load wants this warning too.
    if let Some(caveat) = outcome.caveat {
        println!("  note  {caveat}");
    }
    for marker in &outcome.removed_markers {
        println!("{}", crate::commands::init::report_row("removed", marker));
    }
    if outcome.wrote {
        println!("Skills installed successfully, into {}.", outcome.dest);
    } else {
        println!("Skills already installed, in {}.", outcome.dest);
    }
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
                skills.join("spoolway-calibrate").join("SKILL.md"),
                skills
                    .join("spoolway-calibrate")
                    .join("assets")
                    .join("report.md"),
            ];

            assert_eq!(paths, expected, "{}", provider.name());
        }
    }

    /// Every provider's user-level folder, spelled out for the same reason
    /// [`root_of`] is: a silent change to one is a person whose skills stop
    /// loading in every project with nothing failing.
    #[test]
    fn every_provider_has_its_documented_user_folder() {
        let home = Path::new("/home/someone");
        for (provider, expected) in [
            (Provider::Claude, home.join(".claude").join("skills")),
            (Provider::Codex, home.join(".agents").join("skills")),
            (Provider::Pi, home.join(".pi").join("agent").join("skills")),
        ] {
            assert_eq!(
                provider.user_skills_dir(home),
                expected,
                "{}",
                provider.name()
            );
        }
    }

    /// The user-level counterpart of [`no_two_providers_claim_the_same_directory`].
    #[test]
    fn no_two_providers_claim_the_same_user_folder() {
        let home = Path::new("/home/someone");
        let dirs: Vec<PathBuf> = <Provider as clap::ValueEnum>::value_variants()
            .iter()
            .map(|provider| provider.user_skills_dir(home))
            .collect();
        for (i, dir) in dirs.iter().enumerate() {
            assert!(
                !dirs[i + 1..].contains(dir),
                "{} is claimed twice",
                dir.display()
            );
        }
    }

    /// `install --user` writes the same files a project install would, under
    /// the user folder of the home it runs in, and nothing anywhere else.
    #[test]
    fn install_user_writes_every_skill_under_the_user_folder() {
        let home = crate::scratch::root("install-user");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        crate::platform::test_home::with_home(&home, || {
            install_user(Provider::Codex, false).unwrap();
        });
        for planned in Provider::Codex.plan_user(&home) {
            assert!(planned.path.is_file(), "{} missing", planned.path.display());
        }
        assert!(!home.join(".claude").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A version marker left by 0.10.0 is deleted by `install` and `install
    /// --user`, each reporting its own file as removed, and a second run has
    /// nothing left to name.
    #[test]
    fn install_deletes_a_retired_version_marker_and_names_it() {
        let home = crate::scratch::root("install-marker-row");
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("project");
        std::fs::create_dir_all(root.join(".spoolway")).unwrap();
        let project_marker = root.join(".spoolway").join("spoolway-version");
        let user_marker = Provider::Claude
            .user_skills_dir(&home)
            .join(".spoolway-version");
        std::fs::create_dir_all(user_marker.parent().unwrap()).unwrap();
        std::fs::write(&project_marker, "0.10.0\n").unwrap();
        std::fs::write(&user_marker, "0.10.0\n").unwrap();

        let project = install(&root, Provider::Claude, false).unwrap();
        assert_eq!(project.removed_markers, [".spoolway/spoolway-version"]);
        let user = crate::platform::test_home::with_home(&home, || {
            install_user(Provider::Claude, false).unwrap()
        });
        assert_eq!(user.removed_markers, ["~/.claude/skills/.spoolway-version"]);
        assert!(!project_marker.exists() && !user_marker.exists());

        let again = install(&root, Provider::Claude, false).unwrap();
        assert!(again.removed_markers.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A repeat `install --user` must rewrite a stale skill file left by an
    /// older release and say so — not skip it the way a scaffold file's own
    /// `--force` gate would, and not report "already installed" beside
    /// content that still differs from what this binary ships.
    #[test]
    fn install_user_rewrites_a_stale_skill_and_reports_it_wrote() {
        let home = crate::scratch::root("install-user-stale");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        crate::platform::test_home::with_home(&home, || {
            install_user(Provider::Claude, false).unwrap();
        });
        let stale = Provider::Claude
            .user_skills_dir(&home)
            .join("spoolway-config")
            .join("SKILL.md");
        let older = crate::known_copies::older::skill(Provider::Claude, "spoolway-config");
        std::fs::write(&stale, older).unwrap();

        let outcome = crate::platform::test_home::with_home(&home, || {
            install_user(Provider::Claude, false).unwrap()
        });

        assert!(outcome.wrote, "a stale skill file must count as written");
        assert_ne!(
            std::fs::read_to_string(&stale).unwrap(),
            older,
            "the stale copy must be rewritten to the shipped one"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A skill file that is no copy any release shipped is a newer spoolway's
    /// work or a hand edit. `install` and `install --user` refuse it, naming
    /// the file and `sync --replace`, and write nothing at all, not even the
    /// skills that are missing or stale beside it. `--force` is no way round.
    #[test]
    fn install_refuses_a_skill_no_release_shipped_and_writes_nothing() {
        let home = crate::scratch::root("install-unknown");
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("project");
        std::fs::create_dir_all(&root).unwrap();
        let project_skill = Provider::Claude
            .skills_dir(&root)
            .join("spoolway-config")
            .join("SKILL.md");
        let user_skill = Provider::Claude
            .user_skills_dir(&home)
            .join("spoolway-config")
            .join("SKILL.md");
        for skill in [&project_skill, &user_skill] {
            std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
            std::fs::write(skill, "edited by hand\n").unwrap();
        }

        for force in [false, true] {
            let project = format!(
                "{:#}",
                install(&root, Provider::Claude, force).err().unwrap()
            );
            assert!(
                project.contains(
                    "refused .claude/skills/spoolway-config/SKILL.md — not a copy this spoolway \
                     knows"
                ),
                "{project}"
            );
            assert!(
                project
                    .contains("`spoolway sync --replace .claude/skills/spoolway-config/SKILL.md`"),
                "{project}"
            );
            let user = crate::platform::test_home::with_home(&home, || {
                format!("{:#}", install_user(Provider::Claude, force).err().unwrap())
            });
            assert!(
                user.contains(
                    "`spoolway sync --replace ~/.claude/skills/spoolway-config/SKILL.md`"
                ),
                "{user}"
            );
        }

        for skill in [&project_skill, &user_skill] {
            assert_eq!(std::fs::read_to_string(skill).unwrap(), "edited by hand\n");
        }
        assert!(
            !Provider::Claude
                .skills_dir(&root)
                .join("spoolway-plan")
                .exists(),
            "no sibling skill was written"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A user-level install writes no skill into the checkout, so a hand-edited
    /// one there is not its to refuse, whether or not there is a home to look
    /// in. Without a home it checks nothing, since `install_user` gives that
    /// error itself; otherwise a refusal would name `sync --replace` for a
    /// project file that a home-mode `sync` itself will not replace.
    #[test]
    fn a_user_level_check_leaves_the_checkouts_skills_alone() {
        let home = crate::scratch::root("installable-user-level");
        let _ = std::fs::remove_dir_all(&home);
        let root = home.join("project");
        let skill = Provider::Claude
            .skills_dir(&root)
            .join("spoolway-config")
            .join("SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "edited by hand\n").unwrap();

        let project = format!(
            "{:#}",
            ensure_installable(&root, Provider::Claude, false)
                .err()
                .unwrap()
        );
        assert!(
            project.contains("refused .claude/skills/spoolway-config"),
            "{project}"
        );

        ensure_installable(&root, Provider::Claude, true).unwrap();
        crate::platform::test_home::with_home(&home, || {
            ensure_installable(&root, Provider::Claude, true).unwrap();
        });
        let _ = std::fs::remove_dir_all(&home);
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

    /// `spoolway-config` routes among four cases — edit the tracked setup,
    /// make a private copy, tweak one value, or `init` — and its "never
    /// reach past" rule has to bound every write by `config path --json`'s
    /// own places, in every provider's copy, not only Claude's own. A drift
    /// here is a person on Codex or pi being routed by a rule the other two
    /// agents never read.
    #[test]
    fn spoolway_config_routes_the_same_four_cases_in_every_provider_copy() {
        let skill = SKILLS
            .iter()
            .find(|s| s.name == "spoolway-config")
            .expect("spoolway-config is a shipped skill");
        for (provider, copy) in skill.copies() {
            let provider = provider.name();
            for route in [
                "Edit the setup, for everyone",
                "Try it, just for me",
                "Tweak one value here",
                "Set up, join a workspace, or start from nothing",
            ] {
                assert!(
                    copy.contains(route),
                    "spoolway-config's {provider} copy is missing the {route:?} route"
                );
            }
            for path in ["pipeline copy", "prompt copy", "pipeline promote", "init"] {
                assert!(
                    copy.contains(path),
                    "spoolway-config's {provider} copy never mentions `{path}`"
                );
            }
            assert!(
                copy.contains("config path --json"),
                "spoolway-config's {provider} copy does not read locations from `config path \
                 --json`"
            );
            assert!(
                copy.contains("Never reach past what `config path --json` prints"),
                "spoolway-config's {provider} copy's \"never reach past\" rule does not bound \
                 writes by `config path --json`"
            );
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
            if skill.skill_md.contains("assets/report.md") {
                assert!(
                    skill.assets.iter().any(|(file, _)| *file == "report.md"),
                    "{} tells the agent to copy assets/report.md but ships no such file",
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
                let provider = provider.name();
                assert!(
                    skill_md.starts_with("---\n"),
                    "{name}'s {provider} copy has no frontmatter"
                );
                assert!(
                    skill_md.contains(&format!("name: {name}")),
                    "{name}'s {provider} copy has a frontmatter name that does not match its file"
                );
                // Every skill here is human-triggered, except spoolway-tasks
                // and spoolway-config: `disable-model-invocation: true` would
                // make a skill unreachable from inside another procedure, and
                // spoolway-plan's step 7 still calls into spoolway-tasks that
                // way. Nothing currently calls into spoolway-config the same
                // way — that used to be `spoolway pipeline gen`, now
                // retired — but the exemption stays alongside it rather than
                // this test deciding a skill's own reachability policy.
                //
                // Checked against the frontmatter block alone (between the
                // first two `---` lines), not the whole file: spoolway-config
                // names `disable-model-invocation: true` in its own body
                // prose, instructing the shared skills it may write, and that
                // is not this skill's own frontmatter.
                let frontmatter = skill_md.split("---\n").nth(1).unwrap_or_default();
                if matches!(name, "spoolway-tasks" | "spoolway-config") {
                    assert!(
                        !frontmatter.contains("disable-model-invocation"),
                        "{name}'s {provider} copy's own frontmatter must stay reachable from \
                         another skill's own procedure"
                    );
                } else {
                    assert!(
                        frontmatter.contains("disable-model-invocation: true"),
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
                let provider = provider.name();
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

    /// This repo carries installed copies of the shipped skills, and CI runs
    /// `spoolway init` and then fails on any diff it leaves. An edit made to an
    /// installed copy and not to `assets/skills/` passes every other test and
    /// fails only there, minutes into a CI run. This catches it in
    /// `cargo test`. A provider whose folder this checkout does not carry is
    /// skipped, because nothing here can drift from it.
    #[test]
    fn this_repos_installed_skills_match_the_shipped_copies() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        for provider in <Provider as clap::ValueEnum>::value_variants() {
            if !provider.skills_dir(root).is_dir() {
                continue;
            }
            for planned in provider.plan(root) {
                let shown = planned.path.strip_prefix(root).unwrap().display();
                let on_disk = std::fs::read_to_string(&planned.path)
                    .unwrap_or_else(|e| panic!("{shown}: {e}"));
                assert!(
                    on_disk == planned.contents,
                    "{shown} differs from its {} copy under assets/skills/. Edit the \
                     shipped copy there, then run `spoolway install {} --force` to \
                     refresh this one",
                    provider.name(),
                    provider.name(),
                );
            }
        }
    }

    /// A fresh install leaves nothing for a sync right afterwards to do: every
    /// skill file it just wrote already matches the shipped copy, so a
    /// dry-run scan reads every one of them as kept, not as a rewrite.
    #[test]
    fn install_leaves_a_project_a_sync_finds_nothing_to_change() {
        let root = crate::scratch::root("install-then-sync-is-a-noop");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let home = root.join(".home");

        install(&root, Provider::Claude, false).unwrap();

        // `scan` also covers `.gitignore`, `config.toml` and the task
        // templates dir, none of which this fixture set up, so only the
        // skill files' own outcomes are asserted on.
        std::fs::create_dir_all(root.join(crate::config::TASK_TEMPLATES_DIR)).unwrap();
        let repo = crate::repo::Repo {
            borrowed: false,
            checkout: root.to_path_buf(),
            root: root.to_path_buf(),
            config: crate::config::Config::default(),
            home,
        };
        let outcomes = crate::sync::scan(
            &repo,
            &crate::cli::SyncArgs {
                dry_run: true,
                replace: Vec::new(),
            },
        )
        .unwrap();
        let claude_dir = Provider::Claude.skills_dir(&root);
        assert!(
            outcomes.iter().all(|o| !matches!(
                o,
                crate::sync::Outcome::Wrote { path, .. }
                    if root.join(path).starts_with(&claude_dir)
            )),
            "a file `install` just wrote must not read as needing a rewrite: {:?}",
            outcomes
                .iter()
                .filter_map(|o| match o {
                    crate::sync::Outcome::Wrote { path, detail } => Some((path, detail)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        );
    }
}
