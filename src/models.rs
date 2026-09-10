//! What a model costs, and how big its window is — resolved, not guessed.
//!
//! `[models]` in `config.toml` is a project's own word on a model, by glob,
//! and it is empty by default: spoolway does not know what you run. Behind
//! it sit two tables nobody has to write — a refreshed copy of litellm's price
//! map under `~/.spoolway/`, then the copy vendored into the binary. Both are
//! distilled to the six numbers [`crate::usage::ModelPrice`] holds. [`resolve`]
//! is the one place that order is applied: a project's own glob first, each
//! price table by exact name after that, and nothing last.
//!
//! The refreshed table is optional user-state: an absent, unreadable, or bad
//! file is ignored. The vendored table is compiled in with [`include_str!`]
//! and parsed once, so that optional layer can never take away a shipped row.
//! [`refresh`] is the only network edge: it shells out to curl, distils the
//! answer completely, and only then atomically replaces one of those tables.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

use crate::cli::ModelsRefreshArgs;
use crate::pipeline::{Pipelines, StepKind};
use crate::repo::Repo;
use crate::usage::ModelPrice;

/// The sentinel older scaffolds and the annotated generation template use to
/// instruct a person to name a local model.
///
/// Kept recognizable so doctor can diagnose existing files and so pipeline
/// generation can distinguish its illustrative local model from a real one.
/// Fresh bundled pipelines now use explicit blanks instead.
pub const PLACEHOLDER: &str = "your-local-model";

/// litellm's price map, vendored and distilled. See the module docs.
const BUILTIN_JSON: &str = include_str!("../assets/model-prices.json");

/// The price-table file: a short header saying where the numbers came from,
/// then one row per priced chat model. The same shape is read at both layers
/// and written by [`refresh`], so they cannot drift into separate contracts.
#[derive(Deserialize, Serialize)]
struct BuiltinFile {
    source: String,
    license: String,
    generated: NaiveDate,
    models: BTreeMap<String, ModelPrice>,
}

const SOURCE_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const SOURCE_PAGE: &str =
    "https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json";
const SOURCE_LICENSE: &str = "MIT";
/// The fetch seam for an offline suite. Production leaves it unset; like
/// `SPOOLWAY_GH` for the stack command, the override changes the external
/// input without giving the command a second implementation.
const SOURCE_URL_ENV: &str = "SPOOLWAY_MODEL_PRICES_URL";

/// Distill litellm's raw, provider-wide price map into the rows spoolway uses.
///
/// litellm includes embeddings, image models, and descriptive sample rows in
/// the same map. A chat row without both prices is no price at all, so it is
/// dropped rather than becoming a misleading zero-cost model.
pub(crate) fn distill(raw: &BTreeMap<String, serde_json::Value>) -> BTreeMap<String, ModelPrice> {
    raw.iter()
        .filter_map(|(name, value)| {
            let row = value.as_object()?;
            if row.get("mode")?.as_str()? != "chat" {
                return None;
            }
            let input = finite_number(row.get("input_cost_per_token")?)?;
            let output = finite_number(row.get("output_cost_per_token")?)?;

            Some((
                name.clone(),
                ModelPrice {
                    context_window: window(row.get("max_input_tokens"))
                        .or_else(|| window(row.get("max_tokens")))
                        .unwrap_or(0),
                    input: per_million(input),
                    output: per_million(output),
                    cache_read: optional_rate(row.get("cache_read_input_token_cost")),
                    cache_write_5m: optional_rate(row.get("cache_creation_input_token_cost")),
                    cache_write_1h: optional_rate(
                        row.get("cache_creation_input_token_cost_above_1hr"),
                    ),
                    ..Default::default()
                },
            ))
        })
        .collect()
}

