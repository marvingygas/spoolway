//! Prompts — the role a lane plays, and the contract it is written against.
//!
//! A prompt is a directory under `.spoolway/prompts/` holding a `PROMPT.md`,
//! selected by a step's `prompt:`. Nothing registers one, nothing compiles it
//! in, and — since the file format was removed — nothing parses one either. It
//! is prose the project owns outright: `spoolway update` never touches a
//! prompt, and `init` writes one only where none exists.
//!
//! The directory exists so a role can keep belongings beside its prose, in an
//! `assets/` of its own — the archivist's document skeletons are the only ones
//! today. Those inherit the prompt's rules, not a skeleton's: written once,
//! never updated, the project's to restyle. [`path_for`] still reads the flat
//! `<name>.md` a project set up before this, so an upgrade finds its prompts.
//!
//! Which leaves this module two jobs, and no third.
//!
//! It *emits* the contract a prompt is written against, rather than describing
//! it a second time. Every line of `spoolway prompt contract` has a source
//! already in the binary: both halves of what a lane is sent are rendered by the
//! dispatcher's own functions. A document saying the same things would be wrong
//! the first time one of them changed, and nothing would fail to say so.
//!
//! And it *reads* what a project wrote, against the step that runs it — for a
//! `spoolway …` command this release does not have, checked against clap's
//! own command tree ([`stale_commands`]). This is the one rule left in this
//! lint: the others read prose for style and opinion, and a check that can
//! only ever be somebody's taste is not one worth failing a build over. This
//! rule is different, because it is a fact rather than an opinion — this is
//! what an upgrade used to prevent by regenerating half the file, and nothing
//! is regenerated now, so the drift has to be found by reading instead, which
//! is the trade the format's removal actually made. A surviving finding fails
//! `spoolway pipeline check`, the only command that reaches this lint.
//!
//! What it no longer reads for is a git verb the step was not granted. Nothing
//! grants git verbs any more: git up to the pull request is `spoolway
//! stack`'s, run from a command step rather than read out of a prompt, and
//! policing which command it may run belongs to the harness rather than to a
//! lint over prose.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::cli::PromptContractArgs;
use crate::pipeline::{Pipeline, Pipelines, Step, StepKind};
use crate::repo::Repo;
use crate::task::Task;

/// The environment every lane is started with, and what each variable is for.
///
/// Kept beside the contract that prints it rather than in `dispatch.rs`, because
/// the *explanation* is only ever read here; the assignments stay where the lane
/// is launched.
///
/// This table is the only description of the environment a prompt author ever
/// reads, and a lane told about a variable it will not be given is worse than
/// one told nothing — so
/// `dispatch::tests::the_prompt_contract_promises_no_variable_a_lane_lacks`
/// starts a lane and checks that every name here is one the launch site
/// actually sets. One direction only: a variable a lane sets and this does not
/// mention is spoolway's own, which is what `$SPOOLWAY_HEAD` is.
pub const ENVIRONMENT: &[(&str, &str)] = &[
    (
        "SPOOLWAY_TASK",
        "the task's id — which is why `spoolway report` needs no argument",
    ),
    (
        "SPOOLWAY_TASK_FILE",
        "the task file's path, the same one the typed message names",
    ),
    ("SPOOLWAY_REPO", "the project root — not your worktree"),
    (
        "SPOOLWAY_STEP",
        "which step this is — not yours to read, and a prompt that branches on it is two prompts",
    ),
    (
        "SPOOLWAY_WORKTREE",
        "your worktree — already your cwd, so rarely needed",
    ),
    (
        "SPOOLWAY_SCRATCH",
        "writable space outside the worktree, one per task, removed when the task is archived",
    ),
];

/// A prompt file on disk.
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
}

/// One problem with a prompt, found by reading it against the step that runs it.
///
/// Heuristic on purpose, and it says so: this reads prose looking for commands.
/// It is worth having anyway, because the alternative is finding out twenty
/// minutes into a lane, as a refusal in a pane nobody is watching.
pub struct Finding {
    pub prompt: String,
    /// Where the prompt is used, as `pipeline/step`, or empty for a file-level
    /// finding that holds wherever it runs.
    pub at: String,
    pub message: String,
}

