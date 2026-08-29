//! Comparing versions of a setup by what they cost to run.
//!
//! This is a **view over the ledger**, not a subsystem: every figure here is
//! grouped out of the same `usage.jsonl` that [`crate::spend`] reads for
//! `--by`, with nothing kept in a second store.
//!
//! The honest limit is worth stating where the code is, not only in the help.
//! Tasks differ in difficulty, so a version that happens to have drawn easy
//! work looks better than one that drew hard work, and no statistic in here
//! corrects for that. What the view can do is show the sample size beside
//! every figure, which is why `RUNS` is a column and not a footnote: a reader
//! who sees a `PASS` of 79% next to a `RUNS` of two can discount it
//! themselves, and will do it better than a threshold could.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, bail};

use crate::cli::EvalArgs;
use crate::pipeline::Pipelines;
use crate::repo::Repo;
use crate::screen::{Key, PollableRead, overlay, pad_to, panel, read_key};
use crate::usage::{Entry, ModelPrice};

/// A version with no fingerprint on its lines — everything written before
/// versions were recorded. Kept rather than dropped, because a project's
/// history did not start when this feature landed.
const UNVERSIONED: &str = "unversioned";

pub fn run(repo: &Repo, args: &EvalArgs, json: bool) -> Result<()> {
    if json && args.csv {
        bail!("`--csv` and `--json` are two different exports of the same rows — pick one");
    }

    // `--by` is a deprecated alias: the spend table moved to `spoolway
    // spend`, and this still routes there so a script or skill written
    // before the split keeps working. `by` is a plain `String` here (see
    // `EvalArgs::by`'s own doc) rather than `SpendBy`, because there is no
    // longer an `Auto` sentinel variant for `default_missing_value` to fill a
    // bare `--by` with — an empty string means the same thing `Auto` did.
    if let Some(raw) = &args.by {
        eprintln!(
            "`eval --by` has moved to `spoolway spend` — this still works, but switch when you can."
        );
        let cut = match raw.as_str() {
            "" => None,
            other => Some(
                <crate::cli::SpendBy as clap::ValueEnum>::from_str(other, true)
                    .map_err(|e| anyhow::anyhow!(e))?,
            ),
        };
        let filters = crate::spend::Filters {
            since: args.since.as_deref(),
            until: args.until.as_deref(),
            month: args.month.as_deref(),
            all: args.all,
            project: args.project.as_deref(),
        };
        return crate::spend::print(repo, cut, &filters, json, args.csv);
    }

    // Reading is also what catches the ledger up — see `usage::sweep`, and
    // `spend::print`, which sweeps for the same reason.
    crate::usage::sweep(repo);

    let (mut entries, scope) =
        crate::spend::collect_scoped(repo, args.project.as_deref(), args.all)?;

    // A skill session is a conversation, not a lane: it reports no outcome and
    // belongs to no task, so counting it would put lanes in the table that
    // nothing in the table's columns describes. Held aside rather than dropped
    // — it gets a block of its own, under the pipeline blocks.
    let skills: Vec<Entry> = entries.iter().filter(|e| e.is_skill()).cloned().collect();
    entries.retain(|entry| !entry.is_skill());

    // Derived from the whole scope, before the window/step/pipeline filters
    // below narrow `entries` further — so a historical run's id is the same
    // key in every view over this project.
    let fallback = fallback_keys(&entries);

    let window = crate::spend::window_of(None, args.since.as_deref(), args.until.as_deref())?;
    entries.retain(|entry| window.contains(&entry.ts));
    let skills: Vec<Entry> = skills
        .into_iter()
        .filter(|entry| window.contains(&entry.ts))
        .collect();

    if let Some(step) = &args.step {
        let before = entries.len();
        entries.retain(|entry| &entry.step == step);
        if entries.is_empty() && before > 0 {
            bail!("no lanes ran step `{step}` in that window");
        }
    }

    if let Some(pipeline) = &args.pipeline {
        entries.retain(|entry| &entry.pipeline == pipeline);
    }

    // Whether the skills block is this view's to print at all: it is a cut of
    // no pipeline, so it belongs beside the whole table and nowhere a pipeline
    // or a step has already narrowed the screen to one slice of it.
    let skills_eligible = args.pipeline.is_none() && args.step.is_none() && !args.runs;

    if entries.is_empty() {
        if skills_eligible && !json && !args.csv && !skills.is_empty() {
            print_skills(&skills, &repo.config.skills, args.limit());
            return Ok(());
        }
        println!("Nothing to compare in {} yet.", scope.what);
        println!(
            "A version is recorded when a step finishes, so this fills up as `spoolway \
             dispatch` runs."
        );
        return Ok(());
    }

    entries.sort_by(|a, b| a.ts.cmp(&b.ts));

    if args.runs {
        // `--task`, `--group` and `--trial` narrow which runs land in the
        // table — clap already refuses each of them without `--runs` (they
        // carry `requires = "runs"`), so nothing here has to check that
        // again.
        let mut runs: Vec<Entry> = entries.clone();
        if let Some(task) = &args.task {
            runs.retain(|entry| &entry.task == task);
        }
        if let Some(group) = &args.run_group {
            runs.retain(|entry| entry.plan.as_deref() == Some(group.as_str()));
        }
        if let Some(trial) = &args.trial {
            runs.retain(|entry| entry.trial.as_deref() == Some(trial.as_str()));
        }
        let rows = list_runs(&runs, &fallback, &repo.config.models);
        // `--trial` asks the question a trial exists to answer: its own
        // comparison, not just a narrower version of the same table.
        if args.trial.is_some() && !args.csv {
            return print_trial_comparison(&rows, &runs);
        }
        return match args.csv {
            true => print_run_csv(&rows),
            false => print_run_table(&rows, &runs),
        };
    }

    let blocks = pipeline_blocks(&entries, args.limit());

    if json {
        return print_json(
            &entries,
            &skills,
            &blocks,
            skills_eligible,
            &fallback,
            &repo.config.models,
        );
    }

    if args.csv {
        return print_csv(&entries, &blocks, &fallback, &repo.config.models);
    }

    // Whether the rows about to print touch more than one project — decided
    // over what is actually on screen, not the wider scope `--all` may have
    // pulled in, so `PROJECT` earns its column only when it would otherwise
    // read as two rows nobody could tell apart.
    let show_project = spans_more_than_one_project(&blocks);

    println!("{}", paint("pipelines", "1"));
    for (n, block) in blocks.iter().enumerate() {
        if n > 0 {
            println!();
        }
        print_block(
            &entries,
            block,
            &fallback,
            &repo.config.models,
            show_project,
        );
    }

    footer(&entries, &blocks, &fallback);

    if skills_eligible && !skills.is_empty() {
        println!();
        print_skills(&skills, &repo.config.skills, args.limit());
    }

    Ok(())
}

/// One version, in the order it will be printed: newest first.
struct Version {
    name: String,
    /// The project of this version's earliest row in scope. Shown rather than
    /// used to split rows: two projects producing the same fingerprint by
    /// coincidence is a fact worth seeing, not a reason to fork the table.
    project: String,
    /// Local date of its earliest lane in this window.
    since: String,
}

/// One pipeline's block: its name, and its versions newest first.
struct PipelineBlock {
    name: String,
    versions: Vec<Version>,
}

/// Every pipeline with a lane in `entries`, ordered so the one whose newest
/// version is newest leads — this morning's work heads the stack, whatever
/// pipeline it ran on. `entries` must already be sorted oldest first, which
/// `run` guarantees before calling this.
fn pipeline_blocks(entries: &[Entry], limit: usize) -> Vec<PipelineBlock> {
    let mut order: Vec<&str> = Vec::new();
    let mut latest_ts: HashMap<&str, &str> = HashMap::new();
    for entry in entries {
        let name = entry.pipeline.as_str();
        if !latest_ts.contains_key(name) {
            order.push(name);
        }
        // Overwritten by every later entry — entries arrive oldest first, so
        // what survives is each pipeline's latest timestamp.
        latest_ts.insert(name, entry.ts.as_str());
    }
    order.sort_by(|a, b| latest_ts[b].cmp(latest_ts[a]));

    order
        .into_iter()
        .map(|name| PipelineBlock {
            name: name.to_string(),
            versions: versions_of(entries, name, limit),
        })
        .collect()
}

/// Whether the rows `blocks` is about to print carry more than one distinct
/// project — the test `run` applies to decide whether `PROJECT` earns a
/// column at all. Pulled out of `run` so it is a function with an answer of
/// its own, not a line only exercised end to end.
fn spans_more_than_one_project(blocks: &[PipelineBlock]) -> bool {
    blocks
        .iter()
        .flat_map(|b| b.versions.iter())
        .map(|v| v.project.as_str())
        .collect::<HashSet<_>>()
        .len()
        > 1
}

fn versions_of(entries: &[Entry], pipeline: &str, limit: usize) -> Vec<Version> {
    let mut order: Vec<String> = Vec::new();
    let mut first: HashMap<String, String> = HashMap::new();
    let mut projects: HashMap<String, String> = HashMap::new();

    for entry in entries.iter().filter(|e| e.pipeline == pipeline) {
        let name = version_of(entry).to_string();
        if !first.contains_key(&name) {
            order.push(name.clone());
            first.insert(name.clone(), entry.ts.clone());
            projects.insert(name.clone(), entry.project.clone());
        }
    }

    // Entries arrive oldest first, so `order` is oldest first: reverse for a
    // newest-first table, then keep the newest `limit`.
    order.reverse();
    order.truncate(limit.max(1));
    order
        .into_iter()
        .map(|name| Version {
            since: first
                .get(&name)
                .map(|ts| local_date(ts))
                .unwrap_or_default(),
            project: projects.get(&name).cloned().unwrap_or_default(),
            name,
        })
        .collect()
}

fn version_of(entry: &Entry) -> &str {
    entry.version.as_deref().unwrap_or(UNVERSIONED)
}

pub(crate) fn local_date(ts: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_else(|_| "—".to_string())
}

// ------------------------------------------------------------------- runs

/// The run a lane belongs to: its own id, or — for a line written before runs
/// existed — the stable key [`fallback_keys`] derived for its task.
fn run_key(entry: &Entry, fallback: &HashMap<(String, String), String>) -> String {
    entry.run.clone().unwrap_or_else(|| {
        fallback
            .get(&(entry.project.clone(), entry.task.clone()))
            .cloned()
            .unwrap_or_else(|| entry.task.clone())
    })
}

/// A stable name for every task whose lines predate runs: `<task>@<date of
/// its earliest round here>`. One key per (project, task) — every line that
/// task ever wrote before this feature landed collapses into it, exactly as
/// `spoolway eval` grouped them before a run existed to group by instead.
///
/// A run id (unlike its project) is not qualified anywhere else in this view,
/// so two projects whose same-named task first ran on the same day would
/// otherwise mint the identical bare name — an id that named two runs would
/// pin neither. Where that happens, both are requalified to
/// `<project>/<task>@<date>` so every id this returns is unique across the
/// whole scope it was derived from, not just within one project.
pub(crate) fn fallback_keys(entries: &[Entry]) -> HashMap<(String, String), String> {
    let mut first: HashMap<(String, String), String> = HashMap::new();
    for entry in entries {
        if entry.run.is_some() {
            continue;
        }
        let key = (entry.project.clone(), entry.task.clone());
        first
            .entry(key)
            .and_modify(|ts| {
                if entry.ts < *ts {
                    *ts = entry.ts.clone();
                }
            })
            .or_insert_with(|| entry.ts.clone());
    }

    let mut named: HashMap<(String, String), String> = first
        .into_iter()
        .map(|(key, ts): ((String, String), String)| {
            let date = chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|at| at.with_timezone(&chrono::Local).format("%m%d").to_string())
                .unwrap_or_else(|_| "0000".to_string());
            let name = format!("{}@{date}", key.1);
            (key, name)
        })
        .collect();

    let mut by_name: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for (key, name) in &named {
        by_name.entry(name.clone()).or_default().push(key.clone());
    }
    for (name, keys) in by_name {
        let projects: HashSet<&str> = keys.iter().map(|(project, _)| project.as_str()).collect();
        if projects.len() > 1 {
            for key in keys {
                let qualified = format!("{}/{name}", key.0);
                named.insert(key, qualified);
            }
        }
    }

    named
}

/// Every run in `entries`, keyed by `(project, run key)`, in first-seen order.
fn group_by_run<'a>(
    entries: &'a [Entry],
    fallback: &HashMap<(String, String), String>,
) -> Vec<((String, String), Vec<&'a Entry>)> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), Vec<&Entry>> = HashMap::new();
    for entry in entries {
        let key = (entry.project.clone(), run_key(entry, fallback));
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(entry);
    }
    order
        .into_iter()
        .map(|key| {
            let lanes = groups.remove(&key).unwrap_or_default();
            (key, lanes)
        })
        .collect()
}

/// One run, as `spoolway eval --runs` prints it.
pub struct RunRow {
    pub id: String,
    pub task: String,
    /// The latest lane's timestamp, for sorting and display.
    pub ts: String,
    pub version: String,
    pub pipeline: String,
    pub lanes: usize,
    pub passed: usize,
    pub judged: usize,
    pub blocked: usize,
    pub out: u64,
    pub cost: f64,
    pub unpriced: usize,
    pub time_s: i64,
    /// The largest `ctx_peak` any of this run's lanes banked. See
    /// [`ctx_peak_of`].
    pub ctx_peak_tokens: Option<u64>,
    pub ctx_peak_pct: Option<f64>,
}

impl RunRow {
    pub(crate) fn pass_share(&self) -> Option<f64> {
        (self.judged > 0).then(|| 100.0 * self.passed as f64 / self.judged as f64)
    }
}