/// Fetch and replace the machine-wide or vendored price table.
///
/// Every fallible operation through parsing the response happens before the
/// destination is opened. In particular, a missing curl, an HTTP failure, or
/// invalid JSON cannot truncate the last usable table.
pub fn refresh(repo: &Repo, args: &ModelsRefreshArgs) -> Result<()> {
    let target = refresh_target(repo, args.vendor)?;
    let source_url = std::env::var(SOURCE_URL_ENV).unwrap_or_else(|_| SOURCE_URL.to_string());
    let response = fetch(&source_url, args.vendor)?;
    if !args.vendor {
        println!("  fetched  {}", display_url(&source_url));
    }

    let raw: BTreeMap<String, serde_json::Value> = serde_json::from_slice(&response)
        .with_context(|| format!("parsing litellm's price response from {source_url}"))?;
    let raw_count = raw.len();
    let models = distill(&raw);
    // The machine-wide layer may not exist yet. In that case the active table
    // it is replacing is the built-in one, so reporting every current model as
    // newly added would hide the small upstream delta a refresh is for.
    let old = existing_models(&target).unwrap_or_else(|| builtin().clone());
    let changes = Changes::between(&old, &models);
    let file = BuiltinFile {
        source: SOURCE_PAGE.to_string(),
        license: SOURCE_LICENSE.to_string(),
        generated: chrono::Utc::now().date_naive(),
        models,
    };
    let mut contents = serde_json::to_vec_pretty(&file).context("encoding the distilled table")?;
    contents.push(b'\n');
    crate::task::write_atomic(&target, contents)
        .with_context(|| format!("writing {}", target.display()))?;

    for line in success_lines(
        repo,
        &target,
        args.vendor,
        file.models.len(),
        raw_count - file.models.len(),
        &changes,
    ) {
        println!("{line}");
    }
    Ok(())
}

fn refresh_target(repo: &Repo, vendor: bool) -> Result<PathBuf> {
    if !vendor {
        return Ok(crate::mux::home().join(".spoolway/model-prices.json"));
    }
    let path = repo.checkout.join("assets/model-prices.json");
    if !path.is_file() {
        bail!(vendor_missing_message());
    }
    Ok(path)
}

fn fetch(source_url: &str, vendor: bool) -> Result<Vec<u8>> {
    let output = match Command::new("curl")
        .args(["-fsSL", "--max-time", "30", source_url])
        .output()
    {
        Ok(output) => output,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            bail!(curl_missing_message(vendor))
        }
        Err(err) => return Err(err).context("running curl to fetch litellm's prices"),
    };
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        if detail.is_empty() {
            bail!("curl failed fetching {source_url} ({})", output.status);
        }
        bail!("curl failed fetching {source_url}: {detail}");
    }
    Ok(output.stdout)
}

fn curl_missing_message(vendor: bool) -> String {
    let unchanged = if vendor {
        "assets/model-prices.json"
    } else {
        "~/.spoolway/model-prices.json"
    };
    format!("curl is not on PATH; spoolway fetches prices by running it\n{unchanged} is unchanged")
}

fn vendor_missing_message() -> &'static str {
    "--vendor writes assets/model-prices.json, and this project has no such file\nnothing written"
}

fn display_url(url: &str) -> &str {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
}

fn existing_models(path: &Path) -> Option<BTreeMap<String, ModelPrice>> {
    // A malformed optional table is already ignored by resolution. Return no
    // comparison table here too: the caller then compares with the built-in
    // fallback while still letting refresh repair the bad file in one step.
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BuiltinFile>(&bytes).ok())
        .map(|file| file.models)
}

fn report_path(repo: &Repo, target: &Path, vendor: bool) -> String {
    if vendor {
        return target
            .strip_prefix(&repo.checkout)
            .unwrap_or(target)
            .display()
            .to_string();
    }
    let home = crate::mux::home();
    target
        .strip_prefix(&home)
        .map(|path| format!("~/{}", path.display()))
        .unwrap_or_else(|_| target.display().to_string())
}

