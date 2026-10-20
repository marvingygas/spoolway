//! `spoolway override list|promote|drop`, `spoolway config override`, and
//! `spoolway prompt override` — everything that reads or writes the patch
//! layer as a whole, rather than one pipeline's own fork (`commands::
//! pipeline_override`, which lives beside the rest of the pipeline family
//! instead).
//!
//! Every command here works from `repo.overrides_dir()` — project-wide,
//! never `repo.checkout` — because the layer is a sibling of `queue/`, one
//! per project, not one per worktree; see [`crate::overrides`]'s own doc.
//! `override promote` is the one exception that also touches a *tracked*
//! file. It writes into [`Repo::tracked_write_root`], the same place
//! [`crate::commands::config_set`] does: this checkout's own `.spoolway/`,
//! which in a worktree of its own is a copy the dispatcher does not read
//! until the branch merges.

use super::*;

/// What a target string names — `pipelines/<name>.yml`, `prompts/<name>`,
/// `config.toml`, or a bare pipeline name, the short form the mockup this
/// implements types at the prompt (`override promote impl`, not `override
/// promote pipelines/impl.yml`) — both spellings of a pipeline resolve the
/// same way, since only a pipeline's own target is ever bare.
enum Target {
    Pipeline(String),
    Prompt(String),
    Config,
}

fn parse_target(target: &str) -> Result<Target> {
    if target == crate::config::CONFIG_FILE {
        return Ok(Target::Config);
    }
    if let Some(rest) = target.strip_prefix("pipelines/")
        && let Some(name) = rest.strip_suffix(".yml")
    {
        return Ok(Target::Pipeline(name.to_string()));
    }
    if let Some(name) = target.strip_prefix("prompts/") {
        return Ok(Target::Prompt(name.to_string()));
    }
    if !target.contains('/') {
        return Ok(Target::Pipeline(target.to_string()));
    }
    bail!(
        "`{target}` is not an override target — see `spoolway override list`, which prints \
         `pipelines/<name>.yml`, `prompts/<name>` or `config.toml`"
    );
}

/// `spoolway prompt override <name>`: copy the tracked prompt into the
/// layer, so editing starts from it. Reads `repo.checkout` — the branch
/// actually running, same as [`prompt::show`] — and writes the project-wide
/// layer, so the fork is there for a lane in any worktree on its next start.
pub fn prompt_override(repo: &Repo, name: &str) -> Result<()> {
    if let Some(note) = repo.checkout_note()? {
        note.print(false)?;
    }
    let tracked = crate::prompt::path_for_tracked(repo, name);
    // A private prompt is never tracked, so forking from it reads
    // `local/prompts/` instead — but the layer this writes into only ever
    // replaces a *tracked* file at `prompt::path_for` (it never shadows a
    // private one), so a private prompt's fork sits waiting rather than
    // taking effect the moment this command writes it — promoting the
    // pipeline that runs it is what starts it applying.
    let (source, private) = if tracked.is_file() {
        (tracked.clone(), false)
    } else if let Some(private) = crate::prompt::private_only_path(repo, name) {
        (private, true)
    } else {
        (tracked.clone(), false)
    };
    let body = std::fs::read_to_string(&source)
        .with_context(|| format!("no prompt named `{name}` at {}", tracked.display()))?;

    let overrides_dir = repo.overrides_dir();
    let target = crate::overrides::prompt_patch_path(&overrides_dir, name);
    write_atomic(&target, &body)?;

    println!("  wrote {}", target.display());
    println!();
    if private {
        println!(
            "  waiting on a promote — prompt `{name}` is private; this fork only starts applying \
             once it is tracked. `spoolway override drop prompts/{name}` to clear it."
        );
    } else {
        println!("  active on the next lane. `spoolway override drop prompts/{name}` to clear it.");
    }
    Ok(())
}

/// `spoolway config override`: open (creating if absent) `overrides/
/// config.toml` — the layer's own copy, never the tracked file `config
/// edit` opens.
///
/// `home_error` is `Repo::discover_lenient`'s own report of a real failure
/// resolving `repo`'s stamped home. `repo.overrides_dir()` is nothing but
/// `repo.home` joined onto a constant, and `repo.home` here is only the
/// basename-keyed placeholder a failed resolution falls back to — writing
/// into it, as `write_atomic` below would, is writing into the wrong
/// project's home, so this refuses before doing anything else rather than
/// silently guess.
pub fn config_override(repo: &Repo, home_error: Option<&anyhow::Error>) -> Result<()> {
    if let Some(err) = home_error {
        return Err(anyhow::anyhow!(
            "{err:#} — {}'s stamped home could not be resolved, so `config override` has \
             nowhere real to write; run `spoolway doctor` to see why",
            repo.root.display()
        ));
    }
    if let Some(note) = repo.checkout_note()? {
        note.print(false)?;
    }
    let path = crate::overrides::config_patch_path(&repo.overrides_dir());
    if !path.is_file() {
        write_atomic(
            &path,
            "# Every key here overrides the same key in .spoolway/config.toml.\n\
             # `spoolway override promote config.toml` writes these into the tracked file.\n",
        )?;
    }

    super::config::open_in_editor(&path)?;

    // Two files could be why this now fails to merge, and a person reopening
    // `overrides/config.toml` needs to be told which. The tracked file might
    // already be broken — not this command's to fix, `spoolway config edit`
    // is — or the patch they just wrote might be the problem. Checked
    // separately rather than through `Config::load`, which would blur the
    // two into one error that always seems to name whichever file it
    // happens to mention.
    let tracked = Config::load_tracked(&repo.checkout).with_context(|| {
        format!(
            "the tracked config no longer parses — that is `spoolway config edit`'s to fix, \
             not `override`'s: {}",
            Config::path_in(&repo.checkout).display()
        )
    })?;
    crate::overrides::apply_config_patch(tracked, &repo.overrides_dir())
        .map(|_| ())
        .with_context(|| {
            format!(
                "{} no longer merges — reopen it with `spoolway config override`",
                path.display()
            )
        })?;

    println!("{} — parses", path.display());
    Ok(())
}

