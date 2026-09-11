//! spoolway — a local-first agent pipeline.
//!
//! The pipeline is a graph of steps (`.spoolway/pipeline.yml`), each handled by a
//! named agent profile (`.spoolway/config.toml`). `spoolway dispatch` walks that
//! graph deterministically and only spends an LLM call where a step actually
//! calls for one. Nothing in the control flow is decided by a model.

mod agent;
mod ask;
mod assets;
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
mod gitignore;
mod globs;
mod graph;
mod headless;
mod install;
mod jobs;
mod lane_prompts;
mod lock;
mod models;
mod mux;
mod pipeline;
mod platform;
mod problem_log;
mod prompt;
mod release;
mod release_notes;
mod repo;
mod retain;
mod runfiles;
#[cfg(test)]
mod scratch;
mod screen;
mod skeleton;
mod spend;
mod status;
mod task;
mod task_log;
mod task_template;
mod teardown;
mod tmux;
mod tracking;
mod update;
mod usage;
mod version;

use std::path::PathBuf;

use anyhow::Result;

use cli::{
    AgentCommand, Cli, Command, ConfigCommand, GroupCommand, IssueCommand, JobsCommand,
    ModelsCommand, PipelineCommand, PromptCommand, QueueCommand, TaskCommand,
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
/// `Ok(n)` for any other `n` exits directly with that code. A restart storm's
/// own [`commands::RestartsRefused`] is downcast out of the error and exits
/// 5; every other error falls through to `run`'s usual `Err` handling above,
/// which prints it and exits 1.
fn exit_dispatch(result: Result<i32>) -> Result<()> {
    match result {
        Ok(0) => Ok(()),
        Ok(code) => std::process::exit(code),
        Err(err) => match err.downcast::<commands::RestartsRefused>() {
            Ok(refused) => {
                eprintln!("spoolway: {refused}");
                std::process::exit(5);
            }
            Err(err) => Err(err),
        },
    }
}

fn run() -> Result<()> {
    use std::io::IsTerminal;

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
    notify(&cli, &cwd);

    match &cli.command {
        // `init` is the one command that runs before a project exists, so it
        // resolves its own root rather than discovering one.
        Command::Init(args) => {
            let root = init_root(&cwd)?;
            commands::init(&root, args)
        }

        // The one command that has to survive a config it cannot read, because
        // it is the command you run to find out what is wrong with it. Every
        // other command below dies on the parse error; `doctor` reports it and
        // checks what it can without it.
        Command::Doctor(args) => {
            let (repo, config_error) = Repo::discover_lenient(&cwd)?;
            // Loaded here rather than with `?`: a pipeline file that does not
            // parse is exactly what `doctor` exists to name, so the load
            // failure is handed in as a finding, the same way `config_error`
            // is, rather than aborting the command on it.
            let pipelines = Pipelines::load(&repo.checkout, &repo.config);
            // `config_error` is about `repo.root`'s file — the one
            // `discover_lenient` reads. `doctor` loads `repo.checkout`'s own
            // copy again for everything it checks, and folds this one in as
            // a finding of its own rather than a gate — see its own doc.
            commands::doctor(&repo, pipelines, config_error, args.verbose, cli.json)
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
            let project = Repo::discover_lenient(&cwd).ok().and_then(|(repo, _)| {
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
        // are what is wrong. `dispatch.default_pipeline` naming a file that is
        // not there refuses the whole set, and `config set` is how you fix it:
        // needing a valid set in order to run it would be a locked door with
        // the key inside. Same reasoning as `doctor` reading a broken config.
        //
        // Every read below is against `repo.checkout`, not `repo.root`: a
        // command answers about the file actually in front of it. `set` is
        // the one exception — it writes `repo.root`, and refuses first if a
        // linked worktree's `checkout` differs from it.
        Command::Config(ConfigCommand::Show) => {
            commands::config_show(&Repo::discover(&cwd)?, cli.json)
        }
        Command::Config(ConfigCommand::List) => {
            commands::config_list(&Repo::discover(&cwd)?, cli.json)
        }
        Command::Config(ConfigCommand::Path) => {
            let repo = Repo::discover(&cwd)?;
            if let Some(note) = repo.checkout_note()? {
                note.print(cli.json)?;
            }
            println!("{}", config::Config::path_in(&repo.checkout).display());
            Ok(())
        }
        Command::Config(ConfigCommand::Get { key }) => {
            commands::config_get(&Repo::discover(&cwd)?, key, cli.json)
        }
        Command::Config(ConfigCommand::Set { key, value }) => {
            commands::config_set(&Repo::discover(&cwd)?, key, value)
        }
        // Lenient on purpose: a config that no longer parses is exactly when
        // you want to open it.
        Command::Config(ConfigCommand::Edit) => {
            let (repo, _) = Repo::discover_lenient(&cwd)?;
            commands::config_edit(&repo.checkout)
        }
        command => {
            let repo = Repo::discover(&cwd)?;

            // Byproducts older than `retention.days` go, once per process —
            // see `retain` for the fixed split between those and the
            // directories a task's own work lives in, which this never
            // touches. Here rather than behind `init`, `doctor` or the
            // `[config]` commands above: those exist to work on a project
            // whose config or layout is in question, and a sweep run ahead
            // of them would be one more thing to rule out.
            retain::sweep_once(&repo);

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
            // commands below never ask — `stack`, `update`, `pipeline check`,
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
                | Command::VersionCheck => {
                    unreachable!("handled above")
                }

                Command::Models { command: None } => models::run(&repo, routing(&graph)?, cli.json),
                Command::Models {
                    command: Some(ModelsCommand::Refresh(args)),
                } => models::refresh(&repo, args),
                // Bare `spoolway eval`, none of `eval`'s own flags and no
                // `--json`: the screen. Any of `eval`'s own flags — including
                // one spelled out to its own default — takes the printing
                // path instead, unchanged. The globals `--repo`/`-C` and
                // `--json` are not `eval`'s own, so neither of them makes an
                // otherwise bare `eval` print; `--json` is turned away by the
                // separate `!cli.json` guard below, which is about the output
                // format rather than about bareness. The screen also needs
                // a real tty: it draws with raw mode and full-screen
                // escapes, which is exactly the frame of garbage
                // `spoolway eval > report.txt` used to write when nothing
                // here checked for a pipe — the same check `paint` and
                // `banner` already make before colouring a line.
                Command::Eval(_)
                    if cli.eval_bare && !cli.json && std::io::stdout().is_terminal() =>
                {
                    eval::screen(&repo, routing(&graph)?)
                }
                Command::Eval(args) => eval::run(&repo, args, cli.json),
                Command::Spend(args) => spend::run(&repo, args, cli.json),
                Command::Report(args) => {
                    // The one place the lane's own step is read. Everything
                    // below takes it as an argument.
                    let started_for = std::env::var(dispatch::ENV_STEP).ok();
                    commands::report(&repo, routing(&graph)?, args, started_for.as_deref())
                }
                Command::Stack(args) => commands::stack(&repo, args),
                Command::Resume(args) => {
                    let in_lane = std::env::var(commands::TASK_ENV).is_ok();
                    commands::resume(&repo, routing(&graph)?, args, in_lane)
                }
                Command::Lane(args) => {
                    let mux = mux::backend(&repo);
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
                // Bare `spoolway queue`, with no subcommand: the screen.
                Command::Queue { command: None } => {
                    commands::queue_screen(&repo, routing(&graph)?, &cwd)
                }
                Command::Queue {
                    command: Some(QueueCommand::List),
                } => commands::queue_list(&repo, routing(&graph)?, cli.json),
                Command::Queue {
                    command: Some(QueueCommand::Show { task }),
                } => commands::queue_show(&repo, task),
                // `cwd`, not `repo.root`: a task is based on the branch of the
                // checkout it was queued in, which is a plan's worktree far
                // more often than it is the main one.
                Command::Queue {
                    command: Some(QueueCommand::Add(args)),
                } => {
                    let in_lane = std::env::var(commands::TASK_ENV).is_ok();
                    commands::queue_add(&repo, routing(&graph)?, args, &cwd, in_lane)
                }
                Command::Queue {
                    command: Some(QueueCommand::Conflicts),
                } => commands::queue_conflicts(&repo, routing(&graph)?),
                Command::Queue {
                    command: Some(QueueCommand::Pause(args)),
                } => commands::queue_pause(&repo, routing(&graph)?, &args.task, args.force),
                Command::Queue {
                    command: Some(QueueCommand::Resume { task }),
                } => commands::queue_resume(&repo, routing(&graph)?, task),

                // `cwd`, for the same reason `queue add` reads it: `--from`
                // resolves `base` the same way, and the cross-base rule it
                // checks depends on that being the checkout this ran in.
                Command::Task(TaskCommand::Contract(args)) => {
                    commands::task_contract(&repo, routing(&graph)?, args, &cwd)
                }

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
                    let read = Pipelines::load(&repo.checkout, &repo.config)?;
                    commands::pipeline_contract(&repo, &read)
                }
                Command::Pipeline(PipelineCommand::Gen(args)) => {
                    let mux = mux::backend(&repo);
                    commands::pipeline_gen(&repo, mux.as_ref(), args)
                }

                Command::Prompt(PromptCommand::Contract(args)) => {
                    prompt::contract(&repo, routing(&graph)?, args)
                }
                Command::Prompt(PromptCommand::List) => {
                    let read = Pipelines::load(&repo.checkout, &repo.config)?;
                    prompt::list(&repo, &read, cli.json)
                }
                Command::Prompt(PromptCommand::Show { name }) => prompt::show(&repo, name),
                Command::Prompt(PromptCommand::Check { name }) => {
                    let read = Pipelines::load(&repo.checkout, &repo.config)?;
                    prompt::check(&repo, &read, name.as_ref(), cli.json)
                }

                Command::Group(GroupCommand::List) => commands::group_list(&repo),

                // Bare `spoolway jobs`, with no subcommand: the screen.
                Command::Jobs { command: None } => {
                    commands::jobs_screen(&repo, routing(&graph)?, &cwd)
                }
                Command::Jobs {
                    command: Some(JobsCommand::List),
                } => commands::jobs_list(&repo, cli.json),
                Command::Jobs {
                    command: Some(JobsCommand::Run(args)),
                } => {
                    let in_lane = std::env::var(commands::TASK_ENV).is_ok();
                    commands::jobs_run(&repo, routing(&graph)?, &args.name, in_lane)
                }

                // Not built yet. Each of these is a slice of work in its own
                // right; the surface is declared so the shape is visible.
                Command::Dispatch(args) => {
                    exit_dispatch(commands::dispatch(&repo, routing(&graph)?, args))
                }
                Command::Install(args) => {
                    let installed = crate::install::install(&repo.root, args.provider, args.force)?;
                    crate::install::report(installed);
                    Ok(())
                }
                Command::Update(args) => update::run(&repo, args),
            }
        }
    }
}

/// The project's routing graph, for a command that actually routes.
///
/// The load itself runs for every command; only the commands below it that
/// name this pay for a project pipeline file that no longer parses. That
/// split is the whole point: `stack`, `update`, `pipeline check` and
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
/// it to.
///
/// In front of every command rather than inside any of them, because "which
/// commands should mention this" has exactly one defensible answer and it is
/// "the ones a person typed". What decides that is [`release::Audience`]; this
/// only gathers the five facts, each of which lives somewhere different.
///
/// The config read is deliberately lenient and deliberately discarded: a
/// project whose config does not parse still gets its notice, and a directory
/// that is no project at all — somebody about to run `init` — gets one too.
fn notify(cli: &Cli, cwd: &std::path::Path) {
    use std::io::IsTerminal;

    // Not in front of the command it would advise. `update` says what it is
    // installing as it installs it, and a line telling somebody to run what
    // they are already running is noise — twice over, since the process an
    // upgrade re-execs would print it again on the way through.
    if matches!(cli.command, Command::Update(_) | Command::WhatsNew(_)) {
        return;
    }

    let enabled = Repo::discover_lenient(cwd)
        .map(|(repo, _)| repo.config.update.check)
        .unwrap_or(true);

    release::notify(release::Audience {
        // The dispatcher exports this into every lane it launches. A lane that
        // reads "Run spoolway update" is a lane that runs it, mid-step, in a
        // worktree it is being reviewed on.
        in_lane: std::env::var_os(dispatch::ENV_STEP).is_some(),
        machine_readable: cli.json,
        tty: std::io::stderr().is_terminal(),
        enabled,
        skipped: std::env::var_os(release::ENV_SKIP).is_some(),
    });
}

/// Where `spoolway init` should place `.spoolway/`: the git toplevel if there is
/// one, otherwise here.
fn init_root(cwd: &std::path::Path) -> Result<PathBuf> {
    match repo::run(cwd, "git", &["rev-parse", "--show-toplevel"]) {
        Ok(out) => Ok(PathBuf::from(out.trim())),
        Err(_) => Ok(cwd.to_path_buf()),
    }
}