impl Finding {
    pub fn render(&self) -> String {
        if self.at.is_empty() {
            format!("{}: {}", self.prompt, self.message)
        } else {
            format!("{} ({}): {}", self.prompt, self.at, self.message)
        }
    }
}

/// `spoolway prompt contract` — the whole of what a prompt is written against.
pub fn contract(repo: &Repo, pipelines: &Pipelines, args: &PromptContractArgs) -> Result<()> {
    let (pipeline, step, task, sampled) = subject(repo, pipelines, args)?;

    let profile = step
        .agent
        .as_deref()
        .and_then(|name| repo.config.agents.get(name));
    let kind = profile.map(|p| p.kind.as_str()).unwrap_or("?");

    println!("THE LANE CONTRACT");
    println!("=================");
    println!(
        "For `{}`/`{}` — agent profile `{}`, kind `{}`.",
        pipeline.name,
        step.id,
        step.agent.as_deref().unwrap_or("?"),
        kind
    );
    println!();
    println!(
        "Everything below is read out of this project's own pipeline and config, and out\n\
         of the code that starts a lane. It is what a prompt is written against."
    );

    let prompt_path = path_for(repo, step.prompt_name());

    println!();
    println!("1  WHERE THE PROMPT GOES");
    println!("   {}", prompt_path.display());
    println!("   The step's `prompt:` names the directory; a step without one uses its own id.");
    if let Some(profile) = profile {
        match prompt_flag(profile) {
            Some(flag) => println!(
                "   Composed into the lane's system prompt at launch, and handed over as `{flag}`."
            ),
            // Nothing renders `{prompt_file}` into this profile's args, so the
            // file is written, validated, listed — and never handed to the
            // agent. Silence here would have the author write a prompt for a
            // lane that will never read a word of it.
            None => {
                println!(
                    "   WARNING: agent profile `{}` never uses `{{prompt_file}}` in its args, so",
                    step.agent.as_deref().unwrap_or("?")
                );
                println!(
                    "   this file is never given to the agent. Add it to the profile's `args`"
                );
                println!("   in config.toml — `spoolway doctor` says which flag its kind expects.");
            }
        }
    }

    println!();
    println!("2  THE SYSTEM PROMPT THIS LANE IS STARTED WITH");
    if sampled {
        println!("   Rendered against a sample task — pass `--task <id>` for a real one.");
    } else {
        println!(
            "   Rendered for task `{}`, exactly as its lane received it.",
            task.id()
        );
    }
    println!("   spoolway's framing, then the prompt above verbatim, then this pass's policy.");
    println!("   Only the middle section is yours — do not restate the rest of it.");
    println!();
    // The file itself rather than a paraphrase of it, because the thing being
    // shown is exactly what the agent is handed, and a prompt is written
    // against what the lane actually reads.
    let prompt = std::fs::read_to_string(&prompt_path)
        .unwrap_or_else(|_| format!("[no prompt at {} — `spoolway init`]", prompt_path.display()));
    for line in crate::compose::system_prompt(repo, &task, pipeline, step, &prompt)?.lines() {
        println!("   | {line}");
    }

    println!();
    println!("3  THE MESSAGE TYPED INTO ITS PANE, ONCE IT IS UP");
    println!(
        "   Seven of these, one per state, each read from {} when the project has",
        crate::lane_prompts::path(repo).display()
    );
    println!("   written that section and spoolway's own words otherwise.");
    for state in crate::lane_prompts::STATES {
        println!();
        println!("   This is `{state}`:");
        for line in
            crate::compose::lane_prompt_for_state(repo, &task, pipeline, step, state).lines()
        {
            println!("   | {line}");
        }
    }

    println!();
    println!("4  THE ENVIRONMENT EVERY LANE HAS");
    let width = ENVIRONMENT
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    for (name, why) in ENVIRONMENT {
        println!("   {name:width$}  {why}");
    }

    println!();
    println!("5  WHAT A LANE MAY REACH");
    println!("   Whatever the person running the dispatcher can. What a lane is given is its");
    println!("   worktree, its task file and its scratch directory — but nothing prevents it");
    println!("   going elsewhere, and a prompt is the only place spoolway states what a step");
    println!("   is for, so a role that should stay narrow has to say so in its own prose and");
    println!("   hold itself to it.");
    println!();
    println!("   Nothing inside spoolway bounds it further. Confinement, if a project wants");
    println!("   any, is the person's own agent settings — outside this repository entirely.");

    println!();
    println!("6  HOW A LANE FINISHES");
    // `blocked` gets two forms, not three: `--fail` and `--block` are still
    // accepted there, but `commands::report` reads either the same way it
    // reads a `--pause`, so offering them here as a distinct choice would
    // teach a lane a shape that no longer routes anywhere different.
    if step.id == crate::pipeline::BLOCKED {
        println!("   spoolway report --pass  -m \"<one line on what happened>\"");
        println!("   spoolway report --pause -m \"<what needs a person, and why>\"");
    } else {
        println!("   spoolway report --pass  -m \"<one line on what happened>\"");
        println!("   spoolway report --fail  -m \"<one line on what happened>\"");
        println!("   spoolway report --block -m \"<what is in the way>\"");
    }
    println!();
    println!("   Exactly once, then stop. Where each outcome takes this task:");
    // `blocked` itself is the one step whose graph edges lie: it declares no
    // `on_pass`, and `step.destination` reading that as "stays put" would tell
    // the one lane most likely to read this that a pass leaves the task where
    // it is, which is the opposite of true — a pass moves it on from whatever
    // it blocked on, this lane's own work standing in for that step's. And
    // nothing short of `--pause` — or a `--fail`/`--block`, read the same way
    // — ever routes back onto `blocked` itself any more: both park the task
    // on `paused`, with the same destination a pass would have reached
    // waiting for a person.
    if step.id == crate::pipeline::BLOCKED {
        println!("     --pass  → on from whatever this task blocked on, your work standing in");
        println!(
            "     --pause → `paused`, never back onto `blocked` — the same destination a pass would have reached, waiting for a person"
        );
    } else {
        for (outcome, label) in [
            (crate::pipeline::Outcome::Pass, "--pass "),
            (crate::pipeline::Outcome::Fail, "--fail "),
            (crate::pipeline::Outcome::Block, "--block"),
        ] {
            match step.destination(outcome) {
                Some(next) => println!("     {label} → `{next}`"),
                None => println!("     {label} → stays on `{}`", step.id),
            }
        }
    }
    println!();
    println!("7  THE SHAPE TO WRITE");
    for line in PROMPT_SKELETON.lines() {
        println!("   | {line}");
    }
    println!();
    println!("   Headings and bullets a small local model can skim. One default per choice,");
    println!("   never a menu. Write only what the model does not already know: the traps,");
    println!("   the conventions, the mistake it is about to make.");

    println!();
    println!("Write the prompt at the altitude of the role: what to read, what to judge,");
    println!("what to write, when to stop. It never needs to know the pipeline's shape.");
    Ok(())
}

