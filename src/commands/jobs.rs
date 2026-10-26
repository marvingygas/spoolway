//! `spoolway jobs`: `jobs list` for scripts, `jobs contract` for a producer
//! that has never seen the two store files, and the popups bare `spoolway`'s
//! routines tab writes a job through. That tab is where a person writes a
//! job; the only other writer is `spoolway pipeline rename`, which moves jobs
//! onto a renamed pipeline. Its edit schedule walks the cron expression and
//! the pipeline, then saves through [`crate::jobs::write`] — see
//! [`NewJobWalk`]. Deactivate and activate are [`set_enabled`]; no popup
//! here deletes a job, which leaves only with the routine the routines tab's
//! delete routine removes. No `jobs add`, no config key. A job only schedules:
//! nothing here queues its routine, which fires when the dispatcher's clock
//! crosses the schedule — running a routine now is the routines tab's
//! `enter`.

use std::path::{Path, PathBuf};

use chrono::Local;

use super::queue::clip as clip_to;
use super::*;
use crate::jobs::{self, Job, JobSpec, Scope};
use crate::screen::{Key, Notice, hint, key_hint, keys, panel};

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

    print!("{}", render_jobs_list(repo, &jobs, now_epoch));
    Ok(())
}

/// [`jobs_list`]'s table, its summary line and its store counts, built as a
/// string so a test can assert on it directly rather than capturing stdout.
fn render_jobs_list(repo: &Repo, jobs: &[Job], now_epoch: i64) -> String {
    let mut out = String::new();
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

    out.push_str(&line(&headers.map(str::to_string)));
    out.push('\n');
    for row in &rows {
        out.push_str(&line(row));
        out.push('\n');
    }

    let active = jobs.iter().filter(|job| job.spec.enabled).count();
    out.push('\n');
    out.push_str(&format!(
        "{} job{}, {active} active.\n",
        jobs.len(),
        if jobs.len() == 1 { "" } else { "s" }
    ));
    let stores = [repo.user_jobs_file(), repo.jobs_file()];
    let labels: Vec<String> = stores.iter().map(|path| store_label(repo, path)).collect();
    let width = labels.iter().map(|label| label.len()).max().unwrap_or(0);
    for (path, label) in stores.iter().zip(&labels) {
        let count = jobs.iter().filter(|job| &job.source == path).count();
        out.push_str(&format!("  {label:<width$}   {count}\n"));
    }
    for note in jobs.iter().filter_map(Job::unknown_keys_note) {
        out.push('\n');
        out.push_str(&format!("warning: {note}\n"));
    }

    out
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
                } else if key.name == "enabled" {
                    // Read off `enabled_default` itself so this can never
                    // print a value the parser does not actually default to.
                    crate::jobs::enabled_default().to_string()
                } else {
                    "none".to_string()
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
    /// Keys in the job's table that no job reads. Empty when it has none.
    unknown_keys: Vec<String>,
    source: String,
    /// Local RFC 3339, or `null` when the job is inactive, the expression will
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
            unknown_keys: job.unknown_keys.clone(),
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

/// The NEXT column: `inactive` for a disabled job, else a relative "in 5h
/// 48m" when soon, a weekday and time within the week, or a full date beyond
/// it.
fn next_cell(job: &Job) -> String {
    if !job.spec.enabled {
        return "inactive".to_string();
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
// The popups. The routines tab in `queue` opens them from a routine's options
// menu: a cron field and a pipeline picker to write a job. The tab draws and
// keys them; what each accepts and how a job is saved lives here.
// ---------------------------------------------------------------------------

/// A job being written, carried through the schedule and pipeline popups.
/// `editing` is `Some` for edit schedule over an existing job and `None` for
/// a new one; a new job takes its name from the routine's own leaf, an
/// edited one keeps the name it had.
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
    /// file stem of the routine folder it points at (`nightly` → `nightly`).
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
/// of them — what the routines tab's edit schedule goes through.
/// The routine is settled before the walk opens: the highlighted one, the
/// edited job's own, or the one picked for a job whose routine is gone.
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

    /// The routines tab's edit schedule on a routine with no job: a new job
    /// over the routine folder at `folder`, for the user store, named after
    /// the folder's leaf. `None` for a folder outside `routines_dir`, which
    /// no job can point at.
    pub(super) fn for_routine(
        pipelines: &Pipelines,
        folder: &Path,
        routines_dir: &Path,
    ) -> Option<NewJobWalk> {
        let mut draft = Draft::new(pipelines);
        draft.routine = relative_routine(folder, routines_dir)?;
        Some(NewJobWalk::schedule(draft))
    }

    /// The routines tab's edit schedule on a routine's job: the schedule
    /// popup over `job`'s own schedule, then the pipeline popup on its own
    /// pipeline. The routine is never asked for, so the job keeps it, and
    /// the save goes back under the job's own name to the store its `scope`
    /// names.
    pub(super) fn edit(job: &Job) -> NewJobWalk {
        NewJobWalk::schedule(Draft::from_job(job))
    }

    /// Edit schedule on a job whose routine is not a listed one, once the
    /// routine picker has picked `folder`: [`NewJobWalk::edit`], with the
    /// job moved onto that folder when it saves. `None` for a folder outside
    /// `routines_dir`, the same as [`NewJobWalk::for_routine`].
    pub(super) fn repoint(job: &Job, folder: &Path, routines_dir: &Path) -> Option<NewJobWalk> {
        let mut draft = Draft::from_job(job);
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
            NewJobWalk::PickPipeline { .. } => hint(&crate::screen::answer_keys()),
        }
    }
}

/// The schedule popup's keys, in the popup and on the line under the frame.
const SCHEDULE_KEYS: &[(&str, &str)] = &[("enter", "accept"), ("esc", "cancel")];

/// The routines tab's popup once its edit schedule has saved a job:
/// what it runs, when, on which pipeline, and where to change it.
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
        Some(at) => at.format(FIRING_TIME).to_string(),
        None => "never".to_string(),
    };
    // Edit schedule keeps an inactive job inactive, and its routine's menu then
    // offers activate job, not deactivate job, so the notice names that.
    let toggle = if draft.enabled {
        "deactivate"
    } else {
        "activate"
    };
    Notice::new(
        "job saved",
        format!(
            "{} runs {when}, on {}.\nnext: {next}.\nedit or {toggle} it with o on its routine.",
            draft.name(),
            draft.pipeline,
        ),
    )
}

