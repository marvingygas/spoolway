//! Config as a flat list of dotted keys.
//!
//! One representation serves both `spoolway config get/set` and the settings TUI,
//! so a field added to [`crate::config::Config`] shows up in both without any
//! further work. Writes round-trip through the real `Config` type, which means
//! every edit is validated by the same deserialiser that loads the file.

use anyhow::{Context, Result, bail};
use toml::Value;

use crate::config::{Config, Priority};

/// One editable setting.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Dotted path, e.g. `agents.pi.concurrency`.
    pub key: String,
    /// Current value, rendered the way it is typed back in.
    pub value: String,
    pub kind: Kind,
    /// What this setting is for, when the name alone would mislead. Shown in
    /// the settings screen and written above the key in `config.toml`.
    pub note: Option<&'static str>,
}

/// One entry of the config surface: a dotted key (a profile name or a model
/// glob written as a placeholder, since the entry is generic across every one
/// that exists), what it may be set to, what it defaults to, and one sentence
/// about it.
///
/// The single register both surfaces render from — [`crate::config::Config::
/// render`]'s reference table and the settings screen's per-entry note (see
/// [`note`]) — so a setting is explained once, not in two places that can
/// drift apart. Anything longer than one sentence belongs in
/// `docs/configuration.md`, not here: a note here is the short form, not the
/// whole story.
pub struct Reference {
    pub key: &'static str,
    pub values: &'static str,
    pub default: &'static str,
    pub sentence: &'static str,
}

/// The [`REFERENCE`] row every rate of a price tier shares, which [`note`]
/// finds by this key rather than by its leaf.
const TIER_RATE_KEY: &str = "models.<glob>.above_<N>k_tokens.<rate>";

/// Every setting in `config.toml`, in the order the reference table prints
/// them.
pub const REFERENCE: &[Reference] = &[
    Reference {
        key: "dispatch.backend",
        values: "herdr, headless",
        default: "herdr",
        sentence: "Where lanes run: a real pane you can watch. `headless` is spoolway's own \
                    test backend, refused unless SPOOLWAY_TEST_BACKEND is set.",
    },
    Reference {
        key: "dispatch.lane_quiet",
        values: "<duration>",
        default: "15m",
        sentence: "How long a lane may say nothing before the dispatcher reminds it to \
                    report. Not how often a pass looks — the dispatcher polls at a fixed \
                    rate nobody sets.",
    },
    Reference {
        key: "dispatch.auto_commit",
        values: "true, false",
        default: "true",
        sentence: "Whether spoolway commits a lane's leftover work when its step settles.",
    },
    Reference {
        key: "dispatch.priority",
        values: "group, any",
        default: "group",
        sentence: "Whether a free slot is filled from every ready task, or from the group \
                    already landing first.",
    },
    Reference {
        key: "dispatch.lane_child_ceiling",
        values: "<duration>",
        default: "1h",
        sentence: "How long a lane may be excused for a process it started that is still \
                    running before the reminder loop treats it as silent anyway.",
    },
    Reference {
        key: "dispatch.keep_finished_lanes",
        values: "true, false",
        default: "true",
        sentence: "Whether a finished step's agent stays open in its own pane until its \
                    task is done, or each new step takes over the last one's place.",
    },
    Reference {
        key: "unattended.enabled",
        values: "true, false",
        default: "false",
        sentence: "Whether this run stops for a person, or resumes every block itself \
                    with `max_output_tokens` as the only brake.",
    },
    Reference {
        key: "unattended.max_output_tokens",
        values: "<tokens>",
        default: "0",
        sentence: "Output tokens one unattended run may spend before the dispatcher \
                    stops starting work; 0 is no ceiling.",
    },
    Reference {
        key: "unattended.max_cost_usd",
        values: "<dollars>",
        default: "0",
        sentence: "Dollars one unattended run may spend before the dispatcher stops \
                    starting work, priced from the resolved model table; 0 is no ceiling.",
    },
    Reference {
        key: "unattended.blocked_agent",
        values: "<profile>",
        default: "claude",
        sentence: "Which `[agents.*]` profile staffs `blocked` in an unattended run, for \
                    every pipeline that does not declare its own `blocked` step.",
    },
    Reference {
        key: "unattended.blocked_model",
        values: "<model>",
        default: "claude-opus-5",
        sentence: "The model that profile runs, staffing `blocked`. Blank refuses to start \
                    an unattended run: nobody would be able to clear a block.",
    },
    Reference {
        key: "unattended.blocked_effort",
        values: "<level>",
        default: "(blank)",
        sentence: "How hard that model thinks, staffing `blocked`.",
    },
    Reference {
        key: "unattended.blocked_session",
        values: "true, false",
        default: "true",
        sentence: "Whether the lane staffing `blocked` carries its own earlier session \
                    forward.",
    },
    Reference {
        key: "unattended.blocked_prompt",
        values: "<name>",
        default: "unblocker",
        sentence: "Prompt the lane staffing `blocked` runs.",
    },
    Reference {
        key: "housekeeping.update_check",
        values: "true, false",
        default: "true",
        sentence: "Whether spoolway tells a person at a keyboard that a newer release is out.",
    },
    Reference {
        key: "housekeeping.calibrate_window",
        values: "<duration>",
        default: "14d",
        sentence: "How far back `spoolway-calibrate` reads: archived tasks finished inside \
                    the window, and the ledger entries beside them.",
    },
    Reference {
        key: "housekeeping.retention_days",
        values: "<days>",
        default: "30",
        sentence: "How long system-prompts/, commands/, tracking/, headless/ (its logs/ \
                    included) and scratch/ keep an entry before it is deleted; 0 keeps \
                    everything forever. archive/ is governed by \
                    housekeeping.archive_retention_days instead. queue/, pending/, \
                    worktrees/ and plans/ are never swept.",
    },
    Reference {
        key: "housekeeping.archive_retention_days",
        values: "<days>",
        default: "0",
        sentence: "How long a finished task keeps its file in archive/ before it is \
                    deleted, along with its line in archive/index.jsonl; 0 keeps every \
                    finished task forever. Each task is about 25 KB. A deleted task \
                    can no longer be named in `depends_on`, and falls out of what \
                    `spoolway eval` and `calibrate_window` read.",
    },
    Reference {
        key: "housekeeping.price_max_age_days",
        values: "<days>",
        default: "30",
        sentence: "How old the active shared price table may be before `spoolway doctor` says so; \
                    0 disables the note.",
    },
    Reference {
        key: "watch.dirs",
        values: "<path>, ...",
        default: "(empty)",
        sentence: "Directories whose own sessions are counted beside the lanes. The \
                    project root is always watched; these are extra. Absolute, or \
                    ~-relative.",
    },
    Reference {
        key: "issue_tracking.hook",
        values: "<filename>",
        default: "(blank)",
        sentence: "A bare filename, resolved inside `.spoolway/hooks/` (a home-mode \
                    workspace's own `config/hooks/`); blank runs no hook and changes nothing \
                    about a task's `queued`, `blocked`, `paused` or `done`.",
    },
    Reference {
        key: "issue_tracking.project_key",
        values: "<string>",
        default: "(blank)",
        sentence: "Opaque to spoolway — `owner/repo` on github, a project key on jira — \
                    handed to the hook verbatim as `SPOOLWAY_PROJECT_KEY`.",
    },
    Reference {
        key: "issue_tracking.key_in_names",
        values: "true, false",
        default: "true",
        sentence: "Whether a `slug=` the hook answers prefixes the group, branch and \
                    worktree name `queue add` generates — `task/<slug>-<id>`. With no hook, \
                    or a blank `hook`, there is no `slug=` to prefix with, so this changes \
                    nothing either way.",
    },
    Reference {
        key: "agents.<profile>.kind",
        values: "<agent kind>",
        default: "one per shipped profile",
        sentence: "Which agent binary this profile launches.",
    },
    Reference {
        key: "agents.<profile>.concurrency",
        values: "<n>",
        default: "unset — omitted means no cap",
        sentence: "Most lanes of this profile running at once; 0 is unlimited. A cap on the \
                   harness — a local model's own count is `models.<glob>.slots`.",
    },
    Reference {
        key: "agents.<profile>.session_reuse_ctx",
        values: "0, 1..=100",
        default: "20",
        sentence: "How large a carried session may be, as a percentage of the model's \
                    context window, before a fresh one opens instead; 0 is off.",
    },
    Reference {
        key: "agents.<profile>.session_blocked_ctx",
        values: "0, 1..=100",
        default: "40",
        sentence: "Percentage of the model's context window a *running* lane's last turn may \
                    reach before the dispatcher stops it and blocks the task; 0 is off. Must be \
                    above `session_reuse_ctx` when both are nonzero. The reading is only taken \
                    at turn boundaries, so the ceiling can be overshot.",
    },
    Reference {
        key: "agents.<profile>.permission_mode",
        values: "<mode>",
        default: "the kind's own first mode (absent on a kind with none)",
        sentence: "Which approval mode this profile's lanes are started with. Absent on a \
                    kind that has none.",
    },
    Reference {
        key: "models.<glob>.context_window",
        values: "<tokens>",
        default: "0 (unset)",
        sentence: "What one session of this model gets to work in. `session_reuse_ctx` and \
                    `session_blocked_ctx` take their percentage of it, so a session step \
                    against a model with this unset never reuses or blocks on size.",
    },
    Reference {
        key: "models.<glob>.compact_ctx",
        values: "1..=100",
        default: "(unset)",
        sentence: "Percentage of the agent's own window at which its auto-compaction \
                    fires. Unset keeps the agent's default. Must be below \
                    `session_blocked_ctx` on every step that pairs them. Claude and codex \
                    only.",
    },
    Reference {
        key: "models.<glob>.input",
        values: "<USD per 1M tokens>",
        default: "0",
        sentence: "Cost per million input tokens.",
    },
    Reference {
        key: "models.<glob>.output",
        values: "<USD per 1M tokens>",
        default: "0",
        sentence: "Cost per million output tokens.",
    },
    Reference {
        key: "models.<glob>.cache_read",
        values: "<USD per 1M tokens>",
        default: "0",
        sentence: "Cost per million cached tokens read.",
    },
    Reference {
        key: "models.<glob>.cache_write_5m",
        values: "<USD per 1M tokens>",
        default: "0",
        sentence: "Cost per million tokens written to a 5-minute cache.",
    },
    Reference {
        key: "models.<glob>.cache_write_1h",
        values: "<USD per 1M tokens>",
        default: "0",
        sentence: "Cost per million tokens written to a 1-hour cache.",
    },
    Reference {
        key: TIER_RATE_KEY,
        values: "<USD per 1M tokens>",
        default: "0",
        sentence: "The same five rates, charged once a prompt passes N thousand tokens. \
                    One such tier per model.",
    },
    Reference {
        key: "models.<glob>.prompt_cache_ttl",
        values: "<duration>",
        default: "5m (none if local)",
        sentence: "How long a session's prompt cache is trusted to stay warm. A resume \
                    past it opens a fresh session. \"0\" turns it off.",
    },
    Reference {
        key: "models.<glob>.slots",
        values: "<n>",
        default: "0",
        sentence: "How many lanes this model may run at once, replacing its \
                    profile's `concurrency`; 0 leaves the profile in charge.",
    },
    Reference {
        key: "models.<glob>.local",
        values: "true, false",
        default: "false",
        sentence: "Whether this model runs on hardware you own. Setting it on a model that \
                    carries `slots` silences doctor's note that it should \
                    probably say so. It also removes the 5m `prompt_cache_ttl` default from \
                    this model.",
    },
];

