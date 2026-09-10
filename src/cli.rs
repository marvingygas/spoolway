//! The command surface.
//!
//! Everything the pipeline does deterministically is reachable from here, so
//! that a skill or a role's prompt can say "run this command" instead of
//! describing the procedure in prose an LLM has to re-derive every time.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "spoolway",
    version,
    about = "Local-first agent pipeline: a background dispatcher over local worker lanes",
    long_about = None,
)]
pub struct Cli {
    /// Repo to operate on. Defaults to the current directory's project.
    #[arg(long, short = 'C', global = true, value_name = "DIR")]
    pub repo: Option<PathBuf>,

    /// Machine-readable output, where the command supports it.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,

    /// Whether `eval` was invoked with none of its *own* flags — filled in
    /// by [`parse`] from the raw `ArgMatches`, not derived from `EvalArgs`
    /// itself: `EvalArgs::is_bare` used to enumerate every one of its own
    /// fields by hand, which meant a new flag on `EvalArgs` had to remember
    /// to extend that list too, or bare `spoolway eval` would silently take
    /// the wrong path. [`eval_is_bare`] answers the same question off the
    /// parser's own bookkeeping instead, so nothing here grows when
    /// `EvalArgs` does. `spoolway eval` bare opens the screen; any of
    /// `eval`'s own flags, even one spelled out to its own default, takes
    /// the printing path instead — see `main.rs`. `--repo`/`-C` and
    /// `--json` are excluded on purpose: both are `global = true`, so they
    /// answer a question about the whole invocation, not about `eval`, and a
    /// person piping `-C ~/project` in front of a bare `eval` still wants
    /// the screen.
    #[arg(skip)]
    pub eval_bare: bool,
}

// One of these exists per process; the spread between variants is the args
// structs, and boxing them would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Set up `.spoolway/` in a repo: config, the default pipeline, and prompts.
    Init(InitArgs),

    /// Take what a newer spoolway writes, without touching what you wrote.
    #[command(
        long_about = "Take what a newer spoolway writes, without touching what you wrote.\n\n\
        Three things are brought forward. `config.toml` keeps your values and has every \
        comment and the list of written-down settings around them rewritten. Each pipeline \
        file's fenced key reference — documentation of this binary's contract, not anything \
        you meant — is refreshed in place, with every other line copied through unread. The \
        old `.gitignore` block spoolway used to manage is removed. Skills for every provider \
        you have installed are refreshed too.\n\n\
        Prompts, their assets, and task skeletons are not touched, ever. They are prose a \
        project owns outright, with nothing generated inside them; `spoolway prompt check` \
        is what tells you when one has fallen behind the CLI, and `--replace` is how to take \
        a shipped one back on purpose.\n\n\
        `init` is still how a project starts. This is how one keeps up."
    )]
    Update(UpdateArgs),

    /// Refresh the cached "latest published version" answer, and print nothing.
    ///
    /// Not a verb anybody types. `spoolway` spawns this, detached, when the
    /// cached answer has gone stale — which is what keeps the version check off
    /// the command path entirely. See [`crate::release`].
    #[command(name = crate::release::REFRESH_COMMAND, hide = true)]
    VersionCheck,

    /// Install pipeline skills and agent definitions for a coding agent.
    Install(InstallArgs),

    /// Run the pipeline: a loop that draws the live board until the queue empties.
    Dispatch(DispatchArgs),

    /// Every model this project's pipelines name: window, per-1M rates, source.
    #[command(
        long_about = "Every model an agent step of this project's pipelines names, resolved.\n\n\
            Resolution is this project's own `[models]` table first, by glob, then litellm's \
            refreshed and vendored tables by exact name, then nothing — a model in none is `unknown`, never \
            free. `spoolway doctor` reports the same gap; this is where to see it in full.\n\n\
            Nothing here estimates or edits a price. Set one with:\n  \
            spoolway config set models.'<model-glob>'.input <usd per 1M>"
    )]
    Models,

    /// Read the lane ledger: what versions came to.
    Eval(EvalArgs),

    /// Read the lane ledger back out as a spend summary — by task, group,
    /// step, model, skill, project, month, or one row per lane.
    Spend(SpendArgs),

    /// Report the outcome of a step. This is what prompts call when done.
    Report(ReportArgs),

    /// Hand a task's change over with git and `gh` — no model, no rebase.
    ///
    /// Run in a task's worktree, in order: commits what is uncommitted,
    /// squashes the branch to one commit, pushes with `--force-with-lease`,
    /// opens the pull request against the branch the worktree was cut from
    /// (or reuses the one already there), and registers the GitHub stack.
    /// Never merges anything — landing is a person's call.
    Stack(StackArgs),

    /// Carry a stopped task on, reading off `task.stage()` which kind of stop
    /// it is so you don't have to.
    ///
    /// A task on `blocked` resumes the step it stopped on. A task on `paused`
    /// finished a `gate: true` step and is waiting for you to let it past —
    /// this sends it on to wherever the step's `on_pass` points, or, with
    /// `--reject`, back round by the step's own `on_fail` instead, with your
    /// reason attached for the lane that picks it up — or onto `blocked`,
    /// same as any other fail, when the gated step declares no `on_fail` of
    /// its own; `spoolway pipeline check` warns about a gate shaped that way.
    /// `--stage` overrides either route by hand, and still works on a paused
    /// task: naming a step is you choosing where it goes, gate or no gate.
    Resume(ResumeArgs),

    /// Read what a lane has been doing, answer it, or open its session.
    ///
    /// Bare, or given only a lane name, this reads it — the model-free answer
    /// to "what is it up to", no inference, the way looking at a pane costs
    /// nothing. Under a multiplexer a lane's output is in its pane and this
    /// reads that; headless it is the lane's log, which outlives the lane so
    /// a finished task can still be looked into. Omit the lane too and it
    /// lists the lanes there are.
    ///
    /// `-m` answers a lane that ended its turn on a question for you. Under a
    /// multiplexer you would type into the lane's pane and never need this;
    /// headless there is no pane, so this is how the answer gets in — the
    /// lane's session is resumed with your words and it carries on from where
    /// it asked. Not what a gate uses: a gated lane asks nothing, it reports,
    /// and the approval you give afterwards is `spoolway resume`.
    ///
    /// `--attach` opens the lane's session in this terminal, ready to type
    /// into — the interactive counterpart of `-m`: instead of sending one
    /// line, the lane's own session is reopened here with its whole
    /// conversation. Under a multiplexer the lane's pane already holds the
    /// session, and this says where it is.
    Lane(LaneArgs),

    /// Take in-flight tasks from a colleague's mirror into this queue.
    ///
    /// A move, not a copy: the tasks leave their mirror, so the machine that
    /// had them stops dispatching them. Runs entirely from this side — the
    /// person handing over does not have to be there, or to have done anything.
    Adopt(AdoptArgs),

    /// Mirror this queue to your own ref, so a colleague can adopt from it.
    ///
    /// The only thing that mirrors: run it before handing work over, and run
    /// it again after a colleague has adopted, so this machine lets go of what
    /// they took. Also the place to send unfinished work back to the start
    /// rather than have a colleague inherit it.
    Handover(HandoverArgs),

    /// Work with the task queue. Bare, opens the queue screen.
    Queue {
        #[command(subcommand)]
        command: Option<QueueCommand>,
    },

    /// Print or validate the task-document contract.
    #[command(subcommand)]
    Task(TaskCommand),

    /// Read one issue out of this project's own tracker.
    #[command(subcommand)]
    Issue(IssueCommand),

    /// Inspect or validate the pipeline definitions.
    #[command(subcommand)]
    Pipeline(PipelineCommand),

    /// Ask what agent CLIs this spoolway can run, and check one against its row.
    #[command(subcommand)]
    Agent(AgentCommand),

    /// Write and check the prompts that steps run.
    #[command(subcommand)]
    Prompt(PromptCommand),

    /// Work with groups — the `group:` a task's frontmatter carries.
    #[command(subcommand)]
    Group(GroupCommand),

    /// Read and fire cron jobs — routines the dispatcher runs on a schedule.
    /// Bare, opens the jobs screen a person writes a job from.
    Jobs {
        #[command(subcommand)]
        command: Option<JobsCommand>,
    },

    /// Read or write single config values non-interactively.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Check that everything the configured pipeline needs is actually present.
    Doctor(DoctorArgs),
}

