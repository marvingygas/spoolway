//! `spoolway jobs`: `jobs list` and `jobs run` for scripts, and the bare
//! `spoolway jobs` screen a person writes a job from. The screen is the only
//! thing that writes a job — it walks the routine, the cron expression and the
//! pipeline, then saves through [`crate::jobs::write`]. No `jobs add`, no
//! config key.

use std::path::{Path, PathBuf};

use chrono::Local;

use super::queue::{
    Focus, RoutineNav, handle_routine_key, highlighted_routine_folder, labeled_row, layout,
    render_routines, two_pane_frame, window,
};
use super::routines::RoutineFolder;
use super::*;
use crate::jobs::{self, Job, JobSpec, Scope};
use crate::screen::{Key, PollableRead, RawStdin, overlay, pad_to, panel, read_key};
use crate::task::Task;

/// `spoolway jobs list` — every job across both stores, with when it fires
/// next and when it last fired. `--json` prints the same rows for a script.
pub fn jobs_list(repo: &Repo, json: bool) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(json)?;
    }

    let jobs = jobs::load(repo)?;
    let now = Local::now();
    let now_epoch = now.timestamp();

    if json {
        let rows: Vec<Row> = jobs.iter().map(|job| Row::build(repo, job)).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    let rows: Vec<[String; 6]> = jobs
        .iter()
        .map(|job| {
            [
                job.name.clone(),
                job.scope.label().to_string(),
                job.spec.schedule.clone(),
                job.spec.pipeline.clone(),
                next_cell(job),
                last_cell(jobs::last_fired(repo, &job.name), now_epoch),
            ]
        })
        .collect();

    let headers = ["NAME", "SCOPE", "SCHEDULE", "PIPELINE", "NEXT", "LAST"];
    let mut widths = headers.map(str::len);
    for row in &rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(cell.chars().count());
        }
    }

    // The last column is not padded — nothing follows it.
    let line = |cells: &[String; 6]| {
        let mut out = String::new();
        for (column, cell) in cells.iter().enumerate() {
            if column + 1 == cells.len() {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{cell:<width$}  ", width = widths[column]));
            }
        }
        out.trim_end().to_string()
    };

    println!("{}", line(&headers.map(str::to_string)));
    for row in &rows {
        println!("{}", line(row));
    }

    let enabled = jobs.iter().filter(|job| job.spec.enabled).count();
    println!();
    println!(
        "{} job{}, {enabled} enabled.",
        jobs.len(),
        if jobs.len() == 1 { "" } else { "s" }
    );
    let stores = [repo.user_jobs_file(), repo.jobs_file()];
    let labels: Vec<String> = stores.iter().map(|path| store_label(repo, path)).collect();
    let width = labels.iter().map(|label| label.len()).max().unwrap_or(0);
    for (path, label) in stores.iter().zip(&labels) {
        let count = jobs.iter().filter(|job| &job.source == path).count();
        println!("  {label:<width$}   {count}");
    }

    Ok(())
}

/// `spoolway jobs run <name>` — fire one job now, ignoring its schedule.
pub fn jobs_run(repo: &Repo, pipelines: &Pipelines, name: &str, in_lane: bool) -> Result<()> {
    refuse_from_lane("the queue is mutated", in_lane)?;

    let jobs = jobs::load(repo)?;
    let job = jobs.iter().find(|job| job.name == name).ok_or_else(|| {
        anyhow::anyhow!(
            "no job named `{name}` — run `spoolway jobs list` to see the jobs in both stores"
        )
    })?;

    let base = repo.branch()?;
    let tasks = fire_job(repo, pipelines, job, &base)?;

    for task in &tasks {
        println!("queued {} at `{}`", task.id(), crate::pipeline::QUEUED);
    }
    println!(
        "  {} document{} from routine `{}` queued under pipeline `{}`.",
        tasks.len(),
        if tasks.len() == 1 { "" } else { "s" },
        job.spec.routine,
        job.spec.pipeline
    );
    Ok(())
}

/// Fire one job now, ignoring its schedule: queue its routine under the job's
/// own pipeline and record the manual firing so the LAST column updates and
/// the dispatcher's overlap guard sees this run. Shared by `jobs run` and the
/// screen's `r`.
fn fire_job(repo: &Repo, pipelines: &Pipelines, job: &Job, base: &str) -> Result<Vec<Task>> {
    // Parsed only to fail fast on a job whose expression is broken — firing
    // ignores the schedule itself.
    crate::cron::Cron::parse(&job.spec.schedule).map_err(|err| {
        anyhow::anyhow!(
            "job `{}` will not run — its schedule `{}` in {} does not parse: {err}. Fix it \
             in that file (or run `spoolway doctor` for the full check).",
            job.name,
            job.spec.schedule,
            store_label(repo, &job.source)
        )
    })?;

    let tasks = crate::commands::queue_routine_target(
        repo,
        pipelines,
        base,
        &job.target(repo)?,
        &job.spec.pipeline,
    )?;
    jobs::record_manual_fire(repo, &job.name, &tasks)?;
    Ok(tasks)
}

/// One `--json` row — the same facts the table shows, plus the routine path
/// and the store file. `next` and `last_fired` are always present, `null`
/// when they do not apply; `schedule_error` appears only on a row whose
/// expression will not parse.
#[derive(serde::Serialize)]
struct Row {
    name: String,
    scope: crate::jobs::Scope,
    schedule: String,
    pipeline: String,
    routine: String,
    enabled: bool,
    source: String,
    /// Local RFC 3339, or `null` when the job is paused, the expression will
    /// not parse, or it can never fire.
    next: Option<String>,
    /// Epoch seconds of the last firing, or `null`.
    last_fired: Option<i64>,
    /// The parser's message. Omitted entirely on a row whose schedule
    /// parses, so a script can test the key's presence.
    #[serde(skip_serializing_if = "Option::is_none")]
    schedule_error: Option<String>,
}

impl Row {
    fn build(repo: &Repo, job: &Job) -> Row {
        let schedule_error = crate::cron::Cron::parse(&job.spec.schedule).err();
        Row {
            name: job.name.clone(),
            scope: job.scope,
            schedule: job.spec.schedule.clone(),
            pipeline: job.spec.pipeline.clone(),
            routine: job.spec.routine.clone(),
            enabled: job.spec.enabled,
            source: store_label(repo, &job.source),
            next: match job.spec.enabled {
                true => jobs::next_fire(&job.spec.schedule).map(|when| when.to_rfc3339()),
                false => None,
            },
            last_fired: jobs::last_fired(repo, &job.name),
            schedule_error,
        }
    }
}

