//! Prompts — the role a lane plays, and the contract it is written against.
//!
//! A prompt is a directory under `.spoolway/prompts/` holding a `PROMPT.md`,
//! selected by a step's `prompt:`. Nothing registers one, nothing compiles it
//! in, and — since the file format was removed — nothing parses one either. It
//! is prose the project owns outright: `spoolway sync` never touches a
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
//! And it *reads* what a project wrote, against the step that runs it. Two
//! rules, of two different kinds. [`stale_commands`] is a fact, not an
//! opinion — a `spoolway …` command this release does not have, checked
//! against clap's own command tree. This is what an upgrade used to prevent
//! by regenerating half the file, and nothing is regenerated now, so the
//! drift has to be found by reading instead, which is the trade the format's
//! removal actually made. A surviving finding fails `spoolway pipeline
//! check`. [`restated_report_contract`] is a heuristic on prose, and says so:
//! a prompt naming `spoolway report` or one of its flags usually means
//! section 7's contract got copied into the file it warns about rather than
//! left to the band spoolway injects at launch, but a prompt whose role is
//! writing *about* spoolway has a real reason to name it, and reading prose
//! cannot tell the two apart — so it only ever warns, never fails. Both
//! reach `spoolway pipeline check`, the only command that reaches this lint,
//! on the channel their kind belongs to.
//!
//! What it no longer reads for is a git verb the step was not granted. Nothing
//! grants git verbs any more: git up to the pull request is `spoolway
//! stack`'s, run from a command step rather than read out of a prompt, and
//! policing which command it may run belongs to the harness rather than to a
//! lint over prose.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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
    /// Found only under `local/prompts/`, with no tracked file of the same
    /// name — see [`path_for`]. A reader that lists entries by name must say
    /// so, since a private prompt works everywhere `prompt show` finds one
    /// but is invisible to anyone reading only the tracked checkout.
    pub private: bool,
}

/// One problem with a prompt, found by reading it against the step that runs it.
///
/// Heuristic on purpose, and it says so: this reads prose looking for commands.
/// It is worth having anyway, because the alternative is finding out twenty
/// minutes into a lane, as a refusal in a pane nobody is watching.
pub struct Finding {
    pub prompt: String,
    /// Where the prompt is used, as `pipeline/step`; `"private"` for a
    /// file-level finding on a prompt no tracked file names (see
    /// [`Entry::private`]); or empty for a file-level finding on a tracked
    /// prompt, which holds wherever it runs.
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
    // First, like `pipeline show`: `pipelines` is read out of this checkout,
    // not the project root, so a linked worktree previewing its own pipeline
    // edit needs to see which copy answered before anything else prints.
    if let Some(note) = repo.checkout_note()? {
        note.print(false)?;
    }

