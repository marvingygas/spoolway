//! What the lanes cost to run, grouped one way at a time, beside what the
//! watched directories cost outside them.
//!
//! This is a **view over the ledger**, not a subsystem: every figure here is
//! grouped out of the same `usage.jsonl` that [`crate::spend`] reads, with
//! nothing kept in a second store.
//!
//! There are two tables, because the ledger holds two populations that never
//! share a row — see [`Entry::is_lane`] and [`Entry::dir`]. The lanes table
//! groups dispatched lanes by one [`EvalBy`]; the directory table groups the
//! sessions a person ran by hand in a watched root, by directory or one row
//! per session. Under every `by` only the columns naming a row change: the
//! figure columns stay put, so two groupings can be read against each other
//! without relearning where anything is. A `Total` line closes each table
//! with only what adds up across its rows — a per-run average or a peak
//! summed over rows would be a number nobody could use.
//!
//! The honest limit is worth stating where the code is, not only in the help.
//! Tasks differ in difficulty, so a pipeline that happens to have drawn easy
//! work looks better than one that drew hard work, and no statistic in here
//! corrects for that. What the view can do is show the sample size beside
//! every figure, which is why `RUNS` is a column and not a footnote: a reader
//! who sees a `PASS` of 79% next to a `RUNS` of two can discount it
//! themselves, and will do it better than a threshold could.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result, bail};

use crate::cli::{EvalArgs, EvalBy};
use crate::fmt::csv_field;
use crate::pipeline::Pipelines;
use crate::repo::Repo;
use crate::screen::{Key, PollableRead, boxed, key_hint, overlay, pad_to, panel, read_key};
use crate::usage::{Entry, ModelPrice, Tokens};

/// The printing path: every `spoolway eval` with a flag of its own. The
/// same lanes table the screen draws, without the frame — or its rows as
/// CSV or JSON. `pipelines` orders `--by step`'s rows in each pipeline's own
/// walk order, and is `None` when the project's pipelines do not load: the
/// ledger is still worth reading then, with steps falling back to name order.
pub fn run(repo: &Repo, args: &EvalArgs, json: bool, pipelines: Option<&Pipelines>) -> Result<()> {
    if json && args.csv {
        bail!("`--csv` and `--json` are two different exports of the same rows — pick one");
    }

    // `--month` is a deprecated alias: the calendar-month spend table moved
    // to `spoolway spend`, and this still routes there so a script or skill
    // written before the split keeps working. `--by` used to be the other
    // half of that alias; it is the lanes table's own grouping now.
    if let Some(month) = &args.month {
        eprintln!(
            "`eval --month` has moved to `spoolway spend --month` — this still works, but switch \
             when you can."
        );
        let filters = crate::spend::Filters {
            since: None,
            until: None,
            month: Some(month.as_str()),
            all: args.all,
            project: args.project.as_deref(),
        };
        return crate::spend::print(repo, None, &filters, json, args.csv);
    }

    // Reading is also what catches the ledger up — see `usage::sweep`, and
    // `spend::print`, which sweeps for the same reason.
    crate::usage::sweep(repo);

    let (mut entries, scope) =
        crate::spend::collect_scoped(repo, args.project.as_deref(), args.all)?;

    // An interactive line is a conversation, not a lane: it reports no
    // outcome and belongs to no task, so counting it would put a session in
    // the table that nothing in the table's columns describes. A directory
    // line is the other table's population, never this one's. See
    // `Entry::is_lane`.
    entries.retain(Entry::is_lane);

    // Derived from the whole scope, before the window and the filters below
    // narrow `entries` further — so a historical run's id is the same key
    // under every filter over this project.
    let fallback = fallback_keys(&entries);

    let window = crate::spend::window_of(None, args.since.as_deref(), args.until.as_deref())?;
    entries.retain(|entry| window.contains(&entry.ts));

    if let Some(step) = &args.step {
        let before = entries.len();
        entries.retain(|entry| &entry.step == step);
        if entries.is_empty() && before > 0 {
            bail!("no lanes ran step `{step}` in that window");
        }
    }

    let lane_filters = LaneFilters {
        group: args.group.as_deref(),
        task: args.task.as_deref(),
        pipeline: args.pipeline.as_deref(),
        step: None,
        version: args.pipeline_version.as_deref(),
    };
    entries.retain(|entry| lane_filters.admits(entry));
    if let Some(trial) = &args.trial {
        entries.retain(|entry| entry.trial.as_deref() == Some(trial.as_str()));
    }

    if entries.is_empty() {
        if args.trial.is_some() {
            println!("No runs recorded for that trial yet.");
            return Ok(());
        }
        println!("Nothing to compare in {} yet.", scope.what);
        println!(
            "A lane is recorded when a step finishes, so this fills up as `spoolway \
             dispatch` runs."
        );
        return Ok(());
    }

    entries.sort_by(|a, b| a.ts.cmp(&b.ts));
    let refs: Vec<&Entry> = entries.iter().collect();
    let mut rows = lane_rows(&refs, args.by, &fallback, &repo.config.models, pipelines);
    // A trial's arms read against the first one, so the first one has to be
    // the arm that started first — not whichever finished last, which is
    // what the newest-first order every other `--by task` reads in would put
    // on top.
    if args.trial.is_some() && args.by == EvalBy::Task {
        rows.sort_by(|a, b| a.first_ts.cmp(&b.first_ts));
    }
    let total = LaneTotal::of(&refs, &fallback);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&lanes_json(args.by, &rows, &total))?
        );
        return Ok(());
    }
    if args.csv {
        println!("{}", lanes_csv_header(args.by));
        for row in &rows {
            println!("{}", lanes_csv_row(args.by, row));
        }
        println!("{}", lanes_csv_total(args.by, &total));
        return Ok(());
    }

    let table = lanes_table(args.by, &rows, &total);
    println!("{}", dim(&table.header));
    for row in &table.rows {
        println!("{row}");
    }
    println!("{}", table.total);

    // `--trial` asks the question a trial exists to answer: its own
    // comparison, not just a narrower version of the same table. Only under
    // `--by task`, where one row is one arm; under any other `by` a row
    // mixes arms and there is nothing to subtract.
    if args.trial.is_some() && args.by == EvalBy::Task && rows.len() > 1 {
        println!();
        let baseline = &rows[0];
        for row in &rows[1..] {
            println!("{}", trial_delta_line(baseline, row));
        }
    }

    if let Some(note) = unpriced_note(entries.iter()) {
        println!("\n{note}");
    }
    Ok(())
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

/// What makes a ledger line one lane: its task, step, round and session.
type LaneKey = (String, String, u32, String);

fn lane_key(entry: &Entry) -> LaneKey {
    (
        entry.task.clone(),
        entry.step.clone(),
        entry.round,
        entry.session.clone(),
    )
}

/// One ledger line per lane launched is not one lane: a lane held for a
/// person and later freed banks a second line under the same task, step,
/// round and session — see gh-378 / issue #380, and [`crate::spend`]'s own
/// `Totals::add`, which groups the same way. Tokens, cost and `wall_s` are
/// all banked as deltas, so a plain sum over every line already lands on
/// each lane's real total whether it wrote one line or two — it is only a
/// *count* of lanes, and their verdicts, that double-counts a line as a
/// lane. This answers with one verdict per group instead: `None` until a
/// line in it actually reports one, and the last such line's outcome from
/// then on — a lane resumed and reported again is judged by what it said
/// last, not by every line it ever banked.
fn lane_verdicts<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
) -> HashMap<LaneKey, Option<String>> {
    let mut verdicts: HashMap<LaneKey, Option<String>> = HashMap::new();
    for entry in entries {
        let slot = verdicts.entry(lane_key(entry)).or_insert(None);
        if entry.outcome.is_some() {
            *slot = entry.outcome.clone();
        }
    }
    verdicts
}

// ------------------------------------------------------------------ metrics

/// What one row of the lanes table came to: every lane the row stands for.
#[derive(Default, Clone)]
struct Metrics {
    /// Distinct runs that touched this row — any lane of theirs matched, not
    /// necessarily every one.
    runs: usize,
    /// Distinct lanes on this row — see [`lane_verdicts`] for why that is
    /// not the count of its ledger lines.
    lanes: usize,
    /// Lanes that reported `pass`, and lanes anything is known about. A lane
    /// that never reported — killed, or silent — is left out of both: it is
    /// not a failure, it is a lane nobody heard from.
    passed: usize,
    judged: usize,
    /// Lanes that ended blocked.
    blocked: usize,
    /// Every token class, summed over the row's lines — deltas, like cost.
    tokens: Tokens,
    cost: f64,
    /// Ledger lines behind `cost`, and those among them nothing could price.
    /// Counted in lines rather than lanes because a price is missing from a
    /// line, and a held lane's two lines can differ in whether they have one.
    lines: usize,
    unpriced: usize,
    /// Every lane's own busy time, summed — see [`Entry::wall_s`].
    time_s: i64,
    /// The largest `ctx_peak` any lane on this row banked, and that peak as a
    /// share of its model's window. See [`ctx_peak_of`].
    ctx_peak_tokens: Option<u64>,
    ctx_peak_pct: Option<f64>,
    /// The mean of every lane's own peak. See [`ctx_avg_of`].
    ctx_avg: CtxAvg,
    /// Every `pipeline_version` the row's lanes ran under — one where the row
    /// sits inside a single version, which is when an export names it.
    versions: BTreeSet<String>,
}

impl Metrics {
    fn for_matching(
        matching: &[&Entry],
        fallback: &HashMap<(String, String), String>,
        models: &BTreeMap<String, ModelPrice>,
    ) -> Self {
        let mut out = Metrics::default();
        let mut runs: HashSet<(String, String)> = HashSet::new();
        let mut by_lane: HashMap<LaneKey, Vec<&Entry>> = HashMap::new();
        for entry in matching {
            // Tokens, cost and busy time are all banked as deltas, so a
            // plain sum here already lands on each lane's real total
            // whether it wrote one ledger line or two.
            out.time_s += entry.wall_s;
            out.tokens.add(&entry.tokens);
            out.lines += 1;
            match entry.cost_usd {
                Some(cost) => out.cost += cost,
                // A zero-token line spends nothing, so it must not turn this
                // row's total into a floor — see `unpriced_note`.
                None if entry.tokens.is_zero() => {}
                None => out.unpriced += 1,
            }
            runs.insert((entry.project.clone(), run_key(entry, fallback)));
            out.versions.insert(entry.pipeline_version.clone());
            by_lane.entry(lane_key(entry)).or_default().push(entry);
        }
        // One verdict per lane, not per line — see `lane_verdicts`.
        let verdicts = lane_verdicts(matching.iter().copied());
        out.lanes = verdicts.len();
        for outcome in verdicts.values() {
            if outcome.as_deref() == Some("block") {
                out.blocked += 1;
            }
            if let Some(outcome) = outcome {
                out.judged += 1;
                if outcome == "pass" {
                    out.passed += 1;
                }
            }
        }
        out.runs = runs.len();
        (out.ctx_peak_tokens, out.ctx_peak_pct) = ctx_peak_of(matching, models);
        out.ctx_avg = ctx_avg_of(by_lane.values(), models);
        out
    }

    fn pass_share(&self) -> Option<f64> {
        (self.judged > 0).then(|| 100.0 * self.passed as f64 / self.judged as f64)
    }

    /// `value` over this row's runs — `0.0` rather than a division by zero
    /// for the empty default a test scaffolds with; every row drawn has at
    /// least one run.
    fn per_run(&self, value: f64) -> f64 {
        match self.runs {
            0 => 0.0,
            n => value / n as f64,
        }
    }

    fn tokens_per_run(&self, n: u64) -> u64 {
        self.per_run(n as f64).round() as u64
    }

    fn cost_per_run(&self) -> f64 {
        self.per_run(self.cost)
    }

    fn time_per_run_s(&self) -> f64 {
        self.per_run(self.time_s as f64)
    }

    /// The row's version where every lane on it ran under one, blank where
    /// they ran under several — a row by step spans every version its
    /// pipeline has had, and naming one of them would be a guess.
    fn single_version(&self) -> Option<&str> {
        match self.versions.len() {
            1 => self.versions.iter().next().map(String::as_str),
            _ => None,
        }
    }
}

/// The mean of a set of peaks: in raw tokens over every lane that banked
/// one, and as a share of each lane's own model's window over every lane
/// whose model resolves to one. Two means rather than one because a window
/// can be unset for some models and not others — the share is then a mean
/// over fewer lanes, and the raw figure is what a cell falls back to when
/// there is no share at all, the same rule [`ctx_cell`] follows for a peak.
#[derive(Default, Clone, Copy)]
struct CtxAvg {
    tokens: Option<f64>,
    pct: Option<f64>,
}

impl CtxAvg {
    fn cell(&self) -> String {
        ctx_cell(self.tokens.map(|t| t.round() as u64), self.pct)
    }
}

/// `CTX PEAK AVG`: each group's own peak — a lane's, or a session's — taken
/// through [`ctx_peak_of`], then averaged. Where `CTX PEAK` says how close
/// the worst lane came to its window, this says how close a typical one did.
fn ctx_avg_of<'a, 'b: 'a>(
    groups: impl IntoIterator<Item = &'a Vec<&'b Entry>>,
    models: &BTreeMap<String, ModelPrice>,
) -> CtxAvg {
    let mut tokens: Vec<f64> = Vec::new();
    let mut pcts: Vec<f64> = Vec::new();
    for group in groups {
        let (peak, pct) = ctx_peak_of(group, models);
        if let Some(peak) = peak {
            tokens.push(peak as f64);
        }
        if let Some(pct) = pct {
            pcts.push(pct);
        }
    }
    let mean = |v: &[f64]| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    CtxAvg {
        tokens: mean(&tokens),
        pct: mean(&pcts),
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
/// rule an unpriced model's cost cell follows.
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

/// A token cell: [`crate::fmt::tokens_human`], plus a `B` step it does not
/// have. A session table's `Total` sums every cache read a directory ever
/// made, which passes a billion within weeks, and `1370.00M` reads as a
/// typo where `1.37B` reads as a number.
fn tokens_cell(n: u64) -> String {
    match n {
        n if n >= 1_000_000_000 => format!("{:.2}B", n as f64 / 1_000_000_000.0),
        n => crate::fmt::tokens_human(n),
    }
}

// ----------------------------------------------------------------- lane rows

/// Narrowing the lanes table: each `Some` keeps only the lanes that match
/// it. Shared by the printing path's flags and the screen's filter rows, so
/// `--pipeline-version 1.1` and the `version` row can never disagree about
/// what a version matches.
#[derive(Default)]
struct LaneFilters<'a> {
    group: Option<&'a str>,
    task: Option<&'a str>,
    pipeline: Option<&'a str>,
    step: Option<&'a str>,
    version: Option<&'a str>,
}

impl LaneFilters<'_> {
    fn admits(&self, e: &Entry) -> bool {
        self.group.is_none_or(|g| e.plan.as_deref() == Some(g))
            && self.task.is_none_or(|t| e.task == t)
            && self.pipeline.is_none_or(|p| e.pipeline == p)
            && self.step.is_none_or(|s| e.step == s)
            && self.version.is_none_or(|v| e.pipeline_version == v)
    }
}

/// One row of the lanes table under whichever `by` built it.
struct LaneRow {
    /// The columns naming the row, as drawn — see [`lane_headers`].
    cells: Vec<String>,
    /// The same row's naming columns as an export spells them — see
    /// [`lane_csv_keys`]. Apart from `cells` because an export carries the
    /// run id a screen has no room for, and raw values where the screen
    /// abbreviates.
    keys: Vec<String>,
    /// The project of the row's earliest lane. Shown rather than used to
    /// split rows — two projects sharing a pipeline name is a fact worth
    /// seeing, not a reason to fork the table.
    project: String,
    /// The row's earliest and latest lane.
    first_ts: String,
    last_ts: String,
    metrics: Metrics,
}

/// The columns naming a row under each `by` — the only part of the table
/// that changes with it.
fn lane_headers(by: EvalBy) -> &'static [&'static str] {
    match by {
        EvalBy::Group => &["GROUP"],
        EvalBy::Task => &["TASK", "PIPELINE", "VER", "WHEN"],
        EvalBy::Pipeline => &["PIPELINE"],
        EvalBy::Step => &["PIPELINE", "STEP"],
        EvalBy::Version => &["PIPELINE", "VERSION", "FIRST"],
    }
}

/// An export's naming columns under each `by`, between `project,by` and
/// `pipeline_version`. `--by version` names no `version` here because
/// `pipeline_version` beside it already is one.
fn lane_csv_keys(by: EvalBy) -> &'static [&'static str] {
    match by {
        EvalBy::Group => &["group"],
        EvalBy::Task => &["run", "task", "pipeline", "when"],
        EvalBy::Pipeline => &["pipeline"],
        EvalBy::Step => &["pipeline", "step"],
        EvalBy::Version => &["pipeline", "first"],
    }
}

/// What a lane without a `group:` groups under — a dash, the same "nothing
/// here" every other empty cell in this file reads as, rather than a blank
/// row name nobody could point at.
const NO_GROUP: &str = "—";

/// Every row `entries` makes under `by`, in the order the table reads.
///
/// Group, task and pipeline read newest first — whichever row's newest lane
/// is newest leads, so this morning's work heads the table. Step keeps each
/// pipeline together, newest pipeline first, its steps in their pipeline's
/// own walk order; version keeps each pipeline together the same way, its
/// highest version first.
fn lane_rows(
    entries: &[&Entry],
    by: EvalBy,
    fallback: &HashMap<(String, String), String>,
    models: &BTreeMap<String, ModelPrice>,
    pipelines: Option<&Pipelines>,
) -> Vec<LaneRow> {
    // Insertion-ordered grouping: `entries` arrive oldest first, so each
    // group's first line is its earliest and its last line its latest.
    let mut order: Vec<Vec<String>> = Vec::new();
    let mut groups: HashMap<Vec<String>, Vec<&Entry>> = HashMap::new();
    for entry in entries {
        let key = match by {
            EvalBy::Group => vec![entry.plan.clone().unwrap_or_else(|| NO_GROUP.to_string())],
            EvalBy::Task => vec![entry.project.clone(), run_key(entry, fallback)],
            EvalBy::Pipeline => vec![entry.pipeline.clone()],
            EvalBy::Step => vec![entry.pipeline.clone(), entry.step.clone()],
            EvalBy::Version => vec![entry.pipeline.clone(), entry.pipeline_version.clone()],
        };
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(entry);
    }

    let mut rows: Vec<(Vec<String>, LaneRow)> = order
        .into_iter()
        .map(|key| {
            let lanes = groups.remove(&key).unwrap_or_default();
            let first = lanes.first().expect("a group always has a line");
            let latest = lanes
                .iter()
                .max_by(|a, b| a.ts.cmp(&b.ts))
                .expect("a group always has a line");
            let earliest = lanes
                .iter()
                .min_by(|a, b| a.ts.cmp(&b.ts))
                .expect("a group always has a line");
            let metrics = Metrics::for_matching(&lanes, fallback, models);
            let (cells, keys) = match by {
                EvalBy::Group | EvalBy::Pipeline => (key.clone(), key.clone()),
                EvalBy::Step => (key.clone(), key.clone()),
                // A run's pipeline and version are its latest lane's: a run
                // re-routed mid-way is described by where it ended up.
                EvalBy::Task => (
                    vec![
                        latest.task.clone(),
                        latest.pipeline.clone(),
                        latest.pipeline_version.clone(),
                        local_date(&latest.ts),
                    ],
                    vec![
                        key[1].clone(),
                        latest.task.clone(),
                        latest.pipeline.clone(),
                        local_date(&latest.ts),
                    ],
                ),
                EvalBy::Version => (
                    vec![key[0].clone(), key[1].clone(), local_date(&earliest.ts)],
                    vec![key[0].clone(), local_date(&earliest.ts)],
                ),
            };
            let row = LaneRow {
                cells,
                keys,
                project: first.project.clone(),
                first_ts: earliest.ts.clone(),
                last_ts: latest.ts.clone(),
                metrics,
            };
            (key, row)
        })
        .collect();

    match by {
        EvalBy::Group | EvalBy::Task | EvalBy::Pipeline => {
            rows.sort_by(|a, b| b.1.last_ts.cmp(&a.1.last_ts));
        }
        EvalBy::Step | EvalBy::Version => {
            let pipeline_rank = newest_first_rank(entries, |e| e.pipeline.as_str());
            let step_rank: HashMap<(String, String), usize> = match by {
                EvalBy::Step => pipeline_rank
                    .keys()
                    .flat_map(|p| {
                        ordered_steps(entries, p, pipelines)
                            .into_iter()
                            .enumerate()
                            .map(|(i, s)| ((p.clone(), s), i))
                    })
                    .collect(),
                _ => HashMap::new(),
            };
            rows.sort_by(|(a, _), (b, _)| {
                let by_pipeline = pipeline_rank[&a[0]].cmp(&pipeline_rank[&b[0]]);
                by_pipeline.then_with(|| match by {
                    EvalBy::Step => step_rank
                        .get(&(a[0].clone(), a[1].clone()))
                        .cmp(&step_rank.get(&(b[0].clone(), b[1].clone()))),
                    _ => version_order(&b[1]).cmp(&version_order(&a[1])),
                })
            });
        }
    }
    rows.into_iter().map(|(_, row)| row).collect()
}

