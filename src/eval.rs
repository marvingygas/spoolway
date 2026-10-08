//! What the lanes cost to run, grouped one way at a time, beside what the
//! watched directories cost outside them.
//!
//! This is a **view over the ledger**, not a subsystem: every figure here is
//! grouped out of the same `usage.jsonl`, with nothing kept in a second
//! store. `crate::spend` holds only the scope and window helpers this module
//! calls to find and bound that ledger — it groups nothing itself.
//!
//! There are two tables, because the ledger holds two populations that never
//! share a row — see [`Entry::is_lane`] and [`Entry::dir`]. The lanes table
//! groups dispatched lanes by one [`EvalBy`]; the directory table groups the
//! sessions a person ran by hand in a watched root, by directory or one row
//! per session. The screen adds a third, the trials table, which is not a
//! population of its own: it lists the trials the lanes were arms of, and
//! opening one narrows the lanes table to it. Under every `by` only the columns naming a row change: the
//! figure columns stay put, so two groupings can be read against each other
//! without relearning where anything is. Those figure columns come in two
//! views, every row's totals or each total over the row's runs — see
//! [`Figures`] — and the exports carry both. A last line closes each table
//! with only what adds up across its rows, built from the ledger rather than
//! from the drawn cells: `Total`, the sums, in the totals view, and
//! `Average`, those sums over its runs or sessions, in the per-run view. A share
//! or a peak summed over rows would be a number nobody could use, so those
//! cells stay blank. The screen pins that line under its scrolled rows.
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
use crate::screen::{Key, PollableRead, boxed, key_hint, keys, overlay, pad_to, panel};
use crate::usage::{Entry, ModelPrice, Tokens};

/// The printing path: every `spoolway eval` with a flag of its own. The
/// same lanes table the screen draws, without the frame — or its rows as
/// CSV or JSON. `pipelines` orders `--by step`'s rows in each pipeline's own
/// walk order, and is `None` when the project's pipelines do not load: the
/// ledger is still worth reading then, with steps falling back to name order.
pub fn run(repo: &Repo, args: &EvalArgs, json: bool, pipelines: Option<&Pipelines>) -> Result<()> {
    run_to(repo, args, json, pipelines, &mut std::io::stdout().lock())
}

/// `--per-run` beside anything that is not the printed table. Only that
/// table has two views: `--csv` and `--json` already carry both column sets,
/// and `--discard` prints no figures at all. Checked here rather than by
/// clap, so every one of the three is refused with that reason — `--json`
/// is a global flag clap cannot pair with an `eval` one — and called from
/// `main` before `--discard` is dispatched, since that never reaches [`run`].
pub fn refuse_per_run_beside_an_export(args: &EvalArgs, json: bool) -> Result<()> {
    let beside = match (args.csv, json, args.discard.is_some()) {
        _ if !args.per_run => return Ok(()),
        (true, _, _) => "--csv",
        (_, true, _) => "--json",
        (_, _, true) => "--discard",
        _ => return Ok(()),
    };
    bail!(
        "`--per-run` only changes the printed table, not `{beside}`: `--csv` and `--json` \
         already carry both the totals and the per-run columns. Drop `--per-run` and run it \
         again"
    )
}

/// [`run`], printing to `out` — apart so a test can read what it printed.
fn run_to(
    repo: &Repo,
    args: &EvalArgs,
    json: bool,
    pipelines: Option<&Pipelines>,
    out: &mut impl std::io::Write,
) -> Result<()> {
    if json && args.csv {
        bail!("`--csv` and `--json` are two different exports of the same rows — pick one");
    }
    refuse_per_run_beside_an_export(args, json)?;
    let figures = match args.per_run {
        true => Figures::PerRun,
        false => Figures::Totals,
    };
    // Checked before the ledger is read, so a typo is refused the same way
    // whether or not there is anything yet to sort.
    let sort = args
        .sort
        .as_deref()
        .map(|text| parse_sort(args.by, text))
        .transpose()?;

    // Reading is also what catches the ledger up — see `usage::sweep`.
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

    let window = crate::spend::window_of(args.since.as_deref(), args.until.as_deref())?;
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
        trial: args.trial.as_deref(),
    };
    entries.retain(|entry| lane_filters.admits(entry));

    if entries.is_empty() {
        if args.trial.is_some() {
            writeln!(out, "No runs recorded for that trial yet.")?;
            return Ok(());
        }
        writeln!(out, "Nothing to compare in {} yet.", scope.what)?;
        writeln!(
            out,
            "A lane is recorded when a step finishes, so this fills up as `spoolway \
             dispatch` runs."
        )?;
        return Ok(());
    }

    entries.sort_by(|a, b| a.ts.cmp(&b.ts));
    let refs: Vec<&Entry> = entries.iter().collect();
    let mut rows = lane_rows(&refs, args.by, &fallback, &repo.config.models, pipelines);
    // A trial's arms read against the first one, so the first one has to be
    // the arm that started first — not whichever finished last, which is
    // what the newest-first order every other `--by task` reads in would put
    // on top.
    let trial_arms = args.trial.is_some() && args.by == EvalBy::Task;
    if trial_arms {
        rows.sort_by(|a, b| a.first_ts.cmp(&b.first_ts));
    }
    // Named before `--sort` reorders the rows, which may put a later arm on
    // top: every delta line still reads against the arm that started first.
    // A run id is unique to its arm, so it finds that arm again afterwards.
    let baseline_run = trial_arms.then(|| rows[0].keys[0].clone());
    if let Some(sort) = &sort {
        sort_lane_rows(args.by, &mut rows, sort);
    }
    let total = LaneTotal::of(&refs, &fallback, &repo.config.models);

    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&lanes_json(args.by, &rows, &total))?
        )?;
        return Ok(());
    }
    if args.csv {
        writeln!(out, "{}", lanes_csv_header(args.by))?;
        for row in &rows {
            writeln!(out, "{}", lanes_csv_row(args.by, row))?;
        }
        writeln!(out, "{}", lanes_csv_total(args.by, &total))?;
        return Ok(());
    }

    // No lead column here to carry a sorted first column's mark — see
    // `mark_column` — so a printed table leaves that one column unmarked
    // rather than shift every column by one.
    let table = lanes_table(args.by, &rows, &total, sort.as_ref(), figures);
    writeln!(out, "{}", dim(&table.header))?;
    for row in &table.rows {
        writeln!(out, "{row}")?;
    }
    writeln!(out, "{}", table.total)?;

    // `--trial` asks the question a trial exists to answer: its own
    // comparison, not just a narrower version of the same table. Only under
    // `--by task`, where one row is one arm; under any other `by` a row
    // mixes arms and there is nothing to subtract.
    if let Some(run) = baseline_run
        && rows.len() > 1
    {
        writeln!(out)?;
        let baseline = rows
            .iter()
            .find(|row| row.keys[0] == run)
            .expect("the baseline arm is one of the rows");
        for row in rows.iter().filter(|row| row.keys[0] != run) {
            writeln!(out, "{}", trial_delta_line(baseline, row))?;
        }
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
/// round and session — see gh-378 / issue #380. Tokens, cost and `wall_s` are
/// all banked as deltas, so a plain sum over every line already lands on
/// each lane's real total whether it wrote one line or two — it is only a
/// *count* of lanes, and their verdicts, that double-counts a line as a
/// lane. This answers with one verdict per group instead: an outcome of
/// `None` until a line in it actually reports one, and the last such line's
/// outcome and blocked flag from then on — a lane resumed and reported again is judged by what it said
/// last, not by every line it ever banked.
fn lane_verdicts<'a>(
    entries: impl IntoIterator<Item = &'a Entry>,
) -> HashMap<LaneKey, LaneVerdict> {
    let mut verdicts: HashMap<LaneKey, LaneVerdict> = HashMap::new();
    for entry in entries {
        let slot = verdicts.entry(lane_key(entry)).or_default();
        if entry.outcome.is_some() {
            slot.outcome = entry.outcome.clone();
            slot.blocked = entry.blocked;
        }
    }
    verdicts
}

/// What one lane said, and whether its report left the task on `blocked`.
/// The two differ whenever a `pass` or `fail` ends there anyway: a spent
/// `loop:`, a `fail` from a step that declares no `on_fail`, or a worktree
/// that could not be committed. The outcome stays what the lane reported, so
/// it still counts as that, and `blocked` is what `BLOCKS` counts.
#[derive(Default)]
struct LaneVerdict {
    outcome: Option<String>,
    blocked: bool,
}

impl LaneVerdict {
    /// A lane that reported `--block` ends blocked by definition, so a ledger
    /// line that never recorded the flag still counts, and one that did
    /// cannot count the same lane twice: this is one boolean per lane.
    fn ended_blocked(&self) -> bool {
        self.blocked || self.outcome.as_deref() == Some("block")
    }
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
    /// Lanes whose report left the task on `blocked`, whatever road took it
    /// there — see [`LaneVerdict`].
    blocked: usize,
    /// Every token class, summed over the row's lines — deltas, like cost.
    tokens: Tokens,
    /// Each of `tokens`' classes priced at today's price table — see
    /// [`ClassCost`]. Apart from `cost`, which is what was banked.
    class_cost: ClassCost,
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
                // row's total into a floor.
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
        for verdict in verdicts.values() {
            if verdict.ended_blocked() {
                out.blocked += 1;
            }
            if let Some(outcome) = &verdict.outcome {
                out.judged += 1;
                if outcome == "pass" {
                    out.passed += 1;
                }
            }
        }
        out.runs = runs.len();
        out.class_cost = ClassCost::of(matching.iter().copied(), models);
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

    fn blocks_per_run(&self) -> f64 {
        self.per_run(self.blocked as f64)
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

/// What each token class on a set of ledger lines costs at today's price
/// table: the `IN USD`, `OUT USD`, `CACHE R USD` and `CACHE W USD` columns.
///
/// Priced as eval reads, not banked: a ledger line carries one `cost_usd`
/// for its whole lane and no split by class. So the four need not add up to
/// `USD`, which is what the lane cost when it settled — under that day's
/// price table, or the agent's own report where it banks one, as pi does.
/// A line's `tier_tokens` are priced at the tier's rates, the rest at the base
/// rates. Resolved the way banking resolves a price, through
/// [`crate::models::resolve`].
#[derive(Default, Clone, Copy)]
struct ClassCost {
    input: f64,
    output: f64,
    cache_read: f64,
    /// The five-minute and one-hour writes, each priced at its own rate and
    /// then added — see [`ModelPrice::apply`] for the one-hour fallback.
    cache_write: f64,
    /// Lines that spent any tokens, and those among them whose model today's
    /// table has no price for. Kept apart from [`Metrics::unpriced`] because
    /// a banked cost and today's price can disagree on whether a model has
    /// one at all. A line that spent nothing is not counted: it costs
    /// nothing at any price, and counting it would draw `0.00` on a row
    /// whose every spending lane is unpriced.
    spent: usize,
    unpriced: usize,
}

impl ClassCost {
    fn of<'a>(
        entries: impl IntoIterator<Item = &'a Entry>,
        models: &BTreeMap<String, ModelPrice>,
    ) -> ClassCost {
        // One resolution per model, not per line: `models::resolve` reads
        // the refreshed price file from disk on every call, and the screen
        // rebuilds its table on every keypress.
        let mut prices: HashMap<&str, Option<ModelPrice>> = HashMap::new();
        let mut out = ClassCost::default();
        for entry in entries.into_iter().filter(|e| !e.tokens.is_zero()) {
            out.spent += 1;
            let price = *prices
                .entry(entry.model.as_str())
                .or_insert_with(|| crate::models::resolve(models, &entry.model).price);
            match price {
                Some(price) => out.add(&price, &entry.tokens, &entry.tier_tokens),
                None => out.unpriced += 1,
            }
        }
        out
    }

    /// `tokens` priced at `price`, one class at a time through
    /// [`crate::usage::Rates::apply`], so each column uses the rates banking does.
    ///
    /// `tier_tokens` is the part of `tokens` the ledger line banked as coming
    /// from turns over the threshold. It is priced at the model's tier rates,
    /// and the rest at the base rates. A line with no split, or a model with
    /// no tier today, prices all of `tokens` at the base rates. The split is
    /// clamped to `tokens`, so a hand-edited line cannot price a class twice.
    fn add(&mut self, price: &ModelPrice, tokens: &Tokens, tier_tokens: &Tokens) {
        let base_rates = price.rates();
        let (base, tier, tier_rates) = match price.tier {
            Some(tier) => (
                tokens.since(tier_tokens),
                tier_tokens.within(tokens),
                tier.rates,
            ),
            None => (*tokens, Tokens::default(), base_rates),
        };
        let priced = |class: fn(&Tokens) -> Tokens| {
            base_rates.apply(&class(&base)) + tier_rates.apply(&class(&tier))
        };
        self.input += priced(|t| Tokens {
            input: t.input,
            ..Tokens::default()
        });
        self.output += priced(|t| Tokens {
            output: t.output,
            ..Tokens::default()
        });
        self.cache_read += priced(|t| Tokens {
            cache_read: t.cache_read,
            ..Tokens::default()
        });
        self.cache_write += priced(|t| Tokens {
            cache_write_5m: t.cache_write_5m,
            cache_write_1h: t.cache_write_1h,
            ..Tokens::default()
        });
    }

    /// `self` and `other` as one set of lines — what a `Total` line adds up
    /// from rows that each priced their own. The line counts add too, so
    /// [`ClassCost::priced`] reads the same as if the lines had been priced
    /// together.
    fn plus(self, other: &ClassCost) -> ClassCost {
        ClassCost {
            input: self.input + other.input,
            output: self.output + other.output,
            cache_read: self.cache_read + other.cache_read,
            cache_write: self.cache_write + other.cache_write,
            spent: self.spent + other.spent,
            unpriced: self.unpriced + other.unpriced,
        }
    }

    /// Whether the figures say anything: some line that spent tokens could
    /// be priced, or no line spent any, which is a true `0.00`. Where every
    /// spending line is unpriced, each class cell is blank — unknown, not
    /// free.
    fn priced(&self) -> bool {
        self.spent == 0 || self.spent > self.unpriced
    }

    /// The four figures in column order, each through `each` — a division
    /// by runs or sessions, or nothing.
    fn figures(&self, each: impl Fn(f64) -> f64) -> [f64; 4] {
        [self.input, self.output, self.cache_read, self.cache_write].map(each)
    }

    /// The four figures as drawn: two places, as `0.00` where a class cost
    /// under a cent, and blank where nothing could be priced — see
    /// [`ClassCost::priced`].
    fn cells(&self, each: impl Fn(f64) -> f64) -> [String; 4] {
        self.figures(each).map(|usd| match self.priced() {
            true => format!("{usd:.2}"),
            false => String::new(),
        })
    }

    /// The four figures for `--json`: `null` where nothing could be priced.
    fn json(&self, each: impl Fn(f64) -> f64) -> [Option<f64>; 4] {
        let priced = self.priced();
        self.figures(each).map(|usd| priced.then_some(usd))
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
    /// The trial id a lane must have been an arm of — see [`Entry::trial`].
    trial: Option<&'a str>,
}

impl LaneFilters<'_> {
    fn admits(&self, e: &Entry) -> bool {
        self.group.is_none_or(|g| e.plan.as_deref() == Some(g))
            && self.task.is_none_or(|t| e.task == t)
            && self.pipeline.is_none_or(|p| e.pipeline == p)
            && self.step.is_none_or(|s| e.step == s)
            && self.version.is_none_or(|v| e.pipeline_version == v)
            && self.trial.is_none_or(|t| e.trial.as_deref() == Some(t))
    }
}

/// One row of the lanes table under whichever `by` built it.
struct LaneRow {
    /// The columns naming the row, as drawn — see [`lane_columns`].
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
/// that changes with it — each beside the export column a sort on it reads:
/// see [`lane_sort_value`].
fn lane_columns(by: EvalBy) -> &'static [(&'static str, &'static str)] {
    match by {
        EvalBy::Group => &[("GROUP", "group")],
        EvalBy::Task => &[
            ("TASK", "task"),
            ("PIPELINE", "pipeline"),
            ("VER", "pipeline_version"),
            ("WHEN", "when"),
        ],
        EvalBy::Pipeline => &[("PIPELINE", "pipeline")],
        EvalBy::Step => &[("PIPELINE", "pipeline"), ("STEP", "step")],
        EvalBy::Version => &[
            ("PIPELINE", "pipeline"),
            ("VERSION", "pipeline_version"),
            ("FIRST", "first"),
        ],
    }
}

/// The figure columns the per-run view draws, identical under every `by`,
/// in the order [`lane_figures`] draws them — each beside the export column
/// a sort on it reads. A per-run cell sorts on its per-run figure, not the
/// total beside it in an export. Each token class is followed by its cost at
/// today's prices — see [`ClassCost`].
const LANE_FIGURE_COLUMNS: [(&str, &str); 16] = [
    ("RUNS", "runs"),
    ("PASS", "pass"),
    ("BLOCKS/RUN", "blocks_per_run"),
    ("CTX PEAK AVG", "ctx_peak_avg_pct"),
    ("CTX PEAK", "ctx_peak_pct"),
    ("IN/RUN", "in_per_run"),
    ("IN USD/RUN", "in_usd_per_run"),
    ("OUT/RUN", "out_per_run"),
    ("OUT USD/RUN", "out_usd_per_run"),
    ("CACHE R/RUN", "cache_read_per_run"),
    ("CACHE R USD/RUN", "cache_read_usd_per_run"),
    ("CACHE W/RUN", "cache_write_per_run"),
    ("CACHE W USD/RUN", "cache_write_usd_per_run"),
    ("USD", "cost_usd"),
    ("USD/RUN", "cost_per_run"),
    ("TIME/RUN", "time_per_run_s"),
];

/// The figure columns the totals view draws, in the order
/// [`lane_total_figures`] draws them. No `USD/RUN` beside `USD` here: in
/// this view `USD` already is the total, and a second column saying the same
/// thing per run would be the one per-run figure left in a view of totals.
/// `USD` stays last among the costs, as the banked total the four class
/// costs before it are priced apart from — see [`ClassCost`].
const LANE_TOTAL_COLUMNS: [(&str, &str); 15] = [
    ("RUNS", "runs"),
    ("PASS", "pass"),
    ("BLOCKS", "blocks"),
    ("CTX PEAK AVG", "ctx_peak_avg_pct"),
    ("CTX PEAK", "ctx_peak_pct"),
    ("IN", "in_tokens"),
    ("IN USD", "in_usd"),
    ("OUT", "out_tokens"),
    ("OUT USD", "out_usd"),
    ("CACHE R", "cache_read_tokens"),
    ("CACHE R USD", "cache_read_usd"),
    ("CACHE W", "cache_write_tokens"),
    ("CACHE W USD", "cache_write_usd"),
    ("USD", "cost_usd"),
    ("TIME", "time_s"),
];

/// The lanes table's figure columns in either view — see [`Figures`].
fn lane_figure_columns(figures: Figures) -> &'static [(&'static str, &'static str)] {
    match figures {
        Figures::Totals => &LANE_TOTAL_COLUMNS,
        Figures::PerRun => &LANE_FIGURE_COLUMNS,
    }
}

/// Every column the lanes table draws over `rows`, left to right: `PROJECT`
/// where [`spans_more_than_one_project`] gives it one, the `by`'s own, then
/// the figures of `figures`. What the sort popup lists, and what decides
/// whether a sort still applies to the view on screen.
fn drawn_lane_columns(
    by: EvalBy,
    rows: &[LaneRow],
    figures: Figures,
) -> Vec<(&'static str, &'static str)> {
    let mut columns = Vec::new();
    if spans_more_than_one_project(rows) {
        columns.push(("PROJECT", "project"));
    }
    columns.extend(lane_columns(by));
    columns.extend(lane_figure_columns(figures));
    columns
}

/// Which figures the lanes and directory tables draw: every row's own
/// totals, or each total over the row's runs (sessions, on the directory
/// table). Both sets do not fit across one terminal, so one shows at a time.
/// Totals is the default, since what the work cost in all is the first
/// question a person opening the screen asks; `t` on the screen and
/// `--per-run` on the command line switch to the other. The exports carry
/// both sets whichever is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Figures {
    #[default]
    Totals,
    PerRun,
}

impl Figures {
    fn other(self) -> Figures {
        match self {
            Figures::Totals => Figures::PerRun,
            Figures::PerRun => Figures::Totals,
        }
    }

    /// What the frame title names this view as, after the `by`, and what
    /// the key line names it as beside `[t]` while the other one is drawn.
    fn label(self) -> &'static str {
        match self {
            Figures::Totals => "totals",
            Figures::PerRun => "per run",
        }
    }

    /// `key` as this view draws it, where `pairs` — the table's own, each a
    /// total beside its per-run (or per-session) figure — name it in the
    /// other view: `IN/RUN` for a sort on `IN`, and back. `USD` is drawn in
    /// both views and keeps its key; `USD/RUN` has no column in the totals
    /// view and lands on `USD`, the figure it is a share of. Any other
    /// column — `PASS`, a naming column — is drawn the same in both and is
    /// handed back as it is.
    fn resolve<'k>(self, key: &'k str, pairs: &[(&'k str, &'k str)]) -> &'k str {
        let found = match self {
            Figures::Totals => pairs.iter().find(|(_, per)| *per == key),
            Figures::PerRun => pairs
                .iter()
                .find(|(total, _)| *total == key && *total != "cost_usd"),
        };
        match (found, self) {
            (Some((total, _)), Figures::Totals) => total,
            (Some((_, per)), Figures::PerRun) => per,
            (None, _) => key,
        }
    }

    /// `sort` carried into this view from the other — see
    /// [`Figures::resolve`] — so a sort is not lost with the column it was
    /// on when `t` is pressed.
    fn carry(self, sort: &mut Option<Sort>, pairs: &[(&str, &str)]) {
        if let Some(sort) = sort {
            sort.key = self.resolve(&sort.key, pairs).to_string();
        }
    }
}

/// The lanes table's totals, each beside its per-run figure — what
/// [`Figures::carry`] moves a lanes sort between.
const LANE_PAIRS: [(&str, &str); 11] = [
    ("blocks", "blocks_per_run"),
    ("in_tokens", "in_per_run"),
    ("in_usd", "in_usd_per_run"),
    ("out_tokens", "out_per_run"),
    ("out_usd", "out_usd_per_run"),
    ("cache_read_tokens", "cache_read_per_run"),
    ("cache_read_usd", "cache_read_usd_per_run"),
    ("cache_write_tokens", "cache_write_per_run"),
    ("cache_write_usd", "cache_write_usd_per_run"),
    ("cost_usd", "cost_per_run"),
    ("time_s", "time_per_run_s"),
];

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

/// Every row `entries` makes under `by`, in the table's default order — the
/// one it reads in until a sort reorders it, and the one a sort's ties keep.
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
/// step. `BLOCKS`, the token classes and their costs, `USD` and `TIME` do
/// sum: every ledger line sits on exactly one row. `TIME` is lane time added
/// up, so lanes that ran side by side count once each — it is not the
/// calendar time they took.
struct LaneTotal {
    runs: usize,
    blocked: usize,
    tokens: Tokens,
    /// What `tokens` cost at today's price table — see [`ClassCost`].
    class_cost: ClassCost,
    cost: f64,
    lines: usize,
    unpriced: usize,
    time_s: i64,
}

impl LaneTotal {
    fn of(
        entries: &[&Entry],
        fallback: &HashMap<(String, String), String>,
        models: &BTreeMap<String, ModelPrice>,
    ) -> LaneTotal {
        let runs: HashSet<(String, String)> = entries
            .iter()
            .map(|e| (e.project.clone(), run_key(e, fallback)))
            .collect();
        let blocked = lane_verdicts(entries.iter().copied())
            .values()
            .filter(|v| v.ended_blocked())
            .count();
        let mut tokens = Tokens::default();
        for entry in entries {
            tokens.add(&entry.tokens);
        }
        LaneTotal {
            runs: runs.len(),
            blocked,
            tokens,
            class_cost: ClassCost::of(entries.iter().copied(), models),
            cost: entries.iter().filter_map(|e| e.cost_usd).sum(),
            lines: entries.len(),
            unpriced: entries
                .iter()
                .filter(|e| e.cost_usd.is_none() && !e.tokens.is_zero())
                .count(),
            time_s: entries.iter().map(|e| e.wall_s).sum(),
        }
    }

    /// `value` over the table's distinct runs: the average run, the way
    /// [`Metrics::per_run`] is a row's. `0.0` for an empty table, which no
    /// screen or print ever draws a `Total` line for.
    fn per_run(&self, value: f64) -> f64 {
        match self.runs {
            0 => 0.0,
            n => value / n as f64,
        }
    }

    fn tokens_per_run(&self, n: u64) -> u64 {
        self.per_run(n as f64).round() as u64
    }
}

// ------------------------------------------------------------- lanes table

/// The lanes table as plain text: its header, one line per row, and the
/// `Total` line — no marker column and no colour, so the screen can lay its
/// own frame over exactly these columns and the printing path can print
/// them as they are.
struct Table {
    header: String,
    /// The sort mark for a sorted first column, which has no space of its
    /// own in `header` to carry one — `' '` otherwise. See [`mark_column`].
    lead: char,
    rows: Vec<String>,
    total: String,
}

/// Separator between two columns everywhere in every table.
const SEP: &str = "  ";

/// The per-run view's figure columns, identical under every `by`. `RUNS`
/// is drawn one wider than its title, as the mockup draws it: the extra
/// column is the gap that sets the figures off from whichever naming column
/// precedes them. Each class cost is as wide as its own title.
fn lane_figures(cells: [&str; 16]) -> String {
    let [
        runs,
        pass,
        blocks,
        avg,
        peak,
        inp,
        inp_usd,
        out,
        out_usd,
        cr,
        cr_usd,
        cw,
        cw_usd,
        usd,
        usd_run,
        time,
    ] = cells;
    format!(
        "{SEP}{runs:>5}{SEP}{pass:>4}{SEP}{blocks:>10}{SEP}{avg:>12}{SEP}{peak:>8}{SEP}{inp:>6}\
         {SEP}{inp_usd:>10}{SEP}{out:>7}{SEP}{out_usd:>11}{SEP}{cr:>11}{SEP}{cr_usd:>15}\
         {SEP}{cw:>11}{SEP}{cw_usd:>15}{SEP}{usd:>8}{SEP}{usd_run:>7}{SEP}{time:>8}"
    )
}

