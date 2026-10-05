//! spoolway — a local-first agent pipeline.
//!
//! The pipeline is a graph of steps (`.spoolway/pipeline.yml`), each handled by a
//! named agent profile (`.spoolway/config.toml`). `spoolway dispatch` walks that
//! graph deterministically and only spends an LLM call where a step actually
//! calls for one. Nothing in the control flow is decided by a model.

mod agent;
mod archive_index;
mod ask;
mod assets;
mod claim;
mod cli;
mod command_step;
mod commands;
mod compose;
mod confdoc;
mod config;
mod confkv;
mod cron;
mod dispatch;
mod eval;
mod fmt;
mod gate;
mod gitignore;
mod graph;
mod headless;
mod install;
mod jobs;
mod lane_alias;
mod local;
mod lock;
mod models;
mod mux;
mod overrides;
mod pipeline;
mod platform;
mod problem_log;
mod prompt;
mod release;
mod release_notes;
mod repo;
mod retain;
// The whole module is `#[cfg(test)]` inside — a walk over `commands::
// report::route`, not anything production code calls.
mod route_sim;
mod runfiles;
// Not test-only any more: `doctor`'s live pane check needs a scratch
// directory to open one in, same as a test fixture does — see
// `commands::doctor::live_pane`.
mod scratch;
mod screen;
mod skeleton;
mod spend;
mod status;
mod sync;
mod task;
mod task_template;
mod teardown;
mod tracking;
mod update;
mod usage;
mod version;

use std::path::PathBuf;

use anyhow::Result;

use cli::{
    AgentCommand, Cli, Command, ConfigCommand, GroupCommand, HerdrCommand, HookCommand,
    IssueCommand, JobsCommand, ModelsCommand, OverrideCommand, PipelineCommand, PromptCommand,
    QueueCommand, TaskCommand, TemplateCommand,
};
use pipeline::Pipelines;
use repo::Repo;

fn main() {
    if let Err(err) = run() {
        // `{err:#}` prints the whole anyhow context chain, which is where the
        // useful part of a failure usually lives.
        eprintln!("spoolway: {err:#}");
        std::process::exit(1);
    }
}

/// `dispatch`'s own exit codes, folded back into the uniform `Result<()>`
/// every other command returns.
///
/// `commands::dispatch` answers in a process exit code rather than `()`
/// because three of its endings are not errors at all — an empty queue, a
/// lock already held, a run that dispatched and stopped on its own — and a
/// caller restarting it in a loop has to be able to tell them apart. Settled
/// here, once, rather than in `run`'s own match: every other arm there
/// really is just success or failure, and folding this one command's numbers
/// in among them would make the ordinary arms look like they carried the
/// same distinction.
///
/// `Ok(0)` — a run that dispatched and stopped on its own, or any other
/// ordinary success this command has not given its own code — takes the
/// normal `Ok(())` path below and so exits 0 like every other command.
/// `Ok(n)` for any other `n` exits directly with that code. Every error
/// falls through to `run`'s usual `Err` handling above, which prints it and
/// exits 1.
fn exit_dispatch(result: Result<i32>) -> Result<()> {
    match result {
        Ok(0) => Ok(()),
        Ok(code) => std::process::exit(code),
        Err(err) => Err(err),
    }
}

