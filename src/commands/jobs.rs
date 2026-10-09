//! `spoolway jobs`: `jobs list` for scripts, `jobs contract`
//! for a producer that has never seen the two store files, and bare
//! `spoolway`'s jobs tab, the screen a person writes a job from. The screens
//! are the only thing that write a job — the jobs tab walks the routine, the
//! cron expression and the pipeline, then saves through
//! [`crate::jobs::write`]; the routines tab's `n` skips the routine and goes
//! through the same last two and the same save, [`NewJobWalk`]. No `jobs
//! add`, no config key. A job only schedules: nothing here queues its
//! routine, which fires when the dispatcher's clock crosses the schedule —
//! running a routine now is the routines tab's `enter`.

use std::path::{Path, PathBuf};

use chrono::Local;

use super::queue::{
    Focus, RoutineNav, clip as clip_to, handle_routine_key, highlighted_routine_folder,
    highlighted_routine_task, labeled_row, layout, two_pane_frame,
};
use super::routines::RoutineFolder;
use super::*;
use crate::jobs::{self, Job, JobSpec, Scope};
use crate::screen::pane::{Items, window};
use crate::screen::{Key, Notice, PollableRead, key_hint, keys, overlay, pad_to, panel, read_key};

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

/// A sample `[jobs.<name>]` table, valid on its own — `sample_table_parses`
/// below is what keeps this text and the parser it describes from drifting
/// apart.
const SAMPLE_TABLE: &str = "[jobs.nightly-audit]\n\
                             schedule = \"0 3 * * 1-5\"\n\
                             pipeline = \"default\"\n\
                             routine = \"nightly\"\n";

/// One `[jobs.<name>]` key, as `--json` serializes it. `default` and `text`
/// are owned, not `&'static str`: `default` is read off
/// [`crate::jobs::enabled_default`] rather than a hand-typed `"true"`, and
/// `text` has its `{routines_dir}` placeholder filled in with this
/// project's own path — see [`build_jobs_contract`].
#[derive(serde::Serialize)]
struct JobKeyDoc {
    name: &'static str,
    default: String,
    text: String,
}

/// One store's scope and path, as `--json` serializes it.
#[derive(serde::Serialize)]
struct JobsStoreDoc {
    scope: &'static str,
    path: String,
}

/// `spoolway jobs contract`'s whole body, plain or `--json` — the two store
/// paths, every key, the cron grammar [`crate::cron::Cron::grammar`] builds
/// from the same table [`crate::cron::Cron::parse`] enforces, the one-name-
/// in-both-stores refusal, and [`SAMPLE_TABLE`].
#[derive(serde::Serialize)]
struct JobsContractOut {
    stores: [JobsStoreDoc; 2],
    keys: Vec<JobKeyDoc>,
    cron_grammar: String,
    collision: &'static str,
    sample: &'static str,
}

fn build_jobs_contract(repo: &Repo) -> JobsContractOut {
    // `store_label`, the same way the store paths beside it are: in home
    // mode the setup folder is the workspace's own `config/`
    // (`crate::config::setup_dir_in`), not `.spoolway/`, so a hardcoded
    // `.spoolway/routines/` in the `routine` key's sentence would name a
    // path that project does not even have.
    let routines_dir = store_label(repo, &repo.routines_dir());
    JobsContractOut {
        stores: [
            JobsStoreDoc {
                scope: Scope::User.label(),
                path: store_label(repo, &repo.user_jobs_file()),
            },
            JobsStoreDoc {
                scope: Scope::Project.label(),
                path: store_label(repo, &repo.jobs_file()),
            },
        ],
        keys: crate::jobs::JOB_KEYS
            .iter()
            .map(|key| JobKeyDoc {
                name: key.name,
                default: if key.required {
                    "required".to_string()
                } else {
                    // `enabled` is the only optional key today; its default
                    // is read off `enabled_default` itself so this can
                    // never print a value the parser does not actually
                    // default to.
                    crate::jobs::enabled_default().to_string()
                },
                text: key.text.replace("{routines_dir}", &routines_dir),
            })
            .collect(),
        cron_grammar: crate::cron::Cron::grammar(),
        collision: "A name may be defined in only one store at a time. One found in both is \
                    refused at load, naming both store paths, rather than one silently \
                    shadowing the other.",
        sample: SAMPLE_TABLE,
    }
}

/// `spoolway jobs contract` — the job format, printed rather than guessed at
/// from `docs/jobs.md`. `--json` prints the same facts as one object.
pub fn jobs_contract(repo: &Repo, json: bool) -> Result<()> {
    let contract = build_jobs_contract(repo);
    if json {
        println!("{}", serde_json::to_string_pretty(&contract)?);
        return Ok(());
    }
    print!("{}", render_jobs_contract(&contract));
    Ok(())
}

