//! `spoolway pipeline show`, `pipeline check`, `pipeline contract`, `pipeline
//! list` and `pipeline gen`.

use super::*;

pub fn pipeline_show(repo: &Repo, pipelines: &Pipelines, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }
    for pipeline in pipelines.pipelines.values() {
        show_one(pipeline, &pipelines.default)?;
    }
    Ok(())
}

/// Every pipeline's name, one per line, marking the default, with its
/// `description:` indented underneath — the fact a reader is choosing
/// between pipelines on, not just their names.
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
        let marker = match pipeline.name == pipelines.default {
            true => " (default)",
            false => "",
        };
        println!("{}{marker}", pipeline.name);
        if let Some(description) = &pipeline.description {
            println!("    {description}");
        }
        println!();
    }
    Ok(())
}

/// One pipeline, as `--json pipeline list` names it: exactly the facts a
/// script needs to choose between pipelines, and nothing about its steps —
/// `pipeline contract` already carries those.
#[derive(Debug, serde::Serialize)]
struct PipelineListEntry {
    name: String,
    default: bool,
    description: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct PipelineListJson {
    default: String,
    pipelines: Vec<PipelineListEntry>,
}

fn build_list(pipelines: &Pipelines) -> PipelineListJson {
    PipelineListJson {
        default: pipelines.default.clone(),
        pipelines: pipelines
            .pipelines
            .values()
            .map(|pipeline| PipelineListEntry {
                name: pipeline.name.clone(),
                default: pipeline.name == pipelines.default,
                description: pipeline.description.clone(),
            })
            .collect(),
    }
}

/// Keys a pipeline file may carry at the top level, above `steps:`.
const PIPELINE_KEYS: &[&str] = &["description", "task_template", "steps"];

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
    "on_loop_max",
    "run",
    "timeout",
    "background",
    "headless",
    "last",
    "end",
];

/// Keys `deny_unknown_fields` refuses outright: two retired spellings of
/// `loop:` and the retired `cleanup:`, kept on [`Step`] only so a file still
/// naming them gets a message pointing at the replacement — or, for
/// `cleanup:`, saying why there is none — rather than serde's own "unknown
/// field", and two struct fields that answer a fact about where a `Pipeline`
/// came from rather than something a file could ever set —
/// [`Pipeline::name`], because the file name is the name, and
/// [`Pipeline::blocked_declared`], set only by [`Pipelines::assemble`] once a
/// file is read.
const REFUSED_KEYS: &[&str] = &[
    "name",
    "blocked_declared",
    "max_new_sessions",
    "max_rounds",
    "cleanup",
];

/// One sentence per key a pipeline file may actually set — [`PIPELINE_KEYS`]
/// and [`STEP_KEYS`] together — written from the doc comment already on the
/// field in `pipeline.rs`.
const FIELD_SENTENCES: &[(&str, &str)] = &[
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
        "At the top level, what this pipeline is for, in a few sentences — read \
         to choose between pipelines. On a step, a human-facing one-liner, shown \
         by `spoolway pipeline show`.",
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
        "Laps allowed per route in — arrivals, not conversations. A bare number \
         bounds every route; the map form bounds one at a time.",
    ),
    (
        "on_loop_max",
        "Where a spent loop sends the task. Absent carries it on to `on_pass`, \
         findings and all.",
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
         pane of its own under the herdr and tmux backends.",
    ),
    (
        "last",
        "Command steps only. Only the task at the top of a chain runs it; every \
         task below walks past to `on_pass`.",
    ),
    (
        "end",
        "The task stops here — nothing is scheduled for it again. May name no \
         agent, no command and no transition.",
    ),
];