/// One row of `spoolway override list` — its own type, and
/// [`collect_override_rows`] its own function, so a test can check what the
/// layer holds without parsing a column of printed text.
///
/// `pub(crate)`, fields included: `commands::dispatch`'s own gate and
/// `commands::doctor`'s standing note both read the layer this same way,
/// rather than each re-deriving "what is overridden" from `crate::overrides`
/// on its own.
pub(crate) struct OverrideRow {
    pub(crate) target: String,
    pub(crate) kind: &'static str,
    pub(crate) overrides: String,
    /// Entries this row's file holds that were left out of the merge — see
    /// [`crate::overrides::Ignored`]. Empty for every row where nothing in
    /// the layer is stale.
    pub(crate) ignored: Vec<crate::overrides::Ignored>,
}

/// Every entry the layer under `repo.overrides_dir()` currently holds, in the
/// order `override list` prints them: pipelines, then prompts, then config.
///
/// A pipeline patch's staleness is worked out against the *tracked*
/// pipelines — [`crate::pipeline::Pipelines::load_tracked`] — the same check
/// [`crate::pipeline::Pipelines::load`] runs at every ordinary load, run
/// again here rather than threaded through it, so `override list` sees the
/// same answer whether or not anything has loaded the merged set yet this
/// process. A tracked pipeline that will not even parse leaves staleness
/// undetermined rather than failing the whole row: that tracked file is its
/// own problem, `spoolway pipeline check`'s to report, not a reason to go
/// blind about what the layer holds.
pub(crate) fn collect_override_rows(repo: &Repo) -> Result<Vec<OverrideRow>> {
    let dir = repo.overrides_dir();
    let tracked = Pipelines::load_tracked(&repo.checkout, &repo.config).ok();
    let mut rows = Vec::new();

    for name in crate::overrides::list_pipeline_patches(&dir)? {
        let patch = crate::overrides::read_pipeline_patch(&dir, &name)?.unwrap_or_default();
        let mut keys = Vec::new();
        if patch.description.is_some() {
            keys.push("description".to_string());
        }
        if patch.task_template.is_some() {
            keys.push("task_template".to_string());
        }

        // Four answers, not three: the tracked pipelines failed to load at
        // all (`None` — that tracked file's own problem, not this patch's to
        // report, so staleness here is undetermined rather than assumed),
        // they loaded and have this pipeline (dry-run the patch against it,
        // step by step, same as `Pipelines::load` does for real), they
        // loaded and do not but a private pipeline of the same name is
        // waiting on `pipeline promote` — [`Ignored::private_pipeline`], not
        // [`Ignored::missing_pipeline`], since the name is not wrong, only
        // not tracked yet — or the name is nowhere at all, the identical
        // reason `Pipelines::load` would have skipped it for.
        let ignored = match &tracked {
            None => Vec::new(),
            Some(tracked) => match tracked.pipelines.get(&name) {
                Some(pipeline) => {
                    let mut probe = pipeline.clone();
                    crate::overrides::apply_pipeline_patch(&mut probe, &dir)?
                }
                None if crate::pipeline::Pipelines::private(&repo.checkout, &name)?.is_some() => {
                    vec![crate::overrides::Ignored::private_pipeline(&name)]
                }
                None => vec![crate::overrides::Ignored::missing_pipeline(&name)],
            },
        };
        // A whole-file entry (no step of its own — `fields` names none)
        // means nothing in this patch applies at all, whatever keys it
        // otherwise names; every other ignored entry names one step's own
        // fields, which the main column leaves out by name.
        if ignored.iter().any(|i| i.fields.is_empty()) {
            keys.clear();
        } else {
            let ignored_fields: std::collections::HashSet<&str> =
                ignored.iter().flat_map(|i| i.fields.split(", ")).collect();
            for (step_id, fields) in &patch.steps {
                for key in fields.keys() {
                    if let Some(key) = key.as_str() {
                        let dotted = format!("{step_id}.{key}");
                        if !ignored_fields.contains(dotted.as_str()) {
                            keys.push(dotted);
                        }
                    }
                }
            }
        }

        rows.push(OverrideRow {
            target: format!("pipelines/{name}.yml"),
            kind: "patch",
            overrides: keys.join(", "),
            ignored,
        });
    }

    for name in crate::overrides::list_prompt_overrides(&dir)? {
        let ignored = if crate::prompt::path_for_tracked(repo, &name).is_file() {
            Vec::new()
        } else if crate::prompt::private_only_path(repo, &name).is_some() {
            vec![crate::overrides::Ignored::private_prompt(&name)]
        } else {
            vec![crate::overrides::Ignored::missing_prompt(&name)]
        };
        rows.push(OverrideRow {
            target: format!("prompts/{name}"),
            kind: "whole file",
            overrides: "—".to_string(),
            ignored,
        });
    }

    if let Some(keys) = crate::overrides::config_patch_keys(&dir)? {
        let ignored = match Config::load_tracked(&repo.checkout).ok() {
            Some(tracked) => crate::overrides::apply_config_patch(tracked, &dir)?.1,
            None => Vec::new(),
        };
        let ignored_keys: std::collections::HashSet<&str> =
            ignored.iter().map(|i| i.fields.as_str()).collect();
        let keys: Vec<String> = keys
            .into_iter()
            .filter(|k| !ignored_keys.contains(k.as_str()))
            .collect();
        rows.push(OverrideRow {
            target: crate::config::CONFIG_FILE.to_string(),
            kind: "patch",
            overrides: keys.join(", "),
            ignored,
        });
    }

    Ok(rows)
}

/// The step and keys columns for one ignored entry: `step publish` and
/// `agent, model` for a pipeline step's entry — its `fields` are dotted
/// under the step, `publish.agent, publish.model` — no step and the dotted
/// key itself for a config entry, and `the whole file` for an entry that
/// names no field at all, the same words `override list` uses for it.
/// `commands::dispatch`'s before-start overrides gate builds its own
/// `ignored` row's label from these.
pub(crate) fn ignored_columns(
    row: &OverrideRow,
    item: &crate::overrides::Ignored,
) -> (String, String) {
    if item.fields.is_empty() {
        return (String::new(), "the whole file".to_string());
    }
    if !row.target.starts_with("pipelines/") {
        return (String::new(), item.fields.clone());
    }
    let step = item.fields.split('.').next().unwrap_or_default();
    let keys: Vec<&str> = item
        .fields
        .split(", ")
        .map(|field| field.split_once('.').map_or(field, |(_, key)| key))
        .collect();
    (format!("step {step}"), keys.join(", "))
}