fn run() -> Result<()> {
    let cli = cli::parse();

    // Before anything else, and before a repo is looked for: this is the
    // detached child a stale cache spawned, not a command anybody ran. It has
    // no project, wants no notice of its own, and its whole job is one npm
    // lookup written to a file.
    if matches!(cli.command, Command::VersionCheck) {
        release::refresh();
        return Ok(());
    }

    // Release history belongs to the installed binary, not to a checkout.
    // Keep it ahead of cwd resolution and project discovery so it works from
    // an empty directory just as `--version` does.
    if let Command::WhatsNew(args) = &cli.command {
        print!("{}", release_notes::whats_new(args.since.as_deref())?);
        return Ok(());
    }

    let cwd = cli.repo.clone().unwrap_or(std::env::current_dir()?);
    // Bare `spoolway` shows the notice as a popup over its screen — see
    // `notify` — so it is held here until the screen is open.
    let update = notify(&cli, &cwd);

    match &cli.command {
        // `init` is the one command that runs before a project exists, so it
        // resolves its own root rather than discovering one.
        Command::Init(args) => {
            let root = init_root(&cwd)?;
            commands::init(&root, args)
        }

        // Installing a binary is a fact about this machine's `PATH`, not
        // about any project — so this runs before a project is even looked
        // for, the same as `WhatsNew` above. `spoolway sync`, further down,
        // is the file work `update` used to also do, and that still needs a
        // real checkout to land in.
        Command::Update(_) => update::run(&cwd),

        // A machine-wide fact — which key runs this plugin's panes, in
        // `~/.config/herdr/config.toml` — not a project one: like `update`,
        // this runs before a project is even looked for.
        Command::Herdr(HerdrCommand::Bind(args)) => commands::herdr_bind(args),
        Command::Herdr(HerdrCommand::Unbind(args)) => commands::herdr_unbind(args),

        // A user-level install is a fact about this person's home, not about
        // any project, so like `update` it runs before a project is looked
        // for — a person can take the skills without ever running `init`.
        Command::Install(args) if args.user => {
            crate::install::report(crate::install::install_user(args.provider, args.force)?);
            Ok(())
        }

        // The one command that has to survive a config it cannot read, because
        // it is the command you run to find out what is wrong with it. Every
        // other command below dies on the parse error; `doctor` reports it and
        // checks what it can without it.
        Command::Doctor(args) => {
            let (repo, config_error, home_error) = Repo::discover_lenient(&cwd)?;
            // Loaded here rather than with `?`: a pipeline file that does not
            // parse is exactly what `doctor` exists to name, so the load
            // failure is handed in as a finding, the same way `config_error`
            // is, rather than aborting the command on it.
            let pipelines = Pipelines::load(&repo.checkout, &repo.config);
            // `clap` already refuses `--no-live` together with `--live` (see
            // `DoctorArgs`), so exactly one of these can be true here.
            let live = if args.no_live {
                commands::LiveCheckMode::Skip
            } else if args.live {
                commands::LiveCheckMode::Forced
            } else {
                commands::LiveCheckMode::Default
            };
            // `config_error` is about `repo.root`'s file — the one
            // `discover_lenient` reads. `doctor` loads `repo.checkout`'s own
            // copy again for everything it checks, and folds this one in as
            // a finding of its own rather than a gate — see its own doc.
            commands::doctor(
                &repo,
                pipelines,
                config_error,
                home_error,
                args.verbose,
                cli.json,
                live,
            )
        }

        // Before a project is discovered, and deliberately: what spoolway can
        // run is a fact about the binary and this machine's PATH, not about
        // any project — and the kind somebody runs this to ask about is
        // precisely the one no profile names yet. So it answers the same
        // standing anywhere. `verify --live` is the one part that wants a
        // project, for a model to run its turn on, and reaches for one
        // leniently: a config that does not parse costs it the default, not
        // the command.
        Command::Agent(AgentCommand::List) => commands::agent_list(cli.json),
        Command::Agent(AgentCommand::Verify(args)) => {
            let project = Repo::discover_lenient(&cwd).ok().and_then(|(repo, _, _)| {
                let pipelines = Pipelines::load(&repo.checkout, &repo.config).ok()?;
                Some((repo, pipelines))
            });
            commands::agent_verify(
                args,
                project.as_ref().map(|(repo, pipelines)| (repo, pipelines)),
                cli.json,
            )
        }

        // The config commands, and the migration, before the pipelines are
        // loaded — because these are the commands you run when the pipelines
        // are what is wrong. A pipeline file that will not parse refuses the
        // whole set, and `config set` is how you fix whatever else is broken
        // in `config.toml`: needing a valid set in order to run it would be a
        // locked door with the key inside. Same reasoning as `doctor` reading
        // a broken config.
        //
        // Every read below is against `repo.checkout`, not `repo.root`: a
        // command answers about the file actually in front of it. `set` and
        // `override promote` are the exceptions that write `repo.root`, and
        // refuse first if a linked worktree's `checkout` differs from it.
        // `sync`, further down, writes too, but against `repo.checkout` —
        // see its own module doc — so it stays in step with every read here
        // rather than joining `set`'s exception. `update` writes no project
        // file at all any more, and is handled above, before a project is
        // even discovered.
        Command::Config(ConfigCommand::Contract) => {
            commands::config_contract(&Repo::discover(&cwd)?, cli.json)
        }
        Command::Config(ConfigCommand::Show) => {
            commands::config_show(&Repo::discover(&cwd)?, cli.json)
        }
        Command::Config(ConfigCommand::List) => {
            commands::config_list(&Repo::discover(&cwd)?, cli.json)
        }
        // Lenient like `config edit`/`override`, but for a different reason:
        // a checkout nothing claims yet is exactly what `config path` is for
        // — the skill reads `mode: null` and the workspace list from it
        // before proposing `spoolway init --workspace <name>`. See
        // `commands::config_path_anywhere`.
        Command::Config(ConfigCommand::Path) => commands::config_path_anywhere(&cwd, cli.json),
        Command::Config(ConfigCommand::Get { key }) => {
            commands::config_get(&Repo::discover(&cwd)?, key, cli.json)
        }
        Command::Config(ConfigCommand::Set { key, value }) => {
            commands::config_set(&Repo::discover(&cwd)?, key, value)
        }
        // Lenient on purpose: a config that no longer parses is exactly when
        // you want to open it.
        Command::Config(ConfigCommand::Edit) => {
            let (repo, _, _) = Repo::discover_lenient(&cwd)?;
            commands::config_edit(&repo.checkout)
        }
        // Lenient for the same reason `config edit` is: an overrides/
        // config.toml that no longer parses is exactly when you want it
        // open.
        Command::Config(ConfigCommand::Override) => {
            let (repo, _, home_error) = Repo::discover_lenient(&cwd)?;
            commands::config_override(&repo, home_error.as_ref())
        }
        command => {
            // `sync` is the one command below that has to survive long
            // enough to reach its own read of `config.toml`, for the same
            // reason `doctor` and `config edit`/`config override` read
            // leniently above: `Repo::discover` would otherwise refuse a
            // file still naming a key this binary retired hard enough that
            // `Config::load` rejects it outright, before `sync` ever got
            // the chance to be the one command that brings a file like that
            // forward (see `crate::sync::config`). A file this lenient
            // `Repo` still cannot even parse as TOML is a different case:
            // `sync::config` fails loudly on that one itself, naming the
            // file, rather than leaving it to look like nothing was wrong.
            // Every other command reaching this arm still dies on a config
            // it cannot read.
            let repo = if matches!(command, Command::Sync(_)) {
                let (repo, _, _) = Repo::discover_lenient(&cwd)?;
                repo
            } else {
                Repo::discover(&cwd)?
            };

            // Whether this checkout has fallen behind a `spoolway update`
            // that already ran, said before the command runs and never in
            // its way: `gate::notify` prints one line and returns. `sync` is
            // the one command this must never print in front of — it *is*
            // the thing the line tells a person to run — so it is excluded
            // by name here rather than left to fall out of the match below.
            // Every other excluded command (`init`, `doctor`, `whats-new`,
            // `update`, `config edit`, `config override`) is answered in an
            // earlier arm of the outer match and never reaches this one at
            // all.
            //
            // Bare `spoolway` shows the same line as a popup over the tab it
            // opens on instead — see `gate::sync_popup` — which reads the
            // project's pipelines first, the one file read ahead of the
            // question. When they do not load, the screen could not open to
            // show it, and it is printed here like every other command's,
            // ahead of the refusal: see `gate::asks_as_popup`.
            let popup = matches!(command, Command::Screen) && gate::asks_as_popup(&repo);
            if !matches!(command, Command::Sync(_)) && !popup {
                gate::notify(
                    &repo,
                    std::env::var_os(dispatch::ENV_STEP).is_some(),
                    cli.json,
                )?;
            }

            // Byproducts older than `housekeeping.retention_days`, and archived
            // tasks older than `housekeeping.archive_retention_days` when that
            // is set, go, once per process — see `retain` for the fixed split
            // between those and the directories a task's own work lives in,
            // which this never touches. Here rather than behind `init`, `doctor` or the
            // `[config]` commands above: those exist to work on a project
            // whose config or layout is in question, and a sweep run ahead
            // of them would be one more thing to rule out.
            //
            // Skipped for the same reason under `sync`: `repo.config` came
            // from `discover_lenient` above, which is `Config::default()`
            // — a made-up `retention_days` — on exactly the config `sync`
            // exists to work on. Sweeping against a fabricated setting
            // before a person's own project is even readable is the thing
            // this comment already rules out for every command above; `sync`
            // reaching this arm at all must not put it back.
            if !matches!(command, Command::Sync(_)) {
                retain::sweep_once(&repo);
            }

            // The graph that *routes* the queue is the project's, read from
            // `repo.root` — never the checkout's. A lane reports from inside
            // its own worktree, and a worktree's committed pipelines are not
            // the graph the dispatcher and the queue agree on: letting a
            // report route off them lets two branches disagree about where a
            // task goes next, and loses any pipeline edit the project has not
            // committed yet. The control-plane readers below load the
            // checkout's own copy instead — that is what they are for.
            //
            // Read here, but not *demanded* here: the failure is handed to
            // `routing` and lands at the call sites that actually route. Most
            // commands below never ask — `stack`, `sync`, `pipeline check`,
            // `queue show` — and a project pipeline file that no longer
            // parses must not be what stops them, for the same reason the
            // config commands above run ahead of this at all. Nothing is more
            // certain to break every one of those at once than a rename
            // landing in a worktree before the project has it.
            let graph = Pipelines::load(&repo.root, &repo.config);

            match command {
                Command::Init(_)
                | Command::Agent(_)
                | Command::Config(_)
                | Command::Doctor(_)
                | Command::WhatsNew(_)
                | Command::VersionCheck
                | Command::Update(_)
                | Command::Herdr(_) => {
                    unreachable!("handled above")
                }

                Command::Models { command: None } => models::run(&repo, routing(&graph)?, cli.json),
                Command::Models {
                    command: Some(ModelsCommand::Refresh(args)),
                } => models::refresh(&repo, args),
                // `--discard` is the one thing `eval` does rather than
                // prints, so it is answered before the printing path and
                // never reaches `eval::run`. It needs the two things a
                // reader never does — the pipelines, to tell a live agent
                // lane from a running command step, and a multiplexer to
                // stop either with.
                Command::Eval(args) if args.discard.is_some() => {
                    eval::refuse_per_run_beside_an_export(args, cli.json)?;
                    let mux = mux::backend(&repo)?;
                    let trial = args.discard.clone().expect("checked by the guard");
                    teardown::discard_trial(
                        &repo,
                        routing(&graph)?,
                        mux.as_ref(),
                        &trial,
                        args.force,
                    )
                }
                Command::Eval(args) => eval::run(&repo, args, cli.json, graph.as_ref().ok()),
                Command::Report(args) => {
                    // The one place the lane's own step is read. Everything
                    // below takes it as an argument.
                    let started_for = std::env::var(dispatch::ENV_STEP).ok();
                    commands::report(&repo, routing(&graph)?, args, started_for.as_deref())
                }
                Command::Stack(args) => commands::stack(&repo, args),
                Command::Resume(args) => {
                    let from_step = std::env::var(dispatch::ENV_STEP).ok();
                    commands::resume(&repo, routing(&graph)?, args, from_step.as_deref())
                }
                Command::Lane(args) => {
                    let mux = mux::backend(&repo)?;
                    let in_lane = std::env::var(commands::TASK_ENV).is_ok();
                    commands::lane_cmd(
                        &repo,
                        routing(&graph)?,
                        mux.as_ref(),
                        args,
                        in_lane,
                        cli.json,
                    )
                }
                // Bare `spoolway` in a terminal: the one screen, holding the
                // dispatch, queue, jobs and eval tabs. Off a terminal no
                // command at all never gets this far — `cli::parse` prints
                // the grouped help instead.
                Command::Screen => screen::shell::run(&repo, routing(&graph)?, &cwd, update),
                Command::Queue(QueueCommand::List) => {
                    commands::queue_list(&repo, routing(&graph)?, cli.json)
                }
                Command::Queue(QueueCommand::Show { task }) => commands::queue_show(&repo, task),
                Command::Queue(QueueCommand::Route { task }) => {
                    commands::queue_route(&repo, routing(&graph)?, task, cli.json)
                }
                Command::Queue(QueueCommand::Add(args)) => {
                    let in_lane = std::env::var(commands::TASK_ENV).is_ok();
                    commands::queue_add(&repo, routing(&graph)?, args, &cwd, in_lane)
                }
                Command::Queue(QueueCommand::Pause(args)) => {
                    commands::queue_pause(&repo, routing(&graph)?, &args.task, args.force)
                }
                Command::Queue(QueueCommand::Resume { task }) => {
                    commands::queue_resume(&repo, routing(&graph)?, task)
                }
                Command::Queue(QueueCommand::Unqueue(args)) => {
                    commands::queue_unqueue(&repo, routing(&graph)?, args)
                }

                // `cwd`, for the bare contract's `base`: the branch the
                // checkout this ran in has out, a worktree's own included.
                Command::Task(TaskCommand::Contract(args)) => {
                    commands::task_contract(&repo, routing(&graph)?, args, &cwd)
                }
                Command::Task(TaskCommand::Edit(args)) => commands::task_edit(&repo, args),

                Command::Issue(IssueCommand::Show(args)) => {
                    commands::issue_show(&repo, &args.reference)
                }

                Command::Pipeline(PipelineCommand::Show) => {
                    let read = Pipelines::load(&repo.checkout, &repo.config)?;
                    commands::pipeline_show(&repo, &read, cli.json)
                }
                Command::Pipeline(PipelineCommand::Check) => {
                    // Not `?`: `pipeline check` is the command that names a
                    // pipeline file that will not parse, so the load failure
                    // is handed in and reported rather than aborting it.
                    let read = Pipelines::load(&repo.checkout, &repo.config);
                    commands::pipeline_check(&repo, read, cli.json)
                }
                Command::Pipeline(PipelineCommand::List) => {
                    let read = Pipelines::load(&repo.checkout, &repo.config)?;
                    commands::pipeline_list(&repo, &read, cli.json)
                }
                Command::Pipeline(PipelineCommand::Contract) => {
                    // Not plain `load`: this command prints the pipeline
                    // file format, which needs no pipeline of this
                    // project's own to exist — see `Pipelines::load_or_empty`.
                    let read = Pipelines::load_or_empty(&repo.checkout, &repo.config)?;
                    commands::pipeline_contract(&repo, &read)
                }
                Command::Pipeline(PipelineCommand::Override(args)) => {
                    commands::pipeline_override(&repo, &args.name, &args.set)
                }
                Command::Pipeline(PipelineCommand::Copy(args)) => {
                    commands::pipeline_copy(&repo, &args.from, &args.to, cli.json)
                }
                Command::Pipeline(PipelineCommand::Promote(args)) => {
                    commands::pipeline_promote(&repo, &args.name, cli.json)
                }

                // Read out of the checkout, like `pipeline show` and the
                // other file readers below — not `routing(&graph)`. A pipeline
                // edit made in a worktree has not landed on the project root
                // yet, and `prompt contract` exists to preview exactly that
                // edit before it does.
                Command::Prompt(PromptCommand::Contract(args)) => {
                    // Not plain `load`: a project with no pipelines yet
                    // gets a pointer to `pipeline contract` instead of a
                    // load failure — see `Pipelines::load_or_empty` and
                    // `prompt::contract`.
                    let read = Pipelines::load_or_empty(&repo.checkout, &repo.config)?;
                    prompt::contract(&repo, &read, args)
                }
                Command::Prompt(PromptCommand::List) => {
                    let read = Pipelines::load(&repo.checkout, &repo.config)?;
                    prompt::list(&repo, &read, cli.json)
                }
                Command::Prompt(PromptCommand::Show { name }) => prompt::show(&repo, name),
                Command::Prompt(PromptCommand::Override { name }) => {
                    commands::prompt_override(&repo, name)
                }
                Command::Prompt(PromptCommand::Copy(args)) => {
                    commands::prompt_copy(&repo, &args.from, &args.to, cli.json)
                }

                Command::Override(OverrideCommand::Contract) => commands::override_contract(),
                Command::Override(OverrideCommand::List) => {
                    commands::override_list(&repo, cli.json)
                }
                Command::Override(OverrideCommand::Promote(args)) => {
                    commands::override_promote(&repo, &args.target)
                }
                Command::Override(OverrideCommand::Drop(args)) => {
                    commands::override_drop(&repo, args.target.as_deref())
                }

                Command::Template(TemplateCommand::Contract) => commands::template_contract(&repo),
                Command::Hook(HookCommand::Contract) => commands::hook_contract(&repo),

                Command::Group(GroupCommand::List) => commands::group_list(&repo),

                Command::Jobs(JobsCommand::Contract) => commands::jobs_contract(&repo, cli.json),
                Command::Jobs(JobsCommand::List) => commands::jobs_list(&repo, cli.json),
                // Refused while parsing, so it never gets this far — see
                // `JobsRunArgs`.
                Command::Jobs(JobsCommand::Run(_)) => {
                    unreachable!("`jobs run` is refused by its argument parser")
                }

                // Not built yet. Each of these is a slice of work in its own
                // right; the surface is declared so the shape is visible.
                Command::Dispatch(args) => {
                    exit_dispatch(commands::dispatch(&repo, routing(&graph)?, args))
                }
                Command::Install(args) => {
                    // A home-mode project promises to write nothing into its
                    // checkout, and a project skill folder sits inside it, so
                    // its skills go to the user folder with or without `--user`.
                    let installed = if crate::repo::workspace_clone(&repo.root).is_some() {
                        crate::install::install_user(args.provider, args.force)?
                    } else {
                        crate::install::install(&repo.root, args.provider, args.force)?
                    };
                    crate::install::report(installed);
                    Ok(())
                }
                Command::Sync(args) => sync::run_asking(
                    &repo,
                    args,
                    cli.json,
                    std::env::var_os(dispatch::ENV_STEP).is_some(),
                ),
            }
        }
    }
}