/// How much of the check-up to print.
#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// List every check that ran, not only the failures and the notes.
    #[arg(long, short)]
    pub verbose: bool,
}

/// The top-level help, grouped by who types the command and why.
///
/// clap has no per-subcommand heading — `Arg::help_heading` groups flags, and
/// `Command::subcommand_help_heading` renames the one "Commands:" title for the
/// whole list. So the grouping lives here as data, and [`command`] renders the
/// listing from it. A command missing from this table is a compile-time-green
/// but test-red mistake; see the tests at the bottom of this file.
pub const HELP_GROUPS: &[(&str, &[&str])] = &[
    (
        "Your work:",
        &[
            "queue", "group", "issue", "dispatch", "jobs", "eval", "spend",
        ],
    ),
    ("When something needs you:", &["lane", "resume"]),
    (
        "Shaping the project:",
        &["pipeline", "prompt", "agent", "task", "config", "models"],
    ),
    ("Setting up:", &["init", "install", "update", "doctor"]),
    ("Working with someone else:", &["handover", "adopt"]),
    ("Called by lanes, not by you:", &["report", "stack"]),
];

/// The help layout, with the grouped listing standing in for clap's own
/// "Commands:" block. `{after-help}` is where [`command`] puts that listing,
/// and it sits before the options the way clap's own listing does.
const HELP_TEMPLATE: &str = "\
{about-with-newline}
{usage-heading} {usage}{after-help}
Options:
{options}";

/// The parser, with the top level's command listing grouped by [`HELP_GROUPS`].
///
/// Every real subcommand is hidden from clap's own listing, and the listing is
/// written out group by group instead. Hiding changes nothing but this one
/// screen: each command still parses, and `spoolway <command> --help` is
/// untouched.
pub fn command() -> clap::Command {
    use clap::CommandFactory;

    let base = Cli::command();
    let listing = grouped_listing(&base);
    let names: Vec<String> = base
        .get_subcommands()
        .filter(|sc| !sc.is_hide_set())
        .map(|sc| sc.get_name().to_string())
        .collect();
    let mut cmd = base;
    for name in names {
        cmd = cmd.mut_subcommand(name, |sc| sc.hide(true));
    }
    cmd.help_template(HELP_TEMPLATE)
        .after_help(listing)
        // Hiding every subcommand also drops `<COMMAND>` from the generated
        // usage line, so it is written out here instead.
        .override_usage("spoolway [OPTIONS] <COMMAND>")
}

/// Parse the command line through [`command`], so the grouped help is what
/// `spoolway --help` prints.
pub fn parse() -> Cli {
    use clap::FromArgMatches;

    let base = command();
    let matches = base.clone().get_matches();
    let eval_bare = eval_is_bare(&base, &matches);
    let mut cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };
    cli.eval_bare = eval_bare;
    cli
}

/// Whether the `eval` subcommand, if that is what was typed, carried none of
/// its *own* flags — read off the raw `ArgMatches` rather than `EvalArgs`'s
/// fields, so this never has to change when a flag is added to or removed
/// from `EvalArgs`. `None` (no `eval` subcommand at all) is not bare: this is
/// only ever consulted once `main.rs` has already matched `Command::Eval`.
///
/// `ArgMatches::args_present()` on `eval`'s own submatch is not enough on its
/// own: `--repo`/`-C` and `--json` are declared `global = true` on [`Cli`]
/// (so they can be typed after the subcommand, e.g. `eval --json`, and not
/// only before it), and clap copies a typed global into every subcommand's
/// matches — so `args_present()` sees one and reports "not bare" even though
/// nothing about `eval` itself was asked for. Filtering to arguments this
/// subcommand actually declares (`!a.is_global_set()`) and checking each was
/// really typed on the command line, rather than merely defaulted, is what
/// tells `spoolway -C ~/project eval` apart from `spoolway eval --csv`.
fn eval_is_bare(command: &clap::Command, matches: &clap::ArgMatches) -> bool {
    let Some(sub) = command.find_subcommand("eval") else {
        return false;
    };
    let Some(sub_matches) = matches.subcommand_matches("eval") else {
        return false;
    };
    !sub.get_arguments().any(|arg| {
        !arg.is_global_set()
            && sub_matches.value_source(arg.get_id().as_str())
                == Some(clap::parser::ValueSource::CommandLine)
    })
}

/// The command listing, one block per group, names padded to one column.
fn grouped_listing(base: &clap::Command) -> String {
    let about = |name: &str| -> String {
        base.get_subcommands()
            .find(|sc| sc.get_name() == name)
            .and_then(|sc| sc.get_about())
            .map(|about| about.to_string())
            .unwrap_or_default()
    };

    let width = HELP_GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|name| name.len())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for (heading, names) in HELP_GROUPS {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(heading);
        out.push('\n');
        for name in *names {
            let about = about(name);
            if about.is_empty() {
                out.push_str(&format!("  {name}\n"));
            } else {
                out.push_str(&format!("  {name:<width$}  {about}\n", width = width));
            }
        }
    }
    out
}

/// What a spend-table row groups. Each answers a different question: `step`
/// says where the pipeline's spend goes, `model` what each one costs to run,
/// `task` and `group` what a piece of work came to, and `lane` drops the
/// grouping for one row per lane, newest last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SpendBy {
    Task,
    Group,
    Step,
    Model,
    /// Which skill a session was running. Every lane folds into one
    /// `pipeline` row, so the skills can be compared against it.
    Skill,
    /// Which project the lane ran in. The useful cut once `--all` is on.
    Project,
    /// Calendar month, local time. What a monthly bill is grouped by.
    Month,
    /// One row per lane, newest last, instead of a grouped summary.
    Lane,
}

#[derive(Debug, Args)]
#[command(
    long_about = "Compare versions of your pipeline and prompts by what they cost to run.\n\n\
        A version is a fingerprint of the tracked `.spoolway/` configuration — the config, \
        the pipelines and the prompts. Every edit to any of them mints a new one, and every \
        lane is banked under the version it ran under, so the ledger already holds the \
        comparison.\n\n\
        One block per pipeline, its versions inside it newest first. Read RUNS before \
        believing a delta: a version that ran four runs can swing a long way on luck alone, \
        and the column is there to say so.\n\n\
        `spoolway spend` is a different read of the same ledger: a table grouped by task, \
        group, step, model, project, month or skill, or one row per lane. `eval --by` still \
        works as a deprecated alias for it.",
    after_long_help = "\x1b[1mExamples:\x1b[0m\n  \
        spoolway eval                       every pipeline, the last ten versions\n  \
        spoolway eval --pipeline default    one pipeline's block\n  \
        spoolway eval --step review         one step, across every pipeline\n\n  \
        spoolway eval --since 30d           a window\n  \
        spoolway eval --limit 3             fewer versions per block\n\n  \
        spoolway eval --runs                one row per run, newest last\n  \
        spoolway eval --csv > report.csv    the same rows, flat\n\n\
        \x1b[1mReading a row:\x1b[0m\n  \
        RUNS counts every run that touched the row, so a run straddling a version edit \n  \
        counts in both; a footer line says how many. PASS is the share of lanes that \n  \
        reported pass. L/RUN, USD/RUN and TIME/RUN are each figure's total over RUNS, \n  \
        and BLOCKS is how many lanes ended blocked. CTX PEAK is the largest context reading \n  \
        any lane on the row banked, as a share of that model's window.\n\n\
        The spend table moved to its own command: see `spoolway spend --help`."
)]
pub struct EvalArgs {
    /// One pipeline's block only.
    #[arg(long, value_name = "NAME")]
    pub pipeline: Option<String>,