    // A project `init --no-examples` left with nothing under `pipelines/`
    // has no step to render this contract against yet, and `subject` would
    // otherwise bail asking for a `--pipeline` there is none of. Say so and
    // stop rather than refuse: the skill route this command exists for
    // reads it right after `init`, before any pipeline has been written.
    if args.task.is_none() && args.pipeline.is_none() && pipelines.names().is_empty() {
        println!(
            "No pipeline exists in this project yet, so there is no step to render a lane \
             contract against."
        );
        println!(
            "Write the first one from `spoolway pipeline contract`, then rerun this with \
             `--pipeline <name>` — or `--task <id>` once a task runs on it."
        );
        return Ok(());
    }

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
        "   Seven states, in spoolway's own words. See `compose::STATES`. Each is one message,"
    );
    println!(
        "   except an opening on a step with `skills:`: one message per skill, then the briefing."
    );
    for state in crate::compose::STATES {
        println!();
        println!("   This is `{state}`:");
        let messages = crate::compose::lane_prompt_for_state(&task, pipeline, step, state);
        for (i, message) in messages.iter().enumerate() {
            if messages.len() > 1 {
                let when = match i {
                    0 => "typed at launch".to_string(),
                    _ => "typed once the lane has settled from the one before".to_string(),
                };
                println!("   message {} of {}, {when}:", i + 1, messages.len());
            }
            for line in message.lines() {
                println!("   | {line}");
            }
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
    // The same forms and the same refusals as `report_contract` composes
    // into the system prompt this lane was launched with — read from there
    // rather than restated, so the two can never drift apart. See
    // `crate::compose::report_contract` for why `blocked` keeps two forms,
    // and why every other step offers `--fail` only when it routes
    // somewhere `--block` does not.
    for line in crate::compose::report_contract(&task, pipeline, step).lines() {
        println!("   {line}");
    }
    println!();
    println!("   Exactly once, then stop. Where each outcome takes this task:");
    // `blocked` itself is the one step whose graph edges lie: it declares no
    // `on_pass`, and `step.destination` reading that as "stays put" would tell
    // the one lane most likely to read this that a pass leaves the task where
    // it is, which is the opposite of true — a pass moves it on from whatever
    // it blocked on, this lane's own work standing in for that step's. A
    // `--pause` — or a `--fail`/`--block`, read the same way — never claims
    // that work is done, so it parks on `paused` instead, and a person's
    // `spoolway resume` hands the task back to the step it blocked on rather
    // than past it: the one destination this lane's own pass would have
    // reached, and the only one a `--pause` never gets to on its own.
    if step.id == crate::pipeline::BLOCKED {
        println!("     --pass  → on from whatever this task blocked on, your work standing in");
        println!(
            "     --pause → `paused`, waiting for a person — `spoolway resume` then hands the task back to whatever it blocked on, not past it"
        );
    } else {
        // `--fail` is left out of this table exactly when `report_contract`
        // already refused it above — a destination line for a form the
        // lane may never use would read as a second, contradicting offer.
        let fail_offered = step.destination(crate::pipeline::Outcome::Fail)
            != step.destination(crate::pipeline::Outcome::Block);
        for (outcome, label) in [
            (crate::pipeline::Outcome::Pass, "--pass "),
            (crate::pipeline::Outcome::Fail, "--fail "),
            (crate::pipeline::Outcome::Block, "--block"),
        ] {
            if outcome == crate::pipeline::Outcome::Fail && !fail_offered {
                continue;
            }
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
    println!("   Bullets, not paragraphs. Facts, not narration. No jargon.");
    println!("   No procedure any capable model follows unasked.");
    println!("   Named by role: `reviewer`, never `<pipeline>-reviewer`.");
    println!("   Knowledge two prompts share: a skill, through the step's skills:.");
    println!();
    println!("   NEVER IN A PROMPT. spoolway writes each of these itself, at launch:");
    println!("     the report commands and their flags              section 6");
    println!("     `## Status Log`, `## Handoff`, `## Blocker`, and what goes under them");
    println!("     one step, nobody reads you, nothing wakes you, commit as you go");
    println!("     SPOOLWAY_* and where the task file is            sections 3 and 4");
    println!("   A second copy goes stale, and a lane holding two contracts follows neither.");
    println!("   `spoolway pipeline check` warns on a prompt naming a report command or flag.");
    println!();
    println!("   NO EXAMPLES. No sample task, no worked case, no invented file, no \"for");
    println!("   example\" — the task in front of the lane is the example. A path in this");
    println!("   repo, a command that runs, this project's own names in a table: those are");
    println!("   facts, and they stay.");
    println!();
    println!("   NO SENTENCE DEFENDING A RULE, and no jargon. State it once and stop.");

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

<the job and when it is done, 1\u{2013}3 lines>

## Domain knowledge
- <a fact the model cannot know: a path, a convention, a trap>

## Never
- <a guardrail; none is fine>";

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
        None => bail!(
            "no `--task` and no `--pipeline` — spoolway has no project default to fall back \
             to, so one of the two has to name where this contract comes from. Pipelines \
             defined here: {}.",
            pipelines.names().join(", ")
        ),
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
    let sample = sample_task(repo, pipeline, &step.id)?;
    Ok((pipeline, step, sample, true))
}

/// Refuse a step no agent runs. A command step has no lane, no
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
fn sample_task(repo: &Repo, pipeline: &Pipeline, stage: &str) -> Result<Task> {
    let skeleton = crate::task_template::resolve_for(repo, pipeline);
    let raw = format!("---\nid: example\nstage: {stage}\n---\n{skeleton}");
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
/// `local/prompts/<name>/PROMPT.md` — see [`crate::local`] — is tried only
/// once the tracked file is confirmed absent, so a private prompt can never
/// shadow a tracked one of the same name; [`crate::pipeline::Pipelines::load_impl`]
/// refuses that clash outright, before a pipeline naming either ever reaches
/// this function. Read only in repo mode: in home mode the whole setup is
/// already private, and there is no `local/` beside it to read.
///
/// When none of the three exists this names the directory shape, so an error
/// points at where a prompt belongs rather than where it used to.
pub fn path_for(repo: &Repo, name: &str) -> PathBuf {
    let tracked = path_for_tracked(repo, name);
    let overridden = crate::overrides::prompt_override(&repo.overrides_dir(), name);

    // Only a plain name is ever resolved privately — the same shape
    // `local_names_in` enumerates, direct children only. A name holding `/`
    // would otherwise join straight through into a nested directory the
    // private layer never lists, letting a tracked pipeline reach a prompt
    // `merge_private`'s own clash check never saw and a private-only name
    // never meant to publish — see `crate::local::is_plain_name`.
    //
    // Checked before the override below, since a private prompt is the
    // reason an override here does not apply — an override patches a
    // *tracked* file, and a private prompt is not one — so it must be told
    // apart from a name the checkout genuinely has nothing by, which is
    // `Ignored::missing_prompt`'s own case.
    if !tracked.is_file()
        && crate::local::is_repo_mode(&repo.checkout)
        && crate::local::is_plain_name(name)
    {
        let private = crate::local::prompts_dir(&repo.local_dir())
            .join(name)
            .join(crate::assets::PROMPT_FILE);
        if private.is_file() {
            if overridden.is_some() {
                crate::overrides::print_ignored_notices(&[
                    crate::overrides::Ignored::private_prompt(name),
                ]);
            }
            return private;
        }
    }

    if let Some(overridden) = overridden {
        // A whole-file override for a prompt the checkout no longer has —
        // renamed or removed — is stale: it replaces nothing, so it is left
        // out, exactly as if it had never been written.
        if tracked.is_file() {
            return overridden;
        }
        crate::overrides::print_ignored_notices(&[crate::overrides::Ignored::missing_prompt(name)]);
    }
    tracked
}

/// What a caller says when a step's prompt file does not exist — for
/// `dispatch::prepare_boot` and `commands::pipeline::step_problems`, the two
/// places that already report this (a dispatch failure and a `pipeline
/// check` finding), so the two read the same way.
///
/// A private pipeline may read its prompt from either layer — the tracked
/// `.spoolway/prompts/<name>/` or the private `local/prompts/<name>/` — so a
/// message naming only the one path [`path_for`] happened to fall back to
/// (always the tracked directory form) sends a person hunting in the wrong
/// place, or only half the right one. For a private pipeline, in repo mode,
/// this names both, and says plainly that the private layer only ever reads
/// the directory form: a `local/prompts/<name>.md` written flat — the legacy
/// shape the *tracked* side still accepts, see [`local_names_in`] — is never
/// read there. Every other pipeline keeps the plain, one-path message: there
/// is only ever one place its prompt could be.
pub(crate) fn missing_prompt_message(
    repo: &Repo,
    private_pipeline: bool,
    prompt_name: &str,
    label: &str,
    tail: &str,
) -> String {
    if private_pipeline && crate::local::is_repo_mode(&repo.checkout) {
        let tracked_dir = directory_form(repo, prompt_name);
        let private_dir = crate::local::prompts_dir(&repo.local_dir()).join(prompt_name);
        format!(
            "{label} needs prompt `{prompt_name}` — found at neither {} nor {} (the private \
             layer only reads the directory form; a flat {}.md is not read) — {tail}",
            tracked_dir.display(),
            private_dir.display(),
            private_dir.display(),
        )
    } else {
        format!(
            "{label} needs prompt {} — {tail}",
            path_for(repo, prompt_name).display()
        )
    }
}

/// [`path_for`], with no patch layer applied — for a caller that must see
/// only the tracked file: `commands::prompt_override` reads the source to
/// fork, and `commands::override_promote` the destination to write.
pub fn path_for_tracked(repo: &Repo, name: &str) -> PathBuf {
    tracked_path_in(&repo.checkout, name)
}

/// [`path_for_tracked`], from a bare checkout path rather than a [`Repo`] —
/// for [`crate::pipeline::Pipelines::load_impl`]/`merge_private`, which has
/// not built one yet when it names the tracked file a private prompt clashes
/// with: that message has to say the flat `<name>.md` when that is the shape
/// actually on disk, not always the directory form nothing there uses.
pub(crate) fn tracked_path_in(checkout: &Path, name: &str) -> PathBuf {
    let nested = directory_form_in(checkout, name);
    if nested.is_file() {
        return nested;
    }
    let flat = tracked_prompts_dir_in(checkout).join(format!("{name}.md"));
    if flat.is_file() {
        return flat;
    }
    nested
}

/// The private prompt [`path_for_tracked`] cannot see, for a caller that
/// must tell a genuinely missing name apart from one that merely has no
/// tracked file: `commands::prompt_override` reads it as the source to fork
/// when no tracked file exists, and `collect_override_rows` uses it to
/// report a whole-file override as waiting on a private prompt rather than
/// naming one the checkout does not have.
/// Directory shape only, the same as [`path_for`]'s own private fallback,
/// and `None` outside repo mode or for a name holding `/`.
pub(crate) fn private_only_path(repo: &Repo, name: &str) -> Option<PathBuf> {
    if !crate::local::is_repo_mode(&repo.checkout) || !crate::local::is_plain_name(name) {
        return None;
    }
    let private = crate::local::prompts_dir(&repo.local_dir())
        .join(name)
        .join(crate::assets::PROMPT_FILE);
    private.is_file().then_some(private)
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

/// [`directory_form`], from a bare checkout path rather than a [`Repo`] — for
/// [`crate::pipeline::Pipelines::load_impl`], which has not built one yet
/// when it checks a private prompt against the tracked directory it would
/// clash with.
pub(crate) fn directory_form_in(checkout: &Path, name: &str) -> PathBuf {
    tracked_prompts_dir_in(checkout)
        .join(name)
        .join(crate::assets::PROMPT_FILE)
}

/// `.spoolway/prompts` (or a home-mode workspace's own `config/prompts`) for
/// `checkout` — [`Repo::prompts_dir`]'s own join, resolved from a bare path
/// for the one caller that has no [`Repo`] to ask.
fn tracked_prompts_dir_in(checkout: &Path) -> PathBuf {
    let setup = crate::config::setup_dir_in(checkout);
    crate::config::under_setup(&setup, crate::config::PROMPTS_DIR)
}

/// Every tracked prompt name for `checkout`, with neither the override layer
/// nor the private layer consulted — the root-path twin of [`entries`], for
/// [`crate::pipeline::Pipelines::load_impl`].
pub(crate) fn tracked_names_in(checkout: &Path) -> Result<BTreeSet<String>> {
    Ok(scan(&tracked_prompts_dir_in(checkout))?
        .into_keys()
        .collect())
}

/// Every private prompt name found directly under `dir` — directory shape
/// only, `<name>/PROMPT.md`. Unlike the tracked side ([`tracked_names_in`]),
/// never the legacy flat `<name>.md`: that shape exists only so an
/// already-upgraded tracked project keeps reading what it wrote before the
/// directory shape, and a private prompt has never been anything else, so
/// [`path_for`]'s own private fallback never looks for one — this has to
/// agree with it, or a tracked pipeline could be refused over a flat
/// `local/prompts/<name>.md` that a private pipeline naming the same prompt
/// would then fail to find.
pub(crate) fn local_names_in(dir: &Path) -> Result<BTreeSet<String>> {
    let mut found = BTreeSet::new();
    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(found),
        Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
    };
    for entry in listing {
        let path = entry?.path();
        if path.is_dir()
            && path.join(crate::assets::PROMPT_FILE).is_file()
            && let Some(name) = path.file_name().and_then(|s| s.to_str())
        {
            found.insert(name.to_string());
        }
    }
    Ok(found)
}

/// Every prompt entry directly under `dir`, whichever of the two shapes each
/// one takes, by name — the one directory scan [`entries`] and
/// [`tracked_names_in`] both build on, so a shape either one recognizes can
/// never silently drop out of the other.
fn scan(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();

    let listing = match std::fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(found),
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

    Ok(found)
}

/// Every prompt file this project has, with where it came from — the
/// tracked directory, plus, in repo mode, any private prompt
/// (`local/prompts/<name>/PROMPT.md`) with no tracked file of the same
/// name. A tracked file always wins the name, the same precedence
/// [`path_for`] gives it, so a private prompt never hides one a pipeline
/// actually runs.
pub fn entries(repo: &Repo) -> Result<Vec<Entry>> {
    let tracked = scan(&repo.prompts_dir())?;
    let mut found: Vec<Entry> = tracked
        .iter()
        .map(|(name, path)| Entry {
            name: name.clone(),
            path: path.clone(),
            private: false,
        })
        .collect();

    if crate::local::is_repo_mode(&repo.checkout) {
        let private_dir = crate::local::prompts_dir(&repo.local_dir());
        for name in local_names_in(&private_dir)? {
            if tracked.contains_key(&name) {
                continue;
            }
            found.push(Entry {
                path: private_dir.join(&name).join(crate::assets::PROMPT_FILE),
                name,
                private: true,
            });
        }
    }

    Ok(found)
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
        // A private prompt lives only on this machine's local/, so it is
        // worth saying so right beside it — nothing else in the row marks
        // where a prompt's file actually is.
        let used = if entry.private {
            format!("private, {used}")
        } else {
            used
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

/// Read every prompt against the steps that run it, for [`stale_commands`] —
/// the rule that fails `spoolway pipeline check`. A prompt no step runs yet
/// still gets read, the same as every other rule this lint ever had, since a
/// prompt written before its step is wired up is the normal state halfway
/// through adding one.
pub fn lint(repo: &Repo, pipelines: &Pipelines) -> Result<Vec<Finding>> {
    read_prompts(repo, pipelines, stale_commands)
}

/// Read every prompt against the steps that run it, for
/// [`restated_report_contract`] — the rule that only ever warns. Kept as its
/// own entry point rather than folded into [`lint`], because the two rules
/// answer to different channels of `pipeline_check` and must never be mixed
/// into one list a caller then has to re-sort by kind.
pub fn lint_warnings(repo: &Repo, pipelines: &Pipelines) -> Result<Vec<Finding>> {
    read_prompts(repo, pipelines, restated_report_contract)
}

/// The traversal both lint entry points share: every prompt a step runs,
/// plus every prompt no step runs, each read once against `rule`. Only the
/// rule differs between [`lint`] and [`lint_warnings`], so this is the one
/// place that walks pipelines, steps and the prompt directory.
fn read_prompts(
    repo: &Repo,
    pipelines: &Pipelines,
    rule: fn(&str, &str, &str) -> Vec<Finding>,
) -> Result<Vec<Finding>> {
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
            for finding in rule(name, &body, &at) {
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
        let at = if entry.private { "private" } else { "" };
        for finding in rule(&entry.name, &body, at) {
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
/// This is the check that replaces the sync pass. Prompts are never rewritten
/// by a sync, which is the point — so the thing that used to be prevented by
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

/// The long flags `spoolway report` actually takes, read off clap's own
/// command tree at run time rather than a list kept beside it — the same
/// reason [`stale_commands`] reads the whole tree: a rename here must not
/// need a second edit to stay caught.
fn report_flags() -> Vec<String> {
    use clap::CommandFactory;
    let cli = crate::cli::Cli::command();
    let report = cli
        .find_subcommand("report")
        .expect("`report` is a real spoolway command");
    report
        .get_arguments()
        .filter_map(|arg| arg.get_long())
        .map(str::to_string)
        .collect()
}

/// A prompt naming `spoolway report`, or one of its flags, on its own —
/// section 7's ban on restating what spoolway injects at every launch. Never
/// a `pipeline_check` problem: a prompt whose role is writing *about*
/// spoolway (this project's own `spoolway-config` prompt, say) has a real
/// reason to name the command, and reading prose cannot tell that reason
/// from a copied-in report contract, so this only ever reaches the warnings
/// channel — see [`crate::commands::pipeline::pipeline_check`].
///
/// Two separate scans, because a restated flag is often quoted alone,
/// nowhere near the word `spoolway`: `invocations` only pairs a flag with
/// the command it followed, which would miss "Leave what the fixer needs
/// with `--handoff`."
fn restated_report_contract(name: &str, body: &str, at: &str) -> Vec<Finding> {
    let flags = report_flags();
    let mut out = Vec::new();

    for (line, path, _flags) in invocations(body) {
        if path.first().map(String::as_str) == Some("report") {
            out.push(Finding {
                prompt: name.to_string(),
                at: at.to_string(),
                // The prompt's own line, not the sample line from the
                // mockup — the same choice `stale_commands` and the flag
                // branch below make, and the reason `read_prompts`'
                // dedup-by-message does not collapse two occurrences of
                // `spoolway report` on different lines into one finding.
                message: format!(
                    "names `spoolway report`, which spoolway injects under every prompt at \
                     launch — the line is `{}`",
                    line.trim()
                ),
            });
        }
    }

    for line in body.lines() {
        for segment in line.split('`') {
            for flag in flags_named(segment) {
                if flags.contains(&flag) {
                    out.push(Finding {
                        prompt: name.to_string(),
                        at: at.to_string(),
                        message: format!(
                            "names `--{flag}`, a flag of `spoolway report`, which spoolway \
                             injects at launch — the line is `{}`",
                            line.trim()
                        ),
                    });
                }
            }
        }
    }

    out
}

/// Every `--flag` token in one stretch of text, with no requirement that a
/// `spoolway` word precede it — the same cleaning [`invocations_in`] applies
/// before splitting on whitespace, kept separate because that function only
/// ever looks at flags following a command it has already walked.
fn flags_named(segment: &str) -> Vec<String> {
    let cleaned: String = segment
        .chars()
        .map(|c| match c {
            '"' | '\'' | '(' | ')' | ',' | ';' => ' ',
            other => other,
        })
        .collect();
    cleaned
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("--"))
        .flat_map(|word| word.split('|'))
        .map(|word| word.trim_start_matches('-').trim_end_matches('.'))
        .filter(|word| {
            !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
        .map(str::to_string)
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

    /// There is no project default to fall back to any more, so bare
    /// `spoolway prompt contract` — no `--task`, no `--pipeline` — is
    /// refused rather than silently opening on whichever pipeline used to
    /// be the default.
    #[test]
    fn contract_with_neither_task_nor_pipeline_is_refused() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("prompt-contract-no-subject");
        let pipelines = Pipelines::builtin();
        let args = crate::cli::PromptContractArgs {
            step: None,
            pipeline: None,
            task: None,
        };
        let err = contract(&repo, &pipelines, &args).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains("no `--task` and no `--pipeline`"),
            "{message}"
        );
        assert!(message.contains("bugfix"), "{message}");
        assert!(message.contains("default"), "{message}");
    }

    /// A private pipeline's own missing prompt is named by both layers —
    /// the tracked directory and the private one — with an explicit note
    /// that the private layer never reads the flat legacy shape, so a
    /// person does not write `local/prompts/<name>.md` and wonder why it is
    /// still not found.
    #[test]
    fn a_private_pipelines_missing_prompt_names_both_layers() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("missing-prompt-private");
        let message =
            missing_prompt_message(&repo, true, "ghost", "step `a`", "run `spoolway init`");
        let tracked = directory_form(&repo, "ghost");
        let private = crate::local::prompts_dir(&repo.local_dir()).join("ghost");
        assert!(
            message.contains(&tracked.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&private.display().to_string()),
            "{message}"
        );
        assert!(message.contains("flat"), "{message}");
    }

    /// A tracked pipeline's missing prompt keeps the plain, one-path
    /// message — there is only ever one place its prompt could be, so a
    /// second path would only confuse.
    #[test]
    fn a_tracked_pipelines_missing_prompt_names_one_path() {
        let (repo, _root_guard) = crate::commands::testutil::fixture("missing-prompt-tracked");
        let message =
            missing_prompt_message(&repo, false, "ghost", "step `a`", "run `spoolway init`");
        let private = crate::local::prompts_dir(&repo.local_dir()).join("ghost");
        assert!(
            !message.contains(&private.display().to_string()),
            "{message}"
        );
    }

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

    /// The `##` headings of a prompt, in order.
    fn headings(body: &str) -> Vec<&str> {
        body.lines()
            .filter_map(|line| line.strip_prefix("## "))
            .collect()
    }

    /// The shipped prompts are the first thing a new project runs, so they
    /// follow the shape section 7 prints: a `# <role>` title, one to three
    /// lines of job, then `## Domain knowledge` and `## Never`, each holding
    /// only bullets, in at most 25 lines.
    #[test]
    fn every_shipped_prompt_follows_the_shape_section_seven_prints() {
        for prompt in crate::assets::PROMPTS {
            let name = prompt.name;
            let lines: Vec<&str> = prompt.body.lines().collect();
            assert!(lines.len() <= 25, "{name}: {} lines", lines.len());
            assert!(
                lines[0].starts_with("# ") && lines[0].len() > 2,
                "{name}: opens with {:?}, not `# <role>`",
                lines[0]
            );
            assert_eq!(
                headings(prompt.body),
                ["Domain knowledge", "Never"],
                "{name}"
            );
            let first_section = lines
                .iter()
                .position(|line| line.starts_with("## "))
                .unwrap();
            let job = lines[1..first_section]
                .iter()
                .filter(|line| !line.trim().is_empty())
                .count();
            assert!((1..=3).contains(&job), "{name}: {job} job lines");
            for line in &lines[first_section..] {
                assert!(
                    line.trim().is_empty() || line.starts_with("## ") || line.starts_with("- "),
                    "{name}: {line:?} is neither a heading nor a bullet"
                );
            }
        }
    }

    /// The skeleton section 7 prints and the shipped prompts written to it
    /// name the same sections, so neither can move without the other.
    #[test]
    fn the_printed_skeleton_names_the_shipped_prompts_sections() {
        assert!(PROMPT_SKELETON.starts_with("# <role>\n"));
        assert_eq!(headings(PROMPT_SKELETON), ["Domain knowledge", "Never"]);
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

    /// A prompt naming `spoolway report` outright — section 7's ban on
    /// restating the band spoolway injects at launch. The message quotes
    /// this prompt's own line, not a sample from the contract's mockup —
    /// `-m` is a short flag, so this line alone trips only this one rule,
    /// and pins what the finding actually says.
    #[test]
    fn naming_spoolway_report_is_a_finding() {
        let findings = restated_report_contract(
            "reviewer",
            "Finish with `spoolway report -m \"<verdict>\"`.",
            "rev/review",
        );
        assert_eq!(findings.len(), 1, "{:?}", messages(&findings));
        assert!(
            findings[0]
                .message
                .contains("the line is `Finish with `spoolway report -m \"<verdict>\"`.`"),
            "{}",
            findings[0].message
        );
        assert_eq!(findings[0].at, "rev/review");
    }

    /// Two occurrences on two different lines must be two findings, not
    /// one collapsed by `read_prompts`' dedup-by-message — which is exactly
    /// what a message built from a constant, rather than the prompt's own
    /// line, used to do.
    #[test]
    fn two_occurrences_on_different_lines_are_two_findings() {
        let (repo, _root_guard) = fixture("restated-report-two-lines");
        let tracked = directory_form(&repo, "implementer");
        std::fs::create_dir_all(tracked.parent().unwrap()).unwrap();
        std::fs::write(
            &tracked,
            "# implementer\n\n\
             Finish with `spoolway report -m \"<verdict>\"`.\n\
             On failure, also run `spoolway report -m \"<why>\"`.\n",
        )
        .unwrap();

        let pipeline = crate::pipeline::Pipeline::parse(
            "solo",
            "steps:\n  - id: a\n    agent: claude\n    prompt: implementer\n    \
             model: m\n    on_pass: z\n  - id: z\n    run: x\n    on_pass: done\n",
        )
        .unwrap();
        let pipelines = crate::pipeline::Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        let findings = lint_warnings(&repo, &pipelines).unwrap();
        assert_eq!(findings.len(), 2, "{:?}", messages(&findings));

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// A restated flag is often quoted on its own, nowhere near the word
    /// `spoolway` — `--handoff` here names a flag `spoolway report` takes.
    #[test]
    fn naming_a_report_flag_alone_is_a_finding() {
        let findings = restated_report_contract(
            "reviewer",
            "Leave what the fixer needs with `--handoff`.",
            "rev/review",
        );
        assert_eq!(findings.len(), 1, "{:?}", messages(&findings));
        assert!(
            findings[0].message.contains("--handoff"),
            "{}",
            findings[0].message
        );
    }

    /// The non-goals' own English forms must stay quiet: "report" and
    /// "handoff" show up in ordinary prose that names no command and no
    /// flag, and a rule that cannot tell the difference is one a project
    /// turns off.
    #[test]
    fn ordinary_english_about_reporting_is_not_a_finding() {
        for line in [
            "Something genuinely wrong outside your scope is a finding to report.",
            "A test that is wrong goes in your report, not in your diff.",
            "Record the commands in the handoff.",
        ] {
            let findings = restated_report_contract("ok", line, "");
            assert!(
                findings.is_empty(),
                "false positive on: {line}: {:?}",
                messages(&findings)
            );
        }
    }

    /// A flag some other command takes, not `spoolway report`'s, must not
    /// fire — this rule is about the report contract specifically, not
    /// every `--flag` in a prompt.
    #[test]
    fn a_flag_of_a_different_command_is_not_a_finding() {
        assert!(restated_report_contract("ok", "Run `spoolway queue add --skip`.", "").is_empty());
    }

    /// A repo whose `home` is a scratch directory of its own — `overrides_dir`
    /// reads straight off that field, so no real `$HOME` or git repository is
    /// needed to test the patch layer here, unlike `Pipelines::load` and
    /// `Config::load`.
    fn fixture(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let base = crate::scratch::root(&format!("prompt-override-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        (
            Repo {
                borrowed: false,
                checkout: base.to_path_buf(),
                root: base.to_path_buf(),
                config: crate::config::Config::default(),
                home: base.join(".home"),
            },
            base,
        )
    }

    /// `overrides/prompts/<name>/PROMPT.md` replaces the tracked prompt
    /// whole, rather than being merged into it — a prompt is prose, with no
    /// key in it for a patch to aim at.
    #[test]
    fn an_override_replaces_the_tracked_prompt_whole() {
        let (repo, _root_guard) = fixture("replaces-whole");
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

    /// A whole-file override for a prompt the checkout no longer has is
    /// stale: `path_for` never returns it, answering exactly what it would
    /// with no override at all — the nested tracked shape's own path, unused
    /// though that file is, per [`path_for_tracked`]'s own doc.
    #[test]
    fn an_override_for_a_prompt_the_checkout_no_longer_has_is_not_used() {
        let (repo, _root_guard) = fixture("stale-prompt");

        let overridden = repo
            .overrides_dir()
            .join("prompts")
            .join("retired")
            .join(crate::assets::PROMPT_FILE);
        std::fs::create_dir_all(overridden.parent().unwrap()).unwrap();
        std::fs::write(&overridden, "an override for a prompt nothing tracks").unwrap();

        let path = path_for(&repo, "retired");
        assert_eq!(
            path,
            path_for_tracked(&repo, "retired"),
            "the stale override must never be returned"
        );
        assert_ne!(path, overridden);

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// `path_for_tracked` is the second entry point the plan calls for: it
    /// answers the tracked file even where a patch is sitting right there on
    /// disk, for a caller that must not see the merge.
    #[test]
    fn path_for_tracked_ignores_a_patch_on_disk() {
        let (repo, _root_guard) = fixture("tracked-ignores-patch");
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
        let (repo, _root_guard) = fixture("absent");
        let tracked = directory_form(&repo, "implementer");
        std::fs::create_dir_all(tracked.parent().unwrap()).unwrap();
        std::fs::write(&tracked, "the tracked prompt").unwrap();

        assert_eq!(
            path_for(&repo, "implementer"),
            path_for_tracked(&repo, "implementer")
        );

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// `local/prompts/<name>/PROMPT.md` — the private layer, see
    /// `crate::local` — answers when the tracked file is absent, in repo
    /// mode.
    #[test]
    fn a_private_prompt_is_used_when_the_tracked_file_is_absent() {
        let (repo, _root_guard) = fixture("private-prompt-used");
        let private = crate::local::prompts_dir(&repo.local_dir())
            .join("implementer")
            .join(crate::assets::PROMPT_FILE);
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "the private prompt").unwrap();

        assert_eq!(path_for(&repo, "implementer"), private);

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// A nested name like `nest/inner` is never resolved through the
    /// private layer, even when a file sits exactly where the straight join
    /// would land — `local_names_in` only ever lists direct children, so a
    /// tracked pipeline naming `nest/inner` must see it as missing, the same
    /// as `path_for_tracked` alone would answer, rather than reaching a
    /// private file its own loader does not know exists.
    #[test]
    fn a_private_prompt_name_holding_a_slash_is_never_resolved() {
        let (repo, _root_guard) = fixture("nested-private-name-refused");
        let private = crate::local::prompts_dir(&repo.local_dir())
            .join("nest")
            .join("inner")
            .join(crate::assets::PROMPT_FILE);
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "a nested private prompt").unwrap();

        assert_eq!(
            path_for(&repo, "nest/inner"),
            path_for_tracked(&repo, "nest/inner"),
            "a nested name must never resolve through the private layer"
        );

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// `entries` — the list `prompt list`, doctor's unused-prompt note and
    /// the unused-prompt lint all read — scans only the tracked directory,
    /// so a private-only prompt never appears in any of them, and is
    /// reported missing even though `prompt show` finds it fine.
    #[test]
    fn entries_include_a_private_prompt_the_tracked_folder_does_not_have() {
        let (repo, _root_guard) = fixture("entries-see-private");
        let private = crate::local::prompts_dir(&repo.local_dir())
            .join("impl2")
            .join(crate::assets::PROMPT_FILE);
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "the private prompt").unwrap();

        let found = entries(&repo).unwrap();
        let names: Vec<&str> = found.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"impl2"),
            "a private-only prompt must still be listed: {names:?}"
        );

        std::fs::remove_dir_all(&repo.checkout).ok();
    }

    /// A tracked prompt is never shadowed by a private one of the same
    /// name — nothing private ever replaces a tracked file.
    #[test]
    fn a_tracked_prompt_wins_over_a_private_one_of_the_same_name() {
        let (repo, _root_guard) = fixture("tracked-wins-over-private");
        let tracked = directory_form(&repo, "implementer");
        std::fs::create_dir_all(tracked.parent().unwrap()).unwrap();
        std::fs::write(&tracked, "the tracked prompt").unwrap();

        let private = crate::local::prompts_dir(&repo.local_dir())
            .join("implementer")
            .join(crate::assets::PROMPT_FILE);
        std::fs::create_dir_all(private.parent().unwrap()).unwrap();
        std::fs::write(&private, "the private prompt").unwrap();

        assert_eq!(path_for(&repo, "implementer"), tracked);

        std::fs::remove_dir_all(&repo.checkout).ok();
    }
}
