//! A job: a five-field cron expression plus a pipeline, pointed at a routine
//! target. The dispatcher's own pass fires a job when the local clock crosses
//! its expression, queueing the routine through the same path the queue
//! screen's `r` pane runs — a folder as one batch, a single document alone
//! with its `depends_on` emptied.
//!
//! A job lives in one of two TOML stores: `jobs.toml` in this machine's
//! per-project home ([`Scope::User`], the default) or `.spoolway/jobs.toml`
//! in the checkout ([`Scope::Project`], tracked and shared). Its firing
//! history lives in one JSON file in machine home, whichever store the job
//! came from — a fired minute is a fact about this machine.
//!
//! Reads dominate. The two stores are written only by the `spoolway jobs`
//! screen, through [`write`] and [`delete`] — nothing else stamps a schedule.
//! A missing store is an empty list, not an error, so a project with no job in
//! it lists nothing and the dispatcher behaves exactly as it does today.
//!
//! A missed window stays missed: a job is only ever asked whether it matches
//! the *current* minute, so a window that passed while no dispatcher ran is
//! never queued later.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{Duration, Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use toml_edit::{DocumentMut, Item, Table};

use crate::cron::Cron;
use crate::pipeline::Pipelines;
use crate::repo::Repo;
use crate::task::Task;

/// Machine-home file recording when each job last fired, beside `lanes.json`
/// and `usage.jsonl`.
pub const STATE_FILE: &str = "jobs.state.json";

/// How a job's `fired_minute` and `skipped_minute` are stamped: local time
/// to the minute. Six dispatcher passes across one 03:00 all render the same
/// string, so the job fires once.
const MINUTE_FMT: &str = "%Y-%m-%dT%H:%M";

/// Which store a job was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// One job as its `[jobs.<name>]` table declares it. Not
/// `deny_unknown_fields`: the `spoolway jobs` screen owns this shape and may
/// grow it, and an older reader should ignore a key it does not know rather
/// than refuse the whole store.
#[derive(Debug, Clone, Deserialize)]
pub struct JobSpec {
    /// The five-field cron expression, verbatim. Parsed on use, not here, so
    /// a store with one bad expression still lists the rest and `spoolway
    /// doctor` is what names the bad one.
    pub schedule: String,
    /// The pipeline every document of the routine is queued under.
    pub pipeline: String,
    /// A path under `.spoolway/routines/`, relative: a folder queued as one
    /// batch, or a single `.md` file queued alone.
    pub routine: String,
    /// A paused job is listed but never fired. Absent means enabled.
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}

fn enabled_default() -> bool {
    true
}

/// One store file's contents. `[jobs.<name>]` tables, so a name is unique
/// within a store by construction.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Store {
    #[serde(default)]
    jobs: BTreeMap<String, JobSpec>,
}

/// One job, merged from whichever store it came from.
#[derive(Debug)]
pub struct Job {
    pub name: String,
    pub scope: Scope,
    pub spec: JobSpec,
    /// The store file it was read from — for `jobs list`'s footer and
    /// `doctor`'s messages.
    pub source: PathBuf,
}

impl Job {
    /// The path the job's `routine` resolves to under the checkout's
    /// `.spoolway/routines/`.
    ///
    /// Confined to that directory: `routine` is a project-supplied string,
    /// and an absolute path or a `..` component would let a job queue —
    /// or `doctor` stat — a file anywhere on disk. Rejected before any
    /// caller touches the path.
    pub fn target(&self, repo: &Repo) -> Result<PathBuf> {
        let rel = Path::new(&self.spec.routine);
        let escapes = rel.is_absolute()
            || rel.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            });
        if escapes {
            bail!(
                "job `{}` routine `{}` must be a path inside .spoolway/routines/ — no leading `/` \
                 and no `..`",
                self.name,
                self.spec.routine
            );
        }
        Ok(repo.routines_dir().join(rel))
    }
}

/// Both stores merged into one list: the user store first, then the project
/// store, each job carrying its [`Scope`] and source path. A missing file is
/// an empty list. A file that is present but will not parse is an error, and
/// so is one name defined in both stores — the two paths are printed.
pub fn load(repo: &Repo) -> Result<Vec<Job>> {
    let mut jobs = Vec::new();
    let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();

    for (scope, path) in [
        (Scope::User, repo.user_jobs_file()),
        (Scope::Project, repo.jobs_file()),
    ] {
        let store = read_store(&path)?;
        for (name, spec) in store.jobs {
            if let Some(other) = seen.get(&name) {
                bail!(
                    "job `{name}` is defined in two stores — remove it from one:\n    {}\n    {}",
                    other.display(),
                    path.display()
                );
            }
            seen.insert(name.clone(), path.clone());
            jobs.push(Job {
                name,
                scope,
                spec,
                source: path.clone(),
            });
        }
    }
    Ok(jobs)
}

/// A store file's contents, or an empty store if the file is not there.
fn read_store(path: &Path) -> Result<Store> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("parsing job store {}", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(err) => Err(err).with_context(|| format!("reading job store {}", path.display())),
    }
}

/// The file a job of `scope` lives in.
fn store_path(repo: &Repo, scope: Scope) -> PathBuf {
    match scope {
        Scope::User => repo.user_jobs_file(),
        Scope::Project => repo.jobs_file(),
    }
}