    /// One step's rows only.
    #[arg(long, value_name = "STEP")]
    pub step: Option<String>,

    /// Start of the window: a duration ago (`24h`, `7d`), a local date, or a
    /// whole month (`2026-08`).
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// End of the window, same forms. A date includes the whole of that day,
    /// a month the whole of that month.
    #[arg(long, value_name = "WHEN")]
    pub until: Option<String>,

    /// Deprecated, and only meaningful alongside `--by`: `spoolway spend
    /// --month` is where this lives now.
    #[arg(long, value_name = "YYYY-MM", conflicts_with_all = ["since", "until"], requires = "by")]
    pub month: Option<String>,

    /// How many versions to show per pipeline block. Ten by default.
    ///
    /// `Option` rather than a `default_value_t`: `EvalArgs::limit()` is
    /// where the default of 10 actually applies, so this field itself stays
    /// `None` until a person types the flag.
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,

    /// Every project spoolway knows about, not just this one.
    #[arg(long, conflicts_with = "project")]
    pub all: bool,

    /// One named project — its directory name, or its path.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,

    /// One row per run instead of a grouped summary — task, when, version,
    /// pipeline, lanes, pass, blocks, ctx peak, out, cost, time.
    #[arg(long)]
    pub runs: bool,

    /// `--runs` only: one task's runs. A trial's arms are separate tasks
    /// (`solo-1`, `solo-2`, …), so this is not how to see a trial side by
    /// side — that is `--runs --trial <id>`.
    #[arg(long, value_name = "ID", requires = "runs")]
    pub task: Option<String>,

    /// `--runs` only: only runs whose task carries this `group:`.
    #[arg(long = "group", value_name = "GROUP", requires = "runs")]
    pub run_group: Option<String>,

    /// `--runs` only: one trial's arms, side by side on pass rate, cost and
    /// time — the comparison a trial exists to answer. Named by the trial id
    /// every arm banks, minted once when the queue screen's `p` picker forks
    /// the arms; see `commands::queue::begin_trial`.
    #[arg(long, value_name = "ID", requires = "runs")]
    pub trial: Option<String>,

    /// The same rows this would print, as CSV.
    #[arg(long)]
    pub csv: bool,

    /// Deprecated: the spend table moved to `spoolway spend`. Kept, with a
    /// note to stderr pointing there, so a script or skill still calling
    /// `eval --by` keeps working. A plain string rather than `SpendBy`,
    /// because there is no longer a sentinel variant to fill a bare `--by`
    /// with — `""` (what `default_missing_value` fills a valueless `--by`
    /// with) means "auto" and everything else is parsed as a cut by
    /// `eval::run`.
    #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "CUT", hide = true)]
    pub by: Option<String>,
}

impl EvalArgs {
    /// How many versions a pipeline block shows — `--limit`, or ten when it
    /// was not given. The one place the default actually applies, now that
    /// `limit` itself is `None` until a person types the flag.
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(10)
    }
}

#[derive(Debug, Args)]
#[command(
    long_about = "Read the lane ledger back out as a spend summary, grouped by task, group, \
        step, model, project, month or skill, or one row per lane.\n\n\
        Every figure is collected rather than estimated: token counts come from the \
        transcripts the agents themselves wrote, and a model with no configured price is \
        reported as unpriced rather than counted as free. A ledger lives under its own \
        project's home, at `~/.spoolway/<project>/usage.jsonl`, so this reads the project you \
        are standing in; `--all` and `--project` reach past that, through the index of \
        projects that `init` and `dispatch` keep.\n\n\
        Bare, with no cut named, this still picks one: `step`, or `project` when more than \
        one project is in scope.",
    after_long_help = "\x1b[1mExamples:\x1b[0m\n  \
        spoolway spend                              this project, by step\n  \
        spoolway spend task                         what each task came to\n  \
        spoolway spend lane                         one row per lane, newest last\n\n  \
        spoolway spend --all                        every project, one row each\n  \
        spoolway spend step --all                   where the spend goes, everywhere\n  \
        spoolway spend group --project webshop      one named project\n\n  \
        spoolway spend --month 2026-08              one calendar month, local time\n  \
        spoolway spend month --all                  what each month came to\n  \
        spoolway spend skill                        skills against the pipeline\n  \
        spoolway spend --since 7d                   a duration back from now\n  \
        spoolway spend --since 2026-06-01 --until 4h   dates and durations, either end\n\n  \
        spoolway spend --csv > report.csv           the same rows, flat\n\n  \
        IN, OUT, CACHE R, CACHE W are the four priced token classes, which are disjoint.\n  \
        COST of `-` means no price is configured for that model; the note below the table\n  \
        says when part of a total could not be priced, and names the model responsible.\n  \
        WALL is how long lanes were open, not model time.\n\n  \
        Set a price with: spoolway config set models.'<model-glob>'.input <usd per 1M>"
)]
pub struct SpendArgs {
    /// What to group by. Bare, this resolves to `step`, or `project` when
    /// more than one project is in scope. A plain positional, not a flag: a
    /// cut is what this command is for, not an option that narrows it.
    #[arg(value_enum)]
    pub by: Option<SpendBy>,

    /// Start of the window: a duration ago (`24h`, `7d`), a local date, or a
    /// whole month (`2026-08`).
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// End of the window, same forms. A date includes the whole of that day,
    /// a month the whole of that month.
    #[arg(long, value_name = "WHEN")]
    pub until: Option<String>,

    /// One whole calendar month, local time, as `2026-08`. Shorthand for the
    /// `--since`/`--until` pair that bounds it.
    #[arg(long, value_name = "YYYY-MM", conflicts_with_all = ["since", "until"])]
    pub month: Option<String>,

    /// Every project spoolway knows about, not just this one.
    #[arg(long, conflicts_with = "project")]
    pub all: bool,

    /// One named project — its directory name, or its path.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,

    /// The same rows this would print, as CSV.
    #[arg(long)]
    pub csv: bool,
}

// `Default` is what a test scaffolds with, and it is the *non-interactive*
// answer to every question: no flag given, so each one falls through to the
// person who is not there and then to the default. That is the same shape a
// script gets, which is what makes a test written this way test the path a
// script takes.
#[derive(Debug, Args, Default)]
#[command(
    long_about = "Scaffold a project: config, pipelines, prompts, skeletons, ignore rules \
        — and the five answers that would otherwise be edited in afterwards.\n\n\
        `--agent`, `--model`, `--tracker` and `--project-key` are asked at a terminal and \
        skipped everywhere else, so a script that runs `init` gets the defaults and no \
        prompt. Give any of them as a flag and it is not asked either. `--provider` is asked \
        every time, fresh project or not — installing skills is worth doing even for a \
        project that has everything else already.\n\n\
        `--provider` is the coding agent *you* plan in, and decides only where the skills \
        land. `--agent` is what a lane runs, which is a different question with a different \
        answer — planning in claude while the pipeline's local steps run on pi is the \
        arrangement this project itself uses.\n\n\
        `--agent` settles one profile, `agents.pi`, and nothing else. It is not a \
        project-wide choice of agent: a step names an agent *profile*, a profile names a \
        kind, and a project may define as many profiles of as many kinds as it likes — the \
        shipped config already ships a second one, `agents.claude`, running claude. So a \
        pipeline is free to run one step on pi, the next on codex and the next on claude. \
        Add or change a profile with `spoolway config set`; see `docs/agents.md`.\n\n\
        `--tracker` names the issue tracker `[issue_tracking]` points at — `github`, `jira` \
        or `none` — and `--project-key` is the project its tickets open into. Every hook \
        script is written whichever answer this is, so switching trackers later is a \
        `spoolway config set issue_tracking.hook` away, not a second `init`."
)]
pub struct InitArgs {
    /// Overwrite existing config, pipeline, and prompt files.
    #[arg(long)]
    pub force: bool,

