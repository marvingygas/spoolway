//! `spoolway jobs`: the readers for cron jobs, and `jobs run` to fire one by
//! hand. The screen that writes a job is a later task; a store with no job in
//! it lists nothing, which is the right resting state until then.

use std::path::Path;

use chrono::Local;

use super::*;
use crate::jobs::{self, Job};

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

    // Parsed only to fail fast on a job whose expression is broken — `run`
    // ignores the schedule itself.
    crate::cron::Cron::parse(&job.spec.schedule).map_err(|err| {
        anyhow::anyhow!(
            "job `{name}` will not run — its schedule `{}` in {} does not parse: {err}. Fix it \
             in that file (or run `spoolway doctor` for the full check).",
            job.spec.schedule,
            store_label(repo, &job.source)
        )
    })?;

    let base = repo.branch()?;
    let tasks = crate::commands::queue_routine_target(
        repo,
        pipelines,
        &base,
        &job.target(repo)?,
        &job.spec.pipeline,
    )?;
    jobs::record_manual_fire(repo, name, &tasks)?;

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
}
