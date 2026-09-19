//! The queue and the pending documents over it: listing, adding, dependency
//! checks, and write conflicts.

use std::collections::BTreeMap;

use super::*;
use crate::platform::PathExt;
use crate::screen::{Key, PollableRead, overlay, pad_to, panel, read_key};

/// Why a task on a wait step has not started yet, worst news first.
///
/// `None` means nothing in the graph is holding it: it is simply waiting for a
/// worker slot, and the step's own description is the better thing to print.
pub(crate) fn dependency_note(graph: &Graph, id: &str) -> Option<String> {
    if let Some(cycle) = graph.cycle_with(id) {
        let others: Vec<&str> = cycle
            .iter()
            .map(String::as_str)
            .filter(|member| *member != id)
            .collect();
        return Some(match others.is_empty() {
            true => "depends on itself — nothing can ever start it".to_string(),
            false => format!("in a dependency cycle with {}", others.join(", ")),
        });
    }

    if let Some((dep, state)) = graph.unreachable(id) {
        return Some(match state {
            DepState::Unknown => format!("unreachable — no task named {dep}"),
            _ => format!("unreachable — {dep} is blocked"),
        });
    }

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
    parallel: bool,
    stage: String,
    pipeline: String,
    state: &'static str,
    /// How many times the step this task is on has sent it back, and that
    /// route's own budget — `None` wherever the step declares no `loop:` for
    /// any route out of it. Mirrors `Row::step_loop`, which the board draws
    /// inline on the STEP column as `(round n)`.
    laps: Option<u32>,
    lap_limit: Option<u32>,
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
            parallel: row.parallel,
            stage: row.stage.clone(),
            pipeline: row.pipeline.clone(),
            state: state_label(row.state),
            laps: row.step_loop.map(|(laps, _)| laps),
            lap_limit: row.step_loop.map(|(_, limit)| limit),
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
        Blocked => "blocked",
        Unreachable => "unreachable",
        Queued => "queued",
        Done => "done",
    }
}

pub fn queue_show(repo: &Repo, id: &str) -> Result<()> {
    let task = repo.task(id)?;
    print!("{}", task.render()?);
    Ok(())
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

/// Keys a task document may never set: spoolway writes every one of these
/// itself, over the task's whole life, and a document that sets one is either
/// confused about what it owns or is trying to smuggle a task onto a step, a
/// run or an attempt count that was never earned. `base` is not in this list
/// — it is a document's to set, and [`parse_submission`] keeps it when it
/// does; a submission that sets neither a document's own `base:` nor
/// `queue add --base` is refused rather than given one, since the branch a
/// checkout happens to have out is never read as a base any more.
///
/// `branch` is here because a task body is content an agent wrote, and
/// `spoolway stack` force-pushes a squashed commit onto whatever `branch:`
/// says. spoolway derives the value itself — `task/<id>`, or
/// `task/<slug>-<id>` when `issue_tracking.key_in_names` prefixes it — and
/// stamps it in [`parse_submission`]; every dependency caller then reads that
/// recorded field rather than rebuilding a shape of its own. A value starting
/// with `-` would also reach `gh pr view` as a flag — so a document may not
/// name a branch at all.
pub(crate) const RESERVED_KEYS: &[&str] = &[
    "stage",
    "run",
    "attempts",
    "base_commit",
    "cut_from",
    "trial",
    "branch",
];

pub fn queue_add(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &QueueAddArgs,
    // No longer read for a base: a document's own `base:` or `--base` is the
    // whole of where one comes from now, never the branch a checkout
    // happens to have out. Kept in the signature rather than pulled from
    // every call site — `main.rs`'s dispatch table and every test in this
    // module still pass it.
    _cwd: &std::path::Path,
    in_lane: bool,
) -> Result<()> {
    if args.from.is_empty() {
        return print_skeleton_document(repo, pipelines);
    }
    refuse_from_lane("the queue is mutated", in_lane)?;

    let base = args.base.as_deref();

    let documents = gather_documents(&args.from)?;
    if args.dry_run {
        return queue_add_dry_run(repo, pipelines, base, &documents);
    }
    queue_add_documents(repo, pipelines, base, &documents)
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
    documents: &[(String, String)],
) -> Result<()> {
    let tasks = validate_batch(repo, pipelines, base, documents)?;
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
/// `u`/`U` keys — carry a not-started task's document back to the pending
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
/// [`queue_unqueue_forced`].
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
            "\n  To stop it where it is, keeping the checkout:\n      spoolway queue pause {id}\n\
             \n  To tear the checkout down and unqueue it anyway:\n      spoolway queue unqueue {id} --force\n\
             \nNothing was changed."
        );
        bail!(message);
    }

    queue_unqueue_forced(repo, pipelines, id, &stage, checkout.as_deref())
}

/// Write `id`'s document into pending through
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
/// the document to pending — all or nothing, so a teardown this stops
/// partway through leaves the task queued and its document untouched.
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
    let mut dispatcher = crate::dispatch::Dispatcher::new(repo, pipelines, mux_ref, false);
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
    // archive the document right after (`clean_up`) or delete it outright
    // (`discard_arm`), so a stale path in memory never reaches disk there.
    // This caller carries the document on to pending instead, and every one
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
/// a document, whole, is what `--from` needs, so this prints one unfilled,
/// for a person to save, fill in and hand back through `--from`.
fn print_skeleton_document(repo: &Repo, pipelines: &Pipelines) -> Result<()> {
    print!("{}", skeleton_document(repo, pipelines)?);
    Ok(())
}

/// The text `print_skeleton_document` prints, pulled apart from the printing
/// so it can be checked without capturing standard output.
fn skeleton_document(repo: &Repo, pipelines: &Pipelines) -> Result<String> {
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
         touches: []                  # globs this task expects to modify\n\
         depends_on: []               # sibling task ids that must finish first\n\
         # base: branch-name          # the branch to cut from and merge into — required, here or with `queue add --base`\n\
         {pipeline_row}\n\
         # gate_at: step-id           # pause after that step reports, for a person to `spoolway resume`\n\
         ---\n{}",
        ends_with_newline(body),
    ))
}

/// Every document named by `--from`, in the order it names them: a directory
/// expands to its `*.md` files in filename order, `-` reads a
/// `---`-separated stream from standard input, and anything else is one
/// file. Each entry is named for the errors below — a path, or `<stdin>#N`
/// for a stream's Nth document — and holds the raw, unparsed document text.
///
/// One file is one document, always. There is no page to lift several out
/// of any more: a producer that wants to queue a breakdown writes each task
/// as its own `.md` file, and pointing `--from` at
/// [`Repo::pending_dir`] queues every one of them.
pub(crate) fn gather_documents(from: &[String]) -> Result<Vec<(String, String)>> {
    let mut documents = Vec::new();
    for source in from {
        if source == "-" {
            let stream = read_stdin()?;
            for (i, doc) in split_stream(&stream).into_iter().enumerate() {
                documents.push((format!("<stdin>#{}", i + 1), doc));
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
                gather_one(&entry, &mut documents)?;
            }
        } else {
            gather_one(path, &mut documents)?;
        }
    }
    Ok(documents)
}

/// One `--from` entry that is neither a directory nor `-`.
fn gather_one(path: &std::path::Path, documents: &mut Vec<(String, String)>) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    documents.push((path.display().to_string(), text));
    Ok(())
}

/// Split a `---`-separated stream into whole documents, each still in its own
/// `---\n<yaml>\n---\n<body>` shape.
///
/// A body may not itself contain a line that is exactly `---`: that is the
/// one thing a document trades away for a stream simple enough to split
/// blind, without parsing each document's yaml first to know where it ends.
fn split_stream(input: &str) -> Vec<String> {
    let mut documents = Vec::new();
    let mut offset = 0usize;
    loop {
        let remaining = &input[offset..];
        if remaining.trim().is_empty() {
            break;
        }

        // The third bare `---` line — the first two are this document's own
        // opening and closing fence — is the next document's opening fence,
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
                documents.push(remaining[..cut].to_string());
                offset += cut;
            }
            None => {
                documents.push(remaining.to_string());
                break;
            }
        }
    }
    documents
}

fn read_stdin() -> Result<String> {
    let mut body = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut body)
        .context("reading task documents from standard input")?;
    Ok(body)
}

/// Parse one document into a task, refusing the four keys spoolway owns and
/// stamping in the ones a document may not set at all.
///
/// Reuses [`crate::task::Frontmatter`]'s own `Deserialize` rather than a
/// parallel struct: every field a document is not meant to carry —
/// `worktree_path`, `prompts`, and the rest — lands in its typed
/// slot exactly as it would in a task already on disk, and is then
/// overwritten below the same way `queue_add` always constructed these by
/// hand. Only the [`RESERVED_KEYS`] need a check first, because those are
/// wrong to accept even long enough to overwrite.
pub(crate) fn parse_submission(name: &str, raw: &str, base: Option<&str>) -> Result<Task> {
    let (yaml, body) =
        crate::task::split_fence(raw).with_context(|| format!("{name}: not a task document"))?;

    let value: serde_norway::Value = serde_norway::from_str(yaml)
        .with_context(|| format!("{name}: frontmatter is not valid YAML"))?;
    let mapping = value
        .as_mapping()
        .with_context(|| format!("{name}: frontmatter is not a mapping"))?;

    for key in RESERVED_KEYS {
        if mapping.contains_key(*key) {
            bail!(
                "{name} sets `{key}:`, which spoolway sets on every task itself — \
                 remove it from the document"
            );
        }
    }

    // `stage` is the one field `Frontmatter` requires that a document never
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
    // strips them for a file already on disk — a submitted document copied
    // from an older task carries them just as easily.
    for key in crate::task::RETIRED_PARK_KEYS {
        front.extra.remove(*key);
    }

    if front.id.trim().is_empty() {
        bail!("{name}: a document must set `id:`");
    }
    // The squashed commit's subject, always — and the pull request's title,
    // always. No heading in the body can carry it: the body's shape is the
    // project's, and this needs to survive any template. Written
    // `feat(queue): add a --dry-run flag`, and only its presence is checked:
    // a document predating that convention still queues, rather than being
    // refused over its wording.
    if front.title.trim().is_empty() {
        bail!("{name}: a document must set `title:`");
    }
    // A lane runs in the tab its group shares with its siblings, so a task
    // with no group has nowhere to run — refused here rather than discovered
    // by a dispatcher. The one task that still reaches the queue without a
    // group is one `spoolway eval --replay` once wrote directly, outside this
    // path entirely.
    if front.group.as_deref().unwrap_or("").trim().is_empty() {
        bail!(
            "{name}: a document must set `group:` — a lane runs in the tab its group shares \
             with its siblings, so a task with no group has nowhere to run"
        );
    }
    // The only routing source a task has — there is no project default to
    // fall back to, so a document naming none has nowhere to run.
    if front.pipeline.as_deref().unwrap_or("").trim().is_empty() {
        bail!("{name}: a document must set `pipeline:` — spoolway routes a task on nothing else");
    }

    // Everything spoolway itself decides, whatever the document said —
    // exactly the fields `queue_add` always built by hand rather than trusted
    // from a caller, now reset here instead of never having been set.
    front.stage = crate::pipeline::QUEUED.to_string();
    front.borrowed = false;
    front.last_report = None;
    front.blocked_from = None;
    front.parked_from = None;
    front.escalated = false;
    front.resume = None;
    front.branch = Some(format!("task/{}", front.id));
    // A document that names its own `base:` keeps it — a task cut for a
    // branch other than the one `--base` named for the rest of the
    // submission — and `validate_batch` checks that branch is real before
    // anything is written. A document naming neither is refused by name: a
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
                     the document, or pass --base <branch>.\n\nNothing was queued."
                    )
                })?
                .to_string(),
        },
    );
    front.run = None;
    front.cut_from = None;
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
    front.prompts = Default::default();
    front.rounds = Default::default();
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

/// Parse and validate every document as one set: a `depends_on` naming a
/// sibling in the same submission is satisfied with nothing sorted first,
/// and one bad document queues nothing. Split out from
/// [`queue_add_documents`] so the queue screen can hold the validated batch
/// across its conflict gate instead of writing it the moment it parses —
/// see that screen's own `begin_submission`.
pub(crate) fn validate_batch(
    repo: &Repo,
    pipelines: &Pipelines,
    base: Option<&str>,
    documents: &[(String, String)],
) -> Result<Vec<Task>> {
    if documents.is_empty() {
        bail!("`--from` named nothing to queue");
    }

    let mut tasks = Vec::new();
    for (name, raw) in documents {
        let mut task = parse_submission(name, raw, base)?;
        // Whatever base a task ends up with — a document's own, or the
        // submission's `--base` — has to be a branch this repository really
        // has, checked here rather than only when a document's value
        // happens to differ from the flag: a `--base` is as much an
        // arbitrary value as a document's own `base:` is, and both reach a
        // task file the same way.
        let resolved = task
            .front
            .base
            .as_deref()
            .expect("parse_submission always resolves a base or refuses the document");
        check_document_base(repo, name, resolved)?;

        let pipeline_name = task
            .front
            .pipeline
            .as_deref()
            .expect("parse_submission refuses a document with no `pipeline:`");
        let pipeline = pipelines.get(pipeline_name)?;
        // A task id becomes a lane name, a branch and a file name. The lane
        // is the strictest of the three, and the only one that would fail
        // late.
        crate::mux::check_task_id(&task.front.id, longest_agent_step(pipeline))?;

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
                "`{}` is named by more than one document in this submission",
                task.id()
            );
        }

        tasks.push(task);
    }

    require_group_description(repo, &tasks)?;
    check_dependencies_set(repo, pipelines, &mut tasks)?;
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
/// carries a `group_description:` on none of its documents — the group's
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
                "group `{group}` sets no `group_description:` on any of its documents, and \
                 `issue_tracking.hook` names `{}` — the group's issue would have nothing to \
                 say. Set it on one document of the group.",
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
/// document's own bare id (see [`parse_submission`]), so the first minted
/// arm is the first number that actually tells two runs of the same
/// document apart.
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

/// The base a task ends up with — a document's own `base:`, or the
/// submission's `--base` where the document left it out — checked before
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
fn check_document_base(repo: &Repo, name: &str, base: &str) -> Result<()> {
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
    {
        bail!(
            "{name}: `base: {base}` names a branch this repository does not have locally — \
             create or fetch it first, or name one it already has"
        );
    }
    Ok(())
}

/// The `based on` line every path that queues a batch prints: one line when
/// the whole batch shares a base — the ordinary case, everything given the
/// same `--base` — and one line per task when documents named bases of their
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

/// Validate every document as one set, then write all or none — the shape
/// `queue add --from` needs, and the same all-or-nothing pass
/// [`validate_batch`] runs, right before this saves what it validated.
fn queue_add_documents(
    repo: &Repo,
    pipelines: &Pipelines,
    base: Option<&str>,
    documents: &[(String, String)],
) -> Result<()> {
    let mut tasks = validate_batch(repo, pipelines, base, documents)?;
    // `esc` from an interactive run — a real terminal on both ends of
    // `queue add --from` — is not a refusal: it means the same thing it
    // means on the queue screen, "go back", so it is caught here rather
    // than left to `?`, which would otherwise print it as an ordinary
    // error and exit 1 the way `dispatch::overrides_gate`'s own `esc`
    // never does (review finding 5).
    let task_files = readable_task_files(documents);
    if let Err(err) = open_and_prefix(
        repo,
        documents,
        &task_files,
        &mut tasks,
        crate::ask::interactive(),
        true,
    ) {
        return match err.downcast_ref::<GateCancelled>() {
            Some(_) => Ok(()),
            None => Err(err),
        };
    }

    // All or none: every document above already parsed and validated, so
    // nothing left here can fail — the writes are the commit.
    for task in &tasks {
        task.save()?;
    }

    // The same rule the queue screen's own `finish_submit` keeps: a document
    // that reached the queue is not still waiting to go there. Only a source
    // this project's own pending directory holds is removed — a `--from`
    // pointing anywhere else, including `<stdin>#N`, is read and left
    // exactly where it is, since it was never this batch's inbox copy to
    // begin with.
    remove_pending_sources(repo, documents);

    for task in &tasks {
        println!("queued {} at `{}`", task.id(), crate::pipeline::QUEUED);
        println!("  {}", task.path.display());
    }
    // Where this batch's worktrees will be cut from and where their pull
    // requests will merge back to — worth saying, because a document that
    // set its own base rather than taking `--base` (or the submission's
    // single shared one) is the exception worth seeing.
    println!("{}", based_on_note(&tasks, base.unwrap_or("")));

    // Nothing is banked here any more. Queueing used to be the one path a
    // planning session's spend had onto the ledger; an interactive session's
    // spend is not banked from any command any more, and this one was never
    // special.
    Ok(())
}

/// Delete every `--from` source document that lived in this project's own
/// [`Repo::pending_dir`], now that the whole batch is safely on disk.
///
/// `documents` names each source the way [`gather_documents`] read it —
/// a path exactly as `--from` gave it, or `<stdin>#N` for a stream entry,
/// which has no file to delete and is simply not a match below. Compared
/// through [`PathExt::comparable`] rather than by string equality, since a
/// relative `--from` path and `pending_dir`'s own absolute one otherwise
/// never look alike even when they name the same file. A file that cannot be
/// removed — already gone, or a permissions error — is left silently: this
/// runs after every task in the batch has already been written, so a failure
/// here is not a reason to call the submission itself anything but a
/// success.
fn remove_pending_sources(repo: &Repo, documents: &[(String, String)]) {
    let pending_dir = repo.pending_dir().comparable();
    for (name, _) in documents {
        let path = std::path::Path::new(name).comparable();
        if path.parent() == Some(pending_dir.as_path()) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The path of each of `documents`, in order, that is actually a file on
/// disk right now — what [`open_and_prefix`]'s own `task_files` wants for a
/// `--from` or queue-screen submission, where the document and the file a
/// hook could read are normally the same thing. The empty string stands in
/// for one that is not: a `--from -` stream entry is named `<stdin>#N` by
/// [`gather_documents`], never a real path, and handing that to a hook as
/// `SPOOLWAY_TASK_FILE` would be handing it something no `cat` can open —
/// the same failure this whole `task_files` split exists to end.
///
/// Canonicalised, not merely checked with `is_file`: a name here can be
/// relative to wherever `spoolway` itself was started (`--from ../t.md`,
/// or a `--from <dir>` whose entries [`gather_documents`] joins onto that
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
fn readable_task_files(documents: &[(String, String)]) -> Vec<String> {
    documents
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
/// --from`, the queue screen's `enter`, or a routine fired from the `r` pane
/// or by a job. Only `--from` ran it once, and with the flag on one queue
/// mixed prefixed and bare names by the route each batch had taken (jobs
/// review finding 6).
///
/// `documents` are the files a failed hook call writes the ids it already
/// opened back into, so a re-run resumes rather than opening a second set —
/// see [`write_back_ids`]. A routine hands an empty list: writing an id back
/// into `.spoolway/routines/` would consume a document meant to be queued
/// again, not once.
///
/// `task_files` is a *different* list, index-aligned with `tasks` rather
/// than `documents`, naming the real path each task's document currently
/// sits at for `SPOOLWAY_TASK_FILE` to point a hook at — never the same
/// thing as `documents` staying empty: a routine's document is never
/// written back into, but it is a real file under `.spoolway/routines/` a
/// hook can safely be handed to read, and [`queue_routine_target`] and
/// [`finish_routine`] pass it here while still passing `documents` as `&[]`.
/// An entry is the empty string when nothing backs it — a `--from -` stream
/// document read from standard input, say — rather than a path nothing can
/// open.
///
/// `interactive` is not `crate::ask::interactive()`'s own tty check —
/// callers driving the queue screen (`finish_submit`, `finish_routine`) pass
/// `true` unconditionally, since the screen already blocks on a key for
/// every other prompt it draws regardless of whether a real terminal is on
/// the other end, and `read_key` returning `None` is what ends it gracefully
/// under a script or a closed pane. Only a caller with no screen at all —
/// `queue_add_documents`, `queue_routine_target` — asks `crate::ask` whether
/// anyone is really there.
///
/// `own_terminal` is a separate question: whether the gate must take the
/// terminal for itself before it can safely block on a key. A caller with
/// no screen at all has taken no guard of its own, so it passes `true`. The
/// queue screen has already taken one for the whole of `run_screen` (see
/// `queue_screen`'s own doc comment on why that guard is dropped only once
/// the screen itself is done), and passes `false`: a second `TermGuard`
/// nested inside it would still be safe to construct, but its `Drop` runs
/// `show_cursor` and `drain_stdin` the moment this call returns, undoing the
/// outer guard's own hidden cursor and leaving the rest of the session with
/// the cursor visible again — review finding 2.
fn open_and_prefix(
    repo: &Repo,
    documents: &[(String, String)],
    task_files: &[String],
    tasks: &mut [Task],
    interactive: bool,
    own_terminal: bool,
) -> Result<()> {
    // Before `open_tickets` ever calls the hook: a declared requirement this
    // machine cannot meet means the call can only fail, and by the time it
    // does the group's epic may already exist on the forge — see
    // `tool_requirements_gate`. `true` means the gate drew and this
    // submission goes on with issue tracking switched off for it.
    if tool_requirements_gate(repo, interactive, own_terminal)? {
        return Ok(());
    }

    // Before anything is queued: a ticket opened for a task that never made
    // it into the queue — because a sibling document further down the batch
    // turned out to be broken — would be a ticket nothing ever points back
    // at.
    let group_slug = open_tickets(repo, documents, task_files, tasks)?;

    // The prefix goes on in a pass of its own, after the hook has answered:
    // the slug does not exist until `open_tickets` has run, and `branch:` was
    // already stamped and the lane name already checked by `validate_batch`.
    prefix_generated_names(tasks, &group_slug);
    Ok(())
}

/// `esc` out of [`tool_requirements_gate_with`] — bailed as an ordinary
/// `Err` so [`open_and_prefix`] stays the one place every route reaches
/// [`open_tickets`] through, and told apart at each call site from a real
/// refusal: `esc` means "go back", not "here is what went wrong". The queue
/// screen's own callers — `begin_submission`, `begin_routine_queue`,
/// `begin_routine_solo` — turn it into [`refusal_mode`]'s own `Mode::Outcome`
/// rather than a message saying something failed; a caller with no screen at
/// all — `queue_add_documents`, `queue_routine_target` — catches it and
/// exits clean, the way `dispatch::overrides_gate`'s own `esc` does.
#[derive(Debug)]
struct GateCancelled;

impl std::fmt::Display for GateCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cancelled at the tool-requirements gate")
    }
}

impl std::error::Error for GateCancelled {}

/// What an interactive screen submit's own `Err` becomes: every ordinary
/// refusal keeps the `Mode::Outcome` a person has always seen here, and
/// [`GateCancelled`] gets one too, rather than `Mode::Browsing` — the mode
/// the screen was already showing when `enter` was pressed, and so a frame
/// `draw`'s own unchanged-frame check (review finding 1) would never
/// repaint over the gate's own printed block. `Mode::Outcome` always renders
/// as a new frame, so the next draw clears it away; any key from there
/// returns to browsing exactly as every other outcome message does.
fn refusal_mode(prefix: &str, err: anyhow::Error) -> Mode {
    match err.downcast_ref::<GateCancelled>() {
        Some(_) => Mode::Outcome("nothing was queued.".to_string()),
        None => Mode::Outcome(format!("{prefix}: {err:#}")),
    }
}

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

/// The gate's own box, drawn once per unmet requirement pair — the
/// declaration line and what this machine actually has — followed by the
/// one line saying what it means. Column widths grow with the content
/// rather than staying fixed, the same way [`dispatch::print_overrides_notice`]
/// sizes its own rows, so a longer hook name or tool never runs its column
/// into the next.
fn print_tool_gate_notice(out: &mut impl std::io::Write, unmet: &[UnmetRequirement]) -> Result<()> {
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

    writeln!(out)?;
    for u in unmet {
        writeln!(
            out,
            "    {:<col1$}{:<col2$}{} >= {}",
            u.hook, "requires", u.tool, u.floor
        )?;
        match &u.found {
            Some((found, path)) => writeln!(out, "    {:<col1$}{:<col2$}{}", u.tool, found, path)?,
            None => writeln!(out, "    {:<col1$}not on PATH", u.tool)?,
        }
    }
    writeln!(out)?;
    writeln!(out, "  issue tracking is not supported.")?;
    writeln!(out)?;
    Ok(())
}

/// Whether a batch's configured hook declares a tool requirement this
/// machine does not meet, drawn before [`open_tickets`] is ever reached —
/// `true` means the gate drew and issue tracking is switched off for this
/// submission, `false` means every requirement is met (or none exist) and
/// [`open_and_prefix`] goes on exactly as it does today. The one `Err` this
/// ever returns is [`GateCancelled`], `esc`'s own signal back up to the
/// caller that drove the screen.
///
/// `interactive` is [`open_and_prefix`]'s own — not decided here, since
/// whether anyone is there to answer means something different for a
/// caller with no screen at all than for one already mid-way through
/// driving one; see that function's own doc comment. `own_terminal` is the
/// same function's own second question — whether this call must take the
/// terminal for itself before blocking on a key, or whether a caller
/// already holds one (see review finding 2).
///
/// A thin wrapper over [`tool_requirements_gate_with`], the same split
/// [`dispatch::overrides_gate`] draws around [`dispatch::overrides_gate_with`]
/// and for the same reason: this is the only thing that touches the
/// process's real stdio, so a test can drive every branch — including the
/// no-tty print-and-proceed path — against an injected reader and writer
/// instead.
fn tool_requirements_gate(repo: &Repo, interactive: bool, own_terminal: bool) -> Result<bool> {
    tool_requirements_gate_with(
        repo,
        interactive,
        &mut crate::screen::RawStdin,
        &mut std::io::stdout(),
        own_terminal.then_some(crate::platform::TermGuard::new as fn() -> _),
    )
}

/// [`tool_requirements_gate`]'s own logic. With nobody there to answer, the
/// notice still prints — the run is otherwise silent about why tracking
/// switched off — but nothing waits on a key nobody can press, the same
/// unattended path [`dispatch::overrides_gate_with`] takes for a layer
/// notice.
///
/// `term` is `None` for a caller already holding a terminal guard of its
/// own — the queue screen, for the whole of `run_screen` — and `Some` for
/// one that is not, taken only just before the first blocking read for the
/// same reason `dispatch::overrides_gate_with`'s own guard is: every early
/// return above it constructs nothing, hides nothing and shows nothing. A
/// second guard nested inside the screen's own would still be memory-safe
/// to build, but its `Drop` shows the cursor and drains stdin the moment
/// this call returns — undoing the outer guard's own hidden cursor for the
/// rest of the session (review finding 2), which is why the screen's own
/// callers pass `None` rather than a second `TermGuard::new`.
fn tool_requirements_gate_with(
    repo: &Repo,
    interactive: bool,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
    term: Option<impl FnOnce() -> crate::platform::TermGuard>,
) -> Result<bool> {
    let unmet = unmet_requirements(repo);
    if unmet.is_empty() {
        return Ok(false);
    }

    print_tool_gate_notice(out, &unmet)?;
    if !interactive {
        return Ok(true);
    }
    writeln!(
        out,
        "  [enter] queue anyway, without issue tracking   [esc] back"
    )?;

    let _term = term.map(|term| term());
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
/// generated name is byte-for-byte what it is today.
///
/// Runs after [`open_tickets`] and before any task is saved. The `task/` ref
/// namespace is kept, so `spoolway stack` and the orphaned-branch sweep still
/// find these branches where they look today; the worktree directory follows
/// the branch and so picks the prefix up on its own — see
/// [`crate::mux::branch_slug`].
fn prefix_generated_names(tasks: &mut [Task], group_slug: &BTreeMap<String, String>) {
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
        let prefixed_group = format!("{slug}-{group}");
        task.front.branch = Some(format!("task/{slug}-{}", task.front.id));
        task.front.group = Some(prefixed_group.clone());
        if announced.insert((slug.clone(), prefixed_group.clone())) {
            println!("issue_tracking: names prefixed `{slug}` — group `{prefixed_group}`\n");
        }
    }
}

/// Call the `[issue_tracking]` open hook once for every document in the
/// batch that does not already name a `ticket:`, before any of them is
/// queued — a no-op start to finish when no hook is configured at all.
///
/// Walks the batch in dependency order, so a task's own call always has its
/// dependencies' ticket ids already in hand for `SPOOLWAY_DEPENDS_TICKETS`,
/// and tracks one epic id per `group:` — the first non-empty `epic=` a
/// hook answers with, or whatever a document already names, whichever this
/// batch reaches first — so a group opens at most one epic across however
/// many of its tasks actually call the hook.
///
/// A failing call bails out — nothing in this batch is queued — but not
/// before every id already answered is written back into the document it
/// came from, in place on disk: see [`write_back_ids`], which is what makes
/// re-running the same `queue add` resume rather than open a second set.
///
/// Returns the slug decided for each `group:` this batch touches — one per
/// group, the first non-blank `slug=` a hook answers, seeded from tasks
/// already in the queue as well as this batch. Empty unless
/// `issue_tracking.key_in_names` is on and a hook actually answered a slug;
/// [`prefix_generated_names`] is what applies it.
///
/// `task_files` is index-aligned with `tasks`, not `documents` — see
/// [`open_and_prefix`]'s own doc comment on why the two lists differ — and
/// is what `SPOOLWAY_TASK_FILE` is resolved from below.
fn open_tickets(
    repo: &Repo,
    documents: &[(String, String)],
    task_files: &[String],
    tasks: &mut [Task],
) -> Result<BTreeMap<String, String>> {
    // `group:` on every task in this batch is still the bare name a document
    // wrote — `validate_batch` never prefixes it — so every map here is keyed
    // by the bare group.
    let mut group_slug: BTreeMap<String, String> = BTreeMap::new();
    let key_in_names = repo.config.issue_tracking.key_in_names;

    if !crate::tracking::configured(repo) {
        return Ok(group_slug);
    }

    let mut group_size: BTreeMap<String, usize> = BTreeMap::new();
    // The group's own words for the issue this batch is about to open —
    // whichever document of the group set `group_description:` first, in
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
    // in every document they ever cut, and the second `queue add` still finds
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
    // And from this batch: a document already carrying `slug:` — one reported
    // `kept`, or one a prior failed run wrote back — pins its group's slug
    // the same way a queued sibling does, so a hook answering a different
    // slug on the re-run cannot displace the first non-blank answer. This
    // document *is* this command's input, so a bad `slug:` here is reported —
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
                println!(
                    "  issue_tracking: slug `{slug}` on `{}` is not a valid name \
                     (lowercase letters, digits and hyphens) — ignored",
                    task.id()
                );
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
            println!("issue_tracking: opening tickets for group `{group}`\n");
        }

        let already = tasks[i].extra_str("ticket").to_string();
        if !already.is_empty() {
            let epic = tasks[i].extra_str("epic").to_string();
            if !epic.is_empty() {
                group_epic.entry(group.clone()).or_insert(epic);
            }
            println!(
                "  {:<8} {:<9} {:<13} {}",
                "ticket",
                "kept",
                already,
                tasks[i].id()
            );
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
        // The path this document actually sits at right now — the caller's
        // to resolve, and never `tasks[i].path`: that names where
        // `validate_batch` intends to save the task, a file that does not
        // exist until every document in this batch has opened its ticket.
        let task_file = task_files.get(i).cloned().unwrap_or_default();

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
                    println!("  {:<8} {:<9} {:<13} {}", "epic", "created", epic, group);
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
                println!(
                    "  {:<8} {:<9} {:<13} {}",
                    "ticket",
                    "created",
                    ticket,
                    tasks[i].id()
                );
            }
            crate::tracking::OpenResult::Failed { exit_code } => {
                println!(
                    "  {:<8} {:<9} {:<13} {}",
                    "ticket",
                    "FAILED",
                    "—",
                    tasks[i].id()
                );
                // The group's winning slug onto every task first, so the
                // ids written back carry the dependency-order decision — not
                // a later task's own raw answer, which document order would
                // otherwise let win the re-run.
                stamp_group_slugs(tasks, &group_slug);
                let written_back = write_back_ids(documents, tasks)?;
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
                        "not written — no document on disk to record them in; close those \
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
    println!();
    Ok(group_slug)
}

/// The bare `group:` name, with a recognised `<slug>-` prefix removed. A
/// queued sibling carries the prefix its own `queue add` applied; comparing
/// against the bare name a fresh document writes is what lets the epic and
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

/// Whether a slug a hook or a document offered is one spoolway will build a
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

/// The ticket id `dep` already carries — a sibling document in this same
/// batch, already processed by the time `open_order` reaches whatever
/// depends on it, or a task already queued or archived from an earlier
/// call. Errors only if `dep` names neither, which `validate_batch`'s own
/// `check_dependencies_set` has already ruled out for every document that
/// reaches here.
fn dependency_ticket(repo: &Repo, tasks: &[Task], dep: &str) -> Result<String> {
    if let Some(task) = tasks.iter().find(|t| t.id() == dep) {
        return Ok(task.extra_str("ticket").to_string());
    }
    let existing = crate::task::find(&[&repo.queue_dir(), &repo.archive_dir()], dep)
        .with_context(|| format!("looking up `{dep}`'s own ticket"))?;
    Ok(existing.extra_str("ticket").to_string())
}

/// Write every value `open_tickets` already secured back into the document it
/// came from, in place on disk — called only once a hook call has failed,
/// so a re-run of the same `queue add` sees those documents already carrying
/// `epic:`/`ticket:`/`slug:`/`url:` and does not undo the first call's work:
/// a `ticket:` reports the document `kept` and skips the hook, and a `slug:`
/// pins the group's prefix so the re-run's hook cannot answer a different
/// one.
///
/// `documents` and `tasks` are index-aligned: `validate_batch` parses one
/// [`Task`] per document, in the order `documents` names them, and never
/// reorders that top-level list — only a task's own `depends_on` is ever
/// reordered. A document read from `-` (standard input) has no file to write
/// back to and is silently skipped; there is nowhere on disk for its answer
/// to resume from anyway. Returns whether any document was written at all,
/// so the failure message can say where the ids went — or that they went
/// nowhere.
fn write_back_ids(documents: &[(String, String)], tasks: &[Task]) -> Result<bool> {
    let mut written = false;
    for (task, (name, raw)) in tasks.iter().zip(documents.iter()) {
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
/// Run over the whole submission at once, not one document at a time: a
/// `depends_on` naming a sibling submitted alongside it has to resolve
/// against that sibling, which is not in `repo.tasks()` until every document
/// in this batch has already passed. Every failure this catches is silent
/// otherwise: the task sits on the wait step for as long as anyone leaves it
/// there, looking like ordinary queued work, or its worktree quietly misses
/// a parent's commits. Rejecting them here is what keeps the graph acyclic
/// and fully resolved by construction — `spoolway doctor` is the backstop
/// for hand-edited files.
fn check_dependencies_set(repo: &Repo, pipelines: &Pipelines, batch: &mut [Task]) -> Result<()> {
    let mut tasks = repo.tasks()?;
    tasks.extend(batch.iter().cloned());
    let graph = Graph::build(&tasks, pipelines, &repo.archive_dir());

    for task in batch.iter_mut() {
        let id = task.id().to_string();

        if task.front.depends_on.iter().any(|dep| dep == &id) {
            bail!("`{id}` cannot depend on itself");
        }

        for dep in &task.front.depends_on {
            if graph.state(dep) == DepState::Unknown {
                let days = repo.config.housekeeping.retention_days;
                if days > 0 {
                    // `retain` deletes an `archive/` entry once it is this
                    // old, and a dependency this refused could just as
                    // easily be a typo — so this names the age rather than
                    // claiming it, and still points at fixing the id.
                    bail!(
                        "`{id}` depends on `{dep}`, which is in neither the queue nor the \
                         archive — if `{dep}` finished more than {days} day(s) ago, \
                         `housekeeping.retention_days` has already swept it out of the \
                         archive; otherwise check the id, or queue that task first"
                    );
                }
                bail!(
                    "`{id}` depends on `{dep}`, which is in neither the queue nor the \
                     archive — check the id, or queue that task first"
                );
            }

            // With no collision walk left to invent an edge from an
            // overlapping `touches` glob (see `chains-not-fans`), every edge
            // left comes from a plan page, and a plan cuts one group at a
            // time. A dependency naming another group is always a mistake,
            // and queue time is the cheapest place to say so.
            //
            // Compared bare: a queued sibling's `group:` already carries the
            // `<slug>-` prefix its own `queue add` applied when
            // `issue_tracking.key_in_names` is on, while this document still
            // reads what the person wrote — `open_tickets` runs after this
            // check. The same strip `open_tickets` uses for its epic lookup,
            // so a group that spans two `queue add` calls chains the way it
            // was meant to.
            let sibling = tasks.iter().find(|t| t.id() == dep);
            let (mine, theirs) = (
                task.front.group.as_deref(),
                sibling.and_then(|t| t.front.group.as_deref()),
            );
            if let (Some(mine), Some(theirs)) = (mine, theirs)
                && mine != theirs
                && (!repo.config.issue_tracking.key_in_names
                    || strip_slug_prefix(
                        theirs,
                        sibling
                            .map(|t| t.extra_str("slug"))
                            .filter(|slug| accept_slug(slug))
                            .unwrap_or(""),
                    ) != mine)
            {
                bail!(
                    "`{id}` is in group `{mine}` but depends on `{dep}`, which is in group \
                     `{theirs}` — a chain does not cross a group. Queue one group, let it \
                     land, then queue the other."
                );
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
                        "`{id}` depends on `{parents}`, but none of them reaches all the \
                         others — its worktree would be cut from `{first}`, and `{missing}` \
                         would not be in it. A depends_on must start with the id that \
                         contains the rest."
                    );
                }
            }
        }
    }

    Ok(())
}

/// Two tasks that can write the same file, and whether anything orders them.
struct Conflict<'a> {
    a: &'a str,
    b: &'a str,
    /// The overlapping globs, as `theirs` or `mine ~ theirs`.
    shared: Vec<String>,
    ordered: bool,
    /// Whether the two merge into the same branch. Two groups' tasks do not, and
    /// nothing in the queue can order them.
    same_base: bool,
    /// Whether both tasks are `parallel: true` — an overlap declared on
    /// purpose, not a missing edge. Only meaningful within one group: two
    /// tasks each declared parallel *of their own* group say nothing about
    /// each other.
    declared_parallel: bool,
}

