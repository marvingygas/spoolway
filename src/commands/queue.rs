//! The queue and the pending tasks over it: listing, adding, dependency
//! checks, and write conflicts.

use std::collections::{BTreeMap, BTreeSet};

use super::*;
use crate::platform::PathExt;
use crate::screen::{
    Key, Notice, PollableRead, confirm, hint, key_hint, keys, overlay, pad_to, panel, read_key,
};

/// Which dependencies a task on a wait step is still waiting for.
///
/// `None` means nothing in the graph is holding it: it is simply waiting for a
/// worker slot, and the step's own description is the better thing to print.
pub(crate) fn dependency_note(graph: &Graph, id: &str) -> Option<String> {
    let waiting = graph.waiting_on(id);
    if waiting.is_empty() {
        return None;
    }
    Some(format!("waiting on: {}", waiting.join(", ")))
}

/// Where every task is sitting, once, as text.
///
/// The same reading of the queue a dispatch run draws between passes, for the
/// times there is nothing to draw it: another terminal while a run is going, a
/// skill checking what is already queued, a person with no dispatcher at all.
/// Plain and unsorted-by-colour it would say less than the board does, so it
/// says exactly what the board says — including whether a dispatcher is up,
/// which is the one thing no task file records.
pub fn queue_list(repo: &Repo, pipelines: &Pipelines, json: bool) -> Result<()> {
    let holder = crate::lock::Lock::holder(&repo.lock_file())?;
    let rows = crate::status::rows(repo, pipelines)?;

    if json {
        let payload = serde_json::json!({
            "dispatcher_pid": holder,
            "tasks": rows.iter().map(QueueRowJson::from).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
        return Ok(());
    }

    match holder {
        Some(pid) => println!("dispatcher: running (pid {pid})\n"),
        None => println!("dispatcher: not running — start it with `spoolway dispatch`\n"),
    }

    if rows.is_empty() {
        println!("No tasks queued. Add one with `spoolway queue add --from <path>`.");
        return Ok(());
    }

    // The board's own table, in plain text. Not a second layout that happens
    // to agree with it: this is the same reading from somewhere else, and the
    // two disagreeing about a column would be two answers to one question.
    print!("{}", crate::status::plain_table(&rows));
    Ok(())
}

/// `queue list --json`'s own view of a board row: the fields a script would
/// actually want, named plainly, rather than [`crate::status::Row`] itself —
/// that type carries board-only bookkeeping (`depth`, `steps_left`,
/// `dependents`, its own sort key) that a JSON consumer has no use for and
/// that is free to change shape as the board's own layout does.
#[derive(serde::Serialize)]
struct QueueRowJson {
    id: String,
    group: Option<String>,
    /// The other group this row's own group stacks on — see
    /// [`crate::status::Row::after`].
    after: Option<String>,
    stage: String,
    pipeline: String,
    state: &'static str,
    /// How many times this task has arrived at the step it is on. Mirrors
    /// `Row::arrivals`, which the board draws inline on the STEP column as
    /// `↻ <n>` from the second arrival on.
    arrivals: u32,
    next: String,
    resumable: bool,
    ctx_pct: Option<u64>,
    out_tokens: Option<u64>,
    cost_usd: Option<f64>,
    lane_time_s: Option<i64>,
}

impl From<&crate::status::Row> for QueueRowJson {
    fn from(row: &crate::status::Row) -> Self {
        QueueRowJson {
            id: row.id.clone(),
            group: row.group.clone(),
            after: row.after.clone(),
            stage: row.stage.clone(),
            pipeline: row.pipeline.clone(),
            state: state_label(row.state),
            arrivals: row.arrivals,
            next: row.next.clone(),
            resumable: row.resumable,
            ctx_pct: row.ctx,
            out_tokens: row.out,
            cost_usd: row.cost,
            lane_time_s: row.lane_time,
        }
    }
}

/// [`crate::status::State`] as a plain identifier, rather than the
/// bullet-and-words [`crate::status::State::word`] draws for a terminal.
fn state_label(state: crate::status::State) -> &'static str {
    use crate::status::State::*;
    match state {
        Paused => "paused",
        Running => "running",
        Starting => "starting",
        Finished => "finished",
        Blocked => "blocked",
        Unknown => "unknown",
        Prompt => "prompt",
        Queued => "queued",
        Waiting => "waiting",
        Done => "done",
    }
}

pub fn queue_show(repo: &Repo, id: &str) -> Result<()> {
    let task = repo.task(id)?;
    print!("{}", task.render()?);
    Ok(())
}

/// `spoolway queue route <task>`: where one task stands in its own pipeline,
/// and where resuming it sends it.
///
/// `pipeline show` answers neither: it prints every pipeline, with agents and
/// models, and nothing about any task. This prints the one pipeline the task
/// runs, a step to an entry, and leaves out `blocked` — reached from any step
/// and routed by whichever step the task stopped on, it has no route of its
/// own to draw — and every agent, model and prompt name, which a reader asking
/// what happens next has no use for.
///
/// Read-only, and so never refused from inside a lane the way
/// `spoolway resume` is: a lane reads this before it tells a person where
/// resuming sends its task.
pub fn queue_route(repo: &Repo, pipelines: &Pipelines, id: &str, json: bool) -> Result<()> {
    let task = repo.task(id)?;
    let route = route_view(&task, pipelines)?;
    match json {
        true => println!("{}", serde_json::to_string_pretty(&route)?),
        false => print!("{}", render_route(&route)),
    }
    Ok(())
}

/// Everything `queue route` says, as facts — what `--json` prints, and what
/// [`render_route`] draws the text from, so the two cannot say different
/// things.
#[derive(Debug, serde::Serialize)]
struct RouteView {
    task: String,
    pipeline: String,
    stage: String,
    /// Where the task stands, as the first line says it after the pipeline's
    /// name: `held at look, waiting for a person`.
    state: String,
    /// The step the task is on, or held at — the marked entry. `None` for a
    /// task on `queued`, on `done`, or held before it ever started.
    at: Option<String>,
    steps: Vec<RouteStep>,
    /// Whether the task is stopped on `paused` or `blocked`, the two stages
    /// the board offers `r` on. Only then does a resume line apply.
    held: bool,
    /// Where resuming it on the board sends it — [`crate::commands::
    /// resume_road`], the function `spoolway resume` itself acts on. `None`
    /// when it is not held, or when that function refused.
    resumes_to: Option<String>,
    /// Why `resume_road` refused, when it did: the same error a resume
    /// would stop on.
    resume_error: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct RouteStep {
    id: String,
    description: Option<String>,
    /// The step's own `gate: true`: a pass waits for a person.
    gate: bool,
    /// The task's own `gate_at` names this step: whatever it reports waits
    /// for a person.
    gate_at: bool,
    on_pass: Option<String>,
    on_fail: Option<String>,
    /// A terminal step: the task stops here, with no route onward.
    ends: bool,
}

fn route_view(task: &Task, pipelines: &Pipelines) -> Result<RouteView> {
    let pipeline = pipelines.for_task(task)?;
    let (state, at) = route_state(task, pipeline);
    let held = matches!(
        task.stage(),
        crate::pipeline::PAUSED | crate::pipeline::BLOCKED
    );
    let (resumes_to, resume_error) = match held {
        false => (None, None),
        true => match crate::commands::resume_road(task, pipelines) {
            Ok(road) => (Some(road.destination().to_string()), None),
            Err(err) => (None, Some(format!("{err:#}"))),
        },
    };
    let steps = pipeline
        .steps
        .iter()
        .filter(|step| step.id != crate::pipeline::BLOCKED)
        .map(|step| {
            let ends = step.kind() == StepKind::Terminal;
            RouteStep {
                id: step.id.clone(),
                description: step.description.clone(),
                gate: step.gate,
                gate_at: task.front.gate_at.as_deref() == Some(step.id.as_str()),
                on_pass: (!ends)
                    .then(|| step.destination(Outcome::Pass).map(str::to_string))
                    .flatten(),
                on_fail: (!ends)
                    .then(|| step.destination(Outcome::Fail).map(str::to_string))
                    .flatten(),
                ends,
            }
        })
        .collect();
    Ok(RouteView {
        task: task.id().to_string(),
        pipeline: pipeline.name.clone(),
        stage: task.stage().to_string(),
        state,
        at,
        steps,
        held,
        resumes_to,
        resume_error,
    })
}

/// Where `task` stands, in words, and the step to mark for it.
///
/// A held task is marked at the step it stopped on rather than at `paused` or
/// `blocked`, neither of which is an entry here: `paused_at` for a gate,
/// `parked_from` for a park, and for a block the step [`crate::commands::
/// resume_target`] reads back as the one it stopped on. A blocked task parked
/// by `p` carries `parked_from: blocked`, which names no entry either, so it is
/// marked at that same step.
fn route_state(task: &Task, pipeline: &Pipeline) -> (String, Option<String>) {
    let front = &task.front;
    // The step a block stopped the task on, or `None` when it never started.
    let blocked_at = || {
        let origin = crate::commands::resume_target(task, pipeline);
        (origin != crate::pipeline::QUEUED).then_some(origin)
    };
    match task.stage() {
        crate::pipeline::QUEUED => ("queued, not started yet".to_string(), None),
        crate::pipeline::DONE => ("done".to_string(), None),
        crate::pipeline::PAUSED => {
            if let Some(gated) = &front.paused_at {
                (
                    format!("held at {gated}, waiting for a person"),
                    Some(gated.clone()),
                )
            } else if front.parked_from.as_deref() == Some(crate::pipeline::BLOCKED) {
                match blocked_at() {
                    Some(origin) => (format!("paused while blocked at {origin}"), Some(origin)),
                    None => ("paused while blocked, before it started".to_string(), None),
                }
            } else if let Some(step) = &front.parked_from {
                (format!("paused at {step}"), Some(step.clone()))
            } else if let Some(stage) = &front.hook_paused {
                (format!("paused: its `{stage}` hook failed"), None)
            } else {
                ("paused before it started".to_string(), None)
            }
        }
        crate::pipeline::BLOCKED => match blocked_at() {
            Some(origin) => (format!("blocked at {origin}"), Some(origin)),
            None => ("blocked before it started".to_string(), None),
        },
        step if pipeline.step(step).is_none() => (
            format!("at `{step}`, a step this pipeline does not have"),
            None,
        ),
        step => (format!("at {step}"), Some(step.to_string())),
    }
}

/// The width [`render_route`] wraps a description to once it needs more than
/// one line.
const ROUTE_WRAP: usize = 75;

/// The widest a description may run and still stay on one line. A little
/// past [`ROUTE_WRAP`], so a description just over it is not split to put a
/// single word on a line of its own. Both stay under 80 columns, so a
/// person's terminal never wraps a line a second time.
const ROUTE_ONE_LINE: usize = 77;

/// The text `queue route` prints: a line saying where the task stands, one
/// entry per step, and the resume lines.
fn render_route(route: &RouteView) -> String {
    let mut out = format!("{} — {}\n\n", route.pipeline, route.state);
    // Three spaces past the longest id, so every description starts in one
    // column however long the pipeline's own step names are.
    let column = route
        .steps
        .iter()
        .map(|step| step.id.chars().count())
        .max()
        .unwrap_or(0)
        + 3;
    let indent = " ".repeat(column + 2);
    for step in &route.steps {
        let marker = match route.at.as_deref() == Some(step.id.as_str()) {
            true => "▸ ",
            false => "  ",
        };
        let held = match (step.gate_at, step.gate) {
            (true, _) => Some("Whatever it reports waits for a person."),
            (false, true) => Some("A pass waits for a person."),
            (false, false) => None,
        };
        let about = [step.description.as_deref(), held]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let prefix = format!("{marker}{:<column$}", step.id);
        match about.is_empty() {
            true => out.push_str(prefix.trim_end()),
            false if prefix.chars().count() + about.chars().count() <= ROUTE_ONE_LINE => {
                out.push_str(&prefix);
                out.push_str(&about);
            }
            false => out.push_str(&wrapped(&prefix, &about, ROUTE_WRAP).join("\n")),
        }
        out.push('\n');
        let routes = match (step.ends, &step.on_pass, &step.on_fail) {
            (true, _, _) => "the task ends here".to_string(),
            (false, pass, fail) => format!(
                "pass → {} · fail → {}",
                pass.as_deref().unwrap_or("—"),
                fail.as_deref().unwrap_or("—")
            ),
        };
        out.push_str(&format!("{indent}{routes}\n"));
    }
    out.push('\n');
    if !route.held {
        out.push_str("It is not held, so there is nothing to resume.\n");
        return out;
    }
    match (&route.resumes_to, &route.resume_error) {
        (Some(step), _) => out.push_str(&format!("Resuming on the board sends it to {step}.\n")),
        (None, error) => out.push_str(&format!(
            "Resuming on the board fails: {}\n",
            error.as_deref().unwrap_or("no destination")
        )),
    }
    out.push_str(&format!(
        "To send it to another step, in your own shell:\n  spoolway resume {} --stage <step>\n",
        route.task
    ));
    out
}

/// The longest step id that will ever be part of one of this pipeline's lane
/// names. Only agent steps get a lane, so a long `wait` or `terminal` id costs
/// a task id nothing.
pub(crate) fn longest_agent_step(pipeline: &crate::pipeline::Pipeline) -> &str {
    pipeline
        .steps
        .iter()
        .filter(|s| s.kind() == crate::pipeline::StepKind::Agent)
        .map(|s| s.id.as_str())
        .max_by_key(|id| id.len())
        .unwrap_or("")
}

/// Keys a task may never set: spoolway writes every one of these
/// itself, over the task's whole life, and a task that sets one is either
/// confused about what it owns or is trying to smuggle a task onto a step, a
/// run or an attempt count that was never earned. `base` is not in this list
/// — it is a task's to set, and [`parse_submission`] keeps it when it
/// does; a submission that sets neither a task's own `base:` nor
/// `queue add --base` is refused rather than given one, since the branch a
/// checkout happens to have out is never read as a base any more.
///
/// `branch` is here because a task body is content an agent wrote, and
/// `spoolway stack` force-pushes a squashed commit onto whatever `branch:`
/// says. spoolway derives the value itself — `task/<id>`, or
/// `task/<slug>-<id>` when `issue_tracking.key_in_names` prefixes it — and
/// stamps it in [`parse_submission`]; every dependency caller then reads that
/// recorded field rather than rebuilding a shape of its own. A value starting
/// with `-` would also reach `gh pr view` as a flag — so a task may not
/// name a branch at all.
pub(crate) const RESERVED_KEYS: &[&str] = &[
    "stage",
    "run",
    "attempts",
    "base_commit",
    "trial",
    "trial_group",
    "branch",
];

pub fn queue_add(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &QueueAddArgs,
    // No longer read for a base: a task's own `base:` or `--base` is the
    // whole of where one comes from now, never the branch a checkout
    // happens to have out. Kept in the signature rather than pulled from
    // every call site — `main.rs`'s dispatch table and every test in this
    // module still pass it.
    _cwd: &std::path::Path,
    in_lane: bool,
) -> Result<()> {
    if args.from.is_empty() {
        return print_skeleton_task(repo, pipelines);
    }
    refuse_from_lane("the queue is mutated", in_lane)?;

    let base = args.base.as_deref();

    let tasks = gather_tasks(&args.from)?;
    if args.dry_run {
        return queue_add_dry_run(repo, pipelines, base, &tasks);
    }
    queue_add_tasks(repo, pipelines, base, &tasks)
}

/// `--dry-run`: everything `queue add` decides, said out loud, and nothing
/// written — no task file, no ticket opened, no name prefixed. The project
/// root and home are printed first because the bug this exists for was a
/// `queue add` that quietly resolved to the wrong project: a person who can
/// see where the files *would* go can stop before they go there.
fn queue_add_dry_run(
    repo: &Repo,
    pipelines: &Pipelines,
    base: Option<&str>,
    submitted: &[(String, String)],
) -> Result<()> {
    let tasks = validate_batch(repo, pipelines, base, submitted)?;
    refuse_missing_start(repo, &tasks)?;
    println!("dry run — nothing written");
    println!("  project: {}", repo.root.display());
    println!("  home:    {}", repo.home.display());
    if let Some(base) = base {
        println!("  base:    `{base}`");
    }
    for task in &tasks {
        println!(
            "would queue {} at `{}`\n  {}",
            task.id(),
            crate::pipeline::QUEUED,
            task.path.display()
        );
    }
    println!("{}", based_on_note(&tasks, base.unwrap_or("")));
    Ok(())
}

/// `spoolway queue unqueue <id>` / `--all`: the command form of the board's
/// `u`/`U` keys — carry a not-started task back to the pending
/// directory, with every reserved key stripped, so `queue add --from` takes
/// it again unchanged. The move itself is [`crate::status::unqueue_task`]
/// (or, for `--all`, one call of it per not-started task) — this only adds
/// the arguments a script needs that a keypress does not: a named refusal
/// for everything the board's key silently declines, and `--force`, which a
/// panel with nobody behind it has no way to offer at all.
pub fn queue_unqueue(repo: &Repo, pipelines: &Pipelines, args: &QueueUnqueueArgs) -> Result<()> {
    if args.all && args.force {
        bail!(
            "`--all --force` would tear down every checkout still in the queue in one line — \
             name one task at a time with `--force`, or drop it to unqueue only what has not \
             started"
        );
    }

    if args.all {
        return queue_unqueue_all(repo);
    }

    let id = args
        .task
        .as_deref()
        .context("a task id is required, unless `--all` is given")?;
    queue_unqueue_one(repo, pipelines, id, args.force)
}

/// `--all`: every not-started task, the way the board's `U` does — no
/// per-task [`crate::status::depended_on_by_queued`] check, since anything
/// depending on the set has not started either and is carried back in the
/// same batch. Stops at the first one [`unqueue_or_bail`] cannot carry,
/// leaving every task after it exactly where it was — the same all-or-
/// nothing promise a single `--force` unqueue makes, read over the whole
/// batch rather than one checkout.
fn queue_unqueue_all(repo: &Repo) -> Result<()> {
    let ids: Vec<String> = repo
        .tasks()?
        .iter()
        .filter(|t| crate::status::not_started(t))
        .map(|t| t.id().to_string())
        .collect();
    if ids.is_empty() {
        println!("nothing to unqueue — no task has not started");
        return Ok(());
    }
    for id in &ids {
        unqueue_or_bail(repo, id)?;
    }
    Ok(())
}

/// One task, named on the command line: [`crate::status::not_started`]
/// carries it straight through [`unqueue_or_bail`], refusing a sibling
/// still queued that names it in `depends_on` — this command has no panel to
/// list a chain on, unlike the board's own `u`, which carries that
/// dependent back to pending alongside it instead. Anything further along
/// is refused unless `force` says to tear its checkout down first — see
/// [`queue_unqueue_forced`] — and even then while a queued task depends on it.
fn queue_unqueue_one(repo: &Repo, pipelines: &Pipelines, id: &str, force: bool) -> Result<()> {
    let tasks = repo.tasks()?;
    let task = tasks.iter().find(|t| t.id() == id).with_context(|| {
        format!("no queued task `{id}` — `spoolway queue list` names the ones there are")
    })?;

    if crate::status::not_started(task) {
        if let Some(sibling) = crate::status::depended_on_by_queued(&tasks, id) {
            bail!(
                "`{}` depends on `{id}` and has not started either — unqueuing `{id}` alone \
                 would leave it waiting on a dependency the queue no longer shows. Unqueue \
                 `{}` first, or pass `--all` to carry both back together.",
                sibling.id(),
                sibling.id(),
            );
        }
        return unqueue_or_bail(repo, id);
    }

    let stage = task.stage().to_string();
    let checkout = task.front.worktree_path.clone();

    if !force {
        let mut message = format!(
            "`{id}` is on `{stage}`, not `{}`\n",
            crate::pipeline::QUEUED
        );
        if let Some(path) = &checkout {
            message += &format!(
                "\n  It has a checkout at {}, and unqueuing it\n  would leave that standing.\n",
                path.display()
            );
        }
        message += &format!(
            "\n  To stop it where it is, keeping the checkout:\n      spoolway queue pause {id}\n"
        );
        // `--force` is refused while a queued task depends on this one, so
        // offering it here would send the person into a second refusal.
        match crate::status::depended_on_by_queued(&tasks, id) {
            Some(sibling) => {
                message += &format!(
                    "\n  `{}` depends on `{id}` and has not started, which stops `--force` too.\n  \
                     Unqueue `{}` first.\n",
                    sibling.id(),
                    sibling.id()
                );
            }
            None => {
                message += &format!(
                    "\n  To tear the checkout down and unqueue it anyway:\n      spoolway queue unqueue {id} --force\n"
                );
            }
        }
        message += "\nNothing was changed.";
        bail!(message);
    }

    // The not-started branch above refuses this for a parent that has not
    // started; a started parent needs the same refusal, or `--force` removes
    // it and the dependent waits forever on a task that is no longer one.
    if let Some(sibling) = crate::status::depended_on_by_queued(&tasks, id) {
        bail!(
            "`{}` depends on `{id}` and has not started — unqueuing `{id}` would leave it \
             waiting on a task that no longer exists. Unqueue `{}` first.",
            sibling.id(),
            sibling.id(),
        );
    }

    queue_unqueue_forced(repo, pipelines, id, &stage, checkout.as_deref())
}

/// Write `id`'s task into pending through
/// [`crate::status::unqueue_task`] — the same call the board's bare `u`/`U`
/// make — then check the queue file is actually gone: that function is
/// silent about a newer draft already sitting in pending, right for a
/// keypress with a board to keep drawing but wrong for a script that named
/// a task by hand and needs to know its unqueue did not happen.
fn unqueue_or_bail(repo: &Repo, id: &str) -> Result<()> {
    crate::status::unqueue_task(repo, id)?;
    if repo.queue_dir().join(format!("{id}.md")).exists() {
        bail!(
            "did not unqueue `{id}`: a newer draft is already at {} — move that draft aside, \
             then run this again.",
            repo.pending_dir().join(format!("{id}.md")).display()
        );
    }
    println!(
        "unqueued `{id}` → {}",
        repo.pending_dir().join(format!("{id}.md")).display()
    );
    Ok(())
}

/// `--force` on a task that has started: interrupt any live agent lane and
/// running command step, record uncommitted work through
/// [`crate::commands::auto_commit`], tear the checkout down through
/// [`crate::dispatch::Dispatcher::tear_down_checkout`] and only then carry
/// the task to pending — all or nothing, so a teardown this stops
/// partway through leaves the task queued and its task untouched.
///
/// Refused while a live dispatcher holds the run lock: it re-reads the
/// queue every pass, and a checkout this tears down out from under it is
/// the same corruption the lock exists to prevent. Checked first, before a
/// multiplexer backend or a [`crate::dispatch::Dispatcher`] is built at
/// all — see this task's own acceptance criteria on a task still `queued`,
/// which never reaches this function to begin with.
fn queue_unqueue_forced(
    repo: &Repo,
    pipelines: &Pipelines,
    id: &str,
    stage: &str,
    checkout: Option<&std::path::Path>,
) -> Result<()> {
    if let Some(pid) = crate::lock::Lock::holder(&repo.lock_file())? {
        bail!(
            "a dispatcher is already running for this repo (pid {pid}) — it re-reads the \
             queue every pass and could be mid-turn on `{id}` right now. Stop it first, then \
             run this again."
        );
    }

    // The same per-task lock `unqueue_task` itself takes, held across the
    // whole teardown rather than just the final move: nothing else may
    // read or write this task's file while its checkout is coming down.
    let _task_lock = crate::lock::TaskLock::acquire(&repo.task_lock_file(id));

    // Checked before anything is torn down, not just before the final
    // write: a checkout this brings down for a move that was always going
    // to be refused is not "all or nothing", it is losing the checkout for
    // nothing. `carry_to_pending` re-checks this at the end regardless — a
    // draft that lands in the gap between here and there is the one race
    // this cannot close.
    let pending_dest = repo.pending_dir().join(format!("{id}.md"));
    if pending_dest.exists() {
        bail!(
            "did not unqueue `{id}`: a newer draft is already at {} — move that draft aside, \
             then run this again. Nothing was torn down.",
            pending_dest.display()
        );
    }

    let tasks = repo.tasks()?;
    let idx = tasks
        .iter()
        .position(|t| t.id() == id)
        .with_context(|| format!("no queued task `{id}` — it left the queue while this ran"))?;

    let mux = crate::mux::backend(repo)?;
    let lanes = mux.list_lanes().unwrap_or_default();
    if crate::status::live_agent_lane_tasks(repo, &tasks, pipelines, &lanes).contains(&idx) {
        let name = crate::mux::lane_name(stage, id);
        // Best-effort, the same as the board's own `p`: a lane that has
        // already gone quiet on its own has nothing left to interrupt.
        let _ = mux.interrupt_lane(&name);
        println!("interrupted lane {stage}/{id}");
    }
    let runs = crate::command_step::Runs::new(&repo.commands_dir());
    for run in crate::status::running_command_steps(repo, &tasks, pipelines)
        .into_iter()
        .filter(|run| run.task == id)
    {
        runs.stop(&crate::command_step::Runs::key(&run.step, &run.task));
    }

    let mut task = repo.task(id)?;

    // Committed before the worktree comes down — `tear_down_checkout` does
    // not call this itself, unlike `Dispatcher::clean_up`'s own road to it,
    // because a trial arm's `discard_arm` shares the same teardown and
    // means to throw its work away. This caller means the opposite.
    if let Some(worktree) = checkout {
        let outcome = auto_commit(repo, worktree, "", id, stage);
        let note = outcome.note().unwrap_or_default().to_string();
        if !note.is_empty() {
            println!("{note}");
        }
        if outcome.is_unrecorded() {
            bail!(
                "`{id}`'s worktree at {} has work `auto_commit` could not record ({note}) — \
                 nothing was torn down or unqueued. Commit or discard it by hand, then run \
                 this again.",
                worktree.display()
            );
        }
    }

    let mux_ref = mux.as_ref();
    let mut dispatcher = crate::dispatch::Dispatcher::new(repo, pipelines, mux_ref);
    let mut report = crate::dispatch::Report::default();
    let borrowed = task.front.borrowed;
    dispatcher.tear_down_checkout(&mut task, &mut report);
    // `tear_down_checkout` reports neither success nor failure — every
    // removal inside it is `let _ = …`, and a borrowed checkout is spared
    // on purpose (see this task's own non-goals) — so what actually
    // happened is read back off the filesystem rather than assumed from
    // having called the function at all.
    if let Some(path) = checkout {
        if path.exists() {
            let why = if borrowed {
                "it is borrowed, not spoolway's own"
            } else {
                "tear_down_checkout could not remove it"
            };
            println!("kept worktree {} — {why}", path.display());
        } else {
            println!("removed worktree {}", path.display());
        }
    }
    for problem in &report.problems {
        eprintln!("{problem}");
    }

    // `tear_down_checkout` gives back the workspace and the worktree but
    // never clears the fields recording them — its own two callers either
    // archive the task right after (`clean_up`) or delete it outright
    // (`discard_arm`), so a stale path in memory never reaches disk there.
    // This caller carries the task on to pending instead, and every one
    // of these has `skip_serializing_if = "Option::is_none"`, so clearing
    // them here is what keeps a torn-down checkout's path from riding along
    // into the copy `queue add --from` would hand straight back out.
    task.front.worktree_path = None;
    task.front.workspace_id = None;
    task.front.pane_id = None;
    task.front.tab_id = None;

    match crate::status::carry_to_pending(repo, &task)? {
        Some(dest) => {
            println!("unqueued `{id}` → {}", dest.display());
            Ok(())
        }
        None => bail!(
            "the checkout is torn down, but did not unqueue `{id}`: a newer draft is already \
             at {} — move that draft aside, then run this again.",
            repo.pending_dir().join(format!("{id}.md")).display()
        ),
    }
}

/// The pipeline skeleton, printed rather than written: there is no id yet to
/// name a file after, and bare `queue add` no longer queues anything itself —
/// a task, whole, is what `--from` needs, so this prints one unfilled,
/// for a person to save, fill in and hand back through `--from`.
fn print_skeleton_task(repo: &Repo, pipelines: &Pipelines) -> Result<()> {
    print!("{}", skeleton_task(repo, pipelines)?);
    Ok(())
}

/// The text `print_skeleton_task` prints, pulled apart from the printing
/// so it can be checked without capturing standard output.
fn skeleton_task(repo: &Repo, pipelines: &Pipelines) -> Result<String> {
    // The body preview is the generic skeleton every pipeline without one of
    // its own already falls back to — `task_template::FALLBACK` — rather
    // than any one pipeline's own, which the `pipeline:` line below still
    // has to name a real choice among but the body should not be shaped by.
    let body = crate::task_template::resolve(repo, crate::task_template::FALLBACK);
    let first = pipelines
        .names()
        .into_iter()
        .next()
        .context("no pipelines defined")?;

    // The other rows below align their trailing `#` at column 29 with a
    // literal run of spaces, which only works while the text ahead of it is
    // fixed-width — `pipeline: {name}` is not, since a project's pipeline
    // names vary, so this pads it to the same column instead of hard-coding
    // a count of spaces that would only ever line up for one name's length.
    let pipeline_row = format!(
        "{:<29}# required — one of: {}",
        format!("pipeline: {first}"),
        pipelines.names().join(", ")
    );

    Ok(format!(
        "---\n\
         id: name-this-task\n\
         title: a short, present-tense sentence naming what this task does\n\
         group: group-name            # required — a lane runs in the tab its group shares with its siblings\n\
         # source: where this came from — an issue URL, a plan page path, never parsed\n\
         depends_on: []               # sibling task ids that must finish first\n\
         # base: branch-name          # the branch to cut from and merge into — required, here or with `queue add --base`\n\
         {pipeline_row}\n\
         # gate_at: step-id           # pause after that step reports, for a person to `spoolway resume`\n\
         ---\n{}",
        ends_with_newline(body),
    ))
}

/// Every task named by `--from`, in the order it names them: a directory
/// expands to its `*.md` files in filename order, `-` reads a
/// `---`-separated stream from standard input, and anything else is one
/// file. Each entry is named for the errors below — a path, or `<stdin>#N`
/// for a stream's Nth task — and holds the raw, unparsed task text.
///
/// One file is one task, always. There is no page to lift several out
/// of any more: a producer that wants to queue a breakdown writes each task
/// as its own `.md` file, and pointing `--from` at
/// [`Repo::pending_dir`] queues every one of them.
pub(crate) fn gather_tasks(from: &[String]) -> Result<Vec<(String, String)>> {
    let mut tasks = Vec::new();
    for source in from {
        if source == "-" {
            let stream = read_stdin()?;
            for (i, doc) in split_stream(&stream).into_iter().enumerate() {
                tasks.push((format!("<stdin>#{}", i + 1), doc));
            }
            continue;
        }

        let path = std::path::Path::new(source);
        if path.is_dir() {
            let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(path)
                .with_context(|| format!("reading {}", path.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
                .collect();
            entries.sort();
            for entry in entries {
                gather_one(&entry, &mut tasks)?;
            }
        } else {
            gather_one(path, &mut tasks)?;
        }
    }
    Ok(tasks)
}

/// One `--from` entry that is neither a directory nor `-`.
fn gather_one(path: &std::path::Path, tasks: &mut Vec<(String, String)>) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    tasks.push((path.display().to_string(), text));
    Ok(())
}

/// Split a `---`-separated stream into whole tasks, each still in its own
/// `---\n<yaml>\n---\n<body>` shape.
///
/// A body may not itself contain a line that is exactly `---`: that is the
/// one thing a task trades away for a stream simple enough to split
/// blind, without parsing each task's yaml first to know where it ends.
fn split_stream(input: &str) -> Vec<String> {
    let mut tasks = Vec::new();
    let mut offset = 0usize;
    loop {
        let remaining = &input[offset..];
        if remaining.trim().is_empty() {
            break;
        }

        // The third bare `---` line — the first two are this task's own
        // opening and closing fence — is the next task's opening fence,
        // and where this one ends.
        let mut fences = 0usize;
        let mut cursor = 0usize;
        let mut cut = None;
        for line in remaining.split_inclusive('\n') {
            if line.trim_end() == "---" {
                fences += 1;
                if fences == 3 {
                    cut = Some(cursor);
                    break;
                }
            }
            cursor += line.len();
        }

        match cut {
            Some(cut) => {
                tasks.push(remaining[..cut].to_string());
                offset += cut;
            }
            None => {
                tasks.push(remaining.to_string());
                break;
            }
        }
    }
    tasks
}

fn read_stdin() -> Result<String> {
    let mut body = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)
        .context("reading tasks from standard input")?;
    Ok(body)
}

/// Parse one task into a task, refusing the four keys spoolway owns and
/// stamping in the ones a task may not set at all.
///
/// Reuses [`crate::task::Frontmatter`]'s own `Deserialize` rather than a
/// parallel struct: every field a task is not meant to carry —
/// `worktree_path`, `prompts`, and the rest — lands in its typed
/// slot exactly as it would in a task already on disk, and is then
/// overwritten below the same way `queue_add` always constructed these by
/// hand. Only the [`RESERVED_KEYS`] need a check first, because those are
/// wrong to accept even long enough to overwrite.
pub(crate) fn parse_submission(name: &str, raw: &str, base: Option<&str>) -> Result<Task> {
    let (yaml, body) =
        crate::task::split_fence(raw).with_context(|| format!("{name}: not a task"))?;

    let value: serde_norway::Value = serde_norway::from_str(yaml)
        .with_context(|| format!("{name}: frontmatter is not valid YAML"))?;
    let mapping = value
        .as_mapping()
        .with_context(|| format!("{name}: frontmatter is not a mapping"))?;

    for key in RESERVED_KEYS {
        if mapping.contains_key(*key) {
            bail!(
                "{name} sets `{key}:`, which spoolway sets on every task itself — \
                 remove it from the task"
            );
        }
    }

    // `parallel:` is gone: a group is one chain now, checked outright by
    // `check_dependencies_set` rather than left to a planner's own
    // judgement. Caught here, with its own friendly message, rather than
    // left to land in `extra` the way any other unrecognised key would —
    // `Frontmatter` dropped the typed field, so nothing downstream would
    // ever refuse it again once it did.
    if mapping.contains_key("parallel") {
        bail!(
            "{name} sets `parallel:`, which is gone — a group is one chain. Give it a \
             `depends_on`, or move it to a group of its own."
        );
    }

    // `stage` is the one field `Frontmatter` requires that a task never
    // carries — queue_add sets the real one below regardless of what is
    // written here, so any placeholder that deserialises cleanly does.
    let mut mapping = mapping.clone();
    mapping.insert(
        serde_norway::Value::String("stage".to_string()),
        serde_norway::Value::String(crate::pipeline::QUEUED.to_string()),
    );

    let mut front: crate::task::Frontmatter =
        serde_norway::from_value(serde_norway::Value::Mapping(mapping))
            .with_context(|| format!("{name}: frontmatter is not valid task YAML"))?;

    // The retired quota-and-usage-limit park fields, same as `Task::parse`
    // strips them for a file already on disk — a submitted task copied
    // from an older task carries them just as easily.
    for key in crate::task::RETIRED_PARK_KEYS {
        front.extra.remove(*key);
    }

    if front.id.trim().is_empty() {
        bail!("{name}: a task must set `id:`");
    }
    // The squashed commit's subject, always — and the pull request's title,
    // always. No heading in the body can carry it: the body's shape is the
    // project's, and this needs to survive any template. Written
    // `feat(queue): add a --dry-run flag`, and only its presence is checked:
    // a task predating that convention still queues, rather than being
    // refused over its wording.
    if front.title.trim().is_empty() {
        bail!("{name}: a task must set `title:`");
    }
    // A lane runs in the tab its group shares with its siblings, so a task
    // with no group has nowhere to run — refused here rather than discovered
    // by a dispatcher. The one task that still reaches the queue without a
    // group is one `spoolway eval --replay` once wrote directly, outside this
    // path entirely.
    if front.group.as_deref().unwrap_or("").trim().is_empty() {
        bail!(
            "{name}: a task must set `group:` — a lane runs in the tab its group shares \
             with its siblings, so a task with no group has nowhere to run"
        );
    }
    // The only routing source a task has — there is no project default to
    // fall back to, so a task naming none has nowhere to run.
    if front.pipeline.as_deref().unwrap_or("").trim().is_empty() {
        bail!("{name}: a task must set `pipeline:` — spoolway routes a task on nothing else");
    }
    // `off` is the only value this key ever means anything for — see
    // `Task::tracking_off` — so a task hand-writing anything else is a typo
    // that would otherwise queue with tracking silently left on.
    if let Some(value) = front.extra.get("tracking")
        && value != &serde_norway::Value::String("off".to_string())
    {
        bail!("{name}: `tracking:` may only be set to `off`");
    }
    // `SPOOLWAY_LABELS` hands a hook every label comma-joined — see
    // `tracking::build_env` and `tracking::open_env` — and Jira's own
    // labels cannot hold a space at all, so a label carrying either would
    // reach a hook already broken. Caught here, at the one gate every task
    // passes through before it is ever handed to a hook. An empty label
    // gets its own message, distinct from the whitespace/comma refusal
    // below it, since it fails neither of those checks but is just as
    // unusable once joined.
    for label in &front.labels {
        if label.is_empty() {
            bail!("{name}: `labels:` holds an empty label — remove it, or give it a word");
        }
        if label.contains(char::is_whitespace) || label.contains(',') {
            bail!(
                "{name}: label `{label}` may not hold whitespace or a comma — a hook reads \
                 every label comma-joined in SPOOLWAY_LABELS, and Jira's own labels cannot \
                 hold a space at all. Join the words with a hyphen instead, for example \
                 `needs-triage`."
            );
        }
    }

    // Everything spoolway itself decides, whatever the task said —
    // exactly the fields `queue_add` always built by hand rather than trusted
    // from a caller, now reset here instead of never having been set.
    front.stage = crate::pipeline::QUEUED.to_string();
    front.borrowed = false;
    front.last_report = None;
    front.blocked_from = None;
    front.parked_from = None;
    front.escalated = false;
    front.parked_by_stop = false;
    front.resume = None;
    front.restart = None;
    front.branch = Some(format!("task/{}", front.id));
    // A task that names its own `base:` keeps it — a task cut for a
    // branch other than the one `--base` named for the rest of the
    // submission — and `validate_batch` checks that branch is real before
    // anything is written. A task naming neither is refused by name: a
    // base is chosen, never invented from whichever branch a checkout
    // happens to have out.
    front.base = Some(
        match front.base.take().filter(|own| !own.trim().is_empty()) {
            Some(own) => own,
            None => base
                .filter(|flag| !flag.trim().is_empty())
                .with_context(|| {
                    format!(
                        "`{name}` sets no `base:` and no --base was given.\n\n  Set `base:` in \
                     the task, or pass --base <branch>.\n\nNothing was queued."
                    )
                })?
                .to_string(),
        },
    );
    front.run = None;
    front.base_commit = None;
    front.patch = None;
    front.skip = Vec::new();
    front.replay_of = None;
    front.worktree_path = None;
    front.workspace_id = None;
    front.pane_id = None;
    front.tab_id = None;
    front.attempts = 0;
    front.paused_at = None;
    front.paused_by = None;
    front.launched_at = None;
    front.steps = Default::default();
    front.rounds = Default::default();
    front.arrivals = Default::default();
    front.launch_failures = Default::default();
    front.launch_busy_since = Default::default();
    front.arrived_from = None;

    let body = ends_with_newline(body.to_string());
    if body.trim().is_empty() {
        bail!("{name}: the task body is empty — a lane would have nothing to work from");
    }

    let path = std::path::PathBuf::from(format!("{}.md", front.id));
    Ok(Task { path, front, body })
}

/// Parse and validate every task as one set: a `depends_on` naming a
/// sibling in the same submission is satisfied with nothing sorted first,
/// and one bad task queues nothing. Split out from
/// [`queue_add_tasks`] so the queue screen can hold the validated batch
/// across its conflict gate instead of writing it the moment it parses —
/// see that screen's own `begin_submission`.
pub(crate) fn validate_batch(
    repo: &Repo,
    pipelines: &Pipelines,
    base: Option<&str>,
    submitted: &[(String, String)],
) -> Result<Vec<Task>> {
    if submitted.is_empty() {
        bail!("`--from` named nothing to queue");
    }

    let mut tasks = Vec::new();
    for (name, raw) in submitted {
        let mut task = parse_submission(name, raw, base)?;
        // Whatever base a task ends up with — a task's own, or the
        // submission's `--base` — has to be a branch this repository really
        // has, checked here rather than only when a task's value
        // happens to differ from the flag: a `--base` is as much an
        // arbitrary value as a task's own `base:` is, and both reach a
        // task file the same way.
        let resolved = task
            .front
            .base
            .as_deref()
            .expect("parse_submission always resolves a base or refuses the task");
        check_task_base(repo, name, resolved)?;

        let pipeline_name = task
            .front
            .pipeline
            .as_deref()
            .expect("parse_submission refuses a task with no `pipeline:`");
        // Looked up to check that it exists, and for the `gate_at:` check
        // below. Nothing else is read from it: a lane's own wire name no
        // longer has to fit inside anything this pipeline decides — see
        // gh-359 — so a task id needs no pipeline at all to be checked
        // against, just the plain path-safety rule below.
        let pipeline = pipelines.get(pipeline_name)?;
        // `gate_at:` pauses a task when it reports from the step it names. A
        // name that is no step of this pipeline — a typo, or `done`, which is
        // not a step — is never matched, so the checkpoint is dropped and the
        // task runs straight through. Refused here, where the task can still
        // be corrected, rather than left to fail silently at run time.
        if let Some(gate_at) = task.front.gate_at.as_deref()
            && pipeline.step(gate_at).is_none()
        {
            bail!(
                "task `{}` has `gate_at: {gate_at}`, which is no step of pipeline \
                 `{pipeline_name}` — it may name one of: {}",
                task.front.id,
                pipeline.step_ids().join(", ")
            );
        }
        // A task id becomes a branch and a file name too. Both are checked
        // here rather than only when a task's value happens to differ,
        // the same as `check_task_base` above.
        crate::mux::check_task_id(&task.front.id)?;

        task.path = repo.queue_dir().join(format!("{}.md", task.front.id));
        if let Some(existing) = existing_task_path(repo, &task.front.id) {
            bail!(
                "task `{}` already exists at {}",
                task.front.id,
                existing.display()
            );
        }
        if tasks.iter().any(|t: &Task| t.id() == task.id()) {
            bail!(
                "`{}` is named by more than one task in this submission",
                task.id()
            );
        }

        tasks.push(task);
    }

    require_group_description(repo, &tasks)?;
    check_dependencies_set(repo, &mut tasks)?;
    Ok(tasks)
}

/// Whether `task` itself carries a non-blank `group_description:`.
fn has_group_description(task: &Task) -> bool {
    task.front
        .group_description
        .as_deref()
        .is_some_and(|d| !d.trim().is_empty())
}

/// Refuse this submission when a hook is configured and some group it names
/// carries a `group_description:` on none of its tasks — the group's
/// issue would then have nothing of its own to say, only whatever a hook
/// script guesses from a task's title. A no-op with no hook configured: the
/// acceptance criteria are explicit that `group_description:` is never
/// required for a project that has not turned issue tracking on at all.
///
/// Also satisfied by an already-queued sibling of the same bare group (never
/// an archived one — [`Repo::tasks`] reads only the queue) — the same
/// cross-submission lookup [`open_tickets`] runs for a group's epic and
/// slug, stripping a recognised `<slug>-` prefix before comparing: a group
/// opened over more than one `queue add` call sets its description once, on
/// whichever call opens it, and a later call adding more of the same group
/// is not asked to repeat it.
fn require_group_description(repo: &Repo, tasks: &[Task]) -> Result<()> {
    if !crate::tracking::configured(repo) {
        return Ok(());
    }
    // A hook whose declared tool floor this machine cannot meet never opens
    // a ticket — [`tool_requirements_gate`] switches tracking off for the
    // submission before [`open_tickets`] is reached — so demanding a
    // `group_description:` here as well would refuse a submission over a
    // description nothing is ever going to read. Pre-existing gap: this
    // check validates the whole batch before the screen's own gate runs,
    // so without this it fires first and the gate is never seen.
    if !unmet_requirements(repo).is_empty() {
        return Ok(());
    }

    let existing = repo.tasks().unwrap_or_default();
    let mut seen: std::collections::BTreeSet<&str> = Default::default();
    for group in tasks.iter().filter_map(|t| t.front.group.as_deref()) {
        if !seen.insert(group) {
            continue;
        }
        let in_batch = tasks
            .iter()
            .filter(|t| t.front.group.as_deref() == Some(group))
            .any(has_group_description);
        let in_queue = existing.iter().any(|sibling| {
            let sib_slug = match sibling.extra_str("slug") {
                s if accept_slug(s) => s,
                _ => "",
            };
            let bare = strip_slug_prefix(sibling.front.group.as_deref().unwrap_or(""), sib_slug);
            bare == group && has_group_description(sibling)
        });
        if !(in_batch || in_queue) {
            bail!(
                "group `{group}` sets no `group_description:` on any of its tasks, and \
                 `issue_tracking.hook` names `{}` — the group's issue would have nothing to \
                 say. Set it on one task of the group.",
                repo.config.issue_tracking.hook
            );
        }
    }
    Ok(())
}

/// Where a task id already sits on disk — the queue directory, where a run
/// still in flight holds it, or the archive, where a finished run's whole
/// record lives under it, Status Log and Review included
/// (`src/dispatch.rs`). `None` means the id is free in both, which is what a
/// repeat submission — and a trial's own mint, below — both need to be true
/// before a name is safe to use: a queued collision fails at once, and an
/// archived one would be silently overwritten the moment this task finished.
fn existing_task_path(repo: &Repo, id: &str) -> Option<std::path::PathBuf> {
    for dir in [repo.queue_dir(), repo.archive_dir()] {
        let path = dir.join(format!("{id}.md"));
        if path.exists() {
            return Some(path);
        }
    }
    None
}

/// The lowest-numbered `<id>-N` free in both the queue and the archive — see
/// [`existing_task_path`] — and not already claimed earlier in the same
/// trial, since a batch of arms is minted before any of them is saved and
/// so cannot see each other on disk yet.
///
/// Starts at 1: an ordinary submission already reaches the queue under the
/// task's own bare id (see [`parse_submission`]), so the first minted
/// arm is the first number that actually tells two runs of the same
/// task apart.
fn mint_id(repo: &Repo, base_id: &str, taken: &std::collections::BTreeSet<String>) -> String {
    let mut n = 1usize;
    loop {
        let candidate = format!("{base_id}-{n}");
        if !taken.contains(&candidate) && existing_task_path(repo, &candidate).is_none() {
            return candidate;
        }
        n += 1;
    }
}

/// The base a task ends up with — a task's own `base:`, or the
/// submission's `--base` where the task left it out — checked before
/// anything is written: it has to be a branch this repository actually has,
/// since the worktree is cut from it and the pull request merges into it,
/// and neither of those can wait until dispatch to find out it is not
/// there. Run against every task's resolved base, whichever of the two
/// chose it — an arbitrary value either way, and both reach the task file
/// the same way.
///
/// The leading `-` is refused before git sees the value at all: what
/// reaches `rev-parse` and `check-ref-format` here would otherwise be read
/// as a flag, the same reason `branch:` is refused outright.
fn check_task_base(repo: &Repo, name: &str, base: &str) -> Result<()> {
    if base.starts_with('-') {
        bail!(
            "{name}: `base: {base}` starts with `-`, which git would read as a flag — name a \
             branch instead"
        );
    }
    if repo.git(&["check-ref-format", "--branch", base]).is_err() {
        bail!("{name}: `base: {base}` is not a valid branch name — name a branch instead");
    }
    if repo
        .git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{base}"),
        ])
        .is_err()
        && !repo.remote_branch_exists(base)
    {
        bail!(
            "{name}: `base: {base}` names a branch this repository does not have locally or on \
             `origin` — create or fetch it first, or name one it already has"
        );
    }
    Ok(())
}

/// A task in a batch that queueing leaves out because it cannot start, and
/// why. It stays in the pending directory, to be queued again once a person
/// has set `starts_from:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NotQueued {
    pub(crate) id: String,
    pub(crate) why: NotQueuedWhy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotQueuedWhy {
    /// The task's own start branch, which does not exist.
    MissingStart(String),
    /// A task in the same batch it depends on, itself left out. Queued
    /// alone, it would wait on a dependency that never reaches the queue.
    DependsOn(String),
}

/// The branch `task` starts from, when that branch exists neither locally
/// nor on `origin` — `None` when it exists, when `origin` could not be asked,
/// and when there is nothing to check yet.
///
/// The start branch and the lookup are the dispatcher's own —
/// [`crate::dispatch::start_branch`] and [`crate::dispatch::branch_known`] —
/// so queueing and the dispatcher's pause never judge the same branch
/// differently. What differs is when there is something to check: a task's
/// own `starts_from:` always, a first dependency's branch only once that
/// dependency is `done`. A dependency still queued, or in the same batch,
/// has a branch the dispatcher cuts when it starts. A task with neither
/// starts from `base:`, which [`check_task_base`] has already checked. A
/// blank `starts_from:` names nothing to look up. A dependency whose branch
/// cannot be resolved is left for [`check_dependencies_set`] and the cut to
/// refuse by name.
///
/// A branch `origin` could not be asked about, offline or over a refused
/// credential, is not missing: telling the person to set `starts_from:`
/// would restack the work onto another branch over a network failure. The
/// task is queued, and the dispatcher's pause asks again before the cut.
fn missing_start_branch(
    repo: &Repo,
    task: &Task,
    remote_cache: &mut crate::dispatch::RemoteCache,
) -> Option<String> {
    let base = task.front.base.as_deref()?;
    match (&task.front.starts_from, task.front.depends_on.first()) {
        (Some(own), _) if own.trim().is_empty() => return None,
        (Some(_), _) => {}
        (None, Some(dep)) => {
            let done = repo
                .task(dep)
                .is_ok_and(|dep| dep.front.stage == crate::pipeline::DONE);
            if !done {
                return None;
            }
        }
        (None, None) => return None,
    }
    let start = crate::dispatch::start_branch(repo, task, base).ok()?;
    (crate::dispatch::branch_known(repo, &start, remote_cache) == Some(false)).then_some(start)
}

/// Every task in `tasks` that queueing leaves out: each whose start branch
/// does not exist — see [`missing_start_branch`] — and every task in the
/// batch that depends on one of those, directly or through another left out.
/// In batch order, so a chain reads top to bottom.
pub(crate) fn not_queued(repo: &Repo, tasks: &[Task]) -> Vec<NotQueued> {
    // One ask of `origin` per branch for the whole batch, a failed one
    // included, the way a dispatcher pass shares its own.
    let mut remote_cache = crate::dispatch::RemoteCache::new();
    let mut out: Vec<NotQueued> = tasks
        .iter()
        .filter_map(|task| {
            missing_start_branch(repo, task, &mut remote_cache).map(|branch| NotQueued {
                id: task.id().to_string(),
                why: NotQueuedWhy::MissingStart(branch),
            })
        })
        .collect();
    // Grown until nothing new joins: a dependent can sit before what it
    // depends on in the batch, so one pass in batch order would miss it.
    loop {
        let before = out.len();
        for task in tasks {
            if out.iter().any(|n| n.id == task.id()) {
                continue;
            }
            let left_out_dep = task
                .front
                .depends_on
                .iter()
                .find(|dep| out.iter().any(|n| &n.id == *dep));
            if let Some(dep) = left_out_dep {
                out.push(NotQueued {
                    id: task.id().to_string(),
                    why: NotQueuedWhy::DependsOn(dep.clone()),
                });
            }
        }
        if out.len() == before {
            break;
        }
    }
    out.sort_by_key(|n| tasks.iter().position(|t| t.id() == n.id));
    out
}

/// Move each group's `group_description:` onto a task that is queued when
/// the only tasks carrying it are ones the batch leaves out.
///
/// [`require_group_description`] passed the whole batch before
/// [`not_queued`] took tasks out of it. Without this, a group whose
/// description sat on a left-out task would open its issue for the queued
/// siblings with nothing to say, which is what that check exists to refuse.
/// The first queued task of the group takes it. Only the task about to be
/// written changes; the left-out task's own file keeps its description.
fn carry_group_descriptions(tasks: &mut [Task], not_queued: &[NotQueued]) {
    let left_out = |task: &Task| not_queued.iter().any(|n| n.id == task.id());
    let groups: Vec<String> = tasks.iter().filter_map(|t| t.front.group.clone()).collect();
    for group in groups {
        let in_group = |t: &Task| t.front.group.as_deref() == Some(group.as_str());
        if tasks
            .iter()
            .any(|t| in_group(t) && !left_out(t) && has_group_description(t))
        {
            continue;
        }
        let Some(description) = tasks
            .iter()
            .find(|t| in_group(t) && left_out(t) && has_group_description(t))
            .and_then(|t| t.front.group_description.clone())
        else {
            continue;
        };
        if let Some(kept) = tasks.iter_mut().find(|t| in_group(t) && !left_out(t)) {
            kept.front.group_description = Some(description);
        }
    }
}

/// What a person reads for a task whose own start branch does not exist:
/// the queue tab's queued summary and `queue add`'s refusal say the same.
fn missing_start_sentence(id: &str, branch: &str) -> String {
    format!(
        "{id} starts from {branch}, which doesn't exist. Set starts_from: in the task front \
         matter and requeue."
    )
}

/// One [`missing_start_sentence`] per task in `not_queued` whose own start
/// branch is missing. A task left out only for its dependency gets none:
/// setting the dependency's `starts_from:` is what lets it through.
fn missing_start_sentences(not_queued: &[NotQueued]) -> Vec<String> {
    not_queued
        .iter()
        .filter_map(|n| match &n.why {
            NotQueuedWhy::MissingStart(branch) => Some(missing_start_sentence(&n.id, branch)),
            NotQueuedWhy::DependsOn(_) => None,
        })
        .collect()
}

/// `queue add`'s refusal of a whole batch over any task whose start branch
/// does not exist. A command has no summary to list the rest in, so it
/// queues none of it, the way every other refusal of `queue add` does.
fn refuse_missing_start(repo: &Repo, tasks: &[Task]) -> Result<()> {
    let sentences = missing_start_sentences(&not_queued(repo, tasks));
    if !sentences.is_empty() {
        bail!("{}\n\nNothing was queued.", sentences.join("\n"));
    }
    Ok(())
}

/// The `based on` line every path that queues a batch prints: one line when
/// the whole batch shares a base — the ordinary case, everything given the
/// same `--base` — and one line per task when tasks named bases of their
/// own, so what is printed is always the base each task was actually given.
/// Every task passed in has already been through [`validate_batch`], which
/// never leaves `front.base` unset.
pub(crate) fn based_on_note(tasks: &[Task], base: &str) -> String {
    let bases: Vec<&str> = tasks
        .iter()
        .map(|t| t.front.base.as_deref().unwrap_or(base))
        .collect();
    match bases.first() {
        Some(first) if bases.iter().all(|b| b == first) => format!("  based on `{first}`"),
        _ => tasks
            .iter()
            .zip(&bases)
            .map(|(task, base)| format!("  {} based on `{base}`", task.id()))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Validate every task as one set, then write all or none — the shape
/// `queue add --from` needs, and the same all-or-nothing pass
/// [`validate_batch`] runs, right before this saves what it validated.
fn queue_add_tasks(
    repo: &Repo,
    pipelines: &Pipelines,
    base: Option<&str>,
    submitted: &[(String, String)],
) -> Result<()> {
    let mut tasks = validate_batch(repo, pipelines, base, submitted)?;
    refuse_missing_start(repo, &tasks)?;
    // `esc` from an interactive run — a real terminal on both ends of
    // `queue add --from` — is not a refusal: it means the same thing it
    // means on the queue screen, "go back", so it is caught here rather
    // than left to `?`, which would otherwise print it as an ordinary
    // error and exit 1 the way `dispatch::overrides_gate`'s own `esc`
    // never does (review finding 5).
    let task_files = readable_task_files(submitted);
    let gate = ToolGate::Print {
        interactive: crate::ask::interactive(),
    };
    if let Err(err) = open_and_prefix(
        repo,
        submitted,
        &task_files,
        &mut tasks,
        gate,
        &mut PrintedTickets,
    ) {
        return match err.downcast_ref::<GateCancelled>() {
            Some(_) => Ok(()),
            None => Err(err),
        };
    }

    // All or none: every task above already parsed and validated, so
    // nothing left here can fail — the writes are the commit.
    for task in &tasks {
        task.save()?;
    }

    // The same rule the queue screen's own `finish_submit` keeps: a task
    // that reached the queue is not still waiting to go there. Only a source
    // this project's own pending directory holds is removed — a `--from`
    // pointing anywhere else, including `<stdin>#N`, is read and left
    // exactly where it is, since it was never this batch's inbox copy to
    // begin with.
    remove_pending_sources(repo, submitted);

    for task in &tasks {
        println!("queued {} at `{}`", task.id(), crate::pipeline::QUEUED);
        println!("  {}", task.path.display());
    }
    // Where this batch's worktrees will be cut from and where their pull
    // requests will merge back to — worth saying, because a task that
    // set its own base rather than taking `--base` (or the submission's
    // single shared one) is the exception worth seeing.
    println!("{}", based_on_note(&tasks, base.unwrap_or("")));

    // Nothing is banked here any more. Queueing used to be the one path a
    // planning session's spend had onto the ledger; an interactive session's
    // spend is not banked from any command any more, and this one was never
    // special.
    Ok(())
}

/// Delete every `--from` source task that lived in this project's own
/// [`Repo::pending_dir`], now that the whole batch is safely on disk.
///
/// `tasks` names each source the way [`gather_tasks`] read it —
/// a path exactly as `--from` gave it, or `<stdin>#N` for a stream entry,
/// which has no file to delete and is simply not a match below. Compared
/// through [`PathExt::comparable`] rather than by string equality, since a
/// relative `--from` path and `pending_dir`'s own absolute one otherwise
/// never look alike even when they name the same file. A file that cannot be
/// removed — already gone, or a permissions error — is left silently: this
/// runs after every task in the batch has already been written, so a failure
/// here is not a reason to call the submission itself anything but a
/// success.
fn remove_pending_sources(repo: &Repo, tasks: &[(String, String)]) {
    let pending_dir = repo.pending_dir().comparable();
    for (name, _) in tasks {
        let path = std::path::Path::new(name).comparable();
        if path.parent() == Some(pending_dir.as_path()) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The path of each of `tasks`, in order, that is actually a file on
/// disk right now — what [`open_and_prefix`]'s own `task_files` wants for a
/// `--from` or queue-screen submission, where the task and the file a
/// hook could read are normally the same thing. The empty string stands in
/// for one that is not: a `--from -` stream entry is named `<stdin>#N` by
/// [`gather_tasks`], never a real path, and handing that to a hook as
/// `SPOOLWAY_TASK_FILE` would be handing it something no `cat` can open —
/// the same failure this whole `task_files` split exists to end.
///
/// Canonicalised, not merely checked with `is_file`: a name here can be
/// relative to wherever `spoolway` itself was started (`--from ../t.md`,
/// or a `--from <dir>` whose entries [`gather_tasks`] joins onto that
/// same relative `dir`), but the hook it is handed to runs with its
/// current directory set to `repo.root` (see [`crate::tracking::open_ticket`]
/// and, under it, `Runs::start`'s own `cwd`) — a relative name would resolve
/// against the wrong directory there, either failing to open at all or,
/// worse, silently opening whatever unrelated file happens to sit at that
/// path from `repo.root`. `canonicalize` answers both questions in one
/// call: it fails exactly when there is no real file to resolve, which is
/// the same empty-string case `is_file` caught, and every path it accepts
/// comes back absolute, so `repo.root`'s cwd resolves it identically to
/// wherever `spoolway` was actually run from (review round 2 finding 6).
fn readable_task_files(tasks: &[(String, String)]) -> Vec<String> {
    tasks
        .iter()
        .map(|(name, _)| {
            std::fs::canonicalize(name)
                .ok()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        })
        .collect()
}

/// What every path that saves a validated batch runs between
/// [`validate_batch`] and its writes: open the batch's tickets, then apply
/// the prefix `issue_tracking.key_in_names` asks for. One place for the
/// pair, so a batch is named the same way however it arrived — `queue add
/// --from`, the queue screen's `enter`, or a routine fired from the routines
/// tab or by a job. Only `--from` ran it once, and with the flag on one queue
/// mixed prefixed and bare names by the route each batch had taken (jobs
/// review finding 6).
///
/// `submitted` are the files a failed hook call writes the ids it already
/// opened back into, so a re-run resumes rather than opening a second set —
/// see [`write_back_ids`]. A routine hands an empty list: writing an id back
/// into `.spoolway/routines/` would consume a task meant to be queued
/// again, not once.
///
/// `task_files` is a *different* list, index-aligned with `tasks` rather
/// than `submitted`, naming the real path each task currently
/// sits at for `SPOOLWAY_TASK_FILE` to point a hook at — never the same
/// thing as `submitted` staying empty: a routine's task is never
/// written back into, but it is a real file under `.spoolway/routines/` a
/// hook can safely be handed to read, and [`queue_routine_target`] and
/// [`finish_routine`] pass it here while still passing `submitted` as `&[]`.
/// An entry is the empty string when nothing backs it — a `--from -` stream
/// task read from standard input, say — rather than a path nothing can
/// open.
///
/// `gate` says how the tool-requirements gate is reached — see [`ToolGate`].
/// `log` is where what the hook answers goes as it answers — see
/// [`TicketLog`].
fn open_and_prefix(
    repo: &Repo,
    submitted: &[(String, String)],
    task_files: &[String],
    tasks: &mut [Task],
    gate: ToolGate,
    log: &mut dyn TicketLog,
) -> Result<()> {
    // Before `open_tickets` ever calls the hook: a declared requirement this
    // machine cannot meet means the call can only fail, and by the time it
    // does the group's epic may already exist on the forge — see
    // `tool_requirements_gate`. `true` means the gate drew and this
    // submission goes on with issue tracking switched off for it.
    let tracking_off = match gate {
        ToolGate::Print { interactive } => tool_requirements_gate(repo, interactive)?,
        ToolGate::Answered { tracking_off } => tracking_off,
    };
    if tracking_off {
        // Every route that can switch tracking off for a batch — the queue
        // screen's `n` and the tool gate's `enter` — funnels through here,
        // so this is the one place that needs to stamp it:
        // `dispatch::route_reserved_stage` and `tracking_gate` read
        // `Task::tracking_off` back to fire no hook for these tasks and never
        // hold them waiting on one.
        for task in tasks.iter_mut() {
            task.set_extra_str("tracking", "off");
        }
        return Ok(());
    }

    // Before anything is queued: a ticket opened for a task that never made
    // it into the queue — because a sibling task further down the batch
    // turned out to be broken — would be a ticket nothing ever points back
    // at.
    let group_slug = open_tickets(repo, submitted, task_files, tasks, log)?;

    // The prefix goes on in a pass of its own, after the hook has answered:
    // the slug does not exist until `open_tickets` has run, and `branch:` was
    // already stamped and the lane name already checked by `validate_batch`.
    prefix_generated_names(tasks, &group_slug, log);
    Ok(())
}

/// Where [`open_tickets`] reports each ticket as the hook answers it. A
/// caller with no screen prints the rows under its own command line —
/// [`PrintedTickets`]; the queue screen draws them into a popup over its tab
/// instead, one row at a time — see [`PopupTickets`] — since a row printed
/// under a drawn frame lands where no frame is and is cleared by the next
/// draw.
pub(crate) trait TicketLog {
    /// `group`'s tickets are about to be opened.
    fn group(&mut self, group: &str);
    /// The hook is about to be called for task `id`, and nothing answers
    /// until it returns.
    fn waiting(&mut self, id: &str);
    /// One ticket or epic: `kind`, what became of it, its id on the tracker
    /// and the task or group it belongs to.
    fn row(&mut self, kind: &str, status: &str, ticket: &str, name: &str);
    /// A line that is not a ticket — a slug dropped, names prefixed.
    fn note(&mut self, line: &str);
    /// Every task in the batch has its answer.
    fn done(&mut self);
}

/// [`TicketLog`] for a caller with no screen: `queue add --from` and a job
/// the dispatcher fires — the same lines those have always printed.
pub(crate) struct PrintedTickets;

impl TicketLog for PrintedTickets {
    fn group(&mut self, group: &str) {
        println!("issue_tracking: opening tickets for group `{group}`\n");
    }

    fn waiting(&mut self, _id: &str) {}

    fn row(&mut self, kind: &str, status: &str, ticket: &str, name: &str) {
        println!("  {kind:<12} {status:<9} {ticket:<13} {name}");
    }

    fn note(&mut self, line: &str) {
        println!("{line}");
    }

    fn done(&mut self) {
        println!();
    }
}

/// How [`open_and_prefix`] reaches the tool-requirements gate.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ToolGate {
    /// Nobody has asked yet: [`tool_requirements_gate`] prints it and, when
    /// someone is there, reads the answer — a caller with no screen at all:
    /// `queue add --from`, or the dispatcher firing a job on its schedule.
    ///
    /// `interactive` is that caller's own `crate::ask::interactive()`, whether
    /// anyone is really there to answer. Such a caller holds no terminal
    /// guard of its own, so the gate takes one before it reads a key.
    Print { interactive: bool },
    /// A screen has asked already, as a popup over its own tab — the queue
    /// screen's [`Mode::ToolGate`] — and
    /// printing the gate again under the screen would draw it where no frame
    /// is, and take a second terminal guard inside the screen's. See
    /// [`tool_gate_popup`]. `tracking_off` is the answer: `true` when a
    /// requirement was unmet and `enter` queued anyway, or when the queue
    /// screen's [`Mode::IssueQuestion`] was answered `n`.
    Answered { tracking_off: bool },
}

/// `esc` out of [`tool_requirements_gate_with`] — bailed as an ordinary
/// `Err` so [`open_and_prefix`] stays the one place every route reaches
/// [`open_tickets`] through, and told apart at each call site from a real
/// refusal: `esc` means "go back", not "here is what went wrong". Only a
/// caller with no screen at all ever sees it — `queue_add_tasks`, which
/// catches it and exits clean the way `dispatch::overrides_gate`'s own `esc`
/// does, and `queue_routine_target` under a job the dispatcher fires. The
/// queue screen asks the gate in a popup of its own instead, whose `esc`
/// goes back without an error at all.
#[derive(Debug)]
struct GateCancelled;

impl std::fmt::Display for GateCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled at the tool-requirements gate")
    }
}

impl std::error::Error for GateCancelled {}

/// One `# spoolway-requires:` declaration the configured hook's own text
/// carries that this machine cannot meet right now — what
/// [`tool_requirements_gate_with`] draws instead of letting [`open_tickets`]
/// reach a hook call that can only fail.
struct UnmetRequirement {
    /// The hook's own configured name — `issue_tracking.hook` verbatim, the
    /// bare filename the Mockup's own first column draws (`github.sh`, not
    /// `doctor`'s full `.spoolway/hooks/github.sh` display, a different
    /// surface answering a different question).
    hook: String,
    tool: String,
    floor: String,
    /// The version found and where it resolved, when `tool` is on PATH at
    /// all but reads below `floor`. `None` covers both "not on PATH" and an
    /// answer that does not read as a version at all — `doctor` turns the
    /// second into a note rather than a failure, but a submit gate has no
    /// softer outcome to fall back to: an answer it cannot parse is no more
    /// usable than no answer, so it is unmet either way.
    found: Option<(String, String)>,
}

/// Every [`UnmetRequirement`] the configured hook declares, checked the same
/// way `spoolway doctor`'s own `required_tool_checks` does — see
/// [`doctor::parse_version`], [`doctor::below_floor`] and
/// [`doctor::format_version`], reused rather than parsed a second time —
/// except broader: `doctor` leaves "not on PATH" to `gh_status` and the jira
/// PATH checks, its own separate rows, but a submit gate has no sibling
/// check to leave that to, and a hook calling a tool that plain is not
/// there fails exactly the way one calling it under-versioned does.
///
/// Empty whenever `issue_tracking.hook` is blank or not a bare filename —
/// the same no-op start [`open_tickets`] itself gives in each of those two
/// cases, since [`crate::tracking::configured`] tests the same thing. A bare
/// name whose script is missing or unreadable is *not* a third such case:
/// `open_ticket` still runs it and gets `OpenResult::Failed` back, since
/// `configured` never stats the file — only this gate's own reading of it
/// comes back empty here, with nothing to check a requirement against.
fn unmet_requirements(repo: &Repo) -> Vec<UnmetRequirement> {
    let mut unmet = Vec::new();
    let hook = repo.config.issue_tracking.hook.trim().to_string();
    let Some(path) = crate::tracking::hook_path_in(&repo.checkout, &hook) else {
        return unmet;
    };
    let Ok(script) = std::fs::read_to_string(&path) else {
        return unmet;
    };

    for parsed in crate::tracking::required_tools(&script) {
        // An unreadable `# spoolway-requires:` line is `doctor`'s own note
        // to give, not a reason to gate a submit over text this project did
        // not write.
        let Ok(required) = parsed else { continue };
        // The non-goal ruling out a general constraint grammar covers the
        // floor too — a line this cannot parse is not this gate's to judge.
        let Some(floor) = doctor::parse_version(&required.floor) else {
            continue;
        };

        let Some(bin_path) = which(&required.tool) else {
            unmet.push(UnmetRequirement {
                hook: hook.clone(),
                tool: required.tool,
                floor: required.floor,
                found: None,
            });
            continue;
        };
        let answered = std::process::Command::new(&required.tool)
            .arg("--version")
            .output()
            .ok()
            .map(|out| {
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                )
            });
        match answered.as_deref().and_then(doctor::parse_version) {
            Some(found) if !doctor::below_floor(&found, &floor) => {}
            Some(found) => unmet.push(UnmetRequirement {
                hook: hook.clone(),
                tool: required.tool,
                floor: required.floor,
                found: Some((doctor::format_version(&found), bin_path)),
            }),
            None => unmet.push(UnmetRequirement {
                hook: hook.clone(),
                tool: required.tool,
                floor: required.floor,
                found: None,
            }),
        }
    }
    unmet
}

/// The line under the gate's rows saying what they mean — the same words
/// printed by [`print_tool_gate_notice`] and in the queue screen's popup.
const TOOL_GATE_VERDICT: &str = "issue tracking is not supported.";

/// The gate's two keys, printed and in the popup alike.
const TOOL_GATE_KEYS: &str = "[enter] queue anyway, without issue tracking   [esc] back";

/// The gate's own box, drawn once per unmet requirement pair — the
/// declaration line and what this machine actually has — followed by the
/// one line saying what it means. Column widths grow with the content
/// rather than staying fixed, the same way [`dispatch::print_overrides_notice`]
/// sizes its own rows, so a longer hook name or tool never runs its column
/// into the next.
fn print_tool_gate_notice(out: &mut impl std::io::Write, unmet: &[UnmetRequirement]) -> Result<()> {
    writeln!(out)?;
    for row in tool_gate_rows(unmet) {
        writeln!(out, "    {row}")?;
    }
    writeln!(out)?;
    writeln!(out, "  {TOOL_GATE_VERDICT}")?;
    writeln!(out)?;
    Ok(())
}

/// The gate as a popup, for a screen to lay over its tab before it queues —
/// `None` when every requirement is met and there is nothing to ask. The
/// queue screen's [`Mode::ToolGate`] draws it, and answers
/// [`open_and_prefix`] with [`ToolGate::Answered`].
pub(crate) fn tool_gate_popup(repo: &Repo) -> Option<Vec<String>> {
    let unmet = unmet_requirements(repo);
    (!unmet.is_empty()).then(|| tool_gate_panel(&unmet))
}

/// The screens' own popup for the gate: the same rows and the same line
/// under them as [`print_tool_gate_notice`], in a box over the tab, as step
/// 14 of the screen's mockup draws it.
fn tool_gate_panel(unmet: &[UnmetRequirement]) -> Vec<String> {
    let mut body = vec![String::new()];
    body.extend(tool_gate_rows(unmet));
    body.push(String::new());
    body.push(TOOL_GATE_VERDICT.to_string());
    panel("issue tracking", &body, TOOL_GATE_KEYS)
}

/// One pair of rows per unmet requirement, unindented — see
/// [`print_tool_gate_notice`] on how the columns are sized.
fn tool_gate_rows(unmet: &[UnmetRequirement]) -> Vec<String> {
    let col1 = unmet
        .iter()
        .flat_map(|u| [u.hook.len(), u.tool.len()])
        .max()
        .unwrap_or(0)
        + 3;
    let col2 = unmet
        .iter()
        .map(|u| match &u.found {
            Some((found, _)) => found.len(),
            None => "not on PATH".len(),
        })
        .chain(std::iter::once("requires".len()))
        .max()
        .unwrap_or(0)
        + 3;

    let mut rows = Vec::new();
    for u in unmet {
        rows.push(format!(
            "{:<col1$}{:<col2$}{} >= {}",
            u.hook, "requires", u.tool, u.floor
        ));
        rows.push(match &u.found {
            Some((found, path)) => format!("{:<col1$}{:<col2$}{}", u.tool, found, path),
            None => format!("{:<col1$}not on PATH", u.tool),
        });
    }
    rows
}

/// Whether a batch's configured hook declares a tool requirement this
/// machine does not meet, drawn before [`open_tickets`] is ever reached —
/// `true` means the gate drew and issue tracking is switched off for this
/// submission, `false` means every requirement is met (or none exist) and
/// [`open_and_prefix`] goes on exactly as it does today. The one `Err` this
/// ever returns is [`GateCancelled`], `esc`'s own signal back up to the
/// caller.
///
/// `interactive` is [`ToolGate::Print`]'s own — not decided here; see there.
///
/// A thin wrapper over [`tool_requirements_gate_with`], the same split
/// [`dispatch::overrides_gate`] draws around [`dispatch::overrides_gate_with`]
/// and for the same reason: this is the only thing that touches the
/// process's real stdio, so a test can drive every branch — including the
/// no-tty print-and-proceed path — against an injected reader and writer
/// instead.
fn tool_requirements_gate(repo: &Repo, interactive: bool) -> Result<bool> {
    tool_requirements_gate_with(
        repo,
        interactive,
        &mut crate::screen::RawStdin,
        &mut std::io::stdout(),
        crate::platform::TermGuard::new,
    )
}

/// [`tool_requirements_gate`]'s own logic. With nobody there to answer, the
/// notice still prints — the run is otherwise silent about why tracking
/// switched off — but nothing waits on a key nobody can press, the same
/// unattended path [`dispatch::overrides_gate_with`] takes for a layer
/// notice.
///
/// `term` takes the terminal only just before the first blocking read, for
/// the same reason `dispatch::overrides_gate_with`'s own guard waits: every
/// early return above it constructs nothing, hides nothing and shows
/// nothing. The queue screen never calls this — it holds a guard of its own
/// for as long as it is open, and asks the gate as a popup instead; see
/// [`ToolGate::Answered`].
fn tool_requirements_gate_with(
    repo: &Repo,
    interactive: bool,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
    term: impl FnOnce() -> crate::platform::TermGuard,
) -> Result<bool> {
    let unmet = unmet_requirements(repo);
    if unmet.is_empty() {
        return Ok(false);
    }

    print_tool_gate_notice(out, &unmet)?;
    if !interactive {
        return Ok(true);
    }
    writeln!(out, "  {TOOL_GATE_KEYS}")?;

    let _term = term();
    loop {
        match read_key(input) {
            Some(Key::Enter) => return Ok(true),
            Some(Key::Esc) => return Err(GateCancelled.into()),
            // The tty went away mid-question — nothing here may hang
            // waiting for an answer that can no longer come.
            None => return Ok(true),
            _ => {}
        }
    }
}

/// Prefix each task's `group:`, `branch:` and stored `slug:` with the slug its
/// group's hook answered — `group: <slug>-<group>`, `branch:
/// task/<slug>-<id>`. A no-op for any group with no slug, which is every group
/// when `issue_tracking.key_in_names` is off, so with the flag off every
/// generated name is byte-for-byte what it is today. The group is stripped
/// of the slug before it is reapplied, so a task that comes back through
/// the queue already carrying the prefix — `carry_to_pending` keeps `group:`
/// as written — gets it exactly once, not stacked on again.
///
/// Runs after [`open_tickets`] and before any task is saved. The `task/` ref
/// namespace is kept, so `spoolway stack` and the orphaned-branch sweep still
/// find these branches where they look today; the worktree directory follows
/// the branch and so picks the prefix up on its own — see
/// [`crate::mux::branch_slug`].
fn prefix_generated_names(
    tasks: &mut [Task],
    group_slug: &BTreeMap<String, String>,
    log: &mut dyn TicketLog,
) {
    // The one winning slug per group onto every `slug:` first — the same
    // stamp the failure path runs before `write_back_ids`.
    stamp_group_slugs(tasks, group_slug);

    // One line per distinct prefix applied, naming the new group — the rename
    // is invisible otherwise until somebody runs `git branch`.
    let mut announced: std::collections::BTreeSet<(String, String)> = Default::default();
    for task in tasks.iter_mut() {
        let Some(group) = task.front.group.clone() else {
            continue;
        };
        let Some(slug) = group_slug.get(&group) else {
            continue;
        };
        let bare = strip_slug_prefix(&group, slug);
        let prefixed_group = format!("{slug}-{bare}");
        task.front.branch = Some(format!("task/{slug}-{}", task.front.id));
        task.front.group = Some(prefixed_group.clone());
        if announced.insert((slug.clone(), prefixed_group.clone())) {
            log.note(&format!(
                "issue_tracking: names prefixed `{slug}` — group `{prefixed_group}`\n"
            ));
        }
    }
}

/// Call the `[issue_tracking]` open hook once for every task in the
/// batch that does not already name a `ticket:`, before any of them is
/// queued — a no-op start to finish when no hook is configured at all.
///
/// Walks the batch in dependency order, so a task's own call always has its
/// dependencies' ticket ids already in hand for `SPOOLWAY_DEPENDS_TICKETS`,
/// and tracks one epic id per `group:` — the first non-empty `epic=` a
/// hook answers with, or whatever a task already names, whichever this
/// batch reaches first — so a group opens at most one epic across however
/// many of its tasks actually call the hook.
///
/// A failing call bails out — nothing in this batch is queued — but not
/// before every id already answered is written back into the task it
/// came from, in place on disk: see [`write_back_ids`], which is what makes
/// re-running the same `queue add` resume rather than open a second set.
///
/// Returns the slug decided for each `group:` this batch touches — one per
/// group, the first non-blank `slug=` a hook answers, seeded from tasks
/// already in the queue as well as this batch. Empty unless
/// `issue_tracking.key_in_names` is on and a hook actually answered a slug;
/// [`prefix_generated_names`] is what applies it.
///
/// `task_files` is index-aligned with `tasks`, not `submitted` — see
/// [`open_and_prefix`]'s own doc comment on why the two lists differ — and
/// is what `SPOOLWAY_TASK_FILE` is resolved from below.
fn open_tickets(
    repo: &Repo,
    submitted: &[(String, String)],
    task_files: &[String],
    tasks: &mut [Task],
    log: &mut dyn TicketLog,
) -> Result<BTreeMap<String, String>> {
    // `group:` on every task in this batch is still the bare name a task
    // wrote — `validate_batch` never prefixes it — so every map here is keyed
    // by the bare group.
    let mut group_slug: BTreeMap<String, String> = BTreeMap::new();
    let key_in_names = repo.config.issue_tracking.key_in_names;

    if !crate::tracking::configured(repo) {
        return Ok(group_slug);
    }

    let mut group_size: BTreeMap<String, usize> = BTreeMap::new();
    // The group's own words for the issue this batch is about to open —
    // whichever task of the group set `group_description:` first, in
    // batch order, or an already-queued sibling's if none in the batch did
    // (seeded below, alongside `group_epic`). `require_group_description`
    // has already refused the batch outright when a hook is configured and
    // neither found one, so this only ever comes back empty for a group
    // that skipped that gate because no hook was configured at all.
    let mut group_description: BTreeMap<String, String> = BTreeMap::new();
    for task in tasks.iter() {
        if let Some(group) = &task.front.group {
            *group_size.entry(group.clone()).or_insert(0) += 1;
            if let Some(description) = task
                .front
                .group_description
                .as_deref()
                .filter(|d| !d.trim().is_empty())
            {
                group_description
                    .entry(group.clone())
                    .or_insert_with(|| description.to_string());
            }
        }
    }
    // Seeded from the queue too, not only this batch: a group opened over
    // more than one `queue add` call already has its epic — and its slug — on
    // a sibling this batch never mentions. A queued sibling's `group:` already
    // carries the `<slug>-` prefix from its own `queue add`, so a recognised
    // prefix is stripped before comparing: a person writes the bare `group:`
    // in every task they ever cut, and the second `queue add` still finds
    // the first one's epic instead of opening a second.
    let mut group_epic: BTreeMap<String, String> = BTreeMap::new();
    for sibling in repo.tasks().unwrap_or_default() {
        let Some(group) = sibling.front.group.clone() else {
            continue;
        };
        // A queued file can be hand-edited, and a stored slug with it. One
        // that no longer passes `check_id` is ignored outright — not used to
        // strip a `<slug>-` prefix off the group for the epic lookup, and not
        // seeded as a prefix — so a bad value cannot attach this group to the
        // wrong epic or an invalid branch. It is dropped in silence: a queued
        // sibling is not this command's input to complain about.
        let sib_slug = match sibling.extra_str("slug") {
            s if accept_slug(s) => s,
            _ => "",
        };
        let bare = strip_slug_prefix(&group, sib_slug).to_string();
        let epic = sibling.extra_str("epic");
        if !epic.is_empty() {
            group_epic
                .entry(bare.clone())
                .or_insert_with(|| epic.to_string());
        }
        if has_group_description(&sibling) {
            group_description
                .entry(bare.clone())
                .or_insert_with(|| sibling.front.group_description.clone().unwrap_or_default());
        }
        if key_in_names && !sib_slug.is_empty() {
            group_slug
                .entry(bare)
                .or_insert_with(|| sib_slug.to_string());
        }
    }
    // And from this batch: a task already carrying `slug:` — one reported
    // `kept`, or one a prior failed run wrote back — pins its group's slug
    // the same way a queued sibling does, so a hook answering a different
    // slug on the re-run cannot displace the first non-blank answer. This
    // task *is* this command's input, so a bad `slug:` here is reported —
    // and stripped from the task, so what queues does not carry the value
    // the note just said was dropped.
    if key_in_names {
        for task in tasks.iter_mut() {
            let slug = task.extra_str("slug").to_string();
            let Some(group) = task.front.group.clone().filter(|_| !slug.is_empty()) else {
                continue;
            };
            if accept_slug(&slug) {
                group_slug.entry(group).or_insert(slug);
            } else {
                log.note(&format!(
                    "  issue_tracking: slug `{slug}` on `{}` is not a valid name \
                     (lowercase letters, digits and hyphens) — ignored",
                    task.id()
                ));
                task.front.extra.remove("slug");
            }
        }
    }

    let mut printed_header: std::collections::BTreeSet<String> = Default::default();
    // What this call itself opened, named so a failure partway through can
    // say exactly that — not "an id", the id — the same way the task's own
    // Mockup does.
    let mut opened: Vec<String> = Vec::new();

    for i in open_order(tasks) {
        let group = tasks[i].front.group.clone().unwrap_or_default();
        if printed_header.insert(group.clone()) {
            log.group(&group);
        }

        let already = tasks[i].extra_str("ticket").to_string();
        if !already.is_empty() {
            let epic = tasks[i].extra_str("epic").to_string();
            if !epic.is_empty() {
                group_epic.entry(group.clone()).or_insert(epic);
            }
            log.row("task issue", "kept", &already, tasks[i].id());
            continue;
        }

        let depends_tickets = tasks[i]
            .front
            .depends_on
            .iter()
            .map(|dep| dependency_ticket(repo, tasks, dep))
            .collect::<Result<Vec<_>>>()?
            .join(" ");
        let known_epic = group_epic.get(&group).cloned().unwrap_or_default();
        let known_description = group_description.get(&group).cloned().unwrap_or_default();
        let size = *group_size.get(&group).unwrap_or(&1);
        // The path this task actually sits at right now — the caller's
        // to resolve, and never `tasks[i].path`: that names where
        // `validate_batch` intends to save the task, a file that does not
        // exist until every task in this batch has opened its ticket.
        let task_file = task_files.get(i).cloned().unwrap_or_default();

        log.waiting(tasks[i].id());
        match crate::tracking::open_ticket(
            repo,
            &tasks[i],
            size,
            &known_epic,
            &depends_tickets,
            &known_description,
            &task_file,
        )? {
            crate::tracking::OpenResult::NoHook => unreachable!("checked configured() above"),
            crate::tracking::OpenResult::Answered {
                epic,
                ticket,
                slug,
                url,
            } => {
                if !epic.is_empty() && !group_epic.contains_key(&group) {
                    group_epic.insert(group.clone(), epic.clone());
                    log.row("group issue", "created", &epic, &group);
                    opened.push(format!("epic {epic} (`{group}`)"));
                }
                if !epic.is_empty() {
                    tasks[i].set_extra_str("epic", &epic);
                }
                if !ticket.is_empty() {
                    tasks[i].set_extra_str("ticket", &ticket);
                    opened.push(format!("ticket {ticket} (`{}`)", tasks[i].id()));
                }
                record_slug_and_url(
                    &mut tasks[i],
                    &slug,
                    &url,
                    key_in_names,
                    &group,
                    &mut group_slug,
                );
                log.row("task issue", "created", &ticket, tasks[i].id());
            }
            crate::tracking::OpenResult::Failed { exit_code } => {
                log.row("task issue", "FAILED", "—", tasks[i].id());
                // The group's winning slug onto every task first, so the
                // ids written back carry the dependency-order decision — not
                // a later task's own raw answer, which task order would
                // otherwise let win the re-run.
                stamp_group_slugs(tasks, &group_slug);
                let written_back = write_back_ids(submitted, tasks)?;
                let code = exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "no code".to_string());
                // `opened` and `ids` rows carry what this batch actually
                // did, so they are left out entirely when nothing had been
                // opened yet — there is nothing to resume from, and running
                // this command again starts the batch fresh. `log` always
                // names the one file that holds the hook's own stderr,
                // never the tracking directory it lives under, so a person
                // reading this does not have to go find it themselves.
                let key = crate::command_step::Runs::key("open", tasks[i].id());
                let log = relative(
                    &repo.home,
                    &crate::command_step::Runs::new(&repo.tracking_dir()).log_path(&key),
                );
                let mut rows = String::new();
                for item in &opened {
                    let label = if rows.is_empty() { "opened" } else { "" };
                    rows.push_str(&format!("    {label:<9}{item}\n"));
                }
                if !opened.is_empty() {
                    let ids = if written_back {
                        "written into pending/".to_string()
                    } else {
                        "not written — no task on disk to record them in; close those \
                         by hand first"
                            .to_string()
                    };
                    rows.push_str(&format!("    {:<9}{ids}\n", "ids"));
                }
                rows.push_str(&format!("    {:<9}{log}", "log"));
                bail!(
                    "the open hook exited {code} for `{}` — nothing was queued.\n\n{rows}",
                    tasks[i].id(),
                );
            }
        }
    }
    log.done();
    Ok(group_slug)
}

/// The bare `group:` name, with a recognised `<slug>-` prefix removed. A
/// queued sibling carries the prefix its own `queue add` applied; comparing
/// against the bare name a fresh task writes is what lets the epic and
/// slug lookups in [`open_tickets`] span more than one `queue add`.
fn strip_slug_prefix<'a>(group: &'a str, slug: &str) -> &'a str {
    if slug.is_empty() {
        return group;
    }
    group
        .strip_prefix(slug)
        .and_then(|rest| rest.strip_prefix('-'))
        .unwrap_or(group)
}

/// The bare `group:` a task belongs to, with `issue_tracking.key_in_names`'
/// slug prefix stripped the same way the cross-group refusal in
/// [`check_dependencies_set`] always has — see [`strip_slug_prefix`]'s own
/// doc. Shared with `commands::stack`'s own conflict check, so a group
/// compared there and a group compared here can never disagree about what
/// counts as "the same group".
pub(crate) fn bare_group(repo: &Repo, t: &Task) -> Option<String> {
    let group = t.front.group.as_deref()?;
    if !repo.config.issue_tracking.key_in_names {
        return Some(group.to_string());
    }
    let slug = t.extra_str("slug");
    let slug = if accept_slug(slug) { slug } else { "" };
    Some(strip_slug_prefix(group, slug).to_string())
}

/// The `group:` a still-pending task named `id` declares, read straight off
/// its raw frontmatter YAML rather than through [`crate::task::Task::parse`]
/// — a task waiting in the pending directory has not been through `queue
/// add` yet and so has no `stage:` of its own, which [`crate::task::
/// Frontmatter`] requires and a pending file never carries. Only used to put
/// a name on a dependency [`Graph`] already read as unresolvable, so a
/// missing or unparsable pending file answers `None` exactly as an id with
/// no pending file at all does — [`check_dependencies_set`] falls back to
/// its ordinary "neither the queue nor the archive" refusal either way.
fn pending_task_group(repo: &Repo, id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(repo.pending_dir().join(format!("{id}.md"))).ok()?;
    let (yaml, _) = crate::task::split_fence(&raw).ok()?;
    let value: serde_norway::Value = serde_norway::from_str(yaml).ok()?;
    value
        .as_mapping()?
        .get("group")?
        .as_str()
        .map(str::to_string)
}

/// Whether a slug a hook or a task offered is one spoolway will build a
/// name out of: non-blank and inside [`crate::config::check_id`]'s alphabet,
/// the same one every task id, group and branch already uses.
fn accept_slug(slug: &str) -> bool {
    !slug.is_empty() && crate::config::check_id("issue_tracking slug", slug).is_ok()
}

/// Take the `slug=` and `url=` a hook answered on the `open` event.
///
/// The url is stored on the task as `url:` and validated whatever
/// `key_in_names` holds: the task's goal is to have it on the task for
/// `terminal-names` to use later, and the flag gates only the naming, not
/// that.
///
/// The slug is naming, so it is looked at only when `key_in_names` is on, and
/// only pinned in `group_slug` — where the first valid answer in dependency
/// order wins. It is *not* stamped onto this task here: [`stamp_group_slugs`]
/// writes the one winning value onto every task of the group, so a later
/// task's own answer never ends up on its `slug:` line.
///
/// A slug that [`accept_slug`] rejects, or a url that is not an absolute
/// `http`/`https` address, is dropped with a printed note — the batch queues
/// without it rather than refusing, since neither is load-bearing for the
/// ticket that was already opened.
fn record_slug_and_url(
    task: &mut Task,
    slug: &str,
    url: &str,
    key_in_names: bool,
    group: &str,
    group_slug: &mut BTreeMap<String, String>,
) {
    if key_in_names && !slug.is_empty() {
        if accept_slug(slug) {
            group_slug
                .entry(group.to_string())
                .or_insert_with(|| slug.to_string());
        } else {
            println!(
                "  issue_tracking: slug `{slug}` for `{}` is not a valid name \
                 (lowercase letters, digits and hyphens) — ignored",
                task.id()
            );
        }
    }
    if !url.is_empty() {
        if is_absolute_http_url(url) {
            task.set_extra_str("url", url);
        } else {
            println!(
                "  issue_tracking: url `{url}` for `{}` is not an absolute http(s) address — dropped",
                task.id()
            );
        }
    }
}

/// Stamp the one winning slug for each group onto every task of that group's
/// `slug:` line — the value [`open_tickets`] pinned in `group_slug`, which is
/// the first valid answer in dependency order. Run before a batch is saved
/// (via [`prefix_generated_names`]) and again before [`write_back_ids`] on a
/// mid-batch failure, so what lands on disk is the group's decision, never a
/// later task's own raw answer.
fn stamp_group_slugs(tasks: &mut [Task], group_slug: &BTreeMap<String, String>) {
    for task in tasks.iter_mut() {
        if let Some(group) = &task.front.group
            && let Some(slug) = group_slug.get(group)
        {
            task.set_extra_str("slug", slug);
        }
    }
}

/// Whether `s` is an absolute `http`/`https` URL with a host.
///
/// Parsed by the `url` crate — the WHATWG URL standard — rather than a
/// hand-rolled authority scan, so a malformed IPv6 literal (`https://[abc]/`,
/// `https://[::::]/`), an out-of-range port (`https://host:99999/`) and a
/// bare scheme (`https://`, `https://:`, `https://@`) all fail at
/// `Url::parse`, and a relative path (`/browse/PROJ-12`) is not a URL at all.
/// On top of "it parses": the scheme must be `http` or `https` and there must
/// be a non-empty host. Userinfo and a port are fine.
///
/// One check runs before the parser: the raw string must hold no whitespace.
/// The standard folds an interior space into the userinfo as `%20` and parses
/// on, so a hook answering a `url=` line with a space in it would otherwise
/// read back as a valid URL rather than the malformed answer it is.
///
/// spoolway hands the string on to `terminal-names` untouched and never
/// fetches it, so nothing past this is checked.
fn is_absolute_http_url(s: &str) -> bool {
    if s.contains(char::is_whitespace) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(s) else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str().is_some_and(|host| !host.is_empty())
}

/// This task's own dependencies, processed first, so `open_tickets` can walk
/// the whole batch in an order where `SPOOLWAY_DEPENDS_TICKETS` is always
/// answerable. Kahn's algorithm over `depends_on` edges that stay inside the
/// batch — a dependency named outside it is already queued or archived, so
/// its ticket is read straight off disk in [`dependency_ticket`] rather than
/// ordered for here. `validate_batch` has already refused any cycle, so this
/// never has to detect one of its own.
fn open_order(tasks: &[Task]) -> Vec<usize> {
    let index_of: BTreeMap<&str, usize> =
        tasks.iter().enumerate().map(|(i, t)| (t.id(), i)).collect();
    let mut indegree = vec![0usize; tasks.len()];
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); tasks.len()];
    for (i, task) in tasks.iter().enumerate() {
        for dep in &task.front.depends_on {
            if let Some(&j) = index_of.get(dep.as_str()) {
                children[j].push(i);
                indegree[i] += 1;
            }
        }
    }

    let mut ready: std::collections::VecDeque<usize> =
        (0..tasks.len()).filter(|&i| indegree[i] == 0).collect();
    let mut order = Vec::with_capacity(tasks.len());
    while let Some(i) = ready.pop_front() {
        order.push(i);
        for &child in &children[i] {
            indegree[child] -= 1;
            if indegree[child] == 0 {
                ready.push_back(child);
            }
        }
    }
    order
}

/// The ticket id `dep` already carries — a sibling task in this same
/// batch, already processed by the time `open_order` reaches whatever
/// depends on it, or a task already queued or archived from an earlier
/// call. Errors only if `dep` names neither, which `validate_batch`'s own
/// `check_dependencies_set` has already ruled out for every task that
/// reaches here.
fn dependency_ticket(repo: &Repo, tasks: &[Task], dep: &str) -> Result<String> {
    if let Some(task) = tasks.iter().find(|t| t.id() == dep) {
        return Ok(task.extra_str("ticket").to_string());
    }
    let existing = crate::task::find(&[&repo.queue_dir(), &repo.archive_dir()], dep)
        .with_context(|| format!("looking up `{dep}`'s own ticket"))?;
    Ok(existing.extra_str("ticket").to_string())
}

/// Write every value `open_tickets` already secured back into the task it
/// came from, in place on disk — called only once a hook call has failed,
/// so a re-run of the same `queue add` sees those tasks already carrying
/// `epic:`/`ticket:`/`slug:`/`url:` and does not undo the first call's work:
/// a `ticket:` reports the task `kept` and skips the hook, and a `slug:`
/// pins the group's prefix so the re-run's hook cannot answer a different
/// one.
///
/// `submitted` and `tasks` are index-aligned: `validate_batch` parses one
/// [`Task`] per submitted item, in the order `submitted` names them, and
/// never reorders that top-level list — only a task's own `depends_on` is
/// ever reordered. A task read from `-` (standard input) has no file to
/// write back to and is silently skipped; there is nowhere on disk for its
/// answer to resume from anyway. Returns whether any task was written at
/// all, so the failure message can say where the ids went — or that they
/// went nowhere.
fn write_back_ids(submitted: &[(String, String)], tasks: &[Task]) -> Result<bool> {
    let mut written = false;
    for (task, (name, raw)) in tasks.iter().zip(submitted.iter()) {
        let path = std::path::Path::new(name);
        if !path.is_file() {
            continue;
        }

        let mut updated = raw.clone();
        for key in ["epic", "ticket", "slug", "url"] {
            let value = task.extra_str(key);
            if !value.is_empty() {
                updated = with_frontmatter_field(&updated, key, value);
            }
        }
        if updated != *raw {
            crate::task::write_atomic(path, &updated)
                .with_context(|| format!("writing the opened ids back into {name}"))?;
            written = true;
        }
    }
    Ok(written)
}

/// Sections are appended to a body later, and `append_to_section` reasons in
/// whole lines. A body that stopped mid-line would take the first status log
/// entry onto the end of its last sentence.
fn ends_with_newline(mut body: String) -> String {
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body
}

/// Refuse a batch containing any task whose `depends_on` would never be
/// satisfiable, and put each survivor's `depends_on` in the order its cut
/// needs.
///
/// Run over the whole submission at once, not one task at a time: a
/// `depends_on` naming a sibling submitted alongside it has to resolve
/// against that sibling, which is not in `repo.tasks()` until every task
/// in this batch has already passed. Every failure this catches is silent
/// otherwise: the task sits on the wait step for as long as anyone leaves it
/// there, looking like ordinary queued work, or its worktree quietly misses
/// a parent's commits. Rejecting them here is what keeps the graph acyclic
/// and fully resolved by construction — `spoolway doctor` is the backstop
/// for hand-edited files.
fn check_dependencies_set(repo: &Repo, batch: &mut [Task]) -> Result<()> {
    let mut tasks = repo.tasks()?;

    // Every group comparison in this function goes through [`bare_group`],
    // so the one-chain, stacking and pending-group checks below can never
    // drift from what counts as "the same group" here.
    let bare_group = |t: &Task| -> Option<String> { self::bare_group(repo, t) };

    // Groups with a member still in the queue *before* this batch lands —
    // read here, before archived or batch tasks join `tasks` below, so nothing
    // this call is about to write can answer its own question. A routine or a
    // scheduled job mints a fresh id on every run but leaves `group:`
    // untouched (`mint_routine_batch`), so the same group name is queued
    // again and again, each time as its own one-task chain — once a run
    // lands and is archived, that name is free to be reused. Without this, a
    // second run would find its own predecessor's tail still sitting in
    // `by_group` below with nothing depending on it, and refuse itself as a
    // second root of a group whose first "root" already finished and left.
    let groups_still_queued: BTreeSet<String> = tasks.iter().filter_map(&bare_group).collect();

    // The one-chain and stacking rules below both read a group's whole shape
    // — every task it has ever had, not only what is still queued — because
    // a task that already landed and was archived still counts as that
    // group's root or tail, as long as the group is still open (see
    // `groups_still_queued` above). Read once, here, from the archive index
    // rather than through `status::mod`'s own `cached_archive`: that cache is
    // tuned for a per-second board redraw, and this runs once per `queue add`
    // or `task contract` call. The index costs one small file however many
    // tasks have finished, and holds every field this function reads of an
    // archived task, so no archived file is opened here. Sorted by id, the
    // order the folder listing gave, which the messages below name tasks in.
    let archive_dir = repo.archive_dir();
    let mut indexed = crate::archive_index::read_at(&archive_dir)?;
    indexed.sort_by(|a, b| a.id.cmp(&b.id));
    let archived: Vec<Task> = indexed
        .iter()
        .map(|entry| entry.to_task(&archive_dir))
        .collect();
    let archived_ids: BTreeSet<String> = archived.iter().map(|t| t.id().to_string()).collect();
    tasks.extend(archived);
    // A caller may hand this an id that is already on disk — a task
    // re-validated after `queue add` already wrote it, or a batch that
    // overlaps a sibling `--from` already queued in a run. `by_group` below
    // collects one `Vec` per group and would otherwise count such an id
    // twice within it, reading a lone root as a fan of one against itself.
    // The batch's own copy always wins: it is what a caller is asking this
    // to check right now.
    let batch_ids: BTreeSet<&str> = batch.iter().map(|t| t.id()).collect();
    tasks.retain(|t| !batch_ids.contains(t.id()));
    tasks.extend(batch.iter().cloned());
    let graph = Graph::build(&tasks, &repo.archive_dir());

    // Every task this batch can see, bare-grouped — the set the one-chain
    // and stacking checks below both walk. A trial arm is left out: it
    // forks a whole group for a side-by-side comparison run and has its own
    // `depends_on` emptied on the way in (`queue_routine_target`,
    // `begin_trial`) precisely so it never waits on the source it was
    // forked from. An arm minted before each copy got a group of its own
    // sat in the very group it forked, so counting it here would read that
    // deliberate fan as a second root of the group it forked from.
    let mut by_group: BTreeMap<String, Vec<&Task>> = BTreeMap::new();
    for t in &tasks {
        if t.front.trial.is_some() {
            continue;
        }
        let Some(g) = bare_group(t) else { continue };
        // An archived task of a group with no live queue member before this
        // batch is a past, closed instance of that name — see
        // `groups_still_queued` above — left out so a fresh batch under the
        // same name starts its own chain rather than inheriting a finished
        // one's shape.
        if archived_ids.contains(t.id()) && !groups_still_queued.contains(&g) {
            continue;
        }
        by_group.entry(g).or_default().push(t);
    }

    // A group's tasks depending on each other, restricted to edges that stay
    // inside `group` — a cross-group edge is the stacking check's business,
    // not this one's. A plain fn rather than a closure: a closure over
    // `by_group` cannot also be generic over the lifetime of whichever
    // `&Task` a caller hands it, and every caller below needs `t`'s own
    // lifetime to outlive the `Vec<&str>` this returns.
    fn in_group_deps<'a>(
        by_group: &BTreeMap<String, Vec<&'a Task>>,
        group: &str,
        t: &'a Task,
    ) -> Vec<&'a str> {
        let ids: BTreeSet<&str> = by_group
            .get(group)
            .into_iter()
            .flatten()
            .map(|m| m.id())
            .collect();
        t.front
            .depends_on
            .iter()
            .map(String::as_str)
            .filter(|d| ids.contains(d))
            .collect()
    }

    // The task with nothing depending on it inside `group` — the sink a
    // stacking edge is allowed to name. `None` for a group with more than
    // one, which the one-chain check below refuses before a stacking check
    // ever has to ask.
    let tail_of = |group: &str| -> Option<&str> {
        let members = by_group.get(group)?;
        let depended_on: BTreeSet<&str> = members
            .iter()
            .flat_map(|m| in_group_deps(&by_group, group, m))
            .collect();
        let mut tails = members
            .iter()
            .map(|m| m.id())
            .filter(|id| !depended_on.contains(id));
        let first = tails.next()?;
        match tails.next() {
            None => Some(first),
            Some(_) => None,
        }
    };

    // A group's own words for a count, matching the mockup's "two tasks" —
    // spelled out only for the common case, since a group this tangled is
    // rare enough that a bare digit is still plain English.
    let count_word = |n: usize| -> String {
        match n {
            2 => "two".to_string(),
            _ => n.to_string(),
        }
    };

    for task in batch.iter_mut() {
        let id = task.id().to_string();

        if task.front.depends_on.iter().any(|dep| dep == &id) {
            bail!("`{id}` cannot depend on itself");
        }

        let mine = bare_group(task);
        // Cross-group ids this task names — theirs, resolved once, so the
        // "one other group" and "that group's own last task" checks below
        // read the same answer the pending-directory check already used to
        // decide a dependency is unresolvable rather than merely elsewhere.
        let mut cross: Vec<(&str, String)> = Vec::new();
        let mut same_group_dep_count = 0usize;

        for dep in &task.front.depends_on {
            if graph.state(dep) == DepState::Unknown {
                // A dependency named on a task still sitting in the pending
                // directory — not yet run through `queue add` at all — reads
                // exactly like a typo to `Graph`, which only ever sees the
                // queue, the archive and this batch. Naming the group here
                // is what tells the two apart for whoever reads the refusal.
                if let Some(group) = pending_task_group(repo, dep) {
                    bail!(
                        "`{id}` depends on `{dep}`, in group `{group}`, which is still in the \
                         pending directory — queue `{group}` first."
                    );
                }
                let days = repo.config.housekeeping.archive_retention_days;
                if days > 0 {
                    // `retain` deletes an `archive/` entry once it is this
                    // old and only when a person has set
                    // `archive_retention_days`, and a dependency this refused
                    // could just as easily be a typo — so this names the age
                    // rather than claiming it, and still points at fixing the
                    // id.
                    bail!(
                        "`{id}` depends on `{dep}`, which is in neither the queue nor the \
                         archive — if `{dep}` finished more than {days} day(s) ago, \
                         `housekeeping.archive_retention_days` has already swept it out of the \
                         archive; otherwise check the id, or queue that task first"
                    );
                }
                bail!(
                    "`{id}` depends on `{dep}`, which is in neither the queue nor the \
                     archive — check the id, or queue that task first"
                );
            }

            // A group is one chain, and a group's first task may stack it
            // onto exactly one other group's own last task — see
            // `d-group-stacks-on-group` in the plan this shipped from. Every
            // other cross-group edge is refused below, once every dependency
            // has been sorted into "mine" or "theirs".
            let sibling = tasks.iter().find(|t| t.id() == dep);
            let theirs = sibling.and_then(bare_group);
            match (&mine, &theirs) {
                (Some(mine), Some(theirs)) if mine == theirs => same_group_dep_count += 1,
                (Some(_), Some(theirs)) => cross.push((dep.as_str(), theirs.clone())),
                _ => {}
            }

            // A dependent is cut straight from its dependency's branch now,
            // but that branch is itself only ever cut from — and merges
            // back into — its own group's base. Naming a dependency queued
            // on a different base is still naming one whose branch will
            // never land where this task's own pull request opens, so the
            // wait would end with the work still not in reach — a silent
            // breach of the one guarantee `depends_on` makes.
            let (mine, theirs) = (
                task.front.base.as_deref(),
                tasks
                    .iter()
                    .find(|t| t.id() == dep)
                    .and_then(|t| t.front.base.as_deref()),
            );
            if let (Some(mine), Some(theirs)) = (mine, theirs)
                && mine != theirs
            {
                bail!(
                    "`{id}` is based on `{mine}` but depends on `{dep}`, which is based on \
                         `{theirs}` — work only merges into its own base, so that wait would never \
                         put it in reach. Queue both from the same worktree."
                );
            }
        }

        // A cross-group edge is only ever a stacking edge: written on a
        // group's own first task (nothing else it names inside its own
        // group), naming exactly one other group, and naming that group's
        // own last task — the tail nothing else in it depends on. Anything
        // short of that shape is refused here, once every dependency this
        // task named has been sorted above into `cross` or counted against
        // `same_group_dep_count`.
        if !cross.is_empty() {
            if same_group_dep_count > 0 {
                bail!(
                    "`{id}` depends on `{}`, in another group, but also depends on a task in \
                     its own group `{}` — only a group's first task may stack onto another \
                     group. Move the cross-group `depends_on` onto the first task, or drop it.",
                    cross[0].0,
                    mine.as_deref().unwrap_or("")
                );
            }

            let mut groups: Vec<&str> = cross.iter().map(|(_, g)| g.as_str()).collect();
            groups.sort_unstable();
            groups.dedup();
            if groups.len() > 1 {
                bail!(
                    "`{id}` depends on tasks in {} other groups ({}) — a group may stack onto \
                     only one other group.",
                    groups.len(),
                    groups.join(", ")
                );
            }

            let their_group = groups[0];
            if cross.len() > 1 {
                bail!(
                    "`{id}` names {} tasks in group `{their_group}` — a group may stack onto \
                     another group's own last task, and no more than that one task.",
                    cross.len()
                );
            }

            let dep = cross[0].0;
            // A group that has fully landed — every task archived, nothing
            // left in the queue — is kept out of `by_group` above so a
            // routine can reuse its name, which leaves `tail_of` nothing to
            // read. Stacking onto it is still the ordinary order, though: the
            // pending-directory refusal tells people to queue the other group
            // first, and it may well land before the dependent is queued
            // (seen 2026-09-30, refused as "not one chain itself"). So an
            // archived task of a landed group counts as its last task as long
            // as nothing of that group depends on it. A routine's name may
            // hold many archived one-task runs, each its own tail, so this
            // asks about `dep` alone rather than for the group's one tail.
            if archived_ids.contains(dep) && !groups_still_queued.contains(their_group) {
                if let Some(next) = tasks.iter().find(|t| {
                    bare_group(t).as_deref() == Some(their_group)
                        && t.front.depends_on.iter().any(|d| d == dep)
                }) {
                    bail!(
                        "`{id}` depends on `{dep}`, but `{}` in group `{their_group}` depends \
                         on `{dep}` in turn — a group may only stack onto another group's own \
                         last task.",
                        next.id()
                    );
                }
            } else {
                match tail_of(their_group) {
                    Some(tail) if tail == dep => {}
                    Some(tail) => bail!(
                        "`{id}` depends on `{dep}`, but group `{their_group}`'s own last task is \
                     `{tail}` — a group may only stack onto another group's own last task."
                    ),
                    None => bail!(
                        "`{id}` depends on `{dep}` in group `{their_group}`, which is not one \
                     chain itself — fix `{their_group}` before stacking another group onto it."
                    ),
                }
            }
        }

        if let Some(cycle) = graph.cycle_with(&id) {
            bail!("{}", crate::graph::render_cycle(cycle));
        }

        // `ensure_workspace` (`src/dispatch.rs`) cuts a dependent straight
        // from `depends_on.first()`'s branch, and `spoolway stack` opens its
        // pull request against that same branch — every other id in the
        // list buys nothing but a wait unless the branch it is cut from
        // already carries that id's work too. So the list has to start with
        // whichever dependency's own history already reaches every other
        // one named beside it; `Graph::reaches` already answers that,
        // walking the edges the batch declared, so this needs no traversal
        // of its own. A single-id list has nothing to reorder.
        if task.front.depends_on.len() > 1 {
            let deps: Vec<&str> = task.front.depends_on.iter().map(String::as_str).collect();
            let head = deps.iter().copied().find(|&candidate| {
                deps.iter()
                    .copied()
                    .all(|other| other == candidate || graph.reaches(candidate, other))
            });

            match head {
                Some(head) => {
                    let mut reordered: Vec<String> = vec![head.to_string()];
                    reordered.extend(
                        deps.iter()
                            .copied()
                            .filter(|&d| d != head)
                            .map(str::to_string),
                    );
                    task.front.depends_on = reordered;
                }
                None => {
                    let first = deps[0];
                    let missing: Vec<&str> = deps
                        .iter()
                        .copied()
                        .skip(1)
                        .filter(|&d| !graph.reaches(first, d))
                        .collect();
                    let parents = deps.join("`, `");
                    let missing = missing.join("`, `");
                    bail!(
                        "`{id}` depends on `{parents}`, in its own group{group}, but none of \
                         them reaches all the others — a group is one chain, and a task \
                         joining two of it is not one. Its worktree would be cut from \
                         `{first}`, and `{missing}` would not be in it. A depends_on must \
                         start with the id that contains the rest, or drop one.",
                        group = mine
                            .as_deref()
                            .map(|g| format!(" `{g}`"))
                            .unwrap_or_default(),
                    );
                }
            }
        }
    }

    // A group is one chain: at most one root (no in-group dependency), and at
    // most one dependent per in-group task (no fan). Checked once per group,
    // after every task above has had its own `depends_on` validated and
    // reordered, over the whole set `by_group` already gathered — the queue,
    // the archive and this batch together — so a group split across two
    // `queue add` calls is caught exactly as a single-call fan would be.
    //
    // Only the groups this batch puts a task into, though. A group the batch
    // never touches is not this call's to judge: one already broken on disk —
    // hand-placed, or queued before groups were chains — would otherwise
    // refuse every unrelated `queue add` in the project by its own name, and
    // `spoolway doctor` is the backstop for those. A group the batch only
    // stacks onto is still checked, by `tail_of` above.
    //
    // A join — one task naming two in-group parents that do not reach each
    // other — never reaches here: the reorder loop above already refused it,
    // the moment such a task's own `depends_on.len() > 1` found no id
    // reaching every other one named beside it. What *does* reach here is a
    // task naming several in-group ids that *do* all chain through one of
    // them — not a join, just a verbose way of saying "wait for the tip of
    // the chain" — so a fan is counted against that one effective parent,
    // not against every id the task happened to list.
    let batch_groups: BTreeSet<String> = batch.iter().filter_map(&bare_group).collect();
    for (group, members) in by_group.iter().filter(|(g, _)| batch_groups.contains(*g)) {
        let roots: Vec<&str> = members
            .iter()
            .filter(|t| in_group_deps(&by_group, group, t).is_empty())
            .map(|t| t.id())
            .collect();
        if roots.len() > 1 {
            bail!(
                "group `{group}` has {} tasks with no dependency in it: {} — a group is one \
                 chain. Give one a `depends_on`, or move it to a group of its own.",
                count_word(roots.len()),
                roots.join(" and ")
            );
        }

        let mut dependents_of: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for t in members {
            let deps = in_group_deps(&by_group, group, t);
            // The one dependency the rest of `deps` all lead to — the same
            // "head" the reorder loop above already found for this very
            // list, so a task past that loop is guaranteed one here too.
            let effective_parent = match deps.len() {
                0 => None,
                1 => Some(deps[0]),
                _ => deps.iter().copied().find(|&candidate| {
                    deps.iter()
                        .copied()
                        .all(|other| other == candidate || graph.reaches(candidate, other))
                }),
            };
            if let Some(parent) = effective_parent {
                dependents_of.entry(parent).or_default().push(t.id());
            }
        }
        for (dep, dependents) in &dependents_of {
            if dependents.len() > 1 {
                bail!(
                    "group `{group}` has {} tasks depending on `{dep}`: {} — a group is one \
                     chain. Give one a `depends_on` on the other, or move it to a group of \
                     its own.",
                    count_word(dependents.len()),
                    dependents.join(" and ")
                );
            }
        }
    }

    Ok(())
}

/// `spoolway queue pause <id>`: interrupt any live agent lane the task owns,
/// then park it on `paused` — always, not only when something was actually
/// interrupted.
///
/// The board's own `p` now parks unconditionally too — see
/// `Board::begin_pause_cursor` — so the two agree on every state but one: a
/// running command step. There, `p` opens a confirm panel and waits on a
/// person to answer it, which a script has nobody to do; this refuses
/// outright instead, unless `--force` says the caller already knows what it
/// is choosing.
///
/// Interrupting a live agent lane is best-effort and always allowed — an
/// interrupted turn is not lost work, only a turn that ends early — the same
/// trade `p` itself makes. A running *command* step is different: killing one
/// throws away whatever it was doing, which is why the board asks first
/// rather than acting outright. A script has nobody to ask, so this refuses
/// instead, unless `--force` says the caller already knows what it is
/// choosing.
pub fn queue_pause(repo: &Repo, pipelines: &Pipelines, id: &str, force: bool) -> Result<()> {
    let mut tasks = repo.tasks()?;
    let idx = tasks
        .iter()
        .position(|t| t.id() == id)
        .with_context(|| format!("no queued task `{id}`"))?;

    let mux = crate::mux::backend(repo)?;
    let lanes = mux.list_lanes().unwrap_or_default();
    if crate::status::live_agent_lane_tasks(repo, &tasks, pipelines, &lanes).contains(&idx) {
        let name = crate::mux::lane_name(tasks[idx].stage(), tasks[idx].id());
        // Best-effort, the same as the board's own `p`: a lane that has
        // already gone quiet on its own has nothing left to interrupt.
        let _ = mux.interrupt_lane(&name);
    }

    let running: Vec<_> = crate::status::running_command_steps(repo, &tasks, pipelines)
        .into_iter()
        .filter(|cr| cr.task == id)
        .collect();
    if !running.is_empty() && !force {
        bail!(
            "`{id}` is running a command step (`{}`) — pass `--force` to stop it and \
             pause, or use the board's `p` key to choose interactively",
            running[0].step
        );
    }

    crate::status::park(&mut tasks[idx], "paused via `spoolway queue pause`", false);
    tasks[idx].save()?;

    // Stopped only once the task is on disk as paused. Between the kill and
    // the run's files being cleared the run reads as one that died without an
    // exit code, and a dispatcher pass landing in that gap with the task still
    // on the step would log "running it again" and forget the run, for a task
    // that is about to be paused and will run nothing.
    let runs = crate::command_step::Runs::new(&repo.commands_dir());
    for cr in &running {
        runs.stop(&crate::command_step::Runs::key(&cr.step, &cr.task));
    }
    println!("paused `{id}`");
    Ok(())
}

/// Whether `id` is still queued, or has a live agent lane or running command
/// step. That is a superset of what `spoolway resume` refuses: a blocked task
/// whose unblocker lane is live qualifies and is accepted — see `queue_resume`.
fn queued_or_running(repo: &Repo, pipelines: &Pipelines, id: &str) -> Result<bool> {
    let tasks = repo.tasks()?;
    let Some(idx) = tasks.iter().position(|t| t.id() == id) else {
        return Ok(false);
    };
    if crate::status::not_started(&tasks[idx]) {
        return Ok(true);
    }
    let lanes = crate::mux::backend(repo)?.list_lanes().unwrap_or_default();
    Ok(
        crate::status::live_agent_lane_tasks(repo, &tasks, pipelines, &lanes).contains(&idx)
            || crate::status::running_command_steps(repo, &tasks, pipelines)
                .iter()
                .any(|run| run.task == id),
    )
}

/// `spoolway queue resume <id>`: what the board's `r` key does to the row for
/// `id` — [`crate::status::resume_task`], with `spoolway resume`'s own refusal
/// put first for a task that is queued or has something running on its step.
pub fn queue_resume(repo: &Repo, pipelines: &Pipelines, id: &str) -> Result<()> {
    // `resume_task` is silent about a task the queue no longer has — right
    // for a keypress racing a second process, wrong for a script that named
    // one by hand, so that case is named here instead.
    repo.task(id)
        .with_context(|| format!("no queued task `{id}`"))?;
    // A task waiting in the queue or with a lane or command run live on its
    // step is not stopped, and goes through `spoolway resume`'s own refusal.
    // Only a row standing on a step with nothing running behind it — a
    // question nobody answered — is restarted from here.
    if queued_or_running(repo, pipelines, id)? {
        let args = crate::cli::ResumeArgs {
            task: id.to_string(),
            stage: None,
            message: None,
        };
        // This is the whole resume, not only a check: a task on `blocked` with
        // its unblocker lane live counts as running here, passes the guard, and
        // is moved by it. Resuming it again below would send it to `queued`.
        crate::commands::resume(repo, pipelines, &args, None)?;
    } else {
        crate::status::resume_task(repo, pipelines, id)?;
    }
    println!("resumed `{id}`");
    Ok(())
}

// ----------------------------------------------------------------- the screen
//
// Bare `spoolway`'s queue tab is the only thing that opens this any more —
// `spoolway queue` bare now prints its usage instead. The queue's own
// subcommands are unchanged, but where the work actually enters the queue
// used to be an agent skill typing `queue add --from` on a human's say-so is
// now this screen, reading the same tasks any producer writes into
// the pending directory and submitting through the very same `--from` path
// below. Nothing here decides a split or a pipeline — that stayed with
// whoever wrote the tasks; the screen's whole job is picking which
// already-written groups go, and when.
//
// spoolway does not know what a plan is any more. A group is a `group:`
// string a set of tasks share, and that is the only structure this reads:
// no page, no cards, no chips. A Jira ticket, a GitHub issue and the shipped
// `/spoolway-plan` skill all reach this list the same way, by writing `.md`
// files into one directory.
//
// This is the first code in the project that reads a keystroke —
// `platform::TermGuard` has taken the terminal for other reasons before, but
// nothing has ever read what came back. See `crate::screen::read_key` for why
// that reads one byte at a time rather than reaching for a terminal crate:
// the same loop has to run identically whether stdin is a real tty in raw
// mode or a pipe an end-to-end suite is scripting, and a byte read is the one
// primitive both give for free. `spoolway eval`'s own bare screen is the
// second thing to read a keystroke, and shares that reader and the small
// drawing helpers below through `crate::screen` rather than each deciding
// them for itself.

use super::jobs::WalkStep;
use super::pending::{Group, GroupState, PendingTask, TaskState};
use super::routines::{RoutineFolder, RoutineTask};

/// Which pane a `Char(' ')` or an arrow acts on.
///
/// `pub(super)` because bare `spoolway`'s jobs tab reuses this module's
/// routines browser — its keys and its navigation — to pick a job's target
/// — see [`super::jobs::jobs_tab`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Focus {
    Groups,
    Tasks,
}

/// What a key press means right now — the screen has three, and only one is
/// the ordinary browsing state a person spends most of their time in.
#[derive(Debug)]
enum Mode {
    Browsing,
    /// The filter box has focus: typing narrows the pending pane, `enter`
    /// keeps whatever query is in `ScreenState::filter` and returns to
    /// browsing, `esc` clears it first. The query text itself lives on
    /// `ScreenState` rather than here, so it survives leaving this mode —
    /// see that field's own doc comment.
    Filter,
    /// Choosing a step off the highlighted task's own pipeline, cursor into
    /// that pipeline's `steps`.
    Gate(usize),
    /// Forking a whole group once per ticked pipeline — `t`'s own two
    /// screens, ticking the pipelines and then choosing what each one
    /// skips. The whole of what was picked lives on the [`TrialState`] it
    /// carries, not split across `ScreenState` fields the way `gates` is: a
    /// trial never touches the outer selection, and `esc` off the first
    /// screen dropping this mode is what "leaves the selection exactly as
    /// it was" means.
    Trial(TrialState),
    /// What validating or writing a submission came back with — a refusal,
    /// a result — in a popup over the screen it came from, held until
    /// `enter` closes it. This exists because `draw`'s first act is always
    /// to clear the screen — a message written straight to `out` and then
    /// let the loop redraw over would never be read at all.
    ///
    /// `routines` is the routines pane the popup was opened over, when it
    /// was: that pane stays drawn under it, and closing it goes back there
    /// rather than to the pending screen.
    Outcome {
        notice: Notice,
        routines: Option<RoutineNav>,
    },
    /// The tool-requirements gate, asked in a popup over the screen before
    /// a submit reaches the hook — the printed form is
    /// [`tool_requirements_gate`]. `panel` is [`tool_gate_panel`]'s; `then`
    /// is the submit `enter` goes back and finishes with issue tracking
    /// switched off, and the screen `esc` goes back to having queued
    /// nothing.
    ToolGate {
        panel: Vec<String>,
        then: Resume,
    },
    /// The question a submit stops at before any ticket is opened, with
    /// issue tracking on — [`issue_question`]'s panel. A ticket on a public
    /// tracker is seen by other people and hard to take back, so nothing
    /// reaches the hook until a person says yes here. `enter` runs `then`
    /// again and opens the tickets, `n` runs it with issue tracking switched
    /// off for it, and `esc` goes back having queued nothing.
    IssueQuestion {
        panel: Vec<String>,
        then: Resume,
    },
    /// What a screen submit queued — [`queued_panel`]'s popup, the tickets
    /// the hook opened when it opened any. `enter` closes it, the same as
    /// [`Mode::Outcome`].
    ///
    /// `routines` is the routines pane the submit came from, when it did:
    /// the routines tab has no pending screen to fall back to, so that pane
    /// stays drawn under the popup and closing it goes back there.
    Queued {
        panel: Vec<String>,
        routines: Option<RoutineNav>,
    },
    /// The notice bare `spoolway` opens on once an update is installed but
    /// not yet synced — [`crate::gate::sync_popup`]'s panel. `enter`
    /// dismisses it, the only key it reads, and writes nothing: only
    /// `spoolway sync` applies the update.
    SyncGate(Vec<String>),
    /// The overrides bare `spoolway` opens on that the load left out —
    /// [`crate::commands::ignored_popup`]. `enter` closes it, the only key
    /// it reads, the same as [`Mode::Queued`].
    Ignored(crate::commands::IgnoredPopup),
    /// The routines tab's own screen: the left pane swapped for the routines
    /// under `.spoolway/routines/`, one row each — see [`RoutineNav`] for
    /// what it tracks between keys. Only [`routines_tab`] opens it, and no
    /// key leaves it for [`Mode::Browsing`]: the queue tab has no way in, so
    /// the two tabs never share a visit. The folders and tasks themselves
    /// live in `run_screen`'s own `routines`, read fresh on every visit to
    /// the tab, the same way `groups` is read once up front rather than
    /// carried on the mode, and read again after `x` deletes one.
    Routines(RoutineNav),
    /// `x`'s own popup over the routines pane `nav` describes: the routine
    /// and every job that points into it, deleted together on `enter` —
    /// see [`delete_routine`] — and all kept on `esc`. Every other key does
    /// nothing, so a stray keystroke can neither delete nor dismiss it.
    DeleteRoutine {
        nav: RoutineNav,
        target: RoutineDelete,
    },
    /// `n`'s schedule and pipeline popups over the routines pane `nav`
    /// describes, writing a job for the routine highlighted when `n` was
    /// pressed. The popups, their keys and the save are the jobs tab's own —
    /// see [`super::jobs::NewJobWalk`] — so `esc` on either comes back here
    /// having written nothing.
    NewJob {
        nav: RoutineNav,
        walk: super::jobs::NewJobWalk,
    },
    /// What `n` saved, over the routines pane — [`super::jobs::saved_notice`].
    /// Apart from [`Mode::Outcome`] only in its key line, which names the one
    /// key the popup reads rather than the pane's under it. `enter` closes it
    /// back onto the pane.
    JobSaved {
        nav: RoutineNav,
        notice: Notice,
    },
    /// `s`'s own panel, over the pending screen: saving the named group's
    /// tasks into `.spoolway/routines/<name>/`, `name` typed and edited
    /// the same way `Mode::Filter`'s query is. `group` is fixed at the
    /// moment `s` was pressed, so moving the cursor underneath this panel
    /// — which nothing here lets happen, but the field says so regardless —
    /// could never save the wrong group's tasks.
    SaveRoutine {
        group: GroupKey,
        name: String,
    },
}

/// A [`Mode::Outcome`] over the pending screen.
fn outcome(title: &str, text: impl Into<String>) -> Mode {
    outcome_over(None, title, text)
}

/// A [`Mode::Outcome`] over the routines pane `routines` describes, or over
/// the pending screen with none.
fn outcome_over(routines: Option<&RoutineNav>, title: &str, text: impl Into<String>) -> Mode {
    Mode::Outcome {
        notice: Notice::new(title, text),
        routines: routines.cloned(),
    }
}

/// The submit [`Mode::ToolGate`] or [`Mode::IssueQuestion`] finishes once it
/// is answered, run again from the start as the answer says — see
/// [`resume`].
#[derive(Debug, Clone)]
enum Resume {
    /// The pending screen's `enter`, over `ScreenState::selected`.
    Selection,
    /// The routines pane's `enter`, over the folders ticked in `nav`.
    Routines(RoutineNav),
    /// The routines pane's `space`, over the task under `nav`'s cursor.
    RoutineTask(RoutineNav),
}

impl Resume {
    /// Where `esc` off the gate goes back to: the screen the submit started
    /// from.
    fn back(&self) -> Mode {
        match self {
            Resume::Selection => Mode::Browsing,
            Resume::Routines(nav) | Resume::RoutineTask(nav) => Mode::Routines(nav.clone()),
        }
    }
}

/// Whether a screen submit may still stop at the tool-requirements gate or
/// the issue question, and how it answered them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tracking {
    /// Ask first: an unmet requirement opens [`Mode::ToolGate`], and with
    /// issue tracking on [`Mode::IssueQuestion`] asks before any ticket is
    /// opened. Nothing is queued until each is answered.
    Ask,
    /// `enter` on the tool gate, or `n` on the question: queue with issue
    /// tracking switched off.
    Off,
    /// `enter` on the question: open the tickets and queue.
    Open,
}

/// The gate a submit about to reach [`open_and_prefix`] stops at, or `None`
/// when it goes straight on: already answered, or every requirement met.
/// `Open` has passed the gate on the way to the question.
fn tool_gate(repo: &Repo, tracking: Tracking, then: Resume) -> Option<Mode> {
    if tracking != Tracking::Ask {
        return None;
    }
    tool_gate_popup(repo).map(|panel| Mode::ToolGate { panel, then })
}

/// The issue question's keys, in its popup.
fn issue_keys() -> String {
    keys(&[
        ("enter", "create and queue"),
        ("n", "queue only"),
        ("esc", "back"),
    ])
}

/// A body for one of the issue popups — the question, the tickets opening,
/// what was queued — opening on a blank row as wide as the question's own
/// key row. The three follow one another over the same spot, and holding
/// them to one width keeps the box from jumping between them as each
/// replaces the last, or growing a column every time a row comes in.
fn issue_body(lines: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut body = vec![" ".repeat(issue_keys().chars().count())];
    body.extend(lines);
    body
}

/// [`Mode::IssueQuestion`] over the batch `tasks`, or `None` when there is
/// nothing to ask: the submit has already been answered, no hook is
/// configured, or every task already names its `ticket:` — a batch a failed
/// hook wrote back, which [`open_tickets`] reports `kept` and never hands
/// the hook again.
///
/// Lists every task in the batch and names the tracker — see
/// [`crate::tracking::tracker`]. The count is the tickets still to open.
fn issue_question(repo: &Repo, tracking: Tracking, tasks: &[Task], then: Resume) -> Option<Mode> {
    if tracking != Tracking::Ask || !crate::tracking::configured(repo) {
        return None;
    }
    let opening = tasks
        .iter()
        .filter(|task| task.extra_str("ticket").is_empty())
        .count();
    if opening == 0 {
        return None;
    }
    let mut groups: Vec<&str> = Vec::new();
    for group in tasks.iter().filter_map(|task| task.front.group.as_deref()) {
        if !groups.contains(&group) {
            groups.push(group);
        }
    }
    let ask = format!(
        "create {} on {} for {}",
        plural(opening, "issue"),
        crate::tracking::tracker(repo),
        groups.join(", ")
    );
    let tasks = tasks.iter().map(|task| format!("  {}", task.id()));
    let body = issue_body(std::iter::once(ask).chain(tasks));
    Some(Mode::IssueQuestion {
        panel: panel("issue tracking", &body, &issue_keys()),
        then,
    })
}

/// One ticket row in the issue popups — narrower than the printed form's
/// id column, which leaves room for a jira key the popup's width has no
/// room to spare for.
fn ticket_row(kind: &str, status: &str, ticket: &str, name: &str) -> String {
    format!("{kind:<12} {status:<9} {ticket:<6} {name}")
}

/// [`TicketLog`] for the queue screen: each row goes into the `opening
/// issues` popup as the hook answers it, and `redraw` lays the popup over
/// the tab again straight away — the hook runs on the screen's own thread,
/// so nothing else draws until it returns. The task at the hook is drawn
/// as a `…` row until its answer replaces it, over room for every row the
/// batch can still add, so the box does not grow row by row under the
/// person watching it.
struct PopupTickets<'a> {
    rows: Vec<String>,
    waiting: Option<String>,
    /// A ticket per task and an epic per group: the most rows the batch can
    /// answer with.
    room: usize,
    redraw: &'a mut dyn FnMut(&[String]),
}

impl<'a> PopupTickets<'a> {
    fn new(tasks: &[Task], redraw: &'a mut dyn FnMut(&[String])) -> PopupTickets<'a> {
        let groups: std::collections::BTreeSet<_> = tasks
            .iter()
            .filter_map(|task| task.front.group.as_deref())
            .collect();
        PopupTickets {
            rows: Vec::new(),
            waiting: None,
            room: tasks.len() + groups.len(),
            redraw,
        }
    }

    /// The popup while the hook is still answering.
    fn panel(&self) -> Vec<String> {
        let mut rows = self.rows.clone();
        rows.extend(
            self.waiting
                .as_deref()
                .map(|id| ticket_row("task issue", "…", "", id)),
        );
        while rows.len() < self.room {
            rows.push(String::new());
        }
        rows.extend([
            String::new(),
            String::new(),
            "waiting on the hook".to_string(),
        ]);
        crate::screen::boxed("opening issues", &issue_body(rows))
    }

    fn show(&mut self) {
        let panel = self.panel();
        (self.redraw)(&panel);
    }
}

impl TicketLog for PopupTickets<'_> {
    fn group(&mut self, _group: &str) {}

    fn waiting(&mut self, id: &str) {
        self.waiting = Some(id.to_string());
        self.show();
    }

    fn row(&mut self, kind: &str, status: &str, ticket: &str, name: &str) {
        // A group issue is answered in the same call as the first task issue of its
        // group, and drawn above it — the task stays at the hook until its
        // own row comes in.
        if kind != "group issue" {
            self.waiting = None;
        }
        self.rows.push(ticket_row(kind, status, ticket, name));
        self.show();
    }

    /// Left off the popup. The popups draw tickets and nothing else, and a
    /// note is no ticket: the names-prefixed line — which the shipped
    /// `github.sh` earns on every batch by always answering `slug=` — comes
    /// after [`TicketLog::done`], so drawing it would put `waiting on the
    /// hook` back up once the hook has finished, and it is wider than
    /// [`issue_body`]'s width, so it would stretch the box. The prefix
    /// itself is on every queued name the tab lists.
    fn note(&mut self, _line: &str) {}

    fn done(&mut self) {
        self.waiting = None;
    }
}

/// [`Mode::Queued`] for a batch that queued `ids`: the tickets the hook
/// answered with, when it was asked, over the count queued; or, with no
/// ticket to show, the count over every task it queued. Then the tasks it
/// left out, `not_queued`, when there are any — see [`not_queued_lines`].
/// Either ends on `dispatcher` — [`dispatcher_line`] — a blank row below the
/// list, and then the [`crate::screen::confirm`] row. `routines` is the
/// routines pane the batch was queued from, or `None` over the pending
/// screen.
///
/// A batch that queued nothing at all, every task left out, draws only what
/// it left out, under the title `not queued`.
fn queued_panel(
    ids: &[String],
    tickets: &[String],
    not_queued: &[NotQueued],
    dispatcher: Option<&str>,
    routines: Option<RoutineNav>,
) -> Mode {
    let queued = format!("queued {}", plural(ids.len(), "task"));
    let mut lines = if ids.is_empty() {
        Vec::new()
    } else if tickets.is_empty() {
        std::iter::once(queued)
            .chain(ids.iter().map(|id| format!("  {id}")))
            .collect()
    } else {
        let mut lines = tickets.to_vec();
        lines.extend([String::new(), queued]);
        lines
    };
    if !not_queued.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(not_queued_lines(not_queued));
    }
    if let Some(dispatcher) = dispatcher {
        lines.extend([String::new(), dispatcher.to_string()]);
    }
    let title = if ids.is_empty() {
        "not queued"
    } else if tickets.is_empty() {
        "queued"
    } else {
        "issues created"
    };
    Mode::Queued {
        panel: panel(title, &issue_body(lines), &confirm()),
        routines,
    }
}

/// The queued popup's account of the tasks a batch left out: one row each,
/// a dependent annotated with the left-out task it depends on and the
/// annotations lined up in one column, then one sentence for each task whose
/// own start branch does not exist, wrapped to the popup's width.
fn not_queued_lines(not_queued: &[NotQueued]) -> Vec<String> {
    let widest = not_queued
        .iter()
        .map(|n| n.id.chars().count())
        .max()
        .unwrap_or(0);
    let mut lines = vec!["not queued, start branch doesn't exist".to_string()];
    for n in not_queued {
        lines.push(match &n.why {
            NotQueuedWhy::MissingStart(_) => format!("  {}", n.id),
            NotQueuedWhy::DependsOn(dep) => format!(
                "  {:<widest$}{}(depends on {dep})",
                n.id,
                crate::status::GUTTER
            ),
        });
    }
    for sentence in missing_start_sentences(not_queued) {
        lines.push(String::new());
        lines.extend(crate::screen::wrap(&sentence, crate::screen::NOTICE_WRAP));
    }
    lines
}

/// The queued popup's last line: whether a dispatcher will pick the batch
/// up. Read once, as the popup is built, and never again while it is up.
///
/// Reads the dispatcher's own lock and nothing else — the same answer
/// [`queue_list`] prints. [`crate::commands::dispatch::already_running`]
/// also reads the screen's lock, which the screen drawing this popup always
/// holds, so it would say "running" every time. A lock left by a dead
/// process reads as free, since [`crate::lock::Lock::holder`] checks its
/// process is alive. A lock that cannot be read at all gives `None`, and
/// the popup leaves the line off rather than claim a state it never read.
fn dispatcher_line(repo: &Repo) -> Option<&'static str> {
    match crate::lock::Lock::holder(&repo.lock_file()) {
        Ok(Some(_)) => Some("Dispatcher is running"),
        Ok(None) => Some("Start the dispatcher to begin working"),
        Err(_) => None,
    }
}

/// How the screen ended: on its own, or — inside bare `spoolway`'s queue or
/// routines tab — handing a key back to the shell around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenExit {
    Quit,
    /// `←`, `→` or `q` on a tab's home screen with no popup open, inside
    /// bare `spoolway`'s queue or routines tab — see
    /// [`crate::screen::shell::leave_on`]. Never reached outside those tabs,
    /// where nothing is hosting the screen to hand a key back to.
    Leave(crate::screen::shell::Leave),
}

/// A task, addressed by the path of the task it is written in — exactly
/// the name [`gather_one`] gives that same task, so the key a person's
/// gate is held under is the name a submission failure would report by.
type TaskKey = String;

fn task_key(task: &PendingTask) -> TaskKey {
    task.path.display().to_string()
}

/// A selected group, addressed by its `group:` string.
///
/// A whole group is the unit a person submits: its tasks are a chain, written
/// together and ordered by `depends_on`, and half a chain in the queue is a
/// task waiting on a dependency nobody queued. The tasks pane is what that
/// group holds, not a second list to pick from.
type GroupKey = String;

fn group_key(group: &Group) -> GroupKey {
    group.name.clone()
}

/// Whether a group can be submitted at all: it has to have tasks, and at
/// least one of them must still be waiting in the pending directory — see
/// [`GroupState`], which is what `h` hides on.
fn selectable(group: &Group) -> bool {
    group.state == GroupState::Queueable && !group.tasks.is_empty()
}

/// [`Mode::Routines`]'s own state: which pane has focus, where each pane's
/// own cursor sits, and which routines are ticked for `enter` to queue.
///
/// There is no breadcrumb: a routine is one folder directly under
/// `.spoolway/routines/`, and the list is only ever those. A subfolder
/// inside one is never a row of its own — its tasks are already folded into
/// the routine it sits in (see [`RoutineFolder::tasks`]), so they show and
/// queue with it.
///
/// The folders themselves are not here — it lives in `run_screen`'s own
/// `routines`, read fresh on every visit to the routines tab and again
/// after `x` deletes one, the same read
/// [`super::routines::list_routines`] gives the empty-directory case no
/// error over.
///
/// `pub(super)` along with its fields so the `spoolway jobs` screen can drive
/// the same browser and then read back which folder or task was picked.
#[derive(Debug, Clone)]
pub(super) struct RoutineNav {
    pub(super) focus: Focus,
    pub(super) folder_cursor: usize,
    pub(super) task_cursor: usize,
    /// Ticked folders, by their own absolute path — the same identity
    /// [`RoutineFolder::path`] carries, so two folders that happen to share
    /// a name in different parents are never confused for one another.
    pub(super) selected: std::collections::BTreeSet<std::path::PathBuf>,
}

impl RoutineNav {
    pub(super) fn new() -> RoutineNav {
        RoutineNav {
            focus: Focus::Groups,
            folder_cursor: 0,
            task_cursor: 0,
            selected: Default::default(),
        }
    }
}

/// [`Mode::Trial`]'s two screens, in the order `t` walks a person through
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrialStage {
    /// Ticking which of the project's pipelines the group is tried under —
    /// one row per name [`trial_pipeline_names`] lists.
    PickPipelines,
    /// Choosing which steps of each ticked pipeline to skip, one ticked
    /// pipeline to a page — see [`choose_skips_panel`].
    ChooseSkips,
}

/// [`Mode::Trial`]'s own state: the group `t` was pressed over, fixed for as
/// long as this mode stays open — the same reason [`Mode::SaveRoutine`]
/// fixes its own group rather than reading `state.group_cursor` live, since
/// nothing here lets the cursor move underneath this panel but the field
/// says so regardless — which of its two screens is showing, and the
/// picks made so far on each: the pipelines ticked, and the steps of each
/// one ticked to skip. Both are keyed by pipeline name rather than by
/// position or by task: every task of the group runs under every ticked
/// pipeline, one full copy of the group each, so a skip set belongs to a
/// pipeline and reaches every arm of that pipeline's copy.
#[derive(Debug, Clone)]
struct TrialState {
    group: GroupKey,
    stage: TrialStage,
    /// The cursor's own meaning changes with `stage`: an index into
    /// [`trial_pipeline_names`] while picking pipelines, an index into the
    /// steps of the page's own pipeline while choosing skips.
    cursor: usize,
    /// Which ticked pipeline the skip screen shows, an index into
    /// [`trial_ticked`] — see [`trial_page`]. Unused on the first screen.
    page: usize,
    ticked: std::collections::BTreeSet<String>,
    skip: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}

impl TrialState {
    /// Opened by `t`: every pipeline one of the group's own tasks names
    /// starts ticked, so the run a person was already going to make is one
    /// of the arms without a key pressed. Only a name the project still has
    /// is ticked — a stale `pipeline:` would otherwise be a tick with no row
    /// to untick it from. The cursor starts on the first tick, where the
    /// mockup draws it, or on the top row when nothing is ticked.
    fn new(pipelines: &Pipelines, group: &Group) -> TrialState {
        let names = trial_pipeline_names(pipelines);
        let ticked: std::collections::BTreeSet<String> = group
            .tasks
            .iter()
            .filter_map(|task| doc_pipeline_name(&task.text().ok()?))
            .filter(|name| names.contains(&name.as_str()))
            .collect();
        let cursor = names
            .iter()
            .position(|name| ticked.contains(*name))
            .unwrap_or(0);
        TrialState {
            group: group_key(group),
            stage: TrialStage::PickPipelines,
            cursor,
            page: 0,
            ticked,
            skip: Default::default(),
        }
    }
}

/// The group [`Mode::Trial`] targets, read fresh off `groups` by the id it
/// was opened with rather than trusted to still sit where the cursor left
/// it — a reload between one key and the next can reorder or drop groups out
/// from under a mode that outlives a single frame, the same care
/// [`save_routine_panel`] already takes for [`Mode::SaveRoutine`].
fn trial_group<'a>(groups: &'a [Group], trial: &TrialState) -> Option<&'a Group> {
    groups.iter().find(|group| group_key(group) == trial.group)
}

/// The ticked pipelines, in the order [`trial_pipeline_names`] lists them —
/// the order the skip screen turns its pages in and [`begin_trial`] mints
/// their copies' ids in. A tick naming a pipeline a reload has since taken
/// away is left out, the same care [`trial_group`] takes over a group that
/// moved: there is nothing left to run it under.
fn trial_ticked<'a>(
    pipelines: &'a Pipelines,
    trial: &TrialState,
) -> Vec<(&'a str, &'a crate::pipeline::Pipeline)> {
    pipelines
        .pipelines
        .iter()
        .filter(|(name, _)| trial.ticked.contains(*name))
        .map(|(name, pipeline)| (name.as_str(), pipeline))
        .collect()
}

/// `h`'s own two-way switch: whether the left pane shows the done groups
/// beside the opening, queueable-only view. [`HideScope::next`] flips
/// between the two.
///
/// A group the queue holds even in part is never one of them, at either
/// setting — see [`reachable`]. Nothing on this screen can act on it, and
/// the dispatch tab is where running work is watched; a three-step cycle
/// that also reached it made a person remember which step they were on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HideScope {
    /// The opening state, unchanged: only groups something is still
    /// queueable in.
    Pending,
    /// Plus every group the archive holds in full.
    PlusDone,
}

impl HideScope {
    /// The other setting — what one more `h` press moves to.
    fn next(self) -> HideScope {
        match self {
            HideScope::Pending => HideScope::PlusDone,
            HideScope::PlusDone => HideScope::Pending,
        }
    }
}

/// The screen's whole state between one key and the next.
struct ScreenState {
    focus: Focus,
    mode: Mode,
    hide_scope: HideScope,
    /// The filter's own query text, typed in [`Mode::Filter`] and kept here
    /// rather than on the mode so it survives leaving and re-entering that
    /// mode — `enter` returns to browsing with the narrowed list still in
    /// effect, and only `esc` (from inside the filter) clears it. Empty
    /// means no filter is in effect at all; see [`shown`].
    filter: String,
    group_cursor: usize,
    task_cursor: usize,
    selected: std::collections::BTreeSet<GroupKey>,
    /// A gate chosen from the screen, kept apart from the task itself —
    /// see `with_gate` for why the file on disk is never rewritten to record
    /// one.
    gates: std::collections::BTreeMap<TaskKey, String>,
    /// The branch the board's own checkout has out — what `enter` hands a
    /// task that names no `base:` of its own, and so what the tasks pane's
    /// `Lands in:` row shows for one. `None` on a detached checkout, where
    /// `enter` refuses rather than guess, and in the tests that open the
    /// screen with no checkout behind it.
    board_branch: Option<String>,
    /// Popups waiting behind the one on screen, each opened in turn as the
    /// one before it closes — the notices bare `spoolway` opens with, which
    /// can be more than one at once.
    waiting: std::collections::VecDeque<Mode>,
}

impl ScreenState {
    fn new() -> ScreenState {
        ScreenState {
            focus: Focus::Groups,
            mode: Mode::Browsing,
            // A group the archive already holds is one that can only be
            // scrolled past — see `selectable` — so the screen opens on the
            // groups a person can actually act on, with the done ones one
            // key away rather than crowding the list from the first frame.
            hide_scope: HideScope::Pending,
            filter: String::new(),
            group_cursor: 0,
            task_cursor: 0,
            selected: Default::default(),
            gates: Default::default(),
            board_branch: None,
            waiting: Default::default(),
        }
    }

    /// What the screen shows once the popup on it closes: the next one
    /// waiting, or `closed` with none.
    fn after_popup(&mut self, closed: Mode) -> Mode {
        self.waiting.pop_front().unwrap_or(closed)
    }
}

/// Whether `group` is one this screen lists at all, under any [`HideScope`]
/// or filter: queueable or done, never [`GroupState::Queued`]. The pane
/// title's total and the "nothing to queue — N hidden" count are both taken
/// over these alone, so neither counts a group no key here can bring back.
fn reachable(group: &Group) -> bool {
    group.state != GroupState::Queued
}

/// The groups currently on screen at this [`HideScope`] — `h`'s whole
/// effect, computed fresh each frame rather than stored, so nothing has to
/// be reconciled when a group leaves the list entirely.
fn visible(groups: &[Group], scope: HideScope) -> Vec<&Group> {
    groups
        .iter()
        .filter(|group| match scope {
            HideScope::Pending => group.state == GroupState::Queueable,
            HideScope::PlusDone => reachable(group),
        })
        .collect()
}

/// The groups currently on screen, in the order they are drawn: `h`'s
/// ordinary hide/show list when there is no filter, or every group that
/// clears [`group_score`] against the filter's own query, ranked best score
/// first, when there is — reaching a done group `h` is hiding, since that is
/// the one acceptance criterion `visible` alone could never satisfy, but
/// never a queued one, which [`reachable`] keeps off this screen. A tie in
/// score falls back to name order, the same tie-break
/// [`super::pending::list_groups`] itself uses, so the list does not
/// reshuffle between two draws that score identically.
fn shown<'a>(groups: &'a [Group], state: &ScreenState) -> Vec<&'a Group> {
    if state.filter.is_empty() {
        return visible(groups, state.hide_scope);
    }
    let mut ranked: Vec<(i64, &Group)> = groups
        .iter()
        .filter(|group| reachable(group))
        .filter_map(|group| group_score(group, &state.filter).map(|score| (score, group)))
        .collect();
    ranked.sort_by(|(sa, ga), (sb, gb)| sb.cmp(sa).then_with(|| ga.name.cmp(&gb.name)));
    ranked.into_iter().map(|(_, group)| group).collect()
}

/// A group's own rank against a filter query: the best of its fields' scores
/// under [`super::pending::score`], each weighted the way step 6 of the
/// scoring rule says — or `None` when nothing in the group clears
/// [`super::pending::MATCH_FLOOR`] at all.
///
/// Every task's title is skipped below
/// [`super::pending::RICH_FIELD_MIN_QUERY`] characters — see that constant's
/// own doc comment.
fn group_score(group: &Group, query: &str) -> Option<i64> {
    use super::pending::score;

    let mut best: Option<i64> = None;
    let mut consider = |field: &str, weight: i64| {
        if let Some(s) = score(query, field) {
            let total = s + weight;
            if best.is_none_or(|b| total > b) {
                best = Some(total);
            }
        }
    };

    consider(&group.name, super::pending::GROUP_WEIGHT);
    for task in &group.tasks {
        consider(&task.id, super::pending::TASK_ID_WEIGHT);
    }
    if query.chars().count() >= super::pending::RICH_FIELD_MIN_QUERY {
        for task in &group.tasks {
            if let Some(description) = &task.description {
                consider(description, super::pending::DESCRIPTION_WEIGHT);
            }
        }
    }

    best.filter(|&s| s >= super::pending::MATCH_FLOOR)
}

/// What the queue tab holds as its opening message — or `None` when there is
/// nothing wrong with the pending directory worth saying first.
///
/// An empty pending directory is not one of those things. The screen opens
/// on it like any other, and its own empty left pane says there is nothing
/// to queue. It used to print a message here naming `/spoolway-plan` as the
/// way to fill the directory, which made a bundled sample skill look like a
/// dependency of the binary; tasks reach that directory from anywhere,
/// and this screen has no business naming one writer of them.
///
/// The unreadable-tasks check is gated on no group being
/// [`super::pending::GroupState::Queueable`] any more — true both when
/// `groups` is empty outright and when it holds only rows nobody can select
/// — rather than on `groups.is_empty()` alone. `groups` now also reflects
/// the queue and archive directories (see [`super::pending::list_groups`]),
/// so a group already queued or archived can leave it non-empty even when
/// every task actually sitting in the pending directory is unreadable;
/// gating on emptiness alone would either hide the diagnostic behind that
/// unrelated row, or — the fix that overcorrected the first time — hide a
/// perfectly queueable group behind a stray task that has nothing to do
/// with it. `all` on an empty slice is `true`, so this one condition covers
/// the ordinary empty-pending case together with both the queue-only and
/// archive-only ones.
///
/// Pulled out of [`queue_tab`] so this can be checked without a real
/// terminal or a captured stdout — the same split `skeleton_task` makes
/// from `print_skeleton_task`.
fn opening_message(repo: &Repo, groups: &[Group]) -> Option<String> {
    let dir = repo.pending_dir();
    if groups
        .iter()
        .all(|group| group.state != GroupState::Queueable)
    {
        let skipped = super::pending::unreadable(repo);
        if !skipped.is_empty() {
            let files: Vec<String> = skipped
                .iter()
                .map(|path| format!("  {}", path.display()))
                .collect();
            return Some(format!(
                "Nothing to list under {} — {} there {} no `group:` this could read:\n\n\
                 {}\n\n\
                 A row on this screen is a `group:` value, so a task without one has no row \
                 to be. Run `spoolway task contract --from {}` for the real reason on each.",
                dir.display(),
                plural(skipped.len(), "task"),
                match skipped.len() {
                    1 => "has",
                    _ => "have",
                },
                files.join("\n"),
                dir.display()
            ));
        }
    }

    None
}

/// Bare `spoolway`'s queue tab: the same screen `spoolway queue` used to open
/// on its own, over the terminal the shell around it already holds — so no
/// guard and no `ctrl-c` handler of its own. Answers the [`Leave`] that ended
/// it.
///
/// Where the old standalone screen printed its opening message and ended,
/// the tab has a strip and four other tabs to keep drawing, so the message
/// is held on the tab as its [`Mode::Outcome`] instead, closed like any
/// other.
///
/// `on_open` is what the screen has to say the moment it opens — the sync
/// notice, the ignored overrides and the update notice, see
/// [`crate::screen::shell::OnOpen`] — shown as popups over this tab, the one
/// the screen opens on, in that order and ahead of the opening message.
///
/// [`Leave`]: crate::screen::shell::Leave
pub(crate) fn queue_tab(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &std::path::Path,
    on_open: crate::screen::shell::OnOpen,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<crate::screen::shell::Leave> {
    use crate::screen::shell::Leave;

    let groups = super::pending::list_groups(repo)?;
    let routines = super::routines::list_routines(repo)?;
    let mut state = ScreenState::new();
    state.waiting.extend(on_open.sync.map(Mode::SyncGate));
    state.waiting.extend(on_open.ignored.map(Mode::Ignored));
    state
        .waiting
        .extend(on_open.update.map(|line| outcome("update available", line)));
    if let Some(msg) = opening_message(repo, &groups) {
        state.waiting.push_back(outcome("nothing to queue", msg));
    }
    state.mode = state.after_popup(Mode::Browsing);
    let exit = run_screen_from(
        repo,
        pipelines,
        cwd,
        (groups, routines),
        state,
        writer,
        input,
        out,
    )?;
    Ok(match exit {
        ScreenExit::Quit => Leave::Quit,
        ScreenExit::Leave(leave) => leave,
    })
}

/// Bare `spoolway`'s routines tab: the queue screen opened straight onto its
/// [`Mode::Routines`] pane, over the terminal the shell around it already
/// holds. Answers the [`Leave`] that ended it.
///
/// The routine folders are read here, once per visit — and again only
/// after `x` deletes one, see [`after_routine_delete`] — so a routine saved
/// with `s` on the queue tab is listed the moment `→` reaches this one. Each
/// visit also starts on a fresh [`RoutineNav`], with nothing ticked and both
/// cursors at the top, the same as the queue tab starts each visit fresh.
///
/// The pending groups are not read: nothing on this tab draws or queues
/// them, and the screen's own poll fills them in anyway — see
/// `wait_for_key`.
///
/// [`Leave`]: crate::screen::shell::Leave
pub(crate) fn routines_tab(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &std::path::Path,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<crate::screen::shell::Leave> {
    use crate::screen::shell::Leave;

    let routines = super::routines::list_routines(repo)?;
    let mut state = ScreenState::new();
    state.mode = Mode::Routines(RoutineNav::new());
    let exit = run_screen_from(
        repo,
        pipelines,
        cwd,
        (Vec::new(), routines),
        state,
        writer,
        input,
        out,
    )?;
    Ok(match exit {
        ScreenExit::Quit => Leave::Quit,
        ScreenExit::Leave(leave) => leave,
    })
}

// Only the tests below open the screen through a fresh `ScreenState` any
// more — `spoolway queue` no longer does, and `queue_tab` needs the opening
// message held on its own state instead (see `queue_tab`'s `opening_message`
// call), so this wrapper has no production caller left.
#[cfg(test)]
fn run_screen(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &std::path::Path,
    groups: Vec<Group>,
    routines: Vec<RoutineFolder>,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<ScreenExit> {
    run_screen_from(
        repo,
        pipelines,
        cwd,
        (groups, routines),
        ScreenState::new(),
        &mut crate::screen::frame_writer::FrameWriter::new(),
        input,
        out,
    )
}

/// [`run_screen`], opening on `state` rather than a fresh one — the queue
/// tab's opening message is the one caller that needs another. `lists` is the
/// pending groups and the routine folders, paired so this stays inside the
/// argument count the rest of this module keeps to. `writer` pushed the
/// count past that anyway — every argument here is a distinct piece of the
/// screen's own state, so bundling further would only hide that, the same
/// reasoning `status::mod`'s own `too_many_arguments` allow gives.
#[allow(clippy::too_many_arguments)]
fn run_screen_from(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &std::path::Path,
    lists: (Vec<Group>, Vec<RoutineFolder>),
    mut state: ScreenState,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<ScreenExit> {
    let (mut groups, mut routines) = lists;
    let routines_dir = repo.routines_dir();

    // The first frame reads the branch synchronously, on this thread — the
    // same timing the per-key `branch_at` call this replaces always gave
    // its own first frame. `groups` is already this fresh too: the caller
    // read it to decide the tab's opening message before this function was
    // even called. Seeding the reader with both means starting it never
    // pays for a second read of either right away.
    state.board_branch = crate::repo::branch_at(cwd).ok();
    let reader = start_queue_reader(
        repo.clone(),
        cwd.to_path_buf(),
        QueueSnapshot {
            groups: groups.clone(),
            branch: state.board_branch.clone(),
        },
    );
    // The seed above lands as generation 0 and is already in `groups`, so
    // it counts as adopted: the first call below wakes the reader and
    // adopts the first reading that lands after it, never the seed a
    // second time — see `refresh_from_reader`.
    let mut known_reading = 0;

    loop {
        // Ask the reader for one more reading rather than reading again on
        // this thread — see [`QueueReader`]. Every key reaches here once it
        // has been handled, so this is also what wakes the reader for the
        // next one, the same cadence the inline `branch_at` call this
        // replaces always ran at: the board can be left open while its
        // checkout moves to another branch, and the `Lands in:` row must
        // eventually name the branch `enter` would read right now, just
        // from whatever the reader last landed rather than a read of its
        // own.
        refresh_from_reader(&reader, &mut known_reading, &mut groups, &mut state);
        let panes = Panes {
            routines: &routines,
            routines_dir: &routines_dir,
            pipelines,
        };
        draw(&groups, &panes, &state, writer, out);
        let Some(key) = wait_for_key(
            &reader,
            &mut known_reading,
            &mut groups,
            &panes,
            &mut state,
            writer,
            input,
            out,
        ) else {
            // No terminal, a script driving this run has finished handing
            // over keys, or `ctrl-c` was caught: `wait_for_key` returns
            // `None` for all three, and the screen ends the same way for
            // each — nothing typed here can ever be a command a shell prompt
            // would misread, and `TermGuard`'s own drop drains whatever is
            // left of stdin and restores the mode and the cursor.
            break;
        };

        // Inside bare `spoolway`, `←`, `→` and `q` belong to the shell on
        // each tab's own home screen — browsing on the queue tab, the
        // routines pane on the routines tab — the two modes with no popup
        // or sub-mode open. Every other mode keeps them: a popup over
        // either tab, the trial picker, the filter's `q` typed
        // into its query.
        if matches!(state.mode, Mode::Browsing | Mode::Routines(_))
            && let Some(leave) = crate::screen::shell::leave_on(key)
        {
            return Ok(ScreenExit::Leave(leave));
        }

        // No mode reads a quit key of its own any more — `ctrl-c` is the one
        // way out, caught by bare `spoolway`'s own screen and noticed by
        // `wait_for_key` — so every mode's match is just its own keys,
        // with `q` falling to whatever an unrecognised character already
        // does there. Only the modes with a text field give that character
        // any meaning of its own, appending it the same as any other letter:
        // `Mode::Filter`'s query (see `handle_filter_key`'s own doc comment),
        // `Mode::SaveRoutine`'s name, and `Mode::NewJob`'s cron expression
        // and pipeline search.
        match &state.mode {
            Mode::Filter => handle_filter_key(&groups, &mut state, key),
            Mode::Gate(cursor) => {
                let cursor = *cursor;
                handle_gate_key(&groups, pipelines, &mut state, cursor, key);
            }
            Mode::Trial(trial) => match key {
                // The first screen's own `enter`: advance to the second
                // rather than launch anything — but only once a pipeline is
                // ticked. A trial under no pipeline has no arm to write, so
                // an `enter` with nothing ticked falls to the catch-all
                // below, a no-op there, rather than advancing onto an empty
                // skip screen.
                Key::Enter
                    if trial.stage == TrialStage::PickPipelines
                        && !trial_ticked(pipelines, trial).is_empty() =>
                {
                    let mut trial = trial.clone();
                    trial.stage = TrialStage::ChooseSkips;
                    trial.cursor = 0;
                    trial.page = 0;
                    state.mode = Mode::Trial(trial);
                }
                // The second screen's own `enter`: mint and write the batch.
                // Guarded on `ChooseSkips` rather than a bare fallthrough —
                // an `enter` on the first screen with nothing ticked must
                // fall to the catch-all below instead (a no-op there), not
                // reach `begin_trial`, which would have no copy to write.
                Key::Enter if trial.stage == TrialStage::ChooseSkips => {
                    let trial = trial.clone();
                    let base = crate::repo::branch_at(cwd)?;
                    state.mode = begin_trial(repo, pipelines, &base, &groups, &trial);
                }
                _ => {
                    let trial = trial.clone();
                    state.mode = match trial_group(&groups, &trial) {
                        Some(_) => handle_trial_key(pipelines, trial, key),
                        None => Mode::Browsing,
                    };
                }
            },
            // `enter` closes it, and nothing else does: a popup takes every
            // key while it is open, so a key meant for the screen under it
            // cannot slip past a message not yet read. The message was
            // already drawn on the frame that preceded this key — holding it
            // in `state` rather than writing it straight to `out` is what let
            // it survive that draw at all.
            Mode::Outcome { routines, .. } | Mode::Queued { routines, .. } => {
                if key == Key::Enter {
                    let closed = match routines {
                        Some(nav) => Mode::Routines(nav.clone()),
                        None => Mode::Browsing,
                    };
                    state.mode = state.after_popup(closed);
                }
            }
            Mode::Ignored(_) => {
                if key == Key::Enter {
                    state.mode = state.after_popup(Mode::Browsing);
                }
            }
            Mode::SyncGate(_) => {
                if key == Key::Enter {
                    state.mode = state.after_popup(Mode::Browsing);
                }
            }
            Mode::ToolGate { then, .. } => match key {
                Key::Enter => {
                    let then = then.clone();
                    let base = crate::repo::branch_at(cwd)?;
                    let at = Submitting {
                        repo,
                        pipelines,
                        base: &base,
                        routines: &routines,
                    };
                    state.mode = resume(
                        &at,
                        &then,
                        Tracking::Off,
                        &mut groups,
                        &mut state,
                        &mut |_| {},
                    );
                    // `resume` may have just landed a submission straight
                    // onto `groups` — see `refresh_from_reader`'s own doc
                    // comment for why a reading already in flight at this
                    // instant must not be allowed to overwrite that.
                    known_reading = known_reading.max(reader.started_count());
                }
                Key::Esc => state.mode = then.back(),
                _ => {}
            },
            Mode::IssueQuestion { then, .. } => match key {
                // Yes: the hook runs here, on this thread, and the tab under
                // the question is held as it stands while it does — each
                // ticket it answers is laid over that frame as it comes in.
                // Whatever was typed while the hook ran is thrown away once it
                // returns: the result popup takes `enter` only once it is on
                // screen, never an `enter` pressed at the one still filling.
                Key::Enter => {
                    let then = then.clone();
                    let base = crate::repo::branch_at(cwd)?;
                    let at = Submitting {
                        repo,
                        pipelines,
                        base: &base,
                        routines: &routines,
                    };
                    let held = Held::new(&groups, &panes, &state);
                    let mut redraw = |panel: &[String]| held.draw(panel, writer, out);
                    state.mode = resume(
                        &at,
                        &then,
                        Tracking::Open,
                        &mut groups,
                        &mut state,
                        &mut redraw,
                    );
                    known_reading = known_reading.max(reader.started_count());
                    input.discard_typed();
                }
                Key::Char('n') => {
                    let then = then.clone();
                    let base = crate::repo::branch_at(cwd)?;
                    let at = Submitting {
                        repo,
                        pipelines,
                        base: &base,
                        routines: &routines,
                    };
                    state.mode = resume(
                        &at,
                        &then,
                        Tracking::Off,
                        &mut groups,
                        &mut state,
                        &mut |_| {},
                    );
                    known_reading = known_reading.max(reader.started_count());
                }
                Key::Esc => state.mode = then.back(),
                _ => {}
            },
            Mode::SaveRoutine { group, name } => match key {
                // The name field reads every ordinary character it is typed,
                // `q` included, the same way `Mode::Filter`'s query does —
                // see that mode's own doc comment. `esc` is the only way out
                // besides `enter`.
                Key::Esc => state.mode = Mode::Browsing,
                Key::Enter if !name.trim().is_empty() => {
                    let (group, name) = (group.clone(), name.clone());
                    state.mode = save_routine(repo, &groups, &group, &name);
                }
                Key::Backspace | Key::Char('\u{8}') => {
                    let (group, mut name) = (group.clone(), name.clone());
                    name.pop();
                    state.mode = Mode::SaveRoutine { group, name };
                }
                Key::Char(c) if !c.is_control() => {
                    let (group, mut name) = (group.clone(), name.clone());
                    name.push(c);
                    state.mode = Mode::SaveRoutine { group, name };
                }
                _ => {}
            },
            Mode::Routines(nav) => match key {
                // `esc` over the tasks pane goes back to the list, the way
                // it does on the pending screen, keeping the list's cursor
                // on the routine whose tasks were shown.
                Key::Esc if nav.focus == Focus::Tasks => {
                    let mut nav = nav.clone();
                    nav.focus = Focus::Groups;
                    state.mode = Mode::Routines(nav);
                }
                // `esc` over the list falls through to `handle_routine_key`,
                // which reads nothing on it: the pane is the routines tab's
                // whole screen, with nothing behind it to go back to.
                // `enter` over a ticked folder — ignored with nothing ticked,
                // the same way `Mode::Browsing`'s own `enter` does nothing
                // over an empty `state.selected`.
                Key::Enter if nav.focus == Focus::Groups && !nav.selected.is_empty() => {
                    let nav = nav.clone();
                    let base = crate::repo::branch_at(cwd)?;
                    state.mode = begin_routine_queue(
                        repo,
                        pipelines,
                        &base,
                        &routines,
                        &nav,
                        Tracking::Ask,
                        &mut |_| {},
                    );
                }
                // `space` over the tasks pane queues that one task alone
                // — over the folders pane it is `handle_routine_key`'s own
                // business instead, ticking the highlighted folder.
                Key::Char(' ') if nav.focus == Focus::Tasks => {
                    let nav = nav.clone();
                    let base = crate::repo::branch_at(cwd)?;
                    state.mode = begin_routine_solo(
                        repo,
                        pipelines,
                        &base,
                        &routines,
                        &nav,
                        Tracking::Ask,
                        &mut |_| {},
                    );
                }
                // `x` over the list: the popup naming the routine and every
                // job that goes with it. Read only on the list, so the tasks
                // pane has no delete — see `begin_routine_delete`.
                Key::Char('x') if nav.focus == Focus::Groups => {
                    state.mode = begin_routine_delete(repo, &routines, nav);
                }
                // `n` over the list: a job for the highlighted routine,
                // through the jobs tab's schedule and pipeline popups. Read
                // only on the list, so the tasks pane does not make one.
                Key::Char('n') if nav.focus == Focus::Groups => {
                    if let Some(folder) = highlighted_routine_folder(&routines, nav)
                        && let Some(walk) = super::jobs::NewJobWalk::for_routine(
                            pipelines,
                            &folder.path,
                            &routines_dir,
                        )
                    {
                        state.mode = Mode::NewJob {
                            nav: nav.clone(),
                            walk,
                        };
                    }
                }
                // `o` over the tasks pane: open the highlighted task
                // in an editor pane, the same shape `open_highlighted` gives
                // the pending screen — see `open_highlighted_routine`. Gated
                // the same way the pending screen's own `o` is: live only
                // with a task actually under the cursor to open.
                Key::Char('o')
                    if nav.focus == Focus::Tasks
                        && highlighted_routine_task(&routines, nav).is_some() =>
                {
                    state.mode = open_highlighted_routine(repo, &routines, nav);
                }
                _ => {
                    let mut nav = nav.clone();
                    handle_routine_key(&routines, &mut nav, key);
                    state.mode = Mode::Routines(nav);
                }
            },
            Mode::NewJob { nav, walk } => {
                let nav = nav.clone();
                state.mode = match walk.key(repo, pipelines, key) {
                    WalkStep::Walking(walk) => Mode::NewJob { nav, walk },
                    WalkStep::Cancelled => Mode::Routines(nav),
                    WalkStep::Saved(draft) => Mode::JobSaved {
                        nav,
                        notice: super::jobs::saved_notice(&draft),
                    },
                    // The jobs tab's own title and message for a refused
                    // save, a name already taken among them.
                    WalkStep::Refused(message) => outcome_over(Some(&nav), "not saved", message),
                };
            }
            Mode::JobSaved { nav, .. } => {
                if key == Key::Enter {
                    let closed = Mode::Routines(nav.clone());
                    state.mode = state.after_popup(closed);
                }
            }
            Mode::DeleteRoutine { nav, target } => match key {
                Key::Enter => {
                    let (nav, target) = (nav.clone(), target.clone());
                    let result = delete_routine(repo, &target);
                    state.mode = after_routine_delete(repo, &mut routines, nav, result);
                }
                Key::Esc => state.mode = Mode::Routines(nav.clone()),
                // Every other key leaves the popup open, so a stray
                // keystroke can neither delete nor dismiss it.
                _ => {}
            },
            Mode::Browsing => match key {
                // `enter` validates the selection and writes it straight
                // through — nothing is drawn in between but the
                // tool-requirements gate and the issue question, when each
                // has something to ask. A refusal hands back
                // `Mode::Outcome`; a clean write says what it queued in
                // `Mode::Queued`. Starting a dispatcher is the dispatch
                // tab's `enter`, not this one's.
                Key::Enter if !state.selected.is_empty() => {
                    let base = crate::repo::branch_at(cwd)?;
                    state.mode = begin_submission(
                        repo,
                        pipelines,
                        &base,
                        &mut groups,
                        &mut state,
                        Tracking::Ask,
                        &mut |_| {},
                    );
                    known_reading = known_reading.max(reader.started_count());
                }
                // Gated exactly the way `g` is — see `handle_browse_key`'s
                // own `g` arm — since a task only exists to open when
                // the tasks pane is focused on one.
                Key::Char('o')
                    if state.focus == Focus::Tasks
                        && highlighted_task_key(&groups, &state).is_some() =>
                {
                    state.mode = open_highlighted(repo, &groups, &state);
                }
                // `t`: open the trial picker on whichever group the cursor
                // sits on — see `trial_target`. Needs `pipelines`, which
                // `handle_browse_key` is not handed, so this is the one key
                // `run_screen` reads before falling through to it, the same
                // way `o` already does.
                Key::Char('t') => {
                    if let Some(group) = trial_target(&groups, &state) {
                        state.mode = Mode::Trial(TrialState::new(pipelines, group));
                    }
                }
                _ => handle_browse_key(&groups, &mut state, key),
            },
        }
    }
    Ok(ScreenExit::Quit)
}

/// What a submit resumed off the tool gate or the issue question runs
/// against, bundled to keep [`resume`] inside the argument count this module
/// keeps to.
struct Submitting<'a> {
    repo: &'a Repo,
    pipelines: &'a Pipelines,
    base: &'a str,
    routines: &'a [RoutineFolder],
}

/// Run the submit `then` names again from the start, answered as `tracking`
/// says — validating it again is cheap, and it is what a person would see had
/// they pressed the key that opened the popup a second time.
fn resume(
    at: &Submitting,
    then: &Resume,
    tracking: Tracking,
    groups: &mut Vec<Group>,
    state: &mut ScreenState,
    redraw: &mut dyn FnMut(&[String]),
) -> Mode {
    let Submitting {
        repo,
        pipelines,
        base,
        routines,
    } = *at;
    match then {
        Resume::Selection => {
            begin_submission(repo, pipelines, base, groups, state, tracking, redraw)
        }
        Resume::Routines(nav) => {
            begin_routine_queue(repo, pipelines, base, routines, nav, tracking, redraw)
        }
        Resume::RoutineTask(nav) => {
            begin_routine_solo(repo, pipelines, base, routines, nav, tracking, redraw)
        }
    }
}

/// The tab as it stood when the issue question was answered, with no popup
/// on it — what the `opening issues` popup is laid over while the hook runs.
/// Rendered once and owned, so drawing it again borrows nothing the submit
/// itself is busy changing.
struct Held {
    frame: Vec<String>,
    footer: String,
}

impl Held {
    fn new(groups: &[Group], panes: &Panes, state: &ScreenState) -> Held {
        let footer = footer(state);
        Held {
            frame: beneath(groups, panes, state, &footer),
            footer,
        }
    }

    /// The held tab with `panel` over it, written the way [`draw`] writes
    /// any frame.
    fn draw(
        &self,
        panel: &[String],
        writer: &mut crate::screen::frame_writer::FrameWriter,
        out: &mut impl std::io::Write,
    ) {
        let frame = compose(self.frame.clone(), Some(panel), self.footer.clone());
        paint(frame, writer, out);
    }
}

fn clamp_cursors(groups: &[Group], state: &mut ScreenState) {
    let count = shown(groups, state).len();
    if count == 0 {
        state.group_cursor = 0;
    } else {
        state.group_cursor = state.group_cursor.min(count - 1);
    }
    let tasks = highlighted_tasks(groups, state)
        .map(<[_]>::len)
        .unwrap_or(0);
    if tasks == 0 {
        state.task_cursor = 0;
    } else {
        state.task_cursor = state.task_cursor.min(tasks - 1);
    }
}

/// Wait for the next keystroke, redrawing the screen on every poll slice —
/// see [`draw`] — so a resized terminal or a pending task someone just
/// edited reaches the screen without a key being typed at all. `None` once
/// the input is exhausted, exactly what a direct [`read_key`] would report —
/// and also once `ctrl-c` has been pressed: `stop::asked()` is checked on
/// every slice the same way the reader is woken and the redraw already are,
/// so a caught interrupt ends the screen exactly the way a drained pipe
/// already did, rather than needing a signal-unsafe read to short-circuit
/// the loop.
///
/// The same wait `commands::dispatch`'s own board takes over its pass
/// interval: [`PollableRead::byte_pending`] stands in for a sleep, so a slice
/// with nothing typed into it costs the loop nothing beyond what a plain
/// `thread::sleep(POLL)` would have. Every in-memory reader used in tests
/// reports a byte pending unconditionally (see [`PollableRead`]), so this
/// falls straight through to `read_key` there — the idle tick's own wake
/// below only ever runs against a real, currently idle terminal. A cooked
/// stdin — no raw mode, no `poll` — reports the opposite without waiting,
/// and is read straight away instead: blocking on the line the terminal
/// will deliver is the whole of what this loop is for, where spinning on
/// "nothing pending" would never read a key at all.
///
/// `known` pushes this past clippy's default argument count — every
/// argument here is a distinct piece of the screen's own state, the same
/// reasoning `status::mod`'s own `too_many_arguments` allow gives.
#[allow(clippy::too_many_arguments)]
fn wait_for_key(
    reader: &QueueReader,
    known: &mut u64,
    groups: &mut Vec<Group>,
    panes: &Panes,
    state: &mut ScreenState,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Option<Key> {
    loop {
        if crate::platform::stop::asked() {
            return None;
        }
        if !cfg!(unix) || input.byte_pending(crate::status::POLL) {
            return read_key(input);
        }
        refresh_from_reader(reader, known, groups, state);
        draw(groups, panes, state, writer, out);
    }
}

/// Swap `fresh` into `groups`, keeping `group_cursor` on whatever group it
/// was pointing at, by name, rather than by index — a reload can reorder the
/// list out from under it, since `list_groups` sorts newest first and an
/// edit touches a task's own modified time — and clamping it back on screen
/// the ordinary way when that group is gone.
///
/// Shared by [`reload`], which fetches `fresh` synchronously on the calling
/// thread, and [`refresh_from_reader`], which takes it from whatever
/// [`QueueReader`] last read on a thread of its own — the cursor-preserving
/// swap itself does not care which.
fn adopt_groups(fresh: Vec<Group>, groups: &mut Vec<Group>, state: &mut ScreenState) {
    let cursor = shown(groups, state)
        .get(state.group_cursor)
        .map(|group| group_key(group));
    *groups = fresh;
    if let Some(key) = cursor
        && let Some(index) = shown(groups, state)
            .iter()
            .position(|group| group_key(group) == key)
    {
        state.group_cursor = index;
    }
    clamp_cursors(groups, state);
}

/// Re-read the pending directory into `groups`, on this thread — see
/// [`adopt_groups`] for the cursor-preserving swap.
///
/// A pending directory that fails to read this tick is not a reason to blank
/// the screen: `groups` is left exactly as it was, and the next poll tries
/// again — the same tolerance `commands::dispatch`'s own pass loop gives a
/// queue read that comes back unreadable mid-run.
///
/// No production caller any more — [`run_screen_from`]'s loop reads
/// through [`QueueReader`] and `refresh_from_reader` instead, so a key
/// never waits on this itself — but kept, test-only, as the plain
/// synchronous read it always was: its own test below is what checks the
/// cursor-preserving swap in [`adopt_groups`] directly, against a real
/// `list_groups` read, without a reader thread's timing in the way.
#[cfg(test)]
fn reload(repo: &Repo, groups: &mut Vec<Group>, state: &mut ScreenState) {
    let Ok(fresh) = super::pending::list_groups(repo) else {
        return;
    };
    adopt_groups(fresh, groups, state);
}

/// What [`QueueReader`] hands back: the groups and the board's own branch,
/// read together off one thread so neither can land half a frame behind the
/// other. The generation a reading landed on lives on
/// [`crate::status::Reader`] itself, outside this, since that is what ties
/// one back to [`QueueReader::started_count`] — see `refresh_from_reader`.
#[derive(Default)]
struct QueueSnapshot {
    groups: Vec<Group>,
    branch: Option<String>,
}

/// The read [`run_screen_from`]'s loop used to run on the key thread itself
/// — `crate::repo::branch_at` for `state.board_branch`, every key, and
/// `super::pending::list_groups` once a second while idle — kept off that
/// thread by a thread of its own: [`crate::status`]'s own generic
/// `Reader<T>`, the same `board-reader-thread` decision the dispatch tab's
/// board already reuses for its own `Snapshot`, rather than a second copy
/// of that thread written by hand for this one. Before this, every key here
/// started a `git` process before its frame was drawn, and a key typed
/// during a reload waited for every task file to be read.
type QueueReader = crate::status::Reader<QueueSnapshot>;

/// [`QueueReader::start`], seeded with `initial` — already read by the
/// caller, on its own thread, before the tab decided its opening message —
/// so this never pays for a second read of the same files right away.
/// Unlike the dispatch tab's own reader, nothing here ever blocks waiting
/// for a reading to land: the queue tab's very first frame is already read
/// before this is even started, so there is no "first frame" case that
/// needs one.
fn start_queue_reader(repo: Repo, cwd: std::path::PathBuf, initial: QueueSnapshot) -> QueueReader {
    // `Reader::start` itself calls `build` once, synchronously, for its own
    // seed — but `initial` is already that seed, read by the caller before
    // this ever runs, so the closure hands it straight back the one time it
    // is asked for a value it does not have to build. The `Option` holding
    // it guards that: `take` empties it on that first call, so every call
    // after it really does read.
    let mut initial = Some(initial);
    QueueReader::start(move || {
        if let Some(seed) = initial.take() {
            return Some(seed);
        }
        build_queue_snapshot(&repo, &cwd)
    })
}

/// [`super::pending::list_groups`] and [`crate::repo::branch_at`], together
/// — what [`QueueReader`] reads on its own thread once its seed has been
/// handed back once. `None` when the pending directory itself cannot be
/// read; a detached checkout reads as no branch rather than a reason to
/// fail the whole reading, the same tolerance the inline `branch_at` call
/// this replaces always gave.
fn build_queue_snapshot(repo: &Repo, cwd: &std::path::Path) -> Option<QueueSnapshot> {
    let groups = super::pending::list_groups(repo).ok()?;
    let branch = crate::repo::branch_at(cwd).ok();
    Some(QueueSnapshot { groups, branch })
}

/// Ask [`QueueReader`] for one more reading, and adopt it into `groups` and
/// `state.board_branch` only if it landed on a generation above `known` —
/// the ratchet [`run_screen_from`]'s loop keeps across every call, bumped
/// here to whatever is adopted and bumped separately, by
/// [`QueueReader::started_count`], the moment a key edits `groups` directly
/// (`finish_submit`'s own `groups.retain`, once a submission lands) — see
/// that call site's own comment. Without that second bump a reading already
/// in flight at the moment of the edit could still land afterward and carry
/// the submitted group right back, since nothing about the ratchet alone
/// tells that reading apart from a fresh one: it only ever moves forward
/// when a call here actually adopts something, never on every call, so a
/// reading this very call just woke is never rejected for having started
/// "too recently" — the bug an earlier version of this function had.
fn refresh_from_reader(
    reader: &QueueReader,
    known: &mut u64,
    groups: &mut Vec<Group>,
    state: &mut ScreenState,
) {
    reader.wake();
    let (snapshot, generation) = reader.latest_with_generation();
    if generation <= *known {
        return;
    }
    adopt_groups(snapshot.groups.clone(), groups, state);
    state.board_branch = snapshot.branch.clone();
    *known = generation;
}

/// The highlighted group's tasks, or `None` when nothing is under the cursor
/// at all — an empty pending directory, or a filter that matched nothing.
fn highlighted_tasks<'a>(groups: &'a [Group], state: &ScreenState) -> Option<&'a [PendingTask]> {
    shown(groups, state)
        .get(state.group_cursor)
        .map(|group| group.tasks.as_slice())
}

/// The group a `t` press forks: whichever one sits under the cursor right
/// now, in either pane, filtered or not — `shown` already does the narrowing
/// for one case and ignores it for the other. `None` with nothing under the
/// cursor at all, or a group with no tasks to assign a pipeline to, the same
/// case `s`'s own gate already checks for.
fn trial_target<'a>(groups: &'a [Group], state: &ScreenState) -> Option<&'a Group> {
    let group = shown(groups, state).get(state.group_cursor).copied()?;
    (!group.tasks.is_empty()).then_some(group)
}

/// One key while browsing: moving, focus, selection and the gate picker.
///
/// None of the keys that need the repo are handled here — `enter` needs the
/// repo and the pipelines to submit, `o` needs the repo to open an editor,
/// and `t` needs the pipelines to seed the trial picker's first screen — so
/// nothing this reads can leave the screen or reach outside it. `q` reaches
/// this function like any other unrecognised character and does nothing.
///
/// `tab` is the one key that moves focus between the two panes. `←` and `→`
/// used to as well, but inside bare `spoolway` they move between tabs, and
/// one screen reading them two ways depending on where it was opened would
/// be one more thing to remember — so they do nothing here. `esc` is the
/// other way back from the tasks pane to the groups pane, the same "back" it
/// means on the routines pane's key line; over the groups pane there is
/// nowhere further back to go, so it does nothing there.
fn handle_browse_key(groups: &[Group], state: &mut ScreenState, key: Key) {
    match key {
        Key::Char('h') => {
            state.hide_scope = state.hide_scope.next();
            clamp_cursors(groups, state);
        }
        Key::Char('f') => state.mode = Mode::Filter,
        Key::Tab => {
            state.focus = match state.focus {
                Focus::Groups => Focus::Tasks,
                Focus::Tasks => Focus::Groups,
            };
        }
        // Only the focus moves: `group_cursor` stays on the group whose
        // tasks were just being read, as it does when `tab` goes back.
        Key::Esc => state.focus = Focus::Groups,
        Key::Up | Key::Char('k') => match state.focus {
            Focus::Groups => {
                state.group_cursor = state.group_cursor.saturating_sub(1);
                state.task_cursor = 0;
            }
            Focus::Tasks => state.task_cursor = state.task_cursor.saturating_sub(1),
        },
        Key::Down | Key::Char('j') => match state.focus {
            Focus::Groups => {
                let last = shown(groups, state).len().saturating_sub(1);
                state.group_cursor = (state.group_cursor + 1).min(last);
                state.task_cursor = 0;
            }
            Focus::Tasks => {
                let last = highlighted_tasks(groups, state).unwrap_or(&[]).len();
                let last = last.saturating_sub(1);
                state.task_cursor = (state.task_cursor + 1).min(last);
            }
        },
        // Selection is on the group, whichever pane the focus is in — a
        // group is selected whole, and the tasks pane is what it holds rather
        // than a second list to pick from.
        Key::Char(' ') => {
            if let Some(group) = shown(groups, state).get(state.group_cursor)
                && selectable(group)
            {
                let key = group_key(group);
                if !state.selected.remove(&key) {
                    state.selected.insert(key);
                }
            }
        }
        Key::Char('g')
            if state.focus == Focus::Tasks && highlighted_task_key(groups, state).is_some() =>
        {
            state.mode = Mode::Gate(0);
        }
        // `s`: save the highlighted group as a routine. Gated on the group
        // itself, not on which pane has focus — the same way `enter` reads
        // `state.selected` regardless of `state.focus`, and the same target
        // `t` forks — since a group with nothing in it has nothing for a
        // routine to hold. `t` itself is `run_screen`'s own business: it
        // needs the pipelines to seed the trial picker, which this function
        // is never handed.
        Key::Char('s') => {
            if let Some(group) = trial_target(groups, state) {
                state.mode = Mode::SaveRoutine {
                    group: group_key(group),
                    name: group.name.clone(),
                };
            }
        }
        // Every other key — including a stray newline a scripted input file
        // happens to carry, and an `enter` with nothing selected — has no
        // meaning here and is simply ignored.
        _ => {}
    }
}

/// The routine the left pane's cursor sits on.
pub(super) fn highlighted_routine_folder<'a>(
    routines: &'a [RoutineFolder],
    nav: &RoutineNav,
) -> Option<&'a RoutineFolder> {
    routines.get(nav.folder_cursor)
}

/// The task the right pane's cursor sits on — `None` with no routine
/// highlighted, or a folder with fewer tasks than
/// `nav.task_cursor` names.
pub(super) fn highlighted_routine_task<'a>(
    routines: &'a [RoutineFolder],
    nav: &RoutineNav,
) -> Option<&'a RoutineTask> {
    highlighted_routine_folder(routines, nav)?
        .tasks
        .get(nav.task_cursor)
}

/// One key over the routines pane — everything but `esc` over the tasks
/// pane, `enter` on a selected folder, `space` over the tasks pane, `x`
/// over the list and `o` over a highlighted task, which all need the repo to act on or change
/// this mode outright, so `run_screen` reads those first and only falls through to this for the
/// rest, the same split it makes for `handle_browse_key`. `q` is part of
/// that rest, and does nothing here either.
///
/// `tab` is the only key that moves between the two panes. `←` and `→`
/// read nothing here: they belong to the tab strip, which the routines tab
/// and the jobs picker both sit under. `esc` is not read here either, since
/// it means "back to the list" over the routines tab's tasks pane and
/// "cancel" in the jobs picker, so each caller reads it itself.
pub(super) fn handle_routine_key(routines: &[RoutineFolder], nav: &mut RoutineNav, key: Key) {
    match key {
        Key::Up | Key::Char('k') => match nav.focus {
            Focus::Groups => {
                nav.folder_cursor = nav.folder_cursor.saturating_sub(1);
                nav.task_cursor = 0;
            }
            Focus::Tasks => nav.task_cursor = nav.task_cursor.saturating_sub(1),
        },
        Key::Down | Key::Char('j') => match nav.focus {
            Focus::Groups => {
                let last = routines.len().saturating_sub(1);
                nav.folder_cursor = (nav.folder_cursor + 1).min(last);
                nav.task_cursor = 0;
            }
            Focus::Tasks => {
                let last = highlighted_routine_folder(routines, nav)
                    .map_or(0, |folder| folder.tasks.len())
                    .saturating_sub(1);
                nav.task_cursor = (nav.task_cursor + 1).min(last);
            }
        },
        // Only the focus moves, as `tab` on the pending screen does: the
        // list's cursor stays on the routine whose tasks are shown, and
        // coming back to the tasks pane finds its cursor where it was left.
        Key::Tab => {
            nav.focus = match nav.focus {
                Focus::Groups => Focus::Tasks,
                Focus::Tasks => Focus::Groups,
            };
        }
        // Selecting a task alone is `space` over the tasks pane instead —
        // handled by `run_screen`, which is what needs the repo to queue it.
        Key::Char(' ') if nav.focus == Focus::Groups => {
            if let Some(folder) = highlighted_routine_folder(routines, nav) {
                let path = folder.path.clone();
                if !nav.selected.remove(&path) {
                    nav.selected.insert(path);
                }
            }
        }
        _ => {}
    }
}

/// One key while the filter box has focus. Typing narrows the list —
/// [`shown`] re-scores it against `state.filter` on every keystroke, so
/// nothing here has to re-run the search itself — `enter` and `esc` are the
/// only two ways out, and up/down still move the highlighted group the same
/// as browsing does, over whatever `shown` narrowed the list to. Every other
/// key, `q` included, is not read specially at all: it falls to the
/// `Key::Char(c)` arm and is appended like any other letter — no mode reads
/// `q` as anything but ordinary text any more, but this is the one place
/// that text actually shows up rather than being dropped on the floor.
fn handle_filter_key(groups: &[Group], state: &mut ScreenState, key: Key) {
    match key {
        Key::Enter => state.mode = Mode::Browsing,
        Key::Esc => {
            state.filter.clear();
            state.mode = Mode::Browsing;
            clamp_cursors(groups, state);
        }
        // `0x7f` (DEL) reaches here as `Key::Backspace` — see `read_key`'s
        // own doc comment — but `0x08` (BS, what ctrl-h and many terminals
        // send instead) still comes through as a plain control character,
        // which the `Key::Char(c)` arm below excludes; this is the one
        // control byte a filter query does act on, the same pairing
        // `eval.rs`'s own filter box reads.
        Key::Backspace | Key::Char('\u{8}') => {
            state.filter.pop();
            clamp_cursors(groups, state);
        }
        Key::Up => state.group_cursor = state.group_cursor.saturating_sub(1),
        Key::Down => {
            let last = shown(groups, state).len().saturating_sub(1);
            state.group_cursor = (state.group_cursor + 1).min(last);
        }
        // Every other control byte a typed field never wants to keep as
        // text — a stray ctrl-key struck while typing, say — is dropped
        // rather than appended; only an ordinary character narrows the
        // query.
        Key::Char(c) if !c.is_control() => {
            state.filter.push(c);
            clamp_cursors(groups, state);
        }
        _ => {}
    }
}

fn highlighted_task_key(groups: &[Group], state: &ScreenState) -> Option<TaskKey> {
    let group = shown(groups, state).get(state.group_cursor).copied()?;
    Some(task_key(group.tasks.get(state.task_cursor)?))
}

/// `o`: open the highlighted task in an editor, in a pane the
/// multiplexer opens — gated the same as `g`: live only with the tasks pane
/// focused and a task actually highlighted under it, which is checked by the
/// caller before this is reached. Never blocks: the pane runs the editor on
/// its own, and the screen keeps redrawing and reading keys while it is
/// open, the same non-blocking shape `Board::open_cursor` uses — this calls
/// through `crate::status::editor_command` rather than resolving
/// `$VISUAL`/`$EDITOR` a second time.
///
/// A queued group's task reads from the queue directory rather than
/// pending — see [`PendingTask::path`] — so this opens whichever copy is
/// actually live, the same file every other read of the screen uses.
///
/// A backend with no pane to open one in — headless, which refuses the way
/// `Mux::open_command`'s default does — is surfaced through [`Mode::Outcome`] rather
/// than lost: an `Err` this discarded would leave a person pressing `o` on a
/// headless run with no sign the key did anything at all.
fn open_highlighted(repo: &Repo, groups: &[Group], state: &ScreenState) -> Mode {
    let Some(group) = shown(groups, state).get(state.group_cursor).copied() else {
        return Mode::Browsing;
    };
    let Some(task) = group.tasks.get(state.task_cursor) else {
        return Mode::Browsing;
    };
    let command = crate::status::editor_command(&task.path);
    let mux = match crate::mux::backend(repo) {
        Ok(mux) => mux,
        Err(err) => return outcome("open task", format!("o: {err:#}")),
    };
    match mux.open_command(&repo.root, &format!("{} · edit", task.id), &command) {
        Ok(()) => Mode::Browsing,
        Err(err) => outcome("open task", format!("o: {err:#}")),
    }
}

/// `o` over the routines pane's own tasks pane: open the highlighted
/// task in an editor pane — the same shape [`open_highlighted`] gives
/// the pending screen, including the same [`Mode::Outcome`] a backend with
/// no pane to open one in is surfaced through — drawn over this pane, and
/// closed back onto it. Returns to [`Mode::Routines`]
/// with `nav` exactly as it was rather than [`Mode::Browsing`], since this
/// key never leaves the pane the way the pending screen's `o` has nothing to
/// stay in.
fn open_highlighted_routine(repo: &Repo, routines: &[RoutineFolder], nav: &RoutineNav) -> Mode {
    let Some(task) = highlighted_routine_task(routines, nav) else {
        return Mode::Routines(nav.clone());
    };
    let command = crate::status::editor_command(&task.path);
    let mux = match crate::mux::backend(repo) {
        Ok(mux) => mux,
        Err(err) => return outcome_over(Some(nav), "open task", format!("o: {err:#}")),
    };
    match mux.open_command(&repo.root, &format!("{} · edit", task.id), &command) {
        Ok(()) => Mode::Routines(nav.clone()),
        Err(err) => outcome_over(Some(nav), "open task", format!("o: {err:#}")),
    }
}

/// The pipeline a highlighted task itself names. `parse_submission`
/// refuses a task naming none, so a task still missing one here —
/// still being edited, not yet queueable — resolves nothing rather than
/// guessing at a pipeline no real submission would end up on.
fn task_pipeline<'a>(doc: &str, pipelines: &'a Pipelines) -> Result<&'a Pipeline> {
    let name = doc_pipeline_name(doc).context("task names no `pipeline:`")?;
    pipelines.get(&name)
}

/// A peek at a task's own `pipeline:` key, without the rest of
/// `parse_submission`'s validation — the gate picker needs a pipeline's
/// steps before a task has a `base:` to be validated against at all.
///
/// [`tasks_pane_lines`] does not call this any more: a `PendingTask`'s own
/// `pipeline` field already carries what this would read, parsed once
/// while `pending::list_groups_in` built it — see that field's own
/// comment. Three callers still parse here, on every draw that reads it.
/// The routines pane (`routine_task_lines`) has no choice: a `RoutineTask`
/// carries none of `PendingTask`'s precomputed fields. The gate picker
/// ([`task_pipeline`], used by `handle_gate_key` and `gate_panel`) and the
/// trial picker's tick list (`TrialState::new`) do hold a `PendingTask`
/// whose `pipeline` field already has this value; they were left on this
/// path because neither runs on the idle per-second redraw the queue tab
/// was slow on, only while one of those pickers is open.
fn doc_pipeline_name(doc: &str) -> Option<String> {
    let (yaml, _) = crate::task::split_fence(doc).ok()?;
    let value: serde_norway::Value = serde_norway::from_str(yaml).ok()?;
    value
        .as_mapping()?
        .get("pipeline")?
        .as_str()
        .map(str::to_string)
}

fn handle_gate_key(
    groups: &[Group],
    pipelines: &Pipelines,
    state: &mut ScreenState,
    cursor: usize,
    key: Key,
) {
    let Some(key_of_task) = highlighted_task_key(groups, state) else {
        state.mode = Mode::Browsing;
        return;
    };
    let Some(group) = shown(groups, state).get(state.group_cursor).copied() else {
        state.mode = Mode::Browsing;
        return;
    };
    let Some(task) = group.tasks.get(state.task_cursor) else {
        state.mode = Mode::Browsing;
        return;
    };
    // The same pipeline `parse_submission` would resolve this task
    // against once it is actually submitted — a malformed `pipeline:` on it
    // degrades to "no steps to page through" here rather than a panic, and is
    // caught properly, with a real error, at submit time.
    let pipeline = task
        .text()
        .ok()
        .and_then(|doc| task_pipeline(&doc, pipelines).ok());
    let steps = pipeline.map_or(0, |p| p.steps.len());

    match key {
        Key::Up | Key::Char('k') => state.mode = Mode::Gate(cursor.saturating_sub(1)),
        Key::Down | Key::Char('j') => {
            state.mode = Mode::Gate((cursor + 1).min(steps.saturating_sub(1)))
        }
        Key::Enter | Key::Char(' ') => {
            // Pressing it again on the same step clears it, per the
            // acceptance criterion — a gate is a toggle, not a one-way choice.
            if let Some(step) = pipeline.and_then(|p| p.steps.get(cursor)) {
                if state.gates.get(&key_of_task) == Some(&step.id) {
                    state.gates.remove(&key_of_task);
                } else {
                    state.gates.insert(key_of_task, step.id.clone());
                }
            }
            state.mode = Mode::Browsing;
        }
        Key::Char('g') | Key::Esc => state.mode = Mode::Browsing,
        _ => {}
    }
}

/// Insert or replace a `<key>: <value>` line right after a task's
/// opening `---` fence, dropping any line already there for that same key —
/// and, with it, the indented or `- ` lines that continued it, so a
/// block-list `depends_on:` is replaced whole rather than leaving its items
/// orphaned under the new key (jobs review finding 4). Only the frontmatter
/// is looked at: a `<key>:` in the body, a code sample say, is copied
/// through untouched.
///
/// Shared by [`with_gate`], which never writes this back to disk, and
/// [`write_back_ids`], which does — this only ever touches the text in
/// memory; what happens to the result is each caller's own business.
fn with_frontmatter_field(doc: &str, key: &str, value: &str) -> String {
    let Some(nl) = doc.find('\n') else {
        return doc.to_string();
    };
    if doc[..nl].trim() != "---" {
        return doc.to_string();
    }

    let prefix = format!("{key}:");
    let mut out = String::with_capacity(doc.len() + key.len() + value.len() + 4);
    out.push_str(&doc[..=nl]);
    out.push_str(&prefix);
    out.push(' ');
    out.push_str(value);
    out.push('\n');
    let mut in_frontmatter = true;
    let mut dropping = false;
    for line in doc[nl + 1..].lines() {
        if in_frontmatter {
            if line.trim() == "---" {
                in_frontmatter = false;
            } else if line.starts_with(&prefix) {
                dropping = true;
                continue;
            } else if dropping
                && (line.starts_with(' ') || line.starts_with('\t') || line.starts_with("- "))
            {
                continue;
            } else {
                dropping = false;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Insert a `gate_at: <step>` line into a task, in memory only —
/// the file in the pending directory is never rewritten to record one. The
/// only write the screen makes there is the deletion that follows a
/// submission, and it says nothing about any single task's gate. Any
/// `gate_at:` the task already carried is dropped, so the screen's own
/// choice — the last thing a person actually picked before submitting — is
/// always the one that wins.
fn with_gate(doc: &str, step: &str) -> String {
    with_frontmatter_field(doc, "gate_at", step)
}

/// The groups a person has selected — what `selected_tasks` below turns
/// into the pending batch, and whose tasks `finish_submit` deletes once
/// that batch is actually written.
fn selected_groups<'a>(
    groups: &'a [Group],
    selected: &std::collections::BTreeSet<GroupKey>,
) -> Vec<&'a Group> {
    groups
        .iter()
        .filter(|group| selected.contains(&group_key(group)))
        .collect()
}

/// Every task a selection puts in the queue, in group order, each
/// carrying whatever gate the screen recorded against it.
///
/// Only a task still [`TaskState::Pending`] is a task this submission
/// can write at all — a sibling already read out of `queue/` or `archive/`
/// carries `stage:` and the rest of [`RESERVED_KEYS`], which
/// `parse_submission` refuses on sight. A group is offered here only while
/// it still has a pending task (see [`super::pending::group_state`]), so
/// that task is never the one filtered away.
fn selected_tasks(groups: &[Group], state: &ScreenState) -> Vec<(TaskKey, String)> {
    let mut tasks = Vec::new();
    for group in selected_groups(groups, &state.selected) {
        for task in &group.tasks {
            if task.state != TaskState::Pending {
                continue;
            }
            let key = task_key(task);
            let doc = match state.gates.get(&key) {
                Some(step) => with_gate(&task.doc, step),
                None => task.doc.clone(),
            };
            tasks.push((key, doc));
        }
    }
    tasks
}

/// How wide each pane's content is and how many rows the pair of them get,
/// for one frame. The widths do not count the border or the one space of
/// padding on either side of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Layout {
    pub(super) left: usize,
    pub(super) right: usize,
    /// How many rows the panes are cut to, or `None` where there is no
    /// terminal to measure. Output that is not a terminal has no bottom to
    /// fall off, so it keeps every row it was going to write.
    pub(super) rows: Option<usize>,
}

/// The widths a run with no terminal to measure falls back to. `left` is
/// defined below `MIN_LEFT_PANE` itself rather than repeating its old value
/// (28) — a fallback narrower than the floor every measured layout already
/// respects would be the one width a queued tail could still get truncated
/// at, and headless is exactly where the e2e suite drives this screen.
const LEFT_PANE_WIDTH: usize = MIN_LEFT_PANE;
const RIGHT_PANE_WIDTH: usize = 46;

/// The characters every left-pane row spends before its group name even
/// starts: the cursor marker, a space, the three-character checkbox, and the
/// space after it — see the `format!` at the end of [`groups_pane_lines`].
/// Named so the width constants below stay in lockstep with that layout
/// instead of each hard-coding the same number separately.
const ROW_PREFIX_COLUMNS: usize = 6;

/// The widest tail [`group_tail`] ever builds: `" queued"`, at seven
/// characters — two more than `" done"`, so this stays the floor either
/// tail needs regardless of which one a given row draws.
///
/// Used to carry a timestamp too — the queue file's own modification time,
/// which the dispatcher rewrites at every step boundary. That made the tail
/// read as "last touched" under a label that says "queued", so it dropped
/// back to the bare word.
const QUEUED_TAIL_COLUMNS: usize = 7;

/// The floor [`groups_pane_lines`] and [`task_row`] never let a name's own
/// column shrink past, however long the tail next to it runs — sized to the
/// mockup's own longest example name (`groups-not-plans`), so an ordinary
/// name reads whole even when the tail crowds it.
const MIN_NAME_COLUMN: usize = 12;

/// The narrowest either pane is ever cut to, so a very narrow terminal
/// still draws two panes rather than a column of borders.
///
/// The left pane's floor is wide enough that a queued row never has to
/// choose between a legible name and a whole tail: `ROW_PREFIX_COLUMNS` +
/// `MIN_NAME_COLUMN` + `QUEUED_TAIL_COLUMNS` is exactly the width a row
/// with a floored name and a full tail needs, so at this floor or wider
/// `groups_pane_lines` never produces a row `two_pane_frame`'s own `pad_to`
/// has to cut into — narrower, and the tail would be silently truncated,
/// which cost this feature its own acceptance criterion once already.
const MIN_LEFT_PANE: usize = ROW_PREFIX_COLUMNS + MIN_NAME_COLUMN + QUEUED_TAIL_COLUMNS;
const MIN_RIGHT_PANE: usize = 24;

/// The widest the left pane grows. Past this it is spending on whitespace
/// after the group names the room the task detail on the right needs.
///
/// Gives a name a 33-character budget: `ROW_PREFIX_COLUMNS` + 33 +
/// `QUEUED_TAIL_COLUMNS`.
const MAX_LEFT_PANE: usize = ROW_PREFIX_COLUMNS + 33 + QUEUED_TAIL_COLUMNS;

/// What a frame spends on something other than pane content: six columns of
/// border and padding on every row, and four lines — the two borders, the
/// footer under them, and one spare, so the last line's own newline does not
/// scroll the top of the frame away.
const PANE_CHROME_COLUMNS: usize = 6;
const PANE_CHROME_ROWS: usize = 4;

/// The last column is left empty for the same reason as the last row: a row
/// that reaches the right edge is followed by a newline the terminal has
/// already wrapped for, and the frame comes out double-spaced.
const SPARE_COLUMN: usize = 1;

/// The layout for this frame, measured fresh every draw so a resized
/// terminal reflows on the next one — less the rows bare `spoolway`'s tab
/// strip takes off the top when it hosts this screen or the jobs screen,
/// which lays itself out through this too (zero everywhere else; see
/// `crate::screen::shell::strip_rows`), and less every row past the first
/// that `footer` wraps onto. The queue's own key line is wider than a
/// 100-column terminal, and each row it wraps onto pushes the frame's top
/// row — the strip, or the box's own border — off the top of the screen.
pub(super) fn layout(footer: &str) -> Layout {
    match terminal_size::terminal_size() {
        Some((width, height)) => layout_for(
            width.0 as usize,
            (height.0 as usize)
                .saturating_sub(crate::screen::shell::strip_rows())
                .saturating_sub(wrapped_rows(footer, width.0 as usize) - 1),
        ),
        None => Layout {
            left: LEFT_PANE_WIDTH,
            right: RIGHT_PANE_WIDTH,
            rows: None,
        },
    }
}

/// How many terminal rows `line` takes at `width` columns: one, plus one for
/// every time its visible text runs past the right edge. A line exactly as
/// wide as the terminal still takes one — its newline lands on the wrap the
/// terminal was already holding back.
fn wrapped_rows(line: &str, width: usize) -> usize {
    let columns = crate::status::strip_ansi(line).chars().count();
    columns.div_ceil(width.max(1)).max(1)
}

/// How a terminal that size is split between the two panes. The left pane
/// takes two fifths of what the border leaves, within its own bounds, and
/// the right pane takes the rest — a terminal too narrow for both minimums
/// overflows rather than collapsing a pane to nothing.
fn layout_for(width: usize, height: usize) -> Layout {
    let content = width
        .saturating_sub(PANE_CHROME_COLUMNS + SPARE_COLUMN)
        .max(MIN_LEFT_PANE + MIN_RIGHT_PANE);
    let left = (content * 2 / 5)
        .clamp(MIN_LEFT_PANE, MAX_LEFT_PANE)
        .min(content - MIN_RIGHT_PANE);
    Layout {
        left,
        right: content - left,
        rows: Some(height.saturating_sub(PANE_CHROME_ROWS).max(1)),
    }
}

/// The column every row's value starts at in [`labeled_row`]'s wide layout:
/// four columns of indent, then the widest labels this pane ever draws —
/// `Description:` and `Starts from:`, at 13 characters padded — so
/// `Pipeline:`, `Depends on:`, `Starts from:`, `Lands in:`, `Gate:` and
/// `Description:` all line up under each other regardless of which one owns
/// a given row. A constant rather than something measured off
/// the label set at draw time — see the non-goal this is: the labels are
/// fixed, so the column never has anything to measure.
const LABEL_FIELD: usize = 13;

/// Below this width a row's value has nowhere left to go once its label has
/// taken `LABEL_FIELD` columns — [`labeled_row`] drops the value to its own
/// line, indented two past the label, instead.
const NARROW_RIGHT_PANE: usize = 40;

/// One `label: value` row for the tasks pane, wrapped to fit `width`.
///
/// Wide enough (`width >= NARROW_RIGHT_PANE`), the value sits inline with
/// the label at [`LABEL_FIELD`]'s column, wrapping under itself the same way
/// [`wrapped`] wraps anything else. Narrower than that, there is no room
/// left for a value beside its label at all, so the label gets its own line
/// and the value wraps beneath it, indented two — every label stays on
/// screen either way, which a shrinking label column could not promise.
pub(super) fn labeled_row(label: &str, value: &str, width: usize) -> Vec<String> {
    if width < NARROW_RIGHT_PANE {
        let mut lines = vec![format!("    {label}")];
        lines.extend(wrapped("      ", value, width));
        lines
    } else {
        wrapped(&format!("    {label:<LABEL_FIELD$}"), value, width)
    }
}

/// `body` broken into lines that fit `width`, the first under `prefix` and
/// every one after it indented to sit under the first line's text. A glob
/// list and a long title are both too long to truncate usefully — what a
/// person needs is at the end.
fn wrapped(prefix: &str, body: &str, width: usize) -> Vec<String> {
    let indent = " ".repeat(prefix.chars().count());
    let mut lines = Vec::new();
    let mut current = prefix.to_string();
    let mut bare = true;
    for word in body.split_whitespace() {
        if !bare && current.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::replace(&mut current, indent.clone()));
            bare = true;
        }
        if !bare {
            current.push(' ');
        }
        current.push_str(word);
        bare = false;
    }
    lines.push(current);
    lines
}

/// A group's pane tail: the bare `" queued"` or `" done"`, or empty for a
/// group still in [`GroupState::Queueable`].
///
/// Used to carry the queue file's own modification time too, but the
/// dispatcher rewrites that file at every step boundary, so the instant read
/// as "last touched" under a label that says "queued" rather than when the
/// group was actually submitted — dropped back to the bare word instead of
/// telling that lie.
fn group_tail(group: &Group) -> String {
    match group.state {
        GroupState::Queueable => String::new(),
        GroupState::Queued => " queued".to_string(),
        GroupState::Done => " done".to_string(),
    }
}

/// The indices in `shown` where [`groups_pane_lines`] draws a blank
/// separator row — one for every point [`Group::state`] actually changes
/// between one row and the next. [`reachable`] keeps queued groups off the
/// screen, so a list holding both queueable and done groups draws one.
///
/// `shown` is already in `list_groups`' own order — queueable groups, then
/// queued ones, then done ones — so a single left-to-right scan is enough:
/// a boundary sits wherever a row's state differs from the row right before
/// it. [`group_line_index`] uses these same boundaries to keep a
/// line-addressed pane (see [`window`]) in step with `group_cursor`, which
/// still addresses groups, never a separator between them.
fn group_boundaries(shown: &[&Group]) -> Vec<usize> {
    let mut boundaries = Vec::new();
    let mut previous: Option<GroupState> = None;
    for (i, group) in shown.iter().enumerate() {
        if previous.is_some_and(|state| state != group.state) {
            boundaries.push(i);
        }
        previous = Some(group.state);
    }
    boundaries
}

/// [`group_boundaries`], or empty while a filter is narrowing the list.
///
/// A filter ranks `shown` by score, not by queueable-before-queued-before-done
/// — the halves [`group_boundaries`] assumes are contiguous can end up
/// interleaved, or in either order, once a query is in effect. The mockup's
/// own filtered example draws both its matches back to back with no blank
/// row between them, done or not, which is what this returning no
/// boundaries at all produces.
fn boundary_for(shown: &[&Group], state: &ScreenState) -> Vec<usize> {
    if state.filter.is_empty() {
        group_boundaries(shown)
    } else {
        Vec::new()
    }
}

/// Where `group_cursor` — an index into groups, which never counts a
/// separator [`groups_pane_lines`] draws — lands in the pane's own lines,
/// which do. `cursor` shifts one row for every separator ahead of it: none,
/// or the one drawn once `h` shows done groups beside queueable ones.
fn group_line_index(cursor: usize, boundaries: &[usize]) -> usize {
    cursor + boundaries.iter().filter(|&&b| cursor >= b).count()
}

/// The left pane's rows: one per distinct `group:` across the pending
/// tasks, its checkbox, and whether the archive already holds it — or,
/// once `h` has hidden every one of them, a single line saying so rather
/// than an empty pane a person could mistake for a project with nothing
/// pending at all.
///
/// The checkbox is here and nowhere else, because a group is what gets
/// submitted. A group not still [`GroupState::Queueable`] has no box at all
/// — at least one of its tasks is no longer waiting in `pending/`. A done
/// group is what `h` hides on; a queued one never shows at all.
///
/// No task count: the pane beside this one is the group's tasks, so a number
/// here says the same thing twice and costs the names the room to be read.
///
/// When queueable and done groups are on screen at once, a blank row
/// separates them — see [`group_boundaries`]. That row is never a group:
/// `group_cursor` skips straight from one state's last group to the next
/// state's first in a single key press, because it addresses `shown`
/// directly and every separator lives only in these lines, not in that
/// slice.
///
/// While [`Mode::Filter`] is open, the query itself is the pane's first
/// line — `find: `, whatever has been typed, and a cursor after it — ahead
/// of every row below. `group_cursor` still addresses `shown` alone, never
/// this line, the same way it never addresses the separator; [`draw`] is
/// what shifts the row `window()` centres on to account for it.
///
/// Comes back with the line each group's row sits on as well — see
/// [`Items`] — so neither that `find:` row nor a separator is ever counted
/// as a group out of sight.
fn groups_pane_lines(
    groups: &[Group],
    shown: &[&Group],
    state: &ScreenState,
    width: usize,
) -> (Vec<String>, Vec<usize>) {
    let mut lines = Vec::new();
    if matches!(state.mode, Mode::Filter) {
        lines.push(format!("find: {}▏", state.filter));
    }

    if shown.is_empty() {
        let reachable_count = groups.iter().filter(|group| reachable(group)).count();
        lines.push(if !state.filter.is_empty() {
            "  no groups match this filter".to_string()
        } else if reachable_count == 0 {
            // Nothing this screen lists, so there is nothing behind `h`
            // either and no count to give. This is the whole of what an
            // empty pending directory draws — the screen opens onto it
            // rather than refusing, see `opening_message` — and what a
            // project whose every group is queued draws too, since a
            // queued group is never one `h` brings back.
            "  nothing to queue".to_string()
        } else {
            // Only ever the done groups — nothing is visible, so everything
            // `h` could bring back is hidden — counted over `reachable`
            // groups alone, so a queued group, which no key here brings
            // back, never inflates it. Spelled out as a difference rather
            // than a count of done groups so the message keeps meaning the
            // same thing if `visible()` ever grows a second reason to hide
            // a group.
            let hidden = reachable_count - shown.len();
            format!("  nothing to queue — {} hidden", plural(hidden, "group"))
        });
        return (lines, Vec::new());
    }

    let boundaries = boundary_for(shown, state);
    lines.reserve(shown.len() + boundaries.len());
    let mut starts = Vec::with_capacity(shown.len());
    for (i, group) in shown.iter().enumerate() {
        if boundaries.contains(&i) {
            lines.push(String::new());
        }
        let marker = if i == state.group_cursor && state.focus == Focus::Groups {
            ">"
        } else {
            " "
        };
        let box_ = match (
            selectable(group),
            state.selected.contains(&group_key(group)),
        ) {
            (false, _) => "   ",
            (true, true) => "[x]",
            (true, false) => "[ ]",
        };
        let tail = group_tail(group);
        // The name keeps a floor no tail is allowed to shrink it past
        // — MIN_NAME_COLUMN, sized to the mockup's own longest example
        // name — so a person can still tell groups apart at a glance.
        // MIN_LEFT_PANE is in turn sized so a queued row's full tail
        // and a floored name both fit without the row ever overflowing
        // `width`: below that floor this budget would either starve the
        // name or (were the floor applied blindly) truncate the tail
        // instead, which is what cost this feature its own timestamp
        // acceptance criterion the first time this floor was added.
        let name_budget = width
            .saturating_sub(tail.chars().count() + ROW_PREFIX_COLUMNS)
            .max(MIN_NAME_COLUMN);
        let name = pad_to(&group.name, name_budget);
        starts.push(lines.len());
        lines.push(format!("{marker} {box_} {name}{tail}"));
    }
    (lines, starts)
}

/// The characters a tasks-pane row spends before its own name even starts:
/// the cursor marker and the space after it — half of [`ROW_PREFIX_COLUMNS`],
/// since this pane draws no checkbox.
const TASK_ROW_PREFIX_COLUMNS: usize = 2;

/// A pending task's own tail: `"queued"` or `"done"` once a group holds two
/// states at once and this is the one still in the queue or the one already
/// archived, or empty for a task still waiting in `pending/` — the pane
/// draws no tail at all for that one, the same way [`group_tail`] draws none
/// for a wholly [`GroupState::Queueable`] group.
fn task_tail(state: TaskState) -> &'static str {
    match state {
        TaskState::Pending => "",
        TaskState::Queued => "queued",
        TaskState::Done => "done",
    }
}

/// One task's own row in the tasks pane — the cursor marker, its id, and its
/// own state's tail once a group can hold more than one (`tail` empty
/// otherwise, as the routines pane's own tasks always pass it: a routine's
/// tasks carry no state of their own to draw).
///
/// Budgets the name the same way [`groups_pane_lines`] budgets a group's own
/// — a floor at [`MIN_NAME_COLUMN`] the tail may not shrink past — so a task
/// id stays legible next to a full `queued` tail at any width this pane is
/// ever cut to.
fn task_row(marker: &str, name: &str, tail: &str, width: usize) -> String {
    if tail.is_empty() {
        return format!("{marker} {name}");
    }
    let tail = format!(" {tail}");
    let name_budget = width
        .saturating_sub(tail.chars().count() + TASK_ROW_PREFIX_COLUMNS)
        .max(MIN_NAME_COLUMN);
    format!("{marker} {}{tail}", pad_to(name, name_budget))
}

/// The right pane's rows: the highlighted group's tasks, each as a labelled
/// block — its pipeline, what it depends on, the branch it is cut from (a
/// root task only — a dependent is cut from its dependency's branch instead,
/// so it draws no such row), any gate chosen for it, and its own
/// one-sentence description — plus the task's own header row above them.
///
/// Read-only. Nothing here is picked: the checkbox is on the group, in the
/// pane to the left, and this is what that group holds.
///
/// Comes back with the line each task's header row sits on — see [`Items`]
/// — and the line range the highlighted task occupies as well — its header
/// row and everything drawn under it — which is what [`window`] keeps in
/// view on a pane taller than the terminal.
fn tasks_pane_lines(
    groups: &[Group],
    _pipelines: &Pipelines,
    state: &ScreenState,
    width: usize,
) -> (Vec<String>, Vec<usize>, (usize, usize)) {
    let Some(group) = shown(groups, state).get(state.group_cursor).copied() else {
        return (Vec::new(), Vec::new(), (0, 0));
    };
    let tasks = &group.tasks;

    let mut lines = Vec::new();
    let mut starts = Vec::with_capacity(tasks.len());
    let mut focus = (0, 0);
    for (i, task) in tasks.iter().enumerate() {
        lines.push(String::new());
        let opened = lines.len();
        starts.push(opened);
        let marker = if i == state.task_cursor && state.focus == Focus::Tasks {
            ">"
        } else {
            " "
        };
        // Once the whole group already reads a single state, every task
        // under it shares that same word — the group's own tail already
        // says it once, in the pane to the left, so a tail repeating it on
        // every row here would say nothing a person did not already know.
        let tail = if group.state == GroupState::Done {
            ""
        } else {
            task_tail(task.state)
        };
        lines.push(task_row(marker, &task.id, tail, width));

        // `task.pipeline`, `task.depends_on` and `task.base`, not a parse
        // of `task.doc` apiece: these three rows used to each call their
        // own `doc_pipeline_name`/`depends_on`/`doc_base`, splitting and
        // parsing the same doc three times over on every draw of the
        // highlighted group. `pending::list_groups_in` parses each task's
        // front matter once, while building it, and fills these three
        // fields from that one parse — see `PendingTask::depends_on`'s own
        // comment — so drawing the pane now parses nothing at all.

        // There is no project default any more, so a task naming no
        // pipeline of its own reads as unassigned here — the same as
        // `task_pipeline` resolves nothing for it, and `parse_submission`
        // refuses it outright once it is actually submitted.
        let pipeline = task
            .pipeline
            .clone()
            .unwrap_or_else(|| TRIAL_UNASSIGNED.to_string());
        lines.extend(labeled_row("Pipeline:", &pipeline, width));

        let depends_value = if task.depends_on.is_empty() {
            "-".to_string()
        } else {
            task.depends_on.join(", ")
        };
        lines.extend(labeled_row("Depends on:", &depends_value, width));

        // Where the task starts and where it lands, on every task. `base:`
        // names only where the work lands, so it is `Lands in:`; a task
        // naming none is sent on the board checkout's branch — see
        // `board_branch` — and `-` shows only when the checkout is detached
        // and `enter` would refuse it. A task starts from its own
        // `starts_from:` when it sets one, else from its first dependency.
        // That one is named by id, not by branch: a pending dependency has
        // no branch yet, and the queued summary names the branch itself
        // where it matters — see `missing_start_sentence`.
        let lands_in = task
            .base
            .clone()
            .or_else(|| state.board_branch.clone())
            .unwrap_or_else(|| "-".to_string());
        let starts_from = task
            .starts_from
            .clone()
            .or_else(|| task.depends_on.first().cloned())
            .unwrap_or_else(|| lands_in.clone());
        lines.extend(labeled_row("Starts from:", &starts_from, width));
        lines.extend(labeled_row("Lands in:", &lands_in, width));

        if let Some(step) = state.gates.get(&task_key(task)) {
            lines.extend(labeled_row("Gate:", step, width));
        }
        // A task with no `title:` draws no row at all rather than an
        // empty one — `parse_submission` is what refuses it, at submit time.
        if let Some(description) = &task.description {
            lines.extend(labeled_row("Description:", description, width));
        }

        if i == state.task_cursor {
            focus = (opened, lines.len().saturating_sub(1));
        }
    }
    (lines, starts, focus)
}

/// The routines pane's own left-hand rows: one per routine, each with its
/// checkbox and how many tasks sit at or below it — the same `N tasks` tail
/// the mockup draws, and what `enter` on a ticked one queues whole.
///
/// No routines at all names `routines_dir` directly rather than drawing an
/// empty pane a person could mistake for a project with nothing under it
/// read wrong — see [`Repo::routines_dir`] on why nothing creates that
/// directory for this to find.
///
/// Comes back with the line each folder starts on as well — see [`Items`].
/// A folder is one line, so that is every line; it is noted in the loop
/// all the same, where it cannot drift from the rows it points at.
fn routine_folder_lines(
    routines_dir: &std::path::Path,
    routines: &[RoutineFolder],
    nav: &RoutineNav,
    width: usize,
) -> (Vec<String>, Vec<usize>) {
    if routines.is_empty() {
        return (
            vec![format!("  nothing under {}", routines_dir.display())],
            Vec::new(),
        );
    }

    let mut lines = Vec::with_capacity(routines.len());
    let mut starts = Vec::with_capacity(routines.len());
    for (i, folder) in routines.iter().enumerate() {
        let marker = if i == nav.folder_cursor && nav.focus == Focus::Groups {
            ">"
        } else {
            " "
        };
        let box_ = if nav.selected.contains(&folder.path) {
            "[x]"
        } else {
            "[ ]"
        };
        // Same tail-then-name budget `groups_pane_lines` gives a queued
        // group's own row — see that function's own comment on why the
        // name gets a floor no tail is allowed to shrink it past.
        let tail = format!(" {}", plural(folder.tasks.len(), "task"));
        let name_budget = width
            .saturating_sub(tail.chars().count() + ROW_PREFIX_COLUMNS)
            .max(MIN_NAME_COLUMN);
        let name = pad_to(&folder.name, name_budget);
        starts.push(lines.len());
        lines.push(format!("{marker} {box_} {name}{tail}"));
    }
    (lines, starts)
}

/// The routines pane's own right-hand rows: the highlighted folder's own
/// tasks — every one at or below it, the same set `enter` would queue —
/// each drawn the same `labeled_row` way [`tasks_pane_lines`] draws a
/// pending task, minus the `Gate:` row a routine has no gate picker to set
/// and the `Starts from:` and `Lands in:` rows, which this pane has never
/// drawn. Comes back with
/// each task's start and the highlighted one's range, the same as
/// [`tasks_pane_lines`].
fn routine_task_lines(
    folder: Option<&RoutineFolder>,
    _pipelines: &Pipelines,
    nav: &RoutineNav,
    width: usize,
) -> (Vec<String>, Vec<usize>, (usize, usize)) {
    let Some(folder) = folder else {
        return (Vec::new(), Vec::new(), (0, 0));
    };

    let mut lines = Vec::new();
    let mut starts = Vec::with_capacity(folder.tasks.len());
    let mut focus = (0, 0);
    for (i, task) in folder.tasks.iter().enumerate() {
        lines.push(String::new());
        let opened = lines.len();
        starts.push(opened);
        let marker = if i == nav.task_cursor && nav.focus == Focus::Tasks {
            ">"
        } else {
            " "
        };
        // A routine's own tasks carry no queue/archive state of their
        // own to draw — see `task_row`'s own doc comment.
        lines.push(task_row(marker, &task.id, "", width));

        let pipeline = doc_pipeline_name(&task.doc).unwrap_or_else(|| TRIAL_UNASSIGNED.to_string());
        lines.extend(labeled_row("Pipeline:", &pipeline, width));

        let depends_on = super::pending::depends_on(&task.doc);
        let depends_value = if depends_on.is_empty() {
            "-".to_string()
        } else {
            depends_on.join(", ")
        };
        lines.extend(labeled_row("Depends on:", &depends_value, width));

        if let Some(description) = &task.description {
            lines.extend(labeled_row("Description:", description, width));
        }

        if i == nav.task_cursor {
            focus = (opened, lines.len().saturating_sub(1));
        }
    }
    (lines, starts, focus)
}

/// The whole of [`Mode::Routines`]'s own frame: the same two-pane geometry
/// [`two_pane_frame`] lays the pending screen out with, folders on the left
/// and the highlighted one's own tasks on the right.
fn render_routines(
    routines: &[RoutineFolder],
    routines_dir: &std::path::Path,
    pipelines: &Pipelines,
    nav: &RoutineNav,
    footer: &str,
) -> Vec<String> {
    let layout = layout(footer);
    let (left, left_starts) = routine_folder_lines(routines_dir, routines, nav, layout.left);
    let highlighted = routines.get(nav.folder_cursor);
    let (right, right_starts, focus) =
        routine_task_lines(highlighted, pipelines, nav, layout.right);

    let left_title = format!("routines  {} of {}", routines.len(), routines.len());
    let right_title = highlighted.map_or("no folder", |folder| folder.name.as_str());
    let folder_line = nav.folder_cursor.min(routines.len().saturating_sub(1));
    two_pane_frame(
        &window(
            &left,
            Items {
                starts: &left_starts,
                noun: Some("folder"),
            },
            (folder_line, folder_line),
            layout.rows,
            layout.left,
        ),
        &window(
            &right,
            Items {
                starts: &right_starts,
                noun: Some("task"),
            },
            focus,
            layout.rows,
            layout.right,
        ),
        &left_title,
        right_title,
        layout,
    )
}

/// Where a pane's items start among its lines, and what to call them, so
/// [`window`]'s marker row can count what is out of sight in the pane's own
/// terms rather than in lines.
///
/// A task in the tasks pane is six lines, so a count of lines said "21
/// below" when four tasks were out of sight. Only the builder that draws a
/// pane knows where one item ends and the next begins, so it notes each
/// start in the same loop that pushes the item's first line. A line no
/// start points into ahead of the first item — the filter's `find:` row —
/// and a blank separator row are never counted.
#[derive(Debug, Clone, Copy)]
pub(super) struct Items<'a> {
    /// The line each item starts on, in ascending order.
    pub(super) starts: &'a [usize],
    /// The singular noun the marker counts in (`"task"`, `"group"`,
    /// `"folder"`), or `None` for the jobs screen, which passes one item per
    /// line and keeps the one-sided `↓ N below` it has always drawn.
    pub(super) noun: Option<&'static str>,
}

/// The slice of `lines` a pane `rows` rows tall shows, with the block at
/// `focus` — a group's row, or the highlighted task and everything drawn
/// under it — kept in view.
///
/// `None` rows is a run with no terminal to measure, where nothing is cut at
/// all. Where the content does not fit, the pane's bottom row is spent on
/// saying how many of `items` are out of sight rather than on a line that
/// would be silently the last one a person sees — see [`marker_row`], which
/// fits that row to `width`.
pub(super) fn window(
    lines: &[String],
    items: Items<'_>,
    focus: (usize, usize),
    rows: Option<usize>,
    width: usize,
) -> Vec<String> {
    let Some(rows) = rows else {
        return lines.to_vec();
    };
    if lines.len() <= rows {
        return lines.to_vec();
    }
    let view = rows - 1;
    let (start, end) = focus;
    let offset = (end + 1)
        .saturating_sub(view)
        .min(start)
        .min(lines.len() - view);
    let mut shown = lines[offset..offset + view].to_vec();
    let (above, below) = hidden_items(lines, items.starts, offset, offset + view);
    shown.push(marker_row(above, below, items.noun, width));
    shown
}

/// How many items are not fully in view above and below the lines
/// `top..bottom`. An item cut in half at either edge counts as out of sight
/// on that side: part of it is, and the marker is what says so.
///
/// An item runs from its start to the last non-blank line before the next
/// one starts. The blank row the tasks pane draws ahead of every task is a
/// separator, not the end of the task before it — counting it would call a
/// task whose own last line is on screen "below" whenever only that blank
/// row had scrolled off.
fn hidden_items(lines: &[String], starts: &[usize], top: usize, bottom: usize) -> (usize, usize) {
    let mut above = 0;
    let mut below = 0;
    for (k, &start) in starts.iter().enumerate() {
        let next = starts.get(k + 1).copied().unwrap_or(lines.len());
        let last = (start..next)
            .rev()
            .find(|&line| !lines[line].is_empty())
            .unwrap_or(start);
        if start < top {
            above += 1;
        } else if last >= bottom {
            below += 1;
        }
    }
    (above, below)
}

/// The marker row under a cut pane: both directions on one row when items
/// are hidden on both sides, only the one side otherwise.
///
/// Named with its noun first — `↑ 11 groups above · ↓ 19 groups below` —
/// and shortened when the pane is too narrow, first to `↑ 11 above · ↓ 19
/// below` and then to `↑ 11 · ↓ 19`. The first form that fits `width` is
/// drawn: [`two_pane_frame`]'s own `pad_to` would otherwise cut a wider row
/// off mid-word without a sign it had. The last form is drawn whatever the
/// width, as nothing shorter still says both counts.
///
/// With no noun this is the jobs screen's marker exactly as it has always
/// read: one direction only, below whenever anything is.
///
/// Nothing out of sight on either side — every hidden line was a separator
/// — draws an empty row rather than a `↓ 0 below` that says nothing.
fn marker_row(above: usize, below: usize, noun: Option<&str>, width: usize) -> String {
    let Some(noun) = noun else {
        return match below {
            0 => format!("↑ {above} above"),
            _ => format!("↓ {below} below"),
        };
    };
    // 0 is the full row, 1 drops the noun, 2 drops the words as well.
    let form = |short: u8| {
        let side = |arrow: &str, n: usize, word: &str| match short {
            0 => format!("{arrow} {} {word}", plural(n, noun)),
            1 => format!("{arrow} {n} {word}"),
            _ => format!("{arrow} {n}"),
        };
        let mut sides = Vec::new();
        if above > 0 {
            sides.push(side("↑", above, "above"));
        }
        if below > 0 {
            sides.push(side("↓", below, "below"));
        }
        sides.join(" · ")
    };
    let forms = [form(0), form(1), form(2)];
    forms
        .iter()
        .find(|row| row.chars().count() <= width)
        .unwrap_or(&forms[2])
        .clone()
}

/// Lay two panes of lines side by side, bordered in box-drawing characters —
/// the layout the acceptance criteria and the mockup both call for: the
/// pending groups on the left, the highlighted group's tasks on the right,
/// rather than the two stacked one above the other.
pub(super) fn two_pane_frame(
    left: &[String],
    right: &[String],
    left_title: &str,
    right_title: &str,
    layout: Layout,
) -> Vec<String> {
    // Both halves of the top border have to reach exactly as far as a row
    // does: a row's own pane is `" " + layout.<side> + " │"`, which is
    // `layout.<side> + 2` characters wide including its closing corner. Each
    // title has already spent some of that budget, so the dash run only
    // fills what is left over — and a title too long for its pane is cut to
    // it rather than pushing the corner past the rows below.
    let top_left: String = format!("─ {left_title} ")
        .chars()
        .take(layout.left + 2)
        .collect();
    let top_right: String = format!("─ {right_title} ")
        .chars()
        .take(layout.right + 2)
        .collect();
    let mut frame = vec![format!(
        "┌{}{}┬{}{}┐",
        top_left,
        "─".repeat((layout.left + 2).saturating_sub(top_left.chars().count())),
        top_right,
        "─".repeat((layout.right + 2).saturating_sub(top_right.chars().count()))
    )];
    let rows = layout
        .rows
        .unwrap_or_else(|| left.len().max(right.len()).max(1));
    for i in 0..rows {
        let l = left.get(i).map(String::as_str).unwrap_or("");
        let r = right.get(i).map(String::as_str).unwrap_or("");
        frame.push(format!(
            "│ {} │ {} │",
            pad_to(l, layout.left),
            pad_to(r, layout.right)
        ));
    }
    frame.push(format!(
        "└{}┴{}┘",
        "─".repeat(layout.left + 2),
        "─".repeat(layout.right + 2)
    ));
    frame
}

/// The keys, under the frame — the one line that says what the screen does,
/// every one of them built by [`crate::screen::key_hint`] rather than a
/// second hand-spelled literal, so a key line here reads exactly the same
/// way the board's own does — see that function's own doc comment.
///
/// The arrows that move the cursor are never named on the ordinary line:
/// naming every key a screen reads would crowd out the ones a person
/// actually has to be told about, and `↑↓` are read the same way by every
/// screen in this project regardless. The trial picker's own lines are the
/// exception — they repeat the key row inside its popup, which names them.
/// `q` is not named outside bare `spoolway`, where no mode reads it as
/// anything special, so there is nothing about it to say; `ctrl-c` is the
/// way out, and a footer line has no key of its own to name for that
/// either. Inside bare `spoolway` it does quit, while browsing on the queue
/// tab and over the routines tab's pane, and those two lines name it there.
///
/// The ordinary line follows focus. It always leads with `space select`
/// then `enter queue`, and the pane's own keys come after them. `g` and `o`
/// act only on a task under the tasks pane's cursor, so they are named only
/// while that pane has focus, first among the pane's own keys, since they
/// are what the pane is for. `tab` is named in both panes as the pane it
/// leads to — the one way into the tasks pane inside bare `spoolway`, where
/// `←`/`→` switch tabs instead.
///
/// While a picker is open over the screen — [`Mode::Gate`], [`Mode::Trial`],
/// [`Mode::SaveRoutine`], [`Mode::DeleteRoutine`], [`Mode::NewJob`] — none of the ordinary line's
/// keys is read, so this draws the picker's own key row instead, the same keys its popup names. A
/// notice is different: it reads only `enter` to close it, which its own
/// popup says, and the line under the frame stays the one the screen will
/// read again once it is closed — all but [`Mode::JobSaved`], which the
/// routines tab's mockup draws over its own `[enter] confirm` line.
///
/// While [`Mode::Filter`] is open the ordinary line makes no sense at all —
/// none of the ordinary line's keys is read while the filter box has
/// focus — so this draws the filter's own line instead, naming exactly the
/// one key [`handle_filter_key`] does not simply append to the query:
/// `enter`, which leaves it.
fn footer(state: &ScreenState) -> String {
    match &state.mode {
        Mode::Filter => key_hint(&[("enter", "leave search")]),
        // The save panel reads a name the same way the filter box reads a
        // query, so its own line names the same two keys the filter's does
        // to leave it — `enter`/`esc` — rather than any of the ordinary
        // line's, none of which this mode reads as anything but a letter.
        Mode::SaveRoutine { .. } => key_hint(SAVE_KEYS),
        Mode::DeleteRoutine { .. } => key_hint(DELETE_ROUTINE_KEYS),
        Mode::NewJob { walk, .. } => walk.footer(),
        Mode::JobSaved { .. } => hint(&confirm()),
        Mode::Gate(_) => key_hint(GATE_KEYS),
        Mode::Trial(trial) => match trial.stage {
            TrialStage::PickPipelines => key_hint(PICK_KEYS),
            TrialStage::ChooseSkips => key_hint(SKIP_KEYS),
        },
        // Its own screen, not an overlay over the pending one — so its own
        // line, naming exactly the keys `handle_routine_key`,
        // `open_highlighted_routine` and `run_screen`'s own `Mode::Routines`
        // arm read, rather than the ordinary line's `f`/`g`/`t`/`s`, none of
        // which apply here. One line per pane, as the pending screen's is:
        // `n` and `x` only over the list, the one pane that makes a job of
        // a routine or deletes one, and `o` only where a task is under the
        // cursor. No `esc`: over the list
        // it does nothing, since this pane is the routines tab's whole
        // screen, and over the tasks pane it goes back to the list, which
        // `tab` already names. The same line under a notice drawn over the
        // pane.
        _ if let Some(nav) = routines_beneath(&state.mode) => {
            let pane: &[(&str, &str)] = match nav.focus {
                Focus::Groups => &[("n", "new job"), ("x", "delete"), ("tab", "tasks")],
                Focus::Tasks => &[("o", "open task"), ("tab", "routines")],
            };
            key_hint(
                &[
                    [("space", "select"), ("enter", "queue")].as_slice(),
                    pane,
                    crate::screen::shell::quit_hint(),
                ]
                .concat(),
            )
        }
        _ => {
            let hide = match state.hide_scope {
                HideScope::Pending => "show done tasks",
                HideScope::PlusDone => "hide done tasks",
            };
            // `q` only inside bare `spoolway`'s queue tab, the one place it
            // quits — see `crate::screen::shell::quit_hint`.
            let pane: &[(&str, &str)] = match state.focus {
                Focus::Groups => &[("f", "find"), ("tab", "tasks")],
                Focus::Tasks => &[
                    ("g", "gate"),
                    ("o", "open task"),
                    ("tab", "groups"),
                    ("f", "find"),
                ],
            };
            let keys = [
                [("space", "select"), ("enter", "queue")].as_slice(),
                pane,
                [("t", "trial"), ("s", "save as routine"), ("h", hide)].as_slice(),
                crate::screen::shell::quit_hint(),
            ]
            .concat();
            key_hint(&keys)
        }
    }
}

/// Everything a frame would put on the terminal, as plain lines — the whole
/// of what [`draw`] writes, computed without touching `out` at all so a
/// caller can tell whether a fresh frame differs from the last one before
/// spending a write on it.
/// Everything a frame needs beside `groups` and `state` themselves — bundled
/// so `render`, `draw` and `wait_for_key` stay under the same argument count
/// every other function in this module is held to, rather than each
/// threading `routines`, `routines_dir` and `pipelines` through as three
/// separate parameters.
struct Panes<'a> {
    routines: &'a [RoutineFolder],
    routines_dir: &'a std::path::Path,
    pipelines: &'a Pipelines,
}

/// The gate picker's keys, in its popup and on the line under the frame.
const GATE_KEYS: &[(&str, &str)] = &[("enter", "set"), ("g", "clear"), ("esc", "cancel")];

/// The save panel's keys, in its popup and on the line under the frame.
const SAVE_KEYS: &[(&str, &str)] = &[("enter", "save"), ("esc", "cancel")];

/// `x`'s popup's keys, in its popup and on the line under the frame — the
/// same pair the jobs tab's delete answers to.
const DELETE_ROUTINE_KEYS: &[(&str, &str)] = &[("enter", "delete"), ("esc", "keep")];

/// The trial picker's second screen's keys, in its popup and on the line
/// under the frame.
const SKIP_KEYS: &[(&str, &str)] = &[
    ("↑↓", "step"),
    ("←→", "pipeline"),
    ("space", "skip"),
    ("enter", "run"),
    ("esc", "pipelines"),
];

/// The trial picker's first screen's keys, in its popup and on the line
/// under the frame. `enter` does nothing while no pipeline is ticked; the
/// arm count under the list already reads `0 arms` then, which is the reason
/// a person needs, so the row itself stays the one the mockup draws.
const PICK_KEYS: &[(&str, &str)] = &[
    ("↑↓", "move"),
    ("space", "tick"),
    ("enter", "next"),
    ("esc", "cancel"),
];

/// A trial popup's key row, kept inside `width` like every other row it
/// draws — the bracketed row is often its widest, and on a narrow frame it
/// would otherwise push the popup's right border off it. Tightened to two
/// spaces between keys first, as step 5 of the screen's mockup draws a
/// cramped one, and cut only when even that does not fit.
fn fit_keys(row: String, width: usize) -> String {
    if row.chars().count() <= width {
        return row;
    }
    clip(row.replace(crate::status::GUTTER, "  "), width)
}

/// The routines pane a mode is drawn over, when it is: the pane itself, a
/// notice opened from it, `x`'s delete popup, `n`'s job popups and the
/// notice they end on, or the tool-requirements gate or the issue question
/// a routine submit stopped at. `None` for everything drawn over
/// the pending screen.
fn routines_beneath(mode: &Mode) -> Option<&RoutineNav> {
    match mode {
        Mode::Routines(nav)
        | Mode::Outcome {
            routines: Some(nav),
            ..
        }
        | Mode::Queued {
            routines: Some(nav),
            ..
        }
        | Mode::ToolGate {
            then: Resume::Routines(nav) | Resume::RoutineTask(nav),
            ..
        }
        | Mode::IssueQuestion {
            then: Resume::Routines(nav) | Resume::RoutineTask(nav),
            ..
        }
        | Mode::DeleteRoutine { nav, .. }
        | Mode::NewJob { nav, .. }
        | Mode::JobSaved { nav, .. } => Some(nav),
        _ => None,
    }
}

/// A frame: the pending screen, or the routines pane, with whatever popup
/// the mode has open laid over it and the key line under it.
fn render(groups: &[Group], panes: &Panes, state: &ScreenState) -> Vec<String> {
    let footer = footer(state);
    let frame = beneath(groups, panes, state, &footer);
    let popup = popup(groups, panes.pipelines, state, layout(&footer));
    compose(frame, popup.as_deref(), footer)
}

/// The pending screen or the routines pane the mode is drawn over, with no
/// popup on it and no key line under it.
fn beneath(groups: &[Group], panes: &Panes, state: &ScreenState, footer: &str) -> Vec<String> {
    match routines_beneath(&state.mode) {
        Some(nav) => render_routines(
            panes.routines,
            panes.routines_dir,
            panes.pipelines,
            nav,
            footer,
        ),
        None => pending_frame(groups, panes.pipelines, state, layout(footer)),
    }
}

/// `frame` with `popup` laid over it and `footer` under it.
fn compose(mut frame: Vec<String>, popup: Option<&[String]>, footer: String) -> Vec<String> {
    if let Some(popup) = popup {
        stretch(&mut frame, popup.len() + 4);
        overlay(&mut frame, popup);
    }
    frame.push(footer);
    frame
}

/// Lengthen a [`two_pane_frame`] to at least `lines` lines, borders
/// included, with empty rows above its bottom border.
///
/// A frame drawn with no terminal to measure is only as tall as its content
/// — see [`two_pane_frame`]'s `rows: None` — and [`overlay`] writes nothing
/// past the frame's last line, so a popup taller than a short list would
/// lose its key row and its bottom border. A real terminal's frame already
/// fills the rows it has, and is left as it is.
fn stretch(frame: &mut Vec<String>, lines: usize) {
    let Some(bottom) = frame.last() else {
        return;
    };
    // The bottom border with its corners and dashes blanked is exactly an
    // empty row of the same two panes.
    let empty: String = bottom
        .chars()
        .map(|c| match c {
            '└' | '┴' | '┘' => '│',
            _ => ' ',
        })
        .collect();
    while frame.len() < lines {
        let at = frame.len() - 1;
        frame.insert(at, empty.clone());
    }
}

/// The popup the mode has open over the screen, if any. A picker whose
/// group or task has gone from under it draws none — see [`gate_panel`].
fn popup(
    groups: &[Group],
    pipelines: &Pipelines,
    state: &ScreenState,
    layout: Layout,
) -> Option<Vec<String>> {
    match &state.mode {
        Mode::Gate(cursor) => gate_panel(groups, pipelines, state, *cursor),
        Mode::Trial(trial) => trial_panel(
            groups,
            pipelines,
            trial,
            (checkbox_row_cap(layout), popup_height(layout)),
        ),
        Mode::SaveRoutine { group, name } => save_routine_panel(groups, group, name),
        Mode::DeleteRoutine { target, .. } => Some(delete_routine_panel(target)),
        Mode::NewJob { walk, .. } => Some(walk.panel(pipelines)),
        // Wrapped no wider than the frame has room for, so a narrow
        // terminal still sees the popup's right border — see
        // `checkbox_row_cap`.
        Mode::Outcome { notice, .. } | Mode::JobSaved { notice, .. } => {
            Some(notice.panel(crate::screen::NOTICE_WRAP.min(checkbox_row_cap(layout))))
        }
        Mode::ToolGate { panel, .. }
        | Mode::IssueQuestion { panel, .. }
        | Mode::Queued { panel, .. }
        | Mode::SyncGate(panel) => Some(panel.clone()),
        // Wrapped to the frame the same way as `Mode::Outcome` above.
        Mode::Ignored(popup) => {
            Some(popup.panel(crate::commands::IGNORED_POPUP_WRAP.min(checkbox_row_cap(layout))))
        }
        Mode::Browsing | Mode::Filter | Mode::Routines(_) => None,
    }
}

/// The pending screen's own two panes, groups on the left and the
/// highlighted group's tasks on the right.
fn pending_frame(
    groups: &[Group],
    pipelines: &Pipelines,
    state: &ScreenState,
    layout: Layout,
) -> Vec<String> {
    let shown = shown(groups, state);
    let (left, left_starts) = groups_pane_lines(groups, &shown, state, layout.left);
    let (right, right_starts, focus) = tasks_pane_lines(groups, pipelines, state, layout.right);
    // The total counts only what `h` and `f` can reach — a queued group is
    // never on this screen, so counting it would promise a row no key shows.
    let total = groups.iter().filter(|group| reachable(group)).count();
    let left_title = format!("groups  {} of {}", shown.len(), total);
    let title = shown
        .get(state.group_cursor)
        .map(|group| group.name.as_str())
        .unwrap_or("no group");
    // `group_cursor` addresses `shown`, not `left`'s own lines — the two
    // disagree by one row past every separator [`groups_pane_lines`] draws
    // between two states (never present while filtering — see
    // `boundary_for`), and by one more while `Mode::Filter` is open and the
    // query itself occupies the pane's first line. `group_line_index` is the
    // same shift `groups_pane_lines` made when it inserted those separators;
    // the filter's own row is added on top of that.
    let query_row = usize::from(matches!(state.mode, Mode::Filter));
    let group_line = query_row + group_line_index(state.group_cursor, &boundary_for(&shown, state));
    two_pane_frame(
        &window(
            &left,
            Items {
                starts: &left_starts,
                noun: Some("group"),
            },
            (group_line, group_line),
            layout.rows,
            layout.left,
        ),
        &window(
            &right,
            Items {
                starts: &right_starts,
                noun: Some("task"),
            },
            focus,
            layout.rows,
            layout.right,
        ),
        &left_title,
        title,
        layout,
    )
}

/// Redraw the screen through `writer`, which paints only when [`render`]
/// actually comes back different from the last frame it painted — a resize
/// is read fresh every call through [`layout`], and a poll tick where
/// nothing on screen would change is the common case once `run_screen`'s
/// wait is a loop rather than a single blocking read. Writing on every tick
/// regardless would turn an idle terminal into one that redraws about once
/// a second for no reason a person watching it could see — exactly what the
/// "idle screen writes nothing" acceptance criterion rules out.
fn draw(
    groups: &[Group],
    panes: &Panes,
    state: &ScreenState,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    out: &mut impl std::io::Write,
) {
    // Under the strip when bare `spoolway` hosts this screen as its queue
    // tab, and exactly as before everywhere else — see
    // `crate::screen::shell::under_strip`.
    paint(render(groups, panes, state), writer, out);
}

/// Write `frame` through `writer`, which paints only when it differs from
/// the last frame written — [`draw`]'s own write, shared with
/// [`Held::draw`].
fn paint(
    frame: Vec<String>,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    out: &mut impl std::io::Write,
) {
    let frame = crate::screen::shell::under_strip(frame);
    writer.write_frame(&frame, crate::screen::pane_size(), out);
}

/// The gate picker: the highlighted task's own pipeline, a step at a time,
/// with the one currently chosen marked. `None` when there is no task under
/// the cursor to gate, or its task names a pipeline that will not
/// resolve — the same degradation [`handle_gate_key`] already makes.
fn gate_panel(
    groups: &[Group],
    pipelines: &Pipelines,
    state: &ScreenState,
    cursor: usize,
) -> Option<Vec<String>> {
    let group = shown(groups, state).get(state.group_cursor).copied()?;
    let task = group.tasks.get(state.task_cursor)?;
    let pipeline = task_pipeline(&task.text().ok()?, pipelines).ok()?;

    let chosen = state.gates.get(&task_key(task));
    let mut body = vec![String::new()];
    body.extend(pipeline.steps.iter().enumerate().map(|(i, step)| {
        let marker = if i == cursor { ">" } else { " " };
        let mark = if chosen == Some(&step.id) { " ·" } else { "" };
        format!("{marker} {}{mark}", step.id)
    }));

    Some(panel(
        &format!("gate {} at", task.id),
        &body,
        &keys(GATE_KEYS),
    ))
}

/// The project's pipelines, in the order the trial picker lists them —
/// alphabetical, since [`crate::pipeline::Pipelines::pipelines`] is a
/// `BTreeMap` and this reads its keys straight.
fn trial_pipeline_names(pipelines: &Pipelines) -> Vec<&str> {
    pipelines.pipelines.keys().map(String::as_str).collect()
}

/// The ticked pipeline the skip screen's page shows, with its place among
/// the ticks and how many there are. The page is clamped to the last tick
/// rather than trusted, since a reload can take a ticked pipeline away
/// under an open picker — see [`trial_ticked`]. `None` only when nothing is
/// ticked at all, which the first screen's `enter` never lets through.
fn trial_page<'a>(
    pipelines: &'a Pipelines,
    trial: &TrialState,
) -> Option<(usize, usize, &'a str, &'a crate::pipeline::Pipeline)> {
    let ticked = trial_ticked(pipelines, trial);
    let page = trial.page.min(ticked.len().checked_sub(1)?);
    let (name, pipeline) = ticked[page];
    Some((page, ticked.len(), name, pipeline))
}

/// The rows a trial popup spends on something other than its scrolling
/// list: the two borders, and the blank row and key row [`panel`] puts
/// under every body. Each screen adds its own fixed rows on top — see
/// [`PICK_CHROME_ROWS`] and [`SKIP_CHROME_ROWS`].
const TRIAL_PANEL_ROWS: usize = 4;

/// The first screen's own fixed rows: a blank, its heading and a blank over
/// the list, and a blank and the arm count under it.
const PICK_CHROME_ROWS: usize = TRIAL_PANEL_ROWS + 5;

/// The second screen's own fixed rows: a blank, its title row and a blank
/// over the list.
const SKIP_CHROME_ROWS: usize = TRIAL_PANEL_ROWS + 3;

/// How many list rows a trial popup `height` lines tall has room for once
/// `chrome` of them are spent, and never fewer than one — a terminal too
/// short for even that still shows the cursor's own row, rather than a list
/// of none a person could not move through. `None` is a run with no terminal
/// to measure, where nothing is cut.
fn list_room(height: Option<usize>, chrome: usize) -> usize {
    height.map_or(usize::MAX, |height| height.saturating_sub(chrome).max(1))
}

/// The slice of `rows` a list `room` rows tall shows, with the row at
/// `cursor` always among them, and a `↑ n more` row over it and a `↓ n
/// more` row under it whenever rows are hidden on that side. Both markers
/// are counted inside `room`, so what comes back is never taller than it:
/// a trial popup taller than the frame loses its bottom rows and its own
/// bottom border to [`overlay`], which drops whatever runs past the frame.
///
/// The window is worked out afresh from the cursor on every frame rather
/// than remembered, the way [`window`] keeps a pane's focus in view. Under
/// three rows there is no room for a marker beside a row, so the markers
/// are dropped and only the rows around the cursor are drawn.
fn scroll_rows(rows: &[String], cursor: usize, room: usize) -> Vec<String> {
    let room = room.max(1);
    let len = rows.len();
    if len <= room {
        return rows.to_vec();
    }
    let cursor = cursor.min(len - 1);
    if room < 3 {
        let offset = cursor.saturating_sub(room - 1).min(len - room);
        return rows[offset..offset + room].to_vec();
    }
    // At either end one marker is enough; in the middle both are drawn, and
    // the cursor sits on the last row between them.
    let edge = room - 1;
    let (offset, count) = if cursor < edge {
        (0, edge)
    } else if cursor >= len - edge {
        (len - edge, edge)
    } else {
        (cursor + 3 - room, room - 2)
    };
    let below = len - offset - count;
    let mut out = Vec::with_capacity(room);
    if offset > 0 {
        out.push(format!("  ↑ {offset} more"));
    }
    out.extend_from_slice(&rows[offset..offset + count]);
    if below > 0 {
        out.push(format!("  ↓ {below} more"));
    }
    out
}

/// The trial picker's first screen: every project pipeline, its own row
/// and its own tick, and under the list how many arms the ticks come to —
/// every ticked pipeline runs the whole group, so the count is the product,
/// and a big group with many ticks queues a lot of work a person should see
/// before `enter` writes it. A project with more pipelines than the popup
/// has rows for scrolls the list the way the skip screen does — see
/// [`scroll_rows`].
fn pick_pipelines_panel(
    group: &Group,
    pipelines: &Pipelines,
    trial: &TrialState,
    (width, height): (usize, Option<usize>),
) -> Vec<String> {
    let mut body = vec![
        String::new(),
        "pick pipelines to compare".to_string(),
        String::new(),
    ];
    let rows: Vec<String> = trial_pipeline_names(pipelines)
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            let marker = if i == trial.cursor { '>' } else { ' ' };
            let tick = if trial.ticked.contains(name) {
                "[x]"
            } else {
                "[ ]"
            };
            clip(format!("{marker} {tick} {name}"), width)
        })
        .collect();
    body.extend(scroll_rows(
        &rows,
        trial.cursor,
        list_room(height, PICK_CHROME_ROWS),
    ));

    let ticked = trial_ticked(pipelines, trial).len();
    let tasks = group.tasks.len();
    body.push(String::new());
    body.push(clip(
        format!(
            "{} × {} = {}",
            plural(ticked, "pipeline"),
            plural(tasks, "task"),
            plural(ticked * tasks, "arm")
        ),
        width,
    ));

    panel(
        &clip(format!("trial {}", group.name), width),
        &body,
        &fit_keys(keys(PICK_KEYS), width),
    )
}

/// The widest a line a trial popup draws may run, so the popup [`boxed`]
/// draws around it never grows wider than the frame [`overlay`] is centring
/// it over — which is what happens with nothing here to stop it: `overlay`
/// writes a panel line onto the frame underneath it one character at a time
/// and simply stops once it runs past the frame's own width, taking the
/// panel's own right and bottom border with it and leaving whatever ran off
/// screen undrawn. The same cap bounds every notice popup on this screen.
///
/// [`two_pane_frame`] draws a frame row `layout.left + layout.right + 7`
/// columns wide, and [`boxed`] spends 6 of those around a body line's own
/// text — its two-space indent, its own inner padding, and its two border
/// columns — so a line here has to stay at `layout.left + layout.right + 1`
/// or narrower. Capped one column under that rather than exactly on it, so
/// the cut always happens before `overlay`'s own silent truncation could.
fn checkbox_row_cap(layout: Layout) -> usize {
    layout.left + layout.right
}

/// What the tasks pane shows for a task naming no `pipeline:` of its own —
/// there is no project default to show instead, and `parse_submission`
/// refuses such a task outright once it is submitted.
const TRIAL_UNASSIGNED: &str = "(unset)";

/// Cut `line` to `width` columns, ending it in `…` when there was more,
/// for the lines of the trial popups — a pipeline or step name, and the
/// group name in a popup's title.
///
/// Cutting is safe there because every row is its own pipeline or step, so
/// a cut row still has its own checkbox for the cursor to reach. The rule is
/// the one [`checkbox_row_cap`] states: no line a trial popup draws may be wider
/// than the frame [`overlay`] paints it onto, because `overlay` answers an
/// over-wide line by silently dropping the rest of it along with the
/// popup's own right and bottom border.
pub(crate) fn clip(line: String, width: usize) -> String {
    if line.chars().count() <= width {
        return line;
    }
    let mut out: String = line.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The trial picker's second screen: one ticked pipeline to a page, in the
/// order [`trial_ticked`] lists them, its steps one per row, each with its
/// own tick to skip. A page is a pipeline, not a task: what is ticked on it
/// is skipped by every arm of that pipeline's copy of the group. The title
/// row names the page and counts its own skips, since a tick on another page
/// is out of sight.
///
/// The steps scroll inside whatever rows the popup has left once its own
/// rows are drawn — see [`scroll_rows`] — so the popup is never taller than
/// `height`. `release` runs 17 steps, and several such pipelines in one
/// popup with no scrolling ran past the frame's bottom, where [`overlay`]
/// dropped the rest along with the popup's own bottom border.
fn choose_skips_panel(
    group: &Group,
    pipelines: &Pipelines,
    trial: &TrialState,
    (width, height): (usize, Option<usize>),
) -> Vec<String> {
    let mut body = vec![String::new()];
    if let Some((page, pages, name, pipeline)) = trial_page(pipelines, trial) {
        let skip = trial.skip.get(name);
        let skipped = pipeline
            .steps
            .iter()
            .filter(|step| skip.is_some_and(|set| set.contains(&step.id)))
            .count();
        body.push(clip(
            format!(
                "skip steps   ←  {name}  {} of {pages}  →   {skipped} skipped",
                page + 1
            ),
            width,
        ));
        body.push(String::new());
        let rows: Vec<String> = pipeline
            .steps
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let marker = if i == trial.cursor { '>' } else { ' ' };
                let ticked = skip.is_some_and(|set| set.contains(&step.id));
                let box_ = if ticked { "[x]" } else { "[ ]" };
                clip(format!("{marker} {box_} {}", step.id), width)
            })
            .collect();
        body.extend(scroll_rows(
            &rows,
            trial.cursor,
            list_room(height, SKIP_CHROME_ROWS),
        ));
    }

    panel(
        &clip(format!("trial {}", group.name), width),
        &body,
        &fit_keys(keys(SKIP_KEYS), width),
    )
}

/// The trial picker, on whichever of its two screens `trial.stage` names.
/// `None` when the group `t` was pressed over is no longer among `groups` at
/// all, or has been emptied of every task — which nothing on this screen can
/// actually cause, since the picker only opens over a group that already has
/// at least one, but a reload racing an open picker is handled the same
/// careful way [`gate_panel`] already is. `width` is the budget both
/// screens keep every line they draw inside — see [`checkbox_row_cap`] for
/// what happens to a popup that runs past it — and `height` the most lines
/// either popup may take, `None` where there is no terminal to measure.
fn trial_panel(
    groups: &[Group],
    pipelines: &Pipelines,
    trial: &TrialState,
    bounds: (usize, Option<usize>),
) -> Option<Vec<String>> {
    let group = trial_group(groups, trial)?;
    if group.tasks.is_empty() {
        return None;
    }
    Some(match trial.stage {
        TrialStage::PickPipelines => pick_pipelines_panel(group, pipelines, trial, bounds),
        TrialStage::ChooseSkips => choose_skips_panel(group, pipelines, trial, bounds),
    })
}

/// The most lines a trial popup may take over a frame drawn to `layout`.
/// The frame is `layout.rows` plus its two borders, and [`compose`]
/// stretches any frame shorter than the popup plus four rows — right for a
/// frame with no terminal under it, but on a real one every row it adds
/// pushes the frame's top off the screen. Two under `layout.rows` is the
/// tallest popup that leaves the frame as it was.
fn popup_height(layout: Layout) -> Option<usize> {
    layout.rows.map(|rows| rows.saturating_sub(2))
}

/// The save panel: the folder `name` would save under, and how many
/// tasks it would copy there unchanged — `None` when the group `s` was
/// pressed over is no longer among `groups` at all, which nothing on this
/// screen can actually make happen since the panel only opens with one under
/// the cursor, but a stale key kept past a reload is handled the same
/// careful way [`gate_panel`] and [`trial_panel`] handle it.
fn save_routine_panel(groups: &[Group], group: &GroupKey, name: &str) -> Option<Vec<String>> {
    let group = groups.iter().find(|g| &group_key(g) == group)?;
    // The mockup draws this repo-relative, the same way every other path
    // under the tracked control plane is named in this project's own prose
    // — `crate::config::ROUTINES_DIR` is that relative path already, so
    // this needs no `Repo` to build it from.
    let body = vec![
        String::new(),
        format!("{}/{name}_", crate::config::ROUTINES_DIR),
        String::new(),
        format!(
            "copies {} unchanged, same ids",
            plural(group.tasks.len(), "task")
        ),
    ];
    Some(panel(
        &format!("save {} as a routine", group.name),
        &body,
        &keys(SAVE_KEYS),
    ))
}

/// One key over the trial picker — everything but `enter`, which needs the
/// repo to either advance past the first screen or write the batch. Pure
/// given the pipelines it lists, so it can be checked without a screen to
/// drive: given a state and a key, the next state.
fn handle_trial_key(pipelines: &Pipelines, mut trial: TrialState, key: Key) -> Mode {
    match trial.stage {
        TrialStage::PickPipelines => {
            let names = trial_pipeline_names(pipelines);
            let last = names.len().saturating_sub(1);
            match key {
                Key::Esc => return Mode::Browsing,
                Key::Up | Key::Char('k') => trial.cursor = trial.cursor.saturating_sub(1),
                Key::Down | Key::Char('j') => trial.cursor = (trial.cursor + 1).min(last),
                Key::Char(' ') => {
                    if let Some(name) = names.get(trial.cursor) {
                        // Unticking drops that pipeline's skips with it: a
                        // pipeline ticked again later starts from none,
                        // rather than carrying picks off a screen a person
                        // has since left behind.
                        if !trial.ticked.remove(*name) {
                            trial.ticked.insert(name.to_string());
                        } else {
                            trial.skip.remove(*name);
                        }
                    }
                }
                _ => {}
            }
        }
        TrialStage::ChooseSkips => {
            let pages = trial_ticked(pipelines, &trial).len().max(1);
            let page = trial_page(pipelines, &trial);
            let last = page.map_or(0, |(_, _, _, pipeline)| {
                pipeline.steps.len().saturating_sub(1)
            });
            match key {
                // Back to the first screen, not out of the picker — `enter`
                // moving forward through the two screens is what `esc` walks
                // back through, one at a time; nothing already picked is
                // dropped by moving between them either way.
                Key::Esc => {
                    trial.stage = TrialStage::PickPipelines;
                    trial.cursor = 0;
                }
                Key::Up | Key::Char('k') => trial.cursor = trial.cursor.saturating_sub(1),
                Key::Down | Key::Char('j') => trial.cursor = (trial.cursor + 1).min(last),
                // Turning the page wraps round at either end, so the `←`
                // drawn on the first page and the `→` on the last both do
                // something. A new page starts on its first step: its
                // pipeline's steps are not the ones the cursor was counting.
                Key::Left | Key::Right => {
                    let current = page.map_or(0, |(page, ..)| page);
                    trial.page = match key {
                        Key::Left => (current + pages - 1) % pages,
                        _ => (current + 1) % pages,
                    };
                    trial.cursor = 0;
                }
                Key::Char(' ') => {
                    if let Some((_, _, name, pipeline)) = page
                        && let Some(step) = pipeline.steps.get(trial.cursor)
                    {
                        let set = trial.skip.entry(name.to_string()).or_default();
                        if !set.remove(&step.id) {
                            set.insert(step.id.clone());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Mode::Trial(trial)
}

/// `n` with its noun, singular where that is what `n` is.
fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

/// Validate the selection and write it — the whole of what pressing `enter`
/// does. Nothing is drawn in between but the tool-requirements gate, when
/// the hook declares a tool this machine cannot meet, and with issue
/// tracking on the question [`issue_question`] asks before any ticket is
/// opened: a validation failure hands back a [`Mode::Outcome`] titled
/// `submission refused`, the same refusal `queue_add_tasks` hands back,
/// and a clean batch goes on to [`finish_submit`] and to the
/// [`Mode::Queued`] popup saying what it queued. A task whose start branch
/// does not exist, and every task in the batch depending on it, is left out
/// of a clean batch rather than refusing it — see [`not_queued`] — and the
/// popup lists it, whether or not anything else was queued. A failure past
/// validation — the hook's own, most often — is titled `queue refused`, as
/// the screen's mockup draws a failed hook. Queuing is all `enter` does:
/// starting a dispatcher is the dispatch tab's own `enter`.
///
/// Takes `state` mutably rather than by reference: `selected_tasks` reads
/// it to build the batch, and a landed write clears its selection and gates
/// right here, before the caller ever sees the resulting mode.
///
/// `redraw` lays the `opening issues` popup over the tab as each ticket
/// comes in — see [`PopupTickets`].
fn begin_submission(
    repo: &Repo,
    pipelines: &Pipelines,
    base: &str,
    groups: &mut Vec<Group>,
    state: &mut ScreenState,
    tracking: Tracking,
    redraw: &mut dyn FnMut(&[String]),
) -> Mode {
    let tasks = selected_tasks(groups, state);
    let pending = match validate_batch(repo, pipelines, Some(base), &tasks) {
        Ok(pending) => pending,
        Err(err) => return outcome("submission refused", format!("{err:#}")),
    };
    // Unlike `queue add`, the screen queues the rest of the batch: the
    // queued summary lists what it left out and why. `validate_batch` parses
    // one task per entry of `tasks`, in order, so the two are filtered
    // together and stay index-aligned for `open_and_prefix`.
    let not_queued = not_queued(repo, &pending);
    let mut pending = pending;
    carry_group_descriptions(&mut pending, &not_queued);
    let (tasks, pending): (Vec<_>, Vec<_>) = tasks
        .into_iter()
        .zip(pending)
        .filter(|(_, task)| !not_queued.iter().any(|n| n.id == task.id()))
        .unzip();
    if pending.is_empty() {
        return queued_panel(&[], &[], &not_queued, dispatcher_line(repo), None);
    }
    if let Some(gate) = tool_gate(repo, tracking, Resume::Selection) {
        return gate;
    }
    if let Some(question) = issue_question(repo, tracking, &pending, Resume::Selection) {
        return question;
    }
    let ids: Vec<String> = pending.iter().map(|task| task.id().to_string()).collect();
    let mut tickets = PopupTickets::new(&pending, redraw);
    let selected = state.selected.clone();
    let left_out: Vec<String> = not_queued.iter().map(|n| n.id.clone()).collect();
    let submit = Submit {
        tasks: &tasks,
        base,
        selected: &selected,
        left_out: &left_out,
        tracking_off: tracking == Tracking::Off,
    };
    match finish_submit(repo, groups, pending, &submit, &mut tickets) {
        Ok(_msg) => {
            state.selected.clear();
            state.gates.clear();
            clamp_cursors(groups, state);
            queued_panel(
                &ids,
                &tickets.rows,
                &not_queued,
                dispatcher_line(repo),
                None,
            )
        }
        Err(err) => outcome("queue refused", format!("{err:#}")),
    }
}

/// What [`finish_submit`] writes beside the batch itself: the tasks it
/// came from, the branch it is based on, the groups it was selected as, the
/// ids of the selected tasks it left out — see [`not_queued`] — and whether
/// the tool-requirements gate or the issue question switched issue tracking
/// off for it.
struct Submit<'a> {
    tasks: &'a [(String, String)],
    base: &'a str,
    selected: &'a std::collections::BTreeSet<GroupKey>,
    left_out: &'a [String],
    tracking_off: bool,
}

/// Open the batch's tickets and name it, save it, clear the tasks it
/// came from out of the pending directory, and build the per-task report
/// this function's own return value carries — read back by a test directly
/// rather than by any caller here: a landed batch's popup names the tasks
/// it queued, the tickets opened for them and whether a dispatcher is up,
/// and nothing of this report — see [`queued_panel`].
///
/// The order is the whole guarantee behind "a submission that fails
/// validation removes nothing". Nothing is deleted until every task file has
/// been written, so a batch refused for any reason — a reserved key, an
/// unknown `depends_on`, a cycle among the selection, a hook that failed —
/// leaves the pending directory exactly as it found it, save for the ids a
/// failed hook call had already secured, written back so the next `enter`
/// resumes (see [`open_and_prefix`]).
///
/// Only the selected groups' own tasks go, and only the ones this batch
/// actually queued: a sibling [`selected_tasks`] left out because it was
/// already [`TaskState::Queued`] or [`TaskState::Done`] keeps its own file —
/// deleting it would be putting one task back by erasing another one's place
/// in the queue or the archive. Another group's tasks sit in the same
/// flat directory and are not this submission's to touch either, so the
/// deletion walks the groups it was handed rather than the directory.
fn finish_submit(
    repo: &Repo,
    groups: &mut Vec<Group>,
    mut pending: Vec<Task>,
    submit: &Submit,
    log: &mut dyn TicketLog,
) -> Result<String> {
    let Submit {
        tasks,
        base,
        selected,
        left_out,
        tracking_off,
    } = *submit;
    // `validate_batch` already ran this over the same batch, and nothing
    // between there and here mutates it any more, so today this is a repeat
    // of a check `pending` already passed. Kept anyway as this function's own
    // guarantee rather than a borrowed one: whatever calls `finish_submit`
    // next, with whatever batch, saves nothing without this check standing
    // between it and disk.
    check_dependencies_set(repo, &mut pending)?;
    // The screen asked the tool-requirements gate and the issue question
    // already, in its own popups — see `begin_submission` — so they are
    // answered here, not printed.
    let task_files = readable_task_files(tasks);
    let gate = ToolGate::Answered { tracking_off };
    open_and_prefix(repo, tasks, &task_files, &mut pending, gate, log)?;

    for task in &pending {
        task.save()?;
    }

    // Past this point the queue holds the work, so a failure to unlink is
    // not a reason to refuse a submission that has already landed: the
    // task is left where it is, and the group it belongs to drops off
    // the pane anyway, because the queue now holds every task it names.
    //
    // A task the batch left out keeps its file, and its group keeps its
    // place in the pane, so the person can set `starts_from:` and send it
    // again. Its siblings that did go are marked queued there until the
    // next reload reads them from the queue.
    let mut left_alone: Vec<(TaskState, String)> = Vec::new();
    for group in selected_groups(groups, selected) {
        for task in &group.tasks {
            if left_out.contains(&task.id) {
                continue;
            }
            if task.state == TaskState::Pending {
                let _ = std::fs::remove_file(&task.path);
            } else {
                left_alone.push((task.state, task.id.clone()));
            }
        }
    }
    groups.retain_mut(|group| {
        if !selected.contains(&group_key(group)) {
            return true;
        }
        if !group.tasks.iter().any(|t| left_out.contains(&t.id)) {
            return false;
        }
        for task in &mut group.tasks {
            if task.state == TaskState::Pending && !left_out.contains(&task.id) {
                task.state = TaskState::Queued;
            }
        }
        true
    });

    // The same report `queue add --from` prints for the same batch, so a
    // person reading one has read the other. The branch comes last, because
    // it comes from the checkout this was run in rather than from anything
    // in a task, and somebody on the wrong branch has no other way to
    // find that out.
    let mut msg = String::new();
    for task in &pending {
        msg.push_str(&format!(
            "queued {} at `{}`\n  {}\n",
            task.id(),
            crate::pipeline::QUEUED,
            task.path.display()
        ));
    }
    msg.push_str(&based_on_note(&pending, base));
    msg.push_str(&left_alone_note(&left_alone));
    Ok(msg)
}

/// One line per [`TaskState`] a submission's selected groups still hold once
/// their pending tasks are the ones actually queued — the line the mockup
/// draws under `based on`, naming a sibling rather than silently leaving it
/// out of the report. Empty when every selected task was pending, which is
/// the ordinary, wholly-fresh group.
fn left_alone_note(left_alone: &[(TaskState, String)]) -> String {
    let mut by_state: Vec<(TaskState, Vec<&str>)> = Vec::new();
    for (state, id) in left_alone {
        match by_state.iter_mut().find(|(s, _)| *s == *state) {
            Some((_, ids)) => ids.push(id),
            None => by_state.push((*state, vec![id])),
        }
    }
    by_state
        .into_iter()
        .map(|(state, ids)| {
            let label = match state {
                TaskState::Queued => "already in the queue",
                TaskState::Done => "already archived",
                TaskState::Pending => {
                    unreachable!("a pending task is queued, not left alone")
                }
            };
            format!("\n\n{label}, left alone: {}", ids.join(", "))
        })
        .collect()
}

/// Every typed [`crate::task::Frontmatter`] field a task's author may set
/// — the allowlist [`reset_for_reuse`] keeps. Everything else typed is
/// spoolway's own stamp, dropped on reset the same way `parse_submission`
/// already overwrites it on the way in; the difference here is that this
/// runs *before* a reserved key would refuse the task at all.
const AUTHORED_FIELDS: &[&str] = &[
    "id",
    "title",
    "depends_on",
    "pipeline",
    "group",
    "source",
    "plan",
    "gate_at",
];

/// Passthrough keys that still have to go, despite landing in
/// [`crate::task::Frontmatter`]'s own untyped `extra` map the same as a
/// project's real metadata does: all four are a hook's own answer for the
/// *finished* run. A task that kept `epic:` or `ticket:` into a fresh
/// submission would point the new run at the old run's ticket — `queue add`
/// reports such a task `kept` and never calls the `open` hook for it, so
/// the new run gets no ticket of its own either. `slug:` and `url:` are the
/// machine-written pair from the same answer: a kept `slug:` would pin the
/// new run's group prefix to the old issue's key, and a kept `url:` would
/// address the old issue. See [`crate::task::Task::extra_str`] and
/// `set_extra_str` for how they are carried.
const DROPPED_PASSTHROUGH_FIELDS: &[&str] = &["epic", "ticket", "slug", "url"];

/// A task's text with its frontmatter reduced to what its author owns —
/// [`AUTHORED_FIELDS`], plus any key that is not a typed `Frontmatter` field
/// at all, so a project's own metadata keeps passing through. The body
/// travels byte for byte; only the frontmatter mapping is rebuilt, in its own
/// original key order, so a task already free of stamped keys reads back
/// unchanged.
///
/// The untyped set is read off `Frontmatter` itself rather than a copied
/// list of every stamped field's name: the task is deserialised into a
/// real `Frontmatter` — with the same placeholder `stage:` `parse_submission`
/// inserts, since that field alone has no serde default — and a key survives
/// only if it is in [`AUTHORED_FIELDS`] or it landed in the deserialised
/// `#[serde(flatten)] extra` map, meaning serde found no typed slot for it at
/// all. This is what makes it a real allowlist: a field added to
/// `Frontmatter` tomorrow and left off `AUTHORED_FIELDS` lands in a typed
/// slot, not `extra`, and is dropped by construction — there is no second
/// list of stamped names to forget to update alongside it.
///
/// This is where `s` and `t` make a task handed to them from `queue/` or
/// `archive/` — carrying every key spoolway stamped on its earlier run —
/// safe to hand to `save_routine` and `build_trial_arm`: both go on to call
/// `parse_submission`, which rightly refuses a task that sets a reserved
/// key like `stage:`, and the reset is what makes that task's *reuse*
/// look like the task a producer would have written for a fresh run in
/// the first place. The file on disk this task was read from is never
/// touched — this only ever returns new text.
pub(crate) fn reset_for_reuse(name: &str, doc: &str) -> Result<String> {
    let (yaml, body) =
        crate::task::split_fence(doc).with_context(|| format!("{name}: not a task"))?;
    let value: serde_norway::Value = serde_norway::from_str(yaml)
        .with_context(|| format!("{name}: frontmatter is not valid YAML"))?;
    let mapping = value
        .as_mapping()
        .with_context(|| format!("{name}: frontmatter is not a mapping"))?
        .clone();

    // `Frontmatter::stage` is the one field with no serde default, so a
    // task missing it — everything off `queue/` or `archive/` sets it,
    // but nothing requires that of a hand-edited one — has to get the same
    // placeholder `parse_submission` stamps in before this can deserialise
    // at all. The placeholder is never read back: only which keys landed in
    // `extra` matters below, and `stage` never does.
    let mut typed_probe = mapping.clone();
    typed_probe.insert(
        serde_norway::Value::String("stage".to_string()),
        serde_norway::Value::String(crate::pipeline::QUEUED.to_string()),
    );
    let front: crate::task::Frontmatter =
        serde_norway::from_value(serde_norway::Value::Mapping(typed_probe))
            .with_context(|| format!("{name}: frontmatter is not valid task YAML"))?;

    let mut kept = serde_norway::Mapping::new();
    for (key, val) in &mapping {
        let Some(key_str) = key.as_str() else {
            continue;
        };
        let keep = AUTHORED_FIELDS.contains(&key_str)
            || (front.extra.contains_key(key_str)
                && !DROPPED_PASSTHROUGH_FIELDS.contains(&key_str));
        if keep {
            kept.insert(key.clone(), val.clone());
        }
    }

    let yaml = serde_norway::to_string(&serde_norway::Value::Mapping(kept))
        .with_context(|| format!("{name}: re-serialising the reset frontmatter"))?;
    Ok(format!("---\n{yaml}---\n{body}"))
}

/// The ticked pipeline an arm runs under, written into its task before
/// [`parse_submission`] is given it.
///
/// Every task of the group runs under every ticked pipeline, whatever its
/// own `pipeline:` said, and a task may name none at all. But
/// `parse_submission` refuses a task with no `pipeline:`, and it is handed
/// the *source* task — so without this a group holding such a task could
/// never be tried, the whole batch abandoned with `trial refused:` and
/// nothing minted. Stamping `front.pipeline` on the arm afterwards cannot
/// save it: the refusal has already happened by then.
///
/// Written over whatever the task said rather than only filled in when it
/// is blank, because each copy of the group runs under its own ticked
/// pipeline — the tick is the authority here, which is the same order of
/// precedence `front.pipeline` is stamped in below.
fn with_trial_pipeline(name: &str, doc: &str, pipeline: &str) -> Result<String> {
    let (yaml, body) =
        crate::task::split_fence(doc).with_context(|| format!("{name}: not a task"))?;
    let value: serde_norway::Value = serde_norway::from_str(yaml)
        .with_context(|| format!("{name}: frontmatter is not valid YAML"))?;
    let mut mapping = value
        .as_mapping()
        .with_context(|| format!("{name}: frontmatter is not a mapping"))?
        .clone();
    mapping.insert(
        serde_norway::Value::String("pipeline".to_string()),
        serde_norway::Value::String(pipeline.to_string()),
    );
    let yaml = serde_norway::to_string(&serde_norway::Value::Mapping(mapping))
        .with_context(|| format!("{name}: re-serialising the frontmatter"))?;
    Ok(format!("---\n{yaml}---\n{body}"))
}

/// What a trial stamps on every arm of one copy of the group, beside the
/// arm's own id, pipeline and skips: the trial the whole batch shares, the
/// group a person tried, and the group this copy runs in.
struct TrialStamp<'a> {
    trial: &'a str,
    source_group: &'a str,
    group: &'a str,
}

/// One trial arm: the source task parsed exactly as `queue add --from`
/// would, with what a trial names for the task itself stamped on
/// afterwards — the id spoolway minted, the [`TrialStamp`] its copy shares,
/// the pipeline this arm runs, and the ticked steps that pipeline is asked
/// to walk past.
///
/// `parse_submission` always resets `front.skip` to empty, since a task
/// may not set it itself (see that function's own comment); this is the one
/// caller allowed to put it back; here it is spoolway naming the task, not
/// the task. `front.trial` and `front.trial_group` are reserved the same
/// way. `front.branch` is recomputed too, after the id changes —
/// `parse_submission` already built one, but off the task's own bare
/// id, before this ever had a minted one to use.
///
/// `doc` is reset with [`reset_for_reuse`] before it is parsed, so a task
/// that came from `queue/` or `archive/` — carrying `stage:` and every other
/// key an earlier run stamped on it — reaches `parse_submission` looking like
/// a task a producer wrote for a fresh run, rather than being refused for
/// setting a reserved key spoolway itself put there.
///
/// The ticked pipeline goes into that task *before* it is parsed, by
/// [`with_trial_pipeline`], and not only onto the arm afterwards — see that
/// function for why stamping `front.pipeline` below is too late on its own.
fn build_trial_arm(
    name: &str,
    doc: &str,
    base: &str,
    id: &str,
    stamp: &TrialStamp,
    pipeline: &crate::pipeline::Pipeline,
    skip: &std::collections::BTreeSet<String>,
) -> Result<Task> {
    let doc = reset_for_reuse(name, doc)?;
    let doc = with_trial_pipeline(name, &doc, &pipeline.name)?;
    let mut arm = parse_submission(name, &doc, Some(base))?;
    arm.front.id = id.to_string();
    arm.front.branch = Some(format!("task/{id}"));
    arm.front.trial = Some(stamp.trial.to_string());
    arm.front.trial_group = Some(stamp.source_group.to_string());
    arm.front.group = Some(stamp.group.to_string());
    arm.front.pipeline = Some(pipeline.name.clone());
    // Only the ticked steps this pipeline actually has: `spoolway doctor`
    // refuses a `skip:` naming a step its own pipeline lacks
    // (`src/commands/pipeline.rs`). Each pipeline keeps its own skip set
    // already, so this filter only bites on a set a reload left naming a
    // step the pipeline has since dropped.
    arm.front.skip = skip
        .iter()
        .filter(|step| pipeline.step(step).is_some())
        .cloned()
        .collect();
    arm.path = std::path::PathBuf::from(format!("{id}.md"));
    Ok(arm)
}

/// Every `group:` the queue or the archive already holds — what a trial's
/// own copies are named around, so a copy never lands in a group an earlier
/// run still owns. Read both verbatim and through [`bare_group`], so a
/// group a hook's slug prefixed still counts as the name it was given.
fn groups_on_disk(repo: &Repo) -> Result<std::collections::BTreeSet<String>> {
    let mut tasks = repo.tasks()?;
    let (archived, _) = crate::task::load_dir(&repo.archive_dir())?;
    tasks.extend(archived);
    Ok(tasks
        .iter()
        .flat_map(|task| [task.front.group.clone(), bare_group(repo, task)])
        .flatten()
        .collect())
}

/// The group one copy of a trial runs in: `<group>-<pipeline>`, or the
/// lowest `<group>-<pipeline>-N` from 2 up that neither `on_disk` nor an
/// earlier copy of the same trial (`claimed`) already holds — the same way
/// [`mint_id`] numbers around an id that is taken. Starts at 2 because the
/// bare name is the first copy's; a `-1` would read as a task id's suffix.
fn mint_trial_group(
    group: &str,
    pipeline: &str,
    on_disk: &std::collections::BTreeSet<String>,
    claimed: &std::collections::BTreeSet<String>,
) -> String {
    let base = format!("{group}-{pipeline}");
    let free = |name: &String| !on_disk.contains(name) && !claimed.contains(name);
    if free(&base) {
        return base;
    }
    (2usize..)
        .map(|n| format!("{base}-{n}"))
        .find(free)
        .expect("an unbounded range always reaches a free name")
}

/// `enter` on the trial picker's second screen: one full copy of the group
/// per ticked pipeline, each copy in a group of its own (see
/// [`mint_trial_group`]), every task in it minted its own id and built on
/// that pipeline and that pipeline's skip set — and the whole set written,
/// or refused with nothing touched. Mirrors [`begin_submission`], but over a
/// group forked whole rather than chosen piece by piece, and the source
/// tasks are never deleted: they were templates for the arms, not
/// themselves submitted, and stay in whichever directory `t` found them in
/// exactly as it found them.
///
/// Each copy gets its own group because a group must be one chain: two
/// copies under one name would be two roots, and separate groups also let
/// the copies run side by side rather than one after the other.
fn begin_trial(
    repo: &Repo,
    pipelines: &Pipelines,
    base: &str,
    groups: &[Group],
    trial: &TrialState,
) -> Mode {
    let Some(group) = trial_group(groups, trial) else {
        return Mode::Browsing;
    };
    let ticked = trial_ticked(pipelines, trial);
    if group.tasks.is_empty() || ticked.is_empty() {
        return Mode::Browsing;
    }

    // Minted fresh, not the group's own name or any one task's — two
    // trials of the same group must read apart in the ledger, since
    // `spoolway eval --by task --trial <id>` finds a trial's arms by this
    // value, one row per run, and a reused value would silently
    // fold two unrelated batches into one comparison table.
    let trial_id = crate::usage::new_trial_id();
    let on_disk = match groups_on_disk(repo) {
        Ok(on_disk) => on_disk,
        Err(err) => return outcome("trial refused", format!("{err:#}")),
    };

    let mut minted: std::collections::BTreeSet<String> = Default::default();
    let mut claimed: std::collections::BTreeSet<String> = Default::default();
    let mut arms = Vec::with_capacity(group.tasks.len() * ticked.len());

    // Pipelines outside, tasks inside: every task's arms are numbered in
    // the order the pipelines are listed, so `alpha-1` and `beta-1` are the
    // first pipeline's copy and `alpha-2`, `beta-2` the second's.
    for (pipeline_name, pipeline) in ticked {
        let copy_group = mint_trial_group(&group.name, pipeline_name, &on_disk, &claimed);
        claimed.insert(copy_group.clone());
        let stamp = TrialStamp {
            trial: &trial_id,
            source_group: &group.name,
            group: &copy_group,
        };
        let skip = trial.skip.get(pipeline_name).cloned().unwrap_or_default();
        // Per copy, not per trial: a dependent in this copy waits on its
        // predecessor in this same copy, never on another pipeline's arm.
        let mut id_map: std::collections::BTreeMap<String, String> = Default::default();

        // `group.tasks` lists dependencies before dependents (see `Group`'s
        // own doc comment), so by the time a dependent task is minted here,
        // every group member it could `depends_on` already has its own entry
        // in `id_map` — one forward pass is enough to remap the whole chain.
        for task in &group.tasks {
            // A task id becomes a branch and a file name — the same check an
            // ordinary submission runs in `validate_batch`.
            let id = mint_id(repo, &task.id, &minted);
            if let Err(err) = crate::mux::check_task_id(&id) {
                return outcome("trial refused", format!("{err:#}"));
            }
            minted.insert(id.clone());
            id_map.insert(task.id.clone(), id.clone());

            let text = match task.text() {
                Ok(text) => text,
                Err(err) => {
                    return outcome(
                        "trial refused",
                        format!("reading {}: {err}", task.path.display()),
                    );
                }
            };
            let mut arm = match build_trial_arm(
                &task.path.display().to_string(),
                &text,
                base,
                &id,
                &stamp,
                pipeline,
                &skip,
            ) {
                Ok(arm) => arm,
                Err(err) => {
                    return outcome("trial refused", format!("{err:#}"));
                }
            };

            // Every dependency this copy also forks maps to that
            // predecessor's own minted id — the bare id it named is never
            // itself queued by a trial (see `build_trial_arm`), so this is
            // what keeps the chain the source group named intact inside the
            // copy. A dependency outside the group is left exactly as it
            // read, unless this task itself is already archived: `retain`
            // sweeps a finished predecessor out of `archive/`, so a stale
            // reference an archived task still carries cannot be trusted to
            // resolve, and is dropped instead — the same emptying a lone
            // archived fork always made, generalised from "the one task this
            // forked" to "this task, whichever one of the group it is".
            arm.front.depends_on = arm
                .front
                .depends_on
                .iter()
                .filter_map(|dep| match id_map.get(dep) {
                    Some(mapped) => Some(mapped.clone()),
                    None if task.state == TaskState::Done => None,
                    None => Some(dep.clone()),
                })
                .collect();

            arm.path = repo.queue_dir().join(format!("{id}.md"));
            arms.push(arm);
        }
    }

    match finish_trial(repo, arms) {
        Ok(()) => Mode::Browsing,
        Err(err) => outcome("trial refused", format!("{err:#}")),
    }
}

/// Validate the minted arms as one set and save them — or none, on any
/// failure. Mirrors [`finish_submit`]'s own all-or-nothing write, minus the
/// pending-directory deletion that function makes: a trial's source
/// tasks are never among the tasks being written.
fn finish_trial(repo: &Repo, mut arms: Vec<Task>) -> Result<()> {
    check_dependencies_set(repo, &mut arms)?;
    for arm in &arms {
        arm.save()?;
    }
    Ok(())
}

// ============================= The routines pane ===========================

/// `s`'s own write: copy `group`'s tasks into `.spoolway/routines/<name>/`,
/// each reset with [`reset_for_reuse`] so a task that came from `queue/`
/// or `archive/` saves the way a producer would have written it fresh —
/// refusing a folder that already holds any, the one non-goal this whole
/// feature draws a hard line at, since resolving a merge is a person's call
/// this screen has no way to ask for.
fn save_routine(repo: &Repo, groups: &[Group], group: &GroupKey, name: &str) -> Mode {
    let Some(group) = groups.iter().find(|g| &group_key(g) == group) else {
        return Mode::Browsing;
    };
    // The name is typed, so it is refused before it is joined onto
    // anything: `join` on a `..` or an absolute path walks straight out of
    // the routines directory, and a name carrying a separator would write a
    // routine somewhere the routines tab never reads it back from. One plain
    // folder name, and nothing else.
    if std::path::Path::new(name).components().count() != 1
        || !matches!(
            std::path::Path::new(name).components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return outcome(
            "not saved",
            format!("`{name}` is not a folder name — one plain name, no `/` and no `..`"),
        );
    }
    let dir = repo.routines_dir().join(name);

    match std::fs::read_dir(&dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return outcome(
                    "not saved",
                    format!(
                        "`{}` already holds tasks — pick another name, or clear it first",
                        dir.display()
                    ),
                );
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return outcome(
                "not saved",
                format!("s: reading {}: {err:#}", dir.display()),
            );
        }
    }

    if let Err(err) = std::fs::create_dir_all(&dir) {
        return outcome(
            "not saved",
            format!("s: creating {}: {err:#}", dir.display()),
        );
    }
    for task in &group.tasks {
        let path = dir.join(format!("{}.md", task.id));
        let text = match task.text() {
            Ok(text) => text,
            Err(err) => {
                return outcome(
                    "not saved",
                    format!("s: reading {}: {err}", task.path.display()),
                );
            }
        };
        let reset = match reset_for_reuse(&task.path.display().to_string(), &text) {
            Ok(reset) => reset,
            Err(err) => {
                return outcome(
                    "not saved",
                    format!("s: resetting {}: {err:#}", task.path.display()),
                );
            }
        };
        if let Err(err) = std::fs::write(&path, &reset) {
            return outcome(
                "not saved",
                format!("s: writing {}: {err:#}", path.display()),
            );
        }
    }

    // Named from the checkout, as the save panel names it and step 8 of the
    // screen's mockup draws it: the absolute path is the same for every
    // routine a person saves, and only pushes the part they chose off the
    // popup's right edge.
    outcome(
        "saved",
        format!(
            "saved {} to {}",
            plural(group.tasks.len(), "task"),
            crate::platform::relative(&repo.checkout, &dir)
        ),
    )
}

/// What `x`'s popup asked about, fixed at the moment `x` was pressed: the
/// routine's folder, and every job pointing into it with the store it lives
/// in. `enter` deletes exactly what the popup named, never a list read
/// again behind the person's back.
#[derive(Debug, Clone)]
struct RoutineDelete {
    name: String,
    path: std::path::PathBuf,
    tasks: usize,
    jobs: Vec<(String, crate::jobs::Scope)>,
}

/// Every job in `jobs` whose resolved path is `folder` or anything inside
/// it — a whole-folder job and a single-task one alike, since either fails
/// on every firing once the folder is gone. The match is by path component,
/// so `nightly` never claims a job on `nightly-extra`. A job whose path
/// escapes the routines directory resolves to nothing and points nowhere,
/// so it is left alone for `spoolway doctor` to name.
fn jobs_into<'a>(
    repo: &Repo,
    jobs: &'a [crate::jobs::Job],
    folder: &std::path::Path,
) -> Vec<&'a crate::jobs::Job> {
    jobs.iter()
        .filter(|job| job.target(repo).is_ok_and(|path| path.starts_with(folder)))
        .collect()
}

/// `x` over the routine list: the popup naming the highlighted routine and
/// the jobs [`jobs_into`] finds for it.
///
/// A job store that will not read hides which jobs point in, so the delete
/// is refused here, before anything is removed, rather than leave a job
/// firing at a folder that is gone.
fn begin_routine_delete(repo: &Repo, routines: &[RoutineFolder], nav: &RoutineNav) -> Mode {
    let Some(folder) = highlighted_routine_folder(routines, nav) else {
        return Mode::Routines(nav.clone());
    };
    let jobs = match crate::jobs::load(repo) {
        Ok(jobs) => jobs,
        Err(err) => {
            return outcome_over(
                Some(nav),
                "routine not deleted",
                format!(
                    "{err:#}\n\nthe jobs pointing into {} cannot be told apart until this \
                     store reads, so nothing was removed. `spoolway doctor` names what is wrong.",
                    folder.name
                ),
            );
        }
    };
    let target = RoutineDelete {
        name: folder.name.clone(),
        path: folder.path.clone(),
        tasks: folder.tasks.len(),
        jobs: jobs_into(repo, &jobs, &folder.path)
            .into_iter()
            .map(|job| (job.name.clone(), job.scope))
            .collect(),
    };
    Mode::DeleteRoutine {
        nav: nav.clone(),
        target,
    }
}

/// `x`'s popup: the routine, what deleting it means, and the jobs that go
/// with it, each beside its store. The jobs lines are left out when there
/// are none.
fn delete_routine_panel(target: &RoutineDelete) -> Vec<String> {
    let mut body = vec![
        String::new(),
        format!("  {} · {}", target.name, plural(target.tasks, "task")),
        "  its folder is removed from disk,".to_string(),
        "  and only git can bring it back.".to_string(),
    ];
    if !target.jobs.is_empty() {
        // Names padded to the longest one, three spaces short of the store,
        // so the `user`/`project` column lines up as the mockup draws it.
        let width = target
            .jobs
            .iter()
            .map(|(name, _)| name.chars().count())
            .max()
            .unwrap_or(0);
        body.push(String::new());
        body.push("  these jobs are deleted with it:".to_string());
        for (name, scope) in &target.jobs {
            body.push(format!("    {}   {}", pad_to(name, width), scope.label()));
        }
    }
    panel("delete this routine", &body, &keys(DELETE_ROUTINE_KEYS))
}

/// `enter` on `x`'s popup: every job the popup named, then the folder and
/// everything under it — in that order, so a failure part way leaves at
/// worst a routine nothing fires, never a job firing at a folder that is
/// gone. Nothing is staged: the folder leaves the working tree, and git is
/// the way back, the same as [`save_routine`] writes without touching git.
///
/// Both stores are read again first, and a store broken since `x` refuses
/// the delete with nothing removed, the same refusal `x` itself gives. Each
/// job is then removed from its own store alone, so a failure is only ever
/// that job's, and the jobs listed before it really are gone.
///
/// A failure answers the text of the popup that says so: the error, what
/// was already removed and what is left.
fn delete_routine(repo: &Repo, target: &RoutineDelete) -> std::result::Result<(), String> {
    let folder = format!(
        "folder {}",
        crate::platform::relative(&repo.checkout, &target.path)
    );
    let job_line =
        |(name, scope): &(String, crate::jobs::Scope)| format!("job {name} ({})", scope.label());
    if let Err(err) = crate::jobs::load(repo) {
        let err = err.context("nothing was removed — `spoolway doctor` names what is wrong");
        let left: Vec<String> = target
            .jobs
            .iter()
            .map(job_line)
            .chain([folder.clone()])
            .collect();
        return Err(partial_delete(&err, &[], &left));
    }
    for (done, job) in target.jobs.iter().enumerate() {
        if let Err(err) = crate::jobs::delete_in(repo, job.1, &job.0) {
            let removed: Vec<String> = target.jobs[..done].iter().map(job_line).collect();
            let left: Vec<String> = target.jobs[done..]
                .iter()
                .map(job_line)
                .chain([folder.clone()])
                .collect();
            return Err(partial_delete(&err, &removed, &left));
        }
    }
    match std::fs::remove_dir_all(&target.path) {
        Ok(()) => Ok(()),
        // Gone already, by another hand between `x` and `enter`: the disk
        // already says what this was asked to make it say.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => {
            let err =
                anyhow::Error::new(err).context(format!("removing {}", target.path.display()));
            let removed: Vec<String> = target.jobs.iter().map(job_line).collect();
            Err(partial_delete(&err, &removed, &[folder]))
        }
    }
}

/// The text of the popup a delete stopped part way through opens: the
/// error, then what is gone and what is still there, so a person knows what
/// to finish by hand.
fn partial_delete(err: &anyhow::Error, removed: &[String], left: &[String]) -> String {
    let list = |lines: &[String]| match lines.is_empty() {
        true => "  nothing".to_string(),
        false => lines
            .iter()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    format!(
        "{err:#}\n\nremoved:\n{}\n\nleft:\n{}",
        list(removed),
        list(left)
    )
}

/// The routines pane after a delete: `routines` read again so the gone
/// folder drops out, the cursor pulled back inside what is left, and the
/// tick set cut to folders still listed. Answers the popup to open, if the
/// delete or the read that follows it failed.
fn after_routine_delete(
    repo: &Repo,
    routines: &mut Vec<RoutineFolder>,
    mut nav: RoutineNav,
    result: std::result::Result<(), String>,
) -> Mode {
    let reread = super::routines::list_routines(repo);
    let reread_err = match reread {
        Ok(fresh) => {
            *routines = fresh;
            None
        }
        // The list stays as it was, minus any folder no longer on disk, so
        // the deleted routine is not drawn as if it were still there.
        Err(err) => {
            routines.retain(|folder| folder.path.is_dir());
            Some(err)
        }
    };
    nav.selected
        .retain(|path| routines.iter().any(|folder| &folder.path == path));
    nav.folder_cursor = nav.folder_cursor.min(routines.len().saturating_sub(1));
    nav.task_cursor = 0;
    nav.focus = Focus::Groups;
    match (result, reread_err) {
        (Err(text), _) => outcome_over(Some(&nav), "routine not fully deleted", text),
        (Ok(()), Some(err)) => outcome_over(
            Some(&nav),
            "routine deleted",
            format!("the routine list would not read again: {err:#}"),
        ),
        (Ok(()), None) => Mode::Routines(nav),
    }
}

/// Every task in the ticked routines — each one's nested subfolders
/// included, since [`RoutineFolder::tasks`] already folds those in — each
/// minted a fresh id — never the bare one a routine's
/// own task carries, since a routine exists to be queued more than
/// once, and the second run would collide with the first at the id the
/// task itself always names. `group:` and the body travel unchanged;
/// only `id:` and any `depends_on:` naming a sibling in this same batch are
/// rewritten, to the same minted ids, so a chain saved together still
/// resolves once every id in it has changed.
///
/// Only top-level routines can be ticked, and no two of them share a task
/// file, so no task is ever gathered twice.
fn routine_batch_tasks(
    repo: &Repo,
    routines: &[RoutineFolder],
    nav: &RoutineNav,
) -> Vec<(String, String)> {
    let tasks: Vec<&RoutineTask> = routines
        .iter()
        .filter(|folder| nav.selected.contains(&folder.path))
        .flat_map(|folder| &folder.tasks)
        .collect();

    mint_routine_batch(repo, &tasks)
}

/// Mint a fresh id for every routine task and rewrite its `id:` line,
/// and any `depends_on:` naming a sibling in this same batch, onto the
/// minted ids — the transform the queue screen's `enter` and a scheduled
/// job both run once they have the list of tasks to queue. `tasks` is
/// already deduped and in the order the batch should keep. Nothing is
/// written: the returned `(source path, rewritten task)` pairs are the
/// shape [`validate_batch`] takes.
fn mint_routine_batch(repo: &Repo, tasks: &[&RoutineTask]) -> Vec<(String, String)> {
    let mut minted: std::collections::BTreeSet<String> = Default::default();
    let mut id_map: BTreeMap<String, String> = BTreeMap::new();
    for task in tasks {
        let id = mint_id(repo, &task.id, &minted);
        minted.insert(id.clone());
        id_map.insert(task.id.clone(), id);
    }

    tasks
        .iter()
        .map(|task| {
            let mut doc = with_frontmatter_field(&task.doc, "id", &id_map[&task.id]);

            let deps = super::pending::depends_on(&task.doc);
            if !deps.is_empty() {
                let mapped: Vec<String> = deps
                    .iter()
                    .map(|dep| id_map.get(dep).cloned().unwrap_or_else(|| dep.clone()))
                    .collect();
                doc =
                    with_frontmatter_field(&doc, "depends_on", &format!("[{}]", mapped.join(", ")));
            }

            (task.path.display().to_string(), doc)
        })
        .collect()
}

/// Queue a routine target the way the routines tab does, but driven by a job
/// rather than the screen's nav. `target` is an absolute path under
/// [`Repo::routines_dir`]: a folder queues every task at or below it as
/// one batch, exactly as `enter` does, and a single `.md` file queues that
/// task alone with its `depends_on` emptied, exactly as `space` does. Every
/// queued task is put on `pipeline` — a job names its own, where the `r`
/// pane leaves each task on whatever it carried. The batch goes through
/// [`validate_batch`] all-or-nothing and the saved tasks are handed back so
/// a caller can record which ids it minted. The source tasks under
/// `.spoolway/routines/` are never touched.
///
/// Asks the tool-requirements gate printed, as a caller with no screen does
/// — the dispatcher's scheduled firing is the only one.
pub(crate) fn queue_routine_target(
    repo: &Repo,
    pipelines: &Pipelines,
    base: &str,
    target: &std::path::Path,
    pipeline: &str,
) -> Result<Vec<Task>> {
    let mut submitted = if target.is_dir() {
        let folder = super::routines::read_folder_at(target)?;
        // `folder.tasks` is already this folder's own tasks plus every
        // nested subfolder's, depth-first — the same list `enter` queues.
        if folder.tasks.is_empty() {
            bail!("{} holds no tasks", target.display());
        }
        let tasks: Vec<&RoutineTask> = folder.tasks.iter().collect();
        mint_routine_batch(repo, &tasks)
    } else {
        let task = super::routines::read_task_at(target)?;
        let id = mint_id(repo, &task.id, &Default::default());
        let mut doc = with_frontmatter_field(&task.doc, "id", &id);
        // Emptied for the same reason `begin_routine_solo` empties it: a
        // lone task names no sibling in this batch, so a real
        // `depends_on` would be refused by `check_dependencies_set`.
        doc = with_frontmatter_field(&doc, "depends_on", "[]");
        vec![(task.path.display().to_string(), doc)]
    };

    for (_, doc) in &mut submitted {
        *doc = with_frontmatter_field(doc, "pipeline", pipeline);
    }

    let mut tasks = validate_batch(repo, pipelines, Some(base), &submitted)?;
    // No task to write ids back into — see `open_and_prefix`. The
    // dispatcher firing a job has no screen, so this passes
    // `ToolGate::Print`, which asks `crate::ask` whether anyone is really
    // there and takes its own terminal, the same way `queue_add_tasks`
    // does. `esc`'s `GateCancelled` is left to propagate as an ordinary
    // `Err` rather than caught here: `jobs::fire_due` treats any `Err` as
    // "did not fire" and neither marks the job fired nor records queued
    // ids, which is the one honest answer for a run a person actually
    // declined. Turning it into a fake empty success here would let the
    // dispatcher believe this minute's firing already happened.
    //
    // `submitted` is `&[]` — nothing here is ever written back into — but
    // `task_files` is not: a routine's own task is a real file under
    // `.spoolway/routines/`, safe for a hook to read, only never to write
    // to.
    let task_files = readable_task_files(&submitted);
    let gate = ToolGate::Print {
        interactive: crate::ask::interactive(),
    };
    open_and_prefix(
        repo,
        &[],
        &task_files,
        &mut tasks,
        gate,
        &mut PrintedTickets,
    )?;
    // All or none: everything above parsed and validated, so these writes
    // are the commit — the same discipline `queue_add_tasks` follows.
    for task in &tasks {
        task.save()?;
    }
    Ok(tasks)
}

/// Open the batch's tickets, save every task `validate_batch` handed back
/// — the whole of what queuing a routine does; the popup a landed batch
/// opens is built from the ids and the tickets `log` saw, not from a report
/// here. The source tasks under
/// `.spoolway/routines/` are never touched — a routine is meant to be
/// queued again, not consumed by being queued once — which is why nothing
/// is handed to [`open_and_prefix`] to write ids back into.
/// `task_files` still names each one's real path, for a hook's own
/// `SPOOLWAY_TASK_FILE` to point at — see [`open_and_prefix`]'s own doc
/// comment on why that is a different list from the empty `tasks`.
///
/// `tracking_off` is the answer of the tool-requirements gate or the issue
/// question, asked already in the screen's own popup — see
/// [`ToolGate::Answered`].
fn finish_routine(
    repo: &Repo,
    tasks: &mut [Task],
    task_files: &[String],
    tracking_off: bool,
    log: &mut dyn TicketLog,
) -> Result<()> {
    let gate = ToolGate::Answered { tracking_off };
    open_and_prefix(repo, &[], task_files, tasks, gate, log)?;
    for task in tasks.iter() {
        task.save()?;
    }
    Ok(())
}

/// `enter` over the routines pane's folders: mint every ticked folder's
/// tasks through [`routine_batch_tasks`] and queue them as one
/// batch through [`validate_batch`] — the same all-or-nothing write
/// [`begin_submission`] gives a pending selection.
///
/// A refusal is a popup over the routines pane, which closing it goes back
/// to; the tool-requirements gate and the issue question stop it first the
/// way they stop [`begin_submission`].
fn begin_routine_queue(
    repo: &Repo,
    pipelines: &Pipelines,
    base: &str,
    routines: &[RoutineFolder],
    nav: &RoutineNav,
    tracking: Tracking,
    redraw: &mut dyn FnMut(&[String]),
) -> Mode {
    // Nothing to queue leaves the pane as it stands: the routines tab has
    // no pending screen behind it to drop back to.
    let tasks = routine_batch_tasks(repo, routines, nav);
    if tasks.is_empty() {
        return Mode::Routines(nav.clone());
    }
    let task_files = readable_task_files(&tasks);
    finish_routine_mode(
        repo,
        pipelines,
        base,
        (&tasks, &task_files),
        nav,
        tracking,
        redraw,
    )
}

/// The end both routine submits share: validate `batch` — the tasks
/// and the task files behind them — stop at the tool-requirements gate and
/// the issue question when each has something to ask, and queue it. `nav`
/// is the routines pane it came from: a refusal is drawn over it, and the
/// gate and the question name it as the place `esc` goes back to. A landed
/// batch says what it queued over it too, and closing that goes back to it
/// with nothing ticked.
fn finish_routine_mode(
    repo: &Repo,
    pipelines: &Pipelines,
    base: &str,
    batch: (&[(String, String)], &[String]),
    nav: &RoutineNav,
    tracking: Tracking,
    redraw: &mut dyn FnMut(&[String]),
) -> Mode {
    let (tasks, task_files) = batch;
    let mut tasks = match validate_batch(repo, pipelines, Some(base), tasks) {
        Ok(tasks) => tasks,
        Err(err) => return outcome_over(Some(nav), "queue refused", format!("{err:#}")),
    };
    // Which of the two submits this was, for `enter` on the gate to run
    // again: a batch built from ticked folders, or one task on its own.
    let then = match nav.focus {
        Focus::Groups => Resume::Routines(nav.clone()),
        Focus::Tasks => Resume::RoutineTask(nav.clone()),
    };
    if let Some(gate) = tool_gate(repo, tracking, then.clone()) {
        return gate;
    }
    if let Some(question) = issue_question(repo, tracking, &tasks, then) {
        return question;
    }
    let ids: Vec<String> = tasks.iter().map(|task| task.id().to_string()).collect();
    let mut tickets = PopupTickets::new(&tasks, redraw);
    let tracking_off = tracking == Tracking::Off;
    match finish_routine(repo, &mut tasks, task_files, tracking_off, &mut tickets) {
        Ok(()) => {
            // Back onto the pane with nothing ticked: the batch is queued,
            // and a tick left standing is one `enter` away from queuing the
            // same routine a second time.
            let mut after = nav.clone();
            after.selected.clear();
            queued_panel(&ids, &tickets.rows, &[], dispatcher_line(repo), Some(after))
        }
        Err(err) => outcome_over(Some(nav), "queue refused", format!("{err:#}")),
    }
}

/// `space` over the routines pane's own tasks pane: queue the highlighted
/// task alone, under a minted id, with its `depends_on` emptied before
/// [`validate_batch`] ever sees it. Emptied here rather than left for
/// [`check_dependencies_set`] to refuse: a solo pick names no sibling in
/// this batch for a dependency on one to resolve against, and that check
/// bails on a `depends_on` naming anything outside the queue or the
/// archive.
fn begin_routine_solo(
    repo: &Repo,
    pipelines: &Pipelines,
    base: &str,
    routines: &[RoutineFolder],
    nav: &RoutineNav,
    tracking: Tracking,
    redraw: &mut dyn FnMut(&[String]),
) -> Mode {
    // No task under the cursor leaves the pane as it stands, the same as
    // `begin_routine_queue` with nothing ticked.
    let Some(folder) = highlighted_routine_folder(routines, nav) else {
        return Mode::Routines(nav.clone());
    };
    let Some(task) = folder.tasks.get(nav.task_cursor) else {
        return Mode::Routines(nav.clone());
    };

    let id = mint_id(repo, &task.id, &Default::default());
    let mut doc = with_frontmatter_field(&task.doc, "id", &id);
    doc = with_frontmatter_field(&doc, "depends_on", "[]");
    let tasks = vec![(task.path.display().to_string(), doc)];
    let task_files = readable_task_files(&tasks);
    finish_routine_mode(
        repo,
        pipelines,
        base,
        (&tasks, &task_files),
        nav,
        tracking,
        redraw,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::*;

    /// A minimal, non-empty body — the shape most of these tests only need
    /// to exist, not to say anything in particular.
    const BODY: &str = "## Goal\n\nDo the thing.\n";

    /// `queue list --json` reports `paused` for a gate-held row, key first
    /// in `next` and `resumable: true` — and, distinctly, `prompt` for a
    /// live lane herdr reads a permission prompt off: not resumable, and
    /// `next` names the pane rather than a command. The two used to collapse
    /// into the same `paused` state with `resumable: true` on both, back
    /// when a question-held pane was inferred from silence rather than read
    /// live off `LaneStatus::Blocked` — see `State::Prompt`'s own doc
    /// comment. Nothing emits the old `waiting_on_you` state label, and
    /// nothing emits `parked` either — that state left with the fields
    /// behind it.
    #[test]
    fn queue_json_tells_a_paused_gate_apart_from_a_live_prompt() {
        use crate::status::State;
        use crate::status::testutil::row;

        let mut gate = row("release-me");
        gate.state = State::Paused;
        gate.resumable = true;
        gate.next = "[r] → handover — `spoolway resume release-me`".into();

        let mut prompting = row("question");
        prompting.state = State::Prompt;
        prompting.resumable = false;
        prompting.next = "press a key in pane `question · implement`".into();

        let json: Vec<QueueRowJson> = [&gate, &prompting]
            .iter()
            .map(|r| QueueRowJson::from(*r))
            .collect();

        assert_eq!(json[0].state, "paused");
        assert!(json[0].resumable);
        assert_eq!(json[1].state, "prompt");
        assert!(!json[1].resumable);
        assert_eq!(json[1].next, "press a key in pane `question · implement`");

        let rendered = serde_json::to_string(&json[1]).unwrap();
        assert!(!rendered.contains("waiting_on_you"), "{rendered}");
        assert!(!rendered.contains("parked"), "{rendered}");
    }

    /// A task stranded behind a block is `"state": "queued"` like every
    /// other wait, and nothing was added to `--json` to carry what the old
    /// `unreachable` state used to say. The NEXT column is where a reader
    /// finds out *what* the wait is on, and `dependency_note` still writes
    /// it.
    #[test]
    fn queue_json_calls_a_stranded_task_queued_with_no_field_in_its_place() {
        use crate::status::testutil::{add, fixture};

        let (repo, _root_guard) = fixture("stranded-json");
        let pipelines = Pipelines::builtin();
        add(&repo, "search-typo", &[], Some(crate::pipeline::BLOCKED));
        add(
            &repo,
            "search-facets",
            &["search-typo"],
            Some(crate::pipeline::QUEUED),
        );

        let rows = crate::status::rows(&repo, &pipelines).unwrap();
        let row = rows.iter().find(|r| r.id == "search-facets").unwrap();
        let json = QueueRowJson::from(row);

        assert_eq!(json.state, "queued");
        // `next` now reads `waiting on: search-typo` — an ordinary wait
        // line, not the diagnosis `unreachable` used to spell out — so this
        // looks at the keys rather than the whole line: nothing was added to
        // say what the state stopped saying.
        let rendered = serde_json::to_value(&json).unwrap();
        let keys: Vec<&str> = rendered
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "group",
                "after",
                "stage",
                "pipeline",
                "state",
                "arrivals",
                "next",
                "resumable",
                "ctx_pct",
                "out_tokens",
                "cost_usd",
                "lane_time_s",
            ]
        );
    }

    #[test]
    fn strip_slug_prefix_removes_only_a_recognised_prefix() {
        assert_eq!(
            strip_slug_prefix("proj-12-auth-rework", "proj-12"),
            "auth-rework"
        );
        // No slug known, or the group does not carry it: left whole.
        assert_eq!(strip_slug_prefix("auth-rework", ""), "auth-rework");
        assert_eq!(strip_slug_prefix("auth-rework", "proj-12"), "auth-rework");
        // The slug must be followed by a hyphen to count as a prefix.
        assert_eq!(strip_slug_prefix("proj-12x", "proj-12"), "proj-12x");
    }

    /// A hook runs with its own current directory set to `repo.root`, not
    /// wherever `spoolway` itself happened to be started from — so
    /// `readable_task_files` must hand back an absolute path even for a
    /// task named relatively (`--from ../t.md`, or an entry
    /// `gather_tasks` joined onto a relative `--from <dir>`); a relative
    /// name would resolve against the wrong directory once the hook reads it
    /// (review round 2 finding 6). This does not switch the process's own
    /// working directory to prove it — that is global state shared by every
    /// test running in parallel — it only checks the shape of the answer:
    /// relative in, absolute out, matching what `canonicalize` itself
    /// resolves the same name to.
    ///
    /// The task deliberately does *not* come from [`crate::scratch::
    /// root`], unlike every other fixture in this crate: that helper only
    /// ever answers a path already absolute under the system temp
    /// directory, and this test's whole premise needs a name that starts
    /// out relative to `std::env::current_dir()` — the one shape
    /// `scratch::root` cannot produce (review round 3 finding 7).
    #[test]
    fn readable_task_files_resolves_a_relative_name_to_an_absolute_path() {
        let dir = std::path::PathBuf::from("target").join(format!(
            "task-files-relative-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let relative = dir.join("doc.md");
        std::fs::write(&relative, "hi").unwrap();

        let tasks = vec![(relative.display().to_string(), String::new())];
        let resolved = readable_task_files(&tasks);

        assert_eq!(resolved.len(), 1);
        assert!(
            std::path::Path::new(&resolved[0]).is_absolute(),
            "a relative task name must resolve to an absolute path: {resolved:?}"
        );
        assert_eq!(
            std::fs::canonicalize(&relative)
                .unwrap()
                .display()
                .to_string(),
            resolved[0]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A name naming nothing real — the `<stdin>#N` [`gather_tasks`]
    /// mints for a stream entry, chief among them — resolves to the empty
    /// string, the same as it did under the old `is_file` check.
    #[test]
    fn readable_task_files_is_empty_for_a_name_that_resolves_to_nothing() {
        let tasks = vec![("<stdin>#1".to_string(), String::new())];
        assert_eq!(readable_task_files(&tasks), vec![String::new()]);
    }

    #[test]
    fn is_absolute_http_url_accepts_only_an_absolute_web_address() {
        assert!(is_absolute_http_url(
            "https://acme.atlassian.net/browse/PROJ-12"
        ));
        assert!(is_absolute_http_url("http://localhost:8080/x"));
        assert!(is_absolute_http_url("https://host"));
        // The scheme is case-insensitive, and a plain port is fine.
        assert!(is_absolute_http_url("HTTPS://host/x"));
        assert!(is_absolute_http_url("Http://Example.com"));
        assert!(is_absolute_http_url("http://host:443/x"));
        // Userinfo is fine — the criterion asks only for an absolute http(s) URL.
        assert!(is_absolute_http_url("https://u:p@host:443/x"));
        // A well-formed bracketed IPv6 host, with and without a port.
        assert!(is_absolute_http_url("https://[::1]/"));
        assert!(is_absolute_http_url("https://[2001:db8::1]:8443/x"));

        assert!(!is_absolute_http_url("/browse/PROJ-12"));
        assert!(!is_absolute_http_url("ftp://acme/x"));
        assert!(!is_absolute_http_url("acme.atlassian.net/browse/PROJ-12"));
        // A scheme with no real host before the path, query or fragment.
        assert!(!is_absolute_http_url("https://"));
        assert!(!is_absolute_http_url("https://?x"));
        assert!(!is_absolute_http_url("https://#x"));
        assert!(!is_absolute_http_url("https:// "));
        assert!(!is_absolute_http_url("https://a b/c"));
        assert!(!is_absolute_http_url("https://:"));
        assert!(!is_absolute_http_url("https://:8080/x"));
        assert!(!is_absolute_http_url("https://@"));
        assert!(!is_absolute_http_url("https://@/path"));
        // Whitespace anywhere in the raw string — the standard would fold an
        // interior space into the userinfo and parse on.
        assert!(!is_absolute_http_url("https://user name@host"));
        assert!(!is_absolute_http_url("https://host /x"));
        // A malformed IPv6 literal, and out-of-range or non-numeric ports.
        assert!(!is_absolute_http_url("https://[]/"));
        assert!(!is_absolute_http_url("https://[abc]/"));
        assert!(!is_absolute_http_url("https://[::::]/"));
        assert!(!is_absolute_http_url("https://host:abc"));
        assert!(!is_absolute_http_url("https://host:99999/"));
        assert!(!is_absolute_http_url("https://host:123456"));
    }

    /// A whole task, in the shape `--from` accepts: `id:` plus
    /// whatever else `extra` puts in the frontmatter, then `body`. `pipeline:`
    /// is required now, so this fills in the built-in `default` pipeline
    /// unless `extra` already names one — a test after the unassigned shape
    /// itself builds its own task instead, bypassing this default.
    fn task_text(id: &str, extra: &str, body: &str) -> String {
        let pipeline = if extra.contains("pipeline:") {
            ""
        } else {
            "pipeline: default\n"
        };
        format!("---\nid: {id}\ntitle: {id}, done\n{pipeline}{extra}---\n{body}")
    }

    /// Create `branch` at the fixture's `HEAD`: a done dependency's branch
    /// that has not been deleted, which a dependent queued after it starts
    /// from — see `missing_start_branch`.
    fn start_branch_exists(repo: &Repo, branch: &str) {
        crate::repo::run(&repo.root, "git", &["branch", branch]).unwrap();
    }

    /// Write `text` under `repo.root` and hand back the path a `--from`
    /// entry would name.
    fn write_doc(repo: &Repo, name: &str, text: &str) -> String {
        let path = repo.root.join(name);
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    /// `--base` set to the fixture's own checkout branch — the ambient value
    /// every one of these tests relied on before a base had to be chosen —
    /// so a task under test can still leave `base:` out unless the test
    /// is about `base:` itself, which passes its own task with `base:`
    /// set and so overrides this anyway.
    fn from_args(paths: &[&str]) -> QueueAddArgs {
        QueueAddArgs {
            from: paths.iter().map(|p| p.to_string()).collect(),
            base: Some("plan/demo".to_string()),
            dry_run: false,
        }
    }

    /// A lane cannot mutate the queue for itself — `in_lane: true` is refused
    /// before anything is read from `--from`, naming why.
    #[test]
    fn queue_add_refuses_from_inside_a_lanes_own_environment() {
        let (repo, _root_guard) = fixture("queue-add-in-lane");
        let text = task_text("login", "", BODY);
        let path = write_doc(&repo, "login.md", &text);

        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            true,
        )
        .expect_err("a lane must not be able to mutate the queue for itself");
        let said = format!("{err:#}");
        assert!(said.contains("lane's own environment"), "{said}");
        assert!(
            !repo.queue_dir().join("login.md").exists(),
            "nothing should have been written"
        );
    }

    /// A task's base is what a task's own `base:` or `queue add --base`
    /// chose, never the branch of whichever checkout this was run in — a
    /// worktree on an entirely different branch changes nothing about the
    /// base a submission gets.
    #[test]
    fn a_task_is_based_on_the_flag_not_the_checkout_it_was_queued_in() {
        let (repo, _root_guard) = fixture("base-from-flag-not-cwd");
        let git = |dir: &Path, args: &[&str]| crate::repo::run(dir, "git", args).unwrap();
        git(&repo.root, &["config", "user.email", "t@example.com"]);
        git(&repo.root, &["config", "user.name", "t"]);
        git(&repo.root, &["commit", "-q", "--allow-empty", "-m", "root"]);

        let elsewhere = repo.root.join("worktrees").join("plan-b");
        git(
            &repo.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "plan/b",
                elsewhere.to_str().unwrap(),
            ],
        );

        let text = task_text("login", "group: b\n", BODY);
        let path = write_doc(&repo, "login.md", &text);
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &elsewhere,
            false,
        )
        .unwrap();

        assert_eq!(
            queued(&repo, "login").front.base.as_deref(),
            Some("plan/demo"),
            "the base is `--base`, never the branch of the checkout this ran in"
        );
        // And it arrived in the project's queue all the same: one queue, one
        // dispatcher, whichever worktree the work was queued from.
        assert!(repo.queue_dir().join("login.md").exists());

        git(
            &repo.root,
            &["worktree", "remove", "--force", elsewhere.to_str().unwrap()],
        );
    }

    /// A dependency only means anything if the work it waits for merges where
    /// this task will be cut from. Across two plan branches it never does.
    #[test]
    fn a_dependency_on_a_task_of_another_plan_branch_is_refused() {
        let (repo, _root_guard) = fixture("cross-base-dep");
        add(&repo, "login", &[]);

        let mut sessions = queued(&repo, "login");
        sessions.front.id = "sessions".into();
        sessions.front.depends_on = vec!["login".into()];
        sessions.front.base = Some("plan/other".into());
        let err = check_dependencies_set(&repo, &mut [sessions]).unwrap_err();

        assert!(
            err.to_string().contains("would never put it in reach"),
            "{err:#}"
        );
    }

    #[test]
    fn a_dependency_on_a_task_that_does_not_exist_is_refused() {
        let (repo, _root_guard) = fixture("unknown-dep");
        add(&repo, "login", &[]);

        let text = task_text("sessions", "group: demo\ndepends_on: [lgoin]\n", BODY);
        let path = write_doc(&repo, "sessions.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("`lgoin`"), "{err:#}");
        assert!(
            !repo.queue_dir().join("sessions.md").exists(),
            "a task that could never start should not have been written"
        );
    }

    /// `queue add` checks `depends_on` against the archive index: no task
    /// file is opened for it, and every accept and refuse answer is the one
    /// the files give — after archivings, a sweep and a rebuild. The cases
    /// cover what the check reads of an archived task: its group, its base
    /// and whether it was a trial arm.
    #[test]
    fn check_dependencies_set_answers_from_the_index_as_it_did_from_the_files() {
        use crate::archive_index::testutil::*;
        let (repo, _root_guard) = fixture("deps-from-index");
        archive(&repo, "login", "group: auth\nbase: plan/demo\n");
        archive(
            &repo,
            "arm",
            "group: auth-arm\ntrial: t1\nbase: plan/demo\n",
        );
        archive(&repo, "doomed", "group: gone\n");
        // Two groups that still have a live member, so their archived tasks
        // count. `chain` holds an archived trial arm beside its real root:
        // counting the arm would give the group a second root. `feat` holds
        // an archived root under a slug-prefixed group name that only
        // stripping the slug puts in the same group as the live root.
        archive(&repo, "chain-root", "group: chain\n");
        archive(&repo, "chain-arm", "group: chain\ntrial: t1\n");
        archive(&repo, "feat-root", "group: proj-1-feat\nslug: proj-1\n");
        std::fs::create_dir_all(repo.queue_dir()).unwrap();
        for (id, front) in [
            ("chain-live", "group: chain\ndepends_on: [chain-root]\n"),
            ("feat-live", "group: feat\n"),
        ] {
            Task::parse(
                repo.queue_dir().join(format!("{id}.md")),
                &format!(
                    "---\nid: {id}\ntitle: {id}\nstage: implement\npipeline: impl\n{front}---\n"
                ),
            )
            .unwrap()
            .save()
            .unwrap();
        }

        let batch = |id: &str, front: &str| {
            Task::parse(
                repo.queue_dir().join(format!("{id}.md")),
                &format!(
                    "---\nid: {id}\ntitle: {id}\nstage: implement\npipeline: impl\n{front}---\n"
                ),
            )
            .unwrap()
        };
        let answers = || {
            let cases = [
                ("ok", "group: auth\nbase: plan/demo\ndepends_on: [login]\n"),
                (
                    "stacked",
                    "group: next\nbase: plan/demo\ndepends_on: [login]\n",
                ),
                (
                    "other-base",
                    "group: auth\nbase: plan/other\ndepends_on: [login]\n",
                ),
                ("typo", "group: auth\ndepends_on: [ghost]\n"),
                ("on-arm", "group: second\ndepends_on: [arm]\n"),
                ("chain-next", "group: chain\ndepends_on: [chain-live]\n"),
                ("feat-next", "group: feat\ndepends_on: [feat-live]\n"),
            ];
            cases
                .iter()
                .map(|(id, front)| {
                    let mut tasks = [batch(id, front)];
                    check_dependencies_set(&repo, &mut tasks).map_err(|e| format!("{e:#}"))
                })
                .collect::<Vec<_>>()
        };

        reset_rebuilds();
        let indexed = with_unreadable_files(&repo, answers);
        assert_eq!(rebuilds(), 0, "queue add opened archived task files");
        assert!(indexed[0].is_ok(), "{:?}", indexed[0]);
        assert!(indexed[1].is_ok(), "{:?}", indexed[1]);
        let refusal = |at: usize| indexed[at].clone().unwrap_err();
        assert!(
            refusal(2).contains("is based on `plan/other`"),
            "{}",
            refusal(2)
        );
        assert!(
            refusal(3).contains("neither the queue nor the archive"),
            "{}",
            refusal(3)
        );
        assert!(
            indexed[5].is_ok(),
            "an archived trial arm is no second root: {:?}",
            indexed[5]
        );
        assert!(
            refusal(6).contains("no dependency in it"),
            "the slug must put the archived root in the live group: {}",
            refusal(6)
        );

        // The same answers after another archiving, a sweep and a rebuild.
        assert_eq!(answers(), indexed);
        archive(&repo, "later", "group: unrelated\n");
        assert_eq!(answers(), indexed);
        sweep(&repo, "doomed");
        assert_eq!(answers(), indexed);
        lose_index(&repo);
        assert_eq!(answers(), indexed);
    }

    /// The same refusal, but naming the age rather than leaving a swept
    /// dependency reading like a typo — `retain` is what could have deleted
    /// it, and this is the one caller that knows enough to say so.
    #[test]
    fn a_dependency_swept_out_of_the_archive_says_the_age_is_why() {
        let (mut repo, _root_guard) = fixture("swept-dep");
        // Finished tasks are kept by default; the refusal only blames the
        // sweep for an install that turned the archive sweep on.
        repo.config.housekeeping.archive_retention_days = 30;

        // `login` finished a while ago: written straight into `archive/`,
        // the same shape a real `done` task lands in, rather than queued and
        // driven there — this suite has no dispatcher to do that with.
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("login.md"),
            "---\nid: login\ntitle: login\nstage: done\n---\nbody\n",
        )
        .unwrap();
        // Its branch is still there, so `sessions` has somewhere to start.
        start_branch_exists(&repo, "task/login");

        let text = task_text("sessions", "group: demo\ndepends_on: [login]\n", BODY);
        let path = write_doc(&repo, "sessions.md", &text);
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap();

        // Stand in for `retain` having already swept it: the archived file
        // is simply gone, unconditionally — this test is specifically about
        // a dependency that finished and was then swept, not one that was
        // never queued at all.
        std::fs::remove_file(repo.archive_dir().join("login.md")).unwrap();

        let mut sessions = queued(&repo, "sessions");
        let err = check_dependencies_set(&repo, &mut [sessions.clone()]).unwrap_err();
        assert!(
            err.to_string()
                .contains("housekeeping.archive_retention_days"),
            "the age was not named: {err:#}"
        );

        // `housekeeping.archive_retention_days = 0` never sweeps, so the same
        // missing dependency is reported the plain way instead.
        let mut off = crate::config::Config::default();
        off.housekeeping.archive_retention_days = 0;
        let repo_off = Repo {
            config: off,
            ..repo.clone()
        };
        sessions.front.depends_on = vec!["login".into()];
        let err = check_dependencies_set(&repo_off, &mut [sessions]).unwrap_err();
        assert!(
            !err.to_string()
                .contains("housekeeping.archive_retention_days"),
            "retention off must not be blamed: {err:#}"
        );
    }

    /// `parallel:` is gone — a group is one chain, checked outright rather
    /// than left to a planner's own judgement — and a task still setting it
    /// is refused by name, with the fix named too.
    #[test]
    fn a_task_setting_parallel_is_refused() {
        let (repo, _root_guard) = fixture("parallel-is-gone");
        let text = task_text("cart-empty", "group: cart\nparallel: true\n", BODY);
        let path = write_doc(&repo, "cart-empty.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("parallel"), "{msg}");
        assert!(msg.contains("depends_on"), "{msg}");
        assert!(
            !repo.queue_dir().join("cart-empty.md").exists(),
            "a task setting a gone key must not be written"
        );
    }

    /// A group with two tasks that depend on nothing in it — two roots — is
    /// refused, naming both tasks and the group, matching the plan's own
    /// mockup verbatim.
    #[test]
    fn a_group_with_two_roots_is_refused() {
        let (repo, _root_guard) = fixture("two-roots");
        let a = write_doc(
            &repo,
            "cart-totals.md",
            &task_text("cart-totals", "group: cart\n", BODY),
        );
        let b = write_doc(
            &repo,
            "cart-empty.md",
            &task_text("cart-empty", "group: cart\n", BODY),
        );
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&a, &b]),
            &repo.root,
            false,
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "group `cart` has two tasks with no dependency in it: cart-totals and cart-empty \
             — a group is one chain. Give one a `depends_on`, or move it to a group of its \
             own."
        );
    }

    /// A group already broken on disk — two roots, placed by hand or queued
    /// before groups were chains — is not a reason to refuse a task in a
    /// different group: the one-chain check judges only the groups the batch
    /// puts a task into, and `doctor` is the backstop for the rest.
    #[test]
    fn a_broken_group_on_disk_does_not_refuse_a_task_in_another_group() {
        let (repo, _root_guard) = fixture("unrelated-broken-group");
        for id in ["live-a", "live-b"] {
            std::fs::write(
                repo.queue_dir().join(format!("{id}.md")),
                format!("---\nid: {id}\ntitle: {id}\ngroup: live\nstage: paused\n---\nbody\n"),
            )
            .unwrap();
        }

        let text = task_text("gate", "group: gate\n", BODY);
        let path = write_doc(&repo, "gate.md", &text);
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .expect("group `live`'s own shape is not this batch's to judge");
        assert!(repo.queue_dir().join("gate.md").exists());
    }

    /// A routine or a scheduled job mints a fresh id every run but leaves
    /// `group:` untouched (`mint_routine_batch`), so the same group name is
    /// queued again and again — once a run lands and is archived, with
    /// nothing of its own left in the queue, that name has to be free to
    /// start a fresh one-task chain, or no grouped routine could ever run a
    /// second time.
    #[test]
    fn a_group_reused_after_its_only_task_is_archived_is_not_a_second_root() {
        let (repo, _root_guard) = fixture("group-reused-after-archive");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("audit-deps-1.md"),
            "---\nid: audit-deps-1\ntitle: audit-deps-1\ngroup: nightly\nstage: done\n---\nbody\n",
        )
        .unwrap();

        let text = task_text("audit-deps-2", "group: nightly\n", BODY);
        let path = write_doc(&repo, "audit-deps-2.md", &text);
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .expect(
            "a group whose only earlier task already landed and archived is free to start \
             a fresh chain",
        );
    }

    /// The same reuse, but the earlier run is still sitting in the queue —
    /// a genuine second root of a group that has not landed yet, not a
    /// closed name being picked up again, and still refused.
    #[test]
    fn a_second_root_is_refused_while_the_groups_first_task_is_still_queued() {
        let (repo, _root_guard) = fixture("group-still-queued-two-roots");
        add(&repo, "audit-deps-1", &[]);

        let text = task_text("audit-deps-2", "group: demo\n", BODY);
        let path = write_doc(&repo, "audit-deps-2.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("has two tasks with no dependency in it"),
            "{err:#}"
        );
    }

    /// A group with two tasks depending on the same one task — a fan — is
    /// refused, naming both dependents and the group.
    #[test]
    fn a_group_with_a_fan_is_refused() {
        let (repo, _root_guard) = fixture("group-fan");
        add(&repo, "cart-totals", &[]);
        let left = write_doc(
            &repo,
            "cart-discounts.md",
            &task_text(
                "cart-discounts",
                "group: demo\ndepends_on: [cart-totals]\n",
                BODY,
            ),
        );
        let right = write_doc(
            &repo,
            "cart-copy.md",
            &task_text(
                "cart-copy",
                "group: demo\ndepends_on: [cart-totals]\n",
                BODY,
            ),
        );
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&left, &right]),
            &repo.root,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("group `demo`"), "{msg}");
        assert!(msg.contains("`cart-totals`"), "{msg}");
        assert!(
            msg.contains("cart-copy") && msg.contains("cart-discounts"),
            "{msg}"
        );
        assert!(msg.contains("a group is one chain"), "{msg}");
    }

    /// A dependent group queued while the group it stacks on is still in the
    /// pending directory — not yet run through `queue add` at all — is
    /// refused, naming that group rather than reading like a typo.
    #[test]
    fn a_dependency_on_a_group_still_in_the_pending_directory_is_refused() {
        let (repo, _root_guard) = fixture("pending-group-dep");
        write_pending(
            &repo,
            "login",
            &task_text("login", "group: auth-api\n", BODY),
        );

        let text = task_text("auth-form", "group: auth-ui\ndepends_on: [login]\n", BODY);
        let path = write_doc(&repo, "auth-form.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("`auth-api`"), "{msg}");
        assert!(msg.contains("pending directory"), "{msg}");
    }

    #[test]
    fn a_dependency_that_would_close_a_cycle_is_refused() {
        let (repo, _root_guard) = fixture("cycle-dep");
        add(&repo, "a", &[]);
        add(&repo, "b", &["a"]);

        // `a` already reaches `b`, so this edge would close the loop. It is
        // rejected by editing `a`'s file, which is the only way to express it.
        let mut a = queued(&repo, "a");
        a.front.depends_on = vec!["b".into()];
        let err = check_dependencies_set(&repo, &mut [a]).unwrap_err();

        assert!(err.to_string().contains("a → b → a"), "{err:#}");
    }

    #[test]
    fn a_task_may_not_depend_on_itself() {
        let (repo, _root_guard) = fixture("self-dep");
        let text = task_text("a", "group: demo\ndepends_on: [a]\n", BODY);
        let path = write_doc(&repo, "a.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("depend on itself"), "{err:#}");
    }

    /// A group's own first task may stack onto another group's own last
    /// task — see `a_group_stacks_onto_anothers_last_task_is_accepted` — but
    /// naming anything else in that group is still refused: a stack has
    /// exactly one join point, the tail nothing else in the other group
    /// depends on.
    #[test]
    fn a_dependency_on_a_task_of_another_group_that_is_not_its_last_is_refused() {
        let (repo, _root_guard) = fixture("cross-group-dep");
        add(&repo, "login", &[]);
        add(&repo, "profile", &["login"]);

        let text = task_text("sessions", "group: other\ndepends_on: [login]\n", BODY);
        let path = write_doc(&repo, "sessions.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("`sessions`") && msg.contains("`login`"),
            "{msg}"
        );
        assert!(msg.contains("`profile`"), "{msg}");
        assert!(msg.contains("own last task"), "{msg}");
    }

    /// The shape `d-group-stacks-on-group` actually asks for: a group's own
    /// first task naming another group's own last task is accepted outright,
    /// cut from that task's branch like any other dependency.
    #[test]
    fn a_group_stacks_onto_anothers_last_task_is_accepted() {
        let (repo, _root_guard) = fixture("group-stacks-on-group");
        add(&repo, "login", &[]);

        let text = task_text("sessions", "group: other\ndepends_on: [login]\n", BODY);
        let path = write_doc(&repo, "sessions.md", &text);
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .expect("a group's first task may stack onto another group's own last task");

        assert_eq!(
            queued(&repo, "sessions").front.depends_on,
            vec!["login".to_string()]
        );
    }

    /// A non-first task of its own naming a cross-group dependency alongside
    /// an in-group one is refused: only a group's own first task — the one
    /// with no dependency inside its own group — may stack onto another.
    /// `a_task_naming_two_other_groups_is_refused` and the tests beside it
    /// below cover the rest of this shape.
    #[test]
    fn a_task_naming_a_cross_group_dependency_alongside_an_in_group_one_is_refused() {
        let (repo, _root_guard) = fixture("cross-group-plus-in-group");
        add(&repo, "billing", &[]);
        let text = task_text("login", "group: other\n", BODY);
        let path = write_doc(&repo, "login.md", &text);
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap();

        let text = task_text(
            "sessions",
            "group: demo\ndepends_on: [billing, login]\n",
            BODY,
        );
        let path = write_doc(&repo, "sessions.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("only a group's first task"),
            "{err:#}"
        );
    }

    /// A group naming two other groups is refused: a group may stack onto
    /// at most one, since two would be a join across groups.
    #[test]
    fn a_task_naming_two_other_groups_is_refused() {
        let (repo, _root_guard) = fixture("cross-group-two-groups");
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&write_doc(
                &repo,
                "billing.md",
                &task_text("billing", "group: billing\n", BODY),
            )]),
            &repo.root,
            false,
        )
        .unwrap();
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&write_doc(
                &repo,
                "login.md",
                &task_text("login", "group: auth\n", BODY),
            )]),
            &repo.root,
            false,
        )
        .unwrap();

        let text = task_text(
            "sessions",
            "group: demo\ndepends_on: [billing, login]\n",
            BODY,
        );
        let path = write_doc(&repo, "sessions.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("2 other groups"), "{err:#}");
        assert!(err.to_string().contains("only one other group"), "{err:#}");
    }

    /// A group naming more than one task of the *same* other group is
    /// refused too — a stack has exactly one join point, that group's own
    /// last task, and no more than that one task.
    #[test]
    fn a_task_naming_two_tasks_of_the_same_other_group_is_refused() {
        let (repo, _root_guard) = fixture("cross-group-two-tasks-one-group");
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&write_doc(
                &repo,
                "auth-login.md",
                &task_text("auth-login", "group: auth\n", BODY),
            )]),
            &repo.root,
            false,
        )
        .unwrap();
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&write_doc(
                &repo,
                "auth-sessions.md",
                &task_text(
                    "auth-sessions",
                    "group: auth\ndepends_on: [auth-login]\n",
                    BODY,
                ),
            )]),
            &repo.root,
            false,
        )
        .unwrap();

        let text = task_text(
            "billing",
            "group: demo\ndepends_on: [auth-login, auth-sessions]\n",
            BODY,
        );
        let path = write_doc(&repo, "billing.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("names 2 tasks in group `auth`"),
            "{err:#}"
        );
    }

    /// Stacking onto a group that has fully landed — every task archived,
    /// nothing left in the queue — is accepted: that group's archived tail
    /// is its last task. This is the order the pending-directory refusal
    /// itself asks for ("queue `auth-api` first"), and the other group may
    /// land before the dependent is ever queued.
    #[test]
    fn stacking_onto_a_group_that_has_fully_landed_is_accepted() {
        let (repo, _root_guard) = fixture("stack-onto-landed-group");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("auth-login.md"),
            "---\nid: auth-login\ntitle: auth-login\ngroup: auth-api\nstage: done\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            repo.archive_dir().join("auth-sessions.md"),
            "---\nid: auth-sessions\ntitle: auth-sessions\ngroup: auth-api\n\
             depends_on: [auth-login]\nstage: done\n---\nbody\n",
        )
        .unwrap();
        // Its branch is still there, so `auth-form` has somewhere to start.
        start_branch_exists(&repo, "task/auth-sessions");

        let text = task_text(
            "auth-form",
            "group: auth-ui\ndepends_on: [auth-sessions]\n",
            BODY,
        );
        let path = write_doc(&repo, "auth-form.md", &text);
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .expect("a group may stack onto another group that has already landed");
    }

    /// The same landed group, but naming its first task rather than its last
    /// is still refused, naming the task that depends on it in turn.
    #[test]
    fn stacking_onto_a_landed_groups_non_last_task_is_refused() {
        let (repo, _root_guard) = fixture("stack-onto-landed-non-tail");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("auth-login.md"),
            "---\nid: auth-login\ntitle: auth-login\ngroup: auth-api\nstage: done\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            repo.archive_dir().join("auth-sessions.md"),
            "---\nid: auth-sessions\ntitle: auth-sessions\ngroup: auth-api\n\
             depends_on: [auth-login]\nstage: done\n---\nbody\n",
        )
        .unwrap();

        let text = task_text(
            "auth-form",
            "group: auth-ui\ndepends_on: [auth-login]\n",
            BODY,
        );
        let path = write_doc(&repo, "auth-form.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("`auth-sessions` in group `auth-api`"), "{msg}");
        assert!(msg.contains("own last task"), "{msg}");
    }

    /// Stacking onto a group that is not one chain itself — here, a group
    /// with two roots — is refused: fix the other group before building on
    /// top of it. `auth`'s two tasks are written straight to disk rather
    /// than through `queue_add`, which would refuse the broken shape
    /// outright before this test ever got to stack `demo` onto it.
    #[test]
    fn stacking_onto_a_group_that_is_not_one_chain_is_refused() {
        let (repo, _root_guard) = fixture("cross-group-not-a-chain");
        std::fs::write(
            repo.queue_dir().join("auth-one.md"),
            "---\nid: auth-one\ntitle: auth-one\ngroup: auth\nstage: queued\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            repo.queue_dir().join("auth-two.md"),
            "---\nid: auth-two\ntitle: auth-two\ngroup: auth\nstage: queued\n---\nbody\n",
        )
        .unwrap();

        let text = task_text("billing", "group: demo\ndepends_on: [auth-one]\n", BODY);
        let path = write_doc(&repo, "billing.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("not one chain itself"), "{err:#}");
    }

    /// With `issue_tracking.key_in_names` on, a sibling queued by an
    /// earlier `queue add` carries `group: <slug>-<group>`, and a later
    /// task still writes the bare group — the check has to see them as
    /// one group, or a chain can never span two `queue add` calls (jobs
    /// review finding 3). Only a recognised prefix, and only with the flag
    /// on: with it off the same two groups are still two groups.
    #[test]
    fn a_dependency_on_a_sibling_whose_group_carries_the_slug_prefix_is_accepted() {
        let (repo, _root_guard) = fixture("cross-batch-slug-prefix");
        add(&repo, "auth-01", &[]);
        // What the first `queue add` left behind with the flag on.
        let mut parent = queued(&repo, "auth-01");
        parent.front.group = Some("proj-12-demo".into());
        parent
            .front
            .extra
            .insert("slug".into(), serde_norway::Value::String("proj-12".into()));
        parent.save().unwrap();

        // With the flag off, the prefix means nothing — `demo` and
        // `proj-12-demo` really are two different groups, and this is now
        // simply a group's first task stacking onto the other group's own
        // last task, accepted the same way
        // `a_group_stacks_onto_anothers_last_task_is_accepted` is.
        let text = task_text("auth-02", "group: demo\ndepends_on: [auth-01]\n", BODY);
        let path = write_doc(&repo, "auth-02.md", &text);
        let args = from_args(&[&path]);
        queue_add(&repo, &Pipelines::builtin(), &args, &repo.root, false)
            .expect("a stack across two literally different groups is still accepted");
        assert_eq!(
            queued(&repo, "auth-02").front.depends_on,
            vec!["auth-01".to_string()]
        );
    }

    /// [`bare_group`] strips a recognised `<slug>-` prefix only with the flag
    /// on — the comparison every cross-group and one-chain check in
    /// `check_dependencies_set` goes through, so a chain spanning two `queue
    /// add` calls under `issue_tracking.key_in_names` reads as one group
    /// rather than a two-group stack (jobs review finding 3).
    #[test]
    fn bare_group_strips_a_recognised_slug_prefix_only_with_the_flag_on() {
        let (mut repo, _root_guard) = fixture("bare-group-slug");
        // `fixture`'s own `Config::default` now turns this on, so the "off"
        // half of this test has to say so itself rather than inherit it.
        repo.config.issue_tracking.key_in_names = false;
        add(&repo, "auth-01", &[]);
        let mut parent = queued(&repo, "auth-01");
        parent.front.group = Some("proj-12-demo".into());
        parent
            .front
            .extra
            .insert("slug".into(), serde_norway::Value::String("proj-12".into()));
        parent.save().unwrap();

        assert_eq!(
            bare_group(&repo, &parent).as_deref(),
            Some("proj-12-demo"),
            "off: the prefix means nothing"
        );

        repo.config.issue_tracking.key_in_names = true;
        assert_eq!(
            bare_group(&repo, &parent).as_deref(),
            Some("demo"),
            "on: the recognised prefix strips"
        );
    }

    /// A dependent is cut from `depends_on.first()`'s branch, so a list
    /// naming two parents only means something if the first one already
    /// carries the second's work — `check_dependencies_set` puts that id
    /// first itself rather than trusting the task's own order.
    #[test]
    fn a_depends_on_naming_two_parents_is_reordered_so_the_deeper_one_leads() {
        let (repo, _root_guard) = fixture("reorder-dep");
        add(&repo, "base", &[]);
        add(&repo, "top", &["base"]);

        // `top` already reaches `base`, so naming `base` first here is the
        // order that buys nothing — `top`'s own history already holds it.
        let text = task_text("apex", "group: demo\ndepends_on: [base, top]\n", BODY);
        let path = write_doc(&repo, "apex.md", &text);
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap();

        assert_eq!(
            queued(&repo, "apex").front.depends_on,
            vec!["top".to_string(), "base".to_string()],
            "the id reaching the other one leads, whatever order the task gave"
        );
    }

    /// A single-id list has nothing to reorder — it should come out exactly
    /// as it went in.
    #[test]
    fn a_single_id_depends_on_is_left_untouched() {
        let (repo, _root_guard) = fixture("single-dep");
        add(&repo, "login", &[]);
        add(&repo, "sessions", &["login"]);

        assert_eq!(
            queued(&repo, "sessions").front.depends_on,
            vec!["login".to_string()]
        );
    }

    /// Two parents that genuinely have nothing to say about each other can no
    /// longer both be waited on directly: one has to go behind the other, so
    /// the branch a dependent is cut from actually carries every parent's
    /// work rather than just the first one named.
    #[test]
    fn a_depends_on_with_no_id_reaching_the_rest_is_refused() {
        // `a` and `b` have to be queued in the same call as `c`: each on its
        // own, with no dependency between them, is already refused as a
        // group with two roots — see `a_group_with_two_roots_is_refused` —
        // so the three go in one submission, the one shape that can still
        // reach `c`'s own reorder check before the group-shape check ever
        // gets a look at `a` and `b` alone.
        let (repo, _root_guard) = fixture("no-head-dep");
        let a = write_doc(&repo, "a.md", &task_text("a", "group: demo\n", BODY));
        let b = write_doc(&repo, "b.md", &task_text("b", "group: demo\n", BODY));
        let c = write_doc(
            &repo,
            "c.md",
            &task_text("c", "group: demo\ndepends_on: [a, b]\n", BODY),
        );
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&a, &b, &c]),
            &repo.root,
            false,
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("`c`"), "{msg}");
        assert!(msg.contains("`a`") && msg.contains("`b`"), "{msg}");
        assert!(msg.contains("would not be in it"), "{msg}");
        assert!(
            !repo.queue_dir().join("c.md").exists(),
            "a task that could never be cut cleanly should not have been written"
        );
    }

    /// With no live lane and no command step running, `queue pause` is
    /// exactly `park`: the task lands on `paused` with `parked_from` naming
    /// the step it was pulled off of.
    #[test]
    fn queue_pause_parks_a_task_with_nothing_live_to_interrupt() {
        let (repo, _root_guard) = fixture("queue-pause");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);
        let mut task = queued(&repo, "solo");
        task.set_stage_unbanked("implement", "test setup");
        task.save().unwrap();

        queue_pause(&repo, &pipelines, "solo", false).unwrap();

        let task = queued(&repo, "solo");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(task.front.parked_from.as_deref(), Some("implement"));
    }

    /// `queue pause` on a `blocked` task with the unblocker mid-turn
    /// interrupts that lane and parks the task, exactly as it already does
    /// for any other live step — gained through the same two predicates the
    /// board's `p` reads, with no edit to `queue_pause` itself. `blocked_from`
    /// survives untouched beside the fresh `parked_from: blocked`.
    #[test]
    fn queue_pause_interrupts_a_blocked_tasks_live_unblocker() {
        let (mut repo, _root_guard) = fixture("queue-pause-blocked");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "stuck", &[]);
        let mut task = queued(&repo, "stuck");
        task.front.blocked_from = Some("implement".into());
        task.set_stage_unbanked(crate::pipeline::BLOCKED, "test setup");
        task.save().unwrap();

        let (mux, name) = crate::status::testutil::live_headless_lane_at(
            &repo,
            "stuck",
            crate::pipeline::BLOCKED,
            "unblocker",
        );

        queue_pause(&repo, &pipelines, "stuck", false).unwrap();

        assert!(
            mux.list_lanes().unwrap().iter().all(|l| l.name != name),
            "headless has no keyboard, so an interrupt ends the turn"
        );
        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), crate::pipeline::PAUSED);
        assert_eq!(
            task.front.parked_from.as_deref(),
            Some(crate::pipeline::BLOCKED)
        );
        assert_eq!(task.front.blocked_from.as_deref(), Some("implement"));
    }

    #[test]
    fn queue_pause_refuses_an_unknown_task() {
        let (repo, _root_guard) = fixture("queue-pause-unknown");
        let pipelines = Pipelines::builtin();
        assert!(queue_pause(&repo, &pipelines, "ghost", false).is_err());
    }

    /// A task sitting on a real `Command`-kind step (`handover`, in the
    /// builtin `default` pipeline) with a run genuinely in flight: `queue
    /// pause` without `--force` refuses rather than stopping it out from
    /// under whatever it was doing, and the run is left exactly as it was.
    /// With `--force` it kills the run, the task still lands on `paused`,
    /// and a second call to `state` — reading the pid file back off disk —
    /// proves the process is actually gone, not just forgotten about.
    #[test]
    fn queue_pause_refuses_a_running_command_step_without_force() {
        let (repo, _root_guard) = fixture("queue-pause-running");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);
        let mut task = queued(&repo, "solo");
        task.set_stage_unbanked("handover", "test setup");
        task.save().unwrap();

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("handover", "solo");
        let sleep = "sleep 20";
        let pid = runs
            .start(&key, sleep, &repo.checkout, &BTreeMap::new())
            .unwrap();
        assert_eq!(runs.state(&key), crate::command_step::RunState::Running);

        let err = queue_pause(&repo, &pipelines, "solo", false).unwrap_err();
        assert!(
            err.to_string().contains("--force"),
            "expected a --force refusal, got: {err}"
        );
        assert_eq!(
            queued(&repo, "solo").stage(),
            "handover",
            "a refused pause must not have moved the task"
        );
        assert_eq!(
            runs.state(&key),
            crate::command_step::RunState::Running,
            "a refused pause must not have touched the run either"
        );

        queue_pause(&repo, &pipelines, "solo", true).unwrap();

        assert_eq!(queued(&repo, "solo").stage(), crate::pipeline::PAUSED);
        assert_eq!(
            runs.state(&key),
            crate::command_step::RunState::Fresh,
            "--force clears the run's own bookkeeping"
        );
        assert!(
            !crate::headless::alive(pid),
            "--force must actually kill the process, not just forget its pid"
        );
    }

    /// `queue resume` on a parked task is `unpark`: back onto the step it was
    /// pulled off of, exactly what the board's own `r` key does — see
    /// `crate::status::resume_task`.
    #[test]
    fn queue_resume_sends_a_paused_task_back_onto_its_step() {
        let (repo, _root_guard) = fixture("queue-resume");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);
        let mut task = queued(&repo, "solo");
        task.set_stage_unbanked("implement", "test setup");
        task.save().unwrap();
        queue_pause(&repo, &pipelines, "solo", false).unwrap();

        queue_resume(&repo, &pipelines, "solo").unwrap();

        let task = queued(&repo, "solo");
        assert_eq!(task.stage(), "implement");
    }

    /// A question-held row is never on `paused`, `parked` or `blocked` —
    /// none of `paused_at`, `parked_from` or `blocked_from` is ever set for
    /// it — so `resume_task`'s guard against a task with nothing to resume
    /// must not treat that absence as a reason to do nothing, the way it
    /// does for a genuine race on the persisted `paused` stage. `r` on that
    /// row instead falls through to `back_onto_its_step`, which resumes at
    /// `resume_target`'s `last_report.step`: the step the row was already
    /// on. That restarts it — banking a round on the step's own route to
    /// itself and logging the same "unblocked by hand" message a block's
    /// `r` would — rather than leaving `r` a silent no-op the rest of the
    /// suite could not tell apart from the guard doing its job.
    #[test]
    fn a_question_held_task_is_restarted_on_the_step_its_pane_never_answered() {
        let (repo, _root_guard) = fixture("queue-resume-question-pane");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);
        let mut task = queued(&repo, "solo");
        task.set_stage_unbanked("implement", "test setup");
        task.front.last_report = Some(crate::task::LastReport {
            step: "implement".into(),
            outcome: "pass".into(),
            at: 0,
            blocked: false,
        });
        task.save().unwrap();
        let before = queued(&repo, "solo");
        assert_eq!(before.front.paused_at, None);
        assert_eq!(before.front.parked_from, None);
        assert_eq!(before.front.blocked_from, None);

        queue_resume(&repo, &pipelines, "solo").unwrap();

        let task = queued(&repo, "solo");
        assert_eq!(task.stage(), "implement");
        assert!(
            task.body.contains("unblocked by hand"),
            "the step was restarted rather than left alone: {}",
            task.body
        );
    }

    /// `queue resume` on a task still waiting in the queue is refused, the
    /// same as `spoolway resume`, and says it starts on its own.
    #[test]
    fn queue_resume_refuses_a_task_still_queued() {
        let (repo, _root_guard) = fixture("queue-resume-queued");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);

        let err = queue_resume(&repo, &pipelines, "solo").unwrap_err();
        let said = format!("{err:#}");
        assert!(said.contains("solo") && said.contains("queued"), "{said}");
        assert_eq!(queued(&repo, "solo").stage(), crate::pipeline::QUEUED);
    }

    /// A live lane on the step makes the task running, so `queue resume`
    /// refuses it with `spoolway resume`'s message and leaves it alone.
    #[test]
    fn queue_resume_refuses_a_task_with_a_live_lane() {
        let (mut repo, _root_guard) = fixture("queue-resume-live-lane");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);
        let mut task = queued(&repo, "solo");
        task.set_stage_unbanked("implement", "test setup");
        task.save().unwrap();
        let (mux, name) = crate::status::testutil::live_headless_lane_at(
            &repo,
            "solo",
            "implement",
            "implementer",
        );

        let err = queue_resume(&repo, &pipelines, "solo").unwrap_err();
        let _ = mux.interrupt_lane(&name);
        let said = format!("{err:#}");
        assert!(
            said.contains("solo") && said.contains("implement"),
            "{said}"
        );
        assert!(said.contains("queue pause solo"), "{said}");
        assert_eq!(queued(&repo, "solo").stage(), "implement");
    }

    /// A blocked task whose unblocker lane is live counts as running here, but
    /// is still resumable: it must be moved onto its step once, not again onto
    /// `queued` by a second resume.
    #[test]
    fn queue_resume_moves_a_blocked_task_with_a_live_unblocker_once() {
        let (mut repo, _root_guard) = fixture("queue-resume-blocked-live");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        let pipelines = Pipelines::builtin();
        add(&repo, "stuck", &[]);
        let mut task = queued(&repo, "stuck");
        task.front.blocked_from = Some("implement".into());
        task.set_stage_unbanked(crate::pipeline::BLOCKED, "test setup");
        task.save().unwrap();
        let (mux, name) = crate::status::testutil::live_headless_lane_at(
            &repo,
            "stuck",
            crate::pipeline::BLOCKED,
            "unblocker",
        );

        queue_resume(&repo, &pipelines, "stuck").unwrap();
        let _ = mux.interrupt_lane(&name);

        let task = queued(&repo, "stuck");
        assert_eq!(task.stage(), "implement");
        assert!(
            !task.body.contains("put back from the board"),
            "resumed a second time: {}",
            task.body
        );
    }

    /// Without `--force`, a started task a queued task depends on is not told
    /// to use `--force`, which would be refused; it is told what to do instead.
    #[test]
    fn unqueue_without_force_leaves_out_the_force_hint_when_a_dependent_waits() {
        let (repo, _root_guard) = fixture("unqueue-no-force-dependent");
        let pipelines = Pipelines::builtin();
        add(&repo, "parent", &[]);
        add(&repo, "child", &["parent"]);
        let mut task = queued(&repo, "parent");
        task.front.stage = "implement".to_string();
        task.save().unwrap();

        let err = queue_unqueue(&repo, &pipelines, &unqueue_args("parent")).unwrap_err();
        let said = format!("{err:#}");
        assert!(!said.contains("--force\n"), "{said}");
        assert!(said.contains("Unqueue `child` first"), "{said}");
    }

    #[test]
    fn queue_resume_refuses_an_unknown_task() {
        let (repo, _root_guard) = fixture("queue-resume-unknown");
        let pipelines = Pipelines::builtin();
        assert!(queue_resume(&repo, &pipelines, "ghost").is_err());
    }

    /// Bare `queue add` no longer queues anything with no `--from`: there is
    /// no id to name a file after, so what it hands back is the task to
    /// fill one in with, still unfilled.
    #[test]
    fn bare_queue_add_prints_an_unfilled_skeleton_task() {
        let (repo, _root_guard) = fixture("skeleton");
        let pipelines = Pipelines::builtin();

        let doc = skeleton_task(&repo, &pipelines).unwrap();

        assert!(doc.starts_with("---\nid:"), "{doc}");
        assert!(doc.contains("## Intend"), "{doc}");
        assert!(
            !doc.contains("spoolway:contract"),
            "the note to whoever maintains the skeleton is not task content:\n{doc}"
        );
        // `pipeline:` is required now, so the skeleton names a real, live
        // choice uncommented — never `# pipeline: ...`, the shape every
        // other optional row still uses.
        let pipeline_row = doc
            .lines()
            .find(|line| line.starts_with("pipeline:"))
            .unwrap_or_else(|| panic!("no uncommented `pipeline:` row:\n{doc}"));
        let name = pipeline_row
            .trim_start_matches("pipeline:")
            .split('#')
            .next()
            .unwrap()
            .trim();
        assert!(
            pipelines.get(name).is_ok(),
            "`{name}` names a real pipeline: {doc}"
        );
        assert!(
            !doc.contains("# pipeline:"),
            "pipeline: must not be commented out, unlike the optional rows around it:\n{doc}"
        );

        let args = QueueAddArgs {
            from: vec![],
            base: None,
            dry_run: false,
        };
        assert!(
            queue_add(&repo, &pipelines, &args, &repo.root, false).is_ok(),
            "bare queue add prints rather than errors"
        );
    }

    /// The body is the project's, whatever is in it. Nothing here inspects it,
    /// so a file with no headings at all is queued exactly as written, and a
    /// body missing its trailing newline still ends on a line boundary — a
    /// later `## Status Log` entry is appended line by line.
    #[test]
    fn a_body_is_taken_as_given_however_it_is_shaped() {
        let text = task_text(
            "demo",
            "group: demo\n",
            "Just do the thing. No headings anywhere.",
        );
        let task = parse_submission("demo.md", &text, Some("plan/demo")).unwrap();

        assert_eq!(task.body, "Just do the thing. No headings anywhere.\n");
    }

    /// An empty body is the one refusal: a lane would open the file and find
    /// nothing to work from, and the frontmatter alone says nothing about what
    /// to build.
    #[test]
    fn a_task_with_an_empty_body_is_refused() {
        let text = task_text("demo", "group: demo\n", "   \n\n");
        let err = parse_submission("demo.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err:#}");
    }

    /// `title:` is the squashed commit's subject and the pull request's
    /// title — no heading in the body can supply one, so a task leaving
    /// it blank is refused before anything is queued, naming the task
    /// and the field.
    #[test]
    fn a_task_with_no_title_is_refused() {
        let text = "---\nid: demo\ngroup: demo\n---\n## Goal\n\nDo the thing.\n";
        let err = parse_submission("mine.md", text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("`title:`"), "{err:#}");
        assert!(err.to_string().contains("mine.md"), "{err:#}");
    }

    /// A label reaches a hook comma-joined in `SPOOLWAY_LABELS`, and Jira's
    /// own labels cannot hold a space at all — so a label holding either a
    /// comma or any whitespace is refused before anything is queued, naming
    /// the task and the label. A clean batch of labels is kept verbatim.
    #[test]
    fn a_label_holding_whitespace_or_a_comma_is_refused_naming_task_and_label() {
        let text = task_text("demo", "group: demo\nlabels: [\"has space\"]\n", BODY);
        let err = parse_submission("mine.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("mine.md"), "{err:#}");
        assert!(err.to_string().contains("has space"), "{err:#}");

        let text = task_text("demo", "group: demo\nlabels: [\"a,b\"]\n", BODY);
        let err = parse_submission("mine.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("a,b"), "{err:#}");

        let text = task_text("demo", "group: demo\nlabels: [bug, needs-triage]\n", BODY);
        let task = parse_submission("mine.md", &text, Some("plan/demo")).unwrap();
        assert_eq!(
            task.front.labels,
            vec!["bug".to_string(), "needs-triage".to_string()]
        );
    }

    /// An empty label fails neither the whitespace nor the comma check, so
    /// it gets its own message rather than the wrong one of those two.
    #[test]
    fn an_empty_label_gets_its_own_message() {
        let text = task_text("demo", "group: demo\nlabels: [\"\"]\n", BODY);
        let err = parse_submission("mine.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("empty label"), "{err:#}");
        assert!(
            !err.to_string().contains("whitespace"),
            "an empty label is not a whitespace-or-comma refusal: {err:#}"
        );
    }

    /// The keys spoolway sets on every task itself are refused by name, and
    /// the task they came from is named too — this is the whole
    /// enforcement that a producer cannot smuggle a task onto an arbitrary
    /// step, run or attempt count.
    #[test]
    fn a_task_setting_a_reserved_key_is_refused_by_name() {
        for key in [
            "stage",
            "run",
            "attempts",
            "base_commit",
            "trial",
            "trial_group",
            "branch",
        ] {
            let text = task_text("demo", &format!("group: demo\n{key}: bogus\n"), BODY);
            let err = parse_submission("mine.md", &text, Some("plan/demo")).unwrap_err();
            assert!(err.to_string().contains(key), "{key}: {err:#}");
            assert!(err.to_string().contains("mine.md"), "{key}: {err:#}");
        }
    }

    /// `base:` is a task's to set, and what it sets is kept — a branch
    /// this repository really has is checked for by `validate_batch`, not
    /// here. A task that leaves it out, or writes it blank, takes the
    /// submission's own base instead — the `--base` flag or the checkout's
    /// branch, whichever `parse_submission` was handed.
    #[test]
    fn a_task_setting_base_keeps_it_and_one_without_takes_the_submissions() {
        let text = task_text("demo", "group: demo\nbase: some/other/branch\n", BODY);
        let task = parse_submission("mine.md", &text, Some("plan/live")).unwrap();
        assert_eq!(task.front.base.as_deref(), Some("some/other/branch"));

        let blank = task_text("demo", "group: demo\nbase: \"  \"\n", BODY);
        let task = parse_submission("mine.md", &blank, Some("plan/live")).unwrap();
        assert_eq!(task.front.base.as_deref(), Some("plan/live"));

        let plain = task_text("demo", "group: demo\n", BODY);
        let task = parse_submission("mine.md", &plain, Some("plan/live")).unwrap();
        assert_eq!(task.front.base.as_deref(), Some("plan/live"));
    }

    /// A task that sets no `base:` of its own, submitted with no
    /// `--base` either, is refused by name — never based on whichever
    /// branch a checkout happens to have out.
    #[test]
    fn a_task_with_no_base_and_no_flag_is_refused() {
        let text = task_text("demo", "group: demo\n", BODY);
        let err = parse_submission("explicit-task-base.md", &text, None).unwrap_err();
        assert!(err.to_string().contains("explicit-task-base.md"), "{err:#}");
        assert!(err.to_string().contains("sets no `base:`"), "{err:#}");
        assert!(err.to_string().contains("--base"), "{err:#}");
    }

    /// There is no project default to route an omission through any more, so
    /// a task naming no `pipeline:` is refused the same way one naming
    /// no `group:` already is — built by hand rather than through
    /// `task`, which now fills the key in.
    #[test]
    fn a_task_with_no_pipeline_is_refused() {
        let text = format!("---\nid: demo\ntitle: demo, done\ngroup: demo\n---\n{BODY}");
        let err = parse_submission("no-pipeline.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("no-pipeline.md"), "{err:#}");
        assert!(err.to_string().contains("must set `pipeline:`"), "{err:#}");
    }

    /// A task that sets `pipeline:` to nothing but whitespace is refused
    /// the same as one that omits the key outright — blank counts as absent,
    /// the same courtesy `group:` already gets.
    #[test]
    fn a_task_with_a_blank_pipeline_is_refused() {
        let text =
            format!("---\nid: demo\ntitle: demo, done\ngroup: demo\npipeline: \"  \"\n---\n{BODY}");
        let err = parse_submission("blank-pipeline.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("must set `pipeline:`"), "{err:#}");
    }

    /// `tracking: off` is the one value this key ever means anything for —
    /// a task hand-writing anything else is refused the same validation
    /// `queue add --from` runs, rather than silently queuing with tracking
    /// left on.
    #[test]
    fn a_task_setting_tracking_to_anything_but_off_is_refused() {
        let text = task_text("demo", "group: demo\ntracking: paused\n", BODY);
        let err = parse_submission("bad-tracking.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("bad-tracking.md"), "{err:#}");
        assert!(
            err.to_string()
                .contains("`tracking:` may only be set to `off`"),
            "{err:#}"
        );
    }

    /// The value this whole key exists for is accepted, and round-trips
    /// through `Task::tracking_off` the same way a batch [`open_and_prefix`]
    /// stamped it on would.
    #[test]
    fn a_task_setting_tracking_off_by_hand_is_accepted() {
        let text = task_text("demo", "group: demo\ntracking: off\n", BODY);
        let task = parse_submission("hand-tracking-off.md", &text, Some("plan/demo")).unwrap();
        assert!(task.tracking_off());
    }

    /// The retired quota-and-usage-limit park fields have no struct home any
    /// more — a submission that still carries one from an earlier run has it
    /// dropped on parse, the same as any other task `Task::parse` refuses
    /// to round-trip.
    #[test]
    fn a_task_carrying_park_fields_has_them_dropped() {
        let text = task_text(
            "demo",
            "group: demo\nparked_at: 1788793980\nparked_until: 1788801180\nparked_window: five_hour\n",
            BODY,
        );
        let task = parse_submission("mine.md", &text, Some("plan/demo")).unwrap();
        let rendered = task.render().unwrap();
        assert!(!rendered.contains("parked_at"), "{rendered}");
        assert!(!rendered.contains("parked_until"), "{rendered}");
        assert!(!rendered.contains("parked_window"), "{rendered}");
    }

    /// `launch_failures` is a dispatcher-owned counter too — see
    /// `IGNORED_KEYS` in `src/commands/task.rs` — and a task that carries
    /// one in from an earlier run must not have it survive back into the
    /// queue: a re-queued task that parked with a step's count already at
    /// the ceiling would otherwise write the very first failure of its next
    /// run as the fourth attempt, past `MAX_LAUNCH_FAILURES`, and park again
    /// without ever writing why — the reason-writing guard in
    /// `Dispatcher::note_launch_failure` fires only on the attempt that
    /// exactly spends the ceiling, and a count that starts at 3 skips it.
    #[test]
    fn a_task_carrying_launch_failures_has_them_reset() {
        let text = task_text(
            "demo",
            "group: demo\nlaunch_failures:\n  implement: 3\n",
            BODY,
        );
        let task = parse_submission("mine.md", &text, Some("plan/demo")).unwrap();
        assert!(task.front.launch_failures.is_empty());
    }

    /// `restart` is a dispatcher-owned one-shot key the task contract throws
    /// away, so a submitted task never arrives restarting a step that never
    /// ran.
    #[test]
    fn a_task_carrying_restart_has_it_reset() {
        let text = task_text("demo", "group: demo\nrestart: implement\n", BODY);
        let task = parse_submission("mine.md", &text, Some("plan/demo")).unwrap();
        assert_eq!(task.front.restart, None);
    }

    /// A repeat submission is refused whether the id is still in the queue —
    /// a run still in flight — or already in the archive, naming whichever
    /// file it actually found: an archived run finished under that id, and a
    /// second one queued over it would be silently overwritten the moment
    /// this one finished too.
    #[test]
    fn validate_batch_refuses_an_id_in_either_the_queue_or_the_archive() {
        let (repo, _root_guard) = fixture("validate-batch-existing");
        std::fs::write(
            repo.queue_dir().join("taken.md"),
            "---\nid: taken\ntitle: taken\nstage: queued\n---\nbody\n",
        )
        .unwrap();
        let text = task_text("taken", "group: demo\n", BODY);
        let err = validate_batch(
            &repo,
            &Pipelines::builtin(),
            Some("plan/demo"),
            &[("mine.md".into(), text)],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains(&repo.queue_dir().join("taken.md").display().to_string()),
            "{err:#}"
        );

        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("done.md"),
            "---\nid: done\ntitle: done\nstage: done\n---\nbody\n",
        )
        .unwrap();
        let text = task_text("done", "group: demo\n", BODY);
        let err = validate_batch(
            &repo,
            &Pipelines::builtin(),
            Some("plan/demo"),
            &[("mine.md".into(), text)],
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains(&repo.archive_dir().join("done.md").display().to_string()),
            "{err:#}"
        );
    }

    /// A `gate_at:` naming no step of the task's pipeline is refused with the
    /// task and the steps it may name, and `done` is no step.
    #[test]
    fn validate_batch_refuses_a_gate_at_that_names_no_step() {
        let (repo, _root_guard) = fixture("gate-at-no-step");
        for bad in ["nosuch", "done"] {
            let text = task_text("demo", &format!("group: demo\ngate_at: {bad}\n"), BODY);
            let err = validate_batch(
                &repo,
                &Pipelines::builtin(),
                Some("plan/demo"),
                &[("mine.md".into(), text)],
            )
            .unwrap_err()
            .to_string();
            assert!(err.contains("task `demo`"), "{err}");
            assert!(err.contains(&format!("gate_at: {bad}")), "{err}");
            assert!(err.contains("implement"), "the steps it may name: {err}");
        }

        let text = task_text("demo", "group: demo\ngate_at: implement\n", BODY);
        validate_batch(
            &repo,
            &Pipelines::builtin(),
            Some("plan/demo"),
            &[("mine.md".into(), text)],
        )
        .expect("a real step is accepted");
    }

    /// `--base` is as arbitrary a value as a task's own `base:` — a
    /// leading `-` or a branch this repository does not have locally is
    /// refused whichever of the two named it, not only when a task's
    /// own value happens to disagree with the flag.
    #[test]
    fn a_flags_base_is_checked_the_same_as_a_tasks_own() {
        let (repo, _root_guard) = fixture("flag-base-checked");
        let text = task_text("demo", "group: demo\n", BODY);
        let err = validate_batch(
            &repo,
            &Pipelines::builtin(),
            Some("no/such/branch"),
            &[("mine.md".into(), text)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not have locally"), "{err:#}");

        let text = task_text("demo", "group: demo\n", BODY);
        let err = validate_batch(
            &repo,
            &Pipelines::builtin(),
            Some("-x"),
            &[("mine.md".into(), text)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("would read as a flag"), "{err:#}");
    }

    /// A task naming its own `base:` is checked even when that value
    /// happens to equal the submission's `--base` — the two are not allowed
    /// to shadow each other into skipping the check.
    #[test]
    fn a_tasks_own_base_is_checked_even_when_it_matches_the_flag() {
        let (repo, _root_guard) = fixture("own-base-matches-flag");
        let text = task_text("demo", "group: demo\nbase: no/such/branch\n", BODY);
        let err = validate_batch(
            &repo,
            &Pipelines::builtin(),
            Some("no/such/branch"),
            &[("mine.md".into(), text)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not have locally"), "{err:#}");
    }

    /// A branch that only `origin` has is a valid base — cleanup deletes a
    /// finished task's local branch once it is pushed, so a pending pull
    /// request's branch is usually on `origin` only, and refusing it here
    /// would refuse the ordinary case of stacking a task on one.
    #[test]
    fn a_base_only_origin_has_is_accepted() {
        let (repo, _root_guard) = fixture("remote-only-base");
        let origin = crate::scratch::root("remote-only-base-origin.git");
        let _ = std::fs::remove_dir_all(&origin);
        std::fs::create_dir_all(&origin).unwrap();
        crate::repo::run(&origin, "git", &["init", "-q", "--bare", "-b", "main"]).unwrap();
        crate::repo::run(
            &repo.root,
            "git",
            &["remote", "add", "origin", origin.to_str().unwrap()],
        )
        .unwrap();
        crate::repo::run(
            &repo.root,
            "git",
            &["push", "-q", "origin", "plan/demo:main"],
        )
        .unwrap();
        crate::repo::run(&origin, "git", &["branch", "task/remote-only", "main"]).unwrap();

        assert!(check_task_base(&repo, "t", "task/remote-only").is_ok());
        assert!(
            repo.git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/task/remote-only",
            ])
            .is_err(),
            "accepting it must not have created a local branch"
        );
    }

    /// Unrecognised keys are a project's own metadata, not spoolway's
    /// business, and survive a round trip through `Frontmatter`'s `extra`.
    #[test]
    fn unrecognised_keys_survive_through_extra() {
        let text = task_text("demo", "group: demo\nsize: small\ncomplexity: 3\n", BODY);
        let task = parse_submission("mine.md", &text, Some("plan/demo")).unwrap();

        assert_eq!(
            task.front.extra.get("size").and_then(|v| v.as_str()),
            Some("small")
        );
        assert_eq!(
            task.front.extra.get("complexity").and_then(|v| v.as_i64()),
            Some(3)
        );

        let rendered = task.render().unwrap();
        assert!(rendered.contains("size: small"), "{rendered}");
        assert!(rendered.contains("complexity: 3"), "{rendered}");
    }

    /// `gate_at` is a task's to set, read by `commands::report` the same
    /// way whoever wrote it by hand or the board's own `s` key would have —
    /// though unlike `step.gate`, it catches whatever that step reports, not
    /// only its pass.
    #[test]
    fn gate_at_is_read_from_a_task() {
        let text = task_text("demo", "group: demo\ngate_at: handover\n", BODY);
        let task = parse_submission("mine.md", &text, Some("plan/demo")).unwrap();
        assert_eq!(task.front.gate_at.as_deref(), Some("handover"));
    }

    /// `source` is a task's to set, optionally, and lands on the task
    /// verbatim — nothing in `parse_submission` parses it, the same as
    /// nothing anywhere else in spoolway does.
    #[test]
    fn source_is_read_from_a_task_verbatim() {
        let text = task_text(
            "demo",
            "group: demo\nsource: https://github.com/x/y/issues/42\n",
            BODY,
        );
        let task = parse_submission("mine.md", &text, Some("plan/demo")).unwrap();
        assert_eq!(
            task.front.source.as_deref(),
            Some("https://github.com/x/y/issues/42")
        );
    }

    /// The whole point of validating a submission as a set: a `depends_on`
    /// naming a sibling submitted in the same breath is satisfied with
    /// nothing sorted first — the sibling is not in `repo.tasks()` yet, only
    /// in this batch.
    #[test]
    fn a_sibling_in_the_same_submission_satisfies_depends_on() {
        let (repo, _root_guard) = fixture("sibling-dep");
        let login = task_text("login", "group: demo\n", BODY);
        let sessions = task_text("sessions", "group: demo\ndepends_on: [login]\n", BODY);
        let login_path = write_doc(&repo, "login.md", &login);
        let sessions_path = write_doc(&repo, "sessions.md", &sessions);

        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&login_path, &sessions_path]),
            &repo.root,
            false,
        )
        .unwrap();

        assert!(repo.queue_dir().join("login.md").exists());
        assert!(repo.queue_dir().join("sessions.md").exists());
    }

    /// Written all or none: one task that fails validation must not
    /// leave the ones that would have passed sitting in the queue.
    #[test]
    fn one_bad_task_queues_nothing_from_the_same_submission() {
        let (repo, _root_guard) = fixture("all-or-none");
        let good = task_text("wire", "group: demo\n", BODY);
        let bad = task_text("bogus", "group: demo\ndepends_on: [ghost]\n", BODY);
        let good_path = write_doc(&repo, "wire.md", &good);
        let bad_path = write_doc(&repo, "bogus.md", &bad);

        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&good_path, &bad_path]),
            &repo.root,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("ghost"), "{err:#}");
        assert!(
            !repo.queue_dir().join("wire.md").exists(),
            "the whole submission is one unit — the good half must not land either"
        );
    }

    /// A `---`-separated stream is split back into whole tasks, each
    /// still fenced on both sides — this is what `--from -` reads.
    #[test]
    fn a_stream_splits_into_whole_tasks() {
        let a = task_text("a", "group: demo\n", "## Goal\nFirst.\n");
        let b = task_text("b", "group: demo\n", "## Goal\nSecond.\n");
        let stream = format!("{a}{b}");

        let docs = split_stream(&stream);

        assert_eq!(docs.len(), 2, "{docs:?}");
        assert_eq!(docs[0], a);
        assert_eq!(docs[1], b);
    }

    /// A directory named by `--from` expands to every `*.md` file in it, in
    /// filename order — non-markdown files beside them are not tasks.
    #[test]
    fn a_directory_expands_to_its_md_files_in_order() {
        let (repo, _root_guard) = fixture("from-dir");
        let dir = repo.root.join("tasks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.md"), task_text("b", "group: demo\n", BODY)).unwrap();
        std::fs::write(dir.join("a.md"), task_text("a", "group: demo\n", BODY)).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a task").unwrap();

        let docs = gather_tasks(&[dir.display().to_string()]).unwrap();

        assert_eq!(docs.len(), 2, "{docs:?}");
        assert!(docs[0].0.ends_with("a.md"), "{docs:?}");
        assert!(docs[1].0.ends_with("b.md"), "{docs:?}");
    }

    // ------------------------------------------------------------- the screen

    fn keys(s: &str) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(s.as_bytes().to_vec())
    }

    #[test]
    fn read_key_decodes_arrows_control_bytes_and_plain_chars() {
        let mut input = keys("a\r\t\x1b[A\x1b[B\x1b[C\x1b[D\x1b[5~\x1b[6~\x7f");
        assert_eq!(read_key(&mut input), Some(Key::Char('a')));
        assert_eq!(read_key(&mut input), Some(Key::Enter));
        assert_eq!(read_key(&mut input), Some(Key::Tab));
        assert_eq!(read_key(&mut input), Some(Key::Up));
        assert_eq!(read_key(&mut input), Some(Key::Down));
        assert_eq!(read_key(&mut input), Some(Key::Right));
        assert_eq!(read_key(&mut input), Some(Key::Left));
        assert_eq!(read_key(&mut input), Some(Key::PageUp));
        assert_eq!(read_key(&mut input), Some(Key::PageDown));
        // `0x7f` (DEL) decodes as its own key, not as `Char('\u{7f}')` — see
        // `Key::Backspace`'s own doc comment on why the filter box needs it
        // told apart from a character typed on purpose.
        assert_eq!(read_key(&mut input), Some(Key::Backspace));
        assert_eq!(
            read_key(&mut input),
            None,
            "an empty pipe is the same as no terminal: read_key stops rather than blocking"
        );
    }

    /// One pending task, written into the directory
    /// `list_groups` scans. `doc` is the whole task, its own `group:`
    /// and all — the same bytes `--from` would read.
    fn write_pending(repo: &Repo, id: &str, doc: &str) -> std::path::PathBuf {
        let path = repo.pending_dir().join(format!("{id}.md"));
        std::fs::write(&path, doc).unwrap();
        path
    }

    /// Two tasks of one group, in dependency order — what a test needs
    /// whenever one task is not enough to say what it is about.
    fn write_pending_two(repo: &Repo, a_id: &str, a_doc: &str, b_id: &str, b_doc: &str) {
        write_pending(repo, a_id, a_doc);
        write_pending(repo, b_id, b_doc);
    }

    /// The groups those tasks gather into, in the order the left pane
    /// draws them.
    fn listed(repo: &Repo) -> Vec<Group> {
        super::pending::list_groups(repo).unwrap()
    }

    /// Put a task in the queue without going through the screen, which is
    /// the only thing that makes its group read as queued — see
    /// [`super::pending::GroupState`].
    fn already_queued(repo: &Repo, id: &str) {
        std::fs::write(
            repo.queue_dir().join(format!("{id}.md")),
            format!("---\nid: {id}\ntitle: {id}\nstage: queued\n---\nbody\n"),
        )
        .unwrap();
    }

    /// Put a task in the archive with its own `group:`, which is what makes a
    /// group whose every task is there read as done — see
    /// [`super::pending::GroupState`].
    fn already_done(repo: &Repo, id: &str, group: &str) {
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join(format!("{id}.md")),
            format!("---\nid: {id}\ntitle: {id}\ngroup: {group}\nstage: done\n---\nbody\n"),
        )
        .unwrap();
    }

    /// Plays `input` through `run_screen` against the repo's own root and
    /// the builtin pipelines, and hands back everything it drew.
    fn screen(repo: &Repo, groups: Vec<Group>, input: &str) -> String {
        screen_exit(repo, groups, input).1
    }

    /// The same, keeping the exit `run_screen` came back with too.
    fn screen_exit(repo: &Repo, groups: Vec<Group>, input: &str) -> (ScreenExit, String) {
        let routines = super::super::routines::list_routines(repo).unwrap();
        let mut input = keys(input);
        let mut out = Vec::new();
        let exit = run_screen(
            repo,
            &Pipelines::builtin(),
            &repo.root,
            groups,
            routines,
            &mut input,
            &mut out,
        )
        .unwrap();
        (exit, String::from_utf8(out).unwrap())
    }

    /// How many `git` processes and how many pending/queue files were read
    /// on this thread, running `input` through `screen_exit` against a
    /// fresh fixture seeded with one task — the pair the
    /// `queue-tab-reader-thread` repro compares between a baseline script
    /// and the same script with one key added, so the difference is that
    /// one key's own doing and nothing a fixture's own setup already paid
    /// for.
    fn process_and_read_counts(name: &str, input: &str) -> (usize, usize) {
        let (repo, _root) = fixture(name);
        write_pending(&repo, "wire", &task_text("wire", "group: demo\n", BODY));
        let groups = listed(&repo);
        let before_proc = crate::repo::runs_here_under(&repo.root);
        let before_reads = super::pending::pending_reads_here_under(&repo.pending_dir());
        screen_exit(&repo, groups, input);
        let after_proc = crate::repo::runs_here_under(&repo.root);
        let after_reads = super::pending::pending_reads_here_under(&repo.pending_dir());
        (after_proc - before_proc, after_reads - before_reads)
    }

    /// Every key on the queue tab starts `git branch --show-current` before
    /// its frame is drawn — see `run_screen_from`'s own call to
    /// `crate::repo::branch_at` at the top of its loop — so one more key
    /// typed must mean one more process started, a `j` that only moves the
    /// group cursor included. A reader thread is expected to make this
    /// difference zero: the key wakes it, but draws from whatever it last
    /// read rather than waiting for a fresh one itself.
    #[test]
    fn a_cursor_key_draws_without_starting_a_process_or_reading_a_file() {
        let (baseline_proc, baseline_reads) =
            process_and_read_counts("queue-reader-thread-cursor-baseline", "");
        let (measured_proc, measured_reads) =
            process_and_read_counts("queue-reader-thread-cursor-measured", "j");

        assert_eq!(
            measured_proc, baseline_proc,
            "a cursor key must start no process of its own"
        );
        assert_eq!(
            measured_reads, baseline_reads,
            "a cursor key must read no file of its own"
        );
    }

    /// The same bug, one key deep in the gate picker instead of on the
    /// group list: opening the picker with `g` already pays for a process
    /// per key, and moving inside it with `j` is one more — `handle_gate_key`
    /// itself reads nothing, so this isolates the cost `run_screen_from`'s
    /// own loop adds around it.
    #[test]
    fn a_key_inside_the_gate_picker_draws_without_starting_a_process_or_reading_a_file() {
        let (baseline_proc, baseline_reads) =
            process_and_read_counts("queue-reader-thread-gate-baseline", "\tg");
        let (measured_proc, measured_reads) =
            process_and_read_counts("queue-reader-thread-gate-measured", "\tgj");

        assert_eq!(
            measured_proc, baseline_proc,
            "a key inside the gate picker must start no process of its own"
        );
        assert_eq!(
            measured_reads, baseline_reads,
            "a key inside the gate picker must read no file of its own"
        );
    }

    /// `QueueReader` actually reading a fresh pending task after it starts
    /// is what the two tests above can never exercise: their scripted input
    /// reports a byte pending on every slice, so `wait_for_key`'s idle path
    /// — the one that calls `refresh_from_reader` while there is nothing
    /// else to do — never runs, and the reader's own thread is the only
    /// other caller. Drives `start_queue_reader` and `refresh_from_reader`
    /// directly instead, the same way `status::mod`'s own
    /// `a_landed_reading_is_noticed_once_and_drawn_from_memory` drives
    /// `Reader` directly for the dispatch tab.
    #[test]
    fn a_landed_reading_updates_groups_and_the_branch() {
        let (repo, _root_guard) = fixture("queue-reader-thread-landed-reading");
        let reader = start_queue_reader(
            repo.clone(),
            repo.root.clone(),
            QueueSnapshot {
                groups: Vec::new(),
                branch: None,
            },
        );
        let mut known = 0;
        let mut groups = Vec::new();
        let mut state = ScreenState::new();

        write_pending(&repo, "wire", &task_text("wire", "group: demo\n", BODY));
        reader.wake();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while groups.is_empty() && std::time::Instant::now() < deadline {
            refresh_from_reader(&reader, &mut known, &mut groups, &mut state);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert!(
            groups.iter().any(|g| g.name == "demo"),
            "the landed reading should have picked up the task written after the reader started"
        );
        assert_eq!(
            state.board_branch.as_deref(),
            Some("plan/demo"),
            "the landed reading should have read the fixture's own branch too"
        );
    }

    /// The bug `refresh_from_reader`'s first version had: it compared a
    /// landed reading's generation against `QueueReader::started_count`
    /// read fresh on every call, rather than against `known` — the ratchet
    /// that only moves when a call actually adopts something, or when an
    /// edit bumps it on purpose. That version rejected every reading
    /// forever, since the very wake a call just sent had already pushed
    /// `started_count` past whatever that reading would ever land on. This
    /// drives the real race the fix (and `known`'s edit-time bump at every
    /// `resume`/`begin_submission` call site in `run_screen_from`) exists
    /// for: a reading already in flight when a submission's own
    /// `groups.retain` lands must not carry the submitted group back once
    /// it finishes, and a reading that starts after it must still land
    /// normally.
    #[test]
    fn a_reading_already_in_flight_when_an_edit_lands_must_not_put_it_back() {
        let (repo, _root_guard) = fixture("queue-reader-thread-race");
        let path = write_pending(&repo, "wire", &task_text("wire", "group: demo\n", BODY));
        let mut groups = listed(&repo);
        assert!(groups.iter().any(|g| g.name == "demo"));

        // `build` blocks, after doing its real read, on every call past the
        // first (the seed) whose own index matches `blocked_call` — so the
        // reading `wake` below starts can be held in flight across the
        // "edit" that follows it, the way a real one could be.
        let blocked_call = 1usize;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let gate = std::sync::Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let (thread_repo, thread_cwd) = (repo.clone(), repo.root.clone());
        let (thread_calls, thread_gate) =
            (std::sync::Arc::clone(&calls), std::sync::Arc::clone(&gate));
        let reader = QueueReader::start(move || {
            let call = thread_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let fresh = build_queue_snapshot(&thread_repo, &thread_cwd);
            if call == blocked_call {
                let (lock, cond) = &*thread_gate;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cond.wait(released).unwrap();
                }
            }
            fresh
        });
        let mut known = 0;
        let mut state = ScreenState::new();

        reader.wake();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while reader.started_count() < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            reader.started_count(),
            1,
            "the reading this test holds in flight should have started"
        );

        // The edit: a submission landing removes "demo" from `groups` and
        // deletes its file, then raises `known` the moment it happens — see
        // `refresh_from_reader`'s own doc comment and the `resume` /
        // `begin_submission` call sites in `run_screen_from`.
        groups.retain(|g| g.name != "demo");
        std::fs::remove_file(&path).unwrap();
        known = known.max(reader.started_count());

        // Release the held reading. It lands now, but it read "demo" before
        // the edit above, so adopting it would carry the submitted group
        // right back onto the screen.
        {
            let (lock, cond) = &*gate;
            *lock.lock().unwrap() = true;
            cond.notify_all();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while reader.latest_with_generation().1 < 1 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        refresh_from_reader(&reader, &mut known, &mut groups, &mut state);
        assert!(
            !groups.iter().any(|g| g.name == "demo"),
            "a reading started before the edit must not put the submitted group back"
        );

        // A reading that starts after the edit is not held back by it: it
        // reads the file's own deletion and lands normally. Checked through
        // `known` rather than `groups` alone — the group is just as absent
        // whether this reading was actually adopted or rejected forever,
        // the failure mode an earlier, buggier `refresh_from_reader` had.
        reader.wake();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while known < 2 {
            if std::time::Instant::now() > deadline {
                panic!("a reading started after the edit never landed");
            }
            refresh_from_reader(&reader, &mut known, &mut groups, &mut state);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            !groups.iter().any(|g| g.name == "demo"),
            "a reading that actually started after the edit should keep agreeing with it"
        );
    }

    /// The routines tab's screen driven over `input`, opened the way
    /// [`routines_tab`] opens it — straight onto the routine list with
    /// nothing ticked — keeping the exit and everything it drew.
    fn routines_exit(repo: &Repo, input: &str) -> (ScreenExit, String) {
        let routines = super::super::routines::list_routines(repo).unwrap();
        let mut state = ScreenState::new();
        state.mode = Mode::Routines(RoutineNav::new());
        let mut input = keys(input);
        let mut out = Vec::new();
        let exit = run_screen_from(
            repo,
            &Pipelines::builtin(),
            &repo.root,
            (Vec::new(), routines),
            state,
            &mut crate::screen::frame_writer::FrameWriter::new(),
            &mut input,
            &mut out,
        )
        .unwrap();
        (exit, String::from_utf8(out).unwrap())
    }

    /// The same, keeping only what it drew.
    fn routines_screen(repo: &Repo, input: &str) -> String {
        routines_exit(repo, input).1
    }

    /// The frame that was actually on screen when the input ran out — every
    /// draw opens with the shared frame writer's own start code, so a
    /// captured transcript holds every frame the screen ever drew, back to
    /// back.
    fn last_frame(drawn: &str) -> &str {
        drawn.rsplit("\x1b[?2026h\x1b[H").next().unwrap_or(drawn)
    }

    /// `←` and `→` no longer move focus between the two panes — `tab` is the
    /// one key for that now, so the same arrows can move between tabs inside
    /// bare `spoolway` without meaning two things on one screen.
    #[test]
    fn only_tab_moves_focus_between_the_two_panes() {
        let (repo, _root_guard) = fixture("screen-focus-tab-only");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        let groups = listed(&repo);
        let mut state = ScreenState::new();

        handle_browse_key(&groups, &mut state, Key::Right);
        assert_eq!(state.focus, Focus::Groups);
        handle_browse_key(&groups, &mut state, Key::Tab);
        assert_eq!(state.focus, Focus::Tasks);
        handle_browse_key(&groups, &mut state, Key::Left);
        assert_eq!(state.focus, Focus::Tasks);
    }

    /// `esc` over the tasks pane goes back to the groups pane and leaves the
    /// group cursor where it was; over the groups pane it does nothing.
    #[test]
    fn esc_returns_focus_from_the_tasks_pane_and_keeps_the_group_cursor() {
        let (repo, _root_guard) = fixture("screen-focus-esc");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        write_pending(&repo, "gate", &task_text("gate", "group: b\n", BODY));
        let groups = listed(&repo);
        let mut state = ScreenState::new();

        handle_browse_key(&groups, &mut state, Key::Down);
        assert_eq!(state.group_cursor, 1);
        handle_browse_key(&groups, &mut state, Key::Esc);
        assert_eq!(state.focus, Focus::Groups);
        assert_eq!(state.group_cursor, 1);
        assert!(matches!(state.mode, Mode::Browsing));

        handle_browse_key(&groups, &mut state, Key::Tab);
        assert_eq!(state.focus, Focus::Tasks);
        handle_browse_key(&groups, &mut state, Key::Esc);
        assert_eq!(state.focus, Focus::Groups);
        assert_eq!(state.group_cursor, 1);
    }

    /// The key line follows focus: hosted, both lines lead with `space` then
    /// `enter`; the groups pane's line names the way into the tasks pane and
    /// no `g` or `o`; the tasks pane's line puts them first among its own
    /// keys and names the way back; `esc` brings the first line back.
    #[test]
    fn hosted_the_key_line_follows_focus_between_the_two_panes() {
        use crate::screen::shell::{Hosting, Tab};
        let (repo, _root_guard) = fixture("screen-hosted-footer-focus");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        let _hosting = Hosting::open(Tab::Queue);
        let groups_line = "[space] select   [enter] queue   [f] find   [tab] tasks   [t] trial   \
                           [s] save as routine   [h] show done tasks   [q] quit";
        let tasks_line = "[space] select   [enter] queue   [g] gate   [o] open task   [tab] groups   \
                          [f] find   [t] trial   [s] save as routine   \
                          [h] show done tasks   [q] quit";

        // `key_hint` opens every key line on one column of indent.
        let mut state = ScreenState::new();
        let line = crate::status::strip_ansi(&footer(&state));
        assert_eq!(line, format!(" {groups_line}"));
        state.focus = Focus::Tasks;
        let line = crate::status::strip_ansi(&footer(&state));
        assert_eq!(line, format!(" {tasks_line}"));

        let drawn = screen(&repo, listed(&repo), "\t");
        let last = crate::status::strip_ansi(last_frame(&drawn));
        assert!(
            last.contains("[g] gate   [o] open task   [tab] groups"),
            "{last}"
        );
        let drawn = screen(&repo, listed(&repo), "\t\x1b");
        let last = crate::status::strip_ansi(last_frame(&drawn));
        assert!(last.contains("[f] find   [tab] tasks"), "{last}");
        assert!(!last.contains("[g] gate"), "{last}");
    }

    /// Hosted, the arrows leave for the neighbouring tab from the tasks pane
    /// too — `tab` moving focus there does not hand them to the screen.
    #[test]
    fn hosted_the_arrows_leave_the_tab_from_the_tasks_pane_too() {
        use crate::screen::shell::{Hosting, Leave, Tab, Toward};
        let (repo, _root_guard) = fixture("screen-hosted-leave-tasks");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        let _hosting = Hosting::open(Tab::Queue);

        let (exit, _) = screen_exit(&repo, listed(&repo), "\t\x1b[D");
        assert_eq!(exit, ScreenExit::Leave(Leave::Switch(Toward::Left)));
        let (exit, _) = screen_exit(&repo, listed(&repo), "\t\x1b[C");
        assert_eq!(exit, ScreenExit::Leave(Leave::Switch(Toward::Right)));
    }

    /// Hosted as bare `spoolway`'s queue tab, `←`, `→` and `q` while
    /// browsing hand the screen back to the shell, and the frame opens under
    /// the strip.
    #[test]
    fn hosted_the_arrows_and_q_leave_the_tab_while_browsing() {
        use crate::screen::shell::{Hosting, Leave, Tab, Toward};
        let (repo, _root_guard) = fixture("screen-hosted-leave");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        let _hosting = Hosting::open(Tab::Queue);

        let (exit, drawn) = screen_exit(&repo, listed(&repo), "\x1b[D");
        assert_eq!(exit, ScreenExit::Leave(Leave::Switch(Toward::Left)));
        assert!(last_frame(&drawn).contains("dispatch"), "{drawn}");
        assert!(last_frame(&drawn).contains("[q] quit"), "{drawn}");

        let (exit, _) = screen_exit(&repo, listed(&repo), "\x1b[C");
        assert_eq!(exit, ScreenExit::Leave(Leave::Switch(Toward::Right)));
        let (exit, _) = screen_exit(&repo, listed(&repo), "q");
        assert_eq!(exit, ScreenExit::Leave(Leave::Quit));
    }

    /// Hosted as bare `spoolway`'s routines tab, `←`, `→` and `q` over the
    /// routine list hand the screen back to the shell, from either pane —
    /// and the list's key line names `q` and no `esc`, as the routines tab's
    /// mockup draws it.
    #[test]
    fn hosted_the_arrows_and_q_leave_the_routines_tab_and_its_line_names_q() {
        use crate::screen::shell::{Hosting, Leave, Tab, Toward};
        let (repo, _root_guard) = fixture("screen-hosted-routines-leave");
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        let _hosting = Hosting::open(Tab::Routines);

        let (exit, drawn) = routines_exit(&repo, "q");
        assert_eq!(exit, ScreenExit::Leave(Leave::Quit));
        assert!(
            last_frame(&drawn)
                .contains(" [space] select   [enter] queue   [n] new job   [x] delete   [tab] tasks   [q] quit"),
            "{drawn}"
        );
        assert!(last_frame(&drawn).contains("[routines]"), "{drawn}");
        let (exit, _) = routines_exit(&repo, "\x1b[D");
        assert_eq!(exit, ScreenExit::Leave(Leave::Switch(Toward::Left)));
        let (exit, _) = routines_exit(&repo, "\x1b[C");
        assert_eq!(exit, ScreenExit::Leave(Leave::Switch(Toward::Right)));
        let (exit, _) = routines_exit(&repo, "\t\x1b[C");
        assert_eq!(exit, ScreenExit::Leave(Leave::Switch(Toward::Right)));
    }

    /// A popup over the routines tab keeps `←`, `→` and `q` for itself, the
    /// rule the queue tab keeps: here the headless refusal `o` opens over
    /// the tasks pane, which reads nothing but `enter`.
    #[test]
    fn hosted_a_popup_over_the_routines_tab_keeps_the_arrows_and_q() {
        use crate::screen::shell::{Hosting, Tab};
        let (mut repo, _root_guard) = fixture("screen-hosted-routines-popup");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        let _hosting = Hosting::open(Tab::Routines);

        let (exit, drawn) = routines_exit(&repo, "\to\x1b[C\x1b[Dq");
        assert_eq!(exit, ScreenExit::Quit, "the input ran out under the popup");
        assert!(last_frame(&drawn).contains("┌─ open task "), "{drawn}");
    }

    /// Inside a sub-mode the arrows and `q` stay with it even when hosted:
    /// the filter reads `q` into its query and holds the arrows — it does
    /// not leave the tab.
    #[test]
    fn hosted_a_sub_mode_keeps_the_arrows_and_q_for_itself() {
        use crate::screen::shell::{Hosting, Tab};
        let (repo, _root_guard) = fixture("screen-hosted-sub-mode");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        let _hosting = Hosting::open(Tab::Queue);

        let (exit, _) = screen_exit(&repo, listed(&repo), "fq\x1b[D");
        assert_eq!(
            exit,
            ScreenExit::Quit,
            "the input ran out inside the filter"
        );
    }

    /// Where the old standalone screen printed its opening message and
    /// ended, the queue tab holds it on screen under the strip, as a popup
    /// over the tab — every key but `enter` is the popup's to ignore, `enter`
    /// closes it onto the ordinary screen, and `←` then leaves.
    #[test]
    fn the_queue_tab_holds_its_opening_message_instead_of_ending() {
        use crate::screen::shell::{Hosting, Leave, Tab, Toward};
        let (repo, _root_guard) = fixture("queue-tab-opening-message");
        std::fs::write(
            repo.pending_dir().join("no-group.md"),
            "---\nid: stray\ntitle: stray\n---\n## Goal\n\nx\n",
        )
        .unwrap();
        let _hosting = Hosting::open(Tab::Queue);

        let mut input = keys("x\x1b[D\r\x1b[D");
        let mut out = Vec::new();
        let leave = queue_tab(
            &repo,
            &Pipelines::builtin(),
            &repo.root,
            crate::screen::shell::OnOpen::default(),
            &mut crate::screen::frame_writer::FrameWriter::new(),
            &mut input,
            &mut out,
        )
        .unwrap();
        assert_eq!(leave, Leave::Switch(Toward::Left));
        let drawn = String::from_utf8(out).unwrap();
        let first = drawn.split("\x1b[?2026h\x1b[H").nth(1).unwrap();
        assert!(first.contains("┌─ nothing to queue "), "{first}");
        assert!(first.contains("no-group.md"), "{first}");
        assert!(first.contains("[enter] confirm"), "{first}");
        assert!(first.contains("─ groups"), "the tab under it: {first}");
        assert!(first.contains("dispatch"), "under the strip: {first}");
    }

    /// What the screen opens with — the sync notice, then the update notice
    /// — is shown over the queue tab in that order, each closed by `enter`
    /// alone, before the tab is the person's. Dismissing the sync notice
    /// writes nothing: no stamp appears where `sync` would record one.
    #[test]
    fn the_queue_tab_opens_with_the_sync_notice_then_the_update_notice() {
        use crate::screen::shell::{Hosting, OnOpen, Tab};
        let (repo, _root_guard) = fixture("queue-tab-on-open");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        let _hosting = Hosting::open(Tab::Queue);
        let on_open = OnOpen {
            sync: Some(crate::screen::notice(
                "update installed",
                crate::gate::LINE,
                "[enter] dismiss",
                crate::screen::NOTICE_WRAP,
            )),
            ignored: None,
            update: Some("Update available: 0.42.0. Run \"spoolway update\"".to_string()),
        };

        let mut input = keys("x\r");
        let mut out = Vec::new();
        queue_tab(
            &repo,
            &Pipelines::builtin(),
            &repo.root,
            on_open,
            &mut crate::screen::frame_writer::FrameWriter::new(),
            &mut input,
            &mut out,
        )
        .unwrap();
        let drawn = String::from_utf8(out).unwrap();
        let frames: Vec<&str> = drawn.split("\x1b[?2026h\x1b[H").skip(1).collect();
        assert!(frames[0].contains("┌─ update installed "), "{}", frames[0]);
        assert!(frames[0].contains(crate::gate::LINE), "{}", frames[0]);
        assert!(frames[0].contains("[enter] dismiss"), "{}", frames[0]);
        assert!(frames[0].contains("─ groups"), "{}", frames[0]);
        assert!(
            !crate::sync::stamp_path(&repo.home).exists(),
            "dismissing the notice must not sync"
        );
        let last = frames.last().unwrap();
        assert!(last.contains("┌─ update available "), "{last}");
        assert!(
            last.contains("Update available: 0.42.0. Run \"spoolway update\""),
            "{last}"
        );
        assert!(last.contains("[enter] confirm"), "{last}");
    }

    /// The "override ignored" popup the screen opens on takes every key
    /// until `enter` closes it — `q` and `←` behind it must not quit or leave
    /// with the popup unread — and the tab is the person's again after it.
    #[test]
    fn the_queue_tab_opens_with_the_ignored_overrides_and_enter_closes_them() {
        use crate::screen::shell::{Hosting, Leave, OnOpen, Tab, Toward};
        let (repo, _root_guard) = fixture("queue-tab-ignored");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        let _hosting = Hosting::open(Tab::Queue);
        let on_open = OnOpen {
            ignored: crate::commands::IgnoredPopup::from_rows(&[crate::commands::OverrideRow {
                target: "pipelines/release.yml".into(),
                kind: "patch",
                overrides: String::new(),
                ignored: vec![crate::overrides::Ignored {
                    target: "pipelines/release.yml step `publish`".into(),
                    fields: "publish.agent, publish.model".into(),
                    reason: "names both `run:` and `agent:` — a step runs a process or a \
                             model, not both"
                        .into(),
                }],
            }]),
            ..OnOpen::default()
        };

        let mut input = keys("q\x1b[D\r\x1b[D");
        let mut out = Vec::new();
        let leave = queue_tab(
            &repo,
            &Pipelines::builtin(),
            &repo.root,
            on_open,
            &mut crate::screen::frame_writer::FrameWriter::new(),
            &mut input,
            &mut out,
        )
        .unwrap();
        assert_eq!(leave, Leave::Switch(Toward::Left), "the tab works again");
        let drawn = String::from_utf8(out).unwrap();
        let frames: Vec<&str> = drawn.split("\x1b[?2026h\x1b[H").skip(1).collect();
        assert!(frames[0].contains("┌─ override ignored "), "{}", frames[0]);
        assert!(
            frames[0].contains("pipelines/release.yml   step publish   agent, model"),
            "{}",
            frames[0]
        );
        // Wrapped to the frame it is drawn over, so its right border is
        // still on screen however narrow the tab is drawn.
        let rows: Vec<&str> = frames[0].lines().collect();
        let top = rows
            .iter()
            .find(|r| r.contains("┌─ override ignored "))
            .unwrap();
        assert!(top.contains('┐'), "{}", frames[0]);
        assert!(
            frames[0].contains("─ groups"),
            "over the tab: {}",
            frames[0]
        );
        let last = frames.last().unwrap();
        assert!(!last.contains("override ignored"), "closed: {last}");
    }

    /// Driven through [`run_screen`] directly rather than through the tab,
    /// so nothing hosts the screen: no strip, no `q` on the key line, and
    /// `←` leaves nothing.
    #[test]
    fn unhosted_the_screen_draws_no_strip_and_the_arrows_do_not_leave() {
        let (repo, _root_guard) = fixture("screen-unhosted");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));

        let (exit, drawn) = screen_exit(&repo, listed(&repo), "\x1b[Dq");
        assert_eq!(exit, ScreenExit::Quit);
        assert!(!last_frame(&drawn).contains("dispatch"), "{drawn}");
        assert!(!last_frame(&drawn).contains("[q] quit"), "{drawn}");
    }

    /// `with_gate` is the one place a gate chosen on the screen reaches a
    /// task — in memory only, since the file in the pending
    /// directory is never rewritten to record one.
    #[test]
    fn with_gate_inserts_after_the_opening_fence() {
        let doc = task_text("wire", "group: demo\n", BODY);
        let gated = with_gate(&doc, "handover");
        assert!(gated.starts_with("---\ngate_at: handover\n"), "{gated}");
        assert!(gated.contains("id: wire\n"), "{gated}");
    }

    /// The screen's own choice always wins, even over a `gate_at` a task
    /// already happened to carry.
    #[test]
    fn with_gate_replaces_a_gate_the_task_already_carried() {
        let doc = task_text("wire", "group: demo\ngate_at: fix\n", BODY);
        let gated = with_gate(&doc, "handover");
        assert_eq!(gated.matches("gate_at:").count(), 1, "{gated}");
        assert!(gated.contains("gate_at: handover"), "{gated}");
        assert!(!gated.contains("gate_at: fix"), "{gated}");
    }

    #[test]
    fn with_gate_leaves_a_task_with_no_fence_untouched() {
        assert_eq!(with_gate("not a task", "handover"), "not a task");
    }

    /// A block-list `depends_on:` — the form `pending::depends_on` reads
    /// fine — is replaced whole, items included, and a `<key>:` in the body
    /// is not the frontmatter's to touch (jobs review finding 4).
    #[test]
    fn with_frontmatter_field_replaces_a_block_list_and_leaves_the_body_alone() {
        let doc = "---\nid: wire\ndepends_on:\n  - login\n  - sessions\ngroup: demo\n---\n\
                   ## Goal\n\n```\nid: in-a-sample\ndepends_on:\n  - also-a-sample\n```\n";

        let out = with_frontmatter_field(doc, "depends_on", "[login]");
        assert_eq!(
            out,
            "---\ndepends_on: [login]\nid: wire\ngroup: demo\n---\n\
             ## Goal\n\n```\nid: in-a-sample\ndepends_on:\n  - also-a-sample\n```\n"
        );
        assert_eq!(super::pending::depends_on(&out), vec!["login"]);

        let out = with_frontmatter_field(doc, "id", "wire-2");
        assert!(
            out.starts_with("---\nid: wire-2\ndepends_on:\n  - login\n"),
            "{out}"
        );
        assert!(out.contains("id: in-a-sample"), "{out}");
    }

    /// The task pane's `Depends on:` row reads `depends_on` straight off a
    /// task, without validating it — and reads nothing else. The
    /// task here carries a `touches` too, to hold the pane to not
    /// showing it.
    #[test]
    fn depends_on_reads_the_list_off_the_task() {
        let doc = task_text(
            "wire",
            "group: demo\ntouches: [src/a.rs, src/b.rs]\ndepends_on: [login]\n",
            BODY,
        );
        assert_eq!(super::pending::depends_on(&doc), vec!["login"]);
    }

    /// A task naming no `depends_on` — or carrying no fence at all — reads as
    /// an empty list rather than an error: the pane shows `Depends on:  -`
    /// for the first case and never panics on either.
    #[test]
    fn depends_on_defaults_to_an_empty_list() {
        let doc = task_text("wire", "group: demo\n", BODY);
        assert_eq!(super::pending::depends_on(&doc), Vec::<String>::new());
        assert_eq!(
            super::pending::depends_on("not a task"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn pad_to_pads_short_text_and_truncates_long_text() {
        assert_eq!(pad_to("abc", 5), "abc  ");
        assert_eq!(pad_to("abcdefgh", 5), "abcde");
        assert_eq!(pad_to("abcde", 5), "abcde");
    }

    /// The border and every row `draw_two_panes` writes have to end at the
    /// same column: a corner one character short of a row below it is
    /// exactly the off-by-one this test exists to catch — see the `fix`
    /// round that found it in review with nothing here to have caught it
    /// first. Renders a real group and a real task, through
    /// `groups_pane_lines` and `tasks_pane_lines`, the same as `draw` does.
    #[test]
    fn draw_two_panes_borders_and_rows_are_all_the_same_width() {
        let (repo, _root_guard) = fixture("panes-width");
        write_pending(
            &repo,
            "wire",
            &task_text(
                "wire",
                "group: one\ntouches: [src/wire.rs]\ndepends_on: [login]\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();
        let shown = visible(&groups, state.hide_scope);

        for layout in [
            Layout {
                left: LEFT_PANE_WIDTH,
                right: RIGHT_PANE_WIDTH,
                rows: None,
            },
            Layout {
                left: MIN_LEFT_PANE,
                right: MIN_RIGHT_PANE,
                rows: Some(3),
            },
            Layout {
                left: MAX_LEFT_PANE,
                right: 120,
                rows: Some(40),
            },
        ] {
            let (left, _) = groups_pane_lines(&groups, &shown, &state, layout.left);
            let (right, _, _) = tasks_pane_lines(&groups, &pipelines, &state, layout.right);
            assert!(!left.is_empty(), "expected the one group to have a row");
            assert!(
                right.len() >= 3,
                "expected a blank line, the task row and its Pipeline: row, got {right:?}"
            );

            let rendered = two_pane_frame(&left, &right, "pending", "one", layout).join("\n");

            let widths: Vec<usize> = rendered
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| line.chars().count())
                .collect();
            assert!(
                widths.len() >= 2,
                "expected a top border and at least one row, got {rendered:?}"
            );
            let first = widths[0];
            assert!(
                widths.iter().all(|w| *w == first),
                "every border and row must be the same width at {layout:?}, \
                 got {widths:?} in:\n{rendered}"
            );
        }
    }

    /// A `right_title` too long to fit is cut to the pane rather than
    /// pushing the top border's corner past the rows under it.
    #[test]
    fn draw_two_panes_survives_a_title_longer_than_the_pane() {
        let layout = Layout {
            left: LEFT_PANE_WIDTH,
            right: RIGHT_PANE_WIDTH,
            rows: None,
        };
        let left = vec!["> [ ] demo-group".to_string()];
        let right = vec!["  wire   small   low".to_string()];
        let long_title = "a".repeat(RIGHT_PANE_WIDTH * 2);
        let rendered = two_pane_frame(&left, &right, "pending", &long_title, layout).join("\n");
        let mut lines = rendered.lines();
        let top = lines.next().unwrap();
        assert!(top.ends_with('┐'));
        assert_eq!(
            top.chars().count(),
            lines.next().unwrap().chars().count(),
            "the top border must end where the row under it does:\n{rendered}"
        );
    }

    /// The panes fill the terminal they are drawn in: a measured layout gets
    /// exactly the rows it asked for, however little there is to show.
    #[test]
    fn a_measured_layout_draws_every_row_it_was_given() {
        let layout = Layout {
            left: MIN_LEFT_PANE,
            right: MIN_RIGHT_PANE,
            rows: Some(12),
        };
        let frame = two_pane_frame(
            &["> [ ] only-group".to_string()],
            &[],
            "pending",
            "only",
            layout,
        );
        assert_eq!(
            frame.len(),
            14,
            "expected two borders around 12 rows:\n{}",
            frame.join("\n")
        );
    }

    /// Every width the layout hands out leaves room for both panes, and the
    /// two of them plus the border add up to the terminal they were measured
    /// against — including at widths far narrower than the minimums.
    #[test]
    fn a_layout_splits_the_width_it_is_given_between_two_panes() {
        for width in [20usize, 40, 80, 100, 173, 400] {
            let layout = layout_for(width, 24);
            assert!(layout.left >= MIN_LEFT_PANE, "left too narrow at {width}");
            assert!(layout.left <= MAX_LEFT_PANE, "left too wide at {width}");
            assert!(
                layout.right >= MIN_RIGHT_PANE,
                "right too narrow at {width}"
            );
            assert_eq!(layout.rows, Some(24 - PANE_CHROME_ROWS));
            if width >= MIN_LEFT_PANE + MIN_RIGHT_PANE + PANE_CHROME_COLUMNS + SPARE_COLUMN {
                assert_eq!(
                    layout.left + layout.right + PANE_CHROME_COLUMNS + SPARE_COLUMN,
                    width,
                    "the panes and the border must fill the terminal at {width}"
                );
            }
        }
        assert_eq!(layout_for(100, 1).rows, Some(1), "one row is still a row");
    }

    /// The queue's key line is wider than a 100-column terminal, and every
    /// row it wraps onto has to come off the panes, or the frame's top row —
    /// bare `spoolway`'s tab strip — is scrolled off the screen.
    #[test]
    fn wrapped_rows_counts_the_rows_a_key_line_wraps_onto() {
        let line = footer(&ScreenState::new());
        let columns = crate::status::strip_ansi(&line).chars().count();
        assert!(columns > 100, "the key line fits in 100 columns now");
        assert_eq!(wrapped_rows(&line, 100), 2);
        assert_eq!(
            wrapped_rows(&line, columns),
            1,
            "exactly as wide is one row"
        );
        assert_eq!(wrapped_rows(&line, columns - 1), 2);
        assert_eq!(wrapped_rows("", 100), 1, "an empty line is still a row");
    }

    /// `Items` for a pane of `len` lines where every line is an item and the
    /// marker names no noun — what the jobs screen passes to [`window`].
    fn per_line(len: usize) -> Vec<usize> {
        (0..len).collect()
    }

    /// A pane taller than the terminal scrolls to the highlighted block and
    /// says how much is out of sight, rather than letting the frame run off
    /// the bottom of the screen.
    ///
    /// Passed one item per line and no noun, the way the jobs screen calls
    /// it, so this also pins that screen's marker text exactly as it was
    /// before panes counted items: one direction only, below whenever
    /// anything is, even with lines hidden above as well.
    #[test]
    fn a_pane_taller_than_its_rows_scrolls_to_the_focused_block() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();
        let starts = per_line(lines.len());
        let items = Items {
            starts: &starts,
            noun: None,
        };

        let top = window(&lines, items, (0, 0), Some(5), 40);
        assert_eq!(top.len(), 5);
        assert_eq!(top[0], "line 0");
        assert_eq!(top[4], "↓ 16 below");

        let middle = window(&lines, items, (11, 12), Some(5), 40);
        assert_eq!(middle.len(), 5);
        assert!(
            middle.contains(&"line 11".to_string()) && middle.contains(&"line 12".to_string()),
            "the focused block must stay in view, got {middle:?}"
        );
        assert_eq!(middle[4], "↓ 7 below", "the jobs screen never shows above");

        let bottom = window(&lines, items, (19, 19), Some(5), 40);
        assert_eq!(bottom[3], "line 19");
        assert_eq!(bottom[4], "↑ 16 above");

        assert_eq!(
            window(&lines, items, (0, 0), None, 40).len(),
            20,
            "no terminal, no cut"
        );
        let short = per_line(3);
        let items = Items {
            starts: &short,
            noun: None,
        };
        assert_eq!(window(&lines[..3], items, (0, 0), Some(9), 40).len(), 3);
    }

    /// The tasks pane's marker counts tasks, not lines, at every scroll
    /// position: a task cut at the bottom edge counts as below, one cut at
    /// the top edge as above, and the blank row ahead of each task is never
    /// counted. The truth each marker is checked against is read off where
    /// each task's header and description rows sit, not off the starts the
    /// pane handed `window`.
    #[test]
    fn the_tasks_pane_marker_counts_tasks_at_every_scroll_position() {
        let (repo, _root_guard) = fixture("scroll-counts-tasks");
        let ids: Vec<String> = (1..=6).map(|i| format!("task-{i}")).collect();
        for (i, id) in ids.iter().enumerate() {
            let depends = match i {
                0 => String::new(),
                _ => format!("depends_on: [{}]\n", ids[i - 1]),
            };
            write_pending(
                &repo,
                id,
                &task_text(id, &format!("group: one\n{depends}"), BODY),
            );
        }
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();
        let (lines, starts, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        assert_eq!(starts.len(), ids.len(), "one start per task: {lines:?}");

        // Where each task's header and its last row, the description, sit.
        let header = |id: &str| lines.iter().position(|l| l.trim_end() == format!("  {id}"));
        let last = |id: &str| {
            lines
                .iter()
                .position(|l| l.contains("Description:") && l.contains(&format!("{id}, done")))
        };
        let blocks: Vec<(usize, usize)> = ids
            .iter()
            .map(|id| (header(id).unwrap(), last(id).unwrap()))
            .collect();

        // Every scroll position: a focus on each line in turn walks the
        // view from the top of the pane to its bottom one line at a time.
        for rows in [4, 7, 8, 13] {
            for line in 0..lines.len() {
                let shown = window(
                    &lines,
                    Items {
                        starts: &starts,
                        noun: Some("task"),
                    },
                    (line, line),
                    Some(rows),
                    200,
                );
                // Where the view sits, found by matching its own rows
                // against the pane's, and checked to be the only place
                // they match so the count below is not read off a guess.
                let view = &shown[..shown.len() - 1];
                let at: Vec<usize> = (0..=lines.len() - view.len())
                    .filter(|&o| lines[o..o + view.len()] == *view)
                    .collect();
                assert_eq!(at.len(), 1, "rows {rows}, focus line {line}: {view:?}");
                let (top, bottom) = (at[0], at[0] + view.len());
                let above = blocks.iter().filter(|&&(first, _)| first < top).count();
                let below = blocks
                    .iter()
                    .filter(|&&(first, end)| first >= top && end >= bottom)
                    .count();
                let marker = shown.last().unwrap();
                let expected = marker_row(above, below, Some("task"), 200);
                assert_eq!(
                    marker, &expected,
                    "rows {rows}, focus line {line}: view {view:?}"
                );
            }
        }
    }

    /// The marker names what it counts, singular for one, and shows both
    /// directions on one row only when both have something hidden.
    #[test]
    fn the_marker_row_names_its_items_and_shows_both_directions() {
        assert_eq!(
            marker_row(11, 19, Some("group"), 80),
            "↑ 11 groups above · ↓ 19 groups below"
        );
        assert_eq!(marker_row(0, 4, Some("task"), 80), "↓ 4 tasks below");
        assert_eq!(marker_row(0, 1, Some("task"), 80), "↓ 1 task below");
        assert_eq!(
            marker_row(3, 1, Some("task"), 80),
            "↑ 3 tasks above · ↓ 1 task below"
        );
        assert_eq!(marker_row(2, 0, Some("folder"), 80), "↑ 2 folders above");
        assert_eq!(marker_row(0, 0, Some("group"), 80), "");
    }

    /// Too narrow for the full row, the marker drops the noun, then the
    /// words, and draws the first form that fits — never a row wider than
    /// the narrowest pane the screen ever draws, where `pad_to` would cut
    /// it off without a sign.
    #[test]
    fn the_marker_row_shortens_itself_to_fit_the_pane() {
        assert_eq!(
            marker_row(11, 19, Some("group"), MIN_LEFT_PANE),
            "↑ 11 above · ↓ 19 below"
        );
        assert_eq!(marker_row(11, 19, Some("group"), 12), "↑ 11 · ↓ 19");
        for noun in ["group", "task", "folder"] {
            for above in [0, 1, 9, 11, 99, 111] {
                for below in [0, 1, 9, 19, 99, 199] {
                    let row = marker_row(above, below, Some(noun), MIN_LEFT_PANE);
                    assert!(
                        row.chars().count() <= MIN_LEFT_PANE,
                        "{row:?} is wider than {MIN_LEFT_PANE} columns"
                    );
                }
            }
        }
    }

    /// The groups pane counts groups: the blank row between two states and
    /// the filter's own `find:` row are lines, never groups out of sight.
    #[test]
    fn the_groups_pane_marker_never_counts_separators_or_the_find_row() {
        let (repo, _root_guard) = fixture("scroll-counts-groups");
        for i in 0..4 {
            let id = format!("g{i}");
            write_pending(&repo, &id, &task_text(&id, &format!("group: {id}\n"), BODY));
        }
        // Two more already done, so a separator splits the list in two.
        already_done(&repo, "g4", "g4");
        already_done(&repo, "g5", "g5");
        let groups = listed(&repo);
        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusDone;
        let shown = visible(&groups, state.hide_scope);
        assert_eq!(shown.len(), 6, "every group has to be on screen for this");
        let (lines, starts) = groups_pane_lines(&groups, &shown, &state, 30);
        assert!(
            lines.iter().any(String::is_empty),
            "the fixture must draw a separator: {lines:?}"
        );
        assert_eq!(starts.len(), shown.len());
        assert!(starts.iter().all(|&s| !lines[s].is_empty()));

        // Scrolled to the very bottom: every group but the last two rows'
        // worth is above, and the separator is not one of them.
        let rows = 3;
        let bottom = window(
            &lines,
            Items {
                starts: &starts,
                noun: Some("group"),
            },
            (lines.len() - 1, lines.len() - 1),
            Some(rows),
            80,
        );
        assert_eq!(
            bottom[rows - 1],
            format!("↑ {} groups above", shown.len() - (rows - 1))
        );

        state.mode = Mode::Filter;
        let (lines, starts) = groups_pane_lines(&groups, &shown, &state, 30);
        assert!(lines[0].starts_with("find:"), "{lines:?}");
        assert!(!starts.contains(&0), "the find: row is not a group");
        let bottom = window(
            &lines,
            Items {
                starts: &starts,
                noun: Some("group"),
            },
            (lines.len() - 1, lines.len() - 1),
            Some(rows),
            80,
        );
        assert_eq!(
            bottom[rows - 1],
            format!("↑ {} groups above", shown.len() - (rows - 1))
        );
    }

    /// Selecting a group queues every task in it. A group's tasks are
    /// one chain, and half a chain in the queue is a task waiting on a
    /// dependency nobody sent.
    #[test]
    fn selecting_a_group_queues_every_task_in_it() {
        let (repo, _root_guard) = fixture("screen-whole-group");
        write_pending_two(
            &repo,
            "first",
            &task_text("first", "group: one\ntouches: [src/a.rs]\n", BODY),
            "second",
            &task_text(
                "second",
                "group: one\ntouches: [src/b.rs]\ndepends_on: [first]\n",
                BODY,
            ),
        );
        let groups = listed(&repo);

        // One space on the group, enter to submit it.
        screen(&repo, groups, " \r");

        assert!(repo.queue_dir().join("first.md").exists(), "first task");
        assert!(repo.queue_dir().join("second.md").exists(), "second task");
    }

    /// A group the queue already holds is never on the screen, at either
    /// setting of `h`, so there is no row for `space` to select: its tasks
    /// are already queued, and the dispatch tab is where they are watched.
    #[test]
    fn a_queued_group_cannot_be_selected_again() {
        let (repo, _root_guard) = fixture("screen-queued-group");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);
        let mut state = ScreenState::new();

        for scope in [HideScope::Pending, HideScope::PlusDone] {
            state.hide_scope = scope;
            assert!(
                visible(&groups, state.hide_scope).is_empty(),
                "a queued group must not be on screen at {scope:?}"
            );
            handle_browse_key(&groups, &mut state, Key::Char(' '));
            assert!(
                state.selected.is_empty(),
                "a queued group must not be selectable"
            );
            let (rows, _) = groups_pane_lines(&groups, &[], &state, 30);
            assert_eq!(
                rows,
                vec!["  nothing to queue".to_string()],
                "a queued group is not counted as hidden either: {rows:?}"
            );
        }
    }

    /// The blank row between the queueable and done halves: drawn once,
    /// exactly at the boundary, never carrying the cursor marker whichever
    /// group it is on — and gone entirely once only one half is on screen,
    /// so a project with nothing done yet gets no dangling blank line at
    /// the bottom of the pane.
    #[test]
    fn groups_pane_lines_draws_one_unmarked_separator_between_the_two_groups() {
        let (repo, _root_guard) = fixture("screen-separator-row");
        write_pending(&repo, "wire", &task_text("wire", "group: unqueued\n", BODY));
        already_done(&repo, "cook", "cook");
        let groups = listed(&repo);

        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusDone;
        let shown = visible(&groups, state.hide_scope);
        assert_eq!(shown.len(), 2, "both groups have to be on screen for this");

        // The cursor is on the done group (`group_cursor` addresses
        // `shown`, index 1) — proving the marker still lands past the
        // separator, on the row it belongs to, not on the blank one ahead
        // of it.
        state.group_cursor = 1;
        let (rows, _) = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows.len(),
            3,
            "one group, a blank separator, one more group: {rows:?}"
        );
        assert_eq!(rows[1], "", "the separator itself is a blank row: {rows:?}");
        assert!(
            rows[2].starts_with('>'),
            "the cursor on the done group must land past the separator: {rows:?}"
        );

        // With only one half on screen — `h` pressed again, hiding the done
        // group — there is no boundary left to draw a row for at all.
        state.hide_scope = HideScope::Pending;
        state.group_cursor = 0;
        let shown = visible(&groups, state.hide_scope);
        let (rows, _) = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows.len(),
            1,
            "one group alone draws no separator: {rows:?}"
        );
        assert!(!rows[0].is_empty(), "{rows:?}");
    }

    /// With one group in each of the three states and done groups shown, the
    /// queued group is still left out — so the pane draws one separator, not
    /// two, and `group_cursor` on the done group shifts past just that one.
    #[test]
    fn a_queued_group_leaves_no_separator_of_its_own_once_done_groups_show() {
        let (repo, _root_guard) = fixture("screen-two-separators");
        write_pending(&repo, "wire", &task_text("wire", "group: unqueued\n", BODY));
        write_pending(&repo, "cook", &task_text("cook", "group: cook\n", BODY));
        already_queued(&repo, "cook");
        already_done(&repo, "shipped", "shipped");
        let groups = listed(&repo);
        assert_eq!(groups.len(), 3, "one group per state");

        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusDone;
        let shown = visible(&groups, state.hide_scope);
        assert_eq!(shown.len(), 2, "the queued group is never on screen");

        let (rows, _) = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows.len(),
            3,
            "two groups plus one blank separator: {rows:?}"
        );
        assert_eq!(rows[1], "", "the one separator: {rows:?}");
        assert!(rows[2].contains("shipped"), "{rows:?}");

        // `group_cursor` addresses `shown` directly (index 1, the done
        // group), never a separator — so it lands on the third line.
        state.group_cursor = 1;
        assert_eq!(
            group_line_index(state.group_cursor, &boundary_for(&shown, &state)),
            2
        );
    }

    /// The mockup's exact tail: the bare word `queued`, no timestamp.
    #[test]
    fn the_queued_tail_is_the_bare_word() {
        let (repo, _root_guard) = fixture("screen-queued-tail");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);

        let state = ScreenState::new();
        // No setting of `h` puts a queued group on screen, but the pane is
        // still sized for `group_tail`'s widest tail — see
        // `QUEUED_TAIL_COLUMNS` — so the row is drawn from every group here.
        let shown: Vec<&Group> = groups.iter().collect();
        let (rows, _) = groups_pane_lines(&groups, &shown, &state, 40);
        assert!(
            rows[0].ends_with(" queued"),
            "expected the pane to show the bare ' queued' tail, got {rows:?}"
        );
    }

    /// The regression this guards: a queued row's name used to shrink to
    /// one or two unreadable characters once the tail carried a timestamp —
    /// reproduced live at a 74-column terminal, an ordinary width, not a
    /// narrow one. The name now keeps its `MIN_NAME_COLUMN` floor regardless
    /// of the tail, and every mockup-length name fits inside it whole. The
    /// timestamp is gone, but the floor stays, since a wide filtered field or
    /// a future tail could still crowd the name otherwise.
    #[test]
    fn a_queued_name_stays_legible_at_an_ordinary_terminal_width() {
        let (repo, _root_guard) = fixture("screen-queued-name-width");
        for name in ["bound-loops", "queue-open", "state-paths"] {
            write_pending(
                &repo,
                name,
                &task_text(name, &format!("group: {name}\n"), BODY),
            );
            already_queued(&repo, name);
        }
        let groups = listed(&repo);
        let state = ScreenState::new();
        // No setting of `h` puts a queued group on screen, but the pane is
        // still sized for `group_tail`'s widest tail — see
        // `QUEUED_TAIL_COLUMNS` — so the row is drawn from every group here.
        let shown: Vec<&Group> = groups.iter().collect();

        let layout = layout_for(74, 24);
        let (rows, _) = groups_pane_lines(&groups, &shown, &state, layout.left);
        for (group, row) in groups.iter().zip(&rows) {
            assert!(
                row.contains(&group.name),
                "expected the whole name {:?} to survive a {}-column left pane, got {row:?}",
                group.name,
                layout.left
            );
        }
    }

    /// The other end of the same regression: even at the left pane's widest
    /// setting, the name budget must not shrink below the 33 characters
    /// `MAX_LEFT_PANE` is meant to afford.
    #[test]
    fn a_long_name_still_fits_at_the_widest_left_pane() {
        let (repo, _root_guard) = fixture("screen-queued-name-max-width");
        // 33 characters — exactly what MAX_LEFT_PANE budgeted a name before
        // the tail grew, and the width MAX_LEFT_PANE was widened to keep
        // affording it.
        let name = "a".repeat(33);
        write_pending(
            &repo,
            &name,
            &task_text(&name, &format!("group: {name}\n"), BODY),
        );
        already_queued(&repo, &name);
        let groups = listed(&repo);
        let state = ScreenState::new();
        // No setting of `h` puts a queued group on screen, but the pane is
        // still sized for `group_tail`'s widest tail — see
        // `QUEUED_TAIL_COLUMNS` — so the row is drawn from every group here.
        let shown: Vec<&Group> = groups.iter().collect();

        let (rows, _) = groups_pane_lines(&groups, &shown, &state, MAX_LEFT_PANE);
        assert!(
            rows[0].contains(&name),
            "expected the full 33-character name at MAX_LEFT_PANE, got {:?}",
            rows[0]
        );
    }

    /// The regression this guards: giving the name a floor (`MIN_NAME_COLUMN`)
    /// let a queued row overflow `width`, and `two_pane_frame`'s own `pad_to`
    /// then cut that overrun off the row's own end — the tail — so at
    /// ordinary widths it silently shrank or vanished mid-word.
    /// `MIN_LEFT_PANE` is sized so a floored name and a full tail always fit
    /// together, at the floor and every width above it — never just at
    /// `MAX_LEFT_PANE`, the pane's single widest setting.
    #[test]
    fn the_queued_tail_never_truncates_from_min_left_pane_up() {
        let (repo, _root_guard) = fixture("screen-queued-tail-never-truncates");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);

        let state = ScreenState::new();
        // No setting of `h` puts a queued group on screen, but the pane is
        // still sized for `group_tail`'s widest tail — see
        // `QUEUED_TAIL_COLUMNS` — so the row is drawn from every group here.
        let shown: Vec<&Group> = groups.iter().collect();
        for width in [MIN_LEFT_PANE, MIN_LEFT_PANE + 20, MAX_LEFT_PANE] {
            let (rows, _) = groups_pane_lines(&groups, &shown, &state, width);
            assert!(
                rows[0].ends_with(" queued"),
                "expected the full tail ' queued' at width {width}, got {rows:?}"
            );
        }
    }

    /// The review's own live repro, reproduced headless: a 74-column and a
    /// 99-column terminal — both "ordinary", neither the narrow nor the
    /// widest extreme — each rendered a truncated tail before this fix.
    #[test]
    fn the_queued_tail_survives_the_reviews_live_repro_widths() {
        let (repo, _root_guard) = fixture("screen-queued-tail-live-repro-widths");
        write_pending(
            &repo,
            "bound-loops",
            &task_text("bound-loops", "group: one\n", BODY),
        );
        already_queued(&repo, "bound-loops");
        let groups = listed(&repo);

        let state = ScreenState::new();
        // No setting of `h` puts a queued group on screen, but the pane is
        // still sized for `group_tail`'s widest tail — see
        // `QUEUED_TAIL_COLUMNS` — so the row is drawn from every group here.
        let shown: Vec<&Group> = groups.iter().collect();
        for total_columns in [74usize, 99] {
            let layout = layout_for(total_columns, 24);
            let (rows, _) = groups_pane_lines(&groups, &shown, &state, layout.left);
            assert!(
                rows[0].ends_with(" queued"),
                "expected the full tail at a {total_columns}-column terminal \
                 (left pane {}), got {rows:?}",
                layout.left
            );
        }
    }

    /// A done dependency whose branch is gone, archived as the last task
    /// of its own group — what a merged pull request leaves behind once
    /// GitHub deletes its branch.
    fn done_dependency_without_its_branch(repo: &Repo, id: &str) {
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join(format!("{id}.md")),
            format!(
                "---\nid: {id}\ntitle: {id}\ngroup: landed\nstage: done\nbranch: task/{id}\n\
                 ---\nbody\n"
            ),
        )
        .unwrap();
    }

    /// Sending from the queue tab queues every task it can and leaves out
    /// the one whose start branch is gone, with the task that depends on it.
    /// Both stay in the pending directory and in the pane, and the popup
    /// lists them.
    #[test]
    fn submit_leaves_out_a_task_whose_start_branch_is_gone_and_queues_the_rest() {
        let (repo, _root_guard) = fixture("screen-submit-missing-start");
        done_dependency_without_its_branch(&repo, "reader");
        let first = write_pending(
            &repo,
            "index-file",
            &task_text("index-file", "group: archive\ndepends_on: [reader]\n", BODY),
        );
        let second = write_pending(
            &repo,
            "index-readers",
            &task_text(
                "index-readers",
                "group: archive\ndepends_on: [index-file]\n",
                BODY,
            ),
        );
        let other = write_pending(&repo, "totals", &task_text("totals", "group: eval\n", BODY));
        let mut groups = listed(&repo);
        let mut state = ScreenState::new();
        handle_browse_key(&groups, &mut state, Key::Char(' '));
        handle_browse_key(&groups, &mut state, Key::Down);
        handle_browse_key(&groups, &mut state, Key::Char(' '));
        assert_eq!(state.selected.len(), 2);

        let outcome = begin_submission(
            &repo,
            &Pipelines::builtin(),
            "plan/demo",
            &mut groups,
            &mut state,
            Tracking::Ask,
            &mut |_| {},
        );
        let Mode::Queued { panel, .. } = outcome else {
            panic!("expected the queued popup, got {outcome:?}");
        };
        let all = panel.join("\n");
        assert!(all.contains("queued 1 task"), "{all}");
        assert!(
            all.contains("not queued, start branch doesn't exist"),
            "{all}"
        );
        assert!(all.contains("(depends on index-file)"), "{all}");
        assert!(
            all.contains("index-file starts from task/reader, which"),
            "{all}"
        );

        assert!(!other.exists(), "the task that could start was queued");
        assert!(repo.queue_dir().join("totals.md").exists());
        assert!(first.exists() && second.exists(), "the left-out tasks stay");
        assert!(!repo.queue_dir().join("index-file.md").exists());
        assert!(!repo.queue_dir().join("index-readers.md").exists());
        let names: Vec<&str> = groups.iter().map(|g| g.name.as_str()).collect();
        assert!(
            names.contains(&"archive") && !names.contains(&"eval"),
            "the left-out tasks' group stays in the pane, the queued one goes: {names:?}"
        );
    }

    /// A group selected whole but queued in part stays in the pane, its
    /// queued task marked queued there before any reload, so selecting the
    /// group again sends only the task left out rather than resubmitting its
    /// queued sibling.
    #[test]
    fn a_group_queued_in_part_stays_in_the_pane_with_its_queued_task_marked() {
        let (repo, _root_guard) = fixture("screen-submit-group-in-part");
        let first = write_pending(
            &repo,
            "split-first",
            &task_text("split-first", "group: split\n", BODY),
        );
        let second = write_pending(
            &repo,
            "split-second",
            &task_text(
                "split-second",
                "group: split\ndepends_on: [split-first]\nstarts_from: task/nowhere\n",
                BODY,
            ),
        );
        let mut groups = listed(&repo);
        let mut state = ScreenState::new();
        handle_browse_key(&groups, &mut state, Key::Char(' '));

        let outcome = begin_submission(
            &repo,
            &Pipelines::builtin(),
            "plan/demo",
            &mut groups,
            &mut state,
            Tracking::Ask,
            &mut |_| {},
        );
        assert!(matches!(outcome, Mode::Queued { .. }), "{outcome:?}");
        assert!(!first.exists() && second.exists());
        assert!(repo.queue_dir().join("split-first.md").exists());

        let group = groups
            .iter()
            .find(|g| g.name == "split")
            .expect("the group still holds a task to send");
        let state_of = |id: &str| group.tasks.iter().find(|t| t.id == id).unwrap().state;
        assert_eq!(state_of("split-first"), TaskState::Queued);
        assert_eq!(state_of("split-second"), TaskState::Pending);
        let mut again = ScreenState::new();
        handle_browse_key(&groups, &mut again, Key::Char(' '));
        let keys: Vec<_> = selected_tasks(&groups, &again)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, [task_key(&group.tasks[1])], "{keys:?}");
    }

    /// When the only task carrying its group's `group_description:` is left
    /// out, the first queued task of that group takes it, so the group's
    /// issue still has something to say. A queued task carrying its own is
    /// left alone.
    #[test]
    fn a_left_out_tasks_group_description_moves_to_a_queued_sibling() {
        let parse = |id: &str, extra: &str| {
            parse_submission(id, &task_text(id, extra, BODY), Some("plan/demo")).unwrap()
        };
        let mut tasks = [
            parse("lead", "group: g\ngroup_description: what g is for\n"),
            parse("kept", "group: g\n"),
            parse("own", "group: h\ngroup_description: h's own\n"),
            parse("gone", "group: h\ngroup_description: not this\n"),
        ];
        let not_queued = [
            NotQueued {
                id: "lead".to_string(),
                why: NotQueuedWhy::MissingStart("task/x".to_string()),
            },
            NotQueued {
                id: "gone".to_string(),
                why: NotQueuedWhy::MissingStart("task/x".to_string()),
            },
        ];
        carry_group_descriptions(&mut tasks, &not_queued);
        assert_eq!(
            tasks[1].front.group_description.as_deref(),
            Some("what g is for")
        );
        assert_eq!(tasks[2].front.group_description.as_deref(), Some("h's own"));
    }

    /// A start branch `origin` could not be asked about is not missing:
    /// telling the person to set `starts_from:` over a network failure would
    /// move the work onto another branch. The task is queued, and the
    /// dispatcher asks again before the cut.
    #[test]
    fn not_queued_does_not_leave_out_a_task_when_origin_cannot_be_asked() {
        let (repo, _root_guard) = fixture("not-queued-origin-unreachable");
        crate::repo::run(
            &repo.root,
            "git",
            &["remote", "add", "origin", "/nonexistent/origin.git"],
        )
        .unwrap();
        let task = parse_submission(
            "own",
            &task_text("own", "group: g\nstarts_from: task/somewhere\n", BODY),
            Some("plan/demo"),
        )
        .unwrap();
        assert_eq!(not_queued(&repo, &[task]), []);
    }

    /// `queue add` refuses the whole batch over a dependent whose done
    /// dependency's branch is gone, naming the branch and what to set — and
    /// takes it once the task names a start branch that exists.
    #[test]
    fn queue_add_refuses_a_task_whose_start_branch_is_gone_until_starts_from_is_set() {
        let (repo, _root_guard) = fixture("queue-add-missing-start");
        done_dependency_without_its_branch(&repo, "reader");
        let gone = task_text("index-file", "group: archive\ndepends_on: [reader]\n", BODY);
        let free = task_text("totals", "group: eval\n", BODY);
        let gone_path = write_doc(&repo, "index-file.md", &gone);
        let free_path = write_doc(&repo, "totals.md", &free);

        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&gone_path, &free_path]),
            &repo.root,
            false,
        )
        .expect_err("a task with nowhere to start is refused");
        let said = format!("{err:#}");
        assert!(
            said.contains(
                "index-file starts from task/reader, which doesn't exist. Set starts_from: in \
                 the task front matter and requeue."
            ),
            "{said}"
        );
        assert!(
            !repo.queue_dir().join("totals.md").exists(),
            "the whole batch is refused"
        );

        let set = task_text(
            "index-file",
            "group: archive\ndepends_on: [reader]\nstarts_from: plan/demo\n",
            BODY,
        );
        let set_path = write_doc(&repo, "index-file.md", &set);
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&set_path]),
            &repo.root,
            false,
        )
        .expect("a task naming a start branch that exists is queued");
        assert!(repo.queue_dir().join("index-file.md").exists());
    }

    /// Only a task with something to check is checked: one whose dependency
    /// is still queued starts from a branch the dispatcher cuts when that
    /// dependency starts, and one with no dependency starts from `base:`.
    /// A task's own `starts_from:` is checked whatever it depends on.
    #[test]
    fn not_queued_checks_only_a_done_dependency_or_an_own_starts_from() {
        let (repo, _root_guard) = fixture("not-queued-what-is-checked");
        already_queued(&repo, "running");
        let parse = |id: &str, extra: &str| {
            let extra = format!("group: chain\n{extra}");
            parse_submission(id, &task_text(id, &extra, BODY), Some("plan/demo")).unwrap()
        };
        let batch = [
            parse("after-running", "depends_on: [running]\n"),
            parse("root", ""),
            parse("own-gone", "starts_from: task/nowhere\n"),
            parse(
                "own-there",
                "depends_on: [running]\nstarts_from: plan/demo\n",
            ),
        ];
        assert_eq!(
            not_queued(&repo, &batch),
            [NotQueued {
                id: "own-gone".to_string(),
                why: NotQueuedWhy::MissingStart("task/nowhere".to_string()),
            }]
        );
    }

    /// A dependent is left out with what it depends on, however far down
    /// the chain and in whatever order the batch lists it.
    #[test]
    fn not_queued_follows_dependents_down_the_chain_in_any_order() {
        let (repo, _root_guard) = fixture("not-queued-chain");
        done_dependency_without_its_branch(&repo, "reader");
        let parse = |id: &str, extra: &str| {
            let extra = format!("group: chain\n{extra}");
            parse_submission(id, &task_text(id, &extra, BODY), Some("plan/demo")).unwrap()
        };
        let batch = [
            parse("third", "depends_on: [second]\n"),
            parse("first", "depends_on: [reader]\n"),
            parse("second", "depends_on: [first]\n"),
            parse("apart", ""),
        ];
        assert_eq!(
            not_queued(&repo, &batch),
            [
                NotQueued {
                    id: "third".to_string(),
                    why: NotQueuedWhy::DependsOn("second".to_string()),
                },
                NotQueued {
                    id: "first".to_string(),
                    why: NotQueuedWhy::MissingStart("task/reader".to_string()),
                },
                NotQueued {
                    id: "second".to_string(),
                    why: NotQueuedWhy::DependsOn("first".to_string()),
                },
            ]
        );
    }

    /// A landed submission clears the group out of the pane the same act it
    /// clears it off disk: `list_groups` runs once per screen session, so
    /// nothing else would ever pick up that the tasks are gone, and a
    /// row left behind is one a person can select and submit a second time.
    /// Drives `begin_submission` directly, not through `run_screen`, so the
    /// pane's own copy of the list is what gets inspected afterward.
    #[test]
    fn submit_clears_the_group_from_the_pane_and_from_the_pending_directory() {
        let (repo, _root_guard) = fixture("screen-submit-clears-pending");
        let path = write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let mut groups = listed(&repo);
        assert_eq!(groups.len(), 1);

        let mut state = ScreenState::new();
        handle_browse_key(&groups, &mut state, Key::Char(' '));

        let pipelines = Pipelines::builtin();
        let outcome = begin_submission(
            &repo,
            &pipelines,
            "plan/demo",
            &mut groups,
            &mut state,
            Tracking::Ask,
            &mut |_| {},
        );
        assert!(
            matches!(outcome, Mode::Queued { .. }),
            "expected a clean submission to say what it queued, got {outcome:?}"
        );

        assert!(!path.exists(), "the task must be gone from pending");
        assert!(
            groups.is_empty(),
            "the pane's own list must lose the group it just queued"
        );

        // A re-read no longer comes back empty: `wire.md` now lives in the
        // queue directory instead of the pending one, and that is the other
        // source `list_groups` reads — the group's row survives the
        // submission, marked `queued`, until the dispatcher archives it.
        let reread = listed(&repo);
        assert_eq!(
            reread.len(),
            1,
            "the group's row survives, from the queue: {} groups",
            reread.len()
        );
        assert_eq!(
            reread[0].state,
            GroupState::Queued,
            "and it reads as already queued"
        );
    }

    /// A group whose sibling is already queued must still let its pending
    /// task through: `selected_tasks` puts every task of the group in
    /// one batch, including the sibling's task read straight out of
    /// `queue/`, which carries `stage:` — and `parse_submission`'s
    /// `RESERVED_KEYS` check refuses any task that sets it, so today the
    /// whole submission is refused rather than just queueing the pending one
    /// and leaving the queued sibling alone.
    #[test]
    fn queueing_a_group_leaves_its_queued_sibling_alone_and_queues_the_pending_task() {
        let (repo, _root_guard) = fixture("screen-requeue-group");
        // `beta` depends on `alpha`: a group is one chain now, so the second
        // half of a group queued in two passes has to say how it continues
        // the first — see `a_group_with_two_roots_is_refused`.
        let beta_path = write_pending(
            &repo,
            "beta",
            &task_text("beta", "group: one\ndepends_on: [alpha]\n", BODY),
        );
        let alpha_path = repo.queue_dir().join("alpha.md");
        std::fs::write(
            &alpha_path,
            "---\nid: alpha\ntitle: alpha\ngroup: one\nstage: queued\n---\nbody\n",
        )
        .unwrap();

        let mut groups = listed(&repo);
        assert_eq!(groups.len(), 1, "alpha and beta must fold into one group");
        assert_eq!(groups[0].state, GroupState::Queueable);

        let mut state = ScreenState::new();
        handle_browse_key(&groups, &mut state, Key::Char(' '));

        let pipelines = Pipelines::builtin();
        // Driven straight through `finish_submit` rather than
        // `begin_submission`: the "left alone" note it builds is no longer
        // shown on any screen — a clean submission's popup names only what it
        // queued —
        // so this is the one place left that can still read it back.
        let tasks = selected_tasks(&groups, &state);
        let pending = validate_batch(&repo, &pipelines, Some("plan/demo"), &tasks).unwrap();
        let selected = state.selected.clone();
        let submit = Submit {
            tasks: &tasks,
            base: "plan/demo",
            selected: &selected,
            left_out: &[],
            tracking_off: false,
        };
        let msg = finish_submit(&repo, &mut groups, pending, &submit, &mut PrintedTickets).unwrap();

        assert!(!beta_path.exists(), "beta's pending task must be gone");
        assert!(alpha_path.exists(), "alpha's queue task must be untouched");
        assert!(
            repo.queue_dir().join("beta.md").exists(),
            "beta must have landed in the queue"
        );
        assert!(
            msg.contains("already in the queue, left alone: alpha"),
            "the report must name the sibling left alone, got {msg:?}"
        );
    }

    /// The same shape, but the sibling left behind is archived rather than
    /// queued: `finish_submit` used to walk every task of the selected group
    /// and unlink its file regardless of state, which for an archived
    /// sibling was its whole finished record.
    #[test]
    fn queueing_a_group_leaves_its_archived_sibling_alone_and_names_it() {
        let (repo, _root_guard) = fixture("screen-requeue-group-archived");
        // `beta` depends on `alpha`, same reason as the queued-sibling test
        // above: a group is one chain, archived tasks counted the same as
        // queued ones.
        let beta_path = write_pending(
            &repo,
            "beta",
            &task_text("beta", "group: one\ndepends_on: [alpha]\n", BODY),
        );
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        let alpha_path = repo.archive_dir().join("alpha.md");
        std::fs::write(
            &alpha_path,
            "---\nid: alpha\ntitle: alpha\ngroup: one\nstage: done\n---\nbody\n",
        )
        .unwrap();

        let mut groups = listed(&repo);
        assert_eq!(groups.len(), 1, "alpha and beta must fold into one group");
        assert_eq!(groups[0].state, GroupState::Queueable);

        let mut state = ScreenState::new();
        handle_browse_key(&groups, &mut state, Key::Char(' '));

        let pipelines = Pipelines::builtin();
        // See the sibling test above on why this reads `finish_submit`
        // directly rather than `begin_submission`.
        let tasks = selected_tasks(&groups, &state);
        let pending = validate_batch(&repo, &pipelines, Some("plan/demo"), &tasks).unwrap();
        let selected = state.selected.clone();
        let submit = Submit {
            tasks: &tasks,
            base: "plan/demo",
            selected: &selected,
            left_out: &[],
            tracking_off: false,
        };
        let msg = finish_submit(&repo, &mut groups, pending, &submit, &mut PrintedTickets).unwrap();

        assert!(!beta_path.exists(), "beta's pending task must be gone");
        assert!(
            alpha_path.exists(),
            "alpha's archived record must be untouched"
        );
        assert!(
            repo.queue_dir().join("beta.md").exists(),
            "beta must have landed in the queue"
        );
        assert!(
            msg.contains("already archived, left alone: alpha"),
            "the report must name the archived sibling left alone, got {msg:?}"
        );
    }

    /// The tasks pane draws no header row and no estimate columns — a
    /// task's id and its own title are what carry the weight — and no
    /// checkbox of its own: the selection lives on the group.
    #[test]
    fn the_tasks_pane_draws_no_header_and_no_estimate_columns() {
        let (repo, _root_guard) = fixture("screen-columns");
        write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 46);
        assert!(
            !lines.iter().any(|line| line.contains("size")),
            "no size column belongs in the tasks pane any more, got {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("complexity")),
            "no complexity column belongs in the tasks pane any more, got {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("[ ]") || line.contains("[x]")),
            "no checkbox belongs in the tasks pane, got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("wire")),
            "the task is named by its own id, got {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("Depends on:")),
            "the task still says what it depends on, got {lines:?}"
        );

        // The task above carries `touches: [src/wire.rs]`, and the pane shows
        // neither the label nor the glob. They were the widest thing drawn
        // here, and a person choosing what to queue does not pick by glob.
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("touches") || line.contains("src/wire.rs")),
            "the pane never draws a task's touches, got {lines:?}"
        );
    }

    /// `queue-tab-reads-changed-only`: one draw of the tasks pane used to
    /// parse the highlighted task's own front matter three times over —
    /// once each through `doc_pipeline_name`, `depends_on` and `doc_base` —
    /// rather than once. `pending::list_groups_in` now parses a task's
    /// front matter once while building it, filling `PendingTask::pipeline`,
    /// `depends_on` and `base` from that one parse, and `tasks_pane_lines`
    /// only reads those three fields: a draw over an unchanged file parses
    /// nothing at all, not even once.
    ///
    /// Counted through `list_groups_in`'s own cache-and-counter arguments,
    /// not a process-wide counter: ~40 sibling tests in this module call
    /// `tasks_pane_lines`, `TrialState::new`, `doc_pipeline_name` or
    /// `depends_on`, and the first version of this test read a shared
    /// static any of them could bump — seen failing 16 times in 150
    /// parallel runs (review finding 1). A cache and counter this test
    /// owns outright cannot be touched by any other test.
    #[test]
    fn tasks_pane_lines_reads_a_tasks_front_matter_parsed_once_not_reparsed_per_draw() {
        let (repo, _root_guard) = fixture("tasks-pane-parse-once");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));

        let mut cache = std::collections::HashMap::new();
        let mut parsed = 0usize;
        let groups = super::pending::list_groups_in(&repo, &mut cache, &mut parsed).unwrap();
        assert_eq!(
            parsed, 1,
            "one task, new to the cache, is parsed exactly once while list_groups builds it"
        );

        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();
        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Pipeline:") && line.contains("default")),
            "tasks_pane_lines must still draw the task's own pipeline off the \
             field list_groups already filled in, got {lines:?}"
        );

        // The per-second reload path: a second build over the same,
        // unchanged file must parse nothing, not once per draw.
        // `tasks_pane_lines` itself never parses anything any more, so
        // `list_groups_in`'s own count is the whole of what is left to
        // check.
        let mut parsed_again = 0usize;
        let _ = super::pending::list_groups_in(&repo, &mut cache, &mut parsed_again).unwrap();
        assert_eq!(
            parsed_again, 0,
            "a reload over an unchanged file must parse nothing, not once per draw"
        );
    }

    /// A task's own `title:` reaches the tasks pane under its own
    /// `Description:` label — the one sentence a person picks by.
    #[test]
    fn a_tasks_title_reaches_the_tasks_pane() {
        let (repo, _root_guard) = fixture("screen-description");
        write_pending(
            &repo,
            "ctx-peak",
            "---\nid: ctx-peak\ntitle: Bank the largest context reading a lane reached, as \
             ctx_peak on the ledger line.\ngroup: one\n---\n## Goal\n\nDo the thing.\n",
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        assert!(
            lines.join("\n").contains("ctx_peak"),
            "the title must reach the pane, got {lines:?}"
        );
    }

    /// A task with no `title:` at all draws no `Description:` row,
    /// rather than an empty one. `parse_submission` is what refuses it, at
    /// submit time — the pane just has nothing to draw.
    #[test]
    fn a_task_with_no_title_draws_no_extra_line() {
        let (repo, _root_guard) = fixture("screen-no-description");
        write_pending(
            &repo,
            "wire",
            "---\nid: wire\ngroup: one\n---\n## Goal\n\nDo the thing.\n",
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 46);
        // Only the blank separator the loop always opens a task with, the
        // task row, its Pipeline:, Depends on:, Starts from: and Lands in:
        // rows — nothing
        // past it, since this task has no title to draw a Description:
        // row from. The task names no `pipeline:` either, and there is
        // no project default to show in its place any more.
        assert_eq!(
            lines,
            vec![
                String::new(),
                "  wire".to_string(),
                format!("    {:<LABEL_FIELD$}{}", "Pipeline:", TRIAL_UNASSIGNED),
                "    Depends on:  -".to_string(),
                "    Starts from: -".to_string(),
                "    Lands in:    -".to_string(),
            ],
            "{lines:?}"
        );
    }

    /// The constraint the context notes call out by name: a chain half
    /// archived and half still queued draws each of its tasks with its own
    /// tail, not the group's — the mockup's own `full-project-review`
    /// example, with a third task still waiting in `pending/` and drawing
    /// none at all.
    #[test]
    fn tasks_pane_draws_each_tasks_own_state_beside_its_id() {
        let (repo, _root_guard) = fixture("screen-mixed-task-states");
        write_pending(
            &repo,
            "third",
            &task_text("third", "group: chain\ndepends_on: [second]\n", BODY),
        );
        std::fs::write(
            repo.queue_dir().join("second.md"),
            "---\nid: second\ntitle: second\ngroup: chain\nstage: queued\n\
             depends_on: [first]\n---\nbody\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("first.md"),
            "---\nid: first\ntitle: first\ngroup: chain\nstage: done\n---\nbody\n",
        )
        .unwrap();
        let groups = listed(&repo);
        assert_eq!(
            groups[0].state,
            GroupState::Queueable,
            "one task is still pending, so the group as a whole still is too"
        );
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 46);
        let row_for = |id: &str| {
            lines
                .iter()
                .find(|line| line.trim_start().starts_with(id))
                .unwrap_or_else(|| panic!("no row for `{id}` in {lines:?}"))
        };
        assert!(row_for("first").trim_end().ends_with("done"));
        assert!(row_for("second").trim_end().ends_with("queued"));
        assert!(
            !row_for("third").trim_end().ends_with("queued")
                && !row_for("third").trim_end().ends_with("done"),
            "a task still in pending/ draws no tail at all: {:?}",
            row_for("third")
        );
    }

    /// Once every task in a group has finished, the group's own tail already
    /// says `done` once, in the pane to the left — repeating it on every
    /// task underneath tells a person nothing the group row did not already.
    #[test]
    fn a_wholly_done_groups_own_tasks_draw_no_repeated_done_tail() {
        let (repo, _root_guard) = fixture("screen-done-no-repeat");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        for id in ["first", "second"] {
            std::fs::write(
                repo.archive_dir().join(format!("{id}.md")),
                format!("---\nid: {id}\ntitle: {id}\ngroup: finished\nstage: done\n---\nbody\n"),
            )
            .unwrap();
        }
        let groups = listed(&repo);
        assert_eq!(groups[0].state, GroupState::Done);

        let pipelines = Pipelines::builtin();
        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusDone;

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 46);
        assert!(
            lines.iter().all(|line| !line.trim_end().ends_with("done")),
            "{lines:?}"
        );
    }

    /// Every fact a wide pane draws about a task — its pipeline, what it
    /// depends on, where it starts and lands, its gate and its description —
    /// starts its value at the same column, in that order.
    ///
    /// This task has no dependency and no `starts_from:` of its own, so it
    /// starts from its `base:` — a dependent starts from its dependency, see
    /// `a_dependent_task_starts_from_its_dependency` below.
    #[test]
    fn every_row_in_a_wide_pane_starts_its_value_at_the_same_column() {
        let (repo, _root_guard) = fixture("screen-labelled-rows");
        write_pending(
            &repo,
            "tracking-open",
            &task_text(
                "tracking-open",
                "group: one\npipeline: default\nbase: task/gh-412-checkout\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let mut state = ScreenState::new();
        // The task's own `base:` wins over the board's branch.
        state.board_branch = Some("main".to_string());
        state
            .gates
            .insert(task_key(&groups[0].tasks[0]), "review".to_string());

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        assert_eq!(
            lines,
            vec![
                String::new(),
                "  tracking-open".to_string(),
                "    Pipeline:    default".to_string(),
                "    Depends on:  -".to_string(),
                "    Starts from: task/gh-412-checkout".to_string(),
                "    Lands in:    task/gh-412-checkout".to_string(),
                "    Gate:        review".to_string(),
                "    Description: tracking-open, done".to_string(),
            ],
            "{lines:?}"
        );
    }

    /// A dependent starts from its first dependency, named by id since a
    /// pending dependency has no branch yet, and lands in its own `base:` —
    /// two rows, so the base never reads as where the task starts.
    #[test]
    fn a_dependent_task_starts_from_its_dependency() {
        let (repo, _root_guard) = fixture("screen-dependent-no-base");
        write_pending(
            &repo,
            "cart-discounts",
            &task_text(
                "cart-discounts",
                "group: one\npipeline: default\ndepends_on: [cart-totals]\n\
                 base: plan/other\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        assert_eq!(
            lines,
            vec![
                String::new(),
                "  cart-discounts".to_string(),
                "    Pipeline:    default".to_string(),
                "    Depends on:  cart-totals".to_string(),
                "    Starts from: cart-totals".to_string(),
                "    Lands in:    plan/other".to_string(),
                "    Description: cart-discounts, done".to_string(),
            ],
            "{lines:?}"
        );
    }

    /// A task's own `starts_from:` is where it starts, ahead of its
    /// dependency — the field a person sets when the dependency's branch is
    /// gone.
    #[test]
    fn a_tasks_own_starts_from_wins_over_its_dependency() {
        let (repo, _root_guard) = fixture("screen-own-starts-from");
        write_pending(
            &repo,
            "cart-discounts",
            &task_text(
                "cart-discounts",
                "group: one\npipeline: default\ndepends_on: [cart-totals]\n\
                 starts_from: main\nbase: plan/other\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        assert!(
            lines.contains(&"    Starts from: main".to_string())
                && lines.contains(&"    Lands in:    plan/other".to_string()),
            "{lines:?}"
        );
    }

    /// A task naming no `base:` lands in the branch the board's checkout has
    /// out, since that is the branch `enter` would send it on — and a
    /// blank `base:` reads the same as an absent one, as it does there. With
    /// no dependency either, it starts from that branch too.
    #[test]
    fn a_task_with_no_base_shows_the_boards_branch() {
        let (repo, _root_guard) = fixture("screen-base-fallback");
        write_pending(
            &repo,
            "cart-totals",
            &task_text("cart-totals", "group: one\n", BODY),
        );
        write_pending(
            &repo,
            "cart-empty",
            &task_text("cart-empty", "group: one\nbase: \"\"\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let mut state = ScreenState::new();
        state.board_branch = Some("feat/checkout".to_string());

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        let rows = |label: &str| -> Vec<&String> {
            lines
                .iter()
                .filter(|line| line.trim_start().starts_with(label))
                .collect()
        };
        assert_eq!(
            rows("Lands in:"),
            vec!["    Lands in:    feat/checkout"; 2],
            "{lines:?}"
        );
        assert_eq!(
            rows("Starts from:"),
            vec!["    Starts from: feat/checkout"; 2],
            "{lines:?}"
        );
    }

    /// A right pane narrower than forty columns keeps every label, dropping
    /// its value to its own line underneath, indented two — never dropping
    /// the label itself, which the acceptance criterion rules out.
    #[test]
    fn a_narrow_right_pane_keeps_every_label_and_drops_its_value_below() {
        let (repo, _root_guard) = fixture("screen-narrow-labelled-rows");
        write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: one\npipeline: default\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _, _) = tasks_pane_lines(&groups, &pipelines, &state, 26);
        assert_eq!(
            lines,
            vec![
                String::new(),
                "  wire".to_string(),
                "    Pipeline:".to_string(),
                "      default".to_string(),
                "    Depends on:".to_string(),
                "      -".to_string(),
                "    Starts from:".to_string(),
                "      -".to_string(),
                "    Lands in:".to_string(),
                "      -".to_string(),
                "    Description:".to_string(),
                "      wire, done".to_string(),
            ],
            "{lines:?}"
        );
    }

    /// A reload can reorder `shown` out from under the cursor — a task
    /// just edited moves to the front of `list_groups`' own newest-first
    /// order — but the group the cursor was on stays highlighted, found
    /// again by name rather than by the index it no longer sits at.
    #[test]
    fn reload_keeps_the_cursor_on_the_same_group_after_a_reorder() {
        let (repo, _root_guard) = fixture("screen-reload-cursor");
        write_pending(&repo, "older", &task_text("older", "group: older\n", BODY));
        let mut groups = listed(&repo);
        let mut state = ScreenState::new();
        state.group_cursor = shown(&groups, &state)
            .iter()
            .position(|group| group.name == "older")
            .unwrap();

        // A second, newer task sorts ahead of the first — see
        // `list_groups`' own newest-first order — so a naive reload that
        // kept the cursor's index rather than its group would now be
        // pointing at this one instead.
        write_pending(&repo, "newer", &task_text("newer", "group: newer\n", BODY));
        reload(&repo, &mut groups, &mut state);

        let cursor_group = &shown(&groups, &state)[state.group_cursor];
        assert_eq!(
            cursor_group.name, "older",
            "the cursor must follow the group it was on, not the row"
        );
    }

    /// An idle poll that finds nothing new must not touch the terminal at
    /// all — the acceptance criterion this exists for. `draw` diffs against
    /// what it last wrote, and a second call with nothing changed writes no
    /// second frame.
    #[test]
    fn draw_writes_nothing_when_the_frame_has_not_changed() {
        let (repo, _root_guard) = fixture("screen-idle-draw");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();
        let mut writer = crate::screen::frame_writer::FrameWriter::new();
        let mut out = Vec::new();

        let routines = Vec::new();
        let routines_dir = repo.routines_dir();
        let panes = Panes {
            routines: &routines,
            routines_dir: &routines_dir,
            pipelines: &pipelines,
        };
        draw(&groups, &panes, &state, &mut writer, &mut out);
        let after_first = out.len();
        assert!(after_first > 0, "the first draw must write the frame");

        draw(&groups, &panes, &state, &mut writer, &mut out);
        assert_eq!(
            out.len(),
            after_first,
            "an unchanged frame must write nothing more:\n{}",
            String::from_utf8_lossy(&out)
        );
    }

    /// The acceptance criterion this task exists for: the queue tab's own
    /// frame, painted through the shared [`crate::screen::frame_writer`],
    /// must look exactly as it did through today's frozen erase-then-write —
    /// cell for cell, in text, colour and bold — not merely bytes shaped the
    /// same way.
    #[test]
    fn queue_paints_as_before() {
        let (repo, _root_guard) = fixture("queue-paints-as-before");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();
        let routines = Vec::new();
        let routines_dir = repo.routines_dir();
        let panes = Panes {
            routines: &routines,
            routines_dir: &routines_dir,
            pipelines: &pipelines,
        };
        let frame = crate::screen::shell::under_strip(render(&groups, &panes, &state));
        // Wide enough that no row here reaches the pane's own edge, the same
        // as a real terminal this screen ever draws to — see
        // `commands::queue`'s own `SPARE_COLUMN`.
        let pane_size = (200, 60);

        let mut old = Vec::new();
        crate::screen::frame_writer::todays_write(&frame, &mut old);

        let mut new = Vec::new();
        crate::screen::frame_writer::FrameWriter::new().write_frame(&frame, pane_size, &mut new);

        crate::screen::frame_writer::assert_same_picture(&old, &new, pane_size);
    }

    /// A panel is drawn over the frame, not pushed in between its rows: the
    /// frame keeps its height and every row keeps its width.
    #[test]
    fn a_panel_is_drawn_over_the_frame_without_moving_a_row() {
        let layout = Layout {
            left: 24,
            right: 46,
            rows: Some(16),
        };
        let mut frame = two_pane_frame(&["> [ ] demo".to_string()], &[], "pending", "demo", layout);
        let before = frame.len();
        let widths: Vec<usize> = frame.iter().map(|line| line.chars().count()).collect();

        overlay(
            &mut frame,
            &panel(
                "gate wire at",
                &["> implement".to_string(), "  review".to_string()],
                &crate::screen::keys(GATE_KEYS),
            ),
        );

        assert_eq!(frame.len(), before, "the frame must not grow");
        assert_eq!(
            widths,
            frame
                .iter()
                .map(|line| line.chars().count())
                .collect::<Vec<_>>(),
            "every row must keep its width:\n{}",
            frame.join("\n")
        );
        assert!(
            frame.iter().any(|line| line.contains("gate wire at")),
            "the panel must be visible:\n{}",
            frame.join("\n")
        );
        assert!(
            frame.last().unwrap().starts_with('└'),
            "the frame's own bottom border stays:\n{}",
            frame.join("\n")
        );
    }

    /// `t`'s own first screen: every project pipeline, its own row, in the
    /// order `trial_pipeline_names` gives — and only the pipelines the
    /// group's own tasks name start ticked. A task naming none ticks
    /// nothing. The arm count under the list multiplies the ticks by the
    /// group's tasks.
    #[test]
    fn pick_pipelines_panel_lists_every_pipeline_and_ticks_the_groups_own() {
        let (repo, _root_guard) = fixture("screen-trial-pick");
        // Built by hand rather than through `task`, which fills in
        // `pipeline: default` — alpha's own point here is that it names
        // none, so it ticks nothing and `default` opens unticked.
        write_pending(
            &repo,
            "alpha",
            &format!("---\nid: alpha\ntitle: alpha, done\ngroup: chain\n---\n{BODY}"),
        );
        write_pending(
            &repo,
            "beta",
            &task_text(
                "beta",
                "group: chain\ndepends_on: [alpha]\npipeline: bugfix\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        assert_eq!(trial_pipeline_names(&pipelines), vec!["bugfix", "default"]);

        let trial = TrialState::new(&pipelines, &groups[0]);
        let panel = trial_panel(
            &groups,
            &pipelines,
            &trial,
            (LEFT_PANE_WIDTH + RIGHT_PANE_WIDTH, None),
        )
        .unwrap();
        let flat = panel.join("\n");

        assert!(flat.contains("trial chain"), "{flat}");
        assert!(flat.contains("pick pipelines to compare"), "{flat}");
        // The cursor opens on the first tick, as the mockup draws it.
        let bugfix_at = flat.find("> [x] bugfix").expect(&flat);
        let default_at = flat.find("  [ ] default").expect(&flat);
        assert!(bugfix_at < default_at, "listed in name order: {flat}");
        assert!(flat.contains("1 pipeline × 2 tasks = 2 arms"), "{flat}");
        assert!(
            flat.contains("[↑↓] move   [space] tick   [enter] next   [esc] cancel"),
            "{flat}"
        );
    }

    /// `space` ticks and unticks the pipeline under the cursor, the arm
    /// count follows, and unticking a pipeline drops the skips it held.
    #[test]
    fn space_ticks_the_highlighted_pipeline_and_untick_drops_its_skips() {
        let (repo, _root_guard) = fixture("screen-trial-tick");
        write_pending(&repo, "alpha", &task_text("alpha", "group: demo\n", BODY));
        write_pending(
            &repo,
            "beta",
            &task_text("beta", "group: demo\ndepends_on: [alpha]\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let group = &groups[0];

        let trial = TrialState::new(&pipelines, group);
        assert_eq!(
            trial.ticked.iter().collect::<Vec<_>>(),
            vec!["default"],
            "both tasks name `default`, so it alone starts ticked"
        );
        assert_eq!(trial.cursor, 1, "the cursor opens on `default`'s row");

        // Up onto `bugfix` and tick it.
        let Mode::Trial(trial) = handle_trial_key(&pipelines, trial, Key::Up) else {
            panic!("expected to stay on the trial picker");
        };
        let Mode::Trial(mut trial) = handle_trial_key(&pipelines, trial, Key::Char(' ')) else {
            panic!("expected to stay on the trial picker");
        };
        assert!(trial.ticked.contains("bugfix") && trial.ticked.contains("default"));
        let flat = trial_panel(&groups, &pipelines, &trial, (80, None))
            .unwrap()
            .join("\n");
        assert!(flat.contains("2 pipelines × 2 tasks = 4 arms"), "{flat}");

        // A skip held for `bugfix` goes with its tick.
        trial
            .skip
            .entry("bugfix".to_string())
            .or_default()
            .insert("handover".to_string());
        let Mode::Trial(trial) = handle_trial_key(&pipelines, trial, Key::Char(' ')) else {
            panic!("expected to stay on the trial picker");
        };
        assert!(!trial.ticked.contains("bugfix"));
        assert!(!trial.skip.contains_key("bugfix"), "{:?}", trial.skip);
    }

    /// `enter` does nothing while no pipeline is ticked: the picker stays on
    /// its first screen and nothing is queued.
    #[test]
    fn enter_with_no_pipeline_ticked_does_nothing() {
        let (repo, _root_guard) = fixture("screen-trial-none-ticked");
        write_pending(&repo, "solo", &task_text("solo", "group: audits\n", BODY));
        let groups = listed(&repo);

        // `t` opens with `default` ticked (the cursor on it), `space`
        // unticks it, and two `enter`s would otherwise reach the write.
        let drawn = screen(&repo, groups, "t \r\r");

        assert!(repo.queue_dir().read_dir().unwrap().next().is_none());
        let frame = last_frame(&drawn);
        assert!(frame.contains("pick pipelines to compare"), "{frame}");
        assert!(frame.contains("0 pipelines × 1 task = 0 arms"), "{frame}");
    }

    /// `esc` off the second screen returns to the first without dropping
    /// anything either screen already picked.
    #[test]
    fn esc_off_the_skip_screen_returns_to_pipelines_without_losing_picks() {
        let (repo, _root_guard) = fixture("screen-trial-esc-back");
        write_pending(&repo, "solo", &task_text("solo", "group: audits\n", BODY));
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let group = &groups[0];

        let mut trial = TrialState::new(&pipelines, group);
        trial.ticked.insert("bugfix".to_string());
        trial.stage = TrialStage::ChooseSkips;
        trial
            .skip
            .entry("bugfix".to_string())
            .or_default()
            .insert("checks".to_string());

        let Mode::Trial(trial) = handle_trial_key(&pipelines, trial, Key::Esc) else {
            panic!("expected to stay on the trial picker");
        };

        assert_eq!(trial.stage, TrialStage::PickPipelines);
        assert!(trial.ticked.contains("bugfix"));
        assert!(trial.skip["bugfix"].contains("checks"));
    }

    /// `t`'s own second screen: one ticked pipeline to a page, its steps one
    /// per row, and a title row naming the page and counting only its own
    /// skips. `→` and `←` turn the page, wrapping round at either end, and a
    /// tick made on one page never reaches another's — each pipeline keeps
    /// its own skip set.
    #[test]
    fn the_skip_screen_shows_one_ticked_pipeline_a_page() {
        let (repo, _root_guard) = fixture("screen-trial-skips");
        write_pending(
            &repo,
            "alpha",
            &task_text("alpha", "group: chain\npipeline: bugfix\n", BODY),
        );
        write_pending(
            &repo,
            "beta",
            &task_text(
                "beta",
                "group: chain\ndepends_on: [alpha]\npipeline: default\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let bugfix = pipelines.get("bugfix").unwrap();
        let default = pipelines.get("default").unwrap();

        let mut trial = TrialState::new(&pipelines, &groups[0]);
        trial.stage = TrialStage::ChooseSkips;
        trial
            .skip
            .entry("bugfix".to_string())
            .or_default()
            .insert("handover".to_string());
        let draw = |trial: &TrialState| {
            trial_panel(
                &groups,
                &pipelines,
                trial,
                (LEFT_PANE_WIDTH + RIGHT_PANE_WIDTH, None),
            )
            .unwrap()
        };
        let step_rows = |panel: &[String]| {
            panel
                .iter()
                .filter(|line| line.contains("[ ] ") || line.contains("[x] "))
                .count()
        };

        let panel = draw(&trial);
        let flat = panel.join("\n");
        assert!(
            flat.contains("skip steps   ←  bugfix  1 of 2  →   1 skipped"),
            "{flat}"
        );
        assert_eq!(step_rows(&panel), bugfix.steps.len(), "{flat}");
        assert!(flat.contains("> [ ] reproduce"), "{flat}");
        assert!(flat.contains("  [x] handover"), "{flat}");
        assert!(
            !flat.contains("implement"),
            "default's steps are on its own page: {flat}"
        );
        // Tightened to two spaces a pair, since this row is wider than the
        // fallback layout's popup — see `fit_keys`.
        assert!(
            flat.contains("[↑↓] step  [←→] pipeline  [space] skip  [enter] run  [esc] pipelines"),
            "{flat}"
        );

        // `→` turns to `default`, whose own `handover` — the same step id,
        // another pipeline — is not ticked by bugfix's set.
        let Mode::Trial(trial) = handle_trial_key(&pipelines, trial, Key::Right) else {
            panic!("expected to stay on the trial picker");
        };
        let panel = draw(&trial);
        let flat = panel.join("\n");
        assert!(
            flat.contains("skip steps   ←  default  2 of 2  →   0 skipped"),
            "{flat}"
        );
        assert_eq!(step_rows(&panel), default.steps.len(), "{flat}");
        assert!(!flat.contains("[x]"), "{flat}");

        // `space` here ticks default's own first step and nothing of bugfix's.
        let Mode::Trial(trial) = handle_trial_key(&pipelines, trial, Key::Char(' ')) else {
            panic!("expected to stay on the trial picker");
        };
        assert!(trial.skip["default"].contains("implement"));
        assert_eq!(trial.skip["bugfix"].len(), 1, "{:?}", trial.skip);

        // Past the last page `→` wraps to the first, and `←` from the first
        // back to the last.
        let Mode::Trial(trial) = handle_trial_key(&pipelines, trial, Key::Right) else {
            panic!("expected to stay on the trial picker");
        };
        assert!(draw(&trial).join("\n").contains("←  bugfix  1 of 2  →"));
        let Mode::Trial(trial) = handle_trial_key(&pipelines, trial, Key::Left) else {
            panic!("expected to stay on the trial picker");
        };
        assert!(
            draw(&trial)
                .join("\n")
                .contains("←  default  2 of 2  →   1 skipped")
        );
    }

    /// The window the mockup draws: seven steps with room for six rows show
    /// the first five and a `↓ 2 more` row. And for every length, room and
    /// cursor, the window is never taller than its room, always holds the
    /// cursor's own row, and counts exactly the rows it hides.
    #[test]
    fn scroll_rows_keeps_the_cursor_in_view_and_fits_its_room() {
        let rows = |n: usize| (0..n).map(|i| format!("row {i}")).collect::<Vec<_>>();

        assert_eq!(
            scroll_rows(&rows(7), 0, 6),
            ["row 0", "row 1", "row 2", "row 3", "row 4", "  ↓ 2 more"]
        );
        assert_eq!(
            scroll_rows(&rows(7), 6, 6),
            ["  ↑ 2 more", "row 2", "row 3", "row 4", "row 5", "row 6"]
        );
        assert_eq!(
            scroll_rows(&rows(12), 5, 6),
            [
                "  ↑ 2 more",
                "row 2",
                "row 3",
                "row 4",
                "row 5",
                "  ↓ 6 more"
            ]
        );

        for len in 1..20 {
            for room in 1..22 {
                for cursor in 0..len {
                    let shown = scroll_rows(&rows(len), cursor, room);
                    assert!(shown.len() <= room, "{len}/{room}/{cursor}: {shown:?}");
                    assert!(
                        shown.contains(&format!("row {cursor}")),
                        "{len}/{room}/{cursor}: {shown:?}"
                    );
                    let count = |prefix: &str| {
                        shown
                            .iter()
                            .find_map(|line| line.strip_prefix(prefix)?.strip_suffix(" more"))
                            .map_or(0, |n| n.parse::<usize>().unwrap())
                    };
                    let drawn = shown.iter().filter(|line| line.starts_with("row ")).count();
                    if room >= 3 {
                        assert_eq!(
                            count("  ↑ ") + drawn + count("  ↓ "),
                            len,
                            "{len}/{room}/{cursor}: {shown:?}"
                        );
                    }
                }
            }
        }
    }

    /// The pipelines tests stand in for: `release`'s own seventeen step ids,
    /// each a copy of `default`'s first step, beside the built-in pair — the
    /// skip screen reads nothing of a step but its id, and the real
    /// `.spoolway/pipelines/release.yml` is the control plane, not a fixture
    /// this crate's tests can load.
    fn with_release(mut pipelines: Pipelines) -> Pipelines {
        const RELEASE: [&str; 17] = [
            "ready",
            "upgrade",
            "fix",
            "review-fix",
            "await-fix",
            "version",
            "preflight",
            "notes",
            "candidate",
            "review-release",
            "await-release",
            "publish",
            "released",
            "recover",
            "fixture",
            "review-fixture",
            "await-fixture",
        ];
        let mut release = pipelines.get("default").unwrap().clone();
        let step = release.steps[0].clone();
        release.name = "release".to_string();
        release.steps = RELEASE
            .iter()
            .map(|id| crate::pipeline::Step {
                id: id.to_string(),
                ..step.clone()
            })
            .collect();
        pipelines.pipelines.insert("release".to_string(), release);
        pipelines
    }

    /// The popup's own top and bottom borders on `frame`, as row indices —
    /// told apart from the frame's own by the `┬`/`┴` the frame's carry
    /// between its two panes, which the popup's never do.
    fn popup_borders(frame: &[String]) -> (Option<usize>, Option<usize>) {
        let top = frame
            .iter()
            .position(|line| line.contains("┌─ trial") && !line.contains('┬'));
        let bottom = frame
            .iter()
            .position(|line| line.contains('└') && line.contains('┘') && !line.contains('┴'));
        (top, bottom)
    }

    /// Every row of the popup `panel`, as `overlay` drew it onto `frame`,
    /// still closes on its own right border — the column `overlay` cuts a
    /// popup wider than the frame at.
    fn popup_closes_every_row(frame: &[String], panel: &[String]) -> bool {
        let (Some(top), Some(bottom)) = popup_borders(frame) else {
            return false;
        };
        let x = frame[top].chars().position(|c| c == '┌').unwrap();
        let right = x + panel[0].chars().count() - 1;
        frame[top..=bottom]
            .iter()
            .all(|line| matches!(line.chars().nth(right), Some('┐' | '│' | '┘')))
    }

    /// The height bound the plan proves against: `release`'s seventeen steps
    /// on a 24-row terminal. Moving the cursor down through every one of
    /// them, each frame draws the cursor's own row, both of the popup's
    /// borders, and the frame's own bottom border under the popup. Before
    /// the skip screen scrolled, a popup this tall ran past the frame's
    /// bottom and `overlay` dropped its last rows and its bottom border.
    #[test]
    fn release_on_a_24_row_terminal_reaches_every_step_inside_the_frame() {
        let (repo, _root_guard) = fixture("screen-trial-skips-release");
        write_pending(&repo, "solo", &task_text("solo", "group: audits\n", BODY));
        let groups = listed(&repo);
        let pipelines = with_release(Pipelines::builtin());
        let release = pipelines.get("release").unwrap();
        assert_eq!(release.steps.len(), 17);

        let layout = layout_for(100, 24);
        let mut trial = TrialState::new(&pipelines, &groups[0]);
        trial.ticked = ["release".to_string()].into();
        trial.stage = TrialStage::ChooseSkips;
        trial.cursor = 0;

        for (i, step) in release.steps.iter().enumerate() {
            let panel = trial_panel(
                &groups,
                &pipelines,
                &trial,
                (checkbox_row_cap(layout), popup_height(layout)),
            )
            .unwrap();
            let frame = compose(
                two_pane_frame(&[], &[], "groups", "audits", layout),
                Some(&panel),
                "keys".to_string(),
            );
            let drawn = frame.join("\n");

            assert_eq!(
                frame.len(),
                24 - 1,
                "the frame grew under the popup:\n{drawn}"
            );
            assert!(
                drawn.contains(&format!("> [ ] {}", step.id)),
                "step {i}:\n{drawn}"
            );
            assert!(
                popup_closes_every_row(&frame, &panel),
                "step {i} lost a popup border:\n{drawn}"
            );
            assert!(
                frame[frame.len() - 2].contains('┴'),
                "the frame's own bottom border was drawn over:\n{drawn}"
            );
            if i == 0 {
                assert!(drawn.contains("↓ 7 more"), "{drawn}");
            }
            if i == 16 {
                assert!(drawn.contains("↑ 7 more"), "{drawn}");
            }

            let Mode::Trial(next) = handle_trial_key(&pipelines, trial, Key::Down) else {
                panic!("expected to stay on the trial picker");
            };
            trial = next;
        }
    }

    /// The first screen scrolls in the same kind of window once a project
    /// has more pipelines than the popup has rows: every pipeline can be
    /// reached, and the popup stays inside a 24-row frame.
    #[test]
    fn the_tick_list_scrolls_when_there_are_more_pipelines_than_rows() {
        let (repo, _root_guard) = fixture("screen-trial-pick-scroll");
        write_pending(&repo, "solo", &task_text("solo", "group: audits\n", BODY));
        let groups = listed(&repo);
        let mut pipelines = Pipelines::builtin();
        let bugfix = pipelines.get("bugfix").unwrap().clone();
        for n in 0..30 {
            let name = format!("p{n:02}");
            pipelines.pipelines.insert(name.clone(), bugfix.clone());
        }
        let names: Vec<String> = trial_pipeline_names(&pipelines)
            .into_iter()
            .map(str::to_string)
            .collect();

        let layout = layout_for(100, 24);
        let mut trial = TrialState::new(&pipelines, &groups[0]);
        trial.cursor = 0;
        for (i, name) in names.iter().enumerate() {
            let panel = trial_panel(
                &groups,
                &pipelines,
                &trial,
                (checkbox_row_cap(layout), popup_height(layout)),
            )
            .unwrap();
            assert!(panel.len() <= popup_height(layout).unwrap(), "{panel:?}");
            let frame = compose(
                two_pane_frame(&[], &[], "groups", "audits", layout),
                Some(&panel),
                "keys".to_string(),
            );
            let drawn = frame.join("\n");
            assert_eq!(
                frame.len(),
                24 - 1,
                "the frame grew under the popup:\n{drawn}"
            );
            assert!(
                frame
                    .iter()
                    .any(|line| line.contains("> [") && line.contains(&format!("] {name} "))),
                "`{name}` is not on screen with the cursor on it:\n{drawn}"
            );
            assert!(
                drawn.contains("× 1 task ="),
                "the arm count was cut:\n{drawn}"
            );
            assert!(popup_closes_every_row(&frame, &panel), "row {i}:\n{drawn}");
            if i == 0 {
                assert!(drawn.contains("↓ ") && drawn.contains(" more"), "{drawn}");
            }

            let Mode::Trial(next) = handle_trial_key(&pipelines, trial, Key::Down) else {
                panic!("expected to stay on the trial picker");
            };
            trial = next;
        }
    }

    /// Each page, drawn the way the screen draws it — `boxed` popup over a
    /// real [`two_pane_frame`], composed by the real [`overlay`] — paints
    /// every step of its own pipeline onto the frame, once, with every row
    /// closing on its right border. `overlay` writes a panel row onto the
    /// frame one character at a time and stops at the frame's own right
    /// edge, so a popup wider than the frame lost its right border on every
    /// row and its bottom border entirely.
    #[test]
    fn the_skips_popup_survives_being_overlaid_on_the_frame() {
        let (repo, _root_guard) = fixture("screen-trial-skips-overlay");
        write_pending_two(
            &repo,
            "alpha",
            &task_text("alpha", "group: audits\npipeline: bugfix\n", BODY),
            "beta",
            &task_text("beta", "group: audits\npipeline: default\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();

        let mut trial = TrialState::new(&pipelines, &groups[0]);
        trial.stage = TrialStage::ChooseSkips;
        assert_eq!(trial.ticked.len(), 2, "{:?}", trial.ticked);

        let layout = Layout {
            left: LEFT_PANE_WIDTH,
            right: RIGHT_PANE_WIDTH,
            rows: Some(40),
        };
        for name in ["bugfix", "default"] {
            let pipeline = pipelines.get(name).unwrap();
            let panel = trial_panel(
                &groups,
                &pipelines,
                &trial,
                (checkbox_row_cap(layout), popup_height(layout)),
            )
            .unwrap();
            let mut frame = two_pane_frame(&[], &[], "groups", "audits", layout);
            overlay(&mut frame, &panel);
            let drawn = frame.join("\n");

            assert!(drawn.contains(&format!("←  {name}  ")), "{drawn}");
            // Counted on a whole id rather than a bare substring, so
            // `reproduce` does not also count the `reproduce-again` row.
            for step in &pipeline.steps {
                let drew = frame
                    .iter()
                    .filter(|line| {
                        line.split('│').any(|cell| {
                            cell.trim().trim_start_matches("> ") == format!("[ ] {}", step.id)
                        })
                    })
                    .count();
                assert_eq!(drew, 1, "`{}` on the `{name}` page:\n{drawn}", step.id);
            }
            assert!(
                popup_closes_every_row(&frame, &panel),
                "a popup row lost its right border:\n{drawn}"
            );
            let width = frame[0].chars().count();
            assert_eq!(width, layout.left + layout.right + 7, "{drawn}");
            assert!(
                frame.iter().all(|line| line.chars().count() == width),
                "a frame row changed width under the popup:\n{drawn}"
            );

            let Mode::Trial(next) = handle_trial_key(&pipelines, trial, Key::Right) else {
                panic!("expected to stay on the trial picker");
            };
            trial = next;
        }
    }

    /// Neither trial screen runs off the frame when what it is drawing is a
    /// long name rather than a long pipeline. This is the same defect
    /// `look` failed the first pass for, reached through the other input:
    /// the checkbox rows were bounded by `checkbox_row_cap`, but the group
    /// name in the popup's title was not, so a long enough name overflowed
    /// the popup exactly the same way and `overlay` ate the right border off
    /// every row again.
    ///
    /// Driven at the narrowest layout `layout_for` ever produces, since
    /// that is where the budget is tightest, and across both stages, since
    /// they build their rows separately.
    #[test]
    fn a_long_group_name_is_cut_rather_than_pushing_a_trial_popup_off_the_frame() {
        let (repo, _root_guard) = fixture("screen-trial-long-names");
        let group = "a-group-with-a-really-quite-long-name-of-its-own-that-runs-on-and-on";
        write_pending_two(
            &repo,
            "alpha",
            &task_text(
                "alpha",
                &format!("group: {group}\npipeline: bugfix\n"),
                BODY,
            ),
            "beta",
            &task_text("beta", &format!("group: {group}\npipeline: bugfix\n"), BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();

        let layout = Layout {
            left: MIN_LEFT_PANE,
            right: MIN_RIGHT_PANE,
            rows: Some(40),
        };
        let frame_width = layout.left + layout.right + 7;

        for stage in [TrialStage::PickPipelines, TrialStage::ChooseSkips] {
            let mut trial = TrialState::new(&pipelines, &groups[0]);
            trial.stage = stage;
            let panel = trial_panel(
                &groups,
                &pipelines,
                &trial,
                (checkbox_row_cap(layout), popup_height(layout)),
            )
            .unwrap();

            let widest = panel.iter().map(|l| l.chars().count()).max().unwrap();
            assert!(
                widest <= frame_width,
                "{stage:?} drew a {widest}-column popup onto a {frame_width}-column frame: {panel:?}"
            );

            // And it survives the overlay intact: every row still closes on
            // its own right border, and the box still has a bottom.
            let mut frame = two_pane_frame(&[], &[], "groups", "g", layout);
            overlay(&mut frame, &panel);
            let drawn = frame.join("\n");
            assert!(
                drawn.contains('…'),
                "{stage:?} never cut the long group name at all: \n{drawn}"
            );
            assert!(
                frame.iter().any(|line| {
                    let body = line.trim();
                    body.starts_with('└') && body.ends_with('┘') && !body.contains('┬')
                }),
                "{stage:?} lost the popup's own bottom border:\n{drawn}"
            );
        }
    }

    /// Nothing under the cursor — an empty pending directory, or a filter
    /// that matched nothing — draws an empty tasks pane rather than
    /// panicking on an index that is not there.
    #[test]
    fn the_tasks_pane_is_empty_with_nothing_under_the_cursor() {
        let (repo, _root_guard) = fixture("panes-empty");
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _, focus) = tasks_pane_lines(&groups, &pipelines, &state, 40);
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(focus, (0, 0));
    }

    /// A long list is wrapped under its own label rather than truncated at
    /// the pane's edge, where the end a person needs is what goes.
    #[test]
    fn a_line_too_long_for_the_pane_is_wrapped_under_its_label() {
        let lines = wrapped("      waits on ", "login, signup, session-store", 30);
        assert!(lines.len() > 1, "expected a wrap, got {lines:?}");
        assert!(
            lines.iter().all(|line| line.chars().count() <= 30),
            "{lines:?}"
        );
        assert!(lines[0].starts_with("      waits on login"), "{lines:?}");
        assert!(
            lines[1].starts_with("               "),
            "a continuation sits under the first line's text, got {lines:?}"
        );
        assert_eq!(wrapped("  ", "short", 30), vec!["  short".to_string()]);
    }

    /// Selecting, moving focus and hiding queued groups — the browsing keys
    /// that need no submission to observe. Quitting is not among them: no
    /// mode reads a quit key any more, `ctrl-c` included, which is caught
    /// above `run_screen` rather than read as a key at all.
    #[test]
    fn browsing_keys_select_move_focus_and_hide_queued_groups() {
        let (repo, _root_guard) = fixture("screen-browse");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        let groups = listed(&repo);
        let mut state = ScreenState::new();

        handle_browse_key(&groups, &mut state, Key::Tab);
        assert_eq!(state.focus, Focus::Tasks);

        // Space selects the group, not the task under the cursor — and it does
        // so from either pane, since the tasks pane holds nothing to pick.
        let key = group_key(&groups[0]);
        handle_browse_key(&groups, &mut state, Key::Char(' '));
        assert!(state.selected.contains(&key));
        handle_browse_key(&groups, &mut state, Key::Char(' '));
        assert!(
            !state.selected.contains(&key),
            "pressing space again deselects"
        );
    }

    /// The screen opens on queueable groups only: a done group is hidden
    /// until `h`, and the one line left says how many `h` would bring back —
    /// counting the done group alone, never a queued one beside it.
    #[test]
    fn the_screen_opens_with_done_groups_hidden_and_h_brings_them_back() {
        let (repo, _root_guard) = fixture("screen-opens-hidden");
        already_done(&repo, "finished", "finished");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);
        assert_eq!(groups.len(), 2, "one done group, one queued group");

        let state = ScreenState::new();
        assert_eq!(
            state.hide_scope,
            HideScope::Pending,
            "the screen must open with done groups hidden"
        );
        let shown = visible(&groups, state.hide_scope);
        assert!(shown.is_empty(), "neither group here is queueable");
        let (rows, _) = groups_pane_lines(&groups, &shown, &state, 40);
        assert_eq!(
            rows,
            vec!["  nothing to queue — 1 group hidden".to_string()],
            "only the done group counts as hidden: {rows:?}"
        );

        let mut state = state;
        handle_browse_key(&groups, &mut state, Key::Char('h'));
        let shown = visible(&groups, state.hide_scope);
        assert_eq!(shown.len(), 1, "`h` brings the done group back");
        assert_eq!(shown[0].name, "finished");
    }

    /// `h` is a two-way switch: one press adds the done groups, the next
    /// takes them away again. A queued group is on screen at neither
    /// setting, and the filter does not reach it either.
    #[test]
    fn h_toggles_done_groups_and_never_shows_a_queued_one() {
        let (repo, _root_guard) = fixture("screen-h-toggle");
        already_done(&repo, "finished", "finished");
        write_pending(&repo, "wire", &task_text("wire", "group: sent\n", BODY));
        already_queued(&repo, "wire");
        write_pending(&repo, "cook", &task_text("cook", "group: open\n", BODY));
        let groups = listed(&repo);
        let names = |shown: Vec<&Group>| -> Vec<String> {
            shown.iter().map(|group| group.name.clone()).collect()
        };
        let mut state = ScreenState::new();

        assert_eq!(names(shown(&groups, &state)), vec!["open"]);

        handle_browse_key(&groups, &mut state, Key::Char('h'));
        assert_eq!(state.hide_scope, HideScope::PlusDone);
        assert_eq!(names(shown(&groups, &state)), vec!["open", "finished"]);

        handle_browse_key(&groups, &mut state, Key::Char('h'));
        assert_eq!(
            state.hide_scope,
            HideScope::Pending,
            "the second press switches back"
        );
        assert_eq!(names(shown(&groups, &state)), vec!["open"]);

        // The filter reaches a done group `h` is hiding, but a queued group
        // matching the query exactly stays off the screen.
        state.filter = "sent".to_string();
        assert!(
            shown(&groups, &state).is_empty(),
            "{:?}",
            names(shown(&groups, &state))
        );
        state.filter = "finished".to_string();
        assert_eq!(names(shown(&groups, &state)), vec!["finished"]);
    }

    /// The same story as the test above, but through `run_screen` itself: the
    /// opening frame's own title, empty-list line and footer, then the frame
    /// `h` draws next — every one of those is an acceptance criterion, and
    /// none of them is reachable by calling `groups_pane_lines` alone.
    #[test]
    fn the_first_drawn_frame_hides_done_groups_and_h_shows_them() {
        let (repo, _root_guard) = fixture("screen-first-frame-hidden");
        already_done(&repo, "finished", "finished");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "h");

        let frames: Vec<&str> = drawn
            .split("\x1b[?2026h\x1b[H")
            .filter(|f| !f.is_empty())
            .collect();
        assert_eq!(
            frames.len(),
            2,
            "expected the opening frame and the frame `h` draws next:\n{drawn}"
        );
        let (opening, after_h) = (frames[0], frames[1]);

        assert!(
            opening.contains("groups  0 of 1"),
            "the opening title must count the done group as not shown, and \
             the queued one not at all:\n{opening}"
        );
        // Not the full sentence: the fallback (no-terminal) width this test
        // runs at is now the narrower `MIN_LEFT_PANE`, and `two_pane_frame`'s
        // own `pad_to` cuts this row off there the same as any other line too
        // long for its pane. `the_screen_opens_with_done_groups_hidden_...`
        // above checks the untruncated string directly, off
        // `groups_pane_lines` rather than through a real frame.
        assert!(opening.contains("nothing to queue"), "{opening}");
        assert!(
            opening.contains("[space] select   [enter] queue   [f] find   [tab] tasks   [t] trial"),
            "the groups pane's line must lead with `space` and `enter`:\n{opening}"
        );
        assert!(
            !opening.contains("[g] gate") && !opening.contains("[o] open task"),
            "`g` and `o` do nothing over the groups pane, so its line names neither:\n{opening}"
        );
        assert!(
            opening.contains("[h] show done tasks"),
            "the footer must offer to show the done groups:\n{opening}"
        );

        assert!(
            after_h.contains("groups  1 of 1"),
            "`h` must bring the done group into the shown count:\n{after_h}"
        );
        assert!(after_h.contains("finished"), "{after_h}");
        assert!(!after_h.contains("queued"), "{after_h}");
        assert!(
            after_h.contains("[h] hide done tasks"),
            "the footer must offer to hide the done groups again:\n{after_h}"
        );
    }

    /// Pressing down from the last queueable group lands straight on the
    /// first done one — the separator between the two halves is a drawn
    /// row, not a group `group_cursor` ever points at, so crossing it costs
    /// exactly the one key press an ordinary move between two groups would.
    #[test]
    fn pressing_down_across_the_separator_lands_on_the_first_done_group() {
        let (repo, _root_guard) = fixture("screen-cross-separator");
        write_pending(&repo, "wire", &task_text("wire", "group: unqueued\n", BODY));
        already_done(&repo, "cook", "cook");
        let groups = listed(&repo);

        // `h` first, to bring the done group on screen at all — the screen
        // opens with it hidden — then one `Down` to cross from the one
        // queueable group onto it.
        let drawn = screen(&repo, groups, "h\x1b[B");

        let last = last_frame(&drawn);
        let cursor_row = last
            .lines()
            .find(|line| line.contains("│ >"))
            .unwrap_or_else(|| panic!("no cursor row in the final frame:\n{last}"));
        assert!(
            cursor_row.contains("cook") && cursor_row.contains("done"),
            "one `Down` past the last queueable group must land on the \
             done one, not the blank separator between them:\n{cursor_row}"
        );
    }

    /// `enter` submits only once something is actually selected — an empty
    /// batch has nothing to queue, and must not reach the dispatcher offer.
    #[test]
    fn enter_does_nothing_with_no_selection() {
        let (repo, _root_guard) = fixture("screen-enter-empty");
        write_pending(&repo, "wire", &task_text("wire", "group: a\n", BODY));
        let groups = listed(&repo);
        let mut state = ScreenState::new();

        handle_browse_key(&groups, &mut state, Key::Enter);
        assert!(matches!(state.mode, Mode::Browsing));
    }

    /// The whole submit path, headless: a scripted key sequence played
    /// through `run_screen` exactly the way an end-to-end suite pipes one in
    /// — no terminal, stdin exhausted at the end of the script, and the
    /// screen stopping on its own rather than hanging on the next read.
    #[test]
    fn the_screen_submits_a_selected_group_and_clears_its_tasks() {
        let (repo, _root_guard) = fixture("screen-submit");
        write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let groups = listed(&repo);

        // Tab onto the tasks pane, space to select the one group, enter to
        // submit it.
        screen(&repo, groups, "\t \r");

        assert!(
            repo.queue_dir().join("wire.md").exists(),
            "the task was not queued"
        );
        assert!(
            !repo.pending_dir().join("wire.md").exists(),
            "the queued group's task must be gone from pending"
        );
    }

    /// With a dispatcher already holding the queue's lock, a clean
    /// submission still writes and still only says what it queued — there is
    /// nothing to join and nothing to focus: the running dispatcher picks
    /// the task up on its next pass.
    #[test]
    fn a_dispatcher_already_holding_the_lock_changes_nothing_about_enter() {
        let (repo, _root_guard) = fixture("gate-clean-locked");
        write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let groups = listed(&repo);

        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();

        let (exit, drawn) = screen_exit(&repo, groups, "\t \r");

        assert!(repo.queue_dir().join("wire.md").exists());
        assert_eq!(exit, ScreenExit::Quit);
        assert!(!drawn.contains("go to the dispatcher"), "{drawn}");
        assert!(!drawn.contains("already running"), "{drawn}");
    }

    /// Two tasks in different groups that both change the same file: an edge
    /// across group branches would stack one plan's pull request under
    /// another's — see the plan `chains-not-fans` — so `enter` writes both
    /// straight through with no `depends_on` added, whatever files each
    /// changes. Whether that pair may safely run side by side is now judged
    /// from what each task changes, not from any glob spoolway reads.
    #[test]
    fn two_groups_write_with_no_edge_added() {
        let (repo, _root_guard) = fixture("overlap-across-groups");
        write_pending(&repo, "left", &task_text("left", "group: one\n", BODY));
        write_pending(&repo, "right", &task_text("right", "group: two\n", BODY));
        let groups = listed(&repo);

        // Space selects the highlighted group, `j` moves onto the other,
        // space selects it too, `enter` submits both.
        screen(&repo, groups, " j \r");

        assert!(repo.queue_dir().join("left.md").exists());
        assert!(repo.queue_dir().join("right.md").exists());
        assert!(queued(&repo, "left").front.depends_on.is_empty());
        assert!(queued(&repo, "right").front.depends_on.is_empty());
    }

    /// A gate chosen from the screen — `g`, down onto the second step, enter
    /// to pick it — reaches the queued task file without ever rewriting the
    /// task it came from.
    #[test]
    fn a_gate_chosen_on_the_screen_reaches_the_queued_task() {
        let (repo, _root_guard) = fixture("screen-gate");
        write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let groups = listed(&repo);

        // Tab onto tasks, space to select, g to open the gate picker, down
        // onto the pipeline's second step (`review`), enter to pick it,
        // enter to submit.
        screen(&repo, groups, "\t g\x1b[B\r\r");

        let saved = std::fs::read_to_string(repo.queue_dir().join("wire.md")).unwrap();
        assert!(saved.contains("gate_at: review"), "{saved}");
        assert!(
            !repo.pending_dir().join("wire.md").exists(),
            "still pending"
        );
    }

    /// `t` on a group forks the whole group once per ticked pipeline, each
    /// copy in its own `<group>-<pipeline>` group, ids numbered in the order
    /// the pipelines are listed — and a skip ticked in one pipeline's block
    /// reaches only that pipeline's arms. The source task, never itself
    /// submitted, is left exactly where it was.
    #[test]
    fn a_trial_forks_the_group_once_on_each_ticked_pipeline() {
        let (repo, _root_guard) = fixture("screen-trial");
        write_pending(
            &repo,
            "solo",
            &task_text("solo", "group: audits\ntouches: [src/solo.rs]\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let bugfix = pipelines.get("bugfix").unwrap();
        let handover_at = bugfix
            .steps
            .iter()
            .position(|s| s.id == "handover")
            .unwrap();

        // `t` opens the picker with `default` — what `solo` names — ticked
        // and under the cursor. `↑` onto `bugfix`, `space` ticks it, `enter`
        // advances to the skip screen, whose first block is `bugfix`'s;
        // `handover_at` more `↓` walks onto its `handover`, `space` ticks
        // it, `enter` launches.
        let keys = format!("t\x1b[A \r{} \r", "\x1b[B".repeat(handover_at));
        screen(&repo, groups, &keys);

        let first = queued(&repo, "solo-1");
        assert_eq!(first.front.pipeline.as_deref(), Some("bugfix"));
        assert_eq!(first.front.group.as_deref(), Some("audits-bugfix"));
        assert_eq!(first.front.skip, vec!["handover".to_string()]);
        assert_eq!(first.front.branch.as_deref(), Some("task/solo-1"));
        assert_eq!(first.front.trial_group.as_deref(), Some("audits"));
        assert!(
            first
                .front
                .trial
                .as_deref()
                .is_some_and(|id| id.starts_with('t')),
            "the trial id is freshly minted, not the group's own name or the task's own id: {:?}",
            first.front.trial
        );

        let second = queued(&repo, "solo-2");
        assert_eq!(second.front.pipeline.as_deref(), Some("default"));
        assert_eq!(second.front.group.as_deref(), Some("audits-default"));
        assert!(
            second.front.skip.is_empty(),
            "`handover` was ticked in bugfix's block, not default's: {:?}",
            second.front.skip
        );
        assert_eq!(second.front.trial_group.as_deref(), Some("audits"));
        assert_eq!(
            first.front.trial, second.front.trial,
            "one trial, two copies"
        );

        assert!(
            repo.pending_dir().join("solo.md").exists(),
            "the source task is a template for the arm, not itself submitted"
        );
        assert!(
            !repo.queue_dir().join("solo.md").exists(),
            "the bare id is never queued by a trial"
        );
    }

    /// A task naming no pipeline at all ticks nothing, so the picker opens
    /// with every row empty, and the pipeline a person ticks is what its
    /// arm runs. `build_trial_arm` hands `parse_submission` the source
    /// task, which refuses one with no `pipeline:` — so the tick has to be
    /// written into the task before it is parsed, or the whole batch would
    /// be abandoned with `trial refused:` and nothing minted.
    ///
    /// A bare `pipeline:` rather than no line at all, so `task`'s own
    /// fill-in steps aside and the task reads exactly as unassigned as
    /// one a person left blank by hand — the same shape `scripts/e2e`'s
    /// `task_doc` writes for this.
    #[test]
    fn a_trial_routes_a_task_that_names_no_pipeline_of_its_own() {
        let (repo, _root_guard) = fixture("screen-trial-unassigned");
        write_pending(
            &repo,
            "solo",
            &task_text(
                "solo",
                "group: audits\ntouches: [src/solo.rs]\npipeline:\n",
                BODY,
            ),
        );
        let groups = listed(&repo);

        // `t` opens with nothing ticked and the cursor on `bugfix`, the
        // first row; `space` ticks it, `enter` advances to the skips
        // screen, `enter` launches with nothing skipped.
        screen(&repo, groups, "t \r\r");

        let arm = queued(&repo, "solo-1");
        assert_eq!(arm.front.pipeline.as_deref(), Some("bugfix"));
        assert_eq!(arm.front.group.as_deref(), Some("audits-bugfix"));
        assert_eq!(arm.front.branch.as_deref(), Some("task/solo-1"));
    }

    /// The skip screen driven through the real screen loop: `→` turns from
    /// `bugfix`'s page to `default`'s, the cursor starts on that page's first
    /// step, and the skip ticked on each page reaches only that pipeline's
    /// copy. The same keys `scripts/e2e/suites/trials.sh` sends.
    #[test]
    fn right_turns_the_skip_page_and_each_page_skips_its_own_copy() {
        let (repo, _root_guard) = fixture("screen-trial-skip-pages");
        write_pending(
            &repo,
            "solo",
            &task_text("solo", "group: audits\npipeline:\n", BODY),
        );
        let groups = listed(&repo);

        // Tick `bugfix` and `default`, skip bugfix's second step, turn the
        // page, skip default's third.
        screen(&repo, groups, "t j \rj \x1b[Cjj \r");

        assert_eq!(queued(&repo, "solo-1").front.skip, vec!["fix".to_string()]);
        assert_eq!(
            queued(&repo, "solo-2").front.skip,
            vec!["document".to_string()]
        );
    }

    /// A group of more than one task, tried under two pipelines, becomes two
    /// copies of the whole chain: every arm shares the one trial id, and a
    /// task that named a sibling in `depends_on` waits on that sibling's arm
    /// in its own copy — never on another pipeline's.
    #[test]
    fn a_trial_remaps_depends_on_within_each_pipelines_own_copy() {
        let (repo, _root_guard) = fixture("screen-trial-chain");
        write_pending(&repo, "alpha", &task_text("alpha", "group: chain\n", BODY));
        write_pending(
            &repo,
            "beta",
            &task_text("beta", "group: chain\ndepends_on: [alpha]\n", BODY),
        );
        let groups = listed(&repo);

        // `t` (default ticked), `↑` onto bugfix, `space` ticks it, `enter`
        // twice past both screens.
        screen(&repo, groups, "t\x1b[A \r\r");

        let trial = queued(&repo, "alpha-1").front.trial;
        assert!(trial.is_some());
        for (n, pipeline) in [(1, "bugfix"), (2, "default")] {
            let alpha = queued(&repo, &format!("alpha-{n}"));
            let beta = queued(&repo, &format!("beta-{n}"));
            let group = format!("chain-{pipeline}");
            for arm in [&alpha, &beta] {
                assert_eq!(arm.front.pipeline.as_deref(), Some(pipeline));
                assert_eq!(arm.front.group.as_deref(), Some(group.as_str()));
                assert_eq!(arm.front.trial, trial, "every arm shares one trial id");
            }
            assert_eq!(
                beta.front.depends_on,
                vec![format!("alpha-{n}")],
                "beta's own depends_on must follow alpha into its own copy's minted id"
            );
        }
        assert_eq!(repo.queue_dir().read_dir().unwrap().count(), 4);
    }

    /// A copy's `<group>-<pipeline>` name is numbered around one the queue
    /// or the archive already holds, the way a task id is: `-2`, then `-3`.
    #[test]
    fn a_trial_names_its_copy_around_a_group_already_on_disk() {
        let (repo, _root_guard) = fixture("screen-trial-group-taken");
        write_pending(&repo, "solo", &task_text("solo", "group: audits\n", BODY));
        std::fs::write(
            repo.queue_dir().join("earlier.md"),
            "---\nid: earlier\ntitle: earlier\ngroup: audits-default\nstage: queued\n---\nbody\n",
        )
        .unwrap();
        already_done(&repo, "older", "audits-default-2");
        let groups = listed(&repo);

        screen(&repo, groups, "t\r\r");

        assert_eq!(
            queued(&repo, "solo-1").front.group.as_deref(),
            Some("audits-default-3"),
            "audits-default is queued and audits-default-2 is archived"
        );
    }

    /// A skip set belongs to one pipeline, and an arm is only given the
    /// skips its own pipeline has a step for — a set left naming a step the
    /// pipeline does not have is filtered rather than written, since
    /// `spoolway doctor` refuses a `skip:` naming a step its pipeline lacks.
    #[test]
    fn a_trial_writes_only_skips_the_arms_own_pipeline_has() {
        let (repo, _root_guard) = fixture("screen-trial-skip-filter");
        write_pending(&repo, "solo", &task_text("solo", "group: audits\n", BODY));
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();

        let mut trial = TrialState::new(&pipelines, &groups[0]);
        trial.ticked.insert("bugfix".to_string());
        trial.stage = TrialStage::ChooseSkips;
        trial.skip.insert(
            "bugfix".to_string(),
            ["reproduce".to_string(), "implement".to_string()].into(),
        );
        trial
            .skip
            .insert("default".to_string(), ["implement".to_string()].into());

        let mode = begin_trial(&repo, &pipelines, "main", &groups, &trial);
        assert!(matches!(mode, Mode::Browsing), "{mode:?}");

        assert_eq!(
            queued(&repo, "solo-1").front.skip,
            vec!["reproduce".to_string()],
            "bugfix has no `implement` step"
        );
        assert_eq!(
            queued(&repo, "solo-2").front.skip,
            vec!["implement".to_string()],
            "only default's own set reaches default's arm"
        );
    }

    /// Any refusal writes nothing at all — not the first pipeline's copy,
    /// not the part of a copy minted before the refusal. A dependency on an
    /// id nothing holds is refused by `check_dependencies_set` once every
    /// arm of every copy is built.
    #[test]
    fn a_refused_trial_writes_no_copy_at_all() {
        let (repo, _root_guard) = fixture("screen-trial-refused");
        write_pending(&repo, "alpha", &task_text("alpha", "group: chain\n", BODY));
        write_pending(
            &repo,
            "beta",
            &task_text("beta", "group: chain\ndepends_on: [alpha, nowhere]\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();

        let mut trial = TrialState::new(&pipelines, &groups[0]);
        trial.ticked.insert("bugfix".to_string());
        trial.stage = TrialStage::ChooseSkips;

        let mode = begin_trial(&repo, &pipelines, "main", &groups, &trial);
        assert!(matches!(mode, Mode::Outcome { .. }), "{mode:?}");
        assert_eq!(
            repo.queue_dir().read_dir().unwrap().count(),
            0,
            "a refused trial must write no arm of any copy"
        );
    }

    /// Two trials of the same group must read apart in the ledger — the
    /// whole reason `--trial <id>` exists is to tell one comparison batch
    /// from another, so a launch can never reuse a value a person could
    /// still be looking at from an earlier one. `crate::usage::new_trial_id`
    /// carries its own distinctness test; this checks `begin_trial` actually
    /// calls it fresh on every launch rather than deriving anything from the
    /// group or task it forked, which two runs of the exact same source
    /// would otherwise produce identically.
    #[test]
    fn two_trials_of_the_same_source_mint_two_different_trial_ids() {
        let (repo, _root_guard) = fixture("screen-trial-two-launches-a");
        write_pending(&repo, "solo", &task_text("solo", "group: audits\n", BODY));
        let groups = listed(&repo);
        screen(&repo, groups, "t\r\r");
        let first_trial = queued(&repo, "solo-1").front.trial;

        let (repo, _root_guard) = fixture("screen-trial-two-launches-b");
        write_pending(&repo, "solo", &task_text("solo", "group: audits\n", BODY));
        let groups = listed(&repo);
        screen(&repo, groups, "t\r\r");
        let second_trial = queued(&repo, "solo-1").front.trial;

        assert!(first_trial.is_some() && second_trial.is_some());
        assert_ne!(
            first_trial, second_trial,
            "two trials of the exact same group and task must still mint different ids"
        );
    }

    /// The same pair of directories a repeat submission is refused against —
    /// see `existing_task_path` — is what a trial mints around too, so an
    /// id already queued or already archived is skipped over rather than
    /// collided with.
    #[test]
    fn a_trial_mints_around_ids_already_on_disk() {
        let (repo, _root_guard) = fixture("screen-trial-mint");
        write_pending(&repo, "solo", &task_text("solo", "group: audits\n", BODY));
        already_queued(&repo, "solo-1");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("solo-2.md"),
            "---\nid: solo-2\ntitle: solo-2\nstage: done\n---\nbody\n",
        )
        .unwrap();
        let groups = listed(&repo);

        // `t`, `enter` twice past both screens.
        screen(&repo, groups, "t\r\r");

        assert!(
            repo.queue_dir().join("solo-3.md").exists(),
            "solo-1 is queued and solo-2 is archived, so the mint has to reach -3"
        );
    }

    /// An arm forked from a task already in the archive gets its own
    /// `depends_on` emptied before it is ever validated — the same emptying
    /// [`begin_routine_solo`] already makes for a solo routine pick — so a
    /// predecessor `retain` has since swept out of `archive/` cannot refuse
    /// a trial that never needed to resolve it in the first place.
    #[test]
    fn a_trial_forked_from_an_archived_task_drops_its_depends_on() {
        let (repo, _root_guard) = fixture("screen-trial-archived");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("finished.md"),
            "---\nid: finished\ntitle: finished, done\ngroup: audits\nstage: done\n\
             pipeline: default\ndepends_on: [long-gone]\n---\nbody\n",
        )
        .unwrap();
        let groups = listed(&repo);

        // `h` shows the done groups, where the archived group actually
        // lists; `t` opens the picker on it, `enter` twice past both
        // screens.
        screen(&repo, groups, "ht\r\r");

        let arm = queued(&repo, "finished-1");
        assert_eq!(
            arm.front.depends_on,
            Vec::<String>::new(),
            "an archived predecessor is not durable, so the arm must not wait on it"
        );
    }

    /// gh-359: an id long enough that its lane at the pipeline's longest step
    /// would have overrun herdr's own 32-character rule by a wide margin used
    /// to be refused here even before a trial's own suffix was added — the
    /// queue no longer measures a task id against any lane at all.
    #[test]
    fn a_long_task_id_is_not_refused_for_its_lane_name() {
        let (repo, _root_guard) = fixture("screen-trial-long-id");
        let base_id = "a".repeat(60);
        write_pending(
            &repo,
            &base_id,
            &task_text(&base_id, "group: audits\n", BODY),
        );
        let groups = listed(&repo);

        // `t`, `enter` twice past both screens — the task keeps its own
        // default assignment.
        screen(&repo, groups, "t\r\r");

        assert!(
            repo.queue_dir().read_dir().unwrap().next().is_some(),
            "gh-359: a long task id must still queue"
        );
    }

    /// A refused submission must not clear what was selected — the person
    /// who typed `enter` twice and got told no is still holding the same
    /// batch, minus whatever they fix. Selects both groups, submits, gets
    /// refused because one of them sets a reserved key, dismisses the
    /// outcome, deselects only the bad group, and resubmits — succeeding
    /// with the good one still selected from before the refusal.
    #[test]
    fn a_refused_submission_keeps_the_selection_for_a_second_try() {
        let (repo, _root_guard) = fixture("screen-refused");
        write_pending(
            &repo,
            "bogus",
            &task_text("bogus", "group: one\nstage: taken\n", BODY),
        );
        // Groups list newest-written-first, and this test's key script walks
        // the list in a fixed order — so the two birth times are pulled
        // apart with a sleep rather than trusted to land far enough apart on
        // their own. A birth time has no `set_*` counterpart the way a
        // modification time does, so this cannot be pinned after the fact.
        // Group `two` (the good task), written second, ends up first.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_pending(&repo, "good", &task_text("good", "group: two\n", BODY));
        let groups = listed(&repo);

        // space selects `two`, the highlighted (newest) group; j moves to
        // `one`; space selects it too; enter submits both and is refused — a
        // refusal shows an outcome, which is half of what this test guards;
        // `x` is the popup's to ignore and `enter` closes it; space deselects
        // `one`, which is still highlighted; enter resubmits with `two` alone.
        screen(&repo, groups, " j \rx\r \r");

        assert!(
            repo.queue_dir().join("good.md").exists(),
            "the good task was never queued after the fixed resubmission"
        );
        assert!(
            !repo.queue_dir().join("bogus.md").exists(),
            "the refused task must never reach the queue"
        );
        assert!(
            !repo.pending_dir().join("good.md").exists(),
            "the group that landed must be gone from pending"
        );
        assert!(
            repo.pending_dir().join("bogus.md").exists(),
            "a submission that failed validation removes nothing"
        );
    }

    /// The screen ending with nothing selected and `enter` never pressed
    /// writes nothing — cancelling by running out of input is silent.
    #[test]
    fn ending_the_screen_queues_nothing() {
        let (repo, _root_guard) = fixture("screen-quit");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        screen(&repo, groups, "\t ");

        assert!(!repo.queue_dir().join("wire.md").exists());
    }

    /// `q` has no arm of its own left in `run_screen`'s `Mode::Browsing`
    /// match: it falls to `handle_browse_key`'s own catch-all, the same as
    /// any other key nothing there recognises, and does nothing. `ctrl-c`
    /// is the only way out of the screen now, caught above `run_screen` and
    /// read back through `stop::asked()` in `wait_for_key` — not reachable
    /// from this in-memory reader, so it is exercised at the platform
    /// level instead. `q` from a mode that is *not* browsing still quits —
    /// that is `q_quits_from_a_mode_that_is_not_browsing` just below.
    #[test]
    fn q_does_nothing_while_browsing() {
        let (repo, _root_guard) = fixture("screen-q-inert");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        // `q` first, then a real select-and-submit: if `q` still quit,
        // neither the space nor the enter after it would ever run, and
        // nothing would land in the queue.
        screen(&repo, groups, "q \r");

        assert!(
            repo.queue_dir().join("wire.md").exists(),
            "`q` must not have quit before the submission that followed it"
        );
    }

    /// A popup takes every key until `enter` closes it — `q` included. A
    /// refused submission puts one up; `q` under it neither quits nor
    /// closes it, so a message nobody has read yet is never taken away by a
    /// key meant for the screen underneath.
    ///
    /// End of input ends the loop too, so the run ending proves nothing on
    /// its own. The last frame is the evidence: the popup is still on it.
    #[test]
    fn q_under_a_popup_is_the_popups_to_ignore() {
        let (repo, _root_guard) = fixture("screen-quit-report");
        write_pending(
            &repo,
            "bogus",
            &task_text("bogus", "group: one\nstage: taken\n", BODY),
        );
        let groups = listed(&repo);

        // Space selects, enter submits and is refused for a reserved key,
        // putting the refusal up; `q` is pressed under it.
        let (exit, drawn) = screen_exit(&repo, groups, " \rq");

        assert_eq!(exit, ScreenExit::Quit, "the input ran out");
        let frame = last_frame(&drawn);
        assert!(frame.contains("┌─ submission refused "), "{frame}");
        assert!(frame.contains("[enter] confirm"), "{frame}");
        assert!(frame.contains("─ groups"), "the tab under it: {frame}");
    }

    /// `enter` only queues. A clean submission lands and the screen says
    /// what it queued in a popup: no overview, no overrides gate and no warnings
    /// screen, even with `unattended.enabled` on and something to warn
    /// about — asking those is the dispatch tab's `enter`, before it starts
    /// a dispatcher, and this screen never starts one.
    #[test]
    fn enter_only_queues_and_says_what_it_queued() {
        let (mut repo, _root_guard) = fixture("screen-enter-only-queues");
        repo.config.unattended.enabled = true;
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        let (exit, drawn) = screen_exit(&repo, groups, " \r");

        assert_eq!(exit, ScreenExit::Quit);
        assert!(repo.queue_dir().join("wire.md").exists());
        assert!(!drawn.contains("queued  1 group"), "{drawn}");
        assert!(!drawn.contains("before this run starts"), "{drawn}");
        assert!(!drawn.contains("start a dispatcher"), "{drawn}");
        // With issue tracking off nothing is asked: the popup says what was
        // queued, and `enter` closes it.
        let frame = last_frame(&drawn);
        assert!(frame.contains("┌─ queued "), "{frame}");
        assert!(frame.contains("queued 1 task"), "{frame}");
        assert!(!drawn.contains("┌─ issue tracking "), "{drawn}");
        // No dispatcher holds this fixture's lock, so the popup ends by
        // saying one has to be started.
        assert!(
            frame.contains("│  Start the dispatcher to begin working "),
            "{frame}"
        );
        assert!(!frame.contains("Dispatcher is running"), "{frame}");
    }

    /// The queued popup as the plan's mockup draws it: the task list, a
    /// blank row, the dispatcher line, a blank row, `[enter] confirm` — in
    /// the 60 columns [`issue_body`] holds it to.
    #[test]
    fn the_queued_popup_ends_on_the_dispatcher_line_over_confirm() {
        let ids: Vec<String> = [
            "hook-failure-pauses",
            "hook-started-and-check",
            "task-labels",
            "jira-story-subtasks",
        ]
        .map(str::to_string)
        .to_vec();
        let Mode::Queued { panel, .. } = queued_panel(
            &ids,
            &[],
            &[],
            Some("Start the dispatcher to begin working"),
            None,
        ) else {
            panic!("a landed batch is Mode::Queued");
        };
        assert_eq!(
            panel,
            [
                "┌─ queued ─────────────────────────────────────────────────┐",
                "│                                                          │",
                "│  queued 4 tasks                                          │",
                "│    hook-failure-pauses                                   │",
                "│    hook-started-and-check                                │",
                "│    task-labels                                           │",
                "│    jira-story-subtasks                                   │",
                "│                                                          │",
                "│  Start the dispatcher to begin working                   │",
                "│                                                          │",
                "│  [enter] confirm                                         │",
                "└──────────────────────────────────────────────────────────┘",
            ]
        );
    }

    /// The tasks a batch left out, under their own heading below the ones
    /// it queued: a dependent annotated with what it waits on, the
    /// annotations in one column, then one sentence for the task whose own
    /// start branch is missing, wrapped to the popup's width.
    #[test]
    fn the_queued_popup_lists_what_it_left_out_and_says_what_to_set() {
        let ids: Vec<String> = ["eval-totals-toggle", "eval-totals-total-line"]
            .map(str::to_string)
            .to_vec();
        let not_queued = [
            NotQueued {
                id: "archive-index-file".to_string(),
                why: NotQueuedWhy::MissingStart("task/queue-tab-reader-thread".to_string()),
            },
            NotQueued {
                id: "archive-index-readers".to_string(),
                why: NotQueuedWhy::DependsOn("archive-index-file".to_string()),
            },
            NotQueued {
                id: "retention-and-logs".to_string(),
                why: NotQueuedWhy::DependsOn("archive-index-readers".to_string()),
            },
        ];
        let Mode::Queued { panel, .. } =
            queued_panel(&ids, &[], &not_queued, Some("Dispatcher is running"), None)
        else {
            panic!("a landed batch is Mode::Queued");
        };
        assert!(panel[0].starts_with("┌─ queued "), "{panel:#?}");
        let body: Vec<&str> = panel[1..panel.len() - 1]
            .iter()
            .map(|row| {
                row.trim_start_matches("│  ")
                    .trim_end_matches('│')
                    .trim_end()
            })
            .collect();
        assert_eq!(
            body,
            [
                "",
                "queued 2 tasks",
                "  eval-totals-toggle",
                "  eval-totals-total-line",
                "",
                "not queued, start branch doesn't exist",
                "  archive-index-file",
                "  archive-index-readers   (depends on archive-index-file)",
                "  retention-and-logs      (depends on archive-index-readers)",
                "",
                "archive-index-file starts from task/queue-tab-reader-thread, which",
                "doesn't exist. Set starts_from: in the task front matter and",
                "requeue.",
                "",
                "Dispatcher is running",
                "",
                "[enter] confirm",
            ],
            "{panel:#?}"
        );
    }

    /// A batch that left every task out queued nothing, and its popup says
    /// only that, under its own title.
    #[test]
    fn a_batch_that_queued_nothing_draws_only_what_it_left_out() {
        let not_queued = [NotQueued {
            id: "wire".to_string(),
            why: NotQueuedWhy::MissingStart("task/gone".to_string()),
        }];
        let Mode::Queued { panel, .. } = queued_panel(&[], &[], &not_queued, None, None) else {
            panic!("a batch that left everything out is still Mode::Queued");
        };
        let all = panel.join("\n");
        assert!(panel[0].starts_with("┌─ not queued "), "{all}");
        assert!(!all.contains("queued 0"), "{all}");
        assert!(all.contains("wire starts from task/gone"), "{all}");
    }

    /// The `issues created` form carries the same line, under the count
    /// rather than a task list.
    #[test]
    fn the_issues_created_popup_ends_on_the_dispatcher_line_too() {
        let ids = vec!["wire".to_string()];
        let tickets = vec!["task issue   created   #7   wire".to_string()];
        let Mode::Queued { panel, .. } =
            queued_panel(&ids, &tickets, &[], Some("Dispatcher is running"), None)
        else {
            panic!("a landed batch is Mode::Queued");
        };
        let body: Vec<&str> = panel
            .iter()
            .map(|line| line.trim_matches(['│', ' ']))
            .collect();
        assert!(panel[0].starts_with("┌─ issues created "), "{panel:?}");
        assert_eq!(
            body[1..panel.len() - 1],
            [
                "",
                "task issue   created   #7   wire",
                "",
                "queued 1 task",
                "",
                "Dispatcher is running",
                "",
                "[enter] confirm",
            ],
            "{panel:?}"
        );
    }

    /// A live dispatcher's lock turns the line into `Dispatcher is running`.
    /// This process takes the dispatcher's own lock, the one a real
    /// `spoolway dispatch` holds for as long as it runs.
    #[test]
    fn a_held_dispatcher_lock_says_the_dispatcher_is_running() {
        let (repo, _root_guard) = fixture("queued-popup-dispatcher-running");
        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false, None).unwrap();
        assert_eq!(dispatcher_line(&repo), Some("Dispatcher is running"));
    }

    /// Only the dispatcher's lock decides the line: the screen's own lock,
    /// which the screen showing this popup always holds, leaves it saying
    /// no dispatcher is up.
    #[test]
    fn the_screens_own_lock_does_not_count_as_a_dispatcher() {
        let (repo, _root_guard) = fixture("queued-popup-screen-lock");
        let _screen = crate::lock::Lock::acquire(&repo.screen_lock_file(), false, None).unwrap();
        assert_eq!(
            dispatcher_line(&repo),
            Some("Start the dispatcher to begin working")
        );
    }

    /// A lock file that cannot be read leaves the line off: a directory in
    /// its place fails the read with something other than "not found", and
    /// the popup must not guess either way.
    #[test]
    fn an_unreadable_dispatcher_lock_leaves_the_line_off() {
        let (repo, _root_guard) = fixture("queued-popup-lock-unreadable");
        std::fs::create_dir_all(repo.lock_file()).unwrap();
        assert_eq!(dispatcher_line(&repo), None);
        let Mode::Queued { panel, .. } = queued_panel(
            &["wire".to_string()],
            &[],
            &[],
            dispatcher_line(&repo),
            None,
        ) else {
            panic!("a landed batch is Mode::Queued");
        };
        let flat = panel.join("\n");
        assert!(!flat.contains("Dispatcher is running"), "{flat}");
        assert!(!flat.contains("Start the dispatcher"), "{flat}");
        assert!(flat.contains("[enter] confirm"), "{flat}");
    }

    /// Nothing in the pending directory is not an error — the screen opens
    /// onto an empty list rather than refusing. Driven through `run_screen`
    /// with a scripted input, never the process's own stdin: whether that
    /// ever returns depends on what the harness handed it — a closed
    /// `/dev/null` ends the screen at once, a pipe with a writer that never
    /// closes keeps it redrawing forever, which is how this test once hung
    /// `cargo test` for ten minutes.
    #[test]
    fn opening_the_screen_with_nothing_pending_does_not_error() {
        let (repo, _root_guard) = fixture("screen-nothing-pending");
        let groups = listed(&repo);
        assert_eq!(opening_message(&repo, &groups), None);
        let (exit, _) = screen_exit(&repo, groups, "");
        assert_eq!(exit, ScreenExit::Quit);
    }

    /// An empty pending directory prints nothing and opens the screen. It
    /// used to print a message naming `/spoolway-plan`, a bundled sample
    /// skill the binary does not depend on and must not advertise here.
    #[test]
    fn an_empty_pending_directory_opens_the_screen_with_no_message() {
        let (repo, _root_guard) = fixture("opening-message-empty");
        let groups = listed(&repo);
        assert!(groups.is_empty());
        assert_eq!(
            opening_message(&repo, &groups),
            None,
            "an empty pending directory has nothing to say instead of opening"
        );

        let state = ScreenState::new();
        let shown = visible(&groups, state.hide_scope);
        let (rows, _) = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows,
            vec!["  nothing to queue".to_string()],
            "no groups at all means no hidden count either: {rows:?}"
        );
    }

    /// The whole of the empty screen, through `run_screen` itself: the frame
    /// draws, nothing names a skill, and the screen ends on its own with no
    /// key to read.
    #[test]
    fn the_empty_screen_draws_a_frame_and_names_no_planning_skill() {
        let (repo, _root_guard) = fixture("screen-empty-frame");
        let groups = listed(&repo);

        let (exit, drawn) = screen_exit(&repo, groups, "");

        assert_eq!(exit, ScreenExit::Quit);
        assert!(
            drawn.contains("groups  0 of 0"),
            "the empty screen still draws its title:\n{drawn}"
        );
        assert!(drawn.contains("nothing to queue"), "{drawn}");
        assert!(
            drawn.contains("[h] show done tasks"),
            "the footer must draw too:\n{drawn}"
        );
        assert!(
            !drawn.contains("spoolway-plan"),
            "no skill may be named on this screen:\n{drawn}"
        );
    }

    /// A task with no readable `group:` is named, not silently skipped —
    /// the one diagnostic `pending::unreadable` exists for.
    #[test]
    fn opening_message_names_an_unreadable_task() {
        let (repo, _root_guard) = fixture("opening-message-unreadable");
        std::fs::write(
            repo.pending_dir().join("no-group.md"),
            "---\nid: stray\ntitle: stray\n---\n## Goal\n\nx\n",
        )
        .unwrap();

        let groups = listed(&repo);
        let msg = opening_message(&repo, &groups).expect("nothing readable to open onto");
        assert!(msg.contains("no-group.md"), "{msg}");
    }

    /// The regression this guards: a group already queued fills `groups`
    /// from the queue directory alone (see `list_groups`), which used to
    /// make `groups.is_empty()` false and hide the unreadable-task
    /// diagnostic behind that unrelated row — even though the task named
    /// above has nothing to do with the group already queued below.
    #[test]
    fn opening_message_still_names_an_unreadable_task_beside_an_unrelated_queued_group() {
        let (repo, _root_guard) = fixture("opening-message-unreadable-and-queued");
        std::fs::write(
            repo.pending_dir().join("no-group.md"),
            "---\nid: stray\ntitle: stray\n---\n## Goal\n\nx\n",
        )
        .unwrap();
        std::fs::write(
            repo.queue_dir().join("shipped.md"),
            "---\nid: shipped\ntitle: shipped\ngroup: already-gone\nstage: queued\n---\nbody\n",
        )
        .unwrap();

        let groups = listed(&repo);
        assert!(!groups.is_empty(), "the queue-only group fills the list");
        let msg = opening_message(&repo, &groups).expect("the unreadable task must still be named");
        assert!(msg.contains("no-group.md"), "{msg}");
    }

    /// The regression the fix above overcorrected into: a stray task with
    /// no `group:` must not swallow the whole screen when a real, queueable
    /// group is sitting right beside it in the same directory. Before this,
    /// `opening_message` checked `unreadable` unconditionally, so a single
    /// bad task anywhere in pending refused to open the screen at all —
    /// verified against a real build, where a directory holding one good
    /// task and one stray one used to draw the good group's row and now
    /// printed "Nothing to list" instead.
    #[test]
    fn opening_message_opens_the_screen_past_a_stray_task_beside_a_real_group() {
        let (repo, _root_guard) = fixture("opening-message-stray-beside-real-group");
        write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: real-group\n", BODY),
        );
        std::fs::write(
            repo.pending_dir().join("no-group.md"),
            "---\nid: stray\ntitle: stray\n---\n## Goal\n\nx\n",
        )
        .unwrap();

        let groups = listed(&repo);
        assert!(
            groups.iter().any(|g| g.state == GroupState::Queueable),
            "there is a real, queueable group here"
        );
        assert_eq!(
            opening_message(&repo, &groups),
            None,
            "a queueable group beside a stray task must still open the screen"
        );
    }

    /// An empty pending directory used to be indistinguishable from a project
    /// with nothing queued at all, and the screen printed "No
    /// tasks" instead of opening. A group whose tasks have already
    /// been submitted still has a row — built from the queue directory — so
    /// the screen has something to open onto even here.
    #[test]
    fn a_queue_only_group_still_opens_the_screen_over_an_empty_pending_directory() {
        let (repo, _root_guard) = fixture("screen-queue-only");
        // Not `already_queued`: that helper writes a minimal task with
        // no `group:` at all, which `list_groups` cannot file under any row
        // — this is the shape a real submission would actually leave behind.
        std::fs::write(
            repo.queue_dir().join("shipped.md"),
            "---\nid: shipped\ntitle: shipped\ngroup: already-gone\nstage: queued\n---\nbody\n",
        )
        .unwrap();

        let groups = listed(&repo);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].state, GroupState::Queued);

        let state = ScreenState::new();
        let shown = visible(&groups, state.hide_scope);
        assert!(
            shown.is_empty(),
            "never on screen, same as any other queued group"
        );
        let (rows, _) = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows,
            vec!["  nothing to queue".to_string()],
            "no key here brings a queued group back, so none counts as hidden: {rows:?}"
        );
    }

    // ----------------------------------------------------------- the open key

    /// `o` is gated exactly the way `g` is: with the groups pane focused it
    /// does nothing at all, rather than opening whatever the group cursor
    /// happens to sit on. Never touches a multiplexer, so this passes
    /// whatever backend the fixture's default config names.
    #[test]
    fn o_does_nothing_while_the_groups_pane_has_focus() {
        let (repo, _root_guard) = fixture("screen-open-groups-focus");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "o");

        assert!(
            !drawn.contains("┌─ open task "),
            "`o` with the groups pane focused must never reach `Mode::Outcome`:\n{drawn}"
        );
    }

    /// `o` on a headless run has no pane to open an editor in — headless
    /// refuses it the way `Mux::open_command`'s default does — and the refusal is
    /// surfaced through `Mode::Outcome` rather than lost: an `Err` this
    /// discarded would leave a person pressing `o` on a headless run with no
    /// sign the key did anything at all.
    #[test]
    fn pressing_o_with_no_multiplexer_surfaces_the_refusal() {
        let (mut repo, _root_guard) = fixture("screen-open-headless");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        // `Tab` moves focus onto the tasks pane, where `o` is gated live —
        // see `open_highlighted`'s own doc comment.
        let drawn = screen(&repo, groups, "\to");

        let last = last_frame(&drawn);
        assert!(last.contains("o:"), "{last}");
        assert!(last.contains("┌─ open task "), "in a popup: {last}");
        assert!(last.contains("─ groups"), "over the tab: {last}");
    }

    // -------------------------------------------------------------- the filter

    /// Typing `fqueue` narrows the left pane to the groups whose name scores
    /// above the floor against it, and leaves the one group with nothing in
    /// common with the query off the list entirely.
    #[test]
    fn playing_f_queue_narrows_the_pending_pane_to_matching_names() {
        let (repo, _root_guard) = fixture("screen-filter-narrows");
        for name in ["queue-browse", "queue-second-group", "home-state"] {
            write_pending(
                &repo,
                name,
                &task_text(name, &format!("group: {name}\n"), BODY),
            );
        }
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "fqueue");

        let last = last_frame(&drawn);
        assert!(last.contains("find: queue"), "{last}");
        assert!(last.contains("queue-browse"), "{last}");
        assert!(last.contains("queue-second-group"), "{last}");
        assert!(
            !last.contains("home-state"),
            "a group with no match on its name must drop out of the filtered list:\n{last}"
        );
        assert!(last.contains("groups  2 of 3"), "{last}");
    }

    /// `t`, `s` and `p` are ordinary text while the filter box has focus —
    /// this task's own acceptance criterion that the search box takes every
    /// letter, `handle_filter_key` no longer carving any of the three out
    /// as an action the way `run_screen`'s old `Mode::Filter` arm did.
    #[test]
    fn every_letter_types_while_the_filter_box_has_focus() {
        let (repo, _root_guard) = fixture("screen-filter-every-letter");
        write_pending(&repo, "solo", &task_text("solo", "group: demo\n", BODY));
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "ftsp");

        let last = last_frame(&drawn);
        assert!(
            last.contains("find: tsp"),
            "`t`, `s` and `p` must all reach the query as ordinary text:\n{last}"
        );
        assert!(!last.contains("trial demo"), "{last}");
        assert!(!last.contains("save demo as a routine"), "{last}");
    }

    /// The filter footer names only how to leave the search box — the
    /// ordinary queue actions it used to list are gone along with `p` and
    /// `s`'s old reservation, since none of them apply while every letter
    /// is text.
    #[test]
    fn the_filter_footer_names_only_leaving_search() {
        let (repo, _root_guard) = fixture("screen-filter-footer");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "f");

        let last = last_frame(&drawn);
        assert!(last.contains("[enter] leave search"), "{last}");
        assert!(!last.contains("f find"), "{last}");
        assert!(!last.contains("esc back"), "{last}");
        assert!(!last.contains("type to narrow"), "{last}");
        assert!(!last.contains("q is a letter here"), "{last}");
        assert!(!last.contains("enter keep filter"), "{last}");
    }

    /// A filter reaches a done group `h` is hiding, but never a queued one:
    /// no key on this screen can act on a queued group, so a query naming
    /// it exactly still leaves the pane empty.
    #[test]
    fn a_filter_never_reaches_a_queued_group() {
        let (repo, _root_guard) = fixture("screen-filter-reaches-queued");
        write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: queue-browse\n", BODY),
        );
        already_queued(&repo, "wire");
        already_done(&repo, "shipped", "queue-shipped");
        let groups = listed(&repo);

        // `hide_scope` starts at `HideScope::Pending` — see `ScreenState::new`
        // — so without the filter neither group would be drawn at all.
        let drawn = screen(&repo, groups, "fqueue");

        let last = last_frame(&drawn);
        assert!(
            !last.contains("queue-browse"),
            "the queued group must stay off the pane:\n{last}"
        );
        let row = last
            .lines()
            .find(|line| line.contains("queue-shipped") && line.contains("done"))
            .unwrap_or_else(|| panic!("the done group never made it onto the pane:\n{last}"));
        // Stripped of ANSI first: every row not the pane's own full width now
        // carries the frame writer's own `ESC[K` clear-to-end, which is a
        // `[` that has nothing to do with a checkbox.
        assert!(
            !crate::status::strip_ansi(row).contains('['),
            "a done group draws no checkbox:\n{row}"
        );
    }

    /// `q` typed while the filter box has focus is kept as an ordinary
    /// character rather than doing anything special, and the same key does
    /// nothing once the filter is closed and browsing resumes either — the
    /// two halves of this task's own acceptance criterion on `q`, in one
    /// test so neither can pass by accident while the other regresses.
    #[test]
    fn q_is_a_letter_in_the_filter_and_does_nothing_in_browsing() {
        let (repo, _root_guard) = fixture("screen-filter-q");
        write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: queue-browse\n", BODY),
        );
        let groups = listed(&repo);

        // `fq` types `q` into the filter — it must not do anything special
        // there — `\r` keeps that one-character filter and returns to
        // browsing, and the second `q` is a no-op: the screen only ends
        // because the input ran out right behind it.
        let (exit, drawn) = screen_exit(&repo, groups, "fq\rq");

        assert_eq!(exit, ScreenExit::Quit);
        // Four frames: the first ordinary browsing draw, the empty filter
        // box `f` just opened, the filter box holding `q` after it was
        // typed, and browsing again after `\r`. No fifth: a `q` that did
        // nothing draws the same frame browsing already had, which `draw`
        // dedups away — a fifth here would mean `q` had reached the filter
        // a second time, or changed browsing state, instead of being inert.
        let frames: Vec<&str> = drawn.split("\x1b[?2026h\x1b[H").skip(1).collect();
        assert_eq!(frames.len(), 4, "{frames:?}");
        assert!(frames[2].contains("find: q▏"), "{}", frames[2]);
    }

    /// A control byte other than `0x7f` — `0x08` (BS), what ctrl-h and many
    /// terminals send for backspace — is not appended to the filter as an
    /// invisible character. Review's own finding: `read_key` maps it to
    /// `Key::Char('\u{8}')`, which the filter's typing arm used to accept
    /// like any other character.
    #[test]
    fn a_control_byte_is_not_typed_into_the_filter() {
        let (repo, _root_guard) = fixture("screen-filter-control-byte");
        write_pending(
            &repo,
            "wire",
            &task_text("wire", "group: queue-browse\n", BODY),
        );
        let groups = listed(&repo);

        // `q` then `0x08` (ctrl-h) then `q` again: if the control byte were
        // typed in, the query would read `q\u{8}q`, not the backspace this
        // is meant to be — leaving no `q` in the filter at all.
        let drawn = screen(&repo, groups, "fq\x08q");

        let last = last_frame(&drawn);
        assert!(last.contains("find: q▏"), "{last}");
    }

    // ---------------------------------------------------------- the routines pane

    /// One task under `.spoolway/routines/<folder>/`, the same bytes a
    /// `--from` entry would read.
    fn write_routine(repo: &Repo, folder: &str, id: &str, doc: &str) -> std::path::PathBuf {
        let dir = repo.routines_dir().join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.md"));
        std::fs::write(&path, doc).unwrap();
        path
    }

    /// The routines tab opens on the routine list, and its own footer names
    /// `space`/`enter`/`tab` rather than the pending screen's keys.
    #[test]
    fn the_routines_tab_opens_on_the_routine_list() {
        let (repo, _root_guard) = fixture("routines-tab-opens");
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );

        let drawn = routines_screen(&repo, "");
        let last = last_frame(&drawn);

        assert!(last.contains("routines  1 of 1"), "{last}");
        assert!(last.contains("nightly"), "{last}");
        assert!(last.contains("1 task"), "{last}");
        assert!(
            last.contains(
                "[space] select   [enter] queue   [n] new job   [x] delete   [tab] tasks"
            ),
            "{last}"
        );
        assert!(!last.contains("[esc] back"), "{last}");
        assert!(
            !last.contains("[o] open task"),
            "no task under the cursor: {last}"
        );
        assert!(!last.contains("r pending"), "{last}");
    }

    /// `o` does nothing while the folders pane has focus — a folder has no
    /// task of its own to open — the same gate [`open_highlighted`]
    /// gives the pending screen's own `o`.
    #[test]
    fn o_does_nothing_while_the_folders_pane_has_focus_in_routines() {
        let (repo, _root_guard) = fixture("routines-open-folders-focus");
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );

        let drawn = routines_screen(&repo, "o");

        assert!(
            !drawn.contains("┌─ open task "),
            "`o` with the folders pane focused must never reach `Mode::Outcome`:\n{drawn}"
        );
    }

    /// `o` on a headless run has no pane to open an editor in, in the
    /// routines pane exactly as it does on the pending screen — see
    /// `open_highlighted_routine`'s own doc comment — and staying in
    /// `Mode::Routines` afterwards, not falling back to `Mode::Browsing`.
    #[test]
    fn pressing_o_in_routines_with_no_multiplexer_surfaces_the_refusal() {
        let (mut repo, _root_guard) = fixture("routines-open-headless");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );

        // `tab` focuses the tasks pane on `audit-deps`, `o` tries to open
        // it.
        let drawn = routines_screen(&repo, "\to");

        let last = last_frame(&drawn);
        assert!(last.contains("o:"), "{last}");
        assert!(last.contains("┌─ open task "), "in a popup: {last}");
        assert!(last.contains("─ routines"), "over the pane: {last}");
    }

    /// A subfolder inside a routine is never a row of its own: the list is
    /// one row per folder directly under `.spoolway/routines/`, and the
    /// nested task shows in its routine's tasks pane instead.
    #[test]
    fn a_subfolder_is_no_row_and_its_tasks_show_under_its_routine() {
        let (repo, _root_guard) = fixture("routines-flat-list");
        write_routine(
            &repo,
            "maintenance",
            "sweep",
            &task_text("sweep", "group: maintenance\n", BODY),
        );
        write_routine(
            &repo,
            "maintenance/weekly",
            "prune",
            &task_text("prune", "group: maintenance\n", BODY),
        );

        let drawn = routines_screen(&repo, "");
        let last = last_frame(&drawn);

        assert!(last.contains("routines  1 of 1"), "{last}");
        assert!(last.contains("> [ ] maintenance"), "{last}");
        assert!(!last.contains("weekly"), "a subfolder is no row: {last}");
        assert!(last.contains("  sweep"), "{last}");
        assert!(last.contains("  prune"), "{last}");
    }

    /// `tab` moves between the two panes and back again, keeping each
    /// pane's cursor where it was, and the arrows move nothing at all —
    /// they belong to the tab strip.
    #[test]
    fn tab_moves_between_the_panes_and_the_arrows_do_nothing() {
        let (repo, _root_guard) = fixture("routines-tab-nav");
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        write_routine(
            &repo,
            "nightly",
            "audit-docs",
            &task_text("audit-docs", "group: nightly\n", BODY),
        );
        write_routine(
            &repo,
            "release",
            "tag",
            &task_text("tag", "group: release\n", BODY),
        );
        let routines = super::routines::list_routines(&repo).unwrap();

        let mut nav = RoutineNav::new();
        for key in [Key::Right, Key::Left, Key::Right] {
            handle_routine_key(&routines, &mut nav, key);
        }
        assert_eq!(nav.focus, Focus::Groups, "the arrows move no focus");
        assert_eq!(nav.folder_cursor, 0, "nor the cursor");

        handle_routine_key(&routines, &mut nav, Key::Tab);
        assert_eq!(nav.focus, Focus::Tasks);
        handle_routine_key(&routines, &mut nav, Key::Down);
        assert_eq!(nav.task_cursor, 1);
        handle_routine_key(&routines, &mut nav, Key::Left);
        assert_eq!(nav.focus, Focus::Tasks, "`←` does not go back either");

        handle_routine_key(&routines, &mut nav, Key::Tab);
        assert_eq!(nav.focus, Focus::Groups);
        assert_eq!(nav.folder_cursor, 0, "still on `nightly`");
        handle_routine_key(&routines, &mut nav, Key::Tab);
        assert_eq!(nav.task_cursor, 1, "the tasks pane's cursor kept");
    }

    /// Over the tasks pane the key line names `o` and `tab` back to the
    /// routines, and `esc` moves the cursor back to the list.
    #[test]
    fn esc_over_the_tasks_pane_goes_back_to_the_list() {
        use crate::screen::shell::{Hosting, Tab};
        let (repo, _root_guard) = fixture("routines-esc-tasks");
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        let _hosting = Hosting::open(Tab::Routines);

        let tasks = last_frame(&routines_screen(&repo, "\t")).to_string();
        assert!(
            tasks.contains(
                " [space] select   [enter] queue   [o] open task   [tab] routines   [q] quit"
            ),
            "{tasks}"
        );
        assert!(tasks.contains("> audit-deps"), "{tasks}");

        let back = last_frame(&routines_screen(&repo, "\t\x1b")).to_string();
        assert!(
            back.contains("routines  1 of 1"),
            "still in the view: {back}"
        );
        assert!(back.contains("> [ ] nightly"), "{back}");
        assert!(
            back.contains(" [space] select   [enter] queue   [n] new job   [x] delete   [tab] tasks   [q] quit"),
            "{back}"
        );
    }

    /// Ticking a routine queues its nested subfolder's tasks with it.
    #[test]
    fn enter_on_a_routine_queues_its_nested_tasks_with_it() {
        let (repo, _root_guard) = fixture("routines-enter-nested");
        write_routine(
            &repo,
            "maintenance",
            "sweep",
            &task_text("sweep", "group: maintenance\n", BODY),
        );
        // `prune` depends on `sweep`: a group is one chain, and both share
        // `group: maintenance` here — nesting is what this test is
        // actually about, not whether the two run independently.
        write_routine(
            &repo,
            "maintenance/weekly",
            "prune",
            &task_text("prune", "group: maintenance\ndepends_on: [sweep]\n", BODY),
        );

        let (exit, drawn) = routines_exit(&repo, " \r");
        assert_eq!(exit, ScreenExit::Quit);

        let mut queued_files: Vec<String> = std::fs::read_dir(repo.queue_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        queued_files.sort();
        assert_eq!(
            queued_files,
            vec!["prune-1.md".to_string(), "sweep-1.md".to_string()],
            "{}",
            last_frame(&drawn)
        );
    }

    /// A job on `routine` in `scope`'s store, as the jobs tab would write it.
    fn write_job(repo: &Repo, scope: crate::jobs::Scope, name: &str, routine: &str) {
        crate::jobs::write(
            repo,
            scope,
            name,
            &crate::jobs::JobSpec {
                schedule: "0 3 * * *".to_string(),
                pipeline: "bugfix".to_string(),
                routine: routine.to_string(),
                enabled: true,
            },
        )
        .unwrap();
    }

    /// Two routines, `maintenance` and `nightly`, sorted in that order, with
    /// `nightly` holding two tasks — the shape the mockup draws.
    fn seed_two_routines(repo: &Repo) {
        write_routine(
            repo,
            "maintenance",
            "prune",
            &task_text("prune", "group: maintenance\n", BODY),
        );
        write_routine(
            repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        write_routine(
            repo,
            "nightly",
            "audit-docs",
            &task_text("audit-docs", "group: nightly\n", BODY),
        );
    }

    fn job_names(repo: &Repo) -> Vec<String> {
        crate::jobs::load(repo)
            .unwrap()
            .into_iter()
            .map(|job| job.name)
            .collect()
    }

    /// A job points into a routine when its path is the folder or anything
    /// inside it; a job on another routine, even one whose name starts the
    /// same way, is left alone.
    #[test]
    fn a_routine_claims_its_whole_folder_and_single_task_jobs_only() {
        use crate::jobs::Scope;
        let (repo, _root_guard) = fixture("routine-delete-match");
        seed_two_routines(&repo);
        write_routine(
            &repo,
            "nightly-extra",
            "other",
            &task_text("other", "group: nightly-extra\n", BODY),
        );
        write_job(&repo, Scope::User, "whole", "nightly");
        write_job(&repo, Scope::Project, "single", "nightly/audit-docs.md");
        write_job(&repo, Scope::User, "elsewhere", "maintenance");
        write_job(&repo, Scope::User, "prefix", "nightly-extra");
        write_job(&repo, Scope::User, "escapes", "../nightly");

        let jobs = crate::jobs::load(&repo).unwrap();
        let folder = repo.routines_dir().join("nightly");
        let matched: Vec<(&str, Scope)> = jobs_into(&repo, &jobs, &folder)
            .into_iter()
            .map(|job| (job.name.as_str(), job.scope))
            .collect();
        assert_eq!(
            matched,
            vec![("whole", Scope::User), ("single", Scope::Project)]
        );
    }

    /// A store broken between `x` and `enter` refuses the delete with
    /// nothing removed: the listed job is still in its own store, the folder
    /// is still there, and the popup text sends the person to `spoolway
    /// doctor`.
    #[test]
    fn a_store_broken_after_x_refuses_with_nothing_removed() {
        use crate::jobs::Scope;
        let (repo, _root_guard) = fixture("routine-delete-store-broken");
        seed_two_routines(&repo);
        write_job(&repo, Scope::User, "whole", "nightly");
        let folder = repo.routines_dir().join("nightly");
        let target = RoutineDelete {
            name: "nightly".to_string(),
            path: folder.clone(),
            tasks: 2,
            jobs: vec![("whole".to_string(), Scope::User)],
        };
        // Broken after `x` read the stores, the way a hand edit between the
        // popup and `enter` would break it.
        std::fs::create_dir_all(repo.jobs_file().parent().unwrap()).unwrap();
        std::fs::write(repo.jobs_file(), "[jobs.single\n").unwrap();

        let text = delete_routine(&repo, &target).unwrap_err();

        assert!(folder.is_dir(), "the folder is kept");
        assert!(
            std::fs::read_to_string(repo.user_jobs_file())
                .unwrap()
                .contains("[jobs.whole]"),
            "the job is kept"
        );
        assert!(text.contains("spoolway doctor"), "{text}");
        assert!(text.contains("jobs.toml"), "{text}");
        let (removed, left) = text.split_once("left:").unwrap();
        assert!(removed.contains("removed:\n  nothing"), "{text}");
        assert!(left.contains("job whole (user)"), "{text}");
        assert!(left.contains("folder .spoolway/routines/nightly"), "{text}");
    }

    /// The jobs go first and the folder last. A job that will not delete
    /// stops everything after it: the job before it is really gone from its
    /// store, the failed one and the folder are still there, and the popup
    /// text says exactly that.
    #[test]
    fn a_job_that_will_not_delete_leaves_the_routine_folder() {
        use crate::jobs::Scope;
        use std::os::unix::fs::PermissionsExt;
        let (repo, _root_guard) = fixture("routine-delete-order-jobs");
        seed_two_routines(&repo);
        write_job(&repo, Scope::Project, "single", "nightly/audit-docs.md");
        write_job(&repo, Scope::User, "whole", "nightly");
        let folder = repo.routines_dir().join("nightly");
        let target = RoutineDelete {
            name: "nightly".to_string(),
            path: folder.clone(),
            tasks: 2,
            jobs: vec![
                ("single".to_string(), Scope::Project),
                ("whole".to_string(), Scope::User),
            ],
        };
        // The user store still reads, but its directory takes no write, so
        // `whole` alone fails to delete.
        let home = repo.user_jobs_file().parent().unwrap().to_path_buf();
        let mut perms = std::fs::metadata(&home).unwrap().permissions();
        perms.set_mode(0o500); // read + execute, no write
        std::fs::set_permissions(&home, perms.clone()).unwrap();

        let result = delete_routine(&repo, &target);

        // Restore before asserting, so a failed assertion does not leave a
        // directory this test's own cleanup cannot remove.
        perms.set_mode(0o700);
        std::fs::set_permissions(&home, perms).unwrap();
        let text = result.unwrap_err();
        assert!(folder.is_dir(), "the folder waits on its jobs");
        assert!(
            !std::fs::read_to_string(repo.jobs_file())
                .unwrap()
                .contains("[jobs.single]"),
            "the job before the failure is gone"
        );
        assert!(
            std::fs::read_to_string(repo.user_jobs_file())
                .unwrap()
                .contains("[jobs.whole]"),
            "the failed job is still there"
        );
        let (removed, left) = text.split_once("left:").unwrap();
        assert!(removed.contains("job single (project)"), "{text}");
        assert!(!removed.contains("job whole"), "{text}");
        assert!(left.contains("job whole (user)"), "{text}");
        assert!(left.contains("folder .spoolway/routines/nightly"), "{text}");
    }

    /// A folder that will not remove is reached only after its jobs are
    /// gone, and the popup text says the jobs went and the folder stayed.
    #[test]
    fn a_folder_that_will_not_remove_is_tried_after_its_jobs() {
        use crate::jobs::Scope;
        use std::os::unix::fs::PermissionsExt;
        let (repo, _root_guard) = fixture("routine-delete-order-folder");
        seed_two_routines(&repo);
        write_job(&repo, Scope::User, "whole", "nightly");
        write_job(&repo, Scope::Project, "single", "nightly/audit-docs.md");
        let folder = repo.routines_dir().join("nightly");
        let target = RoutineDelete {
            name: "nightly".to_string(),
            path: folder.clone(),
            tasks: 2,
            jobs: vec![
                ("whole".to_string(), Scope::User),
                ("single".to_string(), Scope::Project),
            ],
        };
        let routines_dir = repo.routines_dir();
        let mut perms = std::fs::metadata(&routines_dir).unwrap().permissions();
        perms.set_mode(0o500); // read + execute, no write
        std::fs::set_permissions(&routines_dir, perms.clone()).unwrap();

        let result = delete_routine(&repo, &target);

        // Restore before asserting, so a failed assertion does not leave a
        // directory this test's own cleanup cannot remove.
        perms.set_mode(0o700);
        std::fs::set_permissions(&routines_dir, perms).unwrap();
        let text = result.unwrap_err();
        assert!(job_names(&repo).is_empty(), "both jobs went first");
        assert!(folder.is_dir());
        let (removed, left) = text.split_once("left:").unwrap();
        assert!(removed.contains("job whole (user)"), "{text}");
        assert!(removed.contains("job single (project)"), "{text}");
        assert!(left.contains("folder .spoolway/routines/nightly"), "{text}");
    }

    /// `x` opens the popup the mockup draws: the routine, its task count,
    /// and every job that goes with it beside its store — with the popup's
    /// own keys on the line under the frame.
    #[test]
    fn x_names_the_routine_and_every_job_pointing_into_it() {
        use crate::jobs::Scope;
        let (repo, _root_guard) = fixture("routine-delete-popup");
        seed_two_routines(&repo);
        write_job(&repo, Scope::User, "nightly-audit", "nightly");
        write_job(&repo, Scope::Project, "audit-docs", "nightly/audit-docs.md");
        write_job(&repo, Scope::User, "weekly-prune", "maintenance");

        let drawn = routines_screen(&repo, "jx");
        let last = last_frame(&drawn);

        assert!(last.contains("─ delete this routine "), "{last}");
        assert!(last.contains("nightly · 2 tasks"), "{last}");
        assert!(last.contains("its folder is removed from disk,"), "{last}");
        assert!(last.contains("and only git can bring it back."), "{last}");
        assert!(last.contains("these jobs are deleted with it:"), "{last}");
        assert!(last.contains("nightly-audit   user"), "{last}");
        assert!(last.contains("audit-docs      project"), "{last}");
        assert!(!last.contains("weekly-prune"), "{last}");
        assert_eq!(
            last.matches("[enter] delete   [esc] keep").count(),
            2,
            "in the popup and on the line under the frame: {last}"
        );
    }

    /// With no job pointing in, the popup leaves the jobs lines out.
    #[test]
    fn x_on_a_routine_no_job_points_into_leaves_the_jobs_lines_out() {
        let (repo, _root_guard) = fixture("routine-delete-no-jobs");
        seed_two_routines(&repo);

        let last = last_frame(&routines_screen(&repo, "x")).to_string();

        assert!(last.contains("maintenance · 1 task"), "{last}");
        assert!(!last.contains("these jobs are deleted with it:"), "{last}");
    }

    /// `esc` keeps everything, and every other key leaves the popup open.
    #[test]
    fn x_then_esc_keeps_the_routine_and_its_jobs() {
        use crate::jobs::Scope;
        let (repo, _root_guard) = fixture("routine-delete-esc");
        seed_two_routines(&repo);
        write_job(&repo, Scope::User, "nightly-audit", "nightly");

        let held = last_frame(&routines_screen(&repo, "jxynq ")).to_string();
        assert!(held.contains("─ delete this routine "), "{held}");
        assert!(repo.routines_dir().join("nightly").is_dir());

        let last = last_frame(&routines_screen(&repo, "jx\x1b")).to_string();
        assert!(!last.contains("delete this routine"), "{last}");
        assert!(last.contains("routines  2 of 2"), "{last}");
        assert!(repo.routines_dir().join("nightly").is_dir());
        assert_eq!(job_names(&repo), vec!["nightly-audit"]);
    }

    /// `enter` deletes the jobs and the folder, reads the list again, keeps
    /// the cursor on a row that is still there and drops the routine from
    /// the ticked set — and stages nothing.
    #[test]
    fn x_then_enter_deletes_the_routine_and_its_jobs() {
        use crate::jobs::Scope;
        let (repo, _root_guard) = fixture("routine-delete-enter");
        seed_two_routines(&repo);
        write_job(&repo, Scope::User, "nightly-audit", "nightly");
        write_job(&repo, Scope::Project, "audit-docs", "nightly/audit-docs.md");
        write_job(&repo, Scope::User, "weekly-prune", "maintenance");

        let last = last_frame(&routines_screen(&repo, "j x\r")).to_string();

        assert!(!repo.routines_dir().join("nightly").exists());
        assert_eq!(job_names(&repo), vec!["weekly-prune"]);
        assert!(last.contains("routines  1 of 1"), "{last}");
        assert!(last.contains("> [ ] maintenance"), "{last}");
        assert!(!last.contains("nightly"), "{last}");
        assert!(!last.contains("delete this routine"), "{last}");
        let staged =
            crate::repo::run(&repo.root, "git", &["diff", "--cached", "--name-only"]).unwrap();
        assert!(staged.trim().is_empty(), "nothing staged: {staged}");
    }

    /// A job store that will not parse refuses the delete before anything
    /// is removed, naming the store and `spoolway doctor`.
    #[test]
    fn x_refuses_while_a_job_store_will_not_parse() {
        let (repo, _root_guard) = fixture("routine-delete-bad-store");
        seed_two_routines(&repo);
        std::fs::create_dir_all(repo.jobs_file().parent().unwrap()).unwrap();
        std::fs::write(repo.jobs_file(), "[jobs.broken\n").unwrap();

        let last = last_frame(&routines_screen(&repo, "jx\r")).to_string();
        assert!(repo.routines_dir().join("nightly").is_dir());

        let refused = last_frame(&routines_screen(&repo, "jx")).to_string();
        assert!(refused.contains("routine not deleted"), "{refused}");
        assert!(refused.contains("jobs.toml"), "{refused}");
        assert!(refused.contains("spoolway doctor"), "{refused}");
        assert!(
            !last.contains("routine not deleted"),
            "enter closes it: {last}"
        );
    }

    /// `x` reads only over the routine list: over the tasks pane it does
    /// nothing, and the key line there does not name it.
    #[test]
    fn x_over_the_tasks_pane_does_nothing_and_is_not_named() {
        let (repo, _root_guard) = fixture("routine-delete-tasks-pane");
        seed_two_routines(&repo);

        let last = last_frame(&routines_screen(&repo, "\tx")).to_string();

        assert!(!last.contains("delete this routine"), "{last}");
        assert!(!last.contains("[x] delete"), "{last}");
        assert!(last.contains("[o] open task"), "{last}");
    }

    /// `n` over the list opens the jobs tab's schedule popup, titled for
    /// the highlighted routine, with its keys on the line under the frame —
    /// and writes nothing yet.
    #[test]
    fn n_opens_the_schedule_popup_for_the_highlighted_routine() {
        let (repo, _root_guard) = fixture("routine-new-job-open");
        seed_two_routines(&repo);

        let last = last_frame(&routines_screen(&repo, "jn")).to_string();

        assert!(last.contains("─ when does nightly run "), "{last}");
        assert_eq!(
            last.matches("[enter] accept   [esc] cancel").count(),
            2,
            "in the popup and on the line under the frame: {last}"
        );
        assert!(job_names(&repo).is_empty());
    }

    /// `n` reads only over the routine list: over the tasks pane it opens
    /// nothing, and the key line there does not name it.
    #[test]
    fn n_over_the_tasks_pane_does_nothing_and_is_not_named() {
        let (repo, _root_guard) = fixture("routine-new-job-tasks-pane");
        seed_two_routines(&repo);

        let last = last_frame(&routines_screen(&repo, "\tn")).to_string();

        assert!(!last.contains("when does"), "{last}");
        assert!(!last.contains("[n] new job"), "{last}");
    }

    /// `esc` on either popup goes back to the routine list and writes
    /// nothing.
    #[test]
    fn esc_on_either_job_popup_writes_nothing() {
        let (repo, _root_guard) = fixture("routine-new-job-esc");
        seed_two_routines(&repo);

        for input in ["n0 3 * * *\x1b", "n0 3 * * *\r\x1b"] {
            let last = last_frame(&routines_screen(&repo, input)).to_string();
            assert!(!last.contains("when does"), "{input:?}: {last}");
            assert!(!last.contains("which pipeline"), "{input:?}: {last}");
            assert!(last.contains("routines  2 of 2"), "{input:?}: {last}");
            assert!(job_names(&repo).is_empty(), "{input:?}");
            assert!(!repo.user_jobs_file().exists(), "{input:?}");
        }
    }

    /// The walk the mockup draws: a schedule, then a pipeline found by
    /// typing, then `enter` — a job in the user store named after the
    /// routine, and the "job saved" popup over its own `[enter] confirm`
    /// line. `enter` closes it onto the routine list.
    #[test]
    fn n_walks_the_schedule_and_pipeline_and_saves_a_user_job() {
        use crate::jobs::Scope;
        let (repo, _root_guard) = fixture("routine-new-job-save");
        seed_two_routines(&repo);

        let saved = last_frame(&routines_screen(&repo, "n0 3 * * 1-5\rbug\r")).to_string();

        let jobs = crate::jobs::load(&repo).unwrap();
        assert_eq!(jobs.len(), 1);
        let job = &jobs[0];
        assert_eq!(job.name, "maintenance");
        assert_eq!(job.scope, Scope::User);
        assert_eq!(job.spec.routine, "maintenance");
        assert_eq!(job.spec.schedule, "0 3 * * 1-5");
        assert_eq!(job.spec.pipeline, "bugfix");
        assert!(job.spec.enabled);
        assert!(!repo.jobs_file().exists(), "nothing in the project store");

        assert!(saved.contains("─ job saved "), "{saved}");
        assert!(
            saved.contains("maintenance runs at 03:00, Monday to Friday, on bugfix."),
            "{saved}"
        );
        assert!(saved.contains("Next: "), "{saved}");
        assert!(
            saved.contains("Edit, pause or delete it on the jobs tab."),
            "{saved}"
        );
        // The key line under the frame is the popup's own `[enter]
        // confirm`, not the routine list's.
        assert!(
            saved.contains(&key_hint(&[("enter", "confirm")])),
            "{saved}"
        );
        assert!(!saved.contains("[n] new job"), "{saved}");

        std::fs::remove_file(repo.user_jobs_file()).unwrap();
        let back = last_frame(&routines_screen(&repo, "n0 3 * * 1-5\rbug\r\r")).to_string();
        assert!(!back.contains("job saved"), "{back}");
        assert!(back.contains("> [ ] maintenance"), "{back}");
        assert!(back.contains("[n] new job"), "{back}");
    }

    /// A routine whose job name is taken is refused at the save with the
    /// jobs tab's own message, and the job already there is left as it was.
    #[test]
    fn n_on_a_routine_whose_job_name_is_taken_is_refused() {
        use crate::jobs::Scope;
        let (repo, _root_guard) = fixture("routine-new-job-taken");
        seed_two_routines(&repo);
        write_job(&repo, Scope::Project, "nightly", "maintenance");

        let last = last_frame(&routines_screen(&repo, "jn0 4 * * *\r\r")).to_string();

        assert!(last.contains("─ not saved "), "{last}");
        assert!(
            last.contains("A job named `nightly` already exists."),
            "{last}"
        );
        let jobs = crate::jobs::load(&repo).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].spec.routine, "maintenance");
        assert_eq!(jobs[0].spec.schedule, "0 3 * * *");
        assert!(!repo.user_jobs_file().exists(), "nothing written");
    }

    /// `esc` over the routine list does nothing: the pane is the routines
    /// tab's whole screen, with no pending screen behind it to go back to.
    #[test]
    fn esc_over_the_routine_list_does_nothing() {
        let (repo, _root_guard) = fixture("routines-esc-back");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );

        let (exit, drawn) = routines_exit(&repo, " \x1b");
        assert_eq!(exit, ScreenExit::Quit);
        let last = last_frame(&drawn);

        assert!(last.contains("routines  1 of 1"), "{last}");
        assert!(last.contains("> [x] nightly"), "the tick kept: {last}");
        assert!(!last.contains("─ groups"), "{last}");
    }

    /// The queue tab reads nothing on `r`: routines have a tab of their own,
    /// so the pending screen stays exactly where it was.
    #[test]
    fn the_queue_tab_ignores_r() {
        let (repo, _root_guard) = fixture("routines-queue-ignores-r");
        write_pending(&repo, "wire", &task_text("wire", "group: one\n", BODY));
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );

        let drawn = screen(&repo, listed(&repo), "r");
        let last = last_frame(&drawn);

        assert!(last.contains("groups  1 of 1"), "{last}");
        assert!(!last.contains("routines  1 of 1"), "{last}");
        assert!(!last.contains("[r] routines"), "{last}");
    }

    /// Closing the popup a queued routine opens goes back to the routine
    /// list, not the pending screen, with nothing left ticked to queue the
    /// same routine twice.
    #[test]
    fn closing_the_queued_popup_goes_back_to_the_routine_list_unticked() {
        // Two fixtures, not one: a group is one chain now, and re-queuing
        // the same routine into a repo that already holds an open run of it
        // would be a second root in `nightly` — see
        // `a_group_with_two_roots_is_refused`. Each `routines_screen` call
        // below replays its own keys from a bare screen, so a shared repo
        // between them would be queuing the routine twice into one group,
        // which is not what either half of this test is about.
        let (repo, _root_guard) = fixture("routines-queued-close");
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );

        let under = last_frame(&routines_screen(&repo, " \r")).to_string();
        assert!(under.contains("queued 1 task"), "{under}");
        assert!(under.contains("routines  1 of 1"), "over the list: {under}");

        let (repo, _root_guard) = fixture("routines-queued-close-2");
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        let drawn = routines_screen(&repo, " \r\r");
        let last = last_frame(&drawn);
        assert!(last.contains("> [ ] nightly"), "{last}");
        assert!(!last.contains("queued 1 task"), "{last}");
        assert!(!last.contains("─ groups"), "{last}");
    }

    /// The one non-goal this feature draws a hard line at: the empty
    /// routines tab names `.spoolway/routines/` rather than opening onto a
    /// blank pane a person could mistake for a project with no keys to press.
    #[test]
    fn the_empty_routines_pane_names_its_own_path() {
        let (repo, _root_guard) = fixture("routines-empty");

        // Checked against `routine_folder_lines` directly, at a width wide
        // enough to hold the whole path: the left pane's own fixed 25
        // columns — the same width the mockup itself draws — would truncate
        // a scratch test repo's own long path well before this could tell
        // "named the path" apart from "named nothing at all".
        let (lines, _) = routine_folder_lines(&repo.routines_dir(), &[], &RoutineNav::new(), 200);
        assert_eq!(
            lines,
            vec![format!("  nothing under {}", repo.routines_dir().display())]
        );
    }

    /// `enter` on a selected folder queues every task under it through
    /// `validate_batch`, under minted ids rather than the bare ones the
    /// tasks themselves carry, with `group:` and the body untouched —
    /// and the source files under `.spoolway/routines/` left exactly where
    /// they were.
    #[test]
    fn enter_on_a_selected_folder_queues_every_task_under_minted_ids() {
        let (repo, _root_guard) = fixture("routines-enter-mints");
        let deps = write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        // `audit-docs` depends on `audit-deps`: a group is one chain, and a
        // folder's tasks sharing one group have to say how they order —
        // `mint_routine_batch` remaps the id either way, which is the one
        // thing this test is actually about.
        let docs = write_routine(
            &repo,
            "nightly",
            "audit-docs",
            &task_text(
                "audit-docs",
                "group: nightly\ndepends_on: [audit-deps]\n",
                BODY,
            ),
        );
        let deps_text = std::fs::read_to_string(&deps).unwrap();
        let docs_text = std::fs::read_to_string(&docs).unwrap();

        // space (select `nightly`), enter (queue it).
        let (exit, _) = routines_exit(&repo, " \r");
        assert_eq!(exit, ScreenExit::Quit);

        assert!(
            !repo.queue_dir().join("audit-deps.md").exists(),
            "the bare id is never used — a routine is meant to be queued again"
        );
        let queued: Vec<String> = std::fs::read_dir(repo.queue_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(queued.len(), 2, "{queued:?}");
        assert!(
            queued.contains(&"audit-deps-1.md".to_string()),
            "{queued:?}"
        );
        assert!(
            queued.contains(&"audit-docs-1.md".to_string()),
            "{queued:?}"
        );

        let queued_deps =
            std::fs::read_to_string(repo.queue_dir().join("audit-deps-1.md")).unwrap();
        assert!(queued_deps.contains("group: nightly"), "{queued_deps}");
        assert!(queued_deps.contains(BODY), "{queued_deps}");

        // The routines directory itself is untouched.
        assert_eq!(std::fs::read_to_string(&deps).unwrap(), deps_text);
        assert_eq!(std::fs::read_to_string(&docs).unwrap(), docs_text);
    }

    /// A chain saved together still resolves once every id in it has been
    /// minted: `depends_on` is rewritten to the new id, even though nothing
    /// else in the task is.
    #[test]
    fn enter_remaps_depends_on_between_siblings_in_the_same_folder() {
        let (repo, _root_guard) = fixture("routines-enter-remaps-depends-on");
        write_routine(
            &repo,
            "chain",
            "split-fields",
            &task_text("split-fields", "group: chain\n", BODY),
        );
        write_routine(
            &repo,
            "chain",
            "scan-pending",
            &task_text(
                "scan-pending",
                "group: chain\ndepends_on: [split-fields]\n",
                BODY,
            ),
        );

        let (exit, _) = routines_exit(&repo, " \r");
        assert_eq!(exit, ScreenExit::Quit);

        let scan = queued(&repo, "scan-pending-1");
        assert_eq!(scan.front.depends_on, vec!["split-fields-1".to_string()]);
    }

    /// `space` over a single task in the routines pane's own tasks list
    /// queues that task alone, with its `depends_on` emptied before
    /// `validate_batch` ever sees it — so a task naming a sibling nobody
    /// queued is not refused for a dependency this solo pick dropped.
    #[test]
    fn space_on_a_solo_task_queues_it_alone_with_depends_on_emptied() {
        let (repo, _root_guard) = fixture("routines-solo-space");
        write_routine(
            &repo,
            "chain",
            "split-fields",
            &task_text("split-fields", "group: chain\n", BODY),
        );
        write_routine(
            &repo,
            "chain",
            "scan-pending",
            &task_text(
                "scan-pending",
                "group: chain\ndepends_on: [split-fields]\n",
                BODY,
            ),
        );

        // tab (focus onto its tasks, already on `scan-pending` — the first
        // one in filename order), space (queue it alone).
        let (exit, drawn) = routines_exit(&repo, "\t ");
        assert_eq!(exit, ScreenExit::Quit);
        let last = last_frame(&drawn);

        let queued_files: Vec<String> = std::fs::read_dir(repo.queue_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            queued_files,
            vec!["scan-pending-1.md".to_string()],
            "{queued_files:?} — {last}"
        );
        let solo = queued(&repo, "scan-pending-1");
        assert!(
            solo.front.depends_on.is_empty(),
            "{:?}",
            solo.front.depends_on
        );
    }

    /// `s` on a highlighted pending group opens the save panel, and `enter`
    /// there copies its tasks into `.spoolway/routines/<name>/`
    /// unchanged, same ids.
    #[test]
    fn s_saves_a_pending_group_into_routines_unchanged() {
        let (repo, _root_guard) = fixture("routines-save");
        write_pending(
            &repo,
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "s");
        let panel = last_frame(&drawn);
        assert!(panel.contains("save nightly as a routine"), "{panel}");
        assert!(
            panel.contains(".spoolway/routines/nightly_"),
            "the mockup draws this repo-relative, not the scratch fixture's \
             own absolute path:\n{panel}"
        );
        assert!(
            panel.contains("copies 1 task unchanged, same ids"),
            "{panel}"
        );

        let (repo2, _root_guard2) = fixture("routines-save-2");
        write_pending(
            &repo2,
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        let groups2 = listed(&repo2);
        let drawn2 = screen(&repo2, groups2, "s\r");
        let last2 = last_frame(&drawn2);
        assert!(last2.contains("saved 1 task"), "{last2}");

        let saved = repo2.routines_dir().join("nightly").join("audit-deps.md");
        let original = std::fs::read_to_string(repo2.pending_dir().join("audit-deps.md")).unwrap();
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), original);
    }

    /// A review round caught this: `q` used to quit the whole screen while
    /// naming a routine, the same way it quit from browsing — so a name like
    /// `quarterly` could never be typed. No mode reads `q` as a quit key any
    /// more, but the save panel's own reason for taking it as an ordinary
    /// character stands regardless — the same way `Mode::Filter`'s query
    /// does — and only `esc` backs out of it.
    #[test]
    fn q_is_a_letter_in_the_save_name() {
        let (repo, _root_guard) = fixture("routines-save-q-is-a-letter");
        write_pending(
            &repo,
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "sq");
        let last = last_frame(&drawn);

        assert!(
            last.contains(".spoolway/routines/nightlyq_"),
            "`q` must be typed into the name:\n{last}"
        );
    }

    /// The name is typed, so it can be typed as a path — and `join` would
    /// happily follow a `..` straight out of `.spoolway/routines/` and
    /// write a routine somewhere the routines tab never reads back. One plain
    /// folder name is all `s` accepts.
    #[test]
    fn s_refuses_a_name_that_is_not_a_plain_folder_name() {
        let (repo, _root_guard) = fixture("routines-save-refuses-a-path");
        write_pending(
            &repo,
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        let groups = listed(&repo);

        // Seven backspaces clear the prefilled `nightly`, and what is typed
        // over it climbs out of the routines directory entirely.
        let drawn = screen(
            &repo,
            groups,
            "s\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}\u{8}../escaped\r",
        );
        let last = last_frame(&drawn);
        assert!(last.contains("is not a folder name"), "{last}");
        assert!(
            !repo
                .routines_dir()
                .parent()
                .unwrap()
                .join("escaped")
                .exists(),
            "nothing was written outside the routines directory"
        );
    }

    /// The non-goal this feature draws a hard line at: `s` refuses to save
    /// into a folder that already holds tasks, rather than merging into
    /// it.
    #[test]
    fn s_refuses_a_folder_that_already_holds_tasks() {
        let (repo, _root_guard) = fixture("routines-save-refuses-merge");
        write_pending(
            &repo,
            "audit-deps",
            &task_text("audit-deps", "group: nightly\n", BODY),
        );
        let groups = listed(&repo);
        write_routine(
            &repo,
            "nightly",
            "already-here",
            &task_text("already-here", "group: nightly\n", BODY),
        );

        let drawn = screen(&repo, groups, "s\r");
        let last = last_frame(&drawn);
        assert!(last.contains("already holds tasks"), "{last}");
        assert!(
            !repo
                .routines_dir()
                .join("nightly")
                .join("audit-deps.md")
                .exists(),
            "nothing was written over the folder that was already there"
        );
    }

    // The `[issue_tracking]` open hook: `queue add` runs it once per task
    // before anything is queued, writes `epic:`/`ticket:` into the result,
    // and refuses the whole batch — while saving what already succeeded —
    // the moment one call fails. A hook run is `sh -c` under `libc::setsid()`
    // under the hood, the same Unix-only path `command_step` and `tracking`
    // themselves are — see their own test modules for why.
    mod open_hook {
        use super::*;

        /// Points `[issue_tracking].hook` at an executable script under
        /// `.spoolway/hooks/`, the only place a hook may ever be named from.
        fn with_hook(repo: &mut Repo, script: &str) {
            let dir = repo.checkout.join(".spoolway/hooks");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("open.sh");
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            repo.config.issue_tracking.hook = "open.sh".to_string();
        }

        /// A project that has never touched `[issue_tracking]` pays for none
        /// of this: no hook call, no `epic:`/`ticket:`, no `tracking/`
        /// directory at all.
        #[test]
        fn no_hook_configured_is_a_no_op() {
            let (repo, _root_guard) = fixture("open-no-hook");
            let text = task_text("login", "group: demo\n", BODY);
            let path = write_doc(&repo, "login.md", &text);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap();

            let task = queued(&repo, "login");
            assert_eq!(task.extra_str("epic"), "");
            assert_eq!(task.extra_str("ticket"), "");
            // `tracking_dir` creates itself the moment anything asks for
            // it, same as every other directory under `Repo::home` — so
            // "no hook ran" is proven by it holding nothing, not by its own
            // absence.
            assert!(
                std::fs::read_dir(repo.tracking_dir())
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true)
            );
        }

        /// The mockup this covers verbatim: a hook is configured, and the
        /// group being submitted sets `group_description:` on none of its
        /// tasks — refused, naming the group and the hook, before
        /// anything is queued or the hook is ever run.
        #[test]
        fn a_group_with_no_description_is_refused_once_a_hook_is_configured() {
            let (mut repo, _root_guard) = fixture("open-no-description");
            with_hook(&mut repo, "exit 1");
            let text = task_text("mirrored", "group: issue-mirror\n", BODY);
            let path = write_doc(&repo, "mirrored.md", &text);

            let err = queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap_err();
            let err = err.to_string();
            assert!(err.contains("group `issue-mirror`"), "{err}");
            assert!(err.contains("group_description"), "{err}");
            assert!(err.contains("open.sh"), "{err}");
            assert!(
                !repo.queue_dir().join("mirrored.md").exists(),
                "nothing was queued"
            );
            assert!(
                std::fs::read_dir(repo.tracking_dir())
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
                "the hook was never run — the refusal is ahead of it"
            );
        }

        /// A project with no hook configured pays for none of this: the same
        /// group with no `group_description:` on any task queues cleanly.
        #[test]
        fn a_group_with_no_description_is_fine_with_no_hook_configured() {
            let (repo, _root_guard) = fixture("open-no-description-no-hook");
            let text = task_text("mirrored", "group: issue-mirror\n", BODY);
            let path = write_doc(&repo, "mirrored.md", &text);

            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap();

            assert!(repo.queue_dir().join("mirrored.md").exists());
        }

        /// `gather_tasks` names a `--from -` stream entry `<stdin>#N`,
        /// never a path — [`readable_task_files`] must not hand that name to
        /// the hook as `SPOOLWAY_TASK_FILE` just because it looks like one:
        /// the empty string, not a name nothing can open.
        #[test]
        fn a_task_with_no_backing_file_gets_an_empty_task_file() {
            let (mut repo, _root_guard) = fixture("open-no-backing-file");
            with_hook(
                &mut repo,
                r#"printf '%s' "$SPOOLWAY_TASK_FILE" >"$(dirname "$SPOOLWAY_OUT")/task-file.seen"
                   echo "ticket=T-1" >"$SPOOLWAY_OUT""#,
            );
            let doc = task_text(
                "streamed",
                "group: streamed\ngroup_description: read from stdin\n",
                BODY,
            );
            let submitted = vec![("<stdin>#1".to_string(), doc)];
            let mut tasks =
                validate_batch(&repo, &Pipelines::builtin(), Some("plan/demo"), &submitted)
                    .unwrap();
            let task_files = readable_task_files(&submitted);
            let gate = ToolGate::Print { interactive: false };
            open_and_prefix(
                &repo,
                &submitted,
                &task_files,
                &mut tasks,
                gate,
                &mut PrintedTickets,
            )
            .unwrap();

            let seen = std::fs::read_to_string(repo.tracking_dir().join("task-file.seen"));
            assert_eq!(
                seen.unwrap(),
                "",
                "`<stdin>#1` is not a path — the hook must see nothing rather than a name it \
                 cannot open"
            );
        }

        /// The hook runs once per task, in dependency order, and its
        /// `epic=`/`ticket=` answer — read from the file at `SPOOLWAY_OUT` —
        /// lands in the queued task's own frontmatter. The dependent's
        /// call also gets its parent's ticket id in
        /// `SPOOLWAY_DEPENDS_TICKETS`.
        #[test]
        fn the_hook_runs_per_task_in_dependency_order_and_writes_both_ids() {
            let (mut repo, _root_guard) = fixture("open-runs");
            with_hook(
                &mut repo,
                r#"echo "$SPOOLWAY_TASK $SPOOLWAY_DEPENDS_TICKETS" >>"$(dirname "$SPOOLWAY_OUT")/order.log"
                   { echo "epic=acme/app#42"; \
                     echo "ticket=acme/app#$(( 100 + $(wc -l <"$(dirname "$SPOOLWAY_OUT")/order.log") ))"; \
                   } >"$SPOOLWAY_OUT""#,
            );

            let parent = task_text(
                "scan-pending",
                "group: scanner-rework\ngroup_description: scanning rework\n",
                BODY,
            );
            let parent_path = write_doc(&repo, "scan-pending.md", &parent);
            let child = task_text(
                "split-fields",
                "group: scanner-rework\ndepends_on: [scan-pending]\n",
                BODY,
            );
            let child_path = write_doc(&repo, "split-fields.md", &child);

            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&parent_path, &child_path]),
                &repo.root,
                false,
            )
            .unwrap();

            let parent_task = queued(&repo, "scan-pending");
            let child_task = queued(&repo, "split-fields");
            assert_eq!(parent_task.extra_str("epic"), "acme/app#42");
            assert!(!parent_task.extra_str("ticket").is_empty());
            assert_eq!(child_task.extra_str("epic"), "acme/app#42");
            assert!(!child_task.extra_str("ticket").is_empty());

            let order = std::fs::read_to_string(repo.tracking_dir().join("order.log")).unwrap();
            let lines: Vec<&str> = order.lines().collect();
            assert_eq!(
                lines[0], "scan-pending ",
                "the parent has no tickets to wait on yet"
            );
            assert_eq!(
                lines[1],
                format!("split-fields {}", parent_task.extra_str("ticket"))
            );
        }

        /// A task that already names a `ticket:` is reported `kept` and
        /// never reaches the hook at all — proven here by a hook that fails
        /// the moment it is ever invoked.
        #[test]
        fn a_task_already_naming_a_ticket_skips_the_hook() {
            let (mut repo, _root_guard) = fixture("open-kept");
            with_hook(&mut repo, "exit 1");
            let text = task_text(
                "retry-drops",
                "group: scanner-rework\ngroup_description: scanning rework\n\
                 epic: acme/app#42\nticket: acme/app#45\n",
                BODY,
            );
            let path = write_doc(&repo, "retry-drops.md", &text);

            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap();

            let task = queued(&repo, "retry-drops");
            assert_eq!(task.extra_str("ticket"), "acme/app#45");
        }

        /// A hook that fails partway through a batch queues nothing at all —
        /// but every id already answered is written back into the pending
        /// task it came from, so a second run of the same command sees
        /// it already there and resumes rather than opening a second set.
        #[test]
        fn a_failing_hook_queues_nothing_and_resumes_on_the_next_run() {
            let (mut repo, _root_guard) = fixture("open-fails-midbatch");
            with_hook(
                &mut repo,
                r#"if [ "$SPOOLWAY_TASK" = "split-fields" ]; then exit 1; fi
                   { echo "epic=acme/app#42"; echo "ticket=acme/app#43"; } >"$SPOOLWAY_OUT""#,
            );

            let first = task_text(
                "scan-pending",
                "group: scanner-rework\ngroup_description: scanning rework\n",
                BODY,
            );
            let first_path = write_doc(&repo, "scan-pending.md", &first);
            let second = task_text(
                "split-fields",
                "group: scanner-rework\ndepends_on: [scan-pending]\n",
                BODY,
            );
            let second_path = write_doc(&repo, "split-fields.md", &second);

            let err = queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&first_path, &second_path]),
                &repo.root,
                false,
            )
            .unwrap_err();
            let err = err.to_string();
            assert!(err.contains("split-fields"), "{err}");
            // The ids already opened before the failure are named, not just
            // gestured at — a person reading this does not have to go dig
            // through the pending file to know what already exists.
            assert!(err.contains("acme/app#42"), "{err}");
            assert!(err.contains("acme/app#43"), "{err}");
            assert!(
                err.contains("ids      written into pending/"),
                "the ids row must say where they landed: {err}"
            );
            assert!(
                err.contains("log      tracking/split-fields · open.log"),
                "{err}"
            );
            assert!(
                !repo.queue_dir().join("scan-pending.md").exists(),
                "nothing was queued, including the task that succeeded"
            );

            // The succeeded task's own id was written back into it, in
            // place — read straight off the file `--from` still names.
            let rewritten = std::fs::read_to_string(&first_path).unwrap();
            assert!(rewritten.contains("ticket: acme/app#43"));
            assert!(rewritten.contains("epic: acme/app#42"));

            // A second run over the same two tasks: the first is now
            // `kept`, the hook only runs for the one that never got an
            // answer, and this time it succeeds because the hook no longer
            // refuses `split-fields`.
            with_hook(
                &mut repo,
                r#"{ echo "epic=$SPOOLWAY_EPIC"; echo "ticket=acme/app#44"; } >"$SPOOLWAY_OUT""#,
            );
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&first_path, &second_path]),
                &repo.root,
                false,
            )
            .unwrap();
            assert_eq!(
                queued(&repo, "scan-pending").extra_str("ticket"),
                "acme/app#43"
            );
            assert_eq!(
                queued(&repo, "split-fields").extra_str("ticket"),
                "acme/app#44"
            );
            assert_eq!(
                queued(&repo, "split-fields").extra_str("epic"),
                "acme/app#42",
                "the group's epic came from the sibling this batch already found `kept`, \
                 not a second hook decision"
            );
        }

        /// The one case above never reaches: the very first call in the
        /// batch is the one that fails, so nothing has been opened yet at
        /// all. The message carries no `opened` or `ids` row at all when
        /// there is nothing for either to name — only `log`, naming the one
        /// file that holds the hook's own stderr rather than the directory
        /// it lives under.
        #[test]
        fn a_hook_failing_on_the_first_call_says_so_with_nothing_to_resume_from() {
            let (mut repo, _root_guard) = fixture("open-fails-first-call");
            with_hook(&mut repo, "exit 3");
            let text = task_text(
                "opens-first",
                "group: solo\ngroup_description: opens first\n",
                BODY,
            );
            let path = write_doc(&repo, "opens-first.md", &text);

            let err = queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap_err();
            let err = err.to_string();

            assert!(err.contains("opens-first"), "{err}");
            assert!(!err.contains("opened"), "{err}");
            assert!(!err.contains("ids"), "{err}");
            assert!(
                err.contains("log      tracking/opens-first · open.log"),
                "{err}"
            );
            assert!(
                !repo.queue_dir().join("opens-first.md").exists(),
                "nothing was queued"
            );
            // Nothing to write back either — the pending task is
            // untouched, since the hook never answered with anything.
            let untouched = std::fs::read_to_string(&path).unwrap();
            assert_eq!(untouched, text);
        }

        /// A slug a successful call secured is written back into its pending
        /// task when a later call in the batch fails, so the re-run reads
        /// it as `slug:` and pins the group's prefix — a hook answering a
        /// different slug on the re-run cannot displace the first one.
        #[test]
        fn a_secured_slug_survives_a_failed_batch_and_pins_the_re_run() {
            let (mut repo, _root_guard) = fixture("open-slug-writeback");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"if [ "$SPOOLWAY_TASK" = "auth-02" ]; then exit 1; fi
                   { echo "ticket=PROJ-13"; echo "slug=proj-12"; } >"$SPOOLWAY_OUT""#,
            );

            let a = task_text(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\n",
                BODY,
            );
            let a_path = write_doc(&repo, "auth-01.md", &a);
            let b = task_text(
                "auth-02",
                "group: auth-rework\ndepends_on: [auth-01]\n",
                BODY,
            );
            let b_path = write_doc(&repo, "auth-02.md", &b);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&a_path, &b_path]),
                &repo.root,
                false,
            )
            .unwrap_err();

            // `auth-01`'s slug landed in its pending task, in place.
            assert!(
                std::fs::read_to_string(&a_path)
                    .unwrap()
                    .contains("slug: proj-12"),
                "the secured slug was not written back"
            );

            // The re-run: the hook now answers a different slug for the task
            // that had not been reached, but `auth-01`'s written-back `slug:`
            // is what wins.
            with_hook(
                &mut repo,
                r#"{ echo "ticket=PROJ-14"; echo "slug=other-99"; } >"$SPOOLWAY_OUT""#,
            );
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&a_path, &b_path]),
                &repo.root,
                false,
            )
            .unwrap();

            for id in ["auth-01", "auth-02"] {
                assert_eq!(
                    queued(&repo, id).front.group.as_deref(),
                    Some("proj-12-auth-rework"),
                    "{id} took the wrong prefix"
                );
            }
        }

        /// The winner is the first valid answer in *dependency* order, not
        /// task order — and it stays the winner across a mid-batch
        /// failure even when the tasks were submitted back to front.
        /// Shows `[c, b, a]`, chained `a <- b <- c`: the hook answers a
        /// different valid slug for `a` and `b`, then fails for `c`. `a`'s
        /// slug is what every task of the group carries afterwards.
        #[test]
        fn the_dependency_order_winner_survives_reversed_task_order() {
            let (mut repo, _root_guard) = fixture("open-slug-dep-order");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"case "$SPOOLWAY_TASK" in
                     chain-a) slug=aa-1 ;;
                     chain-b) slug=bb-2 ;;
                     *) exit 1 ;;
                   esac
                   { echo "ticket=t-$SPOOLWAY_TASK"; echo "slug=$slug"; } >"$SPOOLWAY_OUT""#,
            );

            let a = task_text(
                "chain-a",
                "group: chain\ngroup_description: chained work\n",
                BODY,
            );
            let a_path = write_doc(&repo, "chain-a.md", &a);
            let b = task_text("chain-b", "group: chain\ndepends_on: [chain-a]\n", BODY);
            let b_path = write_doc(&repo, "chain-b.md", &b);
            let c = task_text("chain-c", "group: chain\ndepends_on: [chain-b]\n", BODY);
            let c_path = write_doc(&repo, "chain-c.md", &c);

            // Submitted back to front.
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&c_path, &b_path, &a_path]),
                &repo.root,
                false,
            )
            .unwrap_err();

            for path in [&a_path, &b_path] {
                assert!(
                    std::fs::read_to_string(path)
                        .unwrap()
                        .contains("slug: aa-1"),
                    "{path} did not carry the dependency-order winner"
                );
            }

            // The re-run queues the lot; every task is prefixed `aa-1`.
            with_hook(
                &mut repo,
                r#"{ echo "ticket=t-$SPOOLWAY_TASK"; echo "slug=late-9"; } >"$SPOOLWAY_OUT""#,
            );
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&c_path, &b_path, &a_path]),
                &repo.root,
                false,
            )
            .unwrap();
            for id in ["chain-a", "chain-b", "chain-c"] {
                assert_eq!(
                    queued(&repo, id).front.group.as_deref(),
                    Some("aa-1-chain"),
                    "{id} took the wrong prefix"
                );
            }
        }

        /// End to end, through the real binary path with a real hook script:
        /// with `issue_tracking.key_in_names` on and the hook answering a
        /// `slug=`, `queue add` writes `group: <slug>-<group>` and `branch:
        /// task/<slug>-<id>`, stores the `slug:` and the `url:`, and the
        /// prefixed branch still loads back cleanly through `src/task.rs`'s
        /// own invariant.
        #[test]
        fn key_in_names_prefixes_the_group_the_branch_and_stores_the_url() {
            let (mut repo, _root_guard) = fixture("open-prefix");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"{ echo "epic=PROJ-12"; echo "ticket=PROJ-13"
                     echo "slug=proj-12"
                     echo "url=https://acme.atlassian.net/browse/PROJ-12"; } >"$SPOOLWAY_OUT""#,
            );

            let parent = task_text(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\n",
                BODY,
            );
            let parent_path = write_doc(&repo, "auth-01.md", &parent);
            let child = task_text(
                "auth-02",
                "group: auth-rework\ndepends_on: [auth-01]\n",
                BODY,
            );
            let child_path = write_doc(&repo, "auth-02.md", &child);

            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&parent_path, &child_path]),
                &repo.root,
                false,
            )
            .unwrap();

            for id in ["auth-01", "auth-02"] {
                let task = queued(&repo, id);
                assert_eq!(
                    task.front.group.as_deref(),
                    Some("proj-12-auth-rework"),
                    "{id}"
                );
                assert_eq!(
                    task.front.branch.as_deref(),
                    Some(format!("task/proj-12-{id}").as_str()),
                    "{id}"
                );
                assert_eq!(task.extra_str("slug"), "proj-12", "{id}");
            }
            assert_eq!(
                queued(&repo, "auth-01").extra_str("url"),
                "https://acme.atlassian.net/browse/PROJ-12"
            );
        }

        /// A task that has already been through the queue once —
        /// unqueued and re-submitted — carries its `group:` already
        /// prefixed with the slug, and its own `slug:` alongside it (see
        /// `carry_to_pending`, which drops `branch:` but keeps both). Queued
        /// a second time, the group must come out with exactly one `proj-12-`
        /// on it, not two.
        #[test]
        fn a_task_whose_group_already_carries_the_slug_is_not_prefixed_twice() {
            let (mut repo, _root_guard) = fixture("open-prefix-twice");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(&mut repo, "exit 1");

            let text = task_text(
                "auth-01",
                "group: proj-12-auth-rework\ngroup_description: auth rework\n\
                 slug: proj-12\nticket: PROJ-13\n",
                BODY,
            );
            let path = write_doc(&repo, "auth-01.md", &text);

            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap();

            let task = queued(&repo, "auth-01");
            assert_eq!(
                task.front.group.as_deref(),
                Some("proj-12-auth-rework"),
                "the slug was stamped on again instead of recognised"
            );
            assert_eq!(
                task.front.branch.as_deref(),
                Some("task/proj-12-auth-01"),
                "the branch still names the id, unaffected by the group bug"
            );
        }

        /// A routine fired by a job goes through the same ticket opening and
        /// prefixing `queue add --from` does, so one queue does not end up
        /// mixing `task/<slug>-<id>` and bare names by how each batch
        /// arrived (jobs review finding 6). The routine's own source is not
        /// written to — a `ticket:` landing there would make every later
        /// fire report it `kept` and reuse the first fire's ticket.
        #[test]
        fn a_fired_routine_opens_tickets_and_takes_the_prefix_like_queue_add() {
            let (mut repo, _root_guard) = fixture("open-routine-prefix");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"cat "$SPOOLWAY_TASK_FILE" >"$(dirname "$SPOOLWAY_OUT")/task-file.seen"
                   { echo "ticket=PROJ-13"; echo "slug=proj-12"; } >"$SPOOLWAY_OUT""#,
            );
            let source = "---\nid: audit\ntitle: audit\ngroup: demo\n\
                          group_description: nightly audit\n---\n## Goal\n\nDo it.\n";
            let path = write_routine(&repo, "nightly", "audit", source);

            let tasks = queue_routine_target(
                &repo,
                &Pipelines::builtin(),
                "plan/demo",
                &repo.routines_dir().join("nightly"),
                "bugfix",
            )
            .unwrap();
            assert_eq!(tasks.len(), 1);
            let id = tasks[0].id().to_string();
            assert!(
                id.starts_with("audit-"),
                "minted from the routine's id: {id}"
            );

            let task = queued(&repo, &id);
            assert_eq!(task.front.group.as_deref(), Some("proj-12-demo"));
            assert_eq!(
                task.front.branch.as_deref(),
                Some(format!("task/proj-12-{id}").as_str())
            );
            assert_eq!(task.extra_str("ticket"), "PROJ-13");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                source,
                "the routine's source is never written to"
            );
            // The hook could actually open `SPOOLWAY_TASK_FILE` and read the
            // real task — the routine's own file under
            // `.spoolway/routines/`, not the nonexistent queue path a
            // routine mint never gets written to before this call.
            assert_eq!(
                std::fs::read_to_string(repo.tracking_dir().join("task-file.seen")).unwrap(),
                source
            );
        }

        /// A routine has no task on disk for a failed batch's ids to be
        /// written back into, so the failure must not send a person to
        /// `pending/` to look for them: it says the ids went nowhere, and
        /// that running it again opens a second set.
        #[test]
        fn a_hook_failing_on_a_routine_says_the_ids_were_not_written_anywhere() {
            let (mut repo, _root_guard) = fixture("open-routine-fails");
            with_hook(
                &mut repo,
                r#"if [ "$SPOOLWAY_TASK" != "${SPOOLWAY_TASK#second}" ]; then exit 1; fi
                   echo "ticket=PROJ-13" >"$SPOOLWAY_OUT""#,
            );
            let first = "---\nid: first\ntitle: first\ngroup: demo\n\
                         group_description: nightly work\n---\n## Goal\n\nDo it.\n";
            let second = "---\nid: second\ntitle: second\ngroup: demo\ndepends_on: [first]\n---\n## Goal\n\nDo it.\n";
            let first_path = write_routine(&repo, "nightly", "first", first);
            let second_path = write_routine(&repo, "nightly", "second", second);

            let err = queue_routine_target(
                &repo,
                &Pipelines::builtin(),
                "plan/demo",
                &repo.routines_dir().join("nightly"),
                "bugfix",
            )
            .unwrap_err()
            .to_string();

            assert!(
                err.contains("ids      not written — no task on disk"),
                "{err}"
            );
            assert!(err.contains("close those by hand first"), "{err}");
            assert!(!err.contains("pending/"), "{err}");
            assert_eq!(std::fs::read_to_string(&first_path).unwrap(), first);
            assert_eq!(std::fs::read_to_string(&second_path).unwrap(), second);
            assert!(
                repo.queued_ids().is_empty(),
                "nothing was queued: {:?}",
                repo.queued_ids()
            );
        }

        /// The queue screen's `enter` is the third way a batch arrives, and
        /// it takes the same prefix: driven through `begin_submission` the
        /// way the screen's own tests do, with the issue question already
        /// answered yes.
        #[test]
        fn a_screen_submission_takes_the_prefix_like_queue_add() {
            let (mut repo, _root_guard) = fixture("open-screen-prefix");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"{ echo "ticket=PROJ-13"; echo "slug=proj-12"; } >"$SPOOLWAY_OUT""#,
            );
            write_pending(
                &repo,
                "wire",
                &task_text(
                    "wire",
                    "group: one\ngroup_description: wiring it up\n",
                    BODY,
                ),
            );
            let mut groups = listed(&repo);
            let mut state = ScreenState::new();
            handle_browse_key(&groups, &mut state, Key::Char(' '));

            let outcome = begin_submission(
                &repo,
                &Pipelines::builtin(),
                "plan/demo",
                &mut groups,
                &mut state,
                Tracking::Open,
                &mut |_| {},
            );
            assert!(matches!(outcome, Mode::Queued { .. }), "{outcome:?}");

            let task = queued(&repo, "wire");
            assert_eq!(task.front.group.as_deref(), Some("proj-12-one"));
            assert_eq!(task.front.branch.as_deref(), Some("task/proj-12-wire"));
            assert_eq!(task.extra_str("ticket"), "PROJ-13");
        }

        /// With the flag off, a `slug=` the hook answers is ignored and every
        /// generated name is byte-for-byte what it is today — but a valid
        /// `url=` is still stored on the task, since the goal is to have it
        /// there for `terminal-names` and the flag gates only the naming.
        #[test]
        fn with_the_flag_off_a_slug_answer_changes_no_name_but_the_url_is_kept() {
            let (mut repo, _root_guard) = fixture("open-no-prefix");
            // `fixture`'s own `Config::default` now turns this on; this test
            // is the "off" half, so it has to say so itself.
            repo.config.issue_tracking.key_in_names = false;
            with_hook(
                &mut repo,
                r#"{ echo "ticket=PROJ-13"; echo "slug=proj-12"
                     echo "url=https://acme.atlassian.net/browse/PROJ-12"; } >"$SPOOLWAY_OUT""#,
            );

            let doc = task_text(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\n",
                BODY,
            );
            let path = write_doc(&repo, "auth-01.md", &doc);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap();

            let task = queued(&repo, "auth-01");
            assert_eq!(task.front.group.as_deref(), Some("auth-rework"));
            assert_eq!(task.front.branch.as_deref(), Some("task/auth-01"));
            assert_eq!(task.extra_str("slug"), "", "the slug is naming, gated off");
            assert_eq!(
                task.extra_str("url"),
                "https://acme.atlassian.net/browse/PROJ-12",
                "the url is stored whatever the flag says"
            );
        }

        /// An invalid slug — one that fails `check_id`'s alphabet — is
        /// reported and dropped: the batch still queues, just without a
        /// prefix.
        #[test]
        fn an_invalid_slug_is_dropped_and_the_batch_still_queues() {
            let (mut repo, _root_guard) = fixture("open-bad-slug");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"{ echo "ticket=PROJ-13"; echo "slug=PROJ-12"; } >"$SPOOLWAY_OUT""#,
            );

            let doc = task_text(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\n",
                BODY,
            );
            let path = write_doc(&repo, "auth-01.md", &doc);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap();

            let task = queued(&repo, "auth-01");
            assert_eq!(task.front.group.as_deref(), Some("auth-rework"));
            assert_eq!(task.front.branch.as_deref(), Some("task/auth-01"));
            assert_eq!(task.extra_str("slug"), "");
        }

        /// A `slug:` a person authored (or hand-edited) onto a task is
        /// held to the same `check_id` alphabet as one a hook answers: an
        /// invalid one is dropped, and the batch queues with no prefix rather
        /// than an invalid branch.
        #[test]
        fn an_authored_invalid_slug_is_not_turned_into_a_branch_prefix() {
            let (mut repo, _root_guard) = fixture("open-authored-bad-slug");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(&mut repo, r#"{ echo "ticket=PROJ-13"; } >"$SPOOLWAY_OUT""#);

            let doc = task_text(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\nslug: PROJ-12\n",
                BODY,
            );
            let path = write_doc(&repo, "auth-01.md", &doc);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap();

            let task = queued(&repo, "auth-01");
            assert_eq!(task.front.group.as_deref(), Some("auth-rework"));
            assert_eq!(task.front.branch.as_deref(), Some("task/auth-01"));
            // The value the note said was dropped is not on the queued task.
            assert_eq!(task.extra_str("slug"), "");
        }

        /// A `url=` that is not an absolute http(s) address is dropped, not
        /// stored — the batch still queues.
        #[test]
        fn a_relative_url_is_dropped_and_the_batch_still_queues() {
            let (mut repo, _root_guard) = fixture("open-bad-url");
            with_hook(
                &mut repo,
                r#"{ echo "ticket=PROJ-13"; echo "url=/browse/PROJ-12"; } >"$SPOOLWAY_OUT""#,
            );

            let doc = task_text(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\n",
                BODY,
            );
            let path = write_doc(&repo, "auth-01.md", &doc);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap();

            assert_eq!(queued(&repo, "auth-01").extra_str("url"), "");
        }

        /// One slug and one epic per group, spanning two `queue add` calls: a
        /// second call naming the bare `group:` reuses the first call's epic
        /// and picks the same prefix back up, because the lookup strips the
        /// recognised `<slug>-` prefix off a queued sibling before comparing.
        #[test]
        fn a_second_queue_add_reuses_the_epic_and_the_slug() {
            let (mut repo, _root_guard) = fixture("open-prefix-resume");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"epic=$SPOOLWAY_EPIC
                   if [ -z "$epic" ] && [ "$SPOOLWAY_GROUP_SIZE" -gt 1 ]; then epic=PROJ-12; fi
                   { echo "epic=$epic"; echo "ticket=t-$SPOOLWAY_TASK"
                     echo "slug=proj-12"
                     echo "url=https://acme.atlassian.net/browse/${epic:-PROJ-13}"; } >"$SPOOLWAY_OUT""#,
            );

            let a = task_text(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\n",
                BODY,
            );
            let a_path = write_doc(&repo, "auth-01.md", &a);
            let b = task_text(
                "auth-02",
                "group: auth-rework\ndepends_on: [auth-01]\n",
                BODY,
            );
            let b_path = write_doc(&repo, "auth-02.md", &b);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&a_path, &b_path]),
                &repo.root,
                false,
            )
            .unwrap();
            assert_eq!(queued(&repo, "auth-01").extra_str("epic"), "PROJ-12");

            // A second call, naming the bare group the way a person always
            // writes it — chained onto `auth-02`, the group's own last task,
            // since a group is one chain now.
            let c = task_text(
                "auth-03",
                "group: auth-rework\ndepends_on: [auth-02]\n",
                BODY,
            );
            let c_path = write_doc(&repo, "auth-03.md", &c);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&c_path]),
                &repo.root,
                false,
            )
            .unwrap();

            let third = queued(&repo, "auth-03");
            assert_eq!(
                third.extra_str("epic"),
                "PROJ-12",
                "the second call must reuse the first call's epic, not open a new one"
            );
            assert_eq!(third.front.group.as_deref(), Some("proj-12-auth-rework"));
            assert_eq!(third.front.branch.as_deref(), Some("task/proj-12-auth-03"));
        }

        /// A queued sibling whose stored `slug:` was hand-edited to something
        /// `check_id` rejects is not a recognised prefix: its `<slug>-` is not
        /// stripped off its group for the epic lookup, so a fresh task
        /// naming a *different* group that merely shares the suffix does not
        /// inherit that sibling's epic.
        #[test]
        fn an_invalid_sibling_slug_is_not_a_recognised_prefix_for_the_epic_lookup() {
            let (mut repo, _root_guard) = fixture("open-bad-sibling-slug");
            repo.config.issue_tracking.key_in_names = true;

            // A sibling already in the queue, with a corrupt `slug:` and a
            // group that starts with it.
            std::fs::create_dir_all(repo.queue_dir()).unwrap();
            std::fs::write(
                repo.queue_dir().join("sib.md"),
                "---\nid: sib\ntitle: sib, done\nstage: queued\ngroup: PROJ-rework\n\
                 epic: EPIC-1\nslug: PROJ\n---\n## Goal\n\nx\n",
            )
            .unwrap();

            with_hook(
                &mut repo,
                r#"{ echo "epic=$SPOOLWAY_EPIC"; echo "ticket=t-$SPOOLWAY_TASK"
                     echo "slug=re-1"; } >"$SPOOLWAY_OUT""#,
            );
            let doc = task_text(
                "fresh",
                "group: rework\ngroup_description: fresh rework\n",
                BODY,
            );
            let path = write_doc(&repo, "fresh.md", &doc);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap();

            // `rework` and `PROJ-rework` are unrelated groups once `PROJ` is
            // rejected, so `fresh` did not inherit `EPIC-1`.
            assert_ne!(queued(&repo, "fresh").extra_str("epic"), "EPIC-1");
        }

        /// The queue screen's own issue question: with a hook configured,
        /// nothing reaches it until a person says yes on the tab.
        mod issue_question {
            use super::*;

            /// A hook that logs every call it gets and answers an epic and a
            /// ticket numbered by the call, and one pending group `cart` of
            /// two tasks, ready to select with `space`.
            fn cart(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
                let (mut repo, root_guard) = fixture(name);
                with_hook(
                    &mut repo,
                    r#"log="$(dirname "$SPOOLWAY_OUT")/calls.log"
                       echo "$SPOOLWAY_TASK" >>"$log"
                       n=$(wc -l <"$log")
                       { echo "epic=#410"; echo "ticket=#$(( 410 + n ))"; } >"$SPOOLWAY_OUT""#,
                );
                write_pending_two(
                    &repo,
                    "cart-empty-state",
                    &task_text(
                        "cart-empty-state",
                        "group: cart
group_description: the cart
",
                        BODY,
                    ),
                    "cart-totals",
                    &task_text(
                        "cart-totals",
                        "group: cart
depends_on: [cart-empty-state]
",
                        BODY,
                    ),
                );
                (repo, root_guard)
            }

            fn hook_calls(repo: &Repo) -> Vec<String> {
                std::fs::read_to_string(repo.tracking_dir().join("calls.log"))
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect()
            }

            /// `enter` stops at the question before the hook runs: it lists
            /// every task in the batch and names the tracker, over the tab.
            #[test]
            fn enter_asks_before_the_hook_runs() {
                let (repo, _root_guard) = cart("issue-question-asks");
                let drawn = screen(&repo, listed(&repo), " \r");
                let frame = last_frame(&drawn);
                assert!(frame.contains("┌─ issue tracking "), "{frame}");
                assert!(
                    frame.contains("create 2 issues on open for cart"),
                    "{frame}"
                );
                assert!(frame.contains("    cart-empty-state"), "{frame}");
                assert!(frame.contains("    cart-totals"), "{frame}");
                assert!(
                    frame.contains("[enter] create and queue   [n] queue only   [esc] back"),
                    "{frame}"
                );
                assert!(frame.contains("─ groups"), "the tab under it: {frame}");
                assert!(hook_calls(&repo).is_empty(), "the hook ran unasked");
                assert!(repo.queued_ids().is_empty());
            }

            /// `n` queues the group with issue tracking off: no hook call,
            /// no ticket, and a popup naming what it queued.
            #[test]
            fn n_queues_with_no_hook_call() {
                let (repo, _root_guard) = cart("issue-question-n");
                let drawn = screen(&repo, listed(&repo), " \rn");
                assert!(hook_calls(&repo).is_empty(), "the hook ran on `n`");
                let task = queued(&repo, "cart-totals");
                assert_eq!(task.extra_str("ticket"), "");
                assert_eq!(queued(&repo, "cart-empty-state").extra_str("ticket"), "");

                let frame = last_frame(&drawn);
                assert!(frame.contains("┌─ queued "), "{frame}");
                assert!(frame.contains("queued 2 tasks"), "{frame}");
                assert!(frame.contains("    cart-empty-state"), "{frame}");
                assert!(frame.contains("[enter] confirm"), "{frame}");
            }

            /// `esc` queues nothing and opens nothing, and leaves the
            /// selection as it was.
            #[test]
            fn esc_queues_nothing_and_opens_nothing() {
                let (repo, _root_guard) = cart("issue-question-esc");
                let drawn = screen(&repo, listed(&repo), " \r\x1b");
                assert!(hook_calls(&repo).is_empty());
                assert!(repo.queued_ids().is_empty());
                let frame = last_frame(&drawn);
                assert!(!frame.contains("issue tracking"), "{frame}");
                assert!(frame.contains("[x] cart"), "still selected: {frame}");
            }

            /// `enter` opens the tickets and queues. The popup fills in row
            /// by row while the hook answers — the task at the hook drawn as
            /// `…` — and once it is done says what was opened and queued,
            /// taking `enter` to close.
            #[test]
            fn enter_opens_the_tickets_row_by_row_and_queues() {
                let (repo, _root_guard) = cart("issue-question-enter");
                let drawn = screen(&repo, listed(&repo), " \r\r");
                assert_eq!(hook_calls(&repo), ["cart-empty-state", "cart-totals"]);
                assert_eq!(
                    queued(&repo, "cart-empty-state").extra_str("ticket"),
                    "#411"
                );
                assert_eq!(queued(&repo, "cart-totals").extra_str("ticket"), "#412");

                let frames: Vec<&str> = drawn.split("\x1b[?2026h\x1b[H").collect();
                let opening: Vec<&&str> = frames
                    .iter()
                    .filter(|frame| frame.contains("┌─ opening issues "))
                    .collect();
                assert!(
                    opening
                        .iter()
                        .all(|frame| frame.contains("waiting on the hook")),
                    "{opening:?}"
                );
                // The second task at the hook, under the first one's answer.
                assert!(
                    opening.iter().any(|frame| {
                        frame.contains("group issue  created   #410   cart")
                            && frame.contains("task issue   created   #411   cart-empty-state")
                            && frame.contains("task issue   …                cart-totals")
                    }),
                    "{opening:?}"
                );
                assert!(
                    !opening
                        .iter()
                        .any(|frame| frame.contains("[enter] confirm")),
                    "a popup still filling takes no key: {opening:?}"
                );

                let frame = last_frame(&drawn);
                assert!(frame.contains("┌─ issues created "), "{frame}");
                assert!(
                    frame.contains("task issue   created   #412   cart-totals"),
                    "{frame}"
                );
                assert!(frame.contains("queued 2 tasks"), "{frame}");
                assert!(
                    frame.contains("Start the dispatcher to begin working"),
                    "{frame}"
                );
                assert!(frame.contains("[enter] confirm"), "{frame}");
                // Nothing printed under the frame: every byte went out as part
                // of one frame or another, each opening with the shared frame
                // writer's own start code.
                assert!(drawn.starts_with("\x1b[?2026h\x1b[H"), "{drawn}");
                assert!(
                    !drawn.contains("issue_tracking: opening tickets"),
                    "{drawn}"
                );
            }

            /// A hook answering `slug=` with `key_in_names` on prefixes the
            /// names, as the shipped `github.sh` does on every batch — and
            /// the result is still step 12: ticket rows, then what was
            /// queued, the box held to the question's width, and no
            /// `opening issues` frame drawn once the last ticket is in.
            #[test]
            fn a_slug_answer_keeps_the_result_to_the_ticket_rows() {
                let (mut repo, _root_guard) = cart("issue-question-slug");
                repo.config.issue_tracking.key_in_names = true;
                with_hook(
                    &mut repo,
                    r#"log="$(dirname "$SPOOLWAY_OUT")/calls.log"
                       echo "$SPOOLWAY_TASK" >>"$log"
                       n=$(wc -l <"$log")
                       { echo "epic=#410"; echo "ticket=#$(( 410 + n ))"
                         echo "slug=gh-410"; } >"$SPOOLWAY_OUT""#,
                );
                let drawn = screen(&repo, listed(&repo), " \r\r");
                assert_eq!(
                    queued(&repo, "cart-totals").front.group.as_deref(),
                    Some("gh-410-cart")
                );

                let frames: Vec<&str> = drawn.split("\x1b[?2026h\x1b[H").collect();
                let last_opening = frames
                    .iter()
                    .rposition(|frame| frame.contains("┌─ opening issues "))
                    .unwrap();
                assert!(
                    frames[last_opening].contains("#412   cart-totals"),
                    "the last `opening issues` frame is the last ticket's: {}",
                    frames[last_opening]
                );

                let frame = last_frame(&drawn);
                assert!(!frame.contains("names prefixed"), "{frame}");
                let body: Vec<String> = frame
                    .lines()
                    .skip_while(|line| !line.contains("┌─ issues created "))
                    .skip(1)
                    .take_while(|line| !line.contains('└') || line.contains('│'))
                    .map(|line| line.split('│').nth(2).unwrap_or("").trim().to_string())
                    .collect();
                assert_eq!(
                    body[..7],
                    [
                        "",
                        "group issue  created   #410   cart",
                        "task issue   created   #411   cart-empty-state",
                        "task issue   created   #412   cart-totals",
                        "",
                        "queued 2 tasks",
                        "",
                    ],
                    "{frame}"
                );
                let top = frame
                    .lines()
                    .find(|line| line.contains("┌─ issues created "))
                    .unwrap();
                let width = top
                    .chars()
                    .skip_while(|c| *c != '┌')
                    .take_while(|c| *c != '┐')
                    .count()
                    + 1;
                assert_eq!(width, 60, "held to the question's width: {frame}");
            }

            /// Queuing a routine asks the same question, over the routines
            /// pane, and `n` there queues it with no hook call.
            #[test]
            fn queuing_a_routine_asks_the_same_question() {
                let (repo, _root_guard) = cart("issue-question-routine");
                write_routine(
                    &repo,
                    "nightly",
                    "audit",
                    &task_text(
                        "audit",
                        "group: nightly
group_description: audit
",
                        BODY,
                    ),
                );
                let drawn = routines_screen(&repo, " \r");
                let frame = last_frame(&drawn);
                assert!(frame.contains("┌─ issue tracking "), "{frame}");
                assert!(
                    frame.contains("create 1 issue on open for nightly"),
                    "{frame}"
                );
                assert!(
                    frame.contains("─ routines"),
                    "over the routines pane: {frame}"
                );
                assert!(repo.queued_ids().is_empty());

                routines_screen(&repo, " \rn");
                assert!(hook_calls(&repo).is_empty(), "the hook ran on `n`");
                assert_eq!(repo.queued_ids().len(), 1, "{:?}", repo.queued_ids());
            }

            /// A trial never asks and opens no tickets, as before.
            #[test]
            fn a_trial_never_asks_and_opens_no_tickets() {
                let (repo, _root_guard) = cart("issue-question-trial");
                let drawn = screen(&repo, listed(&repo), "t\r\r");
                assert!(!drawn.contains("issue tracking"), "{drawn}");
                assert!(hook_calls(&repo).is_empty());
                assert_eq!(repo.queued_ids().len(), 2, "{:?}", repo.queued_ids());
            }
        }
    }

    mod tool_requirements_gate_tests {
        use super::*;

        /// Points `[issue_tracking].hook` at an executable script declaring
        /// a `cargo` requirement — `cargo` because it is the one binary this
        /// project's own build guarantees is on PATH wherever its tests run,
        /// the same choice `doctor`'s own version-gate tests make.
        fn with_versioned_hook(repo: &mut Repo, floor: &str) {
            let dir = repo.checkout.join(".spoolway/hooks");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("versioned.sh");
            std::fs::write(
                &path,
                format!("#!/bin/sh\n# spoolway-requires: cargo >= {floor}\nexit 0\n"),
            )
            .unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            repo.config.issue_tracking.hook = "versioned.sh".to_string();
        }

        /// No hook at all: `unmet_requirements` has nothing to read and the
        /// gate returns straight through, exactly like `overrides_gate` over
        /// a project with no layer.
        #[test]
        fn no_hook_configured_has_nothing_unmet() {
            let (repo, _root_guard) = fixture("tool-gate-no-hook");
            assert!(unmet_requirements(&repo).is_empty());
        }

        /// A declared floor this machine's own `cargo` already clears —
        /// `unmet_requirements` reports nothing, and the gate proceeds
        /// without drawing at all.
        #[test]
        fn a_met_requirement_draws_nothing() {
            let (mut repo, _root_guard) = fixture("tool-gate-met");
            with_versioned_hook(&mut repo, "0.0.1");
            assert!(unmet_requirements(&repo).is_empty());

            let mut input = keys("");
            let mut out = Vec::new();
            let skip = tool_requirements_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                crate::platform::TermGuard::inert,
            )
            .unwrap();
            assert!(!skip, "nothing unmet means issue tracking stays on");
            assert!(out.is_empty(), "{out:?}");
        }

        /// An unmet floor draws the box the mockup shows — the hook, the
        /// tool and the floor it declares, and what this machine actually
        /// has for it — with nobody there to answer: printed and proceeding
        /// without a key, the way the mockup's own non-interactive path
        /// promises, and `input` is left empty on purpose so a `read_key`
        /// call here would hang the test rather than fail it.
        #[test]
        fn an_unmet_requirement_draws_and_proceeds_with_no_tty() {
            let (mut repo, _root_guard) = fixture("tool-gate-no-tty");
            with_versioned_hook(&mut repo, "999.0.0");

            let mut input = keys("");
            let mut out = Vec::new();
            let skip = tool_requirements_gate_with(
                &repo,
                false,
                &mut input,
                &mut out,
                crate::platform::TermGuard::inert,
            )
            .unwrap();
            assert!(skip, "issue tracking must be switched off for this run");
            let printed = String::from_utf8(out).unwrap();
            assert!(printed.contains("versioned.sh"), "{printed}");
            assert!(printed.contains("cargo >= 999.0.0"), "{printed}");
            assert!(printed.contains("cargo"), "{printed}");
            assert!(
                printed.contains("issue tracking is not supported."),
                "{printed}"
            );
            assert!(
                !printed.contains("[enter]"),
                "a non-interactive run never waits on a key: {printed}"
            );
        }

        /// `enter` over the drawn gate queues the batch with issue tracking
        /// switched off for this run — the same outcome the no-tty path
        /// gives, reached instead by a person answering the prompt.
        #[test]
        fn enter_over_the_gate_skips_tracking() {
            let (mut repo, _root_guard) = fixture("tool-gate-enter");
            with_versioned_hook(&mut repo, "999.0.0");

            let mut input = keys("\r");
            let mut out = Vec::new();
            let skip = tool_requirements_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                crate::platform::TermGuard::inert,
            )
            .unwrap();
            assert!(skip);
            let printed = String::from_utf8(out).unwrap();
            assert!(printed.contains("[enter] queue anyway"), "{printed}");
        }

        /// `esc` is the one path that must reach the caller as `GateCancelled`
        /// — `queue_add_tasks` turns that into a clean exit rather than
        /// a refusal, since nothing here failed.
        #[test]
        fn esc_over_the_gate_cancels() {
            let (mut repo, _root_guard) = fixture("tool-gate-esc");
            with_versioned_hook(&mut repo, "999.0.0");

            let mut input = keys("\x1b");
            let mut out = Vec::new();
            let err = tool_requirements_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                crate::platform::TermGuard::inert,
            )
            .unwrap_err();
            assert!(err.downcast_ref::<GateCancelled>().is_some(), "{err:#}");
        }

        /// A pending group whose hook declares a floor this machine cannot
        /// meet, selected and submitted on the queue screen: `enter` draws
        /// the gate as a popup over the tab — the tab still drawn under it,
        /// nothing printed outside the frame — and queues nothing yet.
        fn submit_over_the_gate(
            name: &str,
            then: &str,
        ) -> (Repo, String, crate::scratch::ScratchRoot) {
            let (mut repo, root_guard) = fixture(name);
            with_versioned_hook(&mut repo, "999.0.0");
            write_pending(
                &repo,
                "wire",
                &task_text("wire", "group: a\ngroup_description: a\n", BODY),
            );
            let (_, drawn) = screen_exit(&repo, listed(&repo), &format!(" \r{then}"));
            (repo, drawn, root_guard)
        }

        #[test]
        fn the_screen_draws_the_gate_in_a_popup_over_the_tab() {
            let (repo, drawn, _root_guard) = submit_over_the_gate("tool-gate-popup", "");
            let frame = last_frame(&drawn);
            assert!(frame.contains("┌─ issue tracking "), "{frame}");
            assert!(frame.contains("versioned.sh"), "{frame}");
            assert!(frame.contains("cargo >= 999.0.0"), "{frame}");
            assert!(
                frame.contains("issue tracking is not supported."),
                "{frame}"
            );
            assert!(
                frame.contains("[enter] queue anyway, without issue tracking   [esc] back"),
                "{frame}"
            );
            assert!(frame.contains("─ groups"), "the tab under it: {frame}");
            // Nothing outside the frame: every byte written went out as part
            // of one frame or another, each opening with the shared frame
            // writer's own start code.
            assert!(drawn.starts_with("\x1b[?2026h\x1b[H"), "{drawn}");
            assert!(!repo.queue_dir().join("wire.md").exists());
        }

        /// `esc` goes back to the screen the submit started from, having
        /// queued nothing and said nothing more.
        #[test]
        fn esc_off_the_gate_popup_goes_back_and_queues_nothing() {
            let (repo, drawn, _root_guard) = submit_over_the_gate("tool-gate-popup-esc", "\x1b");
            let frame = last_frame(&drawn);
            assert!(!frame.contains("issue tracking"), "{frame}");
            assert!(frame.contains("[x] a"), "still selected: {frame}");
            assert!(!repo.queue_dir().join("wire.md").exists());
        }

        /// `enter` queues the batch with issue tracking switched off for it:
        /// the hook is never called.
        #[test]
        fn enter_on_the_gate_popup_queues_without_tracking() {
            let (repo, drawn, _root_guard) = submit_over_the_gate("tool-gate-popup-enter", "\r");
            let frame = last_frame(&drawn);
            assert!(!frame.contains("issue tracking"), "{frame}");
            let task = queued(&repo, "wire");
            assert_eq!(task.extra_str("ticket"), "");
            assert!(
                std::fs::read_dir(repo.tracking_dir())
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
                "the hook must never have been called"
            );
        }

        /// A tool the declared floor names but that is not on PATH at all is
        /// unmet too — a submit gate has no sibling check to leave that gap
        /// to the way `doctor` does, so it draws the same box with "not on
        /// PATH" in place of a version and a location.
        #[test]
        fn a_tool_missing_from_path_entirely_is_unmet() {
            let (mut repo, _root_guard) = fixture("tool-gate-missing-tool");
            let dir = repo.checkout.join(".spoolway/hooks");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("versioned.sh");
            std::fs::write(
                &path,
                "#!/bin/sh\n# spoolway-requires: definitely-not-a-real-binary >= 1.0.0\nexit 0\n",
            )
            .unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
            std::fs::set_permissions(&path, perms).unwrap();
            repo.config.issue_tracking.hook = "versioned.sh".to_string();

            let unmet = unmet_requirements(&repo);
            assert_eq!(
                unmet.len(),
                1,
                "{:?}",
                unmet.iter().map(|u| &u.tool).collect::<Vec<_>>()
            );
            assert!(unmet[0].found.is_none());

            let mut out = Vec::new();
            print_tool_gate_notice(&mut out, &unmet).unwrap();
            let printed = String::from_utf8(out).unwrap();
            assert!(printed.contains("not on PATH"), "{printed}");
        }

        /// `open_and_prefix` is the one call every submit route shares, and
        /// this is the whole point of the gate living there: an unmet
        /// requirement never reaches `open_tickets` at all, so no `epic:` or
        /// `ticket:` lands on the task — the batch queues exactly as it
        /// would with issue tracking switched off. Driven with
        /// `interactive: false` — `queue_add_tasks`'s own path when
        /// `crate::ask::interactive()` says nobody is there — the same
        /// no-tty branch the test above drives directly.
        #[test]
        fn open_and_prefix_skips_open_tickets_when_a_requirement_is_unmet() {
            let (mut repo, _root_guard) = fixture("tool-gate-open-and-prefix");
            with_versioned_hook(&mut repo, "999.0.0");
            let doc = task_text(
                "solo",
                "group: solo\ngroup_description: a solo task\n",
                BODY,
            );
            let mut tasks = validate_batch(
                &repo,
                &Pipelines::builtin(),
                Some("plan/demo"),
                &[("solo.md".to_string(), doc)],
            )
            .unwrap();

            let gate = ToolGate::Print { interactive: false };
            open_and_prefix(&repo, &[], &[], &mut tasks, gate, &mut PrintedTickets).unwrap();

            assert_eq!(tasks[0].extra_str("ticket"), "");
            assert_eq!(tasks[0].extra_str("epic"), "");
            assert!(
                std::fs::read_dir(repo.tracking_dir())
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
                "the hook must never have been called"
            );
        }

        /// The same unmet-requirement gate, but checked for what this task's
        /// acceptance criteria actually asks: every task in the batch comes
        /// out carrying `tracking: off`, the key `dispatch::route_reserved_stage`
        /// and `tracking_gate` read back through `Task::tracking_off` to fire
        /// no hook for it and never hold it waiting on one.
        #[test]
        fn open_and_prefix_stamps_tracking_off_when_a_requirement_is_unmet() {
            let (mut repo, _root_guard) = fixture("tool-gate-stamps-tracking-off");
            with_versioned_hook(&mut repo, "999.0.0");
            let doc = task_text(
                "solo",
                "group: solo\ngroup_description: a solo task\n",
                BODY,
            );
            let mut tasks = validate_batch(
                &repo,
                &Pipelines::builtin(),
                Some("plan/demo"),
                &[("solo.md".to_string(), doc)],
            )
            .unwrap();

            let gate = ToolGate::Print { interactive: false };
            open_and_prefix(&repo, &[], &[], &mut tasks, gate, &mut PrintedTickets).unwrap();

            assert!(tasks[0].tracking_off());
            assert_eq!(tasks[0].extra_str("tracking"), "off");
        }

        /// The counterpart: a batch whose tickets opened cleanly carries no
        /// `tracking:` key at all — the acceptance criterion this task set
        /// alongside the one above.
        #[test]
        fn open_and_prefix_writes_no_tracking_key_when_tickets_open() {
            let (repo, _root_guard) = fixture("tool-gate-no-tracking-key-when-open");
            let doc = task_text("solo", "group: solo\n", BODY);
            let mut tasks = validate_batch(
                &repo,
                &Pipelines::builtin(),
                Some("plan/demo"),
                &[("solo.md".to_string(), doc)],
            )
            .unwrap();

            let gate = ToolGate::Answered {
                tracking_off: false,
            };
            open_and_prefix(&repo, &[], &[], &mut tasks, gate, &mut PrintedTickets).unwrap();

            assert!(!tasks[0].tracking_off());
            assert_eq!(tasks[0].extra_str("tracking"), "");
        }
    }

    mod reset_for_reuse_tests {
        use super::*;

        /// A task in the shape a queued or archived task actually has on
        /// disk: every key spoolway stamps over a task's life, alongside what
        /// its author wrote. `epic:`/`ticket:`/`slug:`/`url:` stand in for the
        /// passthrough keys the reset has to name specially; `my_custom:`
        /// stands in for an ordinary one it does not.
        const STAMPED_DOC: &str = "\
---
id: board-key-map
title: Split the board's keys
touches:
- src/status.rs
depends_on:
- cursor-in-gap
stage: done
pipeline: ui
group: board-key-map
epic: https://github.com/acme/app/issues/1
ticket: https://github.com/acme/app/issues/2
slug: gh-1
url: https://github.com/acme/app/issues/1
my_custom: kept
branch: task/board-key-map
base: master
run: r0c5746afb1ed8aa4
starts_from: task/cursor-in-gap
base_commit: fe481e57b4c
worktree_path: /home/x/board-key-map
workspace_id: w7H
pane_id: w7H:p1
tab_id: w7H:t1
prompts:
  queued->implement: 1
rounds:
  queued->implement: 1
arrived_from: handover
attempts: 0
---
## Context

body\n";

        /// The allowlist keeps every field an author owns, drops every field
        /// spoolway stamped — `stage:` and the rest of `RESERVED_KEYS` among
        /// them — and drops all four hook-written passthrough keys
        /// (`epic:`/`ticket:`/`slug:`/`url:`) despite none being a typed
        /// `Frontmatter` field at all. A project's own `my_custom:` key
        /// survives, the same as any passthrough key does. The body travels
        /// byte for byte.
        #[test]
        fn drops_every_stamped_key_and_keeps_what_the_author_owns() {
            let reset = reset_for_reuse("board-key-map.md", STAMPED_DOC).unwrap();

            for kept in [
                "id: board-key-map",
                "title: Split the board's keys",
                "touches:",
                "- src/status.rs",
                "depends_on:",
                "- cursor-in-gap",
                "pipeline: ui",
                "group: board-key-map",
                "my_custom: kept",
            ] {
                assert!(reset.contains(kept), "missing `{kept}`:\n{reset}");
            }

            for dropped in [
                "stage:",
                "epic:",
                "ticket:",
                "slug:",
                "url:",
                "branch:",
                "base:",
                "run:",
                "starts_from:",
                "base_commit:",
                "worktree_path:",
                "workspace_id:",
                "pane_id:",
                "tab_id:",
                "prompts:",
                "rounds:",
                "arrived_from:",
                "attempts:",
            ] {
                assert!(!reset.contains(dropped), "`{dropped}` survived:\n{reset}");
            }

            assert!(
                reset.ends_with("## Context\n\nbody\n"),
                "the body must travel byte for byte:\n{reset}"
            );
        }

        /// A task already free of every stamped key — the shape a
        /// producer actually writes — reads back unchanged: nothing here has
        /// anything to drop, and re-serialising a mapping that lost no keys
        /// is a no-op on its contents (block style throughout, so the
        /// re-serialised form matches the written one byte for byte).
        #[test]
        fn a_task_with_no_stamped_keys_round_trips() {
            let doc = task_text("solo", "touches:\n- src/**\ngroup: g\n", BODY);
            let reset = reset_for_reuse("solo.md", &doc).unwrap();
            assert_eq!(reset, doc);
        }

        /// `parse_submission` refuses `STAMPED_DOC` outright, over `stage:`
        /// — the exact refusal a re-used queued or archived task hits
        /// today. Resetting it first is what lets it reach `parse_submission`
        /// at all.
        #[test]
        fn parse_submission_refuses_the_stamped_task_but_not_its_reset() {
            let err =
                parse_submission("board-key-map.md", STAMPED_DOC, Some("master")).unwrap_err();
            assert!(format!("{err:#}").contains("sets `stage:`"));

            let reset = reset_for_reuse("board-key-map.md", STAMPED_DOC).unwrap();
            let task = parse_submission("board-key-map.md", &reset, Some("master")).unwrap();
            assert_eq!(task.id(), "board-key-map");
            assert_eq!(task.stage(), crate::pipeline::QUEUED);
        }
    }

    fn unqueue_args(task: &str) -> QueueUnqueueArgs {
        QueueUnqueueArgs {
            task: Some(task.to_string()),
            all: false,
            force: false,
        }
    }

    fn unqueue_all_args(force: bool) -> QueueUnqueueArgs {
        QueueUnqueueArgs {
            task: None,
            all: true,
            force,
        }
    }

    fn unqueue_forced_args(task: &str) -> QueueUnqueueArgs {
        QueueUnqueueArgs {
            task: Some(task.to_string()),
            all: false,
            force: true,
        }
    }

    /// `--force` may not remove a started task while a queued task depends
    /// on it, since that dependent would wait on a task that no longer
    /// exists. The refusal names the dependent and leaves both in the queue.
    #[test]
    fn unqueue_force_refuses_a_started_task_a_queued_task_depends_on() {
        let (repo, _root_guard) = fixture("queue-unqueue-force-dependent");
        let pipelines = Pipelines::builtin();
        add(&repo, "parent", &[]);
        add(&repo, "child", &["parent"]);
        let mut task = queued(&repo, "parent");
        task.front.stage = "implement".to_string();
        task.save().unwrap();

        let err = queue_unqueue(&repo, &pipelines, &unqueue_forced_args("parent"))
            .expect_err("a started parent with a queued child is not removable");
        let said = format!("{err:#}");
        assert!(said.contains("child"), "{said}");
        assert!(repo.queue_dir().join("parent.md").exists());
        assert!(repo.queue_dir().join("child.md").exists());
    }

    /// `queue unqueue` on a task that has not started: the task goes
    /// back to pending with the stamped keys dropped — the board's own
    /// unqueue, from a script — and the queue file is gone.
    #[test]
    fn queue_unqueue_carries_a_queued_task_back_to_pending() {
        let (repo, _root_guard) = fixture("queue-unqueue");
        add(&repo, "login", &[]);
        assert!(repo.queue_dir().join("login.md").exists());

        queue_unqueue(&repo, &Pipelines::builtin(), &unqueue_args("login")).unwrap();

        assert!(
            !repo.queue_dir().join("login.md").exists(),
            "the queue file must be gone"
        );
        let pending = repo.pending_dir().join("login.md");
        let text = std::fs::read_to_string(&pending).unwrap();
        for key in RESERVED_KEYS {
            assert!(
                !text.contains(&format!("\n{key}:")),
                "`{key}:` must be stripped so `queue add --from` takes it again: {text}"
            );
        }
        assert!(text.contains("id: login"), "{text}");

        // And it is a task again: the same path in queues it back.
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[pending.to_str().unwrap()]),
            &repo.root,
            false,
        )
        .unwrap();
        assert!(repo.queue_dir().join("login.md").exists());
    }

    /// `--all` carries every not-started task back at once, with no
    /// dependency check between them — the board's own `U`, from a script.
    #[test]
    fn queue_unqueue_all_carries_every_not_started_task() {
        let (repo, _root_guard) = fixture("queue-unqueue-all");
        add(&repo, "base", &[]);
        add(&repo, "dependent", &["base"]);

        queue_unqueue(&repo, &Pipelines::builtin(), &unqueue_all_args(false)).unwrap();

        assert!(!repo.queue_dir().join("base.md").exists());
        assert!(!repo.queue_dir().join("dependent.md").exists());
        assert!(repo.pending_dir().join("base.md").exists());
        assert!(repo.pending_dir().join("dependent.md").exists());
    }

    /// `--all --force` is refused outright, in either flag order — tearing
    /// down every checkout in the queue in one line is not a command a
    /// script should be able to reach by accident.
    #[test]
    fn queue_unqueue_all_force_is_refused() {
        let (repo, _root_guard) = fixture("queue-unqueue-all-force");
        add(&repo, "login", &[]);

        let err = queue_unqueue(&repo, &Pipelines::builtin(), &unqueue_all_args(true)).unwrap_err();
        assert!(format!("{err:#}").contains("--all --force"));
        assert!(repo.queue_dir().join("login.md").exists());
    }

    /// A task on `queued` names a sibling that depends on it in the
    /// refusal — this command has no panel to carry the two back together,
    /// unlike the board's own `u` — and a task the queue does not have
    /// names itself.
    #[test]
    fn queue_unqueue_refuses_a_queued_dependency_and_an_unknown_id() {
        let (repo, _root_guard) = fixture("queue-unqueue-refusal");
        let pipelines = Pipelines::builtin();
        add(&repo, "base", &[]);
        add(&repo, "dependent", &["base"]);

        let err = queue_unqueue(&repo, &pipelines, &unqueue_args("base")).unwrap_err();
        let said = format!("{err:#}");
        assert!(
            said.contains("dependent") && said.contains("--all"),
            "{said}"
        );
        assert!(repo.queue_dir().join("base.md").exists());

        let err = queue_unqueue(&repo, &pipelines, &unqueue_args("nope")).unwrap_err();
        assert!(format!("{err:#}").contains("no queued task"), "{err:#}");
    }

    /// A task that has started is refused without `--force`, naming its
    /// stage, its checkout, and both routes onward — and nothing on disk
    /// moves. The same task with `--force` tears the checkout down and
    /// unqueues it, leaving no checkout field behind on the task that
    /// reaches pending.
    #[test]
    fn queue_unqueue_refuses_a_started_task_without_force_and_tears_down_with_it() {
        let (repo, _root_guard) = fixture("queue-unqueue-started");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);

        // No workspace recorded, no real worktree cut: `tear_down_checkout`
        // treats an already-gone checkout as a fine outcome rather than an
        // error, and this test's own job is only the road around it — that
        // the reserved fields are cleared before the task reaches
        // pending — not the removal itself, which `teardown.rs`'s own
        // callers already cover.
        let worktree = repo.root.join("wt-solo");

        let mut task = queued(&repo, "solo");
        task.front.stage = "implement".to_string();
        task.front.worktree_path = Some(worktree.clone());
        task.save().unwrap();

        let err = queue_unqueue(&repo, &pipelines, &unqueue_args("solo")).unwrap_err();
        let said = format!("{err:#}");
        assert!(
            said.contains("is on `implement`, not `queued`")
                && said.contains(&worktree.display().to_string())
                && said.contains("spoolway queue pause solo")
                && said.contains("spoolway queue unqueue solo --force"),
            "{said}"
        );
        assert!(
            repo.queue_dir().join("solo.md").exists(),
            "a refused unqueue must leave the queue file where it is"
        );

        queue_unqueue(&repo, &pipelines, &unqueue_forced_args("solo")).unwrap();
        assert!(!repo.queue_dir().join("solo.md").exists());
        let pending = repo.pending_dir().join("solo.md");
        let text = std::fs::read_to_string(&pending).unwrap();
        for key in ["worktree_path", "workspace_id", "pane_id", "tab_id"] {
            assert!(
                !text.contains(&format!("\n{key}:")),
                "`{key}:` must be stripped: {text}"
            );
        }
    }

    /// A newer draft already sitting in pending under the same id refuses
    /// the bare road, leaving the queue file exactly where it was —
    /// `unqueue_or_bail`'s own reason for existing over bare
    /// `status::unqueue_task`, which is silent about this.
    #[test]
    fn queue_unqueue_refuses_a_task_already_drafted_in_pending() {
        let (repo, _root_guard) = fixture("queue-unqueue-pending-conflict");
        add(&repo, "login", &[]);
        std::fs::create_dir_all(repo.pending_dir()).unwrap();
        std::fs::write(
            repo.pending_dir().join("login.md"),
            task_text("login", "group: demo\n", BODY),
        )
        .unwrap();

        let err = queue_unqueue(&repo, &Pipelines::builtin(), &unqueue_args("login")).unwrap_err();
        let said = format!("{err:#}");
        assert!(
            said.contains("did not unqueue")
                && said.contains(&repo.pending_dir().join("login.md").display().to_string()),
            "{said}"
        );
        assert!(
            repo.queue_dir().join("login.md").exists(),
            "a refused unqueue must leave the queue file where it is"
        );
    }

    /// The same conflict, with `--force` on a task that has started: caught
    /// before anything is torn down, so the checkout is left standing and
    /// the queue file untouched — "all or nothing" over the whole move, not
    /// just the final write.
    #[test]
    fn queue_unqueue_force_refuses_a_task_already_drafted_in_pending_before_tearing_down() {
        let (repo, _root_guard) = fixture("queue-unqueue-force-pending-conflict");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);
        let worktree = repo.root.join("wt-solo");
        let mut task = queued(&repo, "solo");
        task.front.stage = "implement".to_string();
        task.front.worktree_path = Some(worktree.clone());
        task.save().unwrap();

        std::fs::create_dir_all(repo.pending_dir()).unwrap();
        std::fs::write(
            repo.pending_dir().join("solo.md"),
            task_text("solo", "group: demo\n", BODY),
        )
        .unwrap();

        let err = queue_unqueue(&repo, &pipelines, &unqueue_forced_args("solo")).unwrap_err();
        let said = format!("{err:#}");
        assert!(
            said.contains("did not unqueue") && said.contains("Nothing was torn down"),
            "{said}"
        );
        assert!(
            repo.queue_dir().join("solo.md").exists(),
            "a refused forced unqueue must leave the queue file where it is"
        );
        let text = std::fs::read_to_string(repo.queue_dir().join("solo.md")).unwrap();
        assert!(
            text.contains(&worktree.display().to_string()),
            "the checkout must still be recorded: nothing was torn down: {text}"
        );
    }

    /// `--dry-run` validates the batch and says where everything would go,
    /// and writes nothing: no task file, and no file of any kind under the
    /// project's home.
    #[test]
    fn queue_add_dry_run_writes_nothing() {
        let (repo, _root_guard) = fixture("queue-add-dry-run");
        let text = task_text("login", "group: demo\n", BODY);
        let path = write_doc(&repo, "login.md", &text);
        let mut args = from_args(&[&path]);
        args.dry_run = true;

        queue_add(&repo, &Pipelines::builtin(), &args, &repo.root, false).unwrap();

        assert!(
            !repo.queue_dir().join("login.md").exists(),
            "a dry run must not queue"
        );
        let mut files = Vec::new();
        collect_files(&repo.home, &mut files);
        assert!(files.is_empty(), "a dry run wrote under home: {files:?}");
        assert!(
            std::path::Path::new(&path).exists(),
            "the task itself is left where it was"
        );

        // A broken task still fails the dry run, the way the real thing
        // would — that is what it is for.
        let broken = write_doc(&repo, "broken.md", &task_text("nogroup", "", BODY));
        let mut args = from_args(&[&broken]);
        args.dry_run = true;
        let err = queue_add(&repo, &Pipelines::builtin(), &args, &repo.root, false).unwrap_err();
        assert!(format!("{err:#}").contains("`group:`"), "{err:#}");
    }

    /// `queue add --from` a path under this project's own pending directory:
    /// once the batch is written, the source task is gone from there —
    /// it reached the queue, so it is not still waiting to go there — and
    /// unqueueing the same task afterwards is free to write its clean
    /// task back without tripping the "newer draft" refusal a leftover
    /// copy would cause.
    #[test]
    fn queue_add_from_the_pending_directory_removes_its_own_source() {
        let (repo, _root_guard) = fixture("queue-add-from-pending");
        let text = task_text("beta", "group: one\n", BODY);
        let path = write_pending(&repo, "beta", &text);

        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path.display().to_string()]),
            &repo.root,
            false,
        )
        .unwrap();

        assert!(
            repo.queue_dir().join("beta.md").exists(),
            "beta must have landed in the queue"
        );
        assert!(
            !path.exists(),
            "the pending source must be gone once the batch is written"
        );

        // The round trip this leftover used to break: unqueue must be free
        // to write beta's clean task back, with nothing already sitting
        // in its way.
        crate::status::unqueue_task(&repo, "beta").unwrap();
        assert!(
            path.exists(),
            "unqueue must be able to write beta's task back to pending"
        );
        assert!(
            !repo.queue_dir().join("beta.md").exists(),
            "beta's queue file must be gone once it is unqueued"
        );
    }

    /// A `--from` path outside this project's own pending directory is read
    /// and left exactly where it is — only a task this batch's own inbox
    /// held is ever removed.
    #[test]
    fn queue_add_from_outside_pending_leaves_the_source_alone() {
        let (repo, _root_guard) = fixture("queue-add-from-elsewhere");
        let text = task_text("login", "group: demo\n", BODY);
        let path = write_doc(&repo, "login.md", &text);

        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap();

        assert!(
            repo.queue_dir().join("login.md").exists(),
            "login must have landed in the queue"
        );
        assert!(
            std::path::Path::new(&path).exists(),
            "a --from source outside the pending directory must be left alone"
        );
    }

    fn collect_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files(&path, out);
            } else {
                out.push(path);
            }
        }
    }

    /// A task may name the branch it is cut from and merges into. One
    /// that does keeps it — verified against the repository's own branches
    /// first — and one that does not takes the submission's own `--base`
    /// instead.
    #[test]
    fn a_task_naming_its_own_base_is_cut_from_that_branch() {
        let (repo, _root_guard) = fixture("task-base");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        git(&["branch", "release/1.x"]);

        // Two groups of one, not one group of two — a group is one chain
        // now, and neither task here depends on the other.
        let own = write_doc(
            &repo,
            "own.md",
            &task_text("own", "group: own-group\nbase: release/1.x\n", BODY),
        );
        let plain = write_doc(
            &repo,
            "plain.md",
            &task_text("plain", "group: plain-group\n", BODY),
        );
        queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&own, &plain]),
            &repo.root,
            false,
        )
        .unwrap();

        assert_eq!(
            queued(&repo, "own").front.base.as_deref(),
            Some("release/1.x"),
            "the task's own base is what its worktree is cut from"
        );
        assert_eq!(
            queued(&repo, "plain").front.base.as_deref(),
            Some("plan/demo"),
            "a task without `base:` takes the submission's own `--base`"
        );
        assert_eq!(
            based_on_note(&[queued(&repo, "own"), queued(&repo, "plain")], "plan/demo"),
            "  own based on `release/1.x`\n  plain based on `plan/demo`",
            "the printed line names the base each task actually got"
        );
        assert_eq!(
            based_on_note(&[queued(&repo, "plain")], "plan/demo"),
            "  based on `plan/demo`"
        );

        // A dependent has to share its dependency's base, whichever way
        // either of them came by it.
        let dep = write_doc(
            &repo,
            "dep.md",
            &task_text("dep", "group: demo\ndepends_on: [own]\n", BODY),
        );
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&dep]),
            &repo.root,
            false,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("would never put it in reach"),
            "{err:#}"
        );
    }

    /// The three values `base:` is refused for, each named back at the
    /// task: a branch the repository does not have, a name git will not
    /// accept, and anything that would reach git as a flag.
    #[test]
    fn a_task_base_that_is_not_a_local_branch_is_refused() {
        let (repo, _root_guard) = fixture("task-base-refused");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);

        for (base, expected) in [
            ("release/2.x", "does not have locally"),
            ("bad..name", "not a valid branch name"),
            ("-x", "starts with `-`"),
        ] {
            let path = write_doc(
                &repo,
                "t.md",
                &task_text("t", &format!("group: demo\nbase: {base}\n"), BODY),
            );
            let err = queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&path]),
                &repo.root,
                false,
            )
            .unwrap_err();
            let said = format!("{err:#}");
            assert!(said.contains(expected), "base `{base}`: {said}");
            assert!(
                !repo.queue_dir().join("t.md").exists(),
                "base `{base}` must not have been queued"
            );
        }
    }

    /// A gate between two plain steps, and a staffed `blocked` that
    /// `queue route` must leave out — the shapes its tests read.
    fn route_pipelines() -> Pipelines {
        let yaml = "steps:\n  \
             - id: build\n    agent: pi\n    description: Build it.\n    on_pass: deploy\n  \
             - id: deploy\n    agent: pi\n    description: Ship it.\n    gate: true\n    loop: 2\n    \
               on_pass: announce\n    on_fail: build\n  \
             - id: announce\n    agent: pi\n    on_pass: done\n  \
             - id: blocked\n    agent: pi\n    session: true\n";
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "default".into(),
            crate::pipeline::Pipeline::parse("default", yaml).unwrap(),
        );
        pipelines
    }

    /// What `queue route` says a resume does, then what a real resume did:
    /// the two must name the same stage. Returns that stage.
    fn route_then_resume(repo: &Repo, pipelines: &Pipelines, id: &str) -> String {
        let said = route_view(&queued(repo, id), pipelines)
            .unwrap()
            .resumes_to
            .expect("a held task names where resuming sends it");
        crate::commands::resume(
            repo,
            pipelines,
            &crate::cli::ResumeArgs {
                task: id.into(),
                stage: None,
                message: None,
            },
            None,
        )
        .unwrap();
        assert_eq!(queued(repo, id).stage(), said, "the route named {said}");
        said
    }

    #[test]
    fn route_names_where_a_held_gate_resumes() {
        let (repo, _root_guard) = fixture("route-gate");
        let pipelines = route_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.set_stage("deploy", None);
        task.front.paused_at = Some("deploy".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let text = render_route(&route_view(&queued(&repo, "ship"), &pipelines).unwrap());
        assert!(
            text.starts_with("default — held at deploy, waiting for a person\n"),
            "{text}"
        );
        assert!(text.contains("▸ deploy"), "{text}");
        assert!(
            text.contains("Ship it. A pass waits for a person."),
            "{text}"
        );
        assert!(
            text.ends_with(
                "Resuming on the board sends it to announce.\n\
                 To send it to another step, in your own shell:\n  \
                 spoolway resume ship --stage <step>\n"
            ),
            "{text}"
        );

        assert_eq!(route_then_resume(&repo, &pipelines, "ship"), "announce");
    }

    #[test]
    fn route_names_where_a_block_resumes() {
        let (repo, _root_guard) = fixture("route-block");
        let pipelines = route_pipelines();
        add(&repo, "wall", &[]);
        let mut task = queued(&repo, "wall");
        task.set_stage("deploy", None);
        task.front.blocked_from = Some("deploy".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();

        let text = render_route(&route_view(&queued(&repo, "wall"), &pipelines).unwrap());
        assert!(text.starts_with("default — blocked at deploy\n"), "{text}");
        assert!(text.contains("▸ deploy"), "{text}");

        assert_eq!(route_then_resume(&repo, &pipelines, "wall"), "deploy");
    }

    #[test]
    fn route_names_where_a_park_resumes() {
        let (repo, _root_guard) = fixture("route-park");
        let pipelines = route_pipelines();
        add(&repo, "solo", &[]);
        let mut task = queued(&repo, "solo");
        task.set_stage_unbanked("build", "test setup");
        task.save().unwrap();
        queue_pause(&repo, &pipelines, "solo", false).unwrap();

        let text = render_route(&route_view(&queued(&repo, "solo"), &pipelines).unwrap());
        assert!(text.starts_with("default — paused at build\n"), "{text}");
        assert!(text.contains("▸ build"), "{text}");

        assert_eq!(route_then_resume(&repo, &pipelines, "solo"), "build");
    }

    #[test]
    fn route_names_where_a_task_that_never_started_resumes() {
        let (repo, _root_guard) = fixture("route-never-started");
        let pipelines = route_pipelines();
        add(&repo, "fresh", &[]);
        queue_pause(&repo, &pipelines, "fresh", false).unwrap();

        let text = render_route(&route_view(&queued(&repo, "fresh"), &pipelines).unwrap());
        assert!(
            text.starts_with("default — paused before it started\n"),
            "{text}"
        );
        assert!(!text.contains('▸'), "nothing to mark: {text}");

        assert_eq!(
            route_then_resume(&repo, &pipelines, "fresh"),
            crate::pipeline::QUEUED
        );
    }

    /// A task still moving is not held: there is no resume line to give, and
    /// no `--stage` one either, which would reroute a step a lane is working.
    #[test]
    fn route_offers_no_resume_for_a_task_that_is_not_held() {
        let (repo, _root_guard) = fixture("route-running");
        let pipelines = route_pipelines();
        add(&repo, "busy", &[]);
        let mut task = queued(&repo, "busy");
        task.set_stage("build", None);
        task.save().unwrap();

        let route = route_view(&queued(&repo, "busy"), &pipelines).unwrap();
        assert_eq!(route.resumes_to, None);
        let text = render_route(&route);
        assert!(text.starts_with("default — at build\n"), "{text}");
        assert!(
            text.ends_with("It is not held, so there is nothing to resume.\n"),
            "{text}"
        );
        assert!(!text.contains("--stage"), "{text}");
    }

    /// A task on a stage its pipeline does not have is marked at no step and
    /// says so, rather than reading `at <stage>` as if it were one.
    #[test]
    fn route_says_a_stage_the_pipeline_lacks_is_not_a_step() {
        let (repo, _root_guard) = fixture("route-unknown-stage");
        let pipelines = route_pipelines();
        add(&repo, "lost", &[]);
        let mut task = queued(&repo, "lost");
        task.set_stage("nowhere", None);
        task.save().unwrap();

        let route = route_view(&queued(&repo, "lost"), &pipelines).unwrap();
        assert_eq!(route.at, None);
        let text = render_route(&route);
        assert!(
            text.starts_with("default — at `nowhere`, a step this pipeline does not have\n"),
            "{text}"
        );
    }

    /// The task's own `gate_at` holds whatever its step reports, not only a
    /// pass, and the entry says so.
    #[test]
    fn route_says_a_scheduled_gate_holds_any_outcome() {
        let (repo, _root_guard) = fixture("route-gate-at");
        let pipelines = route_pipelines();
        add(&repo, "watched", &[]);
        let mut task = queued(&repo, "watched");
        task.front.gate_at = Some("build".into());
        task.set_stage("build", None);
        task.save().unwrap();

        let text = render_route(&route_view(&queued(&repo, "watched"), &pipelines).unwrap());
        assert!(
            text.contains("Build it. Whatever it reports waits for a person."),
            "{text}"
        );
    }

    /// A blocked task parked with `p` carries `parked_from: blocked`, which is
    /// no entry here: it is marked at the step it was blocked on, while the
    /// resume line still names `blocked`, where unparking really lands it.
    #[test]
    fn route_marks_a_parked_block_at_the_step_it_blocked_on() {
        let (repo, _root_guard) = fixture("route-parked-block");
        let pipelines = route_pipelines();
        add(&repo, "wall", &[]);
        let mut task = queued(&repo, "wall");
        task.set_stage("deploy", None);
        task.front.blocked_from = Some("deploy".into());
        task.set_stage(crate::pipeline::BLOCKED, None);
        task.save().unwrap();
        queue_pause(&repo, &pipelines, "wall", false).unwrap();
        assert_eq!(
            queued(&repo, "wall").front.parked_from.as_deref(),
            Some(crate::pipeline::BLOCKED)
        );

        let route = route_view(&queued(&repo, "wall"), &pipelines).unwrap();
        assert_eq!(route.at.as_deref(), Some("deploy"));
        let text = render_route(&route);
        assert!(
            text.starts_with("default — paused while blocked at deploy\n"),
            "{text}"
        );
        assert!(text.contains("▸ deploy"), "{text}");

        assert_eq!(
            route_then_resume(&repo, &pipelines, "wall"),
            crate::pipeline::BLOCKED
        );
    }

    /// `--json` carries the same facts as the text: every step, the marked
    /// one, and where resuming goes. `blocked` is in neither.
    #[test]
    fn route_json_carries_the_same_facts_as_the_text() {
        let (repo, _root_guard) = fixture("route-json");
        let pipelines = route_pipelines();
        add(&repo, "ship", &[]);
        let mut task = queued(&repo, "ship");
        task.set_stage("deploy", None);
        task.front.paused_at = Some("deploy".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let json =
            serde_json::to_value(route_view(&queued(&repo, "ship"), &pipelines).unwrap()).unwrap();
        assert_eq!(json["pipeline"], "default");
        assert_eq!(json["state"], "held at deploy, waiting for a person");
        assert_eq!(json["at"], "deploy");
        assert_eq!(json["held"], true);
        assert_eq!(json["resumes_to"], "announce");
        let ids: Vec<&str> = json["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| step["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["build", "deploy", "announce"]);
        assert_eq!(json["steps"][1]["gate"], true);
        assert_eq!(json["steps"][1]["on_fail"], "build");
    }

    /// `impl_ui`'s steps, descriptions and routes as this command was drawn
    /// for. Written out here rather than read from `.spoolway/pipelines/`,
    /// which is the project's own control plane and is reworded far more
    /// often than this command changes. Every step runs `pi`, and `loop:`
    /// bounds each cycle, because `Pipeline::parse` refuses an unbounded one.
    const IMPL_UI: &str = "steps:
  - id: implement
    agent: pi
    description: Write the code to satisfy the task's acceptance criteria.
    on_pass: review-spec
  - id: review-spec
    agent: pi
    description: Check the change against the task — every acceptance criterion, the mockup, the non-goals.
    loop: 2
    on_pass: review-code
    on_fail: fix-spec-review
  - id: fix-spec-review
    agent: pi
    description: Clear every finding the spec review raised.
    on_pass: review-spec
  - id: review-code
    agent: pi
    description: \"Review how the change is built: correctness, design, tests and the prose in the source.\"
    loop: 2
    on_pass: look
    on_fail: fix-code-review
  - id: fix-code-review
    agent: pi
    description: Clear every finding the code review raised.
    on_pass: review-code
  - id: look
    agent: pi
    description: Open the changed screen, drive it, and read back what it actually renders.
    gate: true
    on_pass: e2e
    on_fail: implement
  - id: e2e
    agent: pi
    description: Carry this task's change into the end-to-end suites and leave them green, while the suites are still cheap to read and fix.
    loop: 2
    on_pass: test
  - id: test
    agent: pi
    description: The mechanical verdict on this change, as an exit code.
    on_pass: suite
    on_fail: e2e
  - id: suite
    agent: pi
    description: The end-to-end suites, on the last task of the chain.
    on_pass: document
    on_fail: e2e
  - id: document
    agent: pi
    description: Bring the domain documents in line with what this task changed.
    on_pass: handover
  - id: handover
    agent: pi
    description: Commit, squash, push and open this task's pull request with git and `gh` — no model, no rebase. Nothing is merged here; a person lands it.
    on_pass: done
";

    /// That pipeline held at its `look` gate: under 45 lines, no agent,
    /// model or prompt anywhere, and `look`'s own entry exactly as drawn.
    #[test]
    fn route_for_impl_ui_fits_in_45_lines() {
        let (repo, _root_guard) = fixture("route-impl-ui");
        let mut pipelines = Pipelines::builtin();
        pipelines.pipelines.insert(
            "impl_ui".into(),
            crate::pipeline::Pipeline::parse("impl_ui", IMPL_UI).unwrap(),
        );
        add(&repo, "example", &[]);
        let mut task = queued(&repo, "example");
        task.front.pipeline = Some("impl_ui".into());
        task.set_stage("look", None);
        task.front.paused_at = Some("look".into());
        task.set_stage(crate::pipeline::PAUSED, None);
        task.save().unwrap();

        let text = render_route(&route_view(&queued(&repo, "example"), &pipelines).unwrap());
        assert!(
            text.lines().count() <= 45,
            "{} lines:\n{text}",
            text.lines().count()
        );
        // `handover`'s own description says "no model", so the keys
        // `pipeline show` prints are what is looked for, not the bare words.
        for word in ["agent=", "model=", "prompt=", "claude-", "implementer"] {
            assert!(!text.contains(word), "`{word}` in:\n{text}");
        }
        assert!(
            !text
                .lines()
                .any(|line| line.trim_start().starts_with("blocked")),
            "{text}"
        );
        assert!(
            text.starts_with("impl_ui — held at look, waiting for a person\n"),
            "{text}"
        );
        assert!(
            text.contains(
                "▸ look              Open the changed screen, drive it, and read back what\n\
                 \x20                   it actually renders. A pass waits for a person.\n\
                 \x20                   pass → e2e · fail → implement\n"
            ),
            "{text}"
        );
        assert!(
            text.ends_with(
                "Resuming on the board sends it to e2e.\n\
                 To send it to another step, in your own shell:\n  \
                 spoolway resume example --stage <step>\n"
            ),
            "{text}"
        );
    }
}