/// Every run in `entries`, oldest first.
pub fn list_runs(
    entries: &[Entry],
    fallback: &HashMap<(String, String), String>,
    models: &BTreeMap<String, ModelPrice>,
) -> Vec<RunRow> {
    let mut rows: Vec<RunRow> = group_by_run(entries, fallback)
        .into_iter()
        .map(|((_, id), lanes)| {
            let latest = lanes
                .iter()
                .max_by(|a, b| a.ts.cmp(&b.ts))
                .expect("a run always has at least one lane");
            let task = lanes.first().map(|e| e.task.clone()).unwrap_or_default();
            let mut judged = 0;
            let mut passed = 0;
            let mut blocked = 0;
            for entry in &lanes {
                if entry.outcome.as_deref() == Some("block") {
                    blocked += 1;
                }
                if let Some(outcome) = entry.outcome.as_deref() {
                    judged += 1;
                    if outcome == "pass" {
                        passed += 1;
                    }
                }
            }
            let (ctx_peak_tokens, ctx_peak_pct) = ctx_peak_of(&lanes, models);
            RunRow {
                id,
                task,
                ts: latest.ts.clone(),
                version: version_of(latest).to_string(),
                pipeline: latest.pipeline.clone(),
                lanes: lanes.len(),
                passed,
                judged,
                blocked,
                out: lanes.iter().map(|e| e.tokens.output).sum(),
                cost: lanes.iter().filter_map(|e| e.cost_usd).sum(),
                // A zero-token line — an enrolment line, a synthetic turn —
                // spends nothing, so it must not be counted as a lane the
                // total is missing a price for. See `unpriced_note`.
                unpriced: lanes
                    .iter()
                    .filter(|e| e.cost_usd.is_none() && !e.tokens.is_zero())
                    .count(),
                time_s: lanes.iter().map(|e| e.wall_s).sum(),
                ctx_peak_tokens,
                ctx_peak_pct,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.ts.cmp(&b.ts));
    rows
}

/// A cost cell for `spoolway eval`'s own tables and screen: a plain number,
/// or `—` when nothing behind it could be priced at all. Whether it is a
/// floor (some, but not all, of `complete` priced) is no longer marked on
/// the cell itself — the `unpriced_note` printed under the table says that
/// once for the whole table, and a `+?` on every affected cell said the same
/// thing a second time in a shape a pasted table could not parse as a number.
fn cost_of(row_cost: f64, complete: usize, incomplete: usize) -> String {
    match complete - incomplete.min(complete) {
        0 => "—".to_string(),
        _ => crate::fmt::money_plain(Some(row_cost)),
    }
}

/// `entries` is the same slice `list_runs` built `rows` from — kept apart
/// from `rows` because a `RunRow` has already folded its lanes down to a
/// total and no longer carries the per-lane model name `unpriced_note` needs.
pub fn print_run_table(rows: &[RunRow], entries: &[Entry]) -> Result<()> {
    if rows.is_empty() {
        println!("No runs in that window.");
        return Ok(());
    }
    let tw = rows.iter().map(|r| r.task.len()).max().unwrap_or(4).max(4);
    let vw = rows
        .iter()
        .map(|r| r.version.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let pw = rows
        .iter()
        .map(|r| r.pipeline.len())
        .max()
        .unwrap_or(8)
        .max(8);

    println!(
        "{:<tw$}  {:<10}  {:<vw$}  {:<pw$}  {:>5}  {:>4}  {:>6}  {:>8}  {:>7}  {:>8}  {:>8}",
        "TASK",
        "WHEN",
        "VERSION",
        "PIPELINE",
        "LANES",
        "PASS",
        "BLOCKS",
        "CTX PEAK",
        "OUT",
        "COST USD",
        "TIME"
    );
    for row in rows {
        println!(
            "{:<tw$}  {:<10}  {:<vw$}  {:<pw$}  {:>5}  {:>4}  {:>6}  {:>8}  {:>7}  {:>8}  {:>8}",
            row.task,
            local_date(&row.ts),
            row.version,
            row.pipeline,
            row.lanes,
            percent(row.pass_share()),
            row.blocked,
            ctx_cell(row.ctx_peak_tokens, row.ctx_peak_pct),
            crate::fmt::tokens_human(row.out),
            cost_of(row.cost, row.lanes, row.unpriced),
            crate::status::human_secs(row.time_s.max(0)),
        );
    }

    if let Some(note) = unpriced_note(entries.iter()) {
        println!("\n{note}");
    }
    Ok(())
}

/// `spoolway eval --runs --trial <id>`: the same table [`print_run_table`]
/// prints, one row per arm, plus a delta line per arm against the first —
/// the comparison a trial exists to answer, since the table's own columns
/// already carry pass rate, cost and time but say nothing about the gap
/// between two rows without a reader doing the subtraction by eye.
fn print_trial_comparison(rows: &[RunRow], entries: &[Entry]) -> Result<()> {
    if rows.is_empty() {
        println!("No runs recorded for that trial yet.");
        return Ok(());
    }

    print_run_table(rows, entries)?;

    if rows.len() > 1 {
        println!();
        let baseline = &rows[0];
        for row in &rows[1..] {
            println!("{}", trial_delta_line(baseline, row));
        }
    }
    Ok(())
}

/// One arm against the trial's first arm: the sign says which way it moved,
/// never which one is "better" — a trial exists to let the reader decide
/// that, not to decide it for them.
fn trial_delta_line(baseline: &RunRow, row: &RunRow) -> String {
    let pass = match (baseline.pass_share(), row.pass_share()) {
        (Some(base), Some(now)) => format!("{:+.0}pp", now - base),
        _ => "n/a".to_string(),
    };
    let cost = row.cost - baseline.cost;
    let time = row.time_s - baseline.time_s;
    format!(
        "{} vs {}: pass {pass}, cost {}{:.2}, time {}{}",
        row.task,
        baseline.task,
        if cost >= 0.0 { "+$" } else { "-$" },
        cost.abs(),
        if time >= 0 { "+" } else { "-" },
        crate::status::human_secs(time.abs()),
    )
}

fn print_run_csv(rows: &[RunRow]) -> Result<()> {
    println!(
        "run,task,when,version,pipeline,lanes,pass,blocks,out_tokens,cost_usd,unpriced,time_s"
    );
    for row in rows {
        println!(
            "{},{},{},{},{},{},{},{},{},{},{},{}",
            row.id,
            row.task,
            local_date(&row.ts),
            row.version,
            row.pipeline,
            row.lanes,
            csv_fraction(row.pass_share()),
            row.blocked,
            row.out,
            csv_cost(row.cost, row.lanes, row.unpriced),
            row.unpriced,
            row.time_s.max(0),
        );
    }
    Ok(())
}

fn csv_fraction(share: Option<f64>) -> String {
    match share {
        Some(pct) => format!("{:.2}", pct / 100.0),
        None => String::new(),
    }
}

/// A CSV cost cell, blank rather than a false `0.00` when every one of
/// `total` is among `unpriced` — the same "unknown, not free" rule
/// [`cost_of`] states for the table and the screen, spelled the way an empty
/// cell already spells "nothing to resolve" for `ctx_peak_pct`. A floor
/// (some but not all of `total` unpriced) still prints the number it did
/// before; the `unpriced` column beside it is what tells a reader that
/// number is a floor rather than a total, the same job the note under the
/// table now does for `cost_of`'s own cells.
fn csv_cost(cost: f64, total: usize, unpriced: usize) -> String {
    match total.saturating_sub(unpriced) {
        0 => String::new(),
        _ => format!("{cost:.2}"),
    }
}

// -------------------------------------------------------------- versioned metrics

/// What one row of the table came to: every lane whose pipeline and version
/// match this row.
#[derive(Default, Clone)]
pub(crate) struct Metrics {
    /// Distinct runs that touched this row — any lane of theirs matched, not
    /// necessarily every one. A run whose lanes straddled a version edit
    /// counts on both rows it touched; see [`footer`].
    pub(crate) tasks: usize,
    /// One ledger line per lane launched, matching this row.
    pub(crate) lanes: usize,
    /// Lanes that reported `pass`, and lanes anything is known about. A lane
    /// that never reported — killed, or silent — is left out of both: it is
    /// not a failure, it is a lane nobody heard from.
    pub(crate) passed: usize,
    pub(crate) judged: usize,
    /// Lanes that ended blocked.
    pub(crate) blocked: usize,
    pub(crate) out: u64,
    pub(crate) cost: f64,
    /// Lanes nothing could price. Cost is a floor when this is above zero.
    pub(crate) unpriced: usize,
    /// Every lane's own wall time, summed — the same figure
    /// [`crate::status::lane_time_at`] sums for one task at one step, just
    /// over every lane this row holds instead.
    pub(crate) time_s: i64,
    /// The largest `ctx_peak` any lane on this row banked, in raw tokens —
    /// `None` where no lane on the row banked one at all. See [`ctx_peak_of`].
    pub(crate) ctx_peak_tokens: Option<u64>,
    /// That peak as a share of the window of the model that banked it —
    /// `None` where that model resolves to no configured `context_window`,
    /// the same admitted blank `spoolway eval --by` gives an unpriced model.
    pub(crate) ctx_peak_pct: Option<f64>,
}

impl Metrics {
    fn for_row(
        entries: &[Entry],
        pipeline: &str,
        version: &str,
        fallback: &HashMap<(String, String), String>,
        models: &BTreeMap<String, ModelPrice>,
    ) -> Self {
        let matching: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.pipeline == pipeline && version_of(e) == version)
            .collect();
        Metrics::for_matching(&matching, fallback, models)
    }

    /// The same row, narrowed to one step of it, over every version in
    /// scope — what the steps view's own rows are built from. Not a filter
    /// the printing path needs: `--step` already narrows `entries` itself
    /// before a block is ever built, so nothing there calls this.
    fn for_step(
        entries: &[Entry],
        pipeline: &str,
        step: &str,
        fallback: &HashMap<(String, String), String>,
        models: &BTreeMap<String, ModelPrice>,
    ) -> Self {
        let matching: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.pipeline == pipeline && e.step == step)
            .collect();
        Metrics::for_matching(&matching, fallback, models)
    }

    /// The aggregation both `for_row` and `for_step` do, once `matching` has
    /// already narrowed `entries` to whatever this row stands for.
    fn for_matching(
        matching: &[&Entry],
        fallback: &HashMap<(String, String), String>,
        models: &BTreeMap<String, ModelPrice>,
    ) -> Self {
        let mut out = Metrics::default();
        let mut runs: HashSet<(String, String)> = HashSet::new();
        for entry in matching {
            out.lanes += 1;
            out.time_s += entry.wall_s;
            out.out += entry.tokens.output;
            match entry.cost_usd {
                Some(cost) => out.cost += cost,
                // A zero-token line spends nothing, so it must not turn this
                // row's total into a floor — see `unpriced_note`.
                None if entry.tokens.is_zero() => {}
                None => out.unpriced += 1,
            }
            if entry.outcome.as_deref() == Some("block") {
                out.blocked += 1;
            }
            if let Some(outcome) = entry.outcome.as_deref() {
                out.judged += 1;
                if outcome == "pass" {
                    out.passed += 1;
                }
            }
            runs.insert((entry.project.clone(), run_key(entry, fallback)));
        }
        out.tasks = runs.len();
        (out.ctx_peak_tokens, out.ctx_peak_pct) = ctx_peak_of(matching, models);
        out
    }

    pub(crate) fn pass_share(&self) -> Option<f64> {
        (self.judged > 0).then(|| 100.0 * self.passed as f64 / self.judged as f64)
    }

    /// `0.0` rather than a division by zero — every row shown has at least
    /// one lane, and so at least one task, but the default instance a test
    /// scaffolds with does not.
    pub(crate) fn lanes_per_task(&self) -> f64 {
        match self.tasks {
            0 => 0.0,
            n => self.lanes as f64 / n as f64,
        }
    }

    pub(crate) fn out_per_task(&self) -> f64 {
        match self.tasks {
            0 => 0.0,
            n => self.out as f64 / n as f64,
        }
    }

    pub(crate) fn cost_per_task(&self) -> f64 {
        match self.tasks {
            0 => 0.0,
            n => self.cost / n as f64,
        }
    }

    pub(crate) fn time_per_task_s(&self) -> f64 {
        match self.tasks {
            0 => 0.0,
            n => self.time_s as f64 / n as f64,
        }
    }

    pub(crate) fn lanes_per_task_str(&self) -> String {
        format!("{:.1}", self.lanes_per_task())
    }

    /// Dividing a floor by its task count is still a floor, but `cost_of`
    /// prints the same plain number either way — the note under the table
    /// says once, for the whole thing, when any row's figure is an
    /// underestimate, rather than marking every affected cell.
    pub(crate) fn cost_per_task_str(&self) -> String {
        cost_of(self.cost_per_task(), self.lanes, self.unpriced)
    }

    pub(crate) fn time_per_task_str(&self) -> String {
        crate::status::human_secs(self.time_per_task_s().round() as i64)
    }

    pub(crate) fn ctx_str(&self) -> String {
        ctx_cell(self.ctx_peak_tokens, self.ctx_peak_pct)
    }
}

/// The largest `ctx_peak` among `lanes`, and — where it can be resolved —
/// that peak as a share of the `context_window` of the model that banked it.
///
/// Only the winning lane's model is ever resolved: a row can mix models
/// across its lanes, but `CTX PEAK` reports on the one reading that mattered,
/// not an average of windows that may not even be comparable.
fn ctx_peak_of(
    lanes: &[&Entry],
    models: &BTreeMap<String, ModelPrice>,
) -> (Option<u64>, Option<f64>) {
    let peak = lanes
        .iter()
        .filter_map(|e| e.ctx_peak.map(|tokens| (tokens, e.model.as_str())))
        .max_by_key(|(tokens, _)| *tokens);
    let tokens = peak.map(|(tokens, _)| tokens);
    let pct = peak.and_then(|(tokens, model)| {
        crate::models::resolve(models, model)
            .price
            .map(|price| price.context_window)
            .filter(|window| *window > 0)
            .map(|window| tokens as f64 / window as f64)
    });
    (tokens, pct)
}

/// The `CTX PEAK` cell: a percentage of the model's window when one
/// resolved, the raw peak in tokens when the model's window is unset, and
/// `—` when no lane on the row banked a peak at all — never a guess, the same
/// rule `spoolway eval --by` follows for an unpriced model.
fn ctx_cell(tokens: Option<u64>, pct: Option<f64>) -> String {
    match (tokens, pct) {
        (None, _) => "—".to_string(),
        (Some(_), Some(pct)) => format!("{:.0}%", pct * 100.0),
        (Some(tokens), None) => ctx_tokens_human(tokens),
    }
}

/// Token counts for the `CTX PEAK` cell's raw fallback, rounded to a whole
/// unit — unlike [`crate::fmt::tokens_human`]'s one decimal place, because this
/// column is squeezed beside a percentage the rest of the time and a bare
/// `142k` reads as one figure rather than two.
fn ctx_tokens_human(n: u64) -> String {
    match n {
        0 => "0".to_string(),
        n if n < 1_000 => n.to_string(),
        n if n < 1_000_000 => format!("{}k", (n as f64 / 1_000.0).round() as u64),
        n => format!("{:.2}M", n as f64 / 1_000_000.0),
    }
}

// ---------------------------------------------------------------- formatting

/// ANSI colour, only where a terminal is going to read it.
pub(crate) fn paint(text: &str, code: &str) -> String {
    use std::io::IsTerminal;
    match std::io::stdout().is_terminal() {
        true => format!("\x1b[{code}m{text}\x1b[0m"),
        false => text.to_string(),
    }
}

fn percent(value: Option<f64>) -> String {
    value.map_or("—".to_string(), |v| format!("{v:.0}%"))
}

const HEAD: &str = "\x1b[2m";

/// How wide the version column has to be.
///
/// `unversioned` is longer than a fingerprint, so a fixed width would push
/// every column on that row out of line with the rest of the table.
fn name_width(versions: &[Version]) -> usize {
    versions
        .iter()
        .map(|v| v.name.len())
        .max()
        .unwrap_or(8)
        .max("VERSION".len())
}

fn project_width(versions: &[Version]) -> usize {
    versions
        .iter()
        .map(|v| v.project.len())
        .max()
        .unwrap_or(7)
        .max("PROJECT".len())
}

/// `pw` and the leading `PROJECT` cell are only worth printing when
/// `show_project` is set — see [`run`]'s own reasoning for when that is true.
///
/// Colour-free — pulled out of [`header`] so the screen can lay its own
/// header row over exactly these columns without also measuring an ANSI
/// escape as a visible character, which would throw its frame's column
/// count off. See the screen's own module comment.
fn header_plain(show_project: bool, pw: usize, vw: usize) -> String {
    let mut line = String::new();
    if show_project {
        line.push_str(&format!("{:<pw$}  ", "PROJECT"));
    }
    line.push_str(&format!(
        "{:<vw$}   {:<10}  {:>5}  {:>6}  {:>4}  {:>6}  {:>8}  {:>7}  {:>9}",
        "VERSION", "SINCE", "RUNS", "L/RUN", "PASS", "BLOCKS", "CTX PEAK", "USD/RUN", "TIME/RUN",
    ));
    line.trim_end().to_string()
}

fn header(show_project: bool, pw: usize, vw: usize) -> String {
    let line = header_plain(show_project, pw, vw);
    use std::io::IsTerminal;
    match std::io::stdout().is_terminal() {
        true => format!("{HEAD}{line}\x1b[0m"),
        false => line,
    }
}

fn full_row(show_project: bool, version: &Version, m: &Metrics, pw: usize, vw: usize) -> String {
    let mut line = String::new();
    if show_project {
        line.push_str(&format!("{:<pw$}  ", version.project));
    }
    line.push_str(&format!(
        "{:<vw$}   {:<10}  {:>5}  {:>6}  {:>4}  {:>6}  {:>8}  {:>7}  {:>9}",
        version.name,
        version.since,
        m.tasks,
        m.lanes_per_task_str(),
        percent(m.pass_share()),
        m.blocked,
        m.ctx_str(),
        m.cost_per_task_str(),
        m.time_per_task_str(),
    ));
    line
}

fn print_block(
    entries: &[Entry],
    block: &PipelineBlock,
    fallback: &HashMap<(String, String), String>,
    models: &BTreeMap<String, ModelPrice>,
    show_project: bool,
) {
    let metrics: Vec<Metrics> = block
        .versions
        .iter()
        .map(|v| Metrics::for_row(entries, &block.name, &v.name, fallback, models))
        .collect();

    let pw = project_width(&block.versions);
    let vw = name_width(&block.versions);
    println!(
        "{}\n{}",
        paint(&block.name, "1"),
        header(show_project, pw, vw)
    );
    for (i, version) in block.versions.iter().enumerate() {
        println!("{}", full_row(show_project, version, &metrics[i], pw, vw));
    }
}

// ------------------------------------------------------------------- skills

/// What one bucket's one version's skill sessions came to.
///
/// The unit is the distinct **session**, not the ledger row: one session banks
/// a line every time it is swept. There is no `RUNS` and no `PASS` here — a
/// conversation is judged by nobody and belongs to no task.
struct SkillVersion {
    name: String,
    since: String,
    sessions: usize,
    cost: f64,
    /// Sessions nothing could price. Cost is a floor when this is above zero.
    unpriced: usize,
}

impl SkillVersion {
    /// What one session of this version cost on average, or `None` when
    /// nothing here could be priced at all.
    fn per_session(&self) -> Option<f64> {
        match self.sessions > self.unpriced {
            true => Some(self.cost / self.sessions as f64),
            false => None,
        }
    }
}

/// One named skill's block: its versions, newest first, and its total cost —
/// what orders it against the other blocks.
struct SkillBlock {
    name: String,
    total_cost: f64,
    versions: Vec<SkillVersion>,
}

/// Bucket every skill line by [`crate::config::Config::skills`]: a name on
/// that list gets its own block, and every other name — including
/// `interactive` — is left out of `spoolway eval` entirely. Its spend still
/// reaches `spoolway eval --by`, which counts every skill whether or not it is
/// registered here.
fn skill_blocks(entries: &[Entry], configured: &[String], limit: usize) -> Vec<SkillBlock> {
    let mut buckets: HashMap<String, Vec<&Entry>> = HashMap::new();
    for entry in entries {
        let Some(label) = entry.skill_label() else {
            continue;
        };
        if !configured.iter().any(|name| name == label) {
            continue;
        }
        buckets.entry(label.to_string()).or_default().push(entry);
    }

    let mut blocks: Vec<SkillBlock> = buckets
        .into_iter()
        .map(|(name, lines)| {
            let total_cost: f64 = lines.iter().filter_map(|e| e.cost_usd).sum();
            SkillBlock {
                versions: skill_versions(&lines, limit),
                name,
                total_cost,
            }
        })
        .collect();

    // Biggest spender first — there is no `other` catch-all left to sort
    // last regardless of what it cost.
    blocks.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(Ordering::Equal)
    });
    blocks
}

/// Group one bucket's skill lines by the version they ran under, newest
/// first.
fn skill_versions(entries: &[&Entry], limit: usize) -> Vec<SkillVersion> {
    let mut order: Vec<String> = Vec::new();
    let mut first: HashMap<String, String> = HashMap::new();
    let mut sessions: HashMap<String, HashSet<String>> = HashMap::new();
    let mut unpriced: HashMap<String, HashSet<String>> = HashMap::new();
    let mut cost: HashMap<String, f64> = HashMap::new();

    for entry in entries {
        let name = version_of(entry).to_string();
        if !first.contains_key(&name) {
            order.push(name.clone());
            first.insert(name.clone(), entry.ts.clone());
        }
        sessions
            .entry(name.clone())
            .or_default()
            .insert(entry.session.clone());
        match entry.cost_usd {
            Some(usd) => *cost.entry(name).or_default() += usd,
            // A zero-token line spends nothing, so it must not mark its whole
            // session unpriced — the same rule `spoolway eval --by` follows.
            None if entry.tokens.is_zero() => {}
            // A session is unpriced if any of its lines was: what is missing
            // from the total is that whole session's unknown spend.
            None => {
                unpriced
                    .entry(name)
                    .or_default()
                    .insert(entry.session.clone());
            }
        }
    }

    order.reverse();
    order.truncate(limit.max(1));
    order
        .into_iter()
        .map(|name| SkillVersion {
            since: first
                .get(&name)
                .map(|ts| local_date(ts))
                .unwrap_or_default(),
            sessions: sessions.get(&name).map(HashSet::len).unwrap_or(0),
            unpriced: unpriced.get(&name).map(HashSet::len).unwrap_or(0),
            cost: cost.get(&name).copied().unwrap_or(0.0),
            name,
        })
        .collect()
}

fn print_skills(entries: &[Entry], configured: &[String], limit: usize) {
    let blocks = skill_blocks(entries, configured, limit);
    if blocks.is_empty() {
        return;
    }

    let vw = blocks
        .iter()
        .flat_map(|b| b.versions.iter())
        .map(|r| r.name.len())
        .max()
        .unwrap_or(8)
        .max("VERSION".len());

    println!("{}", paint("skills", "1"));
    for (n, block) in blocks.iter().enumerate() {
        if n > 0 {
            println!();
        }
        println!("{}", paint(&block.name, "1"));
        let head = format!(
            "{:<vw$}   {:<10}  {:>8}  {:>8}  {:>11}",
            "VERSION", "SINCE", "SESSIONS", "COST USD", "USD/SESSION"
        );
        use std::io::IsTerminal;
        println!(
            "{}",
            match std::io::stdout().is_terminal() {
                true => format!("{HEAD}{head}\x1b[0m"),
                false => head,
            }
        );

        for row in &block.versions {
            let cost = cost_of(row.cost, row.sessions, row.unpriced);
            // A per-session figure is a floor exactly when `row.unpriced` is
            // above zero — the same condition `cost` above just resolved
            // through `cost_of` — but printed plain either way; the note
            // below the block says once that some of this block's spend has
            // no price, rather than marking every affected cell.
            let per_session = row
                .per_session()
                .map_or_else(|| "—".to_string(), |usd| crate::fmt::money_plain(Some(usd)));
            println!(
                "{:<vw$}   {:<10}  {:>8}  {:>8}  {:>11}",
                row.name, row.since, row.sessions, cost, per_session,
            );
        }
    }

    if let Some(note) = unpriced_note(entries.iter()) {
        println!("\n{note}");
    }
}