/// The one-sentence note for a dotted key, if it has one.
///
/// Matched on the leaf, the way [`REFERENCE`]'s own keys are written: a
/// concrete key like `agents.claude.permission_mode` and the reference's own
/// `agents.<profile>.permission_mode` share a leaf either way, so one row
/// covers every profile or model glob that has that field.
///
/// A tier rate is the exception: its leaf is a base rate's leaf too, and
/// matching on it alone would note `models.m.above_100k_tokens.input` with
/// the base sentence, beside a rate that only applies past the threshold. So
/// a key whose segment before the leaf reads as a tier takes the tier row.
pub fn note(key: &str) -> Option<&'static str> {
    let leaf = key.rsplit('.').next().unwrap_or(key);
    let parent = key.rsplit('.').nth(1);
    if key.starts_with("models.")
        && parent.is_some_and(|p| crate::usage::PriceTier::threshold_in(p).is_some())
    {
        return REFERENCE
            .iter()
            .find(|r| r.key == TIER_RATE_KEY)
            .map(|r| r.sentence);
    }
    REFERENCE
        .iter()
        .find(|r| r.key.rsplit('.').next() == Some(leaf))
        .map(|r| r.sentence)
}

/// The reference table [`crate::config::Config::render`] writes at the top of
/// every `config.toml`: every key, its possible values, its default and one
/// sentence about it, pointing at the doc that says more.
///
/// Unfenced, unlike a pipeline file's own key reference — `config.toml` is
/// spoolway's from top to bottom, so there is no project prose beside it that
/// a marker would need to set it apart from.
pub fn reference_table() -> String {
    let key_w = REFERENCE.iter().map(|r| r.key.len()).max().unwrap_or(0);
    let val_w = REFERENCE.iter().map(|r| r.values.len()).max().unwrap_or(0);
    let def_w = REFERENCE.iter().map(|r| r.default.len()).max().unwrap_or(0);

    let mut out = String::new();
    out.push_str("# Full reference: docs/configuration.md\n#\n");
    out.push_str(&format!(
        "# {:key_w$}  {:val_w$}  {:def_w$}  Meaning\n",
        "Key", "Values", "Default"
    ));
    for r in REFERENCE {
        out.push_str(&format!(
            "# {:key_w$}  {:val_w$}  {:def_w$}  {}\n",
            r.key, r.values, r.default, r.sentence
        ));
    }
    out.push('\n');
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Number,
    Bool,
    /// Comma-separated list of scalars.
    List,
}

/// Every scalar and list setting, in file order.
pub fn entries(config: &Config) -> Result<Vec<Entry>> {
    let value = Value::try_from(config).context("rendering config")?;
    let mut out = Vec::new();
    walk(&value, &mut String::new(), &mut out);
    Ok(out)
}

/// The flat `[dispatch]` keys `get` and `set` accept that [`entries`]
/// never lists, because they are skipped from the file while they hold their
/// default — see [`unset_value`], which resolves each. [`all_settings`] adds
/// these plus the per-entry omissions (`[models]` zeros, an absent
/// `agents.<profile>.concurrency`) for every profile and model glob the
/// config already carries; only a `[models]` glob nobody has named yet stays
/// off the list, and no list could show that open-ended keyspace.
pub const OMITTED_DEFAULT_KEYS: &[&str] = &[
    "dispatch.lane_child_ceiling",
    "dispatch.priority",
    "dispatch.keep_finished_lanes",
];

/// Every scalar setting `config get`/`set` resolves for a key that already
/// names something: [`entries`] plus the omitted-default keys it skips — the
/// flat [`OMITTED_DEFAULT_KEYS`], the `concurrency` of every profile
/// that omits it, and every price/limit field of every `[models]` glob the
/// config already carries. This is the list `spoolway config list` prints, so
/// its promise to name every settable scalar key holds for every key that
/// resolves to a value today.
///
/// A `[models]` glob the config has never named is still settable — [`set`]
/// creates it on first write — so that keyspace is open-ended and no list can
/// enumerate it.
///
/// Sorted by key, unlike [`entries`]'s file order: the omitted keys are found
/// last and would otherwise trail the list, away from the table they belong
/// to.
pub fn all_settings(config: &Config) -> Result<Vec<Entry>> {
    let mut out = entries(config)?;

    let push_omitted = |out: &mut Vec<Entry>, key: String| {
        if out.iter().any(|e| e.key == key) {
            return;
        }
        let Some(value) = unset_value(config, &key) else {
            return;
        };
        let kind = match value.as_str() {
            "true" | "false" => Kind::Bool,
            other if !other.is_empty() && other.parse::<f64>().is_ok() => Kind::Number,
            _ => Kind::Text,
        };
        let note = note(&key);
        out.push(Entry {
            key,
            value,
            kind,
            note,
        });
    };

    for key in OMITTED_DEFAULT_KEYS {
        push_omitted(&mut out, (*key).to_string());
    }
    for profile in config.agents.keys() {
        push_omitted(&mut out, format!("agents.{profile}.concurrency"));
    }
    // The field *names* on `[models]`, read off the probe the same way
    // `models_key` does — a rendered default names none, since a zero is
    // omitted. Scalars only: the probe's tier sub-table is not a setting, and
    // its rates are listed below for a glob that has a tier.
    let scalar_fields = |value: Option<Value>| -> Vec<String> {
        match value {
            Some(Value::Table(table)) => table
                .iter()
                .filter(|(_, value)| is_scalar(value))
                .map(|(key, _)| key.clone())
                .collect(),
            _ => Vec::new(),
        }
    };
    let price_fields = scalar_fields(Value::try_from(crate::usage::ModelPrice::probe()).ok());
    let tier_fields = scalar_fields(tier_probe());
    for (glob, price) in &config.models {
        for field in &price_fields {
            push_omitted(&mut out, format!("models.{glob}.{field}"));
        }
        if let Some(tier) = price.tier {
            for field in &tier_fields {
                push_omitted(&mut out, format!("models.{glob}.{}.{field}", tier.key()));
            }
        }
    }
    // The omitted keys are appended after everything the file carries, so
    // without this an absent `agents.pi.concurrency` would print at the
    // bottom of the list rather than beside the rest of `agents.pi`. A person
    // scanning for one setting reads the key column, so sort by it.
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

fn walk(value: &Value, path: &mut String, out: &mut Vec<Entry>) {
    match value {
        Value::Table(table) => {
            for (key, child) in table {
                let mark = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(key);
                walk(child, path, out);
                path.truncate(mark);
            }
        }
        Value::Array(items) => {
            // A list of scalars is one editable field; anything deeper is not
            // something a line editor can meaningfully represent.
            if items.iter().all(is_scalar) {
                out.push(Entry {
                    key: path.clone(),
                    value: items
                        .iter()
                        .map(render_scalar)
                        .collect::<Vec<_>>()
                        .join(", "),
                    kind: Kind::List,
                    note: note(path),
                });
            }
        }
        scalar => out.push(Entry {
            key: path.clone(),
            value: render_scalar(scalar),
            kind: kind_of(scalar),
            note: note(path),
        }),
    }
}

fn is_scalar(value: &Value) -> bool {
    !matches!(value, Value::Table(_) | Value::Array(_))
}

fn kind_of(value: &Value) -> Kind {
    match value {
        Value::Boolean(_) => Kind::Bool,
        Value::Integer(_) | Value::Float(_) => Kind::Number,
        _ => Kind::Text,
    }
}

fn render_scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Read one dotted key.
///
/// A key whose *unset* form is its absence still answers here, with the value
/// that absence stands for. `[models]` zeros and `agents.<profile>.concurrency`
/// are left out of the file rather than written — see [`set`] — and a reader
/// asking what a setting currently is deserves the answer rather than "no such
/// key", which is what a genuine typo gets.
pub fn get(config: &Config, key: &str) -> Result<String> {
    if let Some(entry) = entries(config)?.into_iter().find(|entry| entry.key == key) {
        return Ok(entry.value);
    }
    if let Some(unset) = unset_value(config, key) {
        return Ok(unset);
    }
    if let Some(hint) = retired_hint(key) {
        bail!("unknown key `{key}`\n  {hint}");
    }
    bail!("no config key `{key}`{}", nearest(config, key))
}

/// A sentence for the one retired key whose absence is worth more than "no
/// config key" — somebody who used to set this is told where it went,
/// rather than left to search for a setting `nearest` cannot find, since
/// nothing left in [`REFERENCE`] is even a substring of its name.
fn retired_hint(key: &str) -> Option<&'static str> {
    match key {
        "dispatch.interval" => {
            Some("the dispatcher polls at a fixed rate; there is nothing to set")
        }
        _ => None,
    }
}