fn success_lines(
    repo: &Repo,
    target: &Path,
    vendor: bool,
    kept: usize,
    dropped_rows: usize,
    changes: &Changes,
) -> Vec<String> {
    let wrote = format!("  wrote    {}", report_path(repo, target, vendor));
    if vendor {
        // Vendoring is deliberately quiet enough to paste into release work:
        // the one path changed is the whole report drawn by the command.
        return vec![wrote];
    }
    vec![
        wrote,
        format!("  models   {kept} priced chat rows kept, {dropped_rows} other rows dropped"),
        format!(
            "  changed  {} added, {} repriced, {} unchanged, {} dropped",
            changes.added, changes.repriced, changes.unchanged, changes.dropped
        ),
    ]
}

#[derive(Default)]
struct Changes {
    added: usize,
    repriced: usize,
    unchanged: usize,
    dropped: usize,
}

impl Changes {
    fn between(old: &BTreeMap<String, ModelPrice>, new: &BTreeMap<String, ModelPrice>) -> Self {
        let mut changes = Self::default();
        for (name, price) in new {
            match old.get(name) {
                None => changes.added += 1,
                Some(old_price) if old_price == price => changes.unchanged += 1,
                Some(_) => changes.repriced += 1,
            }
        }
        changes.dropped = old.keys().filter(|name| !new.contains_key(*name)).count();
        changes
    }
}

fn finite_number(value: &serde_json::Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

fn window(value: Option<&serde_json::Value>) -> Option<usize> {
    usize::try_from(value?.as_u64()?).ok()
}

fn optional_rate(value: Option<&serde_json::Value>) -> f64 {
    value
        .and_then(finite_number)
        .map(per_million)
        .unwrap_or(0.0)
}

/// Per-million prices are persisted and compared, so round away the binary
/// float noise introduced by scaling a per-token decimal.
fn per_million(per_token: f64) -> f64 {
    (per_token * 1_000_000.0 * 1_000_000.0).round() / 1_000_000.0
}

/// Parsed once. `assets/model-prices.json` is vendored, not user input, so a
/// parse failure here is a build-time mistake, not a runtime one — panicking
/// says so plainly instead of silently pricing nothing.
fn builtin_file() -> &'static BuiltinFile {
    static TABLE: OnceLock<BuiltinFile> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str(BUILTIN_JSON)
            .expect("assets/model-prices.json is vendored and must parse")
    })
}

fn builtin() -> &'static BTreeMap<String, ModelPrice> {
    &builtin_file().models
}

struct RefreshedCache {
    path: std::path::PathBuf,
    contents: Vec<u8>,
    file: Arc<BuiltinFile>,
}

/// Read the optional machine-wide table before consulting its parsed cache.
/// The read has to come first: permissions can make an otherwise unchanged
/// file unreadable, and serving its cached row then would defeat the silent
/// built-in fallback promised for that failure. Comparing bytes still avoids
/// reparsing an unchanged table on every frame of a long-running `status`.
fn refreshed_file() -> Option<Arc<BuiltinFile>> {
    let path = crate::mux::home().join(".spoolway/model-prices.json");
    let contents = fs::read(&path).ok()?;

    static CACHE: OnceLock<Mutex<Option<RefreshedCache>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let cached = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cached
        .as_ref()
        .is_some_and(|cached| cached.path == path && cached.contents == contents)
    {
        return Some(Arc::clone(&cached.as_ref()?.file));
    }
    drop(cached);

    let file = Arc::new(serde_json::from_slice::<BuiltinFile>(&contents).ok()?);
    *cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(RefreshedCache {
        path,
        contents,
        file: Arc::clone(&file),
    });
    Some(file)
}

fn refreshed_price(model: &str) -> Option<ModelPrice> {
    refreshed_file()?.models.get(model).copied()
}

/// The generated date and whole-day age of the table resolution currently
/// prefers. A refreshed file wins only when the complete file parses; an
/// unusable optional file therefore cannot make the footer and model rows
/// describe different layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PriceTableAge {
    pub generated: NaiveDate,
    pub days: u64,
}

pub(crate) fn price_table_age() -> PriceTableAge {
    price_table_age_at(chrono::Utc::now().date_naive())
}