/// "Cost is a floor" note, naming every model with no configured price among
/// `entries`. Shared between the pipeline footer and the skills block, so the
/// two say it the same way.
fn unpriced_note<'a>(entries: impl Iterator<Item = &'a Entry>) -> Option<String> {
    // A line that spent nothing is missing nothing from the total, whatever
    // it says about a price — an enrolment line names no model at all, and
    // naming it here would ask for a price for the empty string.
    let unpriced: std::collections::BTreeSet<&str> = entries
        .filter(|e| e.cost_usd.is_none() && !e.tokens.is_zero())
        .map(|e| e.model.as_str())
        .collect();
    (!unpriced.is_empty()).then(|| {
        format!(
            "Cost is a floor — no price configured for: {}",
            unpriced.into_iter().collect::<Vec<_>>().join(", ")
        )
    })
}

// ------------------------------------------------------------------ json

fn print_json(
    entries: &[Entry],
    skills: &[Entry],
    blocks: &[PipelineBlock],
    skills_eligible: bool,
    fallback: &HashMap<(String, String), String>,
    models: &BTreeMap<String, ModelPrice>,
) -> Result<()> {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for block in blocks {
        for version in &block.versions {
            let m = Metrics::for_row(entries, &block.name, &version.name, fallback, models);
            rows.push(json_row(&block.name, version, &m));
        }
    }

    // Skill spend is not a version row: it has no lanes, no pass share and no
    // task to divide by, and inventing those fields to fit the array would be
    // a shape that means something different for half the rows. The ledger
    // lines go out as they are written instead, `skill` label and all, so
    // nothing this view holds back is lost to a reader — and a consumer tells
    // the two apart by that field.
    if skills_eligible {
        for entry in skills {
            rows.push(serde_json::to_value(entry)?);
        }
    }

    println!("{}", serde_json::to_string_pretty(&rows)?);
    Ok(())
}

fn json_row(pipeline: &str, v: &Version, m: &Metrics) -> serde_json::Value {
    serde_json::json!({
        "project": v.project,
        "pipeline": pipeline,
        "version": v.name,
        "since": v.since,
        "tasks": m.tasks,
        "lanes": m.lanes,
        "lanes_per_task": m.lanes_per_task(),
        "pass": m.pass_share().map(|p| p / 100.0),
        "blocks": m.blocked,
        "ctx_peak_tokens": m.ctx_peak_tokens,
        "ctx_peak_pct": m.ctx_peak_pct,
        "out_tokens": m.out,
        "out_per_task": m.out_per_task(),
        "cost_usd": (m.lanes > m.unpriced).then_some(m.cost),
        "cost_per_task": (m.lanes > m.unpriced).then_some(m.cost_per_task()),
        // How many of `lanes` are missing from `cost_usd` — 0 when it is a
        // real total, and the reason `cost_usd` above is `null` rather than a
        // number when it equals `lanes`. A consumer cannot otherwise tell a
        // partly-priced floor from a complete total: both are plain numbers.
        "unpriced": m.unpriced,
        "time_s": m.time_s,
        "time_per_task_s": m.time_per_task_s(),
    })
}

// ------------------------------------------------------------------- csv

fn print_csv(
    entries: &[Entry],
    blocks: &[PipelineBlock],
    fallback: &HashMap<(String, String), String>,
    models: &BTreeMap<String, ModelPrice>,
) -> Result<()> {
    // The whole point of a flat export: blocks fold into a `pipeline` column,
    // in the same order the table would have printed them — no colour, raw
    // seconds and raw tokens, and every total the table itself stopped
    // printing once it started dividing by `RUNS`.
    println!(
        "project,pipeline,version,since,tasks,lanes,lanes_per_task,pass,blocks,ctx_peak_tokens,\
         ctx_peak_pct,out_tokens,out_per_task,cost_usd,cost_per_task,unpriced,time_s,\
         time_per_task_s"
    );
    for block in blocks {
        for version in &block.versions {
            let m = Metrics::for_row(entries, &block.name, &version.name, fallback, models);
            let ctx_tokens = m.ctx_peak_tokens.map_or(String::new(), |t| t.to_string());
            let ctx_pct = m.ctx_peak_pct.map_or(String::new(), |p| format!("{p:.2}"));
            // `out_per_task` and `time_per_task_s` are counts — tokens and
            // seconds — so they round to whole numbers, the same as the
            // `out_tokens` and `time_s` totals beside them; only the ratios
            // (`lanes_per_task`) and the dollar figures carry decimal places.
            println!(
                "{},{},{},{},{},{},{:.2},{},{},{},{},{},{},{},{},{},{},{}",
                version.project,
                block.name,
                version.name,
                version.since,
                m.tasks,
                m.lanes,
                m.lanes_per_task(),
                csv_fraction(m.pass_share()),
                m.blocked,
                ctx_tokens,
                ctx_pct,
                m.out,
                m.out_per_task().round() as u64,
                csv_cost(m.cost, m.lanes, m.unpriced),
                csv_cost(m.cost_per_task(), m.lanes, m.unpriced),
                m.unpriced,
                m.time_s.max(0),
                m.time_per_task_s().round() as i64,
            );
        }
    }
    Ok(())
}

/// What the table could not say inside a row: a run counted on more than one
/// row, a model nothing could price, or a version that predates the ledger
/// recording one at all.
fn footer(
    entries: &[Entry],
    blocks: &[PipelineBlock],
    fallback: &HashMap<(String, String), String>,
) {
    let shown: HashSet<(String, String)> = blocks
        .iter()
        .flat_map(|b| b.versions.iter().map(|v| (b.name.clone(), v.name.clone())))
        .collect();

    let straddled = group_by_run(entries, fallback)
        .iter()
        .filter(|(_, lanes)| {
            let touched: HashSet<(String, String)> = lanes
                .iter()
                .map(|e| (e.pipeline.clone(), version_of(e).to_string()))
                .filter(|key| shown.contains(key))
                .collect();
            touched.len() > 1
        })
        .count();
    if straddled > 0 {
        println!("\n{straddled} run(s) spanned a version change and are counted in both rows.");
    }

    if let Some(note) = unpriced_note(
        entries
            .iter()
            .filter(|e| shown.contains(&(e.pipeline.clone(), version_of(e).to_string()))),
    ) {
        println!("\n{note}");
    }

    if shown.iter().any(|(_, version)| version == UNVERSIONED) {
        println!(
            "\n`{UNVERSIONED}` is lanes recorded before versions were, which cannot be compared \
             to anything."
        );
    }
}

// ----------------------------------------------------------------- the screen
//
// Bare `spoolway eval`, no flags and no `--json`: this rather than the
// printing path above, which every flag still takes exactly as it does
// today — see `EvalArgs::is_bare`. Modelled on `commands::queue_screen`: raw
// mode through `platform::TermGuard`, one byte at a time off stdin through
// `crate::screen::read_key`, so a real tty in raw mode and a pipe an
// end-to-end suite is scripting drive it identically, and the screen ends
// the moment either runs out.
//
// One pane, not two: there is one thing to read here, a ledger, not a queue
// to pick work out of — and nothing here offers to start a dispatcher, the
// way the queue screen's own `Mode::Dispatch` does, because this reads a
// ledger rather than queuing work.
//
// Every view is a second look at rows this file already knows how to build:
// the pipelines view is `pipeline_blocks` and `Metrics::for_row`, the runs
// view is `list_runs`, the skills view is `skill_blocks` — the screen adds a
// `Metrics::for_step` for the steps view and nothing else reads the ledger a
// second way. Rendered without colour throughout, unlike the printing path:
// `pad_to` counts every byte of a string as one column, and an ANSI escape
// slipped into a row would throw the frame's own border out of line with it.

/// Which of the four views is on screen. `Tab` cycles through them in this
/// order, which is also the order their rows read most naturally: the whole
/// pipeline, then one step of it, then one run, then what ran outside any
/// pipeline at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Pipelines,
    Steps,
    Runs,
    Skills,
}

impl View {
    fn next(self) -> View {
        match self {
            View::Pipelines => View::Steps,
            View::Steps => View::Runs,
            View::Runs => View::Skills,
            View::Skills => View::Pipelines,
        }
    }

    fn label(self) -> &'static str {
        match self {
            View::Pipelines => "pipelines",
            View::Steps => "steps",
            View::Runs => "runs",
            View::Skills => "skills",
        }
    }
}

/// Which project's ledger the screen reads. Set from `--project`/`--all` on
/// the command line before the screen ever opens — not a row of the filter
/// panel, and deliberately the only project picker the screen has at all:
/// see the task's own non-goal against adding a second one.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// This project alone — the default, and what `--project`/`--all` being
    /// absent means on the command line too.
    Mine,
    /// One other project this machine has registered, by name — `--project`.
    Named(String),
    /// Every registered project at once — `--all`.
    All,
}

impl Scope {
    /// What the top border's right side names this scope as — the project's
    /// own name, so a person reading the frame sees exactly which ledger it
    /// came from.
    fn label(&self, repo: &Repo) -> String {
        match self {
            Scope::Mine => crate::usage::registry::name_of(&repo.root),
            Scope::Named(name) => name.clone(),
            Scope::All => "all projects".to_string(),
        }
    }
}

/// The filters currently applied to every view — what `load` reads the
/// ledger through, and what the top border's right side names. `since` and
/// `until` hold a plain `YYYY-MM-DD` the calendar wrote in, or a duration
/// (`7d`, `24h`) passed on the command line — not yet parsed either way:
/// parsing happens once, in `load`, so a bad value is reported in one place.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Filters {
    pipeline: Option<String>,
    step: Option<String>,
    since: String,
    until: String,
    scope: Scope,
    limit: usize,
}

impl Filters {
    fn from_args(args: &EvalArgs) -> Filters {
        let scope = match (&args.project, args.all) {
            (_, true) => Scope::All,
            (Some(name), false) => Scope::Named(name.clone()),
            (None, false) => Scope::Mine,
        };
        Filters {
            pipeline: args.pipeline.clone(),
            step: args.step.clone(),
            since: args.since.clone().unwrap_or_default(),
            until: args.until.clone().unwrap_or_default(),
            scope,
            limit: args.limit(),
        }
    }
}

/// `s` trimmed down to `None` when it is blank — what an empty typed field
/// means to every reader below: no bound at all, not a bound of the empty
/// string.
fn non_empty(s: &str) -> Option<&str> {
    let s = s.trim();
    (!s.is_empty()).then_some(s)
}

/// The ledger, read once under the current filters' scope and window —
/// everything a view is built from, before that view's own pipeline/step
/// filter narrows it further. Reloaded on `r`, and every time the filter
/// panel's `enter` commits a change to `scope`, `since` or `until`.
struct Loaded {
    /// Every non-skill lane in scope and window, unfiltered by pipeline or
    /// step — filter-value cycling reads its pipeline and step candidate
    /// lists off of this, so the values on offer are always the project's
    /// own rather than whatever the current view happens to already be
    /// narrowed to.
    entries: Vec<Entry>,
    skills: Vec<Entry>,
    fallback: HashMap<(String, String), String>,
    models: BTreeMap<String, ModelPrice>,
    scope_label: String,
    /// `Config::skills` — which names `skill_blocks` gives their own block.
    /// The screen's skills view reads the same list `spoolway eval` prints
    /// against, rather than inventing a wider one of its own: registering a
    /// skill is what makes it worth comparing across versions, on the screen
    /// exactly as in the table.
    configured_skills: Vec<String>,
}

fn load(repo: &Repo, filters: &Filters) -> Result<Loaded> {
    crate::usage::sweep(repo);

    let (project, all) = match &filters.scope {
        Scope::Mine => (None, false),
        Scope::Named(name) => (Some(name.as_str()), false),
        Scope::All => (None, true),
    };
    let (mut entries, _scope) = crate::spend::collect_scoped(repo, project, all)?;

    let mut skills: Vec<Entry> = entries.iter().filter(|e| e.is_skill()).cloned().collect();
    entries.retain(|entry| !entry.is_skill());

    let fallback = fallback_keys(&entries);

    let window =
        crate::spend::window_of(None, non_empty(&filters.since), non_empty(&filters.until))?;
    entries.retain(|entry| window.contains(&entry.ts));
    skills.retain(|entry| window.contains(&entry.ts));
    entries.sort_by(|a, b| a.ts.cmp(&b.ts));

    Ok(Loaded {
        entries,
        skills,
        fallback,
        models: repo.config.models.clone(),
        scope_label: filters.scope.label(repo),
        configured_skills: repo.config.skills.clone(),
    })
}

/// `entries`, narrowed by the pipeline and step filters — the input every
/// view except skills is built from. Skills reads `loaded.skills` directly
/// instead: a skill session belongs to no pipeline and no step, the same
/// reasoning `run` applies to `skills_eligible`.
fn scoped_entries<'a>(loaded: &'a Loaded, filters: &Filters) -> Vec<&'a Entry> {
    loaded
        .entries
        .iter()
        .filter(|e| filters.pipeline.as_deref().is_none_or(|p| e.pipeline == p))
        .filter(|e| filters.step.as_deref().is_none_or(|s| e.step == s))
        .collect()
}

