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

/// Every setting in `config.toml`, in the order the reference table prints
/// them.
pub const REFERENCE: &[Reference] = &[
    Reference {
        key: "skills",
        values: "<names>",
        default: "spoolway-plan",
        sentence: "Which skills `spoolway eval` gives a block of their own; every other \
                    name is left off the table.",
    },
    Reference {
        key: "dispatch.backend",
        values: "herdr, tmux, headless",
        default: "herdr",
        sentence: "Where lanes run: a real pane you can watch, or no multiplexer at all.",
    },
    Reference {
        key: "dispatch.herdr_mode",
        values: "grouped, split",
        default: "grouped",
        sentence: "How a herdr run is laid out: a pane per task in its project's shared \
                    tab, or a row of its own per task.",
    },
    Reference {
        key: "dispatch.tmux_mode",
        values: "grouped, split",
        default: "grouped",
        sentence: "How a tmux run is laid out: one session for the whole run, or a \
                    session per task.",
    },
    Reference {
        key: "dispatch.worktree_root",
        values: "<path>",
        default: "(blank)",
        sentence: "Where a dispatched task's worktree is cut, whatever the backend; \
                    blank means `~/.spoolway/<project>/worktrees`.",
    },
    Reference {
        key: "dispatch.interval",
        values: "<duration>",
        default: "10s",
        sentence: "How long the dispatcher waits between passes.",
    },
    Reference {
        key: "dispatch.lane_quiet",
        values: "<duration>",
        default: "15m",
        sentence: "How long a lane may say nothing before the dispatcher reminds it to \
                    report. Not the same thing as `interval`, which is only how often a \
                    pass looks.",
    },
    Reference {
        key: "dispatch.default_pipeline",
        values: "<name>",
        default: "default",
        sentence: "Which pipeline a task runs on when its own `pipeline:` is absent.",
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
        key: "dispatch.tear_lanes_on_stop",
        values: "true, false",
        default: "true",
        sentence: "Whether stopping the dispatcher ends the run's live lanes and takes \
                    their worktrees with them; branches and a task on `blocked` are never \
                    swept.",
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
                    starting work, priced off assets/model-prices.json; 0 is no ceiling.",
    },
    Reference {
        key: "unattended.skip_blocked_lane",
        values: "true, false",
        default: "true",
        sentence: "Whether clearing a block carries the task past the step it blocked on, \
                    on the grounds that the unblocker did that step's work; false hands it \
                    back to that step instead.",
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
        key: "pipeline_gen.pipeline_agent",
        values: "<profile>",
        default: "claude",
        sentence: "Which `[agents.*]` profile `spoolway pipeline gen` opens its session as.",
    },
    Reference {
        key: "pipeline_gen.pipeline_model",
        values: "<model>",
        default: "(blank)",
        sentence: "The model that session runs; blank refuses the command.",
    },
    Reference {
        key: "pipeline_gen.pipeline_effort",
        values: "<level>",
        default: "(blank)",
        sentence: "How hard that model thinks; blank means the kind's own default.",
    },
    Reference {
        key: "pipeline_gen.pipeline_auto",
        values: "true, false",
        default: "false",
        sentence: "Whether the generation procedure asks before writing, or takes its own \
                    recommendation.",
    },
    Reference {
        key: "pipeline_gen.pipeline_loop_default",
        values: "<n>",
        default: "1",
        sentence: "The budget every loop a generated pipeline writes starts at.",
    },
    Reference {
        key: "pipeline_gen.pipeline_local_models",
        values: "true, false",
        default: "false",
        sentence: "Whether local models are involved in what gets generated — pre-answers \
                    the procedure's first question.",
    },
    Reference {
        key: "update.check",
        values: "true, false",
        default: "true",
        sentence: "Whether spoolway tells a person at a keyboard that a newer release is out.",
    },
    Reference {
        key: "calibrate.window",
        values: "<duration>",
        default: "14d",
        sentence: "How far back `spoolway-calibrate` reads: archived tasks finished inside \
                    the window, and the ledger entries beside them.",
    },
    Reference {
        key: "retention.days",
        values: "<days>",
        default: "30",
        sentence: "How long system-prompts/, commands/, tracking/, headless/, scratch/ \
                    and archive/ keep an entry before it is deleted; 0 keeps \
                    everything forever. queue/, pending/, worktrees/ and plans/ are \
                    never swept.",
    },
    Reference {
        key: "stack.summary.agent",
        values: "<profile>",
        default: "(blank)",
        sentence: "Agent profile from `[agents.*]` that writes the pull request's title \
                    and summary before `spoolway stack` opens it. Blank alongside `model`, \
                    the whole task file is the pull request body instead.",
    },
    Reference {
        key: "stack.summary.model",
        values: "<name>",
        default: "(blank)",
        sentence: "The model that profile's kind is started with, for the summary turn. \
                    Blank alongside `agent`, the whole task file is the pull request body \
                    instead.",
    },
    Reference {
        key: "stack.summary.effort",
        values: "<string>",
        default: "(blank)",
        sentence: "Passed to the agent kind's effort flag, same as a pipeline step's \
                    `effort:`. Blank means no flag is sent.",
    },
    Reference {
        key: "stack.summary.prompt",
        values: "<name>",
        default: "summariser",
        sentence: "Prompt under `.spoolway/prompts/` the summary turn runs.",
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
        default: "1 (cloud), unset elsewhere — omitted means no cap",
        sentence: "Most lanes of this profile running at once; 0 is unlimited. A cap on the \
                   harness — a local model's own count is `models.<glob>.slots`.",
    },
    Reference {
        key: "agents.<profile>.session_reuse_ctx",
        values: "1..=100",
        default: "50",
        sentence: "How large a carried session may be, as a percentage of the model's \
                    context window, before a fresh one opens instead.",
    },
    Reference {
        key: "agents.<profile>.session_blocked_ctx",
        values: "0, 1..=100",
        default: "0",
        sentence: "Percentage of the model's context window a *running* lane's last turn may \
                    reach before the dispatcher stops it and blocks the task; 0 is off. Must be \
                    above `session_reuse_ctx`. The reading is only taken at turn boundaries, so \
                    the ceiling can be overshot.",
    },
    Reference {
        key: "agents.<profile>.quota_ceiling",
        values: "0, 1..=100",
        default: "0",
        sentence: "Ceiling on this profile's own kind's cached usage percentage (either \
                    window); at or above it a pass starts no new lane and parks every \
                    candidate task instead. 0 is off. An unavailable or stale quota reading \
                    holds new launches; `spoolway agent verify` diagnoses the reading.",
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
        key: "models.<glob>.session_reuse_idle",
        values: "<duration>",
        default: "(unset)",
        sentence: "How long a carried session may sit before it is refused rather than \
                    resumed. Never set it on a local model.",
    },
    Reference {
        key: "models.<glob>.slots",
        values: "<n>",
        default: "0",
        sentence: "How many lanes this model may run at once, replacing its \
                    profile's `concurrency`; 0 leaves the profile in charge.",
    },
    Reference {
        key: "models.<glob>.exclusive",
        values: "true, false",
        default: "false",
        sentence: "Whether this model refuses to run alongside a different \
                    `exclusive` one — one set of weights on the card at a time.",
    },
    Reference {
        key: "models.<glob>.local",
        values: "true, false",
        default: "false",
        sentence: "Whether this model runs on hardware you own — a board line asks \
                    others off the card when a queued task routes to it, and it changes \
                    nothing else.",
    },
    Reference {
        key: "issue_tracking.hook",
        values: "<filename>",
        default: "(blank)",
        sentence: "A bare filename, resolved inside `.spoolway/hooks/`; blank runs no hook \
                    and changes nothing about a task's `queued`, `blocked`, `paused` or \
                    `done`.",
    },
    Reference {
        key: "issue_tracking.project_key",
        values: "<string>",
        default: "(blank)",
        sentence: "Opaque to spoolway — `owner/repo` on github, a project key on jira — \
                    handed to the hook verbatim as `SPOOLWAY_PROJECT_KEY`.",
    },
    Reference {
        key: "issue_tracking.on_fail",
        values: "ignore, pause",
        default: "ignore",
        sentence: "What a non-zero hook exit does: `ignore` only records it, `pause` also \
                    holds the task — on `queued` to `paused`, on `done` out of the archive.",
    },
    Reference {
        key: "issue_tracking.key_in_names",
        values: "true, false",
        default: "false",
        sentence: "Whether a `slug=` the hook answers prefixes the group, branch and \
                    worktree name `queue add` generates — `task/<slug>-<id>`. Off changes \
                    nothing.",
    },
];