    /// Claim this project's name even though the machine's record of it
    /// still holds an archive or queued tasks, when the checkout that name
    /// was registered to no longer exists. Refused without this flag —
    /// nothing here is deleted, but a state that old is not yours to walk
    /// into by accident.
    #[arg(long)]
    pub take_over: bool,

    /// The coding agent you plan in, whose convention the skills are installed
    /// under. Asked at a terminal; `claude` when there is nobody to ask.
    #[arg(long, value_enum)]
    pub provider: Option<Provider>,

    /// The agent kind the pipeline's local steps run on, as `agents.pi.kind`
    /// would name it — that one profile only, not the project. Other steps run
    /// on other profiles, of any launchable kind. Asked at a terminal; `pi`
    /// when there is nobody to ask.
    #[arg(long, value_name = "KIND")]
    pub agent: Option<String>,

    /// The model those local steps name, written into the pipelines in place of
    /// the placeholder they ship with.
    ///
    /// spoolway names no model of its own, so left unset this stays the
    /// placeholder — which every check that asks whether a step names something
    /// is satisfied by, and no lane can actually run.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Which issue tracker `[issue_tracking]` names. Asked at a terminal, on
    /// a fresh project only; `none` when there is nobody to ask, which
    /// writes every hook script but leaves the table empty.
    #[arg(long, value_enum)]
    pub tracker: Option<Tracker>,

    /// The project the chosen tracker's tickets open into: `owner/repo` on
    /// github, a project key on jira. Ignored when `--tracker` answers
    /// `none` or is not given at all.
    #[arg(long, value_name = "KEY")]
    pub project_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Which coding agent to install pipeline skills for.
    #[arg(value_enum, default_value_t = Provider::Claude)]
    pub provider: Provider,

    /// Overwrite files that already exist.
    #[arg(long)]
    pub force: bool,
}

/// A coding agent a person plans in, named by where it looks for skills.
///
/// Not the same list as `agent::ADAPTERS`, and deliberately: that one is what
/// spoolway can *launch*, this one is what a person can plan in. A kind can
/// join either without joining the other.
///
/// Every variant here is one directory per skill holding a `SKILL.md`, because
/// all three converged on the Agent Skills spec — so the only thing a variant
/// carries is a root. See [`crate::install`] for where each one was read off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Provider {
    /// One directory per skill under `.claude/skills/`, each holding a SKILL.md.
    Claude,
    /// The same, under `.agents/skills/`.
    Codex,
    /// The same, under `.pi/skills/` — loaded once the project is trusted.
    Pi,
}

/// Which issue tracker `[issue_tracking]` names, and which hook script pair
/// `spoolway init` points it at.
///
/// The menu `commands::init::Answers::tracker` builds off this copies the
/// shape [`Provider`]'s own menu already uses: one entry per variant, each
/// carrying a derived note — here, whether the tool the tracker's script
/// calls is on `PATH`. `None` sorts last on purpose: it is the default for a
/// script with nobody to ask, so a scripted `init` gets the same "no issue
/// tracking" behaviour a project had before this existed, not a hook nobody
/// asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Tracker {
    /// `.spoolway/hooks/github.sh` (the `.ps1` twin on native Windows),
    /// calling `gh`.
    Github,
    /// `.spoolway/hooks/jira.sh` (or `.ps1`), calling `acli`.
    Jira,
    /// No hook is named. The scripts are written all the same — see
    /// `commands::init` — so turning tracking on later is a config edit, not
    /// a second `init`.
    None,
}

impl Tracker {
    pub fn name(self) -> &'static str {
        match self {
            Tracker::Github => "github",
            Tracker::Jira => "jira",
            Tracker::None => "none",
        }
    }

    /// The command-line tool this tracker's script calls, so `init`'s menu
    /// can say whether it is on `PATH` — `None` for `none`, which calls
    /// nothing.
    pub fn binary(self) -> Option<&'static str> {
        match self {
            Tracker::Github => Some("gh"),
            Tracker::Jira => Some("acli"),
            Tracker::None => None,
        }
    }

    /// The `[issue_tracking].hook` value this answer writes: a bare filename
    /// inside `.spoolway/hooks/`, in the extension this platform's own pair
    /// actually runs — `.sh` wherever `platform::shell_command` reaches for
    /// `sh -c`, `.ps1` wherever it reaches for PowerShell instead. Blank for
    /// `none`, which is what turns issue tracking off.
    pub fn hook_name(self) -> String {
        let ext = if cfg!(windows) { "ps1" } else { "sh" };
        match self {
            Tracker::Github => format!("github.{ext}"),
            Tracker::Jira => format!("jira.{ext}"),
            Tracker::None => String::new(),
        }
    }
}

#[derive(Debug, Args, Default)]
#[command(long_about = "Run the pipeline, and show what it is doing.\n\n\
        A resident run holds the terminal it was started in: each pass starts and tears down \
        what the queue calls for, and between passes the live board is drawn there — a line per \
        task with the step it is on and where it goes next, ordered top to bottom within its \
        group by the order its tasks will run in. `ctrl-c` stops the run and leaves the last \
        frame on screen.\n\n\
        `--plain` keeps the loop but prints a line per pass instead of drawing. `--dry-run` \
        reports what one pass would do and changes nothing.")]
pub struct DispatchArgs {
    /// Override the configured interval between passes, e.g. `5m`.
    #[arg(long, value_name = "DURATION")]
    pub interval: Option<String>,

    /// Report what one pass would do without spawning anything or writing to
    /// task files.
    ///
    /// A single pass, because a dry run archives nothing: looping would report
    /// the same untouched queue for as long as you let it.
    #[arg(long)]
    pub dry_run: bool,

    /// Print a line per pass instead of drawing the live board.
    ///
    /// The board owns the terminal and redraws about once a second, which
    /// is what you want in front of you and not what you want in a pipe, a CI
    /// log or a terminal that mangles the redraw. `--dry-run` prints lines
    /// regardless: there is no run to watch.
    #[arg(long)]
    pub plain: bool,

    /// Stop for nobody: a block starts a lane on `blocked` instead of parking
    /// the task in front of a person. Every pipeline stages `blocked` —
    /// `Pipelines::assemble` materialises one from `[unattended]`'s `blocked_*`
    /// keys onto any pipeline that does not declare its own — so this always has
    /// a step to route to. The lane is staffed by `unattended.blocked_agent` and
    /// its four companion keys (or by the five keys a declared `blocked` step
    /// overrides), `loop` applies to it as usual, and nothing bounds how many
    /// times it round-trips. When that lane passes, `unattended.skip_blocked_lane`
    /// decides where the task lands: on by default, it carries one step past
    /// where the block was hit, on the unblocker's word that the work is done;
    /// set `false`, it hands the task back to the step it blocked on to run
    /// again. A step's `gate:` is not one of the things this lifts — a gate is a
    /// person's decision by design, so a gated pass still parks on `paused` and
    /// still waits for `spoolway resume`.
    ///
    /// Overrides `unattended.enabled` for this run only, which is the shape
    /// the setting wants — the overnight run and the one you sit with are the
    /// same project on the same afternoon. `--attended` is the other direction,
    /// for a project that leaves the config on.
    ///
    /// With neither `unattended.max_output_tokens` nor `unattended.max_cost_usd`
    /// set this run has no ceiling of any kind: no block can park a task, so
    /// nothing but you, a gate, or an empty queue with no job waiting on it
    /// ends it.
    #[arg(long, overrides_with = "attended")]
    pub unattended: bool,

