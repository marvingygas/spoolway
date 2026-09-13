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
//! an override's. There is no command here yet to create, list, promote or
//! drop an entry; this module only reads what is already on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::config::Config;
use crate::pipeline::{Pipeline, Step};

const PIPELINES_SUBDIR: &str = "pipelines";
const PROMPTS_SUBDIR: &str = "prompts";

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
pub(crate) fn dir_for(root: &Path) -> PathBuf {
    let main = crate::repo::main_checkout(root).unwrap_or_else(|| root.to_path_buf());
    crate::mux::project_home(&main).join(crate::config::OVERRIDES_DIR)
}

/// The shape `overrides/pipelines/<name>.yml` is allowed to take: top-level
/// keys a pipeline itself carries, and `steps:` keyed by id rather than the
/// tracked file's own ordered list — a patch names a step, never a
/// position, since position is what decides slot priority.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PipelinePatch {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    task_template: Option<String>,
    #[serde(default)]
    steps: BTreeMap<String, serde_norway::Mapping>,
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
fn apply_step_patch(step: &mut Step, fields: &serde_norway::Mapping) -> Result<()> {
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
    let path = overrides.join(crate::config::CONFIG_FILE);
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
    let path = overrides
        .join(PROMPTS_SUBDIR)
        .join(name)
        .join(crate::assets::PROMPT_FILE);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