/// What an omitted key reads as, for the two kinds of key that may be omitted.
///
/// `None` for anything else, which is how a typo stays a typo.
fn unset_value(config: &Config, key: &str) -> Option<String> {
    // A `[dispatch]` value that is skipped on the way out while it holds its
    // default. Absence *is* the default here rather than a zero, so this reads
    // the value off the struct: a reader who asks what the setting currently is
    // gets its default, not "no such key". The write path needs it too — `set`
    // refuses a key that does not resolve, so without this the key would be
    // documented in the reference table and settable by nothing.
    if key == "dispatch.lane_child_ceiling" {
        return Some(crate::config::format_duration(
            config.dispatch.lane_child_ceiling,
        ));
    }
    if key == "dispatch.priority" {
        return Some(match config.dispatch.priority {
            Priority::Group => "group".to_string(),
            Priority::Any => "any".to_string(),
        });
    }
    if key == "dispatch.keep_finished_lanes" {
        return Some(config.dispatch.keep_finished_lanes.to_string());
    }
    let Some(parts) = models_key(config, key) else {
        concurrency_key(config, key)?;
        return Some("0".to_string());
    };
    // The zero of the field's own type, read off the struct rather than
    // spelled out here, the same way `ensure_models_entry` reads it.
    Some(match models_field_probe(&parts)? {
        Value::Integer(_) => "0".to_string(),
        Value::Float(_) => "0.0".to_string(),
        Value::Boolean(_) => "false".to_string(),
        Value::String(_) => String::new(),
        _ => return None,
    })
}

/// Write one dotted key, keeping its existing type.
///
/// The result is deserialised back into `Config` before being returned, so an
/// edit that would produce an invalid config fails here rather than at the
/// next dispatch pass.
pub fn set(config: &Config, key: &str, input: &str) -> Result<Config> {
    let mut value = Value::try_from(config).context("rendering config")?;

    // `[models]` is the one section that ships empty and is meant to be filled
    // in: every other key exists before you can set it, but a price or a
    // window for a model spoolway never named cannot. So a new glob is
    // created on first write rather than rejected as unknown.
    let parts = match models_key(config, key) {
        Some(parts) => {
            ensure_models_entry(&mut value, &parts)?;
            parts
        }
        None => {
            // `agents.<profile>.concurrency` is the one other key that may be
            // absent from a config that is perfectly valid: it is omitted
            // wherever a profile asserts no cap, which is every local profile
            // spoolway ships. The profile itself still has to exist, so a
            // misspelt profile name is refused exactly as before — it is the
            // one field inside it that may be created.
            if let Some(profile) = concurrency_key(config, key) {
                ensure_concurrency(&mut value, profile)?;
            } else {
                // Every other key has to already exist, so that a typo is a
                // refusal rather than a setting nothing will ever read.
                get(config, key)?;
                // …and one of them can pass that check while still being
                // absent from the rendered document: a `[dispatch]` key
                // skipped on the way out while it holds its default. `get`
                // answers for it out of the struct, so the walk below has to
                // have something to write into.
                ensure_dispatch_default(&mut value, key, config);
            }
            key.split('.').collect()
        }
    };

    let parts: Vec<&str> = parts;
    let mut cursor = &mut value;
    for part in &parts[..parts.len() - 1] {
        cursor = cursor
            .get_mut(*part)
            .with_context(|| format!("no config section `{part}` in `{key}`"))?;
    }

    let leaf = parts[parts.len() - 1];
    let slot = cursor
        .get_mut(leaf)
        .with_context(|| format!("no config key `{key}`"))?;

    *slot = coerce(slot, input).with_context(|| format!("`{key}` cannot be set to `{input}`"))?;

    let models_glob = (parts.first() == Some(&"models") && parts.len() >= 3).then(|| parts[1]);
    let had_row = models_glob.is_some_and(|glob| config.models.contains_key(glob));
    let had_tier = models_glob.is_some()
        && parts.len() == 4
        && config
            .models
            .get(parts[1])
            .is_some_and(|row| row.tier.is_some());
    let mut config: Config = value
        .try_into()
        .with_context(|| format!("setting `{key}` to `{input}` produces an invalid config"))?;

    // A rate typed as zero on a glob that had no row, or a tier rate typed as
    // zero where the row had no tier, leaves an empty row (or an empty tier)
    // behind. Both serialise to no table at all, so the saved file would
    // differ from this config by a table it cannot spell, and `save_key`
    // would refuse the write for it. They are dropped here, as the file never
    // had them; zero means unset, so nothing is lost.
    // Only a `models.*` key can have made one: any other key's second part is
    // not a glob, and a row that happens to share its name is not ours to drop.
    if let Some(glob) = models_glob {
        if parts.len() == 4
            && !had_tier
            && let Some(row) = config.models.get_mut(glob)
            && row
                .tier
                .is_some_and(|tier| tier.rates == crate::usage::Rates::default())
        {
            row.tier = None;
        }
        if !had_row && config.models.get(glob) == Some(&crate::usage::ModelPrice::default()) {
            config.models.remove(glob);
        }
    }

    // A profile switched onto another kind keeps the mode it had on the old
    // one, which the new kind may not accept (`auto` is claude's, `never`
    // is codex's). The mode is settled onto the new kind's default so the
    // switch is one call; `migrate` below only fills a blank one.
    let switched = key
        .strip_prefix("agents.")
        .and_then(|rest| rest.strip_suffix(".kind"))
        .and_then(|name| config.agents.get_mut(name))
        .filter(|profile| {
            crate::agent::adapter(&profile.kind).is_some()
                && profile.permission_mode_status().is_err()
        });
    if let Some(profile) = switched {
        profile.permission_mode.clear();
    }

    // A blank `permission_mode` is refused by name — only when that is the
    // key a person just typed. Any other edit (switching a profile's `kind`,
    // say) can leave a *different* profile's mode blank relative to its kind
    // without anybody having typed a blank anywhere, and that is exactly what
    // `migrate` is for: settle it onto the kind's own default the same way a
    // freshly loaded config would, rather than refusing an edit that never
    // touched the field at all.
    if !key.ends_with(".permission_mode") {
        config.migrate();
    }

    // Cross-field checks serde cannot make, at the one moment a person is
    // typing the value and can fix it. Everything else here is caught by
    // deserialising, but a permission mode is only wrong relative to the `kind`
    // beside it, so it would otherwise be written happily and fail much later.
    // Not a cross-field check: a file edited by hand to a `compact_ctx` over
    // 100 still loads, so `set` refuses it on the next edit rather than
    // writing it back out.
    for (glob, price) in &config.models {
        if price.compact_ctx > 100 {
            bail!(
                "`models.{glob}.compact_ctx` must be between 1 and 100 (1..=100), got {}; \
                 `spoolway config set` cannot unset a key, so correct the line with \
                 `spoolway config edit`, or delete it to keep the agent's own default",
                price.compact_ctx
            );
        }
    }
    for (name, profile) in &config.agents {
        // A kind with no adapter row is `doctor`'s to fail, not a reason to
        // refuse an edit of some other key: a mode means nothing relative to
        // a kind spoolway cannot read, and it is kept as written.
        if crate::agent::adapter(&profile.kind).is_some() {
            profile
                .permission_mode_status()
                .with_context(|| format!("`agents.{name}.permission_mode`"))?;
        }
        if profile.session_reuse_ctx != 0 && !(1..=100).contains(&profile.session_reuse_ctx) {
            bail!(
                "`agents.{name}.session_reuse_ctx` must be 0 (off) or between 1 and 100 \
                 (1..=100), got {}",
                profile.session_reuse_ctx
            );
        }
        if profile.session_blocked_ctx != 0 && !(1..=100).contains(&profile.session_blocked_ctx) {
            bail!(
                "`agents.{name}.session_blocked_ctx` must be 0 (off) or between 1 and 100 \
                 (1..=100), got {}",
                profile.session_blocked_ctx
            );
        }
        // Both directions of the same rule, caught wherever the edit landed:
        // a ceiling set at or under the reuse threshold, or a reuse threshold
        // raised to meet an already-set ceiling. Either way a task blocked on
        // the ceiling would have the very session that just blocked it
        // carried right back in — `carried_session` reuses anything at or
        // under `session_reuse_ctx` — and block again on the next turn.
        if profile.session_reuse_ctx != 0
            && profile.session_blocked_ctx != 0
            && profile.session_blocked_ctx <= profile.session_reuse_ctx
        {
            bail!(
                "`session_blocked_ctx` ({}) must be above `session_reuse_ctx` ({}) — a task \
                 blocked below the reuse threshold carries the same session back and re-blocks",
                profile.session_blocked_ctx,
                profile.session_reuse_ctx,
            );
        }
    }

    Ok(config)
}