/// The shape a prompt file is written in — section 7 of `spoolway prompt
/// contract`, on every call, whatever `--step` names: the headings a role's
/// prose fills in, not what any one role says under them.
const PROMPT_SKELETON: &str = "\
# <role>

## What you are looking at
## How to do it here
## Never";

/// Which step this contract is for, and a task to render its prompt against.
fn subject<'a>(
    repo: &Repo,
    pipelines: &'a Pipelines,
    args: &PromptContractArgs,
) -> Result<(&'a Pipeline, &'a Step, Task, bool)> {
    // A real task decides both the pipeline and the step, unless they were named.
    if let Some(id) = &args.task {
        let task = repo.task(id)?;
        let pipeline = pipelines.for_task(&task)?;
        let step_id = args
            .step
            .clone()
            .unwrap_or_else(|| task.stage().to_string());
        let step = pipeline.require_step(&step_id)?;
        // A task sitting on `queued` or `done` has no prompt to write: say so
        // rather than printing a contract for a step no agent runs.
        agent_step(pipeline, step)?;
        return Ok((pipeline, step, task, false));
    }

    let pipeline = match &args.pipeline {
        Some(name) => pipelines.get(name)?,
        None => pipelines.get(&pipelines.default)?,
    };

    let step = match &args.step {
        Some(id) => pipeline.require_step(id)?,
        // The first step that actually runs an agent: the entry step is a `wait`
        // and has no prompt, so it would print a contract for nobody.
        None => pipeline
            .steps
            .iter()
            .find(|step| step.kind() == StepKind::Agent)
            .with_context(|| format!("pipeline `{}` has no agent step", pipeline.name))?,
    };

    agent_step(pipeline, step)?;
    let sample = sample_task(repo, pipeline.task_template_name(), &step.id)?;
    Ok((pipeline, step, sample, true))
}

