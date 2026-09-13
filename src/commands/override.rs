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
//! file, and it refuses outside the main checkout for the same reason
//! [`crate::commands::config_set`] does: the dispatcher reads `repo.root`'s
//! files, never a linked worktree's own copy.

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

/// A repo standing in for `repo.root` — the tracked file `override promote`
/// writes lives there, but every reader this file otherwise calls
/// (`prompt::path_for_tracked`, chiefly) takes a whole `Repo` and reads
/// `checkout`. Building one rather than adding a second, root-only
/// signature to each of those keeps this the only place that has to know
/// promote wants the project's own copy rather than this worktree's.
fn at_root(repo: &Repo) -> Repo {
    let mut at_root = repo.clone();
    at_root.checkout = repo.root.clone();
    at_root
}

/// Refuse a write to the tracked control plane from inside a linked
/// worktree — the same rule and the same reason as [`config_set`]: the
/// dispatcher reads `repo.root`'s files, never a worktree's own copy, so a
/// promote here would sit in a file nothing reads until the branch merges.
fn refuse_in_worktree(repo: &Repo, target: &str) -> Result<()> {
    if repo.checkout != repo.root {
        bail!(
            "the dispatcher reads the project's tracked files, not this worktree's.\n  spoolway \
             -C {} override promote {target}",
            repo.root.display()
        );
    }
    Ok(())
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
    let body = std::fs::read_to_string(&tracked)
        .with_context(|| format!("no prompt named `{name}` at {}", tracked.display()))?;

    let overrides_dir = repo.overrides_dir();
    let target = crate::overrides::prompt_patch_path(&overrides_dir, name);
    write_atomic(&target, &body)?;

    println!("  wrote {}", target.display());
    println!();
    println!("  active on the next lane. `spoolway override drop prompts/{name}` to clear it.");
    Ok(())
}