/// Refuse a value a person typed that can never be right, naming what is
/// allowed — for `spoolway config set` only.
///
/// Not part of [`set`], which the override layer also loads through: that
/// layer has to keep accepting what the loader accepts, such as a
/// `dispatch.backend = "tmux"` that has always loaded as herdr. Typed text is
/// the one place a value saved as a different one than typed can be stopped.
pub fn check_typed(key: &str, input: &str) -> Result<()> {
    // Deserialising accepts the retired `tmux` and would save `herdr`, a
    // different value from the one typed.
    if key == "dispatch.backend" {
        crate::config::Backend::parse_typed(input).with_context(|| key.to_string())?;
    }
    // Zero is how the struct spells "unset", so the range cannot be held there:
    // a typed `0` would be saved as a key left out, a different statement from
    // the one made.
    if key.starts_with("models.") && key.ends_with(".compact_ctx") {
        match input.trim().parse::<u32>() {
            Ok(1..=100) => {}
            _ => bail!(
                "`{key}` must be a whole percentage between 1 and 100, got `{input}`; \
                 there is no value that turns compaction off, and `spoolway config set` \
                 cannot unset a key, so to keep the agent's own default delete the line \
                 with `spoolway config edit`"
            ),
        }
    }
    // A price or a spending limit below zero, or one that is not a finite
    // number, can never be right. `coerce` takes any `f64`, so `-5` and `inf`
    // would be saved: a negative `max_cost_usd` reads as no limit, and an
    // infinite rate prices every run at infinity. `models refresh` already
    // drops such a rate from the shared table. Text that is not a number at
    // all is left for `coerce` to word.
    if is_money_key(key)
        && let Ok(number) = input.trim().parse::<f64>()
        && !(number.is_finite() && number >= 0.0)
    {
        bail!(
            "`{key}` must be a finite number of dollars, 0 or more, got `{input}`; \
             a negative or infinite value is never a price or a limit"
        );
    }
    // A hook that is not a bare filename can never run, whatever else is set
    // later, so it is refused rather than left for `doctor` to fail.
    if key == "issue_tracking.hook"
        && let Some(problem) = crate::tracking::not_bare_filename_problem(input)
    {
        bail!("{key}: it {problem}");
    }
    Ok(())
}

/// What a rate saved as zero means.
///
/// Zero is how a row spells "unset", and `config set` prints the saved `= 0.0`
/// and nothing else, which reads as "free". A base rate is filled from the
/// price table by [`crate::models::resolve`], so for a model the table knows
/// the table's rate stays in effect. A tier rate is not: a row's tier replaces
/// the table's tier whole, so a zero inside it is charged as zero (the
/// one-hour cache rate excepted, which follows the five-minute one).
///
/// What is named as in effect is read from `resolve` over the project's own
/// rows, never from the table alone: when no row of the glob's own is left, a
/// broader `[models]` row such as `claude-*` answers the model, and the table's
/// figure would be the wrong one.
fn zero_rate_note(config: &Config, key: &str) -> Option<String> {
    let parts = split_models_key(key)?;
    let (glob, field) = (parts[1], *parts.last()?);
    if !MODEL_RATES.contains(&field) {
        return None;
    }
    let rate_of = |rates: &crate::usage::Rates| match field {
        "input" => rates.input,
        "output" => rates.output,
        "cache_read" => rates.cache_read,
        "cache_write_5m" => rates.cache_write_5m,
        _ => rates.cache_write_1h,
    };
    let row = config.models.get(glob).copied().unwrap_or_default();
    let table = crate::models::resolve(&Default::default(), glob).price;
    if parts.len() == 4 {
        let Some(tier) = row.tier else {
            // The empty tier was dropped before saving (see `set`), so the
            // row carries none, and `overlay` takes the table's tier only
            // when the row sets no base rate of its own.
            let answer = crate::models::resolve(&config.models, glob).price?;
            let from = if answer.tier == table.and_then(|price| price.tier) {
                "the price table's"
            } else {
                "the matching `[models]` row's"
            };
            return Some(match answer.tier {
                Some(tier) => format!(
                    "`{key}` = 0 sets no tier: `{glob}` keeps {from} tier above {}k tokens",
                    tier.above_k
                ),
                None => format!(
                    "`{key}` = 0 sets no tier: `{glob}` is priced at its base rates at any size"
                ),
            });
        };
        if rate_of(&tier.rates) != 0.0 {
            return None;
        }
        return Some(
            if field == "cache_write_1h" && tier.rates.cache_write_5m != 0.0 {
                format!(
                    "`{key}` = 0 means unset: the hourly cache is priced at the tier's \
                     five-minute rate"
                )
            } else {
                format!(
                    "`{key}` = 0 is charged as zero, not filled from the price table: \
                     above {}k tokens that part of a request is free",
                    tier.above_k
                )
            },
        );
    }
    if rate_of(&row.rates()) != 0.0 {
        return None;
    }
    // An hourly cache-write rate beside a five-minute one is priced like it,
    // and never from the table (see `overlay`).
    if field == "cache_write_1h" && row.cache_write_5m != 0.0 {
        return Some(format!(
            "`{key}` = 0 means unset: the hourly cache is priced at the five-minute rate"
        ));
    }
    let rate = rate_of(&crate::models::resolve(&config.models, glob).price?.rates());
    if rate == 0.0 {
        return None;
    }
    let from = if table.is_some_and(|price| rate_of(&price.rates()) == rate) {
        "the price table's rate"
    } else {
        "the rate of the `[models]` row that matches it"
    };
    Some(format!(
        "`{key}` = 0 means unset, not free: {from} for `{glob}` \
         (${rate} per million tokens) stays in effect"
    ))
}

/// Whether `key` holds dollars: a `models.*` rate, tier rates included, or the
/// unattended cost ceiling.
fn is_money_key(key: &str) -> bool {
    key == "unattended.max_cost_usd"
        || split_models_key(key)
            .and_then(|parts| parts.last().copied())
            .is_some_and(|field| MODEL_RATES.contains(&field))
}

/// The `models.<glob>` fields that are a price per million tokens.
const MODEL_RATES: [&str; 5] = [
    "input",
    "output",
    "cache_read",
    "cache_write_5m",
    "cache_write_1h",
];

/// What `doctor` would fail on because of the agent `key` just named, for a
/// value that is valid but not yet usable, and what a zero rate really means
/// (see [`zero_rate_note`]).
///
/// These are warnings rather than refusals: a script sets keys in sequence, so
/// `unattended.blocked_agent` may name an agent the next command adds. The
/// blocked-agent line comes from `Config::agent`, the lookup `doctor`'s agent
/// rows make; the zero-rate line is a note `doctor` does not fail on. The
/// other file-based checks are run by [`crate::commands::doctor::failures_after_set`].
pub fn warnings(config: &Config, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    out.extend(zero_rate_note(config, key));
    if key == "unattended.blocked_agent" && config.agent(&config.unattended.blocked_agent).is_err()
    {
        out.push(format!(
            "no agent `{}` in [agents] yet — `spoolway doctor` fails until one is added",
            config.unattended.blocked_agent
        ));
    }
    out
}

/// The parts of a dotted key, as they address it in the file.
///
/// Almost always `key.split('.')`, and the exception is why this exists: a
/// model glob is a name out of someone else's catalogue and quite reasonably
/// holds a dot of its own. Shared with [`crate::confdoc`] so that the key an
/// edit validates and the key it writes are split the same way.
pub fn parts<'a>(config: &Config, key: &'a str) -> Vec<&'a str> {
    models_key(config, key).unwrap_or_else(|| key.split('.').collect())
}

/// Split `models.<model glob>.<field>` into `[models, glob, field]`, or
/// `models.<model glob>.above_<N>k_tokens.<field>` into its four parts, or
/// `None` if this is not a models key. Does not check that the field exists.
///
/// Split from the right rather than on every dot, because a model glob is a
/// name from someone else's catalogue and quite reasonably contains one:
/// `models.gpt-4.1-*.input` is three parts, not four. A segment before the
/// field that reads as a tier name is taken as the tier, not as the end of
/// the glob. Every caller that addresses a models row by a dotted key goes
/// through here, so how such a key is split changes in one place.
pub(crate) fn split_models_key(key: &str) -> Option<Vec<&str>> {
    let rest = key.strip_prefix("models.")?;
    let (head, field) = rest.rsplit_once('.')?;
    let parts = match head.rsplit_once('.') {
        Some((glob, tier)) if crate::usage::PriceTier::threshold_in(tier).is_some() => {
            vec!["models", glob, tier, field]
        }
        _ => vec!["models", head, field],
    };
    if parts[1].is_empty() {
        return None;
    }
    Some(parts)
}

/// [`split_models_key`], for a key whose field `ModelPrice` actually has.
fn models_key<'a>(config: &Config, key: &'a str) -> Option<Vec<&'a str>> {
    let parts = split_models_key(key)?;
    // Only fields ModelPrice actually has, so a misspelt one is still refused.
    let _ = config;
    models_field_probe(&parts)?;
    Some(parts)
}

/// The probe's value for the field a [`models_key`] split names — a scalar
/// whose type is the field's, or `None` for a field spoolway does not know.
///
/// Against `probe` rather than `default`: a default is all zeros now, and a
/// zero is omitted from the serialised form, so a default would name no
/// fields at all and every models key would read as a typo. A table is not a
/// field, so the probe's own tier sub-table never answers a three-part key.
fn models_field_probe(parts: &[&str]) -> Option<Value> {
    let table = match parts.len() {
        4 => tier_probe()?,
        _ => Value::try_from(crate::usage::ModelPrice::probe()).ok()?,
    };
    table
        .get(parts[parts.len() - 1])
        .filter(|value| is_scalar(value))
        .cloned()
}

/// The rates a tier sub-table holds, rendered from the probe's own tier so
/// the keys are read off the struct like every other `[models]` field.
fn tier_probe() -> Option<Value> {
    let tier = crate::usage::ModelPrice::probe().tier?;
    Value::try_from(tier.rates).ok()
}