/// Report tasks whose `touches` globs overlap, since two lanes editing the same
/// files will conflict at merge time.
///
/// This is what tells a planner where a `depends_on` is needed, so both halves
/// of the question have to be answered properly: whether two globs can name the
/// same file, and whether the dependency graph already keeps the two tasks
/// apart — however many hops apart they are.
fn conflicts<'a>(tasks: &'a [Task], graph: &Graph) -> Vec<Conflict<'a>> {
    let mut found = Vec::new();

    for (i, a) in tasks.iter().enumerate() {
        for b in tasks.iter().skip(i + 1) {
            // `src/**` and `src/api/**` are not the same string and do name the
            // same files, which is exactly the case worth catching.
            let shared: Vec<String> = a
                .front
                .touches
                .iter()
                .filter_map(|glob| {
                    let other = b
                        .front
                        .touches
                        .iter()
                        .find(|other| crate::globs::overlaps(glob, other))?;
                    Some(match glob == other {
                        true => glob.clone(),
                        false => format!("{glob} ~ {other}"),
                    })
                })
                .collect();

            if shared.is_empty() {
                continue;
            }

            found.push(Conflict {
                a: a.id(),
                b: b.id(),
                // A task that waits on the other, however indirectly, never
                // runs beside it — so the overlap cannot bite.
                ordered: graph.reaches(a.id(), b.id()) || graph.reaches(b.id(), a.id()),
                same_base: a.front.base == b.front.base,
                // Both sides said so, of the same group — the siblings a
                // `parallel: true` names are the other declared-parallel
                // tasks of that same `group:`, never a task of another one.
                declared_parallel: a.front.parallel
                    && b.front.parallel
                    && a.front.group.is_some()
                    && a.front.group == b.front.group,
                shared,
            });
        }
    }

    found
}

/// The report `spoolway queue conflicts` prints: every overlapping pair the
/// queue already holds, worded the same way `conflicts` always has. The
/// screen no longer runs any check of its own before writing — see
/// `begin_submission` — so this is the only place left that reads `conflicts`
/// at all.
fn conflicts_report(repo: &Repo, pipelines: &Pipelines) -> Result<String> {
    let tasks = repo.tasks()?;
    let graph = Graph::build(&tasks, pipelines, &repo.archive_dir());
    let found = conflicts(&tasks, &graph);

    if found.is_empty() {
        return Ok("No overlapping `touches` globs among queued tasks.".to_string());
    }

    let mut lines = Vec::with_capacity(found.len());
    for conflict in &found {
        // Two groups' tasks are on branches of their own and merge separately,
        // so nothing here orders them and `depends_on` may not: it is refused
        // across bases, because work only merges into the base it was cut from.
        // The overlap is still real — it comes due when both reach main.
        //
        // A pair both marked `parallel: true` is read as a mistake in the
        // group rather than a missing edge: the two tasks said, on purpose,
        // that they mean to run beside each other — this overlap is what
        // that choice costs, not something nobody noticed.
        let note = match (
            conflict.same_base,
            conflict.ordered,
            conflict.declared_parallel,
        ) {
            (false, _, _) => "on different group branches — they meet at main, not here",
            (true, true, _) => "ordered by depends_on",
            (true, false, true) => {
                "both declared parallel — a mistake in the group, not a missing edge"
            }
            (true, false, false) => "NOT ordered — add a depends_on or merge the tasks",
        };
        lines.push(format!(
            "{} and {} both touch {} ({note})",
            conflict.a,
            conflict.b,
            conflict.shared.join(", ")
        ));
    }
    Ok(lines.join("\n"))
}

pub fn queue_conflicts(repo: &Repo, pipelines: &Pipelines) -> Result<()> {
    println!("{}", conflicts_report(repo, pipelines)?);
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
    if !running.is_empty() {
        if !force {
            bail!(
                "`{id}` is running a command step (`{}`) — pass `--force` to stop it and \
                 pause, or use the board's `p` key to choose interactively",
                running[0].step
            );
        }
        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        for cr in &running {
            runs.stop(&crate::command_step::Runs::key(&cr.step, &cr.task));
        }
    }

    crate::status::park(&mut tasks[idx], "paused via `spoolway queue pause`", false);
    tasks[idx].save()?;
    println!("paused `{id}`");
    Ok(())
}

/// `spoolway queue resume <id>`: what the board's `r` key does to the row for
/// `id` — exactly [`crate::status::resume_task`], the body a keypress and
/// `spoolway resume <id>` already share.
pub fn queue_resume(repo: &Repo, pipelines: &Pipelines, id: &str) -> Result<()> {
    // `resume_task` is silent about a task the queue no longer has — right
    // for a keypress racing a second process, wrong for a script that named
    // one by hand, so that case is named here instead.
    repo.task(id)
        .with_context(|| format!("no queued task `{id}`"))?;
    crate::status::resume_task(repo, pipelines, id)?;
    println!("resumed `{id}`");
    Ok(())
}

// ----------------------------------------------------------------- the screen
//
// Bare `spoolway queue` opens this rather than listing anything: the queue's
// own subcommands are unchanged, but where the work actually enters the
// queue used to be an agent skill typing `queue add --from` on a human's say-
// so is now this screen, reading the same task documents any producer writes
// into the pending directory and submitting through the very same `--from`
// path below. Nothing here decides a split or a pipeline — that stayed with
// whoever wrote the documents; the screen's whole job is picking which
// already-written groups go, and when.
//
// spoolway does not know what a plan is any more. A group is a `group:`
// string a set of documents share, and that is the only structure this reads:
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

use super::pending::{Group, GroupState, PendingTask, TaskState};
use super::routines::{RoutineFolder, RoutineTask};

/// Which pane a `Char(' ')` or an arrow acts on.
///
/// `pub(super)` because the `spoolway jobs` screen reuses this module's
/// routines browser whole to pick a job's target — see
/// [`super::jobs::jobs_screen`].
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
    /// Forking a whole group into one arm per task — `p`'s own two screens,
    /// assigning a pipeline to every task and then choosing what each one
    /// skips. The whole of what was picked lives on the [`TrialState`] it
    /// carries, not split across `ScreenState` fields the way `gates` is: a
    /// trial never touches the outer selection, and `esc` off the first
    /// screen dropping this mode is what "leaves the selection exactly as
    /// it was" means.
    Trial(TrialState),
    /// What validating or writing a submission came back with, held on
    /// screen until the next key press dismisses it. This exists because
    /// `draw`'s first act is always to clear the screen — a message written
    /// straight to `out` and then let the loop redraw over would never be
    /// read at all.
    Outcome(String),
    /// A submission that landed: its report, and the offer to start a
    /// dispatcher over what was just queued. Kept apart from [`Mode::Outcome`]
    /// so a refusal can never reach the offer — a refusal queued nothing, and
    /// there is nothing for a dispatcher to pick up.
    Dispatch(String),
    /// `r`'s own screen: the left pane swapped for the folder tree under
    /// `.spoolway/routines/` — see [`RoutineNav`] for what it tracks between
    /// keys. The folders and documents themselves live in `run_screen`'s own
    /// `routines`, read fresh every time this mode is entered, the same way
    /// `groups` is read once up front rather than carried on the mode.
    Routines(RoutineNav),
    /// `s`'s own panel, over the pending screen: saving the named group's
    /// documents into `.spoolway/routines/<name>/`, `name` typed and edited
    /// the same way `Mode::Filter`'s query is. `group` is fixed at the
    /// moment `s` was pressed, so moving the cursor underneath this panel
    /// — which nothing here lets happen, but the field says so regardless —
    /// could never save the wrong group's documents.
    SaveRoutine {
        group: GroupKey,
        name: String,
    },
}

/// How the screen ended, and the one thing it cannot do for itself.
///
/// [`run_screen`] is what learns a person wants a dispatcher, but
/// [`queue_screen`] is what holds the [`TermGuard`]: raw mode and a hidden
/// cursor would fight the dispatch board's own guard and the `ctrl-c` handler
/// it installs. So the answer leaves the loop as data, the guard is dropped,
/// and only then does `commands::dispatch` take the terminal.
///
/// [`TermGuard`]: crate::platform::TermGuard
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScreenExit {
    Quit,
    StartDispatcher,
}

/// A task, addressed by the path of the document it is written in — exactly
/// the name [`gather_one`] gives that same document, so the key a person's
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

/// Whether a group can be submitted at all: it has to have documents, and at
/// least one of them must still be waiting in the pending directory — see
/// [`GroupState`], which is what `h`'s first widening hides on.
fn selectable(group: &Group) -> bool {
    group.state == GroupState::Queueable && !group.tasks.is_empty()
}

/// [`Mode::Routines`]'s own state: the breadcrumb of folder names browsed
/// into so far, which pane has focus, where each pane's own cursor sits, and
/// which folders at the current level are ticked for `enter` to queue.
///
/// The folder tree itself is not here — it lives in `run_screen`'s own
/// `routines`, read fresh every time `r` opens this mode, the same read
/// [`super::routines::list_routines`] gives the empty-directory case no
/// error over.
///
/// `pub(super)` along with its fields so the `spoolway jobs` screen can drive
/// the same browser and then read back which folder or document was picked.
#[derive(Debug, Clone)]
pub(super) struct RoutineNav {
    pub(super) path: Vec<String>,
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
            path: Vec::new(),
            focus: Focus::Groups,
            folder_cursor: 0,
            task_cursor: 0,
            selected: Default::default(),
        }
    }
}

/// [`Mode::Trial`]'s two screens, in the order `p` walks a person through
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrialStage {
    /// Assigning one pipeline to every task in the group — `←`/`→` cycle the
    /// one under the cursor through [`trial_pipeline_names`].
    AssignPipelines,
    /// Choosing which steps of each task's own assigned pipeline to skip.
    ChooseSkips,
}

/// [`Mode::Trial`]'s own state: the group `p` was pressed over, fixed for as
/// long as this mode stays open — the same reason [`Mode::SaveRoutine`]
/// fixes its own group rather than reading `state.group_cursor` live, since
/// nothing here lets the cursor move underneath this panel but the field
/// says so regardless — which of its two screens is showing, and the
/// picks made so far on each: every task's own assigned pipeline, and, once
/// a pipeline is assigned, whatever steps of it are ticked to skip. Both
/// maps are keyed by [`task_key`] rather than position, so a reload that
/// reorders `group.tasks` out from under this mode can never point a pick at
/// the wrong task.
#[derive(Debug, Clone)]
struct TrialState {
    group: GroupKey,
    stage: TrialStage,
    /// The cursor's own meaning changes with `stage`: a task index while
    /// assigning pipelines, a flattened index across every task's own
    /// assigned pipeline's steps while choosing skips — see
    /// [`trial_total_steps`].
    cursor: usize,
    pipeline: std::collections::BTreeMap<TaskKey, String>,
    skip: std::collections::BTreeMap<TaskKey, std::collections::BTreeSet<String>>,
}

impl TrialState {
    /// Opened by `p`: every task in `group` starts out assigned its own
    /// document's `pipeline:` when it names one. There is no project default
    /// to fall back to any more, so a legacy or hand-edited document naming
    /// none opens unassigned instead — `←`/`→` on [`TrialStage::AssignPipelines`]
    /// is what has to give it one before `enter` can advance, rather than the
    /// picker silently choosing for it.
    fn new(_pipelines: &Pipelines, group: &Group) -> TrialState {
        let pipeline = group
            .tasks
            .iter()
            .filter_map(|task| doc_pipeline_name(&task.doc).map(|name| (task_key(task), name)))
            .collect();
        TrialState {
            group: group_key(group),
            stage: TrialStage::AssignPipelines,
            cursor: 0,
            pipeline,
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

/// Whether every task in `group` has a pipeline assignment — what
/// [`TrialStage::AssignPipelines`]'s own `enter` requires before advancing,
/// since [`begin_trial`] has no project default left to hand an unassigned
/// task instead.
fn trial_fully_assigned(group: &Group, trial: &TrialState) -> bool {
    group
        .tasks
        .iter()
        .all(|task| trial.pipeline.contains_key(&task_key(task)))
}

/// `h`'s own three-way state: how far the left pane has widened past the
/// opening, queueable-only view. [`HideScope::next`] is the cycle `h` steps
/// through, one widening at a time, wrapping back to [`HideScope::Pending`]
/// once every group is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HideScope {
    /// The opening state, unchanged: only groups something is still
    /// queueable in.
    Pending,
    /// Plus every group the queue holds in full.
    PlusQueued,
    /// Plus every group the archive holds in full — everything there is.
    PlusDone,
}

impl HideScope {
    /// The widening one more `h` press moves to, wrapping from the widest
    /// scope back to the narrowest rather than getting stuck there.
    fn next(self) -> HideScope {
        match self {
            HideScope::Pending => HideScope::PlusQueued,
            HideScope::PlusQueued => HideScope::PlusDone,
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
    /// A gate chosen from the screen, kept apart from the document itself —
    /// see `with_gate` for why the file on disk is never rewritten to record
    /// one.
    gates: std::collections::BTreeMap<TaskKey, String>,
}

impl ScreenState {
    fn new() -> ScreenState {
        ScreenState {
            focus: Focus::Groups,
            mode: Mode::Browsing,
            // A group the queue or the archive already holds is one that
            // can only be scrolled past — see `selectable` — so the screen
            // opens on the groups a person can actually act on, with the
            // rest a key or two away rather than crowding the list from the
            // first frame.
            hide_scope: HideScope::Pending,
            filter: String::new(),
            group_cursor: 0,
            task_cursor: 0,
            selected: Default::default(),
            gates: Default::default(),
        }
    }
}

/// The groups currently on screen at this [`HideScope`] — `h`'s whole
/// effect, computed fresh each frame rather than stored, so nothing has to
/// be reconciled when a group leaves the list entirely.
fn visible(groups: &[Group], scope: HideScope) -> Vec<&Group> {
    groups
        .iter()
        .filter(|group| match scope {
            HideScope::Pending => group.state == GroupState::Queueable,
            HideScope::PlusQueued => group.state != GroupState::Done,
            HideScope::PlusDone => true,
        })
        .collect()
}

/// The groups currently on screen, in the order they are drawn: `h`'s
/// ordinary hide/show list when there is no filter, or every group that
/// clears [`group_score`] against the filter's own query, ranked best score
/// first, when there is — reaching a group `h` is hiding, since that is the
/// one acceptance criterion `visible` alone could never satisfy. A tie in
/// score falls back to name order, the same tie-break
/// [`super::pending::list_groups`] itself uses, so the list does not
/// reshuffle between two draws that score identically.
fn shown<'a>(groups: &'a [Group], state: &ScreenState) -> Vec<&'a Group> {
    if state.filter.is_empty() {
        return visible(groups, state.hide_scope);
    }
    let mut ranked: Vec<(i64, &Group)> = groups
        .iter()
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

/// What `queue_screen` prints instead of opening — or `None` when there is
/// nothing wrong with the pending directory worth saying first.
///
/// An empty pending directory is not one of those things. The screen opens
/// on it like any other, and its own empty left pane says there is nothing
/// to queue. It used to print a message here naming `/spoolway-plan` as the
/// way to fill the directory, which made a bundled sample skill look like a
/// dependency of the binary; documents reach that directory from anywhere,
/// and this screen has no business naming one writer of them.
///
/// The unreadable-documents check is gated on no group being
/// [`super::pending::GroupState::Queueable`] any more — true both when
/// `groups` is empty outright and when it holds only rows nobody can select
/// — rather than on `groups.is_empty()` alone. `groups` now also reflects
/// the queue and archive directories (see [`super::pending::list_groups`]),
/// so a group already queued or archived can leave it non-empty even when
/// every document actually sitting in the pending directory is unreadable;
/// gating on emptiness alone would either hide the diagnostic behind that
/// unrelated row, or — the fix that overcorrected the first time — hide a
/// perfectly queueable group behind a stray document that has nothing to do
/// with it. `all` on an empty slice is `true`, so this one condition covers
/// the ordinary empty-pending case together with both the queue-only and
/// archive-only ones.
///
/// Pulled out of [`queue_screen`] so this can be checked without a real
/// terminal or a captured stdout — the same split `skeleton_document` makes
/// from `print_skeleton_document`.
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
                 A row on this screen is a `group:` value, so a document without one has no row \
                 to be. Run `spoolway task contract --from {}` for the real reason on each.",
                dir.display(),
                plural(skipped.len(), "document"),
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

/// Open the queue screen: list every group waiting in the pending directory
/// and the queue directory, let a person choose which to queue and where to
/// gate their tasks, and submit the chosen documents through
/// [`validate_batch`] — the same validation `queue add --from` runs — with
/// nothing drawn between `enter` and the write.
///
/// Degrades rather than crashes with no terminal to drive: [`TermGuard`]
/// only changes stdin's mode on a real tty, so a redirected or piped stdin
/// is read exactly as written, and running out of it — `read_key` returning
/// `None` — ends the screen the same way `q` does rather than blocking on a
/// read that will never come. That is also what lets the end-to-end suite
/// drive the submit path headlessly: a script's `printf '...' | spoolway
/// queue` plays a key sequence in and the screen ends the moment the pipe is
/// empty.
///
/// [`TermGuard`]: crate::platform::TermGuard
pub fn queue_screen(repo: &Repo, pipelines: &Pipelines, cwd: &std::path::Path) -> Result<()> {
    let groups = super::pending::list_groups(repo)?;
    if let Some(msg) = opening_message(repo, &groups) {
        println!("{msg}");
        return Ok(());
    }
    // Read once here rather than lazily on the first `r` — the same
    // up-front read `groups` gets, and cheap for the same reason: a
    // routines tree is at most a handful of small documents.
    let routines = super::routines::list_routines(repo)?;

    let mut stdin = crate::screen::RawStdin;
    let mut stdout = std::io::stdout();

    let exit = {
        // Scoped so the guard is dropped before anything else can want the
        // terminal: `commands::dispatch` installs a board with a guard of its
        // own and a `ctrl-c` handler, and a second raw mode still in force
        // under it is a cursor that never comes back. `_term` is a real
        // binding rather than a `_` wildcard so it lives to the closing brace,
        // and its own drop drains stdin before restoring the mode — nothing
        // typed at the screen leaks into the dispatcher's first read.
        let _term = crate::platform::TermGuard::new();
        run_screen(
            repo,
            pipelines,
            cwd,
            groups,
            routines,
            &mut stdin,
            &mut stdout,
        )?
    };

    match exit {
        ScreenExit::Quit => Ok(()),
        // The exit code `dispatch` returns is for a process about to end —
        // this screen's own caller keeps running afterwards, so there is no
        // process exit to carry it into.
        ScreenExit::StartDispatcher => {
            super::dispatch(repo, pipelines, &crate::cli::DispatchArgs::default()).map(|_| ())
        }
    }
}

fn run_screen(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &std::path::Path,
    mut groups: Vec<Group>,
    mut routines: Vec<RoutineFolder>,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<ScreenExit> {
    let mut state = ScreenState::new();
    let mut last = None;
    let routines_dir = repo.routines_dir();

    loop {
        let panes = Panes {
            routines: &routines,
            routines_dir: &routines_dir,
            pipelines,
        };
        draw(&groups, &panes, &state, &mut last, out);
        let Some(key) = wait_for_key(repo, &mut groups, &panes, &mut state, &mut last, input, out)
        else {
            // No terminal, or the script driving this run has finished
            // handing over keys: the same graceful stop `q` gives, since
            // nothing typed here can ever be a command a shell prompt would
            // misread — TermGuard's own drop drains whatever is left.
            break;
        };

        // `q` used to be intercepted here, once, for every mode at once —
        // safe only as long as no mode read a literal character of its own.
        // `Mode::Filter` is exactly that mode: `q` has to reach it as
        // ordinary text, the same as any other letter. So `q` is now each
        // mode's own business: it still quits from every one of them —
        // browsing, the gate picker and a report still on screen — except
        // `Mode::Filter`, which is the one mode this whole change exists to
        // let read `q` as a letter.
        match &state.mode {
            // `p` and `s` are reserved actions even while the filter box has
            // focus — the same live start `Mode::Browsing`'s own arms give
            // them below — so narrowing the list is never the only way to
            // reach either. Every other character, `q` included, still
            // narrows the query; see `handle_filter_key`'s own doc comment.
            Mode::Filter => match key {
                Key::Char('p') => {
                    if let Some(group) = trial_target(&groups, &state) {
                        state.mode = Mode::Trial(TrialState::new(pipelines, group));
                    }
                }
                Key::Char('s') => {
                    if let Some(group) = trial_target(&groups, &state) {
                        state.mode = Mode::SaveRoutine {
                            group: group_key(group),
                            name: group.name.clone(),
                        };
                    }
                }
                _ => handle_filter_key(&groups, &mut state, key),
            },
            Mode::Gate(cursor) => match key {
                Key::Char('q') => break,
                _ => {
                    let cursor = *cursor;
                    handle_gate_key(&groups, pipelines, &mut state, cursor, key);
                }
            },
            Mode::Trial(trial) => match key {
                Key::Char('q') => break,
                // The first screen's own `enter`: advance to the second
                // rather than launch anything — but only once every task has
                // an assignment. A legacy or hand-edited document names none
                // (see `TrialState::new`), and `begin_trial` has no default
                // left to fall back to, so a task still unassigned here would
                // otherwise be silently dropped from the batch rather than
                // queued.
                Key::Enter
                    if trial.stage == TrialStage::AssignPipelines
                        && trial_group(&groups, trial)
                            .is_some_and(|group| trial_fully_assigned(group, trial)) =>
                {
                    let mut trial = trial.clone();
                    trial.stage = TrialStage::ChooseSkips;
                    trial.cursor = 0;
                    state.mode = Mode::Trial(trial);
                }
                // The second screen's own `enter`: mint and write the batch.
                // Guarded on `ChooseSkips` rather than a bare fallthrough —
                // an `enter` on the first screen with a task still
                // unassigned must fall to the catch-all below instead
                // (a no-op there), not reach `begin_trial`, which has
                // nothing left to hand an unassigned task and would panic
                // on the `.expect()` that assumes this screen already
                // refused to let it through.
                Key::Enter if trial.stage == TrialStage::ChooseSkips => {
                    let trial = trial.clone();
                    let base = crate::repo::branch_at(cwd)?;
                    state.mode = begin_trial(repo, pipelines, &base, &groups, &trial);
                }
                _ => {
                    let trial = trial.clone();
                    state.mode = match trial_group(&groups, &trial) {
                        Some(group) => handle_trial_key(group, pipelines, trial, key),
                        None => Mode::Browsing,
                    };
                }
            },
            Mode::Outcome(_) => match key {
                Key::Char('q') => break,
                // Any other key dismisses it. The message was already read
                // on the draw that preceded this key — holding it in
                // `state` rather than writing it straight to `out` is what
                // let it survive that draw at all.
                _ => state.mode = Mode::Browsing,
            },
            Mode::Dispatch(_) => match key {
                // Only `y`. Every other key declines, because a stray `\r` an
                // unattended script's input happens to carry must never start
                // a dispatcher — the same care `q` alone quitting is taken
                // with.
                Key::Char('y') => return Ok(ScreenExit::StartDispatcher),
                Key::Char('q') => break,
                _ => state.mode = Mode::Browsing,
            },
            Mode::SaveRoutine { group, name } => match key {
                // No `q` arm: the name field reads every ordinary character
                // it is typed, `q` included, the same way `Mode::Filter`'s
                // query does — see that mode's own doc comment. `esc` is the
                // only way out besides `enter`.
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
                Key::Char('q') => break,
                Key::Char('r') => state.mode = Mode::Browsing,
                // `enter` over a ticked folder — ignored with nothing ticked,
                // the same way `Mode::Browsing`'s own `enter` does nothing
                // over an empty `state.selected`.
                Key::Enter if nav.focus == Focus::Groups && !nav.selected.is_empty() => {
                    let nav = nav.clone();
                    let base = crate::repo::branch_at(cwd)?;
                    state.mode = begin_routine_queue(repo, pipelines, &base, &routines, &nav);
                }
                // `space` over the tasks pane queues that one document alone
                // — over the folders pane it is `handle_routine_key`'s own
                // business instead, ticking the highlighted folder.
                Key::Char(' ') if nav.focus == Focus::Tasks => {
                    let nav = nav.clone();
                    let base = crate::repo::branch_at(cwd)?;
                    state.mode = begin_routine_solo(repo, pipelines, &base, &routines, &nav);
                }
                _ => {
                    let mut nav = nav.clone();
                    handle_routine_key(&routines, &mut nav, key);
                    state.mode = Mode::Routines(nav);
                }
            },
            Mode::Browsing => match key {
                Key::Char('q') => break,
                // `enter` validates the selection and writes it straight
                // through — nothing is drawn in between any more. A
                // validation failure hands back `Mode::Outcome`; a clean
                // write goes on to `after_write`'s own dispatcher offer.
                Key::Enter if !state.selected.is_empty() => {
                    let base = crate::repo::branch_at(cwd)?;
                    let mode = begin_submission(repo, pipelines, &base, &mut groups, &mut state);
                    state.mode = mode;
                }
                // Gated exactly the way `g` is — see `handle_browse_key`'s
                // own `g` arm — since a document only exists to open when
                // the tasks pane is focused on one.
                Key::Char('o')
                    if state.focus == Focus::Tasks
                        && highlighted_task_key(&groups, &state).is_some() =>
                {
                    state.mode = open_highlighted(repo, &groups, &state);
                }
                // `p`: open the trial picker on whichever group the cursor
                // sits on — see `trial_target`. Needs `pipelines`, which
                // `handle_browse_key` is not handed, so this is the one key
                // `run_screen` reads before falling through to it, the same
                // way `o` and `r` already do.
                Key::Char('p') => {
                    if let Some(group) = trial_target(&groups, &state) {
                        state.mode = Mode::Trial(TrialState::new(pipelines, group));
                    }
                }
                // `r` swaps this screen for the routines pane — read fresh
                // every time, so a `s` save made earlier this same session
                // is already there the moment a person switches to it.
                Key::Char('r') => {
                    routines = super::routines::list_routines(repo)?;
                    state.mode = Mode::Routines(RoutineNav::new());
                }
                _ => handle_browse_key(&groups, &mut state, key),
            },
        }
    }
    Ok(ScreenExit::Quit)
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
/// see [`draw`] — so a resized terminal or a pending document someone just
/// edited reaches the screen without a key being typed at all. `None` once
/// the input is exhausted, exactly what a direct [`read_key`] would report.
///
/// The same wait `commands::dispatch`'s own board takes over its pass
/// interval: [`PollableRead::byte_pending`] stands in for a sleep, so a slice
/// with nothing typed into it costs the loop nothing beyond what a plain
/// `thread::sleep(POLL)` would have. Every in-memory reader used in tests
/// reports a byte pending unconditionally (see [`PollableRead`]), so this
/// falls straight through to `read_key` there — the reload below only ever
/// runs against a real, currently idle terminal. A cooked stdin — no raw
/// mode, no `poll` — reports the opposite without waiting, and is read
/// straight away instead: blocking on the line the terminal will deliver is
/// the whole of what this loop is for, where spinning on "nothing pending"
/// would never read a key at all.
fn wait_for_key(
    repo: &Repo,
    groups: &mut Vec<Group>,
    panes: &Panes,
    state: &mut ScreenState,
    last: &mut Option<Vec<String>>,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Option<Key> {
    loop {
        if !cfg!(unix) || input.byte_pending(crate::status::POLL) {
            return read_key(input);
        }
        reload(repo, groups, state);
        draw(groups, panes, state, last, out);
    }
}

/// Re-read the pending directory into `groups`, keeping `group_cursor` on
/// whatever group it was pointing at, by name, rather than by index — a
/// reload can reorder the list out from under it, since `list_groups` sorts
/// newest first and an edit touches a document's own modified time — and
/// clamping it back on screen the ordinary way when that group is gone.
///
/// A pending directory that fails to read this tick is not a reason to blank
/// the screen: `groups` is left exactly as it was, and the next poll tries
/// again — the same tolerance `commands::dispatch`'s own pass loop gives a
/// queue read that comes back unreadable mid-run.
fn reload(repo: &Repo, groups: &mut Vec<Group>, state: &mut ScreenState) {
    let Ok(fresh) = super::pending::list_groups(repo) else {
        return;
    };
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

/// The highlighted group's tasks, or `None` when nothing is under the cursor
/// at all — an empty pending directory, or a filter that matched nothing.
fn highlighted_tasks<'a>(groups: &'a [Group], state: &ScreenState) -> Option<&'a [PendingTask]> {
    shown(groups, state)
        .get(state.group_cursor)
        .map(|group| group.tasks.as_slice())
}

/// The group a `p` press forks: whichever one sits under the cursor right
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
/// None of the keys that need the repo are handled here — `q` quits from
/// every mode at once in [`run_screen`], `enter` needs the repo and the
/// pipelines to submit, `o` needs the repo to open an editor, and `p` needs
/// the pipelines to seed the trial picker's first screen — so nothing this
/// reads can leave the screen or reach outside it.
fn handle_browse_key(groups: &[Group], state: &mut ScreenState, key: Key) {
    match key {
        Key::Char('h') => {
            state.hide_scope = state.hide_scope.next();
            clamp_cursors(groups, state);
        }
        Key::Char('f') => state.mode = Mode::Filter,
        Key::Tab | Key::Left | Key::Right => {
            state.focus = match state.focus {
                Focus::Groups => Focus::Tasks,
                Focus::Tasks => Focus::Groups,
            };
        }
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
        // group is submitted whole or not at all, and the tasks pane is what
        // it holds rather than a second list to pick from.
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
        // `p` forks — since a group with nothing in it has nothing for a
        // routine to hold. `p` itself is `run_screen`'s own business: it
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

/// The folders shown at `path`'s own breadcrumb: `routines` itself at the
/// root, or whichever folder's own subfolders `path` names. An empty slice
/// for a breadcrumb naming a folder that is no longer there — read fresh
/// only when `r` opens this mode, so nothing here has to reload mid-browse —
/// rather than a panic.
pub(super) fn routine_level<'a>(
    routines: &'a [RoutineFolder],
    path: &[String],
) -> &'a [RoutineFolder] {
    let mut level = routines;
    for name in path {
        match level.iter().find(|folder| &folder.name == name) {
            Some(folder) => level = &folder.folders,
            None => return &[],
        }
    }
    level
}

/// The folder the left pane's cursor sits on, at the current breadcrumb.
pub(super) fn highlighted_routine_folder<'a>(
    routines: &'a [RoutineFolder],
    nav: &RoutineNav,
) -> Option<&'a RoutineFolder> {
    routine_level(routines, &nav.path).get(nav.folder_cursor)
}

/// One key over the routines pane — everything but `q`, `r`, `enter` on a
/// selected folder and `space` over the tasks pane, which all need the repo
/// to act on or leave this mode outright, so `run_screen` reads those first
/// and only falls through to this for the rest, the same split it makes for
/// `handle_browse_key`.
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
                let last = routine_level(routines, &nav.path).len().saturating_sub(1);
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
        // Opens whatever is highlighted, one step at a time so both a
        // folder's own documents and its subfolders stay reachable — a
        // folder holding both is not rare enough to make either side
        // permanently unreachable behind the other. From the folders pane:
        // focus this folder's own tasks pane if it has any documents of its
        // own (see `RoutineFolder::own`), or descend into its subfolders if
        // it has none. From the tasks pane: descend into the same folder's
        // subfolders, if it has any — the second step for a folder that had
        // both — dropping back to the folders pane to draw them; a leaf
        // folder's tasks pane, with nothing further down, does nothing at
        // all rather than reset the cursor it is already sitting on.
        Key::Right => {
            if let Some(folder) = highlighted_routine_folder(routines, nav) {
                match nav.focus {
                    Focus::Groups => {
                        if folder.own > 0 {
                            nav.focus = Focus::Tasks;
                            nav.task_cursor = 0;
                        } else if !folder.folders.is_empty() {
                            nav.path.push(folder.name.clone());
                            nav.folder_cursor = 0;
                            nav.task_cursor = 0;
                        }
                    }
                    Focus::Tasks if !folder.folders.is_empty() => {
                        nav.path.push(folder.name.clone());
                        nav.focus = Focus::Groups;
                        nav.folder_cursor = 0;
                        nav.task_cursor = 0;
                    }
                    Focus::Tasks => {}
                }
            }
        }
        // The reverse of `→`, unwinding the same two steps in the same
        // order: out of the tasks pane first — back to the folders pane,
        // on the very folder whose tasks it was showing — and only then up
        // a level.
        Key::Left => match nav.focus {
            Focus::Tasks => nav.focus = Focus::Groups,
            Focus::Groups => {
                nav.path.pop();
                nav.folder_cursor = 0;
                nav.task_cursor = 0;
            }
        },
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
/// key, `q` chief among them, is not read specially at all: it falls to the
/// `Key::Char(c)` arm and is appended like any other letter, which is what
/// makes it "an ordinary character inside the filter" rather than the quit
/// key it is everywhere else.
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

/// `o`: open the highlighted task's document in an editor, in a pane the
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
/// `open_tab` already does — is surfaced through [`Mode::Outcome`] rather
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
        Err(err) => return Mode::Outcome(format!("o: {err:#}")),
    };
    match mux.open_command(&repo.root, &format!("{} · edit", task.id), &command) {
        Ok(()) => Mode::Browsing,
        Err(err) => Mode::Outcome(format!("o: {err:#}")),
    }
}

/// The pipeline a highlighted task's own document names. `parse_submission`
/// refuses a document naming none, so a document still missing one here —
/// still being edited, not yet queueable — resolves nothing rather than
/// guessing at a pipeline no real submission would end up on.
fn task_pipeline<'a>(doc: &str, pipelines: &'a Pipelines) -> Result<&'a Pipeline> {
    let name = doc_pipeline_name(doc).context("document names no `pipeline:`")?;
    pipelines.get(&name)
}