fn price_table_age_at(today: NaiveDate) -> PriceTableAge {
    let generated = refreshed_file()
        .map(|file| file.generated)
        .unwrap_or_else(|| builtin_file().generated);
    // A future date can only come from a hand-written refreshed file or a
    // clock correction. It is not an old table, so report zero rather than a
    // negative age that would read as stale arithmetic.
    let days = today.signed_duration_since(generated).num_days().max(0) as u64;
    PriceTableAge { generated, days }
}

/// Where a resolved answer came from — what `spoolway models` prints in its
/// `SOURCE` column, and what `spoolway eval --by` and `spoolway doctor` tell apart
/// a project's own word on a model from litellm's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Matched a glob in this project's own `[models]` table.
    Config,
    /// Not configured, but the refreshed table knows this exact name.
    Refreshed,
    /// Not configured, but the vendored table knows this exact name.
    Builtin,
    /// No config, refreshed, or built-in row. Unpriced — not free, not zero.
    Unknown,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Config => "config",
            Source::Refreshed => "refreshed",
            Source::Builtin => "built-in",
            Source::Unknown => "unknown",
        }
    }
}

/// What `model` resolved to, and where that answer came from.
pub struct Resolved {
    pub price: Option<ModelPrice>,
    pub source: Source,
}

/// Resolve `model`: this project's own `[models]` table first, by glob —
/// [`crate::usage::best_match`], the same rule `spoolway eval --by` always used —
/// then the refreshed and vendored tables by exact name, then nothing.
///
/// Exact names in the price files because they hold real model names, not
/// patterns someone wrote to match a family; a project that wants a glob to
/// win writes one in `[models]`, which is checked first for exactly that
/// reason.
pub fn resolve(config_models: &BTreeMap<String, ModelPrice>, model: &str) -> Resolved {
    if let Some(entry) = crate::usage::best_match(config_models, model) {
        return Resolved {
            price: Some(*entry),
            source: Source::Config,
        };
    }
    if let Some(entry) = refreshed_price(model) {
        return Resolved {
            price: Some(entry),
            source: Source::Refreshed,
        };
    }
    if let Some(entry) = builtin().get(model) {
        return Resolved {
            price: Some(*entry),
            source: Source::Builtin,
        };
    }
    Resolved {
        price: None,
        source: Source::Unknown,
    }
}

/// Every model an agent step of this project's pipelines names, and which
/// step ids name it. A model absent here is a model no lane will ever run —
/// [`resolve`] is only worth asking about one that is.
pub fn named(pipelines: &Pipelines) -> BTreeMap<&str, Vec<&str>> {
    let mut map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for pipeline in pipelines.pipelines.values() {
        for step in &pipeline.steps {
            if step.kind() != StepKind::Agent {
                continue;
            }
            let Some(model) = step.model.as_deref().filter(|m| !m.trim().is_empty()) else {
                continue;
            };
            let steps = map.entry(model).or_default();
            if !steps.contains(&step.id.as_str()) {
                steps.push(&step.id);
            }
        }
    }
    map
}

#[derive(Serialize)]
struct Row<'a> {
    model: &'a str,
    window: Option<usize>,
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write_5m: Option<f64>,
    cache_write_1h: Option<f64>,
    slots: Option<u32>,
    exclusive: bool,
    source: &'static str,
    steps: &'a [&'a str],
}