/// The figure columns of the totals view — see [`LANE_TOTAL_COLUMNS`] —
/// padded the way [`lane_figures`] pads the per-run ones. Each token column
/// is seven wide, room for `149.71M`, each class cost as wide as its own
/// title, and `USD` and `TIME` eight, room for `128h 39m`. A wider cell
/// pushes the rest of its row right, as a wide per-run cell does.
fn lane_total_figures(cells: [&str; 15]) -> String {
    let [
        runs,
        pass,
        blocks,
        avg,
        peak,
        inp,
        inp_usd,
        out,
        out_usd,
        cr,
        cr_usd,
        cw,
        cw_usd,
        usd,
        time,
    ] = cells;
    format!(
        "{SEP}{runs:>5}{SEP}{pass:>4}{SEP}{blocks:>6}{SEP}{avg:>12}{SEP}{peak:>8}{SEP}{inp:>7}\
         {SEP}{inp_usd:>6}{SEP}{out:>7}{SEP}{out_usd:>7}{SEP}{cr:>7}{SEP}{cr_usd:>11}\
         {SEP}{cw:>7}{SEP}{cw_usd:>11}{SEP}{usd:>8}{SEP}{time:>8}"
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

/// What the lanes and directory tables call their last line: `Total` over
/// the totals view, whose figures are sums, and `Average` over the per-run
/// view, whose figures are those sums over the table's runs or sessions.
/// Named apart so a reader never takes an average run for the whole bill.
fn total_line_label(figures: Figures) -> &'static str {
    match figures {
        Figures::Totals => "Total",
        Figures::PerRun => "Average",
    }
}

/// The lanes table over `rows`, in the order they are given, drawing the
/// figures of `figures`. `sort` only marks its column's header; the rows are
/// already sorted by then. The last line is the table's `Total` in the
/// totals view and its average run in the per-run view — see
/// [`total_line_label`].
fn lanes_table(
    by: EvalBy,
    rows: &[LaneRow],
    total: &LaneTotal,
    sort: Option<&Sort>,
    figures: Figures,
) -> Table {
    let show_project = spans_more_than_one_project(rows);
    let mut headers: Vec<String> = lane_columns(by)
        .iter()
        .map(|(h, _)| h.to_string())
        .collect();
    let mut body: Vec<Vec<String>> = rows.iter().map(|r| r.cells.clone()).collect();
    if show_project {
        headers.insert(0, "PROJECT".to_string());
        for (cells, row) in body.iter_mut().zip(rows) {
            cells.insert(0, row.project.clone());
        }
    }
    let mut total_cells = vec![String::new(); headers.len()];
    total_cells[0] = total_line_label(figures).to_string();

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
        match figures {
            Figures::Totals => lane_total_figures(LANE_TOTAL_COLUMNS.map(|(h, _)| h)),
            Figures::PerRun => lane_figures(LANE_FIGURE_COLUMNS.map(|(h, _)| h)),
        }
    );
    let (header, lead) = marked_header(header, &drawn_lane_columns(by, rows, figures), sort);
    let lines = body
        .iter()
        .zip(rows)
        .map(|(cells, row)| {
            let m = &row.metrics;
            let (runs, pass, blocks) = (
                m.runs.to_string(),
                percent(m.pass_share()),
                m.blocked.to_string(),
            );
            let (avg, peak) = (
                m.ctx_avg.cell(),
                ctx_cell(m.ctx_peak_tokens, m.ctx_peak_pct),
            );
            let usd = cost_of(m.cost, m.lines, m.unpriced);
            let figures = match figures {
                Figures::Totals => {
                    let [in_usd, out_usd, cr_usd, cw_usd] = m.class_cost.cells(|usd| usd);
                    lane_total_figures([
                        &runs,
                        &pass,
                        &blocks,
                        &avg,
                        &peak,
                        &tokens_cell(m.tokens.input),
                        &in_usd,
                        &tokens_cell(m.tokens.output),
                        &out_usd,
                        &tokens_cell(m.tokens.cache_read),
                        &cr_usd,
                        &tokens_cell(m.tokens.cache_write()),
                        &cw_usd,
                        &usd,
                        &crate::status::human_secs(m.time_s),
                    ])
                }
                Figures::PerRun => {
                    let [in_usd, out_usd, cr_usd, cw_usd] =
                        m.class_cost.cells(|usd| m.per_run(usd));
                    lane_figures([
                        &runs,
                        &pass,
                        &format!("{:.2}", m.blocks_per_run()),
                        &avg,
                        &peak,
                        &tokens_cell(m.tokens_per_run(m.tokens.input)),
                        &in_usd,
                        &tokens_cell(m.tokens_per_run(m.tokens.output)),
                        &out_usd,
                        &tokens_cell(m.tokens_per_run(m.tokens.cache_read)),
                        &cr_usd,
                        &tokens_cell(m.tokens_per_run(m.tokens.cache_write())),
                        &cw_usd,
                        &usd,
                        // Dividing a floor by its run count is still a floor,
                        // but `cost_of` prints the same plain number either
                        // way.
                        &cost_of(m.cost_per_run(), m.lines, m.unpriced),
                        &crate::status::human_secs(m.time_per_run_s().round() as i64),
                    ])
                }
            };
            format!("{}{figures}", naming(cells, &widths))
        })
        .collect();
    // `PASS` and both `CTX PEAK` cells stay blank in either view: a share
    // or a peak summed over rows, or averaged over rows of different
    // sizes, is not a figure of the whole table.
    let runs = total.runs.to_string();
    let t = &total.tokens;
    let total_figures = match figures {
        Figures::Totals => {
            let [in_usd, out_usd, cr_usd, cw_usd] = total.class_cost.cells(|usd| usd);
            lane_total_figures([
                &runs,
                "",
                &total.blocked.to_string(),
                "",
                "",
                &tokens_cell(t.input),
                &in_usd,
                &tokens_cell(t.output),
                &out_usd,
                &tokens_cell(t.cache_read),
                &cr_usd,
                &tokens_cell(t.cache_write()),
                &cw_usd,
                &cost_of(total.cost, total.lines, total.unpriced),
                &crate::status::human_secs(total.time_s),
            ])
        }
        // `USD` is left blank here: a total has no place on a line of
        // averages, and `USD/RUN` beside it carries the average.
        Figures::PerRun => {
            let [in_usd, out_usd, cr_usd, cw_usd] =
                total.class_cost.cells(|usd| total.per_run(usd));
            lane_figures([
                &runs,
                "",
                &format!("{:.2}", total.per_run(total.blocked as f64)),
                "",
                "",
                &tokens_cell(total.tokens_per_run(t.input)),
                &in_usd,
                &tokens_cell(total.tokens_per_run(t.output)),
                &out_usd,
                &tokens_cell(total.tokens_per_run(t.cache_read)),
                &cr_usd,
                &tokens_cell(total.tokens_per_run(t.cache_write())),
                &cw_usd,
                "",
                &cost_of(total.per_run(total.cost), total.lines, total.unpriced),
                &crate::status::human_secs(total.per_run(total.time_s as f64).round() as i64),
            ])
        }
    };
    let total = format!("{}{total_figures}", naming(&total_cells, &widths))
        .trim_end()
        .to_string();
    Table {
        header,
        lead,
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
/// and each token class per run beside them, then each class's cost at
/// today's prices the same way, so a spreadsheet can re-derive the screen's
/// figures or pick its own.
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
        "in_usd",
        "out_usd",
        "cache_read_usd",
        "cache_write_usd",
        "in_usd_per_run",
        "out_usd_per_run",
        "cache_read_usd_per_run",
        "cache_write_usd_per_run",
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
    let class_usd = m.class_cost.cells(|usd| usd);
    let class_usd_per_run = m.class_cost.cells(|usd| m.per_run(usd));
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
    ]);
    cells.extend(class_usd);
    cells.extend(class_usd_per_run);
    cells.extend([
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
/// and blank in every column that does not add up. It carries what the
/// screen's `Total` line carries in either view: the sums, and each per-run
/// column as its sum over the table's distinct runs.
fn lanes_csv_total(by: EvalBy, total: &LaneTotal) -> String {
    let header = lanes_csv_header(by);
    let names: Vec<&str> = header.split(',').collect();
    let t = &total.tokens;
    let [in_usd, out_usd, cr_usd, cw_usd] = total.class_cost.cells(|usd| usd);
    let [in_usd_run, out_usd_run, cr_usd_run, cw_usd_run] =
        total.class_cost.cells(|usd| total.per_run(usd));
    names
        .iter()
        .map(|name| match *name {
            "by" => "total".to_string(),
            "runs" => total.runs.to_string(),
            "blocks" => total.blocked.to_string(),
            "in_tokens" => t.input.to_string(),
            "out_tokens" => t.output.to_string(),
            "cache_read_tokens" => t.cache_read.to_string(),
            "cache_write_tokens" => t.cache_write().to_string(),
            "in_per_run" => total.tokens_per_run(t.input).to_string(),
            "out_per_run" => total.tokens_per_run(t.output).to_string(),
            "cache_read_per_run" => total.tokens_per_run(t.cache_read).to_string(),
            "cache_write_per_run" => total.tokens_per_run(t.cache_write()).to_string(),
            "in_usd" => in_usd.clone(),
            "out_usd" => out_usd.clone(),
            "cache_read_usd" => cr_usd.clone(),
            "cache_write_usd" => cw_usd.clone(),
            "in_usd_per_run" => in_usd_run.clone(),
            "out_usd_per_run" => out_usd_run.clone(),
            "cache_read_usd_per_run" => cr_usd_run.clone(),
            "cache_write_usd_per_run" => cw_usd_run.clone(),
            "cost_usd" => csv_cost(total.cost, total.lines, total.unpriced),
            "cost_per_run" => csv_cost(total.per_run(total.cost), total.lines, total.unpriced),
            "time_s" => total.time_s.max(0).to_string(),
            "time_per_run_s" => (total.per_run(total.time_s as f64).round() as i64).to_string(),
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
            let [in_usd, out_usd, cr_usd, cw_usd] = m.class_cost.json(|usd| usd);
            let [in_usd_run, out_usd_run, cr_usd_run, cw_usd_run] =
                m.class_cost.json(|usd| m.per_run(usd));
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
                "in_usd": in_usd,
                "out_usd": out_usd,
                "cache_read_usd": cr_usd,
                "cache_write_usd": cw_usd,
                "in_usd_per_run": in_usd_run,
                "out_usd_per_run": out_usd_run,
                "cache_read_usd_per_run": cr_usd_run,
                "cache_write_usd_per_run": cw_usd_run,
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
    let total_priced = total.lines > total.unpriced;
    let [in_usd, out_usd, cr_usd, cw_usd] = total.class_cost.json(|usd| usd);
    let [in_usd_run, out_usd_run, cr_usd_run, cw_usd_run] =
        total.class_cost.json(|usd| total.per_run(usd));
    serde_json::json!({
        "by": by.label(),
        "rows": rows,
        "total": {
            "runs": total.runs,
            "blocks": total.blocked,
            "in_tokens": total.tokens.input,
            "out_tokens": total.tokens.output,
            "cache_read_tokens": total.tokens.cache_read,
            "cache_write_tokens": total.tokens.cache_write(),
            "in_per_run": total.tokens_per_run(total.tokens.input),
            "out_per_run": total.tokens_per_run(total.tokens.output),
            "cache_read_per_run": total.tokens_per_run(total.tokens.cache_read),
            "cache_write_per_run": total.tokens_per_run(total.tokens.cache_write()),
            "in_usd": in_usd,
            "out_usd": out_usd,
            "cache_read_usd": cr_usd,
            "cache_write_usd": cw_usd,
            "in_usd_per_run": in_usd_run,
            "out_usd_per_run": out_usd_run,
            "cache_read_usd_per_run": cr_usd_run,
            "cache_write_usd_per_run": cw_usd_run,
            "cost_usd": total_priced.then_some(total.cost),
            "cost_per_run": total_priced.then_some(total.per_run(total.cost)),
            "time_s": total.time_s,
            "time_per_run_s": total.per_run(total.time_s as f64),
        },
    })
}

// ------------------------------------------------------------------ sorting
//
// Every table prints in one default order — see `lane_rows`, `dir_rows` and
// `list_sessions` — and a sort only ever reorders what that order already
// laid out. Rust's `sort_by` is stable, so rows the sorted column ties on
// keep their default order between them.
//
// A sort names its column the way an export's header does (`pass`,
// `cost_usd`, `time_per_run_s`), never the way the screen's header does
// (`PASS`, `USD`, `TIME/RUN`): the export spelling is the one `--sort`
// already has to take, and one key per column is what lets a sort outlive a
// change of `by` wherever the new view still draws that column.
//
// What a row sorts on is its raw figure, never its drawn cell — as text,
// `1h 20m` sorts under `35m` and `54.2k` under `9.1M`.

/// One table's sort: the export column it reads, and which way.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sort {
    key: String,
    descending: bool,
}

impl Sort {
    /// The arrow the sorted column's header carries.
    fn mark(&self) -> char {
        match self.descending {
            true => '▼',
            false => '▲',
        }
    }
}

/// `--sort <column>[:asc|:desc]`, checked against `--by`'s own export
/// header. An unknown column is an error naming every one there is: a typo
/// that quietly sorted nothing would print the default order under a flag
/// that claims otherwise.
fn parse_sort(by: EvalBy, text: &str) -> Result<Sort> {
    let (key, descending) = match text.rsplit_once(':') {
        Some((key, "asc")) => (key, false),
        Some((key, "desc")) => (key, true),
        Some((_, other)) => {
            bail!("`--sort {text}`: the direction after `:` is `asc` or `desc`, not `{other}`")
        }
        None => (text, true),
    };
    let header = lanes_csv_header(by);
    if !header.split(',').any(|name| name == key) {
        bail!(
            "`--sort` has no column `{key}` under `--by {}` — pick one of: {}",
            by.label(),
            header.split(',').collect::<Vec<_>>().join(", ")
        );
    }
    Ok(Sort {
        key: key.to_string(),
        descending,
    })
}

/// A row's raw figure under one column.
enum SortValue {
    /// A number, in a tier: every row in tier `0` sorts before every row in
    /// tier `1`, whichever way the sort runs. Only a `CTX PEAK` column uses
    /// a second tier — see [`ctx_sort_value`].
    Figure(u8, f64),
    Text(String),
    /// A `pipeline_version`, compared through [`version_order`] so `1.10`
    /// sorts above `1.9`.
    Version(String),
}

impl SortValue {
    fn figure(n: impl Into<f64>) -> Option<SortValue> {
        Some(SortValue::Figure(0, n.into()))
    }

    fn text(s: &str) -> Option<SortValue> {
        Some(SortValue::Text(s.to_string()))
    }

    /// `a` against `b` the way the sort runs. Tiers always ascend, so the
    /// direction only ever turns the figures within one.
    fn compare(a: &SortValue, b: &SortValue, descending: bool) -> std::cmp::Ordering {
        use SortValue::*;
        let turn = |o: std::cmp::Ordering| if descending { o.reverse() } else { o };
        match (a, b) {
            (Figure(ta, a), Figure(tb, b)) => ta.cmp(tb).then_with(|| turn(a.total_cmp(b))),
            (Text(a), Text(b)) => turn(a.cmp(b)),
            (Version(a), Version(b)) => turn(version_order(a).cmp(&version_order(b))),
            // One column never mixes kinds; equal keeps the default order
            // should a new column ever get that wrong.
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// `rows`, reordered by `sort` through `value`. A row without a figure —
/// drawn `—`, or an unpriced `USD` — sorts after every row with one in both
/// directions: an unknown is not the smallest value, nor the largest.
fn sort_rows<T>(rows: &mut [T], sort: &Sort, value: impl Fn(&T) -> Option<SortValue>) {
    rows.sort_by(|a, b| match (value(a), value(b)) {
        (Some(a), Some(b)) => SortValue::compare(&a, &b, sort.descending),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
}

/// A `CTX PEAK` figure: the share of the window where one resolved, and the
/// raw peak in tokens where it did not — the same fallback [`ctx_cell`]
/// draws. The two are not on one scale, so every share sorts before every
/// raw count, and a row with neither is blank.
fn ctx_sort_value(tokens: Option<f64>, pct: Option<f64>) -> Option<SortValue> {
    match (tokens, pct) {
        (_, Some(pct)) => Some(SortValue::Figure(0, pct)),
        (Some(tokens), None) => Some(SortValue::Figure(1, tokens)),
        (None, None) => None,
    }
}

/// `sort` where the view on screen draws its column, and `None` — the
/// default order — where it does not, such as `step` once `by` moved off
/// step. `columns` are the view's own, in [`lane_columns`]'s shape.
fn shown_sort<'a>(sort: Option<&'a Sort>, columns: &[(&str, &str)]) -> Option<&'a Sort> {
    sort.filter(|s| columns.iter().any(|(_, key)| *key == s.key))
}

/// Where each column of a table's header line starts, in characters. Two
/// spaces at least separate any two columns — [`SEP`] — and no header name
/// holds more than one in a row (`CTX PEAK AVG`), so a run of two spaces is
/// always a column boundary.
fn column_starts(header: &str) -> Vec<usize> {
    let chars: Vec<char> = header.chars().collect();
    (0..chars.len())
        .filter(|&i| {
            chars[i] != ' '
                && (chars[..i].iter().all(|c| *c == ' ') || (i >= 2 && chars[i - 2..i] == [' '; 2]))
        })
        .collect()
}

/// `header` with `mark` in the space just before column `index`'s name, so
/// no column moves. The first column has no space of its own before it:
/// its mark is handed back as the lead, for the screen to draw in the
/// cursor's column to the left — see [`Line::Head`].
fn mark_column(header: &str, index: usize, mark: char) -> (String, char) {
    match column_starts(header).get(index) {
        Some(&start) if start > 0 => {
            let mut chars: Vec<char> = header.chars().collect();
            chars[start - 1] = mark;
            (chars.into_iter().collect(), ' ')
        }
        Some(_) => (header.to_string(), mark),
        None => (header.to_string(), ' '),
    }
}

/// `header` marked for `sort` where `columns` draw its column — see
/// [`mark_column`] — and as it was otherwise.
fn marked_header(header: String, columns: &[(&str, &str)], sort: Option<&Sort>) -> (String, char) {
    let index = sort.and_then(|s| columns.iter().position(|(_, key)| *key == s.key));
    match (sort, index) {
        (Some(sort), Some(index)) => mark_column(&header, index, sort.mark()),
        _ => (header, ' '),
    }
}

/// One lanes row's raw figure under export column `key`.
fn lane_sort_value(by: EvalBy, row: &LaneRow, key: &str) -> Option<SortValue> {
    let m = &row.metrics;
    let t = &m.tokens;
    let priced = m.lines > m.unpriced;
    let per_run = |n: u64| SortValue::figure(m.tokens_per_run(n) as f64);
    // Unpriced class costs sort last, the way an unpriced `USD` does.
    let class = |usd: f64| m.class_cost.priced().then_some(SortValue::Figure(0, usd));
    let c = &m.class_cost;
    match key {
        "project" => SortValue::text(&row.project),
        // The `VER` and `VERSION` cells name a version where an export's
        // `pipeline_version` would be blank for a row spanning several —
        // and the screen is the order a person reads.
        "pipeline_version" => match by {
            EvalBy::Task => Some(SortValue::Version(row.cells[2].clone())),
            EvalBy::Version => Some(SortValue::Version(row.cells[1].clone())),
            _ => m
                .single_version()
                .map(|v| SortValue::Version(v.to_string())),
        },
        // Dates as drawn are the local day only; the raw timestamps also
        // order two runs that ended on the same one.
        "when" => SortValue::text(&row.last_ts),
        "first" => SortValue::text(&row.first_ts),
        "runs" => SortValue::figure(m.runs as f64),
        "lanes" => SortValue::figure(m.lanes as f64),
        "pass" => m.pass_share().and_then(SortValue::figure),
        "blocks" => SortValue::figure(m.blocked as f64),
        "blocks_per_run" => SortValue::figure(m.blocks_per_run()),
        "ctx_peak_tokens" => m.ctx_peak_tokens.and_then(|n| SortValue::figure(n as f64)),
        "ctx_peak_pct" => ctx_sort_value(m.ctx_peak_tokens.map(|n| n as f64), m.ctx_peak_pct),
        "ctx_peak_avg_tokens" => m.ctx_avg.tokens.and_then(SortValue::figure),
        "ctx_peak_avg_pct" => ctx_sort_value(m.ctx_avg.tokens, m.ctx_avg.pct),
        "in_tokens" => SortValue::figure(t.input as f64),
        "out_tokens" => SortValue::figure(t.output as f64),
        "cache_read_tokens" => SortValue::figure(t.cache_read as f64),
        "cache_write_tokens" => SortValue::figure(t.cache_write() as f64),
        "in_per_run" => per_run(t.input),
        "out_per_run" => per_run(t.output),
        "cache_read_per_run" => per_run(t.cache_read),
        "cache_write_per_run" => per_run(t.cache_write()),
        "in_usd" => class(c.input),
        "out_usd" => class(c.output),
        "cache_read_usd" => class(c.cache_read),
        "cache_write_usd" => class(c.cache_write),
        "in_usd_per_run" => class(m.per_run(c.input)),
        "out_usd_per_run" => class(m.per_run(c.output)),
        "cache_read_usd_per_run" => class(m.per_run(c.cache_read)),
        "cache_write_usd_per_run" => class(m.per_run(c.cache_write)),
        "cost_usd" => priced.then_some(SortValue::Figure(0, m.cost)),
        "cost_per_run" => priced.then(|| SortValue::Figure(0, m.cost_per_run())),
        "unpriced" => SortValue::figure(m.unpriced as f64),
        "time_s" => SortValue::figure(m.time_s as f64),
        "time_per_run_s" => SortValue::figure(m.time_per_run_s()),
        // The naming columns left: `group`, `run`, `task`, `pipeline`,
        // `step` — each read off the row's export keys, where a lane with
        // no `group:` reads as blank rather than as the text `—`.
        key => lane_csv_keys(by)
            .iter()
            .position(|name| *name == key)
            .map(|i| row.keys[i].as_str())
            .filter(|cell| *cell != NO_GROUP)
            .and_then(SortValue::text),
    }
}

/// `rows`, reordered by `sort`.
fn sort_lane_rows(by: EvalBy, rows: &mut [LaneRow], sort: &Sort) {
    sort_rows(rows, sort, |row| lane_sort_value(by, row, &sort.key));
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
/// itself, because a `+?` on every affected cell is not a shape a pasted table
/// can parse as a number.
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
/// number is a floor rather than a total.
fn csv_cost(cost: f64, total: usize, unpriced: usize) -> String {
    match total.saturating_sub(unpriced) {
        0 => String::new(),
        _ => format!("{cost:.2}"),
    }
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
    /// What `tokens` cost at today's price table — see [`ClassCost`].
    class_cost: ClassCost,
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
        .map(|m| normalize_skill(m))
        .collect::<BTreeSet<_>>()
        .into_iter();
    match (names.next(), names.count()) {
        (None, _) => "—".to_string(),
        (Some(first), 0) => first.to_string(),
        (Some(first), more) => format!("{first} +{more}"),
    }
}

/// A skill's name as the `skill` filter and the `SKILL` cell both key on it —
/// a `<command-name>` marker keeps the leading slash a typed command was
/// written with, while a Skill `tool_use` marker never had one, so the two
/// forms of the same skill would otherwise read as two distinct values.
fn normalize_skill(marker: &str) -> &str {
    marker.trim_start_matches('/')
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
                class_cost: ClassCost::of(lines.iter().copied(), models),
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
/// summed. The per-session view divides each sum by `SESSIONS` the same way
/// a lanes row divides by `RUNS`, for comparing directories that ran a
/// different number of sessions — see [`Figures`].
struct DirRow {
    dir: String,
    sessions: usize,
    tokens: Tokens,
    /// What `tokens` cost at today's price table — see [`ClassCost`].
    class_cost: ClassCost,
    cost: f64,
    lines: usize,
    unpriced: usize,
    ctx_peak_tokens: Option<u64>,
    ctx_peak_pct: Option<f64>,
    ctx_avg: CtxAvg,
    time_s: i64,
    /// The latest of its own sessions' `when` — what orders the rows by
    /// default, the same "this morning's work heads the table" rule the
    /// lanes table follows. `None` for a watched root no session has run in yet, which
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

    /// The four class-cost cells in `figures`' view — each divided by
    /// `SESSIONS` per session — or `—` for a watched root no session has run
    /// in, the way its token cells read.
    fn class_cells(&self, figures: Figures) -> [String; 4] {
        let n = match (self.sessions, figures) {
            (0, _) => return std::array::from_fn(|_| "—".to_string()),
            (_, Figures::Totals) => 1.0,
            (n, Figures::PerRun) => n as f64,
        };
        self.class_cost.cells(|usd| usd / n)
    }
}

/// The `by dir` table's rows added into one, for its `Total` line and its
/// export's: every figure here adds up across directories, because a
/// session ran in exactly one of them. `TIME` is the sessions' own time
/// added up, not the calendar time they spanned. The peaks are left empty —
/// neither adds up.
fn dirs_total(rows: &[DirRow]) -> DirRow {
    let mut tokens = Tokens::default();
    for row in rows {
        tokens.add(&row.tokens);
    }
    DirRow {
        dir: String::new(),
        sessions: rows.iter().map(|r| r.sessions).sum(),
        tokens,
        class_cost: rows
            .iter()
            .fold(ClassCost::default(), |sum, r| sum.plus(&r.class_cost)),
        cost: rows.iter().map(|r| r.cost).sum(),
        lines: rows.iter().map(|r| r.lines).sum(),
        unpriced: rows.iter().map(|r| r.unpriced).sum(),
        ctx_peak_tokens: None,
        ctx_peak_pct: None,
        ctx_avg: CtxAvg::default(),
        time_s: rows.iter().map(|r| r.time_s).sum(),
        latest: None,
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
                class_cost: ClassCost::of(lines.iter().copied(), models),
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

/// The directory table's figure columns under `by dir` in the per-session
/// view — see [`lane_figures`] for why `SESSIONS` is drawn one wider than
/// its title. Each class cost is as wide as its own title.
fn dir_figures(cells: [&str; 14]) -> String {
    let [
        sessions,
        inp,
        inp_usd,
        out,
        out_usd,
        cr,
        cr_usd,
        cw,
        cw_usd,
        usd,
        usd_s,
        avg,
        peak,
        time,
    ] = cells;
    format!(
        "{SEP}{sessions:>9}{SEP}{inp:>10}{SEP}{inp_usd:>14}{SEP}{out:>11}{SEP}{out_usd:>15}\
         {SEP}{cr:>15}{SEP}{cr_usd:>19}{SEP}{cw:>15}{SEP}{cw_usd:>19}{SEP}{usd:>8}\
         {SEP}{usd_s:>11}{SEP}{avg:>12}{SEP}{peak:>8}{SEP}{time:>12}"
    )
}

/// The `by dir` table's figure columns in the totals view — see
/// [`lane_total_figures`] for the widths, which this table shares.
fn dir_total_figures(cells: [&str; 13]) -> String {
    let [
        sessions,
        inp,
        inp_usd,
        out,
        out_usd,
        cr,
        cr_usd,
        cw,
        cw_usd,
        usd,
        avg,
        peak,
        time,
    ] = cells;
    format!(
        "{SEP}{sessions:>9}{SEP}{inp:>7}{SEP}{inp_usd:>6}{SEP}{out:>7}{SEP}{out_usd:>7}\
         {SEP}{cr:>7}{SEP}{cr_usd:>11}{SEP}{cw:>7}{SEP}{cw_usd:>11}{SEP}{usd:>8}\
         {SEP}{avg:>12}{SEP}{peak:>8}{SEP}{time:>8}"
    )
}

/// The columns `by dir` draws in the per-session view, left to right, each
/// beside the export column a sort on it reads — see [`dir_sort_value`].
/// Each token class is followed by its cost at today's prices, as on the
/// lanes table — see [`ClassCost`].
const DIR_COLUMNS: [(&str, &str); 15] = [
    ("DIR", "dir"),
    ("SESSIONS", "sessions"),
    ("IN/SESSION", "in_per_session"),
    ("IN USD/SESSION", "in_usd_per_session"),
    ("OUT/SESSION", "out_per_session"),
    ("OUT USD/SESSION", "out_usd_per_session"),
    ("CACHE R/SESSION", "cache_read_per_session"),
    ("CACHE R USD/SESSION", "cache_read_usd_per_session"),
    ("CACHE W/SESSION", "cache_write_per_session"),
    ("CACHE W USD/SESSION", "cache_write_usd_per_session"),
    ("USD", "cost_usd"),
    ("USD/SESSION", "cost_per_session"),
    ("CTX PEAK AVG", "ctx_peak_avg_pct"),
    ("CTX PEAK", "ctx_peak_pct"),
    ("TIME/SESSION", "time_per_session_s"),
];

/// The columns `by dir` draws in the totals view. No `USD/SESSION`, for the
/// reason [`LANE_TOTAL_COLUMNS`] has no `USD/RUN`.
const DIR_TOTAL_COLUMNS: [(&str, &str); 14] = [
    ("DIR", "dir"),
    ("SESSIONS", "sessions"),
    ("IN", "in_tokens"),
    ("IN USD", "in_usd"),
    ("OUT", "out_tokens"),
    ("OUT USD", "out_usd"),
    ("CACHE R", "cache_read_tokens"),
    ("CACHE R USD", "cache_read_usd"),
    ("CACHE W", "cache_write_tokens"),
    ("CACHE W USD", "cache_write_usd"),
    ("USD", "cost_usd"),
    ("CTX PEAK AVG", "ctx_peak_avg_pct"),
    ("CTX PEAK", "ctx_peak_pct"),
    ("TIME", "time_s"),
];

/// The `by dir` table's totals, each beside its per-session figure — see
/// [`LANE_PAIRS`].
const DIR_PAIRS: [(&str, &str); 10] = [
    ("in_tokens", "in_per_session"),
    ("in_usd", "in_usd_per_session"),
    ("out_tokens", "out_per_session"),
    ("out_usd", "out_usd_per_session"),
    ("cache_read_tokens", "cache_read_per_session"),
    ("cache_read_usd", "cache_read_usd_per_session"),
    ("cache_write_tokens", "cache_write_per_session"),
    ("cache_write_usd", "cache_write_usd_per_session"),
    ("cost_usd", "cost_per_session"),
    ("time_s", "time_per_session_s"),
];

/// The columns `by session` draws, the same way — see
/// [`session_sort_value`].
const SESSION_COLUMNS: [(&str, &str); 14] = [
    ("WHEN", "when"),
    ("DIR", "dir"),
    ("SKILL", "skill"),
    ("MODEL", "model"),
    ("IN", "in_tokens"),
    ("IN USD", "in_usd"),
    ("OUT", "out_tokens"),
    ("OUT USD", "out_usd"),
    ("CACHE R", "cache_read_tokens"),
    ("CACHE R USD", "cache_read_usd"),
    ("CACHE W", "cache_write_tokens"),
    ("CACHE W USD", "cache_write_usd"),
    ("USD", "cost_usd"),
    ("TIME", "time_s"),
];

/// The columns the directory table draws under `by`. `by session` is one
/// session per row, so its figures read the same in either view and it has
/// only the one set.
fn dir_columns(by: DirBy, figures: Figures) -> &'static [(&'static str, &'static str)] {
    match (by, figures) {
        (DirBy::Dir, Figures::Totals) => &DIR_TOTAL_COLUMNS,
        (DirBy::Dir, Figures::PerRun) => &DIR_COLUMNS,
        (DirBy::Session, _) => &SESSION_COLUMNS,
    }
}

/// One `by dir` row's raw figure under export column `key`. A token,
/// class-cost or time figure is blank for a watched root no session has run
/// in, as its cell is, in either view.
fn dir_sort_value(row: &DirRow, key: &str) -> Option<SortValue> {
    let t = &row.tokens;
    let priced = row.lines > row.unpriced;
    let per = |n: u64| {
        row.tokens_per_session(n)
            .map(|n| SortValue::Figure(0, n as f64))
    };
    let sum = |n: f64| (row.sessions > 0).then_some(SortValue::Figure(0, n));
    // Unpriced class costs sort last, the way an unpriced `USD` does; so
    // does a watched root no session has run in, as its cells are `—`.
    let class = |usd: f64| {
        (row.sessions > 0 && row.class_cost.priced()).then_some(SortValue::Figure(0, usd))
    };
    let class_per = |usd: f64| {
        row.per_session(usd)
            .filter(|_| row.class_cost.priced())
            .map(|usd| SortValue::Figure(0, usd))
    };
    let c = &row.class_cost;
    match key {
        "dir" => SortValue::text(&row.dir),
        "sessions" => SortValue::figure(row.sessions as f64),
        "in_tokens" => sum(t.input as f64),
        "out_tokens" => sum(t.output as f64),
        "cache_read_tokens" => sum(t.cache_read as f64),
        "cache_write_tokens" => sum(t.cache_write() as f64),
        "time_s" => sum(row.time_s as f64),
        "in_per_session" => per(t.input),
        "out_per_session" => per(t.output),
        "cache_read_per_session" => per(t.cache_read),
        "cache_write_per_session" => per(t.cache_write()),
        "in_usd" => class(c.input),
        "out_usd" => class(c.output),
        "cache_read_usd" => class(c.cache_read),
        "cache_write_usd" => class(c.cache_write),
        "in_usd_per_session" => class_per(c.input),
        "out_usd_per_session" => class_per(c.output),
        "cache_read_usd_per_session" => class_per(c.cache_read),
        "cache_write_usd_per_session" => class_per(c.cache_write),
        "cost_usd" => priced.then_some(SortValue::Figure(0, row.cost)),
        "cost_per_session" => row
            .per_session(row.cost)
            .filter(|_| priced)
            .and_then(SortValue::figure),
        "ctx_peak_avg_pct" => ctx_sort_value(row.ctx_avg.tokens, row.ctx_avg.pct),
        "ctx_peak_pct" => ctx_sort_value(row.ctx_peak_tokens.map(|n| n as f64), row.ctx_peak_pct),
        "time_per_session_s" => row
            .per_session(row.time_s as f64)
            .and_then(SortValue::figure),
        _ => None,
    }
}

/// One `by session` row's raw figure under export column `key`. `DIR` sorts
/// on the name it draws, not the whole path — the path's shared prefix
/// would decide nothing.
fn session_sort_value(row: &SessionRow, key: &str) -> Option<SortValue> {
    let t = &row.tokens;
    // Unpriced class costs sort last, the way an unpriced `USD` does.
    let class = |usd: f64| row.class_cost.priced().then_some(SortValue::Figure(0, usd));
    let c = &row.class_cost;
    match key {
        "when" => SortValue::figure(row.when.timestamp() as f64),
        "dir" => SortValue::text(&dir_name(&row.dir)),
        "skill" => (row.skill != "—").then(|| SortValue::Text(row.skill.clone())),
        "model" => SortValue::text(&row.model),
        "in_tokens" => SortValue::figure(t.input as f64),
        "out_tokens" => SortValue::figure(t.output as f64),
        "cache_read_tokens" => SortValue::figure(t.cache_read as f64),
        "cache_write_tokens" => SortValue::figure(t.cache_write() as f64),
        "in_usd" => class(c.input),
        "out_usd" => class(c.output),
        "cache_read_usd" => class(c.cache_read),
        "cache_write_usd" => class(c.cache_write),
        "cost_usd" => (row.lines > row.unpriced).then_some(SortValue::Figure(0, row.cost)),
        "time_s" => SortValue::figure(row.time_s.max(0) as f64),
        _ => None,
    }
}

/// The `by dir` table over `rows`, in the order they are given, drawing the
/// figures of `figures` — `sort` only marks its header, as [`lanes_table`]'s
/// does. A watched root no session has run in draws `—` for every token,
/// class-cost and time figure in either view, so the row reads the same in
/// both.
fn dirs_table(rows: &[DirRow], sort: Option<&Sort>, figures: Figures) -> Table {
    let dw = rows
        .iter()
        .map(|r| r.dir.chars().count())
        .chain(["DIR".len(), "Average".len()])
        .max()
        .unwrap_or(3);
    let header = match figures {
        Figures::Totals => dir_total_figures([
            "SESSIONS",
            "IN",
            "IN USD",
            "OUT",
            "OUT USD",
            "CACHE R",
            "CACHE R USD",
            "CACHE W",
            "CACHE W USD",
            "USD",
            "CTX PEAK AVG",
            "CTX PEAK",
            "TIME",
        ]),
        Figures::PerRun => dir_figures([
            "SESSIONS",
            "IN/SESSION",
            "IN USD/SESSION",
            "OUT/SESSION",
            "OUT USD/SESSION",
            "CACHE R/SESSION",
            "CACHE R USD/SESSION",
            "CACHE W/SESSION",
            "CACHE W USD/SESSION",
            "USD",
            "USD/SESSION",
            "CTX PEAK AVG",
            "CTX PEAK",
            "TIME/SESSION",
        ]),
    };
    let header = format!("{:<dw$}{header}", "DIR");
    let (header, lead) = marked_header(header, dir_columns(DirBy::Dir, figures), sort);
    let dash = || "—".to_string();
    let lines = rows
        .iter()
        .map(|row| {
            let sessions = row.sessions.to_string();
            let usd = cost_of(row.cost, row.lines, row.unpriced);
            let (avg, peak) = (
                row.ctx_avg.cell(),
                ctx_cell(row.ctx_peak_tokens, row.ctx_peak_pct),
            );
            let [in_usd, out_usd, cr_usd, cw_usd] = row.class_cells(figures);
            let figures = match figures {
                Figures::Totals => {
                    let ran = row.sessions > 0;
                    let tok = |n: u64| if ran { tokens_cell(n) } else { dash() };
                    dir_total_figures([
                        &sessions,
                        &tok(row.tokens.input),
                        &in_usd,
                        &tok(row.tokens.output),
                        &out_usd,
                        &tok(row.tokens.cache_read),
                        &cr_usd,
                        &tok(row.tokens.cache_write()),
                        &cw_usd,
                        &usd,
                        &avg,
                        &peak,
                        &if ran {
                            crate::status::human_secs(row.time_s)
                        } else {
                            dash()
                        },
                    ])
                }
                Figures::PerRun => {
                    let tok = |n: u64| row.tokens_per_session(n).map_or_else(dash, tokens_cell);
                    dir_figures([
                        &sessions,
                        &tok(row.tokens.input),
                        &in_usd,
                        &tok(row.tokens.output),
                        &out_usd,
                        &tok(row.tokens.cache_read),
                        &cr_usd,
                        &tok(row.tokens.cache_write()),
                        &cw_usd,
                        &usd,
                        &row.per_session(row.cost)
                            .map_or_else(dash, |c| cost_of(c, row.lines, row.unpriced)),
                        &avg,
                        &peak,
                        &row.per_session(row.time_s as f64)
                            .map_or_else(dash, |t| crate::status::human_secs(t.round() as i64)),
                    ])
                }
            };
            format!("{:<dw$}{figures}", row.dir)
        })
        .collect();
    // Both `CTX PEAK` cells stay blank, as on the lanes table's own line;
    // a table whose watched roots have run no session yet draws `—` for its
    // token, class-cost and time figures, the same as each of those rows does.
    let total = dirs_total(rows);
    let ran = total.sessions > 0;
    let t = &total.tokens;
    let sessions = total.sessions.to_string();
    let [in_usd, out_usd, cr_usd, cw_usd] = total.class_cells(figures);
    let total_figures = match figures {
        Figures::Totals => {
            let tok = |n: u64| if ran { tokens_cell(n) } else { dash() };
            dir_total_figures([
                &sessions,
                &tok(t.input),
                &in_usd,
                &tok(t.output),
                &out_usd,
                &tok(t.cache_read),
                &cr_usd,
                &tok(t.cache_write()),
                &cw_usd,
                &cost_of(total.cost, total.lines, total.unpriced),
                "",
                "",
                &if ran {
                    crate::status::human_secs(total.time_s)
                } else {
                    dash()
                },
            ])
        }
        // `USD` is blank on the average session, for the reason the lanes
        // table's average run leaves it blank — see `lanes_table`.
        Figures::PerRun => {
            let tok = |n: u64| total.tokens_per_session(n).map_or_else(dash, tokens_cell);
            dir_figures([
                &sessions,
                &tok(t.input),
                &in_usd,
                &tok(t.output),
                &out_usd,
                &tok(t.cache_read),
                &cr_usd,
                &tok(t.cache_write()),
                &cw_usd,
                "",
                &total
                    .per_session(total.cost)
                    .map_or_else(dash, |c| cost_of(c, total.lines, total.unpriced)),
                "",
                "",
                &total
                    .per_session(total.time_s as f64)
                    .map_or_else(dash, |t| crate::status::human_secs(t.round() as i64)),
            ])
        }
    };
    let total = format!("{:<dw$}{total_figures}", total_line_label(figures))
        .trim_end()
        .to_string();
    Table {
        header,
        lead,
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
    class_usd: [&str; 4],
    usd: &str,
    time: &str,
) -> String {
    let [inp, out, cr, cw] = tokens;
    // Each class cost is as wide as its own title.
    let [inp_usd, out_usd, cr_usd, cw_usd] = class_usd;
    format!(
        "{when:<w$}{SEP}{dir:<dw$}{SEP}{skill:<s$}{SEP}{model:<mw$}{SEP}{inp:>6}{SEP}{inp_usd:>6}\
         {SEP}{out:>7}{SEP}{out_usd:>7}{SEP}{cr:>8}{SEP}{cr_usd:>11}{SEP}{cw:>8}{SEP}{cw_usd:>11}\
         {SEP}{usd:>5}{SEP}{time:>8}",
        w = SESSIONS_WHEN_WIDTH,
        s = SESSIONS_SKILL_WIDTH,
    )
}

/// Every session's class costs added into one, for the `by session` `Total`
/// line and its export's.
fn sessions_class_cost(rows: &[SessionRow]) -> ClassCost {
    rows.iter()
        .fold(ClassCost::default(), |sum, r| sum.plus(&r.class_cost))
}

/// The `by session` table over `rows`, in the order they are given — `sort`
/// only marks its header, as [`lanes_table`]'s does.
fn sessions_table(rows: &[SessionRow], sort: Option<&Sort>) -> Table {
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
        ["IN USD", "OUT USD", "CACHE R USD", "CACHE W USD"],
        "USD",
        "TIME",
    );
    let (header, lead) = marked_header(header, &SESSION_COLUMNS, sort);
    let lines = rows
        .iter()
        .map(|row| {
            let [in_usd, out_usd, cr_usd, cw_usd] = row.class_cost.cells(|usd| usd);
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
                [&in_usd, &out_usd, &cr_usd, &cw_usd],
                &cost_of(row.cost, row.lines, row.unpriced),
                &crate::status::human_secs(row.time_s.max(0)),
            )
        })
        .collect();
    // One row per session, so every raw figure on it adds up: each token
    // class and its cost, USD and the time the sessions spanned.
    let mut tokens = Tokens::default();
    for row in rows {
        tokens.add(&row.tokens);
    }
    let [in_usd, out_usd, cr_usd, cw_usd] = sessions_class_cost(rows).cells(|usd| usd);
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
        [&in_usd, &out_usd, &cr_usd, &cw_usd],
        &cost_of(cost, lines_n, unpriced),
        &crate::status::human_secs(time),
    );
    Table {
        header,
        lead,
        rows: lines,
        total,
    }
}

// ------------------------------------------------------ directories export

/// Each token class's cost follows the token columns, then its
/// `_per_session` twin, in the order the lanes export puts its own.
const DIRS_CSV_HEADER: &str = "by,dir,sessions,in_tokens,out_tokens,cache_read_tokens,\
                               cache_write_tokens,in_per_session,out_per_session,\
                               cache_read_per_session,cache_write_per_session,in_usd,\
                               out_usd,cache_read_usd,cache_write_usd,in_usd_per_session,\
                               out_usd_per_session,cache_read_usd_per_session,\
                               cache_write_usd_per_session,cost_usd,cost_per_session,\
                               unpriced,ctx_peak_tokens,ctx_peak_pct,ctx_peak_avg_tokens,\
                               ctx_peak_avg_pct,time_s,time_per_session_s";

/// No `_per_session` twins here: one session per row, so each would repeat
/// the figure beside it.
const SESSIONS_CSV_HEADER: &str = "by,when,dir,skill,model,in_tokens,out_tokens,\
                                   cache_read_tokens,cache_write_tokens,in_usd,out_usd,\
                                   cache_read_usd,cache_write_usd,cost_usd,unpriced,\
                                   ctx_peak_tokens,ctx_peak_pct,time_s";

/// A class cost the way the CSV spells it: a blank, not the screen's `—`,
/// for a watched root no session has run in.
fn csv_class_cells(row: &DirRow, figures: Figures) -> [String; 4] {
    match row.sessions {
        0 => Default::default(),
        _ => row.class_cells(figures),
    }
}

fn csv_dir_row(row: &DirRow) -> String {
    let t = &row.tokens;
    let per = |n: u64| opt_u64(row.tokens_per_session(n));
    let [in_usd, out_usd, cr_usd, cw_usd] = csv_class_cells(row, Figures::Totals);
    let [in_usd_s, out_usd_s, cr_usd_s, cw_usd_s] = csv_class_cells(row, Figures::PerRun);
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
        in_usd,
        out_usd,
        cr_usd,
        cw_usd,
        in_usd_s,
        out_usd_s,
        cr_usd_s,
        cw_usd_s,
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
    let [in_usd, out_usd, cr_usd, cw_usd] = row.class_cost.cells(|usd| usd);
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
        in_usd,
        out_usd,
        cr_usd,
        cw_usd,
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
/// line carries in either view — under `by dir` the sums and each
/// per-session column as its sum over the table's sessions; under
/// `by session` every token class and its cost, USD and time.
fn csv_dirs_total(by: DirBy, dirs: &[DirRow], sessions: &[SessionRow]) -> String {
    let header = match by {
        DirBy::Dir => DIRS_CSV_HEADER,
        DirBy::Session => SESSIONS_CSV_HEADER,
    };
    let mut tokens = Tokens::default();
    for row in sessions {
        tokens.add(&row.tokens);
    }
    let dir = dirs_total(dirs);
    let (cost, lines, unpriced) = match by {
        DirBy::Dir => (dir.cost, dir.lines, dir.unpriced),
        DirBy::Session => (
            sessions.iter().map(|r| r.cost).sum::<f64>(),
            sessions.iter().map(|r| r.lines).sum::<usize>(),
            sessions.iter().map(|r| r.unpriced).sum::<usize>(),
        ),
    };
    let per = |n: u64| opt_u64(dir.tokens_per_session(n));
    let class = match by {
        DirBy::Dir => csv_class_cells(&dir, Figures::Totals),
        DirBy::Session => sessions_class_cost(sessions).cells(|usd| usd),
    };
    let class_per = csv_class_cells(&dir, Figures::PerRun);
    let [in_usd, out_usd, cr_usd, cw_usd] = &class;
    let [in_usd_s, out_usd_s, cr_usd_s, cw_usd_s] = &class_per;
    header
        .split(',')
        .map(|name| match (by, name) {
            (_, "by") => "total".to_string(),
            (_, "cost_usd") => csv_cost(cost, lines, unpriced),
            (_, "in_usd") => in_usd.clone(),
            (_, "out_usd") => out_usd.clone(),
            (_, "cache_read_usd") => cr_usd.clone(),
            (_, "cache_write_usd") => cw_usd.clone(),
            (DirBy::Dir, "in_usd_per_session") => in_usd_s.clone(),
            (DirBy::Dir, "out_usd_per_session") => out_usd_s.clone(),
            (DirBy::Dir, "cache_read_usd_per_session") => cr_usd_s.clone(),
            (DirBy::Dir, "cache_write_usd_per_session") => cw_usd_s.clone(),
            (DirBy::Dir, "sessions") => dir.sessions.to_string(),
            (DirBy::Dir, "in_tokens") => dir.tokens.input.to_string(),
            (DirBy::Dir, "out_tokens") => dir.tokens.output.to_string(),
            (DirBy::Dir, "cache_read_tokens") => dir.tokens.cache_read.to_string(),
            (DirBy::Dir, "cache_write_tokens") => dir.tokens.cache_write().to_string(),
            (DirBy::Dir, "in_per_session") => per(dir.tokens.input),
            (DirBy::Dir, "out_per_session") => per(dir.tokens.output),
            (DirBy::Dir, "cache_read_per_session") => per(dir.tokens.cache_read),
            (DirBy::Dir, "cache_write_per_session") => per(dir.tokens.cache_write()),
            (DirBy::Dir, "cost_per_session") => dir
                .per_session(dir.cost)
                .map_or(String::new(), |c| csv_cost(c, dir.lines, dir.unpriced)),
            (DirBy::Dir, "time_s") => dir.time_s.max(0).to_string(),
            (DirBy::Dir, "time_per_session_s") => dir
                .per_session(dir.time_s as f64)
                .map_or(String::new(), |t| (t.round() as i64).to_string()),
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

// ------------------------------------------------------------------- trials
//
// The screen's third table: one row per trial whose arms banked a lane in
// the window. Read off the ledger alone, because a settled trial's arms are
// deleted and the ledger is the only record of it left — except `STATE`,
// which asks the queue whether any arm is still in it.

/// One trial, as the trials table draws it.
struct TrialRow {
    /// The trial id every arm's ledger line carries — what `enter` sets the
    /// `trial` filter to. Never drawn: `t9f3a…` means nothing to a person.
    id: String,
    /// The group a person tried — see [`trial_group_of`].
    group: String,
    /// The earliest arm line's own timestamp, raw, so two trials started on
    /// the same day still order by when each began.
    first_ts: String,
    /// Every pipeline an arm ran under, in name order.
    pipelines: Vec<String>,
    /// Distinct arm tasks — a task banks a line per step, and counting those
    /// would read a five-step arm as five.
    arms: usize,
    /// A queued task still carries this trial's id.
    running: bool,
}

impl TrialRow {
    /// How a trial is named wherever a person picks one: its group and the
    /// day it started — the `trial` filter row's own spelling.
    fn name(&self) -> String {
        format!("{} · {}", self.group, local_date(&self.first_ts))
    }

    fn state(&self) -> &'static str {
        match self.running {
            true => "running",
            false => "settled",
        }
    }
}

/// The group a trial's arms were forked from, read off `lines` — all one
/// trial's. [`Entry::trial_group`] names it on every arm minted since the
/// trial forked a group per pipeline. An older arm ran in the source group
/// itself, so its own [`Entry::plan`] is the right answer there; and an arm
/// with neither is named by the trial id, the one thing it is sure to carry.
fn trial_group_of(id: &str, lines: &[&Entry]) -> String {
    lines
        .iter()
        .find_map(|e| e.trial_group.clone())
        .or_else(|| lines.iter().find_map(|e| e.plan.clone()))
        .unwrap_or_else(|| id.to_string())
}

/// Every trial `entries` hold an arm line of, newest first — by when each
/// one started, so a trial still running does not jump to the top every
/// time one of its arms banks a step. `running` holds the trial ids a
/// queued task still carries — see [`Loaded::running_trials`].
fn trial_rows(entries: &[Entry], running: &BTreeSet<String>) -> Vec<TrialRow> {
    let mut by_trial: BTreeMap<&str, Vec<&Entry>> = BTreeMap::new();
    for entry in entries {
        if let Some(trial) = entry.trial.as_deref() {
            by_trial.entry(trial).or_default().push(entry);
        }
    }
    let mut rows: Vec<TrialRow> = by_trial
        .into_iter()
        .map(|(id, lines)| TrialRow {
            id: id.to_string(),
            group: trial_group_of(id, &lines),
            first_ts: lines
                .iter()
                .map(|e| e.ts.as_str())
                .min()
                .unwrap_or_default()
                .to_string(),
            pipelines: distinct(lines.iter().copied(), |e| Some(e.pipeline.as_str())),
            arms: lines
                .iter()
                .map(|e| e.task.as_str())
                .collect::<HashSet<_>>()
                .len(),
            running: running.contains(id),
        })
        .collect();
    // The id breaks a tie, so two trials begun in the same second keep one
    // order from draw to draw.
    rows.sort_by(|a, b| b.first_ts.cmp(&a.first_ts).then_with(|| a.id.cmp(&b.id)));
    rows
}

/// The columns the trials table draws, each beside the key its sort reads —
/// the same shape [`DIR_COLUMNS`] has, though this table has no export: the
/// key only names the column to [`trial_sort_value`].
const TRIAL_COLUMNS: [(&str, &str); 5] = [
    ("GROUP", "group"),
    ("WHEN", "when"),
    ("PIPELINES", "pipelines"),
    ("ARMS", "arms"),
    ("STATE", "state"),
];

/// One trials row's raw figure under `key`. `WHEN` sorts on the raw start,
/// not the drawn day, for the reason [`trial_rows`] orders by it.
fn trial_sort_value(row: &TrialRow, key: &str) -> Option<SortValue> {
    match key {
        "group" => SortValue::text(&row.group),
        "when" => SortValue::text(&row.first_ts),
        "pipelines" => SortValue::text(&row.pipelines.join(", ")),
        "arms" => SortValue::figure(row.arms as f64),
        "state" => SortValue::text(row.state()),
        _ => None,
    }
}

/// `PIPELINES` is fixed rather than measured, as the mockup draws it: a
/// trial can tick every pipeline a project has, and one long list would
/// push `ARMS` and `STATE` off the right of the frame.
const TRIAL_PIPELINES_WIDTH: usize = 18;

/// `pipelines` joined into [`TRIAL_PIPELINES_WIDTH`] columns, dropping whole
/// names from the end behind a `…` rather than cutting one in half — a
/// clipped `impl_t…` could be any of several pipelines. Only a first name
/// too long on its own is cut, since there is nothing shorter to show.
fn pipelines_cell(pipelines: &[String]) -> String {
    let all = pipelines.join(", ");
    if all.chars().count() <= TRIAL_PIPELINES_WIDTH {
        return all;
    }
    (1..pipelines.len())
        .rev()
        .map(|k| format!("{}, …", pipelines[..k].join(", ")))
        .find(|cell| cell.chars().count() <= TRIAL_PIPELINES_WIDTH)
        .unwrap_or_else(|| clip_cell(&all, TRIAL_PIPELINES_WIDTH))
}

fn trial_line(
    gw: usize,
    group: &str,
    when: &str,
    pipelines: &str,
    arms: &str,
    state: &str,
) -> String {
    format!(
        "{group:<gw$}{SEP}{when:<10}{SEP}{pipelines:<pw$}{SEP}{arms:>4}{SEP}{state}",
        pw = TRIAL_PIPELINES_WIDTH,
    )
}

/// The trials table over `rows`, in the order they are given — `sort` only
/// marks its header, as [`lanes_table`]'s does. No `Total` line: a trial is
/// a comparison of its own, and adding arms across trials answers nothing.
fn trials_table(rows: &[TrialRow], sort: Option<&Sort>) -> Vec<Line> {
    let gw = rows
        .iter()
        .map(|r| r.group.chars().count())
        .chain(["GROUP".len()])
        .max()
        .unwrap_or(0);
    let header = trial_line(gw, "GROUP", "WHEN", "PIPELINES", "ARMS", "STATE");
    let (header, lead) = marked_header(header, &TRIAL_COLUMNS, sort);
    let mut out = vec![Line::Head(lead, header)];
    out.extend(rows.iter().map(|row| {
        Line::Row(Row {
            text: trial_line(
                gw,
                &row.group,
                &local_date(&row.first_ts),
                &pipelines_cell(&row.pipelines),
                &row.arms.to_string(),
                row.state(),
            ),
        })
    }));
    out
}

// ----------------------------------------------------------------- the screen
//
// Bare `spoolway`'s eval tab, the one place left that reaches this any more
// — `spoolway eval` itself always prints now, whatever flags it carries.
// Raw mode through `platform::TermGuard`, one byte at a time off stdin
// through `crate::screen::read_key`, so a real tty in raw mode and a pipe an
// end-to-end suite is scripting drive it identically, and the screen ends
// the moment either runs out.
//
// Three tables: `tab` cycles from the lanes to the watched directories to
// the trials and back, which `by` and which filter are rows on the filter
// panel, and the sort is the popup `a` or `d` opens. The one drill-in is
// `enter` on a trial, which is nothing but a filter set for the person: the
// lanes table, narrowed to that trial and grouped by pipeline. Every lanes
// and directory table is built by the same functions the
// printing path uses (`lanes_table`, `dirs_table`, `sessions_table`), so the
// screen and a pasted `spoolway eval --by step` can never disagree about a
// column. Rendered without colour throughout: `pad_to` counts every
// character as one column, and an ANSI escape slipped into a row would throw
// the frame's own border out of line with it.

/// Which of the three tables is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TableKind {
    Lanes,
    Dirs,
    Trials,
}

impl TableKind {
    /// The table `tab` moves to: the trials table last, so `tab` from an
    /// opened trial's lanes table runs through the directories back to the
    /// list it was opened from.
    fn next(self) -> TableKind {
        match self {
            TableKind::Lanes => TableKind::Dirs,
            TableKind::Dirs => TableKind::Trials,
            TableKind::Trials => TableKind::Lanes,
        }
    }

    /// What `tab` names this table as, in the key line of the one before it.
    /// The lanes table is `runs` on screen, as its own `RUNS` column counts.
    fn label(self) -> &'static str {
        match self {
            TableKind::Lanes => "runs",
            TableKind::Dirs => "dirs",
            TableKind::Trials => "trials",
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
    /// What the top border's right side names this scope as. This project
    /// alone is left unnamed: it is the default, and the person is in it. Any
    /// other ledger is named, so the frame still says which one it came from.
    fn label(&self) -> String {
        match self {
            Scope::Mine => String::new(),
            Scope::Named(name) => name.clone(),
            Scope::All => "all projects".to_string(),
        }
    }
}

/// Every row of every filter panel, as currently applied — what `load`
/// reads the ledger through and what each table narrows it by. `since` and
/// `until` hold a plain `YYYY-MM-DD` the calendar wrote in, or a duration
/// (`7d`, `24h`) passed on the command line — not yet parsed either way:
/// parsing happens once, in `load`, so a bad value is reported in one place.
///
/// The tables keep their own `by` and their own narrowing rows, so
/// `tab` back to a table finds it the way it was left; `since` and `until`
/// are shared, since a window bounds every table the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Filters {
    by: EvalBy,
    dir_by: DirBy,
    group: Option<String>,
    task: Option<String>,
    pipeline: Option<String>,
    step: Option<String>,
    version: Option<String>,
    /// The trial id a lane must have been an arm of — see [`Entry::trial`].
    /// Held as the id, which is what the ledger matches on; drawn through
    /// [`find_trial`] as the group a person knows it by.
    trial: Option<String>,
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
    /// Each table's own sort, picked from the popup `a` or `d` opens — kept
    /// here rather than on the screen so it rides along with every change of
    /// `by` or a filter row, and with `r`, exactly as the rows above do.
    /// `None` is the default order. One a view does not draw the column of
    /// is set aside there, not cleared — see [`shown_sort`].
    lane_sort: Option<Sort>,
    dir_sort: Option<Sort>,
    trial_sort: Option<Sort>,
    /// Which figures the lanes and directory tables draw, flipped by `t` —
    /// kept here for the same reason the sorts are, so it rides along with
    /// `tab`, `r`, a change of `by` and the filter panel. Never saved: every
    /// visit opens on totals.
    figures: Figures,
}

impl Filters {
    /// What the screen opens with: by pipeline and by dir, every row blank,
    /// every table in its default order and on its totals — the screen has
    /// no `--sort` or `--per-run` of its own to start from, since only
    /// `spoolway eval` prints with them.
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
            trial: args.trial.clone(),
            dir: None,
            skill: None,
            since: args.since.clone().unwrap_or_default(),
            until: args.until.clone().unwrap_or_default(),
            scope,
            lane_sort: None,
            dir_sort: None,
            trial_sort: None,
            figures: Figures::Totals,
        }
    }

    /// `t`: the other set of figures on the lanes and directory tables at
    /// once, the lanes sort carried to the matching column — see
    /// [`Figures::carry`]. The directory sort is not carried here but read
    /// through [`Filters::shown_dir_sort`] each time it is drawn: `by dir`
    /// and `by session` share it, and `t` pressed under either would carry
    /// it to a column the other may not draw.
    fn flip_figures(&mut self) {
        self.figures = self.figures.other();
        self.figures.carry(&mut self.lane_sort, &LANE_PAIRS);
    }

    /// The directory sort as the view on screen draws its column: under `by
    /// dir` in its current figures, and under `by session`, whose columns
    /// are totals in either view, as a total. So a sort on `IN/SESSION`
    /// reads as one on `IN` once `by session` or `t` puts `IN` on screen,
    /// whichever order the two were changed in.
    fn shown_dir_sort(&self) -> Option<Sort> {
        let view = match self.dir_by {
            DirBy::Dir => self.figures,
            DirBy::Session => Figures::Totals,
        };
        let mut sort = self.dir_sort.clone();
        view.carry(&mut sort, &DIR_PAIRS);
        sort
    }

    fn lanes(&self) -> LaneFilters<'_> {
        LaneFilters {
            group: self.group.as_deref(),
            task: self.task.as_deref(),
            pipeline: self.pipeline.as_deref(),
            step: self.step.as_deref(),
            version: self.version.as_deref(),
            trial: self.trial.as_deref(),
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
    /// Every skill each of `dirs`' own sessions ran, whether a
    /// `<command-name>` marker or a Skill `tool_use` named it, keyed by
    /// session id — read once here rather than on every draw, since a
    /// transcript scan is a file read `draw` cannot afford to repeat on every
    /// keypress. A subagent's own markers are folded onto its parent's set
    /// here in `load`, so this never holds an `agent-<id>` key. What the
    /// `skill` filter row cycles over, what a chosen `skill` narrows `dirs`
    /// by — see [`scoped_dirs`] — and what the `SKILL` column names.
    skills_by_session: HashMap<String, BTreeSet<String>>,
    /// Each of `dirs`' own sessions' transcript span — see
    /// [`crate::usage::session_span`] — keyed by session id, read once here
    /// for the same reason `skills_by_session` is. Absent for a session whose
    /// span could not be read at all — `list_sessions` falls back to its own
    /// line's `ts` for that one.
    spans_by_session: Spans,
    fallback: HashMap<(String, String), String>,
    models: BTreeMap<String, ModelPrice>,
    /// Every trial id a task in this project's queue still carries — what
    /// tells a trial's `STATE` `running` from `settled`. Read once here for
    /// the reason `skills_by_session` is. Empty under any other scope: the
    /// screen only reaches this project's own queue, and the tab it opens in
    /// always reads this project.
    running_trials: BTreeSet<String>,
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

    let window = crate::spend::window_of(non_empty(&filters.since), non_empty(&filters.until))?;
    entries.retain(|entry| window.contains(&entry.ts));
    entries.sort_by(|a, b| a.ts.cmp(&b.ts));
    dirs.retain(|entry| window.contains(&entry.ts));
    dirs.sort_by(|a, b| a.ts.cmp(&b.ts));

    // Every distinct (session, kind) in `dirs` — parents and subagents
    // alike — read in one pass: see `usage::read_sessions`, which resolves a
    // whole batch of sessions per kind with one directory walk apiece,
    // rather than the one-walk-per-session `session_file` would otherwise
    // cost here. A post-fold session (below) is *usually* already a member
    // of this same set, since folding ordinarily renames a subagent row onto
    // a parent row that is itself one of `dirs`' own sessions — but not
    // always: the window or the ledger can hold a subagent's own line
    // without the parent's, in which case the parent never earned a row
    // here at all. That gap is read separately, below, once folding has
    // found it.
    let distinct: BTreeSet<(String, String)> = dirs
        .iter()
        .map(|e| (e.session.clone(), e.kind.clone()))
        .collect();
    let reads = crate::usage::read_sessions(
        distinct
            .iter()
            .map(|(session, kind)| (kind.as_str(), session.as_str())),
    );

    // A subagent has no session of its own to show: `agent-<id>` is only the
    // filename Claude Code happened to write its transcript under, next to
    // the parent session that actually ran it — see `usage::SessionRead`'s
    // `parent`. Folded here, before anything groups `dirs` by session at
    // all, so every later read of this table — the row itself, its skill
    // markers, its span — already sees one session, not two. A subagent
    // whose transcript has since been swept off disk keeps its own
    // `agent-<id>` row instead: there is no path left to read its parent's
    // name off of.
    let subagents: Vec<(String, String, String, &BTreeSet<String>)> = distinct
        .iter()
        .filter_map(|(session, kind)| {
            let read = reads.get(session)?;
            let parent = read.parent.clone()?;
            Some((session.clone(), kind.clone(), parent, &read.skills))
        })
        .collect();
    // parent -> a kind that read it, for the gap below — a subagent and the
    // parent it folds onto are always the same kind, so this is the kind a
    // parent's own row would have carried had it had one in `dirs`.
    let parent_kind: HashMap<&str, &str> = subagents
        .iter()
        .map(|(_, kind, parent, _)| (parent.as_str(), kind.as_str()))
        .collect();
    // session -> parent, keyed for a single lookup per `dirs` entry instead
    // of a scan of `subagents` for every one — see the task's own account of
    // this fold.
    let subagent_parent: HashMap<&str, &str> = subagents
        .iter()
        .map(|(session, _, parent, _)| (session.as_str(), parent.as_str()))
        .collect();
    for entry in &mut dirs {
        if let Some(parent) = subagent_parent.get(entry.session.as_str()) {
            entry.session = (*parent).to_string();
        }
    }

    // A parent folded onto here that never had its own line in `dirs` (see
    // the gap called out above `distinct`) is not in `reads` at all yet —
    // resolved with one more batched call, so it is still read exactly
    // once, just not in the first batch.
    let missing: BTreeSet<&str> = dirs
        .iter()
        .map(|e| e.session.as_str())
        .filter(|session| !reads.contains_key(*session))
        .collect();
    let extra_reads = crate::usage::read_sessions(
        missing
            .iter()
            .filter_map(|session| Some((*parent_kind.get(session)?, *session))),
    );

    let mut skills_by_session: HashMap<String, BTreeSet<String>> = HashMap::new();
    let mut spans_by_session = HashMap::new();
    for session in dirs
        .iter()
        .map(|e| e.session.as_str())
        .collect::<BTreeSet<_>>()
    {
        let read = reads.get(session).or_else(|| extra_reads.get(session));
        skills_by_session.insert(
            session.to_string(),
            read.map(|r| r.skills.clone()).unwrap_or_default(),
        );
        if let Some(span) = read.and_then(|r| r.span) {
            spans_by_session.insert(session.to_string(), span);
        }
    }
    // A subagent's own skills belong on its parent's row too — its transcript
    // is never one of `dirs`' sessions any more after the fold above, so the
    // loop over `dirs` just ran can never have read it. The markers were
    // already read above, in the first batch (`reads`), since a subagent's
    // own session is always one of `dirs`' sessions, so this is no further
    // disk access.
    for (_, _, parent, markers) in &subagents {
        skills_by_session
            .entry(parent.clone())
            .or_default()
            .extend(markers.iter().cloned());
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

    let running_trials = match filters.scope {
        Scope::Mine => repo
            .tasks()
            .context("reading the queue for running trials")?
            .into_iter()
            .filter_map(|task| task.front.trial)
            .collect(),
        Scope::Named(_) | Scope::All => BTreeSet::new(),
    };

    Ok(Loaded {
        entries,
        dirs,
        roots,
        skills_by_session,
        spans_by_session,
        fallback,
        models: repo.config.models.clone(),
        running_trials,
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
                    .is_some_and(|markers| {
                        markers
                            .iter()
                            .any(|m| normalize_skill(m) == normalize_skill(skill))
                    })
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

/// Every trial in the window, newest first, by id — the list the `trial`
/// row cycles over. Newest first because a project can hold many trials,
/// and the one a person is looking for is most likely the one just run.
fn trial_candidates(loaded: &Loaded) -> Vec<String> {
    trial_rows(&loaded.entries, &loaded.running_trials)
        .into_iter()
        .map(|row| row.id)
        .collect()
}

/// The trial `id`'s row, where the window holds a line of it — what the
/// filter row and the top border name it by. `None` before the first load
/// lands, or once a `since`/`until` has moved the trial out of the window;
/// both callers fall back to the id, which is still better than naming
/// nothing.
fn find_trial(loaded: Option<&Loaded>, id: &str) -> Option<TrialRow> {
    let loaded = loaded?;
    trial_rows(&loaded.entries, &loaded.running_trials)
        .into_iter()
        .find(|row| row.id == id)
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

/// Every skill any of `loaded.dirs`' own sessions ran, whether a
/// `<command-name>` marker or a Skill `tool_use` named it — the candidate
/// list the `skill` row cycles over. Built from the watched transcripts
/// themselves, never from a configured list.
///
/// One candidate per [`normalize_skill`] key, not one per marker spelling:
/// a `<command-name>`'s own leading slash is kept as that skill's candidate
/// wherever any session's marker carries one, so a skill only ever named
/// through the Skill tool is the one case this shows without it.
fn skill_candidates(loaded: &Loaded) -> Vec<String> {
    let mut by_key: BTreeMap<&str, &str> = BTreeMap::new();
    for marker in loaded.skills_by_session.values().flatten() {
        let key = normalize_skill(marker);
        let slot = by_key.entry(key).or_insert(marker);
        if marker.starts_with('/') {
            *slot = marker;
        }
    }
    by_key.into_values().map(String::from).collect()
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

/// One line of a table's body: a row the cursor can land on, or text — the
/// header, the `Total` line, a note that nothing is here — that it skips
/// over.
enum Line {
    Text(String),
    /// A table's `Total` line, always its last. Drawn as [`Line::Text`] is,
    /// but kept out of the scroll: [`eval_frame_rows`] pins it under the
    /// rows and the scroll indicator, so a long table's total is on screen
    /// wherever the cursor has scrolled to.
    Total(String),
    /// A table's header, drawn after `lead` in the column a row's marker
    /// takes — a space, or the sort mark of a sorted first column: see
    /// [`mark_column`].
    Head(char, String),
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
            Line::Text(text) | Line::Total(text) => out.push(pad_to(&format!(" {text}"), width)),
            Line::Head(lead, text) => out.push(pad_to(&format!("{lead}{text}"), width)),
            Line::Row(row) => {
                let idx = row_n.expect("a Line::Row always has a row number");
                let marker = if idx == cursor { ">" } else { " " };
                out.push(pad_to(&format!("{marker}{}", row.text), width));
            }
        }
    }
    out
}

/// Which row number each of `lines` is, `None` for a [`Line::Text`], a
/// [`Line::Total`] or a [`Line::Head`] — a
/// `Line::Row`'s position among only the other rows, skipping the text lines
/// in between. What lets [`render_lines`] and [`cursor_line_index`] compare
/// a line against the cursor without a hand-rolled counter of their own.
fn row_numbers(lines: &[Line]) -> Vec<Option<usize>> {
    let mut next = 0usize;
    lines
        .iter()
        .map(|line| match line {
            Line::Text(_) | Line::Total(_) | Line::Head(..) => None,
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
    let mut out = vec![Line::Head(table.lead, table.header)];
    out.extend(table.rows.into_iter().map(|text| Line::Row(Row { text })));
    out.push(Line::Total(table.total));
    out
}

/// The lanes rows the screen shows over `entries`: the `by`'s default
/// order, then the lanes sort where this view draws its column — and that
/// sort, for the header to mark. Shared by the view and by `e`, so an
/// export can never write rows in an order the screen did not show.
fn screen_lane_rows<'a>(
    loaded: &Loaded,
    filters: &'a Filters,
    pipelines: &Pipelines,
    entries: &[&Entry],
) -> (Vec<LaneRow>, Option<&'a Sort>) {
    let mut rows = lane_rows(
        entries,
        filters.by,
        &loaded.fallback,
        &loaded.models,
        Some(pipelines),
    );
    let sort = shown_sort(
        filters.lane_sort.as_ref(),
        &drawn_lane_columns(filters.by, &rows, filters.figures),
    );
    if let Some(sort) = sort {
        sort_lane_rows(filters.by, &mut rows, sort);
    }
    (rows, sort)
}

/// `by dir`'s rows as the screen shows them — see [`screen_lane_rows`].
fn screen_dir_rows(
    loaded: &Loaded,
    filters: &Filters,
    entries: &[&Entry],
) -> (Vec<DirRow>, Option<Sort>) {
    let mut rows = dir_rows(
        entries,
        seeded_roots(loaded, filters),
        &loaded.models,
        &loaded.spans_by_session,
    );
    let sort = shown_sort(
        filters.shown_dir_sort().as_ref(),
        dir_columns(DirBy::Dir, filters.figures),
    )
    .cloned();
    if let Some(sort) = &sort {
        sort_rows(&mut rows, sort, |row| dir_sort_value(row, &sort.key));
    }
    (rows, sort)
}

/// `by session`'s rows as the screen shows them — newest first by default,
/// since a person opening the table wants to know what just ran, not what
/// ran first. See [`screen_lane_rows`].
fn screen_sessions(
    loaded: &Loaded,
    filters: &Filters,
    entries: &[&Entry],
) -> (Vec<SessionRow>, Option<Sort>) {
    let mut rows = list_sessions(
        entries,
        &loaded.models,
        &loaded.spans_by_session,
        &loaded.skills_by_session,
    );
    rows.reverse();
    let sort = shown_sort(filters.shown_dir_sort().as_ref(), &SESSION_COLUMNS).cloned();
    if let Some(sort) = &sort {
        sort_rows(&mut rows, sort, |row| session_sort_value(row, &sort.key));
    }
    (rows, sort)
}

/// The trials table's rows as the screen shows them — newest first by
/// default, see [`trial_rows`], then the trials sort. Shared by the view and
/// by `enter`, so the trial opened is always the one the cursor was on.
fn screen_trial_rows<'a>(
    loaded: &Loaded,
    filters: &'a Filters,
) -> (Vec<TrialRow>, Option<&'a Sort>) {
    let mut rows = trial_rows(&loaded.entries, &loaded.running_trials);
    let sort = shown_sort(filters.trial_sort.as_ref(), &TRIAL_COLUMNS);
    if let Some(sort) = sort {
        sort_rows(&mut rows, sort, |row| trial_sort_value(row, &sort.key));
    }
    (rows, sort)
}

/// The table on screen, built from the same functions the printing path
/// prints with — bar the trials table, which the printing path has no
/// counterpart of.
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
            let (rows, sort) = screen_lane_rows(loaded, filters, pipelines, &entries);
            let total = LaneTotal::of(&entries, &loaded.fallback, &loaded.models);
            table_lines(lanes_table(
                filters.by,
                &rows,
                &total,
                sort,
                filters.figures,
            ))
        }
        TableKind::Dirs => {
            let entries = scoped_dirs(loaded, filters);
            match filters.dir_by {
                DirBy::Dir => {
                    let (rows, sort) = screen_dir_rows(loaded, filters, &entries);
                    if rows.is_empty() {
                        return vec![Line::Text(
                            "Nothing outside the lanes in this window.".to_string(),
                        )];
                    }
                    table_lines(dirs_table(&rows, sort.as_ref(), filters.figures))
                }
                DirBy::Session => {
                    if entries.is_empty() {
                        return vec![Line::Text(
                            "No sessions outside the lanes in that window.".to_string(),
                        )];
                    }
                    let (rows, sort) = screen_sessions(loaded, filters, &entries);
                    table_lines(sessions_table(&rows, sort.as_ref()))
                }
            }
        }
        TableKind::Trials => {
            let (rows, sort) = screen_trial_rows(loaded, filters);
            if rows.is_empty() {
                return vec![Line::Text("No trials in this window.".to_string())];
            }
            trials_table(&rows, sort)
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
            let (rows, _) = screen_lane_rows(loaded, filters, pipelines, &entries);
            let total = LaneTotal::of(&entries, &loaded.fallback, &loaded.models);
            let mut lines: Vec<String> =
                rows.iter().map(|r| lanes_csv_row(filters.by, r)).collect();
            lines.push(lanes_csv_total(filters.by, &total));
            (lanes_csv_header(filters.by), lines, rows.len())
        }
        TableKind::Dirs => {
            let entries = scoped_dirs(loaded, filters);
            let (sessions, _) = screen_sessions(loaded, filters, &entries);
            match filters.dir_by {
                DirBy::Dir => {
                    let (rows, _) = screen_dir_rows(loaded, filters, &entries);
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
        TableKind::Trials => unreachable!("the trials table has no export: `e` is not read there"),
    }
}

/// The `by` the table on screen is grouped by — what its title, its export's
/// file name and the export confirmation all name it as. The trials table
/// has no `by`: it is always one row per trial, which is what this names.
fn by_label(filters: &Filters, table: TableKind) -> &'static str {
    match table {
        TableKind::Lanes => filters.by.label(),
        TableKind::Dirs => filters.dir_by.label(),
        TableKind::Trials => "trials",
    }
}

/// The left side of the top border: `by <by>` and which figures are drawn,
/// such as `by pipeline · totals`, or plain `trials` for the table that has
/// no `by` to name and no figures to switch.
fn frame_title(filters: &Filters, table: TableKind) -> String {
    match table {
        TableKind::Trials => "trials".to_string(),
        _ => format!(
            "by {} · {}",
            by_label(filters, table),
            filters.figures.label()
        ),
    }
}

/// `.spoolway/evals/`, so the file `e` writes lives beside the ledger it was
/// exported from. spoolway writes no `.gitignore` rules of its own any more —
/// see [`crate::gitignore`] — so keeping an export out of git is the
/// project's own line to add, like every other directory a person's own run
/// fills in rather than the project's tracked setup.
fn evals_dir(repo: &Repo) -> std::path::PathBuf {
    crate::config::setup_dir_in(&repo.root).join("evals")
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
/// own `PANE_CHROME_ROWS` gives Pulled out of `frame_rows` so the count itself is a
/// pure function a test can pin without a real terminal behind it.
fn frame_chrome() -> usize {
    4
}

/// How many body rows a terminal `height` rows tall gives the frame once
/// `frame_chrome` is counted — and, inside bare `spoolway`'s eval tab, once
/// the strip's own rows are too (see `crate::screen::shell::strip_rows`,
/// zero everywhere else). `None` where there is no terminal to measure,
/// which is what lets a piped run keep every row rather than losing the ones
/// past some guessed height.
fn frame_rows(height: Option<usize>) -> Option<usize> {
    height.map(|h| {
        h.saturating_sub(frame_chrome() + crate::screen::shell::strip_rows())
            .max(1)
    })
}

fn frame_top(title: &str, right: &str, width: usize) -> String {
    let left = format!("─ eval · {title} ");
    let right = match right.is_empty() {
        true => "─".to_string(),
        false => format!(" {right} ─"),
    };
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
/// `scope` is the screen's own [`Scope::label`], not [`Loaded`]'s: the
/// frame is drawn before the first load has landed. `loaded` is only read
/// to name a `trial` by its group rather than its id.
fn filters_label(
    scope: &str,
    filters: &Filters,
    table: TableKind,
    loaded: Option<&Loaded>,
) -> String {
    let mut parts = Vec::new();
    if !scope.is_empty() {
        parts.push(scope.to_string());
    }
    // Only the group, without the day the filter row adds: the parts here
    // are already joined by ` · `, and a date after it would read as a
    // filter of its own.
    let trial = filters
        .trial
        .as_deref()
        .map(|id| find_trial(loaded, id).map_or_else(|| id.to_string(), |row| row.group));
    let named: Vec<(&str, &Option<String>)> = match table {
        TableKind::Lanes => vec![
            ("group", &filters.group),
            ("task", &filters.task),
            ("pipeline", &filters.pipeline),
            ("step", &filters.step),
            ("version", &filters.version),
            ("trial", &trial),
        ],
        TableKind::Dirs => vec![("dir", &filters.dir), ("skill", &filters.skill)],
        TableKind::Trials => Vec::new(),
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
/// content, as the mockup draws it. The trials table trades `[e] export`
/// for `[enter] open`: it has nothing to export, and a trial to open. Nor
/// has it a `[t]`, which names the figures the other two would switch to.
fn footer(table: TableKind, figures: Figures) -> String {
    let tab = ("tab", table.next().label());
    let flip = ("t", figures.other().label());
    let keys: &[(&str, &str)] = match table {
        TableKind::Trials => &[
            ("↑↓", "move"),
            ("enter", "open"),
            ("a", "ascending"),
            ("d", "descending"),
            tab,
            ("f", "filters"),
            ("r", "refresh"),
            ("q", "quit"),
        ],
        TableKind::Lanes | TableKind::Dirs => &[
            ("↑↓", "move"),
            ("a", "ascending"),
            ("d", "descending"),
            flip,
            tab,
            ("f", "filters"),
            ("e", "export"),
            ("r", "refresh"),
            ("q", "quit"),
        ],
    };
    format!(" {}", key_hint(keys))
}

/// One frame, painted through `writer` — see [`eval_frame_rows`] for the
/// rows themselves, split out so a test can play the very same rows through
/// today's frozen write and through [`crate::screen::frame_writer`] and
/// require the same picture, without duplicating everything this builds.
fn draw(
    pipelines: &Pipelines,
    loaded: Option<&Loaded>,
    state: &ScreenState,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    out: &mut impl std::io::Write,
) {
    let rows = eval_frame_rows(pipelines, loaded, state);
    writer.write_frame(&rows, crate::screen::pane_size(), out);
}

/// The eval tab's own frame, as the rows [`draw`] paints — `loaded` is
/// `None` only on a visit whose first load has not landed yet, when the
/// frame is drawn empty under the loading popup.
fn eval_frame_rows(
    pipelines: &Pipelines,
    loaded: Option<&Loaded>,
    state: &ScreenState,
) -> Vec<String> {
    let height = terminal_size::terminal_size().map(|(_, h)| h.0 as usize);
    eval_frame_rows_at(pipelines, loaded, state, height)
}

/// [`eval_frame_rows`] on a terminal `height` rows tall — `None` for none at
/// all. Split out because `terminal_size` reads `None` under the test
/// harness, and a test of what scrolls needs a terminal shorter than its
/// table.
fn eval_frame_rows_at(
    pipelines: &Pipelines,
    loaded: Option<&Loaded>,
    state: &ScreenState,
    height: Option<usize>,
) -> Vec<String> {
    // Bare `spoolway`'s tab strip, when it hosts this screen as its eval tab,
    // and nothing at all otherwise — see `crate::screen::shell::strip`.
    let mut rows_out: Vec<String> = crate::screen::shell::strip();

    let lines = loaded.map_or_else(Vec::new, |loaded| {
        view_lines(loaded, &state.filters, pipelines, state.table)
    });
    let right = filters_label(&state.scope_label, &state.filters, state.table, loaded);
    let title = frame_title(&state.filters, state.table);
    // Wide enough for the widest line plus its marker column, and for the
    // top border's own title and label with a dash or two between them.
    let content = lines
        .iter()
        .map(|l| match l {
            Line::Text(t) | Line::Total(t) | Line::Head(_, t) => t.chars().count(),
            Line::Row(r) => r.text.chars().count(),
        })
        .max()
        .unwrap_or(0)
        + 1;
    let border = format!("─ eval · {title} ").chars().count() + right.chars().count() + 5;
    let width = frame_width(content.max(border));
    let rows = frame_rows(height);
    let mut body = render_lines(&lines, state.cursor, width);
    let cursor_line = cursor_line_index(&lines, state.cursor);
    // The `Total` line comes off before `clip` and goes back after it, so a
    // table taller than the frame scrolls its rows under a total that stays
    // put — the last body line, under the scroll indicator. The column header
    // is pinned the same way at the top, but only on a table that pins its
    // total: the rows scroll between the two. Each pinned line takes one of
    // `clip`'s rows; a table that fits loses nothing and keeps its total
    // right under its last row. `clip` needs two rows to draw the cursor's
    // row beside its indicator, so a line is pinned only while that many
    // are left — the total from three body rows, the header from four. On a
    // shorter frame they scroll with the rows, as they did before pinning,
    // rather than squeezing the cursor's row out of view. With no height
    // measured `clip` keeps every line, so both are pinned.
    let room = rows.unwrap_or(usize::MAX);
    let total = (room >= 3 && matches!(lines.last(), Some(Line::Total(_))))
        .then(|| body.pop())
        .flatten();
    let head = (total.is_some() && room >= 4 && matches!(lines.first(), Some(Line::Head(..))))
        .then(|| body.remove(0));
    let pinned = usize::from(total.is_some()) + usize::from(head.is_some());
    let scrolled = rows.map(|r| r - pinned);
    let cursor_line = cursor_line.saturating_sub(usize::from(head.is_some()));
    let mut body: Vec<String> = head
        .into_iter()
        .chain(clip(&body, cursor_line, scrolled, width))
        .collect();
    body.extend(total);

    // A notice or the filter panel sits over the table as an `overlay` —
    // computed here, before the frame, because its own height is now part
    // of deciding the frame's: `clip` above cuts a body *taller* than the
    // terminal down to size, but a body shorter than whatever panel is
    // about to be drawn on it needs padding the other way, or `overlay`
    // writes past the frame's own last row and the panel loses its bottom
    // border. Reproduced on a small ledger, where the table itself is only a
    // few lines tall and the filter panel is thirteen.
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
        Mode::Loading { .. } => Some(boxed("eval", &["Loading…".to_string()])),
        Mode::Filter(draft) => loaded.map(|loaded| filter_panel(loaded, draft)),
        Mode::Calendar {
            field,
            year,
            month,
            day,
            ..
        } => Some(calendar_panel(*field, *year, *month, *day)),
        Mode::Sort {
            descending,
            columns,
            cursor,
        } => Some(sort_panel(*descending, columns, *cursor)),
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

    rows_out.extend(frame);
    rows_out.push(footer(state.table, state.filters.figures));
    rows_out
}

// -------------------------------------------------------------------- modes

/// One row of the filter panel — `↑`/`↓` moves between these, `←`/`→`
/// changes the value on every row but the two dates, and `enter` on those
/// two opens a calendar rather than applying. `scope` is not a row here at
/// all: the screen only ever opens bare, so it can only ever hold the
/// default it opened with, and a row with a single possible answer is not a
/// question. Nor are `group` and `task`: a project can hold tens of
/// thousands of either, far too many to step through one `→` at a time, so
/// only `--group` and `--task` set them. `trial` is a row despite the same
/// worry, because it only cycles over the trials inside the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterField {
    By,
    Pipeline,
    Step,
    Version,
    Trial,
    Dir,
    Skill,
    Since,
    Until,
}

/// The rows the filter panel draws for `table`, in order: `by` first, since
/// it decides what every row below it is a filter *of*, and the window last
/// on every table, since it bounds them all the same way. The trials table
/// has the window alone: it has no `by`, and a trial is narrowed by opening
/// it, not by a row here.
fn filter_fields(table: TableKind) -> &'static [FilterField] {
    match table {
        TableKind::Lanes => &[
            FilterField::By,
            FilterField::Pipeline,
            FilterField::Step,
            FilterField::Version,
            FilterField::Trial,
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
        TableKind::Trials => &[FilterField::Since, FilterField::Until],
    }
}

/// A copy of [`Filters`] a person is editing in the filter panel, plus which
/// row the cursor is on and which table's rows it draws — fixed at the
/// moment `f` opened the panel, since nothing in [`Mode::Filter`] lets `tab`
/// change the table while it is up. `esc` drops this untouched; `enter`
/// turns it back into the real `Filters` and reloads — see `run_screen_with`'s
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
    /// A [`load`] running on its own thread, under a keyless `eval` popup
    /// reading `Loading…` — over an empty frame on a visit's first load, and
    /// over the table already shown on `r` or the filter panel's `enter`.
    /// Only `←`, `→` and `q` are read: every other key would act on a table
    /// that is about to be replaced. `filters` are the ones loaded through,
    /// the screen's own once the result lands; `reset_cursor` is set by the
    /// filter panel, whose new filters can leave the old row far past the
    /// end.
    ///
    /// The screen owns the only receiver, so a load replaced by a newer one,
    /// or one still running when the person leaves the tab, sends into a
    /// dropped channel: its thread finishes on its own and its result is
    /// never read.
    Loading {
        pending: Pending,
        filters: Filters,
        reset_cursor: bool,
    },
    /// The sort popup `a` (ascending) or `d` (descending) opens: `default
    /// order`, then every column the table on screen draws — `columns`, as
    /// they were when it opened, since nothing in here changes the view.
    /// `cursor` is `0` on `default order`, and `n` on `columns[n - 1]`.
    Sort {
        descending: bool,
        columns: Vec<(&'static str, &'static str)>,
        cursor: usize,
    },
}

/// The result of a [`load`] running on its own thread — see
/// [`load_in_background`].
type Pending = std::sync::mpsc::Receiver<Result<Loaded>>;

/// Run [`load`] on a thread of its own and hand back where its result will
/// land. `load` sweeps the ledger and reads every session's transcript,
/// which on a large transcript tree takes long enough that a screen waiting
/// on it would leave the previous tab's frame up — see [`Mode::Loading`].
fn load_in_background(repo: &Repo, filters: &Filters) -> Pending {
    let (tx, rx) = std::sync::mpsc::channel();
    let failed = tx.clone();
    let repo = repo.clone();
    let filters = filters.clone();
    // The fallible builder, not `thread::spawn`, which panics when the OS
    // cannot start a thread: that reason reaches the person as a failed
    // load instead of tearing the whole shell down.
    if let Err(err) = std::thread::Builder::new().spawn(move || {
        // A send into a dropped receiver is the screen having moved on — a
        // newer load, or the person leaving the tab — and nothing to report.
        let _ = tx.send(load(&repo, &filters));
    }) {
        let _ = failed.send(Err(
            anyhow::Error::new(err).context("could not start the load")
        ));
    }
    rx
}

/// The hint under an error notice.
const NOTICE_KEYS: &str = "[any key] continue";

struct ScreenState {
    table: TableKind,
    cursor: usize,
    filters: Filters,
    /// The top border's scope, read once up front: the frame is drawn before
    /// any load has landed, and nothing on the screen changes the scope.
    scope_label: String,
    mode: Mode,
}

impl ScreenState {
    fn new(args: &EvalArgs) -> ScreenState {
        let filters = Filters::from_args(args);
        ScreenState {
            table: TableKind::Lanes,
            cursor: 0,
            scope_label: filters.scope.label(),
            filters,
            mode: Mode::Browsing,
        }
    }

    /// Start loading through `filters`, under the loading popup.
    fn start_loading(
        &mut self,
        start: &mut impl FnMut(&Filters) -> Pending,
        filters: Filters,
        reset_cursor: bool,
    ) {
        self.mode = Mode::Loading {
            pending: start(&filters),
            filters,
            reset_cursor,
        };
    }

    /// Pick up the load in flight, if it has landed. `true` when it has, and
    /// the screen needs drawing again. A failure with a table already on
    /// screen becomes the same notice every other failure here is; one on
    /// the visit's first load has no table to sit over, and lands in
    /// `failed` for [`run_screen_with`] to hold on the tab.
    fn settle(&mut self, loaded: &mut Option<Loaded>, failed: &mut Option<String>) -> bool {
        let Mode::Loading { pending, .. } = &self.mode else {
            return false;
        };
        let result = match pending.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            // The thread ended without sending: `load` panicked.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(anyhow::anyhow!("the load stopped before it finished"))
            }
        };
        let Mode::Loading {
            filters,
            reset_cursor,
            ..
        } = std::mem::replace(&mut self.mode, Mode::Browsing)
        else {
            unreachable!("matched as loading above");
        };
        match result {
            Ok(fresh) => {
                *loaded = Some(fresh);
                self.filters = filters;
                if reset_cursor {
                    self.cursor = 0;
                }
            }
            Err(err) if loaded.is_none() => *failed = Some(format!("spoolway eval: {err:#}")),
            Err(err) => {
                self.mode = Mode::Notice {
                    title: "eval",
                    body: format!("{err:#}"),
                    keys: Some(NOTICE_KEYS),
                }
            }
        }
        true
    }
}

/// Bare `spoolway`'s eval tab: the eval screen, under the strip and over the
/// terminal the shell around it already holds — so no guard of its own.
/// Nothing below the strip differs. `spoolway eval` itself never opens
/// this any more — it prints, same as every other flag — so this tab is
/// the one place left that reaches it at all.
pub(crate) fn tab(
    repo: &Repo,
    pipelines: &Pipelines,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<crate::screen::shell::Leave> {
    run_screen(repo, pipelines, &bare_args(), writer, input, out)
}

/// Every flag at its bare default — what bare `spoolway`'s eval tab opens
/// on before a person touches anything, the same defaults `spoolway eval`
/// with no flags used to route to the screen for, before this only ever
/// printed.
fn bare_args() -> EvalArgs {
    EvalArgs {
        by: EvalBy::Pipeline,
        group: None,
        task: None,
        pipeline: None,
        step: None,
        pipeline_version: None,
        since: None,
        until: None,
        all: false,
        project: None,
        trial: None,
        discard: None,
        force: false,
        csv: false,
        per_run: false,
        sort: None,
    }
}

/// The screen's own loop, loading on a thread of its own — see
/// [`run_screen_with`].
fn run_screen(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &EvalArgs,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<crate::screen::shell::Leave> {
    run_screen_with(
        repo,
        pipelines,
        args,
        |filters| load_in_background(repo, filters),
        writer,
        input,
        out,
    )
}

/// The screen's own loop. Ends in [`Leave::Quit`] on `q` or when the input
/// runs out, and in whatever [`crate::screen::shell::leave_on`] reads off
/// `←` or `→` while browsing or loading when bare `spoolway` hosts this as
/// its eval tab.
///
/// Every load goes through `start` and lands through [`Mode::Loading`], the
/// first one included, so the frame is drawn on the keypress that opened
/// the tab rather than once the ledger has been read. Apart from
/// [`run_screen`] so a test can hand in a load that answers at once, or one
/// it answers itself: a scripted input always has a key pending, and would
/// otherwise race the thread.
///
/// [`Leave::Quit`]: crate::screen::shell::Leave::Quit
fn run_screen_with(
    repo: &Repo,
    pipelines: &Pipelines,
    args: &EvalArgs,
    mut start: impl FnMut(&Filters) -> Pending,
    writer: &mut crate::screen::frame_writer::FrameWriter,
    input: &mut impl PollableRead,
    out: &mut impl std::io::Write,
) -> Result<crate::screen::shell::Leave> {
    use crate::screen::shell::Leave;

    let mut state = ScreenState::new(args);
    let mut loaded: Option<Loaded> = None;
    // The reason the visit's first load failed, once it has. Hosted, the tab
    // still has a strip and three neighbours to reach, so the reason is held
    // on it rather than printed on the way out.
    let mut failed: Option<String> = None;
    state.start_loading(&mut start, state.filters.clone(), false);

    loop {
        state.settle(&mut loaded, &mut failed);
        match &failed {
            Some(message) => crate::screen::shell::message_frame(message, writer, out),
            None => draw(pipelines, loaded.as_ref(), &state, writer, out),
        }
        // Not a bare `read_key`: inside bare `spoolway` a `ctrl-c` has to end
        // this wait too, and a blocking read never sees one — see
        // `crate::screen::shell::wait_key`. `spoolway eval` on its own installs
        // no handler, so there nothing is ever caught. The idle slices are
        // where a load running behind the popup is noticed landing.
        let Some(key) = crate::screen::shell::wait_key(input, || {
            if state.settle(&mut loaded, &mut failed) {
                match &failed {
                    Some(message) => crate::screen::shell::message_frame(message, writer, out),
                    None => draw(pipelines, loaded.as_ref(), &state, writer, out),
                }
            }
        }) else {
            break;
        };
        if let Some(message) = failed {
            return Ok(match crate::screen::shell::leave_on(key) {
                Some(leave) => leave,
                None if crate::screen::shell::hosted().is_some() => {
                    crate::screen::shell::message_tab(&message, input, out)
                }
                // Standalone there is no neighbour to reach: the reason was
                // read on the frame this key answered, and the screen ends.
                None => Leave::Quit,
            });
        }
        // Browsing and loading are the modes with no filter panel, calendar
        // or notice open: the only ones where `←` and `→` are a hosting
        // shell's.
        if matches!(state.mode, Mode::Browsing | Mode::Loading { .. })
            && let Some(leave) = crate::screen::shell::leave_on(key)
        {
            return Ok(leave);
        }
        if key == Key::Char('q') {
            break;
        }
        // Past here every mode reads the table's data. Only loading can be
        // without it, and loading reads no key but the ones above.
        if matches!(state.mode, Mode::Loading { .. }) {
            continue;
        }
        let Some(loaded) = loaded.as_ref() else {
            continue;
        };

        match &mut state.mode {
            Mode::Loading { .. } => unreachable!("loading reads no key past `q`"),
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
                    handle_filter_change(loaded, draft, key == Key::Right);
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
                    state.start_loading(&mut start, candidate, true);
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
            Mode::Sort {
                descending,
                columns,
                cursor,
            } => match key {
                Key::Esc => state.mode = Mode::Browsing,
                Key::Up | Key::Char('k') => *cursor = cursor.saturating_sub(1),
                Key::Down | Key::Char('j') => *cursor = (*cursor + 1).min(columns.len()),
                Key::Enter => {
                    let sort = cursor.checked_sub(1).map(|i| Sort {
                        key: columns[i].1.to_string(),
                        descending: *descending,
                    });
                    match state.table {
                        TableKind::Lanes => state.filters.lane_sort = sort,
                        TableKind::Dirs => state.filters.dir_sort = sort,
                        TableKind::Trials => state.filters.trial_sort = sort,
                    }
                    // The row the cursor was on has moved somewhere else in
                    // the new order; the top is the one place still worth
                    // reading first.
                    state.cursor = 0;
                    state.mode = Mode::Browsing;
                }
                _ => {}
            },
            Mode::Browsing => match key {
                Key::Up | Key::Char('k') => state.cursor = state.cursor.saturating_sub(1),
                Key::Down | Key::Char('j') => state.cursor += 1,
                Key::Tab => {
                    state.table = state.table.next();
                    state.cursor = 0;
                }
                // Opening a trial is only a filter set for the person, so
                // `tab` and the `trial` row's `all` are what leave it. The
                // `pipeline` and `version` rows are cleared with it: either
                // could hide one of the trial's own pipelines, and showing
                // every one of those side by side is the point of opening it.
                Key::Enter if state.table == TableKind::Trials => {
                    let (rows, _) = screen_trial_rows(loaded, &state.filters);
                    if let Some(row) = rows.get(state.cursor.min(rows.len().saturating_sub(1))) {
                        state.filters.trial = Some(row.id.clone());
                        state.filters.by = EvalBy::Pipeline;
                        state.filters.pipeline = None;
                        state.filters.version = None;
                        state.table = TableKind::Lanes;
                        state.cursor = 0;
                    }
                }
                Key::Char('f') => {
                    state.mode = Mode::Filter(Draft::new(state.table, state.filters.clone()));
                }
                Key::Char(c @ ('a' | 'd')) => {
                    let columns = view_columns(loaded, &state.filters, pipelines, state.table);
                    let current = match state.table {
                        TableKind::Lanes => state.filters.lane_sort.clone(),
                        TableKind::Dirs => state.filters.shown_dir_sort(),
                        TableKind::Trials => state.filters.trial_sort.clone(),
                    };
                    // Opens on the column already sorted, so flipping its
                    // direction is `d` and `enter`; on `default order` when
                    // there is none on this view.
                    let cursor = current
                        .and_then(|s| columns.iter().position(|(_, key)| *key == s.key))
                        .map_or(0, |i| i + 1);
                    state.mode = Mode::Sort {
                        descending: c == 'd',
                        columns,
                        cursor,
                    };
                }
                Key::Char('r') => {
                    state.start_loading(&mut start, state.filters.clone(), false);
                }
                Key::Char('t') if state.table != TableKind::Trials => {
                    state.filters.flip_figures();
                }
                Key::Char('e') if state.table != TableKind::Trials => {
                    match export(repo, loaded, &state.filters, pipelines, state.table) {
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
    Ok(Leave::Quit)
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
            // Never reached: the trials panel draws no `by` row.
            TableKind::Trials => {}
        },
        FilterField::Pipeline => {
            f.pipeline = cycle_option(&pipeline_candidates(loaded), &f.pipeline, forward);
        }
        FilterField::Step => f.step = cycle_option(&step_candidates(loaded, f), &f.step, forward),
        FilterField::Version => {
            f.version = cycle_option(&version_candidates(loaded, f), &f.version, forward);
        }
        FilterField::Trial => {
            f.trial = cycle_option(&trial_candidates(loaded), &f.trial, forward);
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
/// longer on offer — a step or a version the newly chosen pipeline never
/// ran. Cleared rather than kept, because `enter` would apply it as a filter
/// that silently matches nothing. One pass settles it: both lists narrow on
/// the pipeline alone, which neither clearing touches.
fn drop_stranded_values(loaded: &Loaded, f: &mut Filters) {
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
}

fn filter_field_label(field: FilterField) -> &'static str {
    match field {
        FilterField::By => "by",
        FilterField::Pipeline => "pipeline",
        FilterField::Step => "step",
        FilterField::Version => "version",
        FilterField::Trial => "trial",
        FilterField::Dir => "dir",
        FilterField::Skill => "skill",
        FilterField::Since => "since",
        FilterField::Until => "until",
    }
}

/// What a row of the filter panel shows for its own value: `‹ value ›` on
/// every row `←`/`→` cycles, and the plain date — or what a blank one
/// means — on the two a calendar sets, whose missing chevrons are the
/// visible sign that `←`/`→` do nothing there. The same holds within a
/// cycled row: each chevron is drawn only where its key would still change
/// the value — asked of the very cycling functions the keys call, so the
/// two can never disagree — and a space stands in for one that is not, so
/// the value keeps its column whichever end it sits at.
fn filter_field_value(loaded: &Loaded, draft: &Draft, field: FilterField) -> String {
    let f = &draft.filters;
    let cycled = |candidates: Vec<String>, v: &Option<String>| {
        chevrons(
            v.as_deref().unwrap_or("all"),
            cycle_option(&candidates, v, false) != *v,
            cycle_option(&candidates, v, true) != *v,
        )
    };
    match field {
        FilterField::By => chevrons(
            by_label(f, draft.table),
            by_moves(f, draft.table, false),
            by_moves(f, draft.table, true),
        ),
        FilterField::Pipeline => cycled(pipeline_candidates(loaded), &f.pipeline),
        FilterField::Step => cycled(step_candidates(loaded, f), &f.step),
        FilterField::Version => cycled(version_candidates(loaded, f), &f.version),
        // Cycled over ids like every other row, but drawn as the group and
        // day a person knows the trial by.
        FilterField::Trial => {
            let candidates = trial_candidates(loaded);
            let shown = f.trial.as_deref().map_or_else(
                || "all".to_string(),
                |id| find_trial(Some(loaded), id).map_or_else(|| id.to_string(), |row| row.name()),
            );
            chevrons(
                &shown,
                cycle_option(&candidates, &f.trial, false) != f.trial,
                cycle_option(&candidates, &f.trial, true) != f.trial,
            )
        }
        FilterField::Dir => cycled(dir_candidates(loaded), &f.dir),
        FilterField::Skill => cycled(skill_candidates(loaded), &f.skill),
        FilterField::Since => date_field_value(&f.since, "(blank — the start)"),
        FilterField::Until => date_field_value(&f.until, "(blank — now)"),
    }
}

/// Whether `←` (`forward` false) or `→` on the `by` row would move it off
/// the `by` it is on.
fn by_moves(f: &Filters, table: TableKind, forward: bool) -> bool {
    match table {
        TableKind::Lanes => cycle_by(&EvalBy::ALL, f.by, forward) != f.by,
        TableKind::Dirs => cycle_by(&DirBy::ALL, f.dir_by, forward) != f.dir_by,
        TableKind::Trials => false,
    }
}

/// `value` between whichever of `‹` and `›` its keys can still act on, a
/// space in place of each one they cannot.
fn chevrons(value: &str, back: bool, forward: bool) -> String {
    let back = if back { '‹' } else { ' ' };
    let forward = if forward { '›' } else { ' ' };
    format!("{back} {value} {forward}")
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
fn filter_panel(loaded: &Loaded, draft: &Draft) -> Vec<String> {
    let mut body = Vec::with_capacity(draft.fields().len() + 4);
    for (i, field) in draft.fields().iter().enumerate() {
        let marker = if i == draft.field { ">" } else { " " };
        let label = filter_field_label(*field);
        let value = filter_field_value(loaded, draft, *field);
        body.push(format!("{marker} {label:<9} {value}"));
    }
    body.push(String::new());
    body.extend(FILTER_HINTS.iter().map(|h| h.to_string()));
    boxed("filters", &body)
}

/// Every column the table on screen draws, left to right, each beside the
/// export column its sort reads — what the sort popup lists.
fn view_columns(
    loaded: &Loaded,
    filters: &Filters,
    pipelines: &Pipelines,
    table: TableKind,
) -> Vec<(&'static str, &'static str)> {
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
            drawn_lane_columns(filters.by, &rows, filters.figures)
        }
        TableKind::Dirs => dir_columns(filters.dir_by, filters.figures).to_vec(),
        TableKind::Trials => TRIAL_COLUMNS.to_vec(),
    }
}

/// The sort popup — see [`Mode::Sort`] — drawn the way [`filter_panel`]
/// draws its rows: the cursor marked on the one it is on, the keys under
/// them one blank row apart.
fn sort_panel(descending: bool, columns: &[(&str, &str)], cursor: usize) -> Vec<String> {
    let title = match descending {
        true => "sort descending",
        false => "sort ascending",
    };
    let names = std::iter::once("default order").chain(columns.iter().map(|(name, _)| *name));
    let body: Vec<String> = names
        .enumerate()
        .map(|(i, name)| {
            let marker = if i == cursor { ">" } else { " " };
            format!("{marker} {name}")
        })
        .collect();
    panel(
        title,
        &body,
        &keys(&[("↑↓", "column"), ("enter", "sort"), ("esc", "back")]),
    )
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
            tier_tokens: Default::default(),
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
            blocked: false,
            run: Some(format!("r-{task}")),
            trial: None,
            trial_group: None,
            dir: None,
            hand: false,
            project: "demo".into(),
        }
    }

    fn no_models() -> BTreeMap<String, ModelPrice> {
        BTreeMap::new()
    }

    /// `sized` at `window`, with rates: $3 in, $15 out, $0.30 a cache read
    /// and $3.75 a five-minute cache write, each per million tokens — so
    /// the class-cost columns have something to draw.
    fn priced_models(window: usize) -> BTreeMap<String, ModelPrice> {
        let mut models = sized_models(window);
        models.insert(
            "sized".to_string(),
            ModelPrice {
                context_window: window,
                input: 3.0,
                output: 15.0,
                cache_read: 0.30,
                cache_write_5m: 3.75,
                ..ModelPrice::default()
            },
        );
        models
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
        total_of_with(entries, &no_models())
    }

    fn total_of_with(entries: &[Entry], models: &BTreeMap<String, ModelPrice>) -> LaneTotal {
        let refs: Vec<&Entry> = entries.iter().collect();
        LaneTotal::of(&refs, &fallback_keys(entries), models)
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

    /// A `pass` or `fail` that a spent `loop:` sent to `blocked` is a block
    /// the lane never reported. It counts in `BLOCKS` once, and a pass still
    /// counts as a pass.
    #[test]
    fn a_lane_a_spent_loop_blocked_counts_as_a_block_and_keeps_its_outcome() {
        let mut looped = lane("login", "review", 1, Some("fail"));
        looped.blocked = true;
        let mut passed = lane("logout", "implement", 1, Some("pass"));
        passed.blocked = true;
        let mut reported = lane("search", "implement", 1, Some("block"));
        reported.blocked = true;
        let plain = lane("tabs", "implement", 1, Some("pass"));
        let entries = [looped, passed, reported, plain];
        let m = one_row(&entries);
        assert_eq!(m.blocked, 3, "each lane once, the `--block` one not twice");
        assert_eq!(m.judged, 4);
        assert_eq!(m.passed, 2, "a pass sent to `blocked` is still a pass");
        assert_eq!(total_of(&entries).blocked, 3);
    }

    /// A lane's last verdict decides, flag and all: a lane reported again as
    /// a clean pass is not a block, and a later line with no verdict of its
    /// own changes nothing.
    #[test]
    fn the_blocked_flag_follows_the_lanes_last_verdict() {
        let mut first = lane("login", "review", 1, Some("pass"));
        first.blocked = true;
        let again = lane("login", "review", 1, Some("pass"));
        assert_eq!(one_row(&[first.clone(), again]).blocked, 0);
        let mut freed = lane("login", "review", 1, None);
        freed.wall_s = 5;
        assert_eq!(one_row(&[first, freed]).blocked, 1);
    }

    /// A ledger line from before the flag existed carries no `blocked` key.
    #[test]
    fn a_ledger_line_without_the_flag_reads_as_not_blocked() {
        let mut line = serde_json::to_value(lane("login", "implement", 1, Some("pass"))).unwrap();
        assert!(line.get("blocked").is_none(), "false is left off the line");
        line.as_object_mut().unwrap().remove("blocked");
        let back: Entry = serde_json::from_value(line).unwrap();
        assert!(!back.blocked);
        assert_eq!(one_row(&[back]).blocked, 0);
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
    /// `unpriced` column beside it saying how many were left out.
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
        e.trial = Some("t1".into());
        assert!(LaneFilters::default().admits(&e));
        let all = LaneFilters {
            group: Some("audits"),
            task: Some("login"),
            pipeline: Some("default"),
            step: Some("review"),
            version: Some("1.1"),
            trial: Some("t1"),
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
            LaneFilters {
                trial: Some("t2"),
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
            tokens: Tokens::default(),
            class_cost: ClassCost::default(),
            cost: 0.0,
            lines: 0,
            unpriced: 0,
            time_s: 0,
        };
        const FIGURES: &str = "RUNS  PASS  BLOCKS/RUN  CTX PEAK AVG  CTX PEAK  IN/RUN  IN USD/RUN  \
                               OUT/RUN  OUT USD/RUN  CACHE R/RUN  CACHE R USD/RUN  CACHE W/RUN  \
                               CACHE W USD/RUN       USD  USD/RUN  TIME/RUN";
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
            let table = lanes_table(by, &[named(cells)], &total, None, Figures::PerRun);
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
        let models = priced_models(100_000);
        let rows = rows_by_with(&entries, EvalBy::Pipeline, &models);
        let table = lanes_table(
            EvalBy::Pipeline,
            &rows,
            &total_of_with(&entries, &models),
            None,
            Figures::PerRun,
        );
        assert_eq!(
            table.rows[0],
            "impl          2  100%        0.00           32%       52%     642        0.00   \
             155.8k         2.34       41.91M            12.57       799.4k             3.00     \
             33.80    16.90    1h 12m"
        );
        // The average run: `BLOCKS/RUN` to two places, each token class,
        // its cost and `TIME` over the table's two runs, `USD` blank and
        // `USD/RUN` carrying the average.
        assert_eq!(
            table.total,
            "Average       2              0.00                             642        0.00   \
             155.8k         2.34       41.91M            12.57       799.4k             3.00              \
             16.90    1h 12m"
        );
    }

    /// The totals view draws the same row's sums under `IN` … `TIME`, with
    /// no `USD/RUN`, and its header is the one the screen opens on. The
    /// `Total` line carries every sum, each under its own column.
    #[test]
    fn a_row_draws_its_totals_under_the_totals_columns() {
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
        let models = priced_models(100_000);
        let rows = rows_by_with(&entries, EvalBy::Pipeline, &models);
        let total = total_of_with(&entries, &models);
        let table = lanes_table(EvalBy::Pipeline, &rows, &total, None, Figures::Totals);
        assert_eq!(
            table.header,
            "PIPELINE   RUNS  PASS  BLOCKS  CTX PEAK AVG  CTX PEAK       IN  IN USD      OUT  \
             OUT USD  CACHE R  CACHE R USD  CACHE W  CACHE W USD       USD      TIME"
        );
        // Each class cost is its tokens at today's rates — 311.6k out at
        // $15 a million is 4.67 — and need not add up to the banked `USD`.
        assert_eq!(
            table.rows[0],
            "impl          2  100%       0           32%       52%     1.3k    0.00   311.6k     \
             4.67   83.82M        25.15    1.60M         6.00     33.80    2h 24m"
        );
        assert_eq!(
            table.total,
            "Total         2             0                             1.3k    0.00   311.6k     \
             4.67   83.82M        25.15    1.60M         6.00     33.80    2h 24m"
        );
    }

    /// The cell `row` draws under `title` in `header`. Every class-cost
    /// column is exactly as wide as its title, so the title's own span is
    /// the cell's. Counted in characters: a `—` cell is three bytes.
    fn cell_under(header: &str, row: &str, title: &str) -> String {
        let start = header
            .find(title)
            .map(|byte| header[..byte].chars().count())
            .unwrap_or_else(|| panic!("no {title}: {header}"));
        let cell: String = row
            .chars()
            .skip(start)
            .take(title.chars().count())
            .collect();
        cell.trim().to_string()
    }

    /// `CACHE W USD` prices the five-minute and the one-hour writes each at
    /// its own rate and adds them, and a model with no one-hour rate prices
    /// both at the five-minute one — the rule banking keeps.
    #[test]
    fn cache_w_usd_prices_each_write_at_its_own_rate() {
        let mut e = lane("t", "implement", 1, Some("pass"));
        e.model = "sized".into();
        e.tokens = Tokens {
            cache_write_5m: 1_000_000,
            cache_write_1h: 500_000,
            ..Tokens::default()
        };
        let mut models = priced_models(100_000);
        let sized = models.get_mut("sized").unwrap();
        (sized.cache_write_5m, sized.cache_write_1h) = (4.0, 8.0);
        let cost = ClassCost::of([&e], &models);
        assert_eq!(cost.cache_write, 4.0 + 4.0);
        assert_eq!((cost.input, cost.output, cost.cache_read), (0.0, 0.0, 0.0));

        models.get_mut("sized").unwrap().cache_write_1h = 0.0;
        assert_eq!(ClassCost::of([&e], &models).cache_write, 4.0 + 2.0);
    }

    /// `tier_tokens` are priced at the tier's rates and the rest at the base
    /// rates; a line without the split, or a model with no tier, prices at
    /// the base rate as it always did.
    #[test]
    fn the_class_columns_price_tier_tokens_at_the_tiers_rates() {
        let mut models = priced_models(100_000);
        let sized = models.get_mut("sized").unwrap();
        sized.tier = Some(crate::usage::PriceTier {
            above_k: 100,
            rates: crate::usage::Rates {
                input: 6.0,
                output: 30.0,
                cache_read: 0.6,
                cache_write_5m: 7.5,
                cache_write_1h: 0.0,
            },
        });
        let mut e = lane("t", "implement", 1, Some("pass"));
        e.model = "sized".into();
        e.tokens = Tokens {
            input: 3_000_000,
            output: 2_000_000,
            cache_read: 4_000_000,
            cache_write_5m: 2_000_000,
            ..Tokens::default()
        };
        let untiered = ClassCost::of([&e], &models);
        assert_eq!(untiered.input, 9.0);

        e.tier_tokens = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 2_000_000,
            cache_write_5m: 1_000_000,
            ..Tokens::default()
        };
        let cost = ClassCost::of([&e], &models);
        assert!((cost.input - (2.0 * 3.0 + 6.0)).abs() < 1e-9);
        assert!((cost.output - (15.0 + 30.0)).abs() < 1e-9);
        assert!((cost.cache_read - (2.0 * 0.3 + 2.0 * 0.6)).abs() < 1e-9);
        assert!((cost.cache_write - (3.75 + 7.5)).abs() < 1e-9);

        // A split larger than the line's tokens prices no more than the line.
        let mut hand_edited = e.clone();
        hand_edited.tier_tokens.input = 9_000_000;
        let clamped = ClassCost::of([&hand_edited], &models);
        assert!(
            (clamped.input - 3.0 * 6.0).abs() < 1e-9,
            "{}",
            clamped.input
        );

        // A model with no tier today prices the whole line at the base rate.
        models.get_mut("sized").unwrap().tier = None;
        assert_eq!(ClassCost::of([&e], &models).input, untiered.input);
    }

    /// A row whose every lane has no price today draws its four class
    /// cells blank, never `0.00`; a priced row beside it draws its own, and
    /// the `Total` line sums what could be priced.
    #[test]
    fn an_unpriced_row_draws_its_class_costs_blank() {
        let mut unpriced = lane("t1", "implement", 1, Some("pass"));
        unpriced.pipeline = "local".into();
        unpriced.tokens.output = 1_000_000;
        unpriced.cost_usd = None;
        let mut priced = lane("t2", "implement", 1, Some("pass"));
        priced.model = "sized".into();
        priced.tokens.output = 1_000_000;
        // Spends nothing, so it must not count as a priced line on `local`.
        let idle = lane("t3", "implement", 1, Some("pass"));
        let entries = vec![
            unpriced,
            priced,
            Entry {
                pipeline: "local".into(),
                ..idle
            },
        ];
        let models = priced_models(100_000);
        let rows = rows_by_with(&entries, EvalBy::Pipeline, &models);
        let total = total_of_with(&entries, &models);
        let table = lanes_table(EvalBy::Pipeline, &rows, &total, None, Figures::Totals);
        let row = |name: &str| {
            table
                .rows
                .iter()
                .find(|r| r.starts_with(name))
                .unwrap_or_else(|| panic!("no {name} row: {:?}", table.rows))
        };
        for title in ["IN USD", "OUT USD", "CACHE R USD", "CACHE W USD"] {
            assert_eq!(
                cell_under(&table.header, row("local"), title),
                "",
                "{title}"
            );
        }
        assert_eq!(
            cell_under(&table.header, row("default"), "OUT USD"),
            "15.00"
        );
        assert_eq!(cell_under(&table.header, row("default"), "IN USD"), "0.00");
        assert_eq!(cell_under(&table.header, &table.total, "OUT USD"), "15.00");

        let per_run = lanes_table(EvalBy::Pipeline, &rows, &total, None, Figures::PerRun);
        let local = per_run
            .rows
            .iter()
            .find(|r| r.starts_with("local"))
            .unwrap();
        assert_eq!(cell_under(&per_run.header, local, "OUT USD/RUN"), "");
        // 15.00 over the table's three runs.
        assert_eq!(
            cell_under(&per_run.header, &per_run.total, "OUT USD/RUN"),
            "5.00"
        );
    }

    /// `--csv` and `--json` carry each class cost and its per-run twin —
    /// blank and `null` where nothing could be priced — and `--sort` on
    /// one puts an unpriced row last whichever way it runs.
    #[test]
    fn the_class_costs_export_and_sort() {
        let mut a = lane("t1", "implement", 1, Some("pass"));
        a.model = "sized".into();
        a.tokens = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 10_000_000,
            cache_write_5m: 1_000_000,
            ..Tokens::default()
        };
        let mut b = lane("t2", "implement", 1, Some("pass"));
        b.pipeline = "local".into();
        b.tokens.input = 5;
        let entries = vec![a, b];
        let models = priced_models(100_000);
        let mut rows = rows_by_with(&entries, EvalBy::Pipeline, &models);
        let total = total_of_with(&entries, &models);

        let header = lanes_csv_header(EvalBy::Pipeline);
        let names: Vec<&str> = header.split(',').collect();
        let at = |line: &str, name: &str| {
            let i = names.iter().position(|n| *n == name).unwrap();
            line.split(',').nth(i).unwrap().to_string()
        };
        let row_of = |name: &str| rows.iter().find(|r| r.keys[0] == name).unwrap();
        let line = lanes_csv_row(EvalBy::Pipeline, row_of("default"));
        let figures = [
            ("in_usd", "3.00"),
            ("out_usd", "15.00"),
            ("cache_read_usd", "3.00"),
            ("cache_write_usd", "3.75"),
            ("in_usd_per_run", "3.00"),
            ("out_usd_per_run", "15.00"),
            ("cache_read_usd_per_run", "3.00"),
            ("cache_write_usd_per_run", "3.75"),
        ];
        for (name, value) in figures {
            assert_eq!(at(&line, name), value, "{name}: {line}");
        }
        let line = lanes_csv_row(EvalBy::Pipeline, row_of("local"));
        for (name, _) in figures {
            assert_eq!(at(&line, name), "", "{name}: {line}");
        }
        // Over the table's two runs.
        let line = lanes_csv_total(EvalBy::Pipeline, &total);
        assert_eq!(at(&line, "out_usd"), "15.00", "{line}");
        assert_eq!(at(&line, "out_usd_per_run"), "7.50", "{line}");

        let json = lanes_json(EvalBy::Pipeline, &rows, &total);
        let json_row = |name: &str| {
            json["rows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["pipeline"] == name)
                .unwrap()
                .clone()
        };
        let priced = json_row("default");
        assert_eq!(priced["in_usd"], 3.0);
        assert_eq!(priced["cache_write_usd"], 3.75);
        assert_eq!(priced["out_usd_per_run"], 15.0);
        let unpriced = json_row("local");
        for (name, _) in figures {
            assert!(unpriced[name].is_null(), "{name}: {unpriced}");
        }
        assert_eq!(json["total"]["out_usd"], 15.0);
        assert_eq!(json["total"]["out_usd_per_run"], 7.5);

        for (name, _) in figures {
            for descending in [true, false] {
                let sort = parse_sort(EvalBy::Pipeline, name).unwrap();
                let sort = Sort { descending, ..sort };
                sort_lane_rows(EvalBy::Pipeline, &mut rows, &sort);
                assert_eq!(rows[1].keys[0], "local", "{name}, descending {descending}");
            }
        }
    }

    /// The directory table's totals view: the sums under `IN` … `TIME`, and
    /// no `USD/SESSION`.
    #[test]
    fn the_dirs_table_opens_on_its_totals() {
        let a = dir_line("/w/proj", "s1", "2026-09-01T09:00:00+00:00", 0.70);
        let b = dir_line("/w/proj", "s2", "2026-09-01T10:00:00+00:00", 0.18);
        let rows = dir_rows(&[&a, &b], &[], &no_models(), &HashMap::new());
        let table = dirs_table(&rows, None, Figures::Totals);
        assert_eq!(
            table.header,
            "DIR       SESSIONS       IN  IN USD      OUT  OUT USD  CACHE R  CACHE R USD  CACHE W  \
             CACHE W USD       USD  CTX PEAK AVG  CTX PEAK      TIME"
        );
        assert_eq!(
            table.rows[0].split_whitespace().collect::<Vec<_>>(),
            [
                "/w/proj", "2", "0", "0.00", "20", "0.00", "0", "0.00", "0", "0.00", "0.88", "—",
                "—", "0s"
            ]
        );
    }

    /// `t` carries a sort to the matching column of the other view, `USD`
    /// keeps its own, `USD/RUN` lands on `USD`, and a column drawn the same
    /// in both is left alone.
    #[test]
    fn a_sort_carries_to_the_matching_column_across_the_views() {
        let carried = |to: Figures, key: &str, pairs: &[(&str, &str)]| {
            let mut sort = Some(sorted(key, true));
            to.carry(&mut sort, pairs);
            sort.unwrap().key
        };
        let per_run = Figures::PerRun;
        let totals = Figures::Totals;
        assert_eq!(carried(per_run, "in_tokens", &LANE_PAIRS), "in_per_run");
        assert_eq!(carried(totals, "in_per_run", &LANE_PAIRS), "in_tokens");
        assert_eq!(carried(per_run, "time_s", &LANE_PAIRS), "time_per_run_s");
        assert_eq!(carried(totals, "time_per_run_s", &LANE_PAIRS), "time_s");
        assert_eq!(carried(per_run, "cost_usd", &LANE_PAIRS), "cost_usd");
        assert_eq!(carried(totals, "cost_per_run", &LANE_PAIRS), "cost_usd");
        assert_eq!(carried(per_run, "pass", &LANE_PAIRS), "pass");
        assert_eq!(
            carried(per_run, "cache_write_tokens", &DIR_PAIRS),
            "cache_write_per_session"
        );
        assert_eq!(carried(totals, "cost_per_session", &DIR_PAIRS), "cost_usd");

        let mut none = None;
        per_run.carry(&mut none, &LANE_PAIRS);
        assert_eq!(none, None, "the default order stays the default order");

        // Every pair names a column each view really draws.
        for (total, per) in LANE_PAIRS {
            assert!(
                LANE_TOTAL_COLUMNS.iter().any(|(_, k)| *k == total),
                "{total}"
            );
            assert!(LANE_FIGURE_COLUMNS.iter().any(|(_, k)| *k == per), "{per}");
        }
        for (total, per) in DIR_PAIRS {
            assert!(
                DIR_TOTAL_COLUMNS.iter().any(|(_, k)| *k == total),
                "{total}"
            );
            assert!(DIR_COLUMNS.iter().any(|(_, k)| *k == per), "{per}");
        }
    }

    /// `RUNS` on the `Total` line is distinct runs over the whole table: a
    /// run under `--by step` touches every step it ran, and summing the
    /// rows would count it once per step. `BLOCKS`, the tokens, `USD` and
    /// `TIME` do sum, and the average run divides each sum by those two
    /// distinct runs — not by the three rows.
    #[test]
    fn the_total_line_counts_distinct_runs_and_sums_the_rest() {
        let mut entries = vec![
            lane("login", "implement", 1, Some("block")),
            lane("login", "review", 1, Some("pass")),
            lane("logout", "implement", 1, Some("pass")),
        ];
        entries[0].tokens.output = 2_000;
        entries[2].tokens.output = 1_000;
        let rows = rows_by(&entries, EvalBy::Step);
        assert_eq!(rows.iter().map(|r| r.metrics.runs).sum::<usize>(), 3);
        let total = total_of(&entries);
        assert_eq!(total.runs, 2, "two runs, however many steps they touched");
        assert_eq!(total.blocked, 1);
        assert_eq!(total.cost, 3.0);
        assert_eq!(total.tokens.output, 3_000);
        assert_eq!(total.time_s, 180);

        let table = lanes_table(EvalBy::Step, &rows, &total, None, Figures::Totals);
        let cells: Vec<&str> = table.total.split_whitespace().collect();
        assert_eq!(
            cells,
            [
                "Total", "2", "1", "0", "3.0k", "0", "0", "3.00", "3m", "00s"
            ],
            "{:?}",
            table.total
        );

        let table = lanes_table(EvalBy::Step, &rows, &total, None, Figures::PerRun);
        let cells: Vec<&str> = table.total.split_whitespace().collect();
        assert_eq!(
            cells,
            [
                "Average", "2", "0.50", "0", "1.5k", "0", "0", "1.50", "1m", "30s"
            ],
            "{:?}",
            table.total
        );
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
            !lanes_table(EvalBy::Pipeline, &one, &total, None, Figures::PerRun)
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
        let table = lanes_table(EvalBy::Pipeline, &two, &total, None, Figures::PerRun);
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
    /// per-run token columns beside the raw totals, and each class's cost
    /// after them the same way. `qwen` has no price, so those stay blank.
    #[test]
    fn a_lanes_export_carries_totals_beside_per_run_figures_and_marks_its_total() {
        assert_eq!(
            lanes_csv_header(EvalBy::Step),
            "project,by,pipeline,step,pipeline_version,runs,lanes,pass,blocks,ctx_peak_tokens,\
             ctx_peak_pct,ctx_peak_avg_tokens,ctx_peak_avg_pct,in_tokens,out_tokens,\
             cache_read_tokens,cache_write_tokens,in_per_run,out_per_run,cache_read_per_run,\
             cache_write_per_run,in_usd,out_usd,cache_read_usd,cache_write_usd,in_usd_per_run,\
             out_usd_per_run,cache_read_usd_per_run,cache_write_usd_per_run,cost_usd,\
             cost_per_run,unpriced,time_s,time_per_run_s"
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
                "demo,step,default,implement,,2,2,0.50,1,,,,,300,0,0,0,150,0,0,0,,,,,,,,,2.00,1.00,0,120,\
                 60"
            ),
            "two versions on one row name neither: {line}"
        );

        let total = lanes_csv_total(EvalBy::Step, &total_of(&entries));
        assert_eq!(total.split(',').count(), header_cols, "{total}");
        // Every sum, and each per-run column over the table's two runs;
        // `unpriced` and the shares and peaks stay blank.
        assert_eq!(
            total,
            ",total,,,,2,,,1,,,,,300,0,0,0,150,0,0,0,,,,,,,,,2.00,1.00,,120,60"
        );
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
        // The keys the `Total` line always had keep their names and values;
        // the sums and the average run are added beside them.
        let total = &json["total"];
        assert_eq!(total["runs"], 2);
        assert_eq!(total["blocks"], 0);
        assert_eq!(total["cost_usd"], 1.0);
        assert_eq!(total["in_tokens"], 10);
        assert_eq!(total["in_per_run"], 5);
        assert_eq!(total["out_tokens"], 0);
        assert_eq!(total["cache_write_per_run"], 0);
        assert_eq!(total["cost_per_run"], 0.5);
        assert_eq!(total["time_s"], 120);
        assert_eq!(total["time_per_run_s"], 60.0);
    }

    // -------------------------------------------------------- directories

    /// A directory line, the way `usage::sweep`'s directory walk banks one:
    /// no `task`, `step`, `pipeline`, `agent`, `outcome`, `run` or
    /// `pipeline_version` — see `Entry::dir`.
    fn dir_line(dir: &str, session: &str, ts: &str, cost: f64) -> Entry {
        Entry {
            tier_tokens: Default::default(),
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
            blocked: false,
            run: None,
            trial: None,
            trial_group: None,
            dir: Some(dir.into()),
            hand: false,
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
        let table = dirs_table(&rows, None, Figures::PerRun);
        assert!(
            table.rows[1].split_whitespace().skip(2).all(|c| c == "—"),
            "nothing per session to divide: {}",
            table.rows[1]
        );
        let table = dirs_table(&rows, None, Figures::Totals);
        assert!(
            table.rows[1].split_whitespace().skip(2).all(|c| c == "—"),
            "nothing ran, so it reads as it does per session: {}",
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

    /// The mockup's own `by dir` header, and its last line in either view:
    /// the sums on `Total`, and on `Average` each sum over the table's
    /// sessions with `USD` left blank. Neither `CTX PEAK` adds up, so both
    /// stay blank on both.
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
        let table = dirs_table(&rows, None, Figures::PerRun);
        assert_eq!(
            table.header,
            "DIR                         SESSIONS  IN/SESSION  IN USD/SESSION  OUT/SESSION  \
             OUT USD/SESSION  CACHE R/SESSION  CACHE R USD/SESSION  CACHE W/SESSION  \
             CACHE W USD/SESSION       USD  USD/SESSION  CTX PEAK AVG  CTX PEAK  TIME/SESSION"
        );
        assert_eq!(
            table.total,
            "Average                            2           0            0.00           10             \
             0.00                0                 0.00                0                 0.00                   \
             0.44                                    0s"
        );
        let table = dirs_table(&rows, None, Figures::Totals);
        assert_eq!(
            table.total,
            "Total                              2        0    0.00       20     0.00        0         \
             0.00        0         0.00      0.88                                0s"
        );
    }

    /// The mockup's own `by session` header, each token class followed by
    /// its cost, and a `Total` line carrying every token class and its cost,
    /// USD and time.
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
        // Priced in `[models]`, which wins over every price file, so the
        // class costs below do not move with whatever table this machine
        // last refreshed: $3 in, $15 out, $0.40 a cache read and $3.75 a
        // five-minute write, per million. The cache-read rate keeps 9.45M
        // off a half cent, where `{:.2}` would round on binary noise.
        let mut models = no_models();
        models.insert(
            "claude-opus-5".to_string(),
            ModelPrice {
                input: 3.0,
                output: 15.0,
                cache_read: 0.40,
                cache_write_5m: 3.75,
                ..ModelPrice::default()
            },
        );
        let mut rows = list_sessions(&[&a, &b], &models, &HashMap::new(), &skills);
        rows.reverse();
        let table = sessions_table(&rows, None);
        assert_eq!(
            table.header,
            "WHEN           DIR       SKILL              MODEL                 IN  IN USD      OUT  \
             OUT USD   CACHE R  CACHE R USD   CACHE W  CACHE W USD    USD      TIME"
        );
        let first: Vec<&str> = table.rows[0].split_whitespace().collect();
        assert_eq!(
            &first[2..],
            [
                "spoolway",
                "spoolway-plan",
                "claude-opus-5",
                "144",
                "0.00",
                "35.1k",
                "0.53",
                "9.45M",
                "3.78",
                "180.2k",
                "0.68",
                "3.81",
                "0s"
            ]
        );
        assert_eq!(
            table.total.split_whitespace().collect::<Vec<_>>(),
            [
                "Total", "144", "0.00", "35.1k", "0.53", "9.45M", "3.78", "180.2k", "0.68", "4.71",
                "0s"
            ]
        );
    }

    /// Under `by dir` the export's `Total` line carries what the screen's
    /// carries in either view: every sum, and each per-session column as its
    /// sum over the table's two sessions. The peaks stay blank.
    #[test]
    fn a_directory_export_marks_its_total_under_either_by() {
        let a = dir_line("/w/proj", "s1", "2026-09-01T09:00:00+00:00", 0.70);
        let b = dir_line("/w/proj", "s2", "2026-09-01T10:00:00+00:00", 0.18);
        let dirs = dir_rows(&[&a, &b], &[], &no_models(), &HashMap::new());
        let sessions = list_sessions(&[&a, &b], &no_models(), &HashMap::new(), &HashMap::new());
        let cols = DIRS_CSV_HEADER.split(',').count();
        assert_eq!(csv_dir_row(&dirs[0]).split(',').count(), cols);
        let total = csv_dirs_total(DirBy::Dir, &dirs, &sessions);
        assert_eq!(total.split(',').count(), cols);
        assert_eq!(
            total,
            "total,,2,0,20,0,0,0,10,0,0,0.00,0.00,0.00,0.00,0.00,0.00,0.00,0.00,0.88,0.44,,,,,,0,0"
        );

        let cols = SESSIONS_CSV_HEADER.split(',').count();
        assert_eq!(csv_session_row(&sessions[0]).split(',').count(), cols);
        let total = csv_dirs_total(DirBy::Session, &[], &sessions);
        assert_eq!(total, "total,,,,,0,20,0,0,0.00,0.00,0.00,0.00,0.88,,,,0");
    }

    /// Two priced sessions in `/w/priced`, one session in `/w/local` on a
    /// model no table prices, and a watched root no session has run in:
    /// each priced session spends $3 of input, $15 of output, $3 of cache
    /// reads and $3.75 of cache writes at [`priced_models`]' rates.
    fn priced_dirs() -> (Vec<Entry>, Vec<String>, BTreeMap<String, ModelPrice>) {
        let spent = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 10_000_000,
            cache_write_5m: 1_000_000,
            ..Tokens::default()
        };
        let mut a = dir_line("/w/priced", "s1", "2026-09-01T09:00:00+00:00", 25.0);
        a.model = "sized".into();
        a.tokens = spent;
        let mut b = dir_line("/w/priced", "s2", "2026-09-01T10:00:00+00:00", 25.0);
        b.model = "sized".into();
        b.tokens = spent;
        let mut c = dir_line("/w/local", "s3", "2026-09-01T08:00:00+00:00", 0.0);
        c.model = "qwen".into();
        c.cost_usd = None;
        let roots = vec!["/w/quiet".to_string()];
        (vec![a, b, c], roots, priced_models(100_000))
    }

    /// `by dir` draws each class cost right after its token column: the
    /// row's sums in the totals view and each over `SESSIONS` per session,
    /// blank where nothing could be priced and `—` where nothing ran. The
    /// summary line carries the sums, and per session each over the
    /// table's three sessions.
    #[test]
    fn the_dirs_table_prices_each_token_class_beside_it() {
        let (entries, roots, models) = priced_dirs();
        let refs: Vec<&Entry> = entries.iter().collect();
        let rows = dir_rows(&refs, &roots, &models, &HashMap::new());
        let row = |table: &Table, dir: &str| {
            table
                .rows
                .iter()
                .find(|r| r.starts_with(dir))
                .unwrap_or_else(|| panic!("no {dir} row: {:?}", table.rows))
                .clone()
        };

        let table = dirs_table(&rows, None, Figures::Totals);
        assert!(
            table.header.contains(
                "  IN  IN USD      OUT  OUT USD  CACHE R  CACHE R USD  CACHE W  CACHE W USD       USD"
            ),
            "{}",
            table.header
        );
        let cells = [
            ("IN USD", "6.00", "3.00"),
            ("OUT USD", "30.00", "15.00"),
            ("CACHE R USD", "6.00", "3.00"),
            ("CACHE W USD", "7.50", "3.75"),
        ];
        for (title, sum, _) in cells {
            let under = |line: &str| cell_under(&table.header, line, title);
            assert_eq!(under(&row(&table, "/w/priced")), sum, "{title}");
            assert_eq!(under(&row(&table, "/w/local")), "", "{title}");
            assert_eq!(under(&row(&table, "/w/quiet")), "—", "{title}");
            assert_eq!(under(&table.total), sum, "{title} on Total");
        }

        let table = dirs_table(&rows, None, Figures::PerRun);
        for (title, sum, each) in cells {
            let title = format!("{title}/SESSION");
            let under = |line: &str| cell_under(&table.header, line, &title);
            assert_eq!(under(&row(&table, "/w/priced")), each, "{title}");
            assert_eq!(under(&row(&table, "/w/local")), "", "{title}");
            assert_eq!(under(&row(&table, "/w/quiet")), "—", "{title}");
            let average = format!("{:.2}", sum.parse::<f64>().unwrap() / 3.0);
            assert_eq!(under(&table.total), average, "{title} on Average");
        }
    }

    /// `by session` draws each class cost after its token column, blank
    /// for a session no table prices, and its `Total` line sums them.
    #[test]
    fn the_sessions_table_prices_each_token_class_beside_it() {
        let (entries, _, models) = priced_dirs();
        let refs: Vec<&Entry> = entries.iter().collect();
        let rows = list_sessions(&refs, &models, &HashMap::new(), &HashMap::new());
        let table = sessions_table(&rows, None);
        let line = |dir: &str| {
            table
                .rows
                .iter()
                .find(|r| r.contains(dir))
                .unwrap_or_else(|| panic!("no {dir} row: {:?}", table.rows))
        };
        for (title, each, sum) in [
            ("IN USD", "3.00", "6.00"),
            ("OUT USD", "15.00", "30.00"),
            ("CACHE R USD", "3.00", "6.00"),
            ("CACHE W USD", "3.75", "7.50"),
        ] {
            let under = |row: &str| cell_under(&table.header, row, title);
            assert_eq!(under(line("priced")), each, "{title}");
            assert_eq!(under(line("local")), "", "{title}");
            assert_eq!(under(&table.total), sum, "{title} on Total");
        }
    }

    /// The directory export carries each class cost under the lanes
    /// export's names — with a `_per_session` twin under `by dir`, and none
    /// under `by session`, whose one view has no per-session column — blank
    /// where nothing could be priced or nothing ran. A sort on any of them
    /// reads that class's own figure, and puts the unpriced row and the root
    /// nothing ran in last whichever way it runs.
    #[test]
    fn the_dir_class_costs_export_and_sort() {
        let (entries, roots, models) = priced_dirs();
        let refs: Vec<&Entry> = entries.iter().collect();
        let mut dirs = dir_rows(&refs, &roots, &models, &HashMap::new());
        let mut sessions = list_sessions(&refs, &models, &HashMap::new(), &HashMap::new());
        let at = |header: &str, line: &str, name: &str| {
            let i = header
                .split(',')
                .position(|n| n == name)
                .unwrap_or_else(|| panic!("no {name}: {header}"));
            line.split(',').nth(i).unwrap().to_string()
        };
        let dir_of =
            |rows: &[DirRow], dir: &str| csv_dir_row(rows.iter().find(|r| r.dir == dir).unwrap());

        let figures = [
            ("in_usd", "6.00"),
            ("out_usd", "30.00"),
            ("cache_read_usd", "6.00"),
            ("cache_write_usd", "7.50"),
            ("in_usd_per_session", "3.00"),
            ("out_usd_per_session", "15.00"),
            ("cache_read_usd_per_session", "3.00"),
            ("cache_write_usd_per_session", "3.75"),
        ];
        let total = csv_dirs_total(DirBy::Dir, &dirs, &sessions);
        for (name, value) in figures {
            let priced = dir_of(&dirs, "/w/priced");
            assert_eq!(
                at(DIRS_CSV_HEADER, &priced, name),
                value,
                "{name}: {priced}"
            );
            for dir in ["/w/local", "/w/quiet"] {
                let line = dir_of(&dirs, dir);
                assert_eq!(at(DIRS_CSV_HEADER, &line, name), "", "{name}: {line}");
            }
        }
        assert_eq!(at(DIRS_CSV_HEADER, &total, "out_usd"), "30.00", "{total}");
        // 30.00 over the table's three sessions.
        assert_eq!(
            at(DIRS_CSV_HEADER, &total, "out_usd_per_session"),
            "10.00",
            "{total}"
        );

        assert!(!SESSIONS_CSV_HEADER.contains("_per_session"));
        let line = csv_session_row(sessions.iter().find(|r| r.dir == "/w/priced").unwrap());
        assert_eq!(at(SESSIONS_CSV_HEADER, &line, "cache_write_usd"), "3.75");
        let line = csv_session_row(sessions.iter().find(|r| r.dir == "/w/local").unwrap());
        assert_eq!(at(SESSIONS_CSV_HEADER, &line, "out_usd"), "");
        let total = csv_dirs_total(DirBy::Session, &[], &sessions);
        assert_eq!(at(SESSIONS_CSV_HEADER, &total, "in_usd"), "6.00", "{total}");

        for (name, value) in figures {
            assert!(
                DIR_TOTAL_COLUMNS
                    .iter()
                    .chain(DIR_COLUMNS.iter())
                    .any(|(_, k)| *k == name),
                "{name} is drawn"
            );
            // The class's own figure, so a key read off the wrong class fails
            // here even where the order below would not show it.
            let priced = dirs.iter().find(|r| r.dir == "/w/priced").unwrap();
            match dir_sort_value(priced, name) {
                Some(SortValue::Figure(0, usd)) => {
                    assert_eq!(format!("{usd:.2}"), value, "{name}")
                }
                _ => panic!("{name} has no figure on /w/priced"),
            }
            for descending in [true, false] {
                // `dir_rows` already puts /w/priced first, and a sort whose
                // every value is blank keeps the order it was given — so
                // start with /w/quiet on top, or a missing key would pass.
                dirs.sort_by_key(|r| r.dir != "/w/quiet");
                assert_ne!(dirs[0].dir, "/w/priced");
                let sort = sorted(name, descending);
                sort_rows(&mut dirs, &sort, |row| dir_sort_value(row, name));
                assert_eq!(dirs[0].dir, "/w/priced", "{name}, descending {descending}");
            }
        }
        for (name, _) in &figures[..4] {
            assert!(SESSION_COLUMNS.iter().any(|(_, k)| k == name), "{name}");
            for descending in [true, false] {
                let sort = sorted(name, descending);
                sort_rows(&mut sessions, &sort, |row| session_sort_value(row, name));
                assert_eq!(
                    sessions[2].dir, "/w/local",
                    "{name}, descending {descending}"
                );
            }
        }
    }

    // -------------------------------------------------------------- sorting

    fn sorted(key: &str, descending: bool) -> Sort {
        Sort {
            key: key.to_string(),
            descending,
        }
    }

    /// The `STEP` cell of every row, top to bottom.
    fn steps(rows: &[LaneRow]) -> Vec<&str> {
        rows.iter().map(|r| r.cells[1].as_str()).collect()
    }

    /// As text, `1h 20m` sorts under `35m` and `54.2k` under `9.10M` — the
    /// first digit decides. The sort reads the figures behind the cells.
    #[test]
    fn a_sort_reads_raw_time_and_token_figures_not_the_drawn_cells() {
        let mut entries = vec![
            lane("a", "implement", 1, Some("pass")),
            lane("a", "review", 1, Some("pass")),
            lane("a", "document", 1, Some("pass")),
        ];
        (entries[0].wall_s, entries[0].tokens.input) = (80 * 60, 54_200);
        (entries[1].wall_s, entries[1].tokens.input) = (35 * 60, 9_100_000);
        (entries[2].wall_s, entries[2].tokens.input) = (9 * 60, 800);
        let mut rows = rows_by(&entries, EvalBy::Step);
        let drawn = lanes_table(
            EvalBy::Step,
            &rows,
            &total_of(&entries),
            None,
            Figures::PerRun,
        )
        .rows;
        let drawn = drawn.join("\n");
        for cell in ["1h 20m", "35m", "54.2k", "9.10M"] {
            assert!(
                drawn.contains(cell),
                "the cells a text sort would get wrong: {drawn}"
            );
        }

        sort_lane_rows(EvalBy::Step, &mut rows, &sorted("time_per_run_s", true));
        assert_eq!(steps(&rows), ["implement", "review", "document"]);
        sort_lane_rows(EvalBy::Step, &mut rows, &sorted("time_per_run_s", false));
        assert_eq!(steps(&rows), ["document", "review", "implement"]);
        sort_lane_rows(EvalBy::Step, &mut rows, &sorted("in_per_run", true));
        assert_eq!(steps(&rows), ["review", "implement", "document"]);
        sort_lane_rows(EvalBy::Step, &mut rows, &sorted("in_per_run", false));
        assert_eq!(steps(&rows), ["document", "implement", "review"]);
    }

    /// Rows the sorted column ties on keep the order they had, and a row
    /// with no figure at all — `—` drawn — sorts last whichever way.
    #[test]
    fn ties_keep_the_default_order_and_a_blank_sorts_last_both_ways() {
        let mut entries = vec![
            lane("a", "implement", 1, Some("pass")),
            lane("a", "review", 1, None),
            lane("a", "document", 1, Some("pass")),
            lane("a", "e2e", 1, Some("fail")),
        ];
        // Nothing could price `document`: its `USD` is `—`, not `0.00`.
        entries[2].cost_usd = None;
        entries[2].tokens.input = 10;
        let default: Vec<String> = steps(&rows_by(&entries, EvalBy::Step))
            .into_iter()
            .map(String::from)
            .collect();
        let passed: Vec<&str> = default
            .iter()
            .map(String::as_str)
            .filter(|s| ["implement", "document"].contains(s))
            .collect();

        let mut rows = rows_by(&entries, EvalBy::Step);
        sort_lane_rows(EvalBy::Step, &mut rows, &sorted("pass", true));
        let want: Vec<&str> = passed.iter().copied().chain(["e2e", "review"]).collect();
        assert_eq!(
            steps(&rows),
            want,
            "review reported nothing: its PASS is blank"
        );

        let mut rows = rows_by(&entries, EvalBy::Step);
        sort_lane_rows(EvalBy::Step, &mut rows, &sorted("pass", false));
        let want: Vec<&str> = ["e2e"]
            .into_iter()
            .chain(passed.iter().copied())
            .chain(["review"])
            .collect();
        assert_eq!(steps(&rows), want);

        for descending in [true, false] {
            let mut rows = rows_by(&entries, EvalBy::Step);
            sort_lane_rows(EvalBy::Step, &mut rows, &sorted("cost_usd", descending));
            assert_eq!(
                steps(&rows).last(),
                Some(&"document"),
                "unpriced sorts last"
            );
        }
    }

    /// `--sort` takes an export column, descending unless told otherwise,
    /// and refuses a name `--by` does not export with every one it does.
    #[test]
    fn parse_sort_defaults_to_descending_and_refuses_an_unknown_column() {
        assert_eq!(
            parse_sort(EvalBy::Step, "pass").unwrap(),
            sorted("pass", true)
        );
        assert_eq!(
            parse_sort(EvalBy::Step, "pass:asc").unwrap(),
            sorted("pass", false)
        );
        assert_eq!(
            parse_sort(EvalBy::Step, "step:desc").unwrap(),
            sorted("step", true)
        );

        let err = parse_sort(EvalBy::Pipeline, "step")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no column `step` under `--by pipeline`"),
            "{err}"
        );
        assert!(
            err.contains(&lanes_csv_header(EvalBy::Pipeline).replace(',', ", ")),
            "{err}"
        );
        let err = parse_sort(EvalBy::Step, "PASS").unwrap_err().to_string();
        assert!(
            err.contains("no column `PASS`"),
            "the header's spelling is not one: {err}"
        );
        let err = parse_sort(EvalBy::Step, "pass:up").unwrap_err().to_string();
        assert!(err.contains("`asc` or `desc`"), "{err}");
    }

    /// The mark takes the space before the sorted column's name, so no
    /// column moves; a first column has no such space, and hands its mark
    /// back as the lead for the screen to draw in the cursor's column.
    #[test]
    fn the_sort_mark_takes_the_space_before_its_column() {
        let entries = vec![lane("a", "implement", 1, Some("pass"))];
        let rows = rows_by(&entries, EvalBy::Step);
        let total = total_of(&entries);
        let plain = lanes_table(EvalBy::Step, &rows, &total, None, Figures::PerRun);
        assert_eq!(plain.lead, ' ');

        let up = lanes_table(
            EvalBy::Step,
            &rows,
            &total,
            Some(&sorted("pass", false)),
            Figures::PerRun,
        );
        assert!(up.header.contains(" ▲PASS  BLOCKS"), "{}", up.header);
        assert_eq!(up.header.replace('▲', " "), plain.header, "no column moved");
        assert_eq!(up.lead, ' ');

        let down = lanes_table(
            EvalBy::Step,
            &rows,
            &total,
            Some(&sorted("step", true)),
            Figures::PerRun,
        );
        assert!(down.header.contains(" ▼STEP"), "{}", down.header);
        assert_eq!(down.header.replace('▼', " "), plain.header);

        let first = lanes_table(
            EvalBy::Step,
            &rows,
            &total,
            Some(&sorted("pipeline", true)),
            Figures::PerRun,
        );
        assert_eq!(
            first.header, plain.header,
            "nowhere in the header to put it"
        );
        assert_eq!(first.lead, '▼');

        let undrawn = lanes_table(
            EvalBy::Step,
            &rows,
            &total,
            Some(&sorted("in_tokens", true)),
            Figures::PerRun,
        );
        assert_eq!((undrawn.header, undrawn.lead), (plain.header, ' '));
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
                tier_tokens: Default::default(),
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
                blocked: false,
                run: Some(format!("r-{task}")),
                trial: None,
                trial_group: None,
                dir: None,
                hand: false,
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
            all: false,
            project: None,
            trial: None,
            discard: None,
            force: false,
            csv: false,
            per_run: false,
            sort: None,
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
            running_trials: BTreeSet::new(),
        }
    }

    /// A fixture with one ordinary `implement` run banked — the smallest
    /// ledger that draws a real table.
    fn fixture_with_one_run(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let (repo, root_guard) = fixture(name);
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
        (repo, root_guard)
    }

    /// `load` retains only `Entry::is_lane` over the real ledger — pinned
    /// against a fixture repo holding both an interactive line and a lane
    /// line, on disk, so deleting `load`'s own filter breaks this rather
    /// than an out-of-band replay of the same predicate.
    #[test]
    fn load_never_returns_an_interactive_line() {
        let (repo, _root_guard) = fixture_with_one_run("load-excludes-interactive");
        crate::usage::append(
            &repo,
            &Entry {
                tier_tokens: Default::default(),
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
                blocked: false,
                run: None,
                trial: None,
                trial_group: None,
                dir: None,
                hand: false,
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

    /// A load that has already landed by the time the screen looks: a
    /// scripted input always has its next key pending, so against a real
    /// thread every key would race the load.
    fn load_now(repo: &Repo, filters: &Filters) -> Pending {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(load(repo, filters)).unwrap();
        rx
    }

    /// [`run_screen_with`] over `repo`, loading through [`load_now`].
    fn run_now(
        repo: &Repo,
        args: &EvalArgs,
        input: &mut std::io::Cursor<Vec<u8>>,
        out: &mut Vec<u8>,
    ) -> crate::screen::shell::Leave {
        run_screen_with(
            repo,
            &Pipelines::builtin(),
            args,
            |filters| load_now(repo, filters),
            &mut crate::screen::frame_writer::FrameWriter::new(),
            input,
            out,
        )
        .unwrap()
    }

    /// Runs the screen against `repo` with the default args, feeding it
    /// `input`, and hands back everything it drew.
    fn screen(repo: &Repo, input: &str) -> String {
        let mut input = keys(input);
        let mut out = Vec::new();
        run_now(repo, &no_args(), &mut input, &mut out);
        String::from_utf8(out).unwrap()
    }

    /// Hosted, a ledger eval cannot read is held on the tab under the strip
    /// — the shell still has four other tabs to reach — rather than printed
    /// on the way out, and `←` leaves it.
    #[test]
    fn hosted_an_unreadable_window_is_held_on_the_tab_until_a_shell_key() {
        use crate::screen::shell::{Hosting, Leave, Tab, Toward};
        let (repo, _root_guard) = fixture_with_one_run("screen-hosted-load-error");
        let _hosting = Hosting::open(Tab::Eval);
        let args = EvalArgs {
            since: Some("not-a-date".into()),
            ..no_args()
        };
        let mut input = keys("x\x1b[D");
        let mut out = Vec::new();
        let leave = run_now(&repo, &args, &mut input, &mut out);
        assert_eq!(leave, Leave::Switch(Toward::Left));
        let drawn = String::from_utf8(out).unwrap();
        assert!(drawn.contains("spoolway eval: "), "{drawn}");
        assert!(drawn.contains("DISPATCH"), "under the strip: {drawn}");
    }

    /// Hosted as bare `spoolway`'s eval tab, `←` and `→` while browsing hand
    /// the screen back to the shell, and the frame is eval's own under the
    /// strip. Inside the filter panel they keep cycling a row's value.
    #[test]
    fn hosted_browsing_leaves_on_the_arrows_but_the_filters_keep_them() {
        use crate::screen::shell::{Hosting, Leave, Tab, Toward};
        let (repo, _root_guard) = fixture_with_one_run("screen-hosted-leave");
        let _hosting = Hosting::open(Tab::Eval);
        let run = |input: &str| {
            let mut input = keys(input);
            let mut out = Vec::new();
            let leave = run_now(&repo, &no_args(), &mut input, &mut out);
            (leave, String::from_utf8(out).unwrap())
        };

        let (leave, drawn) = run("\x1b[D");
        assert_eq!(leave, Leave::Switch(Toward::Left));
        let first = drawn.split("\x1b[?2026h\x1b[H").nth(1).unwrap();
        assert!(first.contains("DISPATCH"), "{first}");
        assert!(first.contains("─ eval · by pipeline"), "{first}");
        assert_eq!(run("\x1b[C").0, Leave::Switch(Toward::Right));

        // `f` opens the filters; `→` there cycles a row, and the input then
        // runs out inside the panel.
        assert_eq!(run("f\x1b[C\x1b[D").0, Leave::Quit);
    }

    /// Every draw opens with the shared frame writer's own start code, so a
    /// whole captured transcript holds every frame the screen ever drew,
    /// back to back — a plain `text.contains(...)` over it can true
    /// positive on a state that has since moved on. This is the frame that
    /// was actually on screen when the input ran out.
    fn last_frame(text: &str) -> &str {
        text.rsplit("\x1b[?2026h\x1b[H").next().unwrap_or(text)
    }

    /// Every frame the screen drew, in order.
    fn frames(text: &str) -> Vec<&str> {
        text.split("\x1b[?2026h\x1b[H").skip(1).collect()
    }

    /// The loading popup, exactly as the mockup draws it.
    const LOADING: [&str; 3] = ["┌─ eval ─────┐", "│  Loading…  │", "└────────────┘"];

    fn shows_loading(frame: &str) -> bool {
        LOADING.iter().all(|line| frame.contains(line))
    }

    type Senders = std::rc::Rc<std::cell::RefCell<Vec<std::sync::mpsc::Sender<Result<Loaded>>>>>;

    /// A load that has not landed: its sender is kept, so the receiver
    /// reads empty rather than disconnected, until the test answers it.
    fn held(senders: &Senders) -> Pending {
        let (tx, rx) = std::sync::mpsc::channel();
        senders.borrow_mut().push(tx);
        rx
    }

    /// Hosted, switching to eval draws the strip, the eval frame and the
    /// keyless loading popup before the load lands. While it shows, `f` and
    /// `tab` are not read and `→` still leaves, and leaving drops the
    /// receiver, so the thread's late result goes nowhere.
    #[test]
    fn the_first_frame_is_drawn_under_the_loading_popup_before_the_load_lands() {
        use crate::screen::shell::{Hosting, Leave, Tab, Toward};
        let (repo, _root_guard) = fixture_with_one_run("screen-loading-first");
        let _hosting = Hosting::open(Tab::Eval);
        let senders = Senders::default();
        let mut input = keys("f\t\x1b[C");
        let mut out = Vec::new();
        let leave = run_screen_with(
            &repo,
            &Pipelines::builtin(),
            &no_args(),
            |_| held(&senders),
            &mut crate::screen::frame_writer::FrameWriter::new(),
            &mut input,
            &mut out,
        )
        .unwrap();
        assert_eq!(leave, Leave::Switch(Toward::Right));

        let drawn = String::from_utf8(out).unwrap();
        let all = frames(&drawn);
        // Not one per ignored key any more: `f` and `tab` leave the loading
        // frame exactly as it was, and the shared frame writer now skips a
        // frame that matches the last one it painted — the same skip
        // `commands::queue` and `commands::jobs` already had, extended to
        // every redrawing screen by this task.
        assert_eq!(all.len(), 1, "no frame changed, so nothing was repainted");
        for frame in &all {
            assert!(frame.contains("DISPATCH"), "under the strip: {frame}");
            assert!(frame.contains("┌─ eval · by pipeline "), "{frame}");
            assert!(shows_loading(frame), "{frame}");
            assert!(!frame.contains("PIPELINE"), "no table yet: {frame}");
            assert!(!frame.contains("┌─ filters"), "`f` was not read: {frame}");
            assert!(
                frame.contains(
                    "[↑↓] move   [a] ascending   [d] descending   [t] per run   [tab] dirs"
                ),
                "{frame}"
            );
        }

        assert_eq!(senders.borrow().len(), 1, "one load, started once");
        assert!(
            senders.borrow()[0].send(Ok(loaded(Vec::new()))).is_err(),
            "leaving dropped the result"
        );
    }

    /// A scripted input that reports no key pending on its first poll, and
    /// answers the load in flight right then — so the result lands in
    /// `wait_key`'s idle slice, behind a frame already drawn, the way a slow
    /// load's does.
    struct LandsWhileIdle {
        keys: std::io::Cursor<Vec<u8>>,
        senders: Senders,
        result: std::cell::RefCell<Option<Result<Loaded>>>,
    }

    impl std::io::Read for LandsWhileIdle {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.keys.read(buf)
        }
    }

    impl PollableRead for LandsWhileIdle {
        fn byte_pending(&self, _timeout: std::time::Duration) -> bool {
            match self.result.borrow_mut().take() {
                Some(result) => {
                    let senders = self.senders.borrow();
                    senders.last().unwrap().send(result).unwrap();
                    false
                }
                None => true,
            }
        }
    }

    /// Standalone, the late result closes the popup by itself and the table
    /// is drawn on the idle slice it landed in, with no key pressed.
    #[test]
    fn a_result_landing_while_idle_closes_the_popup_and_draws_the_table() {
        let (repo, _root_guard) = fixture_with_one_run("screen-loading-late");
        let senders = Senders::default();
        let mut input = LandsWhileIdle {
            keys: keys("q"),
            senders: senders.clone(),
            result: std::cell::RefCell::new(Some(load(&repo, &no_filters()))),
        };
        let mut out = Vec::new();
        run_screen_with(
            &repo,
            &Pipelines::builtin(),
            &no_args(),
            |_| held(&senders),
            &mut crate::screen::frame_writer::FrameWriter::new(),
            &mut input,
            &mut out,
        )
        .unwrap();

        let drawn = String::from_utf8(out).unwrap();
        let all = frames(&drawn);
        assert_eq!(all.len(), 2, "the loading frame, then the landed one");
        assert!(shows_loading(all[0]), "{}", all[0]);
        assert!(!all[0].contains("PIPELINE"), "{}", all[0]);
        assert!(!all[1].contains("Loading…"), "{}", all[1]);
        assert!(all[1].contains("│ PIPELINE"), "{}", all[1]);
        assert!(all[1].contains("│>default"), "{}", all[1]);
    }

    /// `r` and the filter panel's `enter` each reload under the same popup,
    /// over the table already shown rather than an empty frame.
    #[test]
    fn a_reload_shows_the_popup_over_the_table_already_shown() {
        let (repo, _root_guard) = fixture_with_one_run("screen-loading-reload");
        for script in ["r", "f\r"] {
            let senders = Senders::default();
            let mut first = true;
            let mut input = keys(script);
            let mut out = Vec::new();
            run_screen_with(
                &repo,
                &Pipelines::builtin(),
                &no_args(),
                |filters| {
                    if std::mem::take(&mut first) {
                        load_now(&repo, filters)
                    } else {
                        held(&senders)
                    }
                },
                &mut crate::screen::frame_writer::FrameWriter::new(),
                &mut input,
                &mut out,
            )
            .unwrap();

            let drawn = String::from_utf8(out).unwrap();
            let last = last_frame(&drawn);
            assert!(shows_loading(last), "{script:?}: {last}");
            assert!(last.contains("│ PIPELINE"), "{script:?}: {last}");
            assert!(!last.contains("┌─ filters"), "{script:?}: {last}");
            assert_eq!(senders.borrow().len(), 1, "{script:?}: one reload");
        }
    }

    /// The acceptance criterion this task exists for: the eval tab's own
    /// frame, painted through the shared [`crate::screen::frame_writer`],
    /// must look exactly as it did through today's frozen erase-then-write —
    /// cell for cell, in text, colour and bold.
    #[test]
    fn eval_paints_as_before() {
        let (repo, _root_guard) = fixture_with_one_run("eval-paints-as-before");
        let pipelines = Pipelines::builtin();
        let state = ScreenState::new(&no_args());
        let loaded = load(&repo, &no_filters()).unwrap();
        let frame = eval_frame_rows(&pipelines, Some(&loaded), &state);
        // Wide enough that no row here reaches the pane's own edge.
        let pane_size = (250, 60);

        let mut old = Vec::new();
        crate::screen::frame_writer::todays_write(&frame, &mut old);

        let mut new = Vec::new();
        crate::screen::frame_writer::FrameWriter::new().write_frame(&frame, pane_size, &mut new);

        crate::screen::frame_writer::assert_same_picture(&old, &new, pane_size);
    }

    /// A reload that fails with a table on screen reaches the person in the
    /// same notice it always did, with its key line; the table stays.
    #[test]
    fn a_failed_reload_lands_as_the_eval_notice() {
        let (repo, _root_guard) = fixture_with_one_run("screen-loading-reload-error");
        let senders = Senders::default();
        let mut state = ScreenState::new(&no_args());
        let mut loaded = Some(load(&repo, &no_filters()).unwrap());
        let mut failed = None;
        state.start_loading(&mut |_| held(&senders), no_filters(), false);
        senders.borrow()[0]
            .send(Err(anyhow::anyhow!("no ledger")))
            .unwrap();
        assert!(state.settle(&mut loaded, &mut failed));
        assert!(failed.is_none());
        assert!(loaded.is_some(), "the table already shown stays");
        match &state.mode {
            Mode::Notice { title, body, keys } => {
                assert_eq!(*title, "eval");
                assert_eq!(body, "no ledger");
                assert_eq!(*keys, Some(NOTICE_KEYS));
            }
            _ => panic!("expected the eval notice"),
        }
    }

    /// When a newer load replaces one still in flight, the older one's
    /// late result goes nowhere and only the newest lands, with the filters
    /// it was loaded through and — from the filter panel — the cursor home.
    #[test]
    fn only_the_newest_of_two_overlapping_loads_is_kept() {
        let (_repo, _root_guard) = fixture_with_one_run("screen-loading-overlap");
        let senders = Senders::default();
        let mut state = ScreenState::new(&no_args());
        state.cursor = 3;
        let mut loaded = None;
        let mut failed = None;
        state.start_loading(&mut |_| held(&senders), no_filters(), false);
        assert!(!state.settle(&mut loaded, &mut failed), "nothing landed");

        let newer = Filters {
            step: Some("implement".into()),
            ..no_filters()
        };
        state.start_loading(&mut |_| held(&senders), newer.clone(), true);
        assert!(
            senders.borrow()[0].send(Ok(loaded_one("old"))).is_err(),
            "the older load's result goes nowhere"
        );
        senders.borrow()[1].send(Ok(loaded_one("new"))).unwrap();
        assert!(state.settle(&mut loaded, &mut failed));
        assert!(matches!(state.mode, Mode::Browsing));
        assert_eq!(loaded.unwrap().entries[0].task, "new");
        assert_eq!(state.filters, newer);
        assert_eq!(state.cursor, 0);
    }

    fn loaded_one(task: &str) -> Loaded {
        loaded(vec![tests_entry(task, "implement")])
    }

    /// The thread the tab really loads on hands back what `load` itself
    /// reads.
    #[test]
    fn load_in_background_lands_what_load_reads() {
        let (repo, _root_guard) = fixture_with_one_run("screen-loading-thread");
        let landed = load_in_background(&repo, &no_filters())
            .recv()
            .unwrap()
            .unwrap();
        assert_eq!(landed.entries.len(), 1);
        assert_eq!(landed.entries[0].task, "a");
    }

    /// A table far taller than the terminal — 270 tasks on a 20-row one —
    /// keeps its `Total` line as the frame's last body line, under the
    /// scroll indicator, and its header as the first, wherever the cursor
    /// has scrolled to. The filter panel drawn over it still closes its own
    /// bottom border.
    #[test]
    fn the_total_line_stays_pinned_under_a_scrolled_table() {
        let entries: Vec<Entry> = (0..270)
            .map(|n| tests_entry(&format!("task-{n:03}"), "implement"))
            .collect();
        let tall = loaded(entries);
        let pipelines = Pipelines::builtin();
        let mut state = ScreenState::new(&no_args());
        state.filters.by = EvalBy::Task;
        let last_body_lines = |frame: &[String]| -> (String, String) {
            let bottom = frame
                .iter()
                .rposition(|l| l.starts_with('└'))
                .expect("the frame's own bottom border");
            (frame[bottom - 2].clone(), frame[bottom - 1].clone())
        };

        for (cursor, indicator) in [(0, "↓ "), (120, "↓ "), (269, "↑ ")] {
            state.cursor = cursor;
            let frame = eval_frame_rows_at(&pipelines, Some(&tall), &state, Some(20));
            assert!(frame.len() <= 20, "{}", frame.join("\n"));
            let (above, last) = last_body_lines(&frame);
            assert!(last.starts_with("│ Total "), "{}", frame.join("\n"));
            assert!(last.contains(" 270 "), "{last}");
            assert!(above.starts_with(&format!("│ {indicator}")), "{above}");
            let top = frame.iter().position(|l| l.starts_with('┌')).unwrap();
            assert!(
                frame[top + 1].starts_with("│ TASK "),
                "the header stays at the top at {cursor}: {}",
                frame.join("\n")
            );
            assert!(
                frame.iter().any(|l| l.starts_with('│') && l.contains('>')),
                "the cursor's row is in view at {cursor}: {}",
                frame.join("\n")
            );
        }

        state.cursor = 0;
        state.mode = Mode::Filter(Draft::new(state.table, state.filters.clone()));
        let frame = eval_frame_rows_at(&pipelines, Some(&tall), &state, Some(20));
        let text = frame.join("\n");
        assert!(text.contains("[enter] apply   [esc] back"), "{text}");
        assert_eq!(
            frame.iter().filter(|l| l.contains('└')).count(),
            2,
            "the panel's bottom border and the frame's: {text}"
        );
        assert!(last_body_lines(&frame).1.starts_with("│ Total "), "{text}");

        // A table that fits keeps its `Total` line right under its last row.
        let few = loaded(vec![tests_entry("a", "implement")]);
        state.mode = Mode::Browsing;
        let frame = eval_frame_rows_at(&pipelines, Some(&few), &state, Some(20));
        let total = frame
            .iter()
            .position(|l| l.starts_with("│ Total "))
            .unwrap();
        assert!(frame[total - 1].starts_with("│>a "), "{}", frame.join("\n"));
    }

    /// A frame too short to pin both lines still draws the cursor's row:
    /// the total is pinned only from three body rows, the header only from
    /// four, so `clip` always keeps two — the cursor's row and its
    /// indicator. Below three, both scroll with the rows as they did before
    /// pinning, and the frame never grows taller than the terminal.
    #[test]
    fn a_short_frame_pins_only_what_leaves_the_cursor_s_row_in_view() {
        let entries: Vec<Entry> = (0..270)
            .map(|n| tests_entry(&format!("task-{n:03}"), "implement"))
            .collect();
        let tall = loaded(entries);
        let pipelines = Pipelines::builtin();
        let mut state = ScreenState::new(&no_args());
        state.filters.by = EvalBy::Task;
        state.cursor = 120;
        // `frame_chrome()` is four, so a terminal `n + 4` tall gives the
        // frame `n` body rows.
        for (height, header, total) in [(6, false, false), (7, false, true), (8, true, true)] {
            let frame = eval_frame_rows_at(&pipelines, Some(&tall), &state, Some(height));
            let text = frame.join("\n");
            assert!(frame.len() <= height, "{height}: {text}");
            let body: Vec<&String> = frame.iter().filter(|l| l.starts_with('│')).collect();
            assert!(
                body.iter().any(|l| l.starts_with("│>task-120 ")),
                "{height}: the cursor's row is drawn: {text}"
            );
            assert_eq!(
                body.last().unwrap().starts_with("│ Total "),
                total,
                "{height}: {text}"
            );
            assert_eq!(body[0].starts_with("│ TASK "), header, "{height}: {text}");
        }
        // One body row has room for the indicator alone, as before pinning;
        // nothing pinned pushes the frame past the terminal's own height.
        let frame = eval_frame_rows_at(&pipelines, Some(&tall), &state, Some(5));
        assert!(frame.len() <= 5, "{}", frame.join("\n"));
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
        assert_eq!(
            no_filters().by,
            EvalBy::ALL[0],
            "the screen opens at the left end"
        );
        assert_eq!(
            cycle_by(&EvalBy::ALL, EvalBy::Pipeline, false),
            EvalBy::Pipeline
        );
        assert_eq!(cycle_by(&EvalBy::ALL, EvalBy::Pipeline, true), EvalBy::Step);
        assert_eq!(cycle_by(&EvalBy::ALL, EvalBy::Version, true), EvalBy::Group);
        assert_eq!(cycle_by(&EvalBy::ALL, EvalBy::Group, true), EvalBy::Task);
        assert_eq!(cycle_by(&EvalBy::ALL, EvalBy::Task, true), EvalBy::Task);
        assert_eq!(cycle_by(&DirBy::ALL, DirBy::Dir, true), DirBy::Session);
    }

    /// A change of pipeline that strands the chosen step clears it.
    #[test]
    fn a_new_pipeline_clears_a_step_it_never_ran() {
        let mut other = tests_entry("b", "review");
        other.pipeline = "bugfix".into();
        let loaded = loaded(vec![tests_entry("a", "implement"), other]);
        let mut draft = Draft::new(TableKind::Lanes, no_filters());
        draft.filters.step = Some("implement".into());
        draft.field = 1; // pipeline
        handle_filter_change(&loaded, &mut draft, true);
        assert_eq!(draft.filters.pipeline.as_deref(), Some("bugfix"));
        assert_eq!(draft.filters.step, None, "bugfix never ran `implement`");
    }

    fn tests_entry(task: &str, step: &str) -> Entry {
        Entry {
            tier_tokens: Default::default(),
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
            blocked: false,
            run: None,
            trial: None,
            trial_group: None,
            dir: None,
            hand: false,
            project: "demo".into(),
        }
    }

    /// The top border's right side names the project and, once one is set,
    /// each filter and the window.
    #[test]
    fn filters_label_names_the_project_every_filter_and_the_window() {
        let filters = Filters {
            since: "2026-08-01".to_string(),
            step: Some("review".into()),
            skill: Some("/x".into()),
            ..no_filters()
        };
        assert_eq!(
            filters_label("demo", &filters, TableKind::Lanes, None),
            "demo · step review · 2026-08-01 → now"
        );
        assert_eq!(
            filters_label("demo", &filters, TableKind::Dirs, None),
            "demo · skill /x · 2026-08-01 → now"
        );
    }

    /// This project alone is left unnamed in the top border; any other
    /// ledger is named.
    #[test]
    fn the_scope_is_named_only_when_it_is_not_this_project() {
        assert_eq!(Scope::Mine.label(), "");
        assert_eq!(Scope::All.label(), "all projects");
        assert_eq!(filters_label("", &no_filters(), TableKind::Lanes, None), "");
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
        let (repo, _root_guard) = fixture("screen-empty");
        let text = screen(&repo, "");
        assert!(text.contains("Nothing to compare"), "{text}");
    }

    /// The screen opens on the lanes table by pipeline, on its totals, with
    /// its `Total` line and the bracketed key line naming the figures `t`
    /// switches to and the table `tab` moves to.
    #[test]
    fn the_screen_opens_by_pipeline_with_a_total_and_bracketed_keys() {
        let (repo, _root_guard) = fixture_with_one_run("screen-opens");
        let text = screen(&repo, "q");
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by pipeline · totals "), "{last}");
        assert!(last.contains("│ PIPELINE"), "{last}");
        assert!(last.contains("│>default"), "{last}");
        assert!(last.contains("│ Total"), "{last}");
        assert!(
            last.contains(
                "[↑↓] move   [a] ascending   [d] descending   [t] per run   [tab] dirs   [f] filters   [e] export   [r] refresh   [q] quit"
            ),
            "{last}"
        );
    }

    /// `tab` cycles the runs, the directories and the trials and back,
    /// nothing else.
    #[test]
    fn tab_cycles_the_runs_the_directories_and_the_trials() {
        let (repo, _root_guard) = fixture_with_one_run("screen-tab");
        let text = screen(&repo, "\t");
        let dirs = last_frame(&text);
        assert!(dirs.contains("┌─ eval · by dir "), "{dirs}");
        assert!(dirs.contains("│ DIR"), "{dirs}");
        assert!(dirs.contains("[tab] trials"), "{dirs}");
        let root = repo.root.canonicalize().unwrap();
        assert!(
            dirs.contains(&*root.to_string_lossy()),
            "the project's own directory is a row even with no sessions\n{dirs}"
        );

        let trials = last_frame(&screen(&repo, "\t\t")).to_string();
        assert!(trials.contains("┌─ eval · trials "), "{trials}");
        assert!(trials.contains("No trials in this window."), "{trials}");
        assert!(
            trials.contains(
                "[↑↓] move   [enter] open   [a] ascending   [d] descending   [tab] runs   [f] filters   [r] refresh   [q] quit"
            ),
            "{trials}"
        );

        let text = screen(&repo, "\t\t\t");
        assert!(
            last_frame(&text).contains("┌─ eval · by pipeline "),
            "{text}"
        );
    }

    /// `e` exports the table on screen to a file named after its `by`, and
    /// the confirmation names the rows and the file it wrote.
    #[test]
    fn e_exports_the_table_to_a_file_named_after_its_by() {
        let (repo, _root_guard) = fixture_with_one_run("screen-export-key");
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
        let (repo, _root_guard) = fixture("export-dirs-and-sessions");
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
            body.contains(
                "dir,spoolway,1,0,10,0,0,0,10,0,0,0.00,0.00,0.00,0.00,0.00,0.00,0.00,0.00,0.70,0.70,"
            ),
            "{body}"
        );

        filters.dir_by = DirBy::Session;
        let (path, rows) = export(&repo, &loaded, &filters, &pipelines, TableKind::Dirs).unwrap();
        assert_eq!(rows, 1);
        assert!(path.to_string_lossy().contains("eval-by-session-"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with(SESSIONS_CSV_HEADER), "{body}");
        assert!(
            body.contains(",spoolway,—,claude-opus-5,0,10,0,0,0.00,0.00,0.00,0.00,0.70,"),
            "{body}"
        );
        assert!(body.contains("\ntotal,"), "{body}");
    }

    /// Nothing on the filter panel is typed into, so backspace has no row to
    /// erase from: it must fall through and do nothing, never reach
    /// `field_text_mut`'s `unreachable!()`.
    #[test]
    fn backspace_on_the_filter_panel_does_nothing_rather_than_panicking() {
        let (repo, _root_guard) = fixture("screen-backspace");
        screen(&repo, "f\x7fq");
    }

    /// A small ledger draws a table only a few lines tall — shorter than
    /// either overlay panel. `draw` has to pad the frame's own body out to
    /// fit whichever panel is on top of it, or `overlay` writes past the
    /// frame's last row and the panel loses its own bottom border.
    #[test]
    fn a_short_table_still_fits_the_taller_overlay_panels_whole() {
        let (repo, _root_guard) = fixture_with_one_run("screen-short-table");
        let last = last_frame(&screen(&repo, "fq")).to_string();
        assert!(last.contains("[enter] apply   [esc] back"), "{last}");
        assert!(last.contains("└───"), "{last}");
    }

    /// The lanes panel draws its seven rows in order, `by` first, every one
    /// blank when the screen opens, and the directory panel its five. A
    /// chevron is drawn only where its key still moves the row, a space
    /// holding its place otherwise.
    #[test]
    fn the_filter_panels_draw_their_rows_by_first_every_one_blank() {
        let panel_rows = |loaded: &Loaded, draft: &Draft| -> Vec<String> {
            filter_panel(loaded, draft)[1..=filter_fields(draft.table).len()]
                .iter()
                .map(|l| {
                    let inner = l.trim_matches('│').trim_end();
                    inner.strip_prefix("  ").unwrap_or(inner).to_string()
                })
                .collect()
        };
        let lanes = loaded(vec![tests_entry("a", "implement")]);
        let draft = Draft::new(TableKind::Lanes, no_filters());
        assert_eq!(
            panel_rows(&lanes, &draft),
            [
                "> by          pipeline ›",
                "  pipeline    all ›",
                "  step        all ›",
                "  version     all ›",
                "  trial       all",
                "  since     (blank — the start)",
                "  until     (blank — now)",
            ]
        );
        let text = filter_panel(&lanes, &draft).join("\n");
        for hint in FILTER_HINTS {
            assert!(text.contains(hint), "{hint}\n{text}");
        }
        assert!(!text.contains("group"), "{text}");
        assert!(!text.contains("task"), "{text}");

        // One `→` on `by`, then the last entry; the pipeline row on its only
        // candidate; and a row with nothing to pick from at all.
        let mut draft = Draft::new(TableKind::Lanes, no_filters());
        draft.filters.by = EvalBy::Step;
        draft.filters.pipeline = Some("default".into());
        let rows = panel_rows(&lanes, &draft);
        assert_eq!(rows[0], "> by        ‹ step ›");
        assert_eq!(rows[1], "  pipeline  ‹ default");
        assert_eq!(rows[2], "  step        all ›");
        let rows = panel_rows(
            &loaded(Vec::new()),
            &Draft::new(TableKind::Lanes, no_filters()),
        );
        assert_eq!(rows[1], "  pipeline    all");
        draft.filters.by = EvalBy::Task;
        assert_eq!(panel_rows(&lanes, &draft)[0], "> by        ‹ task");

        let draft = Draft::new(TableKind::Dirs, no_filters());
        let text = filter_panel(&loaded(Vec::new()), &draft).join("\n");
        for row in [
            "> by          dir ›",
            "dir         all  ",
            "skill       all  ",
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
        let lanes = loaded(vec![tests_entry("a", "implement")]);
        let widths: Vec<usize> = (0..filter_fields(TableKind::Lanes).len())
            .flat_map(|field| {
                let mut draft = Draft::new(TableKind::Lanes, no_filters());
                draft.field = field;
                filter_panel(&lanes, &draft)
            })
            .map(|line| line.chars().count())
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
        let (repo, _root_guard) = fixture_with_one_run("screen-by-step");
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
        let (repo, _root_guard) = fixture_with_one_run("screen-filter");
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
        let text = screen(&repo, &format!("f{DOWN}{RIGHT}\rq"));
        let last = last_frame(&text);
        assert!(last.contains("pipeline default"), "{last}");
        assert!(!last.contains("other"), "{last}");
    }

    /// The calendar still opens from the date rows, now the sixth and
    /// seventh: `esc` hands the row back untouched, `enter` picks the day it
    /// opened on, and `x` clears it.
    #[test]
    fn esc_leaves_the_row_untouched_enter_picks_the_day_x_clears_it() {
        let (repo, _root_guard) = fixture("screen-calendar-roundtrip");
        let today = chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string();
        let to_since = format!("f{}", DOWN.repeat(5));

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
        let (repo, _root_guard) = fixture_with_one_run("screen-short-table-calendar");
        let text = screen(&repo, &format!("f{}\rq", DOWN.repeat(5)));
        let last = last_frame(&text);
        assert!(last.contains("┌─ since ─"), "{last}");
        assert!(last.contains("[enter] pick   [esc] back"), "{last}");
    }

    /// Nothing on the date rows is typed into: a run of ordinary characters
    /// on the `since` row does not touch its value, and `enter` still opens
    /// the calendar.
    #[test]
    fn typing_on_a_date_row_does_nothing_and_enter_still_opens_the_calendar() {
        let (repo, _root_guard) = fixture("screen-bad-since");
        let text = screen(&repo, &format!("f{}notadate\rq", DOWN.repeat(5)));
        assert!(last_frame(&text).contains("┌─ since ─"), "{text}");
        assert!(!text.contains("notadate"), "{text}");
    }

    /// A directory line, written straight to `repo`'s own ledger — the same
    /// shape `usage::sweep`'s directory walk banks, skipping the walk.
    fn bank_dir(repo: &Repo, ts: &str, dir: &str, session: &str, cost: f64) {
        crate::usage::append(
            repo,
            &Entry {
                tier_tokens: Default::default(),
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
                blocked: false,
                run: None,
                trial: None,
                trial_group: None,
                dir: Some(dir.to_string()),
                hand: false,
                project: String::new(),
            },
        )
        .unwrap();
    }

    /// A `.claude/projects/<escaped>/<session>.jsonl` transcript under a
    /// fresh scratch home, so `skill_markers` has something real to read.
    fn claude_home_with(name: &str, session: &str, lines: &str) -> crate::scratch::ScratchRoot {
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
        let (repo, _root_guard) = fixture("screen-dirs-and-skill");
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

    /// A skill named through the Skill tool writes no leading slash, unlike
    /// a typed `<command-name>` — see `normalize_skill`. The `skill` filter
    /// must still keep the session that ran it, and the candidate the `→` key
    /// cycles to must be the one already on screen, not a second value next
    /// to it.
    #[test]
    fn a_skill_run_through_the_skill_tool_is_kept_by_the_same_filter_a_typed_command_is() {
        let (repo, _root_guard) = fixture("screen-dirs-and-skill-tool");
        bank_dir(
            &repo,
            "2026-09-01T09:00:00+00:00",
            "/w/spoolway",
            "s1",
            0.70,
        );
        bank_dir(&repo, "2026-09-02T09:00:00+00:00", "/w/notes", "s2", 0.18);
        let lines = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-09-01T09:05:00.000Z",
                "message": {
                    "model": "claude-opus-5",
                    "usage": {"input_tokens": 2, "output_tokens": 10},
                    "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "Skill", "input": {"skill": "spoolway-plan"}}
                    ]
                }
            }),
            serde_json::json!({
                "type": "user",
                "isMeta": true,
                "timestamp": "2026-09-01T09:05:01.000Z",
                "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1"}]},
                "sourceToolUseID": "toolu_1"
            }),
        );
        let home = claude_home_with("skill-tool-filter", "s1", &lines);

        let text = crate::platform::test_home::with_home(&home, || {
            screen(&repo, &format!("\tf{DOWN}{DOWN}{RIGHT}\rq"))
        });
        let filtered = last_frame(&text);
        assert!(filtered.contains("┌─ eval · by dir "), "{filtered}");
        assert!(filtered.contains("skill spoolway-plan ─┐"), "{filtered}");
        assert!(filtered.contains("/w/spoolway"), "{filtered}");
        assert!(!filtered.contains("/w/notes"), "{filtered}");

        std::fs::remove_dir_all(&home).ok();
    }

    /// The same skill named both ways on one session — a typed `/spoolway-plan`
    /// and a Skill `tool_use` for `spoolway-plan` — must read as one skill
    /// throughout: one candidate for the `skill` row to cycle to, and one
    /// name in the `SKILL` cell, not a `+1` for a "second" skill that is
    /// really the same one spelled without its slash.
    #[test]
    fn a_skill_named_both_ways_on_one_session_is_one_skill_everywhere() {
        let (repo, _root_guard) = fixture("screen-dirs-skill-both-forms");
        bank_dir(
            &repo,
            "2026-09-01T09:00:00+00:00",
            "/w/spoolway",
            "s1",
            0.70,
        );
        let lines = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-09-01T09:00:00.000Z",
                "message": {"role": "user", "content":
                    "<command-name>/spoolway-plan</command-name>\n<command-args></command-args>"},
            }),
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-09-01T09:05:00.000Z",
                "message": {
                    "model": "claude-opus-5",
                    "usage": {"input_tokens": 2, "output_tokens": 10},
                    "content": [
                        {"type": "tool_use", "id": "toolu_1", "name": "Skill", "input": {"skill": "spoolway-plan"}}
                    ]
                }
            }),
        );
        let home = claude_home_with("both-forms", "s1", &lines);

        // `tab`, `f`, `→` on `by` to `session`.
        let text = crate::platform::test_home::with_home(&home, || {
            screen(&repo, &format!("\tf{RIGHT}\rq"))
        });
        let sessions = last_frame(&text);
        assert!(sessions.contains("SKILL"), "{sessions}");
        assert!(sessions.contains("spoolway-plan"), "{sessions}");
        assert!(
            !sessions.contains("spoolway-plan +1"),
            "one skill named two ways must not count as two: {sessions}"
        );

        // `tab`, `f`, down twice to `skill`: exactly one candidate to cycle to.
        let text = crate::platform::test_home::with_home(&home, || {
            screen(&repo, &format!("\tf{DOWN}{DOWN}{RIGHT}{RIGHT}\rq"))
        });
        let filtered = last_frame(&text);
        assert!(filtered.contains("skill /spoolway-plan ─┐"), "{filtered}");

        std::fs::remove_dir_all(&home).ok();
    }

    /// A subagent's transcript sits under `<parent-session>/subagents/`, with
    /// no session of its own on screen — see `usage::parent_session`. Its
    /// spend must land on the parent's own row, not a separate `agent-<id>`
    /// one with no skill and no time, and the parent's `SKILL` cell must
    /// include what the subagent ran.
    #[test]
    fn a_subagents_spend_and_skills_land_on_its_parents_row() {
        let (repo, _root_guard) = fixture("screen-dirs-and-subagent");
        let parent = "0198e2c0-3333-4000-8000-00000000d020";
        let subagent = "agent-9988aabb";
        bank_dir(
            &repo,
            "2026-09-01T09:00:00+00:00",
            "/w/spoolway",
            parent,
            0.50,
        );
        bank_dir(
            &repo,
            "2026-09-01T09:05:00+00:00",
            "/w/spoolway",
            subagent,
            0.20,
        );

        let root = crate::scratch::root("eval-subagent-fold");
        let project = root.join(".claude/projects/-home-someone-work");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{parent}.jsonl")), skill_transcript()).unwrap();
        let subagents_dir = project.join(parent).join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        let subagent_lines = format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-09-01T09:05:10.000Z",
                "message": {
                    "model": "claude-opus-5",
                    "usage": {"input_tokens": 2, "output_tokens": 10},
                    "content": [
                        {"type": "tool_use", "id": "toolu_2", "name": "Skill", "input": {"skill": "spoolway-tasks"}}
                    ]
                }
            }),
        );
        std::fs::write(
            subagents_dir.join(format!("{subagent}.jsonl")),
            &subagent_lines,
        )
        .unwrap();

        let text = crate::platform::test_home::with_home(&root, || {
            screen(&repo, &format!("\tf{RIGHT}\rq"))
        });
        let sessions = last_frame(&text);
        assert!(sessions.contains("┌─ eval · by session "), "{sessions}");
        assert!(!sessions.contains(subagent), "{sessions}");
        assert!(
            sessions.contains("0.70"),
            "the subagent's cost must be on its parent's row: {sessions}"
        );
        assert!(
            sessions.contains("spoolway-plan +1"),
            "the parent's SKILL cell must fold in what its subagent ran: {sessions}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A window or filter can keep a subagent's own ledger line while
    /// dropping its parent's — the parent itself may have run before
    /// `since`, with the subagent running (and so being re-banked) later.
    /// The fold in `load` still renames that subagent's row onto the parent,
    /// but the parent never earned a row of its own in the pre-fold `dirs`
    /// that `load`'s batched read is seeded from — see the task's own
    /// account of this gap. The parent's own skills and span must still show
    /// up on the folded row, read on the side rather than silently dropped.
    #[test]
    fn a_parent_s_own_skills_and_span_survive_when_only_its_subagent_is_in_window() {
        let (repo, _root_guard) = fixture("screen-dirs-window-excludes-parent-ledger-line");
        let parent = "0198e2c0-4444-4000-8000-00000000d030";
        let subagent = "agent-aabbccdd";
        bank_dir(
            &repo,
            "2026-08-30T09:00:00+00:00",
            "/w/spoolway",
            parent,
            0.50,
        );
        bank_dir(
            &repo,
            "2026-09-01T09:05:00+00:00",
            "/w/spoolway",
            subagent,
            0.20,
        );

        let root = crate::scratch::root("eval-subagent-fold-window-gap");
        let project = root.join(".claude/projects/-home-someone-work");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{parent}.jsonl")), skill_transcript()).unwrap();
        let subagents_dir = project.join(parent).join("subagents");
        std::fs::create_dir_all(&subagents_dir).unwrap();
        let subagent_lines = format!(
            "{}\n",
            serde_json::json!({
                "type": "assistant",
                "timestamp": "2026-09-01T09:05:10.000Z",
                "message": {
                    "model": "claude-opus-5",
                    "usage": {"input_tokens": 2, "output_tokens": 10},
                    "content": [
                        {"type": "tool_use", "id": "toolu_3", "name": "Skill", "input": {"skill": "other-skill"}}
                    ]
                }
            }),
        );
        std::fs::write(
            subagents_dir.join(format!("{subagent}.jsonl")),
            &subagent_lines,
        )
        .unwrap();

        let filters = Filters {
            since: "2026-09-01".to_string(),
            ..no_filters()
        };
        let loaded =
            crate::platform::test_home::with_home(&root, || load(&repo, &filters).unwrap());

        assert_eq!(
            loaded
                .skills_by_session
                .get(parent)
                .cloned()
                .unwrap_or_default(),
            BTreeSet::from(["/spoolway-plan".to_string(), "other-skill".to_string()]),
            "the parent's own skills must survive even though only its subagent's ledger \
             line is in the window: {:?}",
            loaded.skills_by_session
        );
        assert!(
            loaded.spans_by_session.contains_key(parent),
            "the parent's own span must still be read even though its own ledger line is \
             outside the window: {:?}",
            loaded.spans_by_session
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// One session's own transcript, plus its one subagent's, sized to make a
    /// transcript read and a directory walk show up in a timing, not just a
    /// correctness check.
    fn perf_lines(tag: &str, n_lines: usize) -> String {
        let mut out = String::new();
        for j in 0..n_lines {
            out += &format!(
                "{}\n",
                serde_json::json!({
                    "type": "user",
                    "timestamp": format!("2026-09-01T09:{:02}:{:02}.000Z", (j / 60) % 60, j % 60),
                    "message": {"role": "user", "content": format!(
                        "<command-name>/perf-{tag}-{j}</command-name>\n<command-args></command-args>"
                    )},
                }),
            );
        }
        out
    }

    /// `n` parent sessions, each with one subagent, under one project
    /// directory — the shape `load` walks and reads once per session for
    /// skill markers and span, and once more per subagent. Each session and
    /// its subagent are also banked as a `dir` ledger line, so they are both
    /// rows `load` has to resolve a transcript for.
    fn many_sessions_home(
        name: &str,
        repo: &Repo,
        n: usize,
        lines_per_session: usize,
    ) -> crate::scratch::ScratchRoot {
        let root = crate::scratch::root(&format!("eval-load-perf-{name}"));
        let project = root.join(".claude/projects/-nonsense-escaping-nobody-should-read");
        std::fs::create_dir_all(&project).unwrap();
        for i in 0..n {
            let session = format!("perf-{i:04}");
            std::fs::write(
                project.join(format!("{session}.jsonl")),
                perf_lines(&session, lines_per_session),
            )
            .unwrap();
            let subagents_dir = project.join(&session).join("subagents");
            std::fs::create_dir_all(&subagents_dir).unwrap();
            let subagent = format!("agent-{i:04}");
            std::fs::write(
                subagents_dir.join(format!("{subagent}.jsonl")),
                perf_lines(&subagent, lines_per_session),
            )
            .unwrap();

            let ts = format!("2026-09-01T09:{:02}:{:02}+00:00", (i / 60) % 60, i % 60);
            bank_dir(repo, &ts, "/w/perf", &session, 0.01);
            bank_dir(repo, &ts, "/w/perf", &subagent, 0.01);
        }
        root
    }

    /// `load` must locate and read each session's transcript once, and each
    /// subagent's once, not several times over and not by scanning the whole
    /// `dirs` list for every session — see the task's own account of `load`'s
    /// per-session `kind` lookup and subagent fold, and of `skill_markers`,
    /// `session_span` and `parent_session` each doing their own directory
    /// walk and file read.
    ///
    /// Pinned by comparing `load`'s time over `SMALL_N` sessions against
    /// `SCALE` times as many, rather than by an absolute duration, so the
    /// bound holds whatever the machine's own speed is. A linear read cost
    /// takes about `SCALE` times as long; the repeated walks and the O(n)
    /// `kind` lookup and subagent fold this task describes multiply that by
    /// `SCALE` again, so `BOUND` sits well below `SCALE * SCALE` and well
    /// above `SCALE` on its own.
    #[test]
    fn load_reads_each_transcript_once_so_its_time_grows_about_linearly_with_sessions() {
        const SMALL_N: usize = 60;
        const SCALE: usize = 4;
        const LARGE_N: usize = SMALL_N * SCALE;
        const LINES_PER_SESSION: usize = 20;
        const BOUND: f64 = 8.0;

        let (repo_small, _small_guard) = fixture("eval-load-perf-small");
        let home_small = many_sessions_home("small", &repo_small, SMALL_N, LINES_PER_SESSION);
        let elapsed_small = crate::platform::test_home::with_home(&home_small, || {
            let start = std::time::Instant::now();
            let loaded = load(&repo_small, &no_filters()).unwrap();
            assert_eq!(loaded.dirs.len(), SMALL_N * 2);
            start.elapsed()
        });

        let (repo_large, _large_guard) = fixture("eval-load-perf-large");
        let home_large = many_sessions_home("large", &repo_large, LARGE_N, LINES_PER_SESSION);
        let elapsed_large = crate::platform::test_home::with_home(&home_large, || {
            let start = std::time::Instant::now();
            let loaded = load(&repo_large, &no_filters()).unwrap();
            assert_eq!(loaded.dirs.len(), LARGE_N * 2);
            start.elapsed()
        });

        std::fs::remove_dir_all(&home_small).ok();
        std::fs::remove_dir_all(&home_large).ok();

        let ratio = elapsed_large.as_secs_f64() / elapsed_small.as_secs_f64().max(f64::EPSILON);
        assert!(
            ratio < BOUND,
            "load took {elapsed_small:?} for {SMALL_N} sessions and {elapsed_large:?} for \
             {LARGE_N} sessions ({SCALE}x as many) — a {ratio:.1}x slowdown means load is \
             rescanning the whole session list per session and re-reading each transcript \
             several times over, not once; the bound is {BOUND}x, well under the \
             {}x a quadratic read pattern would cost",
            SCALE * SCALE,
        );
    }

    /// A second `load` over the same `repo` with nothing changed on disk
    /// must not pay to read every transcript again — see the task's own
    /// account of `[r]` and the filter panel's `enter`, both of which call
    /// `load` again through the very same `repo` the first call used.
    /// Counted rather than timed — see `crate::usage::session_reads_under` —
    /// because a second load racing the rest of the suite for the same cores
    /// could come in slower than half the first while reading nothing at all.
    ///
    /// Also checks the two cases a cache has to get right rather than just
    /// go fast: a transcript that changed since the last load is read again
    /// and its new skill shows up, and a transcript removed since the last
    /// load no longer contributes a skill or a span at all.
    #[test]
    fn a_second_load_with_nothing_changed_rereads_no_transcript() {
        const N: usize = 120;
        const LINES_PER_SESSION: usize = 40;

        let (repo, _root_guard) = fixture("eval-load-reuse");
        let home = many_sessions_home("reuse", &repo, N, LINES_PER_SESSION);
        let project = home.join(".claude/projects/-nonsense-escaping-nobody-should-read");

        let (first, second, changed, deleted_gone) =
            crate::platform::test_home::with_home(&home, || {
                let before = crate::usage::session_reads_under(&home);
                let loaded = load(&repo, &no_filters()).unwrap();
                assert_eq!(loaded.dirs.len(), N * 2);
                let first = crate::usage::session_reads_under(&home) - before;

                let before = crate::usage::session_reads_under(&home);
                let loaded = load(&repo, &no_filters()).unwrap();
                assert_eq!(loaded.dirs.len(), N * 2);
                let second = crate::usage::session_reads_under(&home) - before;

                // perf-0000's transcript grows a new skill marker — its size
                // and mtime both move.
                std::fs::write(
                    project.join("perf-0000.jsonl"),
                    perf_lines("perf-0000", LINES_PER_SESSION) + &perf_lines("new", 1),
                )
                .unwrap();
                // perf-0001's own transcript and its one subagent's are both
                // gone entirely, though their ledger lines are still there —
                // see `bank_dir` in `many_sessions_home`.
                std::fs::remove_file(project.join("perf-0001.jsonl")).unwrap();
                std::fs::remove_dir_all(project.join("perf-0001")).unwrap();

                let loaded = load(&repo, &no_filters()).unwrap();
                let changed = loaded
                    .skills_by_session
                    .get("perf-0000")
                    .is_some_and(|skills| skills.contains("/perf-new-0"));
                let deleted_gone = loaded
                    .skills_by_session
                    .get("perf-0001")
                    .is_none_or(BTreeSet::is_empty)
                    && !loaded.spans_by_session.contains_key("perf-0001");
                (first, second, changed, deleted_gone)
            });

        std::fs::remove_dir_all(&home).ok();

        assert!(
            first >= N * 2,
            "the first load read only {first} transcripts, fewer than the {} sessions and \
             subagents it has rows for",
            N * 2,
        );
        assert_eq!(
            second, 0,
            "a second load with nothing changed on disk read {second} transcripts again, \
             against {first} for the first — it should have reused every one it already read",
        );
        assert!(
            changed,
            "perf-0000's new skill marker did not show up after it changed"
        );
        assert!(
            deleted_gone,
            "perf-0001's skills or span survived its transcript being deleted"
        );
    }

    /// A notice's own overlay must never draw wider than the frame it sits
    /// on — `overlay` cannot place a panel that does not fit. This is the
    /// exact message an incomplete `--since` produces, one long sentence
    /// with no newline of its own, checked at the frame's narrowest floor
    /// and at two real widths.
    #[test]
    fn a_long_notice_wraps_to_fit_the_frame_at_any_width() {
        let err = crate::spend::window_of(Some("2026-08-0"), None).unwrap_err();
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

    // -------------------------------------------------------------- sorting

    /// One `default` pipeline lane of `task`'s own run, priced and passed —
    /// for a test to adjust before it banks or loads it.
    fn lane_entry(task: &str, step: &str) -> Entry {
        Entry {
            tier_tokens: Default::default(),
            ts: "2026-08-01T09:00:00+00:00".into(),
            task: task.into(),
            plan: None,
            step: step.into(),
            pipeline: "default".into(),
            agent: "pi".into(),
            kind: "pi".into(),
            model: "qwen".into(),
            session: format!("{task}-{step}"),
            round: 1,
            wall_s: 60,
            turns: 1,
            tokens: Tokens::default(),
            cost_usd: Some(1.0),
            ctx_peak: None,
            pipeline_version: "1.0".into(),
            outcome: Some("pass".into()),
            blocked: false,
            run: Some(format!("r-{task}")),
            trial: None,
            trial_group: None,
            dir: None,
            hand: false,
            project: String::new(),
        }
    }

    /// Three pipelines with three pass rates, banked so the default order
    /// — newest first — is `alpha`, `gamma`, `beta`, and each costs a
    /// different amount: `gamma` most, then `beta`, then `alpha`.
    fn fixture_to_sort(name: &str) -> (Repo, crate::scratch::ScratchRoot) {
        let (repo, root_guard) = fixture(name);
        let lanes = [
            ("2026-08-01T09:00:00+00:00", "b1", "beta", 2.0, "fail"),
            ("2026-08-01T10:00:00+00:00", "g1", "gamma", 3.0, "pass"),
            ("2026-08-01T10:30:00+00:00", "g2", "gamma", 3.0, "fail"),
            ("2026-08-01T11:00:00+00:00", "a1", "alpha", 1.0, "pass"),
        ];
        for (ts, task, pipeline, cost, outcome) in lanes {
            bank(
                &repo,
                ts,
                task,
                "implement",
                pipeline,
                "qwen",
                cost,
                Some(outcome),
            );
        }
        (repo, root_guard)
    }

    /// The pipelines a frame's lanes rows name, top to bottom.
    fn row_order(frame: &str) -> Vec<&'static str> {
        let mut at: Vec<(usize, &'static str)> = ["alpha", "beta", "gamma"]
            .into_iter()
            .filter_map(|p| {
                frame
                    .find(&format!(">{p} "))
                    .or_else(|| frame.find(&format!("│ {p} ")))
                    .map(|i| (i, p))
            })
            .collect();
        at.sort();
        at.into_iter().map(|(_, p)| p).collect()
    }

    /// `a` opens the popup on `default order`, `↓` three times walks to
    /// `PASS`, and `enter` sorts on it: the table reorders on the raw pass
    /// rate, the header marks `PASS`, and the cursor is on the first row.
    #[test]
    fn a_then_a_column_sorts_the_table_and_marks_its_header() {
        let (repo, _root_guard) = fixture_to_sort("screen-sort-pick");
        let text = screen(&repo, &format!("{DOWN}a{DOWN}{DOWN}{DOWN}\rq"));
        let all = frames(&text);

        let popup = all
            .iter()
            .rev()
            .find(|f| f.contains("┌─ sort ascending "))
            .expect("`a` opened the popup");
        let listed: Vec<usize> = [
            "│    default order",
            "│    PIPELINE",
            "│    RUNS",
            "│  > PASS",
            "│    TIME ",
        ]
        .iter()
        .map(|line| {
            popup
                .find(line)
                .unwrap_or_else(|| panic!("{line}: {popup}"))
        })
        .collect();
        assert!(
            listed.is_sorted(),
            "default order first, then the columns in order: {popup}"
        );
        assert!(
            popup.contains("[↑↓] column   [enter] sort   [esc] back"),
            "{popup}"
        );

        let before = all[0..all.len() - 1]
            .iter()
            .rev()
            .find(|f| !f.contains("┌─ sort ") && f.contains("│ PIPELINE"))
            .unwrap();
        assert_eq!(row_order(before), ["alpha", "gamma", "beta"], "{before}");

        let last = last_frame(&text);
        assert_eq!(
            row_order(last),
            ["beta", "gamma", "alpha"],
            "0%, 50%, 100%: {last}"
        );
        assert!(
            last.contains("│>beta "),
            "the cursor is on the first row: {last}"
        );
        assert!(last.contains(" ▲PASS  BLOCKS"), "{last}");
        assert!(
            !last.contains("┌─ sort "),
            "`enter` closed the popup: {last}"
        );
    }

    /// `esc` closes the popup and changes nothing.
    #[test]
    fn esc_closes_the_sort_popup_without_sorting() {
        let (repo, _root_guard) = fixture_to_sort("screen-sort-esc");
        let text = screen(&repo, &format!("d{DOWN}\x1bq"));
        let last = last_frame(&text);
        assert_eq!(row_order(last), ["alpha", "gamma", "beta"], "{last}");
        assert!(!last.contains('▼') && !last.contains("┌─ sort "), "{last}");
    }

    /// A sort on `USD` outlives `tab` round every table, `r`, and a change of
    /// `by` on the filter panel — and the directory table, which has its
    /// own sort, is left in its default order meanwhile.
    #[test]
    fn a_sort_survives_tab_refresh_and_a_new_by_and_each_table_keeps_its_own() {
        let (repo, _root_guard) = fixture_to_sort("screen-sort-survives");
        // `USD` is the fifteenth column under `by pipeline`.
        let to_usd = DOWN.repeat(15);
        let text = screen(&repo, &format!("d{to_usd}\r\t\t\trf{RIGHT}\rq"));
        let all = frames(&text);
        let sorted = |f: &str| f.contains("▼USD");

        let dirs = all
            .iter()
            .find(|f| f.contains("┌─ eval · by dir "))
            .expect("`tab` reached the directory table");
        assert!(
            !dirs.contains('▼'),
            "the directory table keeps its own sort: {dirs}"
        );

        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by step "), "{last}");
        assert!(sorted(last), "{last}");
        let lanes_frames: Vec<&&str> = all
            .iter()
            .filter(|f| f.contains("┌─ eval · by pipeline ") || f.contains("┌─ eval · by step "))
            .filter(|f| !shows_loading(f) && !f.contains("┌─ filters") && !f.contains("┌─ sort "))
            .collect();
        let after_pick = lanes_frames
            .iter()
            .position(|f| sorted(f))
            .expect("the sort took");
        assert!(
            lanes_frames[after_pick..].iter().all(|f| sorted(f)),
            "every lanes frame after the pick is still sorted"
        );
        assert_eq!(
            row_order(last),
            ["gamma", "beta", "alpha"],
            "6.00, 2.00, 1.00: {last}"
        );
    }

    /// A view that does not draw the sorted column shows the default order
    /// and no mark — and the sort comes back with a view that does.
    #[test]
    fn a_view_without_the_sorted_column_falls_back_to_the_default_order() {
        let mut entries = Vec::new();
        for (i, step) in ["implement", "review", "document"].into_iter().enumerate() {
            let mut entry = lane_entry("t", step);
            entry.ts = format!("2026-08-01T0{i}:00:00+00:00");
            entries.push(entry);
        }
        let loaded = loaded(entries);
        let pipelines = Pipelines::builtin();
        let mut filters = no_filters();
        filters.by = EvalBy::Step;
        filters.lane_sort = Some(Sort {
            key: "step".into(),
            descending: false,
        });
        let header = |filters: &Filters| match &view_lines(
            &loaded,
            filters,
            &pipelines,
            TableKind::Lanes,
        )[0]
        {
            Line::Head(lead, text) => format!("{lead}{text}"),
            _ => panic!("a table opens on its header"),
        };
        assert!(header(&filters).contains("▲STEP"));
        let (rows, _) = screen_lane_rows(
            &loaded,
            &filters,
            &pipelines,
            &scoped_entries(&loaded, &filters),
        );
        assert_eq!(
            rows.iter().map(|r| r.cells[1].as_str()).collect::<Vec<_>>(),
            ["document", "implement", "review"]
        );

        filters.by = EvalBy::Pipeline;
        assert!(!header(&filters).contains('▲'), "no STEP column to sort by");
        filters.by = EvalBy::Step;
        assert!(
            header(&filters).contains("▲STEP"),
            "the sort was set aside, not cleared"
        );
    }

    /// `e` writes the rows in the order the screen shows them, on both
    /// tables, the `Total` line still last.
    #[test]
    fn e_writes_rows_in_the_order_shown() {
        let (repo, _root_guard) = fixture_to_sort("screen-sort-export");
        bank_dir(
            &repo,
            "2026-09-01T09:00:00+00:00",
            "spoolway",
            "cheap",
            0.10,
        );
        bank_dir(&repo, "2026-09-01T10:00:00+00:00", "spoolway", "dear", 0.90);
        let mut filters = no_filters();
        let loaded = load(&repo, &filters).unwrap();
        let pipelines = Pipelines::builtin();

        filters.lane_sort = Some(Sort {
            key: "pass".into(),
            descending: false,
        });
        let (_, lines, rows) = export_rows(&loaded, &filters, &pipelines, TableKind::Lanes);
        assert_eq!(rows, 3);
        let named: Vec<&str> = lines.iter().map(|l| l.split(',').nth(2).unwrap()).collect();
        assert_eq!(named, ["beta", "gamma", "alpha", ""], "{lines:?}");
        assert!(lines[3].starts_with(",total,"), "{lines:?}");

        filters.dir_by = DirBy::Session;
        filters.dir_sort = Some(Sort {
            key: "cost_usd".into(),
            descending: false,
        });
        let (_, lines, _) = export_rows(&loaded, &filters, &pipelines, TableKind::Dirs);
        assert!(
            lines[0].contains(",0.10,") && lines[1].contains(",0.90,"),
            "{lines:?}"
        );
        assert!(lines[2].starts_with("total,"), "{lines:?}");
        filters.dir_sort = Some(Sort {
            key: "cost_usd".into(),
            descending: true,
        });
        let (_, lines, _) = export_rows(&loaded, &filters, &pipelines, TableKind::Dirs);
        assert!(
            lines[0].contains(",0.90,") && lines[1].contains(",0.10,"),
            "{lines:?}"
        );
    }

    /// `spoolway eval` with `argv`, against `repo`, as it would print.
    fn print(repo: &Repo, argv: &[&str]) -> Result<String> {
        use clap::Parser;
        let full = ["spoolway", "eval"].iter().chain(argv);
        let crate::cli::Command::Eval(args) = crate::cli::Cli::try_parse_from(full)?.command else {
            panic!("parsed as `eval`");
        };
        let mut out = Vec::new();
        run_to(repo, &args, false, Some(&Pipelines::builtin()), &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    /// `--sort pass:asc --csv` writes the rows the screen's `PASS` sort
    /// shows, `Total` last; left off, the direction is descending; a name
    /// the export does not carry is refused with the ones it does.
    #[test]
    fn sort_on_the_command_line_orders_the_csv() {
        let (repo, _root_guard) = fixture_to_sort("cli-sort-csv");
        let pipelines_of = |csv: &str| -> Vec<String> {
            csv.lines()
                .skip(1)
                .map(|l| l.split(',').nth(2).unwrap().to_string())
                .collect()
        };

        let csv = print(&repo, &["--sort", "pass:asc", "--csv"]).unwrap();
        assert_eq!(
            csv.lines().next(),
            Some(lanes_csv_header(EvalBy::Pipeline).as_str())
        );
        assert_eq!(pipelines_of(&csv), ["beta", "gamma", "alpha", ""], "{csv}");
        assert!(csv.lines().last().unwrap().starts_with(",total,"), "{csv}");

        let csv = print(&repo, &["--sort", "cost_usd", "--csv"]).unwrap();
        assert_eq!(pipelines_of(&csv), ["gamma", "beta", "alpha", ""], "{csv}");

        let table = print(&repo, &["--sort", "pass:asc"]).unwrap();
        assert!(table.contains(" ▲PASS  BLOCKS"), "{table}");

        let err = print(&repo, &["--sort", "usd", "--csv"])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no column `usd`") && err.contains("cost_usd, cost_per_run"),
            "{err}"
        );
    }

    /// `--json` keeps the sorted order in `rows`, with `total` apart.
    #[test]
    fn sort_on_the_command_line_orders_json_rows() {
        let (repo, _root_guard) = fixture_to_sort("cli-sort-json");
        let mut out = Vec::new();
        let args = EvalArgs {
            sort: Some("pass:asc".into()),
            ..no_args()
        };
        run_to(&repo, &args, true, Some(&Pipelines::builtin()), &mut out).unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let named: Vec<&str> = json["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["pipeline"].as_str().unwrap())
            .collect();
        assert_eq!(named, ["beta", "gamma", "alpha"]);
        assert_eq!(json["total"]["runs"], 4);
    }

    /// A sort can put a later arm on top; every delta line still reads
    /// against the arm that started first.
    #[test]
    fn a_trial_sorted_still_reads_each_delta_against_the_first_arm() {
        let (repo, _root_guard) = fixture("cli-sort-trial");
        for (ts, task, cost, outcome) in [
            ("2026-08-01T09:00:00+00:00", "alpha-1", 1.0, "pass"),
            ("2026-08-01T10:00:00+00:00", "beta-1", 5.0, "fail"),
            ("2026-08-01T11:00:00+00:00", "gamma-1", 3.0, "pass"),
        ] {
            let mut entry = lane_entry(task, "implement");
            entry.ts = ts.into();
            entry.cost_usd = Some(cost);
            entry.outcome = Some(outcome.into());
            entry.trial = Some("t1".into());
            crate::usage::append(&repo, &entry).unwrap();
        }
        let text = print(
            &repo,
            &["--by", "task", "--trial", "t1", "--sort", "cost_usd"],
        )
        .unwrap();
        let beta = text.find("\nbeta-1 ").expect("beta's row");
        let alpha = text.find("\nalpha-1 ").expect("alpha's row");
        assert!(beta < alpha, "the sort put the dearest arm on top: {text}");
        assert!(
            text.contains("beta-1 vs alpha-1: pass -100pp, cost +$4.00"),
            "{text}"
        );
        assert!(
            text.contains("gamma-1 vs alpha-1: pass +0pp, cost +$2.00"),
            "{text}"
        );
        assert!(!text.contains("vs beta-1"), "{text}");
        let lines: Vec<&str> = text.lines().filter(|l| l.contains(" vs ")).collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines[0].starts_with("beta-1"),
            "deltas follow the order shown: {text}"
        );
    }

    // --------------------------------------------------------------- trials

    const LEFT: &str = "\x1b[D";

    /// One arm's lane: `task` under `pipeline`, banked at `ts` as an arm of
    /// `trial` forked from `group`.
    fn arm(task: &str, step: &str, pipeline: &str, ts: &str, trial: &str, group: &str) -> Entry {
        Entry {
            ts: ts.into(),
            pipeline: pipeline.into(),
            plan: Some(format!("{group}-{pipeline}")),
            trial: Some(trial.into()),
            trial_group: Some(group.into()),
            ..lane_entry(task, step)
        }
    }

    /// Two trials and an ordinary lane: `t1`, `retire-worktree-root` on
    /// 2026-10-02, two arms over three lines; and `t2`, an older one whose
    /// line predates `trial_group` and so names its group by its own plan.
    fn two_trials() -> Vec<Entry> {
        let mut old = arm(
            "grace-1",
            "implement",
            "impl_tdd",
            "2026-09-30T09:00:00+00:00",
            "t2",
            "unused",
        );
        old.trial_group = None;
        old.plan = Some("board-step-grace-window".into());
        vec![
            old,
            lane_entry("ordinary", "implement"),
            arm(
                "rwr-1",
                "implement",
                "impl",
                "2026-10-02T09:00:00+00:00",
                "t1",
                "retire-worktree-root",
            ),
            arm(
                "rwr-1",
                "review",
                "impl",
                "2026-10-02T10:00:00+00:00",
                "t1",
                "retire-worktree-root",
            ),
            arm(
                "rwr-2",
                "implement",
                "impl_ui",
                "2026-10-02T09:05:00+00:00",
                "t1",
                "retire-worktree-root",
            ),
        ]
    }

    /// A trial is one row named by its source group, newest first by when
    /// it started; `ARMS` counts tasks, not lines; and `STATE` reads
    /// `running` only for a trial a queued task still carries.
    #[test]
    fn trial_rows_name_each_trial_by_its_source_group_newest_first() {
        let running = BTreeSet::from(["t2".to_string()]);
        let rows = trial_rows(&two_trials(), &running);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["t1", "t2"], "the ordinary lane is no trial");

        assert_eq!(rows[0].group, "retire-worktree-root");
        assert_eq!(rows[0].pipelines, ["impl", "impl_ui"]);
        assert_eq!(rows[0].arms, 2, "three lines, two arms");
        assert_eq!(rows[0].state(), "settled");
        assert_eq!(
            rows[0].name(),
            format!(
                "retire-worktree-root · {}",
                local_date("2026-10-02T09:00:00+00:00")
            )
        );

        assert_eq!(rows[1].group, "board-step-grace-window", "from its plan");
        assert_eq!(rows[1].state(), "running");

        let mut bare = lane_entry("x-1", "implement");
        bare.trial = Some("t3".into());
        assert_eq!(trial_rows(&[bare], &BTreeSet::new())[0].group, "t3");
    }

    /// The table draws the mockup's columns, and a pipeline list too wide
    /// for its column drops whole names behind `…`.
    #[test]
    fn the_trials_table_draws_group_when_pipelines_arms_and_state() {
        let rows = trial_rows(&two_trials(), &BTreeSet::new());
        let lines = trials_table(&rows, None);
        let Line::Head(' ', header) = &lines[0] else {
            panic!("a header first");
        };
        assert_eq!(
            header,
            "GROUP                    WHEN        PIPELINES           ARMS  STATE"
        );
        let Line::Row(first) = &lines[1] else {
            panic!("a row next");
        };
        assert_eq!(
            first.text,
            format!(
                "retire-worktree-root     {}  impl, impl_ui          2  settled",
                local_date("2026-10-02T09:00:00+00:00")
            )
        );
        assert_eq!(lines.len(), 3, "no `Total` line");

        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            pipelines_cell(&names(&["impl", "impl_tdd", "impl_ui"])),
            "impl, impl_tdd, …"
        );
        assert_eq!(
            pipelines_cell(&names(&["a_very_long_pipeline_name"])),
            "a_very_long_pipel…"
        );
    }

    /// The `trial` row sits after `version`, cycles the trials newest first
    /// by group and date, and `←` back past the first lands on `all`.
    #[test]
    fn the_trial_row_cycles_newest_first_by_group_and_date_and_all_clears_it() {
        let fields = filter_fields(TableKind::Lanes);
        let at = fields
            .iter()
            .position(|f| *f == FilterField::Trial)
            .expect("a trial row");
        assert_eq!(fields[at - 1], FilterField::Version);

        let loaded = loaded(two_trials());
        let mut draft = Draft::new(TableKind::Lanes, no_filters());
        draft.field = at;
        let row = |draft: &Draft| {
            filter_panel(&loaded, draft)[1 + at]
                .trim_matches('│')
                .trim()
                .to_string()
        };
        assert_eq!(row(&draft), "> trial       all ›");

        handle_filter_change(&loaded, &mut draft, true);
        assert_eq!(draft.filters.trial.as_deref(), Some("t1"));
        assert_eq!(
            row(&draft),
            format!(
                "> trial     ‹ retire-worktree-root · {} ›",
                local_date("2026-10-02T09:00:00+00:00")
            )
        );
        handle_filter_change(&loaded, &mut draft, true);
        assert_eq!(draft.filters.trial.as_deref(), Some("t2"));
        handle_filter_change(&loaded, &mut draft, true);
        assert_eq!(draft.filters.trial.as_deref(), Some("t2"), "clamped");

        handle_filter_change(&loaded, &mut draft, false);
        handle_filter_change(&loaded, &mut draft, false);
        assert_eq!(draft.filters.trial, None, "`all` clears it");
    }

    /// `trial` narrows the lanes table and names the trial by its group in
    /// the right border.
    #[test]
    fn a_set_trial_narrows_the_runs_and_shows_in_the_right_border() {
        let loaded = loaded(two_trials());
        let filters = Filters {
            trial: Some("t1".into()),
            ..no_filters()
        };
        let tasks: BTreeSet<&str> = scoped_entries(&loaded, &filters)
            .iter()
            .map(|e| e.task.as_str())
            .collect();
        assert_eq!(tasks, BTreeSet::from(["rwr-1", "rwr-2"]));
        assert_eq!(
            filters_label("demo", &filters, TableKind::Lanes, Some(&loaded)),
            "demo · trial retire-worktree-root"
        );
        assert_eq!(
            filters_label("demo", &filters, TableKind::Dirs, Some(&loaded)),
            "demo"
        );
    }

    fn bank_two_trials(repo: &Repo) {
        for entry in two_trials() {
            crate::usage::append(repo, &entry).unwrap();
        }
    }

    /// `enter` on a trial opens the runs table by pipeline, narrowed to it:
    /// one row per pipeline the trial ticked, whatever `by` and `pipeline`
    /// were before. `all` on the `trial` row then clears it.
    #[test]
    fn enter_on_a_trial_shows_its_pipelines_side_by_side() {
        let (repo, _root_guard) = fixture("screen-trial-open");
        bank_two_trials(&repo);
        let args = EvalArgs {
            by: EvalBy::Step,
            pipeline: Some("impl".into()),
            ..no_args()
        };
        let open = "\t\t\r";
        let mut input = keys(&format!("{open}q"));
        let mut out = Vec::new();
        run_now(&repo, &args, &mut input, &mut out);
        let text = String::from_utf8(out).unwrap();
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by pipeline "), "{last}");
        assert!(last.contains(" trial retire-worktree-root ─┐"), "{last}");
        assert!(last.contains("│>impl "), "{last}");
        assert!(last.contains("│ impl_ui "), "{last}");
        assert!(!last.contains("impl_tdd"), "the other trial: {last}");
        assert!(!last.contains("default"), "the ordinary lane: {last}");
        assert!(last.contains("[tab] dirs"), "{last}");

        let to_trial = DOWN.repeat(4);
        let mut input = keys(&format!("{open}f{to_trial}{LEFT}\rq"));
        let mut out = Vec::new();
        run_now(&repo, &args, &mut input, &mut out);
        let text = String::from_utf8(out).unwrap();
        let last = last_frame(&text);
        assert!(!last.contains("trial retire"), "{last}");
        assert!(last.contains("│ default "), "{last}");
    }

    /// The trials table is bounded by `since`, sorts on `a`/`d` like the
    /// others, and reads `running` for a trial a queued task still carries.
    #[test]
    fn the_trials_table_is_windowed_sorted_and_reads_a_queued_arm_as_running() {
        let (repo, _root_guard) = fixture("screen-trials");
        bank_two_trials(&repo);
        std::fs::write(
            repo.queue_dir().join("rwr-2.md"),
            "---\nid: rwr-2\nstage: implement\ntrial: t1\n---\n",
        )
        .unwrap();
        assert_eq!(
            repo.tasks().unwrap()[0].front.trial.as_deref(),
            Some("t1"),
            "the queued arm parses"
        );

        let last = last_frame(&screen(&repo, "\t\tq")).to_string();
        assert!(last.contains("┌─ eval · trials "), "{last}");
        let newer = last.find("retire-worktree-root").expect("t1's row");
        let older = last.find("board-step-grace-window").expect("t2's row");
        assert!(newer < older, "newest first: {last}");
        assert!(last.contains("impl, impl_ui          2  running"), "{last}");
        assert!(last.contains("impl_tdd               1  settled"), "{last}");

        // `WHEN` is the second column: `a`, `↓` twice, `enter`.
        let last = last_frame(&screen(&repo, &format!("\t\ta{DOWN}{DOWN}\rq"))).to_string();
        assert!(last.contains(" ▲WHEN"), "{last}");
        let newer = last.find("retire-worktree-root").expect("t1's row");
        let older = last.find("board-step-grace-window").expect("t2's row");
        assert!(older < newer, "oldest first once ascending: {last}");

        let args = EvalArgs {
            since: Some("2026-10-01".into()),
            ..no_args()
        };
        let mut input = keys("\t\tq");
        let mut out = Vec::new();
        run_now(&repo, &args, &mut input, &mut out);
        let text = String::from_utf8(out).unwrap();
        let last = last_frame(&text);
        assert!(last.contains("retire-worktree-root"), "{last}");
        assert!(!last.contains("board-step-grace-window"), "{last}");
    }

    /// The header line of a frame's lanes or directory table.
    fn header_of(frame: &str) -> &str {
        frame
            .lines()
            .find(|l| l.contains("RUNS") || l.contains("SESSIONS"))
            .unwrap_or_else(|| panic!("no header: {frame}"))
    }

    /// `t` switches the lanes table to its per-run columns, names the view
    /// in the title and the other one in the key line, and `t` again
    /// switches back.
    #[test]
    fn t_switches_between_totals_and_per_run() {
        let (repo, _root_guard) = fixture_to_sort("screen-t");
        let opened = screen(&repo, "q");
        let last = last_frame(&opened);
        assert!(last.contains("┌─ eval · by pipeline · totals "), "{last}");
        assert!(
            header_of(last).contains(
                "IN  IN USD      OUT  OUT USD  CACHE R  CACHE R USD  CACHE W  CACHE W USD       \
                 USD      TIME"
            ),
            "{last}"
        );
        assert!(!header_of(last).contains("/RUN"), "{last}");

        let text = screen(&repo, "tq");
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by pipeline · per run "), "{last}");
        assert!(
            header_of(last).contains(
                "IN/RUN  IN USD/RUN  OUT/RUN  OUT USD/RUN  CACHE R/RUN  CACHE R USD/RUN  \
                 CACHE W/RUN  CACHE W USD/RUN       USD  USD/RUN  TIME/RUN"
            ),
            "{last}"
        );
        assert!(
            last.contains("[d] descending   [t] totals   [tab] dirs"),
            "{last}"
        );

        let text = screen(&repo, "ttq");
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by pipeline · totals "), "{last}");
        assert!(
            last.contains("[d] descending   [t] per run   [tab] dirs"),
            "{last}"
        );
    }

    /// One `t` covers both tables, and survives `tab`, `r` and a new `by`
    /// from the filter panel. The trials table has no figures to switch.
    #[test]
    fn the_view_survives_tab_refresh_and_a_new_by() {
        let (repo, _root_guard) = fixture_to_sort("screen-t-keeps");
        bank_dir(&repo, "2026-08-01T09:00:00+00:00", "/w/proj", "s1", 0.5);

        let text = screen(&repo, "t\tq");
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by dir · per run "), "{last}");
        assert!(header_of(last).contains("IN/SESSION"), "{last}");

        let text = screen(&repo, "t\t\tq");
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · trials "), "{last}");
        assert!(!last.contains("[t]"), "{last}");

        let text = screen(&repo, "t\t\t\trq");
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by pipeline · per run "), "{last}");
        assert!(header_of(last).contains("TIME/RUN"), "{last}");

        let text = screen(&repo, &format!("tf{RIGHT}\rq"));
        let last = last_frame(&text);
        assert!(last.contains(" · per run "), "{last}");
        assert!(!last.contains("by pipeline"), "the by moved on: {last}");
        assert!(header_of(last).contains("TIME/RUN"), "{last}");
    }

    /// A sort on `IN` becomes one on `IN/RUN` after `t`, a sort on `IN USD`
    /// becomes one on `IN USD/RUN`, and one on `USD/RUN` becomes one on
    /// `USD` after `t` back. The popup lists the columns of the view on
    /// screen.
    #[test]
    fn t_carries_the_sort_to_the_matching_column() {
        let (repo, _root_guard) = fixture_to_sort("screen-t-sort");
        // default order, PIPELINE, RUNS, PASS, BLOCKS, CTX PEAK AVG,
        // CTX PEAK, then IN.
        let to_in = DOWN.repeat(7);
        let text = screen(&repo, &format!("d{to_in}\rtq"));
        let last = last_frame(&text);
        assert!(header_of(last).contains(" ▼IN/RUN"), "{last}");

        let text = screen(&repo, &format!("d{to_in}\rtdq"));
        let popup = last_frame(&text);
        assert!(
            popup.contains("│  > IN/RUN"),
            "opens on the carried column: {popup}"
        );
        assert!(popup.contains("│    TIME/RUN"), "{popup}");
        assert!(popup.contains("│    CACHE W USD/RUN"), "{popup}");

        // `IN USD` is the one after `IN`, in either view.
        let to_in_usd = DOWN.repeat(8);
        let text = screen(&repo, &format!("d{to_in_usd}\rq"));
        let last = last_frame(&text);
        assert!(header_of(last).contains(" ▼IN USD "), "{last}");
        let text = screen(&repo, &format!("d{to_in_usd}\rtq"));
        let last = last_frame(&text);
        assert!(header_of(last).contains(" ▼IN USD/RUN"), "{last}");

        // Per run, `USD/RUN` is nine further on than `IN/RUN`: each token
        // class has its cost beside it.
        let to_usd_run = DOWN.repeat(16);
        let text = screen(&repo, &format!("td{to_usd_run}\r"));
        let last = last_frame(&text);
        assert!(header_of(last).contains("▼USD/RUN"), "{last}");
        let text = screen(&repo, &format!("td{to_usd_run}\rtq"));
        let last = last_frame(&text);
        assert!(header_of(last).contains("      ▼USD"), "{last}");
    }

    /// `spoolway eval` prints the totals columns closed by `Total`, and
    /// `--per-run` the per-run ones closed by the average run.
    #[test]
    fn the_printed_table_is_totals_unless_per_run() {
        let (repo, _root_guard) = fixture_to_sort("cli-totals");
        let table = print(&repo, &[]).unwrap();
        let header = table.lines().next().unwrap();
        assert!(
            header.ends_with(
                "       IN  IN USD      OUT  OUT USD  CACHE R  CACHE R USD  CACHE W  CACHE W USD       \
                 USD      TIME"
            ),
            "{table}"
        );
        assert!(
            table.lines().last().unwrap().starts_with("Total "),
            "{table}"
        );

        let table = print(&repo, &["--per-run"]).unwrap();
        let header = table.lines().next().unwrap();
        assert!(
            header.ends_with(
                "IN/RUN  IN USD/RUN  OUT/RUN  OUT USD/RUN  CACHE R/RUN  CACHE R USD/RUN  \
                 CACHE W/RUN  CACHE W USD/RUN       USD  USD/RUN  TIME/RUN"
            ),
            "{table}"
        );
        let last = table.lines().last().unwrap();
        assert!(last.starts_with("Average "), "{table}");
        assert!(!table.contains("Total"), "{table}");

        // Every name `--sort` took before it still takes, in either view;
        // a column the view does not draw sorts the rows and marks nothing.
        let table = print(&repo, &["--sort", "cost_per_run"]).unwrap();
        assert!(!table.contains('▼'), "{table}");
        let table = print(&repo, &["--sort", "in_tokens", "--per-run"]).unwrap();
        assert!(!table.contains('▼'), "{table}");
        let table = print(&repo, &["--sort", "time_s"]).unwrap();
        assert!(table.contains("▼TIME"), "{table}");
        // Each class cost sorts under its own name, in the view that draws it.
        for (key, view, mark) in [
            ("in_usd", None, "▼IN USD "),
            ("out_usd", None, "▼OUT USD "),
            ("cache_read_usd", None, "▼CACHE R USD"),
            ("cache_write_usd", None, "▼CACHE W USD"),
            ("in_usd_per_run", Some("--per-run"), "▼IN USD/RUN"),
            ("out_usd_per_run", Some("--per-run"), "▼OUT USD/RUN"),
            (
                "cache_read_usd_per_run",
                Some("--per-run"),
                "▼CACHE R USD/RUN",
            ),
            (
                "cache_write_usd_per_run",
                Some("--per-run"),
                "▼CACHE W USD/RUN",
            ),
        ] {
            let mut args = vec!["--sort", key];
            args.extend(view);
            let table = print(&repo, &args).unwrap();
            assert!(table.contains(mark), "{key}: {table}");
        }
    }

    /// `--per-run` changes only the printed table, so beside an export or
    /// `--discard` it is refused with the reason.
    #[test]
    fn per_run_is_refused_beside_an_export_or_a_discard() {
        let (repo, _root_guard) = fixture_to_sort("cli-per-run-refused");
        let err = print(&repo, &["--per-run", "--csv"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("already carry both"), "{err}");

        let args = EvalArgs {
            per_run: true,
            ..no_args()
        };
        let err = run_to(
            &repo,
            &args,
            true,
            Some(&Pipelines::builtin()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already carry both"), "{err}");

        // `main` asks the same question before `--discard` is dispatched.
        let args = EvalArgs {
            per_run: true,
            discard: Some("some-trial".into()),
            ..no_args()
        };
        let err = refuse_per_run_beside_an_export(&args, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not `--discard`") && err.contains("already carry both"),
            "{err}"
        );
        assert!(
            err.contains("Drop `--per-run`"),
            "says what to do instead: {err}"
        );
        assert!(refuse_per_run_beside_an_export(&no_args(), true).is_ok());
    }

    /// `by dir` and `by session` share one sort, so `t` pressed under `by
    /// session` must not lose a `by dir` sort: back on `by dir`, it reads
    /// as the matching column of whichever view is now on screen.
    #[test]
    fn a_dir_sort_follows_the_view_whichever_by_t_was_pressed_under() {
        let (repo, _root_guard) = fixture_to_sort("screen-t-dir-sort");
        bank_dir(&repo, "2026-08-01T09:00:00+00:00", "/w/proj", "s1", 0.5);
        // default order, DIR, SESSIONS, then IN or IN/SESSION.
        let to_in = DOWN.repeat(3);
        let to_session = format!("f{RIGHT}\r");
        let to_dir = format!("f{LEFT}\r");

        let text = screen(&repo, &format!("\ttd{to_in}\r{to_session}t{to_dir}q"));
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by dir · totals "), "{last}");
        assert!(header_of(last).contains("▼IN  "), "{last}");

        let text = screen(&repo, &format!("\td{to_in}\r{to_session}q"));
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by session "), "{last}");
        assert!(last.contains("▼IN  "), "a sort on IN reads as one: {last}");

        let text = screen(&repo, &format!("\td{to_in}\r{to_session}t{to_dir}q"));
        let last = last_frame(&text);
        assert!(last.contains("┌─ eval · by dir · per run "), "{last}");
        assert!(header_of(last).contains("▼IN/SESSION"), "{last}");
    }
}