/// [`jobs_contract`]'s plain-text body, built as a string so a test can
/// assert on it directly rather than capturing stdout.
fn render_jobs_contract(contract: &JobsContractOut) -> String {
    let mut out = String::new();
    out.push_str("THE JOBS CONTRACT\n");
    out.push_str("=================\n\n");

    out.push_str(
        "A job lives in one of two TOML stores, as a `[jobs.<name>]` table — this \
         project's own two:\n",
    );
    for store in &contract.stores {
        out.push_str(&format!("  {:<8} {}\n", store.scope, store.path));
    }
    out.push('\n');

    out.push_str("EVERY KEY\n");
    for key in &contract.keys {
        out.push_str(&format!("  {}  (default: {})\n", key.name, key.default));
        out.push_str(&format!("    {}\n\n", key.text));
    }

    out.push_str("THE CRON GRAMMAR\n");
    out.push_str(&contract.cron_grammar);
    out.push('\n');

    out.push_str(contract.collision);
    out.push_str("\n\n");

    out.push_str("A SAMPLE TABLE\n");
    out.push_str(contract.sample);
    out
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
// The screen. Bare `spoolway`'s jobs tab is the only thing that opens it any
// more — `spoolway jobs` bare now prints its usage instead. It lists both
// stores' jobs, shows the highlighted one in full, and walks three panels —
// the routines browser (its keys reused from `queue`), a cron field, and a
// pipeline picker — to write one. `e` edits through the same three, `space`
// pauses, `x` deletes, `r` fires now.
// ---------------------------------------------------------------------------

/// A job being written, carried through the routine → schedule → pipeline
/// walk. `editing` is `Some` for `e` over an existing job and `None` for `n`;
/// a new job takes its name from the routine's own leaf, an edited one keeps
/// the name it had.
#[derive(Debug, Clone)]
pub(super) struct Draft {
    editing: Option<String>,
    routine: String,
    schedule: String,
    pipeline: String,
    scope: Scope,
    enabled: bool,
}

impl Draft {
    /// There is no project default to seed a new job's pipeline with, so
    /// this opens on whichever pipeline sorts first — a starting point for
    /// the picker panel to change, not a choice made on the person's behalf.
    fn new(pipelines: &Pipelines) -> Draft {
        Draft {
            editing: None,
            routine: String::new(),
            schedule: String::new(),
            pipeline: pipelines
                .names()
                .first()
                .map(|n| n.to_string())
                .unwrap_or_default(),
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

/// The schedule popup and then the pipeline popup, and the save at the end
/// of them — the part of writing a job that both the jobs tab's walk and the
/// routines tab's `n` go through. Each tab keeps its own way in (the jobs tab
/// picks a routine first, the routines tab uses the highlighted one) and its
/// own way out; what happens between the two lives only here, so the two tabs
/// cannot drift apart on what a schedule or a pipeline accepts, or on which
/// names a save refuses.
#[derive(Debug, Clone)]
pub(super) enum NewJobWalk {
    /// The cron field: `draft.schedule` is the editable buffer.
    Schedule(Draft),
    /// The pipeline picker with fuzzy search; `cursor` indexes the matches.
    PickPipeline {
        draft: Draft,
        query: String,
        cursor: usize,
    },
}

/// Where one key leaves a [`NewJobWalk`].
pub(super) enum WalkStep {
    /// Still open, on this popup — the same one for a key it does not read.
    Walking(NewJobWalk),
    /// `esc` on either popup. Nothing was written.
    Cancelled,
    /// `enter` on the pipeline popup, and the job is in its store: the draft
    /// as saved, its name [`Draft::name`].
    Saved(Draft),
    /// `enter` on the pipeline popup, and the save was refused — a
    /// full-sentence message for a popup. Nothing was written.
    Refused(String),
}

impl NewJobWalk {
    /// Open on the schedule popup for `draft`, whose routine is already
    /// chosen.
    pub(super) fn schedule(draft: Draft) -> NewJobWalk {
        NewJobWalk::Schedule(draft)
    }

    /// The routines tab's `n`: a new job over the routine folder at
    /// `folder`, for the user store, named after the folder's leaf exactly
    /// as the jobs tab's own `n` names one. `None` for a folder outside
    /// `routines_dir`, which no job can point at.
    pub(super) fn for_routine(
        pipelines: &Pipelines,
        folder: &Path,
        routines_dir: &Path,
    ) -> Option<NewJobWalk> {
        let mut draft = Draft::new(pipelines);
        draft.routine = relative_routine(folder, routines_dir)?;
        Some(NewJobWalk::schedule(draft))
    }

    /// One key. `enter` on the schedule popup moves on only once the
    /// expression parses; `enter` on the pipeline popup saves through
    /// [`commit_draft`].
    pub(super) fn key(&self, repo: &Repo, pipelines: &Pipelines, key: Key) -> WalkStep {
        match self {
            NewJobWalk::Schedule(draft) => match key {
                Key::Esc => WalkStep::Cancelled,
                Key::Enter if crate::cron::Cron::parse(draft.schedule.trim()).is_ok() => {
                    WalkStep::Walking(NewJobWalk::PickPipeline {
                        draft: draft.clone(),
                        query: String::new(),
                        cursor: pipeline_index(pipelines, &draft.pipeline),
                    })
                }
                Key::Backspace | Key::Char('\u{8}') => {
                    let mut draft = draft.clone();
                    draft.schedule.pop();
                    WalkStep::Walking(NewJobWalk::Schedule(draft))
                }
                Key::Char(c) if !c.is_control() => {
                    let mut draft = draft.clone();
                    draft.schedule.push(c);
                    WalkStep::Walking(NewJobWalk::Schedule(draft))
                }
                _ => WalkStep::Walking(self.clone()),
            },
            NewJobWalk::PickPipeline {
                draft,
                query,
                cursor,
            } => {
                let matches = pipeline_matches(pipelines, query);
                let with = |query: String, cursor: usize| {
                    WalkStep::Walking(NewJobWalk::PickPipeline {
                        draft: draft.clone(),
                        query,
                        cursor,
                    })
                };
                match key {
                    Key::Esc => WalkStep::Cancelled,
                    Key::Up | Key::Char('k') => with(query.clone(), cursor.saturating_sub(1)),
                    Key::Down | Key::Char('j') => with(
                        query.clone(),
                        (cursor + 1).min(matches.len().saturating_sub(1)),
                    ),
                    Key::Enter => {
                        // Nothing matches the query: `enter` has no pipeline
                        // to save with, so the popup stays open.
                        let Some(name) =
                            matches.get((*cursor).min(matches.len().saturating_sub(1)))
                        else {
                            return WalkStep::Walking(self.clone());
                        };
                        let mut draft = draft.clone();
                        draft.pipeline = name.clone();
                        match commit_draft(repo, draft.clone()) {
                            Ok(_) => WalkStep::Saved(draft),
                            Err(message) => WalkStep::Refused(message),
                        }
                    }
                    Key::Backspace | Key::Char('\u{8}') => {
                        let mut query = query.clone();
                        query.pop();
                        with(query, 0)
                    }
                    Key::Char(c) if !c.is_control() => {
                        let mut query = query.clone();
                        query.push(c);
                        with(query, 0)
                    }
                    _ => WalkStep::Walking(self.clone()),
                }
            }
        }
    }

    /// The popup the walk has open.
    pub(super) fn panel(&self, pipelines: &Pipelines) -> Vec<String> {
        match self {
            NewJobWalk::Schedule(draft) => schedule_panel(draft),
            NewJobWalk::PickPipeline { query, cursor, .. } => {
                pipeline_panel(pipelines, query, *cursor)
            }
        }
    }

    /// The key line under the frame while the walk is open — the same keys
    /// its popup names.
    pub(super) fn footer(&self) -> String {
        match self {
            NewJobWalk::Schedule(_) => key_hint(SCHEDULE_KEYS),
            NewJobWalk::PickPipeline { .. } => key_hint(PIPELINE_KEYS),
        }
    }
}

/// The schedule popup's keys, in the popup and on the line under the frame.
const SCHEDULE_KEYS: &[(&str, &str)] = &[("enter", "accept"), ("esc", "cancel")];

/// The pipeline popup's keys, in the popup and on the line under the frame.
const PIPELINE_KEYS: &[(&str, &str)] = &[("enter", "choose"), ("esc", "cancel")];

/// The routines tab's popup once its `n` has saved a job: what it runs, when,
/// on which pipeline, and where to change it.
pub(super) fn saved_notice(draft: &Draft) -> Notice {
    let schedule = draft.schedule.trim();
    let when = match crate::cron::Cron::parse(schedule) {
        Ok(cron) => {
            let words = cron.describe();
            // `describe` opens with a clock time (`03:00, Monday to Friday`)
            // or `midnight` for a job that fires once a day, and with a
            // phrase of its own (`every hour, on the hour`, `5 minutes past
            // every hour`, `at minute 5 of hour 3`) otherwise — only the
            // first two read as `runs at …`. A leading digit alone is not
            // enough: `5 minutes past every hour` starts with one too, so
            // the clock time is matched by its `HH:` shape.
            let clock = words.as_bytes();
            let is_clock = clock.len() >= 3
                && clock[0].is_ascii_digit()
                && clock[1].is_ascii_digit()
                && clock[2] == b':';
            if is_clock || words.starts_with("midnight") {
                format!("at {words}")
            } else {
                words
            }
        }
        // Unreachable through the walk, whose schedule popup refuses
        // `enter` until the expression parses — but a notice is no place to
        // panic, so the expression itself stands in.
        Err(_) => format!("on `{schedule}`"),
    };
    let next = match jobs::next_fire(schedule) {
        Some(at) => at.format("%a %-d %b %H:%M").to_string(),
        None => "never".to_string(),
    };
    Notice::new(
        "job saved",
        format!(
            "{} runs {when}, on {}.\nNext: {next}.\nEdit, pause or delete it on the jobs tab.",
            draft.name(),
            draft.pipeline,
        ),
    )
}

/// What a keystroke means right now.
enum JobMode {
    /// The list and the highlighted job's detail — the resting state.
    List,
    /// The routines browser `queue` drives — its keys and its navigation,
    /// reused whole — picking the draft's target, drawn as a popup over the
    /// list: see [`routine_panel`].
    PickRoutine { draft: Draft, nav: RoutineNav },
    /// The cron field and then the pipeline picker — the half of the walk
    /// the routines tab's own `n` shares; see [`NewJobWalk`].
    Walk(NewJobWalk),
    /// `x` waiting on `enter` to delete or `esc` to keep.
    ConfirmDelete(String),
    /// A message in a popup over the list until `enter` closes it — a
    /// refused save, a failed delete — since `draw` clears the screen before
    /// every frame. Every other key is the popup's to ignore.
    Outcome(Notice),
}

struct JobsState {
    /// Index of the highlighted job. Only ever addresses a real job — the
    /// `(new)` row belongs to the walk, not the resting list — so it is
    /// clamped to `jobs.len() - 1` and means nothing when the list is empty.
    cursor: usize,
    mode: JobMode,
}

/// Bare `spoolway`'s jobs tab: the same screen `spoolway jobs` used to open
/// on its own, over the terminal the shell around it already holds — so no
/// guard and no `ctrl-c` handler of its own.
pub(crate) fn jobs_tab(
    repo: &Repo,
    pipelines: &Pipelines,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<crate::screen::shell::Leave> {
    let jobs = jobs::load(repo)?;
    let routines = super::routines::list_routines(repo)?;
    run_jobs_screen(repo, pipelines, jobs, routines, writer, input, out)
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

/// The screen's own loop. Ends in [`Leave::Quit`] when the input runs out or
/// `ctrl-c` is caught, and in whatever [`crate::screen::shell::leave_on`]
/// reads off `←`, `→` or `q` over the resting list when bare `spoolway` hosts
/// this as its jobs tab.
///
/// [`Leave::Quit`]: crate::screen::shell::Leave::Quit
fn run_jobs_screen(
    repo: &Repo,
    pipelines: &Pipelines,
    mut jobs: Vec<Job>,
    routines: Vec<RoutineFolder>,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<crate::screen::shell::Leave> {
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

    loop {
        draw_jobs(&ctx, &jobs, &state, writer, out);
        let Some(key) = jobs_wait_for_key(&ctx, &mut jobs, &mut state, writer, input, out) else {
            // No terminal, a scripted input ran dry, or `ctrl-c` was caught
            // and noticed by `jobs_wait_for_key`: all three end the screen
            // the same way.
            break;
        };

        // The resting list is the one mode with no popup or walk open, so
        // it is the one where a hosting shell's keys are the shell's.
        if matches!(state.mode, JobMode::List)
            && let Some(leave) = crate::screen::shell::leave_on(key)
        {
            return Ok(leave);
        }

        match &state.mode {
            JobMode::Outcome(_) => {
                if key == Key::Enter {
                    state.mode = JobMode::List;
                }
            }

            JobMode::ConfirmDelete(name) => match key {
                Key::Enter => {
                    let name = name.clone();
                    match jobs::delete(repo, &name) {
                        Ok(()) => {
                            reload(repo, &mut jobs);
                            state.mode = JobMode::List;
                            clamp_cursor(&jobs, &mut state);
                        }
                        Err(err) => {
                            state.mode = JobMode::Outcome(Notice::new(
                                "delete this job",
                                format!(
                                    "Could not delete the job `{name}`: {err:#}. Its store may \
                                     be read-only; fix that and try again."
                                ),
                            ));
                        }
                    }
                }
                Key::Esc => state.mode = JobMode::List,
                // Every other key, `y` and `n` included, leaves the popup open
                // so a stray keystroke can't delete or dismiss it.
                _ => {}
            },

            JobMode::List => handle_list_key(repo, pipelines, &mut jobs, &mut state, key),

            JobMode::PickRoutine { draft, nav } => match key {
                Key::Esc => state.mode = JobMode::List,
                Key::Enter if nav.focus == Focus::Groups && !nav.selected.is_empty() => {
                    if let Some(rel) = picked_folder(&routines, nav, &routines_dir) {
                        let mut draft = draft.clone();
                        draft.routine = rel;
                        state.mode = JobMode::Walk(NewJobWalk::schedule(draft));
                    }
                }
                Key::Char(' ') if nav.focus == Focus::Tasks => {
                    if let Some(rel) = picked_task(&routines, nav, &routines_dir) {
                        let mut draft = draft.clone();
                        draft.routine = rel;
                        state.mode = JobMode::Walk(NewJobWalk::schedule(draft));
                    }
                }
                // `o` over the tasks pane: open the highlighted task
                // in an editor pane, exactly the shape the queue screen's own
                // routines pane gives it — see
                // `open_highlighted_job_routine`. Gated the same way: live
                // only with a task actually under the cursor.
                Key::Char('o')
                    if nav.focus == Focus::Tasks
                        && highlighted_routine_task(&routines, nav).is_some() =>
                {
                    let draft = draft.clone();
                    state.mode = open_highlighted_job_routine(repo, &routines, nav, draft);
                }
                _ => {
                    let (mut nav, draft) = (nav.clone(), draft.clone());
                    handle_routine_key(&routines, &mut nav, key);
                    state.mode = JobMode::PickRoutine { draft, nav };
                }
            },

            JobMode::Walk(walk) => match walk.key(repo, pipelines, key) {
                WalkStep::Walking(walk) => state.mode = JobMode::Walk(walk),
                WalkStep::Cancelled => state.mode = JobMode::List,
                WalkStep::Saved(draft) => {
                    reload(repo, &mut jobs);
                    let name = draft.name();
                    state.cursor = jobs.iter().position(|job| job.name == name).unwrap_or(0);
                    state.mode = JobMode::List;
                    clamp_cursor(&jobs, &mut state);
                }
                WalkStep::Refused(message) => {
                    state.mode = JobMode::Outcome(Notice::new("not saved", message));
                }
            },
        }
    }
    Ok(crate::screen::shell::Leave::Quit)
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

/// One key over the resting list.
///
/// The cursor only ever addresses a real job — the `(new)` row belongs to the
/// walk, not this state — so `e`, `space` and `x` are gated on the list
/// not being empty and always act on `jobs[cursor]`. `n` starts the walk.
/// Quitting is not among these keys any more — `ctrl-c` is caught by bare
/// `spoolway`'s own screen rather than read as a key at all, and `q` over
/// the resting list is [`crate::screen::shell::leave_on`]'s to read, ahead
/// of this handler.
fn handle_list_key(
    repo: &Repo,
    pipelines: &Pipelines,
    jobs: &mut Vec<Job>,
    state: &mut JobsState,
    key: Key,
) {
    let has_jobs = !jobs.is_empty();
    match key {
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
                    state.mode = JobMode::Outcome(Notice::new(
                        verb,
                        format!(
                            "Could not {verb} the job `{name}` in `{}`: {err:#}. Check the \
                             file is writable and try again.",
                            store_label(repo, &store_of(repo, scope)),
                        ),
                    ));
                }
            }
        }
        Key::Char('x') if has_jobs => {
            state.mode = JobMode::ConfirmDelete(jobs[state.cursor].name.clone());
        }
        _ => {}
    }
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

/// The highlighted task in the browser's tasks pane, same relative form.
fn picked_task(
    routines: &[RoutineFolder],
    nav: &RoutineNav,
    routines_dir: &Path,
) -> Option<String> {
    let folder = highlighted_routine_folder(routines, nav)?;
    let task = folder.tasks.get(nav.task_cursor)?;
    relative_routine(&task.path, routines_dir)
}

/// `o` over the routine picker's own tasks pane: open the highlighted
/// task in an editor pane, the same shape [`crate::commands::queue`]'s
/// own `open_highlighted_routine` gives the queue screen's routines pane,
/// including the same [`JobMode::Outcome`] a backend with no pane to open
/// one in is surfaced through. Returns to [`JobMode::PickRoutine`] with
/// `nav` and `draft` exactly as they were rather than [`JobMode::List`],
/// since this key never leaves the walk the way the queue screen's `o` has
/// nothing of its own to stay in.
fn open_highlighted_job_routine(
    repo: &Repo,
    routines: &[RoutineFolder],
    nav: &RoutineNav,
    draft: Draft,
) -> JobMode {
    let Some(task) = highlighted_routine_task(routines, nav) else {
        return JobMode::PickRoutine {
            draft,
            nav: nav.clone(),
        };
    };
    let command = crate::status::editor_command(&task.path);
    let mux = match crate::mux::backend(repo) {
        Ok(mux) => mux,
        Err(err) => return JobMode::Outcome(Notice::new("open task", format!("o: {err:#}"))),
    };
    match mux.open_command(&repo.root, &format!("{} · edit", task.id), &command) {
        Ok(()) => JobMode::PickRoutine {
            draft,
            nav: nav.clone(),
        },
        Err(err) => JobMode::Outcome(Notice::new("open task", format!("o: {err:#}"))),
    }
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
    writer: &mut crate::screen::frame_writer::FrameWriter,
    out: &mut impl std::io::Write,
) {
    // Under the strip when bare `spoolway` hosts this as its jobs tab — see
    // `crate::screen::shell::under_strip`.
    let frame = crate::screen::shell::under_strip(render_jobs(ctx, jobs, state));
    writer.write_frame(&frame, crate::screen::pane_size(), out);
}

/// Wait for the next key, redrawing on every idle poll slice so a resize or an
/// edit from elsewhere lands without a keystroke. The list is re-read only
/// while browsing — a half-typed draft must survive an idle tick.
///
/// A cooked stdin — no raw mode, no `poll` — reports nothing pending without
/// waiting (see `RawStdin`), and is read straight away instead: blocking on
/// the line the terminal will deliver is what this loop is for, where
/// spinning on that answer would never read a key at all.
///
/// `stop::asked()` is checked on every slice too, so a caught `ctrl-c` ends
/// the screen the same way a drained pipe already does — see `queue`'s own
/// `wait_for_key` for why this is safe to poll rather than needing the
/// signal handler itself to unwind anything.
fn jobs_wait_for_key(
    ctx: &Ctx,
    jobs: &mut Vec<Job>,
    state: &mut JobsState,
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
        if matches!(state.mode, JobMode::List) {
            reload(ctx.repo, jobs);
            clamp_cursor(jobs, state);
        }
        draw_jobs(ctx, jobs, state, writer, out);
    }
}

fn render_jobs(ctx: &Ctx, jobs: &[Job], state: &JobsState) -> Vec<String> {
    let footer = jobs_footer(&state.mode);
    let lay = layout(&footer);
    let in_walk = matches!(state.mode, JobMode::PickRoutine { .. } | JobMode::Walk(_));

    // Build the overlay panel first. The headless frame is only as tall as its
    // own content (`queue::two_pane_frame` with `rows: None`), so both panes
    // have to be padded to at least the panel's height plus its border, or
    // `overlay` writes the panel into rows that do not exist and the firing
    // lines, the key line and the bottom border are silently dropped.
    let overlay_panel: Option<Vec<String>> = match &state.mode {
        JobMode::PickRoutine { nav, .. } => Some(routine_panel(ctx, nav)),
        JobMode::Walk(walk) => Some(walk.panel(ctx.pipelines)),
        JobMode::ConfirmDelete(name) => Some(panel(
            "delete this job",
            &[
                String::new(),
                clip(&format!("  {name}")),
                "  its schedule is removed from the store.".to_string(),
            ],
            &keys(&[("enter", "delete"), ("esc", "keep")]),
        )),
        // Wrapped no wider than the frame has room for, so a narrow
        // terminal still sees the popup's right border.
        JobMode::Outcome(notice) => Some(
            notice.panel(crate::screen::NOTICE_WRAP.min((lay.left + lay.right).saturating_sub(1))),
        ),
        JobMode::List => None,
    };
    let min_rows = overlay_panel.as_ref().map_or(0, |panel| panel.len() + 4);

    let (mut left, left_focus) = jobs_list_lines(jobs, state, lay.left, in_walk);
    // The right pane is titled for what it is, not for the job it shows:
    // the details' own first line already names the highlighted job. The
    // walk is the one exception, because its pane is a form for a job that
    // does not exist yet rather than any job's details.
    let (right_title, mut right, right_focus) = if in_walk {
        ("new job", Vec::new(), (0, 0))
    } else if jobs.is_empty() {
        ("details", empty_detail_lines(ctx.repo), (0, 0))
    } else {
        let (lines, focus) = jobs_detail_lines(ctx.repo, jobs.get(state.cursor), lay.right);
        ("details", lines, focus)
    };
    left.resize(left.len().max(min_rows), String::new());
    right.resize(right.len().max(min_rows), String::new());

    // One item per line and no noun: the jobs screen keeps the marker it
    // has always drawn, counting lines — see `Items::noun`.
    let left_starts: Vec<usize> = (0..left.len()).collect();
    let right_starts: Vec<usize> = (0..right.len()).collect();
    let mut frame = two_pane_frame(
        &window(
            &left,
            Items {
                starts: &left_starts,
                noun: None,
            },
            left_focus,
            lay.rows,
            lay.left,
        ),
        &window(
            &right,
            Items {
                starts: &right_starts,
                noun: None,
            },
            right_focus,
            lay.rows,
            lay.right,
        ),
        "jobs",
        right_title,
        lay,
    );

    if let Some(panel) = &overlay_panel {
        overlay(&mut frame, panel);
    }

    frame.push(footer);
    frame
}

/// The keys, under the frame — built by [`key_hint`] rather than a
/// hand-spelled literal, the same way `queue`'s own `footer` is, so a jobs
/// line reads exactly the way the panels it shares wording with do. Never
/// names `↑↓`, which every screen in this project reads the same way
/// regardless, or `q`, which no mode reads specially at all any more — see
/// `queue`'s own `footer` for why.
fn jobs_footer(mode: &JobMode) -> String {
    match mode {
        // `q` only inside bare `spoolway`'s jobs tab, the one place it quits.
        // The same line under a notice, which reads only the `enter` its own
        // popup names: this is the line the list reads again once it closes.
        JobMode::List | JobMode::Outcome(_) => key_hint(
            &[
                [
                    ("n", "new"),
                    ("e", "edit"),
                    ("space", "pause"),
                    ("x", "delete"),
                ]
                .as_slice(),
                crate::screen::shell::quit_hint(),
            ]
            .concat(),
        ),
        // Names exactly the keys `run_jobs_screen`'s own `PickRoutine` arm
        // and `handle_routine_key` read, the same set the queue screen's own
        // routines pane names — `o` included, gated the same way there.
        JobMode::PickRoutine { .. } => key_hint(ROUTINE_KEYS),
        JobMode::Walk(walk) => walk.footer(),
        JobMode::ConfirmDelete(_) => key_hint(&[("enter", "delete"), ("esc", "keep")]),
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
    let tasks = target
        .as_ref()
        .ok()
        .map(|path| routine_tasks(path))
        .unwrap_or_default();
    let routine_value = match &target {
        Ok(path) if path.exists() => format!(
            "{}  ({} task{})",
            job.spec.routine,
            tasks.len(),
            if tasks.len() == 1 { "" } else { "s" }
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

    if tasks.is_empty() {
        lines.push("  no tasks under this routine".to_string());
    } else {
        lines.push("  the tasks it queues".to_string());
        let id_width = tasks
            .iter()
            .map(|(id, _)| id.chars().count())
            .max()
            .unwrap_or(0);
        for (id, description) in &tasks {
            lines.push(format!("    {id:<id_width$}    {description}"));
        }
    }

    (lines, (0, 0))
}

/// A routine target's own tasks — `(id, one-line description)` each — for
/// the detail pane. Empty when the path is gone or will not read.
fn routine_tasks(path: &Path) -> Vec<(String, String)> {
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
    let mut body = vec![
        String::new(),
        clip(&format!("  {}_", draft.schedule)),
        String::new(),
    ];

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
        &keys(SCHEDULE_KEYS),
    )
}

/// The pipeline picker's overlay: every pipeline the repo defines, narrowed by
/// the typed query, the highlighted one showing the first line of its own
/// description.
fn pipeline_panel(pipelines: &Pipelines, query: &str, cursor: usize) -> Vec<String> {
    let matches = pipeline_matches(pipelines, query);
    let cursor = cursor.min(matches.len().saturating_sub(1));
    let mut body = vec![
        String::new(),
        clip(&format!("  find: {query}_")),
        String::new(),
    ];

    if matches.is_empty() {
        body.push("  no pipeline matches".to_string());
    }
    for (index, name) in matches.iter().enumerate() {
        let marker = if index == cursor { ">" } else { " " };
        body.push(clip(&format!("{marker} {name}")));
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

    panel("which pipeline", &body, &keys(PIPELINE_KEYS))
}

/// The routine picker's keys, in its popup and on the line under the frame.
/// One row for both panes, unlike the queue screen's routines pane: `esc`
/// cancels the picker from either, so nothing on it changes with focus.
const ROUTINE_KEYS: &[(&str, &str)] = &[
    ("space", "select"),
    ("enter", "use it"),
    ("o", "open task"),
    ("tab", "tasks"),
    ("esc", "cancel"),
];

/// The routine picker's overlay, as step 32 of the screen's mockup draws it:
/// one row per routine, each with its tick, and under them the highlighted
/// routine's tasks — the ones `enter` would use
/// — by id and title. The same browser the queue screen's routines pane
/// drives, [`handle_routine_key`] and all, drawn in a box over the list
/// rather than in place of it.
fn routine_panel(ctx: &Ctx, nav: &RoutineNav) -> Vec<String> {
    let key_row = keys(ROUTINE_KEYS);
    // As wide as the key row under it, so a long title never widens the
    // popup past what the keys already take.
    let width = key_row.chars().count();

    let mut body = vec![String::new()];
    if ctx.routines.is_empty() {
        body.push(clip_to(
            format!("nothing under {}", ctx.routines_dir.display()),
            width,
        ));
    }
    for (i, folder) in ctx.routines.iter().enumerate() {
        let marker = match i == nav.folder_cursor && nav.focus == Focus::Groups {
            true => ">",
            false => " ",
        };
        let tick = match nav.selected.contains(&folder.path) {
            true => "[x]",
            false => "[ ]",
        };
        body.push(clip_to(format!("{marker} {tick} {}", folder.name), width));
    }

    if let Some(folder) = highlighted_routine_folder(ctx.routines, nav)
        && !folder.tasks.is_empty()
    {
        body.push(String::new());
        let widest = folder
            .tasks
            .iter()
            .map(|task| task.id.chars().count())
            .max()
            .unwrap_or(0);
        for (i, task) in folder.tasks.iter().enumerate() {
            let marker = match i == nav.task_cursor && nav.focus == Focus::Tasks {
                true => ">",
                false => " ",
            };
            let title = task.description.as_deref().unwrap_or("");
            body.push(clip_to(
                format!("{marker} {}   {title}", pad_to(&task.id, widest))
                    .trim_end()
                    .to_string(),
                width,
            ));
        }
    }

    panel("which routine", &body, &key_row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::fixture;

    /// The `routine` key's `{routines_dir}` placeholder is filled with this
    /// project's own routines path, through [`store_label`] — the same
    /// function the store paths beside it go through, so a home-mode
    /// project (whose setup folder is not `.spoolway/`) gets a path that
    /// actually exists rather than a hardcoded `.spoolway/routines/`. The
    /// `enabled` key's default is read off [`crate::jobs::enabled_default`]
    /// itself, not a hand-typed `"true"`.
    #[test]
    fn the_routine_key_names_this_projects_own_routines_path_and_enabled_reads_its_default() {
        let (repo, _root_guard) = fixture("jobs-contract-routine-dir");
        let contract = build_jobs_contract(&repo);
        let routine_key = contract
            .keys
            .iter()
            .find(|key| key.name == "routine")
            .unwrap();
        assert!(
            !routine_key.text.contains("{routines_dir}"),
            "the placeholder was never filled in: {}",
            routine_key.text
        );
        assert!(
            routine_key
                .text
                .contains(&store_label(&repo, &repo.routines_dir())),
            "{}",
            routine_key.text
        );

        let enabled_key = contract
            .keys
            .iter()
            .find(|key| key.name == "enabled")
            .unwrap();
        assert_eq!(
            enabled_key.default,
            crate::jobs::enabled_default().to_string()
        );
    }

    /// [`SAMPLE_TABLE`] is not just prose: it is one real `[jobs.<name>]`
    /// table, and it has to parse the same way a hand-written store would —
    /// the acceptance criterion behind printing it at all.
    #[test]
    fn sample_table_parses_as_one_job() {
        let (repo, _root_guard) = fixture("jobs-contract-sample");
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::write(repo.user_jobs_file(), SAMPLE_TABLE).unwrap();

        let jobs = jobs::load(&repo).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "nightly-audit");
        assert_eq!(jobs[0].spec.schedule, "0 3 * * 1-5");
        assert_eq!(jobs[0].spec.pipeline, "default");
        assert_eq!(jobs[0].spec.routine, "nightly");
        assert!(jobs[0].spec.enabled);
    }

    /// The mockup's own promise: both store paths, every key, the cron
    /// grammar and the one-name-in-both-stores refusal, all in one printed
    /// body. `--json` carries the same facts as one object.
    #[test]
    fn jobs_contract_names_both_stores_every_key_and_the_grammar() {
        let (repo, _root_guard) = fixture("jobs-contract-text");
        let text = render_jobs_contract(&build_jobs_contract(&repo));
        for fact in [
            "THE JOBS CONTRACT",
            "schedule",
            "pipeline",
            "routine",
            "enabled",
            "minute hour day-of-month month day-of-week",
            "defined in only one store",
            "[jobs.nightly-audit]",
        ] {
            assert!(text.contains(fact), "jobs contract drops `{fact}`: {text}");
        }
        assert!(
            text.contains(&store_label(&repo, &repo.user_jobs_file())),
            "{text}"
        );
        assert!(
            text.contains(&store_label(&repo, &repo.jobs_file())),
            "{text}"
        );

        let json = serde_json::to_value(build_jobs_contract(&repo)).unwrap();
        assert_eq!(
            json["keys"].as_array().unwrap().len(),
            crate::jobs::JOB_KEYS.len()
        );
        assert!(json["cron_grammar"].as_str().unwrap().contains("0-59"));
    }

    /// A routine folder with one queueable task, and a job in the user
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

    /// The acceptance criterion this task exists for: the jobs tab's own
    /// frame, painted through the shared [`crate::screen::frame_writer`],
    /// must look exactly as it did through today's frozen erase-then-write —
    /// cell for cell, in text, colour and bold.
    #[test]
    fn jobs_paints_as_before() {
        let (repo, _root_guard) = fixture("jobs-paints-as-before");
        one_job(&repo);
        let jobs = jobs::load(&repo).unwrap();
        let pipelines = Pipelines::builtin();
        let routines = Vec::new();
        let routines_dir = repo.routines_dir();
        let ctx = Ctx {
            repo: &repo,
            pipelines: &pipelines,
            routines: &routines,
            routines_dir: &routines_dir,
        };
        let state = JobsState {
            cursor: 0,
            mode: JobMode::List,
        };
        let frame = crate::screen::shell::under_strip(render_jobs(&ctx, &jobs, &state));
        // Wide enough that no row here reaches the pane's own edge.
        let pane_size = (200, 60);

        let mut old = Vec::new();
        crate::screen::frame_writer::todays_write(&frame, &mut old);

        let mut new = Vec::new();
        crate::screen::frame_writer::FrameWriter::new().write_frame(&frame, pane_size, &mut new);

        crate::screen::frame_writer::assert_same_picture(&old, &new, pane_size);
    }

    #[test]
    fn a_json_row_carries_schedule_error_only_when_the_expression_will_not_parse() {
        let (repo, _root_guard) = fixture("jobs-json-shape");
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

    /// A routines tree with two folders, one task in each — enough to walk
    /// the browser and pick a target.
    fn seed_routines(repo: &Repo) {
        for (folder, id) in [("nightly", "audit"), ("weekly", "deps")] {
            let dir = repo.routines_dir().join(folder);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{id}.md")),
                // The title is quoted: an unquoted `chore(id): text` reads as
                // a YAML mapping value past its own colon, which used to
                // leave `read_task` silently parsing nothing — every folder
                // one own task short — for anything that reads the task
                // itself rather than just the folder it sits in.
                format!(
                    "---\nid: {id}\ntitle: \"chore({id}): do the {id}\"\ngroup: demo\n---\n\
                     ## Goal\n\nDo it.\n"
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
            jobs,
            routines,
            &mut crate::screen::frame_writer::FrameWriter::new(),
            &mut keys,
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn last_frame(drawn: &str) -> &str {
        drawn.rsplit("\x1b[?2026h\x1b[H").next().unwrap_or(drawn)
    }

    /// Hosted as bare `spoolway`'s jobs tab, the resting list hands `←`, `→`
    /// and `q` to the shell and draws under the strip; a walk in progress
    /// keeps them — `q` is a character of the cron expression there.
    #[test]
    fn hosted_the_resting_list_leaves_on_the_arrows_and_q_but_a_walk_does_not() {
        use crate::screen::shell::{Hosting, Leave, Tab, Toward};
        let (repo, _root_guard) = fixture("jobs-hosted-leave");
        seed_routines(&repo);
        let _hosting = Hosting::open(Tab::Jobs);

        let run = |input: &str| {
            let routines = super::super::routines::list_routines(&repo).unwrap();
            let mut keys = std::io::Cursor::new(input.as_bytes().to_vec());
            let mut out = Vec::new();
            let leave = run_jobs_screen(
                &repo,
                &Pipelines::builtin(),
                Vec::new(),
                routines,
                &mut crate::screen::frame_writer::FrameWriter::new(),
                &mut keys,
                &mut out,
            )
            .unwrap();
            (leave, String::from_utf8(out).unwrap())
        };

        let (leave, drawn) = run("\x1b[D");
        assert_eq!(leave, Leave::Switch(Toward::Left));
        assert!(last_frame(&drawn).contains("DISPATCH"), "{drawn}");
        assert!(last_frame(&drawn).contains("[q] quit"), "{drawn}");
        assert_eq!(run("\x1b[C").0, Leave::Switch(Toward::Right));
        assert_eq!(run("q").0, Leave::Quit);

        // `n`, a folder ticked, `enter` into the cron field, then `q` typed
        // into it: the input runs out inside the walk, never leaving.
        let (leave, drawn) = run("n \rq");
        assert_eq!(leave, Leave::Quit);
        assert!(
            last_frame(&drawn).contains("[enter] accept"),
            "still in the cron field: {drawn}"
        );
    }

    #[test]
    fn the_walk_writes_a_job_to_the_user_store_named_after_its_routine() {
        let (repo, _root_guard) = fixture("jobs-screen-new");
        seed_routines(&repo);

        // n → new; space ticks the first folder (`nightly`); enter uses it;
        // type the expression; enter accepts it; enter chooses whichever
        // pipeline the picker opened on — the one that sorts first, there
        // being no project default to seed it with instead.
        drive(&repo, "n \r0 3 * * 1-5\r\r");

        let jobs = jobs::load(&repo).unwrap();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        assert_eq!(jobs[0].name, "nightly");
        assert_eq!(jobs[0].scope, Scope::User);
        assert_eq!(jobs[0].spec.schedule, "0 3 * * 1-5");
        assert_eq!(jobs[0].spec.routine, "nightly");
        assert_eq!(
            jobs[0].spec.pipeline,
            Pipelines::builtin().names().first().unwrap().to_string()
        );
        assert!(jobs[0].spec.enabled);
    }

    #[test]
    fn esc_out_of_the_routine_browser_writes_nothing() {
        let (repo, _root_guard) = fixture("jobs-screen-esc");
        seed_routines(&repo);
        drive(&repo, "n\x1bq");
        assert!(jobs::load(&repo).unwrap().is_empty());
    }

    /// `q` has no arm of its own left in `handle_list_key`: it falls to the
    /// catch-all and does nothing, the same as any other key the list does
    /// not recognise. `ctrl-c` is the only way out of the screen now — see
    /// `queue`'s own `q_does_nothing_while_browsing` for why that half is
    /// not exercised here.
    #[test]
    fn q_does_nothing_over_the_resting_list() {
        let (repo, _root_guard) = fixture("jobs-screen-q-inert");
        seed_routines(&repo);

        // `q` first, then a real walk that writes a job — see
        // `the_walk_writes_a_job_to_the_user_store_named_after_its_routine`
        // for what each key after it does. If `q` still quit, none of it
        // would ever run.
        drive(&repo, "qn \r0 3 * * 1-5\r\r");

        assert_eq!(jobs::load(&repo).unwrap().len(), 1);
    }

    #[test]
    fn the_schedule_field_refuses_enter_until_the_expression_parses() {
        let (repo, _root_guard) = fixture("jobs-screen-badexpr");
        seed_routines(&repo);
        // A broken expression, then enter (refused), then esc back to the
        // list, then a trailing key that does nothing before input runs out.
        let drawn = drive(&repo, "n \r99 3 * * *\r\x1bq");
        assert!(jobs::load(&repo).unwrap().is_empty(), "enter was refused");
        assert!(
            drawn.contains("won't parse"),
            "the field says so in place of the firings"
        );
    }

    #[test]
    fn an_empty_store_names_both_paths_rather_than_drawing_an_empty_pane() {
        let (repo, _root_guard) = fixture("jobs-screen-empty");
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
        let (repo, _root_guard) = fixture("jobs-screen-pause");
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
    fn x_then_enter_deletes_the_highlighted_job() {
        let (repo, _root_guard) = fixture("jobs-screen-delete");
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

        drive(&repo, "x\r");
        assert!(jobs::load(&repo).unwrap().is_empty());
    }

    #[test]
    fn x_then_esc_keeps_the_highlighted_job() {
        let (repo, _root_guard) = fixture("jobs-screen-delete-esc");
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

        drive(&repo, "x\x1b");
        assert_eq!(jobs::load(&repo).unwrap().len(), 1);
    }

    #[test]
    fn a_derived_name_already_in_use_is_refused_naming_both_stores() {
        let (repo, _root_guard) = fixture("jobs-screen-clash");
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
        // The refusal is a popup over the list, and `q` under it is the
        // popup's to ignore.
        let drawn = drive(&repo, "n \r@daily\r\rq");
        assert_eq!(jobs::load(&repo).unwrap().len(), 1, "nothing was added");
        let frame = last_frame(&drawn);
        assert!(frame.contains("already exists"), "{frame}");
        assert!(frame.contains("┌─ not saved "), "{frame}");
        assert!(frame.contains("[enter] confirm"), "{frame}");
        assert!(frame.contains("─ jobs"), "the list under it: {frame}");
    }

    /// `enter` closes a notice onto the list again.
    #[test]
    fn enter_closes_a_notice_onto_the_list() {
        let (repo, _root_guard) = fixture("jobs-screen-notice-enter");
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

        let drawn = drive(&repo, "n \r@daily\r\r\r");
        let frame = last_frame(&drawn);
        assert!(!frame.contains("not saved"), "{frame}");
        assert!(frame.contains("[n] new"), "{frame}");
    }

    /// `r` over the resting list is no key at all: a job only schedules, and
    /// running its routine now is the routines tab's. The job is real and
    /// the checkout a git repository, so a key that still fired would queue
    /// the routine; the hook declares a floor this machine's `cargo` cannot
    /// meet, so one that still asked first would draw the tool gate.
    #[test]
    fn r_on_the_list_queues_nothing_and_leaves_the_frame_as_it_was() {
        let (mut repo, _root_guard) = fixture("jobs-screen-r-does-nothing");
        one_job(&repo);
        crate::repo::run(&repo.root, "git", &["init", "-q", "-b", "main"]).unwrap();
        let dir = repo.checkout.join(".spoolway/hooks");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("versioned.sh");
        std::fs::write(
            &path,
            "#!/bin/sh\n# spoolway-requires: cargo >= 999.0.0\nexit 0\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        repo.config.issue_tracking.hook = "versioned.sh".to_string();

        let before = drive(&repo, "");
        let after = drive(&repo, "r");
        assert_eq!(last_frame(&after), last_frame(&before));
        assert!(
            std::fs::read_dir(repo.queue_dir())
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "nothing was queued"
        );
        assert_eq!(crate::jobs::last_fired(&repo, "nightly"), None);
    }

    /// `n` opens the routine picker as a popup over the list, as step 32 of
    /// the screen's mockup draws it: the folders with their ticks, the
    /// highlighted folder's tasks under them, and the keys in brackets.
    #[test]
    fn the_routine_picker_is_a_popup_over_the_list() {
        let (repo, _root_guard) = fixture("jobs-screen-routine-popup");
        seed_routines(&repo);
        let frame = drive(&repo, "n ").to_string();
        let frame = last_frame(&frame);
        assert!(frame.contains("┌─ which routine "), "{frame}");
        assert!(frame.contains("> [x] nightly"), "{frame}");
        assert!(frame.contains("  [ ] weekly"), "{frame}");
        assert!(
            frame.contains("audit   chore(audit): do the audit"),
            "{frame}"
        );
        assert!(
            frame.contains(
                "[space] select   [enter] use it   [o] open task   [tab] tasks   [esc] cancel"
            ),
            "{frame}"
        );
        assert!(frame.contains("─ jobs"), "the list under it: {frame}");
    }

    /// "job saved" reads `runs at` only before a clock time; any other
    /// phrase `describe` opens with stands on its own after `runs`.
    #[test]
    fn saved_notice_says_at_only_before_a_clock_time() {
        let mut draft = Draft::new(&Pipelines::builtin());
        draft.routine = "nightly/audit.md".to_string();
        draft.pipeline = "bugfix".to_string();

        draft.schedule = "30 2 * * *".to_string();
        let text = saved_notice(&draft).text;
        assert!(
            text.starts_with("audit runs at 02:30, on bugfix.\n"),
            "{text}"
        );

        draft.schedule = "0 * * * *".to_string();
        let text = saved_notice(&draft).text;
        assert!(
            text.starts_with("audit runs every hour, on the hour, on bugfix.\n"),
            "{text}"
        );
        assert!(
            text.ends_with("\nEdit, pause or delete it on the jobs tab."),
            "{text}"
        );

        // Opens with a digit, but is no clock time.
        draft.schedule = "5 * * * *".to_string();
        let text = saved_notice(&draft).text;
        assert!(
            text.starts_with("audit runs 5 minutes past every hour, on bugfix.\n"),
            "{text}"
        );

        draft.schedule = "0 0 * * *".to_string();
        let text = saved_notice(&draft).text;
        assert!(
            text.starts_with("audit runs at midnight, on bugfix.\n"),
            "{text}"
        );
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
        let (repo, _root_guard) = fixture("jobs-screen-newrow");
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
        assert!(
            last_frame(&resting).contains("─ jobs ─")
                && last_frame(&resting).contains("─ details ─"),
            "fixed titles, not the highlighted job's name: {}",
            last_frame(&resting)
        );

        // During the walk the background list highlights `> (new)` and the
        // detail pane is titled `new job`, not the old job.
        let walking = drive(&repo, "n \r0 3 * * *").to_string();
        let frame = last_frame(&walking);
        assert!(frame.contains("> (new)"), "walk highlights (new): {frame}");
        assert!(
            frame.contains("─ new job ─") && !frame.contains("─ details ─"),
            "detail titled `new job`: {frame}"
        );
    }

    /// With no job at all the titles are the same two words as with jobs:
    /// `no jobs yet` stays the line inside the pane, never its title.
    #[test]
    fn an_empty_jobs_tab_keeps_its_fixed_titles() {
        let (repo, _root_guard) = fixture("jobs-screen-empty-titles");
        let drawn = drive(&repo, "q").to_string();
        let frame = last_frame(&drawn);
        assert!(frame.contains("─ jobs ─"), "{frame}");
        assert!(frame.contains("─ details ─"), "{frame}");
        assert!(!frame.contains("─ no jobs yet"), "{frame}");
        assert!(
            frame.contains("  no jobs yet"),
            "the line inside the pane: {frame}"
        );
    }

    /// `jobs_footer`'s own lines, pinned the same way the queue screen pins
    /// its own (`src/commands/queue.rs`) and the board pins its
    /// (`src/status/mod.rs`) — the mockup draws both of these, and nothing
    /// short of the rendered frame proves `key_hint` produced them byte for
    /// byte rather than some near miss.
    #[test]
    fn the_list_and_routine_picker_key_lines_read_as_the_mockup_draws_them() {
        let (repo, _root_guard) = fixture("jobs-screen-footer-list");
        seed_routines(&repo);

        // Hosted, as bare `spoolway`'s jobs tab is, so the line carries the
        // quit hint the mockup draws after `[x] delete`.
        let hosting = crate::screen::shell::Hosting::open(crate::screen::shell::Tab::Jobs);
        let resting = drive(&repo, "q").to_string();
        drop(hosting);
        assert!(
            last_frame(&resting)
                .contains("[n] new   [e] edit   [space] pause   [x] delete   [q] quit"),
            "{}",
            last_frame(&resting)
        );

        let walking = drive(&repo, "n").to_string();
        assert!(
            last_frame(&walking).contains(
                "[space] select   [enter] use it   [o] open task   [tab] tasks   [esc] cancel"
            ),
            "{}",
            last_frame(&walking)
        );
    }

    #[test]
    fn enter_saves_the_ticked_folder_under_the_cursor_not_the_first_one() {
        let (repo, _root_guard) = fixture("jobs-screen-multitick");
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
        let (repo, _root_guard) = fixture("jobs-screen-unticked");
        seed_routines(&repo);
        // Tick `nightly` (folder 0), move the cursor down to `weekly` — which
        // is not ticked — press enter, then esc back to the list; the
        // trailing key does nothing before input runs out.
        drive(&repo, "n \x1b[B\r\x1bq");
        assert!(
            jobs::load(&repo).unwrap().is_empty(),
            "enter over an unticked folder must not advance the walk"
        );
    }

    /// `o` does nothing while the folders pane has focus — a folder has no
    /// task of its own to open — the same gate the queue screen's own
    /// routines pane gives it.
    #[test]
    fn o_does_nothing_while_the_folders_pane_has_focus_in_the_routine_picker() {
        let (repo, _root_guard) = fixture("jobs-screen-open-folders-focus");
        seed_routines(&repo);

        let drawn = drive(&repo, "no\x1bq");

        assert!(
            !drawn.contains("┌─ open task "),
            "`o` with the folders pane focused must never reach `JobMode::Outcome`:\n{drawn}"
        );
        assert!(jobs::load(&repo).unwrap().is_empty());
    }

    /// `o` on a headless run has no pane to open an editor in, in the routine
    /// picker exactly as it does on the queue screen's own routines pane —
    /// see `open_highlighted_job_routine`'s own doc comment. The refusal
    /// surfaces as `JobMode::Outcome`, same as any other; staying on the
    /// picker rather than falling back to `JobMode::List` is what its `Ok`
    /// path does once a pane actually opens, not this one.
    #[test]
    fn pressing_o_in_the_routine_picker_with_no_multiplexer_surfaces_the_refusal() {
        let (mut repo, _root_guard) = fixture("jobs-screen-open-headless");
        repo.config.dispatch.backend = crate::config::Backend::Headless;
        seed_routines(&repo);

        // `n` opens the routine picker, `tab` focuses the tasks pane on
        // `nightly`'s own `audit`, `o` tries to open it.
        let drawn = drive(&repo, "n\to");

        let last = last_frame(&drawn);
        assert!(last.contains("o:"), "{last}");
        assert!(last.contains("┌─ open task "), "in a popup: {last}");
    }

    /// The picker lists one row per routine. A subfolder inside `nightly`
    /// is never a row of its own; its task shows under `nightly` instead,
    /// the same routine a job pointing at `nightly` fires it with.
    #[test]
    fn the_routine_picker_lists_a_subfolder_s_tasks_under_its_routine() {
        let (repo, _root_guard) = fixture("jobs-picker-flat");
        seed_routines(&repo);
        let sub = repo.routines_dir().join("nightly").join("extra");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("sweep.md"),
            "---\nid: sweep\ntitle: sweep the cache\ngroup: demo\n---\n",
        )
        .unwrap();

        let drawn = drive(&repo, "n");
        let last = last_frame(&drawn);

        assert!(last.contains("> [ ] nightly"), "{last}");
        assert!(last.contains("[ ] weekly"), "{last}");
        assert!(!last.contains("[ ] extra"), "a subfolder is no row: {last}");
        assert!(last.contains("sweep   sweep the cache"), "{last}");
        assert!(
            last.contains(
                "[space] select   [enter] use it   [o] open task   [tab] tasks   [esc] cancel"
            ),
            "{last}"
        );
    }

    /// `tab` moves the picker's cursor onto the tasks pane and back. The
    /// arrows move nothing: they belong to the tab strip now.
    #[test]
    fn tab_moves_the_routine_picker_between_panes_and_the_arrows_do_nothing() {
        let (repo, _root_guard) = fixture("jobs-picker-tab");
        seed_routines(&repo);

        let last = |input: &str| last_frame(&drive(&repo, input)).to_string();

        let arrows = last("n\x1b[C\x1b[D\x1b[C");
        assert!(
            arrows.contains("> [ ] nightly"),
            "still on the list: {arrows}"
        );
        assert!(!arrows.contains("> audit"), "{arrows}");

        let tasks = last("n\t");
        assert!(tasks.contains("  [ ] nightly"), "{tasks}");
        assert!(tasks.contains("> audit"), "{tasks}");

        let back = last("n\t\t");
        assert!(back.contains("> [ ] nightly"), "{back}");
    }

    /// `esc` cancels the picker from the tasks pane too, not only from the
    /// list: the picker has no "back to the list" of its own.
    #[test]
    fn esc_over_the_routine_picker_s_tasks_pane_cancels_it() {
        let (repo, _root_guard) = fixture("jobs-picker-esc-tasks");
        seed_routines(&repo);

        let last = last_frame(&drive(&repo, "n\t\x1b")).to_string();

        assert!(!last.contains("which routine"), "picker closed: {last}");
        assert!(last.contains("[n] new"), "back on the list: {last}");
    }

    #[test]
    fn a_name_added_to_a_store_during_the_walk_is_still_refused_at_save() {
        let (repo, _root_guard) = fixture("jobs-screen-race");
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
            Vec::new(), // the stale snapshot: empty
            routines,
            &mut crate::screen::frame_writer::FrameWriter::new(),
            &mut input,
            &mut out,
        )
        .unwrap();

        assert_eq!(jobs::load(&repo).unwrap().len(), 1, "no second `nightly`");
        assert!(String::from_utf8(out).unwrap().contains("already exists"));
    }
}