/// `activate`/`deactivate` for the message the toggle prints on failure — the
/// spec passed to `write` already carries the target state.
fn toggle_verb(will_be_enabled: bool) -> &'static str {
    if will_be_enabled {
        "activate"
    } else {
        "deactivate"
    }
}

/// Deactivate `job` (`enabled` false) or activate it (`enabled` true),
/// written back to the store its own `scope` names and nowhere else, so a job
/// is never copied into the other store. The routines tab's deactivate job
/// and activate job come here. The state wanted is passed rather than
/// flipped, so a job deactivated by another hand since its menu opened is not
/// activated by a pick that said deactivate. `Err` is the popup that says why
/// the store was not written.
pub(super) fn set_enabled(
    repo: &Repo,
    job: &Job,
    enabled: bool,
) -> std::result::Result<(), Notice> {
    let mut spec = job.spec.clone();
    spec.enabled = enabled;
    let verb = toggle_verb(enabled);
    jobs::write(repo, job.scope, &job.name, &spec).map_err(|err| {
        Notice::new(
            verb,
            format!(
                "could not {verb} the job `{}` in `{}`: {err:#}. check the file is writable and \
                 try again.",
                job.name,
                store_label(repo, &store_of(repo, job.scope)),
            ),
        )
    })
}

/// Write the draft to the store its `scope` names. `Ok(name)` is the saved
/// job's name; `Err(message)` is a full-sentence refusal for the routines
/// tab to show in a popup.
///
/// A new job takes its name from its routine's leaf. Both stores are re-read
/// here, right before the write — not trusted from the snapshot the walk
/// opened on — so a name that another checkout or a hand edit added while the
/// draft was open is still refused, both store paths named. An edit keeps the
/// name it had, so re-saving over it is never a collision with itself.
fn commit_draft(repo: &Repo, draft: Draft) -> std::result::Result<String, String> {
    let name = draft.name();
    if draft.editing.is_none() && draft.routine.is_empty() {
        return Err("pick a routine before saving — a job runs one.".to_string());
    }

    let current = jobs::load(repo).map_err(|err| {
        format!(
            "could not read the job stores to check that `{name}` is free: {err:#}. fix `{}` or \
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
            "a job named `{name}` already exists, in `{}` or `{}`. pick another name and save \
             again.",
            store_label(repo, &repo.user_jobs_file()),
            store_label(repo, &repo.jobs_file()),
        ));
    }

    // A routine has at most one job, active or not: two enabled jobs on one
    // routine fire neither, and an inactive one would clash once activated.
    // The second is refused here, whichever store holds the first. Targets
    // are compared as resolved paths, so `nightly/` and `nightly` are one
    // routine.
    let wanted = repo.routines_dir().join(&draft.routine);
    if let Some(holder) = current.iter().find(|job| {
        draft.editing.as_deref() != Some(job.name.as_str())
            && job.target(repo).is_ok_and(|target| target == wanted)
    }) {
        return Err(format!(
            "routine `{}` already has the job `{}`, in `{}`. a routine has at most one job: \
             edit that job instead, or pick another routine.",
            draft.routine,
            holder.name,
            store_label(repo, &holder.source),
        ));
    }

    let spec = JobSpec {
        schedule: draft.schedule.trim().to_string(),
        pipeline: draft.pipeline,
        routine: draft.routine,
        enabled: draft.enabled,
        // The screen has no field for it yet; `jobs::write` leaves a `base`
        // already in the store alone.
        base: None,
    };

    jobs::write(repo, draft.scope, &name, &spec).map_err(|err| {
        format!(
            "could not save the job `{name}` to `{}`: {err:#}. check the file is writable and try \
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

/// How the routines tab writes a job's firing time: `Mon 26 Oct 03:00`, with
/// no comma. The details pane's `next:` and `last:` rows and the `job saved`
/// notice's `next:` line all use it, so the tab never shows the same moment
/// two ways.
const FIRING_TIME: &str = "%a %-d %b %H:%M";

/// The `next:` value on the routines tab's details pane.
pub(super) fn detail_next(job: &Job) -> String {
    if !job.spec.enabled {
        return "inactive".to_string();
    }
    if crate::cron::Cron::parse(&job.spec.schedule).is_err() {
        return "will not parse".to_string();
    }
    match jobs::next_fire(&job.spec.schedule) {
        Some(when) => when.format(FIRING_TIME).to_string(),
        None => "never".to_string(),
    }
}

/// The `last:` value on the routines tab's details pane.
pub(super) fn detail_last(repo: &Repo, name: &str) -> String {
    match jobs::last_fired(repo, name) {
        Some(epoch) => match chrono::DateTime::from_timestamp(epoch, 0) {
            Some(when) => format!("{}  ok", when.with_timezone(&Local).format(FIRING_TIME)),
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

/// How wide an answer's own text may be: [`PANEL_BODY_WIDTH`], less the
/// two columns [`crate::screen::answer_rows`] puts before every answer row
/// for its `>`.
const ANSWER_WIDTH: usize = PANEL_BODY_WIDTH - 2;

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
/// the typed query, as answers — the highlighted one showing the first line
/// of its own description under its name, and a blank row under that.
fn pipeline_panel(pipelines: &Pipelines, query: &str, cursor: usize) -> Vec<String> {
    let matches = pipeline_matches(pipelines, query);
    let cursor = cursor.min(matches.len().saturating_sub(1));
    let mut body = vec![String::new(), clip(&format!("  search: {query}_"))];

    if matches.is_empty() {
        body.push(String::new());
        body.push("  no pipeline matches".to_string());
    }
    let answers: Vec<String> = matches
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let mut answer = clip_to(name.clone(), ANSWER_WIDTH);
            if index == cursor {
                if let Ok(pipeline) = pipelines.get(name)
                    && let Some(description) = &pipeline.description
                {
                    answer.push('\n');
                    answer.push_str(&clip_to(
                        format!("  {}", crate::fmt::first_line(description)),
                        ANSWER_WIDTH,
                    ));
                }
                answer.push('\n');
            }
            answer
        })
        .collect();

    crate::screen::pick("which pipeline", &body, &answers, cursor)
}

/// Every popup here that asks something, drawn once each with
/// made-up content — for the test that walks every such popup in the
/// project; see `crate::screen`'s own tests.
#[cfg(test)]
pub(crate) fn jobs_asking_panels() -> Vec<(&'static str, Vec<String>)> {
    let pipelines = Pipelines::builtin_with_plain_descriptions();
    vec![("jobs pipeline picker", pipeline_panel(&pipelines, "", 0))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::fixture;

    /// The pipeline picker's query row says `search:`, the word the queue
    /// tab's search box uses, so the board names it one way on every tab.
    #[test]
    fn the_pipeline_picker_names_its_query_search() {
        let panel = pipeline_panel(&Pipelines::builtin(), "bug", 0);
        assert!(
            panel.iter().any(|row| row.contains("  search: bug_")),
            "{panel:#?}"
        );
        assert!(!panel.iter().any(|row| row.contains("find:")), "{panel:#?}");
    }

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

    /// The `enabled` key's text speaks of deactivating and activating a job,
    /// the words the routines tab's menu uses, not of pausing one — `paused`
    /// is a task's stage.
    #[test]
    fn the_enabled_key_says_deactivate_and_activating() {
        let (repo, _root_guard) = fixture("jobs-contract-enabled-words");
        let contract = build_jobs_contract(&repo);
        let text = &contract
            .keys
            .iter()
            .find(|key| key.name == "enabled")
            .unwrap()
            .text;
        assert!(text.contains("to deactivate a job"), "{text}");
        assert!(text.contains("activating it removes the key"), "{text}");
        assert!(!text.contains("pause"), "{text}");
        assert!(!text.contains("resum"), "{text}");
    }

    /// A disabled job reads `inactive` in NEXT, with the column widened to
    /// hold it, and the summary counts the active ones.
    #[test]
    fn jobs_list_says_inactive_for_a_disabled_job() {
        let (repo, _root_guard) = fixture("jobs-list-inactive");
        jobs::write(
            &repo,
            Scope::Project,
            "dependabot-review",
            &JobSpec {
                schedule: "0 */2 * * *".to_string(),
                pipeline: "dependabot".to_string(),
                routine: "dependabot".to_string(),
                enabled: false,
                ..JobSpec::neutral()
            },
        )
        .unwrap();
        let jobs = jobs::load(&repo).unwrap();

        let text = render_jobs_list(&repo, &jobs, Local::now().timestamp());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[..4],
            [
                "NAME               SCOPE    SCHEDULE     PIPELINE    NEXT      LAST",
                "dependabot-review  project  0 */2 * * *  dependabot  inactive  -",
                "",
                "1 job, 0 active.",
            ],
            "{text}"
        );
    }

    /// A store that cannot be written turns the toggle into a popup that
    /// names what was being done: deactivate on the way off, activate on
    /// the way back on.
    #[test]
    fn set_enabled_names_activate_or_deactivate_when_the_store_will_not_write() {
        let (repo, _root_guard) = fixture("jobs-set-enabled-fails");
        let job = Job {
            name: "nightly-audit".to_string(),
            scope: Scope::User,
            spec: JobSpec {
                routine: "nightly".to_string(),
                ..JobSpec::neutral()
            },
            source: repo.user_jobs_file(),
            unknown_keys: Vec::new(),
        };
        // A directory where the store file goes, so the write cannot read it.
        std::fs::create_dir_all(repo.user_jobs_file()).unwrap();

        for (enabled, verb) in [(false, "deactivate"), (true, "activate")] {
            let notice = set_enabled(&repo, &job, enabled).unwrap_err();
            assert_eq!(notice.title, verb);
            assert!(
                notice
                    .text
                    .starts_with(&format!("could not {verb} the job `nightly-audit`")),
                "{}",
                notice.text
            );
        }
    }

    #[test]
    fn a_json_row_carries_schedule_error_only_when_the_expression_will_not_parse() {
        let (repo, _root_guard) = fixture("jobs-json-shape");
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::write(
            repo.user_jobs_file(),
            "[jobs.good]\nschedule = \"@daily\"\npipeline = \"default\"\nroutine = \"nightly\"\n\
             [jobs.bad]\nschedule = \"99 3 * * *\"\npipeline = \"default\"\nroutine = \"nightly\"\n\
             [jobs.typo]\nschedule = \"@daily\"\npipeline = \"default\"\nroutine = \"nightly\"\nenable = false\n",
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

        assert_eq!(good["unknown_keys"], serde_json::json!([]), "{good}");
        assert_eq!(
            row("typo")["unknown_keys"],
            serde_json::json!(["enable"]),
            "a misspelt key is named in the JSON row"
        );
    }

    /// A routines tree with two folders, one task in each — enough for a
    /// job to point at.
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
            text.ends_with("\nedit or deactivate it with o on its routine."),
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

    /// An inactive job saved through edit schedule stays inactive, so "job
    /// saved" points at activate job, the item its routine's menu offers.
    #[test]
    fn saved_notice_names_activate_for_an_inactive_job() {
        let mut draft = Draft::new(&Pipelines::builtin());
        draft.routine = "nightly".to_string();
        draft.pipeline = "bugfix".to_string();
        draft.schedule = "0 3 * * *".to_string();
        draft.enabled = false;

        let text = saved_notice(&draft).text;
        assert!(
            text.ends_with("\nedit or activate it with o on its routine."),
            "{text}"
        );
    }

    /// The details pane's `next:` value is written `Mon 26 Oct 03:00`: day,
    /// date, month and clock time, with no comma before the time.
    #[test]
    fn detail_next_writes_the_date_and_time_with_no_comma() {
        let (repo, _root_guard) = fixture("jobs-detail-next");
        jobs::write(
            &repo,
            Scope::User,
            "nightly",
            &JobSpec {
                schedule: "0 3 * * *".to_string(),
                pipeline: "bugfix".to_string(),
                routine: "nightly".to_string(),
                ..JobSpec::neutral()
            },
        )
        .unwrap();
        let job = &jobs::load(&repo).unwrap()[0];

        let next = detail_next(job);
        let words: Vec<&str> = next.split(' ').collect();
        assert!(!next.contains(','), "{next}");
        assert_eq!(words.len(), 4, "{next}");
        assert_eq!(words[3], "03:00", "{next}");
        assert!(words[1].parse::<u32>().is_ok(), "{next}");
    }

    #[test]
    fn pipeline_matches_narrows_by_subsequence_and_puts_prefixes_first() {
        let pipelines = Pipelines::builtin();
        assert_eq!(pipeline_matches(&pipelines, ""), vec!["bugfix", "default"]);
        assert_eq!(pipeline_matches(&pipelines, "bg"), vec!["bugfix"]);
        assert!(pipeline_matches(&pipelines, "zzz").is_empty());
    }

    /// A save naming a routine that already has a job is refused, whichever
    /// store holds that job, and the notice names the job. Editing that job
    /// over its own routine is not a second job on it.
    #[test]
    fn a_save_on_a_routine_that_already_has_a_job_is_refused() {
        let (repo, _root_guard) = fixture("jobs-save-one-per-routine");
        seed_routines(&repo);
        jobs::write(
            &repo,
            Scope::Project,
            "first",
            &JobSpec {
                schedule: "@daily".to_string(),
                pipeline: "bugfix".to_string(),
                routine: "nightly".to_string(),
                ..JobSpec::neutral()
            },
        )
        .unwrap();
        let mut draft = Draft::new(&Pipelines::builtin());
        draft.routine = "nightly/".to_string();
        draft.schedule = "@weekly".to_string();

        let refusal = commit_draft(&repo, draft.clone()).unwrap_err();
        assert!(
            refusal.contains("`first`") && refusal.contains("at most one job"),
            "{refusal}"
        );
        assert!(
            refusal.ends_with("edit that job instead, or pick another routine."),
            "{refusal}"
        );
        assert_eq!(jobs::load(&repo).unwrap().len(), 1, "nothing was written");

        draft.editing = Some("first".to_string());
        assert_eq!(commit_draft(&repo, draft), Ok("first".to_string()));
    }

    /// A new job whose derived name a store already holds is refused, and
    /// the refusal names both stores. The stores are read at the save itself,
    /// so a job written while the popups were open is still caught.
    #[test]
    fn a_derived_name_already_in_a_store_is_refused_at_save() {
        let (repo, _root_guard) = fixture("jobs-save-name-taken");
        seed_routines(&repo);
        let mut draft = Draft::new(&Pipelines::builtin());
        draft.routine = "weekly".to_string();
        draft.schedule = "@daily".to_string();
        jobs::write(
            &repo,
            Scope::Project,
            "weekly",
            &JobSpec {
                schedule: "@daily".to_string(),
                pipeline: "bugfix".to_string(),
                routine: "nightly".to_string(),
                ..JobSpec::neutral()
            },
        )
        .unwrap();

        let refusal = commit_draft(&repo, draft).unwrap_err();
        assert!(refusal.contains("already exists"), "{refusal}");
        assert!(
            refusal.ends_with("pick another name and save again."),
            "{refusal}"
        );
        assert!(
            refusal.contains(&store_label(&repo, &repo.user_jobs_file()))
                && refusal.contains(&store_label(&repo, &repo.jobs_file())),
            "{refusal}"
        );
        assert_eq!(jobs::load(&repo).unwrap().len(), 1, "no second `weekly`");
    }
}