/// A store file's text, or `None` when the file is simply not there. Any
/// other read failure — a permission wall, an I/O error — is returned with
/// the path, so a caller never mistakes it for an empty store and overwrites
/// the file that would not open.
fn read_store_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("reading job store {}", path.display())),
    }
}

/// Write one job into its scope's store, editing the `[jobs.<name>]` table in
/// place — its own comments, key order and any key this reader does not know
/// are kept — and leaving every other byte of the file where it was.
///
/// `schedule`, `pipeline` and `routine` are set to `spec`'s values. `enabled`
/// is written as `false` only for a paused job; a running one has no key,
/// since absent means enabled (see [`JobSpec::enabled`]) — so resuming a job
/// removes the key rather than writing the default.
pub fn write(repo: &Repo, scope: Scope, name: &str, spec: &JobSpec) -> Result<()> {
    let path = store_path(repo, scope);
    let text = read_store_text(&path)?.unwrap_or_default();
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("job store {} is not valid TOML", path.display()))?;

    let jobs = doc
        .as_table_mut()
        .entry("jobs")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .with_context(|| format!("`jobs` in {} is not a table", path.display()))?;
    // `[jobs]` is only ever a parent of `[jobs.<name>]` tables, so it prints
    // as those headers alone rather than an empty `[jobs]` line of its own.
    jobs.set_implicit(true);

    // Edit the job's own table rather than replace it: a store may carry a
    // comment above a job, or a key a newer screen writes that this build
    // does not know, and reconstructing the table from scratch would drop
    // both.
    let entry = jobs
        .entry(name)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .with_context(|| format!("`jobs.{name}` in {} is not a table", path.display()))?;
    entry["schedule"] = toml_edit::value(spec.schedule.as_str());
    entry["pipeline"] = toml_edit::value(spec.pipeline.as_str());
    entry["routine"] = toml_edit::value(spec.routine.as_str());
    if spec.enabled {
        entry.remove("enabled");
    } else {
        entry["enabled"] = toml_edit::value(false);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    crate::task::write_atomic(&path, doc.to_string())
}

/// Remove a job from whichever store holds it, leaving the rest of that file
/// untouched. Removing a name no store has is not an error — the stores
/// already say what this was asked to make them say — but a store that will
/// not read is, so a confirmed delete never reports success over a job that
/// is still there.
pub fn delete(repo: &Repo, name: &str) -> Result<()> {
    for scope in [Scope::User, Scope::Project] {
        let path = store_path(repo, scope);
        let Some(text) = read_store_text(&path)? else {
            continue;
        };
        let mut doc: DocumentMut = text
            .parse()
            .with_context(|| format!("job store {} is not valid TOML", path.display()))?;
        let removed = doc
            .get_mut("jobs")
            .and_then(Item::as_table_mut)
            .is_some_and(|jobs| jobs.remove(name).is_some());
        if removed {
            crate::task::write_atomic(&path, doc.to_string())?;
        }
    }
    Ok(())
}

/// One job's firing history. All fields optional so a hand-deleted or
/// first-seen record reads as "never fired".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FireRecord {
    /// The local minute this job last fired, `MINUTE_FMT`. Compared to the
    /// current minute so the job fires once per window however many passes
    /// cross it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fired_minute: Option<String>,
    /// Epoch seconds of that firing, for `jobs list`'s LAST column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fired_at: Option<i64>,
    /// The ids the last firing minted. Checked against the queue so a job
    /// whose previous run is still in flight skips its next window instead of
    /// stacking a second copy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_ids: Vec<String>,
    /// The local minute a window was last skipped because that previous run
    /// was still in the queue — so the skip is reported once, not on every
    /// pass that crosses the minute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped_minute: Option<String>,
}

/// The whole state file: one [`FireRecord`] per job name.
type State = BTreeMap<String, FireRecord>;