/// The NEXT column: `paused` for a disabled job, else a relative "in 5h 48m"
/// when soon, a weekday and time within the week, or a full date beyond it.
fn next_cell(job: &Job) -> String {
    if !job.spec.enabled {
        return "paused".to_string();
    }
    if crate::cron::Cron::parse(&job.spec.schedule).is_err() {
        return "bad expr".to_string();
    }
    let Some(when) = jobs::next_fire(&job.spec.schedule) else {
        return "never".to_string();
    };
    let delta = when - Local::now();
    if delta <= chrono::TimeDelta::days(1) {
        jobs::until(delta)
    } else if delta <= chrono::TimeDelta::days(7) {
        when.format("%a %H:%M").to_string()
    } else {
        when.format("%a %-d %b %H:%M").to_string()
    }
}

/// The LAST column: `-` when a job has never fired, else `ok, 19h ago`.
fn last_cell(last_fired_at: Option<i64>, now_epoch: i64) -> String {
    match last_fired_at {
        None => "-".to_string(),
        Some(fired_at) => format!("ok, {}", ago(now_epoch - fired_at)),
    }
}

/// A coarse "19h ago" / "3d ago" for the LAST column.
fn ago(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let minutes = seconds / 60;
    if minutes < 1 {
        return "just now".to_string();
    }
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    format!("{}d ago", hours / 24)
}

/// A store path for display: relative to the checkout when it is inside it,
/// `~`-prefixed when it is under the home directory, absolute otherwise —
/// the shape the plan's mockup draws.
fn store_label(repo: &Repo, path: &Path) -> String {
    if path.starts_with(&repo.checkout) {
        return crate::fmt::relative(&repo.checkout, path);
    }
    if let Some(home) = crate::platform::home_dir()
        && let Ok(under) = path.strip_prefix(&home)
    {
        return format!("~/{}", under.display());
    }
    path.display().to_string()
}

// ---------------------------------------------------------------------------
// The screen. Bare `spoolway jobs` opens it. It lists both stores' jobs,
// shows the highlighted one in full, and walks three panels — the routines
// browser (reused from `queue`), a cron field, and a pipeline picker — to
// write one. `e` edits through the same three, `space` pauses, `x` deletes,
// `r` fires now.
// ---------------------------------------------------------------------------

/// A job being written, carried through the routine → schedule → pipeline
/// walk. `editing` is `Some` for `e` over an existing job and `None` for `n`;
/// a new job takes its name from the routine's own leaf, an edited one keeps
/// the name it had.
#[derive(Debug, Clone)]
struct Draft {
    editing: Option<String>,
    routine: String,
    schedule: String,
    pipeline: String,
    scope: Scope,
    enabled: bool,
}

impl Draft {
    fn new(pipelines: &Pipelines) -> Draft {
        Draft {
            editing: None,
            routine: String::new(),
            schedule: String::new(),
            pipeline: pipelines.default.clone(),
            scope: Scope::User,
            enabled: true,
        }
    }

    fn from_job(job: &Job) -> Draft {
        Draft {
            editing: Some(job.name.clone()),
            routine: job.spec.routine.clone(),
            schedule: job.spec.schedule.clone(),
            pipeline: job.spec.pipeline.clone(),
            scope: job.scope,
            enabled: job.spec.enabled,
        }
    }

    /// The name the job is saved under: the original when editing, else the
    /// file stem of the routine it points at (`nightly/audit.md` → `audit`,
    /// the folder `nightly` → `nightly`).
    fn name(&self) -> String {
        if let Some(name) = &self.editing {
            return name.clone();
        }
        Path::new(&self.routine)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "job".to_string())
    }
}

/// What a keystroke means right now.
enum JobMode {
    /// The list and the highlighted job's detail — the resting state.
    List,
    /// The routines browser, reused whole from `queue`, picking the draft's
    /// target.
    PickRoutine { draft: Draft, nav: RoutineNav },
    /// The cron field: `draft.schedule` is the editable buffer.
    Schedule { draft: Draft },
    /// The pipeline picker with fuzzy search; `cursor` indexes the matches.
    PickPipeline {
        draft: Draft,
        query: String,
        cursor: usize,
    },
    /// `x` waiting on a `y`.
    ConfirmDelete(String),
    /// A message held on screen until the next key — a refused save, a fired
    /// job — since `draw` clears the screen before every frame.
    Outcome(String),
}

struct JobsState {
    /// Index of the highlighted job. Only ever addresses a real job — the
    /// `(new)` row belongs to the walk, not the resting list — so it is
    /// clamped to `jobs.len() - 1` and means nothing when the list is empty.
    cursor: usize,
    mode: JobMode,
}

/// `spoolway jobs` with no subcommand.
pub fn jobs_screen(repo: &Repo, pipelines: &Pipelines, cwd: &Path) -> Result<()> {
    let jobs = jobs::load(repo)?;
    let routines = super::routines::list_routines(repo)?;

    let mut stdin = RawStdin;
    let mut stdout = std::io::stdout();
    // Scoped so raw mode is restored before anything else wants the terminal.
    let _term = crate::platform::TermGuard::new();
    run_jobs_screen(
        repo,
        pipelines,
        cwd,
        jobs,
        routines,
        &mut stdin,
        &mut stdout,
    )
}

/// Everything a frame needs beside the job list and the screen state —
/// bundled so `render`, `draw` and the key wait stay under the argument count
/// the rest of the module keeps to, the same way `queue`'s own `Panes` does.
struct Ctx<'a> {
    repo: &'a Repo,
    pipelines: &'a Pipelines,
    routines: &'a [RoutineFolder],
    routines_dir: &'a Path,
}