/// Refuse a step no agent runs. `wait` and `terminal` steps have no lane, no
/// prompt and no prompt — a contract for one would be a page of blanks.
fn agent_step(pipeline: &Pipeline, step: &Step) -> Result<()> {
    if step.kind() != StepKind::Agent {
        bail!(
            "`{}`/`{}` is a `{}` step: no agent runs there, so there is no prompt to write \
             for it. Agent steps here: {}",
            pipeline.name,
            step.id,
            step.kind().as_str(),
            pipeline
                .steps
                .iter()
                .filter(|step| step.kind() == StepKind::Agent)
                .map(|step| step.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// A task that does not exist, so the contract can be printed in a project with
/// an empty queue. Parsed from the real format rather than constructed, so it
/// cannot drift from what a lane is actually handed.
///
/// Its body is this project's own skeleton rather than an invented example, for
/// the same reason: a prompt author reading the contract should see the shape
/// of the task files their lane will actually open, not the shape spoolway
/// would have picked.
fn sample_task(repo: &Repo, pipeline: &str, stage: &str) -> Result<Task> {
    let skeleton = crate::task_template::resolve(repo, pipeline);
    let raw = format!("---\nid: example\nstage: {stage}\ntouches:\n  - src/**\n---\n{skeleton}");
    Task::parse(repo.queue_dir().join("example.md"), &raw)
}

/// How this profile hands the prompt to its agent, as its kind's argv
/// template actually spells it — so the contract shows the flag in use, not
/// the one we assume.
fn prompt_flag(profile: &crate::config::AgentProfile) -> Option<String> {
    let args = crate::agent::adapter(&profile.kind)?.args;
    let index = args.iter().position(|arg| arg.contains("{prompt_file}"))?;
    let flag = if index == 0 { args[0] } else { args[index - 1] };
    Some(format!("{flag} <prompt file>"))
}

/// Where a prompt's prose lives, whichever shape this project keeps it in,
/// with `overrides/prompts/<name>/PROMPT.md` — see [`crate::overrides`] —
/// preferred whole over the tracked file when it exists: a prompt is prose,
/// with no key in it for a patch to merge onto, so an override of one
/// replaces it rather than being folded in.
///
/// A prompt is a directory holding a `PROMPT.md`, so that a role with
/// belongings has somewhere to keep them. Before that it was a flat
/// `<name>.md`, and a project upgraded in place still has eight of those.
///
/// Reading accepts either and writing only ever produces the directory: an
/// upgrade that stopped finding a project's prompts would fail at dispatch,
/// having been given no chance to say so first.
///
/// When neither exists this names the directory shape, so an error points at
/// where a prompt belongs rather than where it used to.
pub fn path_for(repo: &Repo, name: &str) -> PathBuf {
    if let Some(overridden) = crate::overrides::prompt_override(&repo.overrides_dir(), name) {
        return overridden;
    }
    path_for_tracked(repo, name)
}

/// [`path_for`], with no patch layer applied — for a caller that must see
/// only the tracked file: `commands::prompt_override` reads the source to
/// fork, and `commands::override_promote` the destination to write.
pub fn path_for_tracked(repo: &Repo, name: &str) -> PathBuf {
    let nested = directory_form(repo, name);
    if nested.is_file() {
        return nested;
    }
    let flat = repo.prompts_dir().join(format!("{name}.md"));
    if flat.is_file() {
        return flat;
    }
    nested
}

/// The directory shape of a prompt's file — `<prompts>/<name>/PROMPT.md` —
/// whether or not it exists on disk.
///
/// [`path_for`] prefers this over the legacy flat `<name>.md` whenever it is
/// present, so a caller that means to replace the flat file needs this to tell
/// when the flat path has become one nothing reads. Joining onto the prompts
/// directory is this module's alone (see `commands::tests`), so callers reach
/// for this rather than building the path themselves.
pub fn directory_form(repo: &Repo, name: &str) -> PathBuf {
    repo.prompts_dir()
        .join(name)
        .join(crate::assets::PROMPT_FILE)
}

/// Every prompt file this project has, with where it came from.
pub fn entries(repo: &Repo) -> Result<Vec<Entry>> {
    let dir = repo.prompts_dir();
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();

    let listing = match std::fs::read_dir(&dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };

    for entry in listing {
        let path = entry?.path();

        // A directory is a prompt when it holds the file; anything else in
        // here — an `assets/` of a prompt's own, a stray folder — is not one.
        if path.is_dir() {
            let nested = path.join(crate::assets::PROMPT_FILE);
            if !nested.is_file() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                found.insert(name.to_string(), nested);
            }
            continue;
        }

        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // A half-migrated project has both. The directory is what everything
        // else here writes and reads, so it is the one that counts.
        found.entry(name.to_string()).or_insert(path);
    }

    Ok(found
        .into_iter()
        .map(|(name, path)| Entry { name, path })
        .collect())
}

/// Which `pipeline/step` pairs run each prompt. "Used by nothing" is what
/// `list` says about a prompt that can be deleted, and what `doctor` notes.
pub(crate) fn users(pipelines: &Pipelines) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pipeline in pipelines.pipelines.values() {
        for step in &pipeline.steps {
            if step.kind() != StepKind::Agent {
                continue;
            }
            out.entry(step.prompt_name().to_string())
                .or_default()
                .push(format!("{}/{}", pipeline.name, step.id));
        }
    }
    out
}

/// `spoolway prompt list`.
pub fn list(repo: &Repo, pipelines: &Pipelines, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    let entries = entries(repo)?;
    if entries.is_empty() {
        println!(
            "no prompts in {} — run `spoolway init`",
            repo.prompts_dir().display()
        );
        return Ok(());
    }

    let users = users(pipelines);
    let width = entries
        .iter()
        .map(|e| e.name.len())
        .max()
        .unwrap_or(0)
        .max(7);

    // No column for what spoolway would do to these: it does nothing to them.
    // Whether a project has tailored a prompt or is still running the shipped
    // prose is not something to have an opinion about in a listing.
    println!("{:<width$}  USED BY", "PROMPT");
    for entry in &entries {
        let used = match users.get(&entry.name) {
            Some(steps) => steps.join(", "),
            None => "— nothing runs it".to_string(),
        };
        println!("{:<width$}  {}", entry.name, used);
    }

    // A step whose prompt file is missing is a stopped pipeline, and this is
    // the listing someone reads while wondering why.
    let have: BTreeSet<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    let missing: Vec<&String> = users
        .keys()
        .filter(|name| !have.contains(name.as_str()))
        .collect();
    if !missing.is_empty() {
        println!();
        for name in missing {
            println!(
                "missing: {} is named by {} and does not exist",
                name,
                users[name].join(", ")
            );
        }
    }
    Ok(())
}

/// `spoolway prompt show`.
pub fn show(repo: &Repo, name: &str) -> Result<()> {
    let path = path_for(repo, name);
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("reading prompt {}", path.display()))?;
    print!("{body}");
    Ok(())
}

/// Read every prompt against the steps that run it. The only rule left is
/// [`stale_commands`]; a prompt no step runs yet still gets read, the same as
/// every other rule this lint ever had, since a prompt written before its
/// step is wired up is the normal state halfway through adding one.
pub fn lint(repo: &Repo, pipelines: &Pipelines) -> Result<Vec<Finding>> {
    let mut findings = Vec::new();
    let mut seen = BTreeSet::new();

    for pipeline in pipelines.pipelines.values() {
        for step in &pipeline.steps {
            if step.kind() != StepKind::Agent {
                continue;
            }
            let name = step.prompt_name();
            let path = path_for(repo, name);
            // Absent is `pipeline check`'s to report, and it already does.
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            let at = format!("{}/{}", pipeline.name, step.id);
            for finding in stale_commands(name, &body, &at) {
                // One prompt on two steps must not say the same thing twice.
                if seen.insert((finding.prompt.clone(), finding.message.clone())) {
                    findings.push(finding);
                }
            }
        }
    }

    let used: BTreeSet<String> = users(pipelines).keys().cloned().collect();
    for entry in entries(repo)? {
        if used.contains(&entry.name) {
            continue;
        }
        let body = std::fs::read_to_string(&entry.path)?;
        for finding in stale_commands(&entry.name, &body, "") {
            if seen.insert((finding.prompt.clone(), finding.message.clone())) {
                findings.push(finding);
            }
        }
    }

    Ok(findings)
}

/// Every `spoolway …` command a prompt names, checked against the CLI this
/// binary actually has.
///
/// This is the check that replaces the update pass. Prompts are never rewritten
/// by an upgrade, which is the point — so the thing that used to be prevented by
/// regenerating half the file has to be *found* instead. A renamed subcommand or
/// a dropped flag in prompt prose is a lane running a subcommand
/// against a binary that no longer has it, twenty minutes in, in a pane nobody is
/// watching.
///
/// Read off clap rather than a list kept beside it: a list is a second place to
/// forget, and the whole failure being caught here is somebody forgetting a
/// second place.
fn stale_commands(name: &str, body: &str, at: &str) -> Vec<Finding> {
    use clap::CommandFactory;

    let cli = crate::cli::Cli::command();
    let mut out = Vec::new();

    for (line, path, flags) in invocations(body) {
        // Walk as far down the tree as the prose actually names, so `spoolway
        // queue show` is checked as a pair and `spoolway dispatch` as one word.
        let mut node = &cli;
        let mut walked: Vec<String> = Vec::new();
        for word in &path {
            match node.find_subcommand(word) {
                Some(next) => {
                    node = next;
                    walked.push(word.clone());
                }
                None if walked.is_empty() => {
                    out.push(Finding {
                        prompt: name.to_string(),
                        at: at.to_string(),
                        message: format!(
                            "names `spoolway {word}`, which is not a command this spoolway has: {}",
                            subcommands(&cli).join(", ")
                        ),
                    });
                    break;
                }
                // A second word that is not a subcommand is usually an argument
                // (`spoolway queue show <task>`), not a mistake. Only complain
                // when the command it hangs off has subcommands of its own and
                // this is not one of them.
                None => {
                    if node.has_subcommands() && !word.starts_with('<') {
                        out.push(Finding {
                            prompt: name.to_string(),
                            at: at.to_string(),
                            message: format!(
                                "names `spoolway {} {word}`, and `{}` has no such subcommand: {}",
                                walked.join(" "),
                                walked.join(" "),
                                subcommands(node).join(", ")
                            ),
                        });
                    }
                    break;
                }
            }
        }

        if walked.is_empty() {
            continue;
        }

        for flag in flags {
            // `--help` belongs to every command and is declared by none: clap
            // generates it while building, and this tree is never built, so
            // `get_arguments()` lists what the derive wrote and nothing more.
            // Naming it here rather than building the tree — the answer is the
            // same for every node, and a prompt telling a lane to read
            // `--help` is the prompt doing the right thing.
            let known = flag == "help"
                || node
                    .get_arguments()
                    .any(|arg| arg.get_long() == Some(flag.as_str()))
                || cli
                    .get_arguments()
                    .any(|arg| arg.get_long() == Some(flag.as_str()));
            if !known {
                out.push(Finding {
                    prompt: name.to_string(),
                    at: at.to_string(),
                    message: format!(
                        "tells the lane to run `spoolway {} --{flag}`, and that command takes no \
                         such flag — the line is `{}`",
                        walked.join(" "),
                        line.trim()
                    ),
                });
            }
        }
    }

    out
}

/// The subcommand names of one node, for an error message that says what the
/// available spellings are rather than only that this one is wrong.
fn subcommands(node: &clap::Command) -> Vec<String> {
    node.get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .map(|sub| sub.get_name().to_string())
        .collect()
}

/// Every `spoolway …` invocation in some prose, as `(line, words, long flags)`.
///
/// Deliberately line-scoped: a prompt writes commands one to a line, in prose
/// or in an indented block, and a flag three paragraphs later belongs to a
/// different command. Anything in angle brackets is a placeholder a person is
/// meant to substitute, and never a subcommand. A closing backtick ends the
/// command too: "`spoolway lane` say what you left running" names `spoolway
/// lane`, and the prose after it is not a subcommand of anything.
fn invocations(body: &str) -> Vec<(String, Vec<String>, Vec<String>)> {
    let mut out = Vec::new();

    for line in body.lines() {
        for segment in line.split('`') {
            invocations_in(line, segment, &mut out);
        }
    }

    out
}

/// One backtick-delimited stretch of `line`, scanned for `spoolway …`.
fn invocations_in(line: &str, segment: &str, out: &mut Vec<(String, Vec<String>, Vec<String>)>) {
    {
        // Quotes, list bullets and code fences are punctuation around the
        // command, not part of it.
        let cleaned: String = segment
            .chars()
            .map(|c| match c {
                '"' | '\'' | '(' | ')' | ',' | ';' => ' ',
                other => other,
            })
            .collect();

        let words: Vec<&str> = cleaned.split_whitespace().collect();

        // Every occurrence, not the first: "run `spoolway review` and then
        // `spoolway report`" is two commands, and checking only the first is a
        // lint that passes the second however wrong it is.
        for (at, _) in words
            .iter()
            .enumerate()
            .filter(|(_, word)| **word == "spoolway")
        {
            // Where the next invocation on this line begins, so one command's
            // flags are never read as the previous one's.
            let end = words
                .iter()
                .skip(at + 1)
                .position(|word| *word == "spoolway")
                .map(|offset| at + 1 + offset)
                .unwrap_or(words.len());
            let rest = &words[at + 1..end];

            let path: Vec<String> = rest
                .iter()
                .take_while(|word| {
                    !word.starts_with('-') && !word.starts_with('<') && !word.starts_with('$')
                })
                .take(2)
                .map(|word| word.to_string())
                .collect();

            let flags: Vec<String> = rest
                .iter()
                .filter_map(|word| word.strip_prefix("--"))
                // `--pass|--fail` is three flags written as one word, and a
                // bare `--` is not a flag at all.
                .flat_map(|word| word.split('|'))
                .map(|word| word.trim_start_matches('-').trim_end_matches('.'))
                .filter(|word| {
                    !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                })
                .map(|word| word.to_string())
                .collect();

            if path.is_empty() {
                continue;
            }
            out.push((line.to_string(), path, flags));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(findings: &[Finding]) -> Vec<&String> {
        findings.iter().map(|f| &f.message).collect()
    }

    /// The check that replaced the file format. A prompt naming a command this
    /// binary does not have used to be survivable because half the file was
    /// regenerated on upgrade; nothing is regenerated now, so this has to be
    /// found by reading instead.
    #[test]
    fn a_command_spoolway_does_not_have_is_a_finding() {
        let findings = stale_commands("stale", "Finish with `spoolway announce --pass`.", "");
        assert_eq!(findings.len(), 1, "{:?}", messages(&findings));
        assert!(
            findings[0].message.contains("announce"),
            "{}",
            findings[0].message
        );
    }

    #[test]
    fn a_subcommand_that_does_not_exist_is_a_finding() {
        let findings = stale_commands("stale", "Read it with `spoolway queue explain <task>`.", "");
        assert_eq!(findings.len(), 1, "{:?}", messages(&findings));
        assert!(
            findings[0].message.contains("no such subcommand"),
            "{}",
            findings[0].message
        );
    }

    #[test]
    fn a_flag_the_command_does_not_take_is_a_finding() {
        let findings = stale_commands("stale", "Run `spoolway report --done`.", "");
        assert_eq!(findings.len(), 1, "{:?}", messages(&findings));
        assert!(
            findings[0].message.contains("--done"),
            "{}",
            findings[0].message
        );
    }

    /// `--help` is never declared by a command — clap adds it while building,
    /// and this check reads an unbuilt tree. Telling a lane to read a
    /// command's `--help` is a prompt doing exactly what the archivist's is
    /// written to do, so it must not come back as a finding against it.
    #[test]
    fn asking_a_lane_to_read_help_is_not_a_finding() {
        for line in [
            "Read the keys off `spoolway config show` and `--help`.",
            "Run `spoolway queue add --help`.",
            "Start with `spoolway --help`.",
        ] {
            let findings = stale_commands("helpful", line, "");
            assert!(findings.is_empty(), "{line}: {:?}", messages(&findings));
        }
    }

    /// The commands the shipped prompts name have to pass their own lint, or
    /// every project starts life with findings it did not cause.
    #[test]
    fn every_shipped_prompt_names_only_real_commands() {
        for prompt in crate::assets::PROMPTS {
            let findings = stale_commands(prompt.name, prompt.body, "");
            assert!(
                findings.is_empty(),
                "{}: {:?}",
                prompt.name,
                messages(&findings)
            );
        }
    }

    /// A placeholder is not a subcommand and an argument is not one either — a
    /// lint that cannot tell them apart is a lint people turn off.
    #[test]
    fn placeholders_and_arguments_are_not_mistaken_for_subcommands() {
        for line in [
            "Run `spoolway queue show <task>`.",
            "Run `spoolway lane <lane>`.",
            "Run `spoolway report --pass -m \"done\"`.",
            "Record it with `spoolway report --fail --handoff \"src/a.rs:1 — no test\"`.",
        ] {
            assert!(
                stale_commands("ok", line, "").is_empty(),
                "false positive on: {line}"
            );
        }
    }

    /// `--pass|--fail|--block` is how these get written in prose, and reading it
    /// as one flag named `pass|--fail|--block` would fail every prompt.
    #[test]
    fn alternatives_written_with_a_pipe_are_read_as_separate_flags() {
        assert!(stale_commands("ok", "`spoolway report --pass|--fail|--block`", "").is_empty());
    }

    /// A repo whose `home` is a scratch directory of its own — `overrides_dir`
    /// reads straight off that field, so no real `$HOME` or git repository is
    /// needed to test the patch layer here, unlike `Pipelines::load` and
    /// `Config::load`.
    fn fixture(name: &str) -> Repo {
        let base = crate::scratch::root(&format!("prompt-override-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        Repo {
            checkout: base.clone(),
            root: base.clone(),
            config: crate::config::Config::default(),
            home: base.join(".home"),
        }
    }

    /// `overrides/prompts/<name>/PROMPT.md` replaces the tracked prompt
    /// whole, rather than being merged into it — a prompt is prose, with no
    /// key in it for a patch to aim at.
    #[test]
    fn an_override_replaces_the_tracked_prompt_whole() {
        let repo = fixture("replaces-whole");
        let tracked = directory_form(&repo, "implementer");
        std::fs::create_dir_all(tracked.parent().unwrap()).unwrap();
        std::fs::write(&tracked, "the tracked prompt").unwrap();

        let overridden = repo
            .overrides_dir()
            .join("prompts")
            .join("implementer")
            .join(crate::assets::PROMPT_FILE);
        std::fs::create_dir_all(overridden.parent().unwrap()).unwrap();
        std::fs::write(&overridden, "the overriding prompt").unwrap();

        let path = path_for(&repo, "implementer");
        assert_eq!(path, overridden);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "the overriding prompt"
        );

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// `path_for_tracked` is the second entry point the plan calls for: it
    /// answers the tracked file even where a patch is sitting right there on
    /// disk, for a caller that must not see the merge.
    #[test]
    fn path_for_tracked_ignores_a_patch_on_disk() {
        let repo = fixture("tracked-ignores-patch");
        let tracked = directory_form(&repo, "implementer");
        std::fs::create_dir_all(tracked.parent().unwrap()).unwrap();
        std::fs::write(&tracked, "the tracked prompt").unwrap();

        let overridden = repo
            .overrides_dir()
            .join("prompts")
            .join("implementer")
            .join(crate::assets::PROMPT_FILE);
        std::fs::create_dir_all(overridden.parent().unwrap()).unwrap();
        std::fs::write(&overridden, "the overriding prompt").unwrap();

        assert_eq!(path_for_tracked(&repo, "implementer"), tracked);
        assert_eq!(path_for(&repo, "implementer"), overridden);

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// With no `overrides/` directory on disk at all, `path_for` answers
    /// exactly what it did before this layer existed.
    #[test]
    fn with_no_overrides_directory_path_for_is_unchanged() {
        let repo = fixture("absent");
        let tracked = directory_form(&repo, "implementer");
        std::fs::create_dir_all(tracked.parent().unwrap()).unwrap();
        std::fs::write(&tracked, "the tracked prompt").unwrap();

        assert_eq!(
            path_for(&repo, "implementer"),
            path_for_tracked(&repo, "implementer")
        );

        std::fs::remove_dir_all(&repo.checkout).ok();
    }
}