/// `--json override list`'s payload, rendered as a string so a test can
/// parse it back without capturing stdout — an empty `rows` renders `[]`,
/// never the prose the plain form prints for an empty layer.
fn render_override_rows_json(rows: &[OverrideRow]) -> Result<String> {
    let payload: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "target": r.target,
                "kind": r.kind,
                "overrides": r.overrides,
                "ignored": r.ignored.iter().map(|i| serde_json::json!({
                    "fields": i.fields,
                    "reason": i.reason,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&payload)?)
}

/// `spoolway override contract`: the merge rule, what a patch may carry, and
/// the four `override` commands — this one included.
///
/// Printed rather than copied into the skill that reaches for it: the shape a
/// patch may take is [`crate::overrides::PipelinePatch`] and
/// [`crate::overrides::apply_config_patch`]'s own rules, so a paragraph
/// stating them here separately would be the second copy that drifts.
pub fn override_contract() -> Result<()> {
    print!("{}", render_override_contract());
    Ok(())
}

/// One line of [`render_override_contract`], kept as a slice so a test can
/// walk it looking for the facts the acceptance criteria name, without
/// capturing stdout.
const OVERRIDE_CONTRACT_LINES: &[&str] = &[
    "THE OVERRIDE CONTRACT",
    "=====================",
    "",
    "A layer outside the checkout, applied alongside the tracked files — nothing here",
    "touches the checkout, so `git status` never moves.",
    "",
    "WHEN IT TAKES EFFECT",
    "  config.toml            read again at every dispatcher pass; no restart. Except",
    "                         dispatch.backend, unattended.enabled and unattended.blocked_*,",
    "                         which a running dispatcher fixes at start: restart it.",
    "  prompts/<name>         read when a lane starts.",
    "  pipelines/<name>.yml   the same rule as an edit to a pipeline file: a running",
    "                         dispatcher keeps the pipelines it loaded when it started, so",
    "                         restart it to use the patch. `pipeline show` and `pipeline",
    "                         check` read the patch at once.",
    "",
    "THE MERGE RULE",
    "  A pipeline patch overlays one key at a time onto the step it names — every key the",
    "  patch is silent about is left exactly as the tracked file wrote it. A step id must",
    "  already exist on the tracked pipeline: a patch may set a value on a step, never add,",
    "  remove or reposition one — position decides scheduling priority, and that is a change",
    "  to the graph, not to a value on it.",
    "  A config patch merges by dotted key through the same validated path `spoolway config",
    "  set` writes through, so a patch can never produce a config that command would have",
    "  refused.",
    "  A prompt override replaces the tracked file whole: prose carries no key for a patch to",
    "  aim at.",
    "",
    "WHAT A PATCH MAY CARRY",
    "  pipelines/<name>.yml   `description`, `task_template`, and `steps.<id>.<key>=<value>`",
    "                         for any key `spoolway pipeline contract` lists except `id` —",
    "                         renaming a step is refused by name. `override promote` only",
    "                         ever writes a step's own keys: a layered `description` or",
    "                         `task_template` is refused there by name too, and has to be",
    "                         edited into the tracked file by hand instead.",
    "  config.toml            any key `spoolway config contract` lists, exactly as `config",
    "                         set` would accept it.",
    "  prompts/<name>         the whole file — see the merge rule above.",
    "",
    "THE FOUR `override` COMMANDS",
    "  spoolway override contract          this",
    "  spoolway override list              what is layered right now, and over what",
    "  spoolway override promote <target>  write a patched artifact into the tracked file,",
    "                                       then clear it from the layer",
    "  spoolway override drop [<target>]   remove one entry, or the whole layer",
    "",
    "A patch is written by `spoolway pipeline override <name> --set <step>.<key>=<value>`,",
    "`spoolway prompt override <name>` or `spoolway config override` — never by hand.",
];

/// [`override_contract`]'s body, built as a string so a test can assert on
/// it directly rather than capturing stdout.
fn render_override_contract() -> String {
    let mut out = OVERRIDE_CONTRACT_LINES.join("\n");
    out.push('\n');
    out
}

/// `spoolway override list`.
pub fn override_list(repo: &Repo, json: bool) -> Result<()> {
    let dir = repo.overrides_dir();
    let rows = collect_override_rows(repo)?;

    // Before the empty check, not after: an empty layer is still a valid
    // answer to `--json`, `[]`, and a script parsing it must never be handed
    // the prose meant for a person instead — see `commands::lanes::logs`
    // for the same order.
    if json {
        println!("{}", render_override_rows_json(&rows)?);
        return Ok(());
    }

    if rows.is_empty() {
        println!(
            "no overrides — {} is empty or does not exist",
            dir.display()
        );
        return Ok(());
    }

    let target_w = rows
        .iter()
        .map(|r| r.target.len())
        .max()
        .unwrap_or(0)
        .max(6);
    let kind_w = rows.iter().map(|r| r.kind.len()).max().unwrap_or(0).max(4);
    println!("{:<target_w$}  {:<kind_w$}  OVERRIDES", "TARGET", "KIND");
    for row in &rows {
        println!(
            "{:<target_w$}  {:<kind_w$}  {}",
            row.target, row.kind, row.overrides
        );
        for item in &row.ignored {
            let fields = if item.fields.is_empty() {
                "the whole file"
            } else {
                &item.fields
            };
            println!(
                "{:<target_w$}  {:<kind_w$}  ignored  {fields} — {}",
                "",
                "",
                item.short_reason()
            );
        }
    }
    println!();
    // `layer_fingerprint`, never `stamp`'s combined one: this line is about
    // the layer alone, the value `dispatch`'s gate starts its own
    // acknowledgement from — not the tracked files stamped in beside it. Rows
    // being non-empty means the layer has at least one real file under it,
    // so `unwrap_or_else` here only stands in for a read racing this one.
    let fingerprint =
        crate::version::layer_fingerprint(repo).unwrap_or_else(|| "unknown".to_string());
    println!(
        "layer version  {fingerprint}    {} artifact{}    `override promote <target>` to keep \
         one",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

/// `spoolway override promote <target>`.
pub fn override_promote(repo: &Repo, target: &str) -> Result<()> {
    let write = repo.tracked_write_root("tracked files", &format!("override promote {target}"))?;
    let write_root = write.root;
    match parse_target(target)? {
        Target::Pipeline(name) => promote_pipeline(repo, write_root, &name)?,
        Target::Prompt(name) => promote_prompt(repo, write_root, &name)?,
        Target::Config => promote_config(repo, write_root)?,
    }
    if let Some(note) = write.note() {
        println!("{note}");
    }
    Ok(())
}

fn promote_pipeline(repo: &Repo, write_root: &Path, name: &str) -> Result<()> {
    // Held from the read of the patch to its removal: a `pipeline override
    // --set` that lands in between is deleted with the patch, after this
    // promote has printed that it wrote the file.
    let _lock = crate::lock::EditLock::acquire(&repo.edit_lock_path())?;
    let overrides_dir = repo.overrides_dir();
    let patch = crate::overrides::read_pipeline_patch(&overrides_dir, name)?
        .filter(|p| !p.steps.is_empty() || p.description.is_some() || p.task_template.is_some())
        .with_context(|| format!("no override for pipeline `{name}`"))?;

    let path = Pipelines::file_in(write_root, name);
    let changes = crate::overrides::promote_pipeline_patch(write_root, name, &patch)?;
    crate::overrides::remove_pipeline_patch(&overrides_dir, name)?;

    println!("  wrote {}", path.display());
    for (key, _old, new) in &changes {
        println!("    {key}   {new}");
    }
    println!("  cleared pipelines/{name}.yml from the layer");
    Ok(())
}

fn promote_prompt(repo: &Repo, write_root: &Path, name: &str) -> Result<()> {
    let overrides_dir = repo.overrides_dir();
    let source = crate::overrides::prompt_patch_path(&overrides_dir, name);
    let body = std::fs::read_to_string(&source)
        .with_context(|| format!("no override for prompt `{name}`"))?;

    let dest = crate::prompt::tracked_path_in(write_root, name);
    write_atomic(&dest, &body)?;
    crate::overrides::remove_prompt_override(&overrides_dir, name)?;

    println!("  wrote {}", dest.display());
    println!("  cleared prompts/{name} from the layer");
    Ok(())
}

fn promote_config(repo: &Repo, write_root: &Path) -> Result<()> {
    // Held from the read of the tracked file to its write, so a parallel
    // `config set` is not overwritten by the text this promote read before
    // it landed.
    let _lock = crate::lock::EditLock::acquire(&repo.edit_lock_path())?;
    let changes = crate::overrides::promote_config_patch(write_root)?;
    crate::overrides::remove_config_patch(&repo.overrides_dir())?;

    let path = Config::path_in(write_root);
    println!("  wrote {}", path.display());
    for (key, value) in &changes {
        println!("    {key} = {value}");
    }
    println!("  cleared config.toml from the layer");
    Ok(())
}

/// `spoolway override drop [<target>]`.
pub fn override_drop(repo: &Repo, target: Option<&str>) -> Result<()> {
    let dir = repo.overrides_dir();
    let Some(target) = target else {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", dir.display())),
        }
        println!("cleared the whole layer");
        return Ok(());
    };

    match parse_target(target)? {
        Target::Pipeline(name) => {
            crate::overrides::remove_pipeline_patch(&dir, &name)?;
            println!("cleared pipelines/{name}.yml from the layer");
        }
        Target::Prompt(name) => {
            crate::overrides::remove_prompt_override(&dir, &name)?;
            println!("cleared prompts/{name} from the layer");
        }
        Target::Config => {
            crate::overrides::remove_config_patch(&dir)?;
            println!("cleared config.toml from the layer");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEMO_PIPELINE: &str =
        "steps:\n  - id: implement\n    agent: pi\n    model: claude-sonnet-5\n    on_pass: done\n";

    /// A repo with a tracked pipeline, prompt and config on disk, and a
    /// scratch machine home behind it — `repo.overrides_dir()` and
    /// [`crate::overrides::dir_for`] both resolve through
    /// [`crate::mux::project_home`], so a test has to fake the same home
    /// either of them would otherwise read for real, or the two would land
    /// in different directories the way they never do outside a test.
    fn with_repo<T>(name: &str, f: impl FnOnce(&Repo) -> T) -> T {
        let root = crate::scratch::root(&format!("override-cmd-{name}"));
        let fake_home = crate::scratch::root(&format!("override-cmd-{name}-home"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&fake_home);
        crate::scratch::stamped(&root);
        std::fs::create_dir_all(root.join(".spoolway/pipelines")).unwrap();
        std::fs::write(root.join(".spoolway/pipelines/demo.yml"), DEMO_PIPELINE).unwrap();
        std::fs::create_dir_all(root.join(".spoolway/prompts/reviewer")).unwrap();
        std::fs::write(
            root.join(".spoolway/prompts/reviewer/PROMPT.md"),
            "Review it.\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".spoolway")).unwrap();
        std::fs::write(Config::path_in(&root), "[dispatch]\n").unwrap();

        let result = crate::platform::test_home::with_home(&fake_home, || {
            let home = crate::mux::project_home(&root).unwrap();
            let config = Config::default();
            let repo = Repo {
                borrowed: false,
                checkout: root.to_path_buf(),
                root: root.to_path_buf(),
                config,
                home,
            };
            f(&repo)
        });

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&fake_home).ok();
        result
    }

    /// A stamped home that could not be resolved must refuse `config
    /// override` outright, before it ever opens an editor or writes
    /// anything — `repo.overrides_dir()` is only `repo.home` joined onto a
    /// constant, and `repo.home` here is nothing but the basename-keyed
    /// placeholder a failed resolution falls back to; writing into it would
    /// create `overrides/config.toml` for the wrong project.
    #[test]
    fn config_override_refuses_when_the_home_could_not_be_resolved() {
        with_repo("home-unavailable", |repo| {
            let home_error = anyhow::anyhow!("permission denied reading spoolway-id");
            let err = config_override(repo, Some(&home_error))
                .expect_err("a broken home must refuse, not write through the placeholder");
            assert!(
                format!("{err:#}").contains("permission denied reading spoolway-id"),
                "{err:#}"
            );
            assert!(
                !repo.overrides_dir().exists(),
                "nothing may be created under the unresolved placeholder home"
            );
        });
    }

    /// The acceptance shape: the merge rule, what a patch may carry, the
    /// step-id rule, and all four `override` commands, `contract` included.
    #[test]
    fn override_contract_names_the_merge_rule_and_all_four_commands() {
        override_contract().unwrap();
        let text = render_override_contract();
        for fact in [
            "already exist on the tracked pipeline",
            "config.toml",
            "prompts/<name>",
            "override contract",
            "override list",
            "override promote",
            "override drop",
        ] {
            assert!(text.contains(fact), "override contract drops `{fact}`");
        }
    }

    #[test]
    fn parse_target_reads_every_spelling_override_list_prints_and_the_bare_pipeline_form() {
        assert!(matches!(
            parse_target("config.toml").unwrap(),
            Target::Config
        ));
        assert!(matches!(
            parse_target("prompts/reviewer").unwrap(),
            Target::Prompt(name) if name == "reviewer"
        ));
        assert!(matches!(
            parse_target("pipelines/impl.yml").unwrap(),
            Target::Pipeline(name) if name == "impl"
        ));
        assert!(matches!(
            parse_target("impl").unwrap(),
            Target::Pipeline(name) if name == "impl"
        ));
        assert!(parse_target("pipelines/impl").is_err());
    }

    #[test]
    fn override_list_reports_every_kind_with_its_keys() {
        with_repo("list", |repo| {
            let dir = repo.overrides_dir();
            let mut fields = serde_norway::Mapping::new();
            fields.insert(
                serde_norway::Value::String("model".to_string()),
                serde_norway::Value::String("claude-opus-5".to_string()),
            );
            let mut steps = std::collections::BTreeMap::new();
            steps.insert("implement".to_string(), fields);
            crate::overrides::write_pipeline_patch(
                &dir,
                "demo",
                &crate::overrides::PipelinePatch {
                    description: None,
                    task_template: None,
                    steps,
                },
            )
            .unwrap();
            write_atomic(
                &crate::overrides::prompt_patch_path(&dir, "reviewer"),
                "Whole new review.\n",
            )
            .unwrap();
            write_atomic(
                &crate::overrides::config_patch_path(&dir),
                "[unattended]\nblocked_agent = \"patched\"\n",
            )
            .unwrap();

            let rows = collect_override_rows(repo).unwrap();
            let targets: Vec<&str> = rows.iter().map(|r| r.target.as_str()).collect();
            assert_eq!(
                targets,
                vec!["pipelines/demo.yml", "prompts/reviewer", "config.toml"]
            );
            assert_eq!(rows[0].kind, "patch");
            assert_eq!(rows[0].overrides, "implement.model");
            assert!(rows[0].ignored.is_empty(), "nothing here is stale");
            assert_eq!(rows[1].kind, "whole file");
            assert_eq!(rows[1].overrides, "—");
            assert_eq!(rows[2].kind, "patch");
            assert_eq!(rows[2].overrides, "unattended.blocked_agent");
        });
    }

    /// Where `with_repo`'s tracked pipelines live, for a test that rewrites
    /// `demo.yml` after the fixture wrote its own single-step version.
    fn root_pipelines_dir(repo: &Repo) -> std::path::PathBuf {
        repo.checkout.join(".spoolway/pipelines")
    }

    #[test]
    fn override_list_is_empty_with_no_layer_at_all() {
        with_repo("list-empty", |repo| {
            assert!(collect_override_rows(repo).unwrap().is_empty());
        });
    }

    /// The Mockup's own row: one step's whole patch entry left stale by a
    /// later tracked edit moves out of the main `OVERRIDES` column and into
    /// its own `ignored` line, with the reason — a *different* step's entry
    /// in the same file stays right where it was. One override is one
    /// step's entry, never one field of it: `implement.model` and
    /// `implement.run` above would roll back together were they on the same
    /// step, exactly as the Mockup's own `publish.agent, publish.model` do.
    #[test]
    fn override_list_moves_a_stale_step_entry_into_its_own_ignored_line() {
        with_repo("list-stale-step", |repo| {
            std::fs::write(
                root_pipelines_dir(repo).join("demo.yml"),
                "steps:\n  - id: implement\n    agent: pi\n    model: claude-sonnet-5\n    \
                 on_pass: review\n  - id: review\n    agent: pi\n    on_pass: done\n",
            )
            .unwrap();

            let dir = repo.overrides_dir();
            let mut implement = serde_norway::Mapping::new();
            implement.insert(
                serde_norway::Value::String("model".to_string()),
                serde_norway::Value::String("claude-opus-5".to_string()),
            );
            let mut review = serde_norway::Mapping::new();
            review.insert(
                serde_norway::Value::String("run".to_string()),
                serde_norway::Value::String("echo hi".to_string()),
            );
            let mut steps = std::collections::BTreeMap::new();
            steps.insert("implement".to_string(), implement);
            steps.insert("review".to_string(), review);
            crate::overrides::write_pipeline_patch(
                &dir,
                "demo",
                &crate::overrides::PipelinePatch {
                    description: None,
                    task_template: None,
                    steps,
                },
            )
            .unwrap();

            let rows = collect_override_rows(repo).unwrap();
            let row = rows
                .iter()
                .find(|r| r.target == "pipelines/demo.yml")
                .unwrap();
            assert_eq!(
                row.overrides, "implement.model",
                "the untouched step's entry stays in the main column"
            );
            assert_eq!(row.ignored.len(), 1, "{:?}", row.ignored);
            assert_eq!(row.ignored[0].fields, "review.run");
            assert_eq!(
                row.ignored[0].short_reason(),
                "names both `run:` and `agent:`"
            );
            assert_eq!(
                row.ignored[0].notice(),
                "spoolway: override ignored — pipelines/demo.yml step `review`: names both \
                 `run:` and `agent:` — a step runs a process or a model, not both"
            );
        });
    }

    /// A patch for a pipeline the checkout no longer has at all — renamed or
    /// removed since the patch was written — is the whole-file case: it must
    /// not read as an active override with its raw keys still listed, the
    /// way a tracked pipeline load failure does; the tracked pipelines here
    /// load fine, they just do not have this name, which `Pipelines::load`
    /// itself already treats as stale — see
    /// `pipeline::tests::an_override_naming_a_pipeline_the_checkout_lacks_is_skipped_with_a_notice`.
    #[test]
    fn override_list_marks_a_patch_for_a_pipeline_the_checkout_no_longer_has() {
        with_repo("list-missing-pipeline", |repo| {
            let dir = repo.overrides_dir();
            let mut fields = serde_norway::Mapping::new();
            fields.insert(
                serde_norway::Value::String("model".to_string()),
                serde_norway::Value::String("claude-opus-5".to_string()),
            );
            let mut steps = std::collections::BTreeMap::new();
            steps.insert("implement".to_string(), fields);
            crate::overrides::write_pipeline_patch(
                &dir,
                "gone",
                &crate::overrides::PipelinePatch {
                    description: None,
                    task_template: None,
                    steps,
                },
            )
            .unwrap();

            let rows = collect_override_rows(repo).unwrap();
            let row = rows
                .iter()
                .find(|r| r.target == "pipelines/gone.yml")
                .unwrap();
            assert_eq!(
                row.overrides, "",
                "nothing in a patch for a pipeline that does not exist applies"
            );
            assert_eq!(row.ignored.len(), 1, "{:?}", row.ignored);
            assert_eq!(
                row.ignored[0].reason,
                "names pipeline `gone`, which the checkout does not have"
            );

            let rendered = render_override_rows_json(&rows).unwrap();
            let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
            let json_row = value
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["target"] == "pipelines/gone.yml")
                .unwrap();
            assert_eq!(json_row["ignored"].as_array().unwrap().len(), 1);
        });
    }

    /// The finding this guards: `--json override list` on an empty or
    /// absent layer must still answer `[]`, never the prose the plain form
    /// prints — checked on the actual rendering function `override_list`
    /// calls, and on the command itself succeeding either way, since
    /// nothing here captures stdout to compare the printed bytes directly.
    #[test]
    fn override_list_json_on_an_empty_layer_is_an_empty_array() {
        let rendered = render_override_rows_json(&[]).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value, serde_json::json!([]));

        with_repo("list-json-empty", |repo| {
            override_list(repo, true).unwrap();
            override_list(repo, false).unwrap();
        });
    }

    /// `--json`'s own row carries `ignored` too — each entry's fields and
    /// reason — so a script reading the layer never has to fall back to
    /// scraping the plain form's own prose.
    #[test]
    fn override_list_json_marks_an_ignored_entry() {
        let rows = vec![OverrideRow {
            target: "pipelines/release.yml".to_string(),
            kind: "patch",
            overrides: "fix.agent".to_string(),
            ignored: vec![crate::overrides::Ignored {
                target: "pipelines/release.yml step `publish`".to_string(),
                fields: "publish.agent, publish.model".to_string(),
                reason: "names both `run:` and `agent:` — a step runs a process or a model, \
                         not both"
                    .to_string(),
            }],
        }];
        let rendered = render_override_rows_json(&rows).unwrap();
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(
            value[0]["ignored"][0]["fields"],
            serde_json::json!("publish.agent, publish.model")
        );
        assert_eq!(
            value[0]["ignored"][0]["reason"],
            serde_json::json!(
                "names both `run:` and `agent:` — a step runs a process or a model, not both"
            )
        );
    }

    #[test]
    fn prompt_override_copies_the_tracked_prompt_into_the_layer() {
        with_repo("prompt-fork", |repo| {
            prompt_override(repo, "reviewer").unwrap();
            let forked = std::fs::read_to_string(crate::overrides::prompt_patch_path(
                &repo.overrides_dir(),
                "reviewer",
            ))
            .unwrap();
            assert_eq!(forked, "Review it.\n");
        });
    }

    #[test]
    fn prompt_override_refuses_a_prompt_that_does_not_exist() {
        with_repo("prompt-fork-missing", |repo| {
            let err = prompt_override(repo, "nosuchprompt").unwrap_err();
            assert!(err.to_string().contains("no prompt named"), "{err}");
        });
    }

    /// `prompt override` reads straight off [`crate::prompt::path_for_tracked`],
    /// so a private prompt — one that `prompt show` and `prompt list` both
    /// find — is refused as though it did not exist at all. `prompt_override`
    /// must find it the way every other reader of the private layer does.
    #[test]
    fn prompt_override_accepts_a_private_prompt() {
        with_repo("prompt-fork-private", |repo| {
            let local = crate::local::dir_for(&repo.root).unwrap();
            let private = crate::local::prompts_dir(&local)
                .join("impl2")
                .join(crate::assets::PROMPT_FILE);
            std::fs::create_dir_all(private.parent().unwrap()).unwrap();
            std::fs::write(&private, "the private prompt").unwrap();

            prompt_override(repo, "impl2").expect("a private prompt must fork, not be refused");
            let forked = std::fs::read_to_string(crate::overrides::prompt_patch_path(
                &repo.overrides_dir(),
                "impl2",
            ))
            .unwrap();
            assert_eq!(forked, "the private prompt");
        });
    }

    #[test]
    fn override_promote_writes_the_tracked_pipeline_and_clears_the_layer() {
        with_repo("promote-pipeline", |repo| {
            pipeline_override(repo, "demo", "implement.model=claude-opus-5").unwrap();
            override_promote(repo, "demo").unwrap();

            let tracked = std::fs::read_to_string(Pipelines::file_in(&repo.root, "demo")).unwrap();
            assert!(tracked.contains("model: claude-opus-5"));
            assert!(!tracked.contains("model: claude-sonnet-5"));
            assert!(
                crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "demo")
                    .unwrap()
                    .is_none(),
                "promoting should clear the layer's own copy"
            );
        });
    }

    #[test]
    fn override_promote_refuses_a_target_with_nothing_layered() {
        with_repo("promote-nothing", |repo| {
            let err = override_promote(repo, "demo").unwrap_err();
            assert!(format!("{err:#}").contains("no override"), "{err:#}");
        });
    }

    /// The main checkout, a worktree of its own, a borrowed one and a lane's
    /// — real git, so `checkout` and `root` land in different directories —
    /// each as a `Repo`, with the pipeline `demo` carrying an override on
    /// `implement.model`. `f` gets them in that order, then the main root.
    fn with_worktrees<T>(name: &str, f: impl FnOnce([&Repo; 4], &Path) -> T) -> T {
        with_repo(name, |main| {
            let git = |args: &[&str]| crate::repo::run(&main.root, "git", args).unwrap();
            git(&["init", "-q", "-b", "main"]);
            git(&["config", "user.email", "t@example.com"]);
            git(&["config", "user.name", "t"]);
            git(&["add", "-A"]);
            git(&["commit", "-q", "-m", "root"]);
            let own = main.root.join("own");
            git(&[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature/x",
                own.to_str().unwrap(),
            ]);
            let lane_dir = main.worktree_root().join("lane");
            git(&[
                "worktree",
                "add",
                "-q",
                "-b",
                "task/y",
                lane_dir.to_str().unwrap(),
            ]);

            let mut own_repo = main.clone();
            own_repo.checkout = own;
            let mut lane = main.clone();
            lane.checkout = lane_dir;
            let mut borrowed = main.clone();
            borrowed.borrowed = true;
            pipeline_override(main, "demo", "implement.model=claude-opus-5").unwrap();
            f([main, &own_repo, &borrowed, &lane], &main.root)
        })
    }

    fn tracked_demo(root: &Path) -> String {
        std::fs::read_to_string(Pipelines::file_in(root, "demo")).unwrap()
    }

    /// In a worktree of its own, `override promote` writes that worktree's
    /// pipeline file and clears the layer entry, and the main checkout's
    /// file is left as it was.
    #[test]
    fn override_promote_in_a_worktree_of_its_own_writes_the_worktrees_file() {
        with_worktrees("promote-own", |[_, own, _, _], root| {
            let before = tracked_demo(root);
            override_promote(own, "demo").unwrap();
            assert!(tracked_demo(&own.checkout).contains("model: claude-opus-5"));
            assert_eq!(tracked_demo(root), before);
            assert!(
                crate::overrides::read_pipeline_patch(&own.overrides_dir(), "demo")
                    .unwrap()
                    .is_none(),
                "the entry is cleared at once, as in the main checkout"
            );
        });
    }

    /// The main checkout still writes its own file, with no extra line.
    #[test]
    fn override_promote_in_the_main_checkout_writes_its_own_file() {
        with_worktrees("promote-main", |[main, ..], root| {
            override_promote(main, "demo").unwrap();
            assert!(tracked_demo(root).contains("model: claude-opus-5"));
            let write = main.tracked_write_root("tracked files", "x").unwrap();
            assert!(write.note().is_none());
        });
    }

    /// A borrowed worktree has no `.spoolway/` of its own, so the promote
    /// refuses with that reason and writes neither file nor layer.
    #[test]
    fn override_promote_refuses_a_borrowed_worktree_and_writes_nothing() {
        with_worktrees("promote-borrowed", |[_, _, borrowed, _], root| {
            let before = tracked_demo(root);
            let err = override_promote(borrowed, "demo").unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                format!(
                    "this worktree's branch has no .spoolway/ of its own, so there is no setup \
                     here to write.\n  spoolway -C {} override promote demo",
                    borrowed.root.display()
                )
            );
            assert_eq!(tracked_demo(root), before);
            assert!(
                crate::overrides::read_pipeline_patch(&borrowed.overrides_dir(), "demo")
                    .unwrap()
                    .is_some()
            );
        });
    }

    /// A worktree on a detached HEAD is refused before the file is written,
    /// so the layer entry survives for the person to promote from a branch.
    #[test]
    fn override_promote_refuses_a_detached_worktree_and_keeps_the_entry() {
        with_worktrees("promote-detached", |[_, own, _, _], root| {
            let before = tracked_demo(root);
            let detached = detach(own);
            let err = override_promote(&detached, "demo").unwrap_err();
            assert!(
                format!("{err:#}").contains("detached HEAD"),
                "unexpected message: {err:#}"
            );
            assert_eq!(tracked_demo(&detached.checkout), before);
            assert!(
                crate::overrides::read_pipeline_patch(&detached.overrides_dir(), "demo")
                    .unwrap()
                    .is_some(),
                "a refused promote must not clear the layer entry"
            );
        });
    }

    /// A git that cannot run in the worktree is reported as that, not as a
    /// detached HEAD, and the layer entry survives.
    #[test]
    fn override_promote_reports_a_git_failure_rather_than_a_detached_head() {
        with_worktrees("promote-git-fails", |[_, own, _, _], root| {
            let mut gone = own.clone();
            gone.checkout = root.join("not-a-worktree");
            let err = override_promote(&gone, "demo").unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("cannot tell which branch"), "{message}");
            assert!(!message.contains("detached HEAD"), "{message}");
            assert!(
                crate::overrides::read_pipeline_patch(&gone.overrides_dir(), "demo")
                    .unwrap()
                    .is_some(),
                "a refused promote must not clear the layer entry"
            );
        });
    }

    /// `own` with its worktree moved off its branch onto the same commit.
    fn detach(own: &Repo) -> Repo {
        crate::repo::run(&own.checkout, "git", &["checkout", "-q", "--detach"]).unwrap();
        own.clone()
    }

    /// A dispatcher lane's worktree is told to report the setting, not handed
    /// a `-C` command, and writes nothing.
    #[test]
    fn override_promote_refuses_a_lane_worktree_and_writes_nothing() {
        with_worktrees("promote-lane", |[_, _, _, lane], root| {
            let before = tracked_demo(root);
            let err = override_promote(lane, "demo").unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                "the dispatcher reads the project's tracked files, not this worktree's.\n  \
                 A lane does not change the project's setup. Write the\n  \
                 setting you wanted into your report, and a person\n  \
                 will make the change."
            );
            assert_eq!(tracked_demo(root), before);
            assert_eq!(tracked_demo(&lane.checkout), before);
        });
    }

    /// A dispatcher lane never writes the person's private overrides layer.
    /// Each command that does is refused with the lane wording, exits as an
    /// error, and leaves the layer and the private `local/` copies as they
    /// were.
    #[test]
    fn every_command_that_writes_the_overrides_layer_is_refused_in_a_lane() {
        with_worktrees("layer-lane", |[main, _, _, lane], _| {
            let layer = main.overrides_dir();
            let snapshot = |dir: &Path| -> Vec<(std::path::PathBuf, Vec<u8>)> {
                let mut files = Vec::new();
                let mut stack = vec![dir.to_path_buf()];
                while let Some(d) = stack.pop() {
                    let Ok(entries) = std::fs::read_dir(&d) else {
                        continue;
                    };
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.is_dir() {
                            stack.push(path);
                        } else {
                            files.push((path.clone(), std::fs::read(&path).unwrap()));
                        }
                    }
                }
                files.sort();
                files
            };
            let layer_before = snapshot(&layer);
            let local_before = snapshot(&lane.local_dir());
            assert!(
                !layer_before.is_empty(),
                "the fixture holds a private layer"
            );

            let refusals: Vec<(&str, anyhow::Error)> = vec![
                (
                    "override drop",
                    override_drop(lane, None).expect_err("override drop"),
                ),
                (
                    "override drop <target>",
                    override_drop(lane, Some("demo")).expect_err("override drop demo"),
                ),
                (
                    "pipeline override",
                    super::pipeline::pipeline_override(lane, "demo", "implement.model=other")
                        .expect_err("pipeline override"),
                ),
                (
                    "prompt override",
                    prompt_override(lane, "reviewer").expect_err("prompt override"),
                ),
                (
                    "config override",
                    config_override(lane, None).expect_err("config override"),
                ),
                (
                    "pipeline copy",
                    super::pipeline::pipeline_copy(lane, "demo", "demo-copy", false)
                        .expect_err("pipeline copy"),
                ),
                (
                    "prompt copy",
                    super::pipeline::prompt_copy(lane, "reviewer", "reviewer-copy", false)
                        .expect_err("prompt copy"),
                ),
            ];
            for (command, err) in &refusals {
                let message = format!("{err:#}");
                assert!(
                    message.contains("A lane does not change the project's setup"),
                    "{command} was not refused as a lane: {message}"
                );
            }
            assert_eq!(snapshot(&layer), layer_before, "the layer was changed");
            assert_eq!(
                snapshot(&lane.local_dir()),
                local_before,
                "local/ was changed"
            );
        });
    }

    #[test]
    fn override_promote_promotes_a_prompt() {
        with_repo("promote-prompt", |repo| {
            write_atomic(
                &crate::overrides::prompt_patch_path(&repo.overrides_dir(), "reviewer"),
                "Whole new review.\n",
            )
            .unwrap();

            override_promote(repo, "prompts/reviewer").unwrap();

            assert_eq!(
                std::fs::read_to_string(crate::prompt::path_for_tracked(repo, "reviewer")).unwrap(),
                "Whole new review.\n"
            );
            assert!(
                !crate::overrides::prompt_patch_path(&repo.overrides_dir(), "reviewer").is_file()
            );
        });
    }

    #[test]
    fn override_promote_promotes_a_config_key() {
        with_repo("promote-config", |repo| {
            write_atomic(
                &crate::overrides::config_patch_path(&repo.overrides_dir()),
                "[unattended]\nblocked_agent = \"promoted\"\n",
            )
            .unwrap();

            override_promote(repo, "config.toml").unwrap();

            let tracked_config = std::fs::read_to_string(Config::path_in(&repo.root)).unwrap();
            assert!(tracked_config.contains("blocked_agent = \"promoted\""));
            assert!(!crate::overrides::config_patch_path(&repo.overrides_dir()).is_file());
        });
    }

    /// A promote waits for the edit lock a `config set` holds, so the tracked
    /// file it reads is the one that `set` has finished writing. Without the
    /// lock it ran at once and wrote over whatever landed meanwhile.
    #[test]
    fn override_promote_of_the_config_waits_for_the_edit_lock() {
        with_repo("promote-config-lock", |repo| {
            write_atomic(
                &crate::overrides::config_patch_path(&repo.overrides_dir()),
                "[unattended]\nblocked_agent = \"promoted\"\n",
            )
            .unwrap();
            let held = crate::lock::EditLock::acquire(&repo.edit_lock_path()).unwrap();
            // The promote runs on this thread, which is the one the scratch
            // home is swapped on; another thread releases the lock.
            let started = std::time::Instant::now();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    drop(held);
                });
                override_promote(repo, "config.toml").unwrap();
            });
            assert!(
                started.elapsed() >= std::time::Duration::from_millis(250),
                "the promote ran while the edit lock was held"
            );
        });
    }

    /// A pipeline promote waits for the edit lock a `pipeline override --set`
    /// holds, so a `--set` mid-way is not deleted with the patch it landed in.
    #[test]
    fn override_promote_of_a_pipeline_waits_for_the_edit_lock() {
        with_repo("promote-pipeline-lock", |repo| {
            pipeline_override(repo, "demo", "implement.model=claude-opus-5").unwrap();
            let held = crate::lock::EditLock::acquire(&repo.edit_lock_path()).unwrap();
            let started = std::time::Instant::now();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    drop(held);
                });
                override_promote(repo, "demo").unwrap();
            });
            assert!(
                started.elapsed() >= std::time::Duration::from_millis(250),
                "the promote ran while the edit lock was held"
            );
        });
    }

    #[test]
    fn override_drop_removes_one_entry_and_leaves_the_others() {
        with_repo("drop-one", |repo| {
            pipeline_override(repo, "demo", "implement.model=claude-opus-5").unwrap();
            prompt_override(repo, "reviewer").unwrap();

            override_drop(repo, Some("demo")).unwrap();
            assert!(
                crate::overrides::read_pipeline_patch(&repo.overrides_dir(), "demo")
                    .unwrap()
                    .is_none()
            );
            assert!(
                crate::overrides::prompt_patch_path(&repo.overrides_dir(), "reviewer").is_file(),
                "dropping one target must leave the other alone"
            );
        });
    }

    /// A layer that is not valid TOML can still be dropped, by name or bare:
    /// the removal only deletes the file and never parses it. That `main`
    /// reaches this with a `Repo` at all is covered where discovery is, in
    /// `an_override_layer_that_does_not_parse_stops_discovery_only_for_the_strict_path`.
    #[test]
    fn override_drop_removes_a_config_layer_that_does_not_parse() {
        with_repo("drop-broken", |repo| {
            let path = crate::overrides::config_patch_path(&repo.overrides_dir());
            for target in [Some("config.toml"), None] {
                write_atomic(&path, "[dispatch\nlane_quiet = \"7m\"\n").unwrap();
                override_drop(repo, target).unwrap();
                assert!(!path.is_file(), "{target:?}");
            }
        });
    }

    #[test]
    fn override_drop_with_no_target_clears_the_whole_layer() {
        with_repo("drop-all", |repo| {
            pipeline_override(repo, "demo", "implement.model=claude-opus-5").unwrap();
            prompt_override(repo, "reviewer").unwrap();

            override_drop(repo, None).unwrap();
            assert!(!repo.overrides_dir().is_dir());
        });
    }
}