/// Each distinct `name` among `entries`, ranked `0` for the one whose newest
/// line is newest. `entries` must be sorted oldest first.
fn newest_first_rank<'a>(
    entries: &[&'a Entry],
    name: impl Fn(&'a Entry) -> &'a str,
) -> HashMap<String, usize> {
    let mut latest: HashMap<&str, &str> = HashMap::new();
    for entry in entries {
        latest.insert(name(entry), entry.ts.as_str());
    }
    let mut names: Vec<(&str, &str)> = latest.into_iter().collect();
    names.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    names
        .into_iter()
        .enumerate()
        .map(|(i, (n, _))| (n.to_string(), i))
        .collect()
}

/// A version's sort key: its `x.y` parts as numbers, so `1.10` sorts above
/// `1.9` — as text it would sort below. A part that is not a number sorts
/// below every one that is, rather than making the whole version unsortable;
/// the text itself breaks what ties remain.
fn version_order(version: &str) -> (Vec<Option<u64>>, String) {
    let parts = version.split('.').map(|p| p.parse::<u64>().ok()).collect();
    (parts, version.to_string())
}

/// Every step `pipeline` ran anywhere in `entries`, in the order its own
/// pipeline definition declares them — so the table reads in the order the
/// work happened, not alphabetically. A step the definition no longer names
/// (renamed, or removed, since some of these lines were banked) is appended
/// after, sorted, rather than dropped: the ledger still remembers it ran.
fn ordered_steps(entries: &[&Entry], pipeline: &str, pipelines: Option<&Pipelines>) -> Vec<String> {
    let present: BTreeSet<&str> = entries
        .iter()
        .filter(|e| e.pipeline == pipeline)
        .map(|e| e.step.as_str())
        .collect();

    let mut ordered: Vec<String> = Vec::new();
    if let Some(def) = pipelines.and_then(|p| p.get(pipeline).ok()) {
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

/// The only figures that add up across the lanes table's rows. `RUNS` is
/// distinct runs over the whole table, not a sum of the rows' — a run
/// touches every step it ran, so summing `--by step` would count it once per
/// step. `BLOCKS` and `USD` do sum: every lane sits on exactly one row.
struct LaneTotal {
    runs: usize,
    blocked: usize,
    cost: f64,
    lines: usize,
    unpriced: usize,
}

impl LaneTotal {
    fn of(entries: &[&Entry], fallback: &HashMap<(String, String), String>) -> LaneTotal {
        let runs: HashSet<(String, String)> = entries
            .iter()
            .map(|e| (e.project.clone(), run_key(e, fallback)))
            .collect();
        let blocked = lane_verdicts(entries.iter().copied())
            .values()
            .filter(|v| v.as_deref() == Some("block"))
            .count();
        LaneTotal {
            runs: runs.len(),
            blocked,
            cost: entries.iter().filter_map(|e| e.cost_usd).sum(),
            lines: entries.len(),
            unpriced: entries
                .iter()
                .filter(|e| e.cost_usd.is_none() && !e.tokens.is_zero())
                .count(),
        }
    }
}

// ------------------------------------------------------------- lanes table

/// The lanes table as plain text: its header, one line per row, and the
/// `Total` line — no marker column and no colour, so the screen can lay its
/// own frame over exactly these columns and the printing path can print
/// them as they are.
struct Table {
    header: String,
    rows: Vec<String>,
    total: String,
}

/// Separator between two columns everywhere in both tables.
const SEP: &str = "  ";

/// The figure columns, identical under every `by`. `RUNS` is drawn one wider
/// than its title, as the mockup draws it: the extra column is the gap that
/// sets the figures off from whichever naming column precedes them.
fn lane_figures(cells: [&str; 12]) -> String {
    let [
        runs,
        pass,
        blocks,
        avg,
        peak,
        inp,
        out,
        cr,
        cw,
        usd,
        usd_run,
        time,
    ] = cells;
    format!(
        "{SEP}{runs:>5}{SEP}{pass:>4}{SEP}{blocks:>6}{SEP}{avg:>12}{SEP}{peak:>8}{SEP}{inp:>6}\
         {SEP}{out:>7}{SEP}{cr:>11}{SEP}{cw:>11}{SEP}{usd:>8}{SEP}{usd_run:>7}{SEP}{time:>8}"
    )
}

/// `cells` padded to `widths`, joined with [`SEP`].
fn naming(cells: &[String], widths: &[usize]) -> String {
    cells
        .iter()
        .zip(widths)
        .map(|(cell, w)| format!("{cell:<w$}"))
        .collect::<Vec<_>>()
        .join(SEP)
}

/// Whether `rows` carry more than one distinct project — the test that
/// decides whether `PROJECT` earns a column at all. Decided over what is
/// actually on screen, not the wider scope `--all` may have pulled in.
fn spans_more_than_one_project(rows: &[LaneRow]) -> bool {
    rows.iter()
        .map(|r| r.project.as_str())
        .collect::<HashSet<_>>()
        .len()
        > 1
}

fn lanes_table(by: EvalBy, rows: &[LaneRow], total: &LaneTotal) -> Table {
    let show_project = spans_more_than_one_project(rows);
    let mut headers: Vec<String> = lane_headers(by).iter().map(|h| h.to_string()).collect();
    let mut body: Vec<Vec<String>> = rows.iter().map(|r| r.cells.clone()).collect();
    if show_project {
        headers.insert(0, "PROJECT".to_string());
        for (cells, row) in body.iter_mut().zip(rows) {
            cells.insert(0, row.project.clone());
        }
    }
    let mut total_cells = vec![String::new(); headers.len()];
    total_cells[0] = "Total".to_string();

    let widths: Vec<usize> = (0..headers.len())
        .map(|i| {
            std::iter::once(&headers[i])
                .chain(body.iter().map(|cells| &cells[i]))
                .chain(std::iter::once(&total_cells[i]))
                .map(|c| c.chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();

    let header = format!(
        "{}{}",
        naming(&headers, &widths),
        lane_figures([
            "RUNS",
            "PASS",
            "BLOCKS",
            "CTX PEAK AVG",
            "CTX PEAK",
            "IN/RUN",
            "OUT/RUN",
            "CACHE R/RUN",
            "CACHE W/RUN",
            "USD",
            "USD/RUN",
            "TIME/RUN",
        ])
    );
    let lines = body
        .iter()
        .zip(rows)
        .map(|(cells, row)| {
            let m = &row.metrics;
            format!(
                "{}{}",
                naming(cells, &widths),
                lane_figures([
                    &m.runs.to_string(),
                    &percent(m.pass_share()),
                    &m.blocked.to_string(),
                    &m.ctx_avg.cell(),
                    &ctx_cell(m.ctx_peak_tokens, m.ctx_peak_pct),
                    &tokens_cell(m.tokens_per_run(m.tokens.input)),
                    &tokens_cell(m.tokens_per_run(m.tokens.output)),
                    &tokens_cell(m.tokens_per_run(m.tokens.cache_read)),
                    &tokens_cell(m.tokens_per_run(m.tokens.cache_write())),
                    &cost_of(m.cost, m.lines, m.unpriced),
                    // Dividing a floor by its run count is still a floor,
                    // but `cost_of` prints the same plain number either way
                    // — the note under the table says once, for the whole
                    // thing, when any row's figure is an underestimate.
                    &cost_of(m.cost_per_run(), m.lines, m.unpriced),
                    &crate::status::human_secs(m.time_per_run_s().round() as i64),
                ])
            )
        })
        .collect();
    let total = format!(
        "{}{}",
        naming(&total_cells, &widths),
        lane_figures([
            &total.runs.to_string(),
            "",
            &total.blocked.to_string(),
            "",
            "",
            "",
            "",
            "",
            "",
            &cost_of(total.cost, total.lines, total.unpriced),
            "",
            "",
        ])
    )
    .trim_end()
    .to_string();
    Table {
        header,
        rows: lines,
        total,
    }
}

/// One arm against the trial's first arm: the sign says which way it moved,
/// never which one is "better" — a trial exists to let the reader decide
/// that, not to decide it for them.
fn trial_delta_line(baseline: &LaneRow, row: &LaneRow) -> String {
    let (base, now) = (&baseline.metrics, &row.metrics);
    let pass = match (base.pass_share(), now.pass_share()) {
        (Some(base), Some(now)) => format!("{:+.0}pp", now - base),
        _ => "n/a".to_string(),
    };
    let cost = now.cost - base.cost;
    let time = now.time_s - base.time_s;
    format!(
        "{} vs {}: pass {pass}, cost {}{:.2}, time {}{}",
        row.cells[0],
        baseline.cells[0],
        if cost >= 0.0 { "+$" } else { "-$" },
        cost.abs(),
        if time >= 0 { "+" } else { "-" },
        crate::status::human_secs(time.abs()),
    )
}

// ------------------------------------------------------------ lanes export

/// The one header a lanes export carries: `project,by`, the `by`'s own
/// naming columns, `pipeline_version`, then the figures — raw totals first,
/// and each token class per run beside them, so a spreadsheet can re-derive
/// the screen's figures or pick its own.
fn lanes_csv_header(by: EvalBy) -> String {
    let mut cols = vec!["project", "by"];
    cols.extend(lane_csv_keys(by));
    cols.extend([
        "pipeline_version",
        "runs",
        "lanes",
        "pass",
        "blocks",
        "ctx_peak_tokens",
        "ctx_peak_pct",
        "ctx_peak_avg_tokens",
        "ctx_peak_avg_pct",
        "in_tokens",
        "out_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "in_per_run",
        "out_per_run",
        "cache_read_per_run",
        "cache_write_per_run",
        "cost_usd",
        "cost_per_run",
        "unpriced",
        "time_s",
        "time_per_run_s",
    ]);
    cols.join(",")
}

fn opt_u64(v: Option<u64>) -> String {
    v.map_or(String::new(), |t| t.to_string())
}

fn opt_share(v: Option<f64>) -> String {
    v.map_or(String::new(), |p| format!("{p:.2}"))
}

fn lanes_csv_row(by: EvalBy, row: &LaneRow) -> String {
    let m = &row.metrics;
    let t = &m.tokens;
    let mut cells: Vec<String> = vec![csv_field(&row.project).into_owned(), by.label().to_string()];
    cells.extend(row.keys.iter().map(|k| csv_field(k).into_owned()));
    cells.extend([
        csv_field(m.single_version().unwrap_or_default()).into_owned(),
        m.runs.to_string(),
        m.lanes.to_string(),
        csv_fraction(m.pass_share()),
        m.blocked.to_string(),
        opt_u64(m.ctx_peak_tokens),
        opt_share(m.ctx_peak_pct),
        opt_u64(m.ctx_avg.tokens.map(|t| t.round() as u64)),
        opt_share(m.ctx_avg.pct),
        t.input.to_string(),
        t.output.to_string(),
        t.cache_read.to_string(),
        t.cache_write().to_string(),
        m.tokens_per_run(t.input).to_string(),
        m.tokens_per_run(t.output).to_string(),
        m.tokens_per_run(t.cache_read).to_string(),
        m.tokens_per_run(t.cache_write()).to_string(),
        csv_cost(m.cost, m.lines, m.unpriced),
        csv_cost(m.cost_per_run(), m.lines, m.unpriced),
        m.unpriced.to_string(),
        m.time_s.max(0).to_string(),
        (m.time_per_run_s().round() as i64).to_string(),
    ]);
    cells.join(",")
}

/// The `Total` line as an export writes it: `total` where every row names
/// its `by`, so a filter on that column can never read it as one more row,
/// and blank in every column that does not add up — the same three figures
/// the screen's own `Total` line carries, and nothing else.
fn lanes_csv_total(by: EvalBy, total: &LaneTotal) -> String {
    let header = lanes_csv_header(by);
    let names: Vec<&str> = header.split(',').collect();
    names
        .iter()
        .map(|name| match *name {
            "by" => "total".to_string(),
            "runs" => total.runs.to_string(),
            "blocks" => total.blocked.to_string(),
            "cost_usd" => csv_cost(total.cost, total.lines, total.unpriced),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// `--json`: the rows under `rows` and the `Total` line under `total`, never
/// in the same list — a consumer iterating the rows cannot meet it.
fn lanes_json(by: EvalBy, rows: &[LaneRow], total: &LaneTotal) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let m = &row.metrics;
            let t = &m.tokens;
            let priced = m.lines > m.unpriced;
            let mut obj = serde_json::Map::new();
            obj.insert("project".into(), row.project.clone().into());
            for (name, value) in lane_csv_keys(by).iter().zip(&row.keys) {
                obj.insert((*name).into(), value.clone().into());
            }
            let figures = serde_json::json!({
                "pipeline_version": m.single_version(),
                "runs": m.runs,
                "lanes": m.lanes,
                "pass": m.pass_share().map(|p| p / 100.0),
                "blocks": m.blocked,
                "ctx_peak_tokens": m.ctx_peak_tokens,
                "ctx_peak_pct": m.ctx_peak_pct,
                "ctx_peak_avg_tokens": m.ctx_avg.tokens,
                "ctx_peak_avg_pct": m.ctx_avg.pct,
                "in_tokens": t.input,
                "out_tokens": t.output,
                "cache_read_tokens": t.cache_read,
                "cache_write_tokens": t.cache_write(),
                "in_per_run": m.tokens_per_run(t.input),
                "out_per_run": m.tokens_per_run(t.output),
                "cache_read_per_run": m.tokens_per_run(t.cache_read),
                "cache_write_per_run": m.tokens_per_run(t.cache_write()),
                "cost_usd": priced.then_some(m.cost),
                "cost_per_run": priced.then_some(m.cost_per_run()),
                // How many lines are missing from `cost_usd` — 0 when it is
                // a real total, and the reason `cost_usd` above is `null`
                // rather than a number when every line is. A consumer cannot
                // otherwise tell a partly-priced floor from a complete total:
                // both are plain numbers.
                "unpriced": m.unpriced,
                "time_s": m.time_s,
                "time_per_run_s": m.time_per_run_s(),
            });
            if let serde_json::Value::Object(figures) = figures {
                obj.extend(figures);
            }
            serde_json::Value::Object(obj)
        })
        .collect();
    serde_json::json!({
        "by": by.label(),
        "rows": rows,
        "total": {
            "runs": total.runs,
            "blocks": total.blocked,
            "cost_usd": (total.lines > total.unpriced).then_some(total.cost),
        },
    })
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

/// A printed table's header, dimmed the way every table header here is.
fn dim(text: &str) -> String {
    paint(text, "2")
}

fn percent(value: Option<f64>) -> String {
    value.map_or("—".to_string(), |v| format!("{v:.0}%"))
}

fn csv_fraction(share: Option<f64>) -> String {
    match share {
        Some(pct) => format!("{:.2}", pct / 100.0),
        None => String::new(),
    }
}

/// A cost cell for `spoolway eval`'s own tables and screen: a plain number,
/// or `—` when nothing behind it could be priced at all. Whether it is a
/// floor (some, but not all, of `complete` priced) is not marked on the cell
/// itself — the `unpriced_note` printed under the table says that once for
/// the whole table, and a `+?` on every affected cell said the same thing a
/// second time in a shape a pasted table could not parse as a number.
fn cost_of(row_cost: f64, complete: usize, incomplete: usize) -> String {
    match complete - incomplete.min(complete) {
        0 => "—".to_string(),
        _ => crate::fmt::money_plain(Some(row_cost)),
    }
}

/// A CSV cost cell, blank rather than a false `0.00` when every one of
/// `total` is among `unpriced` — the same "unknown, not free" rule
/// [`cost_of`] states for the table and the screen, spelled the way an empty
/// cell already spells "nothing to resolve" for `ctx_peak_pct`. A floor
/// (some but not all of `total` unpriced) still prints the number it did
/// before; the `unpriced` column beside it is what tells a reader that
/// number is a floor rather than a total, the same job the note under the
/// table does for `cost_of`'s own cells.
fn csv_cost(cost: f64, total: usize, unpriced: usize) -> String {
    match total.saturating_sub(unpriced) {
        0 => String::new(),
        _ => format!("{cost:.2}"),
    }
}

/// "Cost is a floor" note, naming every model with no configured price among
/// `entries`. Shared by the printed table and the screen's own note, so
/// every reader of the ledger says it the same way.
fn unpriced_note<'a>(entries: impl Iterator<Item = &'a Entry>) -> Option<String> {
    // A line that spent nothing is missing nothing from the total, whatever
    // it says about a price — an enrolment line names no model at all, and
    // naming it here would ask for a price for the empty string.
    let unpriced: BTreeSet<&str> = entries
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

// ------------------------------------------------------------ directories
//
// The work outside the lanes: a session nobody dispatched, banked by
// `usage::sweep`'s watched-directory walk — see `Entry::dir`. `by dir` groups
// by the watched root a session ran in; `by session` is the same population
// one row per session. Both read `Loaded::dirs`, never `Loaded::entries`: the
// two populations never mix on one row, the same rule `Entry::is_lane`
// already enforces for the lanes table.

/// How the directory table groups its sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirBy {
    Dir,
    Session,
}

impl DirBy {
    const ALL: [DirBy; 2] = [DirBy::Dir, DirBy::Session];

    fn label(self) -> &'static str {
        match self {
            DirBy::Dir => "dir",
            DirBy::Session => "session",
        }
    }
}

/// One session outside the lanes, folded down to what `by session` and
/// `by dir` both need — a session has no `run`, no `outcome` and no per-line
/// `wall_s` to sum: its own span comes from its transcript's first and last
/// `timestamp` instead, see [`crate::usage::session_span`].
struct SessionRow {
    dir: String,
    /// What the `SKILL` column reads — see [`skill_cell`].
    skill: String,
    model: String,
    /// The session's own first `timestamp`, or — when its transcript could
    /// not be read a second time to answer that — the line's own `ts`
    /// (when it was banked, not when it ran).
    when: chrono::DateTime<chrono::Utc>,
    /// Its transcript's own span, in seconds — `0` where the span could not
    /// be read at all.
    time_s: i64,
    tokens: Tokens,
    cost: f64,
    /// Every ledger line banked for this session — more than one where the
    /// sweep caught it across several passes.
    lines: usize,
    unpriced: usize,
    ctx_peak_tokens: Option<u64>,
    ctx_peak_pct: Option<f64>,
}

/// The `SKILL` cell: a dash where the session ran none, its one skill where
/// it ran one, and the first with `+n` where it ran several — `n` the ones
/// not named, so the cell never claims more room than the column has. The
/// markers are sorted, so "first" is the first by name, not by time: a
/// `<command-name>` marker says which skills a session ran, never in what
/// order they mattered.
fn skill_cell(markers: Option<&BTreeSet<String>>) -> String {
    let mut names = markers
        .into_iter()
        .flatten()
        .map(|m| m.trim_start_matches('/'));
    match (names.next(), names.count()) {
        (None, _) => "—".to_string(),
        (Some(first), 0) => first.to_string(),
        (Some(first), more) => format!("{first} +{more}"),
    }
}

/// Every distinct session in `entries`, oldest first.
///
/// `spans` and `skills` are [`Loaded::spans_by_session`] and
/// [`Loaded::skills_by_session`] — read once at `load` rather than here,
/// since this runs on every draw and each is a transcript read.
fn list_sessions(
    entries: &[&Entry],
    models: &BTreeMap<String, ModelPrice>,
    spans: &Spans,
    skills: &HashMap<String, BTreeSet<String>>,
) -> Vec<SessionRow> {
    let mut rows: Vec<SessionRow> = group_by_session(entries)
        .into_iter()
        .map(|(session, lines)| {
            let latest = lines
                .iter()
                .max_by(|a, b| a.ts.cmp(&b.ts))
                .expect("a session always has at least one line");
            let mut tokens = Tokens::default();
            for line in &lines {
                tokens.add(&line.tokens);
            }
            let (ctx_peak_tokens, ctx_peak_pct) = ctx_peak_of(&lines, models);
            let (when, time_s) = match spans.get(&session) {
                Some((first, last)) => (*first, (*last - *first).num_seconds().max(0)),
                None => (
                    chrono::DateTime::parse_from_rfc3339(&latest.ts)
                        .map(|at| at.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now()),
                    0,
                ),
            };
            SessionRow {
                dir: latest.dir.clone().unwrap_or_default(),
                skill: skill_cell(skills.get(&session)),
                model: latest.model.clone(),
                when,
                time_s,
                tokens,
                cost: lines.iter().filter_map(|e| e.cost_usd).sum(),
                lines: lines.len(),
                unpriced: lines
                    .iter()
                    .filter(|e| e.cost_usd.is_none() && !e.tokens.is_zero())
                    .count(),
                ctx_peak_tokens,
                ctx_peak_pct,
            }
        })
        .collect();
    rows.sort_by_key(|a| a.when);
    rows
}

type Spans = HashMap<String, (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>)>;

/// `entries` by session, in first-seen order.
fn group_by_session<'a>(entries: &[&'a Entry]) -> Vec<(String, Vec<&'a Entry>)> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<&Entry>> = HashMap::new();
    for entry in entries {
        if !groups.contains_key(entry.session.as_str()) {
            order.push(entry.session.clone());
        }
        groups.entry(entry.session.clone()).or_default().push(entry);
    }
    order
        .into_iter()
        .map(|s| {
            let lines = groups.remove(&s).unwrap_or_default();
            (s, lines)
        })
        .collect()
}

/// One watched directory's row under `by dir`: every session it has run,
/// folded to a per-session figure the same way a lanes row divides by
/// `RUNS` — a plain total would just reward whichever directory happened to
/// run the most.
struct DirRow {
    dir: String,
    sessions: usize,
    tokens: Tokens,
    cost: f64,
    lines: usize,
    unpriced: usize,
    ctx_peak_tokens: Option<u64>,
    ctx_peak_pct: Option<f64>,
    ctx_avg: CtxAvg,
    time_s: i64,
    /// The latest of its own sessions' `when` — what orders the rows, the
    /// same "this morning's work heads the table" rule the lanes table
    /// follows. `None` for a watched root no session has run in yet, which
    /// sorts after every root that has.
    latest: Option<chrono::DateTime<chrono::Utc>>,
}

impl DirRow {
    fn per_session(&self, value: f64) -> Option<f64> {
        (self.sessions > 0).then(|| value / self.sessions as f64)
    }

    fn tokens_per_session(&self, n: u64) -> Option<u64> {
        self.per_session(n as f64).map(|v| v.round() as u64)
    }
}

/// Every directory `entries` ran in, plus every one of `roots` that none of
/// them did — so a watched root reads as a row of zero sessions rather than
/// being missing, which would look the same as not being watched at all.
fn dir_rows(
    entries: &[&Entry],
    roots: &[String],
    models: &BTreeMap<String, ModelPrice>,
    spans: &Spans,
) -> Vec<DirRow> {
    let mut order: Vec<String> = Vec::new();
    let mut by_dir: HashMap<String, Vec<&Entry>> = HashMap::new();
    for root in roots {
        if !by_dir.contains_key(root) {
            order.push(root.clone());
            by_dir.insert(root.clone(), Vec::new());
        }
    }
    for entry in entries {
        let Some(dir) = entry.dir.clone() else {
            continue;
        };
        if !by_dir.contains_key(&dir) {
            order.push(dir.clone());
        }
        by_dir.entry(dir).or_default().push(entry);
    }

    let no_skills = HashMap::new();
    let mut rows: Vec<DirRow> = order
        .into_iter()
        .map(|dir| {
            let lines = by_dir.remove(&dir).unwrap_or_default();
            let sessions = list_sessions(&lines, models, spans, &no_skills);
            let mut tokens = Tokens::default();
            for line in &lines {
                tokens.add(&line.tokens);
            }
            let (ctx_peak_tokens, ctx_peak_pct) = ctx_peak_of(&lines, models);
            let groups: Vec<Vec<&Entry>> = group_by_session(&lines)
                .into_iter()
                .map(|(_, g)| g)
                .collect();
            DirRow {
                sessions: sessions.len(),
                tokens,
                cost: lines.iter().filter_map(|e| e.cost_usd).sum(),
                lines: lines.len(),
                unpriced: lines
                    .iter()
                    .filter(|e| e.cost_usd.is_none() && !e.tokens.is_zero())
                    .count(),
                ctx_peak_tokens,
                ctx_peak_pct,
                ctx_avg: ctx_avg_of(groups.iter(), models),
                time_s: sessions.iter().map(|s| s.time_s).sum(),
                latest: sessions.iter().map(|s| s.when).max(),
                dir,
            }
        })
        .collect();
    rows.sort_by_key(|a| std::cmp::Reverse(a.latest));
    rows
}

/// The directory table's figure columns under `by dir` — see
/// [`lane_figures`] for why `SESSIONS` is drawn one wider than its title.
fn dir_figures(cells: [&str; 10]) -> String {
    let [sessions, inp, out, cr, cw, usd, usd_s, avg, peak, time] = cells;
    format!(
        "{SEP}{sessions:>9}{SEP}{inp:>10}{SEP}{out:>11}{SEP}{cr:>15}{SEP}{cw:>15}{SEP}{usd:>8}\
         {SEP}{usd_s:>11}{SEP}{avg:>12}{SEP}{peak:>8}{SEP}{time:>12}"
    )
}

fn dirs_table(rows: &[DirRow]) -> Table {
    let dw = rows
        .iter()
        .map(|r| r.dir.chars().count())
        .chain(["DIR".len(), "Total".len()])
        .max()
        .unwrap_or(3);
    let header = format!(
        "{:<dw$}{}",
        "DIR",
        dir_figures([
            "SESSIONS",
            "IN/SESSION",
            "OUT/SESSION",
            "CACHE R/SESSION",
            "CACHE W/SESSION",
            "USD",
            "USD/SESSION",
            "CTX PEAK AVG",
            "CTX PEAK",
            "TIME/SESSION",
        ])
    );
    let dash = || "—".to_string();
    let lines = rows
        .iter()
        .map(|row| {
            let tok = |n: u64| row.tokens_per_session(n).map_or_else(dash, tokens_cell);
            format!(
                "{:<dw$}{}",
                row.dir,
                dir_figures([
                    &row.sessions.to_string(),
                    &tok(row.tokens.input),
                    &tok(row.tokens.output),
                    &tok(row.tokens.cache_read),
                    &tok(row.tokens.cache_write()),
                    &cost_of(row.cost, row.lines, row.unpriced),
                    &row.per_session(row.cost).map_or_else(dash, |c| cost_of(
                        c,
                        row.lines,
                        row.unpriced
                    )),
                    &row.ctx_avg.cell(),
                    &ctx_cell(row.ctx_peak_tokens, row.ctx_peak_pct),
                    &row.per_session(row.time_s as f64)
                        .map_or_else(dash, |t| { crate::status::human_secs(t.round() as i64) }),
                ])
            )
        })
        .collect();
    // Sessions and USD are the two figures that add up across directories:
    // a session ran in exactly one of them.
    let sessions: usize = rows.iter().map(|r| r.sessions).sum();
    let cost: f64 = rows.iter().map(|r| r.cost).sum();
    let line_count: usize = rows.iter().map(|r| r.lines).sum();
    let unpriced: usize = rows.iter().map(|r| r.unpriced).sum();
    let total = format!(
        "{:<dw$}{}",
        "Total",
        dir_figures([
            &sessions.to_string(),
            "",
            "",
            "",
            "",
            &cost_of(cost, line_count, unpriced),
            "",
            "",
            "",
            "",
        ])
    )
    .trim_end()
    .to_string();
    Table {
        header,
        rows: lines,
        total,
    }
}

/// `WHEN` is always exactly `MM-DD HH:MM` — 11 characters — but the mockup
/// draws the column 13 wide, two columns of trailing pad beyond the content.
const SESSIONS_WHEN_WIDTH: usize = 13;
/// `SKILL` is fixed rather than measured: a skill name is whatever a person
/// called it, and one long name would otherwise push every figure on the
/// table to the right. Longer cells are cut with `…` — see [`clip_cell`].
const SESSIONS_SKILL_WIDTH: usize = 17;
/// A floor rather than a measurement, as the mockup draws it: a model name
/// longer than this still grows the column.
const SESSIONS_MODEL_MIN_WIDTH: usize = 16;

/// `MM-DD HH:MM` in local time — a session has no run and no version to lead
/// a row with, so `WHEN` carries the minute a lane's own `WHEN` leaves at the
/// day: the population here is a person's own hands-on work, not a lane
/// launched once and read back later.
fn session_when(at: chrono::DateTime<chrono::Utc>) -> String {
    at.with_timezone(&chrono::Local)
        .format("%m-%d %H:%M")
        .to_string()
}

/// A watched root by its last component — the session table names the
/// directory beside a skill and a model, where a whole absolute path would
/// take most of the row. `by dir` is where the whole path is.
fn dir_name(dir: &str) -> String {
    std::path::Path::new(dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string())
}

/// `text` cut to `width` characters, the last of them `…` where anything
/// was cut, so a clipped name never reads as a whole one.
fn clip_cell(text: &str, width: usize) -> String {
    match text.chars().count() > width {
        true => {
            let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
            out.push('…');
            out
        }
        false => text.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn session_line(
    dw: usize,
    mw: usize,
    when: &str,
    dir: &str,
    skill: &str,
    model: &str,
    tokens: [&str; 4],
    usd: &str,
    time: &str,
) -> String {
    let [inp, out, cr, cw] = tokens;
    format!(
        "{when:<w$}{SEP}{dir:<dw$}{SEP}{skill:<s$}{SEP}{model:<mw$}{SEP}{inp:>6}{SEP}{out:>7}\
         {SEP}{cr:>8}{SEP}{cw:>8}{SEP}{usd:>5}{SEP}{time:>8}",
        w = SESSIONS_WHEN_WIDTH,
        s = SESSIONS_SKILL_WIDTH,
    )
}

fn sessions_table(rows: &[SessionRow]) -> Table {
    let dw = rows
        .iter()
        .map(|r| dir_name(&r.dir).chars().count())
        .max()
        .unwrap_or(0)
        .max("DIR".len());
    let mw = rows
        .iter()
        .map(|r| r.model.chars().count())
        .max()
        .unwrap_or(0)
        .max(SESSIONS_MODEL_MIN_WIDTH);
    let header = session_line(
        dw,
        mw,
        "WHEN",
        "DIR",
        "SKILL",
        "MODEL",
        ["IN", "OUT", "CACHE R", "CACHE W"],
        "USD",
        "TIME",
    );
    let lines = rows
        .iter()
        .map(|row| {
            session_line(
                dw,
                mw,
                &session_when(row.when),
                &dir_name(&row.dir),
                &clip_cell(&row.skill, SESSIONS_SKILL_WIDTH),
                &row.model,
                [
                    &tokens_cell(row.tokens.input),
                    &tokens_cell(row.tokens.output),
                    &tokens_cell(row.tokens.cache_read),
                    &tokens_cell(row.tokens.cache_write()),
                ],
                &cost_of(row.cost, row.lines, row.unpriced),
                &crate::status::human_secs(row.time_s.max(0)),
            )
        })
        .collect();
    // One row per session, so every raw figure on it adds up: each token
    // class, USD and the time the sessions spanned.
    let mut tokens = Tokens::default();
    for row in rows {
        tokens.add(&row.tokens);
    }
    let cost: f64 = rows.iter().map(|r| r.cost).sum();
    let lines_n: usize = rows.iter().map(|r| r.lines).sum();
    let unpriced: usize = rows.iter().map(|r| r.unpriced).sum();
    let time: i64 = rows.iter().map(|r| r.time_s.max(0)).sum();
    let total = session_line(
        dw,
        mw,
        "Total",
        "",
        "",
        "",
        [
            &tokens_cell(tokens.input),
            &tokens_cell(tokens.output),
            &tokens_cell(tokens.cache_read),
            &tokens_cell(tokens.cache_write()),
        ],
        &cost_of(cost, lines_n, unpriced),
        &crate::status::human_secs(time),
    );
    Table {
        header,
        rows: lines,
        total,
    }
}

// ------------------------------------------------------ directories export

const DIRS_CSV_HEADER: &str = "by,dir,sessions,in_tokens,out_tokens,cache_read_tokens,\
                               cache_write_tokens,in_per_session,out_per_session,\
                               cache_read_per_session,cache_write_per_session,cost_usd,\
                               cost_per_session,unpriced,ctx_peak_tokens,ctx_peak_pct,\
                               ctx_peak_avg_tokens,ctx_peak_avg_pct,time_s,time_per_session_s";

const SESSIONS_CSV_HEADER: &str = "by,when,dir,skill,model,in_tokens,out_tokens,\
                                   cache_read_tokens,cache_write_tokens,cost_usd,unpriced,\
                                   ctx_peak_tokens,ctx_peak_pct,time_s";

fn csv_dir_row(row: &DirRow) -> String {
    let t = &row.tokens;
    let per = |n: u64| opt_u64(row.tokens_per_session(n));
    [
        "dir".to_string(),
        csv_field(&row.dir).into_owned(),
        row.sessions.to_string(),
        t.input.to_string(),
        t.output.to_string(),
        t.cache_read.to_string(),
        t.cache_write().to_string(),
        per(t.input),
        per(t.output),
        per(t.cache_read),
        per(t.cache_write()),
        csv_cost(row.cost, row.lines, row.unpriced),
        row.per_session(row.cost)
            .map_or(String::new(), |c| csv_cost(c, row.lines, row.unpriced)),
        row.unpriced.to_string(),
        opt_u64(row.ctx_peak_tokens),
        opt_share(row.ctx_peak_pct),
        opt_u64(row.ctx_avg.tokens.map(|t| t.round() as u64)),
        opt_share(row.ctx_avg.pct),
        row.time_s.max(0).to_string(),
        row.per_session(row.time_s as f64)
            .map_or(String::new(), |t| (t.round() as i64).to_string()),
    ]
    .join(",")
}

fn csv_session_row(row: &SessionRow) -> String {
    let t = &row.tokens;
    [
        "session".to_string(),
        csv_field(&session_when(row.when)).into_owned(),
        csv_field(&row.dir).into_owned(),
        csv_field(&row.skill).into_owned(),
        csv_field(&row.model).into_owned(),
        t.input.to_string(),
        t.output.to_string(),
        t.cache_read.to_string(),
        t.cache_write().to_string(),
        csv_cost(row.cost, row.lines, row.unpriced),
        row.unpriced.to_string(),
        opt_u64(row.ctx_peak_tokens),
        opt_share(row.ctx_peak_pct),
        row.time_s.max(0).to_string(),
    ]
    .join(",")
}

/// A directory export's `Total` line: `total` in the `by` column, as the
/// lanes export marks its own, and only the figures the screen's `Total`
/// line carries — the session count and USD under `by dir`; every token
/// class, USD and time under `by session`.
fn csv_dirs_total(by: DirBy, dirs: &[DirRow], sessions: &[SessionRow]) -> String {
    let header = match by {
        DirBy::Dir => DIRS_CSV_HEADER,
        DirBy::Session => SESSIONS_CSV_HEADER,
    };
    let mut tokens = Tokens::default();
    for row in sessions {
        tokens.add(&row.tokens);
    }
    let (cost, lines, unpriced) = match by {
        DirBy::Dir => (
            dirs.iter().map(|r| r.cost).sum::<f64>(),
            dirs.iter().map(|r| r.lines).sum::<usize>(),
            dirs.iter().map(|r| r.unpriced).sum::<usize>(),
        ),
        DirBy::Session => (
            sessions.iter().map(|r| r.cost).sum::<f64>(),
            sessions.iter().map(|r| r.lines).sum::<usize>(),
            sessions.iter().map(|r| r.unpriced).sum::<usize>(),
        ),
    };
    header
        .split(',')
        .map(|name| match (by, name) {
            (_, "by") => "total".to_string(),
            (_, "cost_usd") => csv_cost(cost, lines, unpriced),
            (DirBy::Dir, "sessions") => dirs.iter().map(|r| r.sessions).sum::<usize>().to_string(),
            (DirBy::Session, "in_tokens") => tokens.input.to_string(),
            (DirBy::Session, "out_tokens") => tokens.output.to_string(),
            (DirBy::Session, "cache_read_tokens") => tokens.cache_read.to_string(),
            (DirBy::Session, "cache_write_tokens") => tokens.cache_write().to_string(),
            (DirBy::Session, "time_s") => sessions
                .iter()
                .map(|r| r.time_s.max(0))
                .sum::<i64>()
                .to_string(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

// ----------------------------------------------------------------- the screen
//
// Bare `spoolway eval`, no flags and no `--json`: this rather than the
// printing path above, which every flag still takes — see
// `cli::eval_is_bare`. Modelled on `commands::queue_screen`: raw mode through
// `platform::TermGuard`, one byte at a time off stdin through
// `crate::screen::read_key`, so a real tty in raw mode and a pipe an
// end-to-end suite is scripting drive it identically, and the screen ends
// the moment either runs out.
//
// Two tables and no drilling in: `tab` moves between the lanes and the
// watched directories, and everything else — which `by`, which filter — is a
// row on the filter panel. Every table is built by the same functions the
// printing path uses (`lanes_table`, `dirs_table`, `sessions_table`), so the
// screen and a pasted `spoolway eval --by step` can never disagree about a
// column. Rendered without colour throughout: `pad_to` counts every
// character as one column, and an ANSI escape slipped into a row would throw
// the frame's own border out of line with it.

/// Which of the two tables is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableKind {
    Lanes,
    Dirs,
}

impl TableKind {
    fn other(self) -> TableKind {
        match self {
            TableKind::Lanes => TableKind::Dirs,
            TableKind::Dirs => TableKind::Lanes,
        }
    }

    /// What `tab` names this table as, in the key line of the other one.
    fn label(self) -> &'static str {
        match self {
            TableKind::Lanes => "lanes",
            TableKind::Dirs => "dirs",
        }
    }
}

/// Which project's ledger the screen reads. Set from `--project`/`--all` on
/// the command line before the screen ever opens — not a row of the filter
/// panel, and deliberately the only project picker the screen has at all.
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

/// Every row of both filter panels, as currently applied — what `load`
/// reads the ledger through and what each table narrows it by. `since` and
/// `until` hold a plain `YYYY-MM-DD` the calendar wrote in, or a duration
/// (`7d`, `24h`) passed on the command line — not yet parsed either way:
/// parsing happens once, in `load`, so a bad value is reported in one place.
///
/// The two tables keep their own `by` and their own narrowing rows, so
/// `tab` back to a table finds it the way it was left; `since` and `until`
/// are shared, since a window bounds both populations the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Filters {
    by: EvalBy,
    dir_by: DirBy,
    group: Option<String>,
    task: Option<String>,
    pipeline: Option<String>,
    step: Option<String>,
    version: Option<String>,
    /// The watched root a directory row must have banked under — see
    /// [`Entry::dir`].
    dir: Option<String>,
    /// The `<command-name>` a directory session's transcript must hold — see
    /// [`crate::usage::skill_markers`]. Keeps whole sessions, not stretches
    /// of one: see [`scoped_dirs`]'s own note on why that is an upper bound.
    skill: Option<String>,
    since: String,
    until: String,
    scope: Scope,
}

impl Filters {
    /// What the screen opens with: by pipeline and by dir, every row blank.
    fn from_args(args: &EvalArgs) -> Filters {
        let scope = match (&args.project, args.all) {
            (_, true) => Scope::All,
            (Some(name), false) => Scope::Named(name.clone()),
            (None, false) => Scope::Mine,
        };
        Filters {
            by: args.by,
            dir_by: DirBy::Dir,
            group: args.group.clone(),
            task: args.task.clone(),
            pipeline: args.pipeline.clone(),
            step: args.step.clone(),
            version: args.pipeline_version.clone(),
            dir: None,
            skill: None,
            since: args.since.clone().unwrap_or_default(),
            until: args.until.clone().unwrap_or_default(),
            scope,
        }
    }

    fn lanes(&self) -> LaneFilters<'_> {
        LaneFilters {
            group: self.group.as_deref(),
            task: self.task.as_deref(),
            pipeline: self.pipeline.as_deref(),
            step: self.step.as_deref(),
            version: self.version.as_deref(),
        }
    }
}

/// `s` trimmed down to `None` when it is blank — what an empty field means
/// to every reader below: no bound at all, not a bound of the empty string.
fn non_empty(s: &str) -> Option<&str> {
    let s = s.trim();
    (!s.is_empty()).then_some(s)
}

/// The ledger, read once under the current filters' scope and window —
/// everything a table is built from, before its own filter rows narrow it
/// further. Reloaded on `r`, and every time the filter panel's `enter`
/// commits a change.
struct Loaded {
    /// Every lane in scope and window, unfiltered by any row — the filter
    /// rows' candidate lists are read off of this, so the values on offer
    /// are always the project's own rather than whatever the table happens
    /// to already be narrowed to. An interactive line never reaches here at
    /// all — see `Entry::is_lane`.
    entries: Vec<Entry>,
    /// Every directory line in scope and window — see [`Entry::dir`] — the
    /// population the directory table reads, unfiltered by `dir` or `skill`.
    /// Disjoint from `entries`: a line is either a lane or a directory
    /// session, never both.
    dirs: Vec<Entry>,
    /// Every watched root this project names — see
    /// [`crate::config::Config::watch_roots`], which always includes the
    /// project's own directory — spelled the way [`Entry::dir`] spells one.
    /// Empty when the screen reads another project's ledger, or every
    /// project's: their roots are their own configs' to name, not this one's.
    roots: Vec<String>,
    /// Every `<command-name>` marker each of `dirs`' own sessions' transcripts
    /// holds, keyed by session id — read once here rather than on every draw,
    /// since a transcript scan is a file read `draw` cannot afford to repeat
    /// on every keypress. What the `skill` filter row cycles over, what a
    /// chosen `skill` narrows `dirs` by — see [`scoped_dirs`] — and what the
    /// `SKILL` column names.
    skills_by_session: HashMap<String, BTreeSet<String>>,
    /// Each of `dirs`' own sessions' transcript span — see
    /// [`crate::usage::session_span`] — keyed by session id, read once here
    /// for the same reason `skills_by_session` is. Absent for a session whose
    /// span could not be read at all — `list_sessions` falls back to its own
    /// line's `ts` for that one.
    spans_by_session: Spans,
    fallback: HashMap<(String, String), String>,
    models: BTreeMap<String, ModelPrice>,
    scope_label: String,
}

fn load(repo: &Repo, filters: &Filters) -> Result<Loaded> {
    crate::usage::sweep(repo);

    let (project, all) = match &filters.scope {
        Scope::Mine => (None, false),
        Scope::Named(name) => (Some(name.as_str()), false),
        Scope::All => (None, true),
    };
    let (all_entries, _scope) = crate::spend::collect_scoped(repo, project, all)?;

    let mut entries: Vec<Entry> = all_entries
        .iter()
        .filter(|e| e.is_lane())
        .cloned()
        .collect();
    let mut dirs: Vec<Entry> = all_entries
        .into_iter()
        .filter(|e| e.dir.is_some())
        .collect();

    let fallback = fallback_keys(&entries);

    let window =
        crate::spend::window_of(None, non_empty(&filters.since), non_empty(&filters.until))?;
    entries.retain(|entry| window.contains(&entry.ts));
    entries.sort_by(|a, b| a.ts.cmp(&b.ts));
    dirs.retain(|entry| window.contains(&entry.ts));
    dirs.sort_by(|a, b| a.ts.cmp(&b.ts));

    let mut skills_by_session: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut spans_by_session = HashMap::new();
    for session in dirs
        .iter()
        .map(|e| e.session.as_str())
        .collect::<BTreeSet<_>>()
    {
        let kind = dirs
            .iter()
            .find(|e| e.session == session)
            .map(|e| e.kind.as_str())
            .unwrap_or_default();
        skills_by_session.insert(
            session.to_string(),
            crate::usage::skill_markers(kind, session),
        );
        if let Some(span) = crate::usage::session_span(kind, session) {
            spans_by_session.insert(session.to_string(), span);
        }
    }

    // The same `to_string_lossy` spelling `usage::sweep_dirs` banks a root
    // under, so a root with sessions and the same root seeded here are one
    // row, not two.
    let roots = match filters.scope {
        Scope::Mine => repo
            .config
            .watch_roots(&repo.root)
            .iter()
            .map(|root| root.to_string_lossy().into_owned())
            .collect(),
        Scope::Named(_) | Scope::All => Vec::new(),
    };

    Ok(Loaded {
        entries,
        dirs,
        roots,
        skills_by_session,
        spans_by_session,
        fallback,
        models: repo.config.models.clone(),
        scope_label: filters.scope.label(repo),
    })
}

/// `loaded.entries`, narrowed by every lanes filter row.
fn scoped_entries<'a>(loaded: &'a Loaded, filters: &Filters) -> Vec<&'a Entry> {
    let lanes = filters.lanes();
    loaded.entries.iter().filter(|e| lanes.admits(e)).collect()
}

/// `loaded.dirs`, narrowed by the `dir` and `skill` filters — the input the
/// directory table is built from.
///
/// A `skill` filter keeps a *whole session* the moment any of its lines'
/// transcript held that marker, and drops the rest — never a stretch of one
/// session, since a `<command-name>` marks where a skill started and never
/// where it ended. That is why choosing a skill only ever narrows which
/// sessions are counted, not what any of them cost: the figure it leaves on
/// screen is an upper bound on that skill's own spend, not its true share.
fn scoped_dirs<'a>(loaded: &'a Loaded, filters: &Filters) -> Vec<&'a Entry> {
    loaded
        .dirs
        .iter()
        .filter(|e| {
            filters
                .dir
                .as_deref()
                .is_none_or(|d| e.dir.as_deref() == Some(d))
        })
        .filter(|e| {
            filters.skill.as_deref().is_none_or(|skill| {
                loaded
                    .skills_by_session
                    .get(&e.session)
                    .is_some_and(|markers| markers.contains(skill))
            })
        })
        .collect()
}

/// The watched roots to seed the `by dir` table with: every one, unless a
/// `dir` or `skill` row is narrowing it — a root with no sessions of that
/// skill is not an answer to "which directories ran it".
fn seeded_roots<'a>(loaded: &'a Loaded, filters: &Filters) -> &'a [String] {
    match (&filters.dir, &filters.skill) {
        (None, None) => &loaded.roots,
        _ => &[],
    }
}

