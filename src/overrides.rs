//! The optional patch layer read from outside the checkout.
//!
//! `~/.spoolway/<project>/overrides/` holds up to three things: one
//! `pipelines/<name>.yml` per pipeline patched, `config.toml`, and one
//! `prompts/<name>/PROMPT.md` per prompt replaced whole. `Pipelines::load`,
//! `Config::load` and `prompt::path_for` are the only three doors this ever
//! reaches through — see each's own doc — so nothing downstream of them
//! learns a second source exists, and nothing else in this crate reaches
//! into this module at all. The plan this implements records the choice as
//! `d-layer-outside-checkout`, `d-merge-at-load` and `d-merge-per-format`.
//!
//! A patch may only set keys on a pipeline or a step that already exists —
//! never add or remove one, since list order decides slot priority and a
//! spliced step is a change to the graph, the `spoolway-config` skill's job,
//! not an override's.
//!
//! `spoolway pipeline override`, `prompt override` and `config override`
//! write into this layer, `spoolway override list` reads it back out, and
//! `override promote`/`override drop` are the other side of the merge above:
//! promoting edits the tracked file in place — a line edit, never a
//! re-serialisation, see [`promote_pipeline_patch`] — rather than trusting a
//! YAML writer with a file half of it doesn't own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::pipeline::{Pipeline, Step};

const PIPELINES_SUBDIR: &str = "pipelines";
const PROMPTS_SUBDIR: &str = "prompts";

/// One override left out of the merge because it no longer fits the
/// checkout — a stale pipeline-step entry, a config key the tracked file
/// would refuse, or a prompt replacement whose tracked prompt is gone. See
/// the group's own `d-skip-stale` decision: every other entry in the same
/// file, and every other file, still applies; only this one is left as if
/// it had never been written.
#[derive(Debug, Clone)]
pub(crate) struct Ignored {
    /// Where this entry lives and what it names, e.g. "pipelines/release.yml
    /// step `publish`", "config.toml", or "prompts/reviewer" — the part of
    /// the Mockup's stderr line before the colon.
    pub(crate) target: String,
    /// The keys this one entry set, joined with `, ` — `publish.agent,
    /// publish.model` for a step, a dotted key for a config entry, empty for
    /// a prompt, which has none to name.
    pub(crate) fields: String,
    /// Why the merge left it out, phrased as `validate()`'s own errors are:
    /// no leading capital, no trailing period, so it reads naturally after
    /// the colon on the stderr line and after the em dash in `override
    /// list`'s own row.
    pub(crate) reason: String,
}

impl Ignored {
    /// A whole pipeline patch left out because the checkout no longer has a
    /// pipeline by that name — shared by [`crate::pipeline::Pipelines::load`]
    /// (which never even reaches the file, since it only iterates the
    /// tracked ones) and `commands::override::collect_override_rows` (which
    /// has to say the same thing about a patch it finds sitting in the
    /// layer), so the wording cannot drift between what load skips and what
    /// `override list` shows for it.
    pub(crate) fn missing_pipeline(name: &str) -> Ignored {
        Ignored {
            target: format!("pipelines/{name}.yml"),
            fields: String::new(),
            reason: format!("names pipeline `{name}`, which the checkout does not have"),
        }
    }

    /// A whole prompt override left out because the checkout no longer has a
    /// tracked prompt by that name — shared by [`crate::prompt::path_for`]
    /// and `commands::override::collect_override_rows`, for the same reason
    /// as [`Ignored::missing_pipeline`].
    pub(crate) fn missing_prompt(name: &str) -> Ignored {
        Ignored {
            target: format!("prompts/{name}"),
            fields: String::new(),
            reason: format!("names prompt `{name}`, which the checkout does not have"),
        }
    }

    /// A whole pipeline patch left out not because the name is missing —
    /// it exists, privately — but because the patch layer only ever merges
    /// onto a *tracked* pipeline; see `merge_private`'s own doc. Told apart
    /// from [`Ignored::missing_pipeline`] so a person sees it is waiting on
    /// `pipeline promote`, not that they mistyped the name.
    pub(crate) fn private_pipeline(name: &str) -> Ignored {
        Ignored {
            target: format!("pipelines/{name}.yml"),
            fields: String::new(),
            reason: format!(
                "names pipeline `{name}`, which is private — it applies once `spoolway pipeline \
                 promote {name}` makes it tracked"
            ),
        }
    }

    /// [`Ignored::private_pipeline`]'s own prompt twin: a private prompt
    /// with a fork waiting in the layer, told apart from
    /// [`Ignored::missing_prompt`] for the same reason.
    pub(crate) fn private_prompt(name: &str) -> Ignored {
        Ignored {
            target: format!("prompts/{name}"),
            fields: String::new(),
            reason: format!(
                "names prompt `{name}`, which is private — it applies once the pipeline that \
                 runs it is promoted"
            ),
        }
    }

    /// The Mockup's own stderr line.
    pub(crate) fn notice(&self) -> String {
        format!(
            "spoolway: override ignored — {}: {}",
            self.target, self.reason
        )
    }

    /// `reason`, cut to its first clause — `override list`'s row has no room
    /// for a validation error's own trailing explanation, only for which
    /// rule it broke.
    pub(crate) fn short_reason(&self) -> &str {
        self.reason.split(" — ").next().unwrap_or(&self.reason)
    }
}

/// Whether this process should say anything about an ignored override at
/// all — `false` inside a lane, per `commands::TASK_ENV`'s own doc: a lane's
/// prompt never carries a notice a person did not ask to see. Its own
/// function, pulled out of [`print_ignored_notices`], so a test can drive the
/// decision directly rather than trying to catch a real `eprintln!` on the
/// way past.
pub(crate) fn should_announce_ignored_overrides() -> bool {
    crate::platform::env_var(crate::commands::TASK_ENV).is_err()
}

/// Every [`Ignored::notice`] line already printed this process — a plain
/// command reads its pipelines and its config more than once on the way to
/// answering (`main.rs` loads a `Pipelines` ahead of its own dispatch match,
/// and most commands read their own copy again right after), and every one
/// of those loads runs the same merge and hands the same ignored list to
/// [`print_ignored_notices`]. Without this, `spoolway pipeline check` says
/// the identical line twice. Process-wide rather than threaded through every
/// load call, for the same reason `commands::TASK_ENV` is read once at the
/// CLI boundary would not fit here: nothing between the several loads a
/// single command makes carries a value all of them see.
fn printed_notices() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static PRINTED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    PRINTED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Whether `line` — one rendered [`Ignored::notice`] — has not yet been
/// printed this process, marking it printed as a side effect. Pulled out of
/// [`print_ignored_notices`] so a test can drive the dedup directly, without
/// trying to catch a real `eprintln!` on the way past.
pub(crate) fn first_time_this_process(line: &str) -> bool {
    printed_notices()
        .lock()
        .expect("the notice-dedup lock is never held across a panic")
        .insert(line.to_string())
}

/// Set once by `main` for bare `spoolway` and for the `dispatch --from-screen`
/// child it starts: every note [`print_note`] would write goes nowhere for
/// the rest of the process. Bare `spoolway` takes the terminal a moment after
/// it loads its config, so a note printed then is wiped by the first frame,
/// and one printed while the screen is up smears across it until the next.
/// The child's stderr becomes the board's "the dispatcher did not start"
/// popup, where a note stacked above the refusal buries it. Both still reach
/// the person: `commands::doctor::cheap_findings` names each config and
/// workspace note in the "before dispatching" popup, and the overrides gate
/// names each ignored override. Process-wide for the same reason
/// [`printed_notices`] is: the loads that print run from every corner of the
/// screen, and nothing threads one value through all of them. A fatal error
/// never passes through here — `main` prints it itself — so it still shows.
static QUIET_NOTES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Silence every later [`print_note`] in this process — see [`QUIET_NOTES`].
pub(crate) fn quiet_notes() {
    QUIET_NOTES.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether a note `line` goes to stderr now: never once [`quiet_notes`] has
/// run, and otherwise only the first time this process sees it, per
/// [`first_time_this_process`]. Marks the line printed as a side effect, so a
/// caller that asks must print it.
pub(crate) fn note_due(line: &str) -> bool {
    note_due_with(QUIET_NOTES.load(std::sync::atomic::Ordering::Relaxed), line)
}

/// [`note_due`] with the switch handed in, so a test can drive the quiet
/// branch without silencing every other test in the process.
fn note_due_with(quiet: bool, line: &str) -> bool {
    !quiet && first_time_this_process(line)
}

/// Write one note to stderr when [`note_due`] says so — the one printer every
/// config, override and workspace note goes through.
pub(crate) fn print_note(line: &str) {
    if note_due(line) {
        eprintln!("{line}");
    }
}

/// Print one stderr line per entry in `ignored`, in the Mockup's wording —
/// unless [`should_announce_ignored_overrides`] says this process is a
/// lane's own, and only as [`print_note`] lets it.
pub(crate) fn print_ignored_notices(ignored: &[Ignored]) {
    if !should_announce_ignored_overrides() {
        return;
    }
    for item in ignored {
        print_note(&item.notice());
    }
}

/// Where `spoolway dispatch`'s standing consent gate remembers a fingerprint
/// it has already shown — directly under `Repo::home`, never inside
/// [`dir_for`]'s own directory: that one is removed once it holds nothing
/// (see [`remove_empty_dirs_up_to`]), and an acknowledgement has to survive
/// the layer being emptied and refilled with byte-identical content.
const ACK_FILE: &str = "override-ack";

/// The same "don't ask again until this changes" gate the override layer
/// uses, kept for `commands::dispatch`'s own warnings screen — see
/// `commands::dispatch::warnings_gate_with` — but under its own file, since
/// the two gates ack two unrelated fingerprints and one must not silence the
/// other.
const WARNINGS_ACK_FILE: &str = "warnings-ack";

/// Where a project's patch layer lives, given any path already inside its
/// tracked control plane: `repo.checkout` (most callers' `root` — a linked
/// worktree's own checkout when a lane runs in one) or `repo.root` (always
/// the main checkout).
///
/// `Pipelines::load` and `Config::load` take whichever of the two a caller
/// already holds, most often `checkout` — but the layer is one per
/// *project*, not one per worktree, so this resolves back to the main
/// checkout the same way [`crate::repo::Repo::discover`] does before keying
/// [`crate::mux::project_home`] off it: a lane running the merge from
/// inside its own worktree has to land on the identical directory a command
/// run from the main checkout does, with nothing copied in. Refuses a
/// checkout with no id, as [`crate::mux::project_home`] does; a reader that
/// can do without the layer uses [`dir_if_identified`].
pub(crate) fn dir_for(root: &Path) -> Result<PathBuf> {
    let main = crate::repo::main_checkout(root).unwrap_or_else(|| root.to_path_buf());
    Ok(crate::mux::project_home(&main)?.join(crate::config::OVERRIDES_DIR))
}

/// [`dir_for`], but `Ok(None)` for a checkout with no git repository or no
/// stamp. That is a project nothing has stamped or given a home yet, which
/// has overridden nothing, so `Config::load` and `Pipelines::load` read it as "no layer"
/// rather than refusing to read the tracked files.
pub(crate) fn dir_if_identified(root: &Path) -> Result<Option<PathBuf>> {
    let main = crate::repo::main_checkout(root).unwrap_or_else(|| root.to_path_buf());
    Ok(crate::mux::identified_home(&main)?.map(|home| home.join(crate::config::OVERRIDES_DIR)))
}

/// The shape `overrides/pipelines/<name>.yml` is allowed to take: `steps:`
/// keyed by id rather than the tracked file's own ordered list — a patch
/// names a step, never a position, since position is what decides slot
/// priority — beside `description` and `task_template`, the two other keys
/// a patch may set. Not every top-level key a pipeline itself carries: the
/// Mockup raises `version:` by editing the tracked file directly, so this
/// struct carries no field for it, and `deny_unknown_fields` refuses a patch
/// that names it.
///
/// `pub(crate)` and round-trippable (not just readable): `pipeline override
/// --set` reads one of these back, adds one key, and writes it out again,
/// and `override list`/`override promote` read one to report or apply what
/// it carries — the same shape [`apply_pipeline_patch`] merges, so a value
/// any of them writes is a value the merge already accepts.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PipelinePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) task_template: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) steps: BTreeMap<String, serde_norway::Mapping>,
}