    /// Park blocked tasks in front of a person for this run, whatever
    /// `unattended.enabled` says.
    #[arg(long, overrides_with = "unattended")]
    pub attended: bool,

    /// Start anyway, past the restart guard: four starts in a row that could
    /// not run at all, inside 30 seconds, ordinarily refuse a fifth.
    ///
    /// Clears the count on this repo, the same as a start that actually
    /// runs — whatever storm was building is over, one way or another.
    #[arg(long)]
    pub force: bool,
}

impl DispatchArgs {
    /// Whether this run stops for a person: the flags first, the config when
    /// neither was given.
    ///
    /// `overrides_with` on the pair means the last flag typed wins rather than
    /// clap refusing the combination, so `--unattended --attended` is the
    /// attended run somebody most recently asked for.
    pub fn unattended(&self, config: &crate::config::Config) -> bool {
        match (self.unattended, self.attended) {
            (true, _) => true,
            (_, true) => false,
            _ => config.unattended.enabled,
        }
    }
}

#[derive(Debug, Args)]
pub struct StackArgs {
    /// Task id. Defaults to `$SPOOLWAY_TASK`, which a command step's worktree
    /// has set.
    pub task: Option<String>,
}

#[derive(Debug, Args)]
pub struct ReportArgs {
    /// Task id. Defaults to `$SPOOLWAY_TASK`, which every lane has set.
    pub task: Option<String>,

    /// The step succeeded; advance along `on_pass`.
    #[arg(long, group = "outcome")]
    pub pass: bool,

    /// The step failed; route along `on_fail`.
    #[arg(long, group = "outcome")]
    pub fail: bool,

    /// Something outside this step's control is in the way; escalate.
    #[arg(long, group = "outcome")]
    pub block: bool,

    /// Nothing short of a person can clear this. Only means anything on
    /// `blocked`; refused on every other step. Parks the task on `paused`
    /// with the same destination a pass would have reached, waiting for
    /// `spoolway resume`.
    #[arg(long, group = "outcome")]
    pub pause: bool,

    /// One line on what happened, recorded in the task's status log.
    #[arg(long, short = 'm', value_name = "TEXT")]
    pub message: Option<String>,

    /// One thing the next step should know, that does not belong in `-m`.
    /// Repeat for each. Written into the task file's `## Handoff` as
    /// `` - `<step>` — <text> `` in the same save that routes the task —
    /// alongside `--pass`, `--fail` or `--block`, and whatever the outcome.
    #[arg(long, value_name = "TEXT")]
    pub handoff: Vec<String>,
}

#[derive(Debug, Args)]
pub struct AdoptArgs {
    /// Whose mirror to take from: their git `user.email`.
    #[arg(long, value_name = "EMAIL")]
    pub from: String,

    /// Take every task of this group.
    #[arg(long, value_name = "GROUP")]
    pub group: Option<String>,

    /// Take these tasks by id. Without either filter, takes everything.
    pub tasks: Vec<String>,
}

#[derive(Debug, Args)]
pub struct HandoverArgs {
    /// Hand over only this group's tasks.
    #[arg(long, value_name = "GROUP")]
    pub group: Option<String>,

    /// Send tasks that never reached `handover` back to the start of the pipeline.
    ///
    /// Their commits are on this machine and nowhere else, so a colleague
    /// cannot have them; the task file's goal and acceptance criteria can be
    /// worked from instead, which is usually the better trade anyway.
    #[arg(long)]
    pub reset_unpublished: bool,
}

#[derive(Debug, Args)]
pub struct LaneArgs {
    /// The lane, named `<task> · <step>`. Omit to list the lanes there are.
    pub lane: Option<String>,

    /// How many lines from the end.
    #[arg(long, short = 'n', default_value_t = 100, conflicts_with_all = ["message", "attach"])]
    pub lines: usize,

    /// Answer a lane that ended its turn on a question.
    #[arg(
        long,
        short = 'm',
        value_name = "WORDS",
        conflicts_with = "attach",
        requires = "lane"
    )]
    pub message: Option<String>,

    /// Open its session in this terminal, ready to type into.
    #[arg(long, conflicts_with = "message", requires = "lane")]
    pub attach: bool,
}

#[derive(Debug, Args)]
pub struct ResumeArgs {
    /// Task id to resume.
    pub task: String,

    /// Step to resume at, overriding the route. On a blocked task this
    /// replaces the step it stopped on; on a paused task it reroutes past the
    /// gate entirely, to a step of your choosing.
    #[arg(long, value_name = "STEP")]
    pub stage: Option<String>,

    /// Send it back round instead of past the gate: the gated step's
    /// `on_fail` route, with your message written into the task file's `##
    /// Handoff` for the lane that answers it — or `blocked`, same as any
    /// other fail, when the step declares no `on_fail` of its own. Only
    /// makes sense against a task on `paused`.
    #[arg(long)]
    pub reject: bool,

    /// Note recorded in the task's status log — and, with `--reject`, in
    /// `## Handoff` for the lane that answers it.
    #[arg(long, short = 'm')]
    pub message: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum QueueCommand {
    /// Whether a dispatcher is running, and where every task is sitting.
    List,

    /// Print one task file.
    Show { task: String },

    /// Queue whole task documents — the only way a task enters the queue.
    Add(QueueAddArgs),

    /// Check queued tasks for overlapping `touches` globs.
    Conflicts,

    /// Interrupt any live agent lane the task owns, then park it on `paused`
    /// — the same interrupt-and-park `p` does on the board, but unconditional
    /// rather than skipped when there was nothing live to interrupt: a
    /// script naming a task has already decided it wants it paused, whatever
    /// state it is in.
    ///
    /// A running command step is refused rather than acted on, unless
    /// `--force` says to kill it — the board asks a person which with its own
    /// confirm panel, and a script has nobody there to ask.
    Pause(QueuePauseArgs),

    /// What the board's `r` key does to one row, from a script: send it past
    /// a gate it finished, or back onto the step a park or a block pulled it
    /// off of — exactly `spoolway resume <id>` with no other flags.
    Resume { task: String },
}

#[derive(Debug, Args)]
pub struct QueuePauseArgs {
    /// Task id to pause.
    pub task: String,

