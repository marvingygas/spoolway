//! Everything read or written against the pipeline graph itself, as one
//! command family in one file:
//! `pipeline show`, `check`, `contract` and `list` read it; `override`
//! forks one step's key into the patch layer; `copy` and `promote`, with
//! `prompt copy` beside them, are the private layer's own write commands —
//! see [`crate::local`] and the plan's `d-private-pipelines` decision.

use std::path::PathBuf;

use super::*;

pub fn pipeline_show(repo: &Repo, pipelines: &Pipelines, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    for pipeline in pipelines.pipelines.values() {
        show_one(pipeline)?;
    }
    Ok(())
}

/// Every pipeline's name, one per line, with its `description:` indented
/// underneath — the fact a reader is choosing between pipelines on, not
/// just their names.
///
/// For `/spoolway-plan`'s own cutting step, which needs a name to write onto
/// a task's card as `pipeline:` and nothing else — not a step, not an agent,
/// not a prompt. `pipeline show` already prints all of that; this is
/// written to be chosen from instead of read.
pub fn pipeline_list(repo: &Repo, pipelines: &Pipelines, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    if json {
        // Pretty, like every other `--json` command's own payload
        // (`doctor.rs`, `commands/queue.rs`, `commands/agent.rs`,
        // `commands/lanes.rs`, and `pipeline contract` in this file) —
        // compact would be the one command whose `--json` a person cannot
        // read straight off the screen.
        println!("{}", serde_json::to_string_pretty(&build_list(pipelines))?);
        return Ok(());
    }
    for pipeline in pipelines.pipelines.values() {
        println!("{}{}", pipeline.name, private_marker(pipeline));
        if let Some(description) = &pipeline.description {
            println!("    {description}");
        }
        println!();
    }
    Ok(())
}

/// `  private · <file>` for a pipeline loaded from `local/pipelines/`, empty
/// for a tracked one — the one piece of text `pipeline list` and
/// `pipeline show` both print after a private pipeline's name, factored out
/// so it is one sentence to get right and one place a test can check it
/// against, rather than two copies that could read differently. Never a
/// stand-in for a tracked pipeline of the same name — names cannot clash,
/// see `crate::pipeline::merge_private` — only a mark so nobody mistakes
/// this pipeline for the tracked one it may have started life as a copy of.
fn private_marker(pipeline: &Pipeline) -> String {
    match &pipeline.private_file {
        Some(file) => format!("  private · {}", file.display()),
        None => String::new(),
    }
}

/// One pipeline, as `--json pipeline list` names it: exactly the facts a
/// script needs to choose between pipelines, and nothing about its steps —
/// `pipeline contract` already carries those.
#[derive(Debug, serde::Serialize)]
struct PipelineListEntry {
    name: String,
    description: Option<String>,
    /// `"tracked"` for a pipeline from `.spoolway/pipelines/`, `"private"`
    /// for one from `local/pipelines/` — see [`crate::local`].
    source: &'static str,
    /// The private file this pipeline was loaded from, `None` for a tracked
    /// one.
    file: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct PipelineListJson {
    pipelines: Vec<PipelineListEntry>,
}

fn build_list(pipelines: &Pipelines) -> PipelineListJson {
    PipelineListJson {
        pipelines: pipelines
            .pipelines
            .values()
            .map(|pipeline| PipelineListEntry {
                name: pipeline.name.clone(),
                description: pipeline.description.clone(),
                source: if pipeline.private_file.is_some() {
                    "private"
                } else {
                    "tracked"
                },
                file: pipeline
                    .private_file
                    .as_ref()
                    .map(|path| path.display().to_string()),
            })
            .collect(),
    }
}

/// Keys a pipeline file may carry at the top level, above `steps:`.
const PIPELINE_KEYS: &[&str] = &["version", "description", "task_template", "steps"];

/// Keys a step may carry — every one `Step` actually reads a value from.
/// `loop` stands for the struct's own `r#loop`, a raw identifier only
/// because `loop` is a Rust keyword.
const STEP_KEYS: &[&str] = &[
    "id",
    "description",
    "agent",
    "prompt",
    "model",
    "effort",
    "skills",
    "session",
    "slot",
    "gate",
    "on_pass",
    "on_fail",
    "loop",
    "run",
    "timeout",
    "background",
    "headless",
    "last",
    "first",
    "serial",
    "end",
];

/// Keys a file may still name and be refused by name for: two retired
/// spellings of `loop:`, the retired `cleanup:` and the retired
/// `on_loop_max:`, kept on [`Step`] only so a file still naming them gets a
/// message pointing at the replacement — or, for `cleanup:` and
/// `on_loop_max:`, saying why there is none — rather than serde's own
/// "unknown field", and three struct fields that answer a fact about where a
/// `Pipeline` came from rather than something a file could ever set —
/// [`Pipeline::name`], because the file name is the name;
/// [`Pipeline::blocked_declared`], set only by [`Pipelines::assemble`] once a
/// file is read; and [`Pipeline::private_file`], set only by
/// [`crate::pipeline::Pipelines::load_impl`] when this pipeline came from
/// `local/pipelines/` rather than the tracked directory — see
/// [`crate::local`].
const REFUSED_KEYS: &[&str] = &[
    "name",
    "blocked_declared",
    "private_file",
    "max_new_sessions",
    "max_rounds",
    "cleanup",
    "on_loop_max",
];

/// One sentence per key a pipeline file may actually set — [`PIPELINE_KEYS`]
/// and [`STEP_KEYS`] together — written from the doc comment already on the
/// field in `pipeline.rs`.
const FIELD_SENTENCES: &[(&str, &str)] = &[
    (
        "version",
        "Yours to raise when this pipeline changed enough to compare, in `x.y` \
         form. spoolway only records it — absent takes `1.0`.",
    ),
    (
        "task_template",
        "Which task skeleton a task queued on this pipeline is written from, by \
         name under the task-templates directory — absent takes this pipeline's \
         own name, falling back to `default`.",
    ),
    (
        "steps",
        "The ordered list of steps a task walks. The first one is where a task \
         starts, and the same order sets scheduling priority — a step later in \
         the list outranks one earlier in it.",
    ),
    (
        "id",
        "A short, unique identifier for this step — the literal value written to \
         a task file's `stage:` field, so renaming a step renames the stage.",
    ),
    (
        "description",
        "At the top level, one sentence on what this pipeline is for — read to \
         choose between pipelines. On a step, a human-facing one-liner, shown by \
         `spoolway pipeline show`.",
    ),
    (
        "agent",
        "A profile from `[agents.*]`. Carrying it is what makes this an agent \
         step — there is no `kind:`.",
    ),
    (
        "prompt",
        "Prompt file, without extension, under the prompts directory. \
         Defaults to the step id.",
    ),
    (
        "model",
        "The model this step runs. Required on every agent step — spoolway names \
         no model of its own, so a step whose `model:` is missing or blank is \
         refused by `pipeline check`.",
    ),
    (
        "effort",
        "How hard `model:` thinks, handed straight through to the flag its agent \
         kind carries an effort on — refused on a kind with no such flag, and on \
         the literal `auto`.",
    ),
    (
        "skills",
        "Skills invoked at the top of this step's opening prompt, one `/name` \
         per skill, in declaration order. Comma separated, leading slash \
         optional, names only. Absent means none.",
    ),
    (
        "session",
        "Whether this step's conversation carries over from an earlier step that \
         ran the same prompt, rather than opening fresh. `false`, same as \
         absent.",
    ),
    (
        "slot",
        "Whether running this step consumes one of the agent profile's \
         concurrency slots. `true` unless set otherwise.",
    ),
    (
        "gate",
        "A person approves this step's work before the task goes any further — \
         its pass parks on `paused` until `spoolway resume`, instead of routing \
         on.",
    ),
    (
        "on_pass",
        "Step to move to on a `pass`. Absent means the task stays put.",
    ),
    (
        "on_fail",
        "Step to move to on a `fail`. Absent falls back to the pipeline's \
         `blocked` step.",
    ),
    (
        "loop",
        "Most arrivals this step may take, by any route in — the same number \
         the board draws as `↻`. The next arrival past it parks the task on \
         `blocked` for a person. Absent means unbounded. Three is the ceiling \
         worth reaching for; a flow that needs more is welcome to say so, and \
         nothing refuses it.",
    ),
    (
        "run",
        "The command line a command step runs, through the environment's own \
         shell, in the task's worktree. Refused on any other kind of step.",
    ),
    (
        "timeout",
        "How long a command step may run before it is killed. Absent takes \
         thirty minutes.",
    ),
    (
        "background",
        "Let the task move on while the command keeps running. Refuses \
         `on_fail`, since nothing is left to route on once the task has gone.",
    ),
    (
        "headless",
        "Run the command detached, with no pane. Absent, a command step gets a \
         pane of its own under the herdr backend.",
    ),
    (
        "last",
        "Command steps only. Only the task at the top of a chain runs it; every \
         task below walks past to `on_pass`.",
    ),
    (
        "first",
        "Command steps only, `false` when absent. Only a chain's declared root \
         — a task whose own `depends_on` is empty — runs it; every task that \
         names a dependency walks past to `on_pass`. `depends_on` is read \
         from the task's own file, and archiving what it names does not empty \
         it, so a task naming an archived dependency is still not the root. \
         Every root of a fan runs it, since none of them declares a \
         dependency. Refused together with `last:` on the same step.",
    ),
    (
        "serial",
        "Command steps only, `false` when absent. One task at a time runs this \
         step's command: while another task's run of it is still going, a task \
         reaching the step waits there unstarted, and its run starts on the \
         first pass after that one exits. A background run holds the step until \
         it exits, wherever its task has moved on to. The same step id in \
         another pipeline does not hold it.",
    ),
    (
        "end",
        "The task stops here — nothing is scheduled for it again. May name no \
         agent, no command and no transition.",
    ),
];

/// The things refused when a pipeline file is loaded — [`Pipeline::validate`]'s
/// own checks, worded for a reader with no access to its source.
fn rules() -> Vec<&'static str> {
    vec![
        "the file name is the pipeline's name, and a task starts on the first step",
        "every cycle carries a `loop`, wherever along it the bound sits — a spent budget \
         parks on `blocked`",
        "`blocked` may be declared to staff it; `queued`, `done`, `paused` never",
        "a step is what it carries — `agent:`, `run:` or `end: true`",
        "prefer a script the repo already holds over a multi-command `run:` — a chain more \
         than one pipeline runs belongs in a file, named by relative path from the worktree \
         root",
        "a step never names its own id in `on_pass` or `on_fail` — a lap goes through another \
         step or not at all",
        "a `run:` means one thing by each exit code — a command that answers the same code \
         for two different outcomes cannot be routed on, and is not spoolway's to fix",
    ]
}

#[derive(Debug, serde::Serialize)]
struct ContractKeys {
    pipeline: &'static [&'static str],
    step: &'static [&'static str],
    refused: &'static [&'static str],
    reserved_ids: [&'static str; 3],
}

/// One of this project's own agent profiles, as `spoolway pipeline contract`
/// shows it — the facts a pipeline author needs in order to write `agent:`
/// and `effort:` without opening `config.toml`.
#[derive(Debug, serde::Serialize)]
struct AgentContractEntry {
    kind: String,
    /// `hosted` when this profile caps how many of its own lanes may run at
    /// once — a real question for an account with a rate limit — and `local`
    /// when it does not, the same distinction `docs/agents.md` draws between
    /// a cloud kind's `concurrency` and a local one's `models.<glob>.slots`.
    /// Derived rather than stored: no field on [`crate::config::AgentProfile`]
    /// names it directly.
    tier: &'static str,
    /// Whether this kind's adapter carries a flag `effort:` renders into —
    /// [`crate::agent::adapter`]'s own answer, the same one `pipeline check`
    /// refuses an `effort:` against when it is `false`.
    effort: bool,
    concurrency: usize,
}

/// The whole pipeline-file contract, printed as JSON by bare `spoolway
/// pipeline contract`.
#[derive(Debug, serde::Serialize)]
struct Contract {
    path: &'static str,
    keys: ContractKeys,
    fields: std::collections::BTreeMap<&'static str, &'static str>,
    rules: Vec<&'static str>,
    agents: std::collections::BTreeMap<String, AgentContractEntry>,
    prompts: Vec<String>,
    task_skeletons: Vec<String>,
    existing: Vec<String>,
    template: String,
    /// A second file, `overrides/pipelines/<name>.yml`, can patch what a
    /// tracked pipeline file says without touching the checkout — see
    /// `spoolway override contract` for the merge rule and what it may carry.
    overrides: &'static str,
}

/// Every `<name>.md` this project has written under the task-templates
/// directory, by file stem — the skeleton names that actually have a file of
/// their own, as against a pipeline whose `task_template:` falls back to
/// `default.md` with nothing written for it.
fn task_skeletons(repo: &Repo) -> Vec<String> {
    let dir = repo.task_templates_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .filter_map(|path| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names
}

fn build_contract(repo: &Repo, pipelines: &Pipelines) -> Contract {
    let agents = repo
        .config
        .agents
        .iter()
        .map(|(name, profile)| {
            let effort = crate::agent::adapter(&profile.kind)
                .is_some_and(|adapter| adapter.effort.is_some());
            let tier = if profile.concurrency > 0 {
                "hosted"
            } else {
                "local"
            };
            (
                name.clone(),
                AgentContractEntry {
                    kind: profile.kind.clone(),
                    tier,
                    effort,
                    concurrency: profile.concurrency,
                },
            )
        })
        .collect();

    let prompts = crate::prompt::entries(repo)
        .map(|entries| entries.into_iter().map(|e| e.name).collect())
        .unwrap_or_default();

    Contract {
        path: ".spoolway/pipelines/<name>.yml",
        keys: ContractKeys {
            pipeline: PIPELINE_KEYS,
            step: STEP_KEYS,
            refused: REFUSED_KEYS,
            reserved_ids: [
                crate::pipeline::QUEUED,
                crate::pipeline::DONE,
                crate::pipeline::PAUSED,
            ],
        },
        fields: FIELD_SENTENCES.iter().copied().collect(),
        rules: rules(),
        agents,
        prompts,
        task_skeletons: task_skeletons(repo),
        existing: pipelines.names().into_iter().map(str::to_string).collect(),
        template: template(),
        overrides: "A second file, `overrides/pipelines/<name>.yml`, can patch a step's values \
                    without touching this one — see `spoolway override contract`.",
    }
}

/// The annotated blank pipeline `template` carries — every key in
/// [`STEP_KEYS`] shown at least once, live or commented, and a real model on
/// every agent step so the file it becomes validates as written rather than
/// only once somebody has filled it in.
///
/// The model is [`crate::models::PLACEHOLDER`] on the local steps and
/// `claude-opus-5` on the hosted one, read into the string rather than typed
/// twice — the same placeholder `init` writes into a fresh project's own
/// pipelines.
fn template() -> String {
    format!(
        "# An annotated pipeline. Copy it to `.spoolway/pipelines/<name>.yml`, delete\n\
         # what this flow has no use for, and run `spoolway pipeline check`.\n\
         #\n\
         # Three things are never written here, because something else already says\n\
         # them: the name — the file name is the name, and a `name:` key is refused;\n\
         # the entry — a task starts on the first step in `steps:`; and queued, done,\n\
         # paused — the dispatcher's own states, never declared. `blocked` is the one\n\
         # exception: a pipeline may declare it to staff the step.\n\
         \n\
         # What this pipeline is for, in one sentence — read to choose between\n\
         # pipelines.\n\
         description: >-\n\
         \x20\x20One sentence on what this pipeline is for, and which tasks belong on\n\
         \x20\x20it rather than another one.\n\
         \n\
         # Which task skeleton a task queued here is written from. Defaults to this\n\
         # pipeline's own name under `.spoolway/templates/tasks/`, falling back to\n\
         # `default.md`.\n\
         # task_template: default\n\
         \n\
         steps:\n\
         \x20\x20- id: implement\n\
         \x20\x20\x20\x20description: Write the code to satisfy the task's acceptance criteria.\n\
         \x20\x20\x20\x20agent: pi                 # a profile from `[agents.*]` in config.toml\n\
         \x20\x20\x20\x20prompt: implementer       # a file under the prompts dir; defaults to the step id\n\
         \x20\x20\x20\x20model: {local_model}\n\
         \x20\x20\x20\x20# effort: high             `medium` or `high`; the kind must carry a level\n\
         \x20\x20\x20\x20# skills: a, b             invoked at the top of this step's opening prompt\n\
         \x20\x20\x20\x20# slot: false              run without consuming one of the profile's lanes\n\
         \x20\x20\x20\x20# gate: true               this step's pass parks on `paused` until `spoolway resume`\n\
         \x20\x20\x20\x20on_pass: review\n\
         \x20\x20\x20\x20on_fail: fix\n\
         \n\
         \x20\x20- id: review\n\
         \x20\x20\x20\x20description: Check the diff against the acceptance criteria and project standards.\n\
         \x20\x20\x20\x20agent: claude\n\
         \x20\x20\x20\x20prompt: reviewer\n\
         \x20\x20\x20\x20model: {hosted_model}\n\
         \x20\x20\x20\x20effort: high\n\
         \x20\x20\x20\x20on_pass: handover\n\
         \x20\x20\x20\x20on_fail: fix\n\
         \n\
         \x20\x20- id: fix\n\
         \x20\x20\x20\x20description: Work the review's findings, and nothing else.\n\
         \x20\x20\x20\x20agent: pi\n\
         \x20\x20\x20\x20prompt: implementer\n\
         \x20\x20\x20\x20model: {local_model}\n\
         \x20\x20\x20\x20session: true\n\
         \x20\x20\x20\x20loop: 3\n\
         \x20\x20\x20\x20on_pass: review\n\
         \x20\x20\x20\x20# on_fail:                a fail with none of its own goes to `blocked`\n\
         \n\
         \x20\x20# A command step: no model, no lane, no worker slot. `run:` is what makes\n\
         \x20\x20# it one, and its exit code is the outcome — zero takes `on_pass`, anything\n\
         \x20\x20# else `on_fail`.\n\
         \x20\x20- id: handover\n\
         \x20\x20\x20\x20run: spoolway stack\n\
         \x20\x20\x20\x20# timeout: 45m             30m unless the step says otherwise\n\
         \x20\x20\x20\x20# background: true         let the task move on; `on_fail` still routes it later\n\
         \x20\x20\x20\x20# headless: true            run detached, with no pane, the way every command did before\n\
         \x20\x20\x20\x20# last: true                only the top task of a chain runs it\n\
         \x20\x20\x20\x20# first: true               only a chain's declared root runs it\n\
         \x20\x20\x20\x20# serial: true              one task runs it at a time; the rest wait on the step\n\
         \x20\x20\x20\x20on_pass: done\n\
         \x20\x20\x20\x20# on_fail:                a fail with none of its own goes to `blocked`\n\
         \n\
         \x20\x20# A terminal step: the task stops here. `end: true` is declared rather\n\
         \x20\x20# than inferred, so a mistyped `agnet:` is an error instead of a silent stop.\n\
         \x20\x20# - id: shipped\n\
         \x20\x20#   end: true\n",
        local_model = crate::models::PLACEHOLDER,
        hosted_model = "claude-opus-5",
    )
}

/// `spoolway pipeline contract`: the whole pipeline-file format as JSON, and
/// nothing else on stdout.
pub fn pipeline_contract(repo: &Repo, pipelines: &Pipelines) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&build_contract(repo, pipelines))?
    );
    Ok(())
}