/// Apply `overrides/pipelines/<name>.yml`, if there is one, onto a pipeline
/// already parsed from the tracked file, and return each step entry left
/// out because it no longer fits — see [`Ignored`].
///
/// Called from [`crate::pipeline::Pipelines::load`] before
/// [`crate::pipeline::Pipelines::assemble`] runs, per `d-merge-at-load`. The
/// caller first completes the pipeline's `blocked` step from config,
/// appending one when the file declared none, so a patch on `blocked` finds
/// a runnable step to land on.
///
/// A step entry is stale one of two ways: it names a step id the pipeline no
/// longer has, or applying it (validated here against the *whole* pipeline,
/// not the step alone, since a value change can break a transition) leaves
/// the pipeline invalid. Either way the entry is skipped and the step left
/// exactly as the tracked file declared it — never partially patched — so
/// every other entry, in this file and every other, still applies.
pub(crate) fn apply_pipeline_patch(
    pipeline: &mut Pipeline,
    overrides: &Path,
) -> Result<Vec<Ignored>> {
    let path = overrides
        .join(PIPELINES_SUBDIR)
        .join(format!("{}.yml", pipeline.name));
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let patch: PipelinePatch = serde_norway::from_str(crate::task::without_bom(&raw))
        .with_context(|| format!("parsing {}", path.display()))?;

    if let Some(description) = patch.description {
        pipeline.description = Some(description);
    }
    if let Some(task_template) = patch.task_template {
        pipeline.task_template = Some(task_template);
    }
    let target_file = format!("pipelines/{}.yml", pipeline.name);
    let mut ignored = Vec::new();
    for (id, fields) in patch.steps {
        let fields_named = fields
            .keys()
            .filter_map(|k| k.as_str())
            .map(|k| format!("{id}.{k}"))
            .collect::<Vec<_>>()
            .join(", ");
        let Some(index) = pipeline.steps.iter().position(|s| s.id == id) else {
            ignored.push(Ignored {
                target: format!("{target_file} step `{id}`"),
                fields: fields_named,
                reason: format!(
                    "names step `{id}`, which pipeline `{}` does not have",
                    pipeline.name
                ),
            });
            continue;
        };

        let original = pipeline.steps[index].clone();
        if let Err(err) = apply_step_patch(&mut pipeline.steps[index], &fields) {
            pipeline.steps[index] = original;
            ignored.push(Ignored {
                target: format!("{target_file} step `{id}`"),
                fields: fields_named,
                reason: format!("{err:#}"),
            });
            continue;
        }
        if let Err(err) = pipeline.validate() {
            pipeline.steps[index] = original;
            let message = format!("{err:#}");
            let reason = message
                .strip_prefix(&format!("step `{id}` "))
                .unwrap_or(&message)
                .to_string();
            ignored.push(Ignored {
                target: format!("{target_file} step `{id}`"),
                fields: fields_named,
                reason,
            });
        }
    }
    Ok(ignored)
}

/// Merge `fields` onto `step`, keeping every key it does not name.
///
/// Done by serialising the existing step to a mapping, overwriting the
/// fields the patch names, and deserialising the result back — the same
/// `deny_unknown_fields` that refuses a typo in the tracked file catches one
/// here too. `id:` is refused by name first: setting it would rename the
/// step, a change to the graph rather than a value on one.
///
/// `pub(crate)` so `commands::pipeline_override` can probe a candidate
/// `--set` against a cloned step before writing anything: refusing by the
/// exact rule the merge itself would apply, rather than a second copy of it.
///
/// Four keys are refused before the round-trip rather than by it. `id:` would
/// rename the step, a change to the graph rather than a value on one. Next are
/// `on_loop_max:` and `headless:`, retired, which the round-trip would
/// otherwise swallow without a word: each field is `skip_serializing`, so the
/// step that comes back out has lost it — a key accepted here, written into
/// `overrides/pipelines/<name>.yml`, and then silently doing nothing every
/// time the pipelines are loaded after. `pipeline::refuse_retired_step_keys` is the
/// same refusal for a tracked file, where the key survives long enough to be
/// named; here it has to be caught before serde sees it. The last is
/// `description:` on `blocked`: that step's description is fixed, and a
/// pipeline that declares one fails to load as a whole, so a patch setting it
/// must be refused here rather than break every load after it.
pub(crate) fn apply_step_patch(step: &mut Step, fields: &serde_norway::Mapping) -> Result<()> {
    if fields.contains_key("id") {
        bail!(
            "sets `id:` — a patch may only set a value on an existing step, never rename or \
             reposition one"
        );
    }
    if step.id == crate::pipeline::BLOCKED && fields.contains_key("description") {
        bail!(
            "sets `description:` on step `blocked` — its description is fixed, and a pipeline \
             declaring one fails to load; drop it"
        );
    }
    if fields.contains_key("on_loop_max") {
        bail!(
            "sets `on_loop_max:` — a spent loop budget now always parks on `blocked`, so the \
             key no longer chooses anything; drop it"
        );
    }
    if fields.contains_key("headless") {
        bail!(
            "sets `headless:` — the key is now `hide:`; run `spoolway sync` to rename it, or \
             write `hide: true` in its place"
        );
    }
    let mut value = serde_norway::to_value(&*step).context("serialising step for override")?;
    let serde_norway::Value::Mapping(map) = &mut value else {
        unreachable!("a pipeline step always serialises to a mapping")
    };
    for (key, val) in fields {
        map.insert(key.clone(), val.clone());
    }
    *step = serde_norway::from_value(value).context("applying override")?;
    Ok(())
}

/// Apply `overrides/config.toml`, if there is one, onto an already-parsed
/// config — merged by dotted key through [`crate::confkv::set`], the same
/// validated path `spoolway config set` writes through, so a patch cannot
/// produce a config that command would have refused. A key the tracked
/// config would refuse is skipped, reported in the returned [`Ignored`]
/// list, and every other key still applies. A pair of keys valid only
/// together applies whole; see [`settle`].
pub(crate) fn apply_config_patch(
    config: Config,
    overrides: &Path,
) -> Result<(Config, Vec<Ignored>)> {
    let path = config_patch_path(overrides);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((config, Vec::new())),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let parsed: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let toml::Value::Table(table) = parsed else {
        bail!("{} must be a table of settings", path.display());
    };
    let mut ignored = Vec::new();
    let config = apply_config_table(config, &table, &mut ignored);
    Ok((config, ignored))
}

/// Every leaf key `overrides/config.toml` holds that this binary has retired
/// — on [`crate::config::is_retired_key`]'s own list, not merely mistyped —
/// sorted. Nothing on disk is touched; see
/// [`config_patch_without`] for the text the layer should hold afterwards.
///
/// `spoolway sync`'s own config step calls this right after it rebuilds the
/// tracked file from `tracked` alone (never from a patched copy — see
/// `crate::sync::config`'s own doc for why): a key the struct has already
/// dropped would otherwise sit in the layer forever, since nothing else ever
/// edits it, printing "override ignored" at every load with no way to clear
/// itself the way a retired key in the *tracked* file does on its own next
/// save.
///
/// Only a key on that explicit list counts. A typo (`dispatch.lane_quite`),
/// or a live key holding a value [`crate::confkv::set`] refuses, is a
/// mistake worth leaving in front of whoever wrote it, not something sync
/// should delete on their behalf. Telling the two apart by the wording of
/// [`apply_config_patch`]'s complaint did not work: a typo only earns a "did
/// you mean" when `confkv`'s hint happens to find a near key, and one that
/// did not was deleted as retired.
pub(crate) fn retired_config_patch_keys(overrides: &Path, tracked: &Config) -> Result<Vec<String>> {
    let (_, ignored) = apply_config_patch(tracked.clone(), overrides)?;
    let mut retired: Vec<String> = ignored
        .into_iter()
        .filter(|entry| entry.target == crate::config::CONFIG_FILE)
        .filter(|entry| crate::config::is_retired_key(&entry.fields))
        .map(|entry| entry.fields)
        .collect();
    // `apply_config_patch` reads this key as the limit it stood for when it
    // holds a boolean, so it is then not reported as ignored. It still has to
    // leave the layer, replaced by that limit: see [`config_patch_without`].
    // Any other value was refused, so the key is already listed above.
    if !retired.iter().any(|key| key == KEEP_FINISHED_LANES)
        && config_patch_keys(overrides)?
            .is_some_and(|keys| keys.iter().any(|key| key == KEEP_FINISHED_LANES))
    {
        retired.push(KEEP_FINISHED_LANES.to_string());
    }
    retired.sort();
    Ok(retired)
}

/// `overrides/config.toml` as it should read once each of `keys` (as
/// [`retired_config_patch_keys`] found them) is removed from it, leaving every
/// other key in the layer untouched. Nothing on disk is touched: `sync` plans
/// this write with the rest of its writes and makes it afterwards.
///
/// `dispatch.keep_finished_lanes` is the one key that is replaced rather than
/// only removed: the limit it stood for is written as `dispatch.visible_lanes`
/// first, so the layer sets the same thing before and after.
pub(crate) fn config_patch_without(overrides: &Path, keys: &[String]) -> Result<String> {
    let path = config_patch_path(overrides);
    let mut doc =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    for key in keys {
        if key == KEEP_FINISHED_LANES {
            doc = visible_lanes_for_keep_finished(&doc)?;
        }
        // The glob of a `models.<glob>.<field>` key may hold a dot, and
        // splitting on every one would look for a table that is not there and
        // leave the retired key in the layer.
        let parts: Vec<&str> =
            crate::confkv::split_models_key(key).unwrap_or_else(|| key.split('.').collect());
        doc = crate::confdoc::remove(&doc, &parts)?;
    }
    Ok(doc)
}