    /// Stop a running command step and pause anyway, throwing away its work
    /// in progress — the same trade the board's own `k` makes on its confirm
    /// panel. Without it, a task running one is refused rather than killed
    /// on a script's say-so.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct QueueAddArgs {
    /// A task document to queue: a file, a directory of `*.md` files (read in
    /// filename order), or `-` for a `---`-separated stream on standard
    /// input. Repeatable — every document named across every `--from` is
    /// validated together and written all or none, so one naming a sibling
    /// queued in the same breath is satisfied with nothing sorted first.
    ///
    /// Each document is `---\n<frontmatter>\n---\n<body>`, the same shape a
    /// queued task is kept in. `id`, `touches`, `depends_on`, `parallel`,
    /// `group`, `source`, `plan`, `pipeline` and `gate_at` are a document's to set;
    /// `stage`, `run`, `attempts`, `base_commit` and `cut_from` are
    /// spoolway's alone, and a document setting one is refused by name. `base` is not a document key
    /// at all — the checkout this command runs in answers for it. Any other
    /// key survives untouched, for a project's own metadata.
    ///
    /// Omitted entirely, nothing is queued: the pipeline's skeleton document
    /// is printed instead, for a person to save, fill in, and hand back
    /// through this same flag.
    #[arg(long = "from", value_name = "PATH")]
    pub from: Vec<String>,
}

/// Print or validate the task-document contract — the same shape
/// [`PromptCommand::Contract`] takes for a prompt.
#[derive(Debug, Subcommand)]
pub enum TaskCommand {
    /// Print or validate the task-document contract itself.
    ///
    /// Bare, prints the whole contract as JSON — this project's default
    /// pipeline, every key a document may set, every key it may not, one
    /// sentence per settable key on how to fill it, each pipeline's id
    /// budget, the step ids `gate_at` accepts and the body skeleton a task
    /// on it is written from, and the rules that only hold across a set —
    /// so a producer that has never seen the planning skills can write
    /// queueable tasks from it alone. `--from` checks a document against
    /// that same contract and writes nothing, whichever way it comes out:
    /// this is `queue add --from`'s own validation, run with nothing saved
    /// at the end of it.
    Contract(TaskContractArgs),
}

#[derive(Debug, Args)]
pub struct TaskContractArgs {
    /// A document to check, in any of the shapes `queue add --from` reads: a
    /// file, a directory of `*.md` documents, or `-` for a `---`-separated
    /// stream on standard input. Repeatable, checked together as one set —
    /// the same batch `queue add --from` would validate before writing.
    ///
    /// Omitted entirely, nothing is checked: the whole contract is printed
    /// as JSON instead.
    #[arg(long = "from", value_name = "PATH")]
    pub from: Vec<String>,
}

/// Read one issue out of this project's own tracker.
#[derive(Debug, Subcommand)]
pub enum IssueCommand {
    /// Read one issue through the `[issue_tracking]` hook's `fetch` event,
    /// and print it as JSON — `ref`, `url`, `title`, `state`, `labels`,
    /// `body` and `comments`. Writes nothing; the same synchronous run
    /// `queue add` gives the hook's own `open` event, blocking until the
    /// hook is done rather than leaving anything to a later pass.
    ///
    /// Refused, by name, when no hook is configured at all, or when the
    /// configured hook's own script has no `fetch` branch — `spoolway
    /// doctor` reports the second of those too, for every install whose
    /// hook predates this event.
    Show(IssueShowArgs),
}

#[derive(Debug, Args)]
pub struct IssueShowArgs {
    /// The issue's own reference, in whatever shape the tracker and its
    /// hook script expect — a bare number on GitHub, a key like `PROJ-123`
    /// on Jira. Never parsed by spoolway itself; handed to the hook exactly
    /// as typed, as `SPOOLWAY_REF`.
    pub reference: String,
}

#[derive(Debug, Subcommand)]
pub enum GroupCommand {
    /// List groups with tasks still open, one line each: group, count, task ids.
    ///
    /// The one-line-per-group format is depended on and is not to be reflowed:
    /// it is how a person sees what a group has left before landing its stack.
    /// There is no `--json`: one grep-able line is the format.
    ///
    /// A task that reaches `done` is archived, so a group whose tasks have all
    /// finished has no line here. `group:` on a queued task is read verbatim,
    /// never path-parsed — a bare word and a path are different groups even
    /// when they share a file stem.
    List,
}

#[derive(Debug, Subcommand)]
pub enum JobsCommand {
    /// List every job across both stores: name, scope, schedule, pipeline,
    /// when it fires next and when it last fired. `--json` prints the same
    /// rows for a script.
    List,

    /// Fire one job now, ignoring its schedule — the way to test a job you
    /// have just written. Its next scheduled firing is unaffected.
    Run(JobsRunArgs),
}

#[derive(Debug, Args)]
pub struct JobsRunArgs {
    /// The job's name, as `spoolway jobs list` prints it.
    pub name: String,
}

#[derive(Debug, Subcommand)]
pub enum PipelineCommand {
    /// Print the pipeline as a readable flow.
    Show,

    /// Validate the pipeline definition and its agent references.
    Check,

    /// Print the pipeline format: every key, every rule, and a blank to copy.
    ///
    /// Every key a pipeline and a step may carry, one sentence each on how to
    /// fill it, the rules refused at load, this project's own agent profiles
    /// and prompts, and an annotated blank pipeline — copy it to
    /// `.spoolway/pipelines/<name>.yml`, delete what the flow has no use for,
    /// and run `spoolway pipeline check`.
    Contract,

    /// Print every pipeline's name, one per line, marking the default.
    ///
    /// For a skill that only needs a name to pass through — never a step, an
    /// agent or a prompt — so it can ask a person which pipeline runs a task
    /// without reading a single `.spoolway/pipelines/*.yml` itself.
    List,

    /// Open a fresh agent session, in a pane of this checkout, to write a new
    /// pipeline.
    ///
    /// Nothing is written by this command itself: it opens the pane, starts
    /// the `[pipeline_gen]` profile, and prompts the `spoolway-pipeline`
    /// skill to carry the procedure from there.
    Gen(PipelineGenArgs),
}

#[derive(Debug, Args)]
pub struct PipelineGenArgs {
    /// Where the pipeline is being generated for, named however its producer
    /// names it — an issue URL, a page path, a ticket. Never parsed.
    #[arg(long)]
    pub plan: Option<String>,
}

/// What spoolway can run, and whether it really can.
///
/// A separate noun from `doctor` on purpose. `doctor` asks "is this project
/// healthy?" and is scoped to the profiles this project configures; this asks
/// "could this project run X?", over every row in the adapter table — and the
/// kind you want to ask about is precisely the one no profile names yet.
#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Every kind spoolway knows: launch state, accounting state, binary.
    List,

    /// Check one kind against its row, clause by clause.
    Verify(AgentVerifyArgs),
}

#[derive(Debug, Args)]
#[command(
    long_about = "Check one agent kind against the contract its row declares.\n\n\
        Every clause is reported separately — the binary, the launch row, how its args \
        render, how a second turn resumes, its accounting row or the absence of one, and \
        the directory its transcripts land in. The exit code follows the launch half \
        alone: a kind that runs unmetered is reported, at length, and still exits 0, \
        because an unmetered kind is a legal state rather than a fault.\n\n\
        Without `--live` nothing is started and nothing is spent, so this is safe against \
        a kind you have never authenticated.",
    after_long_help = "\x1b[1mExamples:\x1b[0m\n  \
        spoolway agent verify claude          every clause decidable off disk\n  \
        spoolway agent verify codex           including the ones it fails\n  \
        spoolway agent verify pi --live --model qwen3-coder\n                        \
                      a real turn, then a resumed one, and the readings off the transcript"
)]
pub struct AgentVerifyArgs {
    /// The kind to check, as a profile's `kind` would name it.
    pub kind: String,

    /// Run one real turn, then a resumed one, and report the transcript
    /// readings — tokens, last-turn size, mtime, running totals, and whether
    /// the second turn continued the first one's session — or name the
    /// reading that failed.
    ///
    /// This is the only half of the contract a file on disk cannot settle: a
    /// kind can have a perfect launch row and write a transcript in a shape
    /// spoolway's parser no longer recognises, and a resume spelling settled
    /// against last month's binary can stop meaning "continue" with nothing
    /// going red. Nothing short of the turns says so. It spends whatever two
    /// turns of the model you name cost.
    #[arg(long)]
    pub live: bool,