/// `pipeline show`'s marker for a command step carrying `last:` or `first:`
/// — refused together at load, so at most one ever applies. Pulled out of
/// [`show_one`] so the text is a fact `cargo test` can check without
/// capturing stdout.
fn chain_marker(step: &crate::pipeline::Step) -> &'static str {
    if step.last {
        " last-of-chain"
    } else if step.first {
        " first-of-chain"
    } else {
        ""
    }
}

fn show_one(pipeline: &Pipeline) -> Result<()> {
    println!(
        "pipeline `{}`  entry: {}{}",
        pipeline.name,
        pipeline.entry(),
        private_marker(pipeline)
    );
    if let Some(description) = &pipeline.description {
        println!("    {description}");
    }
    println!();

    for step in &pipeline.steps {
        let kind = step.kind().as_str();

        print!("  {:<10} {:<9}", step.id, kind);
        // `blocked` is never in the file the same way another step is: either
        // `Pipelines::assemble` materialised it whole from `[unattended]`, or
        // the pipeline declared an override and assemble filled in whatever
        // it left out. A reader looking at `agent=claude model=claude-opus-5`
        // has no way to tell those apart without this.
        if step.id == crate::pipeline::BLOCKED {
            let origin = if pipeline.blocked_declared {
                format!("overridden in {}.yml", pipeline.name)
            } else {
                "from config".to_string()
            };
            print!(" {origin}  ");
        }
        if let Some(agent) = &step.agent {
            print!(" agent={agent}");
            print!(" prompt={}", step.prompt_name());
            if let Some(model) = &step.model {
                print!(" model={model}");
            }
            if step.session {
                print!(" session");
            }
            if !step.slot {
                print!(" no-slot");
            }
            if step.gate {
                print!(" human-gate");
            }
            if !step.r#loop.is_unbounded() {
                print!(" loop={}", step.r#loop.describe());
                print!(" exit={}", step.loop_exit());
            }
        }
        if step.kind() == StepKind::Command {
            // Whether the pipeline waits is the thing a reader is looking for
            // here, so it is said either way rather than only when it is true.
            match step.background {
                true => print!(" background"),
                false => print!(" waits"),
            }
            // Said only when true, like `last-of-chain` below: a pane is the
            // default now, so a reader only needs telling about the step that
            // opted out of one.
            if step.headless {
                print!(" headless");
            }
            // Always, and resolved rather than only when written: a reader
            // asking what stops this hanging is asking about the default as
            // much as about an override.
            print!(
                " timeout={}",
                crate::config::human_duration::format(step.command_timeout())
            );
            // The keys that make a flow read differently for two tasks on the
            // same pipeline, so a reader who does not see one here would have
            // no way to know a step they are looking at is one most tasks
            // walk straight past.
            print!("{}", chain_marker(step));
            if !step.r#loop.is_unbounded() {
                print!(" loop={}", step.r#loop.describe());
                print!(" exit={}", step.loop_exit());
            }
        }
        println!();

        if let Some(description) = &step.description {
            println!("             {description}");
        }
        if let Some(run) = &step.run {
            println!("             run: {run}");
        }
        match step.kind() {
            // Nothing to say on arrival: a declared terminal simply stops the
            // task there. Only the reserved `done` stage tears a checkout
            // down, and `pipeline show` never lists that stage as a step.
            StepKind::Terminal => {}
            // `blocked` declares neither: where its pass goes is read from the
            // step the task stopped on rather than from here, and anything
            // else — a fail, a block, or a pause — parks the task on `paused`
            // instead, with the same destination waiting — none of which
            // `on_pass`/`on_fail` could say even if it did.
            _ if step.id == crate::pipeline::BLOCKED => {
                println!(
                    "             pass -> past where it blocked   fail/block/pause -> paused \
                     (same destination waiting)"
                );
            }
            _ => {
                println!(
                    "             pass -> {}   fail -> {}",
                    step.on_pass.as_deref().unwrap_or("(stays)"),
                    step.on_fail.as_deref().unwrap_or(crate::pipeline::BLOCKED)
                );
            }
        }
        println!();
    }

    Ok(())
}

/// Every disk-dependent problem `pipeline_check` finds in the project's own
/// loaded pipelines: an agent profile a step names but config does not
/// define, a task skeleton a `task_template:` names but nobody wrote, and —
/// per agent step — a missing prompt file, a blank model, an
/// `effort:`/`session:`/`skills:` the step's agent kind cannot carry.
///
/// Runtime readiness is a project's own to answer: whether `assets/pipelines/*.yml`
/// still parses and stays neutral about Pi, model and effort choices is
/// release-time proof instead, held in `src/assets.rs`'s own tests against
/// the repository rather than any one project's config.
fn step_problems(repo: &Repo, pipelines: &Pipelines, config: &Config) -> Vec<String> {
    let mut problems = Vec::new();

    for (agent, steps) in pipelines.referenced_agents() {
        if !config.agents.contains_key(agent) {
            problems.push(format!(
                "agent profile `{agent}` is not defined in config, and {steps:?} run on it"
            ));
        }
    }

    for pipeline in pipelines.pipelines.values() {
        // A pipeline that names no skeleton is the normal case and needs no
        // file: it takes its own name, and `default.md` answers when there is
        // nothing under it. One that names a skeleton meant that name, so a
        // file that is not there is a typo rather than an intention.
        if let Some(named) = &pipeline.task_template
            && !crate::task_template::exists_for(repo, pipeline)
        {
            let path = repo.task_templates_dir().join(format!("{named}.md"));
            problems.push(format!(
                "pipeline `{}` names task skeleton `{named}`, and {} is not there — write \
                 it, or drop the `task_template:` to take `{}`",
                pipeline.name,
                relative(&repo.checkout, &path),
                crate::task_template::FALLBACK
            ));
        }

        for step in &pipeline.steps {
            // A command or terminal step starts no lane, so `skills:` on one
            // is written in the belief that it does something. `prompt:`,
            // `model:`, `effort:` and `session:` are refused the same way on
            // a non-agent step, but by `Pipeline::validate` — this one stays
            // here instead, so every `skills:` problem a project sees comes
            // out of the one command, in the one pass.
            if !step.skills.is_empty() && step.kind() != StepKind::Agent {
                problems.push(format!(
                    "`{}`/`{}` sets `skills:` on a {} step, which runs no model at all — \
                     delete `skills:`",
                    pipeline.name,
                    step.id,
                    step.kind().as_str()
                ));
            }

            if step.kind() != StepKind::Agent {
                continue;
            }
            let prompt = crate::prompt::path_for(repo, step.prompt_name());
            if !prompt.exists() {
                problems.push(format!(
                    "`{}`/`{}` needs prompt {} — run `spoolway init` or write it",
                    pipeline.name,
                    step.id,
                    prompt.display()
                ));
            }

            // spoolway names no model of its own: a step that names none, or
            // names blank, has nothing to launch.
            let model_is_blank = step.model.as_deref().is_some_and(|m| m.trim().is_empty());
            let model_is_missing = step.model.is_none();
            // `blocked` is assembled from `[unattended]` even for an attended
            // run, where it is a parking state and starts no lane. Its blank
            // model becomes a real staffing error only when unattended mode
            // turns that synthetic step into a lane.
            let unstaffed_blocked =
                step.id == crate::pipeline::BLOCKED && !config.unattended.enabled;
            if !unstaffed_blocked && (model_is_missing || model_is_blank) {
                problems.push(format!(
                    "`{}`/`{}` names no model: — give it one",
                    pipeline.name, step.id
                ));
            }

            // Init writes an explicit blank so every step shows the choice a
            // person still has to make. A blank means no effort argument; it
            // is not an unsupported level on kinds without an effort flag.
            if let Some(effort) = step.effort.as_deref().filter(|e| !e.trim().is_empty()) {
                if effort.trim().eq_ignore_ascii_case("auto") {
                    problems.push(format!(
                        "`{}`/`{}` sets `effort: auto` — spoolway resolved that itself once, \
                         against sensitive paths nothing computes any more, so it is not a \
                         level any kind accepts; name a real one or drop the key",
                        pipeline.name, step.id
                    ));
                } else {
                    let carries_effort = step
                        .agent
                        .as_deref()
                        .and_then(|agent| config.agent(agent).ok())
                        .is_some_and(|profile| {
                            crate::agent::adapter(&profile.kind)
                                .is_some_and(|adapter| adapter.effort.is_some())
                        });
                    if !carries_effort {
                        problems.push(format!(
                            "`{}`/`{}` sets `effort:`, but its agent kind has no way to carry \
                             a level — some kinds spell it as a flag and some as a config \
                             override, and this one does neither",
                            pipeline.name, step.id
                        ));
                    }
                }
            }

            if step.session {
                let kind = step
                    .agent
                    .as_deref()
                    .and_then(|agent| config.agent(agent).ok())
                    .map(|profile| profile.kind.as_str());
                let resumes = kind
                    .and_then(crate::agent::adapter)
                    .is_some_and(|adapter| adapter.resumes());
                if !resumes {
                    let resuming: Vec<&str> = crate::agent::ADAPTERS
                        .iter()
                        .filter(|adapter| adapter.resumes())
                        .map(|adapter| adapter.kind)
                        .collect();
                    problems.push(format!(
                        "`{}`/`{}` sets `session:`, but its agent kind{} has no established \
                         resume flag. Kinds that resume: {}",
                        pipeline.name,
                        step.id,
                        kind.map(|k| format!(" `{k}`")).unwrap_or_default(),
                        resuming.join(", ")
                    ));
                }
            }

            if !step.skills.is_empty() {
                let kind = step
                    .agent
                    .as_deref()
                    .and_then(|agent| config.agent(agent).ok())
                    .map(|profile| profile.kind.clone());
                let loads = kind
                    .as_deref()
                    .and_then(crate::agent::adapter)
                    .is_some_and(|adapter| adapter.skills);
                // Whether the kind loads skills at all is the whole of the
                // check. The names themselves are never read off the disk:
                // spoolway can see a project's own skills and the user's, but
                // a plugin installs its skills somewhere spoolway has no way
                // to enumerate, so "in neither directory" does not mean "not
                // installed" — and refusing on it fails every pipeline that
                // names a plugin skill. The agent resolves the name at
                // launch, which is the only place it can be resolved.
                if !loads {
                    let loading: Vec<&str> = crate::agent::ADAPTERS
                        .iter()
                        .filter(|adapter| adapter.skills)
                        .map(|adapter| adapter.kind)
                        .collect();
                    problems.push(format!(
                        "`{}`/`{}` sets `skills:`, but its agent kind{} loads no skills. \
                         Kinds that load skills: {}",
                        pipeline.name,
                        step.id,
                        kind.map(|k| format!(" `{k}`")).unwrap_or_default(),
                        loading.join(", ")
                    ));
                }
            }
        }
    }

    problems
}

/// Split an old single `pipeline.yml` into one file per pipeline.
///
/// Writes what it can and *says* what it cannot: `default:` and `observer:`
/// belong in config.toml now, and rewriting somebody's config on their behalf
/// is a bigger liberty than this command needs to take. The old file is left
/// alone for the same reason — the directory wins from the moment it exists, so
/// nothing is lost by leaving it until its author has read what came out of it.
/// Validate the pipelines against the config they will actually run with.
pub fn pipeline_check(repo: &Repo, pipelines: Result<Pipelines>, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }

    // A pipeline file that will not load does not stop this check — it is
    // reported, and nothing else is derived: there is no loaded set left to
    // check a step, a skip, or a prompt against, and the embedded samples
    // are release-time proof, not this project's own. A project carrying
    // one of the three retired step shapes `spoolway sync` migrates hears
    // about every occurrence at once, across every pipeline file, rather
    // than the one `Pipelines::load` itself stopped at — see
    // `crate::pipeline::Pipelines::refusals`.
    let pipelines = match pipelines {
        Ok(pipelines) => pipelines,
        Err(err) => {
            let refusals = crate::pipeline::Pipelines::refusals(&repo.checkout);
            if refusals.is_empty() {
                println!("  problem: pipelines do not load: {err:#}");
                bail!("1 problem(s) found");
            }
            for refusal in &refusals {
                println!("  problem: {refusal}");
            }
            bail!("{} problem(s) found", refusals.len());
        }
    };
    let pipelines = &pipelines;

    pipelines.validate()?;

    // Redundant-key, gate and description warnings, gathered up front: they
    // are worth a person's attention but never fail this check on their own
    // — see `Pipeline::redundant_on_fail_warnings`, `Pipeline::gate_warnings`
    // and `Pipeline::description_warnings`.
    let mut gate_warnings = Vec::new();
    for pipeline in pipelines.pipelines.values() {
        gate_warnings.extend(pipeline.redundant_on_fail_warnings());
        gate_warnings.extend(pipeline.gate_warnings());
        gate_warnings.extend(pipeline.description_warnings());
    }

    let mut problems = step_problems(repo, pipelines, &repo.config);

    // A queued task's own `skip:` names steps by hand — a replay's copy, or
    // one a person edited in — and a pipeline's steps can be renamed out from
    // under it. Caught here rather than only at the dispatch pass that would
    // otherwise silently do nothing with a name that never matches.
    for task in repo.tasks().unwrap_or_default() {
        if task.front.skip.is_empty() {
            continue;
        }
        let Ok(pipeline) = pipelines.for_task(&task) else {
            continue;
        };
        for step in &task.front.skip {
            if pipeline.step(step).is_none() {
                problems.push(format!(
                    "task `{}` names `{step}` in `skip:`, and pipeline `{}` has no such step",
                    task.id(),
                    pipeline.name
                ));
            }
        }
    }

    // Read the prompts against the steps that run them: this rule is a fact,
    // not an opinion — a prompt naming a `spoolway …` command this binary
    // does not have, checked against clap's own command tree. Nothing
    // regenerates a prompt on upgrade any more, so this is the only place
    // left that catches the drift, and it fails the check rather than
    // printing beneath a green result — the alternative is a lane running a
    // command that no longer exists, twenty minutes in, in a pane nobody is
    // watching. The lint's other rule, below, reads the same prompts for a
    // restated report contract, and warns instead.
    for finding in crate::prompt::lint(repo, pipelines)? {
        problems.push(finding.render());
    }

    // A prompt naming `spoolway report` or one of its flags is a second
    // report contract shipped inside the prompt, which can contradict the
    // one spoolway injects at launch — but a prompt whose role is writing
    // *about* spoolway has a real reason to name it, and reading prose
    // cannot tell that reason from a careless restatement. Warned rather
    // than failed, on the same channel as the gate and description
    // warnings above.
    for finding in crate::prompt::lint_warnings(repo, pipelines)? {
        gate_warnings.push(finding.render());
    }

    if problems.is_empty() {
        println!(
            "{} pipeline(s) valid: {:?}, agents {:?}",
            pipelines.pipelines.len(),
            pipelines.names(),
            pipelines.referenced_agents().keys().collect::<Vec<_>>()
        );
        report_gate_warnings(&gate_warnings);
        return Ok(());
    }

    for problem in &problems {
        println!("  problem: {problem}");
    }
    report_gate_warnings(&gate_warnings);
    bail!("{} problem(s) found", problems.len())
}

/// Print the gate and description warnings `pipeline_check` gathered —
/// worth a person's attention, but never a reason this check fails.
fn report_gate_warnings(warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    println!();
    for warning in warnings {
        println!("  warning: {warning}");
    }
}