/// `doc` with the limit its `dispatch.keep_finished_lanes` stood for written
/// as `dispatch.visible_lanes`, so that removing the retired key does not
/// change what the layer sets. A layer that already holds a `visible_lanes`
/// is returned as it is, since that one wins when the layer is applied.
fn visible_lanes_for_keep_finished(doc: &str) -> Result<String> {
    let parsed: toml::Value = toml::from_str(doc).context("parsing the override config")?;
    let Some(dispatch) = parsed.get("dispatch") else {
        return Ok(doc.to_string());
    };
    if dispatch.get("visible_lanes").is_some() {
        return Ok(doc.to_string());
    }
    let limit = match dispatch.get("keep_finished_lanes") {
        Some(toml::Value::Boolean(true)) => toml::Value::String("all".to_string()),
        Some(toml::Value::Boolean(false)) => toml::Value::Integer(1),
        _ => return Ok(doc.to_string()),
    };
    crate::confdoc::set(
        doc,
        &["dispatch", "visible_lanes"],
        &limit,
        &Config::default().render()?,
    )
}

/// Apply every leaf of `table` onto `config`, a key the tracked config would
/// refuse going to `ignored` with the reason.
///
/// Each key is checked against the config the keys before it produced, so a
/// pair that is valid only together would lose its second half to whichever
/// key happened to come first: `session_blocked_ctx = 12` is refused under a
/// tracked `session_reuse_ctx` of 20, then `session_reuse_ctx = 5` applies
/// and the pair is half there. See [`settle`], which retries the refused keys
/// once the rest are in.
fn apply_config_table(
    config: Config,
    table: &toml::value::Table,
    ignored: &mut Vec<Ignored>,
) -> Config {
    let mut leaves = Vec::new();
    let mut unrenderable = Vec::new();
    collect_config_leaves(table, &mut String::new(), &mut |key, input| match input {
        Ok(input) => leaves.push((key.to_string(), input)),
        Err(err) => unrenderable.push((key.to_string(), err)),
    });
    let mut config = config;
    let refused = settle(&mut config, &leaves, |config, key, input| {
        let (key, input) = current_spelling(key, input.to_string());
        crate::confkv::set(config, &key, &input)
    });
    for (key, err) in unrenderable.into_iter().chain(refused) {
        ignored.push(Ignored {
            target: crate::config::CONFIG_FILE.to_string(),
            fields: key,
            reason: format!("{err:#}"),
        });
    }
    config
}

/// Apply `leaves` (dotted key, rendered value) to `state` in order, then
/// again for the ones that were refused, until a pass applies none more.
/// Returns what is still refused, in leaf order, with the reason.
///
/// A cross-field check such as the one that keeps `session_blocked_ctx` above
/// `session_reuse_ctx` judges each key against the keys applied before it, so
/// the order the file happens to list them in decides which half of a valid
/// pair is refused. Retrying once the others are in makes the outcome the
/// same in either order. A pair that is wrong together is refused on every
/// pass and stays in the returned list, so it is still reported rather than
/// half applied. `apply` must leave `state` as it was when it errs.
fn settle<S>(
    state: &mut S,
    leaves: &[(String, String)],
    mut apply: impl FnMut(&S, &str, &str) -> Result<S>,
) -> Vec<(String, anyhow::Error)> {
    let mut pending: Vec<(&str, &str)> = leaves
        .iter()
        .map(|(key, input)| (key.as_str(), input.as_str()))
        .collect();
    loop {
        let before = pending.len();
        let mut refused = Vec::new();
        let mut errors = Vec::new();
        for (key, input) in pending {
            match apply(state, key, input) {
                Ok(next) => *state = next,
                Err(err) => {
                    refused.push((key, input));
                    errors.push((key.to_string(), err));
                }
            }
        }
        if refused.is_empty() || refused.len() == before {
            return errors;
        }
        pending = refused;
    }
}

/// `value` as the text [`crate::confkv::set`] expects — the same string a
/// person would type at `spoolway config set <key> <value>`.
fn render_config_leaf(value: &toml::Value) -> Result<String> {
    Ok(match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Array(items) => {
            let mut rendered = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    toml::Value::Table(_) | toml::Value::Array(_) => {
                        bail!("holds a nested value, which cannot be set as text")
                    }
                    toml::Value::String(s) => rendered.push(s.clone()),
                    other => rendered.push(other.to_string()),
                }
            }
            rendered.join(", ")
        }
        toml::Value::Table(_) => unreachable!("a table is matched before this is called"),
        other => other.to_string(),
    })
}

/// The note a private layer's `dispatch.keep_finished_lanes` earns at load:
/// the one the tracked file's earns, so a person who has the key only in the
/// layer is told to run `spoolway sync` as well. `None` when the layer holds
/// no such key, or one that is not a boolean, which is already reported as an
/// ignored override.
///
/// `loaded` is the config with the layer applied and `ignored` what applying
/// it left out. A `visible_lanes` in the same layer wins over the key, unless
/// it was itself refused.
pub(crate) fn keep_finished_lanes_note(
    overrides: &Path,
    loaded: &Config,
    ignored: &[Ignored],
) -> Result<Option<String>> {
    let path = config_patch_path(overrides);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let parsed: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let Some(dispatch) = parsed.get("dispatch") else {
        return Ok(None);
    };
    let Some(keep) = dispatch
        .get("keep_finished_lanes")
        .and_then(toml::Value::as_bool)
    else {
        return Ok(None);
    };
    let beside = (dispatch.get("visible_lanes").is_some()
        && !ignored
            .iter()
            .any(|entry| entry.fields == "dispatch.visible_lanes"))
    .then_some(&loaded.dispatch.visible_lanes);
    Ok(Some(crate::config::keep_finished_lanes_note(
        &path, keep, beside,
    )))
}

/// The retired layer key that `visible_lanes` replaced.
const KEEP_FINISHED_LANES: &str = "dispatch.keep_finished_lanes";

/// `key` and `input` as the config spells them today.
///
/// A private layer written for an older release may hold
/// `dispatch.keep_finished_lanes`. The tracked file migrates that key when it
/// loads, so the layer has to as well: left to `confkv::set`, which refuses a
/// retired key, `false` would silently become the default limit of 5 instead
/// of 1. `true` is `all` and `false` is 1. Every other key, and a value that
/// is not a boolean, passes through unchanged and is refused as before.
///
/// A table is walked in key order, so a `visible_lanes` the same layer holds
/// is applied after this and wins, as it does in the tracked file.
fn current_spelling(key: &str, input: String) -> (String, String) {
    match (key, input.as_str()) {
        (KEEP_FINISHED_LANES, "true") => ("dispatch.visible_lanes".to_string(), "all".to_string()),
        (KEEP_FINISHED_LANES, "false") => ("dispatch.visible_lanes".to_string(), "1".to_string()),
        _ => (key.to_string(), input),
    }
}

/// `overrides/prompts/<name>/PROMPT.md`, if it exists — the whole file,
/// replacing the tracked one rather than merging into it: prose has no key
/// for a patch to aim at.
pub(crate) fn prompt_override(overrides: &Path, name: &str) -> Option<PathBuf> {
    let path = prompt_patch_path(overrides, name);
    path.is_file().then_some(path)
}

// ---------------------------------------------------------------------------
// The gate: `commands::dispatch`'s standing consent check reads and writes
// an acknowledgement here, keyed on `version::layer_fingerprint` together
// with the set of entries the merge above left out — see
// `commands::dispatch::overrides_fingerprint`. An override is allowed to be
// permanent, so this carries no timer and returns the moment a byte of the
// layer changes underneath it, or an entry stops fitting the project.
// ---------------------------------------------------------------------------

/// Whether a gate keyed on the acknowledgement file at `home.join(file)`
/// still owes a person a question about `fingerprint`. `true` (still owed)
/// once the stored acknowledgement names anything else — an unreadable or
/// missing file reads the same as one that names something else, since
/// either way nobody has said yes to *this* fingerprint yet.
fn ack_needed_at(home: &Path, file: &str, fingerprint: &str) -> bool {
    std::fs::read_to_string(home.join(file))
        .map(|stored| stored.trim() != fingerprint)
        .unwrap_or(true)
}

/// Record that a person has agreed to `fingerprint`, under the gate keyed on
/// `home.join(file)`.
fn ack_write_at(home: &Path, file: &str, fingerprint: &str) -> Result<()> {
    crate::task::write_atomic(&home.join(file), fingerprint)
}

/// Whether `dispatch`'s override gate still owes a person a question about
/// the layer at `fingerprint`. See [`ack_needed_at`].
pub(crate) fn ack_needed(home: &Path, fingerprint: &str) -> bool {
    ack_needed_at(home, ACK_FILE, fingerprint)
}

/// Record that a person has agreed to run under `fingerprint` — the layer's
/// own, from [`crate::version::layer_fingerprint`], with its ignored entries
/// folded in: a tracked-file edit alone must not reopen a gate the layer
/// itself has not moved, unless it changes which entries are left out.
pub(crate) fn ack_write(home: &Path, fingerprint: &str) -> Result<()> {
    ack_write_at(home, ACK_FILE, fingerprint)
}

/// Whether `dispatch`'s warnings screen still owes a person a question about
/// the rendered lines hashed as `fingerprint` — see
/// `crate::skeleton::fingerprint`. See [`ack_needed_at`].
pub(crate) fn warnings_ack_needed(home: &Path, fingerprint: &str) -> bool {
    ack_needed_at(home, WARNINGS_ACK_FILE, fingerprint)
}

/// Record that a person has hidden the warnings screen until its rendered
/// lines change from `fingerprint`.
pub(crate) fn warnings_ack_write(home: &Path, fingerprint: &str) -> Result<()> {
    ack_write_at(home, WARNINGS_ACK_FILE, fingerprint)
}

// ---------------------------------------------------------------------------
// The write side: `pipeline override`, `prompt override` and `config
// override` create or update an entry; `override list` reads every entry
// back; `override promote` and `override drop` are the two ways one leaves.
// Everything above this line only ever reads the layer to merge it onto a
// loaded pipeline or config — nothing above needs any of what follows, and
// nothing below reaches back into the merge itself.
// ---------------------------------------------------------------------------

/// The directory holding every pipeline patch — for
/// [`crate::pipeline_snapshot`], which records each file in it.
pub(crate) fn pipeline_patches_dir(overrides: &Path) -> PathBuf {
    overrides.join(PIPELINES_SUBDIR)
}

/// Where a pipeline's patch lives.
pub(crate) fn pipeline_patch_path(overrides: &Path, name: &str) -> PathBuf {
    overrides.join(PIPELINES_SUBDIR).join(format!("{name}.yml"))
}