/// Distinct pipeline names `loaded.entries` actually has — the candidate
/// list the filter panel's `pipeline` row cycles over.
fn pipeline_candidates(loaded: &Loaded) -> Vec<String> {
    loaded
        .entries
        .iter()
        .map(|e| e.pipeline.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Distinct step names among `loaded.entries`, narrowed to `pipeline` when
/// one is given — the candidate list the filter panel's `step` row cycles
/// over, which shrinks to just that pipeline's own steps once one is chosen.
fn step_candidates(loaded: &Loaded, pipeline: Option<&str>) -> Vec<String> {
    loaded
        .entries
        .iter()
        .filter(|e| pipeline.is_none_or(|p| e.pipeline == p))
        .map(|e| e.step.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Move an "all" filter — `pipeline` or `step` — to the next or previous
/// entry of the conceptual list `[None, candidates[0], candidates[1], ...]`,
/// clamped at both ends rather than wrapping: `←` on the first real value
/// lands back on "all", and `←` on "all" itself, or `→` past the last real
/// value, does nothing.
fn cycle_option(candidates: &[String], current: &Option<String>, forward: bool) -> Option<String> {
    let at = match current {
        None => 0,
        Some(v) => candidates.iter().position(|c| c == v).map_or(0, |i| i + 1),
    };
    let next = match forward {
        true => (at + 1).min(candidates.len()),
        false => at.saturating_sub(1),
    };
    match next {
        0 => None,
        i => candidates.get(i - 1).cloned(),
    }
}

/// One selectable row, wherever it sits in whichever view is on screen —
/// what the cursor moves over.
struct Row {
    /// The row's own text, without the two-column marker `render_lines`
    /// prefixes onto it — kept apart so the marker can be decided once,
    /// against the cursor, at the point every row is actually drawn.
    text: String,
}

/// One line of a view's body: a row the cursor can land on, or plain text —
/// a block name, a table header, a blank line between blocks — that it
/// skips over.
enum Line {
    Text(String),
    Row(Row),
}

fn row_count(lines: &[Line]) -> usize {
    lines.iter().filter(|l| matches!(l, Line::Row(_))).count()
}

/// Lay `lines` out as plain text `width` columns wide, marking whichever
/// [`Line::Row`] the cursor has reached — clamped to the last row when there
/// are fewer than `cursor` of them, the same way every other cursor in this
/// screen clamps rather than refuses to move.
fn render_lines(lines: &[Line], cursor: usize, width: usize) -> Vec<String> {
    let total = row_count(lines);
    let cursor = match total {
        0 => 0,
        n => cursor.min(n - 1),
    };

    let mut out = Vec::with_capacity(lines.len());
    for (line, row_n) in lines.iter().zip(row_numbers(lines)) {
        match line {
            // One column of filler where a row would carry its marker — so a
            // block name or a table header lines up under the very columns
            // its rows are printed in, rather than sitting a column to their
            // left. The mockup's own frame carries the marker flush against
            // the border, with nothing between it and the row's first
            // character — `>b210d1a8`, not `> b210d1a8`.
            Line::Text(text) => out.push(pad_to(&format!(" {text}"), width)),
            Line::Row(row) => {
                let idx = row_n.expect("a Line::Row always has a row number");
                let marker = if idx == cursor { ">" } else { " " };
                out.push(pad_to(&format!("{marker}{}", row.text), width));
            }
        }
    }
    out
}

/// Which row number each of `lines` is, `None` for a [`Line::Text`] — a
/// `Line::Row`'s position among only the other rows, skipping the text lines
/// in between. What lets [`render_lines`] and [`cursor_line_index`] compare
/// a line against the cursor without a hand-rolled counter of their own.
fn row_numbers(lines: &[Line]) -> Vec<Option<usize>> {
    let mut next = 0usize;
    lines
        .iter()
        .map(|line| match line {
            Line::Text(_) => None,
            Line::Row(_) => {
                let n = next;
                next += 1;
                Some(n)
            }
        })
        .collect()
}

// ---------------------------------------------------------------- pipelines

fn pipelines_lines(entries: &[&Entry], loaded: &Loaded, filters: &Filters) -> Vec<Line> {
    if entries.is_empty() {
        return vec![Line::Text("Nothing to compare in this window.".to_string())];
    }
    let owned: Vec<Entry> = entries.iter().map(|e| (*e).clone()).collect();
    let blocks = pipeline_blocks(&owned, filters.limit);
    let show_project = spans_more_than_one_project(&blocks);

    // No `pipelines` heading here: the frame's own top border already reads
    // `─ eval · pipelines ─`, and a block key that reads `Pipeline: <name>`
    // says which view this is on its own.
    let mut out = Vec::new();
    for (n, block) in blocks.iter().enumerate() {
        if n > 0 {
            out.push(Line::Text(String::new()));
        }
        out.push(Line::Text(format!("Pipeline: {}", block.name)));

        let metrics: Vec<Metrics> = block
            .versions
            .iter()
            .map(|v| {
                Metrics::for_row(
                    &owned,
                    &block.name,
                    &v.name,
                    &loaded.fallback,
                    &loaded.models,
                )
            })
            .collect();
        let pw = project_width(&block.versions);
        let vw = name_width(&block.versions);
        out.push(Line::Text(header_plain(show_project, pw, vw)));

        for (i, version) in block.versions.iter().enumerate() {
            let text = full_row(show_project, version, &metrics[i], pw, vw);
            out.push(Line::Row(Row { text }));
        }
    }
    out
}

// -------------------------------------------------------------------- steps

/// One step's row in the steps view: its own metrics.
struct StepRow {
    step: String,
    metrics: Metrics,
}

/// One pipeline's block in the steps view: one row per step it ran anywhere
/// in the window, aggregated over every version — not one version compared
/// against another.
struct StepBlock {
    pipeline: String,
    rows: Vec<StepRow>,
}

/// Every step `pipeline` ran anywhere in `entries`, in the order its own
/// pipeline definition declares them — so the table reads in the order the
/// work happened, not alphabetically. A step the definition no longer names
/// (renamed, or removed, since some of these lines were banked) is appended
/// after, sorted, rather than dropped: the ledger still remembers it ran.
fn ordered_steps(entries: &[Entry], pipeline: &str, pipelines: &Pipelines) -> Vec<String> {
    let present: BTreeSet<&str> = entries
        .iter()
        .filter(|e| e.pipeline == pipeline)
        .map(|e| e.step.as_str())
        .collect();

    let mut ordered: Vec<String> = Vec::new();
    if let Ok(def) = pipelines.get(pipeline) {
        for step in &def.steps {
            if present.contains(step.id.as_str()) {
                ordered.push(step.id.clone());
            }
        }
    }
    let known: HashSet<&str> = ordered.iter().map(String::as_str).collect();
    let mut rest: Vec<&str> = present.into_iter().filter(|s| !known.contains(s)).collect();
    rest.sort();
    ordered.extend(rest.into_iter().map(String::from));
    ordered
}

fn step_blocks(
    entries: &[Entry],
    fallback: &HashMap<(String, String), String>,
    models: &BTreeMap<String, ModelPrice>,
    pipelines: &Pipelines,
) -> Vec<StepBlock> {
    // One version is enough to read off the pipeline names and their
    // newest-first order; the steps view no longer reads a block's own
    // versions, so there is nothing to widen the limit for.
    pipeline_blocks(entries, 1)
        .into_iter()
        .map(|block| {
            let steps = ordered_steps(entries, &block.name, pipelines);
            let rows = steps
                .into_iter()
                .map(|step| {
                    let metrics = Metrics::for_step(entries, &block.name, &step, fallback, models);
                    StepRow { step, metrics }
                })
                .collect();

            StepBlock {
                pipeline: block.name,
                rows,
            }
        })
        .collect()
}

fn step_header_plain(sw: usize) -> String {
    format!(
        "{:<sw$}  {:>5}  {:>6}  {:>4}  {:>6}  {:>8}  {:>7}  {:>9}",
        "STEP", "RUNS", "L/RUN", "PASS", "BLOCKS", "CTX PEAK", "USD/RUN", "TIME/RUN"
    )
}

fn step_row_line(row: &StepRow, sw: usize) -> String {
    format!(
        "{:<sw$}  {:>5}  {:>6}  {:>4}  {:>6}  {:>8}  {:>7}  {:>9}",
        row.step,
        row.metrics.tasks,
        row.metrics.lanes_per_task_str(),
        percent(row.metrics.pass_share()),
        row.metrics.blocked,
        row.metrics.ctx_str(),
        row.metrics.cost_per_task_str(),
        row.metrics.time_per_task_str(),
    )
}

fn steps_lines(entries: &[&Entry], loaded: &Loaded, pipelines: &Pipelines) -> Vec<Line> {
    if entries.is_empty() {
        return vec![Line::Text("Nothing to compare in this window.".to_string())];
    }
    let owned: Vec<Entry> = entries.iter().map(|e| (*e).clone()).collect();
    let blocks = step_blocks(&owned, &loaded.fallback, &loaded.models, pipelines);

    // Measured across every block on screen, the same way `skills_lines`
    // measures `VERSION` — so `reproduce-again` in one pipeline's block
    // still lines up with a shorter name in another's.
    let sw = blocks
        .iter()
        .flat_map(|b| b.rows.iter())
        .map(|r| r.step.len())
        .max()
        .unwrap_or(4)
        .max("STEP".len());

    let mut out = Vec::new();
    for (n, block) in blocks.iter().enumerate() {
        if n > 0 {
            out.push(Line::Text(String::new()));
        }
        out.push(Line::Text(format!("Pipeline: {}", block.pipeline)));
        out.push(Line::Text(step_header_plain(sw)));

        for row in &block.rows {
            out.push(Line::Row(Row {
                text: step_row_line(row, sw),
            }));
        }
    }
    out
}

// --------------------------------------------------------------------- runs

fn runs_header_plain(tw: usize, vw: usize, pw: usize) -> String {
    format!(
        "{:<tw$}  {:<10}  {:<vw$}  {:<pw$}  {:>5}  {:>4}  {:>6}  {:>8}  {:>7}  {:>8}  {:>8}",
        "TASK",
        "WHEN",
        "VERSION",
        "PIPELINE",
        "LANES",
        "PASS",
        "BLOCKS",
        "CTX PEAK",
        "OUT",
        "COST USD",
        "TIME"
    )
}

fn runs_row_line(row: &RunRow, tw: usize, vw: usize, pw: usize) -> String {
    format!(
        "{:<tw$}  {:<10}  {:<vw$}  {:<pw$}  {:>5}  {:>4}  {:>6}  {:>8}  {:>7}  {:>8}  {:>8}",
        row.task,
        local_date(&row.ts),
        row.version,
        row.pipeline,
        row.lanes,
        percent(row.pass_share()),
        row.blocked,
        ctx_cell(row.ctx_peak_tokens, row.ctx_peak_pct),
        crate::fmt::tokens_human(row.out),
        cost_of(row.cost, row.lanes, row.unpriced),
        crate::status::human_secs(row.time_s.max(0)),
    )
}

fn runs_lines(entries: &[&Entry], loaded: &Loaded) -> Vec<Line> {
    if entries.is_empty() {
        return vec![Line::Text("No runs in that window.".to_string())];
    }
    let owned: Vec<Entry> = entries.iter().map(|e| (*e).clone()).collect();
    // Newest first: a person opening the screen wants to know what just ran,
    // not what ran first — the opposite of `--runs`' own oldest-first table,
    // which reads top to bottom like a log.
    let mut rows = list_runs(&owned, &loaded.fallback, &loaded.models);
    rows.reverse();

    let tw = rows.iter().map(|r| r.task.len()).max().unwrap_or(4).max(4);
    let vw = rows
        .iter()
        .map(|r| r.version.len())
        .max()
        .unwrap_or(7)
        .max(7);
    let pw = rows
        .iter()
        .map(|r| r.pipeline.len())
        .max()
        .unwrap_or(8)
        .max(8);

    let mut out = vec![Line::Text(runs_header_plain(tw, vw, pw))];
    for row in &rows {
        out.push(Line::Row(Row {
            text: runs_row_line(row, tw, vw, pw),
        }));
    }
    out
}

// ------------------------------------------------------------------ skills

fn skills_header_plain(vw: usize) -> String {
    format!(
        "{:<vw$}   {:<10}  {:>8}  {:>8}  {:>11}",
        "VERSION", "SINCE", "SESSIONS", "COST USD", "USD/SESSION"
    )
}

fn skills_row_line(row: &SkillVersion, vw: usize) -> String {
    let cost = cost_of(row.cost, row.sessions, row.unpriced);
    let per_session = row
        .per_session()
        .map_or_else(|| "—".to_string(), |usd| crate::fmt::money_plain(Some(usd)));
    format!(
        "{:<vw$}   {:<10}  {:>8}  {:>8}  {:>11}",
        row.name, row.since, row.sessions, cost, per_session,
    )
}

/// The skills view of the screen — the same `Config::skills` list and the
/// same [`skill_blocks`] bucketing `spoolway eval`'s own skills block prints,
/// so a name is worth its own row here exactly when it is worth one there.
fn skills_lines(loaded: &Loaded, filters: &Filters) -> Vec<Line> {
    let blocks = skill_blocks(&loaded.skills, &loaded.configured_skills, filters.limit);
    if blocks.is_empty() {
        return vec![Line::Text("No skill sessions in that window.".to_string())];
    }
    let vw = blocks
        .iter()
        .flat_map(|b| b.versions.iter())
        .map(|r| r.name.len())
        .max()
        .unwrap_or(8)
        .max("VERSION".len());

    let mut out = Vec::new();
    for (n, block) in blocks.iter().enumerate() {
        if n > 0 {
            out.push(Line::Text(String::new()));
        }
        out.push(Line::Text(block.name.clone()));
        out.push(Line::Text(skills_header_plain(vw)));

        for row in &block.versions {
            out.push(Line::Row(Row {
                text: skills_row_line(row, vw),
            }));
        }
    }
    out
}

// ---------------------------------------------------------------- one view

fn view_lines(loaded: &Loaded, filters: &Filters, pipelines: &Pipelines, view: View) -> Vec<Line> {
    match view {
        View::Pipelines => pipelines_lines(&scoped_entries(loaded, filters), loaded, filters),
        View::Steps => steps_lines(&scoped_entries(loaded, filters), loaded, pipelines),
        View::Runs => runs_lines(&scoped_entries(loaded, filters), loaded),
        View::Skills => skills_lines(loaded, filters),
    }
}

// ------------------------------------------------------------------- export

/// One row of whichever view is on screen, flattened for `.csv` — the same
/// figures `Line::Row` renders, without a marker column.
fn export_rows(
    loaded: &Loaded,
    filters: &Filters,
    pipelines: &Pipelines,
    view: View,
) -> (String, Vec<String>) {
    match view {
        View::Pipelines => {
            let entries = scoped_entries(loaded, filters);
            let owned: Vec<Entry> = entries.iter().map(|e| (*e).clone()).collect();
            let blocks = pipeline_blocks(&owned, filters.limit);
            let header = "project,pipeline,version,since,tasks,lanes,lanes_per_task,pass,blocks,\
                           ctx_peak_tokens,ctx_peak_pct,out_tokens,out_per_task,cost_usd,\
                           cost_per_task,unpriced,time_s,time_per_task_s"
                .to_string();
            let mut rows = Vec::new();
            for block in &blocks {
                for version in &block.versions {
                    let m = Metrics::for_row(
                        &owned,
                        &block.name,
                        &version.name,
                        &loaded.fallback,
                        &loaded.models,
                    );
                    rows.push(csv_pipeline_row(&version.project, &block.name, version, &m));
                }
            }
            (header, rows)
        }
        View::Steps => {
            let entries = scoped_entries(loaded, filters);
            let owned: Vec<Entry> = entries.iter().map(|e| (*e).clone()).collect();
            let blocks = step_blocks(&owned, &loaded.fallback, &loaded.models, pipelines);
            // No `version`/`since`: a row now mixes every version in the
            // window rather than reading one, so there is no single version
            // left to name.
            let header = "pipeline,step,tasks,lanes,lanes_per_task,pass,blocks,\
                           ctx_peak_tokens,ctx_peak_pct,out_tokens,out_per_task,cost_usd,\
                           cost_per_task,unpriced,time_s,time_per_task_s"
                .to_string();
            let mut rows = Vec::new();
            for block in &blocks {
                for row in &block.rows {
                    rows.push(csv_step_row(&block.pipeline, row));
                }
            }
            (header, rows)
        }
        View::Runs => {
            let entries = scoped_entries(loaded, filters);
            let owned: Vec<Entry> = entries.iter().map(|e| (*e).clone()).collect();
            // Newest first, matching `runs_lines`' own reversal — `e` writes
            // the rows in the order they are on screen, not `list_runs`'
            // natural oldest-first order.
            let mut rows = list_runs(&owned, &loaded.fallback, &loaded.models);
            rows.reverse();
            // Unlike `--runs --csv` (`print_run_csv`), this export carries
            // `ctx_peak_tokens`/`ctx_peak_pct` — the same reading the runs
            // view's own `CTX PEAK` column shows on screen, and `docs/eval.md`
            // promises every export here matches its view's own columns.
            let header = "run,task,when,version,pipeline,lanes,pass,blocks,ctx_peak_tokens,\
                           ctx_peak_pct,out_tokens,cost_usd,unpriced,time_s"
                .to_string();
            let lines = rows.iter().map(csv_run_row).collect();
            (header, lines)
        }
        View::Skills => {
            let blocks = skill_blocks(&loaded.skills, &loaded.configured_skills, filters.limit);
            let header =
                "skill,version,since,sessions,cost_usd,cost_per_session,unpriced".to_string();
            let mut rows = Vec::new();
            for block in &blocks {
                for version in &block.versions {
                    rows.push(csv_skill_row(&block.name, version));
                }
            }
            (header, rows)
        }
    }
}

fn csv_pipeline_row(project: &str, pipeline: &str, version: &Version, m: &Metrics) -> String {
    let ctx_tokens = m.ctx_peak_tokens.map_or(String::new(), |t| t.to_string());
    let ctx_pct = m.ctx_peak_pct.map_or(String::new(), |p| format!("{p:.2}"));
    format!(
        "{project},{pipeline},{},{},{},{},{:.2},{},{},{ctx_tokens},{ctx_pct},{},{},{},{},{},{},{}",
        version.name,
        version.since,
        m.tasks,
        m.lanes,
        m.lanes_per_task(),
        csv_fraction(m.pass_share()),
        m.blocked,
        m.out,
        m.out_per_task().round() as u64,
        csv_cost(m.cost, m.lanes, m.unpriced),
        csv_cost(m.cost_per_task(), m.lanes, m.unpriced),
        m.unpriced,
        m.time_s.max(0),
        m.time_per_task_s().round() as i64,
    )
}

fn csv_step_row(pipeline: &str, row: &StepRow) -> String {
    let m = &row.metrics;
    let ctx_tokens = m.ctx_peak_tokens.map_or(String::new(), |t| t.to_string());
    let ctx_pct = m.ctx_peak_pct.map_or(String::new(), |p| format!("{p:.2}"));
    format!(
        "{pipeline},{},{},{},{},{:.2},{},{},{ctx_tokens},{ctx_pct},{},{},{},{},{},{}",
        row.step,
        m.tasks,
        m.lanes,
        m.lanes_per_task(),
        csv_fraction(m.pass_share()),
        m.blocked,
        m.out,
        m.out_per_task().round() as u64,
        csv_cost(m.cost, m.lanes, m.unpriced),
        csv_cost(m.cost_per_task(), m.lanes, m.unpriced),
        m.unpriced,
        m.time_s.max(0),
        m.time_per_task_s().round() as i64,
    )
}

fn csv_run_row(row: &RunRow) -> String {
    let ctx_tokens = row.ctx_peak_tokens.map_or(String::new(), |t| t.to_string());
    let ctx_pct = row
        .ctx_peak_pct
        .map_or(String::new(), |p| format!("{p:.2}"));
    format!(
        "{},{},{},{},{},{},{},{},{ctx_tokens},{ctx_pct},{},{},{},{}",
        row.id,
        row.task,
        local_date(&row.ts),
        row.version,
        row.pipeline,
        row.lanes,
        csv_fraction(row.pass_share()),
        row.blocked,
        row.out,
        csv_cost(row.cost, row.lanes, row.unpriced),
        row.unpriced,
        row.time_s.max(0),
    )
}

fn csv_skill_row(name: &str, row: &SkillVersion) -> String {
    let per_session = row
        .per_session()
        .map_or(String::new(), |usd| format!("{usd:.2}"));
    format!(
        "{name},{},{},{},{},{per_session},{}",
        row.name,
        row.since,
        row.sessions,
        csv_cost(row.cost, row.sessions, row.unpriced),
        row.unpriced,
    )
}

/// `.spoolway/evals/`, so the file `e` writes lives beside the ledger it was
/// exported from. spoolway writes no `.gitignore` rules of its own any more —
/// see [`crate::gitignore`] — so keeping an export out of git is the
/// project's own line to add, like every other directory a person's own run
/// fills in rather than the project's tracked setup.
fn evals_dir(repo: &Repo) -> std::path::PathBuf {
    repo.root.join(crate::config::STATE_DIR).join("evals")
}

/// Write the rows currently on screen to `.spoolway/evals/eval-<stamp>.csv`,
/// and hand back the path and how many rows it holds — what the confirmation
/// panel names.
fn export(
    repo: &Repo,
    loaded: &Loaded,
    filters: &Filters,
    pipelines: &Pipelines,
    view: View,
) -> Result<(std::path::PathBuf, usize)> {
    let (header, rows) = export_rows(loaded, filters, pipelines, view);
    let dir = evals_dir(repo);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M");
    let path = dir.join(format!("eval-{stamp}.csv"));
    let mut body = String::new();
    body.push_str(&header);
    body.push('\n');
    for row in &rows {
        body.push_str(row);
        body.push('\n');
    }
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok((path, rows.len()))
}

/// `path`, relative to `root` where it is under it — what the export
/// confirmation names, the same way the mockup writes
/// `.spoolway/evals/eval-2026-08-18-2031.csv` rather than the whole absolute
/// path a real project's `repo.root` would make it. Falls back to the path
/// as given when it is not under `root` at all, which should not happen in
/// practice since `export` always writes under `evals_dir(repo)`.
fn display_relative(path: &std::path::Path, root: &std::path::Path) -> String {
    path.strip_prefix(root)
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

// -------------------------------------------------------------------- frame

/// The frame's content width — the terminal's own width where one can be
/// measured, clamped to a floor so a very narrow terminal still draws a
/// readable frame rather than a column of borders. Falls back to a width
/// that fits every mockup row when there is no terminal to measure at all,
/// same as the queue screen's own fallback for a run with no tty.
const FALLBACK_WIDTH: usize = 96;
const MIN_WIDTH: usize = 60;
/// Border a row spends on top of its content: the two `"│"` characters. The
/// marker column and the padding after it are part of the content itself —
/// see `render_lines` — so there is no separate space to account for here.
const FRAME_CHROME: usize = 2;

fn frame_width() -> usize {
    match terminal_size::terminal_size() {
        Some((w, _)) => (w.0 as usize).saturating_sub(FRAME_CHROME).max(MIN_WIDTH),
        None => FALLBACK_WIDTH,
    }
}

/// What `frame_rows` subtracts from the terminal's own height: two borders,
/// the footer, one spare line — so the last line's own newline does not
/// scroll the top of the frame away, the same reasoning `commands::queue`'s
/// own `PANE_CHROME_ROWS` gives — and, when `note_row` is set, the
/// unpriced-cost note's own row. Pulled out of `frame_rows` so the count
/// itself is a pure function a test can pin without a real terminal behind
/// it.
fn frame_chrome(note_row: bool) -> usize {
    4 + usize::from(note_row)
}

/// How many body rows the frame gets once `frame_chrome` is counted. `None`
/// where there is no terminal to measure, which is what lets a piped run
/// keep every row rather than losing the ones past some guessed height.
fn frame_rows(note_row: bool) -> Option<usize> {
    terminal_size::terminal_size()
        .map(|(_, h)| (h.0 as usize).saturating_sub(frame_chrome(note_row)).max(1))
}

fn frame_top(view: View, right: &str, width: usize) -> String {
    let left = format!("─ eval · {} ", view.label());
    let right = format!(" {right} ─");
    let dashes = width.saturating_sub(left.chars().count() + right.chars().count());
    format!("┌{left}{}{right}┐", "─".repeat(dashes.max(1)))
}

fn frame_bottom(width: usize) -> String {
    format!("└{}┘", "─".repeat(width))
}

/// `panel`'s own overhead beyond a body line's text: the box border on
/// either side plus the two-space indent `panel` prefixes every line with —
/// see `screen::panel`. What `wrap_notice` subtracts from the frame's own
/// width so the panel it builds never exceeds it.
const NOTICE_CHROME: usize = 4;

/// A [`Mode::Notice`] body, word-wrapped to fit inside a frame `width`
/// columns wide — an error message from `load` (a bad `--since` in
/// particular) is one long unwrapped sentence, and without this, `panel`
/// sizes its box to that whole sentence and draws wider than the frame has
/// room for. Each of `body`'s own lines wraps on its own, so the export
/// confirmation's two short lines are untouched.
fn wrap_notice(body: &str, width: usize) -> Vec<String> {
    let wrap_width = width.saturating_sub(NOTICE_CHROME).max(1);
    let mut out = Vec::new();
    for line in body.lines() {
        if line.chars().count() <= wrap_width {
            out.push(line.to_string());
            continue;
        }
        let mut current = String::new();
        for word in line.split_whitespace() {
            let extra = if current.is_empty() { 0 } else { 1 };
            if !current.is_empty()
                && current.chars().count() + extra + word.chars().count() > wrap_width
            {
                out.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        out.push(current);
    }
    out
}

/// `lines`, cut to `rows` with the cursor's row kept in view — the same idea
/// as the queue screen's own `window`, simplified for a single cursor index
/// rather than a range: this screen has one pane, not two. `width` pads the
/// scroll indicator line the same way every other row is padded, so it does
/// not throw the frame's right border out of line.
fn clip(lines: &[String], cursor_line: usize, rows: Option<usize>, width: usize) -> Vec<String> {
    let Some(rows) = rows else {
        return lines.to_vec();
    };
    if lines.len() <= rows {
        return lines.to_vec();
    }
    let view = rows - 1;
    let offset = (cursor_line + 1)
        .saturating_sub(view)
        .min(cursor_line)
        .min(lines.len() - view);
    let mut shown = lines[offset..offset + view].to_vec();
    let hidden = lines.len() - (offset + view);
    let indicator = match hidden {
        0 => format!("↑ {offset} above"),
        _ => format!("↓ {hidden} below"),
    };
    // The one-column marker filler every other plain-text row carries — see
    // `render_lines`'s own `Line::Text` arm.
    shown.push(pad_to(&format!(" {indicator}"), width));
    shown
}

/// The line index (into the padded body, not into `Line`s) that holds the
/// cursor's row — what `clip` keeps in view. Recomputed from the same
/// `render_lines` call `draw` already made, so the two never disagree about
/// which row is highlighted.
fn cursor_line_index(lines: &[Line], cursor: usize) -> usize {
    let total = row_count(lines);
    let cursor = match total {
        0 => 0,
        n => cursor.min(n - 1),
    };
    row_numbers(lines)
        .iter()
        .position(|n| *n == Some(cursor))
        .unwrap_or(0)
}

/// The right side of the top border: the scope, any pipeline/step filter,
/// and the window — every filter a person can actually move from the panel,
/// in one place, the same job the queue screen's own frame gives no line to
/// because it has no filters at all. `limit` names no row of its own any
/// more either, for the same reason: nothing on the screen can move it, so
/// naming it on every frame would say nothing a reader could act on.
fn filters_label(loaded: &Loaded, filters: &Filters) -> String {
    let mut parts = vec![loaded.scope_label.clone()];
    if let Some(p) = &filters.pipeline {
        parts.push(format!("pipeline {p}"));
    }
    if let Some(s) = &filters.step {
        parts.push(format!("step {s}"));
    }
    if let (None, None) = (non_empty(&filters.since), non_empty(&filters.until)) {
        // No bound either side: naming a window would say nothing a person
        // does not already know from the absence of a `since`/`until` row.
    } else {
        let since = non_empty(&filters.since).unwrap_or("the start");
        let until = non_empty(&filters.until).unwrap_or("now");
        parts.push(format!("{since} → {until}"));
    }
    parts.join(" · ")
}

/// `n` with its noun, singular where that is what `n` is. The same small
/// helper `commands::queue`'s screen keeps for its own report line — not
/// shared, because sharing one function two modules deep for four words
/// would cost more to find than it saves to write.
fn plural(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ => format!("{n} {noun}s"),
    }
}

/// The keys, under the frame — the one line that says what the screen does.
const FOOTER: &str = "  ↑↓ move   tab view   f filters   e export   r refresh   q quit";

fn draw(
    pipelines: &Pipelines,
    loaded: &Loaded,
    state: &ScreenState,
    out: &mut impl std::io::Write,
) {
    let _ = write!(out, "\x1b[2J\x1b[H");

    // Computed before `frame_rows`, which has to know whether that note is
    // about to take a row of its own — see `frame_rows`'s own doc comment.
    let note = screen_unpriced_note(loaded, &state.filters, state.view);

    let width = frame_width();
    let rows = frame_rows(note.is_some());
    let lines = view_lines(loaded, &state.filters, pipelines, state.view);
    let body = render_lines(&lines, state.cursor, width);
    let cursor_line = cursor_line_index(&lines, state.cursor);
    let mut body = clip(&body, cursor_line, rows, width);

    // A notice or the filter panel sits over the table as an `overlay` —
    // computed here, before the frame, because its own height is now part
    // of deciding the frame's: `clip` above cuts a body *taller* than the
    // terminal down to size, but a body shorter than whatever panel is
    // about to be drawn on it needs padding the other way, or `overlay`
    // writes past the frame's own last row and the panel loses its bottom
    // border. Reproduced on a one-version ledger, where the table itself is
    // only a few lines tall and the filter panel is twelve.
    let overlay_panel: Option<Vec<String>> = match &state.mode {
        // Wrapped to the frame's own width, not just split on the newlines
        // `body` already carries: an error message from `load` — a bad
        // `--since` in particular — is one long unwrapped sentence, and
        // `panel` sizes its box to the widest line with no wrapping of its
        // own. Left unwrapped, that box is wider than the frame, and
        // `overlay` can only place a panel that fits inside it — past that
        // width it draws the panel's border over the frame's own left
        // border and silently drops every character past the right edge.
        Mode::Notice { title, body } => {
            let lines = wrap_notice(body, width);
            Some(panel(title, &lines, "any key to continue"))
        }
        Mode::Filter(draft) => Some(filter_panel(draft)),
        Mode::Calendar {
            field,
            year,
            month,
            day,
            ..
        } => Some(calendar_panel(*field, *year, *month, *day)),
        Mode::Browsing => None,
    };

    // Padded to whichever is taller: the terminal's own rows (when one is
    // measured, the same floor `clip` already cut a longer body to), or the
    // panel about to be overlaid — never shrunk below the content itself.
    // The same idea `commands::queue`'s own `two_pane_frame` follows,
    // padding its body out to `layout.rows` rather than stopping at content.
    let target = rows
        .unwrap_or(0)
        .max(overlay_panel.as_ref().map_or(0, Vec::len))
        .max(body.len());
    let blank = pad_to("", width);
    body.resize(target, blank);

    let right = filters_label(loaded, &state.filters);
    let mut frame = vec![frame_top(state.view, &right, width)];
    for line in &body {
        frame.push(format!("│{line}│"));
    }
    frame.push(frame_bottom(width));

    if let Some(overlay_lines) = &overlay_panel {
        overlay(&mut frame, overlay_lines);
    }

    for line in &frame {
        let _ = writeln!(out, "{line}");
    }
    // Between the frame's own bottom border and the keys line, so it never
    // takes a body row and so it never throws off `clip`'s own scroll
    // indicator — see the task's own non-goal against printing it inside the
    // frame. Its own row was already reserved above, in `frame_rows`.
    if let Some(note) = &note {
        let _ = writeln!(out, "  {note}");
    }
    let _ = writeln!(out, "{FOOTER}");
}

/// The same "Cost is a floor" note the printed tables carry, over whichever
/// rows are actually on screen: the current view's own entries, narrowed by
/// the current filters — not the whole ledger `loaded` holds, which may name
/// a model nowhere in view. Skills reads `loaded.skills` directly, the same
/// way `skills_lines` does, since a skill session belongs to no pipeline or
/// step for `scoped_entries` to narrow.
fn screen_unpriced_note(loaded: &Loaded, filters: &Filters, view: View) -> Option<String> {
    match view {
        View::Skills => unpriced_note(loaded.skills.iter()),
        // Steps and runs show every step or run their own filters admit —
        // neither view truncates by `filters.limit` — so the plain scoped
        // entries are exactly what is on screen.
        View::Steps | View::Runs => unpriced_note(scoped_entries(loaded, filters).into_iter()),
        // Pipelines narrows to `filters.limit` versions per block, the same
        // truncation `footer` accounts for in the printed table — without
        // it, a model priced fine on every version still on screen could be
        // blamed for an older one `limit` has already dropped.
        View::Pipelines => {
            let owned: Vec<Entry> = scoped_entries(loaded, filters)
                .into_iter()
                .cloned()
                .collect();
            let blocks = pipeline_blocks(&owned, filters.limit);
            let shown: HashSet<(String, String)> = blocks
                .iter()
                .flat_map(|b| b.versions.iter().map(|v| (b.name.clone(), v.name.clone())))
                .collect();
            unpriced_note(
                owned
                    .iter()
                    .filter(|e| shown.contains(&(e.pipeline.clone(), version_of(e).to_string()))),
            )
        }
    }
}

// -------------------------------------------------------------------- modes

/// One row of the filter panel — `↑`/`↓` moves between these, `←`/`→`
/// changes the value on the first two, and `enter` on the last two opens a
/// calendar rather than applying. `scope` and `limit` are not rows here at
/// all: the screen only ever opens bare, so both can only ever hold the
/// default they opened with, and a row with a single possible answer is not
/// a question — see the task's own context for why they left the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterField {
    Pipeline,
    Step,
    Since,
    Until,
}

const FILTER_FIELDS: &[FilterField] = &[
    FilterField::Pipeline,
    FilterField::Step,
    FilterField::Since,
    FilterField::Until,
];

/// A copy of [`Filters`] a person is editing in the filter panel, plus which
/// row the cursor is on. `esc` drops this untouched; `enter` turns it back
/// into the real `Filters` and reloads — see `run_screen`'s own handling of
/// [`Mode::Filter`].
#[derive(Debug, Clone)]
struct Draft {
    field: usize,
    filters: Filters,
}

/// What a key press means right now.
enum Mode {
    Browsing,
    /// The filter panel, editing a draft of the real filters.
    Filter(Draft),
    /// The month grid `enter` opens on the `since` or `until` row — the one
    /// place `enter` no longer means apply. `draft` is the panel's own state
    /// from just before the calendar opened, untouched by anything short of
    /// `enter` (pick) or `x` (clear) in here, so `esc` can hand it straight
    /// back to [`Mode::Filter`] exactly as it was.
    Calendar {
        draft: Draft,
        field: FilterField,
        year: i32,
        month: u32,
        day: u32,
    },
    /// A message held on screen until the next key dismisses it — an export
    /// confirmation, or a filter this ledger could not be read through, each
    /// under its own title. Held rather than written straight to `out`, for
    /// the same reason the queue screen's own `Mode::Outcome` is: `draw`'s
    /// first act is always to clear the screen, and a message written
    /// before that would never be read.
    Notice {
        title: &'static str,
        body: String,
    },
}

struct ScreenState {
    view: View,
    cursor: usize,
    filters: Filters,
    mode: Mode,
}

impl ScreenState {
    fn new(args: &EvalArgs) -> ScreenState {
        ScreenState {
            view: View::Pipelines,
            cursor: 0,
            filters: Filters::from_args(args),
            mode: Mode::Browsing,
        }
    }
}

/// Open the eval screen: read the ledger under the default filters — exactly
/// what bare `spoolway eval` means before a person touches anything — and
/// drive it from the keyboard until `q` or the input runs out.
///
/// Degrades rather than crashes with no terminal to drive, the same way
/// `commands::queue_screen` does: [`crate::platform::TermGuard`] only
/// changes stdin's mode on a real tty, so a piped `spoolway eval` reads
/// exactly the bytes it was given and ends the moment they run out.
pub fn screen(repo: &Repo, pipelines: &Pipelines) -> Result<()> {
    let mut stdin = crate::screen::RawStdin;
    let mut stdout = std::io::stdout();
    let _term = crate::platform::TermGuard::new();
    // Every flag at its bare default — `EvalArgs::is_bare` is what routed
    // this call here in the first place, so this is exactly the invocation
    // that reached `screen` rather than `run`.
    let args = EvalArgs {
        pipeline: None,
        step: None,
        since: None,
        until: None,
        month: None,
        limit: None,
        all: false,
        project: None,
        runs: false,
        task: None,
        run_group: None,
        trial: None,
        csv: false,
        by: None,
    };
    run_screen(repo, pipelines, &args, &mut stdin, &mut stdout)
}

fn run_screen(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &EvalArgs,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<()> {
    let mut state = ScreenState::new(args);
    let mut loaded = match load(repo, &state.filters) {
        Ok(loaded) => loaded,
        Err(err) => {
            let _ = writeln!(out, "spoolway eval: {err:#}");
            return Ok(());
        }
    };

    loop {
        draw(pipelines, &loaded, &state, out);
        let Some(key) = read_key(input) else {
            break;
        };
        if key == Key::Char('q') {
            break;
        }

        match &mut state.mode {
            Mode::Notice { .. } => {
                // Any key dismisses it — the message was already read on the
                // draw that preceded this key, the same reasoning the queue
                // screen's own `Mode::Outcome` follows.
                state.mode = Mode::Browsing;
            }
            Mode::Filter(draft) => match key {
                Key::Esc => state.mode = Mode::Browsing,
                Key::Up | Key::Char('k') => {
                    draft.field = draft.field.saturating_sub(1);
                }
                Key::Down | Key::Char('j') => {
                    draft.field = (draft.field + 1).min(FILTER_FIELDS.len() - 1);
                }
                Key::Left | Key::Right => {
                    handle_filter_change(&loaded, draft, key == Key::Right);
                }
                // On a date row, `enter` opens the calendar instead of
                // applying — the one row `enter` means something else on.
                // Everywhere else it applies the draft and reloads.
                Key::Enter
                    if matches!(
                        FILTER_FIELDS[draft.field],
                        FilterField::Since | FilterField::Until
                    ) =>
                {
                    let field = FILTER_FIELDS[draft.field];
                    let text = match field {
                        FilterField::Since => draft.filters.since.as_str(),
                        FilterField::Until => draft.filters.until.as_str(),
                        _ => unreachable!("only since/until open a calendar"),
                    };
                    let (year, month, day) = calendar_open_date(text);
                    state.mode = Mode::Calendar {
                        draft: draft.clone(),
                        field,
                        year,
                        month,
                        day,
                    };
                }
                Key::Enter => {
                    let candidate = draft.filters.clone();
                    match load(repo, &candidate) {
                        Ok(fresh) => {
                            loaded = fresh;
                            state.filters = candidate;
                            state.cursor = 0;
                            state.mode = Mode::Browsing;
                        }
                        Err(err) => {
                            state.mode = Mode::Notice {
                                title: "eval",
                                body: format!("{err:#}"),
                            }
                        }
                    }
                }
                _ => {}
            },
            Mode::Calendar {
                draft,
                field,
                year,
                month,
                day,
            } => match key {
                // The row is handed back exactly as it was — nothing in here
                // touches `draft` itself until `enter` or `x` commits.
                Key::Esc => state.mode = Mode::Filter(draft.clone()),
                Key::Char('x') => {
                    let mut next = draft.clone();
                    field_text_mut(&mut next, *field).clear();
                    state.mode = Mode::Filter(next);
                }
                Key::Enter => {
                    let mut next = draft.clone();
                    *field_text_mut(&mut next, *field) =
                        format!("{:04}-{:02}-{:02}", *year, *month, *day);
                    state.mode = Mode::Filter(next);
                }
                Key::Left => step_calendar_day(year, month, day, -1),
                Key::Right => step_calendar_day(year, month, day, 1),
                Key::Up => step_calendar_day(year, month, day, -7),
                Key::Down => step_calendar_day(year, month, day, 7),
                Key::PageUp => step_calendar_month(year, month, day, -1),
                Key::PageDown => step_calendar_month(year, month, day, 1),
                _ => {}
            },
            Mode::Browsing => match key {
                Key::Up | Key::Char('k') => state.cursor = state.cursor.saturating_sub(1),
                Key::Down | Key::Char('j') => state.cursor += 1,
                Key::Tab => {
                    state.view = state.view.next();
                    state.cursor = 0;
                }
                Key::Char('f') => {
                    state.mode = Mode::Filter(Draft {
                        field: 0,
                        filters: state.filters.clone(),
                    });
                }
                Key::Char('r') => match load(repo, &state.filters) {
                    Ok(fresh) => loaded = fresh,
                    Err(err) => {
                        state.mode = Mode::Notice {
                            title: "eval",
                            body: format!("{err:#}"),
                        }
                    }
                },
                Key::Char('e') => {
                    match export(repo, &loaded, &state.filters, pipelines, state.view) {
                        Ok((path, rows)) => {
                            state.mode = Mode::Notice {
                                title: "exported",
                                body: format!(
                                    "{}, the {} view\n{}",
                                    plural(rows, "row"),
                                    state.view.label(),
                                    display_relative(&path, &repo.root),
                                ),
                            };
                        }
                        Err(err) => {
                            state.mode = Mode::Notice {
                                title: "eval",
                                body: format!("{err:#}"),
                            }
                        }
                    }
                }
                _ => {}
            },
        }
    }
    Ok(())
}

/// The one string a calendar row's own value lives in — what the calendar
/// writes into on `enter` (pick) or `x` (clear). Never called for `pipeline`
/// or `step`, which cycle through `handle_filter_change` instead and have no
/// string of their own to hand back.
fn field_text_mut(draft: &mut Draft, field: FilterField) -> &mut String {
    match field {
        FilterField::Since => &mut draft.filters.since,
        FilterField::Until => &mut draft.filters.until,
        _ => unreachable!("only since/until open a calendar"),
    }
}

fn handle_filter_change(loaded: &Loaded, draft: &mut Draft, forward: bool) {
    match FILTER_FIELDS[draft.field] {
        FilterField::Pipeline => {
            let candidates = pipeline_candidates(loaded);
            draft.filters.pipeline = cycle_option(&candidates, &draft.filters.pipeline, forward);
            // Changing the pipeline can drop today's `step` off the list
            // entirely — cleared rather than left naming a step this
            // pipeline never ran, which `enter` would apply as a filter that
            // silently matches nothing.
            let steps = step_candidates(loaded, draft.filters.pipeline.as_deref());
            if let Some(step) = &draft.filters.step
                && !steps.contains(step)
            {
                draft.filters.step = None;
            }
        }
        FilterField::Step => {
            let candidates = step_candidates(loaded, draft.filters.pipeline.as_deref());
            draft.filters.step = cycle_option(&candidates, &draft.filters.step, forward);
        }
        // Neither answers to `←`/`→` any more — a calendar is the only way
        // to change either now, reached through `enter` instead.
        FilterField::Since | FilterField::Until => {}
    }
}

fn filter_field_label(field: FilterField) -> &'static str {
    match field {
        FilterField::Pipeline => "pipeline",
        FilterField::Step => "step",
        FilterField::Since => "since",
        FilterField::Until => "until",
    }
}

/// What a row of the filter panel shows for its own value. Neither date row
/// ever carries a trailing cursor mark the way a typed field once did:
/// nothing on the panel is typed into any more, so being the active row
/// changes nothing about how a row's value reads.
fn filter_field_value(draft: &Draft, field: FilterField) -> String {
    match field {
        FilterField::Pipeline => {
            format!("‹ {} ›", draft.filters.pipeline.as_deref().unwrap_or("all"))
        }
        FilterField::Step => format!("‹ {} ›", draft.filters.step.as_deref().unwrap_or("all")),
        FilterField::Since => date_field_value(&draft.filters.since, "(blank — the start)"),
        FilterField::Until => date_field_value(&draft.filters.until, "(blank — now)"),
    }
}

fn date_field_value(text: &str, placeholder: &str) -> String {
    non_empty(text).unwrap_or(placeholder).to_string()
}

/// The key line under the filter panel — apply on the two cycled rows,
/// calendar on the two date rows, so it never claims a row can do something
/// it can't. `FILTER_DATE_KEYS` is shorter than `FILTER_APPLY_KEYS`, so
/// `filter_panel` pads it out to the same length before handing it to
/// `boxed`: left as the shorter string, it would be the box's own widest
/// line whenever the cursor sits on a date row, shrinking the panel by those
/// missing columns and shifting the table behind it — then growing back the
/// moment the row changes again. [`calendar_panel`] pins to this same
/// length for the same reason, on the one row that opens it.
const FILTER_APPLY_KEYS: &str = "↑↓ row   ‹› change   enter apply   esc back";
const FILTER_DATE_KEYS: &str = "↑↓ row   enter calendar   esc back";

/// The filter panel itself, as an overlay `draw` centres over the frame —
/// one row per [`FilterField`], the cursor marked on whichever it is on, and
/// a key line that names what `enter` does on the row the cursor is on.
fn filter_panel(draft: &Draft) -> Vec<String> {
    let mut body = Vec::with_capacity(FILTER_FIELDS.len() + 2);
    for (i, field) in FILTER_FIELDS.iter().enumerate() {
        let marker = if i == draft.field { ">" } else { " " };
        let label = filter_field_label(*field);
        let value = filter_field_value(draft, *field);
        body.push(format!("{marker} {label:<8} {value}"));
    }
    body.push(String::new());
    body.push("enter opens a calendar on since and until".to_string());
    let keys = match FILTER_FIELDS[draft.field] {
        FilterField::Since | FilterField::Until => {
            pad_to(FILTER_DATE_KEYS, FILTER_APPLY_KEYS.chars().count())
        }
        FilterField::Pipeline | FilterField::Step => FILTER_APPLY_KEYS.to_string(),
    };
    panel("filters", &body, &keys)
}

/// The day a calendar opens on when `enter` first opens it: the row's own
/// bound, read as a plain `YYYY-MM-DD` the same way `parse_instant` reads
/// one — not through `parse_instant` itself, which this task's own
/// non-goals leave untouched — or today when the row is blank or holds
/// anything else. Deliberately blind to the ledger, the same as the grid
/// itself: see the task's own non-goal against marking days that have lanes.
fn calendar_open_date(text: &str) -> (i32, u32, u32) {
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    let date = non_empty(text)
        .and_then(|t| chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").ok())
        .unwrap_or(today);
    (date.year(), date.month(), date.day())
}

const CALENDAR_KEYS: &str = "←→ day   ↑↓ week   enter pick   esc back";

/// The month grid `enter` opens on a date row — see [`Mode::Calendar`]. Its
/// width is pinned to [`FILTER_APPLY_KEYS`]'s own length rather than sized to
/// its own narrower content: the calendar opens in the exact spot the filter
/// panel sat in, and a narrower box there would shift the table columns to
/// its right sideways the moment `enter` is pressed, then shift them back the
/// moment it closes.
fn calendar_panel(field: FilterField, year: i32, month: u32, day: u32) -> Vec<String> {
    let width = FILTER_APPLY_KEYS.chars().count();
    let mut body = vec![pad_center(
        &format!("‹ {} {year} ›", month_name(month)),
        width,
    )];
    body.push("    Mo Tu We Th Fr Sa Su".to_string());
    for week in month_weeks(year, month) {
        body.push(format!("    {}", render_week(&week, day)));
    }
    body.push(String::new());
    body.push("pgup/pgdn month      x no bound".to_string());
    panel(filter_field_label(field), &body, CALENDAR_KEYS)
}

/// `content` centred within `width`, favouring the left side when the gap is
/// odd — matched against the mockup's own `‹ August 2026 ›`, which sits 14
/// spaces in from either edge of a 43-wide line.
fn pad_center(content: &str, width: usize) -> String {
    let len = content.chars().count();
    if len >= width {
        return content.to_string();
    }
    let left = (width - len) / 2;
    let right = width - len - left;
    format!("{}{content}{}", " ".repeat(left), " ".repeat(right))
}

/// The English name of `month` (`1`..=`12`) — `chrono::Month::name`, so the
/// calendar spells a month the same way any other reader of this codebase
/// would already expect it to, rather than a table written out again here.
fn month_name(month: u32) -> &'static str {
    chrono::Month::try_from(month as u8)
        .map(|m| m.name())
        .unwrap_or("?")
}

/// How many days `month` has in `year`, found by stepping into the next
/// month's own first day rather than a hand-written table of month lengths —
/// so February's leap years fall out of `chrono`'s own calendar instead of
/// one more thing to get right here.
fn days_in_month(year: i32, month: u32) -> u32 {
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).expect("month is 1..=12");
    let next_first = if month == 12 {
        chrono::NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        chrono::NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .expect("month is 1..=12");
    (next_first - first).num_days() as u32
}

/// The Monday-first weeks a month draws across, each a row of at most seven
/// days — `None` where the grid has no day of this month to show, at the
/// start of the first week and the end of the last.
fn month_weeks(year: i32, month: u32) -> Vec<[Option<u32>; 7]> {
    use chrono::Datelike;
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).expect("month is 1..=12");
    let lead = first.weekday().num_days_from_monday() as usize;
    let total = days_in_month(year, month);

    let mut weeks = Vec::new();
    let mut week = [None; 7];
    let mut col = lead;
    for day in 1..=total {
        week[col] = Some(day);
        col += 1;
        if col == 7 {
            weeks.push(week);
            week = [None; 7];
            col = 0;
        }
    }
    if col != 0 {
        weeks.push(week);
    }
    weeks
}

/// One week's worth of the grid — each day right-justified two wide and
/// joined by a single space, except `selected`, which brackets its own
/// digits instead of taking a leading space and in doing so borrows the
/// space that would have opened the day after it too. Matches the mockup's
/// own spacing exactly: `20[21]22`, not `20 [21] 22`.
fn render_week(week: &[Option<u32>; 7], selected: u32) -> String {
    let mut out = String::new();
    let mut no_lead = true; // the first column never gets a leading space
    for cell in week {
        match cell {
            None => {
                out.push_str(if no_lead { "  " } else { "   " });
                no_lead = false;
            }
            Some(day) if *day == selected => {
                out.push_str(&format!("[{day}]"));
                no_lead = true;
            }
            Some(day) => {
                if no_lead {
                    out.push_str(&format!("{day:2}"));
                } else {
                    out.push_str(&format!(" {day:2}"));
                }
                no_lead = false;
            }
        }
    }
    out
}

/// Move the calendar's cursor day by `delta` days — negative for `←`/`↑`,
/// positive for `→`/`↓` — rolling across month and year boundaries the way a
/// calendar always does, so the grid simply redraws around wherever the
/// cursor lands rather than clamping at either edge of the visible month.
fn step_calendar_day(year: &mut i32, month: &mut u32, day: &mut u32, delta: i64) {
    use chrono::Datelike;
    let current = chrono::NaiveDate::from_ymd_opt(*year, *month, *day)
        .unwrap_or_else(|| chrono::Local::now().date_naive());
    let next = current + chrono::Duration::days(delta);
    *year = next.year();
    *month = next.month();
    *day = next.day();
}

/// Move the calendar to the previous or next month, keeping the day of month
/// where the new month has it and clamping down to its last day where it
/// does not — the `31st` of a 31-day month landing on 30-day September's
/// `30th` rather than a day that month's grid never draws.
fn step_calendar_month(year: &mut i32, month: &mut u32, day: &mut u32, delta: i32) {
    let total = *year * 12 + *month as i32 - 1 + delta;
    *year = total.div_euclid(12);
    *month = total.rem_euclid(12) as u32 + 1;
    *day = (*day).min(days_in_month(*year, *month));
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression for the review finding that the skills view's header and row
    // drew to different widths, throwing the frame's right border out of
    // line. Pinned as a property rather than an eyeballed string.
    #[test]
    fn skills_header_and_row_draw_to_the_same_width() {
        let row = SkillVersion {
            name: "b210d1a8".into(),
            since: "2026-08-15".into(),
            sessions: 4,
            cost: 12.0,
            unpriced: 0,
        };
        let vw = row.name.len().max("VERSION".len());
        let header = skills_header_plain(vw);
        let line = skills_row_line(&row, vw);

        assert_eq!(
            header.chars().count(),
            line.chars().count(),
            "header: {header:?}\nrow:    {line:?}"
        );
    }

    // The same regression, for the steps view's own `STEP` column now that
    // its width is measured rather than fixed.
    #[test]
    fn step_header_and_row_draw_to_the_same_width() {
        let row = StepRow {
            step: "reproduce-again".to_string(),
            metrics: Metrics::default(),
        };
        let sw = row.step.len().max("STEP".len());
        let header = step_header_plain(sw);
        let line = step_row_line(&row, sw);

        assert_eq!(
            header.chars().count(),
            line.chars().count(),
            "header: {header:?}\nrow:    {line:?}"
        );
    }

    fn lane(task: &str, step: &str, version: &str, round: u32, outcome: Option<&str>) -> Entry {
        Entry {
            ts: "2026-08-04T07:00:00+00:00".into(),
            task: task.into(),
            plan: None,
            step: step.into(),
            pipeline: "default".into(),
            agent: "pi".into(),
            kind: "pi".into(),
            model: "qwen".into(),
            session: "s".into(),
            round,
            wall_s: 60,
            turns: 1,
            tokens: crate::usage::Tokens::default(),
            cost_usd: Some(1.0),
            ctx_peak: None,
            version: Some(version.into()),
            commit: Some("a91c33e".into()),
            outcome: outcome.map(str::to_string),
            run: None,
            trial: None,
            skill: None,
            project: "demo".into(),
        }
    }

    fn no_fallback() -> HashMap<(String, String), String> {
        HashMap::new()
    }

    fn no_models() -> BTreeMap<String, ModelPrice> {
        BTreeMap::new()
    }

    /// The currency lives in the column header now, not the cell — a cost
    /// cell is a plain number whether it is a full total or a floor, and
    /// `—` only where nothing at all could be priced.
    #[test]
    fn cost_of_is_plain_whether_or_not_it_is_a_floor() {
        assert_eq!(cost_of(0.0, 2, 2), "—", "nothing priced at all");
        assert_eq!(cost_of(23.39, 2, 0), "23.39", "fully priced, no sigil");
        assert_eq!(
            cost_of(23.39, 3, 1),
            "23.39",
            "a floor prints the same plain number, no `+?`"
        );
    }

    #[test]
    fn a_task_that_straddles_a_version_counts_on_both_rows() {
        let entries = vec![
            lane("login", "implement", "aaaa1111", 1, Some("pass")),
            lane("login", "review", "bbbb2222", 1, Some("pass")),
            lane("logout", "implement", "bbbb2222", 1, Some("pass")),
        ];
        let fallback = fallback_keys(&entries);
        let models = no_models();
        assert_eq!(
            Metrics::for_row(&entries, "default", "aaaa1111", &fallback, &models).tasks,
            1,
            "login touched this row too, even though it also ran under bbbb2222"
        );
        assert_eq!(
            Metrics::for_row(&entries, "default", "bbbb2222", &fallback, &models).tasks,
            2,
            "login and logout both touched this row"
        );
    }

    #[test]
    fn pass_is_the_share_of_lanes_not_of_tasks() {
        let entries = vec![
            lane("login", "implement", "v1", 1, Some("pass")),
            lane("login", "review", "v1", 1, Some("fail")),
            lane("login", "implement", "v1", 2, Some("pass")),
            lane("login", "review", "v1", 2, Some("pass")),
        ];
        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &no_models());
        assert_eq!(m.tasks, 1);
        assert_eq!(m.lanes, 4);
        assert_eq!(m.pass_share(), Some(75.0), "one of the four lanes failed");
    }

    #[test]
    fn a_block_is_what_a_lane_reports_not_a_run_grain_judgement() {
        let entries = vec![
            lane("login", "implement", "v1", 1, Some("block")),
            lane("logout", "implement", "v1", 1, Some("pass")),
        ];
        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &no_models());
        assert_eq!(m.tasks, 2);
        assert_eq!(m.blocked, 1);
        assert_eq!(m.pass_share(), Some(50.0));
    }

    #[test]
    fn a_lane_that_never_reported_is_left_out_of_the_pass_share() {
        let entries = vec![
            lane("login", "implement", "v1", 1, Some("pass")),
            lane("logout", "implement", "v1", 1, None),
        ];
        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &no_models());
        assert_eq!(m.lanes, 2, "it still ran, and it still cost something");
        assert_eq!(
            m.pass_share(),
            Some(100.0),
            "one lane reported, and it passed"
        );
    }

    #[test]
    fn l_task_and_dollar_task_are_the_row_s_totals_over_its_tasks() {
        // Two runs, four lanes: L/TASK reads 2.0, not the raw lane count.
        let mut a1 = lane("login", "implement", "v1", 1, Some("pass"));
        a1.run = Some("ra".into());
        let mut a2 = lane("login", "review", "v1", 1, Some("pass"));
        a2.run = Some("ra".into());
        a2.cost_usd = Some(3.0);
        let mut b1 = lane("logout", "implement", "v1", 1, Some("pass"));
        b1.run = Some("rb".into());
        let mut b2 = lane("logout", "review", "v1", 1, Some("pass"));
        b2.run = Some("rb".into());
        b2.cost_usd = Some(3.0);
        let entries = vec![a1, a2, b1, b2];
        let fallback = fallback_keys(&entries);
        let m = Metrics::for_row(&entries, "default", "v1", &fallback, &no_models());
        assert_eq!(m.tasks, 2);
        assert_eq!(m.lanes, 4);
        assert_eq!(m.lanes_per_task(), 2.0);
        assert_eq!(m.cost, 8.0, "two lanes at $1 and two at $3");
        assert_eq!(m.cost_per_task(), 4.0);
    }

    #[test]
    fn ctx_is_the_largest_peak_on_the_row_as_a_percentage_of_its_own_model_s_window() {
        let mut small = lane("login", "implement", "v1", 1, Some("pass"));
        small.model = "sized".into();
        small.ctx_peak = Some(50_000);
        let mut big = lane("logout", "implement", "v1", 1, Some("pass"));
        big.model = "sized".into();
        big.ctx_peak = Some(91_000);
        let entries = vec![small, big];

        let mut models = BTreeMap::new();
        models.insert(
            "sized".to_string(),
            ModelPrice {
                context_window: 100_000,
                ..ModelPrice::default()
            },
        );

        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &models);
        assert_eq!(m.ctx_peak_tokens, Some(91_000), "the larger of the two");
        assert_eq!(m.ctx_str(), "91%");
    }

    #[test]
    fn ctx_falls_back_to_raw_tokens_when_the_model_s_window_is_unconfigured() {
        let mut with_peak = lane("login", "implement", "v1", 1, Some("pass"));
        with_peak.model = "unsized".into();
        with_peak.ctx_peak = Some(142_000);
        let entries = vec![with_peak];
        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &no_models());
        assert_eq!(m.ctx_str(), "142k");
    }

    #[test]
    fn ctx_is_a_dash_when_no_lane_on_the_row_banked_one() {
        let entries = vec![lane("login", "implement", "v1", 1, Some("pass"))];
        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &no_models());
        assert_eq!(m.ctx_peak_tokens, None);
        assert_eq!(m.ctx_str(), "—");
    }

    /// A zero-token line — an enrolment line, a synthetic turn that
    /// contributed nothing — spends nothing, so it must not be counted as a
    /// lane the row's total is missing a price for. Without the exclusion,
    /// this row would read `unpriced: 1` and its cost as a floor even though
    /// every lane that actually spent something was priced.
    #[test]
    fn a_zero_token_unpriced_lane_is_not_counted_as_unpriced() {
        let mut priced = lane("login", "implement", "v1", 1, Some("pass"));
        priced.cost_usd = Some(5.0);
        let mut zero_token = lane("login", "implement", "v1", 2, Some("pass"));
        zero_token.cost_usd = None;
        zero_token.tokens = crate::usage::Tokens::default();

        let entries = vec![priced, zero_token];
        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &no_models());
        assert_eq!(m.unpriced, 0);
        assert_eq!(m.lanes, 2);
    }

    /// The counterpart: a lane that really spent tokens but named a model
    /// nothing prices must still show up as unpriced — `m.unpriced` is where
    /// that floor is recorded, even though `cost_per_task_str` itself prints
    /// the same plain number a full total would.
    #[test]
    fn a_real_unpriced_lane_still_counts() {
        let mut priced = lane("login", "implement", "v1", 1, Some("pass"));
        priced.cost_usd = Some(5.0);
        let mut unpriced = lane("login", "implement", "v1", 2, Some("pass"));
        unpriced.cost_usd = None;
        unpriced.tokens = crate::usage::Tokens {
            input: 10,
            ..Default::default()
        };

        let entries = vec![priced, unpriced];
        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &no_models());
        assert_eq!(m.unpriced, 1);
        assert_eq!(m.cost_per_task_str(), "5.00");
    }

    /// `json_row` has to carry the unpriced count itself — a consumer reading
    /// the `unpriced` field is the only way to tell a partly-priced floor
    /// (`cost_usd` a plain number) apart from a complete total, since both
    /// print as the same shape of number.
    #[test]
    fn json_row_carries_the_unpriced_count() {
        let mut priced = lane("login", "implement", "v1", 1, Some("pass"));
        priced.cost_usd = Some(5.0);
        let mut unpriced = lane("login", "implement", "v1", 2, Some("pass"));
        unpriced.cost_usd = None;
        unpriced.tokens = crate::usage::Tokens {
            input: 10,
            ..Default::default()
        };

        let entries = vec![priced, unpriced];
        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &no_models());
        let version = Version {
            project: "demo".into(),
            name: "v1".into(),
            since: "2026-08-04".into(),
        };
        let row = json_row("default", &version, &m);
        assert_eq!(row["unpriced"], 1);
        // A plain number here, not `null` — only a *wholly* unpriced row nulls
        // `cost_usd`, and this row has one priced lane.
        assert_eq!(row["cost_usd"], 5.0);
    }

    /// A row where nothing at all could be priced must still say so through
    /// `cost_usd: null` — `unpriced` alone does not distinguish "everything
    /// unpriced" from "one of several", which is exactly the shape a consumer
    /// needs `cost_usd`'s presence to answer.
    #[test]
    fn json_row_nulls_cost_when_every_lane_is_unpriced() {
        let mut unpriced = lane("login", "implement", "v1", 1, Some("pass"));
        unpriced.cost_usd = None;
        unpriced.tokens = crate::usage::Tokens {
            input: 10,
            ..Default::default()
        };

        let entries = vec![unpriced];
        let m = Metrics::for_row(&entries, "default", "v1", &no_fallback(), &no_models());
        let version = Version {
            project: "demo".into(),
            name: "v1".into(),
            since: "2026-08-04".into(),
        };
        let row = json_row("default", &version, &m);
        assert_eq!(row["unpriced"], 1);
        assert!(row["cost_usd"].is_null());
    }

    /// `csv_cost` blanks the cell only when every one of `total` is unpriced —
    /// a partial floor still prints the number it always did, with the
    /// `unpriced` column beside it carrying what the table's own note under
    /// it says instead.
    #[test]
    fn csv_cost_is_blank_only_when_everything_is_unpriced() {
        assert_eq!(
            csv_cost(9.05, 3, 1),
            "9.05",
            "a floor still prints a number"
        );
        assert_eq!(
            csv_cost(0.0, 2, 2),
            "",
            "wholly unpriced is blank, not 0.00"
        );
        assert_eq!(csv_cost(4.71, 5, 0), "4.71", "a real total prints plainly");
    }

    #[test]
    fn versions_come_out_newest_first_and_capped() {
        let mut entries = Vec::new();
        for (n, v) in ["v1", "v2", "v3"].iter().enumerate() {
            let mut lane = lane("t", "implement", v, 1, Some("pass"));
            lane.ts = format!("2026-08-0{}T07:00:00+00:00", n + 1);
            entries.push(lane);
        }
        let versions = versions_of(&entries, "default", 2);
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].name, "v3");
        assert_eq!(versions[1].name, "v2");
    }

    #[test]
    fn pipelines_are_ordered_by_their_newest_version() {
        let mut default_old = lane("a", "implement", "v1", 1, Some("pass"));
        default_old.ts = "2026-08-01T07:00:00+00:00".into();
        let mut local_new = lane("b", "implement", "v2", 1, Some("pass"));
        local_new.pipeline = "local".into();
        local_new.ts = "2026-08-10T07:00:00+00:00".into();

        let entries = vec![default_old, local_new];
        let blocks = pipeline_blocks(&entries, 10);
        assert_eq!(blocks[0].name, "local", "its version is the newer one");
        assert_eq!(blocks[1].name, "default");
    }

    #[test]
    fn project_stays_off_screen_when_every_row_is_the_same_project() {
        let entries = vec![
            lane("login", "implement", "v1", 1, Some("pass")),
            lane("logout", "implement", "v2", 1, Some("pass")),
        ];
        let blocks = pipeline_blocks(&entries, 10);
        assert!(
            !spans_more_than_one_project(&blocks),
            "both lanes are `demo`, lane()'s own project"
        );
        assert!(
            !header(false, 7, 8).contains("PROJECT"),
            "the header omits the column entirely, not just its value"
        );
    }

    #[test]
    fn project_prints_once_two_of_them_are_on_screen_together() {
        // Different versions, so alpha and beta each get their own row —
        // sharing one version would merge them into a single row whose
        // `Version.project` names only the first-seen project, hiding the
        // second one from this very check.
        let alpha = Entry {
            project: "alpha".into(),
            ..lane("login", "implement", "v1", 1, Some("pass"))
        };
        let beta = Entry {
            project: "beta".into(),
            ..lane("logout", "implement", "v2", 1, Some("pass"))
        };
        let entries = vec![alpha, beta];
        let blocks = pipeline_blocks(&entries, 10);
        assert!(
            spans_more_than_one_project(&blocks),
            "alpha and beta are both on screen, on their own rows"
        );
        assert!(header(true, 7, 8).contains("PROJECT"));
    }

    #[test]
    fn the_version_column_widens_for_the_unversioned_row() {
        // `unversioned` is longer than a fingerprint, and a fixed-width column
        // would push every figure on that row out of line with the table.
        let old = Entry {
            version: None,
            ..lane("login", "implement", "v1", 1, Some("pass"))
        };
        let versions = versions_of(&[old], "default", 10);
        assert_eq!(versions[0].name, UNVERSIONED);
        assert_eq!(name_width(&versions), UNVERSIONED.len());

        let normal = versions_of(
            &[lane("login", "implement", "3f9a1c04", 1, None)],
            "default",
            10,
        );
        assert_eq!(name_width(&normal), 8);
    }

    #[test]
    fn a_line_with_no_run_falls_back_to_a_key_derived_from_its_task() {
        let entries = vec![lane("session-resume", "land", "v1", 1, Some("pass"))];
        let fallback = fallback_keys(&entries);
        let key = run_key(&entries[0], &fallback);
        assert_eq!(key, "session-resume@0804");
    }

    #[test]
    fn two_projects_whose_same_named_task_first_ran_the_same_day_get_distinct_fallback_ids() {
        let alpha = Entry {
            project: "alpha".into(),
            ..lane("login", "implement", "v1", 1, Some("pass"))
        };
        let beta = Entry {
            project: "beta".into(),
            ..lane("login", "implement", "v1", 1, Some("pass"))
        };
        let entries = vec![alpha, beta];
        let fallback = fallback_keys(&entries);

        let id_alpha = fallback
            .get(&("alpha".to_string(), "login".to_string()))
            .cloned()
            .unwrap();
        let id_beta = fallback
            .get(&("beta".to_string(), "login".to_string()))
            .cloned()
            .unwrap();
        assert_ne!(
            id_alpha, id_beta,
            "two runs pinned by one id resolves to neither"
        );
        assert_eq!(id_alpha, "alpha/login@0804");
        assert_eq!(id_beta, "beta/login@0804");

        let rows = list_runs(&entries, &fallback, &no_models());
        let ids: HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids.len(), 2, "one row per run, each with its own id");
    }

    #[test]
    fn a_run_id_groups_its_lanes_even_across_different_tasks_named_the_same() {
        let mut a = lane("t", "implement", "v1", 1, Some("pass"));
        a.run = Some("r00001".into());
        let mut b = lane("t", "review", "v1", 1, Some("pass"));
        b.run = Some("r00001".into());
        let entries = vec![a, b];
        let fallback = fallback_keys(&entries);
        let groups = group_by_run(&entries, &fallback);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1.len(), 2);
    }

    #[test]
    fn a_historical_run_is_listed_and_named_by_its_fallback_key() {
        let entries = vec![lane("session-resume", "land", "v1", 1, Some("pass"))];
        let fallback = fallback_keys(&entries);
        let rows = list_runs(&entries, &fallback, &no_models());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "session-resume@0804");
        assert_eq!(rows[0].task, "session-resume");
    }

    #[test]
    fn run_pass_share_is_over_its_own_lanes() {
        let mut a = lane("t", "implement", "v1", 1, Some("pass"));
        a.run = Some("r1".into());
        let mut b = lane("t", "review", "v1", 1, Some("fail"));
        b.run = Some("r1".into());
        let entries = vec![a, b];
        let fallback = fallback_keys(&entries);
        let rows = list_runs(&entries, &fallback, &no_models());
        assert_eq!(rows[0].pass_share(), Some(50.0));
        assert_eq!(rows[0].blocked, 0);
    }

    /// A trial's arms are separate tasks, so what has to correlate them is
    /// `entry.trial` — this is the filter `eval::run` applies before
    /// `--trial` reaches `list_runs`, pinned directly since it is a plain
    /// `Vec::retain` inlined there rather than a function of its own.
    #[test]
    fn filtering_entries_by_trial_id_keeps_only_that_trial_s_arms() {
        let mut a = lane("solo-1", "implement", "v1", 1, Some("pass"));
        a.trial = Some("solo".into());
        let mut b = lane("solo-2", "implement", "v1", 1, Some("fail"));
        b.trial = Some("solo".into());
        let unrelated = lane("other-task", "implement", "v1", 1, Some("pass"));

        let entries = vec![a, b, unrelated];
        let trial: Vec<Entry> = entries
            .into_iter()
            .filter(|e| e.trial.as_deref() == Some("solo"))
            .collect();
        assert_eq!(trial.len(), 2);
        assert!(trial.iter().all(|e| e.task.starts_with("solo-")));
    }

    #[test]
    fn a_trial_delta_line_names_both_arms_and_the_direction_each_figure_moved() {
        let mut baseline = lane("solo-1", "implement", "v1", 1, Some("pass"));
        baseline.run = Some("r1".into());
        let mut challenger = lane("solo-2", "implement", "v1", 1, Some("fail"));
        challenger.run = Some("r2".into());
        challenger.cost_usd = Some(3.0);
        challenger.wall_s = 120;

        let fallback = fallback_keys(&[baseline.clone(), challenger.clone()]);
        let base_row = &list_runs(&[baseline], &fallback, &no_models())[0];
        let other_row = &list_runs(&[challenger], &fallback, &no_models())[0];

        let line = trial_delta_line(base_row, other_row);
        assert!(line.starts_with("solo-2 vs solo-1: "), "{line:?}");
        assert!(line.contains("pass -100pp"), "{line:?}");
        assert!(line.contains("cost +$2.00"), "{line:?}");
    }

    // covers: skills — a named skill gets a block of its own and every other folds into `other`

    #[test]
    fn a_skill_named_in_config_gets_its_own_block_and_the_rest_are_left_out_entirely() {
        let mut plan = lane("", "", "v1", 0, None);
        plan.skill = Some("spoolway-plan".to_string());
        plan.cost_usd = Some(2.0);
        let mut named = lane("", "", "v1", 0, None);
        named.session = "s2".into();
        named.skill = Some("code-review".to_string());
        named.cost_usd = Some(1.0);

        let entries = vec![plan, named];
        let configured = vec!["spoolway-plan".to_string()];
        let blocks = skill_blocks(&entries, &configured, 10);

        let names: Vec<&str> = blocks.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["spoolway-plan"],
            "code-review is not configured, and there is no `other` to catch it"
        );
    }
}

