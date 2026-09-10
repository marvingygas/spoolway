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
//! Nothing here fetches either table.

use std::collections::BTreeMap;
use std::fs;
use std::sync::{Mutex, OnceLock};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::pipeline::{Pipelines, StepKind};
use crate::repo::Repo;
use crate::usage::ModelPrice;

/// The name the shipped pipelines put on every local step: not a model, an
/// instruction to name one.
///
/// It is a real string rather than an empty `model:` so that the shipped
/// pipelines are valid files a person edits in place — `pipeline check` wants
/// every agent step to name something, and "nothing" would fail before anybody
/// had read the comment telling them what to put there. The cost of that is
/// that it also *passes* every check asking whether a model is named, which is
/// why `spoolway doctor` knows it by name: a project still carrying it has not
/// chosen a model, whatever the file says.
pub const PLACEHOLDER: &str = "your-local-model";

/// litellm's price map, vendored and distilled. See the module docs.
const BUILTIN_JSON: &str = include_str!("../assets/model-prices.json");

/// The shape `scripts/refresh-model-prices.mjs` writes: a short header saying
/// where the numbers came from, then one row per priced chat model.
#[derive(Deserialize)]
struct BuiltinFile {
    #[allow(dead_code)]
    source: String,
    #[allow(dead_code)]
    license: String,
    #[allow(dead_code)]
    generated: String,
    models: BTreeMap<String, ModelPrice>,
}

/// Distill litellm's raw, provider-wide price map into the rows spoolway uses.
///
/// litellm includes embeddings, image models, and descriptive sample rows in
/// the same map. A chat row without both prices is no price at all, so it is
/// dropped rather than becoming a misleading zero-cost model.
// The refresh command is deliberately the next task: keep its pure half here
// without inventing a runtime caller before that command exists.
#[allow(dead_code)]
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
fn builtin() -> &'static BTreeMap<String, ModelPrice> {
    static TABLE: OnceLock<BTreeMap<String, ModelPrice>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<BuiltinFile>(BUILTIN_JSON)
            .expect("assets/model-prices.json is vendored and must parse")
            .models
    })
}

struct RefreshedCache {
    path: std::path::PathBuf,
    contents: Vec<u8>,
    models: BTreeMap<String, ModelPrice>,
}

/// Read the optional machine-wide table before consulting its parsed cache.
/// The read has to come first: permissions can make an otherwise unchanged
/// file unreadable, and serving its cached row then would defeat the silent
/// built-in fallback promised for that failure. Comparing bytes still avoids
/// reparsing an unchanged table on every frame of a long-running `status`.
fn refreshed_price(model: &str) -> Option<ModelPrice> {
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
        return cached.as_ref()?.models.get(model).copied();
    }
    drop(cached);

    let models = serde_json::from_slice::<BuiltinFile>(&contents)
        .ok()?
        .models;
    let answer = models.get(model).copied();
    *cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(RefreshedCache {
        path,
        contents,
        models,
    });
    answer
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

    Ok(())
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
        let table = serde_json::json!({
            "source": "fixture",
            "license": "MIT",
            "generated": "2026-09-09",
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

    /// The vendored table is real data, not a fixture — this is the one test
    /// that would catch it failing to parse at all.
    #[test]
    fn the_builtin_table_parses_and_is_not_empty() {
        assert!(!builtin().is_empty());
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

    /// Every agent step in the shipped pipelines names a model — the previous
    /// task's own acceptance criterion. This is that criterion's sequel: a
    /// name that cannot be priced or sized fails silently until someone
    /// happens to read `spoolway doctor`, so every real model name here must
    /// resolve to both, against no project config at all.
    ///
    /// One exception, and it is deliberate rather than missed: the shipped
    /// local steps carry the literal placeholder `your-local-model`, telling
    /// a person to put their own local model's name there. Nobody — not
    /// litellm, not spoolway — can know that model's price ahead of a real
    /// name replacing it; `spoolway doctor` is what reports the gap once one
    /// does and still resolves to nothing.
    #[test]
    fn every_real_model_the_shipped_pipelines_name_resolves() {
        let pipelines = Pipelines::builtin();
        let no_config = BTreeMap::new();
        let home = fixture_home("models-shipped-pipelines");

        let real_models: Vec<&str> = named(&pipelines)
            .into_keys()
            .filter(|model| *model != PLACEHOLDER)
            .collect();
        assert!(
            !real_models.is_empty(),
            "no real model names in the shipped pipelines to check"
        );

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