/// Where a prompt's whole-file replacement lives.
pub(crate) fn prompt_patch_path(overrides: &Path, name: &str) -> PathBuf {
    overrides
        .join(PROMPTS_SUBDIR)
        .join(name)
        .join(crate::assets::PROMPT_FILE)
}

/// Where the config patch lives.
pub(crate) fn config_patch_path(overrides: &Path) -> PathBuf {
    overrides.join(crate::config::CONFIG_FILE)
}

/// The three comment lines `pipeline override` writes above a fresh patch —
/// the file's own contract, in its own words, since nothing else documents
/// this shape to a person who opens it by hand.
fn pipeline_patch_header(name: &str) -> String {
    format!(
        "# Every key here overrides the same key in .spoolway/pipelines/{name}.yml.\n\
         # Steps are addressed by id and must already exist there.\n\
         # `spoolway override promote {name}` writes these into the tracked file.\n\n"
    )
}

/// `overrides/pipelines/<name>.yml`, parsed as a patch — `None` when there is
/// none. Unlike [`apply_pipeline_patch`] this does not require the pipeline
/// to exist or validate anything against it: `override list` reads a patch
/// this way for a pipeline that may since have been renamed or removed, and
/// `pipeline override --set` reads one this way to add a key to whatever is
/// already there.
pub(crate) fn read_pipeline_patch(overrides: &Path, name: &str) -> Result<Option<PipelinePatch>> {
    let path = pipeline_patch_path(overrides, name);
    match std::fs::read_to_string(&path) {
        Ok(raw) => Ok(Some(
            serde_norway::from_str(crate::task::without_bom(&raw))
                .with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write a pipeline's patch, replacing whatever was there.
///
/// This file is spoolway's own — nobody is asked to hand-edit it the way a
/// pipeline or a config file is — so, unlike [`promote_pipeline_patch`]
/// below, there is no earlier version of it worth preserving byte for byte:
/// every call regenerates it whole, header included.
pub(crate) fn write_pipeline_patch(
    overrides: &Path,
    name: &str,
    patch: &PipelinePatch,
) -> Result<()> {
    let path = pipeline_patch_path(overrides, name);
    let body = serde_norway::to_string(patch).context("serialising pipeline patch")?;
    crate::task::write_atomic(&path, format!("{}{body}", pipeline_patch_header(name)))
        .with_context(|| format!("writing {}", path.display()))
}

/// Every pipeline with a patch on disk, sorted by name.
pub(crate) fn list_pipeline_patches(overrides: &Path) -> Result<Vec<String>> {
    let dir = overrides.join(PIPELINES_SUBDIR);
    let mut names = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .map(|entry| Ok(entry?.path()))
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("reading {}", dir.display()))?
            .into_iter()
            .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("yml"))
            .filter_map(|path| path.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect::<Vec<_>>(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    names.sort();
    Ok(names)
}

/// Every prompt with a whole-file replacement on disk, sorted by name.
pub(crate) fn list_prompt_overrides(overrides: &Path) -> Result<Vec<String>> {
    let dir = overrides.join(PROMPTS_SUBDIR);
    let mut names = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .map(|entry| Ok(entry?.path()))
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("reading {}", dir.display()))?
            .into_iter()
            .filter(|path| path.join(crate::assets::PROMPT_FILE).is_file())
            .filter_map(|path| path.file_name().map(|s| s.to_string_lossy().into_owned()))
            .collect::<Vec<_>>(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    names.sort();
    Ok(names)
}

/// Whether `overrides/config.toml` sets `key`, spelled as the config spells
/// it today: a layer holding the retired `dispatch.keep_finished_lanes` sets
/// `dispatch.visible_lanes`, since that is the key [`apply_config_table`]
/// applies it as. `false` when there is no patch.
pub(crate) fn config_patch_sets(overrides: &Path, key: &str) -> Result<bool> {
    // A quoted model glob, `models."x".input`, is the key `models.x.input`
    // that `current_spelling` names a layer's leaf by.
    let key = crate::confkv::split_models_key(key)
        .map(|parts| parts.join("."))
        .unwrap_or_else(|| key.to_string());
    let path = config_patch_path(overrides);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let parsed: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let toml::Value::Table(table) = parsed else {
        bail!("{} must be a table of settings", path.display());
    };
    let mut sets = false;
    collect_config_leaves(&table, &mut String::new(), &mut |leaf, input| {
        let spelled = match input {
            Ok(input) => current_spelling(leaf, input).0,
            Err(_) => leaf.to_string(),
        };
        sets |= spelled == key;
    });
    Ok(sets)
}

/// The dotted leaf keys `overrides/config.toml` carries, sorted — `None`
/// when there is no patch. For `override list`, which shows what a config
/// patch overrides the same way it shows a pipeline patch's step keys.
pub(crate) fn config_patch_keys(overrides: &Path) -> Result<Option<Vec<String>>> {
    let path = config_patch_path(overrides);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let parsed: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let toml::Value::Table(table) = parsed else {
        bail!("{} must be a table of settings", path.display());
    };
    let mut keys = Vec::new();
    collect_leaf_keys(&table, &mut String::new(), &mut keys);
    keys.sort();
    Ok(Some(keys))
}

/// Every dotted path to a leaf (non-table) value in `table` — the sibling of
/// [`apply_config_table`]'s walk, over the same shape, kept separate because
/// one applies a value through `confkv::set` and the other only names it.
fn collect_leaf_keys(table: &toml::value::Table, prefix: &mut String, keys: &mut Vec<String>) {
    for (key, value) in table {
        let mark = prefix.len();
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(key);
        match value {
            toml::Value::Table(nested) => collect_leaf_keys(nested, prefix, keys),
            _ => keys.push(prefix.clone()),
        }
        prefix.truncate(mark);
    }
}

/// Remove `dir` if it holds nothing, then its parent if that too is now
/// empty, stopping at `overrides` itself — `override drop` and `override
/// promote` must leave `~/.spoolway/<project>/` and everything above the
/// layer alone even when the layer itself is now empty; `queue/`, `archive/`
/// and the rest of that directory are not this module's to tidy.
fn remove_empty_dirs_up_to(overrides: &Path, mut dir: PathBuf) -> Result<()> {
    loop {
        match std::fs::read_dir(&dir) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        }
        std::fs::remove_dir(&dir).with_context(|| format!("removing {}", dir.display()))?;
        if dir == overrides {
            return Ok(());
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return Ok(()),
        }
    }
}

/// Remove a pipeline's patch, and the `pipelines/` directory beside it if
/// that was the last one.
pub(crate) fn remove_pipeline_patch(overrides: &Path, name: &str) -> Result<()> {
    let path = pipeline_patch_path(overrides, name);
    if !path.is_file() {
        bail!("no override for pipeline `{name}`");
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    remove_empty_dirs_up_to(
        overrides,
        path.parent().expect("has a parent").to_path_buf(),
    )
}

/// Remove a prompt's whole-file replacement, and its now-empty directories.
pub(crate) fn remove_prompt_override(overrides: &Path, name: &str) -> Result<()> {
    let path = prompt_patch_path(overrides, name);
    if !path.is_file() {
        bail!("no override for prompt `{name}`");
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    remove_empty_dirs_up_to(
        overrides,
        path.parent().expect("has a parent").to_path_buf(),
    )
}

/// Remove the config patch.
pub(crate) fn remove_config_patch(overrides: &Path) -> Result<()> {
    let path = config_patch_path(overrides);
    if !path.is_file() {
        bail!("no override for `config.toml`");
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    remove_empty_dirs_up_to(overrides, overrides.to_path_buf())
}

/// `value`, rendered the way it would sit after a YAML `key: `, trimmed of
/// the trailing newline `serde_norway` always writes — for the display of a
/// step field's current value, and for the line [`promote_pipeline_patch`]
/// writes into the tracked file. Refuses a nested value outright: a patch's
/// values are scalars, by the same rule [`apply_step_patch`] leans on, so
/// there is never a mapping or a sequence here to render as one line.
fn render_scalar(value: &serde_norway::Value) -> Result<String> {
    if matches!(
        value,
        serde_norway::Value::Mapping(_) | serde_norway::Value::Sequence(_)
    ) {
        bail!("holds a nested value, which cannot be written as one line");
    }
    let rendered = serde_norway::to_string(value).context("rendering value")?;
    Ok(rendered.trim_end().to_string())
}

/// `step`'s current value for `key`, rendered the same way
/// [`render_scalar`] renders one a patch is about to write — `""` when the
/// field is at a default so unremarkable serialising it skips the key
/// entirely (`slot: true`, an absent `model` on a step that sets none), which
/// reads as blank rather than as an error: there is nothing wrong with the
/// step, only nothing this key currently says.
pub(crate) fn field_display(step: &Step, key: &str) -> Result<String> {
    let value = serde_norway::to_value(step).context("serialising step")?;
    let serde_norway::Value::Mapping(map) = value else {
        unreachable!("a pipeline step always serialises to a mapping")
    };
    match map.get(key) {
        Some(value) => render_scalar(value),
        None => Ok(String::new()),
    }
}

/// Byte range of `- id: <step_id>`'s block: from that line up to the line
/// before the next step's `- id:`, or the end of the file.
fn step_block(text: &str, step_id: &str) -> Option<(usize, usize)> {
    let marker = "- id:";
    let mut offset = 0usize;
    let mut start = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        match start {
            Some(start_offset) if trimmed.starts_with(marker) => {
                return Some((start_offset, offset));
            }
            None if trimmed
                .strip_prefix(marker)
                .is_some_and(|rest| rest.trim() == step_id) =>
            {
                start = Some(offset);
            }
            _ => {}
        }
        offset += line.len();
    }
    start.map(|s| (s, text.len()))
}

/// Split a step field's raw text after `key:` into its value and an
/// optional trailing `# comment`, so replacing the value leaves a comment on
/// the same line exactly as it was — the Goal's "leaves every other byte of
/// the tracked file … exactly as it was" covers a line's own trailing
/// comment too, not only the lines around it.
///
/// Naive on purpose: the first `#` is taken as the comment's start, which is
/// wrong only for a value that is both quoted and holds a literal `#`
/// inside the quotes. No key a patch may set ever takes a value shaped like
/// that — a model name, an effort string, a duration, a step id — so this
/// never has to tell the two apart.
fn split_value_and_comment(raw: &str) -> (&str, Option<&str>) {
    match raw.find('#') {
        Some(at) => (raw[..at].trim(), Some(raw[at..].trim_end())),
        None => (raw.trim(), None),
    }
}

/// Replace `key`'s value on the line that carries it, inside `step_id`'s own
/// block — the whole of what [`promote_pipeline_patch`] does to the tracked
/// file, and the only thing it does: every other byte, `pipeline::KEY_BLOCK`
/// and every `description:` included, is copied through untouched because
/// nothing here ever looks at them.
fn replace_step_value(
    text: &str,
    step_id: &str,
    key: &str,
    new_value: &str,
) -> Result<(String, String)> {
    let (start, end) =
        step_block(text, step_id).with_context(|| format!("no step `- id: {step_id}` found"))?;
    let block = &text[start..end];

    let needle = format!("{key}:");
    let mut offset = 0usize;
    let mut found = None;
    for line in block.split_inclusive('\n') {
        if line.trim_start().starts_with(&needle) {
            found = Some((offset, line.len()));
            break;
        }
        offset += line.len();
    }
    let (line_start, line_len) =
        found.with_context(|| format!("step `{step_id}` has no `{key}:`"))?;
    let line = &block[line_start..line_start + line_len];

    let ending = match () {
        _ if line.ends_with("\r\n") => "\r\n",
        _ if line.ends_with('\n') => "\n",
        _ => "",
    };
    let indent_len = line.len() - line.trim_start().len();
    let raw = &line[indent_len + needle.len()..line.len() - ending.len()];
    let (old_value, comment) = split_value_and_comment(raw);
    let old_value = old_value.to_string();
    let new_line = match comment {
        Some(comment) => format!(
            "{}{key}: {new_value}  {comment}{ending}",
            &line[..indent_len]
        ),
        None => format!("{}{key}: {new_value}{ending}", &line[..indent_len]),
    };

    let new_block = format!(
        "{}{}{}",
        &block[..line_start],
        new_line,
        &block[line_start + line_len..]
    );
    let new_text = format!("{}{}{}", &text[..start], new_block, &text[end..]);
    Ok((new_text, old_value))
}

/// Promote a pipeline patch into its tracked file, one step field at a time,
/// and return what changed as `(step.key, old, new)` triples for
/// `commands::override_promote` to print.
///
/// Never re-serialises the file — see this module's own doc. Only possible
/// because a patch is limited to scalar keys on steps that already exist:
/// the edit is "find `- id: <step>`, find `<key>:` inside its block, replace
/// the value", never a rewrite of the document. `patch.description` and
/// `patch.task_template` are refused rather than silently dropped: they
/// would need touching the file outside any step's block, which this line
/// editor was never built to do — a project that wants one promoted has to
/// edit the tracked file by hand.
pub(crate) fn promote_pipeline_patch(
    root: &Path,
    name: &str,
    patch: &PipelinePatch,
) -> Result<Vec<(String, String, String)>> {
    if patch.description.is_some() || patch.task_template.is_some() {
        bail!(
            "overrides `description` or `task_template` on pipeline `{name}` — `override \
             promote` only writes step keys; edit the tracked file for either of those by hand"
        );
    }
    let path = crate::pipeline::Pipelines::file_in(root, name);
    let mut text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut changes = Vec::new();
    for (step_id, fields) in &patch.steps {
        for (key, value) in fields {
            let key = key
                .as_str()
                .with_context(|| format!("step `{step_id}` has a non-string key"))?;
            let rendered =
                render_scalar(value).with_context(|| format!("step `{step_id}`.`{key}`"))?;
            let (next, old) = replace_step_value(&text, step_id, key, &rendered)
                .with_context(|| format!("step `{step_id}`.`{key}` in {}", path.display()))?;
            text = next;
            changes.push((format!("{step_id}.{key}"), old, rendered));
        }
    }
    std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
    Ok(changes)
}

/// Promote `overrides/config.toml` into the tracked file, one key at a time,
/// through the same [`crate::confkv::set`] / `Config::edit_key` pair
/// `spoolway config set` already writes through — so every comment in
/// `config.toml` survives, exactly as it would if a person had typed
/// `config set` once per key by hand. A pair of keys that is valid only
/// together, such as the two session thresholds, is written whole whichever
/// order the layer lists it in; see [`settle`].
///
/// All or nothing. Every key is set on an in-memory config and edited into an
/// in-memory copy of the tracked text, and the file is written once, after
/// the last key, with the same atomic write `config set` uses. A key the
/// tracked config refuses therefore leaves the file untouched, and so does a
/// process killed partway: otherwise the earlier keys would be written while
/// the layer, kept because the promote failed, still held them all. A
/// tracked file that does not exist yet is written whole from the config, as
/// `config set` does.
///
/// Returns the promoted keys and their new values, for
/// `commands::override_promote` to print.
pub(crate) fn promote_config_patch(root: &Path) -> Result<Vec<(String, String)>> {
    let overrides = dir_for(root)?;
    let path = config_patch_path(&overrides);
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let toml::Value::Table(table) = parsed else {
        bail!("{} must be a table of settings", path.display());
    };

    let tracked_path = Config::path_in(root);
    let text = match std::fs::read_to_string(&tracked_path) {
        Ok(text) => Some(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => {
            return Err(err).with_context(|| format!("reading {}", tracked_path.display()));
        }
    };
    let mut leaves = Vec::new();
    collect_config_leaves(&table, &mut String::new(), &mut |key, input| {
        leaves.push((key.to_string(), input));
    });
    let leaves = leaves
        .into_iter()
        .map(|(key, input)| {
            input
                .map(|input| (key.clone(), input))
                .with_context(|| format!("`{key}`"))
        })
        .collect::<Result<Vec<_>>>()
        .with_context(|| {
            format!(
                "nothing was written to the tracked config; fix or drop the key in {}",
                path.display()
            )
        })?;
    // A pair that is valid only together is written whole, in either order: see
    // [`settle`]. Whatever stays refused after that stops the promote before
    // anything is written.
    let mut state = (Config::load_tracked(root)?, text);
    let refused = settle(&mut state, &leaves, |(config, text), key, input| {
        let (key, input) = current_spelling(key, input.to_string());
        let next = crate::confkv::set(config, &key, &input)?;
        let next_text = match text {
            Some(text) => Some(next.edit_key(text, &key, &tracked_path)?),
            None => None,
        };
        Ok((next, next_text))
    });
    if let Some((_, err)) = refused.into_iter().next() {
        return Err(err).with_context(|| {
            format!(
                "nothing was written to the tracked config; fix or drop the key in {}",
                path.display()
            )
        });
    }
    let (config, text) = state;
    let changes = leaves
        .into_iter()
        .map(|(key, input)| current_spelling(&key, input))
        .collect::<Vec<_>>();

    match text {
        Some(text) => crate::task::write_atomic(&crate::config::write_target(&tracked_path), &text)
            .with_context(|| format!("writing {}", tracked_path.display()))?,
        None => config.save(root)?,
    }
    Ok(changes)
}

/// Call `found(dotted key, rendered value)` for every leaf of `table`, in
/// order. A value that cannot be rendered as text arrives as an error naming
/// why, since `confkv::set`'s own error would not.
fn collect_config_leaves(
    table: &toml::value::Table,
    prefix: &mut String,
    found: &mut dyn FnMut(&str, Result<String>),
) {
    for (key, value) in table {
        let mark = prefix.len();
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(key);
        match value {
            toml::Value::Table(nested) => collect_config_leaves(nested, prefix, found),
            other => found(prefix, render_config_leaf(other)),
        }
        prefix.truncate(mark);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A checkout with no id has no home, so no overrides layer: the lookup
    /// that must have one refuses it, and the one a config or pipeline load
    /// uses reads `None`, so those still read the tracked files.
    #[test]
    fn a_checkout_with_no_id_has_no_overrides_layer() {
        let root = crate::scratch::root("overrides-no-id");
        let home = crate::scratch::root("overrides-no-id-home");
        std::fs::create_dir_all(&root).unwrap();
        crate::scratch::git_init(&root, &["-q"]);
        crate::platform::test_home::with_home(&home, || {
            assert!(dir_for(&root).is_err());
            assert_eq!(dir_if_identified(&root).unwrap(), None);
            assert!(!crate::mux::state_root().exists());
        });
    }

    /// The Mockup's own stderr line, built from `target` and `reason`.
    #[test]
    fn ignored_notice_reads_as_the_mockup_shows_it() {
        let ignored = Ignored {
            target: "pipelines/release.yml step `publish`".to_string(),
            fields: "publish.agent, publish.model".to_string(),
            reason: "names both `run:` and `agent:` — a step runs a process or a model, not both"
                .to_string(),
        };
        assert_eq!(
            ignored.notice(),
            "spoolway: override ignored — pipelines/release.yml step `publish`: names both \
             `run:` and `agent:` — a step runs a process or a model, not both"
        );
        assert_eq!(ignored.short_reason(), "names both `run:` and `agent:`");
    }

    /// A given exact notice line is printed at most once per process — a
    /// plain command loads its pipelines and its config more than once on
    /// the way to answering, and every load hands the same ignored list to
    /// `print_ignored_notices`, so the underlying dedup this delegates to
    /// has to say "already said" the second time it is asked about the
    /// identical line. A distinctive line of its own, so a neighbour test
    /// asking about some other line can never collide with this one.
    #[test]
    fn a_notice_line_is_first_time_only_once_per_process() {
        let line = "spoolway: override ignored — pipelines/dedup-test-9f3c1e.yml step \
                    `only_this_test_names`: a reason nothing else in this suite ever writes"
            .to_string();
        assert!(
            first_time_this_process(&line),
            "the first ever mention of this exact line"
        );
        assert!(
            !first_time_this_process(&line),
            "a second mention of the identical line must not repeat"
        );
    }

    /// Once the switch is on, no note is due, not even one never said
    /// before. A quiet ask must not use up the line either, so the same line
    /// is still due the first time it is asked about with the switch off.
    #[test]
    fn a_quiet_process_prints_no_note() {
        let line = "note: a line only quiet-test-4b7e2a ever writes".to_string();
        assert!(!note_due_with(true, &line), "quiet says nothing");
        assert!(note_due_with(false, &line), "the line was never used up");
        assert!(!note_due_with(false, &line), "and is said once");
    }

    /// The Mockup's own shape names the pipeline and the missing prompt the
    /// same way — a single constructor each, so `Pipelines::load`,
    /// `prompt::path_for` and `commands::override::collect_override_rows`
    /// can never drift apart on the wording.
    #[test]
    fn missing_pipeline_and_missing_prompt_read_as_the_mockups_own_wording() {
        let pipeline = Ignored::missing_pipeline("gone");
        assert_eq!(pipeline.target, "pipelines/gone.yml");
        assert_eq!(
            pipeline.reason,
            "names pipeline `gone`, which the checkout does not have"
        );

        let prompt = Ignored::missing_prompt("retired");
        assert_eq!(prompt.target, "prompts/retired");
        assert_eq!(
            prompt.reason,
            "names prompt `retired`, which the checkout does not have"
        );
    }

    /// A reason with no em dash of its own has nothing to cut — the short
    /// form is the whole thing.
    #[test]
    fn short_reason_with_no_em_dash_is_the_whole_reason() {
        let ignored = Ignored {
            target: "pipelines/impl.yml step `nonesuch`".to_string(),
            fields: "nonesuch.model".to_string(),
            reason: "names step `nonesuch`, which pipeline `impl` does not have".to_string(),
        };
        assert_eq!(
            ignored.short_reason(),
            "names step `nonesuch`, which pipeline `impl` does not have"
        );
    }

    /// Outside a lane — no `SPOOLWAY_TASK` in the environment — an ignored
    /// override is announced. Inside one, it never is: a lane's prompt never
    /// carries a notice a person did not ask to see.
    #[test]
    fn announcing_ignored_overrides_is_gated_on_the_lane_environment() {
        crate::platform::test_env::with_env(crate::commands::TASK_ENV, "some-task", || {
            assert!(!should_announce_ignored_overrides(), "a lane stays quiet");
        });
        // `test_env::with_env` restores whatever this thread had before —
        // a real spoolway session, `SPOOLWAY_TASK` is never set at all.
        if crate::platform::env_var(crate::commands::TASK_ENV).is_err() {
            assert!(
                should_announce_ignored_overrides(),
                "a plain command outside a lane still announces"
            );
        }
    }

    /// No acknowledgement on disk at all is the ordinary starting state: the
    /// gate owes a question about any real fingerprint.
    #[test]
    fn ack_needed_with_nothing_recorded_yet() {
        let home = crate::scratch::root("overrides-ack-nothing-recorded");
        let _ = std::fs::remove_dir_all(&home);
        assert!(ack_needed(&home, "abcd1234"));
    }

    /// Writing an acknowledgement clears the gate for that exact fingerprint,
    /// and brings it right back the moment the fingerprint moves on —
    /// whether or not the file already existed.
    #[test]
    fn ack_write_clears_the_gate_until_the_fingerprint_changes() {
        let home = crate::scratch::root("overrides-ack-roundtrip");
        let _ = std::fs::remove_dir_all(&home);

        ack_write(&home, "abcd1234").unwrap();
        assert!(!ack_needed(&home, "abcd1234"));
        assert!(
            ack_needed(&home, "ffff0000"),
            "a different layer still asks"
        );

        ack_write(&home, "ffff0000").unwrap();
        assert!(!ack_needed(&home, "ffff0000"));
        assert!(
            ack_needed(&home, "abcd1234"),
            "the old fingerprint no longer satisfies the gate"
        );
    }

    /// The warnings screen's own gate is a second, independent instance of
    /// the same mechanism — acknowledging one must never silence the other,
    /// since they ask about two unrelated fingerprints.
    #[test]
    fn warnings_ack_is_independent_of_the_override_ack() {
        let home = crate::scratch::root("overrides-ack-warnings-independence");
        let _ = std::fs::remove_dir_all(&home);

        ack_write(&home, "abcd1234").unwrap();
        assert!(
            warnings_ack_needed(&home, "abcd1234"),
            "the override layer's own ack must not also satisfy the warnings screen"
        );

        warnings_ack_write(&home, "abcd1234").unwrap();
        assert!(!warnings_ack_needed(&home, "abcd1234"));
        assert!(
            !ack_needed(&home, "abcd1234"),
            "and the reverse: the warnings ack must not clear the override gate either"
        );
    }

    /// A list value in a patch reaches [`crate::confkv::set`] as the comma
    /// separated text a person would type at `spoolway config set`, whatever
    /// the scalar type inside it.
    ///
    /// Exercised here rather than through `Config::load` because no field in
    /// the default config is itself a list today — see
    /// `confkv::tests::a_list_of_scalars_gets_kind_list`, which pins the same
    /// branch against a synthetic value for the same reason — so a patch
    /// naming a real key could never reach this arm, and the moment one comes
    /// back this is the rendering it will get.
    #[test]
    fn a_list_in_a_patch_renders_as_the_text_config_set_takes() {
        let value: toml::Value = toml::from_str("k = [\"a\", \"b\"]").unwrap();
        let list = value.get("k").unwrap();
        assert_eq!(render_config_leaf(list).unwrap(), "a, b");

        let numbers: toml::Value = toml::from_str("k = [1, 2]").unwrap();
        assert_eq!(
            render_config_leaf(numbers.get("k").unwrap()).unwrap(),
            "1, 2"
        );

        let empty: toml::Value = toml::from_str("k = []").unwrap();
        assert_eq!(render_config_leaf(empty.get("k").unwrap()).unwrap(), "");
    }

    /// A list holding a table or another list has no text form at all — the
    /// same refusal `confkv::set` makes of a structured entry, made before
    /// the value ever gets there so the error can name what was wrong with
    /// the patch rather than with the config.
    #[test]
    fn a_list_of_structured_entries_is_refused() {
        let nested: toml::Value = toml::from_str("k = [[1, 2]]").unwrap();
        let err = render_config_leaf(nested.get("k").unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("nested value"), "{err:#}");

        let tables: toml::Value = toml::from_str("k = [{ a = 1 }]").unwrap();
        let err = render_config_leaf(tables.get("k").unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("nested value"), "{err:#}");
    }

    /// A scalar renders as the bare text of the value, with no TOML quoting
    /// left on a string — `confkv::set` takes what a shell would have handed
    /// it, not a re-serialised TOML literal.
    #[test]
    fn a_scalar_renders_without_toml_quoting() {
        let value: toml::Value = toml::from_str("s = \"impl\"\nb = true\nn = 30\n").unwrap();
        assert_eq!(render_config_leaf(value.get("s").unwrap()).unwrap(), "impl");
        assert_eq!(render_config_leaf(value.get("b").unwrap()).unwrap(), "true");
        assert_eq!(render_config_leaf(value.get("n").unwrap()).unwrap(), "30");
    }

    /// A pipeline file, real enough for [`promote_pipeline_patch`] to work
    /// on: the actual shipped [`crate::pipeline::KEY_BLOCK`], a `description:`
    /// a project wrote, and two steps.
    fn pipeline_fixture() -> String {
        format!(
            "{}\n\n{}",
            crate::pipeline::key_block(),
            "description: >-\n  A demo pipeline for a promote test.\n\nsteps:\n  - id: \
             implement\n    description: Write the code.\n    agent: claude\n    model: \
             claude-sonnet-5\n    effort: medium\n\n  - id: review\n    description: Check it.\n\
             \x20   agent: claude\n    model: claude-sonnet-5\n"
        )
    }

    fn one_step_patch(step: &str, key: &str, value: &str) -> PipelinePatch {
        let mut fields = serde_norway::Mapping::new();
        fields.insert(
            serde_norway::Value::String(key.to_string()),
            serde_norway::from_str(value).unwrap(),
        );
        let mut steps = BTreeMap::new();
        steps.insert(step.to_string(), fields);
        PipelinePatch {
            description: None,
            task_template: None,
            steps,
        }
    }

    /// `on_loop_max:` is retired, and a patch that sets it is refused the same
    /// way a tracked file naming it is — here rather than by the round-trip,
    /// which drops the `skip_serializing` field without a word. Left to
    /// serde, `spoolway pipeline override <name> --set review.on_loop_max=…`
    /// would be accepted, written into the patch file, and then do nothing every
    /// time the pipelines are loaded after, with `override promote` later dead-ending
    /// on a key the tracked step does not have.
    ///
    /// `apply_step_patch` is both halves of that at once: the merge the
    /// loader runs, and the probe `commands::pipeline_override` runs before
    /// it writes anything.
    #[test]
    fn a_patch_setting_the_retired_on_loop_max_key_is_refused() {
        let mut fields = serde_norway::Mapping::new();
        fields.insert(
            serde_norway::Value::String("on_loop_max".into()),
            serde_norway::Value::String(crate::pipeline::BLOCKED.into()),
        );
        let mut step = Step {
            id: "review".into(),
            ..serde_norway::from_str(
                "id: review
agent: pi
",
            )
            .unwrap()
        };
        let err = apply_step_patch(&mut step, &fields)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`on_loop_max:`"), "the key by name: {err}");
        assert!(
            err.contains("`blocked`"),
            "where a spent budget goes: {err}"
        );
        assert!(
            step.on_loop_max.is_none(),
            "the step must not have been touched on the way to the refusal"
        );
    }

    /// The acceptance criterion in full: promoting changes only the values
    /// named, `KEY_BLOCK` is still findable by `Region::find`, and every
    /// `description:` is still there — because nothing here re-serialises
    /// the file, it only edits one line per named key.
    #[test]
    fn promote_pipeline_patch_changes_only_the_named_values() {
        let root = crate::scratch::root("overrides-promote-pipeline");
        let _ = std::fs::remove_dir_all(&root);
        let original = pipeline_fixture();
        let path = crate::pipeline::Pipelines::file_in(&root, "demo");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &original).unwrap();

        let patch = one_step_patch("implement", "model", "claude-opus-5");
        let changes = promote_pipeline_patch(&root, "demo", &patch).unwrap();
        assert_eq!(
            changes,
            vec![(
                "implement.model".to_string(),
                "claude-sonnet-5".to_string(),
                "claude-opus-5".to_string(),
            )]
        );

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            written,
            original.replacen("model: claude-sonnet-5", "model: claude-opus-5", 1)
        );
        assert!(crate::pipeline::KEY_BLOCK.find(&written).is_some());
        assert!(written.contains("description: Write the code."));
        assert!(written.contains("description: Check it."));
        assert!(written.contains("A demo pipeline for a promote test."));
        // The other step's `model:` is untouched — the edit named one step.
        assert!(written.contains("- id: review\n    description: Check it.\n    agent: claude\n    model: claude-sonnet-5\n"));
    }

    /// A trailing `# comment` on the promoted line survives the edit — only
    /// the value between the key and the comment changes.
    #[test]
    fn promote_pipeline_patch_keeps_a_trailing_comment_on_the_edited_line() {
        let root = crate::scratch::root("overrides-promote-pipeline-comment");
        let _ = std::fs::remove_dir_all(&root);
        let original = format!(
            "{}\n\nsteps:\n  - id: implement\n    agent: claude\n    model: claude-sonnet-5  # \
             pinned for now\n",
            crate::pipeline::key_block()
        );
        let path = crate::pipeline::Pipelines::file_in(&root, "demo");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &original).unwrap();

        let patch = one_step_patch("implement", "model", "claude-opus-5");
        let changes = promote_pipeline_patch(&root, "demo", &patch).unwrap();
        assert_eq!(changes[0].1, "claude-sonnet-5");

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("model: claude-opus-5  # pinned for now"),
            "{written}"
        );
        assert!(!written.contains("claude-sonnet-5"), "{written}");
    }

    #[test]
    fn split_value_and_comment_finds_the_comment_and_trims_both_sides() {
        assert_eq!(
            split_value_and_comment(" claude-sonnet-5  # pinned for now"),
            ("claude-sonnet-5", Some("# pinned for now"))
        );
        assert_eq!(
            split_value_and_comment(" claude-sonnet-5 "),
            ("claude-sonnet-5", None)
        );
    }

    /// A patch naming a top-level key rather than a step's is refused rather
    /// than silently dropped — the line editor has nothing to point at
    /// outside a step's own block.
    #[test]
    fn promote_pipeline_patch_refuses_a_top_level_key() {
        let root = crate::scratch::root("overrides-promote-pipeline-top-level");
        let _ = std::fs::remove_dir_all(&root);
        let path = crate::pipeline::Pipelines::file_in(&root, "demo");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, pipeline_fixture()).unwrap();

        let patch = PipelinePatch {
            description: Some("something else".to_string()),
            task_template: None,
            steps: BTreeMap::new(),
        };
        let err = promote_pipeline_patch(&root, "demo", &patch).unwrap_err();
        assert!(
            format!("{err:#}").contains("only writes step keys"),
            "{err:#}"
        );
    }

    /// `field_display` reads the value a step actually has, rendered the same
    /// way a patch's own value would be — and blank, not an error, for a key
    /// a step happens to leave at its default.
    #[test]
    fn field_display_reads_the_steps_current_value() {
        let pipeline = crate::pipeline::Pipelines::builtin();
        let default = pipeline.pipelines.get("default").unwrap();
        let review = default.steps.iter().find(|s| s.id == "review").unwrap();
        assert_eq!(field_display(review, "agent").unwrap(), "claude");
        assert_eq!(field_display(review, "slot").unwrap(), "");
    }

    /// `render_scalar` refuses a mapping or a sequence: a patch's values are
    /// scalars, and there is no one line to write either of those as.
    #[test]
    fn render_scalar_refuses_a_nested_value() {
        let mapping: serde_norway::Value = serde_norway::from_str("a: 1").unwrap();
        assert!(render_scalar(&mapping).is_err());
        let sequence: serde_norway::Value = serde_norway::from_str("[1, 2]").unwrap();
        assert!(render_scalar(&sequence).is_err());
    }

    /// A patch file saved with a byte order mark reads like one without.
    #[test]
    fn a_pipeline_patch_with_a_byte_order_mark_is_read() {
        let overrides = crate::scratch::root("overrides-patch-bom");
        let _ = std::fs::remove_dir_all(&overrides);
        let path = pipeline_patch_path(&overrides, "impl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "\u{feff}steps:\n  implement:\n    model: m\n").unwrap();

        let patch = read_pipeline_patch(&overrides, "impl").unwrap().unwrap();

        assert_eq!(patch.steps["implement"]["model"].as_str(), Some("m"));
    }

    /// A pipeline patch round-trips through read/write, and a second
    /// `--set` on a different key keeps the first — `write_pipeline_patch`
    /// regenerates the whole file, so this is the guarantee that doing so
    /// never drops an earlier entry.
    #[test]
    fn write_pipeline_patch_keeps_an_earlier_key_from_a_second_write() {
        let overrides = crate::scratch::root("overrides-write-pipeline-patch");
        let _ = std::fs::remove_dir_all(&overrides);

        let first = one_step_patch("implement", "model", "claude-opus-5");
        write_pipeline_patch(&overrides, "impl", &first).unwrap();

        let mut second = read_pipeline_patch(&overrides, "impl").unwrap().unwrap();
        second.steps.entry("test".to_string()).or_default().insert(
            serde_norway::Value::String("timeout".to_string()),
            serde_norway::Value::String("90m".to_string()),
        );
        write_pipeline_patch(&overrides, "impl", &second).unwrap();

        let read_back = read_pipeline_patch(&overrides, "impl").unwrap().unwrap();
        assert_eq!(
            read_back.steps["implement"]["model"].as_str(),
            Some("claude-opus-5")
        );
        assert_eq!(read_back.steps["test"]["timeout"].as_str(), Some("90m"));
    }

    /// Removing the last patch removes the now-empty `pipelines/` directory
    /// with it, and the layer directory too once that was the only thing in
    /// it — "no overrides at all" is how a project with nothing layered is
    /// spelled.
    #[test]
    fn remove_pipeline_patch_clears_now_empty_directories() {
        let overrides = crate::scratch::root("overrides-remove-pipeline-patch");
        let _ = std::fs::remove_dir_all(&overrides);

        let patch = one_step_patch("implement", "model", "claude-opus-5");
        write_pipeline_patch(&overrides, "impl", &patch).unwrap();
        assert!(pipeline_patch_path(&overrides, "impl").is_file());

        remove_pipeline_patch(&overrides, "impl").unwrap();
        assert!(!pipeline_patch_path(&overrides, "impl").is_file());
        assert!(!overrides.join(PIPELINES_SUBDIR).is_dir());
        assert!(!overrides.is_dir());
    }

    /// A sibling entry keeps the layer directory alive: removing one patch
    /// never disturbs another.
    #[test]
    fn remove_pipeline_patch_leaves_a_sibling_entry_alone() {
        let overrides = crate::scratch::root("overrides-remove-pipeline-patch-sibling");
        let _ = std::fs::remove_dir_all(&overrides);

        write_pipeline_patch(
            &overrides,
            "impl",
            &one_step_patch("implement", "model", "claude-opus-5"),
        )
        .unwrap();
        write_pipeline_patch(
            &overrides,
            "bugfix",
            &one_step_patch("reproduce", "model", "claude-opus-5"),
        )
        .unwrap();

        remove_pipeline_patch(&overrides, "impl").unwrap();
        assert!(!pipeline_patch_path(&overrides, "impl").is_file());
        assert!(pipeline_patch_path(&overrides, "bugfix").is_file());
        assert!(overrides.is_dir());
    }

    /// A private layer written for an older release may still hold
    /// `dispatch.keep_finished_lanes`. It applies as the limit it stood for —
    /// `true` is `all`, `false` is 1 — and is not reported as ignored, or
    /// `false` would quietly become the default 5. A `visible_lanes` in the
    /// same layer wins, as it does in the tracked file.
    #[test]
    fn a_layer_keep_finished_lanes_applies_as_a_visible_lanes_limit() {
        use crate::config::VisibleLanes;
        let overrides = crate::scratch::root("overrides-layer-keep-finished");
        std::fs::create_dir_all(&overrides).unwrap();
        for (layer, expected) in [
            ("keep_finished_lanes = true\n", VisibleLanes::All),
            ("keep_finished_lanes = false\n", VisibleLanes::Count(1)),
            (
                "keep_finished_lanes = false\nvisible_lanes = 3\n",
                VisibleLanes::Count(3),
            ),
        ] {
            std::fs::write(
                config_patch_path(&overrides),
                format!("[dispatch]\n{layer}"),
            )
            .unwrap();
            let (config, ignored) = apply_config_patch(Config::default(), &overrides).unwrap();
            assert_eq!(config.dispatch.visible_lanes, expected, "{layer}");
            assert!(ignored.is_empty(), "{layer}: {ignored:?}");
        }
    }

    /// A layer's `keep_finished_lanes` earns the same load note as the
    /// tracked file's, so a person who has the key only in the layer is told
    /// to run `spoolway sync`. A `visible_lanes` beside it is the limit the
    /// note names, unless that one was refused; a key that is not a boolean
    /// earns no note, since it is already reported as ignored.
    #[test]
    fn a_layer_keep_finished_lanes_earns_the_tracked_files_note() {
        let overrides = crate::scratch::root("overrides-layer-keep-finished-note");
        std::fs::create_dir_all(&overrides).unwrap();
        for (layer, expected) in [
            (
                "keep_finished_lanes = true\n",
                Some("it loads as dispatch.visible_lanes = all. Run `spoolway sync` to rewrite"),
            ),
            (
                "keep_finished_lanes = false\n",
                Some("it loads as dispatch.visible_lanes = 1. Run `spoolway sync` to rewrite"),
            ),
            (
                "keep_finished_lanes = false\nvisible_lanes = 3\n",
                Some(
                    "the dispatch.visible_lanes = 3 beside it is the limit that loads. Run `spoolway sync` to drop",
                ),
            ),
            (
                "keep_finished_lanes = false\nvisible_lanes = 0\n",
                Some("it loads as dispatch.visible_lanes = 1. Run `spoolway sync` to rewrite"),
            ),
            ("keep_finished_lanes = \"yes\"\n", None),
            ("visible_lanes = 3\n", None),
        ] {
            std::fs::write(
                config_patch_path(&overrides),
                format!("[dispatch]\n{layer}"),
            )
            .unwrap();
            let (config, ignored) = apply_config_patch(Config::default(), &overrides).unwrap();
            let note = keep_finished_lanes_note(&overrides, &config, &ignored).unwrap();
            match expected {
                Some(expected) => {
                    let note = note.unwrap_or_else(|| panic!("{layer}: no note"));
                    assert!(note.contains(expected), "{layer}: {note}");
                    assert!(
                        note.contains(&config_patch_path(&overrides).display().to_string()),
                        "{note}"
                    );
                }
                None => assert_eq!(note, None, "{layer}"),
            }
        }
    }

    /// `spoolway sync` plans the retired layer key out, and what it leaves
    /// must set the same limit: the key is replaced by `visible_lanes`, not
    /// just deleted, unless the layer already holds one.
    #[test]
    fn sync_rewrites_a_layer_keep_finished_lanes_into_visible_lanes() {
        let overrides = crate::scratch::root("overrides-layer-keep-finished-sync");
        std::fs::create_dir_all(&overrides).unwrap();
        for (layer, expected) in [
            (
                "keep_finished_lanes = true\n",
                Some("visible_lanes = \"all\""),
            ),
            ("keep_finished_lanes = false\n", Some("visible_lanes = 1")),
            (
                "keep_finished_lanes = false\nvisible_lanes = 3\n",
                Some("visible_lanes = 3"),
            ),
            // Not a boolean, so it stood for no limit: it is only dropped,
            // and named once, though it is both refused and on the layer.
            ("keep_finished_lanes = \"yes\"\n", None),
        ] {
            std::fs::write(
                config_patch_path(&overrides),
                format!("[dispatch]\nauto_commit = false\n{layer}"),
            )
            .unwrap();
            let dropped = retired_config_patch_keys(&overrides, &Config::default()).unwrap();
            assert_eq!(dropped, vec!["dispatch.keep_finished_lanes".to_string()]);

            let rewritten = config_patch_without(&overrides, &dropped).unwrap();
            assert!(!rewritten.contains("keep_finished_lanes"), "{rewritten}");
            assert!(rewritten.contains("auto_commit = false"), "{rewritten}");
            match expected {
                Some(expected) => {
                    assert!(rewritten.contains(expected), "{layer}: {rewritten}");
                    assert_eq!(rewritten.matches("visible_lanes").count(), 1, "{rewritten}");
                }
                None => assert!(!rewritten.contains("visible_lanes"), "{rewritten}"),
            }
        }
    }

    /// `promote_config_patch` writes through `confkv::set`/`Config::edit_key`,
    /// so a comment beside the promoted key survives — the same guarantee
    /// `spoolway config set` already gives, extended to a whole patch.
    #[test]
    fn promote_config_patch_keeps_comments_and_writes_every_key() {
        let root = crate::scratch::root("overrides-promote-config");
        let home = crate::scratch::root("overrides-promote-config-home");
        let _ = std::fs::remove_dir_all(&root);
        crate::scratch::stamped(&root);
        std::fs::create_dir_all(root.join(".spoolway")).unwrap();
        std::fs::write(
            Config::path_in(&root),
            "[unattended]\n# chosen for this project\nblocked_agent = \"one\"\n",
        )
        .unwrap();

        // `dir_for` -> `mux::project_home` resolves under `~/.spoolway` — a
        // real `$HOME`, unswapped, sends this test's own fixture there. See
        // issue #188.
        crate::platform::test_home::with_home(&home, || {
            let overrides = dir_for(&root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                config_patch_path(&overrides),
                "[unattended]\nblocked_agent = \"two\"\n",
            )
            .unwrap();

            let changes = promote_config_patch(&root).unwrap();
            assert_eq!(
                changes,
                vec![("unattended.blocked_agent".to_string(), "two".to_string())]
            );

            let written = std::fs::read_to_string(Config::path_in(&root)).unwrap();
            assert!(written.contains("# chosen for this project"));
            assert!(written.contains("blocked_agent = \"two\""));
        });
    }

    /// A layer holding a key the tracked config refuses writes nothing: the
    /// valid key ahead of the typo stays out of the tracked file, and the
    /// error names the typo. Before, the valid key was already saved by the
    /// time the typo stopped the promote.
    #[test]
    fn promote_config_patch_writes_nothing_when_a_key_is_refused() {
        let root = crate::scratch::root("overrides-promote-config-refused");
        let home = crate::scratch::root("overrides-promote-config-refused-home");
        let _ = std::fs::remove_dir_all(&root);
        crate::scratch::stamped(&root);
        std::fs::create_dir_all(root.join(".spoolway")).unwrap();
        let tracked = "[dispatch]\nauto_commit = false\n";
        std::fs::write(Config::path_in(&root), tracked).unwrap();

        crate::platform::test_home::with_home(&home, || {
            let overrides = dir_for(&root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                config_patch_path(&overrides),
                "[dispatch]\nauto_commit = true\nlane_quite = \"7m\"\n",
            )
            .unwrap();

            let err = format!("{:#}", promote_config_patch(&root).unwrap_err());
            assert!(err.contains("lane_quite"), "{err}");
            assert_eq!(err.matches("lane_quite").count(), 1, "named once: {err}");
            assert_eq!(
                std::fs::read_to_string(Config::path_in(&root)).unwrap(),
                tracked
            );
            assert!(config_patch_path(&overrides).is_file());
        });
    }

    /// A project with no tracked `config.toml` yet still promotes: the file is
    /// created from the config the layer produces, as `config set` creates
    /// it, and the layer is left for the caller to clear.
    #[test]
    fn promote_config_patch_creates_a_missing_tracked_file() {
        let root = crate::scratch::root("overrides-promote-config-absent");
        let home = crate::scratch::root("overrides-promote-config-absent-home");
        let _ = std::fs::remove_dir_all(&root);
        crate::scratch::stamped(&root);

        crate::platform::test_home::with_home(&home, || {
            let overrides = dir_for(&root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                config_patch_path(&overrides),
                "[dispatch]\nauto_commit = false\n",
            )
            .unwrap();
            assert!(!Config::path_in(&root).exists());

            let changes = promote_config_patch(&root).unwrap();
            assert_eq!(changes.len(), 1);
            assert!(!Config::load_tracked(&root).unwrap().dispatch.auto_commit);
        });
    }

    /// Dropping an entry that was never there is refused by name, not a
    /// silent no-op — the same courtesy `override promote` gets from the
    /// same check.
    #[test]
    fn remove_pipeline_patch_refuses_a_missing_entry() {
        let overrides = crate::scratch::root("overrides-remove-missing");
        let _ = std::fs::remove_dir_all(&overrides);
        let err = remove_pipeline_patch(&overrides, "impl").unwrap_err();
        assert!(format!("{err:#}").contains("no override"), "{err:#}");
    }

    /// One bad value already in the tracked config affects only itself: an
    /// override key that touches neither field of a hand-written
    /// `session_blocked_ctx` / `session_reuse_ctx` pair still applies, and
    /// nothing is reported as ignored.
    #[test]
    fn an_override_key_still_applies_beside_a_bad_hand_written_session_pair() {
        let overrides = crate::scratch::root("overrides-bad-pair-beside");
        let _ = std::fs::remove_dir_all(&overrides);
        std::fs::create_dir_all(&overrides).unwrap();
        std::fs::write(
            config_patch_path(&overrides),
            "[agents.claude]\nconcurrency = 2\n",
        )
        .unwrap();

        let mut tracked = Config::default();
        let claude = tracked.agents.get_mut("claude").unwrap();
        claude.session_reuse_ctx = 20;
        claude.session_blocked_ctx = 15;

        let (patched, ignored) = apply_config_patch(tracked, &overrides).unwrap();
        assert!(ignored.is_empty(), "{ignored:?}");
        assert_eq!(patched.agents["claude"].concurrency, 2);
    }

    /// `tracked` with its `claude` profile at 20/40 and the layer `layer`
    /// applied on top, for the session-threshold pair tests.
    fn pair_over_twenty_forty(layer: &str) -> (Config, Vec<Ignored>) {
        let overrides = crate::scratch::root("overrides-session-pair");
        let _ = std::fs::remove_dir_all(&overrides);
        std::fs::create_dir_all(&overrides).unwrap();
        std::fs::write(config_patch_path(&overrides), layer).unwrap();
        let mut tracked = Config::default();
        let claude = tracked.agents.get_mut("claude").unwrap();
        claude.session_reuse_ctx = 20;
        claude.session_blocked_ctx = 40;
        apply_config_patch(tracked, &overrides).unwrap()
    }

    /// A pair that is valid as a pair applies whole. The layer is a sorted
    /// table, so the walk always reaches `session_blocked_ctx` first and the
    /// order the file lists them in cannot matter; there, 12 was refused under
    /// a reuse of 20 and the layer applied the reuse half only.
    #[test]
    fn a_session_pair_valid_together_applies_whole() {
        let (config, ignored) = pair_over_twenty_forty(
            "[agents.claude]\nsession_blocked_ctx = 12\nsession_reuse_ctx = 5\n",
        );
        assert!(ignored.is_empty(), "{ignored:?}");
        let claude = &config.agents["claude"];
        assert_eq!(
            (claude.session_reuse_ctx, claude.session_blocked_ctx),
            (5, 12)
        );
    }

    /// [`settle`] gives the same result whichever of the two keys it is handed
    /// first: the blocked ceiling first is refused until the reuse threshold
    /// is in, and the reuse threshold first is not refused at all.
    #[test]
    fn settle_applies_a_session_pair_in_either_order() {
        let reuse = (
            "agents.claude.session_reuse_ctx".to_string(),
            "5".to_string(),
        );
        let blocked = (
            "agents.claude.session_blocked_ctx".to_string(),
            "12".to_string(),
        );
        for (first, leaves) in [
            ("reuse", vec![reuse.clone(), blocked.clone()]),
            ("blocked", vec![blocked, reuse]),
        ] {
            let mut config = Config::default();
            let claude = config.agents.get_mut("claude").unwrap();
            claude.session_reuse_ctx = 20;
            claude.session_blocked_ctx = 40;
            let refused = settle(&mut config, &leaves, |config, key, input| {
                crate::confkv::set(config, key, input)
            });
            assert!(refused.is_empty(), "{first} first: {refused:?}");
            let claude = &config.agents["claude"];
            assert_eq!(
                (claude.session_reuse_ctx, claude.session_blocked_ctx),
                (5, 12),
                "{first} first"
            );
        }
    }

    /// A pair that is wrong together is still refused, and still reported:
    /// settling retries what was refused, it does not waive the check.
    #[test]
    fn a_session_pair_wrong_together_is_still_ignored_with_its_note() {
        let (config, ignored) = pair_over_twenty_forty(
            "[agents.claude]\nsession_reuse_ctx = 30\nsession_blocked_ctx = 10\n",
        );
        assert_eq!(ignored.len(), 1, "{ignored:?}");
        assert_eq!(ignored[0].fields, "agents.claude.session_blocked_ctx");
        assert!(ignored[0].reason.contains("must be above"), "{ignored:?}");
        assert_eq!(config.agents["claude"].session_blocked_ctx, 40);
    }

    /// `override promote` writes the same pair the layer applies, where it
    /// refused the whole layer before.
    #[test]
    fn promote_config_patch_writes_a_session_pair_valid_together() {
        let root = crate::scratch::root("overrides-promote-pair");
        let home = crate::scratch::root("overrides-promote-pair-home");
        let _ = std::fs::remove_dir_all(&root);
        crate::scratch::stamped(&root);
        std::fs::create_dir_all(root.join(".spoolway")).unwrap();
        std::fs::write(
            Config::path_in(&root),
            "[agents.claude]\nsession_reuse_ctx = 20\nsession_blocked_ctx = 40\n",
        )
        .unwrap();

        crate::platform::test_home::with_home(&home, || {
            let overrides = dir_for(&root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                config_patch_path(&overrides),
                "[agents.claude]\nsession_reuse_ctx = 5\nsession_blocked_ctx = 12\n",
            )
            .unwrap();

            let changes = promote_config_patch(&root).unwrap();
            assert_eq!(changes.len(), 2, "{changes:?}");
            let claude = &Config::load_tracked(&root).unwrap().agents["claude"];
            assert_eq!(
                (claude.session_reuse_ctx, claude.session_blocked_ctx),
                (5, 12)
            );
        });
    }

    /// A layer written for an older release sets `visible_lanes` through the
    /// retired `keep_finished_lanes`, and a models glob with a dot in it is
    /// spelled as the key a person types; both are found by their current key.
    #[test]
    fn config_patch_sets_reads_a_key_as_the_layer_applies_it() {
        let overrides = crate::scratch::root("overrides-patch-sets");
        let _ = std::fs::remove_dir_all(&overrides);
        std::fs::create_dir_all(&overrides).unwrap();
        assert!(!config_patch_sets(&overrides, "dispatch.visible_lanes").unwrap());
        std::fs::write(
            config_patch_path(&overrides),
            "[dispatch]\nkeep_finished_lanes = true\n[models.\"gpt-5.1\"]\ncompact_ctx = 50\n",
        )
        .unwrap();
        assert!(config_patch_sets(&overrides, "dispatch.visible_lanes").unwrap());
        assert!(!config_patch_sets(&overrides, "dispatch.keep_finished_lanes").unwrap());
        assert!(config_patch_sets(&overrides, "models.gpt-5.1.compact_ctx").unwrap());
        assert!(config_patch_sets(&overrides, "models.\"gpt-5.1\".compact_ctx").unwrap());
        assert!(!config_patch_sets(&overrides, "dispatch.lane_quiet").unwrap());
    }
}