#[cfg(test)]
mod screen_tests {
    use super::*;
    use crate::commands::testutil::fixture;

    fn keys(s: &str) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(s.as_bytes().to_vec())
    }

    /// A minimal in-memory `Entry` on the `default` pipeline — what the pure
    /// functions below (`ordered_steps`, `step_blocks`, ...) that never touch
    /// disk are exercised against.
    fn entry(step: &str, version: &str) -> Entry {
        Entry {
            ts: "2026-08-04T07:00:00+00:00".into(),
            task: "t".into(),
            plan: None,
            step: step.into(),
            pipeline: "default".into(),
            agent: "pi".into(),
            kind: "pi".into(),
            model: "qwen".into(),
            session: "s".into(),
            round: 1,
            wall_s: 60,
            turns: 1,
            tokens: crate::usage::Tokens::default(),
            cost_usd: Some(1.0),
            ctx_peak: None,
            version: Some(version.into()),
            commit: None,
            outcome: Some("pass".into()),
            run: None,
            trial: None,
            skill: None,
            project: "demo".into(),
        }
    }

    /// One ledger line, written straight to `repo`'s own `usage.jsonl` — the
    /// screen reads through `collect_scoped`, which reads the real file, so
    /// its own tests need one on disk rather than an `Entry` built in memory.
    #[allow(clippy::too_many_arguments)]
    fn bank(
        repo: &Repo,
        ts: &str,
        task: &str,
        step: &str,
        pipeline: &str,
        model: &str,
        version: &str,
        cost: f64,
        outcome: Option<&str>,
    ) {
        crate::usage::append(
            repo,
            &Entry {
                ts: ts.to_string(),
                task: task.to_string(),
                plan: None,
                step: step.to_string(),
                pipeline: pipeline.to_string(),
                agent: "pi".to_string(),
                kind: "pi".to_string(),
                model: model.to_string(),
                session: format!("{task}-{step}-{version}"),
                round: 1,
                wall_s: 60,
                turns: 1,
                tokens: crate::usage::Tokens::default(),
                cost_usd: Some(cost),
                ctx_peak: None,
                version: Some(version.to_string()),
                commit: Some(format!("commit-{version}")),
                outcome: outcome.map(str::to_string),
                run: Some(format!("r-{task}")),
                trial: None,
                skill: None,
                project: String::new(),
            },
        )
        .unwrap();
    }

    /// The default `EvalArgs` — nothing filtered, nothing preset — that
    /// every screen test opens with.
    fn no_args() -> EvalArgs {
        EvalArgs {
            pipeline: None,
            step: None,
            since: None,
            until: None,
            month: None,
            limit: None,
            all: false,
            project: None,
            runs: false,
            task: None,
            run_group: None,
            trial: None,
            csv: false,
            by: None,
        }
    }

    /// Unfiltered `Filters`, exactly as the screen opens with them.
    fn no_filters() -> Filters {
        Filters {
            pipeline: None,
            step: None,
            since: String::new(),
            until: String::new(),
            scope: Scope::Mine,
            limit: 10,
        }
    }

    /// A `Loaded` over `entries` with its fallback keys computed and nothing
    /// else set — the shape every pure-view test wants.
    fn loaded(entries: Vec<Entry>) -> Loaded {
        Loaded {
            fallback: fallback_keys(&entries),
            entries,
            skills: Vec::new(),
            models: BTreeMap::new(),
            scope_label: "demo".to_string(),
            configured_skills: Vec::new(),
        }
    }

    /// A fixture with one ordinary `implement` run banked — the smallest
    /// ledger that draws a real table.
    fn fixture_with_one_run(name: &str) -> Repo {
        let repo = fixture(name);
        bank(
            &repo,
            "2026-08-01T09:00:00+00:00",
            "a",
            "implement",
            "default",
            "qwen",
            "v1",
            1.0,
            Some("pass"),
        );
        repo
    }

    /// Runs the screen against `repo` with the default args, feeding it
    /// `input`, and hands back everything it drew.
    fn screen(repo: &Repo, input: &str) -> String {
        let pipelines = Pipelines::builtin();
        let mut input = keys(input);
        let mut out = Vec::new();
        run_screen(repo, &pipelines, &no_args(), &mut input, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Regression: the note used to take a row `frame_rows` had not made
    /// room for, so drawing it on a real terminal wrote one line past the
    /// spare row `frame_rows` otherwise reserves and scrolled the frame's
    /// own top border off screen. `frame_chrome` is the count `frame_rows`
    /// builds on; pinned directly since `terminal_size` reads `None` in this
    /// harness and so never exercises `frame_rows`'s own subtraction.
    #[test]
    fn frame_chrome_reserves_one_more_row_when_the_unpriced_note_will_be_drawn() {
        assert_eq!(frame_chrome(false), 4);
        assert_eq!(
            frame_chrome(true),
            5,
            "the note takes a row of its own, on top of the usual chrome"
        );
    }

    // ------------------------------------------------------------ cycling

    #[test]
    fn cycle_option_moves_between_all_and_each_candidate_without_wrapping() {
        let candidates = vec!["bugfix".to_string(), "default".to_string()];
        let mut current = None;
        current = cycle_option(&candidates, &current, true);
        assert_eq!(current.as_deref(), Some("bugfix"));
        current = cycle_option(&candidates, &current, true);
        assert_eq!(current.as_deref(), Some("default"));
        // Past the last candidate, `→` does nothing.
        current = cycle_option(&candidates, &current, true);
        assert_eq!(current.as_deref(), Some("default"));

        current = cycle_option(&candidates, &current, false);
        assert_eq!(current.as_deref(), Some("bugfix"));
        current = cycle_option(&candidates, &current, false);
        assert_eq!(current, None, "back past the first candidate is `all`");
        // `←` on `all` itself does nothing.
        current = cycle_option(&candidates, &current, false);
        assert_eq!(current, None);
    }

    /// The top border's right side names the project and, once one is set,
    /// the window — never the limit, which no row of the panel can move any
    /// more.
    #[test]
    fn filters_label_names_the_window_but_never_the_limit() {
        let loaded = loaded(Vec::new());
        let filters = Filters {
            since: "2026-08-01".to_string(),
            ..no_filters()
        };
        let label = filters_label(&loaded, &filters);
        assert_eq!(label, "demo · 2026-08-01 → now");
        assert!(!label.contains("limit"), "{label}");
    }

    // ---------------------------------------------------------- pipelines

    /// The mockup's pipelines view opens straight on its first block key,
    /// `Pipeline: default` — no `pipelines` heading above it, because the
    /// frame's own top border already names the view.
    #[test]
    fn pipelines_lines_opens_on_the_first_block_key() {
        let entries = vec![entry("implement", "v1")];
        let refs: Vec<&Entry> = entries.iter().collect();
        let loaded = loaded(entries.clone());
        let lines = pipelines_lines(&refs, &loaded, &no_filters());
        match &lines[0] {
            Line::Text(text) => assert_eq!(text, "Pipeline: default"),
            Line::Row(_) => panic!("the first line should be the `Pipeline: default` block key"),
        }
    }

    // --------------------------------------------------------------- steps

    #[test]
    fn ordered_steps_follows_the_pipeline_s_own_declared_order() {
        let entries = vec![entry("review", "v1"), entry("implement", "v1")];
        let pipelines = Pipelines::builtin();
        let order = ordered_steps(&entries, "default", &pipelines);
        // `default`'s own pipeline runs `implement` before `review`, even
        // though the ledger lines above arrived in the opposite order.
        assert_eq!(order, vec!["implement".to_string(), "review".to_string()]);
    }

    #[test]
    fn ordered_steps_appends_a_step_the_pipeline_no_longer_names() {
        let entries = vec![entry("retired-step", "v1")];
        let pipelines = Pipelines::builtin();
        let order = ordered_steps(&entries, "default", &pipelines);
        assert_eq!(order, vec!["retired-step".to_string()]);
    }

    /// A step's row mixes every version in the window rather than reading
    /// the newest against the one before it: `implement` ran once under
    /// `v1` (`pass`) and once under `v2` (`fail`), and its row here is both
    /// lanes together, not `v2`'s lane alone.
    #[test]
    fn step_blocks_aggregates_every_version_in_the_window() {
        let repo = fixture("step-blocks");
        bank(
            &repo,
            "2026-08-01T09:00:00+00:00",
            "a",
            "implement",
            "default",
            "qwen",
            "v1",
            1.0,
            Some("pass"),
        );
        bank(
            &repo,
            "2026-08-05T09:00:00+00:00",
            "b",
            "implement",
            "default",
            "qwen",
            "v2",
            2.0,
            Some("fail"),
        );
        let entries = crate::usage::read(&repo).unwrap();
        let fallback = fallback_keys(&entries);
        let pipelines = Pipelines::builtin();
        let blocks = step_blocks(&entries, &fallback, &BTreeMap::new(), &pipelines);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].pipeline, "default");
        assert_eq!(blocks[0].rows.len(), 1, "only `implement` ever ran");
        assert_eq!(blocks[0].rows[0].step, "implement");
        assert_eq!(blocks[0].rows[0].metrics.tasks, 2, "both runs count");
        assert_eq!(blocks[0].rows[0].metrics.pass_share(), Some(50.0));
    }

    /// The mirror of `pipelines_lines_opens_on_the_first_block_key`: the
    /// steps view's own block key reads `Pipeline: <name>` too, not a bare
    /// pipeline name or a fingerprint.
    #[test]
    fn steps_lines_opens_on_the_pipeline_block_key() {
        let entries = vec![entry("implement", "v1")];
        let refs: Vec<&Entry> = entries.iter().collect();
        let loaded = loaded(entries.clone());
        let pipelines = Pipelines::builtin();
        let lines = steps_lines(&refs, &loaded, &pipelines);
        match &lines[0] {
            Line::Text(text) => assert_eq!(text, "Pipeline: default"),
            Line::Row(_) => panic!("the first line should be the `Pipeline: default` block key"),
        }
    }

    /// Two pipelines on screen together, one with a step name longer than
    /// `STEP` itself (`reproduce-again`, from the task's own mockup) — both
    /// blocks' `STEP` columns must be measured from that longest name, the
    /// same width in both, not each block measuring its own.
    #[test]
    fn steps_lines_measures_step_width_from_the_longest_name_on_screen() {
        let mut long_step = entry("reproduce-again", "v1");
        long_step.pipeline = "bugfix".to_string();
        let entries = vec![entry("implement", "v1"), long_step];
        let refs: Vec<&Entry> = entries.iter().collect();
        let loaded = loaded(entries.clone());
        let pipelines = Pipelines::builtin();
        let lines = steps_lines(&refs, &loaded, &pipelines);

        let headers: Vec<&str> = lines
            .iter()
            .filter_map(|l| match l {
                Line::Text(t) if t.starts_with("STEP") => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers.len(), 2, "one header per block");
        assert_eq!(
            headers[0], headers[1],
            "both blocks' headers should share the width the longest step name on screen sets"
        );
        let padded_step = format!("{:<w$}", "STEP", w = "reproduce-again".len());
        assert!(
            headers[0].starts_with(&padded_step),
            "header should be padded out to `reproduce-again`'s own width: {:?}",
            headers[0]
        );
    }

    // ------------------------------------------------------------- render

    #[test]
    fn render_lines_marks_the_cursor_s_row() {
        let lines = vec![
            Line::Text("default".to_string()),
            Line::Row(Row {
                text: "v1".to_string(),
            }),
            Line::Row(Row {
                text: "v0".to_string(),
            }),
        ];
        let body = render_lines(&lines, 1, 20);
        assert!(body[1].starts_with(" v1"), "{:?}", body[1]);
        assert!(body[2].starts_with(">v0"), "{:?}", body[2]);
    }

    #[test]
    fn render_lines_clamps_a_cursor_past_the_last_row() {
        let lines = vec![Line::Row(Row {
            text: "only".to_string(),
        })];
        let body = render_lines(&lines, 99, 10);
        assert!(body[0].starts_with(">"));
    }

    // -------------------------------------------------------------- export

    #[test]
    fn export_writes_the_rows_on_screen_to_dot_spoolway_evals() {
        let repo = fixture("export-writes");
        bank(
            &repo,
            "2026-08-01T09:00:00+00:00",
            "a",
            "implement",
            "default",
            "qwen",
            "v1",
            1.5,
            Some("pass"),
        );
        let filters = no_filters();
        let loaded = load(&repo, &filters).unwrap();
        let pipelines = Pipelines::builtin();
        let (path, rows) = export(&repo, &loaded, &filters, &pipelines, View::Pipelines).unwrap();

        assert_eq!(rows, 1);
        assert!(path.starts_with(repo.root.join(".spoolway").join("evals")));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("project,pipeline,version,since,tasks,lanes,"));
        assert!(body.contains(",default,v1,2026-08-01,1,1,"));
    }

    // --------------------------------------------------------- run_screen

    /// Bare, with an empty ledger: the screen says so rather than drawing an
    /// empty table, and ends the moment the (empty) input runs out instead of
    /// hanging — the same degrade-not-crash guarantee the queue screen gives
    /// with no plans to show.
    #[test]
    fn an_empty_ledger_says_so_and_the_screen_ends_when_input_runs_out() {
        let repo = fixture("screen-empty");
        let text = screen(&repo, "");
        assert!(text.contains("Nothing to compare"), "{text}");
    }

    /// `tab` cycles through all four views without panicking on any of
    /// them, and `q` ends the screen from browsing mode.
    #[test]
    fn tab_cycles_every_view_and_q_quits() {
        let repo = fixture_with_one_run("screen-tab");
        let text = screen(&repo, "\t\t\t\tq");
        for label in ["pipelines", "steps", "runs", "skills"] {
            assert!(text.contains(&format!("eval · {label}")), "{label}\n{text}");
        }
    }

    /// A lane nothing could price prints the "Cost is a floor" note between
    /// the frame's own bottom border and the keys line — never inside the
    /// frame, where it would cost the table a body row, per the task's own
    /// non-goal.
    #[test]
    fn the_screen_prints_the_unpriced_note_under_the_frame_not_inside_it() {
        let repo = fixture("screen-unpriced-note");
        crate::usage::append(
            &repo,
            &Entry {
                ts: "2026-08-01T09:00:00+00:00".into(),
                task: "a".into(),
                plan: None,
                step: "implement".into(),
                pipeline: "default".into(),
                agent: "pi".into(),
                kind: "pi".into(),
                model: "some-local-model".into(),
                session: "s1".into(),
                round: 1,
                wall_s: 60,
                turns: 1,
                tokens: crate::usage::Tokens {
                    input: 10,
                    ..Default::default()
                },
                cost_usd: None,
                ctx_peak: None,
                version: Some("v1".into()),
                commit: Some("commit-v1".into()),
                outcome: Some("pass".into()),
                run: Some("r-a".into()),
                trial: None,
                skill: None,
                project: String::new(),
            },
        )
        .unwrap();

        let text = screen(&repo, "q");
        let last = last_frame(&text);
        let note = "Cost is a floor — no price configured for: some-local-model";
        assert!(last.contains(note), "{last}");

        let border = last.rfind('└').expect("the frame's own bottom border");
        let note_at = last.find(note).unwrap();
        let footer_at = last.find(FOOTER).expect("the keys line");
        assert!(
            border < note_at && note_at < footer_at,
            "the note must sit between the frame's bottom border and the keys line\n{last}"
        );
    }

    /// `e` exports whichever view is on screen, and the confirmation panel
    /// names the file it wrote — `q` after it (any key dismisses the notice,
    /// and this input has nothing further) ends the screen the same way it
    /// would from browsing.
    #[test]
    fn e_exports_the_current_view_and_names_the_file_it_wrote() {
        let repo = fixture_with_one_run("screen-export-key");
        let text = screen(&repo, "eq");
        assert!(text.contains("the pipelines view"), "{text}");
        assert!(text.contains("exported"), "the panel's own title\n{text}");
        // Spelled with the platform's own separator — the panel shows a
        // relative `PathBuf`, which is `\` on Windows.
        let shown = std::path::PathBuf::from(".spoolway")
            .join("evals")
            .join("eval-");
        assert!(
            text.contains(&shown.display().to_string()),
            "the path named is relative to the project root, not absolute\n{text}"
        );

        let dir = repo.root.join(".spoolway").join("evals");
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1, "exactly one export was written");
    }

    /// Nothing on the filter panel is typed into any more, so backspace has
    /// no row left to erase from at all: it must fall through the catch-all
    /// arm and do nothing, never reach `field_text_mut`'s `unreachable!()`.
    /// Regression for the panic this used to be reachable through, back when
    /// `pipeline` (the cursor's own default row) still guarded that call
    /// with `is_field_text` rather than having no typing arm to guard.
    #[test]
    fn backspace_on_the_filter_panel_does_nothing_rather_than_panicking() {
        let repo = fixture("screen-backspace");
        screen(&repo, "f\x7fq");
    }

    /// A one-version ledger draws a table only a few lines tall — shorter
    /// than either overlay panel. `draw` has to pad the frame's own body out
    /// to fit whichever panel is on top of it, or `overlay` writes past the
    /// frame's last row and the panel loses its own bottom border along
    /// with whatever line sat past it — reproduced exactly this way by the
    /// review that found it.
    #[test]
    fn a_short_table_still_fits_the_taller_overlay_panels_whole() {
        let repo = fixture_with_one_run("screen-short-table");

        // The filter panel: its own bottom border and its key line, naming
        // `esc back`, both survive.
        let text = screen(&repo, "fq");
        assert!(text.contains("esc back"), "{text}");
        assert!(text.contains("└"), "the panel's own bottom border\n{text}");

        // The export panel: its "any key to continue" line, past the two
        // lines of message, survives too.
        let text = screen(&repo, "eq");
        assert!(text.contains("any key to continue"), "{text}");
    }

    /// The panel draws exactly the four rows the task's own acceptance
    /// criteria name, in order, and neither date row carries the `‹ ›`
    /// chevron a cycled row does — the visible sign that `←`/`→` do nothing
    /// there any more.
    #[test]
    fn the_filter_panel_draws_exactly_four_rows_with_no_chevrons_on_the_date_rows() {
        assert_eq!(
            FILTER_FIELDS,
            [
                FilterField::Pipeline,
                FilterField::Step,
                FilterField::Since,
                FilterField::Until,
            ]
        );

        let draft = Draft {
            field: 0,
            filters: no_filters(),
        };
        let panel = filter_panel(&draft).join("\n");
        for label in ["pipeline", "step", "since", "until"] {
            assert!(panel.contains(label), "{label}\n{panel}");
        }
        assert!(
            !panel.contains("scope") && !panel.contains("limit"),
            "the scope and limit rows must be gone from the panel\n{panel}"
        );
        assert!(
            !panel.contains("since    ‹") && !panel.contains("until    ‹"),
            "a date row must carry no chevron\n{panel}"
        );
    }

    /// Regression: `FILTER_DATE_KEYS` is shorter than `FILTER_APPLY_KEYS`,
    /// so moving the cursor onto `since` or `until` used to make that
    /// shorter line the panel's own widest, shrinking the whole box and
    /// shifting the table behind it — then growing back the moment the
    /// cursor left again. The box must draw the same width on every row.
    #[test]
    fn the_filter_panel_is_the_same_width_on_every_row() {
        let base = no_filters();
        let widths: Vec<usize> = (0..FILTER_FIELDS.len())
            .map(|field| {
                let draft = Draft {
                    field,
                    filters: base.clone(),
                };
                filter_panel(&draft)[0].chars().count()
            })
            .collect();
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "every row must draw the same width: {widths:?}"
        );
    }

    /// A calendar is taller than the filter panel it replaces — fourteen
    /// rows against the panel's ten — so a table too short for the panel is
    /// shorter still against the calendar. Its own bottom border and key
    /// line must survive the same way the filter and export panels already
    /// do in `a_short_table_still_fits_the_taller_overlay_panels_whole`.
    #[test]
    fn a_short_table_still_fits_the_calendars_whole_height_key_line_included() {
        let repo = fixture_with_one_run("screen-short-table-calendar");

        // `f`, down twice onto `since`, `enter` to open its calendar.
        let text = screen(&repo, "f\x1b[B\x1b[B\rq");
        let last = last_frame(&text);
        assert!(
            last.contains("esc back"),
            "the calendar's own key line\n{last}"
        );
        assert!(
            last.contains("└"),
            "the calendar's own bottom border\n{last}"
        );
    }

    /// `esc` in the calendar hands the row back exactly as it was; `enter`
    /// there picks the day it opened on (today, since the row was blank) and
    /// writes it in; `x` on the row after clears it back to blank. One test
    /// covers the three because they share the same round trip through
    /// `Mode::Calendar` and back.
    #[test]
    fn esc_leaves_the_row_untouched_enter_picks_the_day_x_clears_it() {
        let repo = fixture("screen-calendar-roundtrip");
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();

        // `f`, down twice onto `since`, `enter` opens the calendar, `esc`
        // returns — the row must still read blank. No trailing `q`: a bare
        // `Esc`'s own lookahead read would silently eat it (there being
        // nothing real behind it to find instead), so the input running out
        // is what ends the loop here, the same way it does with no input at
        // all.
        let text = screen(&repo, "f\x1b[B\x1b[B\r\x1b");
        let last = last_frame(&text);
        assert!(
            last.contains("(blank — the start)"),
            "esc must leave the row untouched\n{last}"
        );

        // The same path, but `enter` inside the calendar this time: the row
        // now names today.
        let text = screen(&repo, "f\x1b[B\x1b[B\r\rq");
        let last = last_frame(&text);
        assert!(
            last.contains(&today),
            "enter in the calendar must pick the day it opened on\n{last}"
        );

        // And `x` instead of `enter` clears it straight back to blank.
        let text = screen(&repo, "f\x1b[B\x1b[B\rxq");
        let last = last_frame(&text);
        assert!(
            last.contains("(blank — the start)"),
            "x must clear the bound back to none\n{last}"
        );
    }

    /// The filter panel: `f` opens it, `→` on the `pipeline` row moves off
    /// "all" onto a real name, and `enter` applies it — narrowing the frame
    /// to that one pipeline, which the top border then names.
    #[test]
    fn the_filter_panel_narrows_the_view_once_applied() {
        let repo = fixture_with_one_run("screen-filter");
        bank(
            &repo,
            "2026-08-02T09:00:00+00:00",
            "b",
            "implement",
            "other",
            "qwen",
            "v1",
            1.0,
            Some("pass"),
        );
        // `f`, `→` to pick the first real pipeline off "all", `enter` to
        // apply, `q` to leave.
        let text = screen(&repo, "f\x1b[C\rq");
        let last = last_frame(&text);
        assert!(last.contains("pipeline default"), "{last}");
        assert!(
            !last.contains("other"),
            "the other pipeline is filtered out of the frame that applied it\n{last}"
        );
    }

    /// Every draw opens on a clear-screen, so a whole captured transcript
    /// holds every frame the screen ever drew, back to back — a plain
    /// `text.contains(...)` over it can true positive on a state that has
    /// since moved on. This is the frame that was actually on screen when
    /// the input ran out, for a test that means "the final state" and not
    /// "at some point during the session".
    fn last_frame(text: &str) -> &str {
        text.rsplit("\x1b[2J\x1b[H").next().unwrap_or(text)
    }

    /// Nothing on the date rows is typed into any more: a run of ordinary
    /// characters on the `since` row does not touch its value, and `enter`
    /// still opens the calendar rather than applying whatever a stray key
    /// run would have left behind. Regression for the panel's own typed
    /// `since`/`until` fields, which this task's own acceptance criteria
    /// retire in favour of the calendar.
    #[test]
    fn typing_on_a_date_row_does_nothing_and_enter_still_opens_the_calendar() {
        let repo = fixture("screen-bad-since");
        // `f`, down twice (`pipeline` → `step` → `since`), a run of ordinary
        // characters, `enter`, `q`.
        let text = screen(&repo, "f\x1b[B\x1b[Bnotadate\rq");
        let last = last_frame(&text);
        assert!(
            last.contains("┌─ since ─"),
            "enter on the since row must open its calendar\n{last}"
        );
        assert!(
            !text.contains("notadate"),
            "no key press may append a character to a filter value\n{text}"
        );
    }

    /// A notice's own overlay must never draw wider than the frame it sits
    /// on — `overlay` cannot place a panel that does not fit, and falls back
    /// to `x = 0`, drawing the panel's border over the frame's own left one
    /// and silently dropping every character past the right edge. This is
    /// the exact message the review's own repro produced, typing an
    /// incomplete `--since` — one long sentence with no newline of its own
    /// — checked against the review's own 83-column terminal and against
    /// the frame's narrowest floor.
    #[test]
    fn a_long_notice_wraps_to_fit_the_frame_at_any_width() {
        let err = crate::spend::window_of(None, Some("2026-08-0"), None).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.chars().count() > 60,
            "the repro needs a message long enough to actually overflow a narrow frame: {message:?}"
        );

        for width in [MIN_WIDTH, 83, FALLBACK_WIDTH] {
            let lines = wrap_notice(&message, width);
            for line in &lines {
                assert!(
                    line.chars().count() <= width,
                    "at width {width}, {line:?} ({} chars) still overflows the wrap width",
                    line.chars().count()
                );
            }
            let rendered = panel("eval", &lines, "any key to continue");
            for row in &rendered {
                assert!(
                    row.chars().count() <= width + 2,
                    "at width {width}, the panel itself is wider than the frame ({} > {}): \
                     {row:?}",
                    row.chars().count(),
                    width + 2
                );
            }
        }
    }
}