/// The distinct values `value` reads off `entries`, sorted.
fn distinct<'a>(
    entries: impl Iterator<Item = &'a Entry>,
    value: impl Fn(&'a Entry) -> Option<&'a str>,
) -> Vec<String> {
    entries
        .filter_map(value)
        .map(String::from)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn group_candidates(loaded: &Loaded) -> Vec<String> {
    distinct(loaded.entries.iter(), |e| e.plan.as_deref())
}

fn pipeline_candidates(loaded: &Loaded) -> Vec<String> {
    distinct(loaded.entries.iter(), |e| Some(e.pipeline.as_str()))
}

/// Steps, narrowed to the chosen pipeline's own once one is chosen.
fn step_candidates(loaded: &Loaded, filters: &Filters) -> Vec<String> {
    let pipeline = filters.pipeline.as_deref();
    distinct(
        loaded
            .entries
            .iter()
            .filter(|e| pipeline.is_none_or(|p| e.pipeline == p)),
        |e| Some(e.step.as_str()),
    )
}

/// Versions, narrowed to the chosen pipeline's own once one is chosen.
fn version_candidates(loaded: &Loaded, filters: &Filters) -> Vec<String> {
    let pipeline = filters.pipeline.as_deref();
    distinct(
        loaded
            .entries
            .iter()
            .filter(|e| pipeline.is_none_or(|p| e.pipeline == p)),
        |e| Some(e.pipeline_version.as_str()),
    )
}

/// Only the tasks every other lanes row leaves on the table — a project's
/// whole task list is far too long to cycle through one at a time, and
/// most of it would narrow the table to nothing.
fn task_candidates(loaded: &Loaded, filters: &Filters) -> Vec<String> {
    let others = LaneFilters {
        task: None,
        ..filters.lanes()
    };
    distinct(loaded.entries.iter().filter(|e| others.admits(e)), |e| {
        Some(e.task.as_str())
    })
}

/// Every directory the table can show — the watched roots and every
/// directory a banked line names — for the `dir` row to cycle over.
fn dir_candidates(loaded: &Loaded) -> Vec<String> {
    loaded
        .roots
        .iter()
        .cloned()
        .chain(loaded.dirs.iter().filter_map(|e| e.dir.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Every `<command-name>` marker held by any of `loaded.dirs`' own sessions —
/// the candidate list the `skill` row cycles over. Built from the watched
/// transcripts themselves, never from a configured list.
fn skill_candidates(loaded: &Loaded) -> Vec<String> {
    loaded
        .skills_by_session
        .values()
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Move an "all" filter to the next or previous entry of the conceptual list
/// `[None, candidates[0], candidates[1], ...]`, clamped at both ends rather
/// than wrapping: `←` on the first real value lands back on "all", and `←` on
/// "all" itself, or `→` past the last real value, does nothing.
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

/// The `by` row's own cycling: the same clamp [`cycle_option`] applies, over
/// a list with no "all" at its head — there is always a `by`.
fn cycle_by<T: Copy + PartialEq>(all: &[T], current: T, forward: bool) -> T {
    let at = all.iter().position(|b| *b == current).unwrap_or(0);
    let next = match forward {
        true => (at + 1).min(all.len() - 1),
        false => at.saturating_sub(1),
    };
    all[next]
}

/// One selectable row, wherever it sits in whichever table is on screen —
/// what the cursor moves over.
struct Row {
    /// The row's own text, without the one-column marker `render_lines`
    /// prefixes onto it — kept apart so the marker can be decided once,
    /// against the cursor, at the point every row is actually drawn.
    text: String,
}

/// One line of a table's body: a row the cursor can land on, or plain text —
/// the header, the `Total` line, a note that nothing is here — that it skips
/// over.
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
            // header or the `Total` line lines up under the very columns its
            // rows are printed in, rather than sitting a column to their
            // left. The mockup's own frame carries the marker flush against
            // the border, with nothing between it and the row's first
            // character — `>impl`, not `> impl`.
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

/// A [`Table`] as the screen's body lines: its header, a cursor row per
/// row, and its `Total` line.
fn table_lines(table: Table) -> Vec<Line> {
    let mut out = vec![Line::Text(table.header)];
    out.extend(table.rows.into_iter().map(|text| Line::Row(Row { text })));
    out.push(Line::Text(table.total));
    out
}

/// The table on screen, built from the same functions the printing path
/// prints with.
fn view_lines(
    loaded: &Loaded,
    filters: &Filters,
    pipelines: &Pipelines,
    table: TableKind,
) -> Vec<Line> {
    match table {
        TableKind::Lanes => {
            let entries = scoped_entries(loaded, filters);
            if entries.is_empty() {
                return vec![Line::Text("Nothing to compare in this window.".to_string())];
            }
            let rows = lane_rows(
                &entries,
                filters.by,
                &loaded.fallback,
                &loaded.models,
                Some(pipelines),
            );
            let total = LaneTotal::of(&entries, &loaded.fallback);
            table_lines(lanes_table(filters.by, &rows, &total))
        }
        TableKind::Dirs => {
            let entries = scoped_dirs(loaded, filters);
            match filters.dir_by {
                DirBy::Dir => {
                    let rows = dir_rows(
                        &entries,
                        seeded_roots(loaded, filters),
                        &loaded.models,
                        &loaded.spans_by_session,
                    );
                    if rows.is_empty() {
                        return vec![Line::Text(
                            "Nothing outside the lanes in this window.".to_string(),
                        )];
                    }
                    table_lines(dirs_table(&rows))
                }
                DirBy::Session => {
                    if entries.is_empty() {
                        return vec![Line::Text(
                            "No sessions outside the lanes in that window.".to_string(),
                        )];
                    }
                    // Newest first: a person opening the table wants to know
                    // what just ran, not what ran first.
                    let mut rows = list_sessions(
                        &entries,
                        &loaded.models,
                        &loaded.spans_by_session,
                        &loaded.skills_by_session,
                    );
                    rows.reverse();
                    table_lines(sessions_table(&rows))
                }
            }
        }
    }
}

// ------------------------------------------------------------------- export

/// The table on screen, flattened for `.csv`: one header, a line per row in
/// the order they are on screen, and the `Total` line marked `total` — plus
/// how many of those lines are rows, for the confirmation to count.
fn export_rows(
    loaded: &Loaded,
    filters: &Filters,
    pipelines: &Pipelines,
    table: TableKind,
) -> (String, Vec<String>, usize) {
    match table {
        TableKind::Lanes => {
            let entries = scoped_entries(loaded, filters);
            let rows = lane_rows(
                &entries,
                filters.by,
                &loaded.fallback,
                &loaded.models,
                Some(pipelines),
            );
            let total = LaneTotal::of(&entries, &loaded.fallback);
            let mut lines: Vec<String> =
                rows.iter().map(|r| lanes_csv_row(filters.by, r)).collect();
            lines.push(lanes_csv_total(filters.by, &total));
            (lanes_csv_header(filters.by), lines, rows.len())
        }
        TableKind::Dirs => {
            let entries = scoped_dirs(loaded, filters);
            let mut sessions = list_sessions(
                &entries,
                &loaded.models,
                &loaded.spans_by_session,
                &loaded.skills_by_session,
            );
            sessions.reverse();
            match filters.dir_by {
                DirBy::Dir => {
                    let rows = dir_rows(
                        &entries,
                        seeded_roots(loaded, filters),
                        &loaded.models,
                        &loaded.spans_by_session,
                    );
                    let mut lines: Vec<String> = rows.iter().map(csv_dir_row).collect();
                    lines.push(csv_dirs_total(DirBy::Dir, &rows, &sessions));
                    (DIRS_CSV_HEADER.to_string(), lines, rows.len())
                }
                DirBy::Session => {
                    let mut lines: Vec<String> = sessions.iter().map(csv_session_row).collect();
                    lines.push(csv_dirs_total(DirBy::Session, &[], &sessions));
                    (SESSIONS_CSV_HEADER.to_string(), lines, sessions.len())
                }
            }
        }
    }
}

/// The `by` the table on screen is grouped by — what its title, its export's
/// file name and the export confirmation all name it as.
fn by_label(filters: &Filters, table: TableKind) -> &'static str {
    match table {
        TableKind::Lanes => filters.by.label(),
        TableKind::Dirs => filters.dir_by.label(),
    }
}

/// `.spoolway/evals/`, so the file `e` writes lives beside the ledger it was
/// exported from. spoolway writes no `.gitignore` rules of its own any more —
/// see [`crate::gitignore`] — so keeping an export out of git is the
/// project's own line to add, like every other directory a person's own run
/// fills in rather than the project's tracked setup.
fn evals_dir(repo: &Repo) -> std::path::PathBuf {
    repo.root.join(crate::config::STATE_DIR).join("evals")
}

/// Write the table currently on screen to
/// `.spoolway/evals/eval-by-<by>-<stamp>.csv`, and hand back the path and how
/// many rows it holds — what the confirmation panel names.
///
/// The stamp is to the minute, and a suffix is added rather than a file
/// overwritten: two `e` presses in the same minute used to leave only the
/// second file while both reported success (review finding 44).
fn export(
    repo: &Repo,
    loaded: &Loaded,
    filters: &Filters,
    pipelines: &Pipelines,
    table: TableKind,
) -> Result<(std::path::PathBuf, usize)> {
    let (header, lines, rows) = export_rows(loaded, filters, pipelines, table);
    let dir = evals_dir(repo);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let stamp = chrono::Local::now().format("%Y-%m-%d-%H%M");
    let base = format!("eval-by-{}-{stamp}", by_label(filters, table));
    let mut path = dir.join(format!("{base}.csv"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{base}-{n}.csv"));
        n += 1;
    }
    let mut body = String::new();
    body.push_str(&header);
    body.push('\n');
    for line in &lines {
        body.push_str(line);
        body.push('\n');
    }
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok((path, rows))
}

/// `path`, relative to `root` where it is under it — what the export
/// confirmation names, so it reads `.spoolway/evals/eval-by-step-….csv`
/// rather than the whole absolute path a real project's `repo.root` would
/// make it. Falls back to the path as given when it is not under `root` at
/// all, which should not happen in practice since `export` always writes
/// under `evals_dir(repo)`.
fn display_relative(path: &std::path::Path, root: &std::path::Path) -> String {
    path.strip_prefix(root)
        .map(|rel| rel.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

// -------------------------------------------------------------------- frame

/// The narrowest frame drawn, so a very narrow terminal still draws a
/// readable frame rather than a column of borders.
const MIN_WIDTH: usize = 60;
/// Border a row spends on top of its content: the two `"│"` characters. The
/// marker column and the padding after it are part of the content itself —
/// see `render_lines` — so there is no separate space to account for here.
const FRAME_CHROME: usize = 2;

/// The frame's content width — the terminal's own width where one can be
/// measured, clamped to [`MIN_WIDTH`]. With no terminal to measure, wide
/// enough for `content` columns plus the one blank column the mockup leaves
/// before the right border: the tables are wider than any fixed guess, and a
/// piped screen cut down to one would lose the figures on the right.
fn frame_width(content: usize) -> usize {
    match terminal_size::terminal_size() {
        Some((w, _)) => (w.0 as usize).saturating_sub(FRAME_CHROME).max(MIN_WIDTH),
        None => (content + 1).max(MIN_WIDTH),
    }
}

/// What `frame_rows` subtracts from the terminal's own height: two borders,
/// the footer, one spare line — so the last line's own newline does not
/// scroll the top of the frame away, the same reasoning `commands::queue`'s
/// own `PANE_CHROME_ROWS` gives — and, one for each of `notes`, such as the
/// unpriced-cost note. Pulled out of `frame_rows` so the count itself is a
/// pure function a test can pin without a real terminal behind it.
fn frame_chrome(notes: usize) -> usize {
    4 + notes
}

/// How many body rows the frame gets once `frame_chrome` is counted. `None`
/// where there is no terminal to measure, which is what lets a piped run
/// keep every row rather than losing the ones past some guessed height.
fn frame_rows(notes: usize) -> Option<usize> {
    terminal_size::terminal_size()
        .map(|(_, h)| (h.0 as usize).saturating_sub(frame_chrome(notes)).max(1))
}

fn frame_top(title: &str, right: &str, width: usize) -> String {
    let left = format!("─ eval · {title} ");
    let right = format!(" {right} ─");
    let dashes = width.saturating_sub(left.chars().count() + right.chars().count());
    format!("┌{left}{}{right}┐", "─".repeat(dashes.max(1)))
}

fn frame_bottom(width: usize) -> String {
    format!("└{}┘", "─".repeat(width))
}

/// `panel`'s own overhead beyond a body line's text: the box border on
/// either side plus the two-space indent `boxed` prefixes every line with —
/// see `screen::boxed`. What `wrap_notice` subtracts from the frame's own
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

/// The right side of the top border: the scope, every filter row the table
/// on screen has narrowed by, and the window — so a frame read on its own
/// still says what it is a table of. The left side names only the `by`.
fn filters_label(loaded: &Loaded, filters: &Filters, table: TableKind) -> String {
    let mut parts = vec![loaded.scope_label.clone()];
    let named: Vec<(&str, &Option<String>)> = match table {
        TableKind::Lanes => vec![
            ("group", &filters.group),
            ("task", &filters.task),
            ("pipeline", &filters.pipeline),
            ("step", &filters.step),
            ("version", &filters.version),
        ],
        TableKind::Dirs => vec![("dir", &filters.dir), ("skill", &filters.skill)],
    };
    for (name, value) in named {
        if let Some(value) = value {
            parts.push(format!("{name} {value}"));
        }
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

/// The keys, under the frame — the one line that says what the screen does,
/// bracketed the way the board brackets its own by [`key_hint`]. The frame
/// sits one column in from the key line's own leading space, so one more is
/// added in front to line `[↑↓]` up under the frame's first column of
/// content, as the mockup draws it.
fn footer(table: TableKind) -> String {
    format!(
        " {}",
        key_hint(&[
            ("↑↓", "move"),
            ("tab", table.other().label()),
            ("f", "filters"),
            ("e", "export"),
            ("r", "refresh"),
            ("q", "quit"),
        ])
    )
}

fn draw(
    pipelines: &Pipelines,
    loaded: &Loaded,
    state: &ScreenState,
    out: &mut impl std::io::Write,
) {
    let _ = write!(out, "\x1b[2J\x1b[H");

    // Computed before `frame_rows`, which has to know how many of these are
    // about to take a row of their own — see `frame_rows`'s own doc comment.
    let mut notes = Vec::new();
    if let Some(note) = screen_unpriced_note(loaded, &state.filters, state.table) {
        notes.push(note);
    }

    let lines = view_lines(loaded, &state.filters, pipelines, state.table);
    let right = filters_label(loaded, &state.filters, state.table);
    let title = format!("by {}", by_label(&state.filters, state.table));
    // Wide enough for the widest line plus its marker column, and for the
    // top border's own title and label with a dash or two between them.
    let content = lines
        .iter()
        .map(|l| match l {
            Line::Text(t) => t.chars().count(),
            Line::Row(r) => r.text.chars().count(),
        })
        .max()
        .unwrap_or(0)
        + 1;
    let border = format!("─ eval · {title} ").chars().count() + right.chars().count() + 5;
    let width = frame_width(content.max(border));
    let rows = frame_rows(notes.len());
    let body = render_lines(&lines, state.cursor, width);
    let cursor_line = cursor_line_index(&lines, state.cursor);
    let mut body = clip(&body, cursor_line, rows, width);

    // A notice or the filter panel sits over the table as an `overlay` —
    // computed here, before the frame, because its own height is now part
    // of deciding the frame's: `clip` above cuts a body *taller* than the
    // terminal down to size, but a body shorter than whatever panel is
    // about to be drawn on it needs padding the other way, or `overlay`
    // writes past the frame's own last row and the panel loses its bottom
    // border. Reproduced on a small ledger, where the table itself is only a
    // few lines tall and the filter panel is fourteen.
    let overlay_panel: Option<Vec<String>> = match &state.mode {
        // Wrapped to the frame's own width, not just split on the newlines
        // `body` already carries: an error message from `load` — a bad
        // `--since` in particular — is one long unwrapped sentence, and
        // `panel` sizes its box to the widest line with no wrapping of its
        // own. Left unwrapped, that box is wider than the frame, and
        // `overlay` can only place a panel that fits inside it — past that
        // width it draws the panel's border over the frame's own left
        // border and silently drops every character past the right edge.
        Mode::Notice { title, body, keys } => {
            let lines = wrap_notice(body, width);
            Some(match keys {
                Some(keys) => panel(title, &lines, keys),
                None => boxed(title, &lines),
            })
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

    let mut frame = vec![frame_top(&title, &right, width)];
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
    // Between the frame's own bottom border and the keys line, so neither
    // takes a body row and so neither throws off `clip`'s own scroll
    // indicator. Their rows were already reserved above, in `frame_rows`.
    for note in &notes {
        let _ = writeln!(out, "  {note}");
    }
    let _ = writeln!(out, "{}", footer(state.table));
}

/// The same "Cost is a floor" note the printed table carries, over whichever
/// rows are actually on screen: the current table's own entries, narrowed
/// by the current filters — not the whole ledger `loaded` holds, which may
/// name a model nowhere in view.
fn screen_unpriced_note(loaded: &Loaded, filters: &Filters, table: TableKind) -> Option<String> {
    match table {
        TableKind::Lanes => unpriced_note(scoped_entries(loaded, filters).into_iter()),
        TableKind::Dirs => unpriced_note(scoped_dirs(loaded, filters).into_iter()),
    }
}

// -------------------------------------------------------------------- modes

/// One row of the filter panel — `↑`/`↓` moves between these, `←`/`→`
/// changes the value on every row but the two dates, and `enter` on those
/// two opens a calendar rather than applying. `scope` is not a row here at
/// all: the screen only ever opens bare, so it can only ever hold the
/// default it opened with, and a row with a single possible answer is not a
/// question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterField {
    By,
    Group,
    Task,
    Pipeline,
    Step,
    Version,
    Dir,
    Skill,
    Since,
    Until,
}

/// The rows the filter panel draws for `table`, in order: `by` first, since
/// it decides what every row below it is a filter *of*, and the window last
/// on both, since it bounds both populations the same way.
fn filter_fields(table: TableKind) -> &'static [FilterField] {
    match table {
        TableKind::Lanes => &[
            FilterField::By,
            FilterField::Group,
            FilterField::Task,
            FilterField::Pipeline,
            FilterField::Step,
            FilterField::Version,
            FilterField::Since,
            FilterField::Until,
        ],
        TableKind::Dirs => &[
            FilterField::By,
            FilterField::Dir,
            FilterField::Skill,
            FilterField::Since,
            FilterField::Until,
        ],
    }
}

/// A copy of [`Filters`] a person is editing in the filter panel, plus which
/// row the cursor is on and which table's rows it draws — fixed at the
/// moment `f` opened the panel, since nothing in [`Mode::Filter`] lets `tab`
/// change the table while it is up. `esc` drops this untouched; `enter`
/// turns it back into the real `Filters` and reloads — see `run_screen`'s
/// own handling of [`Mode::Filter`].
#[derive(Debug, Clone)]
struct Draft {
    field: usize,
    table: TableKind,
    filters: Filters,
}

impl Draft {
    fn new(table: TableKind, filters: Filters) -> Draft {
        Draft {
            field: 0,
            table,
            filters,
        }
    }

    fn fields(&self) -> &'static [FilterField] {
        filter_fields(self.table)
    }

    fn current(&self) -> FilterField {
        self.fields()[self.field]
    }
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
    /// before that would never be read. `keys` is the hint line under it,
    /// `None` for the export confirmation, which the mockup draws bare.
    Notice {
        title: &'static str,
        body: String,
        keys: Option<&'static str>,
    },
}

/// The hint under an error notice.
const NOTICE_KEYS: &str = "[any key] continue";

struct ScreenState {
    table: TableKind,
    cursor: usize,
    filters: Filters,
    mode: Mode,
}

impl ScreenState {
    fn new(args: &EvalArgs) -> ScreenState {
        ScreenState {
            table: TableKind::Lanes,
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
    // Every flag at its bare default — `cli::eval_is_bare` is what routed
    // this call here in the first place, so this is exactly the invocation
    // that reached `screen` rather than `run`.
    let args = EvalArgs {
        by: EvalBy::Pipeline,
        group: None,
        task: None,
        pipeline: None,
        step: None,
        pipeline_version: None,
        since: None,
        until: None,
        month: None,
        all: false,
        project: None,
        trial: None,
        discard: None,
        force: false,
        csv: false,
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
                    draft.field = (draft.field + 1).min(draft.fields().len() - 1);
                }
                Key::Left | Key::Right => {
                    handle_filter_change(&loaded, draft, key == Key::Right);
                }
                // On a date row, `enter` opens the calendar instead of
                // applying — the one row `enter` means something else on.
                // Everywhere else it applies the draft and reloads.
                Key::Enter
                    if matches!(draft.current(), FilterField::Since | FilterField::Until) =>
                {
                    let field = draft.current();
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
                                keys: Some(NOTICE_KEYS),
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
                    state.table = state.table.other();
                    state.cursor = 0;
                }
                Key::Char('f') => {
                    state.mode = Mode::Filter(Draft::new(state.table, state.filters.clone()));
                }
                Key::Char('r') => match load(repo, &state.filters) {
                    Ok(fresh) => loaded = fresh,
                    Err(err) => {
                        state.mode = Mode::Notice {
                            title: "eval",
                            body: format!("{err:#}"),
                            keys: Some(NOTICE_KEYS),
                        }
                    }
                },
                Key::Char('e') => {
                    match export(repo, &loaded, &state.filters, pipelines, state.table) {
                        Ok((path, rows)) => {
                            state.mode = Mode::Notice {
                                title: "exported",
                                body: format!(
                                    "{}, by {}\n{}",
                                    plural(rows, "row"),
                                    by_label(&state.filters, state.table),
                                    display_relative(&path, &repo.root),
                                ),
                                keys: None,
                            };
                        }
                        Err(err) => {
                            state.mode = Mode::Notice {
                                title: "eval",
                                body: format!("{err:#}"),
                                keys: Some(NOTICE_KEYS),
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
/// writes into on `enter` (pick) or `x` (clear). Never called for a cycled
/// row, which changes through `handle_filter_change` instead and has no
/// string of its own to hand back.
fn field_text_mut(draft: &mut Draft, field: FilterField) -> &mut String {
    match field {
        FilterField::Since => &mut draft.filters.since,
        FilterField::Until => &mut draft.filters.until,
        _ => unreachable!("only since/until open a calendar"),
    }
}

fn handle_filter_change(loaded: &Loaded, draft: &mut Draft, forward: bool) {
    let (field, table) = (draft.current(), draft.table);
    let f = &mut draft.filters;
    match field {
        FilterField::By => match table {
            TableKind::Lanes => f.by = cycle_by(&EvalBy::ALL, f.by, forward),
            TableKind::Dirs => f.dir_by = cycle_by(&DirBy::ALL, f.dir_by, forward),
        },
        FilterField::Group => f.group = cycle_option(&group_candidates(loaded), &f.group, forward),
        FilterField::Task => {
            f.task = cycle_option(&task_candidates(loaded, f), &f.task, forward);
        }
        FilterField::Pipeline => {
            f.pipeline = cycle_option(&pipeline_candidates(loaded), &f.pipeline, forward);
        }
        FilterField::Step => f.step = cycle_option(&step_candidates(loaded, f), &f.step, forward),
        FilterField::Version => {
            f.version = cycle_option(&version_candidates(loaded, f), &f.version, forward);
        }
        FilterField::Dir => f.dir = cycle_option(&dir_candidates(loaded), &f.dir, forward),
        FilterField::Skill => f.skill = cycle_option(&skill_candidates(loaded), &f.skill, forward),
        // Neither answers to `←`/`→` — a calendar is the only way to change
        // either, reached through `enter` instead.
        FilterField::Since | FilterField::Until => {}
    }
    drop_stranded_values(loaded, f);
}

/// Clear any row a change elsewhere has left naming a value that is no
/// longer on offer — a step the newly chosen pipeline never ran, a task the
/// newly chosen group never held. Cleared rather than kept, because `enter`
/// would apply it as a filter that silently matches nothing. Run to a fixed
/// point: clearing a step can bring a task back into range, never the other
/// way round, so two passes always settle it.
fn drop_stranded_values(loaded: &Loaded, f: &mut Filters) {
    for _ in 0..2 {
        if f.step
            .as_ref()
            .is_some_and(|s| !step_candidates(loaded, f).contains(s))
        {
            f.step = None;
        }
        if f.version
            .as_ref()
            .is_some_and(|v| !version_candidates(loaded, f).contains(v))
        {
            f.version = None;
        }
        if f.task
            .as_ref()
            .is_some_and(|t| !task_candidates(loaded, f).contains(t))
        {
            f.task = None;
        }
    }
}

fn filter_field_label(field: FilterField) -> &'static str {
    match field {
        FilterField::By => "by",
        FilterField::Group => "group",
        FilterField::Task => "task",
        FilterField::Pipeline => "pipeline",
        FilterField::Step => "step",
        FilterField::Version => "version",
        FilterField::Dir => "dir",
        FilterField::Skill => "skill",
        FilterField::Since => "since",
        FilterField::Until => "until",
    }
}

/// What a row of the filter panel shows for its own value: `‹ value ›` on
/// every row `←`/`→` cycles, and the plain date — or what a blank one
/// means — on the two a calendar sets, whose missing chevrons are the
/// visible sign that `←`/`→` do nothing there.
fn filter_field_value(draft: &Draft, field: FilterField) -> String {
    let f = &draft.filters;
    let cycled = |v: &Option<String>| format!("‹ {} ›", v.as_deref().unwrap_or("all"));
    match field {
        FilterField::By => format!("‹ {} ›", by_label(f, draft.table)),
        FilterField::Group => cycled(&f.group),
        FilterField::Task => cycled(&f.task),
        FilterField::Pipeline => cycled(&f.pipeline),
        FilterField::Step => cycled(&f.step),
        FilterField::Version => cycled(&f.version),
        FilterField::Dir => cycled(&f.dir),
        FilterField::Skill => cycled(&f.skill),
        FilterField::Since => date_field_value(&f.since, "(blank — the start)"),
        FilterField::Until => date_field_value(&f.until, "(blank — now)"),
    }
}

fn date_field_value(text: &str, placeholder: &str) -> String {
    non_empty(text).unwrap_or(placeholder).to_string()
}

/// The hint lines under the filter panel's rows, the same on every row —
/// which is also what keeps the box one width whichever row the cursor is
/// on, so the table behind it never shifts. The first is the widest line
/// the panel holds, and [`calendar_panel`] pins its own width to it.
const FILTER_HINTS: [&str; 3] = [
    "[enter] on since/until opens a calendar",
    "[↑↓] row   [←→] change",
    "[enter] apply   [esc] back",
];

/// The filter panel itself, as an overlay `draw` centres over the frame —
/// one row per [`FilterField`], the cursor marked on whichever it is on,
/// then the hints. Built with `boxed` rather than `panel`: the mockup draws
/// its three hint lines together under one blank row, where `panel` would
/// set its key line apart by a blank row of its own.
fn filter_panel(draft: &Draft) -> Vec<String> {
    let mut body = Vec::with_capacity(draft.fields().len() + 4);
    for (i, field) in draft.fields().iter().enumerate() {
        let marker = if i == draft.field { ">" } else { " " };
        let label = filter_field_label(*field);
        let value = filter_field_value(draft, *field);
        body.push(format!("{marker} {label:<9} {value}"));
    }
    body.push(String::new());
    body.extend(FILTER_HINTS.iter().map(|h| h.to_string()));
    boxed("filters", &body)
}

/// The day a calendar opens on when `enter` first opens it: the row's own
/// bound, read as a plain `YYYY-MM-DD` — or today when the row is blank or
/// holds anything else, such as a `7d` passed on the command line.
/// Deliberately blind to the ledger, the same as the grid itself.
fn calendar_open_date(text: &str) -> (i32, u32, u32) {
    use chrono::Datelike;
    let today = chrono::Local::now().date_naive();
    let date = non_empty(text)
        .and_then(|t| chrono::NaiveDate::parse_from_str(t, "%Y-%m-%d").ok())
        .unwrap_or(today);
    (date.year(), date.month(), date.day())
}

const CALENDAR_KEYS: &str = "[enter] pick   [esc] back";

/// The month grid `enter` opens on a date row — see [`Mode::Calendar`]. Its
/// width is pinned to the filter panel's own widest line rather than sized
/// to its own narrower content: the calendar opens in the exact spot the
/// filter panel sat in, and a narrower box there would shift the table
/// columns to its right sideways the moment `enter` is pressed, then shift
/// them back the moment it closes.
fn calendar_panel(field: FilterField, year: i32, month: u32, day: u32) -> Vec<String> {
    let width = FILTER_HINTS[0].chars().count();
    let mut body = vec![pad_center(
        &format!("‹ {} {year} ›", month_name(month)),
        width,
    )];
    body.push("    Mo Tu We Th Fr Sa Su".to_string());
    for week in month_weeks(year, month) {
        body.push(format!("    {}", render_week(&week, day)));
    }
    body.push(String::new());
    body.push("[←→] day   [↑↓] week".to_string());
    body.push("[pgup/pgdn] month   [x] no bound".to_string());
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

    fn lane(task: &str, step: &str, round: u32, outcome: Option<&str>) -> Entry {
        Entry {
            ts: "2026-08-04T07:00:00+00:00".into(),
            task: task.into(),
            plan: None,
            step: step.into(),
            pipeline: "default".into(),
            agent: "pi".into(),
            kind: "pi".into(),
            model: "qwen".into(),
            session: format!("{task}-{step}-{round}"),
            round,
            wall_s: 60,
            turns: 1,
            tokens: Tokens::default(),
            cost_usd: Some(1.0),
            ctx_peak: None,
            pipeline_version: "1.0".into(),
            outcome: outcome.map(str::to_string),
            run: Some(format!("r-{task}")),
            trial: None,
            dir: None,
            project: "demo".into(),
        }
    }

    fn no_models() -> BTreeMap<String, ModelPrice> {
        BTreeMap::new()
    }

    fn sized_models(window: usize) -> BTreeMap<String, ModelPrice> {
        let mut models = BTreeMap::new();
        models.insert(
            "sized".to_string(),
            ModelPrice {
                context_window: window,
                ..ModelPrice::default()
            },
        );
        models
    }

    /// Every row `entries` makes under `by`, with fallback keys derived the
    /// way `run` derives them.
    fn rows_by(entries: &[Entry], by: EvalBy) -> Vec<LaneRow> {
        rows_by_with(entries, by, &no_models())
    }

    fn rows_by_with(
        entries: &[Entry],
        by: EvalBy,
        models: &BTreeMap<String, ModelPrice>,
    ) -> Vec<LaneRow> {
        let fallback = fallback_keys(entries);
        let refs: Vec<&Entry> = entries.iter().collect();
        lane_rows(&refs, by, &fallback, models, Some(&Pipelines::builtin()))
    }

    fn one_row(entries: &[Entry]) -> Metrics {
        let rows = rows_by(entries, EvalBy::Pipeline);
        assert_eq!(rows.len(), 1, "one pipeline, one row");
        rows.into_iter().next().unwrap().metrics
    }

    fn total_of(entries: &[Entry]) -> LaneTotal {
        let refs: Vec<&Entry> = entries.iter().collect();
        LaneTotal::of(&refs, &fallback_keys(entries))
    }

    // ------------------------------------------------------------- metrics

    /// The currency lives in the column header, not the cell — a cost cell
    /// is a plain number whether it is a full total or a floor, and `—`
    /// only where nothing at all could be priced.
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
    fn a_row_counts_every_run_that_touched_it() {
        let m = one_row(&[
            lane("login", "implement", 1, Some("pass")),
            lane("login", "review", 1, Some("pass")),
            lane("logout", "implement", 1, Some("pass")),
        ]);
        assert_eq!(m.runs, 2, "login and logout both touched this row");
        assert_eq!(m.lanes, 3);
    }

    #[test]
    fn pass_is_the_share_of_lanes_not_of_runs() {
        let m = one_row(&[
            lane("login", "implement", 1, Some("pass")),
            lane("login", "review", 1, Some("fail")),
            lane("login", "implement", 2, Some("pass")),
            lane("login", "review", 2, Some("pass")),
        ]);
        assert_eq!(m.runs, 1);
        assert_eq!(m.lanes, 4);
        assert_eq!(m.pass_share(), Some(75.0), "one of the four lanes failed");
    }

    #[test]
    fn a_block_is_what_a_lane_reports_not_a_run_grain_judgement() {
        let m = one_row(&[
            lane("login", "implement", 1, Some("block")),
            lane("logout", "implement", 1, Some("pass")),
        ]);
        assert_eq!(m.runs, 2);
        assert_eq!(m.blocked, 1);
        assert_eq!(m.pass_share(), Some(50.0));
    }

    #[test]
    fn a_lane_that_never_reported_is_left_out_of_the_pass_share() {
        let m = one_row(&[
            lane("login", "implement", 1, Some("pass")),
            lane("logout", "implement", 1, None),
        ]);
        assert_eq!(m.lanes, 2, "it still ran, and it still cost something");
        assert_eq!(
            m.pass_share(),
            Some(100.0),
            "one lane reported, and it passed"
        );
    }

    /// A lane held for a person and freed later banks two ledger lines under
    /// the same task, step, round and session — the hold-time line, which
    /// carries the report that parked it, and the freed-pane line, which
    /// carries only what happened after (see gh-378 / issue #380). Both
    /// count as the one lane they are, and the verdict is the block the
    /// hold-time line actually reported, not doubled by the freed line's own
    /// blank one.
    #[test]
    fn a_lane_banked_twice_counts_once_and_keeps_its_own_verdict() {
        let held = lane("login", "implement", 1, Some("block"));
        let mut freed = lane("login", "implement", 1, None);
        freed.wall_s = 15;
        freed.cost_usd = Some(0.5);
        let m = one_row(&[held, freed]);
        assert_eq!(m.lanes, 1, "one lane, whichever of its lines this counts");
        assert_eq!(m.blocked, 1, "the hold-time line's own verdict survives");
        assert_eq!(m.judged, 1);
        assert_eq!(m.time_s, 75, "both lines' own share of the busy time");
        assert_eq!(m.cost, 1.5, "both lines' own share of the cost");
    }

    /// Every per-run figure is the row's total over its distinct runs — two
    /// runs of two lanes each read as one run's worth, not one lane's.
    #[test]
    fn per_run_figures_are_the_row_s_totals_over_its_runs() {
        let mut entries = Vec::new();
        for task in ["login", "logout"] {
            for step in ["implement", "review"] {
                let mut e = lane(task, step, 1, Some("pass"));
                e.tokens = Tokens {
                    input: 100,
                    output: 1_000,
                    cache_read: 10_000,
                    cache_write_5m: 400,
                    cache_write_1h: 100,
                    ..Tokens::default()
                };
                e.cost_usd = Some(if step == "review" { 3.0 } else { 1.0 });
                entries.push(e);
            }
        }
        let m = one_row(&entries);
        assert_eq!(m.runs, 2);
        assert_eq!(m.cost, 8.0, "two lanes at $1 and two at $3");
        assert_eq!(m.cost_per_run(), 4.0);
        assert_eq!(m.tokens_per_run(m.tokens.input), 200);
        assert_eq!(m.tokens_per_run(m.tokens.output), 2_000);
        assert_eq!(m.tokens_per_run(m.tokens.cache_read), 20_000);
        assert_eq!(
            m.tokens_per_run(m.tokens.cache_write()),
            1_000,
            "both cache-write lifetimes, as one class"
        );
        assert_eq!(m.time_per_run_s(), 120.0);
    }

    #[test]
    fn ctx_is_the_largest_peak_on_the_row_as_a_percentage_of_its_own_model_s_window() {
        let mut small = lane("login", "implement", 1, Some("pass"));
        small.model = "sized".into();
        small.ctx_peak = Some(50_000);
        let mut big = lane("logout", "implement", 1, Some("pass"));
        big.model = "sized".into();
        big.ctx_peak = Some(91_000);
        let rows = rows_by_with(&[small, big], EvalBy::Pipeline, &sized_models(100_000));
        let m = &rows[0].metrics;
        assert_eq!(m.ctx_peak_tokens, Some(91_000), "the larger of the two");
        assert_eq!(ctx_cell(m.ctx_peak_tokens, m.ctx_peak_pct), "91%");
    }

    /// `CTX PEAK AVG` is the mean of each lane's own peak — a lane that
    /// banked two lines counts its larger reading once, not both — each as a
    /// share of its model's window.
    #[test]
    fn ctx_peak_avg_is_the_mean_of_each_lane_s_own_peak() {
        let mut a1 = lane("login", "implement", 1, Some("pass"));
        a1.model = "sized".into();
        a1.ctx_peak = Some(20_000);
        let mut a2 = lane("login", "implement", 1, None);
        a2.model = "sized".into();
        a2.ctx_peak = Some(50_000);
        let mut b = lane("logout", "implement", 1, Some("pass"));
        b.model = "sized".into();
        b.ctx_peak = Some(90_000);
        let rows = rows_by_with(&[a1, a2, b], EvalBy::Pipeline, &sized_models(100_000));
        let m = &rows[0].metrics;
        assert_eq!(m.ctx_avg.tokens, Some(70_000.0), "(50k + 90k) / 2 lanes");
        assert!((m.ctx_avg.pct.unwrap() - 0.70).abs() < 1e-9);
        assert_eq!(m.ctx_avg.cell(), "70%");
    }

    #[test]
    fn ctx_falls_back_to_raw_tokens_when_the_model_s_window_is_unconfigured() {
        let mut with_peak = lane("login", "implement", 1, Some("pass"));
        with_peak.model = "unsized".into();
        with_peak.ctx_peak = Some(142_000);
        let m = one_row(&[with_peak]);
        assert_eq!(ctx_cell(m.ctx_peak_tokens, m.ctx_peak_pct), "142k");
        assert_eq!(m.ctx_avg.cell(), "142k");
    }

    #[test]
    fn ctx_is_a_dash_when_no_lane_on_the_row_banked_one() {
        let m = one_row(&[lane("login", "implement", 1, Some("pass"))]);
        assert_eq!(m.ctx_peak_tokens, None);
        assert_eq!(ctx_cell(m.ctx_peak_tokens, m.ctx_peak_pct), "—");
        assert_eq!(m.ctx_avg.cell(), "—");
    }

    /// A zero-token line — an enrolment line, a synthetic turn that
    /// contributed nothing — spends nothing, so it must not be counted as a
    /// line the row's total is missing a price for.
    #[test]
    fn a_zero_token_unpriced_lane_is_not_counted_as_unpriced() {
        let mut priced = lane("login", "implement", 1, Some("pass"));
        priced.cost_usd = Some(5.0);
        let mut zero_token = lane("login", "implement", 2, Some("pass"));
        zero_token.cost_usd = None;
        let m = one_row(&[priced, zero_token]);
        assert_eq!(m.unpriced, 0);
        assert_eq!(m.lanes, 2);
    }

    /// The counterpart: a lane that really spent tokens but named a model
    /// nothing prices must still show up as unpriced — `unpriced` is where
    /// that floor is recorded, even though the cost cell itself prints the
    /// same plain number a full total would.
    #[test]
    fn a_real_unpriced_lane_still_counts() {
        let mut priced = lane("login", "implement", 1, Some("pass"));
        priced.cost_usd = Some(5.0);
        let mut unpriced = lane("login", "implement", 2, Some("pass"));
        unpriced.cost_usd = None;
        unpriced.tokens.input = 10;
        let m = one_row(&[priced, unpriced]);
        assert_eq!(m.unpriced, 1);
        assert_eq!(cost_of(m.cost_per_run(), m.lines, m.unpriced), "5.00");
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
    fn a_token_cell_steps_up_to_billions() {
        assert_eq!(tokens_cell(642), "642");
        assert_eq!(tokens_cell(155_800), "155.8k");
        assert_eq!(tokens_cell(41_910_000), "41.91M");
        assert_eq!(tokens_cell(1_370_000_000), "1.37B");
    }

    // ----------------------------------------------------------- run keys

    #[test]
    fn a_line_with_no_run_falls_back_to_a_key_derived_from_its_task() {
        let mut e = lane("session-resume", "land", 1, Some("pass"));
        e.run = None;
        let entries = vec![e];
        let fallback = fallback_keys(&entries);
        assert_eq!(run_key(&entries[0], &fallback), "session-resume@0804");
    }

    #[test]
    fn two_projects_whose_same_named_task_first_ran_the_same_day_get_distinct_fallback_ids() {
        let alpha = Entry {
            project: "alpha".into(),
            run: None,
            ..lane("login", "implement", 1, Some("pass"))
        };
        let beta = Entry {
            project: "beta".into(),
            run: None,
            ..lane("login", "implement", 1, Some("pass"))
        };
        let entries = vec![alpha, beta];
        let fallback = fallback_keys(&entries);
        assert_eq!(
            fallback[&("alpha".to_string(), "login".to_string())],
            "alpha/login@0804"
        );
        assert_eq!(
            fallback[&("beta".to_string(), "login".to_string())],
            "beta/login@0804"
        );
        let rows = rows_by(&entries, EvalBy::Task);
        assert_eq!(rows.len(), 2, "one row per run, each with its own id");
    }

    // ------------------------------------------------------------- by

    #[test]
    fn by_pipeline_reads_newest_first() {
        let mut default_old = lane("a", "implement", 1, Some("pass"));
        default_old.ts = "2026-08-01T07:00:00+00:00".into();
        let mut local_new = lane("b", "implement", 1, Some("pass"));
        local_new.pipeline = "local".into();
        local_new.ts = "2026-08-10T07:00:00+00:00".into();
        let rows = rows_by(&[default_old, local_new], EvalBy::Pipeline);
        assert_eq!(rows[0].cells, ["local"], "its lane is the newer one");
        assert_eq!(rows[1].cells, ["default"]);
    }

    /// `--by task` is one row per run — the runs view it replaces — naming
    /// the task, the pipeline and version it ran under, and its date.
    #[test]
    fn by_task_is_one_row_per_run_with_its_pipeline_version_and_date() {
        let mut review = lane("login", "review", 1, Some("pass"));
        review.pipeline_version = "1.1".into();
        let rows = rows_by(
            &[lane("login", "implement", 1, Some("pass")), review],
            EvalBy::Task,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metrics.runs, 1);
        assert_eq!(rows[0].metrics.lanes, 2);
        assert_eq!(
            rows[0].cells,
            [
                "login",
                "default",
                "1.1",
                &local_date("2026-08-04T07:00:00+00:00")
            ]
        );
        assert_eq!(rows[0].keys[0], "r-login", "the export carries the run id");
    }

    #[test]
    fn by_group_puts_lanes_with_no_group_under_a_dash() {
        let mut grouped = lane("a", "implement", 1, Some("pass"));
        grouped.plan = Some("audits".into());
        grouped.ts = "2026-08-05T07:00:00+00:00".into();
        let rows = rows_by(
            &[lane("b", "implement", 1, Some("pass")), grouped],
            EvalBy::Group,
        );
        let names: Vec<&str> = rows.iter().map(|r| r.cells[0].as_str()).collect();
        assert_eq!(names, ["audits", NO_GROUP]);
    }

    /// Steps read in their pipeline's own walk order, whatever order the
    /// ledger banked them in, and a step the pipeline no longer names is
    /// kept, after the rest.
    #[test]
    fn by_step_follows_the_pipeline_s_walk_order_and_keeps_a_retired_step() {
        let entries = vec![
            lane("a", "retired-step", 1, Some("pass")),
            lane("a", "review", 1, Some("pass")),
            lane("a", "implement", 1, Some("pass")),
        ];
        let rows = rows_by(&entries, EvalBy::Step);
        let steps: Vec<&str> = rows.iter().map(|r| r.cells[1].as_str()).collect();
        assert_eq!(steps, ["implement", "review", "retired-step"]);
    }

    /// Newest version first, compared as numbers — `1.10` is above `1.9` —
    /// and `FIRST` is the version's earliest lane, not its latest.
    #[test]
    fn by_version_reads_highest_version_first_and_first_is_its_earliest_lane() {
        let mut old = lane("a", "implement", 1, Some("pass"));
        old.pipeline_version = "1.9".into();
        old.ts = "2026-08-01T07:00:00+00:00".into();
        let mut new_early = lane("b", "implement", 1, Some("pass"));
        new_early.pipeline_version = "1.10".into();
        new_early.ts = "2026-08-02T07:00:00+00:00".into();
        let mut new_late = lane("c", "implement", 1, Some("pass"));
        new_late.pipeline_version = "1.10".into();
        new_late.ts = "2026-08-09T07:00:00+00:00".into();
        let rows = rows_by(&[old, new_early, new_late], EvalBy::Version);
        let versions: Vec<&str> = rows.iter().map(|r| r.cells[1].as_str()).collect();
        assert_eq!(versions, ["1.10", "1.9"]);
        assert_eq!(rows[0].cells[2], local_date("2026-08-02T07:00:00+00:00"));
        assert_eq!(rows[0].metrics.runs, 2);
    }

    #[test]
    fn lane_filters_narrow_by_every_row() {
        let mut e = lane("login", "review", 1, Some("pass"));
        e.plan = Some("audits".into());
        e.pipeline_version = "1.1".into();
        assert!(LaneFilters::default().admits(&e));
        let all = LaneFilters {
            group: Some("audits"),
            task: Some("login"),
            pipeline: Some("default"),
            step: Some("review"),
            version: Some("1.1"),
        };
        assert!(all.admits(&e));
        for miss in [
            LaneFilters {
                group: Some("other"),
                ..Default::default()
            },
            LaneFilters {
                task: Some("other"),
                ..Default::default()
            },
            LaneFilters {
                pipeline: Some("other"),
                ..Default::default()
            },
            LaneFilters {
                step: Some("other"),
                ..Default::default()
            },
            LaneFilters {
                version: Some("1.0"),
                ..Default::default()
            },
        ] {
            assert!(!miss.admits(&e));
        }
    }

    // ----------------------------------------------------------- the table

    /// The mockup's own header lines, drawn from row names as wide as the
    /// mockup's widest: only the naming columns differ between them, and
    /// the figure columns are one fixed block after them. The mockup puts
    /// three columns between the last name and `RUNS` under pipeline, group
    /// and version, but two under task and four under step; this holds all
    /// five to the three most of them draw, so `RUNS` is one column right of
    /// the task mockup and one left of the step mockup.
    #[test]
    fn every_by_s_header_is_the_mockup_s_own() {
        let named = |cells: &[&str]| LaneRow {
            cells: cells.iter().map(|c| c.to_string()).collect(),
            keys: Vec::new(),
            project: "spoolway".into(),
            first_ts: String::new(),
            last_ts: String::new(),
            metrics: Metrics::default(),
        };
        let total = LaneTotal {
            runs: 0,
            blocked: 0,
            cost: 0.0,
            lines: 0,
            unpriced: 0,
        };
        const FIGURES: &str = "RUNS  PASS  BLOCKS  CTX PEAK AVG  CTX PEAK  IN/RUN  OUT/RUN  \
                               CACHE R/RUN  CACHE W/RUN       USD  USD/RUN  TIME/RUN";
        let cases: [(EvalBy, &[&str], &str); 5] = [
            (EvalBy::Pipeline, &["impl_fast"], "PIPELINE    "),
            (
                EvalBy::Group,
                &["command-lane-stuck-reporting"],
                "GROUP                          ",
            ),
            (
                EvalBy::Step,
                &["impl_ui", "implement"],
                "PIPELINE  STEP        ",
            ),
            (
                EvalBy::Version,
                &["impl_ui", "1.1", "2026-09-26"],
                "PIPELINE  VERSION  FIRST        ",
            ),
            (
                EvalBy::Task,
                &["release-spoolway-1", "impl_tdd", "1.0", "2026-09-16"],
                "TASK                PIPELINE  VER  WHEN         ",
            ),
        ];
        for (by, cells, naming) in cases {
            let table = lanes_table(by, &[named(cells)], &total);
            assert_eq!(
                table.header,
                format!("{naming}{FIGURES}"),
                "by {}",
                by.label()
            );
        }
    }

    /// The mockup's own pipeline row, rebuilt from the figures behind it.
    #[test]
    fn a_row_draws_its_figures_under_the_mockup_s_columns() {
        let mut entries = Vec::new();
        for n in 0..2 {
            let mut e = lane(&format!("t{n}"), "implement", 1, Some("pass"));
            e.pipeline = "impl".into();
            e.model = "sized".into();
            e.ctx_peak = Some(if n == 0 { 12_000 } else { 52_000 });
            e.tokens = Tokens {
                input: 642,
                output: 155_800,
                cache_read: 41_910_000,
                cache_write_5m: 799_400,
                ..Tokens::default()
            };
            e.cost_usd = Some(16.90);
            e.wall_s = 4_320;
            entries.push(e);
        }
        let rows = rows_by_with(&entries, EvalBy::Pipeline, &sized_models(100_000));
        let table = lanes_table(EvalBy::Pipeline, &rows, &total_of(&entries));
        assert_eq!(
            table.rows[0],
            "impl          2  100%       0           32%       52%     642   155.8k       \
             41.91M       799.4k     33.80    16.90    1h 12m"
        );
        assert_eq!(
            table.total,
            "Total         2             0                                                                        33.80"
        );
    }

    /// `RUNS` on the `Total` line is distinct runs over the whole table: a
    /// run under `--by step` touches every step it ran, and summing the
    /// rows would count it once per step. `BLOCKS` and `USD` do sum.
    #[test]
    fn the_total_line_carries_distinct_runs_blocks_and_usd_only() {
        let entries = vec![
            lane("login", "implement", 1, Some("block")),
            lane("login", "review", 1, Some("pass")),
            lane("logout", "implement", 1, Some("pass")),
        ];
        let rows = rows_by(&entries, EvalBy::Step);
        assert_eq!(rows.iter().map(|r| r.metrics.runs).sum::<usize>(), 3);
        let total = total_of(&entries);
        assert_eq!(total.runs, 2, "two runs, however many steps they touched");
        assert_eq!(total.blocked, 1);
        assert_eq!(total.cost, 3.0);
        let table = lanes_table(EvalBy::Step, &rows, &total);
        assert!(table.total.starts_with("Total"), "{:?}", table.total);
        let cells: Vec<&str> = table.total.split_whitespace().collect();
        assert_eq!(cells, ["Total", "2", "1", "3.00"], "{:?}", table.total);
    }

    #[test]
    fn project_earns_a_column_only_once_two_are_on_screen() {
        let one = rows_by(
            &[
                lane("login", "implement", 1, Some("pass")),
                lane("logout", "implement", 1, Some("pass")),
            ],
            EvalBy::Pipeline,
        );
        let total = total_of(&[]);
        assert!(
            !lanes_table(EvalBy::Pipeline, &one, &total)
                .header
                .contains("PROJECT")
        );

        // Different pipelines, so alpha and beta each get their own row —
        // sharing one would merge them into a single row naming only the
        // first-seen project.
        let alpha = Entry {
            project: "alpha".into(),
            ..lane("login", "implement", 1, Some("pass"))
        };
        let beta = Entry {
            project: "beta".into(),
            pipeline: "local".into(),
            ..lane("logout", "implement", 1, Some("pass"))
        };
        let two = rows_by(&[alpha, beta], EvalBy::Pipeline);
        let table = lanes_table(EvalBy::Pipeline, &two, &total);
        assert!(table.header.starts_with("PROJECT"), "{}", table.header);
    }

    #[test]
    fn a_trial_delta_line_names_both_arms_and_the_direction_each_figure_moved() {
        let baseline = lane("solo-1", "implement", 1, Some("pass"));
        let mut challenger = lane("solo-2", "implement", 1, Some("fail"));
        challenger.cost_usd = Some(3.0);
        challenger.wall_s = 120;
        let rows = rows_by(&[baseline, challenger], EvalBy::Task);
        let (base, other) = match rows[0].cells[0].as_str() {
            "solo-1" => (&rows[0], &rows[1]),
            _ => (&rows[1], &rows[0]),
        };
        let line = trial_delta_line(base, other);
        assert!(line.starts_with("solo-2 vs solo-1: "), "{line:?}");
        assert!(line.contains("pass -100pp"), "{line:?}");
        assert!(line.contains("cost +$2.00"), "{line:?}");
        assert!(line.contains("time +1m"), "{line:?}");
    }

    // ------------------------------------------------------------- export

    /// The mockup's own `--csv` header for `--by step`, with the four
    /// per-run token columns beside the raw totals.
    #[test]
    fn a_lanes_export_carries_totals_beside_per_run_figures_and_marks_its_total() {
        assert_eq!(
            lanes_csv_header(EvalBy::Step),
            "project,by,pipeline,step,pipeline_version,runs,lanes,pass,blocks,ctx_peak_tokens,\
             ctx_peak_pct,ctx_peak_avg_tokens,ctx_peak_avg_pct,in_tokens,out_tokens,\
             cache_read_tokens,cache_write_tokens,in_per_run,out_per_run,cache_read_per_run,\
             cache_write_per_run,cost_usd,cost_per_run,unpriced,time_s,time_per_run_s"
        );
        let mut entries = vec![
            lane("login", "implement", 1, Some("block")),
            lane("logout", "implement", 1, Some("pass")),
        ];
        entries[1].pipeline_version = "1.1".into();
        entries[0].tokens.input = 300;
        let rows = rows_by(&entries, EvalBy::Step);
        let header_cols = lanes_csv_header(EvalBy::Step).split(',').count();
        let line = lanes_csv_row(EvalBy::Step, &rows[0]);
        assert_eq!(line.split(',').count(), header_cols, "{line}");
        assert!(
            line.starts_with(
                "demo,step,default,implement,,2,2,0.50,1,,,,,300,0,0,0,150,0,0,0,2.00,1.00,0,120,60"
            ),
            "two versions on one row name neither: {line}"
        );

        let total = lanes_csv_total(EvalBy::Step, &total_of(&entries));
        assert_eq!(total.split(',').count(), header_cols, "{total}");
        assert_eq!(total, ",total,,,,2,,,1,,,,,,,,,,,,,2.00,,,,");
    }

    /// `--json` keeps the `Total` line out of `rows`, where a consumer
    /// iterating them could read it as one more.
    #[test]
    fn a_lanes_json_keeps_the_total_apart_from_the_rows() {
        let mut unpriced = lane("login", "implement", 1, Some("pass"));
        unpriced.cost_usd = None;
        unpriced.tokens.input = 10;
        let entries = vec![unpriced, lane("logout", "implement", 1, Some("pass"))];
        let rows = rows_by(&entries, EvalBy::Pipeline);
        let json = lanes_json(EvalBy::Pipeline, &rows, &total_of(&entries));
        assert_eq!(json["by"], "pipeline");
        assert_eq!(json["rows"].as_array().unwrap().len(), 1);
        let row = &json["rows"][0];
        assert_eq!(row["pipeline"], "default");
        assert_eq!(row["pipeline_version"], "1.0");
        assert_eq!(row["in_per_run"], 5);
        assert_eq!(row["unpriced"], 1);
        // A plain number, not `null`: only a *wholly* unpriced row nulls it.
        assert_eq!(row["cost_usd"], 1.0);
        assert_eq!(json["total"]["runs"], 2);
        assert_eq!(json["total"]["cost_usd"], 1.0);
    }

    // -------------------------------------------------------- directories

    /// A directory line, the way `usage::sweep`'s directory walk banks one:
    /// no `task`, `step`, `pipeline`, `agent`, `outcome`, `run` or
    /// `pipeline_version` — see `Entry::dir`.
    fn dir_line(dir: &str, session: &str, ts: &str, cost: f64) -> Entry {
        Entry {
            ts: ts.into(),
            task: String::new(),
            plan: None,
            step: String::new(),
            pipeline: String::new(),
            agent: String::new(),
            kind: "claude".into(),
            model: "claude-opus-5".into(),
            session: session.into(),
            round: 0,
            wall_s: 0,
            turns: 1,
            tokens: Tokens {
                output: 10,
                ..Tokens::default()
            },
            cost_usd: Some(cost),
            ctx_peak: None,
            pipeline_version: String::new(),
            outcome: None,
            run: None,
            trial: None,
            dir: Some(dir.into()),
            project: "demo".into(),
        }
    }

    /// A session the sweep caught across two passes is one row under
    /// `by session`, its cost and tokens the sum of what each pass banked.
    #[test]
    fn list_sessions_dedupes_a_session_the_sweep_banked_across_several_passes() {
        let a = dir_line("proj", "s1", "2026-09-01T09:00:00+00:00", 0.10);
        let b = dir_line("proj", "s1", "2026-09-01T09:05:00+00:00", 0.20);
        let rows = list_sessions(&[&a, &b], &no_models(), &HashMap::new(), &HashMap::new());
        assert_eq!(rows.len(), 1, "one session, however many sweeps banked it");
        assert!((rows[0].cost - 0.30).abs() < 1e-9);
        assert_eq!(rows[0].tokens.output, 20);
        assert_eq!(rows[0].lines, 2);
        assert_eq!(rows[0].skill, "—", "a session that ran no skill");
    }

    #[test]
    fn the_skill_cell_names_the_first_skill_and_counts_the_rest() {
        let set = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>();
        assert_eq!(skill_cell(None), "—");
        assert_eq!(skill_cell(Some(&set(&[]))), "—");
        assert_eq!(skill_cell(Some(&set(&["/spoolway-plan"]))), "spoolway-plan");
        assert_eq!(
            skill_cell(Some(&set(&["/spoolway-plan", "/code-review", "/simplify"]))),
            "code-review +2"
        );
        assert_eq!(
            clip_cell("merge-pull-request-now", SESSIONS_SKILL_WIDTH),
            "merge-pull-reque…"
        );
        assert_eq!(
            clip_cell("spoolway-plan +2", SESSIONS_SKILL_WIDTH),
            "spoolway-plan +2"
        );
    }

    /// `SESSIONS` counts distinct session ids, not ledger rows, and a watched
    /// root nothing has run in yet is a row of zero rather than missing.
    #[test]
    fn dir_rows_count_distinct_sessions_and_keep_a_root_with_none() {
        let a = dir_line("/w/proj", "s1", "2026-09-01T09:00:00+00:00", 0.10);
        let b = dir_line("/w/proj", "s1", "2026-09-01T09:05:00+00:00", 0.20);
        let c = dir_line("/w/proj", "s2", "2026-09-02T09:00:00+00:00", 0.50);
        let roots = vec!["/w/proj".to_string(), "/w/quiet".to_string()];
        let rows = dir_rows(&[&a, &b, &c], &roots, &no_models(), &HashMap::new());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].dir, "/w/proj");
        assert_eq!(rows[0].sessions, 2, "s1 across two lines and s2");
        assert!((rows[0].cost - 0.80).abs() < 1e-9);
        assert_eq!(
            rows[1].dir, "/w/quiet",
            "a root with no sessions sorts last"
        );
        assert_eq!(rows[1].sessions, 0);
        let table = dirs_table(&rows);
        assert!(
            table.rows[1].split_whitespace().skip(2).all(|c| c == "—"),
            "nothing per session to divide: {}",
            table.rows[1]
        );
    }

    #[test]
    fn dir_rows_orders_by_latest_activity() {
        let old = dir_line("notes", "s1", "2026-09-01T09:00:00+00:00", 0.10);
        let recent = dir_line("spoolway", "s2", "2026-09-10T09:00:00+00:00", 0.20);
        let rows = dir_rows(&[&old, &recent], &[], &no_models(), &HashMap::new());
        assert_eq!(
            rows.iter().map(|r| r.dir.as_str()).collect::<Vec<_>>(),
            ["spoolway", "notes"]
        );
    }

    /// The mockup's own `by dir` header and `Total` line: the session count
    /// and USD, and nothing that does not add up.
    #[test]
    fn the_dirs_table_is_the_mockup_s_own() {
        let a = dir_line(
            "/home/lima.guest/spoolway",
            "s1",
            "2026-09-01T09:00:00+00:00",
            0.70,
        );
        let b = dir_line(
            "/home/lima.guest/notes",
            "s2",
            "2026-09-01T08:00:00+00:00",
            0.18,
        );
        let rows = dir_rows(&[&a, &b], &[], &no_models(), &HashMap::new());
        let table = dirs_table(&rows);
        assert_eq!(
            table.header,
            "DIR                         SESSIONS  IN/SESSION  OUT/SESSION  CACHE R/SESSION  \
             CACHE W/SESSION       USD  USD/SESSION  CTX PEAK AVG  CTX PEAK  TIME/SESSION"
        );
        assert_eq!(
            table.total.split_whitespace().collect::<Vec<_>>(),
            ["Total", "2", "0.88"]
        );
    }

    /// The mockup's own `by session` header, and a `Total` line carrying
    /// every token class, USD and time.
    #[test]
    fn the_sessions_table_is_the_mockup_s_own() {
        let mut a = dir_line(
            "/home/lima.guest/spoolway",
            "s1",
            "2026-09-01T09:00:00+00:00",
            3.81,
        );
        a.tokens = Tokens {
            input: 144,
            output: 35_100,
            cache_read: 9_450_000,
            cache_write_5m: 180_200,
            ..Tokens::default()
        };
        let b = dir_line(
            "/home/lima.guest/notes",
            "s2",
            "2026-09-01T08:00:00+00:00",
            0.90,
        );
        let mut skills = HashMap::new();
        skills.insert(
            "s1".to_string(),
            ["/spoolway-plan".to_string()].into_iter().collect(),
        );
        let mut rows = list_sessions(&[&a, &b], &no_models(), &HashMap::new(), &skills);
        rows.reverse();
        let table = sessions_table(&rows);
        assert_eq!(
            table.header,
            "WHEN           DIR       SKILL              MODEL                 IN      OUT   \
             CACHE R   CACHE W    USD      TIME"
        );
        let first: Vec<&str> = table.rows[0].split_whitespace().collect();
        assert_eq!(
            &first[2..],
            [
                "spoolway",
                "spoolway-plan",
                "claude-opus-5",
                "144",
                "35.1k",
                "9.45M",
                "180.2k",
                "3.81",
                "0s"
            ]
        );
        assert_eq!(
            table.total.split_whitespace().collect::<Vec<_>>(),
            ["Total", "144", "35.1k", "9.45M", "180.2k", "4.71", "0s"]
        );
    }

    #[test]
    fn a_directory_export_marks_its_total_under_either_by() {
        let a = dir_line("/w/proj", "s1", "2026-09-01T09:00:00+00:00", 0.70);
        let dirs = dir_rows(&[&a], &[], &no_models(), &HashMap::new());
        let sessions = list_sessions(&[&a], &no_models(), &HashMap::new(), &HashMap::new());
        let cols = DIRS_CSV_HEADER.split(',').count();
        assert_eq!(csv_dir_row(&dirs[0]).split(',').count(), cols);
        let total = csv_dirs_total(DirBy::Dir, &dirs, &sessions);
        assert_eq!(total.split(',').count(), cols);
        assert!(total.starts_with("total,,1,"), "{total}");

        let cols = SESSIONS_CSV_HEADER.split(',').count();
        assert_eq!(csv_session_row(&sessions[0]).split(',').count(), cols);
        let total = csv_dirs_total(DirBy::Session, &[], &sessions);
        assert_eq!(total, "total,,,,,0,10,0,0,0.70,,,,0");
    }
}

#[cfg(test)]
mod screen_tests {
    use super::*;
    use crate::commands::testutil::fixture;

    fn keys(s: &str) -> std::io::Cursor<Vec<u8>> {
        std::io::Cursor::new(s.as_bytes().to_vec())
    }

    const DOWN: &str = "\x1b[B";
    const RIGHT: &str = "\x1b[C";

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
                session: format!("{task}-{step}"),
                round: 1,
                wall_s: 60,
                turns: 1,
                tokens: Tokens::default(),
                cost_usd: Some(cost),
                ctx_peak: None,
                pipeline_version: "1.0".into(),
                outcome: outcome.map(str::to_string),
                run: Some(format!("r-{task}")),
                trial: None,
                dir: None,
                project: String::new(),
            },
        )
        .unwrap();
    }

    /// The default `EvalArgs` — nothing filtered, nothing preset — that
    /// every screen test opens with, exactly as `screen` builds it.
    fn no_args() -> EvalArgs {
        EvalArgs {
            by: EvalBy::Pipeline,
            group: None,
            task: None,
            pipeline: None,
            step: None,
            pipeline_version: None,
            since: None,
            until: None,
            month: None,
            all: false,
            project: None,
            trial: None,
            discard: None,
            force: false,
            csv: false,
        }
    }

    fn no_filters() -> Filters {
        Filters::from_args(&no_args())
    }

    /// A `Loaded` over `entries` with its fallback keys computed and nothing
    /// else set — the shape every pure-view test wants.
    fn loaded(entries: Vec<Entry>) -> Loaded {
        Loaded {
            fallback: fallback_keys(&entries),
            entries,
            dirs: Vec::new(),
            roots: Vec::new(),
            skills_by_session: HashMap::new(),
            spans_by_session: HashMap::new(),
            models: BTreeMap::new(),
            scope_label: "demo".to_string(),
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
            1.0,
            Some("pass"),
        );
        repo
    }

    /// `load` retains only `Entry::is_lane` over the real ledger — pinned
    /// against a fixture repo holding both an interactive line and a lane
    /// line, on disk, so deleting `load`'s own filter breaks this rather
    /// than an out-of-band replay of the same predicate.
    #[test]
    fn load_never_returns_an_interactive_line() {
        let repo = fixture_with_one_run("load-excludes-interactive");
        crate::usage::append(
            &repo,
            &Entry {
                ts: "2026-08-01T09:30:00+00:00".into(),
                task: String::new(),
                plan: None,
                step: String::new(),
                pipeline: String::new(),
                agent: crate::usage::INTERACTIVE_AGENT.into(),
                kind: "claude".into(),
                model: "claude-opus-5".into(),
                session: "interactive-s".into(),
                round: 1,
                wall_s: 0,
                turns: 1,
                tokens: Tokens::default(),
                cost_usd: Some(2.0),
                ctx_peak: None,
                pipeline_version: String::new(),
                outcome: None,
                run: None,
                trial: None,
                dir: None,
                project: String::new(),
            },
        )
        .unwrap();

        let loaded = load(&repo, &no_filters()).unwrap();
        assert_eq!(loaded.entries.len(), 1, "the interactive line stayed out");
        assert!(loaded.entries.iter().all(Entry::is_lane));
        assert_eq!(
            loaded.roots,
            [repo
                .root
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned()],
            "the project's own directory is always watched"
        );
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

    /// Every draw opens on a clear-screen, so a whole captured transcript
    /// holds every frame the screen ever drew, back to back — a plain
    /// `text.contains(...)` over it can true positive on a state that has
    /// since moved on. This is the frame that was actually on screen when
    /// the input ran out.
    fn last_frame(text: &str) -> &str {
        text.rsplit("\x1b[2J\x1b[H").next().unwrap_or(text)
    }

    /// Regression: the note used to take a row `frame_rows` had not made
    /// room for, so drawing it on a real terminal scrolled the frame's own
    /// top border off screen. Pinned directly since `terminal_size` reads
    /// `None` in this harness and so never exercises `frame_rows` itself.
    #[test]
    fn frame_chrome_reserves_one_more_row_per_note_on_screen() {
        assert_eq!(frame_chrome(0), 4);
        assert_eq!(frame_chrome(1), 5);
        assert_eq!(frame_chrome(2), 6);
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
        current = cycle_option(&candidates, &current, true);
        assert_eq!(
            current.as_deref(),
            Some("default"),
            "past the last, nothing"
        );
        current = cycle_option(&candidates, &current, false);
        current = cycle_option(&candidates, &current, false);
        assert_eq!(current, None, "back past the first candidate is `all`");
        current = cycle_option(&candidates, &current, false);
        assert_eq!(current, None);
    }

    #[test]
    fn the_by_row_cycles_every_key_and_stops_at_either_end() {
        assert_eq!(cycle_by(&EvalBy::ALL, EvalBy::Pipeline, true), EvalBy::Step);
        assert_eq!(
            cycle_by(&EvalBy::ALL, EvalBy::Version, true),
            EvalBy::Version
        );
        assert_eq!(cycle_by(&EvalBy::ALL, EvalBy::Group, false), EvalBy::Group);
        assert_eq!(cycle_by(&DirBy::ALL, DirBy::Dir, true), DirBy::Session);
    }

    /// The `task` row cycles only the tasks every other row leaves on the
    /// table, and a change elsewhere that strands the chosen task clears it.
    #[test]
    fn the_task_row_cycles_only_the_tasks_the_other_rows_leave() {
        let mut other = tests_entry("b", "implement");
        other.pipeline = "bugfix".into();
        let loaded = loaded(vec![tests_entry("a", "implement"), other]);
        let mut filters = no_filters();
        assert_eq!(task_candidates(&loaded, &filters), ["a", "b"]);
        filters.pipeline = Some("bugfix".into());
        assert_eq!(task_candidates(&loaded, &filters), ["b"]);

        let mut draft = Draft::new(TableKind::Lanes, no_filters());
        draft.filters.task = Some("a".into());
        draft.field = 3; // pipeline
        handle_filter_change(&loaded, &mut draft, true);
        assert_eq!(draft.filters.pipeline.as_deref(), Some("bugfix"));
        assert_eq!(draft.filters.task, None, "`a` never ran on bugfix");
    }

    fn tests_entry(task: &str, step: &str) -> Entry {
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
            round: 1,
            wall_s: 60,
            turns: 1,
            tokens: Tokens::default(),
            cost_usd: Some(1.0),
            ctx_peak: None,
            pipeline_version: "1.0".into(),
            outcome: Some("pass".into()),
            run: None,
            trial: None,
            dir: None,
            project: "demo".into(),
        }
    }

    /// The top border's right side names the project and, once one is set,
    /// each filter and the window.
    #[test]
    fn filters_label_names_the_project_every_filter_and_the_window() {
        let loaded = loaded(Vec::new());
        let filters = Filters {
            since: "2026-08-01".to_string(),
            step: Some("review".into()),
            skill: Some("/x".into()),
            ..no_filters()
        };
        assert_eq!(
            filters_label(&loaded, &filters, TableKind::Lanes),
            "demo · step review · 2026-08-01 → now"
        );
        assert_eq!(
            filters_label(&loaded, &filters, TableKind::Dirs),
            "demo · skill /x · 2026-08-01 → now"
        );
    }

    // ------------------------------------------------------------- render

    #[test]
    fn render_lines_marks_the_cursor_s_row() {
        let lines = vec![
            Line::Text("HEADER".to_string()),
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

    // --------------------------------------------------------- run_screen

    #[test]
    fn an_empty_ledger_says_so_and_the_screen_ends_when_input_runs_out() {
        let repo = fixture("screen-empty");
        let text = screen(&repo, "");
        assert!(text.contains("Nothing to compare"), "{text}");
    }

    /// Bare `spoolway eval` opens on the lanes table by pipeline, with its
    /// `Total` line and the bracketed key line naming `tab`'s other table.
    #[test]
    fn the_screen_opens_by_pipeline_with_a_total_and_bracketed_keys() {
        let repo = fixture_with_one_run("screen-opens");
        let text = screen(&repo, "q");
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by pipeline "), "{last}");
        assert!(last.contains("│ PIPELINE"), "{last}");
        assert!(last.contains("│>default"), "{last}");
        assert!(last.contains("│ Total"), "{last}");
        assert!(
            last.contains(
                "[↑↓] move   [tab] dirs   [f] filters   [e] export   [r] refresh   [q] quit"
            ),
            "{last}"
        );
    }

    /// `tab` moves between the two tables and back, nothing else.
    #[test]
    fn tab_switches_between_the_lanes_and_the_directories() {
        let repo = fixture_with_one_run("screen-tab");
        let text = screen(&repo, "\t");
        let dirs = last_frame(&text);
        assert!(dirs.contains("┌─ eval · by dir "), "{dirs}");
        assert!(dirs.contains("│ DIR"), "{dirs}");
        assert!(dirs.contains("[tab] lanes"), "{dirs}");
        let root = repo.root.canonicalize().unwrap();
        assert!(
            dirs.contains(&*root.to_string_lossy()),
            "the project's own directory is a row even with no sessions\n{dirs}"
        );

        let text = screen(&repo, "\t\t");
        assert!(
            last_frame(&text).contains("┌─ eval · by pipeline "),
            "{text}"
        );
    }

    /// A lane nothing could price prints the "Cost is a floor" note between
    /// the frame's own bottom border and the keys line — never inside the
    /// frame, where it would cost the table a body row.
    #[test]
    fn the_screen_prints_the_unpriced_note_under_the_frame_not_inside_it() {
        let repo = fixture("screen-unpriced-note");
        let mut e = tests_entry("a", "implement");
        e.model = "some-local-model".into();
        e.cost_usd = None;
        e.tokens.input = 10;
        e.project = String::new();
        crate::usage::append(&repo, &e).unwrap();

        let text = screen(&repo, "q");
        let last = last_frame(&text);
        let note = "Cost is a floor — no price configured for: some-local-model";
        let border = last.rfind('└').expect("the frame's own bottom border");
        let note_at = last.find(note).expect("the note");
        let footer_at = last.find("[↑↓] move").expect("the keys line");
        assert!(border < note_at && note_at < footer_at, "{last}");
    }

    /// `e` exports the table on screen to a file named after its `by`, and
    /// the confirmation names the rows and the file it wrote.
    #[test]
    fn e_exports_the_table_to_a_file_named_after_its_by() {
        let repo = fixture_with_one_run("screen-export-key");
        let text = screen(&repo, "eq");
        let shown = std::path::PathBuf::from(".spoolway")
            .join("evals")
            .join("eval-by-pipeline-");
        assert!(text.contains("┌─ exported "), "{text}");
        assert!(text.contains("1 row, by pipeline"), "{text}");
        assert!(text.contains(&shown.display().to_string()), "{text}");

        let dir = repo.root.join(".spoolway").join("evals");
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "exactly one export was written");
        let body = std::fs::read_to_string(files[0].path()).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines[0], lanes_csv_header(EvalBy::Pipeline));
        assert!(lines[1].contains(",pipeline,default,1.0,1,1,"), "{body}");
        assert!(lines[2].starts_with(",total,"), "{body}");
        assert_eq!(lines.len(), 3, "one header, one row, one total");
    }

    #[test]
    fn e_on_the_directory_table_writes_its_own_header() {
        let repo = fixture("export-dirs-and-sessions");
        bank_dir(&repo, "2026-09-01T09:00:00+00:00", "spoolway", "s1", 0.70);
        let mut filters = no_filters();
        let loaded = load(&repo, &filters).unwrap();
        let pipelines = Pipelines::builtin();

        let (path, rows) = export(&repo, &loaded, &filters, &pipelines, TableKind::Dirs).unwrap();
        assert_eq!(rows, 2, "the session's dir and the project's own root");
        assert!(path.to_string_lossy().contains("eval-by-dir-"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with(DIRS_CSV_HEADER), "{body}");
        assert!(
            body.contains("dir,spoolway,1,0,10,0,0,0,10,0,0,0.70,0.70,"),
            "{body}"
        );

        filters.dir_by = DirBy::Session;
        let (path, rows) = export(&repo, &loaded, &filters, &pipelines, TableKind::Dirs).unwrap();
        assert_eq!(rows, 1);
        assert!(path.to_string_lossy().contains("eval-by-session-"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with(SESSIONS_CSV_HEADER), "{body}");
        assert!(
            body.contains(",spoolway,—,claude-opus-5,0,10,0,0,0.70,"),
            "{body}"
        );
        assert!(body.contains("\ntotal,"), "{body}");
    }

    /// Nothing on the filter panel is typed into, so backspace has no row to
    /// erase from: it must fall through and do nothing, never reach
    /// `field_text_mut`'s `unreachable!()`.
    #[test]
    fn backspace_on_the_filter_panel_does_nothing_rather_than_panicking() {
        let repo = fixture("screen-backspace");
        screen(&repo, "f\x7fq");
    }

    /// A small ledger draws a table only a few lines tall — shorter than
    /// either overlay panel. `draw` has to pad the frame's own body out to
    /// fit whichever panel is on top of it, or `overlay` writes past the
    /// frame's last row and the panel loses its own bottom border.
    #[test]
    fn a_short_table_still_fits_the_taller_overlay_panels_whole() {
        let repo = fixture_with_one_run("screen-short-table");
        let last = last_frame(&screen(&repo, "fq")).to_string();
        assert!(last.contains("[enter] apply   [esc] back"), "{last}");
        assert!(last.contains("└───"), "{last}");
    }

    /// The lanes panel draws its eight rows in order, `by` first, every one
    /// blank when the screen opens, and the directory panel its five.
    #[test]
    fn the_filter_panels_draw_their_rows_by_first_every_one_blank() {
        let draft = Draft::new(TableKind::Lanes, no_filters());
        let panel = filter_panel(&draft);
        let rows: Vec<String> = panel[1..9]
            .iter()
            .map(|l| l.trim_matches('│').trim().to_string())
            .collect();
        assert_eq!(
            rows,
            [
                "> by        ‹ pipeline ›",
                "group     ‹ all ›",
                "task      ‹ all ›",
                "pipeline  ‹ all ›",
                "step      ‹ all ›",
                "version   ‹ all ›",
                "since     (blank — the start)",
                "until     (blank — now)",
            ]
        );
        let text = panel.join("\n");
        for hint in FILTER_HINTS {
            assert!(text.contains(hint), "{hint}\n{text}");
        }

        let draft = Draft::new(TableKind::Dirs, no_filters());
        let text = filter_panel(&draft).join("\n");
        for row in [
            "> by        ‹ dir ›",
            "dir       ‹ all ›",
            "skill     ‹ all ›",
        ] {
            assert!(text.contains(row), "{row}\n{text}");
        }
        assert!(!text.contains("pipeline"), "{text}");
    }

    /// The box draws the same width whichever row the cursor is on, so the
    /// table behind it never shifts — and the calendar opens at that width
    /// too.
    #[test]
    fn the_filter_panel_and_its_calendar_are_one_width_on_every_row() {
        let widths: Vec<usize> = (0..filter_fields(TableKind::Lanes).len())
            .map(|field| {
                let mut draft = Draft::new(TableKind::Lanes, no_filters());
                draft.field = field;
                filter_panel(&draft)[0].chars().count()
            })
            .collect();
        assert!(widths.iter().all(|w| *w == widths[0]), "{widths:?}");
        let calendar = calendar_panel(FilterField::Since, 2026, 9, 25);
        assert_eq!(
            calendar[0].chars().count(),
            widths[0],
            "{}",
            calendar.join("\n")
        );
    }

    /// `f`, `→` on the `by` row, `enter`: the lanes table regroups by step,
    /// and the top border says so.
    #[test]
    fn the_by_row_regroups_the_table_once_applied() {
        let repo = fixture_with_one_run("screen-by-step");
        let text = screen(&repo, &format!("f{RIGHT}\rq"));
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by step "), "{last}");
        assert!(last.contains("│ PIPELINE  STEP"), "{last}");
        assert!(last.contains("│>default   implement"), "{last}");
    }

    /// The `pipeline` row: `→` moves off "all" onto a real name, and `enter`
    /// applies it — narrowing the frame to that one pipeline, which the top
    /// border then names.
    #[test]
    fn the_filter_panel_narrows_the_table_once_applied() {
        let repo = fixture_with_one_run("screen-filter");
        bank(
            &repo,
            "2026-08-02T09:00:00+00:00",
            "b",
            "implement",
            "other",
            "qwen",
            1.0,
            Some("pass"),
        );
        let text = screen(&repo, &format!("f{DOWN}{DOWN}{DOWN}{RIGHT}\rq"));
        let last = last_frame(&text);
        assert!(last.contains("pipeline default"), "{last}");
        assert!(!last.contains("other"), "{last}");
    }

    /// The calendar still opens from the date rows, now the seventh and
    /// eighth: `esc` hands the row back untouched, `enter` picks the day it
    /// opened on, and `x` clears it.
    #[test]
    fn esc_leaves_the_row_untouched_enter_picks_the_day_x_clears_it() {
        let repo = fixture("screen-calendar-roundtrip");
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let to_since = format!("f{}", DOWN.repeat(6));

        // No trailing `q` after a bare `Esc`: its own lookahead read would
        // silently eat it, so the input running out ends the loop here.
        let last = last_frame(&screen(&repo, &format!("{to_since}\r\x1b"))).to_string();
        assert!(last.contains("(blank — the start)"), "{last}");

        let last = last_frame(&screen(&repo, &format!("{to_since}\r\rq"))).to_string();
        assert!(last.contains(&today), "{last}");

        let last = last_frame(&screen(&repo, &format!("{to_since}\rxq"))).to_string();
        assert!(last.contains("(blank — the start)"), "{last}");
    }

    /// A calendar is taller than the filter panel it replaces, so a table
    /// too short for the panel is shorter still against the calendar. Its
    /// own bottom border and key line must survive.
    #[test]
    fn a_short_table_still_fits_the_calendars_whole_height_key_line_included() {
        let repo = fixture_with_one_run("screen-short-table-calendar");
        let text = screen(&repo, &format!("f{}\rq", DOWN.repeat(6)));
        let last = last_frame(&text);
        assert!(last.contains("┌─ since ─"), "{last}");
        assert!(last.contains("[enter] pick   [esc] back"), "{last}");
    }

    /// Nothing on the date rows is typed into: a run of ordinary characters
    /// on the `since` row does not touch its value, and `enter` still opens
    /// the calendar.
    #[test]
    fn typing_on_a_date_row_does_nothing_and_enter_still_opens_the_calendar() {
        let repo = fixture("screen-bad-since");
        let text = screen(&repo, &format!("f{}notadate\rq", DOWN.repeat(6)));
        assert!(last_frame(&text).contains("┌─ since ─"), "{text}");
        assert!(!text.contains("notadate"), "{text}");
    }

    /// A directory line, written straight to `repo`'s own ledger — the same
    /// shape `usage::sweep`'s directory walk banks, skipping the walk.
    fn bank_dir(repo: &Repo, ts: &str, dir: &str, session: &str, cost: f64) {
        crate::usage::append(
            repo,
            &Entry {
                ts: ts.to_string(),
                task: String::new(),
                plan: None,
                step: String::new(),
                pipeline: String::new(),
                agent: String::new(),
                kind: "claude".to_string(),
                model: "claude-opus-5".to_string(),
                session: session.to_string(),
                round: 0,
                wall_s: 0,
                turns: 1,
                tokens: Tokens {
                    output: 10,
                    ..Tokens::default()
                },
                cost_usd: Some(cost),
                ctx_peak: None,
                pipeline_version: String::new(),
                outcome: None,
                run: None,
                trial: None,
                dir: Some(dir.to_string()),
                project: String::new(),
            },
        )
        .unwrap();
    }

    /// A `.claude/projects/<escaped>/<session>.jsonl` transcript under a
    /// fresh scratch home, so `skill_markers` has something real to read.
    fn claude_home_with(name: &str, session: &str, lines: &str) -> std::path::PathBuf {
        let root = crate::scratch::root(&format!("eval-skill-{name}"));
        let dir = root.join(".claude/projects/-nonsense-escaping-nobody-should-read");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{session}.jsonl")), lines).unwrap();
        root
    }

    fn skill_transcript() -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-09-01T09:00:00.000Z",
                "message": {"role": "user", "content":
                    "<command-name>/spoolway-plan</command-name>\n<command-args></command-args>"},
            }),
        )
    }

    /// The directory table by session names the skill each session ran; a
    /// `skill` filter keeps only the sessions that ran it, and the top
    /// border names it.
    #[test]
    fn by_session_names_each_skill_and_the_skill_row_narrows_to_it() {
        let repo = fixture("screen-dirs-and-skill");
        bank_dir(
            &repo,
            "2026-09-01T09:00:00+00:00",
            "/w/spoolway",
            "s1",
            0.70,
        );
        bank_dir(&repo, "2026-09-02T09:00:00+00:00", "/w/notes", "s2", 0.18);
        let home = claude_home_with("narrows", "s1", &skill_transcript());

        // `tab`, `f`, `→` on `by` to `session`, `enter`.
        let text = crate::platform::test_home::with_home(&home, || {
            screen(&repo, &format!("\tf{RIGHT}\rq"))
        });
        let sessions = last_frame(&text);
        assert!(sessions.contains("┌─ eval · by session "), "{sessions}");
        assert!(sessions.contains("SKILL"), "{sessions}");
        assert!(sessions.contains("spoolway-plan"), "{sessions}");
        assert!(sessions.contains(" notes "), "{sessions}");

        // `tab`, `f`, down twice to `skill`, `→` to its one candidate.
        let text = crate::platform::test_home::with_home(&home, || {
            screen(&repo, &format!("\tf{DOWN}{DOWN}{RIGHT}\rq"))
        });
        let filtered = last_frame(&text);
        assert!(filtered.contains("┌─ eval · by dir "), "{filtered}");
        assert!(filtered.contains("skill /spoolway-plan ─┐"), "{filtered}");
        assert!(filtered.contains("/w/spoolway"), "{filtered}");
        assert!(!filtered.contains("/w/notes"), "{filtered}");

        std::fs::remove_dir_all(&home).ok();
    }

    /// A notice's own overlay must never draw wider than the frame it sits
    /// on — `overlay` cannot place a panel that does not fit. This is the
    /// exact message an incomplete `--since` produces, one long sentence
    /// with no newline of its own, checked at the frame's narrowest floor
    /// and at two real widths.
    #[test]
    fn a_long_notice_wraps_to_fit_the_frame_at_any_width() {
        let err = crate::spend::window_of(None, Some("2026-08-0"), None).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.chars().count() > MIN_WIDTH, "{message:?}");

        for width in [MIN_WIDTH, 83, 130] {
            let lines = wrap_notice(&message, width);
            let rendered = panel("eval", &lines, NOTICE_KEYS);
            for row in &rendered {
                assert!(row.chars().count() <= width + 2, "at {width}: {row:?}");
            }
        }
    }
}