/// `spoolway pipeline override <name> --set <step>.<key>=<value>`.
///
/// Reads the tracked pipeline from `repo.checkout` — the branch actually
/// running, same as [`pipeline_show`] and every other reader here — so a
/// step id or a key the merge would refuse is caught before anything is
/// written, by probing the exact rule [`crate::overrides::apply_step_patch`]
/// applies at load rather than a second copy of it. The layer itself is
/// project-wide (`repo.overrides_dir()`), so a lane in any worktree sees the
/// same fork on its next pass.
pub fn pipeline_override(repo: &Repo, name: &str, set: &str) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(false)?;
    }
    let (step_key, raw_value) = set
        .split_once('=')
        .with_context(|| format!("`--set {set}` — expected `<step>.<key>=<value>`"))?;
    let (step_id, key) = step_key
        .split_once('.')
        .with_context(|| format!("`--set {set}` — expected `<step>.<key>=<value>`"))?;

    let tracked = Pipelines::load_tracked(&repo.checkout, &repo.config)
        .with_context(|| format!("reading the tracked pipeline `{name}`"))?;
    // A private pipeline is never in `tracked` — `load_tracked` never reads
    // `local/` — so it is looked up there separately before giving up. The
    // patch this writes only ever merges onto a *tracked* pipeline at load
    // (`merge_private` never applies one to a private file), so a private
    // pipeline's patch sits waiting rather than taking effect the moment
    // this command writes it — `spoolway pipeline promote` is what starts
    // it applying. `private` is kept past this match to tell that case
    // apart from a tracked one below, for the closing message.
    let private = if tracked.pipelines.contains_key(name) {
        None
    } else {
        crate::pipeline::Pipelines::private(&repo.checkout, name)?
    };
    let pipeline = match tracked.pipelines.get(name).or(private.as_ref()) {
        Some(pipeline) => pipeline,
        None => bail!("no pipeline named `{name}`"),
    };
    let step = pipeline.steps.iter().find(|s| s.id == step_id).with_context(|| {
        format!(
            "pipeline `{name}` has no step `{step_id}` — a patch may only set a value on a step \
             that already exists"
        )
    })?;

    let value: serde_norway::Value = serde_norway::from_str(raw_value)
        .with_context(|| format!("`{raw_value}` is not a value `{key}` can take"))?;

    // Refused by the exact rule the merge itself applies at load — see
    // `apply_step_patch` — so nothing accepted here can be rejected later,
    // silently, on the very next dispatcher pass.
    let mut fields = serde_norway::Mapping::new();
    fields.insert(serde_norway::Value::String(key.to_string()), value.clone());
    let mut probe = step.clone();
    crate::overrides::apply_step_patch(&mut probe, &fields)
        .with_context(|| format!("`{key}` on step `{step_id}` of pipeline `{name}`"))?;

    // The value currently active, override already layered on included, so a
    // second `--set` on the same key shows what it is actually replacing.
    let active = Pipelines::load(&repo.checkout, &repo.config)?;
    let old = match active
        .pipelines
        .get(name)
        .and_then(|p| p.steps.iter().find(|s| s.id == step_id))
    {
        Some(step) => crate::overrides::field_display(step, key)?,
        None => String::new(),
    };
    let new = crate::overrides::field_display(&probe, key)?;

    let overrides_dir = repo.overrides_dir();
    let mut patch =
        crate::overrides::read_pipeline_patch(&overrides_dir, name)?.unwrap_or_default();
    patch
        .steps
        .entry(step_id.to_string())
        .or_default()
        .insert(serde_norway::Value::String(key.to_string()), value);
    crate::overrides::write_pipeline_patch(&overrides_dir, name, &patch)?;

    let path = crate::overrides::pipeline_patch_path(&overrides_dir, name);
    println!("  wrote {}", path.display());
    println!("    {step_id}.{key}   {old} -> {new}");
    println!();
    if private.is_some() {
        println!(
            "  waiting on `spoolway pipeline promote {name}` — a private pipeline's patch only \
             starts applying once it is tracked. `spoolway override drop {name}` to clear it."
        );
    } else {
        println!(
            "  active on the next dispatcher pass. `spoolway override drop {name}` to clear it."
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `pipeline copy`, `prompt copy` and `pipeline promote` — the private
// layer's own write commands, see `crate::local` and the plan's
// `d-private-pipelines` decision. `copy` starts a private pipeline or
// prompt from one that already exists, tracked or private; `promote` is its
// reverse, moving a private pipeline — the private prompts it names and its
// own private skeleton — into the tracked control plane. Both refuse a
// clash rather than silently shadow or overwrite a file.
// ---------------------------------------------------------------------------

/// Extensions [`crate::pipeline::Pipelines::dir_in`]'s own directory accepts
/// for a pipeline file — `.yml` first, `.yaml` for a project that wrote
/// that instead — the same two [`crate::pipeline`]'s own `read_pipeline_dir`
/// reads, so a `.yaml` pipeline is found, and clashed against, the way
/// `merge_private` already treats it.
const PIPELINE_EXTS: &[&str] = &["yml", "yaml"];

/// The first of `dir/<name>.yml` and `dir/<name>.yaml` that exists, or
/// `None` with neither.
fn pipeline_file_in(dir: &Path, name: &str) -> Option<PathBuf> {
    PIPELINE_EXTS
        .iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .find(|path| path.is_file())
}

/// Where `pipeline copy` and `prompt copy` write: `local/pipelines/`,
/// `local/prompts/` and `local/templates/tasks/` in repo mode, or the
/// tracked directories themselves in home mode, where
/// [`crate::config::setup_dir_in`] already resolves those to the
/// workspace's own `config/` — the whole setup there being private already,
/// there is no second private layer to add. See [`crate::local`].
fn copy_target_dirs(repo: &Repo) -> (PathBuf, PathBuf) {
    if crate::local::is_repo_mode(&repo.checkout) {
        let local = repo.local_dir();
        (
            crate::local::pipelines_dir(&local),
            crate::local::task_templates_dir(&local),
        )
    } else {
        (Pipelines::dir_in(&repo.checkout), repo.task_templates_dir())
    }
}

/// Where `prompt copy <from> <to>` writes: `local/prompts/<to>/PROMPT.md`
/// in repo mode, or the tracked directory shape itself in home mode —
/// [`crate::prompt::directory_form`], never `repo.prompts_dir().join(…)`
/// built by hand, which
/// `commands::tests::nothing_builds_a_prompt_path_except_the_one_function_that_should`
/// refuses outright.
fn prompt_copy_dest(repo: &Repo, to: &str) -> PathBuf {
    if crate::local::is_repo_mode(&repo.checkout) {
        crate::local::prompts_dir(&repo.local_dir())
            .join(to)
            .join(crate::assets::PROMPT_FILE)
    } else {
        crate::prompt::directory_form(repo, to)
    }
}

/// Refuse `name` as a `<to>` that already names a pipeline — tracked or
/// private, `.yml` or `.yaml`.
fn refuse_pipeline_clash(repo: &Repo, name: &str) -> Result<()> {
    if let Some(path) = pipeline_file_in(&Pipelines::dir_in(&repo.checkout), name) {
        bail!(
            "`{name}` already exists — {} — see `spoolway pipeline list` and choose a \
             different `<to>`",
            path.display()
        );
    }
    if crate::local::is_repo_mode(&repo.checkout)
        && let Some(path) = pipeline_file_in(&crate::local::pipelines_dir(&repo.local_dir()), name)
    {
        bail!(
            "`{name}` already exists — {} — see `spoolway pipeline list` and choose a \
             different `<to>`",
            path.display()
        );
    }
    Ok(())
}

/// Refuse `name` as a `<to>` whose skeleton already exists — tracked
/// `<name>.md` or, in repo mode, a private `local/templates/tasks/<name>.md`
/// — the clash `refuse_pipeline_clash` cannot see, since a skeleton is a
/// file of its own that can exist with no pipeline of that name at all (the
/// task's own repro: `echo MINE > local/templates/tasks/foo.md` with no
/// `foo` pipeline anywhere). Checked before `pipeline_copy` writes anything,
/// so a person's skeleton is never silently replaced.
fn refuse_skeleton_clash(repo: &Repo, name: &str) -> Result<()> {
    let tracked = repo.task_templates_dir().join(format!("{name}.md"));
    if tracked.is_file() {
        bail!(
            "a skeleton for `{name}` already exists — {} — choose a different `<to>`",
            tracked.display()
        );
    }
    if crate::local::is_repo_mode(&repo.checkout) {
        let private =
            crate::local::task_templates_dir(&repo.local_dir()).join(format!("{name}.md"));
        if private.is_file() {
            bail!(
                "a skeleton for `{name}` already exists — {} — choose a different `<to>`",
                private.display()
            );
        }
    }
    Ok(())
}

/// Refuse `name` as a `<to>` that already names a prompt — tracked (nested
/// or the legacy flat shape, same as [`crate::prompt::path_for_tracked`]) or
/// private.
fn refuse_prompt_clash(repo: &Repo, name: &str) -> Result<()> {
    let tracked = crate::prompt::path_for_tracked(repo, name);
    if tracked.is_file() {
        bail!(
            "`{name}` already exists — {} — see `spoolway prompt list` and choose a different \
             `<to>`",
            tracked.display()
        );
    }
    if crate::local::is_repo_mode(&repo.checkout) {
        let private = crate::local::prompts_dir(&repo.local_dir())
            .join(name)
            .join(crate::assets::PROMPT_FILE);
        if private.is_file() {
            bail!(
                "`{name}` already exists — {} — see `spoolway prompt list` and choose a \
                 different `<to>`",
                private.display()
            );
        }
    }
    Ok(())
}

/// The `wrote    <path>` lines `pipeline copy` and `prompt copy` print, `~`
/// standing in for home — both write under a project's home (`local/`) or a
/// home-mode workspace's own folder, never inside the checkout, so
/// [`crate::fmt::relative`] (repo-relative) would only draw a `../..`
/// nobody wants to read; [`crate::repo::shorten_home`] is the same shortening
/// the `checkout:` note itself uses.
fn print_wrote(json: bool, paths: &[&Path]) -> Result<()> {
    if json {
        let payload = serde_json::json!({
            "wrote": paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    for path in paths {
        println!("{:<9}{}", "wrote", crate::repo::shorten_home(path));
    }
    Ok(())
}

/// `spoolway pipeline copy <from> <to>`.
///
/// `from` may be tracked or already private — the tracked directory checked
/// first, the private one only once that is confirmed absent, the same
/// order every other private-layer reader in this project uses. The
/// skeleton copied alongside it is whatever
/// [`crate::task_template::resolve_for`] would hand a task queued on `from`
/// right now, following the identical fallback chain a queued task gets —
/// `from`'s own file if it has one, down to the built-in — so a pipeline
/// copied from one with no skeleton of its own still gets a real file to
/// diverge from, not an empty one.
///
/// Only a `from` with no `task_template:` of its own gets a skeleton
/// written, as `<to>.md` — the name its copy resolves through once it takes
/// `to`'s identity. `raw` is copied byte for byte, so a `from` that names an
/// explicit `task_template:` leaves the copy naming that identical skeleton:
/// the two share it, exactly as the YAML says, and nothing is written —
/// `<to>.md` would be a file neither reads, and rewriting the shared file
/// would change what `from` reads too.
pub fn pipeline_copy(repo: &Repo, from: &str, to: &str, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }

    // Caught before `from` is ever looked up or `to` ever joined onto a
    // directory — see `crate::local::refuse_unless_plain_name`.
    crate::local::refuse_unless_plain_name("pipeline", from)?;
    crate::local::refuse_unless_plain_name("pipeline", to)?;

    let source = pipeline_file_in(&Pipelines::dir_in(&repo.checkout), from)
        .or_else(|| {
            crate::local::is_repo_mode(&repo.checkout)
                .then(|| pipeline_file_in(&crate::local::pipelines_dir(&repo.local_dir()), from))
                .flatten()
        })
        .with_context(|| format!("no pipeline named `{from}` — see `spoolway pipeline list`"))?;
    let raw = std::fs::read_to_string(&source)
        .with_context(|| format!("reading {}", source.display()))?;
    // Unchecked, the same trust level `Pipelines::load` itself gives every
    // file before `assemble`/`validate` run on the whole set — read only
    // for `task_template:`, never run, so an unrelated broken pipeline file
    // elsewhere must not stop this copy the way loading the full set would.
    let from_pipeline = crate::pipeline::parse_unchecked(from, &raw)
        .with_context(|| format!("parsing {}", source.display()))?;

    // An explicit `task_template:` is shared, not copied — see the doc
    // comment above.
    let shares_skeleton = from_pipeline.task_template.is_some();

    refuse_pipeline_clash(repo, to)?;
    if !shares_skeleton {
        refuse_skeleton_clash(repo, to)?;
    }

    let (pipelines_dir, templates_dir) = copy_target_dirs(repo);
    let pipeline_dest = pipelines_dir.join(format!("{to}.yml"));
    write_atomic(&pipeline_dest, &raw)?;
    if shares_skeleton {
        return print_wrote(json, &[&pipeline_dest]);
    }

    let skeleton = crate::task_template::resolve_for(repo, &from_pipeline);
    let skeleton_dest = templates_dir.join(format!("{to}.md"));
    write_atomic(&skeleton_dest, &skeleton)?;

    print_wrote(json, &[&pipeline_dest, &skeleton_dest])
}

/// `spoolway prompt copy <from> <to>`.
///
/// Reads the tracked or already-private file whole, the same as
/// `commands::prompt_override` forking a tracked prompt into the patch
/// layer — never through [`crate::prompt::path_for`], which would also
/// consult the override layer, a different question from "what does `from`
/// name, tracked or private" this command answers.
pub fn prompt_copy(repo: &Repo, from: &str, to: &str, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }

    // Caught before `from` is ever looked up or `to` ever joined onto a
    // directory — see `crate::local::refuse_unless_plain_name`.
    crate::local::refuse_unless_plain_name("prompt", from)?;
    crate::local::refuse_unless_plain_name("prompt", to)?;

    let tracked = crate::prompt::path_for_tracked(repo, from);
    let source = if tracked.is_file() {
        tracked
    } else if crate::local::is_repo_mode(&repo.checkout) {
        let private = crate::local::prompts_dir(&repo.local_dir())
            .join(from)
            .join(crate::assets::PROMPT_FILE);
        if !private.is_file() {
            bail!("no prompt named `{from}` — see `spoolway prompt list`");
        }
        private
    } else {
        bail!("no prompt named `{from}` — see `spoolway prompt list`");
    };
    let body = std::fs::read_to_string(&source)
        .with_context(|| format!("reading {}", source.display()))?;

    refuse_prompt_clash(repo, to)?;

    let dest = prompt_copy_dest(repo, to);
    write_atomic(&dest, &body)?;

    print_wrote(json, &[&dest])
}

/// One file or directory `pipeline promote` moved.
struct Moved {
    from: PathBuf,
    to: PathBuf,
    dir: bool,
}

/// Copy `from`'s whole text to `to` — a copy built from a read and an
/// atomic write rather than [`std::fs::copy`], so a reader never sees a
/// half-written `to`, and rather than [`std::fs::rename`], because `from`
/// (under a project's home) and `to` (under the checkout) are not
/// guaranteed to share a filesystem, and `rename` refuses across one.
///
/// This only copies — it never touches `from`. [`pipeline_promote`] runs
/// every item's copy first and only deletes the private sources once every
/// one of them has landed, so a copy failing partway through only ever has
/// to remove the copies already made ([`undo_copy`]) — the private sources
/// are all still there. A *delete* failing later is a different case,
/// handled with [`undo_delete`]: by then some private sources are already
/// gone, and those have to be restored from their own tracked copy before
/// that copy, too, can be removed.
fn copy_file(from: &Path, to: &Path) -> Result<()> {
    let body = std::fs::read(from).with_context(|| format!("reading {}", from.display()))?;
    write_atomic(to, body)
}

/// A plain recursive copy, every file under `from` landing at the same
/// relative path under `to`.
fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry.with_context(|| format!("reading {}", from.display()))?;
        let dest = to.join(entry.file_name());
        if entry
            .file_type()
            .with_context(|| format!("reading {}", entry.path().display()))?
            .is_dir()
        {
            copy_dir_all(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest).with_context(|| {
                format!("copying {} to {}", entry.path().display(), dest.display())
            })?;
        }
    }
    Ok(())
}

/// Whether `dir`, or anything nested under it, holds a symlink that points
/// at a directory — or at nothing at all.
///
/// `copy_dir_all` walks a directory with [`std::fs::read_dir`] and, for
/// anything that is not itself a directory, hands it to [`std::fs::copy`].
/// That call follows a symlink to a regular file and copies its contents
/// correctly, so a plain file symlink is left for it to handle exactly as
/// before this check existed — this is not the "changing what promote
/// moves" the task ruled out. A symlink to a directory is different:
/// `std::fs::copy` cannot copy a directory at all, and fails partway
/// through a copy that `pipeline_promote` has otherwise already committed
/// to. A dangling symlink fails the same way, with nothing to follow to
/// find out it would have been a problem. Both are checked up front, before
/// the plan is executed at all, so a prompt folder holding one is refused
/// outright rather than copied halfway.
fn dir_contains_symlinked_directory(dir: &Path) -> Result<bool> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("reading {}", entry.path().display()))?;
        if file_type.is_symlink() {
            // `metadata` (unlike the `symlink_metadata` that `file_type`
            // above already reads) follows the link. `is_dir()` false here
            // also covers a dangling symlink, where `metadata` errors out —
            // treated the same as a directory target, since there is
            // nothing for `std::fs::copy` to copy either way.
            let points_at_dir = std::fs::metadata(entry.path())
                .map(|meta| meta.is_dir())
                .unwrap_or(true);
            if points_at_dir {
                return Ok(true);
            }
            continue;
        }
        if file_type.is_dir() && dir_contains_symlinked_directory(&entry.path())? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Copy a [`Moved`] plan item forward, directory or plain file alike.
fn copy_item(item: &Moved) -> Result<()> {
    if item.dir {
        copy_dir_all(&item.from, &item.to)
    } else {
        copy_file(&item.from, &item.to)
    }
}

/// Remove a plan item's private source, once its copy has already landed —
/// the second half of a move.
fn delete_source(item: &Moved) -> Result<()> {
    if item.dir {
        std::fs::remove_dir_all(&item.from)
            .with_context(|| format!("removing {}", item.from.display()))
    } else {
        std::fs::remove_file(&item.from)
            .with_context(|| format!("removing {}", item.from.display()))
    }
}

/// Undo a copy already made — removes `item.to`, leaving `item.from`
/// untouched. Used to roll back the copies a failed promote already wrote,
/// so a part that finished is not left behind by a part that did not.
fn undo_copy(item: &Moved) -> Result<()> {
    if !item.to.exists() {
        return Ok(());
    }
    if item.dir {
        std::fs::remove_dir_all(&item.to).with_context(|| format!("removing {}", item.to.display()))
    } else {
        std::fs::remove_file(&item.to).with_context(|| format!("removing {}", item.to.display()))
    }
}