/// `spoolway config override`: open (creating if absent) `overrides/
/// config.toml` — the layer's own copy, never the tracked file `config
/// edit` opens.
pub fn config_override(repo: &Repo) -> Result<()> {
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
    crate::overrides::apply_config_patch(tracked, &repo.overrides_dir()).with_context(|| {
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
}

/// Every entry the layer under `dir` currently holds, in the order `override
/// list` prints them: pipelines, then prompts, then config.
pub(crate) fn collect_override_rows(dir: &Path) -> Result<Vec<OverrideRow>> {
    let mut rows = Vec::new();

    for name in crate::overrides::list_pipeline_patches(dir)? {
        let patch = crate::overrides::read_pipeline_patch(dir, &name)?.unwrap_or_default();
        let mut keys = Vec::new();
        if patch.description.is_some() {
            keys.push("description".to_string());
        }
        if patch.task_template.is_some() {
            keys.push("task_template".to_string());
        }
        for (step_id, fields) in &patch.steps {
            for key in fields.keys() {
                if let Some(key) = key.as_str() {
                    keys.push(format!("{step_id}.{key}"));
                }
            }
        }
        rows.push(OverrideRow {
            target: format!("pipelines/{name}.yml"),
            kind: "patch",
            overrides: keys.join(", "),
        });
    }

    for name in crate::overrides::list_prompt_overrides(dir)? {
        rows.push(OverrideRow {
            target: format!("prompts/{name}"),
            kind: "whole file",
            overrides: "—".to_string(),
        });
    }

    if let Some(keys) = crate::overrides::config_patch_keys(dir)? {
        rows.push(OverrideRow {
            target: crate::config::CONFIG_FILE.to_string(),
            kind: "patch",
            overrides: keys.join(", "),
        });
    }

    Ok(rows)
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
    "A layer outside the checkout, read live at every dispatcher pass, `pipeline show` and",
    "lane start, alongside the tracked files — nothing here touches the checkout, so `git",
    "status` never moves.",
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
    let rows = collect_override_rows(&dir)?;

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
    }
    println!();
    // `layer_fingerprint`, never `stamp`'s combined one: this line is about
    // the layer alone, the same value `dispatch`'s gate keys its own
    // acknowledgement on — not the tracked files stamped in beside it. Rows
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
    refuse_in_worktree(repo, target)?;
    match parse_target(target)? {
        Target::Pipeline(name) => promote_pipeline(repo, &name),
        Target::Prompt(name) => promote_prompt(repo, &name),
        Target::Config => promote_config(repo),
    }
}

fn promote_pipeline(repo: &Repo, name: &str) -> Result<()> {
    let overrides_dir = repo.overrides_dir();
    let patch = crate::overrides::read_pipeline_patch(&overrides_dir, name)?
        .filter(|p| !p.steps.is_empty() || p.description.is_some() || p.task_template.is_some())
        .with_context(|| format!("no override for pipeline `{name}`"))?;

    let path = Pipelines::file_in(&repo.root, name);
    let changes = crate::overrides::promote_pipeline_patch(&repo.root, name, &patch)?;
    crate::overrides::remove_pipeline_patch(&overrides_dir, name)?;

    println!("  wrote {}", path.display());
    for (key, _old, new) in &changes {
        println!("    {key}   {new}");
    }
    println!("  cleared pipelines/{name}.yml from the layer");
    Ok(())
}

fn promote_prompt(repo: &Repo, name: &str) -> Result<()> {
    let overrides_dir = repo.overrides_dir();
    let source = crate::overrides::prompt_patch_path(&overrides_dir, name);
    let body = std::fs::read_to_string(&source)
        .with_context(|| format!("no override for prompt `{name}`"))?;

    let dest = crate::prompt::path_for_tracked(&at_root(repo), name);
    write_atomic(&dest, &body)?;
    crate::overrides::remove_prompt_override(&overrides_dir, name)?;

    println!("  wrote {}", dest.display());
    println!("  cleared prompts/{name} from the layer");
    Ok(())
}

fn promote_config(repo: &Repo) -> Result<()> {
    let changes = crate::overrides::promote_config_patch(&repo.root)?;
    crate::overrides::remove_config_patch(&repo.overrides_dir())?;

    let path = Config::path_in(&repo.root);
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
        std::fs::create_dir_all(root.join(".spoolway/pipelines")).unwrap();
        std::fs::write(root.join(".spoolway/pipelines/demo.yml"), DEMO_PIPELINE).unwrap();
        std::fs::create_dir_all(root.join(".spoolway/prompts/reviewer")).unwrap();
        std::fs::write(
            root.join(".spoolway/prompts/reviewer/PROMPT.md"),
            "Review it.\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".spoolway")).unwrap();
        std::fs::write(
            Config::path_in(&root),
            "[dispatch]\ndefault_pipeline = \"demo\"\n",
        )
        .unwrap();

        let result = crate::platform::test_home::with_home(&fake_home, || {
            let home = crate::mux::project_home(&root);
            let mut config = Config::default();
            config.dispatch.default_pipeline = "demo".to_string();
            let repo = Repo {
                checkout: root.clone(),
                root: root.clone(),
                config,
                home,
            };
            f(&repo)
        });

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&fake_home).ok();
        result
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
                "[dispatch]\ndefault_pipeline = \"bugfix\"\n",
            )
            .unwrap();

            let rows = collect_override_rows(&dir).unwrap();
            let targets: Vec<&str> = rows.iter().map(|r| r.target.as_str()).collect();
            assert_eq!(
                targets,
                vec!["pipelines/demo.yml", "prompts/reviewer", "config.toml"]
            );
            assert_eq!(rows[0].kind, "patch");
            assert_eq!(rows[0].overrides, "implement.model");
            assert_eq!(rows[1].kind, "whole file");
            assert_eq!(rows[1].overrides, "—");
            assert_eq!(rows[2].kind, "patch");
            assert_eq!(rows[2].overrides, "dispatch.default_pipeline");
        });
    }

    #[test]
    fn override_list_is_empty_with_no_layer_at_all() {
        with_repo("list-empty", |repo| {
            assert!(
                collect_override_rows(&repo.overrides_dir())
                    .unwrap()
                    .is_empty()
            );
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

    #[test]
    fn override_promote_refuses_from_a_linked_worktree() {
        with_repo("promote-worktree", |repo| {
            pipeline_override(repo, "demo", "implement.model=claude-opus-5").unwrap();
            let mut worktree = repo.clone();
            worktree.checkout = repo.root.join("elsewhere");
            let err = override_promote(&worktree, "demo").unwrap_err();
            assert!(err.to_string().contains("this worktree's"), "{err}");
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
                "[dispatch]\ndefault_pipeline = \"bugfix\"\n",
            )
            .unwrap();

            override_promote(repo, "config.toml").unwrap();

            let tracked_config = std::fs::read_to_string(Config::path_in(&repo.root)).unwrap();
            assert!(tracked_config.contains("default_pipeline = \"bugfix\""));
            assert!(!crate::overrides::config_patch_path(&repo.overrides_dir()).is_file());
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