/// The project's routing graph, for a command that actually routes.
///
/// The load itself runs for every command; only the commands below it that
/// name this pay for a project pipeline file that no longer parses. That
/// split is the whole point: `stack`, `sync`, `pipeline check` and
/// `queue show` do not route, and stopping them because some *other* file
/// under `.spoolway/pipelines/` is unreadable is how `pipeline check` — the
/// command you run to find out which file — came to be unrunnable exactly
/// when it was wanted.
///
/// A fresh [`anyhow::Error`] because the stored one cannot be moved out of a
/// borrow and is not [`Clone`]; `{err:#}` carries the whole context chain, so
/// the message a caller sees is the one [`Pipelines::load`] wrote.
fn routing(graph: &Result<Pipelines>) -> Result<&Pipelines> {
    graph.as_ref().map_err(|err| anyhow::anyhow!("{err:#}"))
}

/// Say, once, that a newer release is out — if there is a person here to say
/// it to. Bare `spoolway` gets the line back instead of printed, for its
/// screen to show as a popup: printed ahead of the screen, it would be wiped
/// by the first frame before anybody could read it.
///
/// In front of every command rather than inside any of them, because "which
/// commands should mention this" has exactly one defensible answer and it is
/// "the ones a person typed". What decides that is [`release::Audience`]; this
/// only gathers the five facts, each of which lives somewhere different.
///
/// The config read is deliberately lenient and deliberately discarded: a
/// project whose config does not parse still gets its notice, and a directory
/// that is no project at all — somebody about to run `init` — gets one too.
fn notify(cli: &Cli, cwd: &std::path::Path) -> Option<String> {
    use std::io::IsTerminal;

    // Not in front of the command it would advise. `update` says what it is
    // installing as it installs it, and a line telling somebody to run what
    // they are already running is noise — twice over, since the process an
    // upgrade re-execs would print it again on the way through.
    //
    // `herdr` is machine-wide too. Its command arms deliberately run before a
    // project is looked for, so the best-effort config read below must not do
    // that lookup first: discovery binds an otherwise-unclaimed checkout,
    // making a keybinding edit silently stamp whichever git repository the
    // caller happened to be standing in.
    if matches!(
        cli.command,
        Command::Update(_) | Command::WhatsNew(_) | Command::Herdr(_)
    ) {
        return None;
    }

    let enabled = Repo::discover_lenient(cwd)
        .map(|(repo, _, _)| repo.config.housekeeping.update_check)
        .unwrap_or(true);

    let audience = release::Audience {
        // The dispatcher exports this into every lane it launches. A lane that
        // reads "Run spoolway update" is a lane that runs it, mid-step, in a
        // worktree it is being reviewed on.
        in_lane: std::env::var_os(dispatch::ENV_STEP).is_some(),
        machine_readable: cli.json,
        tty: std::io::stderr().is_terminal(),
        enabled,
        skipped: std::env::var_os(release::ENV_SKIP).is_some(),
    };
    if matches!(cli.command, Command::Screen) {
        return release::notice(audience);
    }
    release::notify(audience);
    None
}