/// Undo a delete — copies back from `item.to` whatever [`delete_source`]
/// actually removed from `item.from`, and nothing else.
///
/// Only what is missing is written. A delete that failed may have removed
/// nothing at all: with `local/pipelines` read-only, `remove_file` on the
/// pipeline file fails and leaves it in place. Re-copying it anyway wrote a
/// temporary file into that same read-only folder, failed, and stopped the
/// rollback with tracked copies left beside the private ones — the clash
/// that breaks every command (seen in review, 2026-10-01). A file still
/// present is intact, because `remove_file` removes a file whole or not at
/// all.
fn undo_delete(item: &Moved) -> Result<()> {
    if item.dir {
        restore_missing(&item.to, &item.from)
    } else if item.from.symlink_metadata().is_ok() {
        Ok(())
    } else {
        copy_file(&item.to, &item.from)
    }
}

/// Copy every file under `from` that is missing at the same relative path
/// under `to`, leaving the ones already there alone — [`undo_delete`]'s
/// half for a directory that `remove_dir_all` only partly removed.
fn restore_missing(from: &Path, to: &Path) -> Result<()> {
    if !to.is_dir() {
        std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    }
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry.with_context(|| format!("reading {}", from.display()))?;
        let dest = to.join(entry.file_name());
        if entry
            .file_type()
            .with_context(|| format!("reading {}", entry.path().display()))?
            .is_dir()
        {
            restore_missing(&entry.path(), &dest)?;
        } else if dest.symlink_metadata().is_err() {
            std::fs::copy(entry.path(), &dest).with_context(|| {
                format!("copying {} to {}", entry.path().display(), dest.display())
            })?;
        }
    }
    Ok(())
}