    /// The model that turn runs. Defaults to one this project's pipelines
    /// already name for this kind, since spoolway names no model of its own.
    #[arg(long)]
    pub model: Option<String>,
}

/// Writing a prompt, and checking one against the step that will run it.
///
/// `contract` is the one that matters: a prompt is written against what a lane
/// is handed and what it may reach for, and that lives in the code rather than
/// in any document. Printing it from the binary is the only version of it that
/// cannot be out of date.
#[derive(Debug, Subcommand)]
pub enum PromptCommand {
    /// Print the contract a prompt is written against, rendered from this
    /// project's own pipeline and the code that starts a lane.
    Contract(PromptContractArgs),

    /// List every prompt this project has, and which steps run it — or
    /// that nothing does.
    List,

    /// Print one prompt file.
    Show { name: String },

    /// Read prompts against the steps that run them. Also part of
    /// `spoolway pipeline check`.
    Check {
        /// Only this prompt. Default: every one.
        name: Option<String>,
    },
}

#[derive(Debug, Args)]
#[command(after_long_help = "\x1b[1mExamples:\x1b[0m\n  \
        spoolway update --dry-run     what it would change, and nothing else\n  \
        spoolway update               take it\n\n\
        Everything spoolway writes is tracked in git, so `git diff` is the review.")]
pub struct UpdateArgs {
    /// Print what would change and write nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Replace this whole file with the one spoolway ships. Your version is
    /// saved beside it as a `.bak` first, so nothing you wrote is lost, only
    /// displaced. Repeat for each. The way back for a file so far from the
    /// shape we know that no region can be found in it.
    ///
    /// Paths are named one at a time on purpose: losing a file you asked for is
    /// a decision, losing eleven you forgot about is an accident.
    #[arg(long = "replace", value_name = "PATH")]
    pub replace: Vec<String>,
}

#[derive(Debug, Args)]
pub struct PromptContractArgs {
    /// Step to render the contract for. Defaults to the first agent step of the
    /// default pipeline, since that is the one most prompts are written for.
    #[arg(long)]
    pub step: Option<String>,

    /// Pipeline the step belongs to. Defaults to the file's `default:`.
    #[arg(long)]
    pub pipeline: Option<String>,

    /// Render both halves against a real queued task instead of a sample
    /// one — which is also how to see what a lane that misbehaved was handed.
    #[arg(long)]
    pub task: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the whole config.
    Show,

    /// List every scalar setting as `key = value`, one per line — every key
    /// `get`/`set` resolve to a value today, with the value each holds. A
    /// `[models]` glob nobody has named yet is settable but not listed.
    List,

    /// Print the path to the config file.
    Path,

    /// Read one value, e.g. `agents.pi.kind`.
    Get { key: String },

    /// Write one value, e.g. `models.claude-opus-5.input 5.0`.
    Set { key: String, value: String },

    /// Open the config file in `$EDITOR`, and re-validate it on save.
    ///
    /// The file is the interface — its comments carry every explanation — so
    /// editing it directly is the supported way to change several things at
    /// once. This merely saves finding the path and tells you immediately if
    /// what you wrote no longer parses.
    Edit,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_args(argv: &[&str]) -> EvalArgs {
        let mut full = vec!["spoolway", "eval"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).unwrap().command {
            Command::Eval(args) => args,
            other => panic!("expected Command::Eval, got {other:?}"),
        }
    }

    fn spend_args(argv: &[&str]) -> SpendArgs {
        let mut full = vec!["spoolway", "spend"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full).unwrap().command {
            Command::Spend(args) => args,
            other => panic!("expected Command::Spend, got {other:?}"),
        }
    }

    #[test]
    fn bare_spend_names_no_cut() {
        assert_eq!(spend_args(&[]).by, None);
    }

    #[test]
    fn spend_reads_its_cut_as_a_positional() {
        assert_eq!(spend_args(&["task"]).by, Some(SpendBy::Task));
        assert_eq!(spend_args(&["lane"]).by, Some(SpendBy::Lane));
    }

    #[test]
    fn spend_month_conflicts_with_since_and_until() {
        assert!(
            Cli::try_parse_from(["spoolway", "spend", "--month", "2026-08", "--since", "7d"])
                .is_err()
        );
    }

    fn try_lane_args(argv: &[&str]) -> Result<LaneArgs, clap::Error> {
        let mut full = vec!["spoolway", "lane"];
        full.extend_from_slice(argv);
        match Cli::try_parse_from(full)?.command {
            Command::Lane(args) => Ok(args),
            other => panic!("expected Command::Lane, got {other:?}"),
        }
    }

    fn lane_args(argv: &[&str]) -> LaneArgs {
        try_lane_args(argv).unwrap()
    }

    #[test]
    fn a_bare_lane_parses_with_no_lane_name() {
        let args = lane_args(&[]);
        assert_eq!(args.lane, None);
        assert_eq!(args.lines, 100);
        assert_eq!(args.message, None);
        assert!(!args.attach);
    }

    #[test]
    fn a_lane_alone_reads_it() {
        let args = lane_args(&["login · deploy"]);
        assert_eq!(args.lane.as_deref(), Some("login · deploy"));
        assert_eq!(args.message, None);
        assert!(!args.attach);
    }

    /// `-m` together with `--attach` asks for two different things from one
    /// turn, so clap refuses the combination before either body ever runs.
    #[test]
    fn message_and_attach_are_refused_together() {
        assert!(try_lane_args(&["login · deploy", "-m", "yes", "--attach"]).is_err());
    }

    #[test]
    fn lines_and_message_are_refused_together() {
        assert!(try_lane_args(&["login · deploy", "-n", "5", "-m", "yes"]).is_err());
    }

    #[test]
    fn lines_and_attach_are_refused_together() {
        assert!(try_lane_args(&["login · deploy", "-n", "5", "--attach"]).is_err());
    }

    /// `-m` and `--attach` both name a lane to act on; bare `spoolway lane -m
    /// ...` has nothing to answer, so clap refuses it rather than falling
    /// through to a body that would have to invent the same check.
    #[test]
    fn message_requires_a_lane() {
        assert!(try_lane_args(&["-m", "yes"]).is_err());
    }

    #[test]
    fn attach_requires_a_lane() {
        assert!(try_lane_args(&["--attach"]).is_err());
    }

    /// `eval_is_bare` reads the raw `ArgMatches` for the `eval` subcommand
    /// rather than `EvalArgs`'s own fields, through the same `command()`
    /// [`parse`] itself calls — so this exercises the real path, not a
    /// second parse of the same argv.
    fn eval_bare_from(argv: &[&str]) -> bool {
        let mut full = vec!["spoolway", "eval"];
        full.extend_from_slice(argv);
        let base = command();
        let matches = base.clone().try_get_matches_from(full).unwrap();
        eval_is_bare(&base, &matches)
    }

    /// Argv built before the subcommand name, for a global flag typed the
    /// way a person actually types it: `spoolway -C ~/project eval`, not
    /// `spoolway eval -C ~/project`.
    fn eval_bare_from_full(argv: &[&str]) -> bool {
        let base = command();
        let matches = base.clone().try_get_matches_from(argv).unwrap();
        eval_is_bare(&base, &matches)
    }

    #[test]
    fn truly_bare_eval_is_bare() {
        assert!(eval_bare_from(&[]));
    }

    /// Clap's `args_present` sees a flag whether or not its value happens to
    /// equal what `EvalArgs::limit()` would have supplied anyway — which is
    /// the whole point: a person who typed `--limit 10` asked for the
    /// printing path, not the screen, even though the number changes nothing.
    #[test]
    fn limit_spelled_out_to_its_own_default_is_not_bare() {
        let args = eval_args(&["--limit", "10"]);
        assert_eq!(args.limit, Some(10));
        assert!(
            !eval_bare_from(&["--limit", "10"]),
            "a person who typed --limit 10 asked for the printing path, not the screen"
        );
    }