/// The profile name in `agents.<profile>.concurrency`, or `None` if this is
/// some other key.
///
/// The profile has to be one the config actually defines: creating the field
/// is allowed, inventing the profile around it is not.
fn concurrency_key<'a>(config: &Config, key: &'a str) -> Option<&'a str> {
    let rest = key.strip_prefix("agents.")?;
    let profile = rest.strip_suffix(".concurrency")?;
    config.agents.contains_key(profile).then_some(profile)
}

/// Put `concurrency = 0` on a profile that omits it, so the generic walk below
/// has something to write into. Zero because that is what its absence already
/// means — the value is overwritten a moment later either way.
/// Put a `[dispatch]` key that is omitted while it holds its default back into
/// the rendered document, so [`set`] has a slot to write into.
///
/// The value seeded is the one the key currently *means* — read off the config
/// rather than assumed — because the very next thing [`set`] does is overwrite
/// it, and a wrong seed would only matter if the write failed.
///
/// Silent when the key is not one of these: the caller has already established
/// that it resolves, and a key that resolves *and* is present needs nothing.
fn ensure_dispatch_default(value: &mut Value, key: &str, config: &Config) {
    let Some(field) = key.strip_prefix("dispatch.") else {
        return;
    };
    let seed = match field {
        "priority" => Value::try_from(config.dispatch.priority)
            .expect("Priority always serialises to a string"),
        "lane_child_ceiling" => Value::String(crate::config::format_duration(
            config.dispatch.lane_child_ceiling,
        )),
        "keep_finished_lanes" => Value::Boolean(config.dispatch.keep_finished_lanes),
        _ => return,
    };
    if let Some(table) = value.get_mut("dispatch").and_then(Value::as_table_mut) {
        table.entry(field).or_insert(seed);
    }
}

fn ensure_concurrency(value: &mut Value, profile: &str) -> Result<()> {
    let Some(table) = value
        .get_mut("agents")
        .and_then(|agents| agents.get_mut(profile))
        .and_then(Value::as_table_mut)
    else {
        bail!("config has no agent profile `{profile}`");
    };
    table
        .entry("concurrency")
        .or_insert_with(|| Value::Integer(0));
    Ok(())
}

/// Put an entry at `models.<glob>` if there is none yet, its tier sub-table
/// when the key names one, and the one field about to be written if *it* is
/// missing, so the generic walk below has something to write into.
///
/// Both halves are needed now that a zero is omitted: a fresh entry serialises
/// to an empty table, and even an entry that already exists holds only the
/// fields somebody has set. The field is seeded with the zero of its own type,
/// taken from [`crate::usage::ModelPrice::probe`] so the shape is read off the
/// struct rather than repeated here — the value is overwritten a moment later
/// either way.
///
/// A tier with a different threshold from one the entry already has is still
/// created here; deserialising the result is what refuses a second tier.
fn ensure_models_entry(value: &mut Value, parts: &[&str]) -> Result<()> {
    let glob = parts[1];
    let field = parts[parts.len() - 1];
    let table = value
        .get_mut("models")
        .context("config has no `models` section")?;
    let Some(table) = table.as_table_mut() else {
        bail!("`models` is not a table");
    };
    let entry = table
        .entry(glob.to_string())
        .or_insert_with(|| Value::Table(Default::default()));
    let Some(mut entry) = entry.as_table_mut() else {
        bail!("`models.{glob}` is not a table");
    };
    if parts.len() == 4 {
        let tier = parts[2];
        let Some(table) = entry
            .entry(tier.to_string())
            .or_insert_with(|| Value::Table(Default::default()))
            .as_table_mut()
        else {
            bail!("`models.{glob}.{tier}` is not a table");
        };
        entry = table;
    }
    if !entry.contains_key(field) {
        let zero = match models_field_probe(parts).as_ref() {
            Some(Value::Integer(_)) => Value::Integer(0),
            Some(Value::Float(_)) => Value::Float(0.0),
            Some(Value::Boolean(_)) => Value::Boolean(false),
            Some(Value::String(_)) => Value::String(String::new()),
            _ => bail!("`models.<glob>.{field}` is not a field spoolway knows"),
        };
        entry.insert(field.to_string(), zero);
    }
    Ok(())
}

/// Parse `input` into whatever shape the current value has.
fn coerce(current: &Value, input: &str) -> Result<Value> {
    Ok(match current {
        Value::Boolean(_) => match input.trim() {
            "true" | "yes" | "on" | "1" => Value::Boolean(true),
            "false" | "no" | "off" | "0" => Value::Boolean(false),
            other => bail!("expected true or false, got `{other}`"),
        },
        Value::Integer(_) => Value::Integer(
            input
                .trim()
                .parse()
                .with_context(|| format!("expected a whole number, got `{input}`"))?,
        ),
        Value::Float(_) => Value::Float(
            input
                .trim()
                .parse()
                .with_context(|| format!("expected a number, got `{input}`"))?,
        ),
        Value::Array(items) => {
            let text_items = input
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| Value::String(s.to_string()))
                .collect::<Vec<_>>();
            if !items.iter().all(is_scalar) {
                bail!("this list holds structured entries and cannot be edited as text");
            }
            Value::Array(text_items)
        }
        _ => Value::String(input.to_string()),
    })
}

