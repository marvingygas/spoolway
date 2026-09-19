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
//! spliced step is a change to the graph, `spoolway pipeline gen`'s job, not
//! an override's.
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
/// run from the main checkout does, with nothing copied in. A path with no
/// git repository behind it at all — a bare fixture directory, most of the
/// unit tests below `Pipelines::load` and `Config::load` — resolves to
/// itself instead; the directory this then names is simply never on disk,
/// which reads exactly like a project that has overridden nothing.
pub(crate) fn dir_for(root: &Path) -> Result<PathBuf> {
    let main = crate::repo::main_checkout(root).unwrap_or_else(|| root.to_path_buf());
    Ok(crate::mux::project_home(&main)?.join(crate::config::OVERRIDES_DIR))
}

/// The shape `overrides/pipelines/<name>.yml` is allowed to take: top-level
/// keys a pipeline itself carries, and `steps:` keyed by id rather than the
/// tracked file's own ordered list — a patch names a step, never a
/// position, since position is what decides slot priority.
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
/// already parsed from the tracked file.
///
/// Called from [`crate::pipeline::Pipelines::load`] before
/// [`crate::pipeline::Pipelines::assemble`] runs, per `d-merge-at-load`:
/// assembling first would materialise a `blocked` step that a pipeline
/// declaring none of its own never had in the file, letting a patch reach a
/// step that, from the file's own perspective, does not exist.
pub(crate) fn apply_pipeline_patch(pipeline: &mut Pipeline, overrides: &Path) -> Result<()> {
    let path = overrides
        .join(PIPELINES_SUBDIR)
        .join(format!("{}.yml", pipeline.name));
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let patch: PipelinePatch =
        serde_norway::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;

    if let Some(description) = patch.description {
        pipeline.description = Some(description);
    }
    if let Some(task_template) = patch.task_template {
        pipeline.task_template = Some(task_template);
    }
    for (id, fields) in patch.steps {
        let step = pipeline
            .steps
            .iter_mut()
            .find(|s| s.id == id)
            .with_context(|| {
                format!(
                    "{} names step `{id}`, which pipeline `{}` does not have — a patch may \
                     only set keys on a step that already exists",
                    path.display(),
                    pipeline.name,
                )
            })?;
        apply_step_patch(step, &fields)
            .with_context(|| format!("step `{id}` in {}", path.display()))?;
    }
    Ok(())
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
pub(crate) fn apply_step_patch(step: &mut Step, fields: &serde_norway::Mapping) -> Result<()> {
    if fields.contains_key("id") {
        bail!(
            "sets `id:` — a patch may only set a value on an existing step, never rename or \
             reposition one"
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
/// produce a config that command would have refused.
pub(crate) fn apply_config_patch(config: Config, overrides: &Path) -> Result<Config> {
    let path = config_patch_path(overrides);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(config),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let parsed: toml::Value = raw
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let toml::Value::Table(table) = parsed else {
        bail!("{} must be a table of settings", path.display());
    };
    apply_config_table(config, &table, &mut String::new())
        .with_context(|| format!("in {}", path.display()))
}

fn apply_config_table(
    mut config: Config,
    table: &toml::value::Table,
    prefix: &mut String,
) -> Result<Config> {
    for (key, value) in table {
        let mark = prefix.len();
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(key);
        config = match value {
            toml::Value::Table(nested) => apply_config_table(config, nested, prefix)?,
            other => {
                let input = render_config_leaf(other).with_context(|| format!("`{prefix}`"))?;
                crate::confkv::set(&config, prefix, &input)?
            }
        };
        prefix.truncate(mark);
    }
    Ok(config)
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

/// `overrides/prompts/<name>/PROMPT.md`, if it exists — the whole file,
/// replacing the tracked one rather than merging into it: prose has no key
/// for a patch to aim at.
pub(crate) fn prompt_override(overrides: &Path, name: &str) -> Option<PathBuf> {
    let path = prompt_patch_path(overrides, name);
    path.is_file().then_some(path)
}

// ---------------------------------------------------------------------------
// The gate: `commands::dispatch`'s standing consent check reads and writes
// an acknowledgement here, keyed on `version::layer_fingerprint` rather than
// on anything the merge above computes — an override is allowed to be
// permanent, so this carries no timer and returns the moment a byte of the
// layer changes underneath it.
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
/// own, from [`crate::version::layer_fingerprint`], never `stamp`'s combined
/// one: a tracked-file edit alone must not reopen a gate the layer itself
/// has not moved.
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
            serde_norway::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?,
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
    let parsed: toml::Value = raw
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
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
/// through the same [`crate::confkv::set`] / `Config::save_key` pair
/// `spoolway config set` already writes through — so every comment in
/// `config.toml` survives, exactly as it would if a person had typed
/// `config set` once per key by hand.
///
/// Returns the promoted keys and their new values, for
/// `commands::override_promote` to print.
pub(crate) fn promote_config_patch(root: &Path) -> Result<Vec<(String, String)>> {
    let overrides = dir_for(root)?;
    let path = config_patch_path(&overrides);
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: toml::Value = raw
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let toml::Value::Table(table) = parsed else {
        bail!("{} must be a table of settings", path.display());
    };

    let mut config = Config::load_tracked(root)?;
    let mut changes = Vec::new();
    promote_config_table(&mut config, root, &table, &mut String::new(), &mut changes)?;
    Ok(changes)
}

fn promote_config_table(
    config: &mut Config,
    root: &Path,
    table: &toml::value::Table,
    prefix: &mut String,
    changes: &mut Vec<(String, String)>,
) -> Result<()> {
    for (key, value) in table {
        let mark = prefix.len();
        if !prefix.is_empty() {
            prefix.push('.');
        }
        prefix.push_str(key);
        match value {
            toml::Value::Table(nested) => {
                promote_config_table(config, root, nested, prefix, changes)?
            }
            other => {
                let input = render_config_leaf(other).with_context(|| format!("`{prefix}`"))?;
                *config = crate::confkv::set(config, prefix, &input)?;
                config.save_key(root, prefix)?;
                changes.push((prefix.clone(), input));
            }
        }
        prefix.truncate(mark);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let value: toml::Value = "k = [\"a\", \"b\"]".parse().unwrap();
        let list = value.get("k").unwrap();
        assert_eq!(render_config_leaf(list).unwrap(), "a, b");

        let numbers: toml::Value = "k = [1, 2]".parse().unwrap();
        assert_eq!(
            render_config_leaf(numbers.get("k").unwrap()).unwrap(),
            "1, 2"
        );

        let empty: toml::Value = "k = []".parse().unwrap();
        assert_eq!(render_config_leaf(empty.get("k").unwrap()).unwrap(), "");
    }

    /// A list holding a table or another list has no text form at all — the
    /// same refusal `confkv::set` makes of a structured entry, made before
    /// the value ever gets there so the error can name what was wrong with
    /// the patch rather than with the config.
    #[test]
    fn a_list_of_structured_entries_is_refused() {
        let nested: toml::Value = "k = [[1, 2]]".parse().unwrap();
        let err = render_config_leaf(nested.get("k").unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("nested value"), "{err:#}");

        let tables: toml::Value = "k = [{ a = 1 }]".parse().unwrap();
        let err = render_config_leaf(tables.get("k").unwrap()).unwrap_err();
        assert!(format!("{err:#}").contains("nested value"), "{err:#}");
    }

    /// A scalar renders as the bare text of the value, with no TOML quoting
    /// left on a string — `confkv::set` takes what a shell would have handed
    /// it, not a re-serialised TOML literal.
    #[test]
    fn a_scalar_renders_without_toml_quoting() {
        let value: toml::Value = "s = \"impl\"\nb = true\nn = 30\n".parse().unwrap();
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

    /// `promote_config_patch` writes through `confkv::set`/`Config::save_key`,
    /// so a comment beside the promoted key survives — the same guarantee
    /// `spoolway config set` already gives, extended to a whole patch.
    #[test]
    fn promote_config_patch_keeps_comments_and_writes_every_key() {
        let root = crate::scratch::root("overrides-promote-config");
        let home = crate::scratch::root("overrides-promote-config-home");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".spoolway")).unwrap();
        std::fs::write(
            Config::path_in(&root),
            "[dispatch]\n# chosen for this project\nworktree_root = \"/one\"\n",
        )
        .unwrap();

        // `dir_for` -> `mux::project_home` resolves under `~/.spoolway` for a
        // bare fixture directory with no git repository behind it — a real
        // `$HOME`, unswapped, sends this test's own fixture there. See
        // issue #188.
        crate::platform::test_home::with_home(&home, || {
            let overrides = dir_for(&root).unwrap();
            std::fs::create_dir_all(&overrides).unwrap();
            std::fs::write(
                config_patch_path(&overrides),
                "[dispatch]\nworktree_root = \"/two\"\n",
            )
            .unwrap();

            let changes = promote_config_patch(&root).unwrap();
            assert_eq!(
                changes,
                vec![("dispatch.worktree_root".to_string(), "/two".to_string())]
            );

            let written = std::fs::read_to_string(Config::path_in(&root)).unwrap();
            assert!(written.contains("# chosen for this project"));
            assert!(written.contains("worktree_root = \"/two\""));
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
}