/// The one-sentence note for a dotted key, if it has one.
///
/// Matched on the leaf, the way [`REFERENCE`]'s own keys are written: a
/// concrete key like `agents.claude.permission_mode` and the reference's own
/// `agents.<profile>.permission_mode` share a leaf either way, so one row
/// covers every profile or model glob that has that field.
pub fn note(key: &str) -> Option<&'static str> {
    let leaf = key.rsplit('.').next().unwrap_or(key);
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

/// The three flat `[dispatch]` keys `get` and `set` accept that [`entries`]
/// never lists, because they are skipped from the file while they hold their
/// default — see [`unset_value`], which resolves each. [`all_settings`] adds
/// these plus the per-entry omissions (`[models]` zeros, an absent
/// `agents.<profile>.concurrency`) for every profile and model glob the
/// config already carries; only a `[models]` glob nobody has named yet stays
/// off the list, and no list could show that open-ended keyspace.
pub const OMITTED_DEFAULT_KEYS: &[&str] = &[
    "dispatch.tear_lanes_on_stop",
    "dispatch.lane_child_ceiling",
    "dispatch.priority",
];

/// Every scalar setting `config get`/`set` resolves for a key that already
/// names something: [`entries`] plus the omitted-default keys it skips — the
/// three flat [`OMITTED_DEFAULT_KEYS`], the `concurrency` of every profile
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
    // omitted.
    let price_fields: Vec<String> = match Value::try_from(crate::usage::ModelPrice::probe()) {
        Ok(Value::Table(table)) => table.keys().cloned().collect(),
        _ => Vec::new(),
    };
    for glob in config.models.keys() {
        for field in &price_fields {
            push_omitted(&mut out, format!("models.{glob}.{field}"));
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
    bail!("no config key `{key}`{}", nearest(config, key))
}

/// What an omitted key reads as, for the two kinds of key that may be omitted.
///
/// `None` for anything else, which is how a typo stays a typo.
fn unset_value(config: &Config, key: &str) -> Option<String> {
    // A `[dispatch]` bool that is skipped on the way out while it holds its
    // default. Absence *is* the default here rather than a zero, so this reads
    // the value off the struct: a reader who asks what the setting currently is
    // gets `true`, not "no such key". The write path needs it too — `set`
    // refuses a key that does not resolve, so without this the key would be
    // documented in the reference table and settable by nothing.
    if key == "dispatch.tear_lanes_on_stop" {
        return Some(config.dispatch.tear_lanes_on_stop.to_string());
    }
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
    let field = match models_key(config, key) {
        Some(parts) => parts[2],
        None => {
            concurrency_key(config, key)?;
            "concurrency"
        }
    };
    // The zero of the field's own type, read off the struct rather than
    // spelled out here, the same way `ensure_models_entry` reads it.
    if field == "concurrency" {
        return Some("0".to_string());
    }
    let probe = Value::try_from(crate::usage::ModelPrice::probe()).ok()?;
    Some(match probe.get(field)? {
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
            ensure_models_entry(&mut value, parts[1], parts[2])?;
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

    let mut config: Config = value
        .try_into()
        .with_context(|| format!("setting `{key}` to `{input}` produces an invalid config"))?;

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
    for (name, profile) in &config.agents {
        profile
            .permission_mode_status()
            .with_context(|| format!("`agents.{name}.permission_mode`"))?;
        if !(1..=100).contains(&profile.session_reuse_ctx) {
            bail!(
                "`agents.{name}.session_reuse_ctx` must be between 1 and 100 (1..=100), got {}",
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
        if profile.quota_ceiling != 0 && !(1..=100).contains(&profile.quota_ceiling) {
            bail!(
                "`agents.{name}.quota_ceiling` must be 0 (off) or between 1 and 100 (1..=100), \
                 got {}",
                profile.quota_ceiling
            );
        }
        // Both directions of the same rule, caught wherever the edit landed:
        // a ceiling set at or under the reuse threshold, or a reuse threshold
        // raised to meet an already-set ceiling. Either way a task blocked on
        // the ceiling would have the very session that just blocked it
        // carried right back in — `carried_session` reuses anything at or
        // under `session_reuse_ctx` — and block again on the next turn.
        if profile.session_blocked_ctx != 0
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

/// The parts of a dotted key, as they address it in the file.
///
/// Almost always `key.split('.')`, and the exception is why this exists: a
/// model glob is a name out of someone else's catalogue and quite reasonably
/// holds a dot of its own. Shared with [`crate::confdoc`] so that the key an
/// edit validates and the key it writes are split the same way.
pub fn parts<'a>(config: &Config, key: &'a str) -> Vec<&'a str> {
    models_key(config, key).unwrap_or_else(|| key.split('.').collect())
}

/// Split `models.<model glob>.<field>` into its three parts, or `None` if this
/// is not a models key.
///
/// Split from both ends rather than on every dot, because a model glob is a
/// name from someone else's catalogue and quite reasonably contains one:
/// `models.gpt-4.1-*.input` is three parts, not four.
fn models_key<'a>(config: &Config, key: &'a str) -> Option<Vec<&'a str>> {
    let rest = key.strip_prefix("models.")?;
    let (glob, field) = rest.rsplit_once('.')?;
    if glob.is_empty() {
        return None;
    }
    // Only fields ModelPrice actually has, so a misspelt one is still refused.
    // Against `probe` rather than `default`: a default is all zeros now, and a
    // zero is omitted from the serialised form, so a default would name no
    // fields at all and every models key would read as a typo.
    let known = Value::try_from(crate::usage::ModelPrice::probe()).ok()?;
    let _ = config;
    known.get(field)?;
    Some(vec!["models", glob, field])
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
        "tear_lanes_on_stop" => Value::Boolean(config.dispatch.tear_lanes_on_stop),
        "priority" => Value::try_from(config.dispatch.priority)
            .expect("Priority always serialises to a string"),
        "lane_child_ceiling" => Value::String(crate::config::format_duration(
            config.dispatch.lane_child_ceiling,
        )),
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

/// Put an entry at `models.<glob>` if there is none yet, and the one field
/// about to be written if *it* is missing, so the generic walk below has
/// something to write into.
///
/// Both halves are needed now that a zero is omitted: a fresh entry serialises
/// to an empty table, and even an entry that already exists holds only the
/// fields somebody has set. The field is seeded with the zero of its own type,
/// taken from [`crate::usage::ModelPrice::probe`] so the shape is read off the
/// struct rather than repeated here — the value is overwritten a moment later
/// either way.
fn ensure_models_entry(value: &mut Value, glob: &str, field: &str) -> Result<()> {
    let table = value
        .get_mut("models")
        .context("config has no `models` section")?;
    let Some(table) = table.as_table_mut() else {
        bail!("`models` is not a table");
    };
    let entry = table
        .entry(glob.to_string())
        .or_insert_with(|| Value::Table(Default::default()));
    let Some(entry) = entry.as_table_mut() else {
        bail!("`models.{glob}` is not a table");
    };
    if !entry.contains_key(field) {
        let probe = Value::try_from(crate::usage::ModelPrice::probe())
            .context("rendering the model field probe")?;
        let zero = match probe.get(field) {
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

        assert!(keys.contains(&"dispatch.interval"));
        assert!(keys.contains(&"agents.pi.kind"));
        // `permission_mode` is listed for a kind that has modes and not for
        // one that does not: it is omitted wherever the kind carries none,
        // which is `pi` among the shipped profiles.
        assert!(keys.contains(&"agents.claude.permission_mode"));
        assert!(!keys.contains(&"agents.pi.permission_mode"));

        // `concurrency` is listed for the profile that has one and not for the
        // profiles that do not: it is omitted wherever it would be zero, which
        // is every local profile spoolway ships. `set` still creates it — see
        // `a_concurrency_can_be_set_on_a_profile_that_omits_it` below — so the
        // key being absent from this list costs nothing but the row.
        assert!(keys.contains(&"agents.claude.concurrency"));
        assert!(!keys.contains(&"agents.pi.concurrency"));

        // Retired: no longer in the file at all, so nothing here to get or set.
        for gone in [
            "agents.pi.model",
            "agents.pi.sandbox",
            "agents.pi.env",
            "agents.pi.session_reuse_uncached",
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

    /// `unattended.skip_blocked_lane` is written out at every value, unlike
    /// the old `[dispatch]` key it replaced — see
    /// [`crate::config::UnattendedConfig`] — so `set` reaches it the ordinary
    /// way, through the file rather than through a seeded slot.
    #[test]
    fn skip_blocked_lane_is_written_at_every_value_and_set_the_ordinary_way() {
        let config = Config::default();
        assert_eq!(
            get(&config, "unattended.skip_blocked_lane").unwrap(),
            "true"
        );
        assert!(
            toml::to_string(&config)
                .unwrap()
                .contains("skip_blocked_lane"),
            "unlike the retired dispatch.blocked_takes_over, this key is never hidden"
        );

        let off = set(&config, "unattended.skip_blocked_lane", "false").unwrap();
        assert!(!off.unattended.skip_blocked_lane);
        assert_eq!(get(&off, "unattended.skip_blocked_lane").unwrap(), "false");

        let on = set(&off, "unattended.skip_blocked_lane", "true").unwrap();
        assert!(on.unattended.skip_blocked_lane);
    }

    /// The same read/write-while-absent mechanism, for `tear_lanes_on_stop`.
    #[test]
    fn tear_lanes_on_stop_is_readable_and_writable_while_absent() {
        let config = Config::default();
        assert_eq!(get(&config, "dispatch.tear_lanes_on_stop").unwrap(), "true");
        assert!(
            !toml::to_string(&config)
                .unwrap()
                .contains("tear_lanes_on_stop")
        );

        let off = set(&config, "dispatch.tear_lanes_on_stop", "false").unwrap();
        assert!(!off.dispatch.tear_lanes_on_stop);
        assert_eq!(get(&off, "dispatch.tear_lanes_on_stop").unwrap(), "false");

        let on = set(&off, "dispatch.tear_lanes_on_stop", "true").unwrap();
        assert!(on.dispatch.tear_lanes_on_stop);
        assert!(!toml::to_string(&on).unwrap().contains("tear_lanes_on_stop"));
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

    #[test]
    fn kinds_are_inferred_from_the_value() {
        let config = Config::default();
        let entries = entries(&config).unwrap();
        let kind = |key: &str| entries.iter().find(|e| e.key == key).unwrap().kind;

        assert_eq!(kind("agents.claude.concurrency"), Kind::Number);
        assert_eq!(kind("agents.pi.kind"), Kind::Text);
        assert_eq!(kind("skills"), Kind::List);
        assert_eq!(kind("update.check"), Kind::Bool);
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
        assert_eq!(reparsed.agents["claude"].concurrency, 1);
    }

    #[test]
    fn setting_a_value_keeps_its_type() {
        let config = Config::default();

        let updated = set(&config, "agents.pi.concurrency", "4").unwrap();
        assert_eq!(updated.agents["pi"].concurrency, 4);

        let updated = set(&config, "skills", "plan, queue").unwrap();
        assert_eq!(updated.skills, ["plan", "queue"]);
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

    /// A model's slot budget, its exclusivity and its `local` flag are set the
    /// same way as any other `[models]` field — on a glob nothing has written
    /// yet — and each reads back the value that its absence stands for.
    #[test]
    fn a_models_slots_exclusive_and_local_fields_can_be_set() {
        let config = Config::default();

        let updated = set(&config, "models.my-local-*.slots", "3").unwrap();
        assert_eq!(updated.models["my-local-*"].slots, 3);

        let updated = set(&updated, "models.my-local-*.exclusive", "true").unwrap();
        assert!(updated.models["my-local-*"].exclusive);

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
        assert!(set(&config, "dispatch.interval", "every so often").is_err());
        assert!(set(&config, "dispatch.interval", "5m").is_ok());
    }

    /// Cross-field check, not serde: any `u8` deserialises into
    /// `session_reuse_ctx`, so the range is enforced here, at the moment a
    /// person is typing the value and can fix it.
    #[test]
    fn a_session_reuse_ctx_outside_its_range_is_refused() {
        let config = Config::default();

        let err = set(&config, "agents.claude.session_reuse_ctx", "0")
            .unwrap_err()
            .to_string();
        assert!(err.contains("1..=100"), "{err}");

        let err = set(&config, "agents.claude.session_reuse_ctx", "101")
            .unwrap_err()
            .to_string();
        assert!(err.contains("1..=100"), "{err}");

        assert!(set(&config, "agents.claude.session_reuse_ctx", "75").is_ok());
    }

    /// The mockup's own scenario: `session_reuse_ctx` ships at 50, and a
    /// ceiling set at or under it is refused with the exact wording the task
    /// draws — both directions of the same rule, since either edit puts the
    /// same session past the ceiling right back in.
    // covers: agents.<profile>.session_blocked_ctx — the ceiling on a running lane's size
    #[test]
    fn a_session_blocked_ctx_at_or_under_the_reuse_threshold_is_refused() {
        let config = Config::default();
        assert_eq!(config.agents["claude"].session_reuse_ctx, 50);

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

    #[test]
    // covers: agents.<profile>.quota_ceiling — the ceiling a pass reads before starting a new lane
    fn a_quota_ceiling_outside_its_range_is_refused() {
        let config = Config::default();
        assert_eq!(config.agents["claude"].quota_ceiling, 0);

        let off = set(&config, "agents.claude.quota_ceiling", "0")
            .map(|c| c.agents["claude"].quota_ceiling);
        assert_eq!(off.ok(), Some(0), "0 is off, and always allowed");

        let err = set(&config, "agents.claude.quota_ceiling", "101")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("must be 0 (off) or between 1 and 100"),
            "{err}"
        );
        assert!(set(&config, "agents.claude.quota_ceiling", "85").is_ok());
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
        assert_eq!(get(&config, "dispatch.interval").unwrap(), "10s");
        assert_eq!(get(&config, "agents.claude.kind").unwrap(), "claude");
    }

    /// The mockup's own default, and the one value that turns the sweep off.
    #[test]
    fn retention_days_round_trips_and_zero_means_off() {
        let config = Config::default();
        assert_eq!(get(&config, "retention.days").unwrap(), "30");

        let off = set(&config, "retention.days", "0").unwrap();
        assert_eq!(off.retention.days, 0);
        assert_eq!(get(&off, "retention.days").unwrap(), "0");

        let ninety = set(&off, "retention.days", "90").unwrap();
        assert_eq!(ninety.retention.days, 90);
    }
}