/// A peek at a document's own `pipeline:` key, without the rest of
/// `parse_submission`'s validation — the gate picker needs a pipeline's
/// steps before a document has a `base:` to be validated against at all,
/// and the tasks pane's own `Pipeline:` row needs nothing more than this.
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
    // The same pipeline `parse_submission` would resolve this document
    // against once it is actually submitted — a malformed `pipeline:` on it
    // degrades to "no steps to page through" here rather than a panic, and is
    // caught properly, with a real error, at submit time.
    let pipeline = task_pipeline(&task.doc, pipelines).ok();
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

/// Insert or replace a `<key>: <value>` line right after a document's
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

/// Insert a `gate_at: <step>` line into a task's document, in memory only —
/// the file in the pending directory is never rewritten to record one. The
/// only write the screen makes there is the deletion that follows a
/// submission, and it says nothing about any single task's gate. Any
/// `gate_at:` the document already carried is dropped, so the screen's own
/// choice — the last thing a person actually picked before submitting — is
/// always the one that wins.
fn with_gate(doc: &str, step: &str) -> String {
    with_frontmatter_field(doc, "gate_at", step)
}

/// The groups a person has selected — what `selected_documents` below turns
/// into the pending batch, and whose documents `finish_submit` deletes once
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

/// Every document a selection puts in the queue, in group order, each
/// carrying whatever gate the screen recorded against it.
///
/// Only a task still [`TaskState::Pending`] is a document this submission
/// can write at all — a sibling already read out of `queue/` or `archive/`
/// carries `stage:` and the rest of [`RESERVED_KEYS`], which
/// `parse_submission` refuses on sight. A group is offered here only while
/// it still has a pending task (see [`super::pending::group_state`]), so
/// that task is never the one filtered away.
fn selected_documents(groups: &[Group], state: &ScreenState) -> Vec<(TaskKey, String)> {
    let mut documents = Vec::new();
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
            documents.push((key, doc));
        }
    }
    documents
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
/// terminal reflows on the next one.
pub(super) fn layout() -> Layout {
    match terminal_size::terminal_size() {
        Some((width, height)) => layout_for(width.0 as usize, height.0 as usize),
        None => Layout {
            left: LEFT_PANE_WIDTH,
            right: RIGHT_PANE_WIDTH,
            rows: None,
        },
    }
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
/// four columns of indent, then the widest label this pane ever draws —
/// `Description:`, at 13 characters padded — so `Pipeline:`, `Depends on:`,
/// `Gate:` and `Description:` all line up under each other regardless of
/// which one owns a given row. A constant rather than something measured off
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
/// between one row and the next, so a list holding only two of the three
/// states draws one, and a list holding all three draws two.
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
/// row between them, queued or not, which is what this returning no
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
/// one, or the two drawn once every state is on screen at once.
fn group_line_index(cursor: usize, boundaries: &[usize]) -> usize {
    cursor + boundaries.iter().filter(|&&b| cursor >= b).count()
}

/// The left pane's rows: one per distinct `group:` across the pending
/// documents, its checkbox, and whether the queue already holds it — or,
/// once `h` has hidden every one of them, a single line saying so rather
/// than an empty pane a person could mistake for a project with nothing
/// pending at all.
///
/// The checkbox is here and nowhere else, because a group is what gets
/// submitted. A group not still [`GroupState::Queueable`] has no box at all
/// — at least one of its tasks is no longer waiting in `pending/`, and that
/// is what `h`'s first widening hides on.
///
/// No task count: the pane beside this one is the group's tasks, so a number
/// here says the same thing twice and costs the names the room to be read.
///
/// When two or three states are on screen at once, a blank row separates
/// each pair of them — see [`group_boundaries`]. That row is never a group:
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
fn groups_pane_lines(
    groups: &[Group],
    shown: &[&Group],
    state: &ScreenState,
    width: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    if matches!(state.mode, Mode::Filter) {
        lines.push(format!("find: {}▏", state.filter));
    }

    if shown.is_empty() {
        lines.push(if !state.filter.is_empty() {
            "  no groups match this filter".to_string()
        } else if groups.is_empty() {
            // Nothing anywhere, so there is nothing behind `h` either and
            // no count to give. This is the whole of what an empty pending
            // directory draws — the screen opens onto it rather than
            // refusing, see `opening_message`.
            "  nothing to queue".to_string()
        } else {
            // Only ever `groups.len()` — nothing is visible, so everything
            // hidden is everything there is — but spelled out from the
            // hidden count rather than `groups.len()` directly, so the
            // message keeps meaning the same thing if `visible()` ever
            // grows a second reason to hide a group.
            let hidden = groups.len() - shown.len();
            format!("  nothing to queue — {} hidden", plural(hidden, "group"))
        });
        return lines;
    }

    let boundaries = boundary_for(shown, state);
    lines.reserve(shown.len() + boundaries.len());
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
        lines.push(format!("{marker} {box_} {name}{tail}"));
    }
    lines
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
/// documents carry no state of their own to draw).
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
/// block — its pipeline, what it depends on, any gate chosen for it, and its
/// own one-sentence description — plus the task's own header row above them.
///
/// A document's `touches` globs are not among the labels. They were the
/// widest thing the pane drew, wrapping over several lines per task, and a
/// person choosing what to queue is not picking by glob.
///
/// Read-only. Nothing here is picked: the checkbox is on the group, in the
/// pane to the left, and this is what that group holds.
///
/// Comes back with the line range the highlighted task occupies as well —
/// its header row and everything drawn under it — which is what [`window`]
/// keeps in view on a pane taller than the terminal.
fn tasks_pane_lines(
    groups: &[Group],
    _pipelines: &Pipelines,
    state: &ScreenState,
    width: usize,
) -> (Vec<String>, (usize, usize)) {
    let Some(group) = shown(groups, state).get(state.group_cursor).copied() else {
        return (Vec::new(), (0, 0));
    };
    let tasks = &group.tasks;

    let mut lines = Vec::new();
    let mut focus = (0, 0);
    for (i, task) in tasks.iter().enumerate() {
        lines.push(String::new());
        let opened = lines.len();
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

        // There is no project default any more, so a document naming no
        // pipeline of its own reads as unassigned here — the same as
        // `task_pipeline` resolves nothing for it, and `parse_submission`
        // refuses it outright once it is actually submitted.
        let pipeline = doc_pipeline_name(&task.doc).unwrap_or_else(|| TRIAL_UNASSIGNED.to_string());
        lines.extend(labeled_row("Pipeline:", &pipeline, width));

        let depends_on = super::pending::depends_on(&task.doc);
        let depends_value = if depends_on.is_empty() {
            "-".to_string()
        } else {
            depends_on.join(", ")
        };
        lines.extend(labeled_row("Depends on:", &depends_value, width));

        if let Some(step) = state.gates.get(&task_key(task)) {
            lines.extend(labeled_row("Gate:", step, width));
        }
        // A document with no `title:` draws no row at all rather than an
        // empty one — `parse_submission` is what refuses it, at submit time.
        if let Some(description) = &task.description {
            lines.extend(labeled_row("Description:", description, width));
        }

        if i == state.task_cursor {
            focus = (opened, lines.len().saturating_sub(1));
        }
    }
    (lines, focus)
}

/// The routines pane's own left-hand rows: the current level's folders, each
/// with its checkbox and how many documents sit at or below it — the same
/// `N tasks` tail the mockup draws, and what `enter` on a ticked one queues
/// whole.
///
/// An empty level at the root names `routines_dir` directly rather than
/// drawing an empty pane a person could mistake for a project with nothing
/// under it read wrong — see [`Repo::routines_dir`] on why nothing creates
/// that directory for this to find. An empty level anywhere else is simply a
/// folder with no subfolders of its own, which [`handle_routine_key`]'s own
/// `→` already refused to descend into.
fn routine_folder_lines(
    routines_dir: &std::path::Path,
    level: &[RoutineFolder],
    nav: &RoutineNav,
    width: usize,
) -> Vec<String> {
    if level.is_empty() {
        return vec![if nav.path.is_empty() {
            format!("  nothing under {}", routines_dir.display())
        } else {
            "  nothing here".to_string()
        }];
    }

    level
        .iter()
        .enumerate()
        .map(|(i, folder)| {
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
            format!("{marker} {box_} {name}{tail}")
        })
        .collect()
}

/// The routines pane's own right-hand rows: the highlighted folder's own
/// documents — every one at or below it, the same set `enter` would queue —
/// each drawn the same `labeled_row` way [`tasks_pane_lines`] draws a
/// pending task, minus the `Gate:` row a routine has no gate picker to set.
fn routine_task_lines(
    folder: Option<&RoutineFolder>,
    _pipelines: &Pipelines,
    nav: &RoutineNav,
    width: usize,
) -> (Vec<String>, (usize, usize)) {
    let Some(folder) = folder else {
        return (Vec::new(), (0, 0));
    };

    let mut lines = Vec::new();
    let mut focus = (0, 0);
    for (i, task) in folder.tasks.iter().enumerate() {
        lines.push(String::new());
        let opened = lines.len();
        let marker = if i == nav.task_cursor && nav.focus == Focus::Tasks {
            ">"
        } else {
            " "
        };
        // A routine's own documents carry no queue/archive state of their
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
    (lines, focus)
}

/// The whole of [`Mode::Routines`]'s own frame: the same two-pane geometry
/// [`two_pane_frame`] lays the pending screen out with, folders on the left
/// and the highlighted one's own documents on the right.
pub(super) fn render_routines(
    routines: &[RoutineFolder],
    routines_dir: &std::path::Path,
    pipelines: &Pipelines,
    nav: &RoutineNav,
) -> Vec<String> {
    let layout = layout();
    let level = routine_level(routines, &nav.path);
    let left = routine_folder_lines(routines_dir, level, nav, layout.left);
    let highlighted = level.get(nav.folder_cursor);
    let (right, focus) = routine_task_lines(highlighted, pipelines, nav, layout.right);

    let left_title = format!("routines  {} of {}", level.len(), level.len());
    let right_title = highlighted.map_or("no folder", |folder| folder.name.as_str());
    let folder_line = nav.folder_cursor.min(level.len().saturating_sub(1));
    two_pane_frame(
        &window(&left, (folder_line, folder_line), layout.rows),
        &window(&right, focus, layout.rows),
        &left_title,
        right_title,
        layout,
    )
}

/// The slice of `lines` a pane `rows` rows tall shows, with the block at
/// `focus` — a group's row, or the highlighted task and everything drawn
/// under it — kept in view.
///
/// `None` rows is a run with no terminal to measure, where nothing is cut at
/// all. Where the content does not fit, the pane's bottom row is spent on
/// saying how much is out of sight rather than on a line that would be
/// silently the last one a person sees.
pub(super) fn window(lines: &[String], focus: (usize, usize), rows: Option<usize>) -> Vec<String> {
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
    let hidden = lines.len() - (offset + view);
    shown.push(match hidden {
        0 => format!("↑ {offset} above"),
        _ => format!("↓ {hidden} below"),
    });
    shown
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

/// The keys, under the frame — the one line that says what the screen does.
///
/// While [`Mode::Filter`] is open the ordinary line makes no sense at all —
/// none of `space select` through `enter queue now` reads a key while the
/// filter box has focus, and `q` has stopped meaning quit — so this draws
/// the filter's own line instead, naming exactly the keys
/// [`handle_filter_key`] reads.
///
/// Two-space separators throughout in the ordinary line, not three, to keep
/// it as short as it can be — it is written straight to the terminal rather
/// than through `two_pane_frame`'s own pane widths, so nothing here wraps or
/// truncates it even past an 80-column terminal. `h`'s own words name the
/// next widening [`HideScope::next`] would make — "show queued", then "show
/// done", then "hide all" once every group is already on screen — with no
/// count either way: the pane itself already says how many are hidden, in
/// its own `nothing to queue — <n> groups hidden` line once every group is
/// behind `h`.
fn footer(state: &ScreenState) -> String {
    match &state.mode {
        // `p` and `s` are reserved actions here too — see `run_screen`'s own
        // `Mode::Filter` arm — so the line names the ordinary queue actions
        // a narrowed list still leaves live, rather than describing the
        // query box itself: there is no more "type to narrow" or "q is a
        // letter here" to explain, since typing has nothing special left to
        // say, and no more a distinct `enter` to describe, since leaving the
        // filter and acting on what it narrowed are no longer two separate
        // steps a person has to know about.
        Mode::Filter => "  ↑↓ move   f find   p trial   s save routine   esc back".to_string(),
        // The save panel reads a name the same way the filter box reads a
        // query, so its own line names the same two keys the filter's does
        // to leave it — `enter`/`esc` — rather than any of the ordinary
        // line's, none of which this mode reads as anything but a letter.
        Mode::SaveRoutine { .. } => "  type to rename   enter save   esc cancel".to_string(),
        // Its own screen, not an overlay over the pending one — so its own
        // line, naming exactly the keys `handle_routine_key` and
        // `run_screen`'s own `Mode::Routines` arm read, rather than the
        // ordinary line's `f`/`g`/`o`/`p`/`s`, none of which apply here.
        Mode::Routines(_) => {
            "  ↑↓ move  → open  ← up  space select  enter queue now  r pending  q quit".to_string()
        }
        _ => {
            let hide = match state.hide_scope {
                HideScope::Pending => "h show queued",
                HideScope::PlusQueued => "h show done",
                HideScope::PlusDone => "h hide all",
            };
            format!(
                "  ↑↓ move  space select  f find  g gate  o open  p trial  s save routine  \
                 enter queue now  {hide}  q quit"
            )
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

fn render(groups: &[Group], panes: &Panes, state: &ScreenState) -> Vec<String> {
    match &state.mode {
        Mode::Outcome(msg) => {
            return vec![
                msg.clone(),
                String::new(),
                "press any key to continue".to_string(),
            ];
        }
        Mode::Dispatch(msg) => {
            return vec![
                msg.clone(),
                String::new(),
                "start a dispatcher here now?".to_string(),
                String::new(),
                "  y  yes, in this terminal    n  no, back to the screen    q  quit".to_string(),
            ];
        }
        Mode::Routines(nav) => {
            let mut frame =
                render_routines(panes.routines, panes.routines_dir, panes.pipelines, nav);
            frame.push(footer(state));
            return frame;
        }
        Mode::Browsing
        | Mode::Gate(_)
        | Mode::Trial(_)
        | Mode::Filter
        | Mode::SaveRoutine { .. } => {}
    }

    let pipelines = panes.pipelines;
    let layout = layout();
    let shown = shown(groups, state);
    let left = groups_pane_lines(groups, &shown, state, layout.left);
    let (right, focus) = tasks_pane_lines(groups, pipelines, state, layout.right);
    let left_title = format!("groups  {} of {}", shown.len(), groups.len());
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
    let mut frame = two_pane_frame(
        &window(&left, (group_line, group_line), layout.rows),
        &window(&right, focus, layout.rows),
        &left_title,
        title,
        layout,
    );

    match &state.mode {
        Mode::Gate(cursor) => {
            if let Some(picker) = gate_panel(groups, pipelines, state, *cursor) {
                overlay(&mut frame, &picker);
            }
        }
        Mode::Trial(trial) => {
            if let Some(picker) = trial_panel(groups, pipelines, trial, checkbox_row_cap(layout)) {
                overlay(&mut frame, &picker);
            }
        }
        Mode::SaveRoutine { group, name } => {
            if let Some(picker) = save_routine_panel(groups, group, name) {
                overlay(&mut frame, &picker);
            }
        }
        Mode::Browsing
        | Mode::Outcome(_)
        | Mode::Dispatch(_)
        | Mode::Filter
        | Mode::Routines(_) => {}
    }

    frame.push(footer(state));
    frame
}

/// Redraw the screen, but only when [`render`] actually comes back
/// different from what `last` already holds — a resize is read fresh every
/// call through [`layout`], and a poll tick where nothing on screen would
/// change is the common case once `run_screen`'s wait is a loop rather than
/// a single blocking read. Writing on every tick regardless would turn an
/// idle terminal into one that redraws about once a second for no
/// reason a person watching it could see — exactly what the "idle screen
/// writes nothing" acceptance criterion rules out.
fn draw(
    groups: &[Group],
    panes: &Panes,
    state: &ScreenState,
    last: &mut Option<Vec<String>>,
    out: &mut impl std::io::Write,
) {
    let frame = render(groups, panes, state);
    if last.as_ref() == Some(&frame) {
        return;
    }
    let _ = write!(out, "\x1b[2J\x1b[H");
    for line in &frame {
        let _ = writeln!(out, "{line}");
    }
    *last = Some(frame);
}

/// The gate picker: the highlighted task's own pipeline, a step at a time,
/// with the one currently chosen marked. `None` when there is no task under
/// the cursor to gate, or its document names a pipeline that will not
/// resolve — the same degradation [`handle_gate_key`] already makes.
fn gate_panel(
    groups: &[Group],
    pipelines: &Pipelines,
    state: &ScreenState,
    cursor: usize,
) -> Option<Vec<String>> {
    let group = shown(groups, state).get(state.group_cursor).copied()?;
    let task = group.tasks.get(state.task_cursor)?;
    let pipeline = task_pipeline(&task.doc, pipelines).ok()?;

    let chosen = state.gates.get(&task_key(task));
    let body: Vec<String> = pipeline
        .steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let marker = if i == cursor { ">" } else { " " };
            let mark = if chosen == Some(&step.id) { " ·" } else { "" };
            format!("{marker} {}{mark}", step.id)
        })
        .collect();

    Some(panel(
        &format!("gate {} at", task.id),
        &body,
        "enter set   g clear   esc cancel",
    ))
}

/// The project's pipelines, in the order the trial picker lists them —
/// alphabetical, since [`crate::pipeline::Pipelines::pipelines`] is a
/// `BTreeMap` and this reads its keys straight.
fn trial_pipeline_names(pipelines: &Pipelines) -> Vec<&str> {
    pipelines.pipelines.keys().map(String::as_str).collect()
}

/// The pipeline `trial` has assigned a given task, or `None` for a task the
/// picker has not been given one for yet — still unassigned, since
/// [`TrialState::new`] seeds one only from the task's own document — or for
/// one whose assignment named a pipeline a reload swapped out from under an
/// open picker, handled the same careful way [`trial_group`] handles a group
/// that moved.
fn trial_task_pipeline<'a>(
    pipelines: &'a Pipelines,
    trial: &TrialState,
    task: &PendingTask,
) -> Option<&'a crate::pipeline::Pipeline> {
    let name = trial.pipeline.get(&task_key(task))?;
    pipelines.get(name).ok()
}

/// How many rows [`choose_skips_panel`] draws once every task's steps are
/// flattened into one list — the span [`handle_trial_key`]'s own cursor
/// clamps `ChooseSkips` to, and what [`toggle_trial_skip`] walks to find
/// which task and step the cursor is actually sitting on.
fn trial_total_steps(group: &Group, pipelines: &Pipelines, trial: &TrialState) -> usize {
    group
        .tasks
        .iter()
        .filter_map(|task| trial_task_pipeline(pipelines, trial, task))
        .map(|pipeline| pipeline.steps.len())
        .sum()
}

/// The trial picker's first screen: every task in the group, its own row,
/// and — on the row the cursor sits on — the pipeline it is currently
/// assigned to, between the arrows `←`/`→` cycle it with. Every other row
/// shows its own assignment plainly, so the whole batch's pipelines are
/// readable without moving the cursor onto each one in turn.
fn assign_pipelines_panel(
    group: &Group,
    pipelines: &Pipelines,
    trial: &TrialState,
    width: usize,
) -> Vec<String> {
    let names = trial_pipeline_names(pipelines);
    // The id column is budgeted against the pipeline column beside it, not
    // simply sized to the longest id there happens to be: the pipeline name
    // and the arrows around it are the whole point of this screen, so a very
    // long id gives up its own tail rather than pushing them off the row.
    // Same trade `groups_pane_lines` makes between a group name and its tail.
    let widest_pipeline = names
        .iter()
        .map(|name| name.chars().count())
        .chain(std::iter::once(TRIAL_UNASSIGNED.chars().count()))
        .max()
        .unwrap_or(0);
    let id_budget = width
        .saturating_sub(widest_pipeline + ASSIGN_ROW_CHROME_COLUMNS)
        .max(MIN_NAME_COLUMN);
    let id_width = group
        .tasks
        .iter()
        .map(|task| task.id.chars().count())
        .max()
        .unwrap_or(0)
        .min(id_budget);

    let mut body = vec![
        "assign one pipeline to every task".to_string(),
        String::new(),
    ];
    for (i, task) in group.tasks.iter().enumerate() {
        let marker = if i == trial.cursor { '>' } else { ' ' };
        let pipeline = trial
            .pipeline
            .get(&task_key(task))
            .map(String::as_str)
            .unwrap_or(TRIAL_UNASSIGNED);
        let id = pad_to(&clip(task.id.clone(), id_width), id_width);
        body.push(clip(
            if i == trial.cursor && !names.is_empty() {
                format!("{marker} {id}   ←  {pipeline}  →")
            } else {
                format!("{marker} {id}      {pipeline}")
            },
            width,
        ));
    }

    // `enter` on this screen silently refuses to advance until every task
    // has an assignment (see `handle_trial_key`) — silently unless the
    // footer says why, which is what it is here for.
    let footer = if trial_fully_assigned(group, trial) {
        "↑↓ task   ←→ pipeline   enter next   esc cancel"
    } else {
        "↑↓ task   ←→ pipeline   assign every task, then enter   esc cancel"
    };
    panel(&clip(format!("trial {}", group.name), width), &body, footer)
}

/// The widest a line [`choose_skips_panel`] draws may run before it wraps
/// onto a new one, so the popup [`boxed`] draws around it never grows wider
/// than the frame [`overlay`] is centring it over — which is what happens
/// with nothing here to stop it: `overlay` writes a panel line onto the
/// frame underneath it one character at a time and simply stops once it
/// runs past the frame's own width, taking the panel's own right and bottom
/// border with it and leaving whatever ran off screen undrawn.
///
/// [`two_pane_frame`] draws a frame row `layout.left + layout.right + 7`
/// columns wide, and [`boxed`] spends 6 of those around a body line's own
/// text — its two-space indent, its own inner padding, and its two border
/// columns — so a line here has to stay at `layout.left + layout.right + 1`
/// or narrower. Capped one column under that rather than exactly on it, so
/// the wrap always fires before `overlay`'s own silent truncation could.
fn checkbox_row_cap(layout: Layout) -> usize {
    layout.left + layout.right
}

/// What one [`assign_pipelines_panel`] row spends on something other than
/// the task id and the pipeline name themselves: the cursor marker and the
/// space after it, the gap between the two columns, and the `←`/`→` the
/// cursor's own row draws around its pipeline. The cursor row spends all of
/// that — 11 columns — while the non-cursor row, with no arrows to draw,
/// spends only 8; the constant budgets against the wider of the two, which
/// is what keeps the cursor row from ever being the one that overflows.
/// Both shapes still start their pipeline column at the same offset, which
/// is what keeps every row's pipeline name aligned under the last.
const ASSIGN_ROW_CHROME_COLUMNS: usize = 11;

/// What [`assign_pipelines_panel`] shows in the pipeline column for a task
/// the trial picker has not been given one for yet — there is no project
/// default to show instead, so this is what a legacy or hand-edited document
/// naming none reads as until `←`/`→` gives it one.
const TRIAL_UNASSIGNED: &str = "(unset)";

/// Cut `line` to `width` columns, ending it in `…` when there was more,
/// for the lines of the trial popups that are not built out of fixed-width
/// pieces — a task's own id, and the group name in a popup's title.
///
/// The steps of a long pipeline wrap instead of being cut, because every
/// one of them has its own checkbox a person has to be able to reach. A
/// task id has nothing to reach, so it is cheaper to read one cut id than
/// to reflow a header across two lines. Either way the rule is the same one
/// [`checkbox_row_cap`] states: no line a trial popup draws may be wider
/// than the frame [`overlay`] paints it onto, because `overlay` answers an
/// over-wide line by silently dropping the rest of it along with the
/// popup's own right and bottom border.
fn clip(line: String, width: usize) -> String {
    if line.chars().count() <= width {
        return line;
    }
    let mut out: String = line.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The trial picker's second screen: every task's own header — its id and
/// the pipeline the first screen left it with — and, under it, that
/// pipeline's own steps, each with its own tick to skip, packed several to a
/// line the way the mockup draws a short pipeline and wrapped onto as many
/// lines as [`checkbox_row_cap`] takes once a pipeline runs longer than that
/// — every step still gets its own checkbox drawn, whichever line it lands
/// on. The cursor is one index flattened across every task's steps in turn,
/// so `↑↓` walks the whole batch top to bottom without a second axis to move
/// along; the `>` marker sits right before whichever checkbox that
/// flattened index has currently reached, on whichever line that checkbox
/// wrapped to.
fn choose_skips_panel(
    group: &Group,
    pipelines: &Pipelines,
    trial: &TrialState,
    width: usize,
) -> Vec<String> {
    let mut body = vec!["choose steps to skip".to_string(), String::new()];
    let mut flat = 0usize;
    for task in &group.tasks {
        let key = task_key(task);
        let Some(pipeline) = trial_task_pipeline(pipelines, trial, task) else {
            continue;
        };
        let pipeline_name = trial
            .pipeline
            .get(&key)
            .map(String::as_str)
            .expect("trial_task_pipeline resolved, so this task has an assignment");
        // Same budget as the first screen's rows: the pipeline name says
        // which steps are listed underneath, so it outranks the tail of a
        // very long id. The outer `clip` is the backstop for a pipeline
        // name so long that even a floored id cannot fit beside it.
        let id_budget = width
            .saturating_sub(pipeline_name.chars().count() + 4)
            .max(MIN_NAME_COLUMN);
        body.push(clip(
            format!(" {} · {pipeline_name}", clip(task.id.clone(), id_budget)),
            width,
        ));

        let skip = trial.skip.get(&key);
        let mut line = String::from("   ");
        for (i, step) in pipeline.steps.iter().enumerate() {
            let marker = if flat + i == trial.cursor { "> " } else { "  " };
            let ticked = skip.is_some_and(|set| set.contains(&step.id));
            let box_ = if ticked { "[x]" } else { "[ ]" };
            let token = format!("{marker}{box_} {:<13}", step.id);
            if line.chars().count() + token.chars().count() > width && !line.trim().is_empty() {
                body.push(line.trim_end().to_string());
                line = String::from("   ");
            }
            line.push_str(&token);
        }
        if !line.trim().is_empty() {
            body.push(line.trim_end().to_string());
        }
        flat += pipeline.steps.len();
    }

    panel(
        &clip(format!("trial {}", group.name), width),
        &body,
        "↑↓ move   space toggle   enter run   esc pipelines",
    )
}

/// The trial picker, on whichever of its two screens `trial.stage` names.
/// `None` when the group `p` was pressed over is no longer among `groups` at
/// all, or has been emptied of every task — which nothing on this screen can
/// actually cause, since the picker only opens over a group that already has
/// at least one, but a reload racing an open picker is handled the same
/// careful way [`gate_panel`] already is. `width` is the budget both
/// screens keep every line they draw inside — see [`checkbox_row_cap`] for
/// what happens to a popup that runs past it.
fn trial_panel(
    groups: &[Group],
    pipelines: &Pipelines,
    trial: &TrialState,
    width: usize,
) -> Option<Vec<String>> {
    let group = trial_group(groups, trial)?;
    if group.tasks.is_empty() {
        return None;
    }
    Some(match trial.stage {
        TrialStage::AssignPipelines => assign_pipelines_panel(group, pipelines, trial, width),
        TrialStage::ChooseSkips => choose_skips_panel(group, pipelines, trial, width),
    })
}

/// The save panel: the folder `name` would save under, and how many
/// documents it would copy there unchanged — `None` when the group `s` was
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
        format!("{}/{name}_", crate::config::ROUTINES_DIR),
        format!(
            "copies {} unchanged, same ids",
            plural(group.tasks.len(), "document")
        ),
    ];
    Some(panel(
        &format!("save {} as a routine", group.name),
        &body,
        "enter save   esc cancel",
    ))
}

/// One key over the trial picker — everything but `enter`, which needs the
/// repo to either advance past the first screen or write the batch, and `q`,
/// which is `run_screen`'s own business the same as every other mode. Pure
/// given the group it targets, so it can be checked without a screen to
/// drive: given a group, a state and a key, the next state.
fn handle_trial_key(group: &Group, pipelines: &Pipelines, mut trial: TrialState, key: Key) -> Mode {
    match trial.stage {
        TrialStage::AssignPipelines => {
            let names = trial_pipeline_names(pipelines);
            let last = group.tasks.len().saturating_sub(1);
            match key {
                Key::Esc => return Mode::Browsing,
                Key::Up | Key::Char('k') => trial.cursor = trial.cursor.saturating_sub(1),
                Key::Down | Key::Char('j') => trial.cursor = (trial.cursor + 1).min(last),
                // Cycles the highlighted task's own assignment through every
                // project pipeline, wrapping either direction rather than
                // stopping at the ends — the mockup draws both arrows live
                // on every row, never one greyed out.
                Key::Left | Key::Right if !names.is_empty() => {
                    if let Some(task) = group.tasks.get(trial.cursor) {
                        let key_ = task_key(task);
                        let at = trial
                            .pipeline
                            .get(&key_)
                            .and_then(|current| names.iter().position(|n| n == current));
                        let next = match (at, key) {
                            (Some(at), Key::Left) => (at + names.len() - 1) % names.len(),
                            (Some(at), _) => (at + 1) % names.len(),
                            // Unassigned: whichever arrow is pressed first
                            // lands on the pipeline that sorts first — there
                            // is no current position to cycle away from.
                            (None, _) => 0,
                        };
                        trial.pipeline.insert(key_, names[next].to_string());
                    }
                }
                _ => {}
            }
        }
        TrialStage::ChooseSkips => {
            let total = trial_total_steps(group, pipelines, &trial);
            match key {
                // Back to the first screen, not out of the picker — `enter`
                // moving forward through the two screens is what `esc` walks
                // back through, one at a time; nothing already picked is
                // dropped by moving between them either way.
                Key::Esc => {
                    trial.stage = TrialStage::AssignPipelines;
                    trial.cursor = 0;
                }
                Key::Up | Key::Char('k') => trial.cursor = trial.cursor.saturating_sub(1),
                Key::Down | Key::Char('j') => {
                    trial.cursor = (trial.cursor + 1).min(total.saturating_sub(1))
                }
                Key::Char(' ') => toggle_trial_skip(group, pipelines, &mut trial),
                _ => {}
            }
        }
    }
    Mode::Trial(trial)
}

/// Space on the second trial screen: tick or untick whichever step the
/// flattened cursor is currently sitting on, in whichever task's own skip set
/// that step belongs to — [`trial_total_steps`]'s own walk over every task's
/// steps in turn, stopped as soon as the cursor's index falls inside one.
fn toggle_trial_skip(group: &Group, pipelines: &Pipelines, trial: &mut TrialState) {
    let mut flat = 0usize;
    for task in &group.tasks {
        let Some(pipeline) = trial_task_pipeline(pipelines, trial, task) else {
            continue;
        };
        if trial.cursor < flat + pipeline.steps.len() {
            let step = pipeline.steps[trial.cursor - flat].id.clone();
            let set = trial.skip.entry(task_key(task)).or_default();
            if !set.remove(&step) {
                set.insert(step);
            }
            return;
        }
        flat += pipeline.steps.len();
    }
}

/// `n` with its noun, singular where that is what `n` is.
fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

/// Validate the selection and write it — the whole of what pressing `enter`
/// does. Nothing is drawn in between: a validation failure hands back
/// [`Mode::Outcome`], the same refusal `queue_add_documents` used to hand
/// straight back, and a clean batch goes on to [`finish_submit`] and then
/// [`after_write`]'s own dispatcher offer.
///
/// Takes `state` mutably rather than by reference: `selected_documents` reads
/// it to build the batch, and a landed write clears its selection and gates
/// right here, before the caller ever sees the resulting `Mode` — see
/// [`after_write`]'s own doc comment on why that can no longer be read back
/// off it.
fn begin_submission(
    repo: &Repo,
    pipelines: &Pipelines,
    base: &str,
    groups: &mut Vec<Group>,
    state: &mut ScreenState,
) -> Mode {
    let documents = selected_documents(groups, state);
    let pending = match validate_batch(repo, pipelines, Some(base), &documents) {
        Ok(pending) => pending,
        Err(err) => return Mode::Outcome(format!("submission refused: {err:#}")),
    };
    let selected = state.selected.clone();
    match finish_submit(
        repo, pipelines, groups, pending, &documents, base, &selected,
    ) {
        Ok(msg) => {
            state.selected.clear();
            state.gates.clear();
            clamp_cursors(groups, state);
            after_write(repo, msg)
        }
        Err(err) => refusal_mode("submission refused", err),
    }
}

/// What a landed write becomes on screen: the dispatcher offer, unless a
/// dispatcher already holds the queue's own lock. Starting a second one
/// would only be refused once `commands::dispatch` actually ran — see
/// `Lock::acquire`'s own bail — so the offer is dropped here, before it is
/// ever shown, rather than sent through a `y` that would fail downstream
/// with the whole screen already gone.
///
/// A lock file `Lock::holder` could not even read falls back to the
/// ordinary offer: an unreadable file is closer to "no answer" than to "a
/// dispatcher is running", and this path must never claim a live pid it did
/// not actually see.
fn after_write(repo: &Repo, msg: String) -> Mode {
    match crate::lock::Lock::holder(&repo.lock_file()) {
        Ok(Some(pid)) => Mode::Outcome(format!(
            "{msg}\n\na dispatcher is already running (pid {pid}) — it picks these up on its \
             next pass"
        )),
        _ => Mode::Dispatch(msg),
    }
}

/// Open the batch's tickets and name it, save it, clear the documents it
/// came from out of the pending directory, and build the message
/// [`Mode::Dispatch`] shows.
///
/// The order is the whole guarantee behind "a submission that fails
/// validation removes nothing". Nothing is deleted until every task file has
/// been written, so a batch refused for any reason — a reserved key, an
/// unknown `depends_on`, a cycle among the selection, a hook that failed —
/// leaves the pending directory exactly as it found it, save for the ids a
/// failed hook call had already secured, written back so the next `enter`
/// resumes (see [`open_and_prefix`]).
///
/// Only the selected groups' own documents go, and only the ones this batch
/// actually queued: a sibling [`selected_documents`] left out because it was
/// already [`TaskState::Queued`] or [`TaskState::Done`] keeps its own file —
/// deleting it would be putting one task back by erasing another one's place
/// in the queue or the archive. Another group's documents sit in the same
/// flat directory and are not this submission's to touch either, so the
/// deletion walks the groups it was handed rather than the directory.
fn finish_submit(
    repo: &Repo,
    pipelines: &Pipelines,
    groups: &mut Vec<Group>,
    mut pending: Vec<Task>,
    documents: &[(String, String)],
    base: &str,
    selected: &std::collections::BTreeSet<GroupKey>,
) -> Result<String> {
    // `validate_batch` already ran this over the same batch, and nothing
    // between there and here mutates it any more, so today this is a repeat
    // of a check `pending` already passed. Kept anyway as this function's own
    // guarantee rather than a borrowed one: whatever calls `finish_submit`
    // next, with whatever batch, saves nothing without this check standing
    // between it and disk.
    check_dependencies_set(repo, pipelines, &mut pending)?;
    // `interactive: true` unconditionally: the queue screen already blocks
    // on a key for every other prompt it draws — `Mode::Dispatch`'s `y`/`n`,
    // `Mode::SaveRoutine` — whether or not the process happens to have a
    // real terminal, and relies on `read_key` answering `None` to end
    // gracefully under a script or a closed pane. `own_terminal: false`
    // since `queue_screen` already holds a `TermGuard` for the whole of
    // `run_screen` — see `open_and_prefix`'s own doc comment.
    let task_files = readable_task_files(documents);
    open_and_prefix(repo, documents, &task_files, &mut pending, true, false)?;

    for task in &pending {
        task.save()?;
    }

    // Past this point the queue holds the work, so a failure to unlink is
    // not a reason to refuse a submission that has already landed: the
    // document is left where it is, and the group it belongs to drops off
    // the pane anyway, because the queue now holds every task it names.
    let mut left_alone: Vec<(TaskState, String)> = Vec::new();
    for group in selected_groups(groups, selected) {
        for task in &group.tasks {
            if task.state == TaskState::Pending {
                let _ = std::fs::remove_file(&task.path);
            } else {
                left_alone.push((task.state, task.id.clone()));
            }
        }
    }
    groups.retain(|group| !selected.contains(&group_key(group)));

    // The same report `queue add --from` prints for the same batch, so a
    // person reading one has read the other. The branch comes last, because
    // it comes from the checkout this was run in rather than from anything
    // in a document, and somebody on the wrong branch has no other way to
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

/// Every typed [`crate::task::Frontmatter`] field a document's author may set
/// — the allowlist [`reset_for_reuse`] keeps. Everything else typed is
/// spoolway's own stamp, dropped on reset the same way `parse_submission`
/// already overwrites it on the way in; the difference here is that this
/// runs *before* a reserved key would refuse the document at all.
const AUTHORED_FIELDS: &[&str] = &[
    "id",
    "title",
    "touches",
    "depends_on",
    "parallel",
    "pipeline",
    "group",
    "source",
    "plan",
    "gate_at",
];

/// Passthrough keys that still have to go, despite landing in
/// [`crate::task::Frontmatter`]'s own untyped `extra` map the same as a
/// project's real metadata does: all four are a hook's own answer for the
/// *finished* run. A document that kept `epic:` or `ticket:` into a fresh
/// submission would point the new run at the old run's ticket — `queue add`
/// reports such a document `kept` and never calls the `open` hook for it, so
/// the new run gets no ticket of its own either. `slug:` and `url:` are the
/// machine-written pair from the same answer: a kept `slug:` would pin the
/// new run's group prefix to the old issue's key, and a kept `url:` would
/// address the old issue. See [`crate::task::Task::extra_str`] and
/// `set_extra_str` for how they are carried.
const DROPPED_PASSTHROUGH_FIELDS: &[&str] = &["epic", "ticket", "slug", "url"];

/// A document's text with its frontmatter reduced to what its author owns —
/// [`AUTHORED_FIELDS`], plus any key that is not a typed `Frontmatter` field
/// at all, so a project's own metadata keeps passing through. The body
/// travels byte for byte; only the frontmatter mapping is rebuilt, in its own
/// original key order, so a document already free of stamped keys reads back
/// unchanged.
///
/// The untyped set is read off `Frontmatter` itself rather than a copied
/// list of every stamped field's name: the document is deserialised into a
/// real `Frontmatter` — with the same placeholder `stage:` `parse_submission`
/// inserts, since that field alone has no serde default — and a key survives
/// only if it is in [`AUTHORED_FIELDS`] or it landed in the deserialised
/// `#[serde(flatten)] extra` map, meaning serde found no typed slot for it at
/// all. This is what makes it a real allowlist: a field added to
/// `Frontmatter` tomorrow and left off `AUTHORED_FIELDS` lands in a typed
/// slot, not `extra`, and is dropped by construction — there is no second
/// list of stamped names to forget to update alongside it.
///
/// This is where `s` and `p` make a document handed to them from `queue/` or
/// `archive/` — carrying every key spoolway stamped on its earlier run —
/// safe to hand to `save_routine` and `build_trial_arm`: both go on to call
/// `parse_submission`, which rightly refuses a document that sets a reserved
/// key like `stage:`, and the reset is what makes that document's *reuse*
/// look like the document a producer would have written for a fresh run in
/// the first place. The file on disk this document was read from is never
/// touched — this only ever returns new text.
pub(crate) fn reset_for_reuse(name: &str, doc: &str) -> Result<String> {
    let (yaml, body) =
        crate::task::split_fence(doc).with_context(|| format!("{name}: not a task document"))?;
    let value: serde_norway::Value = serde_norway::from_str(yaml)
        .with_context(|| format!("{name}: frontmatter is not valid YAML"))?;
    let mapping = value
        .as_mapping()
        .with_context(|| format!("{name}: frontmatter is not a mapping"))?
        .clone();

    // `Frontmatter::stage` is the one field with no serde default, so a
    // document missing it — everything off `queue/` or `archive/` sets it,
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

/// The pipeline the trial picker assigned this task, written into its document
/// before [`parse_submission`] is given it.
///
/// The picker's first screen exists precisely to route a document that names
/// no pipeline of its own: [`TrialState::new`] opens such a task unassigned,
/// the panel draws it `(unset)`, and that screen's `enter` refuses to advance
/// until `←`/`→` has given every task one. But `parse_submission` refuses a
/// document with no `pipeline:`, and it is handed the *source* document — so
/// without this the picker refused every task it was built to route, the whole
/// batch was abandoned with `trial refused:`, and nothing was minted. Stamping
/// `front.pipeline` on the arm afterwards cannot save it: the refusal has
/// already happened by then.
///
/// Written over whatever the document said rather than only filled in when it
/// is blank, because the screen may equally have cycled a task *off* the
/// pipeline its own document named — `trial.pipeline` is the authority here,
/// which is the same order of precedence `front.pipeline` is stamped in below.
fn with_trial_pipeline(name: &str, doc: &str, pipeline: &str) -> Result<String> {
    let (yaml, body) =
        crate::task::split_fence(doc).with_context(|| format!("{name}: not a task document"))?;
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

/// One trial arm: the source document parsed exactly as `queue add --from`
/// would, with the four things a trial names for the task itself stamped
/// on afterwards — the id spoolway minted, the trial the whole batch shares,
/// the pipeline this arm runs, and the ticked steps that pipeline is asked
/// to walk past.
///
/// `parse_submission` always resets `front.skip` to empty, since a document
/// may not set it itself (see that function's own comment); this is the one
/// caller allowed to put it back; here it is spoolway naming the task, not
/// the document. `front.branch` is recomputed too, after the id changes —
/// `parse_submission` already built one, but off the document's own bare
/// id, before this ever had a minted one to use.
///
/// `doc` is reset with [`reset_for_reuse`] before it is parsed, so a document
/// that came from `queue/` or `archive/` — carrying `stage:` and every other
/// key an earlier run stamped on it — reaches `parse_submission` looking like
/// a document a producer wrote for a fresh run, rather than being refused for
/// setting a reserved key spoolway itself put there.
///
/// The picker's chosen pipeline goes into that document *before* it is parsed,
/// by [`with_trial_pipeline`], and not only onto the arm afterwards — see that
/// function for why stamping `front.pipeline` below is too late on its own.
fn build_trial_arm(
    name: &str,
    doc: &str,
    base: &str,
    id: &str,
    trial_id: &str,
    pipeline: &crate::pipeline::Pipeline,
    skip: &std::collections::BTreeSet<String>,
) -> Result<Task> {
    let doc = reset_for_reuse(name, doc)?;
    let doc = with_trial_pipeline(name, &doc, &pipeline.name)?;
    let mut arm = parse_submission(name, &doc, Some(base))?;
    arm.front.id = id.to_string();
    arm.front.branch = Some(format!("task/{id}"));
    arm.front.trial = Some(trial_id.to_string());
    arm.front.pipeline = Some(pipeline.name.clone());
    // Only the ticked steps this pipeline actually has: `spoolway doctor`
    // refuses a `skip:` naming a step its own pipeline lacks
    // (`src/commands/pipeline.rs`), and a trial's arms rarely share every
    // step of their own pipelines.
    arm.front.skip = skip
        .iter()
        .filter(|step| pipeline.step(step).is_some())
        .cloned()
        .collect();
    arm.path = std::path::PathBuf::from(format!("{id}.md"));
    Ok(arm)
}

/// `enter` on the trial picker's second screen: mint one id per task in the
/// group, build that task's own arm on the pipeline and skip set the two
/// screens chose for it, and write the whole set — or refuse, and touch
/// nothing. Mirrors [`begin_submission`], but over a group forked whole
/// rather than chosen piece by piece, and the source documents are never
/// deleted: they were templates for the arms, not themselves submitted, and
/// stay in whichever directory `p` found them in exactly as it found them —
/// the same "nothing minted is ever written back" the doc comment on
/// `TrialState` promises.
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
    if group.tasks.is_empty() {
        return Mode::Browsing;
    }

    // Minted fresh, not the group's own name or any one task's — two
    // trials of the same group must read apart in the ledger, since
    // `spoolway eval --runs --trial <id>` groups arms by this value the same
    // way `group_by_run` groups a run, and a reused value would silently
    // fold two unrelated batches into one comparison table.
    let trial_id = crate::usage::new_trial_id();

    let mut minted: std::collections::BTreeSet<String> = Default::default();
    let mut id_map: std::collections::BTreeMap<String, String> = Default::default();
    let mut arms = Vec::with_capacity(group.tasks.len());
    let mut summary = Vec::with_capacity(group.tasks.len());

    // `group.tasks` lists dependencies before dependents (see `Group`'s own
    // doc comment), so by the time a dependent task is minted here, every
    // group member it could `depends_on` already has its own entry in
    // `id_map` — one forward pass is enough to remap the whole chain.
    for task in &group.tasks {
        let key = task_key(task);
        let pipeline_name = trial
            .pipeline
            .get(&key)
            .cloned()
            .expect("the AssignPipelines screen's own `enter` refuses to advance until every task is assigned");
        let pipeline = match pipelines.get(&pipeline_name) {
            Ok(pipeline) => pipeline,
            Err(err) => return Mode::Outcome(format!("trial refused: {err:#}")),
        };

        // A task id becomes a lane name, a branch and a file name — the same
        // check an ordinary submission runs in `validate_batch`, against the
        // pipeline this particular arm names rather than a document's own.
        let id = mint_id(repo, &task.id, &minted);
        if let Err(err) = crate::mux::check_task_id(&id, longest_agent_step(pipeline)) {
            return Mode::Outcome(format!("trial refused: {err:#}"));
        }
        minted.insert(id.clone());
        id_map.insert(task.id.clone(), id.clone());

        let skip = trial.skip.get(&key).cloned().unwrap_or_default();
        let mut arm = match build_trial_arm(
            &task.path.display().to_string(),
            &task.doc,
            base,
            &id,
            &trial_id,
            pipeline,
            &skip,
        ) {
            Ok(arm) => arm,
            Err(err) => return Mode::Outcome(format!("trial refused: {err:#}")),
        };

        // Every dependency this trial is also forking maps to that
        // predecessor's own minted id — the bare id it named is never itself
        // queued by a trial (see `build_trial_arm`), so this is what keeps
        // the chain the source group named intact inside the batch. A
        // dependency outside the batch is left exactly as it read, unless
        // this task's own document is already archived: `retain` sweeps a
        // finished predecessor out of `archive/`, so a stale reference an
        // archived document still carries cannot be trusted to resolve, and
        // is dropped instead — the same emptying a lone archived fork always
        // made, generalised from "the one task this forked" to "this task,
        // whichever one of the group it is".
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
        summary.push((task.id.clone(), pipeline_name, skip));
        arms.push(arm);
    }

    match finish_trial(repo, pipelines, arms) {
        Ok(()) => after_write(repo, trial_report(&trial_id, &group.name, base, &summary)),
        Err(err) => Mode::Outcome(format!("trial refused: {err:#}")),
    }
}

/// Validate the minted arms as one set and save them — or none, on any
/// failure. Mirrors [`finish_submit`]'s own all-or-nothing write, minus the
/// pending-directory deletion that function makes: a trial's source
/// documents are never among the tasks being written.
fn finish_trial(repo: &Repo, pipelines: &Pipelines, mut arms: Vec<Task>) -> Result<()> {
    check_dependencies_set(repo, pipelines, &mut arms)?;
    for arm in &arms {
        arm.save()?;
    }
    Ok(())
}

/// The report `enter` leaves on screen once a trial's arms have all landed:
/// the batch's own shared id, the group it forked, and — one row per task,
/// in the same order the two screens showed it — the pipeline it runs and
/// the steps it skips, so a person can read the whole batch without opening
/// any one arm's document. Ends the same way every other write this screen
/// makes does, naming the branch every arm was cut from.
fn trial_report(
    trial_id: &str,
    group_name: &str,
    base: &str,
    rows: &[(String, String, std::collections::BTreeSet<String>)],
) -> String {
    let id_width = rows
        .iter()
        .map(|(id, _, _)| id.chars().count())
        .max()
        .unwrap_or(0);
    let pipeline_width = rows
        .iter()
        .map(|(_, pipeline, _)| pipeline.chars().count())
        .max()
        .unwrap_or(0);

    let mut msg = format!("trial {trial_id} queued from {group_name}\n\n");
    for (id, pipeline, skip) in rows {
        let skip = if skip.is_empty() {
            "-".to_string()
        } else {
            skip.iter().cloned().collect::<Vec<_>>().join(", ")
        };
        msg.push_str(&format!(
            "  {id:<id_width$}   {pipeline:<pipeline_width$}   skip {skip}\n"
        ));
    }
    msg.push_str(&format!("\n  dependencies preserved\n  based on `{base}`"));
    msg
}

// ============================= The routines pane ===========================

/// `s`'s own write: copy `group`'s documents into `.spoolway/routines/<name>/`,
/// each reset with [`reset_for_reuse`] so a document that came from `queue/`
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
    // routine somewhere no `r` pane ever reads it back from. One plain
    // folder name, and nothing else.
    if std::path::Path::new(name).components().count() != 1
        || !matches!(
            std::path::Path::new(name).components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Mode::Outcome(format!(
            "`{name}` is not a folder name — one plain name, no `/` and no `..`"
        ));
    }
    let dir = repo.routines_dir().join(name);

    match std::fs::read_dir(&dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Mode::Outcome(format!(
                    "`{}` already holds documents — pick another name, or clear it first",
                    dir.display()
                ));
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Mode::Outcome(format!("s: reading {}: {err:#}", dir.display())),
    }

    if let Err(err) = std::fs::create_dir_all(&dir) {
        return Mode::Outcome(format!("s: creating {}: {err:#}", dir.display()));
    }
    for task in &group.tasks {
        let path = dir.join(format!("{}.md", task.id));
        let reset = match reset_for_reuse(&task.path.display().to_string(), &task.doc) {
            Ok(reset) => reset,
            Err(err) => {
                return Mode::Outcome(format!("s: resetting {}: {err:#}", task.path.display()));
            }
        };
        if let Err(err) = std::fs::write(&path, &reset) {
            return Mode::Outcome(format!("s: writing {}: {err:#}", path.display()));
        }
    }

    Mode::Outcome(format!(
        "saved {} to {}",
        plural(group.tasks.len(), "document"),
        dir.display()
    ))
}

/// Every document at or below the ticked folders at the routines pane's
/// current level, each minted a fresh id — never the bare one a routine's
/// own document carries, since a routine exists to be queued more than
/// once, and the second run would collide with the first at the id the
/// document itself always names. `group:` and the body travel unchanged;
/// only `id:` and any `depends_on:` naming a sibling in this same batch are
/// rewritten, to the same minted ids, so a chain saved together still
/// resolves once every id in it has changed.
///
/// A folder appearing under more than one ticked ancestor — ticking both a
/// folder and one it already contains — is not doubled: each document's own
/// path is only ever queued once.
fn routine_batch_documents(
    repo: &Repo,
    routines: &[RoutineFolder],
    nav: &RoutineNav,
) -> Vec<(String, String)> {
    let level = routine_level(routines, &nav.path);
    let mut tasks: Vec<&RoutineTask> = Vec::new();
    let mut seen: std::collections::BTreeSet<&std::path::Path> = Default::default();
    for folder in level {
        if !nav.selected.contains(&folder.path) {
            continue;
        }
        for task in &folder.tasks {
            if seen.insert(task.path.as_path()) {
                tasks.push(task);
            }
        }
    }

    mint_routine_batch(repo, &tasks)
}

/// Mint a fresh id for every routine document and rewrite its `id:` line,
/// and any `depends_on:` naming a sibling in this same batch, onto the
/// minted ids — the transform the queue screen's `enter` and a scheduled
/// job both run once they have the list of documents to queue. `tasks` is
/// already deduped and in the order the batch should keep. Nothing is
/// written: the returned `(source path, rewritten document)` pairs are the
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

/// Queue a routine target the way the `r` pane does, but driven by a job
/// rather than the screen's nav. `target` is an absolute path under
/// [`Repo::routines_dir`]: a folder queues every document at or below it as
/// one batch, exactly as `enter` does, and a single `.md` file queues that
/// task alone with its `depends_on` emptied, exactly as `space` does. Every
/// queued document is put on `pipeline` — a job names its own, where the `r`
/// pane leaves each document on whatever it carried. The batch goes through
/// [`validate_batch`] all-or-nothing and the saved tasks are handed back so
/// a caller can record which ids it minted. The source documents under
/// `.spoolway/routines/` are never touched.
pub(crate) fn queue_routine_target(
    repo: &Repo,
    pipelines: &Pipelines,
    base: &str,
    target: &std::path::Path,
    pipeline: &str,
) -> Result<Vec<Task>> {
    let mut documents = if target.is_dir() {
        let folder = super::routines::read_folder_at(target)?;
        // `folder.tasks` is already this folder's own documents plus every
        // nested subfolder's, depth-first — the same list `enter` queues.
        if folder.tasks.is_empty() {
            bail!("{} holds no task documents", target.display());
        }
        let tasks: Vec<&RoutineTask> = folder.tasks.iter().collect();
        mint_routine_batch(repo, &tasks)
    } else {
        let task = super::routines::read_task_at(target)?;
        let id = mint_id(repo, &task.id, &Default::default());
        let mut doc = with_frontmatter_field(&task.doc, "id", &id);
        // Emptied for the same reason `begin_routine_solo` empties it: a
        // lone document names no sibling in this batch, so a real
        // `depends_on` would be refused by `check_dependencies_set`.
        doc = with_frontmatter_field(&doc, "depends_on", "[]");
        vec![(task.path.display().to_string(), doc)]
    };

    for (_, doc) in &mut documents {
        *doc = with_frontmatter_field(doc, "pipeline", pipeline);
    }

    let mut tasks = validate_batch(repo, pipelines, Some(base), &documents)?;
    // No document to write ids back into — see `open_and_prefix`. This route
    // has no screen at all — a job, or `--pipeline`'s own CLI caller — so it
    // asks `crate::ask` whether anyone is really there and takes its own
    // terminal, the same way `queue_add_documents` does. `esc`'s
    // `GateCancelled` is left to propagate as an ordinary `Err` rather than
    // caught here: both callers — `jobs::fire_job`'s dispatcher loop and its
    // own `jobs run` — already treat any `Err` as "did not fire" and neither
    // marks the job fired nor records queued ids, which is the one honest
    // answer for a run a person actually declined. Turning it into a fake
    // empty success here would let the dispatcher believe this minute's
    // firing already happened.
    //
    // `documents` is `&[]` — nothing here is ever written back into — but
    // `task_files` is not: a routine's own document is a real file under
    // `.spoolway/routines/`, safe for a hook to read, only never to write
    // to.
    let task_files = readable_task_files(&documents);
    open_and_prefix(
        repo,
        &[],
        &task_files,
        &mut tasks,
        crate::ask::interactive(),
        true,
    )?;
    // All or none: everything above parsed and validated, so these writes
    // are the commit — the same discipline `queue_add_documents` follows.
    for task in &tasks {
        task.save()?;
    }
    Ok(tasks)
}

/// Open the batch's tickets and name it, save every task `validate_batch`
/// handed back, and build the same per-task report [`finish_submit`] builds
/// for its own batch — [`finish_trial`] no longer builds this shape itself;
/// see [`trial_report`] for what a trial reports instead. The source
/// documents under `.spoolway/routines/` are never touched — a routine is
/// meant to be queued again, not consumed by being queued once — which is
/// why nothing is handed to [`open_and_prefix`] to write ids back into.
/// `task_files` still names each one's real path, for a hook's own
/// `SPOOLWAY_TASK_FILE` to point at — see [`open_and_prefix`]'s own doc
/// comment on why that is a different list from the empty `documents`.
fn finish_routine(
    repo: &Repo,
    tasks: &mut [Task],
    task_files: &[String],
    base: &str,
) -> Result<String> {
    // `interactive: true`, `own_terminal: false` — driven from the queue
    // screen's own routines pane, which already holds the terminal for the
    // whole of `run_screen`; see `open_and_prefix`'s own doc comment.
    open_and_prefix(repo, &[], task_files, tasks, true, false)?;
    for task in tasks.iter() {
        task.save()?;
    }

    let mut msg = String::new();
    for task in tasks.iter() {
        msg.push_str(&format!(
            "queued {} at `{}`\n  {}\n",
            task.id(),
            crate::pipeline::QUEUED,
            task.path.display()
        ));
    }
    msg.push_str(&based_on_note(tasks, base));
    Ok(msg)
}

/// `enter` over the routines pane's folders: mint every ticked folder's
/// documents through [`routine_batch_documents`] and queue them as one
/// batch through [`validate_batch`] — the same all-or-nothing write
/// [`begin_submission`] gives a pending selection.
fn begin_routine_queue(
    repo: &Repo,
    pipelines: &Pipelines,
    base: &str,
    routines: &[RoutineFolder],
    nav: &RoutineNav,
) -> Mode {
    let documents = routine_batch_documents(repo, routines, nav);
    if documents.is_empty() {
        return Mode::Browsing;
    }
    let task_files = readable_task_files(&documents);
    match validate_batch(repo, pipelines, Some(base), &documents) {
        Ok(mut tasks) => match finish_routine(repo, &mut tasks, &task_files, base) {
            Ok(msg) => after_write(repo, msg),
            Err(err) => refusal_mode("queue refused", err),
        },
        Err(err) => Mode::Outcome(format!("queue refused: {err:#}")),
    }
}

/// `space` over the routines pane's own tasks pane: queue the highlighted
/// document alone, under a minted id, with its `depends_on` emptied before
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
) -> Mode {
    let Some(folder) = highlighted_routine_folder(routines, nav) else {
        return Mode::Browsing;
    };
    let Some(task) = folder.tasks.get(nav.task_cursor) else {
        return Mode::Browsing;
    };

    let id = mint_id(repo, &task.id, &Default::default());
    let mut doc = with_frontmatter_field(&task.doc, "id", &id);
    doc = with_frontmatter_field(&doc, "depends_on", "[]");
    let documents = vec![(task.path.display().to_string(), doc)];
    let task_files = readable_task_files(&documents);

    match validate_batch(repo, pipelines, Some(base), &documents) {
        Ok(mut tasks) => match finish_routine(repo, &mut tasks, &task_files, base) {
            Ok(msg) => after_write(repo, msg),
            Err(err) => refusal_mode("queue refused", err),
        },
        Err(err) => Mode::Outcome(format!("queue refused: {err:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::*;

    /// A minimal, non-empty body — the shape most of these tests only need
    /// to exist, not to say anything in particular.
    const BODY: &str = "## Goal\n\nDo the thing.\n";

    /// `queue list --json` reports one `paused` for both a gate-held row and
    /// a question-held one — `State::WaitingOnYou` is gone — while `next`
    /// still carries the wording that tells the two apart and `resumable` is
    /// `true` on both: a question-held row offers the same `[r]` a gate does,
    /// alongside the pane it names. Nothing emits the old `waiting_on_you`
    /// state label, and nothing emits `parked` either — that state left with
    /// the fields behind it, and a lane that stops reporting now reads as an
    /// ordinary `paused` row like these two.
    #[test]
    fn queue_json_reports_paused_for_both_a_gate_and_a_question() {
        use crate::status::State;
        use crate::status::testutil::row;

        let mut gate = row("release-me");
        gate.state = State::Paused;
        gate.resumable = true;
        gate.next = "→ handover — [r] resumes it".into();

        let mut question = row("question");
        question.state = State::Paused;
        question.resumable = true;
        question.next = "look at pane `question · implement` — [r] resumes it".into();

        let json: Vec<QueueRowJson> = [&gate, &question]
            .iter()
            .map(|r| QueueRowJson::from(*r))
            .collect();

        assert_eq!(json[0].state, "paused");
        assert!(json[0].resumable);
        assert_eq!(json[1].state, "paused");
        assert!(json[1].resumable);
        assert_eq!(
            json[1].next,
            "look at pane `question · implement` — [r] resumes it"
        );

        let rendered = serde_json::to_string(&json[1]).unwrap();
        assert!(!rendered.contains("waiting_on_you"), "{rendered}");
        assert!(!rendered.contains("parked"), "{rendered}");
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
    /// document named relatively (`--from ../t.md`, or an entry
    /// `gather_documents` joined onto a relative `--from <dir>`); a relative
    /// name would resolve against the wrong directory once the hook reads it
    /// (review round 2 finding 6). This does not switch the process's own
    /// working directory to prove it — that is global state shared by every
    /// test running in parallel — it only checks the shape of the answer:
    /// relative in, absolute out, matching what `canonicalize` itself
    /// resolves the same name to.
    ///
    /// The document deliberately does *not* come from [`crate::scratch::
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

        let documents = vec![(relative.display().to_string(), String::new())];
        let resolved = readable_task_files(&documents);

        assert_eq!(resolved.len(), 1);
        assert!(
            std::path::Path::new(&resolved[0]).is_absolute(),
            "a relative document name must resolve to an absolute path: {resolved:?}"
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

    /// A name naming nothing real — the `<stdin>#N` [`gather_documents`]
    /// mints for a stream entry, chief among them — resolves to the empty
    /// string, the same as it did under the old `is_file` check.
    #[test]
    fn readable_task_files_is_empty_for_a_name_that_resolves_to_nothing() {
        let documents = vec![("<stdin>#1".to_string(), String::new())];
        assert_eq!(readable_task_files(&documents), vec![String::new()]);
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

    /// A whole task document, in the shape `--from` accepts: `id:` plus
    /// whatever else `extra` puts in the frontmatter, then `body`. `pipeline:`
    /// is required now, so this fills in the built-in `default` pipeline
    /// unless `extra` already names one — a test after the unassigned shape
    /// itself builds its own document instead, bypassing this default.
    fn document(id: &str, extra: &str, body: &str) -> String {
        let pipeline = if extra.contains("pipeline:") {
            ""
        } else {
            "pipeline: default\n"
        };
        format!("---\nid: {id}\ntitle: {id}, done\n{pipeline}{extra}---\n{body}")
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
    /// so a document under test can still leave `base:` out unless the test
    /// is about `base:` itself, which passes its own document with `base:`
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
        let repo = fixture("queue-add-in-lane");
        let text = document("login", "", BODY);
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

    /// A task's base is what a document's own `base:` or `queue add --base`
    /// chose, never the branch of whichever checkout this was run in — a
    /// worktree on an entirely different branch changes nothing about the
    /// base a submission gets.
    #[test]
    fn a_task_is_based_on_the_flag_not_the_checkout_it_was_queued_in() {
        let repo = fixture("base-from-flag-not-cwd");
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

        let text = document("login", "group: b\n", BODY);
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
        let repo = fixture("cross-base-dep");
        add(&repo, "login", &[]);

        let mut sessions = queued(&repo, "login");
        sessions.front.id = "sessions".into();
        sessions.front.depends_on = vec!["login".into()];
        sessions.front.base = Some("plan/other".into());
        let err =
            check_dependencies_set(&repo, &Pipelines::builtin(), &mut [sessions]).unwrap_err();

        assert!(
            err.to_string().contains("would never put it in reach"),
            "{err:#}"
        );
    }

    #[test]
    fn a_dependency_on_a_task_that_does_not_exist_is_refused() {
        let repo = fixture("unknown-dep");
        add(&repo, "login", &[]);

        let text = document("sessions", "group: demo\ndepends_on: [lgoin]\n", BODY);
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

    /// The same refusal, but naming the age rather than leaving a swept
    /// dependency reading like a typo — `retain` is what could have deleted
    /// it, and this is the one caller that knows enough to say so.
    #[test]
    fn a_dependency_swept_out_of_the_archive_says_the_age_is_why() {
        let repo = fixture("swept-dep");

        // `login` finished a while ago: written straight into `archive/`,
        // the same shape a real `done` task lands in, rather than queued and
        // driven there — this suite has no dispatcher to do that with.
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("login.md"),
            "---\nid: login\ntitle: login\nstage: done\n---\nbody\n",
        )
        .unwrap();

        let text = document("sessions", "group: demo\ndepends_on: [login]\n", BODY);
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
        let err = check_dependencies_set(&repo, &Pipelines::builtin(), &mut [sessions.clone()])
            .unwrap_err();
        assert!(
            err.to_string().contains("housekeeping.retention_days"),
            "the age was not named: {err:#}"
        );

        // `housekeeping.retention_days = 0` never sweeps, so the same
        // missing dependency is reported the plain way instead.
        let mut off = crate::config::Config::default();
        off.housekeeping.retention_days = 0;
        let repo_off = Repo {
            config: off,
            ..repo.clone()
        };
        sessions.front.depends_on = vec!["login".into()];
        let err =
            check_dependencies_set(&repo_off, &Pipelines::builtin(), &mut [sessions]).unwrap_err();
        assert!(
            !err.to_string().contains("housekeeping.retention_days"),
            "retention off must not be blamed: {err:#}"
        );
    }

    #[test]
    fn a_dependency_that_would_close_a_cycle_is_refused() {
        let repo = fixture("cycle-dep");
        add(&repo, "a", &[]);
        add(&repo, "b", &["a"]);

        // `a` already reaches `b`, so this edge would close the loop. It is
        // rejected by editing `a`'s file, which is the only way to express it.
        let mut a = queued(&repo, "a");
        a.front.depends_on = vec!["b".into()];
        let err = check_dependencies_set(&repo, &Pipelines::builtin(), &mut [a]).unwrap_err();

        assert!(err.to_string().contains("a → b → a"), "{err:#}");
    }

    #[test]
    fn a_task_may_not_depend_on_itself() {
        let repo = fixture("self-dep");
        let text = document("a", "group: demo\ndepends_on: [a]\n", BODY);
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

    /// With the collision walk gone, every edge left comes from a plan page,
    /// and a plan cuts one group at a time — so a `depends_on` reaching into
    /// another group is always a mistake, not a real chain.
    #[test]
    fn a_dependency_on_a_task_of_another_group_is_refused() {
        let repo = fixture("cross-group-dep");
        add(&repo, "login", &[]);

        let text = document("sessions", "group: other\ndepends_on: [login]\n", BODY);
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
        assert!(msg.contains("`other`") && msg.contains("`demo`"), "{msg}");
        assert!(msg.contains("a chain does not cross a group"), "{msg}");
    }

    /// With `issue_tracking.key_in_names` on, a sibling queued by an
    /// earlier `queue add` carries `group: <slug>-<group>`, and a later
    /// document still writes the bare group — the check has to see them as
    /// one group, or a chain can never span two `queue add` calls (jobs
    /// review finding 3). Only a recognised prefix, and only with the flag
    /// on: with it off the same two groups are still two groups.
    #[test]
    fn a_dependency_on_a_sibling_whose_group_carries_the_slug_prefix_is_accepted() {
        let mut repo = fixture("cross-batch-slug-prefix");
        add(&repo, "auth-01", &[]);
        // What the first `queue add` left behind with the flag on.
        let mut parent = queued(&repo, "auth-01");
        parent.front.group = Some("proj-12-demo".into());
        parent
            .front
            .extra
            .insert("slug".into(), serde_norway::Value::String("proj-12".into()));
        parent.save().unwrap();

        let text = document("auth-02", "group: demo\ndepends_on: [auth-01]\n", BODY);
        let path = write_doc(&repo, "auth-02.md", &text);
        let args = from_args(&[&path]);

        let err = queue_add(&repo, &Pipelines::builtin(), &args, &repo.root, false).unwrap_err();
        assert!(
            err.to_string().contains("a chain does not cross a group"),
            "with the flag off the prefix means nothing: {err:#}"
        );

        repo.config.issue_tracking.key_in_names = true;
        queue_add(&repo, &Pipelines::builtin(), &args, &repo.root, false)
            .expect("the bare group and its prefixed sibling are one group");
        assert_eq!(
            queued(&repo, "auth-02").front.depends_on,
            vec!["auth-01".to_string()]
        );
    }

    /// A dependent is cut from `depends_on.first()`'s branch, so a list
    /// naming two parents only means something if the first one already
    /// carries the second's work — `check_dependencies_set` puts that id
    /// first itself rather than trusting the document's own order.
    #[test]
    fn a_depends_on_naming_two_parents_is_reordered_so_the_deeper_one_leads() {
        let repo = fixture("reorder-dep");
        add(&repo, "base", &[]);
        add(&repo, "top", &["base"]);

        // `top` already reaches `base`, so naming `base` first here is the
        // order that buys nothing — `top`'s own history already holds it.
        let text = document("apex", "group: demo\ndepends_on: [base, top]\n", BODY);
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
            "the id reaching the other one leads, whatever order the document gave"
        );
    }

    /// A single-id list has nothing to reorder — it should come out exactly
    /// as it went in.
    #[test]
    fn a_single_id_depends_on_is_left_untouched() {
        let repo = fixture("single-dep");
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
        let repo = fixture("no-head-dep");
        add(&repo, "a", &[]);
        add(&repo, "b", &[]);

        let text = document("c", "group: demo\ndepends_on: [a, b]\n", BODY);
        let path = write_doc(&repo, "c.md", &text);
        let err = queue_add(
            &repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
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

    fn touching(repo: &Repo, id: &str, touches: &[&str], depends_on: &[&str]) {
        touching_with(repo, id, touches, depends_on, false);
    }

    fn touching_with(repo: &Repo, id: &str, touches: &[&str], depends_on: &[&str], parallel: bool) {
        let mut extra = format!("group: demo\ntouches: [{}]\n", touches.join(", "));
        if !depends_on.is_empty() {
            extra += &format!("depends_on: [{}]\n", depends_on.join(", "));
        }
        if parallel {
            extra += "parallel: true\n";
        }
        let text = document(id, &extra, BODY);
        let path = write_doc(repo, &format!("{id}.md"), &text);
        queue_add(
            repo,
            &Pipelines::builtin(),
            &from_args(&[&path]),
            &repo.root,
            false,
        )
        .unwrap();
    }

    fn found(repo: &Repo) -> Vec<String> {
        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &Pipelines::builtin(), &repo.archive_dir());
        conflicts(&tasks, &graph)
            .iter()
            .map(|c| {
                format!(
                    "{} {} [{}] {}",
                    c.a,
                    c.b,
                    c.shared.join(", "),
                    if c.ordered { "ordered" } else { "UNORDERED" }
                )
            })
            .collect()
    }

    #[test]
    fn globs_that_name_the_same_files_conflict_even_when_written_differently() {
        let repo = fixture("overlap");
        touching(&repo, "broad", &["src/**"], &[]);
        touching(&repo, "narrow", &["src/api/**"], &[]);
        touching(&repo, "elsewhere", &["docs/**"], &[]);

        assert_eq!(
            found(&repo),
            ["broad narrow [src/** ~ src/api/**] UNORDERED"],
            "a wide glob really does cover a narrow one under it"
        );
    }

    /// Across two plans there is no edge to add: `depends_on` is refused across
    /// bases, and the two branches never see each other until main. Advising a
    /// `depends_on` there would send whoever queued the plan to a command that
    /// refuses them.
    #[test]
    fn an_overlap_between_two_plans_is_not_one_depends_on_could_fix() {
        let repo = fixture("overlap-cross-base");
        touching(&repo, "mine", &["src/**"], &[]);
        touching(&repo, "theirs", &["src/api/**"], &[]);

        // As though it had been queued from another plan's worktree.
        let mut theirs = queued(&repo, "theirs");
        theirs.front.base = Some("plan/other".into());
        theirs.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &Pipelines::builtin(), &repo.archive_dir());
        let found = conflicts(&tasks, &graph);

        assert_eq!(found.len(), 1, "the overlap is real and still reported");
        assert!(!found[0].same_base);
        assert!(!found[0].ordered, "and nothing in the queue orders it");
    }

    /// Two tasks of the same group, each marked `parallel: true`, that still
    /// overlap: `queue conflicts` reads that as a mistake in the group rather
    /// than an edge nobody added — the pair said, on purpose, that they mean
    /// to run beside each other.
    #[test]
    fn a_declared_parallel_pair_is_read_as_a_mistake_not_a_missing_edge() {
        let repo = fixture("overlap-declared-parallel");
        touching_with(&repo, "left", &["src/**"], &[], true);
        touching_with(&repo, "right", &["src/api/**"], &[], true);

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &Pipelines::builtin(), &repo.archive_dir());
        let found = conflicts(&tasks, &graph);

        assert_eq!(found.len(), 1);
        assert!(found[0].declared_parallel);
        assert!(!found[0].ordered);
    }

    /// Only meaningful within one group: a declared-parallel task never reads
    /// an overlap with another group's declared-parallel task as deliberate —
    /// the two never chose to run beside each other, they merely both did.
    #[test]
    fn declared_parallel_does_not_cross_groups() {
        let repo = fixture("overlap-parallel-cross-group");
        touching_with(&repo, "mine", &["src/**"], &[], true);
        touching_with(&repo, "theirs", &["src/api/**"], &[], true);

        // As though it had been queued from another group's worktree.
        let mut theirs = queued(&repo, "theirs");
        theirs.front.group = Some("other".into());
        theirs.front.base = Some("plan/other".into());
        theirs.save().unwrap();

        let tasks = repo.tasks().unwrap();
        let graph = Graph::build(&tasks, &Pipelines::builtin(), &repo.archive_dir());
        let found = conflicts(&tasks, &graph);

        assert_eq!(found.len(), 1);
        assert!(!found[0].declared_parallel);
    }

    #[test]
    fn tasks_the_graph_already_keeps_apart_are_not_flagged() {
        let repo = fixture("overlap-ordered");
        // `ui` waits on `api` waits on `schema`, so `ui` and `schema` never run
        // at once — even though neither names the other.
        touching(&repo, "schema", &["src/db/**"], &[]);
        touching(&repo, "api", &["src/api/**"], &["schema"]);
        touching(&repo, "ui", &["src/db/schema.rs"], &["api"]);

        assert_eq!(
            found(&repo),
            ["schema ui [src/db/** ~ src/db/schema.rs] ordered"],
            "two hops of depends_on order a pair just as well as one"
        );
    }

    /// `spoolway queue conflicts` reads the queue alone — no pending batch
    /// involved — and reports an unordered overlap the same way it always
    /// has, whether or not anything is ever typed at the screen.
    #[test]
    fn queue_conflicts_reports_an_unordered_overlap_with_no_screen_involved() {
        let repo = fixture("conflicts-report");
        touching(&repo, "broad", &["src/**"], &[]);
        touching(&repo, "narrow", &["src/api/**"], &[]);

        let report = conflicts_report(&repo, &Pipelines::builtin()).unwrap();
        assert_eq!(
            report,
            "broad and narrow both touch src/** ~ src/api/** \
             (NOT ordered — add a depends_on or merge the tasks)"
        );
    }

    /// With no live lane and no command step running, `queue pause` is
    /// exactly `park`: the task lands on `paused` with `parked_from` naming
    /// the step it was pulled off of.
    #[test]
    fn queue_pause_parks_a_task_with_nothing_live_to_interrupt() {
        let repo = fixture("queue-pause");
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
        let mut repo = fixture("queue-pause-blocked");
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
        let repo = fixture("queue-pause-unknown");
        let pipelines = Pipelines::builtin();
        assert!(queue_pause(&repo, &pipelines, "ghost", false).is_err());
    }

    /// A task sitting on a real `Command`-kind step (`checks`, in the builtin
    /// `default` pipeline) with a run genuinely in flight: `queue pause`
    /// without `--force` refuses rather than stopping it out from under
    /// whatever it was doing, and the run is left exactly as it was. With
    /// `--force` it kills the run, the task still lands on `paused`, and a
    /// second call to `state` — reading the pid file back off disk — proves
    /// the process is actually gone, not just forgotten about.
    #[test]
    fn queue_pause_refuses_a_running_command_step_without_force() {
        let repo = fixture("queue-pause-running");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);
        let mut task = queued(&repo, "solo");
        task.set_stage_unbanked("checks", "test setup");
        task.save().unwrap();

        let runs = crate::command_step::Runs::new(&repo.commands_dir());
        let key = crate::command_step::Runs::key("checks", "solo");
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
            "checks",
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
        let repo = fixture("queue-resume");
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
        let repo = fixture("queue-resume-question-pane");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);
        let mut task = queued(&repo, "solo");
        task.set_stage_unbanked("implement", "test setup");
        task.front.last_report = Some(crate::task::LastReport {
            step: "implement".into(),
            outcome: "pass".into(),
            at: 0,
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

    #[test]
    fn queue_resume_refuses_an_unknown_task() {
        let repo = fixture("queue-resume-unknown");
        let pipelines = Pipelines::builtin();
        assert!(queue_resume(&repo, &pipelines, "ghost").is_err());
    }

    /// Bare `queue add` no longer queues anything with no `--from`: there is
    /// no id to name a file after, so what it hands back is the document to
    /// fill one in with, still unfilled.
    #[test]
    fn bare_queue_add_prints_an_unfilled_skeleton_document() {
        let repo = fixture("skeleton");
        let pipelines = Pipelines::builtin();

        let doc = skeleton_document(&repo, &pipelines).unwrap();

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
        let text = document(
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
    fn a_document_with_an_empty_body_is_refused() {
        let text = document("demo", "group: demo\n", "   \n\n");
        let err = parse_submission("demo.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err:#}");
    }

    /// `title:` is the squashed commit's subject and the pull request's
    /// title — no heading in the body can supply one, so a document leaving
    /// it blank is refused before anything is queued, naming the document
    /// and the field.
    #[test]
    fn a_document_with_no_title_is_refused() {
        let text = "---\nid: demo\ngroup: demo\n---\n## Goal\n\nDo the thing.\n";
        let err = parse_submission("mine.md", text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("`title:`"), "{err:#}");
        assert!(err.to_string().contains("mine.md"), "{err:#}");
    }

    /// The keys spoolway sets on every task itself are refused by name, and
    /// the document they came from is named too — this is the whole
    /// enforcement that a producer cannot smuggle a task onto an arbitrary
    /// step, run or attempt count.
    #[test]
    fn a_document_setting_a_reserved_key_is_refused_by_name() {
        for key in [
            "stage",
            "run",
            "attempts",
            "base_commit",
            "cut_from",
            "trial",
            "branch",
        ] {
            let text = document("demo", &format!("group: demo\n{key}: bogus\n"), BODY);
            let err = parse_submission("mine.md", &text, Some("plan/demo")).unwrap_err();
            assert!(err.to_string().contains(key), "{key}: {err:#}");
            assert!(err.to_string().contains("mine.md"), "{key}: {err:#}");
        }
    }

    /// `base:` is a document's to set, and what it sets is kept — a branch
    /// this repository really has is checked for by `validate_batch`, not
    /// here. A document that leaves it out, or writes it blank, takes the
    /// submission's own base instead — the `--base` flag or the checkout's
    /// branch, whichever `parse_submission` was handed.
    #[test]
    fn a_document_setting_base_keeps_it_and_one_without_takes_the_submissions() {
        let text = document("demo", "group: demo\nbase: some/other/branch\n", BODY);
        let task = parse_submission("mine.md", &text, Some("plan/live")).unwrap();
        assert_eq!(task.front.base.as_deref(), Some("some/other/branch"));

        let blank = document("demo", "group: demo\nbase: \"  \"\n", BODY);
        let task = parse_submission("mine.md", &blank, Some("plan/live")).unwrap();
        assert_eq!(task.front.base.as_deref(), Some("plan/live"));

        let plain = document("demo", "group: demo\n", BODY);
        let task = parse_submission("mine.md", &plain, Some("plan/live")).unwrap();
        assert_eq!(task.front.base.as_deref(), Some("plan/live"));
    }

    /// A document that sets no `base:` of its own, submitted with no
    /// `--base` either, is refused by name — never based on whichever
    /// branch a checkout happens to have out.
    #[test]
    fn a_document_with_no_base_and_no_flag_is_refused() {
        let text = document("demo", "group: demo\n", BODY);
        let err = parse_submission("explicit-task-base.md", &text, None).unwrap_err();
        assert!(err.to_string().contains("explicit-task-base.md"), "{err:#}");
        assert!(err.to_string().contains("sets no `base:`"), "{err:#}");
        assert!(err.to_string().contains("--base"), "{err:#}");
    }

    /// There is no project default to route an omission through any more, so
    /// a document naming no `pipeline:` is refused the same way one naming
    /// no `group:` already is — built by hand rather than through
    /// `document`, which now fills the key in.
    #[test]
    fn a_document_with_no_pipeline_is_refused() {
        let text = format!("---\nid: demo\ntitle: demo, done\ngroup: demo\n---\n{BODY}");
        let err = parse_submission("no-pipeline.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("no-pipeline.md"), "{err:#}");
        assert!(err.to_string().contains("must set `pipeline:`"), "{err:#}");
    }

    /// A document that sets `pipeline:` to nothing but whitespace is refused
    /// the same as one that omits the key outright — blank counts as absent,
    /// the same courtesy `group:` already gets.
    #[test]
    fn a_document_with_a_blank_pipeline_is_refused() {
        let text =
            format!("---\nid: demo\ntitle: demo, done\ngroup: demo\npipeline: \"  \"\n---\n{BODY}");
        let err = parse_submission("blank-pipeline.md", &text, Some("plan/demo")).unwrap_err();
        assert!(err.to_string().contains("must set `pipeline:`"), "{err:#}");
    }

    /// The retired quota-and-usage-limit park fields have no struct home any
    /// more — a submission that still carries one from an earlier run has it
    /// dropped on parse, the same as any other document `Task::parse` refuses
    /// to round-trip.
    #[test]
    fn a_document_carrying_park_fields_has_them_dropped() {
        let text = document(
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
    /// `IGNORED_KEYS` in `src/commands/task.rs` — and a document that carries
    /// one in from an earlier run must not have it survive back into the
    /// queue: a re-queued task that parked with a step's count already at
    /// the ceiling would otherwise write the very first failure of its next
    /// run as the fourth attempt, past `MAX_LAUNCH_FAILURES`, and park again
    /// without ever writing why — the reason-writing guard in
    /// `Dispatcher::note_launch_failure` fires only on the attempt that
    /// exactly spends the ceiling, and a count that starts at 3 skips it.
    #[test]
    fn a_document_carrying_launch_failures_has_them_reset() {
        let text = document(
            "demo",
            "group: demo\nlaunch_failures:\n  implement: 3\n",
            BODY,
        );
        let task = parse_submission("mine.md", &text, Some("plan/demo")).unwrap();
        assert!(task.front.launch_failures.is_empty());
    }

    /// A repeat submission is refused whether the id is still in the queue —
    /// a run still in flight — or already in the archive, naming whichever
    /// file it actually found: an archived run finished under that id, and a
    /// second one queued over it would be silently overwritten the moment
    /// this one finished too.
    #[test]
    fn validate_batch_refuses_an_id_in_either_the_queue_or_the_archive() {
        let repo = fixture("validate-batch-existing");
        std::fs::write(
            repo.queue_dir().join("taken.md"),
            "---\nid: taken\ntitle: taken\nstage: queued\n---\nbody\n",
        )
        .unwrap();
        let text = document("taken", "group: demo\n", BODY);
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
        let text = document("done", "group: demo\n", BODY);
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

    /// `--base` is as arbitrary a value as a document's own `base:` — a
    /// leading `-` or a branch this repository does not have locally is
    /// refused whichever of the two named it, not only when a document's
    /// own value happens to disagree with the flag.
    #[test]
    fn a_flags_base_is_checked_the_same_as_a_documents_own() {
        let repo = fixture("flag-base-checked");
        let text = document("demo", "group: demo\n", BODY);
        let err = validate_batch(
            &repo,
            &Pipelines::builtin(),
            Some("no/such/branch"),
            &[("mine.md".into(), text)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not have locally"), "{err:#}");

        let text = document("demo", "group: demo\n", BODY);
        let err = validate_batch(
            &repo,
            &Pipelines::builtin(),
            Some("-x"),
            &[("mine.md".into(), text)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("would read as a flag"), "{err:#}");
    }

    /// A document naming its own `base:` is checked even when that value
    /// happens to equal the submission's `--base` — the two are not allowed
    /// to shadow each other into skipping the check.
    #[test]
    fn a_documents_own_base_is_checked_even_when_it_matches_the_flag() {
        let repo = fixture("own-base-matches-flag");
        let text = document("demo", "group: demo\nbase: no/such/branch\n", BODY);
        let err = validate_batch(
            &repo,
            &Pipelines::builtin(),
            Some("no/such/branch"),
            &[("mine.md".into(), text)],
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not have locally"), "{err:#}");
    }

    /// Unrecognised keys are a project's own metadata, not spoolway's
    /// business, and survive a round trip through `Frontmatter`'s `extra`.
    #[test]
    fn unrecognised_keys_survive_through_extra() {
        let text = document("demo", "group: demo\nsize: small\ncomplexity: 3\n", BODY);
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

    /// `gate_at` is a document's to set, read by `commands::report` the same
    /// way whoever wrote it by hand or the board's own `s` key would have —
    /// though unlike `step.gate`, it catches whatever that step reports, not
    /// only its pass.
    #[test]
    fn gate_at_is_read_from_a_document() {
        let text = document("demo", "group: demo\ngate_at: handover\n", BODY);
        let task = parse_submission("mine.md", &text, Some("plan/demo")).unwrap();
        assert_eq!(task.front.gate_at.as_deref(), Some("handover"));
    }

    /// `source` is a document's to set, optionally, and lands on the task
    /// verbatim — nothing in `parse_submission` parses it, the same as
    /// nothing anywhere else in spoolway does.
    #[test]
    fn source_is_read_from_a_document_verbatim() {
        let text = document(
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
        let repo = fixture("sibling-dep");
        let login = document("login", "group: demo\n", BODY);
        let sessions = document("sessions", "group: demo\ndepends_on: [login]\n", BODY);
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

    /// Written all or none: one document that fails validation must not
    /// leave the ones that would have passed sitting in the queue.
    #[test]
    fn one_bad_document_queues_nothing_from_the_same_submission() {
        let repo = fixture("all-or-none");
        let good = document("wire", "group: demo\n", BODY);
        let bad = document("bogus", "group: demo\ndepends_on: [ghost]\n", BODY);
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

    /// A `---`-separated stream is split back into whole documents, each
    /// still fenced on both sides — this is what `--from -` reads.
    #[test]
    fn a_stream_splits_into_whole_documents() {
        let a = document("a", "group: demo\n", "## Goal\nFirst.\n");
        let b = document("b", "group: demo\n", "## Goal\nSecond.\n");
        let stream = format!("{a}{b}");

        let docs = split_stream(&stream);

        assert_eq!(docs.len(), 2, "{docs:?}");
        assert_eq!(docs[0], a);
        assert_eq!(docs[1], b);
    }

    /// A directory named by `--from` expands to every `*.md` file in it, in
    /// filename order — non-markdown files beside them are not documents.
    #[test]
    fn a_directory_expands_to_its_md_files_in_order() {
        let repo = fixture("from-dir");
        let dir = repo.root.join("tasks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.md"), document("b", "group: demo\n", BODY)).unwrap();
        std::fs::write(dir.join("a.md"), document("a", "group: demo\n", BODY)).unwrap();
        std::fs::write(dir.join("notes.txt"), "not a document").unwrap();

        let docs = gather_documents(&[dir.display().to_string()]).unwrap();

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

    /// One pending task document, written into the directory
    /// `list_groups` scans. `doc` is the whole document, its own `group:`
    /// and all — the same bytes `--from` would read.
    fn write_pending(repo: &Repo, id: &str, doc: &str) -> std::path::PathBuf {
        let path = repo.pending_dir().join(format!("{id}.md"));
        std::fs::write(&path, doc).unwrap();
        path
    }

    /// Two documents of one group, in dependency order — what a test needs
    /// whenever one document is not enough to say what it is about.
    fn write_pending_two(repo: &Repo, a_id: &str, a_doc: &str, b_id: &str, b_doc: &str) {
        write_pending(repo, a_id, a_doc);
        write_pending(repo, b_id, b_doc);
    }

    /// The groups those documents gather into, in the order the left pane
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

    /// The frame that was actually on screen when the input ran out — every
    /// draw opens on a clear-screen, so a captured transcript holds every
    /// frame the screen ever drew, back to back.
    fn last_frame(drawn: &str) -> &str {
        drawn.rsplit("\x1b[2J\x1b[H").next().unwrap_or(drawn)
    }

    /// `with_gate` is the one place a gate chosen on the screen reaches a
    /// task's document — in memory only, since the file in the pending
    /// directory is never rewritten to record one.
    #[test]
    fn with_gate_inserts_after_the_opening_fence() {
        let doc = document("wire", "group: demo\n", BODY);
        let gated = with_gate(&doc, "handover");
        assert!(gated.starts_with("---\ngate_at: handover\n"), "{gated}");
        assert!(gated.contains("id: wire\n"), "{gated}");
    }

    /// The screen's own choice always wins, even over a `gate_at` a task's
    /// document already happened to carry.
    #[test]
    fn with_gate_replaces_a_gate_the_document_already_carried() {
        let doc = document("wire", "group: demo\ngate_at: fix\n", BODY);
        let gated = with_gate(&doc, "handover");
        assert_eq!(gated.matches("gate_at:").count(), 1, "{gated}");
        assert!(gated.contains("gate_at: handover"), "{gated}");
        assert!(!gated.contains("gate_at: fix"), "{gated}");
    }

    #[test]
    fn with_gate_leaves_a_document_with_no_fence_untouched() {
        assert_eq!(with_gate("not a document", "handover"), "not a document");
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
    /// document, without validating it — and reads nothing else. The
    /// document here carries a `touches` too, to hold the pane to not
    /// showing it.
    #[test]
    fn depends_on_reads_the_list_off_the_document() {
        let doc = document(
            "wire",
            "group: demo\ntouches: [src/a.rs, src/b.rs]\ndepends_on: [login]\n",
            BODY,
        );
        assert_eq!(super::pending::depends_on(&doc), vec!["login"]);
    }

    /// A document naming no `depends_on` — or carrying no fence at all — reads as
    /// an empty list rather than an error: the pane shows `Depends on:  -`
    /// for the first case and never panics on either.
    #[test]
    fn depends_on_defaults_to_an_empty_list() {
        let doc = document("wire", "group: demo\n", BODY);
        assert_eq!(super::pending::depends_on(&doc), Vec::<String>::new());
        assert_eq!(
            super::pending::depends_on("not a document"),
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
        let repo = fixture("panes-width");
        write_pending(
            &repo,
            "wire",
            &document(
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
            let left = groups_pane_lines(&groups, &shown, &state, layout.left);
            let (right, _) = tasks_pane_lines(&groups, &pipelines, &state, layout.right);
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

    /// A pane taller than the terminal scrolls to the highlighted block and
    /// says how much is out of sight, rather than letting the frame run off
    /// the bottom of the screen.
    #[test]
    fn a_pane_taller_than_its_rows_scrolls_to_the_focused_block() {
        let lines: Vec<String> = (0..20).map(|i| format!("line {i}")).collect();

        let top = window(&lines, (0, 0), Some(5));
        assert_eq!(top.len(), 5);
        assert_eq!(top[0], "line 0");
        assert_eq!(top[4], "↓ 16 below");

        let middle = window(&lines, (11, 12), Some(5));
        assert_eq!(middle.len(), 5);
        assert!(
            middle.contains(&"line 11".to_string()) && middle.contains(&"line 12".to_string()),
            "the focused block must stay in view, got {middle:?}"
        );

        let bottom = window(&lines, (19, 19), Some(5));
        assert_eq!(bottom[3], "line 19");
        assert_eq!(bottom[4], "↑ 16 above");

        assert_eq!(
            window(&lines, (0, 0), None).len(),
            20,
            "no terminal, no cut"
        );
        assert_eq!(window(&lines[..3], (0, 0), Some(9)).len(), 3);
    }

    /// Selecting a group queues every document in it. A group's tasks are
    /// one chain, and half a chain in the queue is a task waiting on a
    /// dependency nobody sent.
    #[test]
    fn selecting_a_group_queues_every_document_in_it() {
        let repo = fixture("screen-whole-group");
        write_pending_two(
            &repo,
            "first",
            &document("first", "group: one\ntouches: [src/a.rs]\n", BODY),
            "second",
            &document(
                "second",
                "group: one\ntouches: [src/b.rs]\ndepends_on: [first]\n",
                BODY,
            ),
        );
        let groups = listed(&repo);

        // One space on the group, enter to submit it, `n` to decline the
        // dispatcher the screen then offers.
        screen(&repo, groups, " \rn");

        assert!(repo.queue_dir().join("first.md").exists(), "first task");
        assert!(repo.queue_dir().join("second.md").exists(), "second task");
    }

    /// A group the queue already holds has no checkbox and cannot be
    /// selected again: its tasks are already queued, and `h` is what takes it
    /// off the screen.
    #[test]
    fn a_queued_group_cannot_be_selected_again() {
        let repo = fixture("screen-queued-group");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);
        let mut state = ScreenState::new();
        // `h` shows it: this test is about the row a queued group draws, not
        // about the default that now hides it.
        state.hide_scope = HideScope::PlusQueued;

        handle_browse_key(&groups, &mut state, Key::Char(' '));
        assert!(
            state.selected.is_empty(),
            "a queued group must not be selectable"
        );

        let shown = visible(&groups, state.hide_scope);
        let rows = groups_pane_lines(&groups, &shown, &state, 30);
        assert!(
            !rows[0].contains('[') && rows[0].contains("queued"),
            "a queued group shows the mark and no box, got {rows:?}"
        );
    }

    /// The blank row between the queueable and queued halves: drawn once,
    /// exactly at the boundary, never carrying the cursor marker whichever
    /// group it is on — and gone entirely once only one half is on screen,
    /// so a project with nothing queued yet gets no dangling blank line at
    /// the bottom of the pane.
    #[test]
    fn groups_pane_lines_draws_one_unmarked_separator_between_the_two_groups() {
        let repo = fixture("screen-separator-row");
        write_pending(&repo, "wire", &document("wire", "group: unqueued\n", BODY));
        // A birth time has no `set_*` the way a modification time does —
        // see `pending::list_groups`' own tests — so this sleeps to
        // guarantee the second document really is the later-written one.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_pending(&repo, "cook", &document("cook", "group: cook\n", BODY));
        already_queued(&repo, "cook");
        let groups = listed(&repo);

        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusQueued;
        let shown = visible(&groups, state.hide_scope);
        assert_eq!(shown.len(), 2, "both groups have to be on screen for this");

        // The cursor is on the queued group (`group_cursor` addresses
        // `shown`, index 1) — proving the marker still lands past the
        // separator, on the row it belongs to, not on the blank one ahead
        // of it.
        state.group_cursor = 1;
        let rows = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows.len(),
            3,
            "one group, a blank separator, one more group: {rows:?}"
        );
        assert_eq!(rows[1], "", "the separator itself is a blank row: {rows:?}");
        assert!(
            rows[2].starts_with('>'),
            "the cursor on the queued group must land past the separator: {rows:?}"
        );

        // With only one half on screen — `h` back on, hiding the queued
        // group — there is no boundary left to draw a row for at all.
        state.hide_scope = HideScope::Pending;
        state.group_cursor = 0;
        let shown = visible(&groups, state.hide_scope);
        let rows = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows.len(),
            1,
            "one group alone draws no separator: {rows:?}"
        );
        assert!(!rows[0].is_empty(), "{rows:?}");
    }

    /// Widen all the way to `done` and every state draws its own separator:
    /// two blank rows, not one, and `group_cursor` shifts by however many of
    /// them sit ahead of it — the acceptance criterion a single boundary
    /// could never exercise.
    #[test]
    fn two_separators_are_drawn_once_every_state_is_on_screen() {
        let repo = fixture("screen-two-separators");
        write_pending(&repo, "wire", &document("wire", "group: unqueued\n", BODY));
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_pending(&repo, "cook", &document("cook", "group: cook\n", BODY));
        already_queued(&repo, "cook");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("shipped.md"),
            "---\nid: shipped\ntitle: shipped\ngroup: shipped\nstage: done\n---\nbody\n",
        )
        .unwrap();
        let groups = listed(&repo);

        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusDone;
        let shown = visible(&groups, state.hide_scope);
        assert_eq!(shown.len(), 3, "one group per state");

        let rows = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows.len(),
            5,
            "three groups plus two blank separators: {rows:?}"
        );
        assert_eq!(rows[1], "", "the first separator: {rows:?}");
        assert_eq!(rows[3], "", "the second separator: {rows:?}");

        // `group_cursor` addresses `shown` directly (index 2, the done
        // group), never a separator — so it has to land on the fifth line,
        // shifted past both blank rows ahead of it.
        state.group_cursor = 2;
        assert_eq!(
            group_line_index(state.group_cursor, &boundary_for(&shown, &state)),
            4
        );
    }

    /// The mockup's exact tail: the bare word `queued`, no timestamp.
    #[test]
    fn the_queued_tail_is_the_bare_word() {
        let repo = fixture("screen-queued-tail");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);

        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusQueued;
        let shown = visible(&groups, state.hide_scope);
        let rows = groups_pane_lines(&groups, &shown, &state, 40);
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
        let repo = fixture("screen-queued-name-width");
        for name in ["bound-loops", "queue-open", "state-paths"] {
            write_pending(
                &repo,
                name,
                &document(name, &format!("group: {name}\n"), BODY),
            );
            already_queued(&repo, name);
        }
        let groups = listed(&repo);
        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusQueued;
        let shown = visible(&groups, state.hide_scope);

        let layout = layout_for(74, 24);
        let rows = groups_pane_lines(&groups, &shown, &state, layout.left);
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
        let repo = fixture("screen-queued-name-max-width");
        // 33 characters — exactly what MAX_LEFT_PANE budgeted a name before
        // the tail grew, and the width MAX_LEFT_PANE was widened to keep
        // affording it.
        let name = "a".repeat(33);
        write_pending(
            &repo,
            &name,
            &document(&name, &format!("group: {name}\n"), BODY),
        );
        already_queued(&repo, &name);
        let groups = listed(&repo);
        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusQueued;
        let shown = visible(&groups, state.hide_scope);

        let rows = groups_pane_lines(&groups, &shown, &state, MAX_LEFT_PANE);
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
        let repo = fixture("screen-queued-tail-never-truncates");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);

        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusQueued;
        let shown = visible(&groups, state.hide_scope);
        for width in [MIN_LEFT_PANE, MIN_LEFT_PANE + 20, MAX_LEFT_PANE] {
            let rows = groups_pane_lines(&groups, &shown, &state, width);
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
        let repo = fixture("screen-queued-tail-live-repro-widths");
        write_pending(
            &repo,
            "bound-loops",
            &document("bound-loops", "group: one\n", BODY),
        );
        already_queued(&repo, "bound-loops");
        let groups = listed(&repo);

        let mut state = ScreenState::new();
        state.hide_scope = HideScope::PlusQueued;
        let shown = visible(&groups, state.hide_scope);
        for total_columns in [74usize, 99] {
            let layout = layout_for(total_columns, 24);
            let rows = groups_pane_lines(&groups, &shown, &state, layout.left);
            assert!(
                rows[0].ends_with(" queued"),
                "expected the full tail at a {total_columns}-column terminal \
                 (left pane {}), got {rows:?}",
                layout.left
            );
        }
    }

    /// A landed submission clears the group out of the pane the same act it
    /// clears it off disk: `list_groups` runs once per screen session, so
    /// nothing else would ever pick up that the documents are gone, and a
    /// row left behind is one a person can select and submit a second time.
    /// Drives `begin_submission` directly, not through `run_screen`, so the
    /// pane's own copy of the list is what gets inspected afterward.
    #[test]
    fn submit_clears_the_group_from_the_pane_and_from_the_pending_directory() {
        let repo = fixture("screen-submit-clears-pending");
        let path = write_pending(
            &repo,
            "wire",
            &document("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let mut groups = listed(&repo);
        assert_eq!(groups.len(), 1);

        let mut state = ScreenState::new();
        handle_browse_key(&groups, &mut state, Key::Char(' '));

        let pipelines = Pipelines::builtin();
        let mode = begin_submission(&repo, &pipelines, "plan/demo", &mut groups, &mut state);
        assert!(
            matches!(mode, Mode::Dispatch(_)),
            "expected a clean submission to reach Mode::Dispatch, got {mode:?}"
        );

        assert!(!path.exists(), "the document must be gone from pending");
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
    /// task through: `selected_documents` puts every task of the group in
    /// one batch, including the sibling's document read straight out of
    /// `queue/`, which carries `stage:` — and `parse_submission`'s
    /// `RESERVED_KEYS` check refuses any document that sets it, so today the
    /// whole submission is refused rather than just queueing the pending one
    /// and leaving the queued sibling alone.
    #[test]
    fn queueing_a_group_leaves_its_queued_sibling_alone_and_queues_the_pending_task() {
        let repo = fixture("screen-requeue-group");
        let beta_path = write_pending(&repo, "beta", &document("beta", "group: one\n", BODY));
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
        let mode = begin_submission(&repo, &pipelines, "plan/demo", &mut groups, &mut state);
        assert!(
            matches!(mode, Mode::Dispatch(_)),
            "expected beta to queue with alpha left alone, got {mode:?}"
        );

        assert!(!beta_path.exists(), "beta's pending document must be gone");
        assert!(
            alpha_path.exists(),
            "alpha's queue document must be untouched"
        );
        assert!(
            repo.queue_dir().join("beta.md").exists(),
            "beta must have landed in the queue"
        );
        let Mode::Dispatch(msg) = mode else {
            unreachable!("checked above");
        };
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
        let repo = fixture("screen-requeue-group-archived");
        let beta_path = write_pending(&repo, "beta", &document("beta", "group: one\n", BODY));
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
        let mode = begin_submission(&repo, &pipelines, "plan/demo", &mut groups, &mut state);
        let Mode::Dispatch(msg) = mode else {
            panic!("expected beta to queue with alpha left alone, got {mode:?}");
        };

        assert!(!beta_path.exists(), "beta's pending document must be gone");
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
        let repo = fixture("screen-columns");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _) = tasks_pane_lines(&groups, &pipelines, &state, 46);
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

        // The document above carries `touches: [src/wire.rs]`, and the pane shows
        // neither the label nor the glob. They were the widest thing drawn
        // here, and a person choosing what to queue does not pick by glob.
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("touches") || line.contains("src/wire.rs")),
            "the pane never draws a document's touches, got {lines:?}"
        );
    }

    /// A document's own `title:` reaches the tasks pane under its own
    /// `Description:` label — the one sentence a person picks by.
    #[test]
    fn a_documents_title_reaches_the_tasks_pane() {
        let repo = fixture("screen-description");
        write_pending(
            &repo,
            "ctx-peak",
            "---\nid: ctx-peak\ntitle: Bank the largest context reading a lane reached, as \
             ctx_peak on the ledger line.\ngroup: one\n---\n## Goal\n\nDo the thing.\n",
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        assert!(
            lines.join("\n").contains("ctx_peak"),
            "the title must reach the pane, got {lines:?}"
        );
    }

    /// A document with no `title:` at all draws no `Description:` row,
    /// rather than an empty one. `parse_submission` is what refuses it, at
    /// submit time — the pane just has nothing to draw.
    #[test]
    fn a_task_with_no_title_draws_no_extra_line() {
        let repo = fixture("screen-no-description");
        write_pending(
            &repo,
            "wire",
            "---\nid: wire\ngroup: one\n---\n## Goal\n\nDo the thing.\n",
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _) = tasks_pane_lines(&groups, &pipelines, &state, 46);
        // Only the blank separator the loop always opens a task with, the
        // task row, its Pipeline: row and its Depends on: row — nothing
        // past it, since this document has no title to draw a Description:
        // row from. The document names no `pipeline:` either, and there is
        // no project default to show in its place any more.
        assert_eq!(
            lines,
            vec![
                String::new(),
                "  wire".to_string(),
                format!("    {:<LABEL_FIELD$}{}", "Pipeline:", TRIAL_UNASSIGNED),
                "    Depends on:  -".to_string(),
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
        let repo = fixture("screen-mixed-task-states");
        write_pending(
            &repo,
            "third",
            &document("third", "group: chain\ndepends_on: [second]\n", BODY),
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

        let (lines, _) = tasks_pane_lines(&groups, &pipelines, &state, 46);
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
        let repo = fixture("screen-done-no-repeat");
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

        let (lines, _) = tasks_pane_lines(&groups, &pipelines, &state, 46);
        assert!(
            lines.iter().all(|line| !line.trim_end().ends_with("done")),
            "{lines:?}"
        );
    }

    /// Every fact a wide pane draws about a task — its pipeline, what it
    /// depends on, its gate and its description — starts its value at the
    /// same column, in that order, the same as the mockup this task's own
    /// acceptance criterion is drawn from.
    #[test]
    fn every_row_in_a_wide_pane_starts_its_value_at_the_same_column() {
        let repo = fixture("screen-labelled-rows");
        write_pending(
            &repo,
            "tracking-open",
            &document(
                "tracking-open",
                "group: one\npipeline: default\ndepends_on: [tracking-core]\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let mut state = ScreenState::new();
        state
            .gates
            .insert(task_key(&groups[0].tasks[0]), "review".to_string());

        let (lines, _) = tasks_pane_lines(&groups, &pipelines, &state, 60);
        assert_eq!(
            lines,
            vec![
                String::new(),
                "  tracking-open".to_string(),
                "    Pipeline:    default".to_string(),
                "    Depends on:  tracking-core".to_string(),
                "    Gate:        review".to_string(),
                "    Description: tracking-open, done".to_string(),
            ],
            "{lines:?}"
        );
    }

    /// A right pane narrower than forty columns keeps every label, dropping
    /// its value to its own line underneath, indented two — never dropping
    /// the label itself, which the acceptance criterion rules out.
    #[test]
    fn a_narrow_right_pane_keeps_every_label_and_drops_its_value_below() {
        let repo = fixture("screen-narrow-labelled-rows");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: one\npipeline: default\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, _) = tasks_pane_lines(&groups, &pipelines, &state, 26);
        assert_eq!(
            lines,
            vec![
                String::new(),
                "  wire".to_string(),
                "    Pipeline:".to_string(),
                "      default".to_string(),
                "    Depends on:".to_string(),
                "      -".to_string(),
                "    Description:".to_string(),
                "      wire, done".to_string(),
            ],
            "{lines:?}"
        );
    }

    /// A reload can reorder `shown` out from under the cursor — a document
    /// just edited moves to the front of `list_groups`' own newest-first
    /// order — but the group the cursor was on stays highlighted, found
    /// again by name rather than by the index it no longer sits at.
    #[test]
    fn reload_keeps_the_cursor_on_the_same_group_after_a_reorder() {
        let repo = fixture("screen-reload-cursor");
        write_pending(&repo, "older", &document("older", "group: older\n", BODY));
        let mut groups = listed(&repo);
        let mut state = ScreenState::new();
        state.group_cursor = shown(&groups, &state)
            .iter()
            .position(|group| group.name == "older")
            .unwrap();

        // A second, newer document sorts ahead of the first — see
        // `list_groups`' own newest-first order — so a naive reload that
        // kept the cursor's index rather than its group would now be
        // pointing at this one instead.
        write_pending(&repo, "newer", &document("newer", "group: newer\n", BODY));
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
        let repo = fixture("screen-idle-draw");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();
        let mut last = None;
        let mut out = Vec::new();

        let routines = Vec::new();
        let routines_dir = repo.routines_dir();
        let panes = Panes {
            routines: &routines,
            routines_dir: &routines_dir,
            pipelines: &pipelines,
        };
        draw(&groups, &panes, &state, &mut last, &mut out);
        let after_first = out.len();
        assert!(after_first > 0, "the first draw must write the frame");

        draw(&groups, &panes, &state, &mut last, &mut out);
        assert_eq!(
            out.len(),
            after_first,
            "an unchanged frame must write nothing more:\n{}",
            String::from_utf8_lossy(&out)
        );
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
                "enter set   g clear   esc cancel",
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

    /// `p`'s own first screen: every task in the group, its own row, and a
    /// task whose document names its own `pipeline:` shown assigned to it
    /// already — one that names none shows unassigned instead, there being
    /// no project default left to seed it with.
    #[test]
    fn assign_pipelines_panel_lists_every_task_already_assigned_a_pipeline() {
        let repo = fixture("screen-trial-assign");
        // Built by hand rather than through `document`, which now fills in
        // `pipeline: default` — alpha's own point here is that it names
        // none, so the picker opens it unassigned.
        write_pending(
            &repo,
            "alpha",
            &format!("---\nid: alpha\ntitle: alpha, done\ngroup: chain\n---\n{BODY}"),
        );
        write_pending(
            &repo,
            "beta",
            &document(
                "beta",
                "group: chain\ndepends_on: [alpha]\npipeline: bugfix\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();

        let trial = TrialState::new(&pipelines, &groups[0]);
        let panel = trial_panel(
            &groups,
            &pipelines,
            &trial,
            LEFT_PANE_WIDTH + RIGHT_PANE_WIDTH,
        )
        .unwrap();
        let flat = panel.join("\n");

        assert!(flat.contains("trial chain"), "{flat}");
        assert!(flat.contains("assign one pipeline to every task"), "{flat}");
        assert!(flat.contains("alpha"), "{flat}");
        assert!(flat.contains(TRIAL_UNASSIGNED), "{flat}");
        assert!(flat.contains("beta"), "{flat}");
        assert!(flat.contains("bugfix"), "{flat}");
        // alpha is still unassigned, so the footer says so rather than
        // offering an `enter` that would silently refuse to advance.
        assert!(
            flat.contains("↑↓ task   ←→ pipeline   assign every task, then enter   esc cancel"),
            "{flat}"
        );
    }

    /// `←`/`→` on the first screen cycles the highlighted task's own
    /// assignment through every project pipeline, wrapping past either end
    /// rather than stopping there, and never touches any other task's own
    /// pick — an unassigned task the cursor never visited stays unassigned.
    #[test]
    fn left_right_cycles_only_the_highlighted_tasks_own_pipeline() {
        let repo = fixture("screen-trial-cycle");
        // Built by hand, the same as the panel test above — both tasks have
        // to open unassigned, and `document` would otherwise fill in
        // `pipeline: default` for them.
        write_pending(
            &repo,
            "alpha",
            &format!("---\nid: alpha\ntitle: alpha, done\ngroup: demo\n---\n{BODY}"),
        );
        write_pending(
            &repo,
            "beta",
            &format!("---\nid: beta\ntitle: beta, done\ngroup: demo\n---\n{BODY}"),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let group = &groups[0];
        let beta_key = task_key(&group.tasks[1]);
        assert!(
            !TrialState::new(&pipelines, group)
                .pipeline
                .contains_key(&beta_key),
            "a document naming no `pipeline:` opens unassigned"
        );

        let trial = TrialState::new(&pipelines, group);
        let Mode::Trial(trial) = handle_trial_key(group, &pipelines, trial, Key::Right) else {
            panic!("expected to stay on the trial picker");
        };

        // Unassigned, so `→` lands on whichever pipeline sorts first.
        let names = trial_pipeline_names(&pipelines);
        let expected = names[0];
        assert_eq!(
            trial.pipeline[&task_key(&group.tasks[0])],
            expected,
            "the highlighted task (alpha) moved on"
        );
        assert!(
            !trial.pipeline.contains_key(&beta_key),
            "beta was never under the cursor, so it must still be unassigned"
        );
    }

    /// `esc` off the second screen returns to the first without dropping
    /// anything either screen already picked — the acceptance criterion this
    /// task names explicitly.
    #[test]
    fn esc_off_the_skip_screen_returns_to_pipelines_without_losing_picks() {
        let repo = fixture("screen-trial-esc-back");
        write_pending(&repo, "solo", &document("solo", "group: audits\n", BODY));
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let group = &groups[0];
        let key = task_key(&group.tasks[0]);

        let mut trial = TrialState::new(&pipelines, group);
        trial.pipeline.insert(key.clone(), "bugfix".to_string());
        trial.stage = TrialStage::ChooseSkips;
        trial
            .skip
            .entry(key.clone())
            .or_default()
            .insert("checks".to_string());

        let Mode::Trial(trial) = handle_trial_key(group, &pipelines, trial, Key::Esc) else {
            panic!("expected to stay on the trial picker");
        };

        assert_eq!(trial.stage, TrialStage::AssignPipelines);
        assert_eq!(trial.pipeline[&key], "bugfix");
        assert!(trial.skip[&key].contains("checks"));
    }

    /// `p`'s own second screen: every task's own header names the pipeline
    /// the first screen left it with, its steps are listed under it, and a
    /// tick made against one task's steps never reaches another's — each
    /// keeps its own skip set, keyed by the task rather than by position.
    #[test]
    fn choose_skips_panel_keeps_a_separate_skip_set_per_task() {
        let repo = fixture("screen-trial-skips");
        write_pending(
            &repo,
            "alpha",
            &document("alpha", "group: chain\npipeline: bugfix\n", BODY),
        );
        write_pending(
            &repo,
            "beta",
            &document(
                "beta",
                "group: chain\ndepends_on: [alpha]\npipeline: default\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let group = &groups[0];

        let mut trial = TrialState::new(&pipelines, group);
        trial.stage = TrialStage::ChooseSkips;
        trial
            .skip
            .entry(task_key(&group.tasks[0]))
            .or_default()
            .insert("checks".to_string());

        let panel = trial_panel(
            &groups,
            &pipelines,
            &trial,
            LEFT_PANE_WIDTH + RIGHT_PANE_WIDTH,
        )
        .unwrap();
        let flat = panel.join("\n");

        assert!(flat.contains("choose steps to skip"), "{flat}");
        assert!(flat.contains("alpha · bugfix"), "{flat}");
        assert!(flat.contains("beta · default"), "{flat}");
        assert!(flat.contains("[x] checks"), "{flat}");
        // `beta`'s own `checks` — the same step id, a different task — is
        // never ticked by alpha's own skip set.
        assert_eq!(flat.matches("[x]").count(), 1, "{flat}");
        assert!(
            flat.contains("↑↓ move   space toggle   enter run   esc pipelines"),
            "{flat}"
        );
    }

    /// A pipeline with more steps than one line comfortably packs — `bugfix`
    /// at seven, against a realistic terminal width — wraps onto as many
    /// lines as it takes instead of running off the edge of the frame: every
    /// one of its steps still draws its own checkbox, and no line this panel
    /// draws is wider than what `overlay` can actually paint onto the frame
    /// underneath it. The regression this guards: before `checkbox_row_cap`,
    /// every step of a pipeline this long was packed onto one line with no
    /// wrap at all, so `overlay`'s own silent per-character stop cut the row
    /// short partway through, taking the popup's own right and bottom
    /// border with it, and left a step past the cut with no checkbox drawn
    /// at all even though the flattened cursor could still land on it.
    #[test]
    fn a_long_pipelines_steps_wrap_rather_than_run_off_the_panel() {
        let repo = fixture("screen-trial-skips-wrap");
        write_pending(
            &repo,
            "solo",
            &document("solo", "group: audits\npipeline: bugfix\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let group = &groups[0];
        let bugfix = pipelines.get("bugfix").unwrap();

        let mut trial = TrialState::new(&pipelines, group);
        trial.stage = TrialStage::ChooseSkips;

        let width = LEFT_PANE_WIDTH + RIGHT_PANE_WIDTH;
        let panel = trial_panel(&groups, &pipelines, &trial, width).unwrap();

        for step in &bugfix.steps {
            assert!(
                panel
                    .iter()
                    .any(|line| line.contains(&format!("[ ] {}", step.id))),
                "`{}` never drew a checkbox: {panel:?}",
                step.id
            );
        }
        assert!(
            panel.iter().all(|line| line.chars().count() <= width + 6),
            "a body line ran wider than {} + 6 columns of `boxed` overhead: {panel:?}",
            width
        );
        // The frame's own closing border survives — `boxed` always draws it
        // last — rather than the panel simply running out mid-box the way
        // it used to once `overlay` gave up partway through an over-wide
        // row.
        assert!(
            panel
                .last()
                .is_some_and(|line| line.starts_with('└') && line.ends_with('┘')),
            "{panel:?}"
        );
    }

    /// The same long pipeline, but drawn the way the screen actually draws
    /// it: `boxed` popup over a real [`two_pane_frame`], composed by the
    /// real [`overlay`]. This is the shape `look` reproduced live and the
    /// one the panel-level test above cannot see, because `overlay`'s
    /// truncation happens after `choose_skips_panel` has already handed its
    /// lines over — it writes a panel row onto the frame one character at a
    /// time and simply stops at the frame's own right edge, so an over-wide
    /// popup lost its right border on every row and its bottom border
    /// entirely, and any checkbox past the cut was never painted at all.
    ///
    /// Two tasks rather than one, so the assertion covers a panel tall
    /// enough to have a header, wrapped rows and a keys line all competing
    /// for the same width.
    #[test]
    fn the_skips_popup_survives_being_overlaid_on_the_frame() {
        let repo = fixture("screen-trial-skips-overlay");
        write_pending_two(
            &repo,
            "alpha",
            &document("alpha", "group: audits\npipeline: bugfix\n", BODY),
            "beta",
            &document("beta", "group: audits\npipeline: bugfix\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let bugfix = pipelines.get("bugfix").unwrap();

        let mut trial = TrialState::new(&pipelines, &groups[0]);
        trial.stage = TrialStage::ChooseSkips;

        let layout = Layout {
            left: LEFT_PANE_WIDTH,
            right: RIGHT_PANE_WIDTH,
            rows: Some(40),
        };
        let panel = trial_panel(&groups, &pipelines, &trial, checkbox_row_cap(layout)).unwrap();

        let mut frame = two_pane_frame(&[], &[], "groups", "audits", layout);
        overlay(&mut frame, &panel);
        let drawn = frame.join("\n");

        // Every step of both tasks is painted onto the frame itself, not
        // merely present in the panel that was handed to `overlay`. Counted
        // on a whole id rather than a bare substring, so `reproduce` does
        // not also count the `reproduce-again` row two lines under it.
        for step in &bugfix.steps {
            let needle = format!("[ ] {}", step.id);
            let drew = drawn
                .match_indices(&needle)
                .filter(|(at, _)| {
                    !drawn[at + needle.len()..]
                        .starts_with(|c: char| c.is_alphanumeric() || c == '-' || c == '_')
                })
                .count();
            assert_eq!(
                drew, 2,
                "`{}` drew {drew} checkboxes on the frame, not one per task:\n{drawn}",
                step.id
            );
        }

        // The popup's own bottom border survives the overlay. This is the
        // half `look` found missing outright: once a body row ran past the
        // frame's right edge, `overlay` stopped mid-row and the box simply
        // had no closing line at all. Matched on a run of dashes with no
        // `┬` in it, so the frame's own bottom border — which carries a
        // `┘` of its own, and a `┬` between the two panes — cannot stand in
        // for the popup's.
        assert!(
            frame.iter().any(|line| {
                let body = line.trim();
                body.starts_with('└')
                    && body.ends_with('┘')
                    && !body.contains('┬')
                    && body.chars().filter(|c| *c == '─').count() > 10
            }),
            "the popup's own bottom border never made it onto the frame:\n{drawn}"
        );

        // Every row of the popup still closes on its right border, which is
        // the other half `overlay` used to eat once a row ran over.
        let closing = frame
            .iter()
            .filter(|line| line.contains("[ ] "))
            .collect::<Vec<_>>();
        assert!(!closing.is_empty(), "{drawn}");
        for line in closing {
            assert!(
                line.trim_end().ends_with('│'),
                "a checkbox row lost its right border:\n{drawn}"
            );
        }

        // And the frame itself is still rectangular — no row was widened or
        // eaten by the popup sitting on it.
        let width = frame[0].chars().count();
        assert_eq!(width, layout.left + layout.right + 7, "{drawn}");
        assert!(
            frame.iter().all(|line| line.chars().count() == width),
            "a frame row changed width under the popup:\n{drawn}"
        );
    }

    /// Neither trial screen runs off the frame when what it is drawing is a
    /// long name rather than a long pipeline. This is the same defect
    /// `look` failed the first pass for, reached through the other input:
    /// the checkbox rows were bounded by `checkbox_row_cap`, but a task's
    /// own id and the group name in the popup's title were not, so a long
    /// enough id overflowed the popup exactly the same way and `overlay`
    /// ate the right border off every row again.
    ///
    /// Driven at the narrowest layout `layout_for` ever produces, since
    /// that is where the budget is tightest, and across both stages, since
    /// they build their rows separately.
    #[test]
    fn a_long_task_id_is_cut_rather_than_pushing_a_trial_popup_off_the_frame() {
        let repo = fixture("screen-trial-long-names");
        let long = "alpha-with-a-really-quite-long-task-identifier";
        write_pending_two(
            &repo,
            long,
            &document(
                long,
                "group: a-group-with-a-long-name-of-its-own\npipeline: bugfix\n",
                BODY,
            ),
            "beta",
            &document(
                "beta",
                "group: a-group-with-a-long-name-of-its-own\npipeline: bugfix\n",
                BODY,
            ),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();

        let layout = Layout {
            left: MIN_LEFT_PANE,
            right: MIN_RIGHT_PANE,
            rows: Some(40),
        };
        let frame_width = layout.left + layout.right + 7;

        for stage in [TrialStage::AssignPipelines, TrialStage::ChooseSkips] {
            let mut trial = TrialState::new(&pipelines, &groups[0]);
            trial.stage = stage;
            let panel = trial_panel(&groups, &pipelines, &trial, checkbox_row_cap(layout)).unwrap();

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
                "{stage:?} never cut the long id at all: \n{drawn}"
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
        let repo = fixture("panes-empty");
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new();

        let (lines, focus) = tasks_pane_lines(&groups, &pipelines, &state, 40);
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
    /// that need no submission to observe. Quitting is not among them any
    /// more: `q` is `run_screen`'s, so that it works from every mode.
    #[test]
    fn browsing_keys_select_move_focus_and_hide_queued_groups() {
        let repo = fixture("screen-browse");
        write_pending(&repo, "wire", &document("wire", "group: a\n", BODY));
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

    /// The screen opens with every already-queued group hidden — the mockup's
    /// own "0 of 5" frame — and `h` brings them straight back.
    #[test]
    fn the_screen_opens_with_queued_groups_hidden_and_h_brings_them_back() {
        let repo = fixture("screen-opens-hidden");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);

        let state = ScreenState::new();
        assert_eq!(
            state.hide_scope,
            HideScope::Pending,
            "the screen must open with queued groups hidden"
        );
        let shown = visible(&groups, state.hide_scope);
        assert!(shown.is_empty(), "the one group here is already queued");
        let rows = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows,
            vec!["  nothing to queue — 1 group hidden".to_string()],
            "{rows:?}"
        );

        let mut state = state;
        handle_browse_key(&groups, &mut state, Key::Char('h'));
        let shown = visible(&groups, state.hide_scope);
        assert_eq!(shown.len(), 1, "`h` brings the queued group back");
    }

    /// `h` cycles pending-only, plus queued, plus done, then wraps — three
    /// presses widen a group that started wholly archived into view and a
    /// fourth hides it again, the exact shape the mockup draws.
    #[test]
    fn h_cycles_three_scopes_and_wraps() {
        let repo = fixture("screen-h-cycle");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("finished.md"),
            "---\nid: finished\ntitle: finished\ngroup: finished\nstage: done\n---\nbody\n",
        )
        .unwrap();
        let groups = listed(&repo);
        let mut state = ScreenState::new();

        assert!(
            visible(&groups, state.hide_scope).is_empty(),
            "the opening scope shows nothing but what is still queueable"
        );

        handle_browse_key(&groups, &mut state, Key::Char('h'));
        assert_eq!(state.hide_scope, HideScope::PlusQueued);
        assert!(
            visible(&groups, state.hide_scope).is_empty(),
            "an archived-only group is not `queued` either"
        );

        handle_browse_key(&groups, &mut state, Key::Char('h'));
        assert_eq!(state.hide_scope, HideScope::PlusDone);
        assert_eq!(
            visible(&groups, state.hide_scope).len(),
            1,
            "the third widening is the one that reaches `done`"
        );

        handle_browse_key(&groups, &mut state, Key::Char('h'));
        assert_eq!(
            state.hide_scope,
            HideScope::Pending,
            "the third press wraps"
        );
        assert!(
            visible(&groups, state.hide_scope).is_empty(),
            "and the archived group is hidden again"
        );
    }

    /// The same story as the test above, but through `run_screen` itself: the
    /// opening frame's own title, empty-list line and footer, then the frame
    /// `h` draws next — every one of those is an acceptance criterion, and
    /// none of them is reachable by calling `groups_pane_lines` alone.
    #[test]
    fn the_first_drawn_frame_hides_queued_groups_and_h_shows_them() {
        let repo = fixture("screen-first-frame-hidden");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        already_queued(&repo, "wire");
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "h");

        let frames: Vec<&str> = drawn
            .split("\x1b[2J\x1b[H")
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
            "the opening title must count the hidden group as not shown:\n{opening}"
        );
        // Not the full sentence: the fallback (no-terminal) width this test
        // runs at is now the narrower `MIN_LEFT_PANE`, and `two_pane_frame`'s
        // own `pad_to` cuts this row off there the same as any other line too
        // long for its pane. `the_screen_opens_with_queued_groups_hidden_...`
        // below checks the untruncated string directly, off
        // `groups_pane_lines` rather than through a real frame.
        assert!(opening.contains("nothing to queue"), "{opening}");
        assert!(
            opening.contains("space select  f find  g gate  o open"),
            "`f find` and `o open` must sit between `space select` and \
             `enter queue now`:\n{opening}"
        );
        assert!(
            opening.contains("h show queued"),
            "the footer must offer to show the hidden groups:\n{opening}"
        );

        assert!(
            after_h.contains("groups  1 of 1"),
            "`h` must bring the group into the shown count:\n{after_h}"
        );
        assert!(after_h.contains("wire"), "{after_h}");
        assert!(
            after_h.contains("h show done"),
            "the footer must name the next widening once queued groups are shown:\n{after_h}"
        );
    }

    /// Pressing down from the last queueable group lands straight on the
    /// first queued one — the separator between the two halves is a drawn
    /// row, not a group `group_cursor` ever points at, so crossing it costs
    /// exactly the one key press an ordinary move between two groups would.
    #[test]
    fn pressing_down_across_the_separator_lands_on_the_first_queued_group() {
        let repo = fixture("screen-cross-separator");
        write_pending(&repo, "wire", &document("wire", "group: unqueued\n", BODY));
        // Written after the group above, so it is not already the newest —
        // being in the queue does not move a group inside its own half of
        // the list, only across the boundary between the two halves.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_pending(&repo, "cook", &document("cook", "group: cook\n", BODY));
        already_queued(&repo, "cook");
        let groups = listed(&repo);

        // `h` first, to bring the queued group back on screen at all — the
        // screen opens with it hidden — then one `Down` to cross from the
        // one queueable group onto it.
        let drawn = screen(&repo, groups, "h\x1b[B");

        let last = last_frame(&drawn);
        let cursor_row = last
            .lines()
            .find(|line| line.contains("│ >"))
            .unwrap_or_else(|| panic!("no cursor row in the final frame:\n{last}"));
        assert!(
            cursor_row.contains("cook") && cursor_row.contains("queued"),
            "one `Down` past the last queueable group must land on the \
             queued one, not the blank separator between them:\n{cursor_row}"
        );
    }

    /// `enter` submits only once something is actually selected — an empty
    /// batch has nothing to queue, and must not reach the dispatcher offer.
    #[test]
    fn enter_does_nothing_with_no_selection() {
        let repo = fixture("screen-enter-empty");
        write_pending(&repo, "wire", &document("wire", "group: a\n", BODY));
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
    fn the_screen_submits_a_selected_group_and_clears_its_documents() {
        let repo = fixture("screen-submit");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let groups = listed(&repo);

        // Tab onto the tasks pane, space to select the one group, enter to
        // submit it, `n` to decline the dispatcher.
        screen(&repo, groups, "\t \rn");

        assert!(
            repo.queue_dir().join("wire.md").exists(),
            "the document was not queued"
        );
        assert!(
            !repo.pending_dir().join("wire.md").exists(),
            "the queued group's document must be gone from pending"
        );
    }

    /// `enter` submits straight through to the dispatcher offer — nothing
    /// drawn in between, and no report text sitting above "start a
    /// dispatcher here now?" the way the old post-write report used to.
    #[test]
    fn a_clean_submission_reaches_the_dispatcher_offer_with_nothing_drawn_first() {
        let repo = fixture("gate-clean");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "\t \r");

        assert!(repo.queue_dir().join("wire.md").exists());

        // The last frame drawn is the dispatcher offer, and above it the
        // same report `queue add --from` prints for the same batch — one
        // pair of lines per task, then the branch — and nothing else.
        let last = last_frame(&drawn);
        let lines: Vec<&str> = last.lines().collect();
        assert_eq!(lines[0], "queued wire at `queued`", "{lines:?}");
        assert!(lines[1].trim().ends_with("wire.md"), "{lines:?}");
        assert!(lines[2].starts_with("  based on `"), "{lines:?}");
        assert_eq!(lines[3], "", "{lines:?}");
        assert_eq!(lines[4], "start a dispatcher here now?", "{lines:?}");
    }

    /// With a dispatcher already holding the queue's lock, a clean submission
    /// still writes — the task reaches the queue exactly as it would with no
    /// dispatcher running — but the screen shows the report rather than the
    /// offer: `Mode::Dispatch` is never entered, since starting a second
    /// dispatcher would only be refused once `commands::dispatch` actually
    /// ran. Takes a real `Lock::acquire` rather than writing the lock file's
    /// bytes by hand, so this exercises the same holder check every other
    /// caller of `Lock::holder` does.
    #[test]
    fn a_dispatcher_already_holding_the_lock_gets_the_report_not_the_offer() {
        let repo = fixture("gate-clean-locked");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let groups = listed(&repo);

        let _lock = crate::lock::Lock::acquire(&repo.lock_file(), false).unwrap();
        let pid = std::process::id();

        let (exit, drawn) = screen_exit(&repo, groups, "\t \r");

        // The write landed whether or not a dispatcher offer follows it.
        assert!(repo.queue_dir().join("wire.md").exists());
        // No `y` was ever offered to answer, so end of input is the same
        // dismissal any key gives `Mode::Outcome` — the screen just quits.
        assert_eq!(exit, ScreenExit::Quit);

        let last = last_frame(&drawn);
        assert!(
            !last.contains("start a dispatcher here now?"),
            "the dispatcher offer must never appear while one already holds the lock:\n{last}"
        );
        let lines: Vec<&str> = last.lines().collect();
        assert_eq!(
            lines.last().copied(),
            Some("press any key to continue"),
            "{lines:?}"
        );
        assert!(
            last.contains(&format!(
                "a dispatcher is already running (pid {pid}) — it picks these up on its \
                 next pass"
            )),
            "{last}"
        );
    }

    /// Two tasks in different groups whose `touches` globs overlap: an edge
    /// across group branches would stack one plan's pull request under
    /// another's — see the plan `chains-not-fans` — so `enter` writes both
    /// straight through with no `depends_on` added. The walk that used to
    /// pause over a collision like this is gone; `spoolway queue conflicts`
    /// is the only place the overlap is reported now.
    #[test]
    fn overlapping_touches_across_two_groups_writes_with_no_edge_added() {
        let repo = fixture("overlap-across-groups");
        write_pending(
            &repo,
            "left",
            &document("left", "group: one\ntouches: [src/dispatch.rs]\n", BODY),
        );
        write_pending(
            &repo,
            "right",
            &document("right", "group: two\ntouches: [src/dispatch.rs]\n", BODY),
        );
        let groups = listed(&repo);

        // Space selects the highlighted group, `j` moves onto the other,
        // space selects it too, `enter` submits both, `n` declines the
        // dispatcher.
        screen(&repo, groups, " j \rn");

        assert!(repo.queue_dir().join("left.md").exists());
        assert!(repo.queue_dir().join("right.md").exists());
        assert!(queued(&repo, "left").front.depends_on.is_empty());
        assert!(queued(&repo, "right").front.depends_on.is_empty());
    }

    /// A gate chosen from the screen — `g`, down onto the second step, enter
    /// to pick it — reaches the queued task file without ever rewriting the
    /// document it came from.
    #[test]
    fn a_gate_chosen_on_the_screen_reaches_the_queued_task() {
        let repo = fixture("screen-gate");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: one\ntouches: [src/wire.rs]\n", BODY),
        );
        let groups = listed(&repo);

        // Tab onto tasks, space to select, g to open the gate picker, down
        // onto the pipeline's second step (`review`), enter to pick it,
        // enter to submit, `n` to decline the dispatcher.
        screen(&repo, groups, "\t g\x1b[B\r\rn");

        let saved = std::fs::read_to_string(repo.queue_dir().join("wire.md")).unwrap();
        assert!(saved.contains("gate_at: review"), "{saved}");
        assert!(
            !repo.pending_dir().join("wire.md").exists(),
            "still pending"
        );
    }

    /// `p` on a group forks every one of its tasks, one arm each, on the
    /// pipeline the first screen assigned it and the steps the second
    /// screen ticked to skip — and the source document, never itself
    /// submitted, is left exactly where it was.
    #[test]
    fn a_trial_forks_every_task_on_its_own_assigned_pipeline() {
        let repo = fixture("screen-trial");
        write_pending(
            &repo,
            "solo",
            &document("solo", "group: audits\ntouches: [src/solo.rs]\n", BODY),
        );
        let groups = listed(&repo);
        let pipelines = Pipelines::builtin();
        let bugfix = pipelines.get("bugfix").unwrap();
        let checks_at = bugfix.steps.iter().position(|s| s.id == "checks").unwrap();

        // `p` opens the picker straight from the groups pane — no `Tab`
        // needed, since the whole group forks regardless of which pane has
        // focus. One `→` cycles `solo`'s own assignment from the project
        // default to `bugfix` (the two builtin pipelines sort `bugfix`
        // first), `enter` advances to the skip screen, `checks_at` more `↓`
        // walks onto its last step, `space` ticks it, `enter` launches, `n`
        // declines the dispatcher.
        let keys = format!("p\x1b[C\r{} \rn", "\x1b[B".repeat(checks_at));
        screen(&repo, groups, &keys);

        let arm = queued(&repo, "solo-1");
        assert_eq!(arm.front.pipeline.as_deref(), Some("bugfix"));
        assert_eq!(arm.front.skip, vec!["checks".to_string()]);
        assert_eq!(arm.front.group.as_deref(), Some("audits"));
        assert_eq!(arm.front.branch.as_deref(), Some("task/solo-1"));
        assert!(
            arm.front
                .trial
                .as_deref()
                .is_some_and(|id| id.starts_with('t')),
            "the trial id is freshly minted, not the group's own name or the task's own id: {:?}",
            arm.front.trial
        );

        assert!(
            repo.pending_dir().join("solo.md").exists(),
            "the source document is a template for the arm, not itself submitted"
        );
        assert!(
            !repo.queue_dir().join("solo.md").exists(),
            "the bare id is never queued by a trial"
        );
    }

    /// The picker's whole reason to exist, and the one case nothing covered:
    /// a document naming no pipeline at all. Every other trial test goes
    /// through `document`, which fills in `pipeline: default` unless the
    /// document names one — so all of them arrived already routed, and the
    /// unassigned task the assign screen is *for* was never driven end to
    /// end. It did not work: `build_trial_arm` hands `parse_submission` the
    /// source document, which refuses one with no `pipeline:`, so the whole
    /// batch was abandoned with `trial refused:` and nothing was minted.
    ///
    /// A bare `pipeline:` rather than no line at all, so `document`'s own
    /// fill-in steps aside and the document reads exactly as unassigned as
    /// one a person left blank by hand — the same shape `scripts/e2e`'s
    /// `task_doc` writes for this.
    #[test]
    fn a_trial_routes_a_document_that_names_no_pipeline_of_its_own() {
        let repo = fixture("screen-trial-unassigned");
        write_pending(
            &repo,
            "solo",
            &document(
                "solo",
                "group: audits\ntouches: [src/solo.rs]\npipeline:\n",
                BODY,
            ),
        );
        let groups = listed(&repo);

        // `p` opens the picker with `solo` drawn `(unset)`, which `enter`
        // alone will not advance past. One `→` lands it on `bugfix` — the
        // pipeline that sorts first, there being no current position to
        // cycle away from — `enter` advances to the skips screen, `enter`
        // launches with nothing ticked, and `n` declines the dispatcher.
        screen(&repo, groups, "p\x1b[C\r\rn");

        let arm = queued(&repo, "solo-1");
        assert_eq!(arm.front.pipeline.as_deref(), Some("bugfix"));
        assert_eq!(arm.front.group.as_deref(), Some("audits"));
        assert_eq!(arm.front.branch.as_deref(), Some("task/solo-1"));
    }

    /// A group of more than one task mints one arm per task, all sharing the
    /// one trial id, and a task that named a sibling in `depends_on` keeps
    /// waiting on it — remapped to that sibling's own minted id, since the
    /// bare id a trial's arms name is never itself queued.
    #[test]
    fn a_trial_remaps_depends_on_to_the_sibling_arms_own_minted_ids() {
        let repo = fixture("screen-trial-chain");
        write_pending(&repo, "alpha", &document("alpha", "group: chain\n", BODY));
        write_pending(
            &repo,
            "beta",
            &document("beta", "group: chain\ndepends_on: [alpha]\n", BODY),
        );
        let groups = listed(&repo);

        // `p`, `enter` twice past both screens (nothing changed — both tasks
        // keep the project default), `n` declines the dispatcher.
        screen(&repo, groups, "p\r\rn");

        let alpha = queued(&repo, "alpha-1");
        let beta = queued(&repo, "beta-1");
        assert!(alpha.front.trial.is_some(), "{:?}", alpha.front.trial);
        assert_eq!(
            alpha.front.trial, beta.front.trial,
            "both arms of the same trial share one minted id"
        );
        assert_eq!(
            beta.front.depends_on,
            vec!["alpha-1".to_string()],
            "beta's own depends_on must follow alpha into its minted id, not the bare one"
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
        let repo = fixture("screen-trial-two-launches-a");
        write_pending(&repo, "solo", &document("solo", "group: audits\n", BODY));
        let groups = listed(&repo);
        screen(&repo, groups, "p\r\rn");
        let first_trial = queued(&repo, "solo-1").front.trial;

        let repo = fixture("screen-trial-two-launches-b");
        write_pending(&repo, "solo", &document("solo", "group: audits\n", BODY));
        let groups = listed(&repo);
        screen(&repo, groups, "p\r\rn");
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
        let repo = fixture("screen-trial-mint");
        write_pending(&repo, "solo", &document("solo", "group: audits\n", BODY));
        already_queued(&repo, "solo-1");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("solo-2.md"),
            "---\nid: solo-2\ntitle: solo-2\nstage: done\n---\nbody\n",
        )
        .unwrap();
        let groups = listed(&repo);

        // `p`, `enter` twice past both screens, `n` declines the dispatcher.
        screen(&repo, groups, "p\r\rn");

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
        let repo = fixture("screen-trial-archived");
        std::fs::create_dir_all(repo.archive_dir()).unwrap();
        std::fs::write(
            repo.archive_dir().join("finished.md"),
            "---\nid: finished\ntitle: finished, done\ngroup: audits\nstage: done\n\
             pipeline: default\ndepends_on: [long-gone]\n---\nbody\n",
        )
        .unwrap();
        let groups = listed(&repo);

        // `h` twice widens the scope all the way to `done`, where the
        // archived group actually lists; `p` opens the picker on it, `enter`
        // twice past both screens, `n` declines the dispatcher.
        screen(&repo, groups, "hhp\r\rn");

        let arm = queued(&repo, "finished-1");
        assert_eq!(
            arm.front.depends_on,
            Vec::<String>::new(),
            "an archived predecessor is not durable, so the arm must not wait on it"
        );
    }

    /// An id that would fit its lane budget bare can still be refused once a
    /// trial's own suffix pushes it over — named with the budget and the
    /// overage, and nothing written for the arm.
    #[test]
    fn a_trial_arm_over_its_id_budget_is_refused() {
        let repo = fixture("screen-trial-budget");
        let pipelines = Pipelines::builtin();
        let default = pipelines.get("default").unwrap();
        let longest = longest_agent_step(default);
        // The shortest bare id whose `-1` arm already overflows the lane
        // budget by exactly one character — the same arithmetic
        // `crate::mux`'s own `check_task_id` test uses, just short two
        // characters for the suffix a mint adds.
        let overflow = crate::mux::LANE_NAME_MAX - crate::mux::lane_name(longest, "").len() + 1;
        let base_id = "a".repeat(overflow.saturating_sub(2));
        write_pending(
            &repo,
            &base_id,
            &document(&base_id, "group: audits\n", BODY),
        );
        let groups = listed(&repo);

        // `p`, `enter` twice past both screens — the task keeps its own
        // default assignment, which is what `overflow` was sized against.
        screen(&repo, groups, "p\r\r");

        assert!(
            repo.queue_dir().read_dir().unwrap().next().is_none(),
            "a refused trial writes nothing"
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
        let repo = fixture("screen-refused");
        write_pending(
            &repo,
            "bogus",
            &document("bogus", "group: one\nstage: taken\n", BODY),
        );
        // Groups list newest-written-first, and this test's key script walks
        // the list in a fixed order — so the two birth times are pulled
        // apart with a sleep rather than trusted to land far enough apart on
        // their own. A birth time has no `set_*` counterpart the way a
        // modification time does, so this cannot be pinned after the fact.
        // Group `two` (the good document), written second, ends up first.
        std::thread::sleep(std::time::Duration::from_millis(5));
        write_pending(&repo, "good", &document("good", "group: two\n", BODY));
        let groups = listed(&repo);

        // space selects `two`, the highlighted (newest) group; j moves to
        // `one`; space selects it too; enter submits both and is refused — a
        // refusal shows an outcome and never the dispatcher offer, which is
        // half of what this test guards; any key (`x`) dismisses it; space
        // deselects `one`, which is still highlighted; enter resubmits with
        // `two` alone, and `n` declines the dispatcher that one does offer.
        screen(&repo, groups, " j \rx \rn");

        assert!(
            repo.queue_dir().join("good.md").exists(),
            "the good document was never queued after the fixed resubmission"
        );
        assert!(
            !repo.queue_dir().join("bogus.md").exists(),
            "the refused document must never reach the queue"
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

    /// `q` ends the screen before anything is submitted, whatever was
    /// selected — cancelling is silent and writes nothing.
    #[test]
    fn quitting_queues_nothing() {
        let repo = fixture("screen-quit");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        screen(&repo, groups, "\t q");

        assert!(!repo.queue_dir().join("wire.md").exists());
    }

    /// `q` ends the screen from a mode that is not browsing — the bug this
    /// fixes. The confirmation panel `enter` used to open swallowed the key
    /// through its own catch-all arm, dropping back to browsing instead of
    /// quitting, so a person who pressed enter and then `q` saw nothing
    /// happen. `Mode::Dispatch` is where that same catch-all lives now.
    ///
    /// End of input ends the loop too, so a run that merely stops proves
    /// nothing. The frames are the evidence: `q` quitting means no further
    /// draw, and the footer is drawn once per browsing frame.
    #[test]
    fn q_quits_from_a_mode_that_is_not_browsing() {
        let repo = fixture("screen-quit-report");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        let groups = listed(&repo);
        // Computed before `groups` moves into `run_screen`, but the
        // footer's own text does not change across these two frames either
        // way — no group here is queued yet, and `h` is never pressed.
        let ordinary_footer = footer(&ScreenState::new());

        // Space selects, enter submits and puts the report up, `q` quits from
        // under it.
        let (exit, drawn) = screen_exit(&repo, groups, " \rq");

        assert_eq!(exit, ScreenExit::Quit);

        // Two browsing frames — one before the space, one before the enter —
        // and then the report, which draws no footer. A third would mean `q`
        // had dropped back to browsing rather than quitting.
        let frames = drawn.matches(&ordinary_footer).count();
        assert_eq!(
            frames, 2,
            "`q` drew another browsing frame instead of quitting"
        );
    }

    /// `y` on the report asks for a dispatcher. The screen only says so and
    /// stops — starting one is `queue_screen`'s, once the `TermGuard` it holds
    /// has come off — which is also why no test here can launch anything.
    #[test]
    fn answering_yes_asks_the_caller_to_start_a_dispatcher() {
        let repo = fixture("screen-dispatch-yes");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        let (exit, _) = screen_exit(&repo, groups, " \ry");

        assert_eq!(exit, ScreenExit::StartDispatcher);
        assert!(repo.queue_dir().join("wire.md").exists());
    }

    /// Anything but `y` declines. A stray `\r` — the kind a scripted input
    /// carries at the end of a line — must never be read as yes, or every
    /// double-tap on enter would start a dispatcher.
    #[test]
    fn a_stray_key_on_the_report_declines_the_dispatcher() {
        let repo = fixture("screen-dispatch-stray");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        let (exit, _) = screen_exit(&repo, groups, " \r\r");

        assert_eq!(exit, ScreenExit::Quit);
        assert!(
            repo.queue_dir().join("wire.md").exists(),
            "the submission still landed; only the dispatcher was declined"
        );
    }

    /// Nothing in the pending directory is not an error — the screen opens
    /// onto an empty list rather than refusing. Driven through `run_screen`
    /// with a scripted input, never `queue_screen`: that one reads the test
    /// process's own stdin, and whether it ever returns depends on what the
    /// harness handed it — a closed `/dev/null` ends the screen at once, a
    /// pipe with a writer that never closes keeps it redrawing forever,
    /// which is how this test once hung `cargo test` for ten minutes.
    #[test]
    fn opening_the_screen_with_nothing_pending_does_not_error() {
        let repo = fixture("screen-nothing-pending");
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
        let repo = fixture("opening-message-empty");
        let groups = listed(&repo);
        assert!(groups.is_empty());
        assert_eq!(
            opening_message(&repo, &groups),
            None,
            "an empty pending directory has nothing to say instead of opening"
        );

        let state = ScreenState::new();
        let shown = visible(&groups, state.hide_scope);
        let rows = groups_pane_lines(&groups, &shown, &state, 30);
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
        let repo = fixture("screen-empty-frame");
        let groups = listed(&repo);

        let (exit, drawn) = screen_exit(&repo, groups, "");

        assert_eq!(exit, ScreenExit::Quit);
        assert!(
            drawn.contains("groups  0 of 0"),
            "the empty screen still draws its title:\n{drawn}"
        );
        assert!(drawn.contains("nothing to queue"), "{drawn}");
        assert!(
            drawn.contains("q quit"),
            "the footer must draw too:\n{drawn}"
        );
        assert!(
            !drawn.contains("spoolway-plan"),
            "no skill may be named on this screen:\n{drawn}"
        );
    }

    /// A document with no readable `group:` is named, not silently skipped —
    /// the one diagnostic `pending::unreadable` exists for.
    #[test]
    fn opening_message_names_an_unreadable_document() {
        let repo = fixture("opening-message-unreadable");
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
    /// make `groups.is_empty()` false and hide the unreadable-document
    /// diagnostic behind that unrelated row — even though the document named
    /// above has nothing to do with the group already queued below.
    #[test]
    fn opening_message_still_names_an_unreadable_document_beside_an_unrelated_queued_group() {
        let repo = fixture("opening-message-unreadable-and-queued");
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
        let msg =
            opening_message(&repo, &groups).expect("the unreadable document must still be named");
        assert!(msg.contains("no-group.md"), "{msg}");
    }

    /// The regression the fix above overcorrected into: a stray document with
    /// no `group:` must not swallow the whole screen when a real, queueable
    /// group is sitting right beside it in the same directory. Before this,
    /// `opening_message` checked `unreadable` unconditionally, so a single
    /// bad document anywhere in pending refused to open the screen at all —
    /// verified against a real build, where a directory holding one good
    /// document and one stray one used to draw the good group's row and now
    /// printed "Nothing to list" instead.
    #[test]
    fn opening_message_opens_the_screen_past_a_stray_document_beside_a_real_group() {
        let repo = fixture("opening-message-stray-beside-real-group");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: real-group\n", BODY),
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
            "a queueable group beside a stray document must still open the screen"
        );
    }

    /// An empty pending directory used to be indistinguishable from a project
    /// with nothing queued at all, and `queue_screen` printed "No task
    /// documents" instead of opening. A group whose documents have already
    /// been submitted still has a row — built from the queue directory — so
    /// the screen has something to open onto even here.
    #[test]
    fn a_queue_only_group_still_opens_the_screen_over_an_empty_pending_directory() {
        let repo = fixture("screen-queue-only");
        // Not `already_queued`: that helper writes a minimal document with
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
            "hidden by default behind `h`, same as any other queued group"
        );
        let rows = groups_pane_lines(&groups, &shown, &state, 30);
        assert_eq!(
            rows,
            vec!["  nothing to queue — 1 group hidden".to_string()],
            "{rows:?}"
        );
    }

    // ----------------------------------------------------------- the open key

    /// `o` is gated exactly the way `g` is: with the groups pane focused it
    /// does nothing at all, rather than opening whatever the group cursor
    /// happens to sit on. Never touches a multiplexer, so this passes
    /// whatever backend the fixture's default config names.
    #[test]
    fn o_does_nothing_while_the_groups_pane_has_focus() {
        let repo = fixture("screen-open-groups-focus");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "o");

        assert!(
            !drawn.contains("press any key to continue"),
            "`o` with the groups pane focused must never reach `Mode::Outcome`:\n{drawn}"
        );
    }

    /// `o` on a headless run has no pane to open an editor in — headless
    /// refuses it the way `open_tab` already does — and the refusal is
    /// surfaced through `Mode::Outcome` rather than lost: an `Err` this
    /// discarded would leave a person pressing `o` on a headless run with no
    /// sign the key did anything at all.
    #[test]
    fn pressing_o_with_no_multiplexer_surfaces_the_refusal() {
        let mut repo = fixture("screen-open-headless");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        // `Tab` moves focus onto the tasks pane, where `o` is gated live —
        // see `open_highlighted`'s own doc comment.
        let drawn = screen(&repo, groups, "\to");

        let last = last_frame(&drawn);
        assert!(last.contains("o:"), "{last}");
    }

    // -------------------------------------------------------------- the filter

    /// Typing `fqueue` narrows the left pane to the groups whose name scores
    /// above the floor against it, and leaves the one group with nothing in
    /// common with the query off the list entirely.
    #[test]
    fn playing_f_queue_narrows_the_pending_pane_to_matching_names() {
        let repo = fixture("screen-filter-narrows");
        for name in ["queue-browse", "queue-second-group", "home-state"] {
            write_pending(
                &repo,
                name,
                &document(name, &format!("group: {name}\n"), BODY),
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

    /// `p` and `s` are reserved actions even while the filter box has focus
    /// — this task's own acceptance criterion on reaching a trial or a save
    /// from a filtered view — rather than one more character
    /// `handle_filter_key` would otherwise have appended to the query.
    #[test]
    fn p_is_reserved_while_the_filter_box_has_focus() {
        let repo = fixture("screen-filter-reserved-p");
        write_pending(&repo, "solo", &document("solo", "group: demo\n", BODY));
        let groups = listed(&repo);

        // `demo` has neither `p` nor `s` in it, so every one of these keys
        // reaches the filter as ordinary text except the last.
        let drawn = screen(&repo, groups, "fdemop");

        let last = last_frame(&drawn);
        assert!(
            last.contains("trial demo"),
            "`p` should have opened the trial picker rather than typing `p` into the query:\n{last}"
        );
        assert!(!last.contains("find: demop"), "{last}");
    }

    /// The same reservation, for `s`.
    #[test]
    fn s_is_reserved_while_the_filter_box_has_focus() {
        let repo = fixture("screen-filter-reserved-s");
        write_pending(&repo, "solo", &document("solo", "group: demo\n", BODY));
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "fdemos");

        let last = last_frame(&drawn);
        assert!(
            last.contains("save demo as a routine"),
            "`s` should have opened the save panel rather than typing `s` into the query:\n{last}"
        );
    }

    /// The filter footer names the ordinary queue actions a narrowed list
    /// still leaves live, rather than the query box's own mechanics —
    /// `type to narrow`, a distinct `enter`, and `q is a letter here` are
    /// all gone.
    #[test]
    fn the_filter_footer_shows_the_ordinary_queue_actions() {
        let repo = fixture("screen-filter-footer");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "f");

        let last = last_frame(&drawn);
        assert!(
            last.contains("↑↓ move   f find   p trial   s save routine   esc back"),
            "{last}"
        );
        assert!(!last.contains("type to narrow"), "{last}");
        assert!(!last.contains("q is a letter here"), "{last}");
        assert!(!last.contains("enter keep filter"), "{last}");
    }

    /// `h` hides a queued group from ordinary browsing, but a filter reaches
    /// it anyway — the acceptance criterion this task exists for. The
    /// matched group still draws its `queued` tail and no checkbox, exactly
    /// as it does when `h` is the one showing it.
    #[test]
    fn a_filter_reaches_a_queued_group_that_h_is_hiding() {
        let repo = fixture("screen-filter-reaches-queued");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: queue-browse\n", BODY),
        );
        already_queued(&repo, "wire");
        let groups = listed(&repo);

        // `hide_scope` starts at `HideScope::Pending` — see `ScreenState::new`
        // — so without the filter this group would not be drawn at all.
        let drawn = screen(&repo, groups, "fqueue");

        let last = last_frame(&drawn);
        // The pane's own title (`┬─ queue-browse ─…`) also carries the name,
        // so the row is picked out by carrying both the name and the
        // `queued` tail together — the title never carries the tail.
        let row = last
            .lines()
            .find(|line| line.contains("queue-browse") && line.contains("queued"))
            .unwrap_or_else(|| panic!("the queued group never made it onto the pane:\n{last}"));
        assert!(
            !row.contains('['),
            "a queued group draws no checkbox:\n{row}"
        );
    }

    /// `q` typed while the filter box has focus is kept as an ordinary
    /// character rather than quitting the screen, but the same key still
    /// ends the screen from browsing once the filter is closed — the two
    /// halves of this task's own acceptance criterion on `q`, in one test so
    /// neither can pass by accident while the other regresses.
    #[test]
    fn q_is_a_letter_in_the_filter_but_still_quits_from_browsing() {
        let repo = fixture("screen-filter-q");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: queue-browse\n", BODY),
        );
        let groups = listed(&repo);

        // `fq` types `q` into the filter — it must not quit here — `\r`
        // keeps that one-character filter and returns to browsing, and the
        // second `q` is what actually ends the screen.
        let (exit, drawn) = screen_exit(&repo, groups, "fq\rq");

        assert_eq!(exit, ScreenExit::Quit);
        // Four frames: the first ordinary browsing draw, the empty filter
        // box `f` just opened, the filter box holding `q` after it was
        // typed, and browsing again after `\r` — a fifth would mean `q` had
        // quit while still inside the filter instead of being kept as text.
        let frames: Vec<&str> = drawn.split("\x1b[2J\x1b[H").skip(1).collect();
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
        let repo = fixture("screen-filter-control-byte");
        write_pending(
            &repo,
            "wire",
            &document("wire", "group: queue-browse\n", BODY),
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

    /// One document under `.spoolway/routines/<folder>/`, the same bytes a
    /// `--from` entry would read.
    fn write_routine(repo: &Repo, folder: &str, id: &str, doc: &str) -> std::path::PathBuf {
        let dir = repo.routines_dir().join(folder);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.md"));
        std::fs::write(&path, doc).unwrap();
        path
    }

    /// `r` swaps the left pane for the folder tree, and its own footer names
    /// `→`/`←`/`space`/`enter`/`r`/`q` rather than the pending screen's keys.
    #[test]
    fn r_swaps_the_left_pane_for_the_routines_tree() {
        let repo = fixture("routines-r-swap");
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &document("audit-deps", "group: nightly\n", BODY),
        );

        let drawn = screen(&repo, Vec::new(), "r");
        let last = last_frame(&drawn);

        assert!(last.contains("routines  1 of 1"), "{last}");
        assert!(last.contains("nightly"), "{last}");
        assert!(last.contains("1 task"), "{last}");
        assert!(last.contains("→ open"), "{last}");
        assert!(last.contains("r pending"), "{last}");
    }

    /// `→` descends into a folder holding subfolders of its own, and `←`
    /// climbs back up out of it — `handle_routine_key` directly, since a
    /// nested descent is easier to state as a sequence of states than to
    /// read back off a rendered frame.
    #[test]
    fn arrow_right_descends_and_arrow_left_climbs_back_up() {
        let repo = fixture("routines-arrow-nav");
        // `maintenance` holds nothing of its own — only `weekly` — so `→`
        // has nothing to focus tasks on and descends instead.
        write_routine(
            &repo,
            "maintenance/weekly",
            "prune",
            &document("prune", "group: maintenance\n", BODY),
        );
        let routines = super::routines::list_routines(&repo).unwrap();

        let mut nav = RoutineNav::new();
        handle_routine_key(&routines, &mut nav, Key::Right);
        assert_eq!(nav.path, vec!["maintenance".to_string()]);
        assert_eq!(nav.focus, Focus::Groups);
        assert_eq!(
            routine_level(&routines, &nav.path).len(),
            1,
            "`weekly` is `maintenance`'s only subfolder"
        );

        handle_routine_key(&routines, &mut nav, Key::Left);
        assert!(nav.path.is_empty(), "back at the root");
    }

    /// A folder holding both its own documents and a subfolder must not
    /// have either shadowed by the other: the first `→` focuses this
    /// folder's own tasks pane rather than descending, and a second `→`
    /// from there descends into its subfolders — `→` is a two-step for
    /// exactly this shape, never a choice between the two. This is the
    /// shape a review round caught twice: first a fix that only ever
    /// descended, shadowing a folder's own documents; then a fix that only
    /// ever focused tasks, shadowing its subfolders in the other direction.
    #[test]
    fn a_folder_with_both_its_own_documents_and_a_subfolder_reaches_both() {
        let repo = fixture("routines-mixed-folder");
        write_routine(
            &repo,
            "maintenance",
            "sweep",
            &document("sweep", "group: maintenance\n", BODY),
        );
        write_routine(
            &repo,
            "maintenance/weekly",
            "prune",
            &document("prune", "group: maintenance\n", BODY),
        );
        let routines = super::routines::list_routines(&repo).unwrap();
        assert_eq!(routines[0].own, 1, "only `sweep` sits directly in it");
        assert_eq!(
            routines[0].tasks.len(),
            2,
            "`prune` still counts at or below it"
        );

        let mut nav = RoutineNav::new();
        handle_routine_key(&routines, &mut nav, Key::Right);
        assert!(nav.path.is_empty(), "the first → never descends");
        assert_eq!(nav.focus, Focus::Tasks);
        assert_eq!(
            highlighted_routine_folder(&routines, &nav).unwrap().tasks[nav.task_cursor].id,
            "sweep"
        );

        handle_routine_key(&routines, &mut nav, Key::Right);
        assert_eq!(
            nav.path,
            vec!["maintenance".to_string()],
            "the second → descends into its subfolders"
        );
        assert_eq!(nav.focus, Focus::Groups);
        assert_eq!(routine_level(&routines, &nav.path).len(), 1, "`weekly`");
    }

    /// `→` over a leaf folder's own tasks pane — nothing further down to
    /// open — must leave the cursor exactly where it was, not reset it back
    /// to the first document the way it would if this reused the same
    /// unconditional `task_cursor = 0` the first `→` into the pane sets.
    #[test]
    fn arrow_right_over_a_leaf_tasks_pane_moves_nothing() {
        let repo = fixture("routines-arrow-leaf-tasks");
        write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &document("audit-deps", "group: nightly\n", BODY),
        );
        write_routine(
            &repo,
            "nightly",
            "audit-docs",
            &document("audit-docs", "group: nightly\n", BODY),
        );
        let routines = super::routines::list_routines(&repo).unwrap();

        let mut nav = RoutineNav::new();
        handle_routine_key(&routines, &mut nav, Key::Right); // into the tasks pane
        handle_routine_key(&routines, &mut nav, Key::Down); // off the first document
        assert_eq!(nav.task_cursor, 1);

        handle_routine_key(&routines, &mut nav, Key::Right); // nothing further down
        assert_eq!(nav.task_cursor, 1, "the cursor must not jump back to 0");
        assert_eq!(nav.focus, Focus::Tasks, "and focus must not move either");
    }

    /// `r` a second time returns to the pending screen.
    #[test]
    fn r_again_returns_to_the_pending_screen() {
        let repo = fixture("routines-r-back");
        write_pending(&repo, "wire", &document("wire", "group: one\n", BODY));
        let groups = listed(&repo);

        let drawn = screen(&repo, groups, "rr");
        let last = last_frame(&drawn);

        assert!(last.contains("groups  1 of 1"), "{last}");
        assert!(last.contains("space select"), "{last}");
    }

    /// The one non-goal this feature draws a hard line at: the empty `r`
    /// screen names `.spoolway/routines/` rather than opening onto a blank
    /// pane a person could mistake for a project with no keys to press.
    #[test]
    fn the_empty_routines_pane_names_its_own_path() {
        let repo = fixture("routines-empty");

        // Checked against `routine_folder_lines` directly, at a width wide
        // enough to hold the whole path: the left pane's own fixed 25
        // columns — the same width the mockup itself draws — would truncate
        // a scratch test repo's own long path well before this could tell
        // "named the path" apart from "named nothing at all".
        let lines = routine_folder_lines(&repo.routines_dir(), &[], &RoutineNav::new(), 200);
        assert_eq!(
            lines,
            vec![format!("  nothing under {}", repo.routines_dir().display())]
        );
    }

    /// `enter` on a selected folder queues every document under it through
    /// `validate_batch`, under minted ids rather than the bare ones the
    /// documents themselves carry, with `group:` and the body untouched —
    /// and the source files under `.spoolway/routines/` left exactly where
    /// they were.
    #[test]
    fn enter_on_a_selected_folder_queues_every_document_under_minted_ids() {
        let repo = fixture("routines-enter-mints");
        let deps = write_routine(
            &repo,
            "nightly",
            "audit-deps",
            &document("audit-deps", "group: nightly\n", BODY),
        );
        let docs = write_routine(
            &repo,
            "nightly",
            "audit-docs",
            &document("audit-docs", "group: nightly\n", BODY),
        );
        let deps_text = std::fs::read_to_string(&deps).unwrap();
        let docs_text = std::fs::read_to_string(&docs).unwrap();

        // r (open routines), space (select `nightly`), enter (queue it).
        let (exit, _) = screen_exit(&repo, Vec::new(), "r \r");
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
    /// else in the document is.
    #[test]
    fn enter_remaps_depends_on_between_siblings_in_the_same_folder() {
        let repo = fixture("routines-enter-remaps-depends-on");
        write_routine(
            &repo,
            "chain",
            "split-fields",
            &document("split-fields", "group: chain\n", BODY),
        );
        write_routine(
            &repo,
            "chain",
            "scan-pending",
            &document(
                "scan-pending",
                "group: chain\ndepends_on: [split-fields]\n",
                BODY,
            ),
        );

        let (exit, _) = screen_exit(&repo, Vec::new(), "r \r");
        assert_eq!(exit, ScreenExit::Quit);

        let scan = queued(&repo, "scan-pending-1");
        assert_eq!(scan.front.depends_on, vec!["split-fields-1".to_string()]);
    }

    /// `space` over a single task in the routines pane's own tasks list
    /// queues that document alone, with its `depends_on` emptied before
    /// `validate_batch` ever sees it — so a task naming a sibling nobody
    /// queued is not refused for a dependency this solo pick dropped.
    #[test]
    fn space_on_a_solo_task_queues_it_alone_with_depends_on_emptied() {
        let repo = fixture("routines-solo-space");
        write_routine(
            &repo,
            "chain",
            "split-fields",
            &document("split-fields", "group: chain\n", BODY),
        );
        write_routine(
            &repo,
            "chain",
            "scan-pending",
            &document(
                "scan-pending",
                "group: chain\ndepends_on: [split-fields]\n",
                BODY,
            ),
        );

        // r (open routines), → (the folder has no subfolders, so this moves
        // focus onto its own tasks, already on `scan-pending` — the first
        // one in filename order), space (queue it alone).
        let (exit, drawn) = screen_exit(&repo, Vec::new(), "r\x1b[C ");
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
    /// there copies its documents into `.spoolway/routines/<name>/`
    /// unchanged, same ids.
    #[test]
    fn s_saves_a_pending_group_into_routines_unchanged() {
        let repo = fixture("routines-save");
        write_pending(
            &repo,
            "audit-deps",
            &document("audit-deps", "group: nightly\n", BODY),
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
            panel.contains("copies 1 document unchanged, same ids"),
            "{panel}"
        );

        let repo2 = fixture("routines-save-2");
        write_pending(
            &repo2,
            "audit-deps",
            &document("audit-deps", "group: nightly\n", BODY),
        );
        let groups2 = listed(&repo2);
        let drawn2 = screen(&repo2, groups2, "s\r");
        let last2 = last_frame(&drawn2);
        assert!(last2.contains("saved 1 document"), "{last2}");

        let saved = repo2.routines_dir().join("nightly").join("audit-deps.md");
        let original = std::fs::read_to_string(repo2.pending_dir().join("audit-deps.md")).unwrap();
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), original);
    }

    /// A review round caught this: `q` used to quit the whole screen while
    /// naming a routine, the same way it quits from browsing — so a name
    /// like `quarterly` could never be typed. The save panel reads `q` as
    /// an ordinary character instead, the same way `Mode::Filter`'s query
    /// does, and only `esc` backs out of it.
    #[test]
    fn q_is_a_letter_in_the_save_name_not_a_quit() {
        let repo = fixture("routines-save-q-is-a-letter");
        write_pending(
            &repo,
            "audit-deps",
            &document("audit-deps", "group: nightly\n", BODY),
        );
        let groups = listed(&repo);

        // The input runs out right after, so the screen ends either way —
        // through `q` breaking the loop under the old bug, or through the
        // pipe simply going empty under the fix. What tells the two apart
        // is what the panel drew on its way out: `q` broke before a second
        // draw could ever show it appended, and the fix draws it.
        let drawn = screen(&repo, groups, "sq");
        let last = last_frame(&drawn);

        assert!(
            last.contains(".spoolway/routines/nightlyq_"),
            "`q` must be typed into the name, not read as quit:\n{last}"
        );
    }

    /// The name is typed, so it can be typed as a path — and `join` would
    /// happily follow a `..` straight out of `.spoolway/routines/` and
    /// write a routine somewhere the `r` pane never reads back. One plain
    /// folder name is all `s` accepts.
    #[test]
    fn s_refuses_a_name_that_is_not_a_plain_folder_name() {
        let repo = fixture("routines-save-refuses-a-path");
        write_pending(
            &repo,
            "audit-deps",
            &document("audit-deps", "group: nightly\n", BODY),
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
    /// into a folder that already holds documents, rather than merging into
    /// it.
    #[test]
    fn s_refuses_a_folder_that_already_holds_documents() {
        let repo = fixture("routines-save-refuses-merge");
        write_pending(
            &repo,
            "audit-deps",
            &document("audit-deps", "group: nightly\n", BODY),
        );
        let groups = listed(&repo);
        write_routine(
            &repo,
            "nightly",
            "already-here",
            &document("already-here", "group: nightly\n", BODY),
        );

        let drawn = screen(&repo, groups, "s\r");
        let last = last_frame(&drawn);
        assert!(last.contains("already holds documents"), "{last}");
        assert!(
            !repo
                .routines_dir()
                .join("nightly")
                .join("audit-deps.md")
                .exists(),
            "nothing was written over the folder that was already there"
        );
    }

    // The `[issue_tracking]` open hook: `queue add` runs it once per document
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
            let repo = fixture("open-no-hook");
            let text = document("login", "group: demo\n", BODY);
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
        /// documents — refused, naming the group and the hook, before
        /// anything is queued or the hook is ever run.
        #[test]
        fn a_group_with_no_description_is_refused_once_a_hook_is_configured() {
            let mut repo = fixture("open-no-description");
            with_hook(&mut repo, "exit 1");
            let text = document("mirrored", "group: issue-mirror\n", BODY);
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
        /// group with no `group_description:` on any document queues cleanly.
        #[test]
        fn a_group_with_no_description_is_fine_with_no_hook_configured() {
            let repo = fixture("open-no-description-no-hook");
            let text = document("mirrored", "group: issue-mirror\n", BODY);
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

        /// `gather_documents` names a `--from -` stream entry `<stdin>#N`,
        /// never a path — [`readable_task_files`] must not hand that name to
        /// the hook as `SPOOLWAY_TASK_FILE` just because it looks like one:
        /// the empty string, not a name nothing can open.
        #[test]
        fn a_document_with_no_backing_file_gets_an_empty_task_file() {
            let mut repo = fixture("open-no-backing-file");
            with_hook(
                &mut repo,
                r#"printf '%s' "$SPOOLWAY_TASK_FILE" >"$(dirname "$SPOOLWAY_OUT")/task-file.seen"
                   echo "ticket=T-1" >"$SPOOLWAY_OUT""#,
            );
            let doc = document(
                "streamed",
                "group: streamed\ngroup_description: read from stdin\n",
                BODY,
            );
            let documents = vec![("<stdin>#1".to_string(), doc)];
            let mut tasks =
                validate_batch(&repo, &Pipelines::builtin(), Some("plan/demo"), &documents)
                    .unwrap();
            let task_files = readable_task_files(&documents);
            open_and_prefix(&repo, &documents, &task_files, &mut tasks, false, true).unwrap();

            let seen = std::fs::read_to_string(repo.tracking_dir().join("task-file.seen"));
            assert_eq!(
                seen.unwrap(),
                "",
                "`<stdin>#1` is not a path — the hook must see nothing rather than a name it \
                 cannot open"
            );
        }

        /// The hook runs once per document, in dependency order, and its
        /// `epic=`/`ticket=` answer — read from the file at `SPOOLWAY_OUT` —
        /// lands in the queued document's own frontmatter. The dependent's
        /// call also gets its parent's ticket id in
        /// `SPOOLWAY_DEPENDS_TICKETS`.
        #[test]
        fn the_hook_runs_per_document_in_dependency_order_and_writes_both_ids() {
            let mut repo = fixture("open-runs");
            with_hook(
                &mut repo,
                r#"echo "$SPOOLWAY_TASK $SPOOLWAY_DEPENDS_TICKETS" >>"$(dirname "$SPOOLWAY_OUT")/order.log"
                   { echo "epic=acme/app#42"; \
                     echo "ticket=acme/app#$(( 100 + $(wc -l <"$(dirname "$SPOOLWAY_OUT")/order.log") ))"; \
                   } >"$SPOOLWAY_OUT""#,
            );

            let parent = document(
                "scan-pending",
                "group: scanner-rework\ngroup_description: scanning rework\n",
                BODY,
            );
            let parent_path = write_doc(&repo, "scan-pending.md", &parent);
            let child = document(
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

        /// A document that already names a `ticket:` is reported `kept` and
        /// never reaches the hook at all — proven here by a hook that fails
        /// the moment it is ever invoked.
        #[test]
        fn a_document_already_naming_a_ticket_skips_the_hook() {
            let mut repo = fixture("open-kept");
            with_hook(&mut repo, "exit 1");
            let text = document(
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
        /// document it came from, so a second run of the same command sees
        /// it already there and resumes rather than opening a second set.
        #[test]
        fn a_failing_hook_queues_nothing_and_resumes_on_the_next_run() {
            let mut repo = fixture("open-fails-midbatch");
            with_hook(
                &mut repo,
                r#"if [ "$SPOOLWAY_TASK" = "split-fields" ]; then exit 1; fi
                   { echo "epic=acme/app#42"; echo "ticket=acme/app#43"; } >"$SPOOLWAY_OUT""#,
            );

            let first = document(
                "scan-pending",
                "group: scanner-rework\ngroup_description: scanning rework\n",
                BODY,
            );
            let first_path = write_doc(&repo, "scan-pending.md", &first);
            let second = document("split-fields", "group: scanner-rework\n", BODY);
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
                "nothing was queued, including the document that succeeded"
            );

            // The succeeded document's own id was written back into it, in
            // place — read straight off the file `--from` still names.
            let rewritten = std::fs::read_to_string(&first_path).unwrap();
            assert!(rewritten.contains("ticket: acme/app#43"));
            assert!(rewritten.contains("epic: acme/app#42"));

            // A second run over the same two documents: the first is now
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
            let mut repo = fixture("open-fails-first-call");
            with_hook(&mut repo, "exit 3");
            let text = document(
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
            // Nothing to write back either — the pending document is
            // untouched, since the hook never answered with anything.
            let untouched = std::fs::read_to_string(&path).unwrap();
            assert_eq!(untouched, text);
        }

        /// A slug a successful call secured is written back into its pending
        /// document when a later call in the batch fails, so the re-run reads
        /// it as `slug:` and pins the group's prefix — a hook answering a
        /// different slug on the re-run cannot displace the first one.
        #[test]
        fn a_secured_slug_survives_a_failed_batch_and_pins_the_re_run() {
            let mut repo = fixture("open-slug-writeback");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"if [ "$SPOOLWAY_TASK" = "auth-02" ]; then exit 1; fi
                   { echo "ticket=PROJ-13"; echo "slug=proj-12"; } >"$SPOOLWAY_OUT""#,
            );

            let a = document(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\n",
                BODY,
            );
            let a_path = write_doc(&repo, "auth-01.md", &a);
            let b = document("auth-02", "group: auth-rework\n", BODY);
            let b_path = write_doc(&repo, "auth-02.md", &b);
            queue_add(
                &repo,
                &Pipelines::builtin(),
                &from_args(&[&a_path, &b_path]),
                &repo.root,
                false,
            )
            .unwrap_err();

            // `auth-01`'s slug landed in its pending document, in place.
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
        /// document order — and it stays the winner across a mid-batch
        /// failure even when the documents were submitted back to front.
        /// Documents `[c, b, a]`, chained `a <- b <- c`: the hook answers a
        /// different valid slug for `a` and `b`, then fails for `c`. `a`'s
        /// slug is what every document of the group carries afterwards.
        #[test]
        fn the_dependency_order_winner_survives_reversed_document_order() {
            let mut repo = fixture("open-slug-dep-order");
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

            let a = document(
                "chain-a",
                "group: chain\ngroup_description: chained work\n",
                BODY,
            );
            let a_path = write_doc(&repo, "chain-a.md", &a);
            let b = document("chain-b", "group: chain\ndepends_on: [chain-a]\n", BODY);
            let b_path = write_doc(&repo, "chain-b.md", &b);
            let c = document("chain-c", "group: chain\ndepends_on: [chain-b]\n", BODY);
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

            // The re-run queues the lot; every document is prefixed `aa-1`.
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
            let mut repo = fixture("open-prefix");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"{ echo "epic=PROJ-12"; echo "ticket=PROJ-13"
                     echo "slug=proj-12"
                     echo "url=https://acme.atlassian.net/browse/PROJ-12"; } >"$SPOOLWAY_OUT""#,
            );

            let parent = document(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\n",
                BODY,
            );
            let parent_path = write_doc(&repo, "auth-01.md", &parent);
            let child = document(
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

        /// A routine fired by a job goes through the same ticket opening and
        /// prefixing `queue add --from` does, so one queue does not end up
        /// mixing `task/<slug>-<id>` and bare names by how each batch
        /// arrived (jobs review finding 6). The routine's own source is not
        /// written to — a `ticket:` landing there would make every later
        /// fire report it `kept` and reuse the first fire's ticket.
        #[test]
        fn a_fired_routine_opens_tickets_and_takes_the_prefix_like_queue_add() {
            let mut repo = fixture("open-routine-prefix");
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
            // real document — the routine's own file under
            // `.spoolway/routines/`, not the nonexistent queue path a
            // routine mint never gets written to before this call.
            assert_eq!(
                std::fs::read_to_string(repo.tracking_dir().join("task-file.seen")).unwrap(),
                source
            );
        }

        /// A routine has no document on disk for a failed batch's ids to be
        /// written back into, so the failure must not send a person to
        /// `pending/` to look for them: it says the ids went nowhere, and
        /// that running it again opens a second set.
        #[test]
        fn a_hook_failing_on_a_routine_says_the_ids_were_not_written_anywhere() {
            let mut repo = fixture("open-routine-fails");
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
                err.contains("ids      not written — no document on disk"),
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
        /// way the screen's own tests do.
        #[test]
        fn a_screen_submission_takes_the_prefix_like_queue_add() {
            let mut repo = fixture("open-screen-prefix");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"{ echo "ticket=PROJ-13"; echo "slug=proj-12"; } >"$SPOOLWAY_OUT""#,
            );
            write_pending(
                &repo,
                "wire",
                &document(
                    "wire",
                    "group: one\ngroup_description: wiring it up\n",
                    BODY,
                ),
            );
            let mut groups = listed(&repo);
            let mut state = ScreenState::new();
            handle_browse_key(&groups, &mut state, Key::Char(' '));

            let mode = begin_submission(
                &repo,
                &Pipelines::builtin(),
                "plan/demo",
                &mut groups,
                &mut state,
            );
            assert!(matches!(mode, Mode::Dispatch(_)), "{mode:?}");

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
            let mut repo = fixture("open-no-prefix");
            with_hook(
                &mut repo,
                r#"{ echo "ticket=PROJ-13"; echo "slug=proj-12"
                     echo "url=https://acme.atlassian.net/browse/PROJ-12"; } >"$SPOOLWAY_OUT""#,
            );

            let doc = document(
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
            let mut repo = fixture("open-bad-slug");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"{ echo "ticket=PROJ-13"; echo "slug=PROJ-12"; } >"$SPOOLWAY_OUT""#,
            );

            let doc = document(
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

        /// A `slug:` a person authored (or hand-edited) onto a document is
        /// held to the same `check_id` alphabet as one a hook answers: an
        /// invalid one is dropped, and the batch queues with no prefix rather
        /// than an invalid branch.
        #[test]
        fn an_authored_invalid_slug_is_not_turned_into_a_branch_prefix() {
            let mut repo = fixture("open-authored-bad-slug");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(&mut repo, r#"{ echo "ticket=PROJ-13"; } >"$SPOOLWAY_OUT""#);

            let doc = document(
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
            let mut repo = fixture("open-bad-url");
            with_hook(
                &mut repo,
                r#"{ echo "ticket=PROJ-13"; echo "url=/browse/PROJ-12"; } >"$SPOOLWAY_OUT""#,
            );

            let doc = document(
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
            let mut repo = fixture("open-prefix-resume");
            repo.config.issue_tracking.key_in_names = true;
            with_hook(
                &mut repo,
                r#"epic=$SPOOLWAY_EPIC
                   if [ -z "$epic" ] && [ "$SPOOLWAY_GROUP_SIZE" -gt 1 ]; then epic=PROJ-12; fi
                   { echo "epic=$epic"; echo "ticket=t-$SPOOLWAY_TASK"
                     echo "slug=proj-12"
                     echo "url=https://acme.atlassian.net/browse/${epic:-PROJ-13}"; } >"$SPOOLWAY_OUT""#,
            );

            let a = document(
                "auth-01",
                "group: auth-rework\ngroup_description: auth rework\n",
                BODY,
            );
            let a_path = write_doc(&repo, "auth-01.md", &a);
            let b = document("auth-02", "group: auth-rework\n", BODY);
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
            // writes it.
            let c = document("auth-03", "group: auth-rework\n", BODY);
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
        /// stripped off its group for the epic lookup, so a fresh document
        /// naming a *different* group that merely shares the suffix does not
        /// inherit that sibling's epic.
        #[test]
        fn an_invalid_sibling_slug_is_not_a_recognised_prefix_for_the_epic_lookup() {
            let mut repo = fixture("open-bad-sibling-slug");
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
            let doc = document(
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
            let repo = fixture("tool-gate-no-hook");
            assert!(unmet_requirements(&repo).is_empty());
        }

        /// A declared floor this machine's own `cargo` already clears —
        /// `unmet_requirements` reports nothing, and the gate proceeds
        /// without drawing at all.
        #[test]
        fn a_met_requirement_draws_nothing() {
            let mut repo = fixture("tool-gate-met");
            with_versioned_hook(&mut repo, "0.0.1");
            assert!(unmet_requirements(&repo).is_empty());

            let mut input = keys("");
            let mut out = Vec::new();
            let skip = tool_requirements_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
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
            let mut repo = fixture("tool-gate-no-tty");
            with_versioned_hook(&mut repo, "999.0.0");

            let mut input = keys("");
            let mut out = Vec::new();
            let skip = tool_requirements_gate_with(
                &repo,
                false,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
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
            let mut repo = fixture("tool-gate-enter");
            with_versioned_hook(&mut repo, "999.0.0");

            let mut input = keys("\r");
            let mut out = Vec::new();
            let skip = tool_requirements_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap();
            assert!(skip);
            let printed = String::from_utf8(out).unwrap();
            assert!(printed.contains("[enter] queue anyway"), "{printed}");
        }

        /// `esc` is the one path that must reach the caller as `GateCancelled`
        /// — [`open_and_prefix`]'s own callers turn that into `Mode::Browsing`
        /// rather than a refusal, since nothing here failed.
        #[test]
        fn esc_over_the_gate_cancels() {
            let mut repo = fixture("tool-gate-esc");
            with_versioned_hook(&mut repo, "999.0.0");

            let mut input = keys("\x1b");
            let mut out = Vec::new();
            let err = tool_requirements_gate_with(
                &repo,
                true,
                &mut input,
                &mut out,
                Some(crate::platform::TermGuard::inert),
            )
            .unwrap_err();
            assert!(err.downcast_ref::<GateCancelled>().is_some(), "{err:#}");
        }

        /// Review finding 1: `Mode::Browsing` is exactly the mode the
        /// screen was already showing when `enter` fired the submission
        /// that hit the gate, so returning it on `esc` would render the
        /// same frame `draw`'s own unchanged-frame check already has
        /// cached and never repaint over the gate's raw-printed block.
        /// `refusal_mode` must hand back a *different* mode instead, so the
        /// next `draw` clears the screen — `Mode::Outcome` is what every
        /// other refusal here already uses for exactly that reason.
        #[test]
        fn refusal_mode_never_returns_to_the_frame_that_was_already_on_screen() {
            let cancelled = refusal_mode("submission refused", GateCancelled.into());
            assert!(
                !matches!(cancelled, Mode::Browsing),
                "esc must not redraw the identical frame `enter` was pressed on: {cancelled:?}"
            );
            assert!(matches!(cancelled, Mode::Outcome(_)), "{cancelled:?}");

            let refused = refusal_mode("submission refused", anyhow::anyhow!("bad batch"));
            match refused {
                Mode::Outcome(msg) => assert!(msg.contains("bad batch"), "{msg}"),
                other => panic!("{other:?}"),
            }
        }

        /// A tool the declared floor names but that is not on PATH at all is
        /// unmet too — a submit gate has no sibling check to leave that gap
        /// to the way `doctor` does, so it draws the same box with "not on
        /// PATH" in place of a version and a location.
        #[test]
        fn a_tool_missing_from_path_entirely_is_unmet() {
            let mut repo = fixture("tool-gate-missing-tool");
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
        /// `interactive: false` — `queue_add_documents`'s own path when
        /// `crate::ask::interactive()` says nobody is there — the same
        /// no-tty branch the test above drives directly.
        #[test]
        fn open_and_prefix_skips_open_tickets_when_a_requirement_is_unmet() {
            let mut repo = fixture("tool-gate-open-and-prefix");
            with_versioned_hook(&mut repo, "999.0.0");
            let doc = document(
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

            open_and_prefix(&repo, &[], &[], &mut tasks, false, true).unwrap();

            assert_eq!(tasks[0].extra_str("ticket"), "");
            assert_eq!(tasks[0].extra_str("epic"), "");
            assert!(
                std::fs::read_dir(repo.tracking_dir())
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
                "the hook must never have been called"
            );
        }
    }

    mod reset_for_reuse_tests {
        use super::*;

        /// A document in the shape a queued or archived task actually has on
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
cut_from: task/cursor-in-gap
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
                "cut_from:",
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

        /// A document already free of every stamped key — the shape a
        /// producer actually writes — reads back unchanged: nothing here has
        /// anything to drop, and re-serialising a mapping that lost no keys
        /// is a no-op on its contents (block style throughout, so the
        /// re-serialised form matches the written one byte for byte).
        #[test]
        fn a_document_with_no_stamped_keys_round_trips() {
            let doc = document("solo", "touches:\n- src/**\ngroup: g\n", BODY);
            let reset = reset_for_reuse("solo.md", &doc).unwrap();
            assert_eq!(reset, doc);
        }

        /// `parse_submission` refuses `STAMPED_DOC` outright, over `stage:`
        /// — the exact refusal a re-used queued or archived document hits
        /// today. Resetting it first is what lets it reach `parse_submission`
        /// at all.
        #[test]
        fn parse_submission_refuses_the_stamped_document_but_not_its_reset() {
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

    /// `queue unqueue` on a task that has not started: the document goes
    /// back to pending with the stamped keys dropped — the board's own
    /// unqueue, from a script — and the queue file is gone.
    #[test]
    fn queue_unqueue_carries_a_queued_task_back_to_pending() {
        let repo = fixture("queue-unqueue");
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

        // And it is a document again: the same path in queues it back.
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
        let repo = fixture("queue-unqueue-all");
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
        let repo = fixture("queue-unqueue-all-force");
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
        let repo = fixture("queue-unqueue-refusal");
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
    /// unqueues it, leaving no checkout field behind on the document that
    /// reaches pending.
    #[test]
    fn queue_unqueue_refuses_a_started_task_without_force_and_tears_down_with_it() {
        let repo = fixture("queue-unqueue-started");
        let pipelines = Pipelines::builtin();
        add(&repo, "solo", &[]);

        // No workspace recorded, no real worktree cut: `tear_down_checkout`
        // treats an already-gone checkout as a fine outcome rather than an
        // error, and this test's own job is only the road around it — that
        // the reserved fields are cleared before the document reaches
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
        let repo = fixture("queue-unqueue-pending-conflict");
        add(&repo, "login", &[]);
        std::fs::create_dir_all(repo.pending_dir()).unwrap();
        std::fs::write(
            repo.pending_dir().join("login.md"),
            document("login", "group: demo\n", BODY),
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
        let repo = fixture("queue-unqueue-force-pending-conflict");
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
            document("solo", "group: demo\n", BODY),
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
        let repo = fixture("queue-add-dry-run");
        let text = document("login", "group: demo\n", BODY);
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
            "the document itself is left where it was"
        );

        // A broken document still fails the dry run, the way the real thing
        // would — that is what it is for.
        let broken = write_doc(&repo, "broken.md", &document("nogroup", "", BODY));
        let mut args = from_args(&[&broken]);
        args.dry_run = true;
        let err = queue_add(&repo, &Pipelines::builtin(), &args, &repo.root, false).unwrap_err();
        assert!(format!("{err:#}").contains("`group:`"), "{err:#}");
    }

    /// `queue add --from` a path under this project's own pending directory:
    /// once the batch is written, the source document is gone from there —
    /// it reached the queue, so it is not still waiting to go there — and
    /// unqueueing the same task afterwards is free to write its clean
    /// document back without tripping the "newer draft" refusal a leftover
    /// copy would cause.
    #[test]
    fn queue_add_from_the_pending_directory_removes_its_own_source() {
        let repo = fixture("queue-add-from-pending");
        let text = document("beta", "group: one\n", BODY);
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
        // to write beta's clean document back, with nothing already sitting
        // in its way.
        crate::status::unqueue_task(&repo, "beta").unwrap();
        assert!(
            path.exists(),
            "unqueue must be able to write beta's document back to pending"
        );
        assert!(
            !repo.queue_dir().join("beta.md").exists(),
            "beta's queue file must be gone once it is unqueued"
        );
    }

    /// A `--from` path outside this project's own pending directory is read
    /// and left exactly where it is — only a document this batch's own inbox
    /// held is ever removed.
    #[test]
    fn queue_add_from_outside_pending_leaves_the_source_alone() {
        let repo = fixture("queue-add-from-elsewhere");
        let text = document("login", "group: demo\n", BODY);
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

    /// A document may name the branch it is cut from and merges into. One
    /// that does keeps it — verified against the repository's own branches
    /// first — and one that does not takes the submission's own `--base`
    /// instead.
    #[test]
    fn a_document_naming_its_own_base_is_cut_from_that_branch() {
        let repo = fixture("document-base");
        let git = |args: &[&str]| crate::repo::run(&repo.root, "git", args).unwrap();
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "-q", "--allow-empty", "-m", "root"]);
        git(&["branch", "release/1.x"]);

        let own = write_doc(
            &repo,
            "own.md",
            &document("own", "group: demo\nbase: release/1.x\n", BODY),
        );
        let plain = write_doc(&repo, "plain.md", &document("plain", "group: demo\n", BODY));
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
            "the document's own base is what its worktree is cut from"
        );
        assert_eq!(
            queued(&repo, "plain").front.base.as_deref(),
            Some("plan/demo"),
            "a document without `base:` takes the submission's own `--base`"
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
            &document("dep", "group: demo\ndepends_on: [own]\n", BODY),
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
    /// document: a branch the repository does not have, a name git will not
    /// accept, and anything that would reach git as a flag.
    #[test]
    fn a_document_base_that_is_not_a_local_branch_is_refused() {
        let repo = fixture("document-base-refused");
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
                &document("t", &format!("group: demo\nbase: {base}\n"), BODY),
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
}