/// A "did you mean" tail for an unknown key.
fn nearest(config: &Config, key: &str) -> String {
    let Ok(entries) = entries(config) else {
        return String::new();
    };
    let needle = key.to_ascii_lowercase();
    let close: Vec<String> = entries
        .into_iter()
        .map(|e| e.key)
        .filter(|k| k.contains(&needle) || needle.contains(k.as_str()))
        .take(4)
        .collect();

    if close.is_empty() {
        String::new()
    } else {
        format!(" — did you mean {}?", close.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scalar_setting_is_listed() {
        let config = Config::default();
        let entries = entries(&config).unwrap();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();

        assert!(keys.contains(&"dispatch.lane_quiet"));
        assert!(keys.contains(&"agents.pi.kind"));
        // `permission_mode` is listed for a kind that has modes and not for
        // one that does not: it is omitted wherever the kind carries none,
        // which is `pi` among the shipped profiles.
        assert!(keys.contains(&"agents.claude.permission_mode"));
        assert!(!keys.contains(&"agents.pi.permission_mode"));

        // `concurrency` is absent for every shipped profile: it is omitted
        // wherever it would be zero. `set` still creates it — see
        // `a_concurrency_can_be_set_on_a_profile_that_omits_it` below — so the
        // key being absent from this list costs nothing but the row.
        assert!(!keys.contains(&"agents.claude.concurrency"));
        assert!(!keys.contains(&"agents.pi.concurrency"));

        // Retired: no longer in the file at all, so nothing here to get or set.
        for gone in [
            "agents.pi.model",
            "agents.pi.sandbox",
            "agents.pi.env",
            "agents.pi.session_reuse_uncached",
            "agents.pi.quota_ceiling",
            "paths.prompts",
            "effort.tier_models.high",
            "sandbox.mode",
            "docs.path",
            "docs.format",
        ] {
            assert!(
                !keys.contains(&gone),
                "`{gone}` is retired and must not be listed"
            );
        }
    }

    /// A setting that ships empty on a kind that offers no modes at all
    /// cannot be set — there is nothing for the mode to mean.
    ///
    /// `set` refuses a key that is not already in the file, and a field that
    /// `skip_serializing_if` hides while empty is never in the file until it
    /// has been set — which for `pi` it cannot be, since `pi` carries no
    /// permission modes to pick from.
    #[test]
    fn a_kind_with_no_modes_cannot_have_its_permission_mode_set() {
        let config = Config::default();
        let keys: Vec<String> = entries(&config)
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();

        assert!(
            !keys.iter().any(|k| k == "agents.pi.permission_mode"),
            "`pi` has no permission modes, so the key must not be listed"
        );
        assert!(get(&config, "agents.pi.permission_mode").is_err());
        assert!(set(&config, "agents.pi.permission_mode", "auto").is_err());
    }

    /// Switching a profile onto a kind that has modes leaves its
    /// `permission_mode` blank without anybody having typed a blank — `pi`
    /// carries none, so its field is empty before the edit. That is not the
    /// same failure as typing a blank directly: `set` settles it onto the new
    /// kind's own default instead of refusing an edit that never touched
    /// `permission_mode` at all.
    #[test]
    fn switching_a_profile_onto_a_kind_with_modes_fills_its_blank_mode() {
        let config = Config::default();
        assert!(config.agents["pi"].permission_mode.is_empty());

        let updated = set(&config, "agents.pi.kind", "codex").unwrap();
        assert_eq!(updated.agents["pi"].kind, "codex");
        assert_eq!(updated.agents["pi"].permission_mode, "never");
    }

    /// The other direction: switching a profile off a kind with modes leaves
    /// its `permission_mode` set to a mode the new kind knows nothing about.
    /// That is not a person typing a bad value either, so `set` clears it
    /// rather than refusing an edit that never touched `permission_mode`.
    #[test]
    fn switching_a_profile_off_a_kind_with_modes_clears_its_mode() {
        let config = Config::default();
        assert_eq!(config.agents["claude"].permission_mode, "auto");

        let updated = set(&config, "agents.claude.kind", "pi").unwrap();
        assert_eq!(updated.agents["claude"].kind, "pi");
        assert!(updated.agents["claude"].permission_mode.is_empty());
    }

    /// A kind that does offer modes ships with a real one already set, and
    /// `set` still changes it the ordinary way.
    #[test]
    fn a_kind_with_modes_ships_a_real_one_and_can_be_changed() {
        let config = Config::default();
        assert_eq!(config.agents["claude"].permission_mode, "auto");

        let updated = set(
            &config,
            "agents.claude.permission_mode",
            "bypassPermissions",
        )
        .unwrap();
        assert_eq!(
            updated.agents["claude"].permission_mode,
            "bypassPermissions"
        );
    }

    /// `unattended.skip_blocked_lane` is retired, the same road
    /// `dispatch.blocked_takes_over` — the key it replaced — took:
    /// `get`/`set` never resolve it, so this reaches `config.rs`'s own
    /// refusal on load rather than confkv's read/write-while-absent path.
    /// Clearing a block reads the reported verb now — see
    /// [`crate::commands::cleared_block_target`].
    #[test]
    fn skip_blocked_lane_is_refused_rather_than_resolved() {
        let err = get(&Config::default(), "unattended.skip_blocked_lane").unwrap_err();
        assert!(err.to_string().contains("no config key"), "{err}");
    }

    /// A config naming the retired `dispatch.tear_lanes_on_stop` is refused
    /// outright — `get`/`set` never resolve it, so this reaches `config.rs`'s
    /// own refusal on load rather than confkv's read/write-while-absent path.
    #[test]
    fn tear_lanes_on_stop_is_refused_rather_than_resolved() {
        let err = get(&Config::default(), "dispatch.tear_lanes_on_stop").unwrap_err();
        assert!(err.to_string().contains("no config key"), "{err}");
    }

    /// `dispatch.tmux_mode` retired with the tmux backend itself. The field
    /// stays on `DispatchConfig` so a config still holding it parses — see
    /// [`crate::config::DispatchConfig`] — but it left `REFERENCE`, so
    /// `get`/`set` never resolve it and a person naming it is told so rather
    /// than handed a value nothing reads.
    #[test]
    fn tmux_mode_is_refused_rather_than_resolved() {
        let err = get(&Config::default(), "dispatch.tmux_mode").unwrap_err();
        assert!(err.to_string().contains("no config key"), "{err}");
    }

    /// `dispatch.worktree_root` retired along with the setting it named —
    /// every worktree lands under the project home now, with no way to
    /// move it. The field stays on `DispatchConfig` so a config still
    /// holding it parses — see [`crate::config::DispatchConfig`] — but it
    /// left `REFERENCE`, so `get`/`set` never resolve it and a person naming
    /// it is told so rather than handed a value nothing reads.
    #[test]
    fn worktree_root_is_refused_rather_than_resolved() {
        let err = get(&Config::default(), "dispatch.worktree_root").unwrap_err();
        assert!(err.to_string().contains("no config key"), "{err}");
    }

    /// The same read/write-while-absent mechanism, for `keep_finished_lanes`.
    #[test]
    fn keep_finished_lanes_is_readable_and_writable_while_absent() {
        let config = Config::default();
        assert_eq!(
            get(&config, "dispatch.keep_finished_lanes").unwrap(),
            "true"
        );

        let off = set(&config, "dispatch.keep_finished_lanes", "false").unwrap();
        assert!(!off.dispatch.keep_finished_lanes);
        assert_eq!(get(&off, "dispatch.keep_finished_lanes").unwrap(), "false");
        assert!(
            toml::to_string(&off)
                .unwrap()
                .contains("keep_finished_lanes = false")
        );

        let back = set(&off, "dispatch.keep_finished_lanes", "true").unwrap();
        assert!(back.dispatch.keep_finished_lanes);
        assert!(
            !toml::to_string(&back)
                .unwrap()
                .contains("keep_finished_lanes")
        );
    }

    /// The same read/write-while-absent mechanism, for `lane_child_ceiling`.
    #[test]
    fn lane_child_ceiling_is_readable_and_writable_while_absent() {
        let config = Config::default();
        assert_eq!(get(&config, "dispatch.lane_child_ceiling").unwrap(), "1h");
        assert!(
            !toml::to_string(&config)
                .unwrap()
                .contains("lane_child_ceiling")
        );

        let changed = set(&config, "dispatch.lane_child_ceiling", "30m").unwrap();
        assert_eq!(
            changed.dispatch.lane_child_ceiling,
            std::time::Duration::from_secs(30 * 60)
        );
        assert_eq!(get(&changed, "dispatch.lane_child_ceiling").unwrap(), "30m");
        assert!(
            toml::to_string(&changed)
                .unwrap()
                .contains("lane_child_ceiling")
        );

        let back = set(&changed, "dispatch.lane_child_ceiling", "1h").unwrap();
        assert_eq!(
            back.dispatch.lane_child_ceiling,
            std::time::Duration::from_secs(3600)
        );
        assert!(
            !toml::to_string(&back)
                .unwrap()
                .contains("lane_child_ceiling")
        );
    }

    /// The same read/write-while-absent mechanism, for `priority`.
    #[test]
    fn priority_is_readable_and_writable_while_absent() {
        let config = Config::default();
        assert_eq!(get(&config, "dispatch.priority").unwrap(), "group");
        assert!(!toml::to_string(&config).unwrap().contains("priority"));

        let any = set(&config, "dispatch.priority", "any").unwrap();
        assert_eq!(any.dispatch.priority, Priority::Any);
        assert_eq!(get(&any, "dispatch.priority").unwrap(), "any");
        assert!(
            toml::to_string(&any)
                .unwrap()
                .contains("priority = \"any\"")
        );

        let group = set(&any, "dispatch.priority", "group").unwrap();
        assert_eq!(group.dispatch.priority, Priority::Group);
        assert!(!toml::to_string(&group).unwrap().contains("priority"));
    }

    /// A negative or non-finite price or cost limit is refused by name; zero
    /// and an ordinary figure are not. Fails before the check: `check_typed`
    /// let all of them through.
    #[test]
    fn a_negative_or_infinite_price_or_limit_is_refused() {
        for key in [
            "models.m.input",
            "models.m.cache_write_1h",
            "models.m.above_200k_tokens.output",
            "unattended.max_cost_usd",
        ] {
            for typed in ["-5", "-0.01", "inf", "-inf", "NaN"] {
                let err = format!("{:#}", check_typed(key, typed).unwrap_err());
                assert!(err.contains(key) && err.contains("0 or more"), "{err}");
            }
            for typed in ["0", "2.5", "abc"] {
                check_typed(key, typed).unwrap();
            }
        }
        // Not a dollar amount.
        check_typed("models.m.slots", "-5").unwrap();
    }

    /// A zero rate on a glob with no row saves, instead of being refused for
    /// the empty row it leaves behind, and notes that a known model keeps the
    /// table's rate. Fails before the fix: `set` kept the empty row.
    #[test]
    fn a_zero_rate_on_a_fresh_glob_saves_and_names_the_table_rate() {
        let config = set(&Config::default(), "models.claude-opus-5-5.input", "0").unwrap();
        assert!(!config.models.contains_key("claude-opus-5-5"));
        let note = zero_rate_note(&config, "models.claude-opus-5-5.input").unwrap();
        assert!(note.contains("price table's rate") && note.contains("stays in effect"));
        assert_eq!(zero_rate_note(&config, "models.no-such-model.input"), None);

        let priced = set(&Config::default(), "models.claude-opus-5-5.input", "3").unwrap();
        assert_eq!(
            zero_rate_note(&priced, "models.claude-opus-5-5.input"),
            None
        );
    }

    /// A zero tier rate saves and says what it means, instead of being refused
    /// for the empty tier it leaves behind. Fails before the fix: the empty
    /// tier stayed in the config and `save_key` refused the write.
    #[test]
    fn a_zero_tier_rate_saves_and_says_what_it_means() {
        let key = "models.claude-haiku-5-5.above_100k_tokens.input";
        let fresh = set(&Config::default(), key, "0").unwrap();
        assert_eq!(fresh.models.get("claude-haiku-5-5"), None);
        let note = zero_rate_note(&fresh, key).unwrap();
        assert!(note.contains("keeps the price table's tier"), "{note}");

        let tiered = set(&Config::default(), key, "2").unwrap();
        let zeroed = set(&tiered, key, "0").unwrap();
        let note = zero_rate_note(&zeroed, key).unwrap();
        assert!(note.contains("charged as zero"), "{note}");
    }

    /// Zeroing a models rate must not drop an unrelated empty row that shares
    /// a name with the second segment of a non-models key. Fails before the
    /// guard: `issue_tracking.hook` removed the empty `models.hook` row and
    /// `save_key` then refused the write.
    #[test]
    fn a_non_models_key_leaves_an_empty_models_row_alone() {
        let mut config = Config::default();
        config.models.insert("hook".into(), Default::default());
        let after = set(&config, "issue_tracking.hook", "").unwrap();
        assert!(after.models.contains_key("hook"));
    }

    /// With a broader row answering the model, the zero-rate note names that
    /// row's rate, not the price table's. Fails before: the note quoted the
    /// table's $4 while `claude-*` priced the model at $7.
    #[test]
    fn a_zero_rate_note_names_the_row_that_answers_not_the_table() {
        let covered = set(&Config::default(), "models.claude-*.input", "7").unwrap();
        let key = "models.claude-opus-5-5.input";
        let zeroed = set(&covered, key, "0").unwrap();
        let note = zero_rate_note(&zeroed, key).unwrap();
        assert!(
            note.contains("`[models]` row") && note.contains("$7"),
            "{note}"
        );
        assert!(!note.contains("price table's"), "{note}");
    }

    #[test]
    fn kinds_are_inferred_from_the_value() {
        let config = Config::default();
        let entries = entries(&config).unwrap();
        let kind = |key: &str| entries.iter().find(|e| e.key == key).unwrap().kind;

        assert_eq!(kind("agents.claude.session_reuse_ctx"), Kind::Number);
        assert_eq!(kind("agents.pi.kind"), Kind::Text);
        assert_eq!(kind("housekeeping.update_check"), Kind::Bool);
    }

    /// A list of scalars is one editable field, whichever key it sits under —
    /// pinned directly against a synthetic value now that no field in the
    /// default config is itself a list, the way `skills` used to be.
    #[test]
    fn a_list_of_scalars_gets_kind_list() {
        let mut path = String::new();
        let mut out = Vec::new();
        walk(
            &Value::Array(vec![Value::String("a".into()), Value::String("b".into())]),
            &mut path,
            &mut out,
        );
        assert_eq!(out[0].kind, Kind::List);
    }

    /// A profile that omits `concurrency` can still be given one, the way a
    /// `[models]` glob spoolway never named can. Without this, omitting the key
    /// would make it unsettable except by hand — the whole reason every other
    /// setting is written out even at its default.
    #[test]
    fn a_concurrency_can_be_set_on_a_profile_that_omits_it() {
        let config = Config::default();
        assert_eq!(config.agents["pi"].concurrency, 0);

        let updated = set(&config, "agents.pi.concurrency", "4").unwrap();
        assert_eq!(updated.agents["pi"].concurrency, 4);
        // And back to none, which is what writing zero means. The key is in
        // the file at that point; the next whole-file render drops it again.
        let updated = set(&updated, "agents.pi.concurrency", "0").unwrap();
        assert_eq!(updated.agents["pi"].concurrency, 0);

        // The profile still has to exist: creating the field is allowed,
        // inventing the profile around it is not.
        assert!(set(&config, "agents.nope.concurrency", "4").is_err());
    }

    /// A profile whose `concurrency` is omitted reloads as *no cap*, not as
    /// whatever `AgentProfile::default()` happens to carry. The two disagreeing
    /// is how a config saying nothing about a cap comes back asserting one.
    #[test]
    fn an_omitted_concurrency_reloads_as_no_cap() {
        let config = Config::default();
        let text = toml::to_string_pretty(&config).unwrap();
        assert!(
            !text.contains("concurrency = 0"),
            "a zero concurrency is written out: {text}"
        );

        let reparsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(reparsed.agents["pi"].concurrency, 0);
        assert_eq!(reparsed.agents["claude"].concurrency, 0);
    }

    #[test]
    fn setting_a_value_keeps_its_type() {
        let config = Config::default();

        let updated = set(&config, "agents.pi.concurrency", "4").unwrap();
        assert_eq!(updated.agents["pi"].concurrency, 4);
    }

    /// `compact_ctx` is a whole percentage from 1 to 100. A typed `0` is
    /// refused rather than saved as the key left out, and it is not written at
    /// all while unset.
    // covers: models.<glob>.compact_ctx — a model's compaction percentage is 1..=100 and omitted when unset
    #[test]
    fn a_models_compact_ctx_takes_one_to_a_hundred_and_is_omitted_when_unset() {
        let key = "models.claude-opus-5.compact_ctx";
        for typed in ["0", "101", "-1", "half", "30.5"] {
            let err = format!("{:#}", check_typed(key, typed).unwrap_err());
            assert!(err.contains("between 1 and 100"), "{typed}: {err}");
        }
        for typed in ["1", "30", "100"] {
            check_typed(key, typed).unwrap();
        }
        // A file edited by hand to 101 is refused the next time `set` runs.
        let mut config = Config::default();
        config.models.insert(
            "m".to_string(),
            crate::usage::ModelPrice {
                compact_ctx: 101,
                ..Default::default()
            },
        );
        let err = set(&config, "models.m.input", "1.0")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("compact_ctx") && err.contains("between 1 and 100"),
            "{err}"
        );

        let config = set(&Config::default(), key, "30").unwrap();
        assert_eq!(config.models["claude-opus-5"].compact_ctx, 30);
        assert_eq!(get(&config, key).unwrap(), "30");
        let written = toml::to_string(&config).unwrap();
        assert!(written.contains("compact_ctx = 30"), "{written}");

        let unset = set(&Config::default(), "models.claude-opus-5.input", "5.0").unwrap();
        let written = toml::to_string(&unset).unwrap();
        assert!(!written.contains("compact_ctx"), "{written}");
    }

    /// `[models]` ships empty, so — like `[pricing]` before it — a glob is
    /// created on first write rather than refused as an unknown key.
    #[test]
    fn a_models_entry_is_created_on_first_write() {
        let config = Config::default();
        assert!(config.models.is_empty());

        let updated = set(&config, "models.claude-opus-5.input", "5.0").unwrap();
        assert_eq!(updated.models["claude-opus-5"].input, 5.0);
        // The rest of the entry exists too, all zero, ready for the next key.
        assert_eq!(updated.models["claude-opus-5"].context_window, 0);

        let updated = set(&updated, "models.claude-opus-5.context_window", "1000000").unwrap();
        assert_eq!(updated.models["claude-opus-5"].context_window, 1_000_000);
    }

    /// A model's slot budget and its `local` flag are set the same way as any
    /// other `[models]` field — on a glob nothing has written yet — and each
    /// reads back the value that its absence stands for. The retired
    /// `exclusive` is no longer a field that can be set.
    #[test]
    fn a_models_slots_and_local_fields_can_be_set() {
        let config = Config::default();

        let updated = set(&config, "models.my-local-*.slots", "3").unwrap();
        assert_eq!(updated.models["my-local-*"].slots, 3);

        assert!(set(&updated, "models.my-local-*.exclusive", "true").is_err());

        // Registered, so it is readable before it has ever been written…
        assert_eq!(get(&config, "models.my-local-*.local").unwrap(), "false");
        // …and writable on a glob config never named.
        let updated = set(&updated, "models.my-local-*.local", "true").unwrap();
        assert!(updated.models["my-local-*"].local);
        assert_eq!(get(&updated, "models.my-local-*.local").unwrap(), "true");
    }

    #[test]
    fn a_bad_value_is_refused_rather_than_written() {
        let config = Config::default();

        assert!(set(&config, "agents.pi.concurrency", "lots").is_err());
        // Durations are validated by the real deserialiser, not by this module.
        assert!(set(&config, "dispatch.lane_quiet", "every so often").is_err());
        assert!(set(&config, "dispatch.lane_quiet", "5m").is_ok());
    }

    #[test]
    fn a_backend_that_cannot_be_saved_as_typed_is_refused_naming_the_allowed_ones() {
        let config = Config::default();

        for typed in ["tmux", "zellij"] {
            let err = format!("{:#}", check_typed("dispatch.backend", typed).unwrap_err());
            assert!(
                err.contains("not a backend — use herdr or headless"),
                "{err}"
            );
        }
        check_typed("dispatch.backend", "headless").unwrap();
        // `set` itself still accepts what the loader does, so the override
        // layer's `tmux` keeps loading, as herdr.
        let updated = set(&config, "dispatch.backend", "tmux").unwrap();
        assert_eq!(get(&updated, "dispatch.backend").unwrap(), "herdr");
        let updated = set(&config, "dispatch.backend", "headless").unwrap();
        assert_eq!(get(&updated, "dispatch.backend").unwrap(), "headless");
    }

    #[test]
    fn a_hook_that_can_never_run_is_refused_and_a_bare_one_saved() {
        let config = Config::default();

        let err = format!(
            "{:#}",
            check_typed("issue_tracking.hook", "../evil.sh").unwrap_err()
        );
        assert!(err.contains("not a bare filename"), "{err}");
        check_typed("issue_tracking.hook", "record.sh").unwrap();
        check_typed("issue_tracking.hook", "").unwrap();
        let updated = set(&config, "issue_tracking.hook", "record.sh").unwrap();
        assert_eq!(get(&updated, "issue_tracking.hook").unwrap(), "record.sh");
    }

    #[test]
    fn a_blocked_agent_not_yet_defined_is_saved_with_a_warning() {
        let config = Config::default();

        let updated = set(&config, "unattended.blocked_agent", "ghost").unwrap();
        assert_eq!(get(&updated, "unattended.blocked_agent").unwrap(), "ghost");
        let found = warnings(&updated, "unattended.blocked_agent");
        assert_eq!(found.len(), 1);
        assert!(found[0].contains("no agent `ghost` in [agents] yet"));
        assert!(found[0].contains("`spoolway doctor` fails until one is added"));

        // A defined agent, and an unrelated key, are both silent.
        let defined = set(&config, "unattended.blocked_agent", "claude").unwrap();
        assert!(warnings(&defined, "unattended.blocked_agent").is_empty());
        assert!(warnings(&updated, "housekeeping.retention_days").is_empty());
    }

    /// `dispatch.interval` is gone, not merely undocumented: this project no
    /// longer offers a way to tune how often a pass runs.
    #[test]
    fn dispatch_interval_is_an_unknown_key() {
        let config = Config::default();
        let get_err = get(&config, "dispatch.interval").unwrap_err().to_string();
        assert!(
            get_err.contains("the dispatcher polls at a fixed rate; there is nothing to set"),
            "{get_err}"
        );
        let set_err = set(&config, "dispatch.interval", "5s")
            .unwrap_err()
            .to_string();
        assert!(
            set_err.contains("the dispatcher polls at a fixed rate; there is nothing to set"),
            "{set_err}"
        );
    }

    /// Cross-field check, not serde: any `u8` deserialises into
    /// `session_reuse_ctx`, so the range is enforced here, at the moment a
    /// person is typing the value and can fix it.
    #[test]
    fn a_session_reuse_ctx_accepts_off_and_refuses_values_above_its_range() {
        let config = Config::default();

        let off = set(&config, "agents.claude.session_reuse_ctx", "0")
            .map(|c| c.agents["claude"].session_reuse_ctx);
        assert_eq!(off.ok(), Some(0), "0 is off, and always allowed");

        let err = set(&config, "agents.claude.session_reuse_ctx", "101")
            .unwrap_err()
            .to_string();
        assert!(err.contains("1..=100"), "{err}");

        // Under the shipped `session_blocked_ctx` of 40, which it must stay below.
        assert!(set(&config, "agents.claude.session_reuse_ctx", "30").is_ok());
    }

    /// When both guards are enabled, a blocked ceiling set at or under the
    /// reuse ceiling is refused in both edit directions: either edit would put
    /// the same oversized session straight back into the task.
    // covers: agents.<profile>.session_blocked_ctx — the ceiling on a running lane's size
    #[test]
    fn a_session_blocked_ctx_at_or_under_the_reuse_threshold_is_refused() {
        let config = set(&Config::default(), "agents.claude.session_blocked_ctx", "0").unwrap();
        let config = set(&config, "agents.claude.session_reuse_ctx", "0").unwrap();
        // With reuse off there is no reuse threshold for the blocked ceiling
        // to sit above.
        assert!(set(&config, "agents.claude.session_blocked_ctx", "1").is_ok());

        let config = set(&config, "agents.claude.session_reuse_ctx", "50").unwrap();

        let err = set(&config, "agents.claude.session_blocked_ctx", "40")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "`session_blocked_ctx` (40) must be above `session_reuse_ctx` (50) — a task \
                 blocked below the reuse threshold carries the same session back and re-blocks"
            ),
            "{err}"
        );
        // Equal is refused too, not only strictly under.
        assert!(set(&config, "agents.claude.session_blocked_ctx", "50").is_err());
        assert!(set(&config, "agents.claude.session_blocked_ctx", "51").is_ok());

        // The other direction: a ceiling is already set, and raising
        // `session_reuse_ctx` to meet or pass it is refused the same way.
        let with_ceiling = set(&config, "agents.claude.session_blocked_ctx", "80").unwrap();
        let err = set(&with_ceiling, "agents.claude.session_reuse_ctx", "80")
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be above `session_reuse_ctx`"), "{err}");
        assert!(set(&with_ceiling, "agents.claude.session_reuse_ctx", "79").is_ok());

        // `0` is off and is never compared against the reuse threshold.
        assert!(set(&config, "agents.claude.session_blocked_ctx", "0").is_ok());
    }

    /// `quota_ceiling` is retired along with the quota gate it configured —
    /// gone from the reference table, so neither `get` nor `set` knows it any
    /// more, the same as every other retired key.
    #[test]
    fn a_retired_quota_ceiling_is_an_unknown_key() {
        let config = Config::default();
        assert!(get(&config, "agents.claude.quota_ceiling").is_err());
        assert!(set(&config, "agents.claude.quota_ceiling", "85").is_err());
    }

    #[test]
    fn an_unknown_key_suggests_something() {
        let config = Config::default();
        let err = get(&config, "agents.pi.modell").unwrap_err().to_string();
        assert!(err.contains("no config key"));
    }

    /// The note travels with the entry so both surfaces — the settings screen
    /// and the reference table on top of the file — explain a setting the
    /// same way, and every setting has one now: each carries its own
    /// sentence, distinct from its neighbour's.
    #[test]
    fn every_setting_carries_its_own_explanation() {
        // Both set, not one: a field left at zero is omitted from the file and
        // so from this list, which is the whole point of `[models]` no longer
        // printing six rates a local model does not have.
        let config = set(&Config::default(), "models.claude-opus-5.input", "5.0").unwrap();
        let config = set(&config, "models.claude-opus-5.context_window", "1000000").unwrap();
        let entries = entries(&config).unwrap();
        let entry = |key: &str| entries.iter().find(|e| e.key == key).unwrap();

        let window = entry("models.claude-opus-5.context_window");
        // The window is what `session_reuse_ctx`/`session_blocked_ctx` take a
        // percentage of — not a planning-only number (finding 26).
        let window_note = window.note.unwrap();
        assert!(window_note.contains("session_reuse_ctx"));
        assert!(!window_note.contains("planning"));
        let input = entry("models.claude-opus-5.input");
        assert!(input.note.unwrap().contains("input tokens"));
        assert_ne!(window.note, input.note);
    }

    #[test]
    fn round_trips_through_get() {
        let config = Config::default();
        assert_eq!(get(&config, "dispatch.lane_quiet").unwrap(), "15m");
        assert_eq!(get(&config, "agents.claude.kind").unwrap(), "claude");
    }

    /// The mockup's own default, and the one value that turns the sweep off.
    #[test]
    fn retention_days_round_trips_and_zero_means_off() {
        let config = Config::default();
        assert_eq!(get(&config, "housekeeping.retention_days").unwrap(), "30");

        let off = set(&config, "housekeeping.retention_days", "0").unwrap();
        assert_eq!(off.housekeeping.retention_days, 0);
        assert_eq!(get(&off, "housekeeping.retention_days").unwrap(), "0");

        let ninety = set(&off, "housekeeping.retention_days", "90").unwrap();
        assert_eq!(ninety.housekeeping.retention_days, 90);
    }

    /// Finished tasks are kept unless a person says otherwise, so the default
    /// is the off value and only an explicit number turns the archive sweep on.
    #[test]
    fn archive_retention_days_defaults_to_off_and_round_trips() {
        let config = Config::default();
        assert_eq!(
            get(&config, "housekeeping.archive_retention_days").unwrap(),
            "0"
        );

        let ninety = set(&config, "housekeeping.archive_retention_days", "90").unwrap();
        assert_eq!(ninety.housekeeping.archive_retention_days, 90);
        assert_eq!(
            get(&ninety, "housekeeping.archive_retention_days").unwrap(),
            "90"
        );
        assert_eq!(ninety.housekeeping.retention_days, 30);
    }

    #[test]
    fn price_table_age_round_trips_and_zero_means_quiet() {
        let config = Config::default();
        assert_eq!(
            get(&config, "housekeeping.price_max_age_days").unwrap(),
            "30"
        );

        let off = set(&config, "housekeeping.price_max_age_days", "0").unwrap();
        assert_eq!(off.housekeeping.price_max_age_days, 0);
        assert_eq!(get(&off, "housekeeping.price_max_age_days").unwrap(), "0");
    }

    /// The four one-key tables the mockup folds into `[housekeeping]` are
    /// gone by their old names — `config list` (via [`entries`]) must name
    /// none of them, only the housekeeping keys they became.
    #[test]
    fn the_old_one_key_tables_are_gone_and_housekeeping_names_every_key() {
        let config = Config::default();
        let keys: Vec<String> = entries(&config)
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();

        for gone in [
            "update.check",
            "calibrate.window",
            "retention.days",
            "prices.max_age_days",
            "stack.summary.agent",
            "stack.summary.model",
            "stack.summary.effort",
            "stack.summary.prompt",
            "pipeline_gen.pipeline_agent",
            "pipeline_gen.pipeline_model",
            "pipeline_gen.pipeline_effort",
            "pipeline_gen.pipeline_auto",
            "pipeline_gen.pipeline_loop_default",
            "pipeline_gen.pipeline_local_models",
        ] {
            assert!(
                !keys.contains(&gone.to_string()),
                "`{gone}` is retired and must not be listed"
            );
        }
        for present in [
            "housekeeping.update_check",
            "housekeeping.calibrate_window",
            "housekeeping.retention_days",
            "housekeeping.archive_retention_days",
            "housekeeping.price_max_age_days",
        ] {
            assert!(
                keys.contains(&present.to_string()),
                "`{present}` must be listed"
            );
        }
    }

    /// A tier rate is set through its nested key, on a glob nothing has
    /// written yet, and reads back as the value written.
    #[test]
    fn a_models_tier_rate_is_created_on_first_write_and_reads_back() {
        let key = "models.claude-haiku-5-5.above_100k_tokens.input";
        assert_eq!(get(&Config::default(), key).unwrap(), "0.0");

        let updated = set(&Config::default(), key, "0.5").unwrap();
        let tier = updated.models["claude-haiku-5-5"].tier.unwrap();
        assert_eq!(tier.above_k, 100);
        assert_eq!(tier.rates.input, 0.5);
        assert_eq!(get(&updated, key).unwrap(), "0.5");

        // A glob with a dot in it still splits right.
        let dotted = "models.gpt-4.1-*.above_128k_tokens.cache_write_1h";
        let updated = set(&updated, dotted, "2").unwrap();
        assert_eq!(
            updated.models["gpt-4.1-*"]
                .tier
                .unwrap()
                .rates
                .cache_write_1h,
            2.0
        );

        // The tier's unset rates are listed for a glob that has a tier.
        let listed = all_settings(&updated).unwrap();
        assert!(
            listed
                .iter()
                .any(|e| e.key == "models.claude-haiku-5-5.above_100k_tokens.output")
        );
        assert!(!listed.iter().any(|e| e.key.contains("above_1k_tokens")));
    }

    /// A tier rate shares its leaf with a base rate, but its note says it
    /// applies past the threshold rather than repeating the base sentence.
    #[test]
    fn a_models_tier_rate_takes_the_tier_note_not_the_base_one() {
        let base = note("models.m.input").unwrap();
        let tier = note("models.m.above_100k_tokens.input").unwrap();
        assert_eq!(base, "Cost per million input tokens.");
        assert!(
            tier.contains("once a prompt passes N thousand tokens"),
            "{tier}"
        );
        assert_eq!(
            note("models.gpt-4.1-*.above_128k_tokens.cache_write_1h"),
            Some(tier)
        );
        assert!(reference_table().contains(TIER_RATE_KEY));
    }

    #[test]
    fn a_models_tier_key_that_names_no_rate_or_a_second_tier_is_refused() {
        let config = set(
            &Config::default(),
            "models.m.above_100k_tokens.input",
            "0.5",
        )
        .unwrap();
        assert!(set(&config, "models.m.above_100k_tokens.slots", "2").is_err());
        assert!(set(&config, "models.m.above_100k_tokens", "2").is_err());
        let err = set(&config, "models.m.above_200k_tokens.input", "1").unwrap_err();
        assert!(format!("{err:#}").contains("one price tier"), "{err:#}");
    }
}