/// Where `spoolway init` should place `.spoolway/`: the main checkout if
/// `cwd` is a linked worktree of one, the git toplevel if there is one,
/// otherwise here.
///
/// `repo::main_checkout` first, ahead of `toplevel_raw`: a linked
/// worktree's own `git rev-parse --show-toplevel` answers with the
/// worktree itself, which is never where `init` should write — it would
/// set up a second, ignored project there and still stamp the shared
/// `.git`. `main_checkout` answers `Some` of the checkout itself for every
/// ordinary repository too, not only a linked worktree's, so `toplevel_raw`
/// is reached only for a non-git folder or a checkout `main_checkout`
/// cannot yet resolve — a fresh `--separate-git-dir` clone or submodule
/// nothing has stamped or listed in a workspace.
///
/// From the main checkout of such a clone, `toplevel_raw` answers the
/// checkout itself, which is right. From a linked worktree of one it
/// answers the worktree, and git has no way to name the main checkout
/// either (`git worktree list` gives the git directory in its place), so
/// that case is refused: setting up the worktree would register a path the
/// main checkout and every other worktree never find again.
fn init_root(cwd: &std::path::Path) -> Result<PathBuf> {
    if let Some(main) = repo::main_checkout(cwd) {
        return Ok(main);
    }
    if repo::is_linked_worktree(cwd) {
        anyhow::bail!(
            "{} is a linked worktree, and spoolway cannot tell which checkout it was cut from, \
             because its git directory sits outside that checkout\n  run `spoolway init` in \
             the main checkout first, or pass `-C <main checkout>` — every worktree finds the \
             project from there afterwards",
            cwd.display()
        );
    }
    match repo::toplevel_raw(cwd) {
        Ok(top) => Ok(top),
        Err(_) => Ok(cwd.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::PathExt;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap_or_else(|e| panic!("git {args:?} in {dir:?}: {e}"));
        assert!(status.success(), "git {args:?} in {dir:?} failed");
    }

    /// `init_root` picks the folder `spoolway init` writes into. A linked
    /// worktree's own `git rev-parse --show-toplevel` answers with the
    /// worktree itself, not the main checkout it was cut from — so running
    /// `init` from inside one must still resolve to the main checkout,
    /// never the worktree, or it sets up a second, ignored project there.
    #[test]
    fn init_root_from_a_linked_worktree_resolves_to_the_main_checkout() {
        let base = crate::scratch::root("init-root-worktree");
        let _ = std::fs::remove_dir_all(&base);
        let work = base.join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "t"]);
        std::fs::write(work.join("f.txt"), "x").unwrap();
        git(&work, &["add", "f.txt"]);
        git(&work, &["commit", "-q", "-m", "x"]);

        let wt = base.join("task-wt");
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/a",
                wt.to_str().unwrap(),
            ],
        );

        let root = init_root(&wt).unwrap();
        assert_eq!(
            root.canonical().unwrap(),
            work.canonical().unwrap(),
            "`spoolway init` run from a linked worktree must act on the \
             main checkout {work:?}, not the worktree {wt:?} it was \
             resolved from — got {root:?}"
        );
    }

    /// `repo::main_checkout` answers `None`, not a wrong guess, for a fresh
    /// `--separate-git-dir` clone nothing has stamped yet — see that
    /// function's own doc. `init_root` has to fall through to `toplevel_raw`
    /// for that case, which answers the checkout itself correctly, or the
    /// very first `spoolway init` in such a clone would write into — and
    /// stamp — the wrong folder entirely, with no way to ever reach the
    /// real one afterwards.
    #[test]
    fn init_root_falls_through_to_toplevel_for_an_unstamped_separate_git_dir_clone() {
        let base = crate::scratch::root("init-root-separate-git-dir");
        let _ = std::fs::remove_dir_all(&base);
        let git_dir = base.join("elsewhere").join("git");
        std::fs::create_dir_all(git_dir.parent().unwrap()).unwrap();
        let work = base.join("work");
        git(
            &base,
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--separate-git-dir={}", git_dir.display()),
                work.to_str().unwrap(),
            ],
        );
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "t"]);

        let root = init_root(&work).unwrap();
        assert_eq!(
            root.canonical().unwrap(),
            work.canonical().unwrap(),
            "the very first `spoolway init` in a fresh --separate-git-dir \
             clone must still target the checkout itself, not the git \
             directory's own parent — got {root:?}"
        );
    }

    /// From a linked worktree of a `--separate-git-dir` clone that nothing
    /// has stamped or listed, neither `main_checkout` nor git can name the
    /// main checkout, and `toplevel_raw` answers the worktree itself.
    /// `init_root` must refuse rather than hand that worktree to `init`,
    /// which registered it as a project no other checkout could find.
    #[test]
    fn init_root_refuses_a_linked_worktree_whose_main_checkout_cannot_be_found() {
        let base = crate::scratch::root("init-root-separate-git-dir-worktree");
        let _ = std::fs::remove_dir_all(&base);
        let git_dir = base.join("repos").join("foo.git");
        std::fs::create_dir_all(git_dir.parent().unwrap()).unwrap();
        let work = base.join("work");
        git(
            &base,
            &[
                "init",
                "-q",
                "-b",
                "main",
                &format!("--separate-git-dir={}", git_dir.display()),
                work.to_str().unwrap(),
            ],
        );
        git(&work, &["config", "user.email", "t@example.com"]);
        git(&work, &["config", "user.name", "t"]);
        std::fs::write(work.join("f.txt"), "x").unwrap();
        git(&work, &["add", "f.txt"]);
        git(&work, &["commit", "-q", "-m", "x"]);
        let wt = base.join("task-wt");
        git(
            &work,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/a",
                wt.to_str().unwrap(),
            ],
        );

        let err = crate::platform::test_home::with_home(&base.join("home"), || init_root(&wt))
            .expect_err("the worktree must not be taken for the project");
        let said = format!("{err:#}");
        assert!(
            said.contains("linked worktree") && said.contains("main checkout"),
            "the refusal says to run init in the main checkout: {said}"
        );
    }
}