fn read_state(repo: &Repo) -> State {
    std::fs::read_to_string(repo.jobs_state_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_state(repo: &Repo, state: &State) -> Result<()> {
    let text = serde_json::to_string_pretty(state)?;
    crate::task::write_atomic(&repo.jobs_state_file(), text)
}

/// Fire every enabled job whose expression matches the current local minute,
/// recording the minute in machine home. Called at the top of a dispatcher
/// pass, before it reads the queue, so a job's freshly queued documents are
/// dispatched by the same pass. The caller skips this entirely on a dry run.
///
/// Trouble with one job goes to `problems` and the rest still run. A job
/// that fires, and one skipped because its previous run is still in the
/// queue, are each pushed to `actions` naming the job.
pub fn fire_due(
    repo: &Repo,
    pipelines: &Pipelines,
    actions: &mut Vec<String>,
    problems: &mut Vec<String>,
) {
    let jobs = match load(repo) {
        Ok(jobs) => jobs,
        Err(err) => {
            problems.push(format!("jobs: {err:#}"));
            return;
        }
    };
    if !jobs.iter().any(|job| job.spec.enabled) {
        return;
    }

    let now = Local::now().naive_local();
    let minute = now.format(MINUTE_FMT).to_string();
    let base = repo.branch().unwrap_or_else(|_| "main".to_string());
    let queued = repo.queued_ids();

    let mut state = read_state(repo);
    let mut dirty = false;

    for job in &jobs {
        if !job.spec.enabled {
            continue;
        }
        let Ok(cron) = Cron::parse(&job.spec.schedule) else {
            // An expression that will not parse never fires; `doctor` names it.
            continue;
        };
        if !cron.matches(&now) {
            continue;
        }

        let record = state.entry(job.name.clone()).or_default();
        if record.fired_minute.as_deref() == Some(minute.as_str())
            || record.skipped_minute.as_deref() == Some(minute.as_str())
        {
            // Already fired, or already reported as skipped, for this minute.
            continue;
        }

        if record.queued_ids.iter().any(|id| queued.contains(id)) {
            record.skipped_minute = Some(minute.clone());
            dirty = true;
            actions.push(format!(
                "job {} · due now · skipped, its last run is still in the queue",
                job.name
            ));
            continue;
        }

        let target = match job.target(repo) {
            Ok(target) => target,
            Err(err) => {
                problems.push(format!("{err:#}"));
                continue;
            }
        };
        match crate::commands::queue_routine_target(
            repo,
            pipelines,
            &base,
            &target,
            &job.spec.pipeline,
        ) {
            Ok(tasks) => {
                let ids: Vec<String> = tasks.iter().map(|task| task.id().to_string()).collect();
                record.fired_minute = Some(minute.clone());
                record.fired_at = Some(Local::now().timestamp());
                record.queued_ids = ids.clone();
                record.skipped_minute = None;
                dirty = true;
                actions.push(format!(
                    "job {} · fired · queued {}",
                    job.name,
                    ids.join(", ")
                ));
            }
            Err(err) => problems.push(format!("job {}: {err:#}", job.name)),
        }
    }

    if dirty && let Err(err) = write_state(repo, &state) {
        problems.push(format!("jobs: could not write {}: {err:#}", STATE_FILE));
    }
}

/// Record a firing that `spoolway jobs run` drove by hand, so LAST updates
/// and the overlap guard sees this run. The schedule is untouched.
pub fn record_manual_fire(repo: &Repo, name: &str, tasks: &[Task]) -> Result<()> {
    let mut state = read_state(repo);
    let now = Local::now();
    let record = state.entry(name.to_string()).or_default();
    record.fired_minute = Some(now.naive_local().format(MINUTE_FMT).to_string());
    record.fired_at = Some(now.timestamp());
    record.queued_ids = tasks.iter().map(|task| task.id().to_string()).collect();
    record.skipped_minute = None;
    write_state(repo, &state)
}

/// One job's last-fired time, for `jobs list`'s LAST column.
pub fn last_fired(repo: &Repo, name: &str) -> Option<i64> {
    read_state(repo)
        .get(name)
        .and_then(|record| record.fired_at)
}

/// Why the dispatcher is staying up on an empty queue.
#[derive(Clone)]
pub struct StayingUp {
    /// How many jobs are enabled across both stores.
    pub enabled: usize,
    /// The enabled job that fires soonest, and when — `None` if none is
    /// enabled or none will ever fire.
    pub next: Option<(String, chrono::DateTime<Local>)>,
}

/// A caller-held memo for [`staying_up`]. The status board asks on every
/// one-second redraw, and [`next_fire`] behind it runs [`Cron::next_after`]'s
/// calendar scan for every enabled job — so the board keeps one of these and
/// only recomputes when the local minute turns over or a store file is
/// touched. See [`staying_up_cached`].
pub struct StayingUpMemo {
    minute: i64,
    stores: [Option<std::time::SystemTime>; 2],
    value: StayingUp,
}

/// How many jobs are enabled across both stores. A store that will not load
/// counts as none — cheap enough to call every idle pass, where
/// [`staying_up`] (which also scans for the next firing) is not.
pub fn enabled_count(repo: &Repo) -> usize {
    load(repo)
        .map(|jobs| jobs.iter().filter(|job| job.spec.enabled).count())
        .unwrap_or(0)
}

/// The enabled-job count and the soonest next firing, for the dispatcher's
/// "staying up for them" lines. A store that will not load counts as no
/// jobs — the dispatcher then behaves exactly as it does with none.
pub fn staying_up(repo: &Repo) -> StayingUp {
    let Ok(jobs) = load(repo) else {
        return StayingUp {
            enabled: 0,
            next: None,
        };
    };
    let next = jobs
        .iter()
        .filter(|job| job.spec.enabled)
        .filter_map(|job| Some((job.name.clone(), next_fire(&job.spec.schedule)?)))
        .min_by_key(|(_, when)| *when);

    StayingUp {
        enabled: jobs.iter().filter(|job| job.spec.enabled).count(),
        next,
    }
}

/// [`staying_up`], reusing `memo`'s answer while the current local minute and
/// both stores' modification times are unchanged — so a per-second caller
/// pays the calendar scan at most once a minute, and at once when a store is
/// edited.
pub fn staying_up_cached(repo: &Repo, memo: &mut Option<StayingUpMemo>) -> StayingUp {
    let minute = Local::now().timestamp().div_euclid(60);
    let stores = [
        store_mtime(&repo.user_jobs_file()),
        store_mtime(&repo.jobs_file()),
    ];
    if let Some(memo) = memo.as_ref()
        && memo.minute == minute
        && memo.stores == stores
    {
        return memo.value.clone();
    }
    let value = staying_up(repo);
    *memo = Some(StayingUpMemo {
        minute,
        stores,
        value: value.clone(),
    });
    value
}

fn store_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// The lines that say a job is keeping the dispatcher resident on an empty
/// queue — the wording the plan draws. One source for both the plain run and
/// the status board; each caller adds its own indent and styling. Assumes
/// `jobs.enabled > 0`.
pub fn staying_up_lines(jobs: &StayingUp) -> Vec<String> {
    let mut lines = vec![format!(
        "queue is empty. {} job{} enabled — staying up for {}.",
        jobs.enabled,
        if jobs.enabled == 1 { "" } else { "s" },
        if jobs.enabled == 1 { "it" } else { "them" },
    )];
    match &jobs.next {
        Some((name, when)) => lines.push(format!(
            "next: {name}, {}  ({})",
            when.format("%a %-d %b %H:%M"),
            until(*when - Local::now()),
        )),
        None => lines.push("next: no job will fire — run `spoolway doctor`".to_string()),
    }
    lines.push("ctrl-c stops.".to_string());
    lines
}

/// The next local instant an expression fires, from now. `None` if it will
/// not parse, or never comes round within one full Gregorian cycle in a
/// form this machine's timezone actually has.
pub fn next_fire(schedule: &str) -> Option<chrono::DateTime<Local>> {
    let cron = Cron::parse(schedule).ok()?;
    let now = Local::now();
    let at = next_real_fire(
        &cron,
        now.naive_local(),
        now.timestamp(),
        |naive| match Local.from_local_datetime(naive) {
            chrono::LocalResult::None => Wall::Skipped,
            chrono::LocalResult::Single(when) => Wall::Once(when.timestamp()),
            chrono::LocalResult::Ambiguous(earlier, later) => {
                Wall::Twice(earlier.timestamp(), later.timestamp())
            }
        },
    )?;
    Some(chrono::DateTime::from_timestamp(at, 0)?.with_timezone(&Local))
}

/// The next `count` local firings of `schedule` from now, for the jobs
/// screen's schedule field, which shows the next three as the expression is
/// typed. Empty when the expression will not parse; shorter than `count` only
/// when it fires fewer times than that before [`crate::cron::SEARCH_LIMIT_DAYS`].
///
/// A minute that a spring-forward gap skips is stepped over — the scan simply
/// moves to the next matching minute — the same way [`next_fire`] treats one.
pub fn next_fires(schedule: &str, count: usize) -> Vec<chrono::DateTime<Local>> {
    let Ok(cron) = Cron::parse(schedule) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(count);
    let mut cursor = Local::now().naive_local();
    while out.len() < count {
        let Some(next) = cron.next_after(cursor) else {
            break;
        };
        cursor = next;
        match Local.from_local_datetime(&next) {
            chrono::LocalResult::Single(when) | chrono::LocalResult::Ambiguous(when, _) => {
                out.push(when)
            }
            chrono::LocalResult::None => continue,
        }
    }
    out
}

/// How far before `after` [`next_real_fire`] begins its wall-clock scan, so
/// that a fall-back window which repeats a wall label is not missed.
///
/// A candidate instant `t` later than `now` carries a wall label
/// `t + offset(t)`. `chrono` clamps every UTC offset to the open interval
/// `(-24h, +24h)`, and `after` is itself `now + offset(now)`, so any such
/// label is greater than `now - 48h`, hence greater than `after - 48h`.
/// Deriving the window from the offset range this way, not from a table of
/// how far known zones roll back, is the whole point: date-line moves have
/// shifted a zone by nearly a full day (Pacific/Kwajalein lost 23 hours in
/// 1969), so any fixed few-hour guess would be a latent bug.
const LOOKBACK_HOURS: i64 = 48;

/// How one naive wall-clock minute lands in a real timezone.
enum Wall {
    /// It happens once, at this UTC-seconds instant.
    Once(i64),
    /// A spring-forward jump skips it — it never occurs.
    Skipped,
    /// A fall-back repeats it: the earlier occurrence, then the later one.
    Twice(i64, i64),
}

/// The UTC-seconds instant of the next firing of `cron` strictly after
/// `now` (UTC seconds), scanning naive-local minutes and asking `resolve`
/// how each one lands in the real timezone.
///
/// The scan starts [`LOOKBACK_HOURS`] before `after`, not at it. During the
/// first pass of a fall-back window the current wall label repeats later, so
/// a matching wall label *behind* `after` can still have a later occurrence
/// whose absolute instant is ahead of `now`. The window has to span the
/// widest a zone can ever roll back; [`LOOKBACK_HOURS`] derives that from the
/// UTC-offset range rather than from any one zone's transition. Candidates
/// that resolve to an instant at or before `now` are dropped by the match
/// arms below, so the earlier start only costs extra iterations, never a
/// wrong answer.
///
/// A spring-forward gap is scanned straight across — no candidate-count
/// bound, so a three-hour jump is no worse than a one-hour one. A fall-back
/// minute that repeats is honoured at whichever of its two occurrences is
/// still ahead of `now`, so the second pass through a repeated window is
/// never advertised at an instant already behind.
///
/// Termination: [`Cron::next_after`]'s own 400-year bound restarts from
/// `cursor` each call, so it cannot bound this loop. `deadline` does — one
/// fixed horizon, 400 years past `after`, past which the answer is `None`
/// whether or not any candidate ever landed on a real future instant.
fn next_real_fire(
    cron: &Cron,
    after: NaiveDateTime,
    now: i64,
    resolve: impl Fn(&NaiveDateTime) -> Wall,
) -> Option<i64> {
    let deadline = after
        .checked_add_signed(Duration::days(crate::cron::SEARCH_LIMIT_DAYS))
        .unwrap_or(NaiveDateTime::MAX);
    let mut cursor = after - Duration::hours(LOOKBACK_HOURS);
    loop {
        let candidate = cron.next_after(cursor)?;
        if candidate > deadline {
            return None;
        }
        cursor = candidate;
        match resolve(&candidate) {
            Wall::Skipped => continue,
            Wall::Once(at) if at > now => return Some(at),
            Wall::Once(_) => continue,
            Wall::Twice(earlier, _) if earlier > now => return Some(earlier),
            Wall::Twice(_, later) if later > now => return Some(later),
            Wall::Twice(..) => continue,
        }
    }
}

/// A rough "in 5h 48m" for a future delta, for the dispatcher's "next:" line
/// and `jobs list`'s NEXT column. Past or within the minute is "now".
pub fn until(delta: chrono::TimeDelta) -> String {
    let secs = delta.num_seconds();
    if secs <= 30 {
        return "now".to_string();
    }
    // Round up so a delta of a few seconds never renders as "in 0m".
    let mins = (secs + 59) / 60;
    if mins < 60 {
        return format!("in {mins}m");
    }
    let (hours, mins) = (mins / 60, mins % 60);
    if hours < 24 {
        return match mins {
            0 => format!("in {hours}h"),
            _ => format!("in {hours}h {mins}m"),
        };
    }
    let (days, hours) = (hours / 24, hours % 24);
    match (days < 14, hours) {
        (true, 0) => format!("in {days}d"),
        (true, _) => format!("in {days}d {hours}h"),
        (false, _) => format!("in {days}d"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::testutil::fixture;

    /// A naive local minute, for the timezone-resolution tests.
    fn naive(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    fn write_user_store(repo: &Repo, body: &str) {
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::write(repo.user_jobs_file(), body).unwrap();
    }

    fn write_project_store(repo: &Repo, body: &str) {
        std::fs::create_dir_all(repo.jobs_file().parent().unwrap()).unwrap();
        std::fs::write(repo.jobs_file(), body).unwrap();
    }

    #[test]
    fn no_store_is_an_empty_list() {
        let repo = fixture("jobs-none");
        assert!(load(&repo).unwrap().is_empty());
    }

    #[test]
    fn a_store_is_read_with_its_scope() {
        let repo = fixture("jobs-one");
        write_user_store(
            &repo,
            "[jobs.nightly-audit]\nschedule = \"0 3 * * 1-5\"\npipeline = \"impl\"\nroutine = \"nightly\"\n",
        );
        let jobs = load(&repo).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "nightly-audit");
        assert_eq!(jobs[0].scope, Scope::User);
        assert!(jobs[0].spec.enabled, "enabled by default");
    }

    #[test]
    fn the_two_stores_merge() {
        let repo = fixture("jobs-merge");
        write_user_store(
            &repo,
            "[jobs.a]\nschedule = \"@daily\"\npipeline = \"impl\"\nroutine = \"nightly\"\n",
        );
        write_project_store(
            &repo,
            "[jobs.b]\nschedule = \"@weekly\"\npipeline = \"impl\"\nroutine = \"weekly\"\n",
        );
        let jobs = load(&repo).unwrap();
        let names: Vec<&str> = jobs.iter().map(|job| job.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(jobs[0].scope, Scope::User);
        assert_eq!(jobs[1].scope, Scope::Project);
    }

    #[test]
    fn one_name_in_both_stores_is_refused_naming_both_paths() {
        let repo = fixture("jobs-collide");
        let spec = "schedule = \"@daily\"\npipeline = \"impl\"\nroutine = \"nightly\"\n";
        write_user_store(&repo, &format!("[jobs.dup]\n{spec}"));
        write_project_store(&repo, &format!("[jobs.dup]\n{spec}"));
        let err = format!("{:#}", load(&repo).unwrap_err());
        assert!(err.contains("defined in two stores"), "{err}");
        assert!(err.contains("jobs.toml"), "{err}");
    }

    #[test]
    fn a_malformed_store_is_an_error() {
        let repo = fixture("jobs-malformed");
        write_user_store(&repo, "[jobs.x]\nschedule = \"@daily\"\n"); // no pipeline/routine
        assert!(load(&repo).is_err());
    }

    #[test]
    fn a_routine_target_that_escapes_the_routines_directory_is_refused() {
        let repo = fixture("jobs-escape");
        for bad in ["../../etc/passwd", "/etc/passwd", "nightly/../../secret"] {
            write_user_store(
                &repo,
                &format!(
                    "[jobs.x]\nschedule = \"@daily\"\npipeline = \"default\"\nroutine = \"{bad}\"\n"
                ),
            );
            let job = &load(&repo).unwrap()[0];
            let err = format!("{:#}", job.target(&repo).unwrap_err());
            assert!(err.contains("inside .spoolway/routines/"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_routine_target_inside_the_routines_directory_resolves() {
        let repo = fixture("jobs-inside");
        write_user_store(
            &repo,
            "[jobs.x]\nschedule = \"@daily\"\npipeline = \"default\"\nroutine = \"nightly/audit.md\"\n",
        );
        let job = &load(&repo).unwrap()[0];
        assert_eq!(
            job.target(&repo).unwrap(),
            repo.routines_dir().join("nightly/audit.md")
        );
    }

    fn spec(schedule: &str) -> JobSpec {
        JobSpec {
            schedule: schedule.to_string(),
            pipeline: "impl".to_string(),
            routine: "nightly".to_string(),
            enabled: true,
        }
    }

    #[test]
    fn a_written_job_reads_back_through_load() {
        let repo = fixture("jobs-write");
        write(&repo, Scope::User, "nightly-audit", &spec("0 3 * * 1-5")).unwrap();

        let jobs = load(&repo).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "nightly-audit");
        assert_eq!(jobs[0].scope, Scope::User);
        assert_eq!(jobs[0].spec.schedule, "0 3 * * 1-5");
        assert!(jobs[0].spec.enabled, "no `enabled` key means enabled");
    }

    #[test]
    fn writing_a_job_leaves_a_sibling_and_a_comment_where_they_were() {
        let repo = fixture("jobs-write-preserve");
        write_user_store(
            &repo,
            "# hand-written note\n[jobs.weekly-deps]\nschedule = \"0 3 * * sun\"\n\
             pipeline = \"impl\"\nroutine = \"weekly\"\n",
        );

        write(&repo, Scope::User, "nightly-audit", &spec("@daily")).unwrap();

        let text = std::fs::read_to_string(repo.user_jobs_file()).unwrap();
        assert!(text.contains("# hand-written note"), "{text}");
        assert!(text.contains("[jobs.weekly-deps]"), "{text}");
        assert!(text.contains("[jobs.nightly-audit]"), "{text}");
        let jobs = load(&repo).unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn editing_a_job_keeps_its_own_comment_and_any_key_this_reader_does_not_know() {
        let repo = fixture("jobs-write-inplace");
        write_user_store(
            &repo,
            "# nightly deps audit\n[jobs.nightly]\nschedule = \"0 3 * * *\"\n\
             pipeline = \"impl\"\nroutine = \"nightly\"\nnotify = \"slack\"\n",
        );

        // Re-save with a changed schedule, the way `e` does.
        write(
            &repo,
            Scope::User,
            "nightly",
            &JobSpec {
                schedule: "0 4 * * 1-5".to_string(),
                pipeline: "impl".to_string(),
                routine: "nightly".to_string(),
                enabled: true,
            },
        )
        .unwrap();

        let text = std::fs::read_to_string(repo.user_jobs_file()).unwrap();
        assert!(
            text.contains("# nightly deps audit"),
            "comment kept: {text}"
        );
        assert!(
            text.contains("notify = \"slack\""),
            "unknown key kept: {text}"
        );
        assert!(text.contains("0 4 * * 1-5"), "schedule updated: {text}");
        assert!(!text.contains("0 3 * * *"), "old schedule gone: {text}");
    }

    #[test]
    fn write_and_delete_surface_a_store_that_will_not_read_rather_than_treat_it_as_absent() {
        let repo = fixture("jobs-store-unreadable");
        // A directory where the store file should be: `read_to_string` fails
        // with something other than NotFound, and neither call may pretend
        // the store is empty.
        std::fs::create_dir_all(repo.home()).unwrap();
        std::fs::create_dir_all(repo.user_jobs_file()).unwrap();

        assert!(write(&repo, Scope::User, "x", &spec("@daily")).is_err());
        assert!(delete(&repo, "x").is_err());
    }

    #[test]
    fn a_paused_job_writes_enabled_false_and_resuming_drops_the_key() {
        let repo = fixture("jobs-write-paused");
        let mut paused = spec("@daily");
        paused.enabled = false;
        write(&repo, Scope::User, "lint-sweep", &paused).unwrap();
        assert!(
            std::fs::read_to_string(repo.user_jobs_file())
                .unwrap()
                .contains("enabled = false")
        );

        write(&repo, Scope::User, "lint-sweep", &spec("@daily")).unwrap();
        assert!(
            !std::fs::read_to_string(repo.user_jobs_file())
                .unwrap()
                .contains("enabled"),
            "resuming removes the key rather than writing the default"
        );
    }

    #[test]
    fn delete_removes_a_job_from_the_store_that_holds_it_and_is_quiet_otherwise() {
        let repo = fixture("jobs-delete");
        write(&repo, Scope::Project, "a", &spec("@daily")).unwrap();
        write(&repo, Scope::Project, "b", &spec("@weekly")).unwrap();

        delete(&repo, "a").unwrap();
        delete(&repo, "never-existed").unwrap();

        let names: Vec<String> = load(&repo).unwrap().into_iter().map(|j| j.name).collect();
        assert_eq!(names, vec!["b"]);
    }

    #[test]
    fn next_fires_returns_the_next_three_in_order() {
        let fires = next_fires("*/30 * * * *", 3);
        assert_eq!(fires.len(), 3);
        assert!(fires[0] < fires[1] && fires[1] < fires[2]);
        assert!(fires[0] > Local::now());
        for when in &fires {
            let minute = when.format("%M").to_string();
            assert!(minute == "00" || minute == "30", "on the half hour: {when}");
        }
    }

    #[test]
    fn next_fires_is_empty_for_an_expression_that_will_not_parse() {
        assert!(next_fires("99 3 * * *", 3).is_empty());
    }

    #[test]
    fn until_reads_roughly() {
        assert_eq!(until(chrono::TimeDelta::seconds(10)), "now");
        assert_eq!(until(chrono::TimeDelta::minutes(5)), "in 5m");
        assert_eq!(until(chrono::TimeDelta::minutes(60 * 5 + 48)), "in 5h 48m");
        assert_eq!(until(chrono::TimeDelta::hours(72)), "in 3d");
    }

    #[test]
    fn a_nonexistent_local_minute_is_stepped_over_not_nudged_forward() {
        // `0 2 * * *` — 02:00 every day. 02:00 does not exist on 2027-03-14
        // (a spring-forward gap): the next real firing is 02:00 the day
        // after, not a fabricated 03:00 on the gap day.
        let cron = Cron::parse("0 2 * * *").unwrap();
        let gap = naive(2027, 3, 14, 2, 0);
        let after = naive(2027, 3, 13, 12, 0);
        let now = after.and_utc().timestamp();

        let at = next_real_fire(&cron, after, now, |when| {
            if *when == gap {
                Wall::Skipped
            } else {
                Wall::Once(when.and_utc().timestamp())
            }
        })
        .unwrap();

        assert_eq!(at, naive(2027, 3, 15, 2, 0).and_utc().timestamp());
    }

    #[test]
    fn a_two_hour_spring_gap_does_not_stop_the_search() {
        // A synthetic zone that jumps 01:00 straight to 03:00 in spring — 120
        // nonexistent minutes. Antarctica/Troll has a real spring gap of this
        // width; the date and offsets here are made up, not its schedule. An
        // every-minute job must still find the first real minute past the
        // gap; a candidate-count bound would give up.
        let cron = Cron::parse("* * * * *").unwrap();
        let gap_start = naive(2027, 3, 27, 1, 0);
        let gap_end = naive(2027, 3, 27, 3, 0);
        let after = naive(2027, 3, 27, 0, 59);
        let now = after.and_utc().timestamp();

        let at = next_real_fire(&cron, after, now, |when| {
            if *when >= gap_start && *when < gap_end {
                Wall::Skipped
            } else {
                Wall::Once(when.and_utc().timestamp())
            }
        })
        .unwrap();

        assert_eq!(
            at,
            gap_end.and_utc().timestamp(),
            "03:00, the first minute past the two-hour gap"
        );
    }

    #[test]
    fn a_repeated_fall_back_hour_advertises_the_occurrence_still_ahead() {
        // A synthetic fall-back: 01:30 local occurs twice, at `first` then
        // `second`, one hour apart in absolute time. The made-up UTC labels
        // stand in for a real zone's offsets.
        let cron = Cron::parse("30 1 * * *").unwrap();
        let dup = naive(2027, 11, 7, 1, 30);
        let first = naive(2027, 11, 7, 5, 30).and_utc().timestamp();
        let second = naive(2027, 11, 7, 6, 30).and_utc().timestamp();
        let resolve = move |when: &NaiveDateTime| {
            if *when == dup {
                Wall::Twice(first, second)
            } else {
                Wall::Once(when.and_utc().timestamp())
            }
        };

        // `after` before the wall label, `now` before both occurrences: the
        // earlier one.
        assert_eq!(
            next_real_fire(&cron, naive(2027, 11, 7, 1, 29), first - 600, resolve).unwrap(),
            first
        );
        // `after` already past the 01:30 wall label — the first pass of the
        // repeated hour — with `now` between the two occurrences. The second
        // 01:30 is still ahead in absolute time and must be found even though
        // its wall label is behind `after`. `cron.next_after(01:40)` alone
        // would jump to tomorrow.
        assert_eq!(
            next_real_fire(&cron, naive(2027, 11, 7, 1, 40), first + 600, resolve).unwrap(),
            second
        );
        // `after` past the wall label, `now` past both occurrences: the next
        // day's 01:30.
        assert_eq!(
            next_real_fire(&cron, naive(2027, 11, 7, 1, 40), second + 600, resolve).unwrap(),
            naive(2027, 11, 8, 1, 30).and_utc().timestamp()
        );
    }

    #[test]
    fn a_three_hour_fall_back_window_still_finds_the_later_occurrence() {
        // Casey Station rolls 03:00 back to 00:00 — the 00:00-03:00 window
        // runs twice, three hours apart. A 00:30 job seen at 02:50 on the
        // first pass, with `now` between the two 00:30s, must still be
        // advertised at the second 00:30. A two-hour lookback would miss the
        // 00:30 label and jump to tomorrow.
        let cron = Cron::parse("30 0 * * *").unwrap();
        let dup = naive(2027, 3, 9, 0, 30);
        let first = naive(2027, 3, 9, 16, 30).and_utc().timestamp();
        let second = first + 3 * 3600;
        let resolve = move |when: &NaiveDateTime| {
            if *when == dup {
                Wall::Twice(first, second)
            } else {
                Wall::Once(when.and_utc().timestamp())
            }
        };

        // `after` well past the wall label, `now` before both: the earlier.
        assert_eq!(
            next_real_fire(&cron, naive(2027, 3, 9, 2, 50), first - 600, resolve).unwrap(),
            first
        );
        // `now` between the two occurrences: the later one, three hours on.
        assert_eq!(
            next_real_fire(&cron, naive(2027, 3, 9, 2, 50), first + 600, resolve).unwrap(),
            second
        );
    }

    #[test]
    fn a_schedule_whose_every_firing_is_a_skipped_local_minute_returns_none_and_terminates() {
        // A resolver that never yields a real instant must not spin forever:
        // one fixed horizon bounds the scan, not `Cron::next_after`'s
        // per-call limit. `0 0 1 1 *` fires once a year, so reaching the
        // 400-year horizon is a few hundred cheap iterations.
        let cron = Cron::parse("0 0 1 1 *").unwrap();
        let after = naive(2027, 1, 1, 0, 0);
        assert_eq!(
            next_real_fire(&cron, after, after.and_utc().timestamp(), |_| Wall::Skipped),
            None
        );
    }

    /// A routine folder with one queueable document, and a job pointing at it
    /// on `pipeline` with an expression that matches every minute.
    fn every_minute_job(repo: &Repo, routine: &str, pipeline: &str) {
        let dir = repo.routines_dir().join(routine);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("audit.md"),
            "---\nid: audit\ntitle: audit, done\ngroup: demo\n---\n## Goal\n\nDo it.\n",
        )
        .unwrap();
        write_user_store(
            repo,
            &format!(
                "[jobs.nightly]\nschedule = \"* * * * *\"\npipeline = \"{pipeline}\"\nroutine = \"{routine}\"\n"
            ),
        );
    }

    fn fire(repo: &Repo) -> (Vec<String>, Vec<String>) {
        let (mut actions, mut problems) = (Vec::new(), Vec::new());
        fire_due(
            repo,
            &crate::pipeline::Pipelines::builtin(),
            &mut actions,
            &mut problems,
        );
        (actions, problems)
    }

    #[test]
    fn a_due_job_fires_onto_its_own_pipeline_and_records_the_minute() {
        let repo = fixture("jobs-fire");
        // `bugfix`, not the default: the job names its own pipeline and every
        // queued document has to land on it.
        every_minute_job(&repo, "nightly", "bugfix");

        let (actions, problems) = fire(&repo);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(
            actions
                .iter()
                .any(|line| line.contains("job nightly") && line.contains("fired")),
            "{actions:?}"
        );
        let minted: Vec<String> = repo
            .queued_ids()
            .into_iter()
            .filter(|id| id.starts_with("audit-"))
            .collect();
        assert_eq!(minted.len(), 1, "one document, one minted id: {minted:?}");
        assert_eq!(
            repo.task(&minted[0]).unwrap().front.pipeline.as_deref(),
            Some("bugfix")
        );
        assert!(read_state(&repo)["nightly"].fired_minute.is_some());
    }

    #[test]
    fn a_second_pass_in_the_same_minute_does_not_fire_again() {
        let repo = fixture("jobs-dedup");
        every_minute_job(&repo, "nightly", "default");

        fire(&repo);
        let before = repo.queued_ids().len();
        let (actions, _) = fire(&repo);
        assert!(
            actions.is_empty(),
            "nothing fired the second time: {actions:?}"
        );
        assert_eq!(repo.queued_ids().len(), before, "no new task was queued");
    }

    #[test]
    fn a_job_whose_last_run_is_still_in_the_queue_skips_and_says_so() {
        let repo = fixture("jobs-overlap");
        every_minute_job(&repo, "nightly", "default");
        crate::commands::testutil::add(&repo, "leftover", &[]);

        // A previous firing, an earlier minute, whose queued task is still here.
        let mut state = State::new();
        state.insert(
            "nightly".to_string(),
            FireRecord {
                fired_minute: Some("2000-01-01T00:00".to_string()),
                queued_ids: vec!["leftover".to_string()],
                ..Default::default()
            },
        );
        write_state(&repo, &state).unwrap();

        let (actions, problems) = fire(&repo);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(
            actions
                .iter()
                .any(|line| line.contains("job nightly") && line.contains("skipped")),
            "{actions:?}"
        );
        assert!(
            !repo.queued_ids().iter().any(|id| id.starts_with("audit-")),
            "nothing new was queued while the previous run is in flight"
        );
    }

    #[test]
    fn a_paused_job_never_fires() {
        let repo = fixture("jobs-paused");
        let dir = repo.routines_dir().join("nightly");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("audit.md"),
            "---\nid: audit\ntitle: audit, done\ngroup: demo\n---\n## Goal\n\nDo it.\n",
        )
        .unwrap();
        write_user_store(
            &repo,
            "[jobs.nightly]\nschedule = \"* * * * *\"\npipeline = \"default\"\nroutine = \"nightly\"\nenabled = false\n",
        );

        let (actions, problems) = fire(&repo);
        assert!(
            actions.is_empty() && problems.is_empty(),
            "{actions:?} {problems:?}"
        );
        assert!(repo.queued_ids().is_empty());
    }
}