    #[test]
    fn any_other_flag_is_not_bare_either() {
        assert!(!eval_bare_from(&["--pipeline", "default"]));
        assert!(!eval_bare_from(&["--csv"]));
        assert!(!eval_bare_from(&["--runs"]));
        assert!(!eval_bare_from(&["--by", "--month", "2026-08"]));
        assert!(!eval_bare_from(&["--by"]));
    }

    /// The mechanism this replaced — `EvalArgs::is_bare()` enumerating every
    /// field by hand — silently stayed correct only as long as every new
    /// flag remembered to add itself to the list. `eval_is_bare` reads
    /// `args_present()` instead, so a flag this test adds without ever
    /// touching `cli.rs`'s bareness logic still gets caught by it.
    #[test]
    fn a_flag_never_mentioned_by_name_still_defeats_bareness() {
        // `--task`, `--group` and `--trial` all `requires = "runs"`, so each
        // needs `--runs` alongside it to parse at all — already covered on
        // its own by `any_other_flag_is_not_bare_either` above, which is
        // exactly why these three are worth pinning separately.
        assert!(!eval_bare_from(&["--runs", "--trial", "solo"]));
        assert!(!eval_bare_from(&["--runs", "--task", "solo-1"]));
        assert!(!eval_bare_from(&["--runs", "--group", "audits"]));
    }

    /// `--repo`/`-C` and `--json` are `global = true` on [`Cli`], so clap
    /// copies a typed one into `eval`'s own submatches — the regression this
    /// pins: an earlier version of `eval_is_bare` read `args_present()` on
    /// that submatch directly, which saw the global and reported "not bare"
    /// even though nothing about `eval` itself was asked for, so `spoolway
    /// -C ~/project eval` silently stopped opening the screen.
    #[test]
    fn a_global_flag_never_defeats_bareness() {
        assert!(eval_bare_from_full(&["spoolway", "-C", "/tmp", "eval"]));
        assert!(eval_bare_from_full(&["spoolway", "eval", "-C", "/tmp"]));
        assert!(eval_bare_from_full(&["spoolway", "--json", "eval"]));
        assert!(!eval_bare_from_full(&[
            "spoolway", "-C", "/tmp", "eval", "--csv"
        ]));
    }

    #[test]
    fn limit_defaults_to_ten_when_unset() {
        assert_eq!(eval_args(&[]).limit(), 10);
        assert_eq!(eval_args(&["--limit", "3"]).limit(), 3);
    }

    /// `--by` with no cut named still has to be told from "not typed at
    /// all" apart — an empty string is what `default_missing_value` fills a
    /// bare `--by` with, since `by` is a plain string on the deprecated
    /// alias now rather than a `SpendBy` with a sentinel variant of its own.
    #[test]
    fn by_with_no_value_is_the_empty_string() {
        assert_eq!(eval_args(&["--by"]).by.as_deref(), Some(""));
    }

    #[test]
    fn by_with_a_cut_is_that_cut() {
        assert_eq!(eval_args(&["--by", "task"]).by.as_deref(), Some("task"));
        assert_eq!(eval_args(&["--by", "lane"]).by.as_deref(), Some("lane"));
    }

    #[test]
    fn month_conflicts_with_since_and_until() {
        assert!(
            Cli::try_parse_from([
                "spoolway", "eval", "--by", "--month", "2026-08", "--since", "7d"
            ])
            .is_err()
        );
    }

    /// The version screen has no notion of a calendar month, so `--month`
    /// meant nothing there — accepting it anyway would have parsed fine and
    /// silently changed nothing, which is worse than refusing it outright.
    #[test]
    fn month_with_no_by_is_refused() {
        assert!(
            Cli::try_parse_from(["spoolway", "eval", "--month", "2026-08"]).is_err(),
            "--month with no --by should be refused, not silently ignored"
        );
    }

    #[test]
    fn month_with_by_is_accepted() {
        assert_eq!(
            eval_args(&["--by", "--month", "2026-08"]).month.as_deref(),
            Some("2026-08")
        );
    }
    #[test]
    fn every_visible_command_has_a_help_heading() {
        use clap::CommandFactory;

        let placed: Vec<&str> = HELP_GROUPS
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect();

        for sc in Cli::command().get_subcommands() {
            if sc.is_hide_set() {
                continue;
            }
            assert!(
                placed.contains(&sc.get_name()),
                "`{}` has no heading in HELP_GROUPS, so `spoolway --help` would not list it",
                sc.get_name()
            );
        }
    }

    #[test]
    fn every_heading_names_only_real_commands_once() {
        use clap::CommandFactory;
        use std::collections::HashSet;

        let cmd = Cli::command();
        let mut seen: HashSet<&str> = HashSet::new();
        for (heading, names) in HELP_GROUPS {
            for name in *names {
                assert!(
                    cmd.get_subcommands().any(|sc| sc.get_name() == *name),
                    "`{name}` under `{heading}` is not a command"
                );
                assert!(seen.insert(name), "`{name}` is under more than one heading");
            }
        }
    }

    #[test]
    fn the_top_level_help_prints_the_six_headings_in_order() {
        let help = command().render_help().to_string();
        let mut rest = help.as_str();
        for (heading, _) in HELP_GROUPS {
            let at = rest
                .find(heading)
                .unwrap_or_else(|| panic!("`{heading}` is missing from the top-level help"));
            rest = &rest[at + heading.len()..];
        }
    }

    #[test]
    fn a_group_lists_its_commands_in_the_order_written() {
        let help = command().render_help().to_string();
        let block = help
            .split("Your work:")
            .nth(1)
            .and_then(|rest| rest.split("\n\n").next())
            .expect("the first group is in the help");
        let names: Vec<&str> = block
            .lines()
            .filter(|line| line.starts_with("  "))
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert_eq!(
            names,
            vec![
                "queue", "group", "issue", "dispatch", "jobs", "eval", "spend"
            ]
        );
    }

    /// What `spoolway <name> --help` prints, rendered from the built `Command`
    /// rather than re-stated here.
    fn subcommand_long_help(name: &str) -> String {
        use clap::CommandFactory;
        Cli::command()
            .find_subcommand_mut(name)
            .unwrap_or_else(|| panic!("no `{name}` subcommand"))
            .render_long_help()
            .to_string()
    }

    /// `--unattended` help describes the assembled-`blocked` behaviour, not the
    /// "pipeline that does not stage `blocked`" a real binary never has any
    /// more — `Pipelines::assemble` materialises one onto every pipeline
    /// (finding 23).
    #[test]
    fn unattended_help_matches_assembled_blocked() {
        let help = subcommand_long_help("dispatch");
        assert!(
            !help.contains("does not stage `blocked`"),
            "stale two-branch description is back: {help}"
        );
        assert!(help.contains("skip_blocked_lane"), "{help}");
        assert!(help.contains("materialises"), "{help}");
    }

    /// `update --help` describes what `src/update.rs` does — config values kept,
    /// pipeline key reference refreshed, `.gitignore` block removed, prompts and
    /// skeletons untouched — and no longer promises a task-skeleton block or the
    /// dead `--force-contract` flag (finding 24).
    #[test]
    fn update_help_matches_what_update_does() {
        let help = subcommand_long_help("update");
        assert!(!help.contains("force-contract"), "{help}");
        assert!(
            !help.contains("Task skeletons and page templates carry"),
            "{help}"
        );
        assert!(help.contains("key reference"), "{help}");
    }
}