/// The `moved    <from>  ->  <to>` lines `pipeline promote` prints.
///
/// `from` is shown relative to `repo.home` — `local/pipelines/impl-strict.yml`,
/// the mockup's own spelling — and `to` relative to `repo.root` —
/// `.spoolway/pipelines/impl-strict.yml`. The two sides never share a root
/// to be relative to together: the private layer lives under a project's
/// home ([`crate::local::dir_for`]), the tracked control plane under the
/// checkout, and `pipeline_promote` already refuses to run anywhere but the
/// main checkout, so `repo.root` is exactly the tracked side's own root.
fn print_moved(repo: &Repo, json: bool, moved: &[Moved]) -> Result<()> {
    if json {
        let payload = serde_json::json!({
            "moved": moved.iter().map(|m| serde_json::json!({
                "from": crate::fmt::relative(&repo.home, &m.from),
                "to": crate::fmt::relative(&repo.root, &m.to),
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }
    let rows: Vec<(String, String)> = moved
        .iter()
        .map(|m| {
            let suffix = if m.dir { "/" } else { "" };
            (
                format!("{}{suffix}", crate::fmt::relative(&repo.home, &m.from)),
                format!("{}{suffix}", crate::fmt::relative(&repo.root, &m.to)),
            )
        })
        .collect();
    let from_w = rows
        .iter()
        .map(|(f, _)| f.chars().count())
        .max()
        .unwrap_or(0);
    for (from, to) in &rows {
        println!("{:<9}{from:<from_w$}  ->  {to}", "moved");
    }
    Ok(())
}

/// `spoolway pipeline promote <name>`.
///
/// Moves the private pipeline, every private prompt it names, and its own
/// private skeleton into the tracked control plane, then deletes the
/// private files — the reverse of `pipeline copy`.
///
/// Refused inside a linked worktree for the same reason `override promote`
/// refuses there: the dispatcher reads `repo.root`'s tracked files, never a
/// linked worktree's own copy, and `repo.checkout == repo.root` is what
/// lets every path below be built from [`Repo`]'s own tracked accessors
/// rather than a second, root-only copy of each. Refused outright in home
/// mode, where [`crate::local::is_repo_mode`] is false because the whole
/// setup is already private and there is nothing to promote into.
pub fn pipeline_promote(repo: &Repo, name: &str, json: bool) -> Result<()> {
    if repo.checkout != repo.root {
        bail!(
            "the dispatcher reads the project's tracked files, not this worktree's.\n  spoolway \
             -C {} pipeline promote {name}",
            repo.root.display()
        );
    }
    if !crate::local::is_repo_mode(&repo.checkout) {
        bail!(
            "this setup is already private — there is nothing for `pipeline promote` to move \
             into"
        );
    }

    let local = repo.local_dir();
    let source =
        pipeline_file_in(&crate::local::pipelines_dir(&local), name).with_context(|| {
            format!("no private pipeline named `{name}` — see `spoolway pipeline list`")
        })?;
    if let Some(clash) = pipeline_file_in(&Pipelines::dir_in(&repo.root), name) {
        bail!(
            "`{name}` already exists in the tracked pipelines — {} — rename one of the two",
            clash.display()
        );
    }

    // Loaded, not merely read off disk: the private prompts this pipeline
    // names are read off its own parsed steps, the same source
    // `crate::pipeline::merge_private`'s own refusal reads, and loading
    // also proves the pipeline is one that would actually work once
    // promoted, rather than moving a file nothing has ever validated.
    let pipelines = Pipelines::load(&repo.checkout, &repo.config)?;
    let pipeline = pipelines
        .pipelines
        .get(name)
        .filter(|p| p.private_file.is_some())
        .with_context(|| {
            format!("no private pipeline named `{name}` — see `spoolway pipeline list`")
        })?;

    // Every destination this promote would touch, clash-checked in full
    // before anything is moved — see
    // `pipeline_promote_refuses_a_skeleton_clash_without_moving_anything_first`.
    // A clash on the private prompt or the skeleton used to surface only
    // after the pipeline file (and any prompt checked before it) had
    // already landed under `.spoolway/`, leaving a half-promoted pipeline
    // behind a refusal and a re-run that could no longer find it privately
    // either. Building the whole plan first, and moving only once every
    // entry in it is known to be clash-free, is what keeps a refusal here
    // exactly as inert as every other refusal in this command.
    let mut plan = Vec::new();

    let ext = source.extension().and_then(|e| e.to_str()).unwrap_or("yml");
    let pipeline_dest = Pipelines::dir_in(&repo.root).join(format!("{name}.{ext}"));
    plan.push(Moved {
        from: source,
        to: pipeline_dest,
        dir: false,
    });

    let mut prompt_names: Vec<&str> = pipeline
        .steps
        .iter()
        .filter(|s| s.kind() == StepKind::Agent)
        .map(|s| s.prompt_name())
        .collect();
    prompt_names.sort_unstable();
    prompt_names.dedup();

    // The subset of `prompt_names` this promote actually moves — the ones
    // with no private directory of their own (a tracked prompt the pipeline
    // already named, nothing to move) never reach `plan`, so they are kept
    // out of this list too: an override waiting on one of those was never
    // waiting on this promote to begin with.
    let mut promoted_prompt_names: Vec<&str> = Vec::new();

    for prompt_name in prompt_names {
        // Read off the pipeline's own parsed steps rather than typed at a
        // prompt, so nothing before this ever checked it — unlike
        // `pipeline_copy`/`prompt_copy`'s own `<from>`/`<to>`, caught before
        // either is ever joined onto a directory. The same refusal, run
        // here before `prompt_name` is joined onto `local/prompts/` at all:
        // a private pipeline naming `prompt: ../../evil` would otherwise
        // move a directory from outside `local/prompts/` straight into (or
        // out of) the tracked control plane.
        crate::local::refuse_unless_plain_name("prompt", prompt_name)?;
        let private_dir = crate::local::prompts_dir(&local).join(prompt_name);
        if !private_dir.is_dir() {
            continue;
        }
        promoted_prompt_names.push(prompt_name);
        // `directory_form`'s own parent, not `repo.prompts_dir().join(…)` —
        // `commands::tests::nothing_builds_a_prompt_path_except_the_one_function_that_should`
        // refuses that shape outright, `crate::prompt::path_for` being the
        // one function meant to join a prompt's directory onto its name.
        let tracked_dir = crate::prompt::directory_form(repo, prompt_name)
            .parent()
            .expect("directory_form always nests PROMPT_FILE under a name directory")
            .to_path_buf();
        if tracked_dir.is_dir() {
            bail!(
                "`{prompt_name}` already exists in the tracked prompts — {} — rename the \
                 private one first",
                tracked_dir.display()
            );
        }
        // Refused here, before anything is moved — `copy_dir_all` cannot
        // copy a symlinked directory correctly (it hands a symlink to
        // `std::fs::copy`, which errors out on one pointing at a
        // directory), and failing mid-copy after the pipeline file above
        // has already landed is exactly the half-promoted state this
        // command exists to avoid. A symlink to a plain file is unaffected
        // — `std::fs::copy` follows and copies those correctly, so it is
        // left to do exactly that, the same as before this check existed.
        if dir_contains_symlinked_directory(&private_dir)
            .with_context(|| format!("checking {} for symlinks", private_dir.display()))?
        {
            bail!(
                "`{prompt_name}` holds a symlinked directory — {} — promote cannot copy it; move \
                 the symlink's target in by hand first",
                private_dir.display()
            );
        }
        plan.push(Moved {
            from: private_dir,
            to: tracked_dir,
            dir: true,
        });
    }

    // The skeleton this pipeline actually names — `task_template:` when it
    // sets one, its own name otherwise, exactly what
    // `crate::task_template::resolve` would read a task's body from — never
    // the hardcoded `<name>.md` a promote used to move regardless of
    // `task_template:`, which left a pipeline naming a differently-named
    // private skeleton with that file still private after promote, and the
    // promoted pipeline now depending on it from across the tracked/private
    // line.
    let skel_name = pipeline.task_template_name();
    // `task_template:` is raw YAML the pipeline parser never validates as a
    // filename — unlike `prompt_name` just above, read off a parsed step
    // rather than typed free text, this is read straight from the
    // pipeline's own frontmatter. Refused here, before it is ever joined
    // onto `local/` or the tracked tree, for the identical reason the
    // prompt loop refuses one above: a private pipeline naming
    // `task_template: ../../../x` must not move an arbitrary `.md` from
    // outside `local/` into, or out of, the tracked control plane.
    crate::local::refuse_unless_plain_name("task template", skel_name)?;
    let skeleton_source = crate::local::task_templates_dir(&local).join(format!("{skel_name}.md"));
    if skeleton_source.is_file() {
        // A skeleton another private pipeline also names is not this
        // promote's alone to move — doing so would silently pull it out
        // from under whichever private pipeline still needs it there.
        if let Some(sharer) = pipelines.pipelines.values().find(|p| {
            p.name != name && p.private_file.is_some() && p.task_template_name() == skel_name
        }) {
            bail!(
                "`{skel_name}` is also the skeleton for the private pipeline `{}` — rename one \
                 of the two before promoting `{name}`",
                sharer.name
            );
        }
        let skeleton_dest = repo.task_templates_dir().join(format!("{skel_name}.md"));
        if skeleton_dest.is_file() {
            bail!(
                "`{skel_name}` already has a tracked skeleton — {} — rename the private one \
                 first",
                skeleton_dest.display()
            );
        }
        plan.push(Moved {
            from: skeleton_source,
            to: skeleton_dest,
            dir: false,
        });
    }

    // Every destination above is now known clash-free, so nothing past this
    // point ever refuses on a name — but the filesystem itself can still
    // fail partway (a read-only tracked prompts directory, a read-only
    // private one), and a promote that fails here must not leave some of
    // the plan tracked and the rest still private. Copy every item first;
    // only once every copy has landed are the private sources deleted.
    for (done, item) in plan.iter().enumerate() {
        if let Err(err) = copy_item(item) {
            // `..=done`, not `..done` — a directory copy that fails partway
            // through (a permission-denied file three entries in, say) has
            // already created the destination and copied whatever came
            // before it, so the failing item's own half-made copy needs
            // undoing too, not just the ones that finished cleanly before
            // it. Every destination here was checked absent while the plan
            // was built, so removing any of them, however much of it
            // exists, only ever gets back to that starting state.
            for item in &plan[..=done] {
                let _ = undo_copy(item);
            }
            return Err(err);
        }
    }

    // Every copy is down. Delete the private sources, in the same order.
    for (done, item) in plan.iter().enumerate() {
        if let Err(err) = delete_source(item) {
            // `delete_source` can fail partway through a directory too
            // (`remove_dir_all` stops at the first entry it cannot remove),
            // so the failing item's own source may now be incomplete —
            // `..=done` restores it, from its still-intact tracked copy,
            // right alongside every delete that had already finished.
            let mut unrestored = Vec::new();
            for item in plan[..=done].iter().rev() {
                if undo_delete(item).is_err() {
                    unrestored.push(item.from.display().to_string());
                }
            }
            if !unrestored.is_empty() {
                // A source could not be put back, and its tracked copy is
                // now the only surviving copy of it — removing that copy
                // too, the way the clean path below does, would lose the
                // data outright. Stop here and say so, rather than finish
                // the rollback and make that worse.
                let paths = unrestored.join(", ");
                bail!(
                    "{err:#}\n\nand restoring what that delete removed also failed for: \
                     {paths} — part of them now exists only in its tracked copy under \
                     `.spoolway`; copy the missing files back by hand before trying `pipeline \
                     promote` again"
                );
            }
            // Every private source is confirmed back in place, so it is now
            // safe to remove every tracked copy this promote made —
            // including the ones past `done` that were copied but never
            // reached the delete loop at all.
            for item in &plan {
                let _ = undo_copy(item);
            }
            return Err(err);
        }
    }

    print_moved(repo, json, &plan)?;

    // Only a tracked pipeline or prompt is ever patched by the override
    // layer — see `merge_private`'s own comment. A patch or fork written
    // while `name` (or one of its prompts) was still private was already
    // named as such — `Ignored::private_pipeline`/`private_prompt`, not
    // silence — by every `override list` and every load since it was
    // written. Promoting is the moment it stops being merely named and
    // starts actually applying, which is worth a line of its own, here,
    // rather than left for the next load to notice as a quiet behaviour
    // change. `json` output stays exactly what `print_moved` wrote above —
    // a second, differently-shaped line appended after it would not parse
    // as the same document.
    if !json {
        let mut starting = Vec::new();
        if crate::overrides::read_pipeline_patch(&repo.overrides_dir(), name)?.is_some() {
            starting.push(
                crate::overrides::pipeline_patch_path(&repo.overrides_dir(), name)
                    .display()
                    .to_string(),
            );
        }
        for prompt_name in &promoted_prompt_names {
            if crate::overrides::prompt_override(&repo.overrides_dir(), prompt_name).is_some() {
                starting.push(
                    crate::overrides::prompt_patch_path(&repo.overrides_dir(), prompt_name)
                        .display()
                        .to_string(),
                );
            }
        }
        if !starting.is_empty() {
            println!();
            for path in &starting {
                println!("  {path} already exists and will start applying now that it is tracked");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch checkout with a repo's config loaded — shared by every
    /// command test below that needs a real `Repo` to run against.
    fn repo_for(name: &str) -> Repo {
        let root = crate::scratch::root(&format!("pipeline-cmd-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        init_at(&root);
        let config = Config::default();
        // A scratch home beside the checkout, the same as every other
        // command test's — nothing here touches the real `~/.spoolway/`.
        let home = root.join(".home");
        Repo {
            checkout: root.clone(),
            root,
            config,
            home,
        }
    }

    /// `init` now claims a name under the real `~/.spoolway/`, so every test
    /// below that calls it for real does so with `$HOME` swapped for a
    /// scratch directory — see `crate::platform::test_home`. Without this a
    /// test run would write into whoever ran it's real home directory, and
    /// could spuriously fail if that machine already has a project by this
    /// scratch root's basename.
    fn init_at(root: &Path) {
        let home = root.parent().unwrap().join(format!(
            "{}-home",
            root.file_name().unwrap().to_string_lossy()
        ));
        crate::platform::test_home::with_home(&home, || {
            init(
                root,
                &InitArgs {
                    // The opening confirmation, answered — a default
                    // `InitArgs` declines it and writes nothing. See
                    // `commands::init`'s own `confirmed` helper.
                    yes: true,
                    ..InitArgs::default()
                },
            )
            .expect("init")
        });
    }

    // -----------------------------------------------------------------
    // `pipeline copy`, `prompt copy` and `pipeline promote`.
    // -----------------------------------------------------------------

    /// `pipeline copy` writes both the pipeline and its resolved task
    /// skeleton under `local/`, byte for byte what the tracked source
    /// holds — the acceptance shape's first sentence.
    #[test]
    fn pipeline_copy_writes_the_pipeline_and_its_skeleton_into_the_private_layer() {
        let repo = repo_for("copy-writes");

        pipeline_copy(&repo, "default", "default-strict", false).expect("copy");

        let tracked_raw =
            std::fs::read_to_string(Pipelines::file_in(&repo.checkout, "default")).unwrap();
        let private_raw = std::fs::read_to_string(
            crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml"),
        )
        .unwrap();
        assert_eq!(private_raw, tracked_raw);

        let skeleton = std::fs::read_to_string(
            crate::local::task_templates_dir(&repo.local_dir()).join("default-strict.md"),
        )
        .unwrap();
        assert_eq!(skeleton, crate::task_template::resolve(&repo, "default"));
    }

    /// `from` naming an explicit `task_template:` used to get copied with a
    /// skeleton written as `<to>.md` — a file the copied YAML, which still
    /// names the original `task_template:`, would never actually read. The
    /// copy now shares the named skeleton and writes none: not refused when
    /// that skeleton exists, private or tracked, and the existing file left
    /// as it was.
    #[test]
    fn pipeline_copy_shares_an_explicit_task_template_it_names() {
        for tracked in [false, true] {
            let repo = repo_for(&format!("copy-explicit-task-template-{tracked}"));

            let default_raw = std::fs::read_to_string(
                crate::pipeline::Pipelines::dir_in(&repo.checkout).join("default.yml"),
            )
            .unwrap();
            std::fs::create_dir_all(crate::local::pipelines_dir(&repo.local_dir())).unwrap();
            std::fs::write(
                crate::local::pipelines_dir(&repo.local_dir()).join("withtemplate.yml"),
                format!("task_template: myskel\n{default_raw}"),
            )
            .unwrap();
            let skel_dir = if tracked {
                repo.task_templates_dir()
            } else {
                crate::local::task_templates_dir(&repo.local_dir())
            };
            std::fs::create_dir_all(&skel_dir).unwrap();
            std::fs::write(skel_dir.join("myskel.md"), "MINE\n").unwrap();

            pipeline_copy(&repo, "withtemplate", "copy1", false).unwrap();

            let copy_raw = std::fs::read_to_string(
                crate::local::pipelines_dir(&repo.local_dir()).join("copy1.yml"),
            )
            .unwrap();
            assert!(copy_raw.contains("task_template: myskel"), "{copy_raw}");
            assert_eq!(
                std::fs::read_to_string(skel_dir.join("myskel.md")).unwrap(),
                "MINE\n",
                "the shared skeleton must be left as it was"
            );
            assert!(
                !crate::local::task_templates_dir(&repo.local_dir())
                    .join("copy1.md")
                    .exists(),
                "nothing must be written under `copy1.md`, which nobody reads"
            );

            let copy1 = Pipeline::parse("copy1", &copy_raw).unwrap();
            assert_eq!(crate::task_template::resolve_for(&repo, &copy1), "MINE\n");
        }
    }

    /// Both spellings of a clash are refused: a `<to>` that already names a
    /// tracked pipeline, and one that already names a private one.
    #[test]
    fn pipeline_copy_refuses_a_to_that_already_exists_tracked_or_private() {
        let repo = repo_for("copy-clash");

        let err = pipeline_copy(&repo, "default", "default", false).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");

        pipeline_copy(&repo, "default", "default-strict", false).unwrap();
        let err = pipeline_copy(&repo, "default", "default-strict", false).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    }

    /// `refuse_pipeline_clash` only checks for a `<to>` that already names a
    /// pipeline — never for a `<to>` whose skeleton already exists on its
    /// own, private or tracked. `pipeline copy` must refuse that too, rather
    /// than silently overwriting a skeleton nobody asked to replace. See the
    /// task's "How to see it": `echo MINE > local/templates/tasks/foo.md;
    /// spoolway pipeline copy default foo` currently replaces the file with
    /// no warning.
    // covers: pipeline copy refuses when the destination skeleton already exists, private or tracked
    #[test]
    fn pipeline_copy_refuses_when_the_destination_skeleton_already_exists() {
        let repo = repo_for("copy-skeleton-clash");

        let private_skeleton = crate::local::task_templates_dir(&repo.local_dir()).join("foo.md");
        std::fs::create_dir_all(private_skeleton.parent().unwrap()).unwrap();
        std::fs::write(&private_skeleton, "MINE\n").unwrap();

        let err = pipeline_copy(&repo, "default", "foo", false).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");

        assert_eq!(
            std::fs::read_to_string(&private_skeleton).unwrap(),
            "MINE\n",
            "pipeline copy must not overwrite an existing private skeleton"
        );
    }

    /// A private pipeline may itself be `from` — copying a copy.
    #[test]
    fn pipeline_copy_reads_from_an_already_private_pipeline() {
        let repo = repo_for("copy-from-private");
        pipeline_copy(&repo, "default", "default-strict", false).unwrap();

        pipeline_copy(&repo, "default-strict", "default-stricter", false).unwrap();

        assert!(
            crate::local::pipelines_dir(&repo.local_dir())
                .join("default-stricter.yml")
                .is_file()
        );
    }

    /// `pipeline copy` on a name nothing has is refused by name.
    #[test]
    fn pipeline_copy_refuses_an_unknown_from() {
        let repo = repo_for("copy-unknown");
        let err = pipeline_copy(&repo, "nosuchpipeline", "x", false).unwrap_err();
        assert!(format!("{err:#}").contains("no pipeline named"), "{err:#}");
    }

    /// `prompt copy` writes the tracked prompt's own body under
    /// `local/prompts/<to>/PROMPT.md`.
    #[test]
    fn prompt_copy_writes_the_prompt_into_the_private_layer() {
        let repo = repo_for("prompt-copy-writes");

        prompt_copy(&repo, "implementer", "implementer-strict", false).expect("copy");

        let tracked =
            std::fs::read_to_string(crate::prompt::path_for_tracked(&repo, "implementer")).unwrap();
        let private = std::fs::read_to_string(
            crate::local::prompts_dir(&repo.local_dir())
                .join("implementer-strict")
                .join(crate::assets::PROMPT_FILE),
        )
        .unwrap();
        assert_eq!(private, tracked);
    }

    /// The same clash refusal `pipeline copy` gives, on the prompt side.
    #[test]
    fn prompt_copy_refuses_a_to_that_already_exists_tracked_or_private() {
        let repo = repo_for("prompt-copy-clash");

        let err = prompt_copy(&repo, "implementer", "implementer", false).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");

        prompt_copy(&repo, "implementer", "implementer-strict", false).unwrap();
        let err = prompt_copy(&repo, "implementer", "implementer-strict", false).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    }

    /// A `<to>` that is empty, absolute, or holds `/`, `\` or `..` must be
    /// refused — not joined onto the private directory, which is what lets
    /// it write outside `local/`. See the task's "How to see it": `..` walks
    /// out to `$HOME`, an absolute path writes wherever it points, `""`
    /// writes `local/pipelines/.yml`, and `a/b` writes a nested file the
    /// loader never reads.
    #[test]
    fn pipeline_copy_refuses_a_to_that_is_not_one_plain_name() {
        let repo = repo_for("copy-bad-to");
        let pipelines_dir = crate::local::pipelines_dir(&repo.local_dir());
        let templates_dir = crate::local::task_templates_dir(&repo.local_dir());

        for bad in [
            "",
            "/etc/passwd",
            "a/b",
            "..",
            "../../../../escaped",
            "a\\b",
        ] {
            let err = pipeline_copy(&repo, "default", bad, false)
                .expect_err(&format!("`{bad}` should have been refused"));
            assert!(
                format!("{err:#}").to_lowercase().contains("name"),
                "refusal for `{bad}` should name the problem: {err:#}"
            );
        }

        // Nothing was written anywhere a bad `<to>` could have reached —
        // not even inside the private directory itself.
        assert!(
            !pipelines_dir.exists() || std::fs::read_dir(&pipelines_dir).unwrap().next().is_none()
        );
        assert!(
            !templates_dir.exists() || std::fs::read_dir(&templates_dir).unwrap().next().is_none()
        );
        assert!(!repo.home.join("escaped.yml").exists());
        assert!(!PathBuf::from("/etc/passwd.yml").exists());
    }

    /// The same refusal, on the prompt side — a bad `<to>` must not reach
    /// `prompt_copy_dest`, which `repo_for` runs in repo mode, where it
    /// joins `<to>` straight onto `local/prompts/`. `prompt_copy`'s own
    /// `refuse_unless_plain_name` call runs before `prompt_copy_dest` is
    /// ever reached at all, so the same refusal holds in home mode too,
    /// where that function instead joins onto the tracked directory shape
    /// through `crate::prompt::directory_form` — untested here, since
    /// nothing about the join itself differs by mode.
    #[test]
    fn prompt_copy_refuses_a_to_that_is_not_one_plain_name() {
        let repo = repo_for("prompt-copy-bad-to");
        let prompts_dir = crate::local::prompts_dir(&repo.local_dir());

        for bad in [
            "",
            "/etc/passwd",
            "nest/inner",
            "..",
            "../../../../../evilp",
            "a\\b",
        ] {
            let err = prompt_copy(&repo, "implementer", bad, false)
                .expect_err(&format!("`{bad}` should have been refused"));
            assert!(
                format!("{err:#}").to_lowercase().contains("name"),
                "refusal for `{bad}` should name the problem: {err:#}"
            );
        }

        assert!(!prompts_dir.exists() || std::fs::read_dir(&prompts_dir).unwrap().next().is_none());
    }

    /// The end-to-end shape the task's own acceptance criteria name: copy a
    /// pipeline and a prompt, point the copy at the private prompt, promote
    /// it, and find the pipeline, the prompt and the skeleton under
    /// `.spoolway/` with `local/` left empty of all three.
    ///
    /// `pipeline_promote` reads the private pipeline back through
    /// `Pipelines::load`, which resolves the project's home path-based
    /// (`crate::local::dir_for`, through `crate::mux::project_home`) rather
    /// than off this test's own `Repo::home` field — so the whole body runs
    /// inside the one `with_home` that both `repo_for`'s `init` and that
    /// resolution have to agree on, unlike every test above that only ever
    /// writes and reads through `repo.local_dir()` itself.
    #[test]
    fn pipeline_promote_moves_the_pipeline_its_private_prompt_and_its_skeleton() {
        let (repo, home) = repo_for_home_aware("promote-e2e");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            prompt_copy(&repo, "implementer", "implementer-strict", false).unwrap();

            // Point the private pipeline's `implement` step at the private
            // prompt, the way a person editing it after `pipeline copy`
            // would.
            let pipeline_path =
                crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            let raw = raw.replace("prompt: implementer", "prompt: implementer-strict");
            // Guard against `default.yml`'s own shape drifting out from
            // under this rewrite and silently promoting the untouched
            // tracked prompt.
            assert!(raw.contains("implementer-strict"), "{raw}");
            std::fs::write(&pipeline_path, raw).unwrap();

            pipeline_promote(&repo, "default-strict", false).expect("promote");

            assert!(
                Pipelines::file_in(&repo.root, "default-strict").is_file(),
                "the pipeline must land in the tracked directory"
            );
            assert!(
                crate::prompt::path_for_tracked(&repo, "implementer-strict").is_file(),
                "the private prompt it names must move too"
            );
            assert!(
                repo.task_templates_dir()
                    .join("default-strict.md")
                    .is_file(),
                "its own skeleton must move too"
            );

            assert!(
                !crate::local::pipelines_dir(&repo.local_dir())
                    .join("default-strict.yml")
                    .exists(),
                "the private pipeline file must be gone"
            );
            assert!(
                !crate::local::prompts_dir(&repo.local_dir())
                    .join("implementer-strict")
                    .exists(),
                "the private prompt directory must be gone"
            );
            assert!(
                !crate::local::task_templates_dir(&repo.local_dir())
                    .join("default-strict.md")
                    .exists(),
                "the private skeleton must be gone"
            );

            // Nothing else was ever written under `local/` by this test, so
            // an empty (or absent) directory here is the whole layer gone,
            // not just these three files.
            let remaining: Vec<_> = walk_files(&repo.local_dir());
            assert!(remaining.is_empty(), "{remaining:?}");
        });
    }

    /// Every file still under `dir`, recursively — for asserting an
    /// emptied-out private layer without caring how many nested directories
    /// `pipeline promote` left behind.
    fn walk_files(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk_files(&path));
            } else {
                found.push(path);
            }
        }
        found
    }

    /// A promote that fails partway through must undo everything it already
    /// moved, rather than leave the project with some of a pipeline tracked
    /// and the rest still private.
    ///
    /// Here the pipeline file itself is the one item in the plan whose copy
    /// finishes before the failure: `pipeline_promote`'s copy loop copies
    /// the pipeline file tracked first, then reaches the private prompt it
    /// names. Making the tracked prompts directory read-only — the same "a
    /// read-only `.spoolway/prompts`" shape the task's own "how to see it"
    /// names — makes that prompt's own `copy_dir_all` fail at its
    /// `create_dir_all` outright, before either private source has been
    /// deleted.
    ///
    /// A correct promote leaves every file exactly where it was before it
    /// ran once any part of it fails: the pipeline file back under
    /// `local/`, nothing of the prompt ever landing tracked, every pipeline
    /// still loadable, and the same `pipeline promote` runnable again
    /// afterward. `pipeline_promote`'s copy loop undoes exactly that —
    /// removing the pipeline file's already-made tracked copy, since
    /// nothing was ever deleted privately to put back — once the prompt's
    /// own copy fails.
    #[test]
    fn a_promote_that_fails_partway_leaves_every_file_exactly_where_it_was() {
        use std::os::unix::fs::PermissionsExt;

        let (repo, home) = repo_for_home_aware("promote-atomic");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            prompt_copy(&repo, "implementer", "worker", false).unwrap();

            let pipeline_path =
                crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            let raw = raw.replace("prompt: implementer", "prompt: worker");
            assert!(raw.contains("prompt: worker"), "{raw}");
            std::fs::write(&pipeline_path, &raw).unwrap();

            let tracked_prompts_dir = repo.prompts_dir();
            let mut perms = std::fs::metadata(&tracked_prompts_dir)
                .unwrap()
                .permissions();
            let writable = perms.clone();
            perms.set_mode(0o555);
            std::fs::set_permissions(&tracked_prompts_dir, perms).unwrap();

            let err = pipeline_promote(&repo, "default-strict", false);

            // Restored before any assertion, so a failed one does not leave
            // a directory this test's own cleanup cannot remove.
            std::fs::set_permissions(&tracked_prompts_dir, writable).unwrap();

            let err = err.expect_err("the read-only tracked prompts directory must fail the move");
            assert!(
                format!("{err:#}").contains("worker"),
                "the failure should be the prompt move: {err:#}"
            );

            // The pipeline file must be back where it was, not left tracked
            // with nothing having caught up to it.
            assert!(
                !Pipelines::file_in(&repo.root, "default-strict").is_file(),
                "a failed promote must undo a part that already finished — the pipeline file \
                 must not be left tracked"
            );
            assert!(
                crate::local::pipelines_dir(&repo.local_dir())
                    .join("default-strict.yml")
                    .is_file(),
                "the private pipeline file must be restored so a retry has something to work \
                 from"
            );

            // Nothing of the prompt should have landed tracked, and the
            // private copy must still be there.
            assert!(
                !crate::prompt::directory_form(&repo, "worker").is_file(),
                "the prompt move itself never finished, so nothing should be tracked"
            );
            assert!(
                crate::local::prompts_dir(&repo.local_dir())
                    .join("worker")
                    .join(crate::assets::PROMPT_FILE)
                    .is_file(),
                "the private prompt must still be there"
            );

            // The project must still be able to load every pipeline.
            Pipelines::load(&repo.checkout, &repo.config)
                .expect("a failed promote must leave the project loadable");

            // And the same promote must be runnable again.
            pipeline_promote(&repo, "default-strict", false)
                .expect("a failed promote must leave something to retry from");
        });
    }

    /// A copy that fails partway *through* a directory must undo the
    /// partial copy it already made, not just the copies that finished
    /// before it.
    ///
    /// `worker` holds a file with no read permission, so `copy_dir_all`
    /// creates `worker`'s tracked directory, copies `PROMPT.md` into it,
    /// then fails reading `blocked.txt` — `worker`'s own destination is
    /// left half-populated rather than merely absent, unlike the
    /// read-only-tracked-prompts-dir case above where `create_dir_all`
    /// itself never gets anywhere. A correct promote removes that partial
    /// directory along with the pipeline file's own already-made copy,
    /// leaving nothing tracked and every private file untouched — nothing
    /// was ever deleted, since the copy loop runs in full before any
    /// delete does.
    #[test]
    fn a_promote_removes_its_own_partial_copy_when_a_directory_copy_fails_midway() {
        use std::os::unix::fs::PermissionsExt;

        let (repo, home) = repo_for_home_aware("promote-copy-rollback");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            prompt_copy(&repo, "implementer", "worker", false).unwrap();

            let pipeline_path =
                crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            let raw = raw.replace("prompt: implementer", "prompt: worker");
            assert!(raw.contains("prompt: worker"), "{raw}");
            std::fs::write(&pipeline_path, &raw).unwrap();

            let worker_dir = crate::local::prompts_dir(&repo.local_dir()).join("worker");
            let blocked = worker_dir.join("blocked.txt");
            std::fs::write(&blocked, b"private data").unwrap();
            let mut perms = std::fs::metadata(&blocked).unwrap().permissions();
            let writable = perms.clone();
            perms.set_mode(0o000);
            std::fs::set_permissions(&blocked, perms).unwrap();

            let err = pipeline_promote(&repo, "default-strict", false);

            std::fs::set_permissions(&blocked, writable).unwrap();

            let err = err.expect_err("the unreadable file must fail the directory copy");
            assert!(format!("{err:#}").contains("blocked.txt"), "{err:#}");

            let tracked_worker_dir = crate::prompt::directory_form(&repo, "worker")
                .parent()
                .unwrap()
                .to_path_buf();
            assert!(
                !tracked_worker_dir.exists(),
                "the partial tracked copy must be removed entirely, not left half-populated: {}",
                tracked_worker_dir.display()
            );
            assert!(!Pipelines::file_in(&repo.root, "default-strict").is_file());

            assert!(
                crate::local::pipelines_dir(&repo.local_dir())
                    .join("default-strict.yml")
                    .is_file()
            );
            assert!(worker_dir.join(crate::assets::PROMPT_FILE).is_file());
            assert!(blocked.is_file(), "nothing private was ever touched");

            pipeline_promote(&repo, "default-strict", false)
                .expect("a failed promote must leave something to retry from");
        });
    }

    /// A delete failing *after* an earlier delete in the same promote has
    /// already succeeded must undo both — not just the one that failed.
    ///
    /// Two prompts, `aaa` and `zzz`, sort in that order, so `aaa`'s delete
    /// runs and finishes before `zzz`'s is ever attempted. `zzz` holds a
    /// subdirectory made read-only, so `remove_dir_all` on it fails partway
    /// through — a real file inside is left behind. (A read-only
    /// `local/pipelines`, where the delete removes nothing at all, is a
    /// different case — see
    /// `a_promote_whose_first_delete_removes_nothing_rolls_back_cleanly`.)
    /// A correct promote restores `zzz`'s own source from its
    /// tracked copy, restores `aaa`'s too even though its own delete never
    /// failed, and then removes every tracked copy, leaving nothing
    /// promoted and every private file back.
    #[test]
    fn a_promote_undoes_an_earlier_delete_when_a_later_one_fails() {
        use std::os::unix::fs::PermissionsExt;

        let (repo, home) = repo_for_home_aware("promote-delete-rollback");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            prompt_copy(&repo, "implementer", "aaa", false).unwrap();
            prompt_copy(&repo, "reviewer", "zzz", false).unwrap();

            let pipeline_path =
                crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            let raw = raw
                .replace("prompt: implementer", "prompt: aaa")
                .replace("prompt: reviewer", "prompt: zzz");
            assert!(
                raw.contains("prompt: aaa") && raw.contains("prompt: zzz"),
                "{raw}"
            );
            std::fs::write(&pipeline_path, &raw).unwrap();

            let zzz_dir = crate::local::prompts_dir(&repo.local_dir()).join("zzz");
            let locked_dir = zzz_dir.join("locked");
            std::fs::create_dir_all(&locked_dir).unwrap();
            std::fs::write(locked_dir.join("inside.txt"), b"private data").unwrap();
            let mut locked_perms = std::fs::metadata(&locked_dir).unwrap().permissions();
            let locked_writable = locked_perms.clone();
            locked_perms.set_mode(0o555);
            std::fs::set_permissions(&locked_dir, locked_perms).unwrap();

            let err = pipeline_promote(&repo, "default-strict", false);

            // Restored before any assertion, same as the read-only-tracked-
            // prompts-dir test above — a failed assertion must not leave a
            // directory this test's own cleanup cannot remove.
            std::fs::set_permissions(&locked_dir, locked_writable).unwrap();

            let err = err.expect_err("the undeletable `locked` subdirectory must fail the move");
            assert!(format!("{err:#}").contains("zzz"), "{err:#}");

            // Nothing is left tracked — not the pipeline, not either prompt.
            assert!(!Pipelines::file_in(&repo.root, "default-strict").is_file());
            assert!(!crate::prompt::directory_form(&repo, "aaa").is_file());
            assert!(!crate::prompt::directory_form(&repo, "zzz").is_file());

            // Every private file is back — the pipeline, `aaa` (whose own
            // delete had already finished), and `zzz` (whose delete failed
            // partway through), including the file under the read-only
            // subdirectory.
            assert!(
                crate::local::pipelines_dir(&repo.local_dir())
                    .join("default-strict.yml")
                    .is_file()
            );
            assert!(
                crate::local::prompts_dir(&repo.local_dir())
                    .join("aaa")
                    .join(crate::assets::PROMPT_FILE)
                    .is_file(),
                "aaa's already-finished delete must be undone too"
            );
            assert!(
                locked_dir.join("inside.txt").is_file(),
                "zzz's own partially-deleted source must be fully restored"
            );

            Pipelines::load(&repo.checkout, &repo.config)
                .expect("a failed promote must leave the project loadable");

            pipeline_promote(&repo, "default-strict", false)
                .expect("a failed promote must leave something to retry from");
        });
    }

    /// A read-only `local/pipelines` — the task's own "the delete fails
    /// after the copy" case — rolls back to nothing tracked.
    ///
    /// The pipeline file is the first item deleted, and `remove_file` fails
    /// on it without removing anything. Nothing private is missing, so the
    /// rollback must not try to write the file back into that read-only
    /// folder. It used to, failed, and stopped with the pipeline and its
    /// prompt both tracked and private, so every command refused to load.
    #[test]
    fn a_promote_whose_first_delete_removes_nothing_rolls_back_cleanly() {
        use std::os::unix::fs::PermissionsExt;

        let (repo, home) = repo_for_home_aware("promote-readonly-private");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            prompt_copy(&repo, "implementer", "worker", false).unwrap();

            let private_pipelines = crate::local::pipelines_dir(&repo.local_dir());
            let pipeline_path = private_pipelines.join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            let raw = raw.replace("prompt: implementer", "prompt: worker");
            assert!(raw.contains("prompt: worker"), "{raw}");
            std::fs::write(&pipeline_path, &raw).unwrap();

            let mut perms = std::fs::metadata(&private_pipelines).unwrap().permissions();
            let writable = perms.clone();
            perms.set_mode(0o555);
            std::fs::set_permissions(&private_pipelines, perms).unwrap();

            let err = pipeline_promote(&repo, "default-strict", false);

            // Restored before any assertion, same as the tests above.
            std::fs::set_permissions(&private_pipelines, writable).unwrap();

            let err = err.expect_err("the read-only private pipelines folder must fail the move");
            assert!(format!("{err:#}").contains("default-strict.yml"), "{err:#}");
            assert!(
                !format!("{err:#}").contains("restoring"),
                "nothing was removed, so nothing needed restoring: {err:#}"
            );

            assert!(!Pipelines::file_in(&repo.root, "default-strict").is_file());
            assert!(!crate::prompt::directory_form(&repo, "worker").is_file());
            assert!(pipeline_path.is_file());
            assert!(
                crate::local::prompts_dir(&repo.local_dir())
                    .join("worker")
                    .join(crate::assets::PROMPT_FILE)
                    .is_file()
            );

            Pipelines::load(&repo.checkout, &repo.config)
                .expect("a failed promote must leave the project loadable");

            pipeline_promote(&repo, "default-strict", false)
                .expect("a failed promote must leave something to retry from");
        });
    }

    /// A private prompt folder holding a symlinked directory is refused
    /// before `pipeline_promote` moves anything — `copy_dir_all` cannot
    /// copy one correctly (`std::fs::copy` errors out on a symlink that
    /// points at a directory), so the whole promote must bail out while the
    /// pipeline file is still private, rather than land it tracked and then
    /// fail on the prompt that names it.
    #[test]
    fn pipeline_promote_refuses_a_prompt_holding_a_symlinked_directory() {
        let (repo, home) = repo_for_home_aware("promote-symlink");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            prompt_copy(&repo, "implementer", "worker", false).unwrap();

            let pipeline_path =
                crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            let raw = raw.replace("prompt: implementer", "prompt: worker");
            assert!(raw.contains("prompt: worker"), "{raw}");
            std::fs::write(&pipeline_path, &raw).unwrap();

            let worker_dir = crate::local::prompts_dir(&repo.local_dir()).join("worker");
            let target_dir = worker_dir.parent().unwrap().join("elsewhere");
            std::fs::create_dir_all(&target_dir).unwrap();
            std::os::unix::fs::symlink(&target_dir, worker_dir.join("assets")).unwrap();

            let err = pipeline_promote(&repo, "default-strict", false)
                .expect_err("a symlinked directory inside the prompt must be refused");
            assert!(format!("{err:#}").contains("worker"), "{err:#}");

            // Refused before anything moved — the pipeline file must still
            // be private, not land tracked ahead of the prompt's own
            // refusal.
            assert!(
                !Pipelines::file_in(&repo.root, "default-strict").is_file(),
                "nothing should have moved yet"
            );
            assert!(
                crate::local::pipelines_dir(&repo.local_dir())
                    .join("default-strict.yml")
                    .is_file()
            );
        });
    }

    /// `pipeline promote` on a name with no private pipeline is refused by
    /// name.
    #[test]
    fn pipeline_promote_refuses_an_unknown_name() {
        let repo = repo_for("promote-unknown");
        let err = pipeline_promote(&repo, "nosuchpipeline", false).unwrap_err();
        assert!(
            format!("{err:#}").contains("no private pipeline named"),
            "{err:#}"
        );
    }

    /// A private pipeline whose name already belongs to a tracked one is
    /// refused rather than overwriting it — `pipeline copy` already
    /// prevents this from `copy`'s own side, but `promote` checks again in
    /// case the tracked file arrived afterwards.
    #[test]
    fn pipeline_promote_refuses_a_clash_with_a_tracked_pipeline() {
        let repo = repo_for("promote-tracked-clash");
        pipeline_copy(&repo, "default", "default-strict", false).unwrap();
        // Land a tracked pipeline by this name after the private copy was
        // made, so `pipeline copy` itself never got a chance to refuse it.
        std::fs::write(
            Pipelines::file_in(&repo.root, "default-strict"),
            "steps:\n  - id: solo\n    agent: pi\n    model: m\n    on_pass: done\n",
        )
        .unwrap();

        let err = pipeline_promote(&repo, "default-strict", false).unwrap_err();
        assert!(format!("{err:#}").contains("already exists"), "{err:#}");
    }

    /// A private pipeline's own `prompt:` is never typed through
    /// `pipeline_copy`'s or `prompt_copy`'s `<from>`/`<to>`, so nothing
    /// catches it earlier — a name like `../evil` walking outside
    /// `local/prompts/` must still be refused the moment `pipeline_promote`
    /// goes to resolve it privately, before anything is moved.
    ///
    /// `pipeline_promote` reads the private pipeline back through
    /// `Pipelines::load`, path-based, so this needs the same `with_home`
    /// `repo_for_home_aware` sets up for the clash tests below.
    #[test]
    fn pipeline_promote_refuses_a_pipeline_naming_a_non_plain_prompt() {
        let (repo, home) = repo_for_home_aware("promote-bad-prompt-name");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            let pipeline_path =
                crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            let raw = raw.replace("prompt: implementer", "prompt: ../evil");
            assert!(raw.contains("../evil"), "{raw}");
            std::fs::write(&pipeline_path, &raw).unwrap();

            let err = pipeline_promote(&repo, "default-strict", false).unwrap_err();
            assert!(
                format!("{err:#}").contains("is not a valid prompt name"),
                "{err:#}"
            );

            assert!(
                !Pipelines::file_in(&repo.root, "default-strict").is_file(),
                "a refused prompt name must leave the pipeline file exactly where it was"
            );
            assert!(
                pipeline_path.is_file(),
                "the private pipeline file must still be there to retry from"
            );
            assert_eq!(
                std::fs::read_to_string(&pipeline_path).unwrap(),
                raw,
                "nothing in the private pipeline itself was touched"
            );
        });
    }

    /// `task_template:` is raw YAML the pipeline parser never validates as
    /// a filename, and `pipeline_promote` joins it straight onto both
    /// `local/templates/tasks/` and the tracked templates directory. A
    /// private pipeline naming `task_template: ../../../evil` must be
    /// refused the same way a non-plain `prompt:` already is, before
    /// either join ever happens — see `refuse_unless_plain_name`.
    // covers: standards — path traversal through an unvalidated task_template:
    #[test]
    fn pipeline_promote_refuses_a_pipeline_naming_a_non_plain_task_template() {
        let (repo, home) = repo_for_home_aware("promote-bad-task-template-name");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            let pipeline_path =
                crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            let raw = format!("task_template: ../../../evil\n{raw}");
            std::fs::write(&pipeline_path, &raw).unwrap();

            let err = pipeline_promote(&repo, "default-strict", false).unwrap_err();
            assert!(
                format!("{err:#}").contains("is not a valid task template name"),
                "{err:#}"
            );

            assert!(
                !Pipelines::file_in(&repo.root, "default-strict").is_file(),
                "a refused task_template name must leave the pipeline file exactly where it was"
            );
            assert!(
                pipeline_path.is_file(),
                "the private pipeline file must still be there to retry from"
            );
        });
    }

    /// A clash found only on the *last* destination `pipeline_promote`
    /// checks — the skeleton, after the pipeline itself (and any private
    /// prompt) would already have passed their own checks — must still
    /// refuse before anything is moved. `default`'s own `implement` step
    /// names the tracked prompt `implementer`, so nothing here ever reaches
    /// the prompt loop at all: the whole plan is just the pipeline and its
    /// skeleton, and the skeleton is the one entry planted to clash.
    ///
    /// Like the end-to-end test above, `pipeline_promote` reads the private
    /// pipeline back through `Pipelines::load`, path-based, so the whole
    /// body has to run inside the one `with_home` `repo_for_home_aware`
    /// and that resolution agree on.
    #[test]
    fn pipeline_promote_refuses_a_skeleton_clash_without_moving_anything_first() {
        let (repo, home) = repo_for_home_aware("promote-skeleton-clash");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            // Land a tracked skeleton by this name after the private copy
            // was made — the pipeline file itself has no tracked
            // counterpart, so only this last check in the plan ever fires.
            std::fs::write(
                repo.task_templates_dir().join("default-strict.md"),
                "the tracked skeleton\n",
            )
            .unwrap();

            let err = pipeline_promote(&repo, "default-strict", false).unwrap_err();
            assert!(
                format!("{err:#}").contains("already has a tracked skeleton"),
                "{err:#}"
            );

            assert!(
                !Pipelines::file_in(&repo.root, "default-strict").exists(),
                "a later clash must leave the pipeline file exactly where it was, not \
                 half-moved"
            );
            assert!(
                crate::local::pipelines_dir(&repo.local_dir())
                    .join("default-strict.yml")
                    .is_file(),
                "the private pipeline file must still be there to retry from"
            );
            assert_eq!(
                std::fs::read_to_string(repo.task_templates_dir().join("default-strict.md"))
                    .unwrap(),
                "the tracked skeleton\n",
                "the tracked skeleton that caused the refusal must be untouched"
            );
        });
    }

    /// `pipeline_promote` used to move `<name>.md` regardless of what the
    /// pipeline's own `task_template:` named, which left a pipeline naming
    /// a differently-named private skeleton still depending on it privately
    /// after promote. It must move the skeleton the pipeline actually
    /// names instead.
    // covers: promote moves the skeleton the pipeline actually names
    #[test]
    fn pipeline_promote_moves_the_skeleton_task_template_names_not_the_pipelines_own_name() {
        let (repo, home) = repo_for_home_aware("promote-task-template-name");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();

            let pipeline_path =
                crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            std::fs::write(&pipeline_path, format!("task_template: myskel\n{raw}")).unwrap();

            let skeleton_dir = crate::local::task_templates_dir(&repo.local_dir());
            std::fs::rename(
                skeleton_dir.join("default-strict.md"),
                skeleton_dir.join("myskel.md"),
            )
            .unwrap();

            pipeline_promote(&repo, "default-strict", false).expect("promote");

            assert!(
                repo.task_templates_dir().join("myskel.md").is_file(),
                "the skeleton `task_template:` names must move, not `default-strict.md`"
            );
            assert!(
                !repo.task_templates_dir().join("default-strict.md").exists(),
                "nothing must be written under the pipeline's own name instead"
            );
            assert!(
                !skeleton_dir.join("myskel.md").exists(),
                "the private skeleton must be gone"
            );
        });
    }

    /// A skeleton another private pipeline also names is not this promote's
    /// alone to move — doing so would silently pull it out from under the
    /// private pipeline still depending on it.
    // covers: promote refuses when that skeleton is shared
    #[test]
    fn pipeline_promote_refuses_a_skeleton_shared_with_another_private_pipeline() {
        let (repo, home) = repo_for_home_aware("promote-skeleton-shared");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            pipeline_copy(&repo, "default", "default-stricter", false).unwrap();

            let local = repo.local_dir();
            let skeleton_dir = crate::local::task_templates_dir(&local);
            std::fs::remove_file(skeleton_dir.join("default-stricter.md")).unwrap();
            std::fs::rename(
                skeleton_dir.join("default-strict.md"),
                skeleton_dir.join("myskel.md"),
            )
            .unwrap();

            for to in ["default-strict", "default-stricter"] {
                let pipeline_path = crate::local::pipelines_dir(&local).join(format!("{to}.yml"));
                let raw = std::fs::read_to_string(&pipeline_path).unwrap();
                std::fs::write(&pipeline_path, format!("task_template: myskel\n{raw}")).unwrap();
            }

            let err = pipeline_promote(&repo, "default-strict", false).unwrap_err();
            assert!(format!("{err:#}").contains("also the skeleton"), "{err:#}");
            assert!(
                !Pipelines::file_in(&repo.root, "default-strict").exists(),
                "the refusal must leave nothing moved"
            );
            assert!(
                skeleton_dir.join("myskel.md").is_file(),
                "the shared skeleton must still be private"
            );
        });
    }

    /// The same refusal `override promote` gives from a linked worktree,
    /// for the identical reason: the dispatcher reads `repo.root`'s tracked
    /// files, never a worktree's own copy.
    #[test]
    fn pipeline_promote_refuses_from_a_linked_worktree() {
        let repo = repo_for("promote-worktree");
        pipeline_copy(&repo, "default", "default-strict", false).unwrap();

        let mut worktree = repo.clone();
        worktree.checkout = repo.root.join("elsewhere");

        let err = pipeline_promote(&worktree, "default-strict", false).unwrap_err();
        assert!(err.to_string().contains("this worktree's"), "{err}");
    }

    /// Home mode has no private layer of its own: `pipeline copy` writes
    /// straight into the workspace's `config/`, and `pipeline promote`
    /// refuses outright, saying why.
    #[test]
    fn home_mode_writes_copies_straight_in_and_refuses_to_promote() {
        let root = crate::scratch::root("pipeline-cmd-home-mode");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        let fake_home = root.parent().unwrap().join(format!(
            "{}-realhome",
            root.file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_dir_all(&fake_home);

        crate::platform::test_home::with_home(&fake_home, || {
            let clone = crate::repo::create_workspace(&root).unwrap();
            let pipelines_dir = clone.config_dir().join("pipelines");
            std::fs::create_dir_all(&pipelines_dir).unwrap();
            std::fs::write(
                pipelines_dir.join("default.yml"),
                "steps:\n  - id: solo\n    agent: pi\n    model: m\n    on_pass: done\n",
            )
            .unwrap();

            let home = crate::mux::project_home(&root).unwrap();
            let repo = Repo {
                checkout: root.clone(),
                root: root.clone(),
                config: Config::default(),
                home,
            };

            pipeline_copy(&repo, "default", "default-strict", false).expect("copy");
            assert!(
                pipelines_dir.join("default-strict.yml").is_file(),
                "home mode writes straight into the workspace's config/"
            );
            assert!(
                !repo.local_dir().exists(),
                "home mode has no private layer to write into"
            );

            let err = pipeline_promote(&repo, "default-strict", false).unwrap_err();
            assert!(format!("{err:#}").contains("already private"), "{err:#}");
        });

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&fake_home).ok();
    }

    /// [`repo_for`], but with `Repo::home` resolved the real way — through
    /// `crate::mux::project_home`, inside the scratch `$HOME` `init` just
    /// stamped the project under — and that `$HOME` handed back alongside
    /// it. For a test that goes on to call something, like
    /// `pipeline_promote`, that resolves the project's home itself,
    /// path-based, rather than asking this `Repo`'s own field: the caller
    /// must re-enter the same `crate::platform::test_home::with_home` for
    /// the two resolutions to agree, since [`repo_for`]'s own placeholder
    /// `root.join(".home")` never would.
    fn repo_for_home_aware(name: &str) -> (Repo, PathBuf) {
        let root = crate::scratch::root(&format!("pipeline-cmd-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        let home = root.parent().unwrap().join(format!(
            "{}-home",
            root.file_name().unwrap().to_string_lossy()
        ));
        let repo = crate::platform::test_home::with_home(&home, || {
            init(
                &root,
                &InitArgs {
                    yes: true,
                    ..InitArgs::default()
                },
            )
            .expect("init");
            Repo {
                checkout: root.clone(),
                root: root.clone(),
                config: Config::default(),
                home: crate::mux::project_home(&root).unwrap(),
            }
        });
        (repo, home)
    }

    /// Every pipeline's name and description are printed, and nothing more —
    /// nothing here should ever open a step, an agent or a prompt the way
    /// `pipeline show` does.
    #[test]
    fn pipeline_list_prints_every_pipeline_and_succeeds() {
        let repo = repo_for("list");
        let pipelines = Pipelines::builtin();
        assert!(!pipelines.pipelines.is_empty(), "nothing to list against");
        pipeline_list(&repo, &pipelines, false).expect("listing names alone cannot fail");
    }

    /// `--json pipeline list` carries, per pipeline, its name and
    /// description — the same facts the prose form prints, as fields a
    /// script can read without parsing English — and no default marker,
    /// since spoolway routes on nothing but each task's own `pipeline:`.
    #[test]
    fn build_list_carries_name_and_description_per_pipeline_with_no_default() {
        let pipelines = Pipelines::builtin();
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&build_list(&pipelines)).unwrap()).unwrap();
        assert!(value.get("default").is_none(), "{value}");
        let entries = value["pipelines"].as_array().expect("a pipelines array");
        assert_eq!(entries.len(), pipelines.pipelines.len());
        for entry in entries {
            assert!(entry.get("name").is_some(), "missing `name`: {entry}");
            assert!(
                entry.get("default").is_none(),
                "unexpected `default`: {entry}"
            );
            assert!(
                entry.get("description").is_some(),
                "missing `description`: {entry}"
            );
        }
    }

    /// `--json pipeline list` carries a private pipeline's file under
    /// `source`/`file`, and a tracked one's `source: "tracked"` with no
    /// `file` — acceptance criterion 4's json half.
    #[test]
    fn build_list_carries_source_and_file_for_a_private_pipeline() {
        let tracked = Pipeline::parse(
            "impl",
            "steps:\n  - id: a\n    agent: pi\n    model: m\n    on_pass: done\n",
        )
        .unwrap();
        let mut private = Pipeline::parse(
            "impl-strict",
            "steps:\n  - id: a\n    agent: pi\n    model: m\n    on_pass: done\n",
        )
        .unwrap();
        private.private_file = Some(std::path::PathBuf::from(
            "/home/x/.spoolway/proj/local/pipelines/impl-strict.yml",
        ));

        let pipelines = Pipelines {
            pipelines: [
                ("impl".to_string(), tracked),
                ("impl-strict".to_string(), private),
            ]
            .into_iter()
            .collect(),
            ignored_overrides: Vec::new(),
        };

        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&build_list(&pipelines)).unwrap()).unwrap();
        let entries: std::collections::BTreeMap<String, serde_json::Value> = value["pipelines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| (entry["name"].as_str().unwrap().to_string(), entry.clone()))
            .collect();

        assert_eq!(entries["impl"]["source"], "tracked");
        assert!(entries["impl"]["file"].is_null(), "{:?}", entries["impl"]);

        assert_eq!(entries["impl-strict"]["source"], "private");
        assert_eq!(
            entries["impl-strict"]["file"],
            "/home/x/.spoolway/proj/local/pipelines/impl-strict.yml"
        );
    }

    /// `private_marker` — the text `pipeline list` and `pipeline show` both
    /// print after a private pipeline's name — is empty for a tracked
    /// pipeline and names the file for a private one.
    #[test]
    fn private_marker_is_empty_for_tracked_and_names_the_file_for_private() {
        let tracked = Pipeline::parse(
            "impl",
            "steps:\n  - id: a\n    agent: pi\n    model: m\n    on_pass: done\n",
        )
        .unwrap();
        assert_eq!(private_marker(&tracked), "");

        let mut private = tracked.clone();
        private.private_file = Some(std::path::PathBuf::from(
            "/x/local/pipelines/impl-strict.yml",
        ));
        assert_eq!(
            private_marker(&private),
            "  private · /x/local/pipelines/impl-strict.yml"
        );
    }

    /// `pipeline show`'s marker for a command step that reads differently for
    /// two tasks on the same pipeline — `last:` and `first:` are refused
    /// together at load, so a step is marked for at most one of them.
    #[test]
    fn chain_marker_names_first_or_last_or_neither() {
        let neither = Pipeline::parse(
            "p",
            "steps:\n  - id: a\n    run: make\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(chain_marker(neither.step("a").unwrap()), "");

        let last = Pipeline::parse(
            "p",
            "steps:\n  - id: a\n    run: make\n    last: true\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(chain_marker(last.step("a").unwrap()), " last-of-chain");

        let first = Pipeline::parse(
            "p",
            "steps:\n  - id: a\n    run: make\n    first: true\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        assert_eq!(chain_marker(first.step("a").unwrap()), " first-of-chain");
    }

    /// A `session:` step whose agent profile runs a kind spoolway cannot
    /// point back at a session — no established resume flag — is a lane
    /// that would silently open a fresh conversation every time. `pipeline
    /// check` catches it, and only that: `implementer` is the prompt
    /// `init` writes, and `model:` is set, so a failure here can only be the
    /// session check.
    #[test]
    fn pipeline_check_refuses_a_session_step_whose_kind_cannot_resume() {
        let root = crate::scratch::root("commands-session-check-refuses");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let mut config = Config::default();
        let mut flaky = crate::config::AgentProfile::defaults()["pi"].clone();
        flaky.kind = "gemini".into();
        config.agents.insert("flaky".into(), flaky);
        let home = root.join(".home");
        let repo = Repo {
            checkout: root.clone(),
            root,
            config,
            home,
        };

        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: a\n    agent: flaky\n    prompt: implementer\n    \
             model: m\n    session: true\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        let err = pipeline_check(&repo, Ok(pipelines), false).unwrap_err();
        assert!(err.to_string().contains("1 problem"), "{err}");
    }

    /// The same shape, on a kind that does resume — `pi`, what `pi`
    /// already runs — which should pass cleanly.
    #[test]
    fn pipeline_check_accepts_a_session_step_whose_kind_resumes() {
        let root = crate::scratch::root("commands-session-check-accepts");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config: Config::default(),
        };

        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: a\n    agent: pi\n    prompt: implementer\n    \
             model: m\n    session: true\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        pipeline_check(&repo, Ok(pipelines), false).expect("pi resumes, so this should pass");
    }

    /// `pipeline check` is the command you run to find out which pipeline
    /// file does not parse, so a load failure is reported as a problem
    /// rather than aborting the command before it starts — and nothing else
    /// is derived once it has failed: there is no loaded set left to check a
    /// step, a skip or a prompt against, and the embedded samples are
    /// `src/assets.rs`'s own release-time proof, never this project's.
    #[test]
    fn pipeline_check_reports_a_load_failure_instead_of_refusing_to_run() {
        let root = crate::scratch::root("commands-pipeline-check-load-failure");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config: Config::default(),
        };

        let err = pipeline_check(&repo, Err(anyhow::anyhow!("unknown field priority")), false)
            .expect_err("a pipeline that will not load is a problem");
        assert!(
            err.to_string().contains("1 problem(s) found"),
            "the load failure is the only counted problem — nothing from the embedded \
             samples is appended behind it: {err}"
        );
    }

    /// A project carrying one of the three retired step shapes `spoolway
    /// sync` migrates hears about every occurrence at once — across every
    /// pipeline file — rather than only the first one `Pipelines::load`
    /// itself stopped at.
    #[test]
    fn pipeline_check_lists_every_retired_shape_refusal_not_just_the_first() {
        let root = crate::scratch::root("commands-pipeline-check-retired-shapes");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let dir = crate::pipeline::Pipelines::dir_in(&root);
        std::fs::write(
            dir.join("default.yml"),
            "steps:\n  \
             - id: implement\n    agent: pi\n    on_pass: checks\n  \
             - id: checks\n    run: gh pr checks\n    loop:\n      checks: 3\n    \
             on_pass: done\n    on_fail: checks\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("bugfix.yml"),
            "steps:\n  \
             - id: fix\n    agent: pi\n    on_pass: review\n  \
             - id: review\n    agent: pi\n    loop:\n      fix: 2\n    on_pass: done\n    \
             on_fail: fix\n",
        )
        .unwrap();

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config: Config::default(),
        };

        let err = pipeline_check(
            &repo,
            Err(anyhow::anyhow!("stale error, superseded below")),
            false,
        )
        .expect_err("both files still carry a retired shape");
        let message = err.to_string();
        let count: usize = message.split_whitespace().next().unwrap().parse().unwrap();
        // default.yml: `checks` self-routes and its own `loop:` is a map — two.
        // bugfix.yml: `review`'s `loop:` is a map, naming `fix` — one more.
        assert_eq!(count, 3, "{message}");
    }

    /// The bug this task fixed: a project that owns no `bugfix.yml` of its
    /// own, and dropped the `reproducer` prompt that only that shipped
    /// sample ever calls for, used to fail `pipeline check` anyway — the
    /// command validated the binary's own embedded copy of `bugfix.yml`
    /// against this project's config regardless of what the project
    /// actually loaded, and would have pushed "shipped pipeline
    /// `bugfix`/`reproduce` needs prompt …" into `problems`. A project's own
    /// `solo` pipeline, naming neither, has to pass clean instead — `Ok`
    /// here is the proof, since `pipeline_check` bails whenever `problems`
    /// is non-empty.
    ///
    /// `config.agents.remove("pi")` is not what made the old code fail —
    /// every shipped `agent:` line is already `claude`, and the deleted
    /// `shipped_step_config` pinned that profile regardless of a project's
    /// own config — but it does put the project in the exact shape the
    /// acceptance criteria describe: local files naming no `pi` profile at
    /// all, so a `pi`-sourced finding leaking in would be as visible as any
    /// other.
    #[test]
    fn pipeline_check_derives_findings_only_from_the_loaded_set() {
        let root = crate::scratch::root("commands-pipeline-check-project-owned-only");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        init_at(&root);

        let mut config = Config::default();
        config.agents.remove("pi");

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config,
        };

        // Only the shipped `bugfix` sample calls for this prompt — `solo`,
        // below, never does.
        std::fs::remove_file(crate::prompt::path_for(&repo, "reproducer")).unwrap();

        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: a\n    agent: claude\n    prompt: implementer\n    \
             model: m\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        pipeline_check(&repo, Ok(pipelines), false)
            .expect("a prompt only the embedded bugfix sample needs must not leak in");
    }

    /// `step_problems` used to look for a `task_template:` skeleton in the
    /// tracked directory only, so a pipeline naming a private one — exactly
    /// what `task_contract` and a queued task both resolve through
    /// `crate::task_template::resolve` — was reported missing even though it
    /// was right there, privately. `pipeline check` must agree with the
    /// resolver it is checking, not its own separate, tracked-only lookup.
    // covers: pipeline check and the task commands resolve a pipeline's skeleton through one shared function
    #[test]
    fn pipeline_check_sees_a_private_skeleton_a_pipeline_names() {
        let repo = repo_for("check-private-skeleton");

        let private_skeleton =
            crate::local::task_templates_dir(&repo.local_dir()).join("myskel.md");
        std::fs::create_dir_all(private_skeleton.parent().unwrap()).unwrap();
        std::fs::write(&private_skeleton, "our private skeleton\n").unwrap();

        let pipeline = Pipeline::parse(
            "solo",
            "task_template: myskel\n\
             steps:\n  - id: a\n    agent: claude\n    prompt: implementer\n    \
             model: m\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        pipeline_check(&repo, Ok(pipelines), false)
            .expect("the private skeleton `myskel` names must satisfy the check");
    }

    /// The reverse of the test above: a pipeline that is genuinely tracked
    /// (a real `.yml` file on disk, not just an in-memory one `pipeline
    /// check` happens to be handed) naming `task_template: foo`, where
    /// `foo` is private and not itself a tracked pipeline, must still be
    /// reported — never silently satisfied by the private file.
    /// `exists_for` used to decide trackedness from `task_template_name()`
    /// (`foo`, not a tracked pipeline) rather than the pipeline's own name
    /// (`impl`, which is), so it passed exactly the case AC4 forbids.
    // covers: a tracked pipeline never resolves to a private skeleton
    #[test]
    fn pipeline_check_still_reports_a_tracked_pipelines_private_skeleton_missing() {
        let repo = repo_for("check-tracked-pipeline-private-skeleton");

        std::fs::write(
            Pipelines::dir_in(&repo.checkout).join("impl.yml"),
            "task_template: foo\n\
             steps:\n  - id: a\n    agent: claude\n    prompt: implementer\n    \
             model: m\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();

        let private_skeleton = crate::local::task_templates_dir(&repo.local_dir()).join("foo.md");
        std::fs::create_dir_all(private_skeleton.parent().unwrap()).unwrap();
        std::fs::write(&private_skeleton, "an unrelated private foo\n").unwrap();

        // Only `impl` itself is handed to `pipeline_check` — the shipped
        // `default`/`bugfix` samples this project's own `init_at` writes
        // carry unrelated problems (no `model:` on their steps) that would
        // otherwise drown out the one count assertion below is checking.
        let pipeline = Pipeline::parse(
            "impl",
            "task_template: foo\n\
             steps:\n  - id: a\n    agent: claude\n    prompt: implementer\n    \
             model: m\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("impl".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        let err = pipeline_check(&repo, Ok(pipelines), false).unwrap_err();
        assert!(err.to_string().contains("1 problem"), "{err}");
    }

    /// A gate with no `on_fail` is a warning, not a refusal: `Pipeline::validate`
    /// already accepts the shape, and `pipeline_check` must not start failing a
    /// pipeline that has always been legal just because it now also names the
    /// gap.
    #[test]
    fn pipeline_check_warns_but_passes_a_gate_with_no_on_fail() {
        let root = crate::scratch::root("commands-gate-warning-check-passes");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config: Config::default(),
        };

        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: verify\n    agent: pi\n    prompt: implementer\n    \
             model: m\n    gate: true\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        pipeline_check(&repo, Ok(pipelines), false)
            .expect("a gate with no on_fail is legal — only worth a warning");
    }

    /// A prompt naming `spoolway report` (or one of its flags) must never
    /// turn a clean `pipeline check` into a failing one — `Ok` here is the
    /// proof, since `pipeline_check` bails whenever `problems` is
    /// non-empty, and the restated-report-contract rule is warnings-only.
    #[test]
    fn pipeline_check_warns_but_passes_a_prompt_restating_the_report_contract() {
        let root = crate::scratch::root("commands-restated-report-check-passes");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config: Config::default(),
        };

        std::fs::write(
            crate::prompt::path_for(&repo, "implementer"),
            "# implementer\n\nFinish with `spoolway report --pass -m \"<verdict>\"`.\n",
        )
        .unwrap();

        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: a\n    agent: claude\n    prompt: implementer\n    \
             model: m\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        pipeline_check(&repo, Ok(pipelines), false)
            .expect("naming `spoolway report` is a warning, never a problem");
    }

    /// A command step starts no lane, so `skills:` on one is a key written
    /// in the belief that it does something — the same reasoning
    /// `Pipeline::validate` already refuses `prompt:`, `model:`, `effort:`
    /// and `session:` by, just reported here instead. Model and prompt are
    /// left off entirely, since neither is a command step's to set.
    #[test]
    fn pipeline_check_refuses_skills_on_a_command_step() {
        let root = crate::scratch::root("commands-skills-check-refuses-command-step");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config: Config::default(),
        };

        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: a\n    run: make\n    skills: code-review\n    on_pass: z\n  \
             - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        let err = pipeline_check(&repo, Ok(pipelines), false).unwrap_err();
        assert!(err.to_string().contains("1 problem"), "{err}");
    }

    /// A `skills:` on a kind with no skills directory would launch and
    /// quietly load none — the same silent no-op `session:` on a kind that
    /// cannot resume would be. `flaky` names a kind spoolway has no adapter
    /// row for at all, the same trick the session test above uses.
    #[test]
    fn pipeline_check_refuses_skills_on_a_kind_that_loads_none() {
        let root = crate::scratch::root("commands-skills-check-refuses-kind");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let mut config = Config::default();
        let mut flaky = crate::config::AgentProfile::defaults()["pi"].clone();
        flaky.kind = "gemini".into();
        config.agents.insert("flaky".into(), flaky);
        let home = root.join(".home");
        let repo = Repo {
            checkout: root.clone(),
            root,
            config,
            home,
        };

        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: a\n    agent: flaky\n    prompt: implementer\n    \
             model: m\n    skills: code-review\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        let err = pipeline_check(&repo, Ok(pipelines), false).unwrap_err();
        assert!(err.to_string().contains("1 problem"), "{err}");
    }

    /// A name found in neither place spoolway can see — a project's own
    /// skills directory, or the user's — is still accepted, because a plugin
    /// installs its skills where spoolway has no way to enumerate them.
    /// Refusing on those two lookups would fail every pipeline naming a
    /// plugin skill. Both homes here are scratch directories, so a real
    /// `~/.pi/skills` on the machine running this test cannot make it pass
    /// for the wrong reason.
    #[test]
    fn pipeline_check_accepts_a_skill_it_cannot_find_on_disk() {
        let root = crate::scratch::root("commands-skills-check-accepts-unfound-skill");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config: Config::default(),
        };

        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: a\n    agent: pi\n    prompt: implementer\n    \
             model: m\n    skills: code-reveiw\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        let home = crate::scratch::root("commands-skills-check-accepts-unfound-skill-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();

        crate::platform::test_home::with_home(&home, || {
            pipeline_check(&repo, Ok(pipelines), false)
                .expect("a skill spoolway cannot see is not a problem");
        });
    }

    /// A task's own `skip:` names steps by hand — a replay's copy, or one a
    /// person edited in — and a pipeline's steps can be renamed out from
    /// under it. `pipeline check` is where that mismatch is caught, not left
    /// for the dispatch pass that would otherwise silently do nothing with a
    /// name that never matches.
    #[test]
    fn pipeline_check_refuses_a_skip_naming_a_step_the_pipeline_does_not_have() {
        let root = crate::scratch::root("commands-skip-check-refuses");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "plan/demo"]);
        init_at(&root);

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config: Config::default(),
        };

        let pipeline = Pipeline::parse(
            "solo",
            "steps:\n  - id: a\n    agent: pi\n    prompt: implementer\n    \
             model: m\n    on_pass: z\n  - id: z\n    end: true\n",
        )
        .unwrap();
        let pipelines = Pipelines {
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
            ignored_overrides: Vec::new(),
        };

        let task = crate::task::Task::parse(
            repo.queue_dir().join("demo.md"),
            "---\nid: demo\nstage: queued\npipeline: solo\nskip: [not-a-step]\n---\n## Goal\ndo \
             the thing\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        task.save().unwrap();

        let err = pipeline_check(&repo, Ok(pipelines), false).unwrap_err();
        assert!(err.to_string().contains("1 problem"), "{err}");
    }

    /// The name of every field declared directly on `pub struct <name> {` in
    /// `pipeline.rs`, in source order — the same technique `task.rs`'s own
    /// `frontmatter_field_names` uses, so a field added there and forgotten
    /// here fails the build instead of silently going missing from the
    /// contract. `r#loop` is returned as `loop`: the raw-identifier prefix is
    /// only there because `loop` is a Rust keyword, and it names nothing
    /// about the pipeline file's own key.
    fn struct_field_names(struct_name: &str) -> Vec<&'static str> {
        const SRC: &str = include_str!("../pipeline.rs");
        let marker = format!("pub struct {struct_name} {{\n");
        let struct_start = SRC
            .find(&marker)
            .unwrap_or_else(|| panic!("`{marker}` not found in pipeline.rs"));
        let body = &SRC[struct_start + marker.len()..];
        let struct_end = body
            .find("\n}")
            .unwrap_or_else(|| panic!("{struct_name}'s closing brace not found"));
        let body = &body[..struct_end];

        body.lines()
            .filter_map(|line| line.trim().strip_prefix("pub "))
            .filter_map(|rest| rest.split(':').next())
            .map(str::trim)
            .map(|name| name.strip_prefix("r#").unwrap_or(name))
            .collect()
    }

    /// Every public field of `Step` and `Pipeline` has to show up in one of
    /// the three groups bare `pipeline contract` prints — read off
    /// `pipeline.rs`'s own source rather than a hand-copied list, the way
    /// `task.rs`'s `every_frontmatter_field_is_in_some_contract_group` holds
    /// `Frontmatter` to the same standard.
    #[test]
    fn every_pipeline_field_is_in_some_contract_group() {
        let mut known = std::collections::BTreeSet::new();
        known.extend(PIPELINE_KEYS.iter().copied());
        known.extend(STEP_KEYS.iter().copied());
        known.extend(REFUSED_KEYS.iter().copied());

        for field in struct_field_names("Step")
            .into_iter()
            .chain(struct_field_names("Pipeline"))
        {
            assert!(
                known.contains(field),
                "`{field}` is a public field of Step or Pipeline but is in none of \
                 pipeline_contract's contract groups — add it to one"
            );
        }
    }

    /// A key a pipeline file may actually set — `PIPELINE_KEYS` or
    /// `STEP_KEYS` — has to carry one sentence in `fields` on how to fill it.
    #[test]
    fn every_settable_key_has_a_fields_entry() {
        let fields: std::collections::BTreeMap<&str, &str> =
            FIELD_SENTENCES.iter().copied().collect();
        for key in PIPELINE_KEYS.iter().chain(STEP_KEYS.iter()) {
            assert!(
                fields.contains_key(key),
                "`{key}` may be set in a pipeline file but has no `fields` entry — add one"
            );
        }
    }

    /// `rules` is the one place a format rule is stated to every caller at
    /// once — a script preference belongs there, not only in a reviewer's
    /// head.
    #[test]
    fn rules_prefer_a_script_over_a_multi_command_run() {
        assert!(
            rules().iter().any(|rule| rule.contains("script")),
            "`rules` should tell a caller to prefer a script over a multi-command `run:`: \
             {:?}",
            rules()
        );
    }

    /// `loop`'s own sentence in `fields` states a recommended ceiling —
    /// stated, not enforced: `pipeline check` still refuses nothing over it,
    /// per this task's own non-goals.
    #[test]
    fn loop_field_states_a_recommended_ceiling() {
        let fields: std::collections::BTreeMap<&str, &str> =
            FIELD_SENTENCES.iter().copied().collect();
        assert!(
            fields["loop"].contains("Three"),
            "`loop`'s sentence should recommend a ceiling: {}",
            fields["loop"]
        );
    }

    /// The top-level `description` sentence asks for one sentence, not the
    /// multi-sentence prose a pipeline file may still get away with.
    #[test]
    fn description_field_asks_for_one_sentence() {
        let fields: std::collections::BTreeMap<&str, &str> =
            FIELD_SENTENCES.iter().copied().collect();
        assert!(
            fields["description"].contains("one sentence"),
            "`description`'s sentence should ask for one sentence: {}",
            fields["description"]
        );
    }

    /// The template and the `description` field sentence ship inside one
    /// `pipeline contract` payload, so they cannot ask for different sizes —
    /// the template's own prose has to want the one sentence too.
    #[test]
    fn the_template_asks_for_the_same_one_sentence_description() {
        let template = template();
        assert!(
            !template.to_lowercase().contains("a few sentences"),
            "the template should not still ask for a few sentences: {template}"
        );
        assert!(
            template.contains("One sentence on what this pipeline is for"),
            "the template's `description:` placeholder should ask for one sentence: {template}"
        );
    }

    /// The `template` field is not decoration: written to a real project's
    /// `.spoolway/pipelines/<name>.yml`, it has to load and pass the exact
    /// validation `pipeline check` runs, and every key `pipeline contract`
    /// lists under `keys.step` has to actually appear in it — live or
    /// commented — or a reader copying it would never learn that key exists.
    #[test]
    fn the_template_is_a_valid_pipeline_covering_every_step_key() {
        let root = crate::scratch::root("commands-pipeline-contract-template");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        init_at(&root);

        // Fresh scaffolds deliberately carry blank model choices and therefore
        // fail `pipeline check` until a person fills them in. This test is
        // about the runnable annotated template, so keep only the file it is
        // about rather than asking those fresh scaffold files to be runnable.
        for (name, _) in crate::pipeline::BUILTIN_PIPELINES {
            std::fs::remove_file(Pipelines::file_in(&root, name)).unwrap();
        }

        let text = template();
        for key in STEP_KEYS {
            assert!(
                text.contains(key),
                "`{key}` is listed under `keys.step` but never appears in `template`"
            );
        }

        std::fs::write(Pipelines::file_in(&root, "default"), &text).unwrap();

        let repo = Repo {
            home: root.join(".home"),
            checkout: root.clone(),
            root,
            config: Config::default(),
        };
        let pipelines = Pipelines::load(&repo.root, &repo.config).expect("template must load");
        pipeline_check(&repo, Ok(pipelines), false).expect("template must pass `pipeline check`");
    }

    /// Bare `pipeline contract` prints one JSON object carrying every section
    /// the acceptance criteria names, and this project's own pipelines,
    /// agents and prompts rather than an invented example.
    #[test]
    fn bare_contract_prints_every_section_as_json() {
        let repo = repo_for("pipeline-contract-bare");
        let pipelines = Pipelines::builtin();
        let value: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&build_contract(&repo, &pipelines)).unwrap(),
        )
        .unwrap();
        for key in [
            "path",
            "keys",
            "fields",
            "rules",
            "agents",
            "prompts",
            "task_skeletons",
            "existing",
            "template",
        ] {
            assert!(value.get(key).is_some(), "missing `{key}`: {value}");
        }
        assert!(value["keys"]["reserved_ids"].is_array(), "{value}");
        assert!(
            value["agents"]["pi"]["kind"] == "pi",
            "this project's own agent profiles should be named directly: {value}"
        );
    }

    /// A repo with a tracked `.spoolway/pipelines/<name>.yml` on disk, and a
    /// scratch machine home behind it — for `pipeline_override`, which
    /// reads the tracked file rather than the built-in fixture
    /// [`Pipelines::builtin`] stands in for elsewhere.
    ///
    /// Faking `$HOME` matters here for the same reason it does in
    /// `commands::override::tests::with_repo`: `repo.overrides_dir()` and
    /// `overrides::dir_for` both resolve through
    /// [`crate::mux::project_home`], so a test that leaves the real home in
    /// place has `pipeline_override`'s own `Pipelines::load` reading a
    /// layer under the developer's real `~/.spoolway/…` while the write
    /// lands in the scratch one — two different directories agreeing by
    /// accident, or not agreeing at all, rather than by construction.
    fn with_repo_and_pipeline<T>(
        name: &str,
        pipeline: &str,
        body: &str,
        f: impl FnOnce(&Repo) -> T,
    ) -> T {
        let root = crate::scratch::root(&format!("pipeline-override-{name}"));
        let fake_home = crate::scratch::root(&format!("pipeline-override-{name}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&fake_home);
        std::fs::create_dir_all(root.join(".spoolway/pipelines")).unwrap();
        std::fs::write(
            root.join(format!(".spoolway/pipelines/{pipeline}.yml")),
            body,
        )
        .unwrap();

        let result = crate::platform::test_home::with_home(&fake_home, || {
            let home = crate::mux::project_home(&root).unwrap();
            let config = Config::default();
            let repo = Repo {
                checkout: root.clone(),
                root: root.clone(),
                config,
                home,
            };
            f(&repo)
        });

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&fake_home).ok();
        result
    }

    const DEMO_PIPELINE: &str =
        "steps:\n  - id: implement\n    agent: pi\n    model: claude-sonnet-5\n    on_pass: done\n";

    #[test]
    fn pipeline_override_writes_the_layer_and_leaves_the_tracked_file_alone() {
        with_repo_and_pipeline("happy", "demo", DEMO_PIPELINE, |repo| {
            let tracked_before =
                std::fs::read_to_string(Pipelines::file_in(&repo.checkout, "demo")).unwrap();

            pipeline_override(repo, "demo", "implement.model=claude-opus-5").unwrap();

            assert_eq!(
                std::fs::read_to_string(Pipelines::file_in(&repo.checkout, "demo")).unwrap(),
                tracked_before,
            );
            let patch = crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "demo")
                .unwrap()
                .unwrap();
            assert_eq!(
                patch.steps["implement"]["model"].as_str(),
                Some("claude-opus-5")
            );
        });
    }

    #[test]
    fn pipeline_override_refuses_a_step_the_pipeline_does_not_have() {
        with_repo_and_pipeline("bad-step", "demo", DEMO_PIPELINE, |repo| {
            let err =
                pipeline_override(repo, "demo", "nosuchstep.model=claude-opus-5").unwrap_err();
            assert!(
                err.to_string().contains("has no step `nosuchstep`"),
                "{err}"
            );
            assert!(
                crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "demo")
                    .unwrap()
                    .is_none(),
                "nothing should be written on a refusal"
            );
        });
    }

    #[test]
    fn pipeline_override_refuses_the_id_key() {
        with_repo_and_pipeline("id-key", "demo", DEMO_PIPELINE, |repo| {
            let err = pipeline_override(repo, "demo", "implement.id=other").unwrap_err();
            assert!(format!("{err:#}").contains("rename"), "{err:#}");
        });
    }

    /// The retired key, refused at `--set` rather than accepted and then
    /// ignored for the life of the layer. It is the probe above that catches
    /// it — the same `apply_step_patch` the loader runs — which is what keeps
    /// the promise that nothing accepted here is rejected, or silently
    /// dropped, on the next dispatcher pass.
    #[test]
    fn pipeline_override_refuses_the_retired_on_loop_max_key() {
        with_repo_and_pipeline("retired-key", "demo", DEMO_PIPELINE, |repo| {
            let err = pipeline_override(repo, "demo", "implement.on_loop_max=blocked").unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("`on_loop_max:`"), "{message}");
            assert!(message.contains("`blocked`"), "{message}");
            assert!(
                crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "demo")
                    .unwrap()
                    .is_none(),
                "a refused `--set` must leave no patch behind"
            );
        });
    }

    #[test]
    fn pipeline_override_refuses_an_unknown_pipeline() {
        with_repo_and_pipeline("unknown-pipeline", "demo", DEMO_PIPELINE, |repo| {
            let err = pipeline_override(repo, "nosuchpipeline", "implement.model=x").unwrap_err();
            assert!(err.to_string().contains("no pipeline named"), "{err}");
        });
    }

    /// `pipeline override` probes `Pipelines::load_tracked` alone, so a
    /// private pipeline — one `pipeline list` and `pipeline show` both find —
    /// is refused as "no pipeline named", exactly as if it did not exist.
    #[test]
    fn pipeline_override_accepts_a_private_pipeline() {
        with_repo_and_pipeline("private-pipeline", "demo", DEMO_PIPELINE, |repo| {
            let local = crate::local::dir_for(&repo.root).unwrap();
            let dir = crate::local::pipelines_dir(&local);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("strict.yml"),
                "steps:\n  - id: implement\n    agent: pi\n    model: base-model\n    on_pass: \
                 done\n",
            )
            .unwrap();

            pipeline_override(repo, "strict", "implement.model=x")
                .expect("a private pipeline must patch, not be refused");
            let patch = crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "strict")
                .unwrap()
                .unwrap();
            assert_eq!(patch.steps["implement"]["model"].as_str(), Some("x"));
        });
    }

    /// `promote` is what starts a private pipeline's waiting patch applying
    /// — see `pipeline_override_accepts_a_private_pipeline` above, which
    /// only writes it. Before `promote`, `Pipelines::load` carries the patch
    /// as waiting (`Ignored::private_pipeline`, not silence); after, the
    /// same load actually carries the patched value.
    #[test]
    fn pipeline_promote_starts_an_override_file_applying() {
        let (repo, home) = repo_for_home_aware("promote-starts-applying");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            pipeline_override(&repo, "default-strict", "implement.model=claude-opus-5").unwrap();

            // Still waiting: the tracked load never even reaches the patch,
            // since `load_tracked` never merges the private pipeline it
            // targets in the first place.
            let before = Pipelines::load(&repo.checkout, &repo.config).unwrap();
            assert!(
                before.ignored_overrides.iter().any(
                    |i| i.notice().contains("default-strict") && i.notice().contains("private")
                ),
                "{:?}",
                before
                    .ignored_overrides
                    .iter()
                    .map(|i| i.notice())
                    .collect::<Vec<_>>()
            );
            assert_ne!(
                before.pipelines["default-strict"]
                    .step("implement")
                    .unwrap()
                    .model
                    .as_deref(),
                Some("claude-opus-5"),
            );

            pipeline_promote(&repo, "default-strict", false).unwrap();

            let after = Pipelines::load(&repo.checkout, &repo.config).unwrap();
            assert!(
                !after
                    .ignored_overrides
                    .iter()
                    .any(|i| i.notice().contains("default-strict")),
                "{:?}",
                after
                    .ignored_overrides
                    .iter()
                    .map(|i| i.notice())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                after.pipelines["default-strict"]
                    .step("implement")
                    .unwrap()
                    .model
                    .as_deref(),
                Some("claude-opus-5"),
            );
        });
    }

    /// The prompt twin of the test above: a fork of a *private* prompt also
    /// waits on the promote of the pipeline that runs it, named rather than
    /// silent while it waits, and starts applying once that promote lands.
    #[test]
    fn pipeline_promote_starts_a_prompt_fork_applying_too() {
        let (repo, home) = repo_for_home_aware("promote-starts-prompt-applying");

        crate::platform::test_home::with_home(&home, || {
            pipeline_copy(&repo, "default", "default-strict", false).unwrap();
            prompt_copy(&repo, "implementer", "implementer-strict", false).unwrap();
            let pipeline_path =
                crate::local::pipelines_dir(&repo.local_dir()).join("default-strict.yml");
            let raw = std::fs::read_to_string(&pipeline_path).unwrap();
            let raw = raw.replace("prompt: implementer", "prompt: implementer-strict");
            assert!(raw.contains("implementer-strict"), "{raw}");
            std::fs::write(&pipeline_path, raw).unwrap();

            prompt_override(&repo, "implementer-strict").expect("a private prompt must fork");
            std::fs::write(
                crate::overrides::prompt_patch_path(&repo.overrides_dir(), "implementer-strict"),
                "the patched prose",
            )
            .unwrap();

            // Still waiting: `prompt::path_for` never applies a fork onto a
            // private prompt, only a tracked one.
            assert_ne!(
                std::fs::read_to_string(crate::prompt::path_for(&repo, "implementer-strict"))
                    .unwrap(),
                "the patched prose",
            );

            pipeline_promote(&repo, "default-strict", false).expect("promote");

            assert_eq!(
                std::fs::read_to_string(crate::prompt::path_for(&repo, "implementer-strict"))
                    .unwrap(),
                "the patched prose",
            );
        });
    }
}