fn run_jobs_screen(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &Path,
    mut jobs: Vec<Job>,
    routines: Vec<RoutineFolder>,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<()> {
    let routines_dir = repo.routines_dir();
    let ctx = Ctx {
        repo,
        pipelines,
        routines: &routines,
        routines_dir: &routines_dir,
    };
    let mut state = JobsState {
        cursor: 0,
        mode: JobMode::List,
    };
    let mut last: Option<Vec<String>> = None;

    loop {
        draw_jobs(&ctx, &jobs, &state, &mut last, out);
        let Some(key) = jobs_wait_for_key(&ctx, &mut jobs, &mut state, &mut last, input, out)
        else {
            // No terminal, or a scripted input ran dry: stop the same way `q`
            // does.
            break;
        };

        match &state.mode {
            JobMode::Outcome(_) => state.mode = JobMode::List,

            JobMode::ConfirmDelete(name) => match key {
                Key::Char('y') => {
                    let name = name.clone();
                    match jobs::delete(repo, &name) {
                        Ok(()) => {
                            reload(repo, &mut jobs);
                            state.mode = JobMode::List;
                            clamp_cursor(&jobs, &mut state);
                        }
                        Err(err) => {
                            state.mode = JobMode::Outcome(format!(
                                "Could not delete the job `{name}`: {err:#}. Its store may be \
                                 read-only; fix that and try again."
                            ));
                        }
                    }
                }
                _ => state.mode = JobMode::List,
            },

            JobMode::List => {
                if handle_list_key(repo, pipelines, cwd, &mut jobs, &mut state, key)? {
                    break;
                }
            }

            JobMode::PickRoutine { draft, nav } => match key {
                Key::Esc => state.mode = JobMode::List,
                Key::Enter if nav.focus == Focus::Groups && !nav.selected.is_empty() => {
                    if let Some(rel) = picked_folder(&routines, nav, &routines_dir) {
                        let mut draft = draft.clone();
                        draft.routine = rel;
                        state.mode = JobMode::Schedule { draft };
                    }
                }
                Key::Char(' ') if nav.focus == Focus::Tasks => {
                    if let Some(rel) = picked_document(&routines, nav, &routines_dir) {
                        let mut draft = draft.clone();
                        draft.routine = rel;
                        state.mode = JobMode::Schedule { draft };
                    }
                }
                _ => {
                    let (mut nav, draft) = (nav.clone(), draft.clone());
                    handle_routine_key(&routines, &mut nav, key);
                    state.mode = JobMode::PickRoutine { draft, nav };
                }
            },

            JobMode::Schedule { draft } => match key {
                Key::Esc => state.mode = JobMode::List,
                Key::Enter => {
                    if crate::cron::Cron::parse(draft.schedule.trim()).is_ok() {
                        let cursor = pipeline_index(pipelines, &draft.pipeline);
                        state.mode = JobMode::PickPipeline {
                            draft: draft.clone(),
                            query: String::new(),
                            cursor,
                        };
                    }
                }
                Key::Backspace | Key::Char('\u{8}') => {
                    let mut draft = draft.clone();
                    draft.schedule.pop();
                    state.mode = JobMode::Schedule { draft };
                }
                Key::Char(c) if !c.is_control() => {
                    let mut draft = draft.clone();
                    draft.schedule.push(c);
                    state.mode = JobMode::Schedule { draft };
                }
                _ => {}
            },

            JobMode::PickPipeline {
                draft,
                query,
                cursor,
            } => {
                let matches = pipeline_matches(pipelines, query);
                match key {
                    Key::Esc => state.mode = JobMode::List,
                    Key::Up | Key::Char('k') => {
                        let cursor = cursor.saturating_sub(1);
                        state.mode = JobMode::PickPipeline {
                            draft: draft.clone(),
                            query: query.clone(),
                            cursor,
                        };
                    }
                    Key::Down | Key::Char('j') => {
                        let cursor = (cursor + 1).min(matches.len().saturating_sub(1));
                        state.mode = JobMode::PickPipeline {
                            draft: draft.clone(),
                            query: query.clone(),
                            cursor,
                        };
                    }
                    Key::Enter => {
                        if let Some(name) =
                            matches.get((*cursor).min(matches.len().saturating_sub(1)))
                        {
                            let mut draft = draft.clone();
                            draft.pipeline = name.clone();
                            let saved = commit_draft(repo, draft);
                            state.mode = match saved {
                                Ok(name) => {
                                    reload(repo, &mut jobs);
                                    state.cursor =
                                        jobs.iter().position(|job| job.name == name).unwrap_or(0);
                                    JobMode::List
                                }
                                Err(message) => JobMode::Outcome(message),
                            };
                            clamp_cursor(&jobs, &mut state);
                        }
                    }
                    Key::Backspace | Key::Char('\u{8}') => {
                        let mut query = query.clone();
                        query.pop();
                        state.mode = JobMode::PickPipeline {
                            draft: draft.clone(),
                            query,
                            cursor: 0,
                        };
                    }
                    Key::Char(c) if !c.is_control() => {
                        let mut query = query.clone();
                        query.push(c);
                        state.mode = JobMode::PickPipeline {
                            draft: draft.clone(),
                            query,
                            cursor: 0,
                        };
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

/// Re-read both stores into `jobs`, keeping the old list if the read fails —
/// a cross-store name collision from someone else's branch is a reason to say
/// so on the next `load`, not to blank the screen.
fn reload(repo: &Repo, jobs: &mut Vec<Job>) {
    if let Ok(fresh) = jobs::load(repo) {
        *jobs = fresh;
    }
}

fn clamp_cursor(jobs: &[Job], state: &mut JobsState) {
    state.cursor = state.cursor.min(jobs.len().saturating_sub(1));
}

/// One key over the resting list. `Ok(true)` means quit.
///
/// The cursor only ever addresses a real job — the `(new)` row belongs to the
/// walk, not this state — so `e`, `space`, `x` and `r` are gated on the list
/// not being empty and always act on `jobs[cursor]`. `n` starts the walk.
fn handle_list_key(
    repo: &Repo,
    pipelines: &Pipelines,
    cwd: &Path,
    jobs: &mut Vec<Job>,
    state: &mut JobsState,
    key: Key,
) -> Result<bool> {
    let has_jobs = !jobs.is_empty();
    match key {
        Key::Char('q') => return Ok(true),
        Key::Up | Key::Char('k') => state.cursor = state.cursor.saturating_sub(1),
        Key::Down | Key::Char('j') => {
            state.cursor = (state.cursor + 1).min(jobs.len().saturating_sub(1));
        }
        Key::Char('n') => {
            state.mode = JobMode::PickRoutine {
                draft: Draft::new(pipelines),
                nav: RoutineNav::new(),
            };
        }
        Key::Char('e') if has_jobs => {
            state.mode = JobMode::PickRoutine {
                draft: Draft::from_job(&jobs[state.cursor]),
                nav: RoutineNav::new(),
            };
        }
        Key::Char(' ') if has_jobs => {
            let job = &jobs[state.cursor];
            let mut spec = job.spec.clone();
            spec.enabled = !spec.enabled;
            let (scope, name, verb) = (job.scope, job.name.clone(), pause_verb(spec.enabled));
            match jobs::write(repo, scope, &name, &spec) {
                Ok(()) => reload(repo, jobs),
                Err(err) => {
                    state.mode = JobMode::Outcome(format!(
                        "Could not {verb} the job `{name}` in `{}`: {err:#}. Check the file is \
                         writable and try again.",
                        store_label(repo, &store_of(repo, scope)),
                    ));
                }
            }
        }
        Key::Char('x') if has_jobs => {
            state.mode = JobMode::ConfirmDelete(jobs[state.cursor].name.clone());
        }
        Key::Char('r') if has_jobs => {
            let base = crate::repo::branch_at(cwd)?;
            let job = &jobs[state.cursor];
            state.mode = match fire_job(repo, pipelines, job, &base) {
                Ok(tasks) => {
                    let ids: Vec<String> = tasks.iter().map(|task| task.id().to_string()).collect();
                    JobMode::Outcome(if ids.is_empty() {
                        format!(
                            "Fired `{}` now, but its routine had no documents to queue.",
                            job.name
                        )
                    } else {
                        format!("Fired `{}` now. Queued {}.", job.name, ids.join(", "))
                    })
                }
                Err(err) => JobMode::Outcome(format!(
                    "Could not fire `{}` now: {err:#}. Fix its routine, pipeline or store — \
                     `spoolway doctor` names the fault — then press r again.",
                    job.name
                )),
            };
        }
        _ => {}
    }
    Ok(false)
}

/// `resume`/`pause` for the message the pause toggle prints on failure — the
/// spec passed to `write` already carries the target state.
fn pause_verb(will_be_enabled: bool) -> &'static str {
    if will_be_enabled { "resume" } else { "pause" }
}

/// Write the draft to the store its `scope` names. `Ok(name)` is the saved
/// job's name; `Err(message)` is a full-sentence refusal to show as an
/// [`JobMode::Outcome`].
///
/// A new job takes its name from its routine's leaf. Both stores are re-read
/// here, right before the write — not trusted from the snapshot the walk
/// opened on — so a name that another checkout or a hand edit added while the
/// draft was open is still refused, both store paths named. An edit keeps the
/// name it had, so re-saving over it is never a collision with itself.
fn commit_draft(repo: &Repo, draft: Draft) -> std::result::Result<String, String> {
    let name = draft.name();
    if draft.editing.is_none() && draft.routine.is_empty() {
        return Err("Pick a routine before saving — a job runs one.".to_string());
    }

    let current = jobs::load(repo).map_err(|err| {
        format!(
            "Could not read the job stores to check that `{name}` is free: {err:#}. Fix `{}` or \
             `{}` — or run `spoolway doctor` for the exact fault — then save again.",
            store_label(repo, &repo.user_jobs_file()),
            store_label(repo, &repo.jobs_file()),
        )
    })?;
    let taken = current
        .iter()
        .any(|job| job.name == name && draft.editing.as_deref() != Some(name.as_str()));
    if taken {
        return Err(format!(
            "A job named `{name}` already exists. Delete it first — from `{}` or `{}` — then \
             save this one.",
            store_label(repo, &repo.user_jobs_file()),
            store_label(repo, &repo.jobs_file()),
        ));
    }

    let spec = JobSpec {
        schedule: draft.schedule.trim().to_string(),
        pipeline: draft.pipeline,
        routine: draft.routine,
        enabled: draft.enabled,
    };

    jobs::write(repo, draft.scope, &name, &spec).map_err(|err| {
        format!(
            "Could not save the job `{name}` to `{}`: {err:#}. Check the file is writable and try \
             again.",
            store_label(repo, &store_of(repo, draft.scope)),
        )
    })?;
    Ok(name)
}

/// The store file a job of `scope` is written to — for an error message.
fn store_of(repo: &Repo, scope: Scope) -> PathBuf {
    match scope {
        Scope::User => repo.user_jobs_file(),
        Scope::Project => repo.jobs_file(),
    }
}

/// The routine folder the walk's `enter` picks: the folder under the browser
/// cursor, and only when it is itself ticked. `None` — so `enter` does
/// nothing — when the cursor sits on an unticked folder, even if some other
/// folder is ticked; the criterion is `enter` *over a ticked folder*, not
/// over any folder while a tick exists elsewhere. Returned as the relative
/// string a job's `routine` field holds.
fn picked_folder(
    routines: &[RoutineFolder],
    nav: &RoutineNav,
    routines_dir: &Path,
) -> Option<String> {
    let folder = highlighted_routine_folder(routines, nav)?;
    if !nav.selected.contains(&folder.path) {
        return None;
    }
    relative_routine(&folder.path, routines_dir)
}

/// The highlighted document in the browser's tasks pane, same relative form.
fn picked_document(
    routines: &[RoutineFolder],
    nav: &RoutineNav,
    routines_dir: &Path,
) -> Option<String> {
    let folder = highlighted_routine_folder(routines, nav)?;
    let task = folder.tasks.get(nav.task_cursor)?;
    relative_routine(&task.path, routines_dir)
}

fn relative_routine(path: &Path, routines_dir: &Path) -> Option<String> {
    Some(
        path.strip_prefix(routines_dir)
            .ok()?
            .to_string_lossy()
            .replace('\\', "/"),
    )
}

/// Every pipeline whose name contains `query` as a subsequence, prefix
/// matches first and then name order — the picker's narrowing.
fn pipeline_matches(pipelines: &Pipelines, query: &str) -> Vec<String> {
    let needle = query.to_ascii_lowercase();
    let mut names: Vec<String> = pipelines
        .names()
        .into_iter()
        .filter(|name| is_subsequence(&needle, &name.to_ascii_lowercase()))
        .map(str::to_string)
        .collect();
    names.sort_by(|a, b| {
        let a_prefix = a.to_ascii_lowercase().starts_with(&needle);
        let b_prefix = b.to_ascii_lowercase().starts_with(&needle);
        b_prefix.cmp(&a_prefix).then_with(|| a.cmp(b))
    });
    names
}

/// Whether every char of `needle` appears in `haystack` in order.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|want| chars.any(|have| have == want))
}

/// Where `name` sits in the unfiltered pipeline list, so the picker opens
/// with the draft's current pipeline under the cursor.
fn pipeline_index(pipelines: &Pipelines, name: &str) -> usize {
    pipeline_matches(pipelines, "")
        .iter()
        .position(|candidate| candidate == name)
        .unwrap_or(0)
}

fn draw_jobs(
    ctx: &Ctx,
    jobs: &[Job],
    state: &JobsState,
    last: &mut Option<Vec<String>>,
    out: &mut impl std::io::Write,
) {
    let frame = render_jobs(ctx, jobs, state);
    if last.as_ref() == Some(&frame) {
        return;
    }
    let _ = write!(out, "\x1b[2J\x1b[H");
    for line in &frame {
        let _ = writeln!(out, "{line}");
    }
    *last = Some(frame);
}

/// Wait for the next key, redrawing on every idle poll slice so a resize or an
/// edit from elsewhere lands without a keystroke. The list is re-read only
/// while browsing — a half-typed draft must survive an idle tick.
fn jobs_wait_for_key(
    ctx: &Ctx,
    jobs: &mut Vec<Job>,
    state: &mut JobsState,
    last: &mut Option<Vec<String>>,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Option<Key> {
    loop {
        if input.byte_pending(crate::status::POLL) {
            return read_key(input);
        }
        if matches!(state.mode, JobMode::List) {
            reload(ctx.repo, jobs);
            clamp_cursor(jobs, state);
        }
        draw_jobs(ctx, jobs, state, last, out);
    }
}

fn render_jobs(ctx: &Ctx, jobs: &[Job], state: &JobsState) -> Vec<String> {
    match &state.mode {
        JobMode::Outcome(message) => {
            let mut lines: Vec<String> = message.lines().map(str::to_string).collect();
            lines.push(String::new());
            lines.push("press any key to continue".to_string());
            return lines;
        }
        JobMode::PickRoutine { nav, .. } => {
            let mut frame = render_routines(ctx.routines, ctx.routines_dir, ctx.pipelines, nav);
            frame.push(jobs_footer(&state.mode));
            return frame;
        }
        _ => {}
    }

    let lay = layout();
    let in_walk = matches!(
        state.mode,
        JobMode::Schedule { .. } | JobMode::PickPipeline { .. }
    );

    // Build the overlay panel first. The headless frame is only as tall as its
    // own content (`queue::two_pane_frame` with `rows: None`), so both panes
    // have to be padded to at least the panel's height plus its border, or
    // `overlay` writes the panel into rows that do not exist and the firing
    // lines, the key line and the bottom border are silently dropped.
    let overlay_panel: Option<Vec<String>> = match &state.mode {
        JobMode::Schedule { draft } => Some(schedule_panel(draft)),
        JobMode::PickPipeline { query, cursor, .. } => {
            Some(pipeline_panel(ctx.pipelines, query, *cursor))
        }
        JobMode::ConfirmDelete(name) => Some(panel(
            "delete this job",
            &[
                clip(&format!("  {name}")),
                String::new(),
                "  its schedule is removed from the store.".to_string(),
            ],
            "y delete   n keep",
        )),
        _ => None,
    };
    let min_rows = overlay_panel.as_ref().map_or(0, |panel| panel.len() + 4);

    let (mut left, left_focus) = jobs_list_lines(jobs, state, lay.left, in_walk);
    let (right_title, mut right, right_focus) = if in_walk {
        ("new job".to_string(), Vec::new(), (0, 0))
    } else if jobs.is_empty() {
        (
            "no jobs yet".to_string(),
            empty_detail_lines(ctx.repo),
            (0, 0),
        )
    } else {
        let job = jobs.get(state.cursor);
        let title = job
            .map(|job| job.name.clone())
            .unwrap_or_else(|| "no job".to_string());
        let (lines, focus) = jobs_detail_lines(ctx.repo, job, lay.right);
        (title, lines, focus)
    };
    left.resize(left.len().max(min_rows), String::new());
    right.resize(right.len().max(min_rows), String::new());

    let left_title = format!("jobs  {} of {}", jobs.len(), jobs.len());
    let mut frame = two_pane_frame(
        &window(&left, left_focus, lay.rows),
        &window(&right, right_focus, lay.rows),
        &left_title,
        &right_title,
        lay,
    );

    if let Some(panel) = &overlay_panel {
        overlay(&mut frame, panel);
    }

    frame.push(jobs_footer(&state.mode));
    frame
}

fn jobs_footer(mode: &JobMode) -> String {
    match mode {
        JobMode::List => {
            "  ↑↓ move  n new  e edit  space pause  x delete  r run now  q quit".to_string()
        }
        JobMode::PickRoutine { .. } => {
            "  ↑↓ move  → open  ← up  space select  enter use it  esc cancel".to_string()
        }
        JobMode::Schedule { .. } => "  type to edit   enter accept   esc cancel".to_string(),
        JobMode::PickPipeline { .. } => {
            "  type to narrow   ↑↓ move   enter choose   esc cancel".to_string()
        }
        JobMode::ConfirmDelete(_) => "  y delete   n keep".to_string(),
        JobMode::Outcome(_) => "  press any key".to_string(),
    }
}

/// The left pane. Resting (`show_new` false), it is one row per job — or a
/// single "no jobs yet" line when there are none, with both store paths named
/// over in the detail pane. During the new-job walk (`show_new` true) it
/// gains a trailing blank and a highlighted `(new)` row, the way the mockup's
/// panels 3 and 4 draw it; no job row is highlighted then.
fn jobs_list_lines(
    jobs: &[Job],
    state: &JobsState,
    width: usize,
    show_new: bool,
) -> (Vec<String>, (usize, usize)) {
    if jobs.is_empty() && !show_new {
        return (vec!["  no jobs yet".to_string()], (0, 0));
    }

    let mut lines: Vec<String> = jobs
        .iter()
        .enumerate()
        .map(|(index, job)| {
            let marker = if !show_new && state.cursor == index {
                ">"
            } else {
                " "
            };
            let tail = format!(" {}", next_cell(job));
            let budget = width.saturating_sub(tail.chars().count() + 2).max(8);
            format!("{marker} {}{tail}", pad_to(&job.name, budget))
        })
        .collect();

    let focus_line = if show_new {
        if !jobs.is_empty() {
            lines.push(String::new());
        }
        lines.push("> (new)".to_string());
        lines.len() - 1
    } else {
        state.cursor.min(lines.len().saturating_sub(1))
    };
    (lines, (focus_line, focus_line))
}

/// The detail pane when both stores are empty: what the screen is for and the
/// two files a job will be saved to.
fn empty_detail_lines(repo: &Repo) -> Vec<String> {
    vec![
        String::new(),
        "  No jobs yet.".to_string(),
        String::new(),
        "  Press n to write one: pick a routine, type a".to_string(),
        "  cron expression, pick a pipeline.".to_string(),
        String::new(),
        "  A job is saved to one of:".to_string(),
        format!(
            "    user      {}",
            store_label(repo, &repo.user_jobs_file())
        ),
        format!("    project   {}", store_label(repo, &repo.jobs_file())),
    ]
}

/// The right pane for the highlighted job — the first mockup panel.
fn jobs_detail_lines(
    repo: &Repo,
    job: Option<&Job>,
    width: usize,
) -> (Vec<String>, (usize, usize)) {
    let Some(job) = job else {
        return (Vec::new(), (0, 0));
    };

    let mut lines = vec![String::new(), format!("  {}", job.name)];

    let target = job.target(repo);
    let documents = target
        .as_ref()
        .ok()
        .map(|path| routine_documents(path))
        .unwrap_or_default();
    let routine_value = match &target {
        Ok(path) if path.exists() => format!(
            "{}  ({} document{})",
            job.spec.routine,
            documents.len(),
            if documents.len() == 1 { "" } else { "s" }
        ),
        _ => format!("{}  (missing)", job.spec.routine),
    };

    lines.extend(labeled_row("Routine:", &routine_value, width));
    lines.extend(labeled_row("Schedule:", &job.spec.schedule, width));
    lines.extend(labeled_row("Pipeline:", &job.spec.pipeline, width));
    lines.extend(labeled_row("Scope:", job.scope.label(), width));
    lines.extend(labeled_row("Next:", &detail_next(job), width));
    lines.extend(labeled_row("Last:", &detail_last(repo, &job.name), width));
    lines.push(String::new());

    if documents.is_empty() {
        lines.push("  no documents under this routine".to_string());
    } else {
        lines.push("  the documents it queues".to_string());
        let id_width = documents
            .iter()
            .map(|(id, _)| id.chars().count())
            .max()
            .unwrap_or(0);
        for (id, description) in &documents {
            lines.push(format!("    {id:<id_width$}    {description}"));
        }
    }

    (lines, (0, 0))
}

/// A routine target's own documents — `(id, one-line description)` each — for
/// the detail pane. Empty when the path is gone or will not read.
fn routine_documents(path: &Path) -> Vec<(String, String)> {
    if path.is_dir() {
        super::routines::read_folder_at(path)
            .map(|folder| {
                folder
                    .tasks
                    .iter()
                    .map(|task| {
                        (
                            task.id.clone(),
                            task.description.clone().unwrap_or_default(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    } else {
        super::routines::read_task_at(path)
            .map(|task| vec![(task.id, task.description.unwrap_or_default())])
            .unwrap_or_default()
    }
}

/// The detail pane's `Next:` value.
fn detail_next(job: &Job) -> String {
    if !job.spec.enabled {
        return "paused".to_string();
    }
    if crate::cron::Cron::parse(&job.spec.schedule).is_err() {
        return "will not parse".to_string();
    }
    match jobs::next_fire(&job.spec.schedule) {
        Some(when) => when.format("%a %-d %b, %H:%M").to_string(),
        None => "never".to_string(),
    }
}

/// The detail pane's `Last:` value.
fn detail_last(repo: &Repo, name: &str) -> String {
    match jobs::last_fired(repo, name) {
        Some(epoch) => match chrono::DateTime::from_timestamp(epoch, 0) {
            Some(when) => format!(
                "{}  ok",
                when.with_timezone(&Local).format("%a %-d %b, %H:%M")
            ),
            None => "never".to_string(),
        },
        None => "never".to_string(),
    }
}

/// The widest a picker's own body line is allowed to run. Kept well under the
/// no-terminal frame width (see `queue::layout`'s fallback) so the box
/// [`crate::screen::boxed`] draws around it always fits inside the frame
/// rather than overwriting its border.
const PANEL_BODY_WIDTH: usize = 52;

/// `s` cut to `PANEL_BODY_WIDTH` with a trailing `…` when it does not fit.
fn clip(s: &str) -> String {
    if s.chars().count() <= PANEL_BODY_WIDTH {
        return s.to_string();
    }
    let kept: String = s.chars().take(PANEL_BODY_WIDTH - 1).collect();
    format!("{kept}…")
}

/// The schedule field's overlay: the expression as typed, the same expression
/// in words, and the next three firings — recomputed on every keystroke. An
/// expression that will not parse says so in place of the three lines.
fn schedule_panel(draft: &Draft) -> Vec<String> {
    let expr = draft.schedule.trim();
    let mut body = vec![clip(&format!("  {}_", draft.schedule)), String::new()];

    if expr.is_empty() {
        body.push("  (type a cron expression)".to_string());
    } else {
        match crate::cron::Cron::parse(expr) {
            Ok(cron) => {
                body.push(clip(&format!("  {}", cron.describe())));
                body.push(String::new());
                let fires = jobs::next_fires(expr, 3);
                if fires.is_empty() {
                    body.push("  next   (never fires)".to_string());
                }
                for (index, when) in fires.iter().enumerate() {
                    let label = if index == 0 { "  next   " } else { "         " };
                    body.push(format!("{label}{}", when.format("%a %e %b  %H:%M")));
                }
            }
            Err(err) => body.push(clip(&format!("  won't parse — {err}"))),
        }
    }

    panel(
        &format!("when does {} run", draft.name()),
        &body,
        "enter accept   esc cancel",
    )
}

/// The pipeline picker's overlay: every pipeline the repo defines, narrowed by
/// the typed query, the highlighted one showing the first line of its own
/// description and the project default marked.
fn pipeline_panel(pipelines: &Pipelines, query: &str, cursor: usize) -> Vec<String> {
    let matches = pipeline_matches(pipelines, query);
    let cursor = cursor.min(matches.len().saturating_sub(1));
    let mut body = vec![clip(&format!("  find: {query}_")), String::new()];

    if matches.is_empty() {
        body.push("  no pipeline matches".to_string());
    }
    for (index, name) in matches.iter().enumerate() {
        let marker = if index == cursor { ">" } else { " " };
        let default = if *name == pipelines.default {
            "  (default)"
        } else {
            ""
        };
        body.push(clip(&format!("{marker} {name}{default}")));
        if index == cursor {
            if let Ok(pipeline) = pipelines.get(name)
                && let Some(description) = &pipeline.description
            {
                body.push(clip(&format!(
                    "    {}",
                    crate::fmt::first_line(description)
                )));
            }
            body.push(String::new());
        }
    }

    panel("which pipeline", &body, "enter choose   esc cancel")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::fixture;

    /// A routine folder with one queueable document, and a job in the user
    /// store pointing at it.
    fn one_job(repo: &Repo) {
        let dir = repo.routines_dir().join("nightly");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("audit.md"),
            "---\nid: audit\ntitle: audit, done\ngroup: demo\n---\n## Goal\n\nDo it.\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::write(
            repo.user_jobs_file(),
            "[jobs.nightly]\nschedule = \"0 3 * * *\"\npipeline = \"bugfix\"\nroutine = \"nightly\"\n",
        )
        .unwrap();
    }

    #[test]
    fn jobs_run_queues_the_minted_documents_and_records_the_firing() {
        let repo = fixture("jobs-run");
        one_job(&repo);

        jobs_run(&repo, &Pipelines::builtin(), "nightly", false).unwrap();

        let minted: Vec<String> = repo
            .queued_ids()
            .into_iter()
            .filter(|id| id.starts_with("audit-"))
            .collect();
        assert_eq!(
            minted.len(),
            1,
            "the routine's one document, minted: {minted:?}"
        );
        assert_eq!(
            repo.task(&minted[0]).unwrap().front.pipeline.as_deref(),
            Some("bugfix"),
            "queued on the job's own pipeline"
        );

        let record = crate::jobs::last_fired(&repo, "nightly");
        assert!(
            record.is_some(),
            "the firing is recorded in jobs.state.json"
        );
    }

    #[test]
    fn jobs_run_names_jobs_list_when_the_job_is_unknown() {
        let repo = fixture("jobs-run-unknown");
        one_job(&repo);
        let err = format!(
            "{:#}",
            jobs_run(&repo, &Pipelines::builtin(), "ghost", false).unwrap_err()
        );
        assert!(err.contains("no job named `ghost`"), "{err}");
        assert!(err.contains("spoolway jobs list"), "{err}");
    }

    #[test]
    fn jobs_run_is_refused_from_inside_a_lane() {
        let repo = fixture("jobs-run-lane");
        one_job(&repo);
        assert!(jobs_run(&repo, &Pipelines::builtin(), "nightly", true).is_err());
    }

    #[test]
    fn a_json_row_carries_schedule_error_only_when_the_expression_will_not_parse() {
        let repo = fixture("jobs-json-shape");
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::write(
            repo.user_jobs_file(),
            "[jobs.good]\nschedule = \"@daily\"\npipeline = \"default\"\nroutine = \"nightly\"\n\
             [jobs.bad]\nschedule = \"99 3 * * *\"\npipeline = \"default\"\nroutine = \"nightly\"\n",
        )
        .unwrap();
        let jobs = crate::jobs::load(&repo).unwrap();
        let row = |name: &str| {
            serde_json::to_value(Row::build(
                &repo,
                jobs.iter().find(|job| job.name == name).unwrap(),
            ))
            .unwrap()
        };

        let good = row("good");
        assert!(good.get("schedule_error").is_none(), "{good}");
        assert!(
            good.get("next").is_some(),
            "core fields stay present: {good}"
        );

        let bad = row("bad");
        assert!(
            bad.get("schedule_error").and_then(|v| v.as_str()).is_some(),
            "{bad}"
        );
    }

    // --------------------------------------------------------------- the screen

    /// A routines tree with two folders, one document in each — enough to walk
    /// the browser and pick a target.
    fn seed_routines(repo: &Repo) {
        for (folder, id) in [("nightly", "audit"), ("weekly", "deps")] {
            let dir = repo.routines_dir().join(folder);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{id}.md")),
                format!(
                    "---\nid: {id}\ntitle: chore({id}): do the {id}\ngroup: demo\n---\n## Goal\n\nDo it.\n"
                ),
            )
            .unwrap();
        }
        std::fs::create_dir_all(repo.home()).unwrap();
    }

    /// Play `input` through the screen headlessly and hand back everything it
    /// drew, exactly as the queue screen's own tests drive `run_screen`.
    fn drive(repo: &Repo, input: &str) -> String {
        let jobs = jobs::load(repo).unwrap();
        let routines = super::super::routines::list_routines(repo).unwrap();
        let mut keys = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        run_jobs_screen(
            repo,
            &Pipelines::builtin(),
            &repo.root,
            jobs,
            routines,
            &mut keys,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn last_frame(drawn: &str) -> &str {
        drawn.rsplit("\x1b[2J\x1b[H").next().unwrap_or(drawn)
    }

    #[test]
    fn the_walk_writes_a_job_to_the_user_store_named_after_its_routine() {
        let repo = fixture("jobs-screen-new");
        seed_routines(&repo);

        // n → new; space ticks the first folder (`nightly`); enter uses it;
        // type the expression; enter accepts it; enter chooses the default
        // pipeline.
        drive(&repo, "n \r0 3 * * 1-5\r\r");

        let jobs = jobs::load(&repo).unwrap();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].name, "nightly");
        assert_eq!(jobs[0].scope, Scope::User);
        assert_eq!(jobs[0].spec.schedule, "0 3 * * 1-5");
        assert_eq!(jobs[0].spec.routine, "nightly");
        assert_eq!(jobs[0].spec.pipeline, Pipelines::builtin().default);
        assert!(jobs[0].spec.enabled);
    }

    #[test]
    fn esc_out_of_the_routine_browser_writes_nothing() {
        let repo = fixture("jobs-screen-esc");
        seed_routines(&repo);
        drive(&repo, "n\x1bq");
        assert!(jobs::load(&repo).unwrap().is_empty());
    }

    #[test]
    fn the_schedule_field_refuses_enter_until_the_expression_parses() {
        let repo = fixture("jobs-screen-badexpr");
        seed_routines(&repo);
        // A broken expression, then enter (refused), then esc, then quit.
        let drawn = drive(&repo, "n \r99 3 * * *\r\x1bq");
        assert!(jobs::load(&repo).unwrap().is_empty(), "enter was refused");
        assert!(
            drawn.contains("won't parse"),
            "the field says so in place of the firings"
        );
    }

    #[test]
    fn an_empty_store_names_both_paths_rather_than_drawing_an_empty_pane() {
        let repo = fixture("jobs-screen-empty");
        seed_routines(&repo);
        let frame = drive(&repo, "q").to_string();
        let frame = last_frame(&frame);
        assert!(frame.contains("No jobs yet"), "{frame}");
        assert!(
            frame.contains("jobs.toml"),
            "names the store files: {frame}"
        );
    }

    #[test]
    fn space_pauses_and_resumes_the_highlighted_job() {
        let repo = fixture("jobs-screen-pause");
        seed_routines(&repo);
        jobs::write(
            &repo,
            Scope::User,
            "nightly",
            &JobSpec {
                schedule: "@daily".to_string(),
                pipeline: "bugfix".to_string(),
                routine: "nightly".to_string(),
                enabled: true,
            },
        )
        .unwrap();

        drive(&repo, " q");
        assert!(!jobs::load(&repo).unwrap()[0].spec.enabled, "paused");

        drive(&repo, " q");
        assert!(jobs::load(&repo).unwrap()[0].spec.enabled, "resumed");
    }

    #[test]
    fn x_then_y_deletes_the_highlighted_job() {
        let repo = fixture("jobs-screen-delete");
        seed_routines(&repo);
        jobs::write(
            &repo,
            Scope::Project,
            "nightly",
            &JobSpec {
                schedule: "@daily".to_string(),
                pipeline: "bugfix".to_string(),
                routine: "nightly".to_string(),
                enabled: true,
            },
        )
        .unwrap();

        drive(&repo, "xy");
        assert!(jobs::load(&repo).unwrap().is_empty());
    }

    #[test]
    fn a_derived_name_already_in_use_is_refused_naming_both_stores() {
        let repo = fixture("jobs-screen-clash");
        seed_routines(&repo);
        jobs::write(
            &repo,
            Scope::Project,
            "nightly",
            &JobSpec {
                schedule: "@daily".to_string(),
                pipeline: "bugfix".to_string(),
                routine: "nightly".to_string(),
                enabled: true,
            },
        )
        .unwrap();

        // Walk a fresh job off the same `nightly` folder: same derived name.
        let drawn = drive(&repo, "n \r@daily\r\rq");
        assert_eq!(jobs::load(&repo).unwrap().len(), 1, "nothing was added");
        assert!(drawn.contains("already exists"), "{}", last_frame(&drawn));
    }

    #[test]
    fn pipeline_matches_narrows_by_subsequence_and_puts_prefixes_first() {
        let pipelines = Pipelines::builtin();
        assert_eq!(pipeline_matches(&pipelines, ""), vec!["bugfix", "default"]);
        assert_eq!(pipeline_matches(&pipelines, "bg"), vec!["bugfix"]);
        assert!(pipeline_matches(&pipelines, "zzz").is_empty());
    }

    #[test]
    fn the_resting_list_has_no_new_row_and_the_walk_shows_it_highlighted() {
        let repo = fixture("jobs-screen-newrow");
        seed_routines(&repo);
        jobs::write(
            &repo,
            Scope::User,
            "nightly",
            &JobSpec {
                schedule: "@daily".to_string(),
                pipeline: "bugfix".to_string(),
                routine: "nightly".to_string(),
                enabled: true,
            },
        )
        .unwrap();

        // The resting list: mockup panel 1 has only real jobs.
        let resting = drive(&repo, "q").to_string();
        assert!(
            !last_frame(&resting).contains("(new)"),
            "no (new) row at rest: {}",
            last_frame(&resting)
        );

        // During the walk the background list highlights `> (new)` and the
        // detail pane is titled `new job`, not the old job.
        let walking = drive(&repo, "n \r0 3 * * *").to_string();
        let frame = last_frame(&walking);
        assert!(frame.contains("> (new)"), "walk highlights (new): {frame}");
        assert!(
            frame.contains("new job"),
            "detail titled `new job`: {frame}"
        );
    }

    #[test]
    fn enter_saves_the_ticked_folder_under_the_cursor_not_the_first_one() {
        let repo = fixture("jobs-screen-multitick");
        seed_routines(&repo);
        // Tick `nightly` (folder 0), move down, tick `weekly`, then use it:
        // the cursor is on `weekly`, so that is the target.
        drive(&repo, "n \x1b[B \r@daily\r\r");

        let jobs = jobs::load(&repo).unwrap();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].name, "weekly");
        assert_eq!(jobs[0].spec.routine, "weekly");
    }

    #[test]
    fn enter_over_an_unticked_folder_does_nothing_even_with_a_tick_elsewhere() {
        let repo = fixture("jobs-screen-unticked");
        seed_routines(&repo);
        // Tick `nightly` (folder 0), move the cursor down to `weekly` — which
        // is not ticked — press enter, then esc out and quit.
        drive(&repo, "n \x1b[B\r\x1bq");
        assert!(
            jobs::load(&repo).unwrap().is_empty(),
            "enter over an unticked folder must not advance the walk"
        );
    }

    #[test]
    fn a_name_added_to_a_store_during_the_walk_is_still_refused_at_save() {
        let repo = fixture("jobs-screen-race");
        seed_routines(&repo);

        // The job exists on disk, but `run_jobs_screen` is handed an empty
        // list — the stale snapshot a walk that started before the write
        // would hold. `commit_draft` re-reads the stores, so it still refuses.
        jobs::write(
            &repo,
            Scope::Project,
            "nightly",
            &JobSpec {
                schedule: "@daily".to_string(),
                pipeline: "bugfix".to_string(),
                routine: "nightly".to_string(),
                enabled: true,
            },
        )
        .unwrap();
        let routines = super::super::routines::list_routines(&repo).unwrap();
        let mut input = std::io::Cursor::new("n \r@daily\r\rq".as_bytes().to_vec());
        let mut out = Vec::new();
        run_jobs_screen(
            &repo,
            &Pipelines::builtin(),
            &repo.root,
            Vec::new(), // the stale snapshot: empty
            routines,
            &mut input,
            &mut out,
        )
        .unwrap();

        assert_eq!(jobs::load(&repo).unwrap().len(), 1, "no second `nightly`");
        assert!(String::from_utf8(out).unwrap().contains("already exists"));
    }
}