/// The four things refused when a pipeline file is loaded — [`Pipeline::validate`]'s
/// own checks, worded for a reader with no access to its source.
fn rules() -> Vec<&'static str> {
    vec![
        "the file name is the pipeline's name, and a task starts on the first step",
        "every cycle carries a `loop`, and the exit it names must leave the cycle",
        "`blocked` may be declared to staff it; `queued`, `done`, `paused` never",
        "a step is what it carries — `agent:`, `run:` or `end: true`",
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

#[derive(Debug, serde::Serialize)]
struct PreferencesContract {
    local_models: bool,
    loop_default: u32,
    auto: bool,
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
    preferences: PreferencesContract,
    template: String,
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

    let cfg = &repo.config.pipeline_gen;

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
        preferences: PreferencesContract {
            local_models: cfg.pipeline_local_models,
            loop_default: cfg.pipeline_loop_default,
            auto: cfg.pipeline_auto,
        },
        template: template(),
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
         # What this pipeline is for, in a few sentences — read to choose between\n\
         # pipelines.\n\
         description: >-\n\
         \x20\x20A few sentences on what this pipeline is for, and which tasks belong\n\
         \x20\x20on it rather than another one.\n\
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
         \x20\x20\x20\x20loop:\n\
         \x20\x20\x20\x20\x20\x20fix: 3\n\
         \x20\x20\x20\x20# on_loop_max: blocked     where a spent loop lands; `on_pass` by default\n\
         \x20\x20\x20\x20on_pass: handover\n\
         \x20\x20\x20\x20on_fail: fix\n\
         \n\
         \x20\x20- id: fix\n\
         \x20\x20\x20\x20description: Work the review's findings, and nothing else.\n\
         \x20\x20\x20\x20agent: pi\n\
         \x20\x20\x20\x20prompt: implementer\n\
         \x20\x20\x20\x20model: {local_model}\n\
         \x20\x20\x20\x20session: true\n\
         \x20\x20\x20\x20on_pass: review\n\
         \x20\x20\x20\x20on_fail: blocked\n\
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
         \x20\x20\x20\x20on_pass: done\n\
         \x20\x20\x20\x20on_fail: blocked\n\
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

fn show_one(pipeline: &Pipeline, default: &str) -> Result<()> {
    let marks = if pipeline.name == default {
        "  (default)"
    } else {
        ""
    };
    println!(
        "pipeline `{}`{marks}  entry: {}",
        pipeline.name,
        pipeline.entry(),
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
            // The one key that makes a flow read differently for two tasks on
            // the same pipeline, so a reader who does not see it here would
            // have no way to know a step they are looking at is one most tasks
            // walk straight past.
            if step.last {
                print!(" last-of-chain");
            }
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
        if let Some(named) = &pipeline.task_template {
            let path = repo.task_templates_dir().join(format!("{named}.md"));
            if !path.exists() {
                problems.push(format!(
                    "pipeline `{}` names task skeleton `{named}`, and {} is not there — write \
                     it, or drop the `task_template:` to take `{}`",
                    pipeline.name,
                    relative(&repo.checkout, &path),
                    crate::task_template::FALLBACK
                ));
            }
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
    // reported as the one problem, and nothing else is derived: there is no
    // loaded set left to check a step, a skip, or a prompt against, and the
    // embedded samples are release-time proof, not this project's own.
    let pipelines = match pipelines {
        Ok(pipelines) => pipelines,
        Err(err) => {
            println!("  problem: pipelines do not load: {err:#}");
            bail!("1 problem(s) found");
        }
    };
    let pipelines = &pipelines;

    pipelines.validate()?;

    // Gate and description warnings, gathered up front like the prompt
    // findings below: they are worth a person's attention but never fail
    // this check on their own — see `Pipeline::gate_warnings` and
    // `Pipeline::description_warnings`.
    let mut gate_warnings = Vec::new();
    for pipeline in pipelines.pipelines.values() {
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

    // Read the prompts against the steps that run them. These are findings,
    // not problems: they come out of prose and a project may have a reason for
    // any one of them, so they are printed and the check still passes. A
    // capability a prompt asks for and its step does not grant is the one that
    // matters — it fails mid-lane otherwise, in a pane nobody is watching.
    let findings = crate::prompt::lint(repo, pipelines)?;

    if problems.is_empty() {
        println!(
            "{} pipeline(s) valid: {:?}, agents {:?}",
            pipelines.pipelines.len(),
            pipelines.names(),
            pipelines.referenced_agents().keys().collect::<Vec<_>>()
        );
        report_gate_warnings(&gate_warnings);
        report_prompt_findings(&findings);
        return Ok(());
    }

    for problem in &problems {
        println!("  problem: {problem}");
    }
    report_gate_warnings(&gate_warnings);
    report_prompt_findings(&findings);
    bail!("{} problem(s) found", problems.len())
}

/// Print the gate and description warnings `pipeline_check` gathered,
/// exactly the way it prints the prompt findings beside them — findings a
/// person should see, but never a reason this check fails.
fn report_gate_warnings(warnings: &[String]) {
    if warnings.is_empty() {
        return;
    }
    println!();
    for warning in warnings {
        println!("  warning: {warning}");
    }
}

/// Open a fresh agent session, in a pane of this checkout, to write a new
/// pipeline — never a step of the graph this binary walks itself.
///
/// Nothing about a pipeline is written here: the session that opens reads
/// `spoolway-pipeline`'s own generation procedure and does the rest. This
/// command's whole job is getting that session started, with the right
/// preferences in front of it.
pub fn pipeline_gen(repo: &Repo, mux: &dyn Mux, args: &PipelineGenArgs) -> Result<()> {
    let cfg = &repo.config.pipeline_gen;

    if cfg.pipeline_model.trim().is_empty() {
        bail!(
            "`pipeline_gen.pipeline_model` is blank — set one with `spoolway config set \
             pipeline_gen.pipeline_model <model>`"
        );
    }
    let profile = repo.config.agent(&cfg.pipeline_agent).with_context(|| {
        format!(
            "`pipeline_gen.pipeline_agent` names `{}` — set it to a profile from `[agents.*]` \
             with `spoolway config set pipeline_gen.pipeline_agent <profile>`",
            cfg.pipeline_agent
        )
    })?;
    if repo.config.dispatch.backend == crate::config::Backend::Headless {
        bail!(
            "`dispatch.backend` is `headless` — `spoolway pipeline gen` opens a real pane to \
             work in, so set it to `herdr` or `tmux` with `spoolway config set \
             dispatch.backend <backend>`"
        );
    }

    let plan = args
        .plan
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());

    // A session id of its own, the same shape a lane's is — it is what names
    // this session's scratch file and, for a kind that mints its own id and
    // will not take one, its per-session home.
    let session = crate::usage::new_session_id();
    let name = format!("pipeline-gen-{}", &session[..8]);

    let system_prompt = pipeline_gen_system_prompt(plan);
    // Under the project's home rather than the checkout, the same as every
    // other scratch file a lane leaves behind — `scratch_dir` creates it.
    let scratch_dir = repo.scratch_dir();
    let prompt_file = scratch_dir.join(format!("{name}.md"));
    write_atomic(&prompt_file, &system_prompt)?;

    let state_dir = repo.root.join(crate::config::STATE_DIR);
    // This session opens directly in `repo.root`, never a worktree of its
    // own, so its git directory is the main checkout's — resolved through
    // git rather than assumed, the same as a real lane start.
    let git_dir = crate::repo::git_dir(&repo.root)?;
    let values: std::collections::BTreeMap<&str, String> = std::collections::BTreeMap::from([
        ("model", cfg.pipeline_model.clone()),
        ("session_id", session.clone()),
        ("prompt_file", prompt_file.display().to_string()),
        ("task_file", String::new()),
        ("worktree", repo.root.display().to_string()),
        ("repo", repo.root.display().to_string()),
        ("state_dir", state_dir.display().to_string()),
        // Same grant a lane gets: a kind confined to what it names has to be
        // told where this project's runtime state lives — see
        // `crate::repo::Repo::home`.
        ("project_home", repo.home().display().to_string()),
        ("git_dir", git_dir.display().to_string()),
    ]);

    let mut lane_args = profile.render_args(&values)?;
    let effort = (!cfg.pipeline_effort.trim().is_empty()).then(|| cfg.pipeline_effort.trim());
    lane_args.extend(profile.effort_args(effort));

    let workspace = mux.create_pane(&repo.root, &name)?;
    let launched = mux.start_lane(&crate::mux::LaneSpec {
        name: &name,
        label: &name,
        kind: &profile.kind,
        pane_id: &workspace.pane_id,
        args: &lane_args,
        env: &std::collections::BTreeMap::new(),
        path_prefix: None,
    });
    if let Err(err) = launched {
        let _ = mux.close_pane(&workspace.pane_id);
        return Err(err);
    }

    let prompt = pipeline_gen_opening_prompt(plan);
    mux.prompt(&name, &prompt)?;

    println!(
        "agent         {} · {} · effort {}",
        cfg.pipeline_agent,
        cfg.pipeline_model,
        if cfg.pipeline_effort.trim().is_empty() {
            "(default)"
        } else {
            cfg.pipeline_effort.trim()
        }
    );
    if let Some(plan) = plan {
        println!("plan          {plan}");
    }
    println!(
        "preferences   auto = {} · loop_default = {} · local_models = {}",
        cfg.pipeline_auto, cfg.pipeline_loop_default, cfg.pipeline_local_models
    );
    println!();
    println!("opened a pane on this checkout");
    println!("prompted `spoolway-pipeline`");
    println!();
    println!("Nothing is written yet. Answer it in that pane.");

    Ok(())
}

/// The system prompt written to the project's own `scratch/`, for a kind whose argv
/// template needs one — see [`crate::agent::Adapter::args`]'s `{prompt_file}`.
/// There is no prompt for this session: the `spoolway-pipeline` skill is the
/// whole brief, so this names only the plan and says as much.
fn pipeline_gen_system_prompt(plan: Option<&str>) -> String {
    match plan {
        Some(plan) => format!(
            "Generating a pipeline for the plan at {plan}.\n\nThe `spoolway-pipeline` skill \
             carries the whole procedure.\n"
        ),
        None => "Generating a pipeline.\n\nThe `spoolway-pipeline` skill carries the whole \
                  procedure.\n"
            .to_string(),
    }
}

/// The opening line typed into the pane once the session is up.
fn pipeline_gen_opening_prompt(plan: Option<&str>) -> String {
    match plan {
        Some(plan) => format!(
            "Use the `spoolway-pipeline` skill to generate a pipeline for the plan at {plan}."
        ),
        None => "Use the `spoolway-pipeline` skill to generate a pipeline.".to_string(),
    }
}

fn report_prompt_findings(findings: &[crate::prompt::Finding]) {
    if findings.is_empty() {
        return;
    }
    println!();
    for finding in findings {
        println!("  prompt: {}", finding.render());
    }
    println!();
    println!(
        "  {} prompt finding(s) — `spoolway prompt check` for these alone.",
        findings.len()
    );
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use crate::mux::{Lane, LaneSpec, Mux, Workspace};

    use super::*;

    /// A `Mux` that records what `pipeline_gen` asked of it, the same way
    /// `dispatch.rs`'s own test fake does — the one lane launch, and only it,
    /// is what a refusal or a success is checked against here.
    #[derive(Default)]
    struct FakeMux {
        panes: RefCell<u32>,
        lanes: RefCell<Vec<String>>,
        prompts: RefCell<Vec<(String, String)>>,
    }

    impl Mux for FakeMux {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn is_available(&self) -> bool {
            true
        }
        fn unavailable(&self) -> String {
            String::new()
        }
        fn resident_while_waiting(&self) -> bool {
            true
        }
        fn list_lanes(&self) -> Result<Vec<Lane>> {
            Ok(Vec::new())
        }
        fn create_workspace(
            &self,
            _cwd: &Path,
            _branch: &str,
            _base: &str,
            _label: &str,
        ) -> Result<Workspace> {
            unimplemented!("pipeline_gen never cuts a task worktree")
        }
        fn remove_workspace(&self, _id: &str) -> Result<()> {
            unimplemented!()
        }
        fn close_workspace(&self, _id: &str) -> Result<()> {
            unimplemented!()
        }
        fn close_tab(&self, _id: &str) -> Result<()> {
            unimplemented!()
        }
        fn create_pane(&self, cwd: &Path, label: &str) -> Result<Workspace> {
            *self.panes.borrow_mut() += 1;
            Ok(Workspace {
                workspace_id: "w0".into(),
                pane_id: format!("{label}-pane"),
                tab_id: Some("w0:t1".into()),
                checkout_path: cwd.to_path_buf(),
            })
        }
        fn split_pane(&self, _tab_id: &str, _cwd: &Path) -> Result<String> {
            unimplemented!("pipeline_gen splits no pane of its own")
        }
        fn close_pane(&self, _pane_id: &str) -> Result<()> {
            Ok(())
        }
        fn start_lane(&self, spec: &LaneSpec<'_>) -> Result<()> {
            self.lanes
                .borrow_mut()
                .push(format!("{} {}", spec.kind, spec.args.join(" ")));
            Ok(())
        }
        fn prompt(&self, name: &str, text: &str) -> Result<()> {
            self.prompts
                .borrow_mut()
                .push((name.to_string(), text.to_string()));
            Ok(())
        }
        fn read(&self, _name: &str, _lines: usize) -> Result<String> {
            unimplemented!()
        }
        fn interrupt_lane(&self, _name: &str) -> Result<()> {
            unimplemented!()
        }
        fn stop_lane(&self, _name: &str, _pane_id: &str) -> Result<()> {
            unimplemented!()
        }
        fn focus_lane(&self, _name: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_tab(&self, _tab_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_workspace(&self, _workspace_id: &str, _label: &str) -> Result<()> {
            unimplemented!()
        }
        fn rename_pane(&self, _pane_id: &str, _label: &str) -> Result<()> {
            Ok(())
        }
    }

    /// A repo whose config carries a usable `[pipeline_gen]` block, in a
    /// fresh scratch checkout — `pipeline_gen` writes its system prompt under
    /// the project's own `scratch/`, which has to actually exist to write into.
    fn repo_for(name: &str) -> Repo {
        let root = crate::scratch::root(&format!("pipeline-gen-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-b", "main"]);
        init_at(&root);
        let mut config = Config::default();
        config.pipeline_gen.pipeline_model = "claude-opus-5".into();
        config.pipeline_gen.pipeline_effort = "high".into();
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

    #[test]
    fn pipeline_gen_refuses_a_blank_model() {
        let mut repo = repo_for("blank-model");
        repo.config.pipeline_gen.pipeline_model.clear();
        let mux = FakeMux::default();
        let err = pipeline_gen(&repo, &mux, &PipelineGenArgs { plan: None }).unwrap_err();
        assert!(
            err.to_string().contains("pipeline_gen.pipeline_model"),
            "{err}"
        );
        assert_eq!(*mux.panes.borrow(), 0);
    }

    #[test]
    fn pipeline_gen_refuses_an_agent_naming_no_profile() {
        let mut repo = repo_for("bad-agent");
        repo.config.pipeline_gen.pipeline_agent = "nosuchprofile".into();
        let mux = FakeMux::default();
        let err = pipeline_gen(&repo, &mux, &PipelineGenArgs { plan: None }).unwrap_err();
        assert!(
            err.to_string().contains("pipeline_gen.pipeline_agent"),
            "{err}"
        );
        assert_eq!(*mux.panes.borrow(), 0);
    }

    #[test]
    fn pipeline_gen_refuses_a_headless_backend() {
        let mut repo = repo_for("headless");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let mux = FakeMux::default();
        let err = pipeline_gen(&repo, &mux, &PipelineGenArgs { plan: None }).unwrap_err();
        assert!(err.to_string().contains("dispatch.backend"), "{err}");
        assert_eq!(*mux.panes.borrow(), 0);
    }

    /// The happy path: exactly one pane, one lane, one prompt — and the
    /// system prompt file the lane's argv points at actually names the plan.
    #[test]
    fn pipeline_gen_opens_one_pane_and_prompts_the_skill() {
        let repo = repo_for("happy");
        let mux = FakeMux::default();
        pipeline_gen(
            &repo,
            &mux,
            &PipelineGenArgs {
                plan: Some(".spoolway/plans/my-plan.html".into()),
            },
        )
        .expect("a valid config should launch cleanly");

        assert_eq!(*mux.panes.borrow(), 1);
        assert_eq!(mux.lanes.borrow().len(), 1);
        let launch = &mux.lanes.borrow()[0];
        assert!(launch.contains("claude-opus-5"), "{launch}");
        assert!(launch.contains("--effort high"), "{launch}");

        assert_eq!(mux.prompts.borrow().len(), 1);
        let (_, text) = &mux.prompts.borrow()[0];
        assert!(text.contains("spoolway-pipeline"), "{text}");
        assert!(text.contains(".spoolway/plans/my-plan.html"), "{text}");

        // The system prompt file itself, found by scanning the scratch
        // directory rather than guessing its name — the launch's own argv
        // proves the two agree.
        let scratch = repo.scratch_dir();
        let written = std::fs::read_dir(&scratch)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension().is_some_and(|ext| ext == "md"))
            .expect("pipeline_gen writes a system prompt file under scratch");
        let contents = std::fs::read_to_string(written.path()).unwrap();
        assert!(
            contents.contains(".spoolway/plans/my-plan.html"),
            "{contents}"
        );
        assert!(contents.contains("spoolway-pipeline"), "{contents}");
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
            init(root, &InitArgs::default()).expect("init")
        });
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

    /// `--json pipeline list` carries the default pipeline's name and, per
    /// pipeline, its name, whether it is the default, and its description —
    /// the same facts the prose form prints, as fields a script can read
    /// without parsing English.
    #[test]
    fn build_list_carries_name_default_and_description_per_pipeline() {
        let pipelines = Pipelines::builtin();
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&build_list(&pipelines)).unwrap()).unwrap();
        assert_eq!(value["default"], pipelines.default);
        let entries = value["pipelines"].as_array().expect("a pipelines array");
        assert_eq!(entries.len(), pipelines.pipelines.len());
        for entry in entries {
            assert!(entry.get("name").is_some(), "missing `name`: {entry}");
            assert!(entry.get("default").is_some(), "missing `default`: {entry}");
            assert!(
                entry.get("description").is_some(),
                "missing `description`: {entry}"
            );
        }
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
            default: "solo".into(),
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
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
            default: "solo".into(),
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
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
            default: "solo".into(),
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
        };

        pipeline_check(&repo, Ok(pipelines), false)
            .expect("a prompt only the embedded bugfix sample needs must not leak in");
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
            default: "solo".into(),
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
        };

        pipeline_check(&repo, Ok(pipelines), false)
            .expect("a gate with no on_fail is legal — only worth a warning");
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
            default: "solo".into(),
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
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
            default: "solo".into(),
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
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
            default: "solo".into(),
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
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
            default: "solo".into(),
            pipelines: [("solo".to_string(), pipeline)].into_iter().collect(),
        };

        let task = crate::task::Task::parse(
            repo.queue_dir().join("demo.md"),
            "---\nid: demo\nstage: queued\nskip: [not-a-step]\n---\n## Goal\ndo the thing\n",
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
            "preferences",
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
}