/// `spoolway models`: every model this project's pipelines name, with what it
/// costs, how big its window is, and where that answer came from.
pub fn run(repo: &Repo, pipelines: &Pipelines, json: bool) -> Result<()> {
    let named = named(pipelines);

    let rows: Vec<Row> = named
        .iter()
        .map(|(model, steps)| {
            let resolved = resolve(&repo.config.models, model);
            Row {
                model,
                window: resolved.price.map(|p| p.context_window),
                input: resolved.price.map(|p| p.input),
                output: resolved.price.map(|p| p.output),
                cache_read: resolved.price.map(|p| p.cache_read),
                cache_write_5m: resolved.price.map(|p| p.cache_write_5m),
                cache_write_1h: resolved.price.map(|p| p.cache_write_1h),
                slots: resolved
                    .price
                    .and_then(|p| (p.slots > 0).then_some(p.slots)),
                exclusive: resolved.price.is_some_and(|p| p.exclusive),
                source: resolved.source.label(),
                steps,
            }
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No agent step in this project's pipelines names a model.");
        println!("\n{}", price_table_footer(price_table_age()));
        return Ok(());
    }

    let width = rows
        .iter()
        .map(|r| r.model.len())
        .max()
        .unwrap_or(5)
        .max("MODEL".len());

    println!(
        "{:<width$}  {:>9}  {:>8}  {:>8}  {:>8}  {:>9}  {:>9}  {:>5}  {:>4}  {:<9}  STEPS",
        "MODEL",
        "WINDOW",
        "IN",
        "OUT",
        "CACHE R",
        "CACHE W5M",
        "CACHE W1H",
        "SLOTS",
        "EXCL",
        "SOURCE"
    );
    for row in &rows {
        println!(
            "{:<width$}  {:>9}  {:>8}  {:>8}  {:>8}  {:>9}  {:>9}  {:>5}  {:>4}  {:<9}  {}",
            row.model,
            row.window
                .map(|w| crate::fmt::tokens_human(w as u64))
                .unwrap_or_else(|| "—".into()),
            crate::fmt::money(row.input),
            crate::fmt::money(row.output),
            crate::fmt::money(row.cache_read),
            crate::fmt::money(row.cache_write_5m),
            crate::fmt::money(row.cache_write_1h),
            row.slots
                .map(|s| s.to_string())
                .unwrap_or_else(|| "—".into()),
            if row.exclusive { "yes" } else { "—" },
            row.source,
            row.steps.join(", "),
        );
    }

    let unknown: Vec<&str> = rows
        .iter()
        .filter(|r| r.source == Source::Unknown.label())
        .map(|r| r.model)
        .collect();
    if !unknown.is_empty() {
        println!(
            "\nUnpriced and unsized — not in litellm's table, and nothing in [models] names it: {}",
            unknown.join(", ")
        );
        println!("Add one with `spoolway config set models.'<model-glob>'.input <usd per 1M>`.");
    }

    println!("\n{}", price_table_footer(price_table_age()));

    Ok(())
}

fn price_table_footer(age: PriceTableAge) -> String {
    format!(
        "Prices generated {}, {} days ago. Refresh with `spoolway models refresh`.",
        age.generated, age.days
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_home(name: &str) -> std::path::PathBuf {
        let home = crate::scratch::root(name);
        fs::create_dir_all(home.join(".spoolway")).unwrap();
        home
    }

    fn write_refreshed(home: &std::path::Path, models: serde_json::Value) {
        write_refreshed_generated(home, "2026-09-09", models);
    }

    fn write_refreshed_generated(
        home: &std::path::Path,
        generated: &str,
        models: serde_json::Value,
    ) {
        let table = serde_json::json!({
            "source": "fixture",
            "license": "MIT",
            "generated": generated,
            "models": models,
        });
        fs::write(
            home.join(".spoolway/model-prices.json"),
            serde_json::to_vec(&table).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn litellm_rows_are_filtered_scaled_and_rounded() {
        let raw: BTreeMap<String, serde_json::Value> = serde_json::from_value(serde_json::json!({
            "kept": {
                "mode": "chat",
                "max_input_tokens": 123456,
                "max_tokens": 9,
                "input_cost_per_token": 0.000004999999999999,
                "output_cost_per_token": 0.000025,
                "cache_read_input_token_cost": 0.000000499999999999,
                "cache_creation_input_token_cost": 0.00000625,
                "cache_creation_input_token_cost_above_1hr": 0.00001
            },
            "fallback-window": {
                "mode": "chat",
                "max_input_tokens": "not a number",
                "max_tokens": 8192,
                "input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000002
            },
            "no-window": {
                "mode": "chat",
                "input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000002
            },
            "embedding": {
                "mode": "embedding",
                "input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000002
            },
            "missing-output": {
                "mode": "chat",
                "input_cost_per_token": 0.000001
            },
            "wrong-input-type": {
                "mode": "chat",
                "input_cost_per_token": "0.000001",
                "output_cost_per_token": 0.000002
            }
        }))
        .unwrap();

        let table = distill(&raw);
        assert_eq!(table.len(), 3);
        assert_eq!(
            table["kept"],
            ModelPrice {
                context_window: 123456,
                input: 5.0,
                output: 25.0,
                cache_read: 0.5,
                cache_write_5m: 6.25,
                cache_write_1h: 10.0,
                ..Default::default()
            }
        );
        assert_eq!(table["fallback-window"].context_window, 8192);
        assert_eq!(table["fallback-window"].cache_read, 0.0);
        assert_eq!(table["no-window"].context_window, 0);
    }

    #[test]
    fn refresh_changes_compare_names_and_complete_prices() {
        let old = BTreeMap::from([
            (
                "same".to_string(),
                ModelPrice {
                    input: 1.0,
                    ..Default::default()
                },
            ),
            (
                "repriced".to_string(),
                ModelPrice {
                    input: 2.0,
                    ..Default::default()
                },
            ),
            ("gone".to_string(), ModelPrice::default()),
        ]);
        let new = BTreeMap::from([
            (
                "same".to_string(),
                ModelPrice {
                    input: 1.0,
                    ..Default::default()
                },
            ),
            (
                "repriced".to_string(),
                ModelPrice {
                    input: 3.0,
                    ..Default::default()
                },
            ),
            ("new".to_string(), ModelPrice::default()),
        ]);

        let changes = Changes::between(&old, &new);
        assert_eq!(changes.added, 1);
        assert_eq!(changes.repriced, 1);
        assert_eq!(changes.unchanged, 1);
        assert_eq!(changes.dropped, 1);
    }

    #[test]
    fn vendor_success_reports_only_the_written_path() {
        let checkout = crate::scratch::root("models-vendor-report");
        let repo = Repo {
            root: checkout.clone(),
            checkout: checkout.clone(),
            config: Default::default(),
            home: checkout.join("state"),
        };
        let lines = success_lines(
            &repo,
            &checkout.join("assets/model-prices.json"),
            true,
            12,
            3,
            &Changes {
                added: 1,
                repriced: 2,
                unchanged: 9,
                dropped: 4,
            },
        );

        assert_eq!(lines, ["  wrote    assets/model-prices.json"]);
    }

    #[test]
    fn no_write_failures_match_the_two_line_command_report() {
        assert_eq!(
            curl_missing_message(false),
            "curl is not on PATH; spoolway fetches prices by running it\n\
             ~/.spoolway/model-prices.json is unchanged"
        );
        assert_eq!(
            vendor_missing_message(),
            "--vendor writes assets/model-prices.json, and this project has no such file\n\
             nothing written"
        );
    }

    /// The vendored table is real data, not a fixture — this is the one test
    /// that would catch it failing to parse at all.
    #[test]
    fn the_builtin_table_parses_and_is_not_empty() {
        assert!(!builtin().is_empty());
    }

    #[test]
    fn table_age_uses_the_refreshed_date_and_falls_back_as_a_whole_file() {
        let home = fixture_home("models-table-age");
        crate::platform::test_home::with_home(&home, || {
            let today = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
            assert_eq!(
                price_table_age_at(today),
                PriceTableAge {
                    generated: NaiveDate::from_ymd_opt(2026, 8, 9).unwrap(),
                    days: 32,
                }
            );

            write_refreshed_generated(&home, "2026-09-01", serde_json::json!({}));
            assert_eq!(
                price_table_age_at(today),
                PriceTableAge {
                    generated: NaiveDate::from_ymd_opt(2026, 9, 1).unwrap(),
                    days: 9,
                }
            );

            // An invalid header makes the optional file unusable, just like
            // an invalid model row: both model lookup and age fall back to
            // the complete vendored table rather than mixing the layers.
            write_refreshed_generated(&home, "not-a-date", serde_json::json!({}));
            assert_eq!(price_table_age_at(today).days, 32);
        });
    }

    #[test]
    fn table_footer_names_the_date_age_and_explicit_refresh() {
        let age = PriceTableAge {
            generated: NaiveDate::from_ymd_opt(2026, 8, 9).unwrap(),
            days: 31,
        };
        assert_eq!(
            price_table_footer(age),
            "Prices generated 2026-08-09, 31 days ago. Refresh with `spoolway models refresh`."
        );
    }

    /// The three models this project's own steps run (see
    /// `.spoolway/pipelines/*.yml`) are exactly the ones the plan's
    /// references promised litellm's data carries.
    #[test]
    fn the_anthropic_models_this_project_runs_are_in_the_builtin_table() {
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-haiku-4-5-20251001",
        ] {
            assert!(
                builtin().contains_key(model),
                "{model} is missing from the built-in table"
            );
        }
    }

    // covers: models — a rate written in config is what is billed, over the table shipped in the binary

    #[test]
    fn config_wins_over_the_builtin_table() {
        let mut config = BTreeMap::new();
        config.insert(
            "claude-opus-5".to_string(),
            ModelPrice {
                input: 1.0,
                ..Default::default()
            },
        );
        let resolved = resolve(&config, "claude-opus-5");
        assert_eq!(resolved.source, Source::Config);
        assert_eq!(resolved.price.unwrap().input, 1.0);
    }

    #[test]
    fn the_builtin_table_answers_a_model_config_does_not_name() {
        let home = fixture_home("models-builtin-fallback");
        crate::platform::test_home::with_home(&home, || {
            let resolved = resolve(&BTreeMap::new(), "claude-opus-5");
            assert_eq!(resolved.source, Source::Builtin);
            assert!(resolved.price.unwrap().input > 0.0);
        });
    }

    #[test]
    fn a_model_in_neither_resolves_to_nothing() {
        let home = fixture_home("models-unknown");
        crate::platform::test_home::with_home(&home, || {
            let resolved = resolve(&BTreeMap::new(), "not-a-real-model");
            assert_eq!(resolved.source, Source::Unknown);
            assert!(resolved.price.is_none());
        });
    }

    #[test]
    fn config_then_refreshed_then_builtin_are_checked_per_model() {
        let home = fixture_home("models-resolution-layers");
        write_refreshed(
            &home,
            serde_json::json!({
                "claude-opus-5": { "input": 2.0 },
                "refreshed-only": { "input": 3.0 },
                "looks-*": { "input": 4.0 }
            }),
        );
        let config = BTreeMap::from([(
            "claude-*".to_string(),
            ModelPrice {
                input: 1.0,
                ..Default::default()
            },
        )]);

        crate::platform::test_home::with_home(&home, || {
            let configured = resolve(&config, "claude-opus-5");
            assert_eq!(configured.source, Source::Config);
            assert_eq!(configured.price.unwrap().input, 1.0);

            let refreshed = resolve(&config, "refreshed-only");
            assert_eq!(refreshed.source, Source::Refreshed);
            assert_eq!(refreshed.price.unwrap().input, 3.0);

            let exact_only = resolve(&config, "looks-like-a-glob");
            assert_eq!(exact_only.source, Source::Unknown);

            // This row is deliberately absent from the refreshed fixture: a
            // newer partial table must not erase what the binary still knows.
            let vendored = resolve(&config, "gpt-5.6-sol");
            assert_eq!(vendored.source, Source::Builtin);
        });
    }

    #[test]
    fn unusable_refreshed_files_are_silent_fallbacks() {
        for (name, contents) in [
            ("missing", None),
            ("malformed", Some(b"not json".as_slice())),
        ] {
            let home = fixture_home(&format!("models-refreshed-{name}"));
            if let Some(contents) = contents {
                fs::write(home.join(".spoolway/model-prices.json"), contents).unwrap();
            }
            crate::platform::test_home::with_home(&home, || {
                assert_eq!(
                    resolve(&BTreeMap::new(), "claude-opus-5").source,
                    Source::Builtin
                );
            });
        }

        let home = fixture_home("models-refreshed-unreadable");
        fs::create_dir(home.join(".spoolway/model-prices.json")).unwrap();
        crate::platform::test_home::with_home(&home, || {
            assert_eq!(
                resolve(&BTreeMap::new(), "claude-opus-5").source,
                Source::Builtin
            );
        });
    }

    /// Losing read permission changes neither a file's contents nor the
    /// modification timestamp used by the old cache. Resolution still has to
    /// notice that the optional layer is no longer readable and fall back.
    #[cfg(unix)]
    #[test]
    fn a_cached_refreshed_file_that_becomes_unreadable_is_not_served() {
        use std::os::unix::fs::PermissionsExt;

        let home = fixture_home("models-refreshed-cached-unreadable");
        write_refreshed(
            &home,
            serde_json::json!({ "claude-opus-5": { "input": 2.0 } }),
        );
        let path = home.join(".spoolway/model-prices.json");

        crate::platform::test_home::with_home(&home, || {
            assert_eq!(
                resolve(&BTreeMap::new(), "claude-opus-5").source,
                Source::Refreshed
            );
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
            assert_eq!(
                resolve(&BTreeMap::new(), "claude-opus-5").source,
                Source::Builtin
            );
        });
    }

    /// Shipped pipeline text makes no model choice. Its explicit blanks stay
    /// visible for a person to fill before dispatch. A text-level check
    /// rather than a parsed one, kept beside `resolve`'s own model-pricing
    /// tests; `crate::assets::tests::bundled_pipelines_parse_and_stay_agent_neutral`
    /// asserts the same blanks off the parsed, assembled pipeline instead,
    /// alongside its structural and `pi`-neutrality checks.
    #[test]
    fn shipped_pipelines_name_no_model() {
        for (name, body) in crate::pipeline::BUILTIN_PIPELINES {
            let mut agent_steps = 0;
            let mut blank_models = 0;
            let mut blank_efforts = 0;
            for line in body.lines().map(str::trim) {
                match line {
                    line if line.starts_with("agent:") => agent_steps += 1,
                    "model: \"\"" => blank_models += 1,
                    "effort: \"\"" => blank_efforts += 1,
                    _ => {}
                }
            }
            assert_eq!(blank_models, agent_steps, "pipeline `{name}`");
            assert_eq!(blank_efforts, agent_steps, "pipeline `{name}`");
        }
    }

    /// A model name that cannot be priced or sized fails silently until
    /// someone happens to read `spoolway doctor`. So a real model name
    /// carried by a shipped pipeline has to resolve to both a price and a
    /// window, against no project config at all.
    ///
    /// Today there are no such names to check, and that is deliberate rather
    /// than missed: every shipped agent step leaves `model: ""` blank for a
    /// person to fill, which `shipped_pipelines_name_no_model` above pins.
    /// `named` skips those blanks, and the shipped local steps carry the
    /// literal placeholder `your-local-model`, whose price nobody — not
    /// litellm, not spoolway — can know ahead of a real name replacing it.
    /// So this check guards a name that arrives later rather than one that is
    /// here now.
    #[test]
    fn every_real_model_the_shipped_pipelines_name_resolves() {
        let pipelines = Pipelines::builtin();
        let no_config = BTreeMap::new();
        let home = fixture_home("models-shipped-pipelines");

        let real_models: Vec<&str> = named(&pipelines)
            .into_keys()
            .filter(|model| *model != PLACEHOLDER)
            .collect();

        crate::platform::test_home::with_home(&home, || {
            for model in real_models {
                let resolved = resolve(&no_config, model);
                let price = resolved.price.unwrap_or_else(|| {
                    panic!("`{model}`, named by a shipped pipeline step, resolves to nothing")
                });
                assert!(
                    price.context_window > 0,
                    "`{model}`, named by a shipped pipeline step, resolves to a price with no window"
                );
            }
        });
    }
}
